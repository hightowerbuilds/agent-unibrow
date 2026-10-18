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
    ) -> Result<Self, String> {
        if settings.backend == BackendKind::AnthropicApi {
            let key = api_key.ok_or("No Anthropic API key set.")?;
            return Ok(Backend::Api(api::ApiBackend::new(key, host, events)));
        }
        let server = mcp::start(host)
            .await
            .map_err(|e| format!("Couldn't start the local tool server: {e}"))?;
        Ok(match settings.backend {
            BackendKind::ClaudeCode => Backend::ClaudeCode(claude_code::ClaudeCode::new(settings, server, events)),
            BackendKind::Codex => Backend::Codex(codex::Codex::new(settings, server, events)),
            BackendKind::OpenCode => Backend::OpenCode(opencode::OpenCode::new(settings, server, events)?),
            BackendKind::AnthropicApi => unreachable!(),
        })
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
