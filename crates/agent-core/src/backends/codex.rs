//! Codex backend: `codex exec --json` per turn, `codex exec resume <thread>`
//! after the first, signed in with the user's own ChatGPT account.
//!
//! Locked down: the shell tool, browser, computer use, apps, plugins and
//! image generation are disabled; the user's `~/.codex/config.toml` is
//! ignored (auth still comes from `CODEX_HOME`), so the only tools are ours.

use super::proc::{JsonLines, Line, died};
use crate::mcp::{McpServer, SERVER_NAME};
use crate::{AgentEvent, Settings, prompt, workspace_dir};
use serde_json::Value;
use tokio::process::Command;

const TOKEN_ENV: &str = "UNIBROW_MCP_TOKEN";
const DISABLED_FEATURES: [&str; 7] =
    ["apps", "browser_use", "computer_use", "image_generation", "plugins", "in_app_browser", "browser_use_external"];

pub struct Codex {
    model: String,
    mcp: McpServer,
    thread_id: Option<String>,
    events: async_channel::Sender<AgentEvent>,
}

impl Codex {
    pub fn new(
        settings: &Settings,
        mcp: McpServer,
        events: async_channel::Sender<AgentEvent>,
        thread_id: Option<String>,
    ) -> Self {
        Self { model: settings.codex_model.clone(), mcp, thread_id, events }
    }

    fn emit(&self, ev: AgentEvent) {
        let _ = self.events.try_send(ev);
    }

    fn command(&self, message: &str) -> Command {
        let mut cmd = Command::new("codex");
        cmd.current_dir(workspace_dir()).arg("exec");
        if let Some(id) = &self.thread_id {
            cmd.args(["resume", id]);
        }
        let s = SERVER_NAME;
        cmd.args(["--json", "--skip-git-repo-check", "--ignore-user-config"])
            .args(["-c", "sandbox_mode=\"read-only\""])
            .args(["-c", "approval_policy=\"never\""])
            .args(["-c", "features.shell_tool=false"])
            .args(["-c", "web_search=\"disabled\""])
            .args(["-c", &format!("mcp_servers.{s}.url=\"{}\"", self.mcp.url)])
            .args(["-c", &format!("mcp_servers.{s}.bearer_token_env_var=\"{TOKEN_ENV}\"")])
            .args(["-c", &format!("mcp_servers.{s}.tool_timeout_sec=1800")])
            .args(["-c", &format!("mcp_servers.{s}.startup_timeout_sec=30")])
            // Codex's own MCP approval would auto-reject under
            // approval_policy=never. Our ToolHost does the gating instead.
            .args(["-c", &format!("mcp_servers.{s}.default_tools_approval_mode=\"approve\"")])
            .env(TOKEN_ENV, &self.mcp.token)
            .stdin(std::process::Stdio::null());
        for f in DISABLED_FEATURES {
            cmd.args(["--disable", f]);
        }
        if !self.model.is_empty() {
            cmd.args(["-m", &self.model]);
        }
        cmd.arg("--").arg(message);
        cmd
    }

    pub async fn run_turn(&mut self, text: String) {
        let message = if self.thread_id.is_none() {
            prompt::with_instructions(&prompt::stamp(&text))
        } else {
            prompt::stamp(&text)
        };
        let mut cmd = self.command(&message);
        let (mut child, mut out) = match JsonLines::spawn(&mut cmd) {
            Ok(x) => x,
            Err(e) => {
                self.emit(AgentEvent::Error(e));
                return;
            }
        };
        self.emit(AgentEvent::TurnStarted);
        let mut finished = false;
        loop {
            let ev = match out.next().await {
                Line::Json(v) => v,
                Line::Eof => break,
            };
            match ev["type"].as_str().unwrap_or("") {
                "thread.started" => {
                    if let Some(id) = ev["thread_id"].as_str() {
                        if self.thread_id.is_none() {
                            self.emit(AgentEvent::Info(format!(
                                "Codex session started via your ChatGPT sign-in (thread {id})"
                            )));
                        }
                        if self.thread_id.as_deref() != Some(id) {
                            self.emit(AgentEvent::Resume(serde_json::json!({"codex_thread": id})));
                        }
                        self.thread_id = Some(id.to_string());
                    }
                }
                "item.completed" => self.on_item(&ev["item"]),
                "turn.completed" => {
                    finished = true;
                    self.emit(AgentEvent::Done {
                        stop_reason: "turn.completed".into(),
                        model: (!self.model.is_empty()).then(|| self.model.clone()),
                        usage: ev["usage"].clone(),
                    });
                }
                "turn.failed" => {
                    finished = true;
                    let msg = ev["error"]["message"].as_str().unwrap_or("Codex turn failed.").to_string();
                    self.emit(AgentEvent::Error(msg));
                }
                "error" => {
                    let msg = ev["message"].as_str().unwrap_or("Codex reported an error.").to_string();
                    self.emit(AgentEvent::Error(msg));
                }
                _ => {}
            }
        }
        let status = child.wait().await.ok();
        if !finished {
            self.emit(AgentEvent::Error(died("Codex", status, &out.stderr_tail())));
        }
    }

    fn on_item(&self, item: &Value) {
        match item["type"].as_str().unwrap_or("") {
            "agent_message" => {
                if let Some(t) = item["text"].as_str() {
                    self.emit(AgentEvent::TurnStarted);
                    self.emit(AgentEvent::TextDelta(t.to_string()));
                }
            }
            "reasoning" => {
                if let Some(t) = item["text"].as_str().filter(|t| !t.trim().is_empty()) {
                    self.emit(AgentEvent::Note(t.trim().to_string()));
                }
            }
            "error" => {
                if let Some(m) = item["message"].as_str() {
                    self.emit(AgentEvent::Error(m.to_string()));
                }
            }
            // mcp_tool_call items are reported by the tool host directly.
            _ => {}
        }
    }
}
