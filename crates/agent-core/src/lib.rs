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
pub mod models;
mod prompt;
mod sse;

pub use client::ModelConfig;
pub use host::Steering;

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
    /// Anthropic API model id.
    pub api_model: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            backend: BackendKind::ClaudeCode,
            claude_model: "opus".into(),
            claude_effort: "high".into(),
            codex_model: String::new(),
            opencode_model: String::new(),
            api_model: ModelConfig::default().model,
        }
    }
}

impl Settings {
    /// The model chosen for a backend.
    pub fn model(&self, kind: BackendKind) -> &str {
        match kind {
            BackendKind::ClaudeCode => &self.claude_model,
            BackendKind::Codex => &self.codex_model,
            BackendKind::OpenCode => &self.opencode_model,
            BackendKind::AnthropicApi => &self.api_model,
        }
    }

    pub fn set_model(&mut self, kind: BackendKind, id: String) {
        match kind {
            BackendKind::ClaudeCode => self.claude_model = id,
            BackendKind::Codex => self.codex_model = id,
            BackendKind::OpenCode => self.opencode_model = id,
            BackendKind::AnthropicApi => self.api_model = id,
        }
    }

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
    ToolResult {
        id: String,
        name: String,
        ok: bool,
        summary: String,
        content: String,
        /// Screenshots and other images for the trace.
        evidence: Vec<router::ToolImage>,
    },
    ApprovalNeeded {
        id: String,
        name: String,
        description: String,
        reason: String,
        /// Screenshot of what's about to happen, when the tool has one.
        image: Option<router::ToolImage>,
        respond: ApprovalSender,
    },
    ApprovalDecided { id: String, name: String, approval: Approval },
    /// The turn is over; it is the user's turn.
    Done { stop_reason: String, model: Option<String>, usage: Value },
    /// What the backend needs to pick this conversation up again later
    /// (session or thread id, or the API message list). Save it with the
    /// chat and pass it back to [`spawn`].
    Resume(Value),
    /// The saved conversation couldn't be resumed (deleted session). The
    /// backend has started fresh; resend with a recap.
    ResumeFailed,
    /// Messages the user typed while the agent worked reached the model.
    Steered { count: usize },
    Error(String),
}

pub struct AgentHandle {
    tx: async_channel::Sender<String>,
    cancel: async_channel::Sender<()>,
    steering: Steering,
}

impl AgentHandle {
    /// Queues a user message. Returns false if the agent thread has exited.
    pub fn send(&self, text: String) -> bool {
        self.tx.try_send(text).is_ok()
    }

    /// Adds to the turn in progress (chats roadmap C3): the message reaches
    /// the model with its next tool result, or as the next turn if it's
    /// only writing its answer. [`AgentEvent::Steered`] says when.
    pub fn steer(&self, text: String) {
        self.steering.push(text);
    }

    /// Stops the turn in progress, if any. The agent reports
    /// `Done { stop_reason: "cancelled" }` once it has stopped.
    pub fn cancel(&self) {
        let _ = self.cancel.try_send(());
    }
}

/// Starts an agent with the given backend on a background thread.
/// `api_key` is only used by [`BackendKind::AnthropicApi`]. `resume` is a
/// value from an earlier [`AgentEvent::Resume`] for the same backend.
pub fn spawn(
    settings: Settings,
    api_key: Option<String>,
    registry: Registry,
    policy: Arc<Mutex<Policy>>,
    resume: Option<Value>,
) -> (AgentHandle, async_channel::Receiver<AgentEvent>) {
    let steering = Steering::default();
    let steering_for_agent = steering.clone();
    let (in_tx, in_rx) = async_channel::unbounded::<String>();
    let (ev_tx, ev_rx) = async_channel::unbounded::<AgentEvent>();
    let (cancel_tx, cancel_rx) = async_channel::bounded::<()>(1);
    std::thread::Builder::new()
        .name("agent-core".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(async move {
                let steering = steering_for_agent;
                let host = host::ToolHost::new(registry, policy, ev_tx.clone(), steering.clone());
                let turns = host.clone();
                let mut backend = match backends::Backend::start(&settings, api_key, host, ev_tx.clone(), resume).await {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = ev_tx.try_send(AgentEvent::Error(e));
                        return;
                    }
                };
                loop {
                    let text = tokio::select! {
                        t = in_rx.recv() => match t {
                            Ok(t) => t,
                            Err(_) => break,
                        },
                        // Steering that arrived after the last turn ended.
                        _ = steering.pushed() => {
                            let queued = steering.take();
                            if queued.is_empty() {
                                continue;
                            }
                            let _ = ev_tx.try_send(AgentEvent::Steered { count: queued.len() });
                            prompt::steer_turn(&queued)
                        }
                    };
                    // A Stop pressed while idle shouldn't cancel the next turn.
                    while cancel_rx.try_recv().is_ok() {}
                    let mut next = Some(text);
                    while let Some(text) = next.take() {
                        // Each user message gets fresh per-turn caps (6.6).
                        turns.new_turn();
                        let cancelled = tokio::select! {
                            _ = backend.run_turn(text) => false,
                            _ = cancel_rx.recv() => true,
                        };
                        if cancelled {
                            // Undelivered steering is the UI's to resend.
                            steering.take();
                            backend.on_cancel().await;
                            let _ = ev_tx.try_send(AgentEvent::Done {
                                stop_reason: "cancelled".into(),
                                model: None,
                                usage: Value::Null,
                            });
                            break;
                        }
                        // Typed while it was writing its answer: send now.
                        let queued = steering.take();
                        if !queued.is_empty() {
                            let _ = ev_tx.try_send(AgentEvent::Steered { count: queued.len() });
                            next = Some(prompt::steer_turn(&queued));
                        }
                    }
                }
            });
        })
        .expect("spawn agent thread");
    (AgentHandle { tx: in_tx, cancel: cancel_tx, steering }, ev_rx)
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
