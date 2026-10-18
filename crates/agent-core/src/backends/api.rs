//! Direct Messages API backend (API key). Owns the tool loop itself.

use crate::client::{ApiError, Client, Live, ModelConfig};
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
    /// History length before the current turn, to undo a cancelled turn.
    turn_start: usize,
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
        Self {
            client: Client::new(api_key, config),
            tools: host.registry().definitions(true),
            host,
            turn_start: messages.len(),
            messages,
            events,
        }
    }

    fn emit(&self, ev: AgentEvent) {
        let _ = self.events.try_send(ev);
    }

    /// Stop: drop the unfinished turn so history stays a valid alternation
    /// (no tool_use without its tool_result). Only the tail is removed.
    pub fn on_cancel(&mut self) {
        self.messages.truncate(self.turn_start);
    }

    pub async fn run_turn(&mut self, text: String) {
        self.run_steps(text).await;
        // Saved with the chat. Images are left out; they're large and the
        // model doesn't need old screenshots to continue.
        let history: Vec<Value> = self.messages.iter().map(without_images).collect();
        self.emit(AgentEvent::Resume(json!({"api_messages": history})));
    }

    async fn run_steps(&mut self, text: String) {
        self.turn_start = self.messages.len();
        self.messages.push(json!({
            "role": "user",
            "content": [{"type": "text", "text": prompt::stamp(&text)}],
        }));
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
                    let msg = match e {
                        ApiError::Fatal(m) => m,
                        ApiError::Retryable(m) => format!("{m} (gave up after retries)"),
                    };
                    self.emit(AgentEvent::Error(msg));
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
            self.messages.push(json!({"role": "assistant", "content": strip_bookkeeping(&kept)}));

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

            let mut results = Vec::with_capacity(tool_uses.len());
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
                    });
                    results.push(tool_result(&block["id"], &content, true));
                    continue;
                }
                let out = self.host.execute(id, name, block["input"].clone()).await;
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
                results.push(result);
            }
            // Every result for this assistant turn goes back in one message.
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
}
