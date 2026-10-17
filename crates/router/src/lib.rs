//! Tool and rail definitions, and the registry the agent draws tools from.
//!
//! Rails depend on this crate to implement [`Tool`]; this crate depends on no
//! rail. The shell constructs each rail and registers it, so dependencies
//! point one way: shell → agent-core → router ← rails.

use futures::future::BoxFuture;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

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

/// What a tool hands back to the model.
#[derive(Clone, Debug)]
pub struct ToolOutput {
    /// Text returned to the model as the tool_result.
    pub content: String,
    /// One line for the trace pane.
    pub summary: String,
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
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult>;
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
