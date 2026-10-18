//! Claude Code backend: one long-lived `claude -p` process per conversation,
//! speaking stream-json on stdin/stdout, signed in with the user's own
//! Claude subscription. The app never reads or reuses that sign-in; it only
//! runs the `claude` command.
//!
//! Locked down: no built-in tools (`--tools ""`), only our MCP server
//! (`--strict-mcp-config`), only our tools allowed, and anything that would
//! prompt is denied (`--permission-prompts none`) so a headless run can't hang.

use super::proc::{JsonLines, Line, died};
use crate::mcp::{McpServer, SERVER_NAME};
use crate::{AgentEvent, Settings, prompt, workspace_dir};
use serde_json::{Value, json};
use std::collections::HashMap;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};

struct Running {
    child: Child,
    stdin: ChildStdin,
    out: JsonLines,
}

pub struct ClaudeCode {
    model: String,
    effort: String,
    mcp: McpServer,
    running: Option<Running>,
    /// Kept so a crashed process can be restarted with `--resume`.
    session_id: Option<String>,
    announced: bool,
    events: async_channel::Sender<AgentEvent>,
}

impl ClaudeCode {
    pub fn new(
        settings: &Settings,
        mcp: McpServer,
        events: async_channel::Sender<AgentEvent>,
        session_id: Option<String>,
    ) -> Self {
        Self {
            model: settings.claude_model.clone(),
            effort: settings.claude_effort.clone(),
            mcp,
            running: None,
            session_id,
            announced: false,
            events,
        }
    }

    fn emit(&self, ev: AgentEvent) {
        let _ = self.events.try_send(ev);
    }

    fn command(&self) -> Command {
        let mcp_config = json!({
            "mcpServers": {
                SERVER_NAME: {
                    "type": "http",
                    "url": self.mcp.url,
                    "headers": {"Authorization": format!("Bearer {}", self.mcp.token)},
                }
            }
        });
        let mut cmd = Command::new("claude");
        cmd.current_dir(workspace_dir())
            .args(["-p", "--input-format", "stream-json", "--output-format", "stream-json"])
            .args(["--verbose", "--include-partial-messages"])
            .args(["--tools", ""])
            .arg("--strict-mcp-config")
            .args(["--mcp-config", &mcp_config.to_string()])
            .args(["--allowedTools", &format!("mcp__{SERVER_NAME}")])
            .args(["--permission-prompts", "none"])
            .args(["--setting-sources", "project"])
            .arg("--disable-slash-commands")
            .arg("--no-chrome")
            .args(["--system-prompt", prompt::SYSTEM_PROMPT])
            .args(["--model", &self.model])
            .args(["--effort", &self.effort])
            // Approval cards can sit open for a while; don't time the tool out.
            .env("MCP_TOOL_TIMEOUT", "1800000")
            .stdin(std::process::Stdio::piped());
        if let Some(id) = &self.session_id {
            cmd.args(["--resume", id]);
        }
        cmd
    }

    async fn ensure_running(&mut self) -> Result<(), String> {
        if self.running.is_some() {
            return Ok(());
        }
        let mut cmd = self.command();
        let (mut child, out) = JsonLines::spawn(&mut cmd)?;
        let stdin = child.stdin.take().ok_or("claude stdin unavailable")?;
        self.running = Some(Running { child, stdin, out });
        Ok(())
    }

    pub async fn run_turn(&mut self, text: String) {
        if let Err(e) = self.ensure_running().await {
            self.emit(AgentEvent::Error(e));
            return;
        }
        let line = json!({
            "type": "user",
            "message": {"role": "user", "content": prompt::stamp(&text)},
        })
        .to_string()
            + "\n";
        let write_ok = {
            let r = self.running.as_mut().expect("running");
            r.stdin.write_all(line.as_bytes()).await.is_ok() && r.stdin.flush().await.is_ok()
        };
        if !write_ok {
            self.crashed().await;
            return;
        }

        // Thinking text per content-block index, flushed as a note on stop.
        let mut thinking: HashMap<u64, String> = HashMap::new();
        let mut model: Option<String> = None;
        loop {
            let next = self.running.as_mut().expect("running").out.next().await;
            let ev = match next {
                Line::Json(v) => v,
                Line::Eof => {
                    self.crashed().await;
                    return;
                }
            };
            match ev["type"].as_str().unwrap_or("") {
                "system" if ev["subtype"] == "init" => {
                    if let Some(id) = ev["session_id"].as_str() {
                        if self.session_id.as_deref() != Some(id) {
                            self.emit(AgentEvent::Resume(json!({"claude_session": id})));
                        }
                        self.session_id = Some(id.to_string());
                    }
                    model = ev["model"].as_str().map(str::to_string);
                    if !self.announced {
                        self.announced = true;
                        let auth = match ev["apiKeySource"].as_str() {
                            Some("none") | None => "your Claude subscription".to_string(),
                            Some(other) => format!("API key from {other}"),
                        };
                        let tools = ev["tools"].as_array().map(|t| t.len()).unwrap_or(0);
                        self.emit(AgentEvent::Info(format!(
                            "Claude Code session: {} via {auth}; {tools} tools from the app",
                            model.as_deref().unwrap_or(&self.model)
                        )));
                    }
                }
                "stream_event" => {
                    let e = &ev["event"];
                    match e["type"].as_str().unwrap_or("") {
                        "message_start" => self.emit(AgentEvent::TurnStarted),
                        "content_block_start" => {
                            if e["content_block"]["type"] == "thinking" {
                                thinking.insert(e["index"].as_u64().unwrap_or(0), String::new());
                            }
                        }
                        "content_block_delta" => {
                            let d = &e["delta"];
                            match d["type"].as_str() {
                                Some("text_delta") => {
                                    if let Some(t) = d["text"].as_str() {
                                        self.emit(AgentEvent::TextDelta(t.to_string()));
                                    }
                                }
                                Some("thinking_delta") => {
                                    let i = e["index"].as_u64().unwrap_or(0);
                                    thinking.entry(i).or_default().push_str(d["thinking"].as_str().unwrap_or(""));
                                }
                                _ => {}
                            }
                        }
                        "content_block_stop" => {
                            let i = e["index"].as_u64().unwrap_or(0);
                            if let Some(t) = thinking.remove(&i).filter(|t| !t.trim().is_empty()) {
                                self.emit(AgentEvent::Note(t.trim().to_string()));
                            }
                        }
                        _ => {}
                    }
                }
                "rate_limit_event" => {
                    let info = &ev["rate_limit_info"];
                    if info["status"].as_str().is_some_and(|s| s != "allowed") {
                        self.emit(AgentEvent::Info(format!(
                            "Claude subscription limit: {} ({})",
                            info["status"].as_str().unwrap_or("?"),
                            info["rateLimitType"].as_str().unwrap_or("?")
                        )));
                    }
                }
                "result" => {
                    let subtype = ev["subtype"].as_str().unwrap_or("result").to_string();
                    if ev["is_error"].as_bool().unwrap_or(false) || subtype != "success" {
                        let msg = ev["result"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("Claude Code ended the turn with `{subtype}`."));
                        self.emit(AgentEvent::Error(msg));
                    }
                    let mut usage = ev["usage"].clone();
                    if let (Some(u), Some(cost)) = (usage.as_object_mut(), ev["total_cost_usd"].as_f64()) {
                        u.insert("equivalent_cost_usd".into(), Value::from(cost));
                    }
                    self.emit(AgentEvent::Done { stop_reason: subtype, model, usage });
                    return;
                }
                _ => {} // assistant/user echoes, status, hooks
            }
        }
    }

    /// Stop: kill the process. The next message restarts it with
    /// `--resume`, so the conversation continues.
    pub fn on_cancel(&mut self) {
        self.running = None; // kill_on_drop
    }

    async fn crashed(&mut self) {
        let msg = match self.running.take() {
            Some(mut r) => {
                let status = r.child.wait().await.ok();
                died("Claude Code", status, &r.out.stderr_tail())
            }
            None => "Claude Code isn't running.".into(),
        };
        if msg.contains("No conversation found") {
            // The saved session is gone; start fresh next time.
            self.session_id = None;
            self.emit(AgentEvent::Info("The saved Claude Code session couldn't be resumed; starting a new one.".into()));
            self.emit(AgentEvent::ResumeFailed);
            return;
        }
        let hint = if msg.contains("login") || msg.contains("auth") {
            "\nRun `claude` in a terminal and sign in, then try again."
        } else {
            "\nYour next message restarts it and resumes this conversation."
        };
        self.emit(AgentEvent::Error(msg + hint));
    }
}
