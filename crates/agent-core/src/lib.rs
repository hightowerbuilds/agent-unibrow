//! The agent: a conversation with a model that acts through the app's tools.
//!
//! Four interchangeable backends:
//! - **Claude Code** (`claude -p`, your Claude subscription)
//! - **Codex** (`codex exec`, your ChatGPT sign-in)
//! - **OpenCode** (`opencode run`, whatever provider you've signed into)
//! - **Anthropic API** (direct Messages API with an API key)
//!
//! The CLI backends reach the app's tools through a local MCP server; their
//! own built-in tools (shell, file editing, browser) are switched off. All
//! tool calls, from any backend, go through [`host::ToolHost`], which applies
//! policy and asks the user before anything with an external effect.
//!
//! Everything runs on a background Tokio thread. The UI talks to it through
//! channels only, so nothing here depends on GPUI.

mod backends;
mod client;
mod host;
mod mcp;
mod prompt;
mod sse;

pub use client::ModelConfig;

use futures::channel::oneshot;
use policy::Policy;
use router::Registry;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    ClaudeCode,
    Codex,
    OpenCode,
    AnthropicApi,
}

impl BackendKind {
    pub const ALL: [BackendKind; 4] =
        [BackendKind::ClaudeCode, BackendKind::Codex, BackendKind::OpenCode, BackendKind::AnthropicApi];

    pub fn label(self) -> &'static str {
        match self {
            BackendKind::ClaudeCode => "Claude Code",
            BackendKind::Codex => "Codex",
            BackendKind::OpenCode => "OpenCode",
            BackendKind::AnthropicApi => "API key",
        }
    }

    pub fn needs_api_key(self) -> bool {
        self == BackendKind::AnthropicApi
    }
}

/// User settings, stored in `~/.config/agent-unibrow/settings.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub backend: BackendKind,
    /// Claude Code model alias or full name (`opus`, `sonnet`, `fable`, …).
    pub claude_model: String,
    pub claude_effort: String,
    /// Codex model; empty uses Codex's default.
    pub codex_model: String,
    /// OpenCode model as `provider/model`. Required for OpenCode.
    pub opencode_model: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            backend: BackendKind::ClaudeCode,
            claude_model: "opus".into(),
            claude_effort: "high".into(),
            codex_model: String::new(),
            opencode_model: String::new(),
        }
    }
}

impl Settings {
    fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("agent-unibrow").join("settings.json"))
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(p) = Self::path() else { return };
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(p, s);
        }
    }
}

/// What the user decided on an approval card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    Approve,
    /// Approve, and stop asking for this tool and these counterparties.
    AlwaysAllow,
    Deny,
}

/// Answers an [`AgentEvent::ApprovalNeeded`]. Dropping it counts as Deny.
pub type ApprovalSender = oneshot::Sender<Approval>;

pub enum AgentEvent {
    /// Backend status worth recording (session started, which account).
    Info(String),
    /// A new model response started.
    TurnStarted,
    /// Reply text (streamed where the backend streams).
    TextDelta(String),
    /// A short progress note or reasoning summary.
    Note(String),
    ToolCall { id: String, name: String, input: Value },
    ToolResult { id: String, name: String, ok: bool, summary: String, content: String },
    ApprovalNeeded { id: String, name: String, description: String, reason: String, respond: ApprovalSender },
    ApprovalDecided { id: String, name: String, approval: Approval },
    /// The turn is over; it is the user's turn.
    Done { stop_reason: String, model: Option<String>, usage: Value },
    Error(String),
}

pub struct AgentHandle {
    tx: async_channel::Sender<String>,
}

impl AgentHandle {
    /// Queues a user message. Returns false if the agent thread has exited.
    pub fn send(&self, text: String) -> bool {
        self.tx.try_send(text).is_ok()
    }
}

/// Starts an agent with the given backend on a background thread.
/// `api_key` is only used by [`BackendKind::AnthropicApi`].
pub fn spawn(
    settings: Settings,
    api_key: Option<String>,
    registry: Registry,
    policy: Arc<Mutex<Policy>>,
) -> (AgentHandle, async_channel::Receiver<AgentEvent>) {
    let (in_tx, in_rx) = async_channel::unbounded::<String>();
    let (ev_tx, ev_rx) = async_channel::unbounded::<AgentEvent>();
    std::thread::Builder::new()
        .name("agent-core".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(async move {
                let host = host::ToolHost::new(registry, policy, ev_tx.clone());
                let mut backend = match backends::Backend::start(&settings, api_key, host, ev_tx.clone()).await {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = ev_tx.try_send(AgentEvent::Error(e));
                        return;
                    }
                };
                while let Ok(text) = in_rx.recv().await {
                    backend.run_turn(text).await;
                }
            });
        })
        .expect("spawn agent thread");
    (AgentHandle { tx: in_tx }, ev_rx)
}

/// Directory the CLI agents run in: empty, so no project files or
/// instructions leak into their context.
pub(crate) fn workspace_dir() -> PathBuf {
    let dir = dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("agent-unibrow")
        .join("workspace");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub(crate) fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.chars().count() > 120 { line.chars().take(117).collect::<String>() + "…" } else { line.to_string() }
}
