//! Tool and rail definitions, and the registry the agent draws tools from.
//!
//! Rails depend on this crate to implement [`Tool`]; this crate depends on no
//! rail. The shell constructs each rail and registers it, so dependencies
//! point one way: shell → agent-core → router ← rails.

use futures::future::BoxFuture;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Starts the page note that warns the model about instructions planted in
/// a page (roadmap 6.5). The tool host copies such lines to the trace.
pub const INJECTION_MARK: &str = "Prompt-injection warning:";

/// How risky a call is. Policy decides what to do with it; tools only report it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effect {
    /// Reads data. Never needs approval.
    Read,
    /// Creates something the user can review before it matters (a draft).
    Stage,
    /// Has an external effect that can't be taken back (sending, buying).
    Commit,
}

/// An image attached to a tool result.
#[derive(Clone, Debug)]
pub struct ToolImage {
    /// `image/png` or `image/jpeg`.
    pub mime: String,
    pub base64: String,
}

/// What a tool hands back to the model.
#[derive(Clone, Debug, Default)]
pub struct ToolOutput {
    /// Text returned to the model as the tool_result.
    pub content: String,
    /// One line for the trace pane.
    pub summary: String,
    /// Images the model should see (e.g. a screenshot it asked for).
    pub images: Vec<ToolImage>,
    /// Images kept for the trace only (before/after screenshots).
    pub evidence: Vec<ToolImage>,
}

impl ToolOutput {
    pub fn text(content: impl Into<String>, summary: impl Into<String>) -> Self {
        Self { content: content.into(), summary: summary.into(), ..Default::default() }
    }
}

/// Lets a tool ask the user to approve something it only discovers while
/// running (for example, that the button it was asked to click says
/// "Place order"). Implemented by the agent's tool host.
pub trait Approver: Send + Sync {
    /// Resolves to true if the user approves (or has pre-approved this tool
    /// for these counterparties).
    /// `image` is shown on the approval card (e.g. a screenshot of the form
    /// about to be submitted).
    fn approve(
        &self,
        tool: &'static str,
        description: String,
        reason: String,
        counterparties: Vec<String>,
        image: Option<ToolImage>,
    ) -> BoxFuture<'static, bool>;

    /// Like [`Approver::approve`], with more detail (a purchase amount, so
    /// a spending limit can override "always allow").
    fn approve_request(&self, req: ApprovalRequest) -> BoxFuture<'static, bool> {
        self.approve(req.tool, req.description, req.reason, req.counterparties, req.image)
    }
}

/// A purchase or payment an approval is for (roadmap 6.6). The amount is
/// what the tool could read off the page, the button or the form.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Purchase {
    /// The amount in the page's currency, if one was found.
    pub amount: Option<f64>,
    /// The amount as shown ("$129.00"), with where it came from.
    pub shown: Option<String>,
}

/// Everything an approval card needs. See [`Approver::approve_request`].
#[derive(Clone, Debug)]
pub struct ApprovalRequest {
    pub tool: &'static str,
    pub description: String,
    pub reason: String,
    pub counterparties: Vec<String>,
    pub image: Option<ToolImage>,
    /// Set when the action buys or pays for something.
    pub purchase: Option<Purchase>,
}

/// Passed to [`Tool::call_with`].
#[derive(Clone)]
pub struct ToolContext {
    pub approver: std::sync::Arc<dyn Approver>,
}

pub type ToolResult = Result<ToolOutput, String>;

pub trait Tool: Send + Sync {
    /// Name the model calls the tool by. Must match `^[a-zA-Z0-9_-]{1,64}$`.
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// JSON Schema for the input object.
    fn input_schema(&self) -> Value;
    fn effect(&self) -> Effect;
    /// A one-line description of a specific call, shown on approval cards.
    fn describe_call(&self, input: &Value) -> String {
        format!("{} {}", self.name(), input)
    }
    /// Recipients or counterparties of a call, for policy allowlists.
    fn counterparties(&self, _input: &Value) -> Vec<String> {
        Vec::new()
    }
    /// The site (host without `www.`) this call acts on: clicks, typing and
    /// form fills on the current page. `None` for calls that only read or
    /// navigate, or don't touch a website. The user's site scope (6.3)
    /// applies to calls that return a site; browser tools that change a page
    /// must implement this.
    fn site_acted_on(&self, _input: &Value) -> BoxFuture<'static, Option<String>> {
        Box::pin(async { None })
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult>;
    /// Like [`Tool::call`], with access to mid-call approval. Tools that only
    /// need up-front policy can ignore this.
    fn call_with(&self, input: Value, _ctx: ToolContext) -> BoxFuture<'static, ToolResult> {
        self.call(input)
    }
}

/// A source of tools: an API rail, a commerce protocol, the browser.
pub trait Rail: Send + Sync {
    fn name(&self) -> &'static str;
    fn tools(&self) -> Vec<Arc<dyn Tool>>;
}

#[derive(Default, Clone)]
pub struct Registry {
    tools: BTreeMap<&'static str, Arc<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_rail(&mut self, rail: &dyn Rail) {
        for tool in rail.tools() {
            self.add_tool(tool);
        }
    }

    pub fn add_tool(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name();
        assert!(
            self.tools.insert(name, tool).is_none(),
            "two tools registered as {name}"
        );
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Tool definitions for the Messages API, in a stable order so the
    /// request prefix stays cacheable.
    pub fn definitions(&self, eager_input_streaming: bool) -> Vec<Value> {
        self.tools
            .values()
            .map(|t| {
                let mut def = serde_json::json!({
                    "name": t.name(),
                    "description": t.description(),
                    "input_schema": t.input_schema(),
                });
                if eager_input_streaming {
                    def["eager_input_streaming"] = Value::Bool(true);
                }
                def
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
