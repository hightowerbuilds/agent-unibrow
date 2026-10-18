//! OpenCode backend: `opencode run --format json` per turn, continuing the
//! same session afterwards. Uses whatever provider the user has signed into
//! with `opencode auth login`. (Not a Claude subscription: Anthropic doesn't
//! allow subscription sign-ins in third-party tools.)
//!
//! Locked down through a generated config file (`OPENCODE_CONFIG`): every
//! built-in tool is disabled and denied, and the only MCP server is ours.

use super::proc::{JsonLines, Line, died};
use crate::mcp::{McpServer, SERVER_NAME};
use crate::{AgentEvent, Settings, prompt, workspace_dir};
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::process::Command;

const BUILTIN_TOOLS: [&str; 14] = [
    "bash", "edit", "write", "patch", "read", "glob", "grep", "list", "webfetch", "websearch", "task", "todowrite",
    "todoread", "skill",
];

pub struct OpenCode {
    model: String,
    config_path: PathBuf,
    _mcp: McpServer,
    session_id: Option<String>,
    events: async_channel::Sender<AgentEvent>,
}

impl OpenCode {
    pub fn new(
        settings: &Settings,
        mcp: McpServer,
        events: async_channel::Sender<AgentEvent>,
        session_id: Option<String>,
    ) -> Result<Self, String> {
        if settings.opencode_model.trim().is_empty() {
            return Err("OpenCode needs a model. Sign in to a provider with `opencode auth login`, then set \
                 \"opencode_model\" (as provider/model, from `opencode models`) in \
                 ~/.config/agent-unibrow/settings.json."
                .into());
        }
        let tools: serde_json::Map<String, Value> =
            BUILTIN_TOOLS.iter().map(|t| (t.to_string(), Value::Bool(false))).collect();
        let config = json!({
            "$schema": "https://opencode.ai/config.json",
            "mcp": {
                SERVER_NAME: {
                    "type": "remote",
                    "url": mcp.url,
                    "headers": {"Authorization": format!("Bearer {}", mcp.token)},
                    "enabled": true,
                }
            },
            "tools": tools,
            "permission": {"bash": "deny", "edit": "deny", "webfetch": "deny"},
        });
        // Holds the per-launch token, so it's written owner-only.
        let config_path = workspace_dir().join(".opencode-unibrow.json");
        write_private(&config_path, &config.to_string()).map_err(|e| format!("Couldn't write OpenCode config: {e}"))?;
        Ok(Self { model: settings.opencode_model.clone(), config_path, _mcp: mcp, session_id, events })
    }

    fn emit(&self, ev: AgentEvent) {
        let _ = self.events.try_send(ev);
    }

    pub async fn run_turn(&mut self, text: String) {
        let message = if self.session_id.is_none() {
            prompt::with_instructions(&prompt::stamp(&text))
        } else {
            prompt::stamp(&text)
        };
        let dir = workspace_dir();
        let mut cmd = Command::new("opencode");
        cmd.current_dir(&dir)
            .env("OPENCODE_CONFIG", &self.config_path)
            .args(["run", "--format", "json"])
            .arg("--dir")
            .arg(&dir)
            .args(["-m", &self.model])
            .stdin(std::process::Stdio::null());
        if let Some(id) = &self.session_id {
            cmd.args(["--session", id]);
        }
        cmd.arg("--").arg(&message);

        let (mut child, mut out) = match JsonLines::spawn(&mut cmd) {
            Ok(x) => x,
            Err(e) => {
                self.emit(AgentEvent::Error(e));
                return;
            }
        };
        self.emit(AgentEvent::TurnStarted);
        let mut errored = false;
        let mut last_reason = String::new();
        loop {
            let ev = match out.next().await {
                Line::Json(v) => v,
                Line::Eof => break,
            };
            if let Some(id) = ev["sessionID"].as_str() {
                if self.session_id.is_none() {
                    self.emit(AgentEvent::Info(format!("OpenCode session {id} on {}", self.model)));
                }
                if self.session_id.as_deref() != Some(id) {
                    self.emit(AgentEvent::Resume(json!({"opencode_session": id})));
                }
                self.session_id = Some(id.to_string());
            }
            let part = &ev["part"];
            match ev["type"].as_str().unwrap_or("") {
                "text" => {
                    if let Some(t) = part["text"].as_str() {
                        self.emit(AgentEvent::TurnStarted);
                        self.emit(AgentEvent::TextDelta(t.to_string()));
                    }
                }
                "reasoning" => {
                    if let Some(t) = part["text"].as_str().filter(|t| !t.trim().is_empty()) {
                        self.emit(AgentEvent::Note(t.trim().to_string()));
                    }
                }
                "step_finish" => {
                    last_reason = part["reason"].as_str().unwrap_or("").to_string();
                }
                "error" => {
                    errored = true;
                    let msg = ev["error"]["data"]["message"]
                        .as_str()
                        .or_else(|| ev["error"]["message"].as_str())
                        .unwrap_or("OpenCode reported an error.");
                    self.emit(AgentEvent::Error(msg.to_string()));
                }
                _ => {} // step_start, tool_use (reported by the tool host)
            }
        }
        let status = child.wait().await.ok();
        if status.is_some_and(|s| s.success()) && !errored {
            self.emit(AgentEvent::Done {
                stop_reason: if last_reason.is_empty() { "stop".into() } else { last_reason },
                model: Some(self.model.clone()),
                usage: Value::Null,
            });
        } else if !errored {
            self.emit(AgentEvent::Error(died("OpenCode", status, &out.stderr_tail())));
        } else {
            self.emit(AgentEvent::Done { stop_reason: "error".into(), model: Some(self.model.clone()), usage: Value::Null });
        }
    }
}

fn write_private(path: &PathBuf, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(contents.as_bytes())
}
