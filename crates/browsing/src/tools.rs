//! Agent tools over history and focus sites. Read-only: the user decides
//! what's in focus.

use crate::{Browsing, HistoryQuery};
use futures::future::BoxFuture;
use router::{Effect, Tool, ToolOutput, ToolResult};
use serde_json::{Value, json};
use std::sync::Arc;

pub fn tools(store: Arc<Browsing>) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(History(store.clone())), Arc::new(FocusSites(store))]
}

pub struct History(Arc<Browsing>);
pub struct FocusSites(Arc<Browsing>);

impl Tool for History {
    fn name(&self) -> &'static str {
        "browser_history"
    }
    fn description(&self) -> &'static str {
        "Search the pages the user has visited in the app's browser, newest first: time, title, site and URL. \
         Call this when the user refers to something they looked at, asks what they were browsing, or when \
         past visits would tell you which store, product or page they mean. Filter by text (title or URL), \
         site, and/or the last N days."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "Words to match in the title or URL."},
                "site": {"type": "string", "description": "Only this site, e.g. amazon.com (subdomains included)."},
                "since_days": {"type": "integer", "minimum": 1, "description": "Only visits in the last N days."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 200, "description": "Max results, default 30."}
            },
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        let mut parts = vec!["Search browsing history".to_string()];
        if let Some(t) = input["text"].as_str() {
            parts.push(format!("for “{t}”"));
        }
        if let Some(s) = input["site"].as_str() {
            parts.push(format!("on {s}"));
        }
        parts.join(" ")
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let store = self.0.clone();
        Box::pin(async move {
            if !input.is_object() && !input.is_null() {
                return Err("Invalid input: expected an object".into());
            }
            for key in input.as_object().into_iter().flat_map(|o| o.keys()) {
                if !["text", "site", "since_days", "limit"].contains(&key.as_str()) {
                    return Err(format!("Invalid input: unknown field `{key}`"));
                }
            }
            let q = HistoryQuery {
                text: input["text"].as_str().map(str::to_string),
                site: input["site"].as_str().map(str::to_string),
                since_days: input["since_days"].as_u64().map(|d| d.min(3650) as u32),
                limit: input["limit"].as_u64().unwrap_or(30).clamp(1, 200) as usize,
            };
            let visits = store.history(&q);
            if visits.is_empty() {
                return Ok(ToolOutput { content: "No matching history.".into(), summary: "0 visits".into(), ..Default::default() });
            }
            let lines: Vec<String> = visits
                .iter()
                .map(|v| {
                    let title = if v.title.is_empty() { "(untitled)" } else { &v.title };
                    format!("- {} | {} | {} | {}", v.at.format("%a %b %-d %-I:%M %p"), title, v.site, v.url)
                })
                .collect();
            Ok(ToolOutput {
                summary: format!("{} visits", visits.len()),
                content: format!("{} visit(s), newest first:\n{}", visits.len(), lines.join("\n")), ..Default::default()
            })
        })
    }
}

impl Tool for FocusSites {
    fn name(&self) -> &'static str {
        "focus_sites"
    }
    fn description(&self) -> &'static str {
        "List the sites the user has put in focus: the sites they want you to concentrate on. Prefer these \
         sites when choosing where to look, shop or act, unless the user says otherwise."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, _: &Value) -> String {
        "List focus sites".into()
    }
    fn call(&self, _: Value) -> BoxFuture<'static, ToolResult> {
        let store = self.0.clone();
        Box::pin(async move {
            let sites = store.focus_sites();
            if sites.is_empty() {
                return Ok(ToolOutput { content: "No sites are in focus.".into(), summary: "none".into(), ..Default::default() });
            }
            let lines: Vec<String> = sites
                .iter()
                .map(|f| format!("- {} ({}), added {}", f.site, f.label, f.added.format("%b %-d")))
                .collect();
            Ok(ToolOutput { summary: format!("{} sites", sites.len()), content: lines.join("\n"), ..Default::default() })
        })
    }
}
