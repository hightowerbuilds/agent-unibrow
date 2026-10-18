//! Agent tools over history, focus sites and watches. History and focus are
//! read-only: the user decides what's in focus.

use crate::{Browsing, HistoryQuery, MAX_WATCHES, NewWatch, Watch};
use futures::future::BoxFuture;
use router::{Effect, Tool, ToolOutput, ToolResult};
use serde_json::{Value, json};
use std::sync::Arc;

pub fn tools(store: Arc<Browsing>) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(History(store.clone())), Arc::new(FocusSites(store.clone())), Arc::new(Watches(store))]
}

pub struct Watches(Arc<Browsing>);

fn watch_line(w: &Watch) -> String {
    let what = match (w.kind.as_str(), &w.selector, &w.needle) {
        ("price", Some(sel), _) => format!("price at `{sel}`"),
        ("price", None, _) => "the price".to_string(),
        (_, _, Some(n)) => format!("whether “{n}” is on the page"),
        (_, Some(sel), None) => format!("the text at `{sel}`"),
        _ => "the page".to_string(),
    };
    let mut line = format!(
        "- #{} {} — {what}, tells the user when it {}; checked every {} min\n  {}",
        w.id, w.label, w.when, w.every_min, w.url
    );
    match (&w.last_value, &w.last_checked) {
        (Some(v), Some(at)) => line.push_str(&format!("\n  Last seen: {v} ({})", at.format("%b %-d %-I:%M %p"))),
        _ => line.push_str("\n  Not checked yet."),
    }
    if let Some(e) = &w.last_error {
        line.push_str(&format!("\n  Last check failed: {e}"));
    }
    line
}

impl Tool for Watches {
    fn name(&self) -> &'static str {
        "browser_watch"
    }
    fn description(&self) -> &'static str {
        "Watch a page and tell the user when something changes: \"tell me when this price drops\", \"let me \
         know when it's back in stock\", \"has my order shipped?\". The app re-checks the page in the background \
         on a schedule and notifies the user (desktop notification and a note in chat), even when you're not \
         running. action=add needs url, label and kind: kind=price reads the page's price (or the price inside \
         `selector`); kind=text tracks whether `text` is on the page (when=appears or disappears), or the text \
         inside `selector` (when=changes). action=list shows watches with their last value; action=remove \
         takes id; action=check makes a watch re-check now."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "list", "remove", "check"]},
                "url": {"type": "string"},
                "label": {"type": "string", "description": "A short name the user will recognize, e.g. \"TUSQ nut on Sweetwater\"."},
                "kind": {"type": "string", "enum": ["price", "text"]},
                "selector": {"type": "string", "description": "CSS selector for the part of the page to watch (optional)."},
                "text": {"type": "string", "description": "For kind=text: the words to look for, e.g. \"In stock\" or \"Shipped\"."},
                "when": {"type": "string", "enum": ["changes", "drops", "rises", "appears", "disappears"], "description": "Default: drops for price, appears for text, changes with a selector."},
                "every_minutes": {"type": "integer", "minimum": 15, "maximum": 10080, "description": "How often to check. Default 60."},
                "id": {"type": "integer"}
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        match input["action"].as_str().unwrap_or("") {
            "add" => format!("Watch {}", input["label"].as_str().or(input["url"].as_str()).unwrap_or("a page")),
            "remove" => format!("Remove watch #{}", input["id"]),
            "check" => format!("Re-check watch #{}", input["id"]),
            _ => "List watches".into(),
        }
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let store = self.0.clone();
        Box::pin(async move {
            for key in input.as_object().into_iter().flat_map(|o| o.keys()) {
                if !["action", "url", "label", "kind", "selector", "text", "when", "every_minutes", "id"].contains(&key.as_str()) {
                    return Err(format!("Invalid input: unknown field `{key}`"));
                }
            }
            let text = |k: &str| input[k].as_str().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
            match input["action"].as_str().unwrap_or("") {
                "list" => {
                    let all = store.watches();
                    if all.is_empty() {
                        return Ok(ToolOutput::text("No watches.", "0 watches"));
                    }
                    let lines: Vec<String> = all.iter().map(watch_line).collect();
                    Ok(ToolOutput::text(lines.join("\n"), format!("{} watches", all.len())))
                }
                "remove" => {
                    let id = input["id"].as_i64().ok_or("Invalid input: `id` is required")?;
                    if store.remove_watch(id) {
                        Ok(ToolOutput::text(format!("Removed watch #{id}."), format!("removed #{id}")))
                    } else {
                        Err(format!("There's no watch #{id}."))
                    }
                }
                "check" => {
                    let id = input["id"].as_i64().ok_or("Invalid input: `id` is required")?;
                    if !store.watches().iter().any(|w| w.id == id) {
                        return Err(format!("There's no watch #{id}."));
                    }
                    store.check_watch_soon(id);
                    Ok(ToolOutput::text(
                        format!("Watch #{id} will re-check within a minute; the user is told if it changed."),
                        format!("re-check #{id}"),
                    ))
                }
                "add" => {
                    let url = text("url").ok_or("Invalid input: `url` is required")?;
                    if !url.starts_with("https://") && !url.starts_with("http://") {
                        return Err("Invalid input: `url` must start with https:// or http://".into());
                    }
                    let label = text("label").ok_or("Invalid input: `label` is required")?;
                    let kind = text("kind").ok_or("Invalid input: `kind` is required (price or text)")?;
                    let selector = text("selector");
                    let needle = text("text");
                    let default_when = match (kind.as_str(), &needle) {
                        ("price", _) => "drops",
                        (_, Some(_)) => "appears",
                        _ => "changes",
                    };
                    let when = text("when").unwrap_or_else(|| default_when.into());
                    let ok = match kind.as_str() {
                        "price" => ["changes", "drops", "rises"].contains(&when.as_str()),
                        "text" if needle.is_some() => ["appears", "disappears", "changes"].contains(&when.as_str()),
                        "text" if selector.is_some() => when == "changes",
                        "text" => return Err("Invalid input: a text watch needs `text` to look for or a `selector`".into()),
                        _ => return Err("Invalid input: `kind` must be price or text".into()),
                    };
                    if !ok {
                        return Err(format!("Invalid input: when={when} doesn't fit a {kind} watch"));
                    }
                    let every_min = input["every_minutes"].as_i64().unwrap_or(60).clamp(15, 10_080);
                    let id = store.add_watch(&NewWatch { url, label: label.clone(), kind, selector, needle, when, every_min })?;
                    let w = store.watches().into_iter().find(|w| w.id == id).ok_or("The watch wasn't saved.")?;
                    Ok(ToolOutput::text(
                        format!(
                            "Watching (#{id}, {} of {MAX_WATCHES} watches). The first check runs within a minute and \
                             records the current value; the user is told when it {}.\n{}",
                            store.watches().len(),
                            w.when,
                            watch_line(&w)
                        ),
                        format!("watching {label}"),
                    ))
                }
                _ => Err("Invalid input: `action` must be add, list, remove or check".into()),
            }
        })
    }
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
