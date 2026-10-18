//! Runs tool calls for every backend: looks the tool up, asks policy,
//! waits for the user when policy says so, executes, and reports each step
//! as an [`AgentEvent`]. The API backend calls this directly; the CLI
//! backends reach it through the local MCP server.

use crate::{AgentEvent, Approval};
use futures::channel::oneshot;
use policy::{Decision, Policy};
use futures::future::BoxFuture;
use router::{Approver, Registry, ToolContext, ToolImage};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct ToolHost {
    registry: Registry,
    policy: Arc<Mutex<Policy>>,
    events: async_channel::Sender<AgentEvent>,
    next_id: Arc<AtomicU64>,
}

/// What goes back to the model.
pub struct Outcome {
    pub content: String,
    pub is_error: bool,
    pub images: Vec<ToolImage>,
}

impl ToolHost {
    pub fn new(registry: Registry, policy: Arc<Mutex<Policy>>, events: async_channel::Sender<AgentEvent>) -> Self {
        Self { registry, policy, events, next_id: Arc::new(AtomicU64::new(1)) }
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn fresh_id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    fn emit(&self, ev: AgentEvent) {
        let _ = self.events.try_send(ev);
    }

    pub async fn execute(&self, id: String, name: &str, input: Value) -> Outcome {
        let name = name.to_string();
        self.emit(AgentEvent::ToolCall { id: id.clone(), name: name.clone(), input: input.clone() });

        let fail = |content: String| {
            self.emit(AgentEvent::ToolResult {
                id: id.clone(),
                name: name.clone(),
                ok: false,
                summary: crate::first_line(&content),
                content: content.clone(),
                evidence: Vec::new(),
            });
            Outcome { content, is_error: true, images: Vec::new() }
        };

        let Some(tool) = self.registry.get(&name) else {
            return fail(format!("No tool named {name}."));
        };

        let decision = self.policy.lock().expect("policy lock").decide(tool.as_ref(), &input);
        if let Decision::Ask { reason } = decision {
            let (tx, rx) = oneshot::channel();
            self.emit(AgentEvent::ApprovalNeeded {
                id: id.clone(),
                name: name.clone(),
                description: tool.describe_call(&input),
                reason,
                respond: tx,
            });
            let approval = rx.await.unwrap_or(Approval::Deny);
            self.emit(AgentEvent::ApprovalDecided { id: id.clone(), name: name.clone(), approval });
            match approval {
                Approval::Deny => return fail("The user declined this action.".into()),
                Approval::AlwaysAllow => {
                    self.policy.lock().expect("policy lock").always_allow(&name, tool.counterparties(&input))
                }
                Approval::Approve => {}
            }
        }

        let ctx = ToolContext { approver: std::sync::Arc::new(self.clone()) };
        match tool.call_with(input, ctx).await {
            Ok(out) => {
                let mut evidence = out.evidence;
                evidence.extend(out.images.iter().cloned());
                self.emit(AgentEvent::ToolResult {
                    id,
                    name,
                    ok: true,
                    summary: out.summary,
                    content: out.content.clone(),
                    evidence,
                });
                Outcome { content: out.content, is_error: false, images: out.images }
            }
            Err(e) => fail(e),
        }
    }

    /// Tool definitions in MCP `tools/list` shape.
    pub fn mcp_tool_list(&self) -> Vec<Value> {
        self.registry
            .definitions(false)
            .into_iter()
            .map(|d| {
                json!({
                    "name": d["name"],
                    "description": d["description"],
                    "inputSchema": d["input_schema"],
                })
            })
            .collect()
    }
}

impl Approver for ToolHost {
    fn approve(
        &self,
        tool: &'static str,
        description: String,
        reason: String,
        counterparties: Vec<String>,
    ) -> BoxFuture<'static, bool> {
        let host = self.clone();
        Box::pin(async move {
            if host.policy.lock().expect("policy lock").is_always_allowed(tool, &counterparties) {
                return true;
            }
            let id = host.fresh_id("approval");
            let (tx, rx) = oneshot::channel();
            host.emit(AgentEvent::ApprovalNeeded {
                id: id.clone(),
                name: tool.to_string(),
                description,
                reason,
                respond: tx,
            });
            let approval = rx.await.unwrap_or(Approval::Deny);
            host.emit(AgentEvent::ApprovalDecided { id, name: tool.to_string(), approval });
            match approval {
                Approval::Deny => false,
                Approval::Approve => true,
                Approval::AlwaysAllow => {
                    host.policy.lock().expect("policy lock").always_allow(tool, counterparties);
                    true
                }
            }
        })
    }
}
