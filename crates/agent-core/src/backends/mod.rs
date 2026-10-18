//! The four ways to run the agent. Each turns a user message into
//! [`AgentEvent`](crate::AgentEvent)s; tool calls all go through the shared
//! [`ToolHost`](crate::host::ToolHost).

mod api;
mod claude_code;
mod codex;
mod opencode;
mod proc;

use crate::host::ToolHost;
use crate::{AgentEvent, BackendKind, Settings, mcp};

pub enum Backend {
    Api(api::ApiBackend),
    ClaudeCode(claude_code::ClaudeCode),
    Codex(codex::Codex),
    OpenCode(opencode::OpenCode),
}

impl Backend {
    pub async fn start(
        settings: &Settings,
        api_key: Option<String>,
        host: ToolHost,
        events: async_channel::Sender<AgentEvent>,
        resume: Option<serde_json::Value>,
    ) -> Result<Self, String> {
        let resume_str = |key: &str| resume.as_ref().and_then(|r| r[key].as_str()).map(str::to_string);
        if settings.backend == BackendKind::AnthropicApi {
            let key = api_key.ok_or("No Anthropic API key set.")?;
            let messages = resume.as_ref().and_then(|r| r["api_messages"].as_array().cloned()).unwrap_or_default();
            return Ok(Backend::Api(api::ApiBackend::new(key, host, events, messages)));
        }
        let server = mcp::start(host)
            .await
            .map_err(|e| format!("Couldn't start the local tool server: {e}"))?;
        Ok(match settings.backend {
            BackendKind::ClaudeCode => {
                Backend::ClaudeCode(claude_code::ClaudeCode::new(settings, server, events, resume_str("claude_session")))
            }
            BackendKind::Codex => Backend::Codex(codex::Codex::new(settings, server, events, resume_str("codex_thread"))),
            BackendKind::OpenCode => {
                Backend::OpenCode(opencode::OpenCode::new(settings, server, events, resume_str("opencode_session"))?)
            }
            BackendKind::AnthropicApi => unreachable!(),
        })
    }

    /// Cleans up after a turn was cut short by Stop.
    pub async fn on_cancel(&mut self) {
        match self {
            Backend::Api(b) => b.on_cancel(),
            Backend::ClaudeCode(b) => b.on_cancel(),
            // Their per-turn processes were dropped (and killed) with the turn.
            Backend::Codex(_) | Backend::OpenCode(_) => {}
        }
    }

    pub async fn run_turn(&mut self, text: String) {
        match self {
            Backend::Api(b) => b.run_turn(text).await,
            Backend::ClaudeCode(b) => b.run_turn(text).await,
            Backend::Codex(b) => b.run_turn(text).await,
            Backend::OpenCode(b) => b.run_turn(text).await,
        }
    }
}
