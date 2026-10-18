//! Smoke test for a backend without the UI. Registers only the clock tool.
//!
//!   cargo run -p agent-core --example headless -- claude_code "What time is it?"

use agent_core::{AgentEvent, Approval, BackendKind, Settings};
use policy::Policy;
use router::{Effect, Rail, Registry, Tool, ToolOutput, ToolResult};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// A pretend send that always needs approval.
struct FakeSend;
impl Tool for FakeSend {
    fn name(&self) -> &'static str {
        "fake_send_message"
    }
    fn description(&self) -> &'static str {
        "Send a short message to a person. Call this when the user asks you to send a message."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {"to": {"type": "string"}, "text": {"type": "string"}}, "required": ["to", "text"], "additionalProperties": false})
    }
    fn effect(&self) -> Effect {
        Effect::Commit
    }
    fn counterparties(&self, input: &Value) -> Vec<String> {
        input["to"].as_str().map(|s| vec![s.to_string()]).unwrap_or_default()
    }
    fn call(&self, _: Value) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { Ok(ToolOutput::text("sent", "sent")) })
    }
}

struct ClockOnly;
impl Rail for ClockOnly {
    fn name(&self) -> &'static str {
        "clock"
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(rail_api::gmail::CurrentTime), Arc::new(FakeSend)]
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let backend: BackendKind = serde_json::from_value(serde_json::Value::String(args.next().unwrap_or("claude_code".into())))
        .expect("backend: claude_code | codex | open_code | anthropic_api");
    let mut messages: Vec<String> = args.collect();
    if messages.is_empty() {
        messages.push("What time is it right now? Use the tool.".into());
    }

    let mut settings = Settings::load();
    settings.backend = backend;
    let mut registry = Registry::new();
    registry.add_rail(&ClockOnly);
    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let (handle, events) = agent_core::spawn(settings, api_key, registry, Arc::new(Mutex::new(Policy::in_memory())));
    futures::executor::block_on(async move {
      for message in messages {
        println!("\n>>> {message}");
        handle.send(message);
        while let Ok(ev) = events.recv().await {
            match ev {
                AgentEvent::Info(s) => println!("[info] {s}"),
                AgentEvent::TurnStarted => {}
                AgentEvent::TextDelta(t) => print!("{t}"),
                AgentEvent::Note(n) => println!("\n[note] {n}"),
                AgentEvent::ToolCall { name, input, .. } => println!("\n[call] {name} {input}"),
                AgentEvent::ToolResult { name, ok, summary, .. } => println!("[result] {name} ok={ok}: {summary}"),
                AgentEvent::ApprovalNeeded { respond, description, .. } => {
                    println!("[approval] {description} -- waiting 8s, then denying");
                    std::thread::sleep(std::time::Duration::from_secs(8));
                    let _ = respond.send(Approval::Deny);
                }
                AgentEvent::ApprovalDecided { .. } => {}
                AgentEvent::Done { stop_reason, model, usage } => {
                    println!("\n[done] {stop_reason} model={model:?} usage={usage}");
                    break;
                }
                AgentEvent::Error(e) => {
                    println!("\n[error] {e}");
                    break;
                }
            }
        }
      }
    });
}
