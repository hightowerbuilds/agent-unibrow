//! `browser_history_list`: a tab's back/forward history (roadmap 3.3),
//! and jumping straight to an entry in it, like the list under a long
//! press on Back.

use super::{Browser, check_keys};
use futures::future::BoxFuture;
use router::{Effect, Tool, ToolOutput, ToolResult};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

pub struct HistoryList(pub(super) Arc<Browser>);

/// Long addresses (reader views are whole pages in a `data:` URL) are cut.
const URL_CHARS: usize = 150;

/// One entry from DevTools' navigation history.
#[derive(Debug, PartialEq)]
pub(super) struct Entry {
    pub id: i64,
    pub url: String,
    pub title: String,
}

pub(super) fn parse(history: &Value) -> (Vec<Entry>, usize) {
    let entries: Vec<Entry> = history["entries"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| Entry {
                    id: e["id"].as_i64().unwrap_or(-1),
                    url: e["url"].as_str().unwrap_or("").to_string(),
                    title: e["title"].as_str().unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    let current = (history["currentIndex"].as_u64().unwrap_or(0) as usize).min(entries.len().saturating_sub(1));
    (entries, current)
}

/// "0. Title — url (2 back)" lines, oldest first.
pub(super) fn render(entries: &[Entry], current: usize) -> String {
    entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let title = if e.title.is_empty() { "(untitled)" } else { &e.title };
            let url: String = if e.url.chars().count() > URL_CHARS {
                format!("{}…", e.url.chars().take(URL_CHARS).collect::<String>())
            } else {
                e.url.clone()
            };
            let place = match i.cmp(&current) {
                std::cmp::Ordering::Equal => "current page".to_string(),
                std::cmp::Ordering::Less => format!("{} back", current - i),
                std::cmp::Ordering::Greater => format!("{} forward", i - current),
            };
            format!("{}{i}. {title} — {url} ({place})", if i == current { "→ " } else { "  " })
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl Tool for HistoryList {
    fn name(&self) -> &'static str {
        "browser_history_list"
    }
    fn description(&self) -> &'static str {
        "The back/forward history of the tab you're working in (what a long press on Back shows), oldest \
         first, with the current page marked. Pass go_to with an entry's number to go straight to that page, \
         however many steps back or forward it is, instead of calling browser_back repeatedly."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "go_to": {"type": "integer", "description": "An entry number from the list to go to."}
            },
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        match input["go_to"].as_u64() {
            Some(i) => format!("Go to history entry {i}"),
            None => "List the tab's history".into(),
        }
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["go_to"])?;
            let st = b.state().await?;
            if st.dialog.is_some() {
                return Err("A dialog is open on the page; answer it with browser_dialog first.".into());
            }
            let (entries, current) = parse(&b.bridge.cdp("Page.getNavigationHistory", json!({})).await?);
            if entries.is_empty() {
                return Err("This tab has no history yet.".into());
            }
            let list = render(&entries, current);
            let Some(go_to) = input.get("go_to").filter(|v| !v.is_null()) else {
                let back = current;
                let forward = entries.len() - 1 - current;
                return Ok(ToolOutput::text(
                    format!("This tab's history, oldest first ({back} back, {forward} forward):\n{list}"),
                    format!("{} history entries", entries.len()),
                ));
            };
            let i = go_to.as_u64().ok_or("Invalid input: `go_to` must be an entry number")? as usize;
            let Some(e) = entries.get(i) else {
                return Err(format!("There's no entry {i}; the history has 0 to {}:\n{list}", entries.len() - 1));
            };
            if i == current {
                return Err(format!("Entry {i} is the current page already.\n{list}"));
            }
            b.bridge.cdp("Page.navigateToHistoryEntry", json!({"entryId": e.id})).await?;
            tokio::time::sleep(Duration::from_millis(400)).await;
            let steps = if i < current { format!("{} back", current - i) } else { format!("{} forward", i - current) };
            let title = if e.title.is_empty() { e.url.clone() } else { e.title.clone() };
            b.after_action(format!("Went {steps} in the tab's history, to “{title}”."), format!("history entry {i}")).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_entries_around_the_current_one() {
        let v = json!({"currentIndex": 1, "entries": [
            {"id": 7, "url": "http://a/", "title": "A"},
            {"id": 9, "url": "http://b/", "title": "B"},
            {"id": 12, "url": "http://c/", "title": ""}
        ]});
        let (entries, current) = parse(&v);
        assert_eq!(current, 1);
        assert_eq!(entries[2], Entry { id: 12, url: "http://c/".into(), title: String::new() });
        assert_eq!(
            render(&entries, current),
            "  0. A — http://a/ (1 back)\n→ 1. B — http://b/ (current page)\n  2. (untitled) — http://c/ (1 forward)"
        );
    }

    #[test]
    fn long_urls_are_cut() {
        let long = format!("data:text/html;base64,{}", "x".repeat(400));
        let out = render(&[Entry { id: 1, url: long, title: "Reader".into() }], 0);
        assert!(out.chars().count() < 200, "{out}");
        assert!(out.contains('…'));
    }
}
