//! `browser_tabs`: list, open, show, pick and close tabs.
//!
//! The browser tools act on the agent's tab. That's normally the active
//! tab, the one the user sees, but the agent can open its own tab in the
//! background (or pick an existing one) and work there while the user
//! keeps browsing theirs (roadmap 3.1). Popup windows a page opened with
//! `window.open` (sign-in popups, 3.2) show up as tabs marked with the tab
//! that opened them.

use super::{Browser, arg_str, check_keys};
use crate::bridge::{TabInfo, TabOp};
use futures::future::BoxFuture;
use router::{Effect, Tool, ToolOutput, ToolResult};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

pub struct Tabs(pub(super) Arc<Browser>);

/// What the list's marks mean, said once above it.
const LEGEND: &str = "Tabs (→ = the tab the user is looking at, ★ = the tab your browser tools act on):";

pub(super) fn tab_list(tabs: &[TabInfo]) -> String {
    tabs.iter()
        .map(|t| {
            let title = if t.title.is_empty() { "(untitled)" } else { &t.title };
            let popup = t.opener.map(|o| format!(" (popup window opened by tab {o})")).unwrap_or_default();
            format!(
                "{}{} tab {}: {title} — {}{popup}",
                if t.active { "→" } else { " " },
                if t.agent { "★" } else { " " },
                t.id,
                t.url
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl Tool for Tabs {
    fn name(&self) -> &'static str {
        "browser_tabs"
    }
    fn description(&self) -> &'static str {
        "List, open, switch to, or close tabs in the app's browser. The other browser tools act on your tab \
         (★ in the list): normally the active tab, the one the user sees. To work without taking over the \
         user's screen, open a tab with background: true, or pick an existing tab with action use: the user \
         keeps browsing their own tab while you read, click and type in yours, and screenshots still work. \
         switch shows a tab to the user and makes the active tab yours again. Open a new tab to look \
         something up without losing the current page. Popup windows a page opens (sign-in popups) are tabs \
         too, marked with the tab that opened them; they close themselves when done."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["list", "new", "switch", "use", "close"],
                           "description": "use: work in a tab without showing it to the user."},
                "url": {"type": "string", "description": "For new: what to open (default: the home page)."},
                "background": {"type": "boolean", "description": "For new: open it as your own tab in the background, leaving the user on theirs."},
                "tab": {"type": "integer", "description": "For switch/use/close: the tab id from the list."}
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        let background = input["background"].as_bool().unwrap_or(false);
        match input["action"].as_str().unwrap_or("list") {
            "new" if background => format!("Open a background tab: {}", input["url"].as_str().unwrap_or("home")),
            "new" => format!("Open a tab: {}", input["url"].as_str().unwrap_or("home")),
            "switch" => format!("Switch to tab {}", input["tab"]),
            "use" => format!("Work in tab {} in the background", input["tab"]),
            "close" => format!("Close tab {}", input["tab"]),
            _ => "List tabs".into(),
        }
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["action", "url", "tab", "background"])?;
            let action = arg_str(&input, "action")?.to_string();
            let tab = || input["tab"].as_u64().map(|t| t as u32).ok_or("Invalid input: `tab` is required".to_string());
            let background = input["background"].as_bool().unwrap_or(false);
            let op = match action.as_str() {
                "list" => TabOp::List,
                "new" => TabOp::New { url: input["url"].as_str().map(str::to_string), background },
                "switch" => TabOp::Switch { id: tab()? },
                "use" => TabOp::Use { id: tab()? },
                "close" => TabOp::Close { id: tab()? },
                other => return Err(format!("Unknown action `{other}`")),
            };
            let tabs = b.bridge.tabs(op).await?;
            let list = tab_list(&tabs);
            if action == "list" || action == "close" {
                return Ok(ToolOutput::text(format!("{LEGEND}\n{list}"), format!("{} tabs", tabs.len())));
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
            let what = match (action.as_str(), background) {
                ("new", true) | ("use", _) => format!(
                    "You're now working in a background tab; the user stays on theirs. The other browser tools act \
                     on it until you switch tabs.\n{LEGEND}\n{list}"
                ),
                _ => format!("{LEGEND}\n{list}"),
            };
            b.after_action(what, format!("{action} tab")).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_marks_user_agent_and_popup_tabs() {
        let tab = |id, active, agent, opener| TabInfo { id, title: format!("T{id}"), url: format!("http://x/{id}"), active, agent, opener };
        let got = tab_list(&[tab(1, true, false, None), tab(2, false, true, None), tab(3, false, false, Some(2))]);
        assert_eq!(
            got,
            "→  tab 1: T1 — http://x/1\n ★ tab 2: T2 — http://x/2\n   tab 3: T3 — http://x/3 (popup window opened by tab 2)"
        );
    }
}
