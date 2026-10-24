//! Direct Messages API backend (API key). Owns the tool loop itself.

use crate::client::{Client, Live, ModelConfig};
use crate::host::ToolHost;
use crate::{AgentEvent, prompt};
use serde_json::{Value, json};

/// Cap on model calls per user message, so a confused loop can't run away.
const MAX_STEPS: usize = 40;

pub struct ApiBackend {
    client: Client,
    host: ToolHost,
    tools: Vec<Value>,
    /// Append-only conversation history in wire format.
    messages: Vec<Value>,
    /// A turn is in progress, so Stop has history to close off.
    in_turn: bool,
    /// Results for the latest tool calls, kept here as each one finishes so
    /// Stop can still report them.
    finished: Vec<Value>,
    /// The tool call running now, if any.
    running: Option<String>,
    events: async_channel::Sender<AgentEvent>,
}

impl ApiBackend {
    /// `messages` continues a saved conversation (from [`AgentEvent::Resume`]).
    pub fn new(
        api_key: String,
        model: &str,
        host: ToolHost,
        events: async_channel::Sender<AgentEvent>,
        messages: Vec<Value>,
    ) -> Self {
        let mut config = ModelConfig::default();
        if !model.trim().is_empty() {
            config.model = model.trim().to_string();
        }
        let _ = events.try_send(AgentEvent::Info(format!(
            "Anthropic API: {} at {} effort (API key billing)",
            config.model, config.effort
        )));
        // Chats saved before empty replies were skipped can hold one; the API
        // refuses any later request with it.
        let messages: Vec<Value> = messages.into_iter().filter(|m| !is_empty_assistant(m)).collect();
        Self {
            client: Client::new(api_key, config),
            tools: host.registry().definitions(true),
            host,
            in_turn: false,
            finished: Vec::new(),
            running: None,
            messages,
            events,
        }
    }

    fn emit(&self, ev: AgentEvent) {
        let _ = self.events.try_send(ev);
    }

    /// Stop: keep what already happened (the user's message, tools that ran)
    /// and close the turn so the next request is valid. The `run_steps`
    /// future was dropped at an await, where history is always consistent.
    pub fn on_cancel(&mut self) {
        let finished = std::mem::take(&mut self.finished);
        let running = self.running.take();
        if std::mem::take(&mut self.in_turn) {
            close_cancelled_turn(&mut self.messages, finished, running.as_deref());
            self.save();
        }
    }

    pub async fn run_turn(&mut self, text: String) {
        self.run_steps(text).await;
        self.in_turn = false;
        self.save();
    }

    /// Saved with the chat. Images are left out; they're large and the
    /// model doesn't need old screenshots to continue.
    fn save(&self) {
        let history: Vec<Value> = self.messages.iter().map(without_images).collect();
        self.emit(AgentEvent::Resume(json!({"api_messages": history})));
    }

    async fn run_steps(&mut self, text: String) {
        self.messages.push(json!({
            "role": "user",
            "content": [{"type": "text", "text": prompt::stamp(&text)}],
        }));
        self.in_turn = true;
        let user_index = self.messages.len() - 1;

        for _ in 0..MAX_STEPS {
            self.emit(AgentEvent::TurnStarted);
            let events = self.events.clone();
            let result = self
                .client
                .stream(prompt::SYSTEM_PROMPT, &self.tools, &self.messages, |live| {
                    let ev = match live {
                        Live::Text(t) => AgentEvent::TextDelta(t.to_string()),
                        Live::Note(n) => AgentEvent::Note(n),
                    };
                    let _ = events.try_send(ev);
                })
                .await;

            let done = match result {
                Ok(done) => done,
                Err(e) => {
                    // The client's message says whether it retried.
                    self.emit(AgentEvent::Error(e.to_string()));
                    self.rollback_if_unanswered(user_index);
                    return;
                }
            };

            let stop = done.stop_reason.clone().unwrap_or_default();

            if stop == "refusal" {
                let category = done
                    .stop_details
                    .as_ref()
                    .and_then(|d| d["category"].as_str())
                    .unwrap_or("unspecified");
                self.emit(AgentEvent::Error(format!(
                    "The model declined this request (category: {category}). Nothing was run."
                )));
                // A refused turn is discarded rather than echoed back.
                self.rollback_if_unanswered(user_index);
                self.emit(AgentEvent::Done { stop_reason: stop, model: done.model, usage: done.usage });
                return;
            }

            // Blocks that survive the fallback boundary. Tools run from this
            // list (which still carries parse-failure markers); history gets
            // the cleaned copy.
            let kept = after_fallback(&done.content);
            let tool_uses: Vec<Value> = kept.iter().filter(|b| b["type"] == "tool_use").cloned().collect();
            // An empty reply (end_turn with no content, which can follow
            // tool results) isn't kept: the API refuses an empty assistant
            // message anywhere but last, so it would break every later turn.
            let reply = json!({"role": "assistant", "content": strip_bookkeeping(&kept)});
            if !is_empty_assistant(&reply) {
                self.messages.push(reply);
            }

            if stop == "max_tokens" && !tool_uses.is_empty() {
                // Inputs may be cut off; never run them. Answer each call with
                // an error so the history stays valid, and let the model retry.
                let results: Vec<Value> = tool_uses
                    .iter()
                    .map(|t| {
                        tool_result(
                            &t["id"],
                            "Output limit reached before this call was complete. Retry with a shorter input.",
                            true,
                        )
                    })
                    .collect();
                self.messages.push(json!({"role": "user", "content": results}));
                continue;
            }

            if tool_uses.is_empty() {
                self.emit(AgentEvent::Done { stop_reason: stop, model: done.model, usage: done.usage });
                return;
            }

            // Results collect on `self` so Stop can report the ones that ran.
            self.finished.clear();
            for block in &tool_uses {
                let id = block["id"].as_str().unwrap_or_default().to_string();
                let name = block["name"].as_str().unwrap_or_default();
                if let Some(raw) = block["__invalid_json"].as_str() {
                    let content = json!({"INVALID_JSON": raw}).to_string();
                    self.emit(AgentEvent::ToolResult {
                        id,
                        name: name.to_string(),
                        ok: false,
                        summary: "invalid tool input JSON".into(),
                        content: content.clone(),
                        evidence: Vec::new(),
                        changes: Vec::new(),
                    });
                    self.finished.push(tool_result(&block["id"], &content, true));
                    continue;
                }
                self.running = Some(id.clone());
                let out = self.host.execute(id, name, block["input"].clone()).await;
                self.running = None;
                let mut result = tool_result(&block["id"], &out.content, out.is_error);
                if !out.images.is_empty() {
                    let mut blocks = vec![json!({"type": "text", "text": out.content})];
                    for img in &out.images {
                        blocks.push(json!({
                            "type": "image",
                            "source": {"type": "base64", "media_type": img.mime, "data": img.base64},
                        }));
                    }
                    result["content"] = Value::Array(blocks);
                }
                self.finished.push(result);
            }
            // Every result for this assistant turn goes back in one message.
            let results = std::mem::take(&mut self.finished);
            self.messages.push(json!({"role": "user", "content": results}));
        }

        self.emit(AgentEvent::Error(format!(
            "Stopped after {MAX_STEPS} steps without finishing. Send another message to continue."
        )));
    }

    /// If the newest history entry is the user message that just failed, drop
    /// it so the next message starts from a valid alternating history.
    fn rollback_if_unanswered(&mut self, user_index: usize) {
        if self.messages.len() == user_index + 1 {
            self.messages.pop();
        }
    }
}

fn is_empty_assistant(m: &Value) -> bool {
    m["role"] == "assistant" && m["content"].as_array().is_some_and(|c| c.is_empty())
}

/// Told to the model when the user stops a turn.
const STOPPED_NOTE: &str = "[The user pressed Stop, which cancelled this turn here.]";
/// Result for a tool call that hadn't started when the user pressed Stop.
const NOT_RUN: &str = "Cancelled by the user before this ran.";
/// Result for the tool call that was running when the user pressed Stop.
const INTERRUPTED: &str =
    "Stopped by the user while this was running. It may or may not have taken effect; check before trying it again.";

/// Closes a turn cut short by Stop without forgetting what happened. The
/// tool calls in a trailing assistant message are answered in one new user
/// message: the real result from `finished`, or a cancelled error (`running`
/// names the call that was in progress). A note tells the model the user
/// stopped the turn.
fn close_cancelled_turn(messages: &mut Vec<Value>, mut finished: Vec<Value>, running: Option<&str>) {
    let note = json!({"type": "text", "text": STOPPED_NOTE});
    let Some(last) = messages.last_mut() else { return };
    if last["role"] == "user" {
        // Stopped while the model was answering: the user message (or tool
        // results) stays, and the note rides along with it.
        match last["content"].take() {
            Value::Array(mut blocks) => {
                blocks.push(note);
                last["content"] = Value::Array(blocks);
            }
            other => last["content"] = json!([{"type": "text", "text": other.as_str().unwrap_or("")}, note]),
        }
        return;
    }
    let calls: Vec<Value> = last["content"]
        .as_array()
        .map(|c| c.iter().filter(|b| b["type"] == "tool_use").cloned().collect())
        .unwrap_or_default();
    let mut content: Vec<Value> = calls
        .iter()
        .map(|call| {
            let id = &call["id"];
            if let Some(at) = finished.iter().position(|r| r["tool_use_id"] == *id) {
                finished.remove(at)
            } else if id.as_str().is_some() && id.as_str() == running {
                tool_result(id, INTERRUPTED, true)
            } else {
                tool_result(id, NOT_RUN, true)
            }
        })
        .collect();
    content.push(note);
    messages.push(json!({"role": "user", "content": content}));
}

/// After a mid-output fallback, drops the blocks before the last `fallback`
/// marker that the fallback model can't accept (thinking, tool calls), per
/// the fallback echo rules. Without a fallback, keeps everything.
fn after_fallback(content: &[Value]) -> Vec<Value> {
    let boundary = content.iter().rposition(|b| b["type"] == "fallback");
    content
        .iter()
        .enumerate()
        .filter(|(i, b)| match boundary {
            Some(at) if *i < at => {
                !matches!(b["type"].as_str(), Some("thinking" | "redacted_thinking" | "tool_use" | "server_tool_use"))
            }
            _ => true,
        })
        .map(|(_, b)| b.clone())
        .collect()
}

/// A message with image blocks inside tool results replaced by a note.
fn without_images(message: &Value) -> Value {
    let mut m = message.clone();
    if let Some(blocks) = m["content"].as_array_mut() {
        for b in blocks.iter_mut() {
            if let Some(inner) = b["content"].as_array_mut() {
                for part in inner.iter_mut() {
                    if part["type"] == "image" {
                        *part = json!({"type": "text", "text": "[screenshot not kept]"});
                    }
                }
            }
        }
    }
    m
}

/// Removes client-side bookkeeping fields before a block goes back on the wire.
fn strip_bookkeeping(content: &[Value]) -> Vec<Value> {
    content
        .iter()
        .map(|b| {
            let mut b = b.clone();
            if let Some(o) = b.as_object_mut() {
                o.remove("__invalid_json");
            }
            b
        })
        .collect()
}

fn tool_result(id: &Value, content: &str, is_error: bool) -> Value {
    let mut r = json!({"type": "tool_result", "tool_use_id": id, "content": content});
    if is_error {
        r["is_error"] = Value::Bool(true);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_assistant_replies_are_spotted() {
        assert!(is_empty_assistant(&json!({"role": "assistant", "content": []})));
        assert!(!is_empty_assistant(&json!({"role": "assistant", "content": [{"type": "text", "text": "hi"}]})));
        assert!(!is_empty_assistant(&json!({"role": "user", "content": []})));
    }

    #[test]
    fn fallback_echo_drops_pre_boundary_thinking_and_tools() {
        let content = vec![
            json!({"type": "thinking", "thinking": "", "signature": "s"}),
            json!({"type": "text", "text": "partial"}),
            json!({"type": "tool_use", "id": "t1", "name": "x", "input": {}}),
            json!({"type": "fallback", "from": {}, "to": {}}),
            json!({"type": "tool_use", "id": "t2", "name": "x", "input": {}, "__invalid_json": "{"}),
        ];
        let out = strip_bookkeeping(&after_fallback(&content));
        let types: Vec<_> = out.iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(types, vec!["text", "fallback", "tool_use"]);
        assert!(out[2].get("__invalid_json").is_none());
    }

    #[test]
    fn no_fallback_keeps_everything() {
        let content = vec![
            json!({"type": "thinking", "thinking": "", "signature": "s"}),
            json!({"type": "tool_use", "id": "t1", "name": "x", "input": {}}),
        ];
        assert_eq!(after_fallback(&content).len(), 2);
    }

    fn user_text(text: &str) -> Value {
        json!({"role": "user", "content": [{"type": "text", "text": text}]})
    }

    fn calls(ids: &[&str]) -> Value {
        let mut content = vec![json!({"type": "thinking", "thinking": "", "signature": "s"})];
        for id in ids {
            content.push(json!({"type": "tool_use", "id": id, "name": "x", "input": {}}));
        }
        json!({"role": "assistant", "content": content})
    }

    /// Every tool_use in an assistant message is answered exactly once, by
    /// a tool_result in the very next message, which is a user message.
    fn assert_valid(messages: &[Value]) {
        for (i, m) in messages.iter().enumerate() {
            let content = m["content"].as_array().expect("content array");
            assert!(!content.is_empty(), "message {i} is empty");
            if m["role"] != "assistant" {
                continue;
            }
            let ids: Vec<&Value> = content.iter().filter(|b| b["type"] == "tool_use").map(|b| &b["id"]).collect();
            if ids.is_empty() {
                continue;
            }
            let next = &messages[i + 1];
            assert_eq!(next["role"], "user");
            let answered: Vec<&Value> = next["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|b| b["type"] == "tool_result")
                .map(|b| &b["tool_use_id"])
                .collect();
            assert_eq!(answered, ids);
        }
    }

    fn last_text(messages: &[Value]) -> &str {
        let content = messages.last().unwrap()["content"].as_array().unwrap();
        content.last().unwrap()["text"].as_str().unwrap()
    }

    #[test]
    fn stop_before_any_reply_keeps_the_user_message() {
        let mut messages = vec![user_text("hi"), calls(&[]), user_text("send it")];
        close_cancelled_turn(&mut messages, Vec::new(), None);
        assert_eq!(messages.len(), 3);
        let content = messages[2]["content"].as_array().unwrap();
        assert_eq!(content[0]["text"], "send it");
        assert_eq!(last_text(&messages), STOPPED_NOTE);
        assert_valid(&messages);
    }

    #[test]
    fn stop_mid_tools_keeps_results_that_ran() {
        let mut messages = vec![user_text("send both"), calls(&["a", "b", "c"])];
        let finished = vec![tool_result(&json!("a"), "sent", false)];
        close_cancelled_turn(&mut messages, finished, Some("b"));
        assert_eq!(messages.len(), 3);
        assert_valid(&messages);
        let results = messages[2]["content"].as_array().unwrap();
        assert_eq!(results[0]["content"], "sent");
        assert!(results[0].get("is_error").is_none());
        assert_eq!(results[1]["content"], INTERRUPTED);
        assert_eq!(results[1]["is_error"], true);
        assert_eq!(results[2]["content"], NOT_RUN);
        assert_eq!(results[2]["is_error"], true);
        assert_eq!(last_text(&messages), STOPPED_NOTE);
    }

    #[test]
    fn stop_while_model_answers_tool_results() {
        let mut messages = vec![
            user_text("go"),
            calls(&["a"]),
            json!({"role": "user", "content": [tool_result(&json!("a"), "done", false)]}),
        ];
        close_cancelled_turn(&mut messages, Vec::new(), None);
        assert_eq!(messages.len(), 3);
        assert_valid(&messages);
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(last_text(&messages), STOPPED_NOTE);
    }

    #[test]
    fn stop_with_string_content_and_empty_history() {
        let mut messages = vec![json!({"role": "user", "content": "plain"})];
        close_cancelled_turn(&mut messages, Vec::new(), None);
        assert_eq!(messages[0]["content"][0]["text"], "plain");
        assert_eq!(last_text(&messages), STOPPED_NOTE);

        let mut empty = Vec::new();
        close_cancelled_turn(&mut empty, Vec::new(), None);
        assert!(empty.is_empty());
    }
}
