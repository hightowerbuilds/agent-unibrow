//! Runs tool calls for every backend: looks the tool up, asks policy,
//! waits for the user when policy says so, executes, and reports each step
//! as an [`AgentEvent`]. The API backend calls this directly; the CLI
//! backends reach it through the local MCP server.

use crate::{AgentEvent, Approval};
use futures::channel::oneshot;
use policy::{Decision, Policy, TurnBudget};
use futures::future::BoxFuture;
use router::{ApprovalRequest, Approver, Effect, Registry, ToolContext, ToolImage};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Messages the user typed while the agent works, waiting for the next
/// tool result to carry them to the model (chats roadmap C3).
#[derive(Clone, Default)]
pub struct Steering(Arc<(Mutex<Vec<String>>, tokio::sync::Notify)>);

impl Steering {
    pub fn push(&self, text: String) {
        self.0.0.lock().unwrap_or_else(|e| e.into_inner()).push(text);
        // Wakes an idle agent: a message typed just as a turn ended.
        self.0.1.notify_one();
    }

    /// Everything queued, oldest first; the queue is left empty.
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.0.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Resolves once something has been pushed (maybe already taken).
    pub async fn pushed(&self) {
        self.0.1.notified().await;
    }
}

/// Tools outside the per-turn call cap: they only touch the app's own
/// bookkeeping, never a site or an account.
const UNCAPPED: &[&str] = &["task_progress"];

#[derive(Clone)]
pub struct ToolHost {
    registry: Registry,
    policy: Arc<Mutex<Policy>>,
    events: async_channel::Sender<AgentEvent>,
    next_id: Arc<AtomicU64>,
    steering: Steering,
    /// Calls and committing actions used this turn (6.6).
    budget: Arc<Mutex<TurnBudget>>,
}

/// What goes back to the model.
pub struct Outcome {
    pub content: String,
    pub is_error: bool,
    pub images: Vec<ToolImage>,
}

impl ToolHost {
    pub fn new(
        registry: Registry,
        policy: Arc<Mutex<Policy>>,
        events: async_channel::Sender<AgentEvent>,
        steering: Steering,
    ) -> Self {
        Self { registry, policy, events, next_id: Arc::new(AtomicU64::new(1)), steering, budget: Arc::default() }
    }

    /// A new user turn: the per-turn caps start over.
    pub fn new_turn(&self) {
        self.budget.lock().unwrap_or_else(|e| e.into_inner()).reset();
    }

    fn budget(&self) -> std::sync::MutexGuard<'_, TurnBudget> {
        self.budget.lock().unwrap_or_else(|e| e.into_inner())
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

    /// Runs a tool call. Anything the user typed meanwhile rides along
    /// with the result, so the model reads it before its next step.
    pub async fn execute(&self, id: String, name: &str, input: Value) -> Outcome {
        let mut out = self.execute_call(id, name, input).await;
        let queued = self.steering.take();
        if !queued.is_empty() {
            out.content.push_str(&crate::prompt::steer_note(&queued));
            self.emit(AgentEvent::Steered { count: queued.len() });
        }
        out
    }

    async fn execute_call(&self, id: String, name: &str, input: Value) -> Outcome {
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

        // Per-turn caps (6.6). The task checklist stays usable after the
        // call cap, so the agent can note where it stopped (7.4).
        let limits = self.policy.lock().expect("policy lock").limits();
        if UNCAPPED.contains(&name.as_str()) {
            // Neither counted nor refused.
        } else if let Err(msg) = self.budget().start_call(&limits) {
            self.emit(AgentEvent::Info(crate::first_line(&msg)));
            return fail(msg);
        }
        self.budget().take_commit_blocked();
        let commits = tool.effect() == Effect::Commit;
        if commits {
            if let Err(msg) = self.budget().check_commit(&limits) {
                self.budget().take_commit_blocked();
                self.emit(AgentEvent::Info(crate::first_line(&msg)));
                return fail(msg);
            }
        }

        // Site scope (6.3): acting outside the focus sites asks.
        let site = tool.site_acted_on(&input).await;
        let scope_reason =
            site.as_deref().and_then(|s| self.policy.lock().expect("policy lock").scope_check(&name, s));

        let decision = self.policy.lock().expect("policy lock").decide(tool.as_ref(), &input);
        let reason = match (decision, &scope_reason) {
            (Decision::Ask { reason }, Some(scope)) => Some(format!("{reason}; also {scope}")),
            (Decision::Ask { reason }, None) => Some(reason),
            (Decision::Allow, Some(scope)) => Some(scope.clone()),
            (Decision::Allow, None) => None,
        };
        if let Some(reason) = reason {
            let (tx, rx) = oneshot::channel();
            self.emit(AgentEvent::ApprovalNeeded {
                id: id.clone(),
                name: name.clone(),
                description: tool.describe_call(&input),
                reason,
                image: None,
                respond: tx,
            });
            let approval = rx.await.unwrap_or(Approval::Deny);
            self.emit(AgentEvent::ApprovalDecided { id: id.clone(), name: name.clone(), approval });
            match approval {
                Approval::Deny => return fail("The user declined this action.".into()),
                Approval::AlwaysAllow => {
                    let mut policy = self.policy.lock().expect("policy lock");
                    if tool.effect() == Effect::Commit {
                        policy.always_allow(&name, tool.counterparties(&input));
                    }
                    if let (Some(site), Some(_)) = (&site, &scope_reason) {
                        policy.allow_site(site);
                    }
                }
                Approval::Approve => {}
            }
        }
        if commits {
            self.budget().record_commit();
        }

        let ctx = ToolContext { approver: std::sync::Arc::new(self.clone()) };
        match tool.call_with(input, ctx).await {
            Ok(out) => {
                // Prompt-injection warnings get their own trace line (6.5).
                if let Some(line) = out.content.lines().find(|l| l.contains(router::INJECTION_MARK)) {
                    self.emit(AgentEvent::Info(crate::first_line(line.trim())));
                }
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
            Err(e) => {
                // A mid-call approval refused by the commit cap: say so,
                // rather than "the user declined".
                if self.budget().take_commit_blocked() {
                    let msg = policy::limits::commit_cap_message(&limits);
                    self.emit(AgentEvent::Info(crate::first_line(&msg)));
                    return fail(msg);
                }
                fail(e)
            }
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
        image: Option<ToolImage>,
    ) -> BoxFuture<'static, bool> {
        self.approve_request(ApprovalRequest { tool, description, reason, counterparties, image, purchase: None })
    }

    /// Mid-call approvals: these are committing actions (a click on "Place
    /// order"), so they count toward the per-turn cap, and a purchase over
    /// the user's limit asks even where "always allow" applies.
    fn approve_request(&self, req: ApprovalRequest) -> BoxFuture<'static, bool> {
        let host = self.clone();
        Box::pin(async move {
            let ApprovalRequest { tool, mut description, mut reason, counterparties, image, purchase } = req;
            let limits = host.policy.lock().expect("policy lock").limits();
            if host.budget().check_commit(&limits).is_err() {
                return false;
            }
            let (allowed, over_limit, limit) = {
                let p = host.policy.lock().expect("policy lock");
                let over = purchase.as_ref().is_some_and(|pu| p.purchase_needs_user(pu.amount));
                (p.is_always_allowed(tool, &counterparties), over, p.purchase_limit())
            };
            if let Some(pu) = &purchase {
                let shown = pu.shown.clone().unwrap_or_else(|| "amount not found on the page".into());
                description = format!("Amount: {shown}\n{description}");
                reason = format!("Purchase: {reason}");
                if over_limit {
                    let limit = limit.map(|l| format!("${l:.2}")).unwrap_or_default();
                    reason.push_str(&format!(" (over your {limit} limit for approving purchases automatically)"));
                }
            }
            if allowed && !over_limit {
                host.budget().record_commit();
                return true;
            }
            let id = host.fresh_id("approval");
            let (tx, rx) = oneshot::channel();
            host.emit(AgentEvent::ApprovalNeeded {
                id: id.clone(),
                name: tool.to_string(),
                description,
                reason,
                image,
                respond: tx,
            });
            let approval = rx.await.unwrap_or(Approval::Deny);
            host.emit(AgentEvent::ApprovalDecided { id, name: tool.to_string(), approval });
            let ok = match approval {
                Approval::Deny => false,
                Approval::Approve => true,
                Approval::AlwaysAllow => {
                    host.policy.lock().expect("policy lock").always_allow(tool, counterparties);
                    true
                }
            };
            if ok {
                host.budget().record_commit();
            }
            ok
        })
    }
}
