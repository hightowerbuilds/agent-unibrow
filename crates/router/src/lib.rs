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

/// A change a tool made to the user's files, for the change log: which
/// message caused it (blame) and how to take it back (undo). Hashes are the
/// SHA-256 of the contents, in hex.
#[derive(Clone, Debug, PartialEq)]
pub enum FileChange {
    /// A new file.
    Created { path: std::path::PathBuf, hash: String },
    /// A file's contents replaced. `before` is the old contents (`None` if
    /// it was too big to keep, which can't be undone).
    Replaced { path: std::path::PathBuf, before: Option<Vec<u8>>, hash: String },
    /// A new folder; `top` is the highest one of the folders it made.
    MadeDir { path: std::path::PathBuf, top: std::path::PathBuf },
    Moved { from: std::path::PathBuf, to: std::path::PathBuf },
    /// Moved to the Trash, to `trashed_as` (`None`: another drive's Trash,
    /// through `gio`).
    Trashed { path: std::path::PathBuf, trashed_as: Option<std::path::PathBuf> },
    /// An empty folder removed (undoing a new folder).
    RemovedDir { path: std::path::PathBuf },
}

impl FileChange {
    /// The path it leaves changed (a move's destination).
    pub fn path(&self) -> &std::path::Path {
        match self {
            FileChange::Created { path, .. }
            | FileChange::Replaced { path, .. }
            | FileChange::MadeDir { path, .. }
            | FileChange::Trashed { path, .. }
            | FileChange::RemovedDir { path } => path,
            FileChange::Moved { to, .. } => to,
        }
    }

    /// A short name for the kind of change ("created", "moved"...).
    pub fn op(&self) -> &'static str {
        match self {
            FileChange::Created { .. } => "created",
            FileChange::Replaced { .. } => "replaced",
            FileChange::MadeDir { .. } => "made_dir",
            FileChange::Moved { .. } => "moved",
            FileChange::Trashed { .. } => "trashed",
            FileChange::RemovedDir { .. } => "removed_dir",
        }
    }
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
    /// Changes made to the user's files, for the change log.
    pub changes: Vec<FileChange>,
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

/// A verification code found in the user's email.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeEmail {
    pub code: String,
    pub from: String,
    pub subject: String,
    /// Unix seconds.
    pub received: i64,
}

/// Somewhere the app can look for verification codes the user was just
/// sent (their Gmail). Implemented by the API rail, used by the browser
/// rail; codes go to the user's screen, never to the model.
pub trait CodeSource: Send + Sync {
    /// Codes in messages received at or after `since` (Unix seconds),
    /// newest first. `hint` is the site asking (`netflix.com`) to rank its
    /// sender first.
    fn recent_codes(&self, since: i64, hint: Option<String>) -> BoxFuture<'static, Result<Vec<CodeEmail>, String>>;
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
