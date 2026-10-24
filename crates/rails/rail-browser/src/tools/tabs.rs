//! `browser_tabs`: list, open, show, pick and close tabs, and show 2 to 4
//! side by side (split view). `browser_view_tabs`: look at several tabs at
//! once (a screenshot and the text of each) without switching to them.
//!
//! The browser tools act on the agent's tab. That's normally the active
//! tab, the one the user sees, but the agent can open its own tab in the
//! background (or pick an existing one) and work there while the user
//! keeps browsing theirs (roadmap 3.1). Popup windows a page opened with
//! `window.open` (sign-in popups, 3.2) show up as tabs marked with the tab
//! that opened them.

use super::{Browser, arg_str, check_keys};
use crate::bridge::{PageState, TabInfo, TabOp};
use futures::future::BoxFuture;
use router::{Effect, Tool, ToolImage, ToolOutput, ToolResult};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

pub struct Tabs(pub(super) Arc<Browser>);
pub struct ViewTabs(pub(super) Arc<Browser>);

/// Most tabs in split view, and looked at in one call.
const MAX_SPLIT: usize = 4;

/// What the list's marks mean, said once above it.
const LEGEND: &str = "Tabs (→ = the tab the user is looking at, ★ = the tab your browser tools act on):";

pub(super) fn tab_list(tabs: &[TabInfo]) -> String {
    tabs.iter()
        .map(|t| {
            let title = if t.title.is_empty() { "(untitled)" } else { &t.title };
            let popup = t.opener.map(|o| format!(" (popup window opened by tab {o})")).unwrap_or_default();
            let split = if t.split && !t.active { " (on screen in split view)" } else { "" };
            format!(
                "{}{} tab {}: {title} — {}{popup}{split}",
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
         switch shows a tab to the user and makes the active tab yours again. split shows 2 to 4 tabs to \
         the user side by side (the first is focused and becomes yours; unsplit goes back to one); use \
         browser_view_tabs to see several tabs at once. Open a new tab to look \
         something up without losing the current page. Popup windows a page opens (sign-in popups) are tabs \
         too, marked with the tab that opened them; they close themselves when done."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["list", "new", "switch", "use", "close", "split", "unsplit"],
                           "description": "use: work in a tab without showing it to the user. split: show `tabs` side by side."},
                "url": {"type": "string", "description": "For new: what to open (default: the home page)."},
                "background": {"type": "boolean", "description": "For new: open it as your own tab in the background, leaving the user on theirs."},
                "tab": {"type": "integer", "description": "For switch/use/close: the tab id from the list."},
                "tabs": {"type": "array", "items": {"type": "integer"}, "minItems": 2, "maxItems": MAX_SPLIT,
                         "description": "For split: 2 to 4 tab ids to show side by side; the first gets the focus."}
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
            "split" => format!("Show tabs {} side by side", ids_text(&input["tabs"])),
            "unsplit" => "Show one tab again".into(),
            _ => "List tabs".into(),
        }
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["action", "url", "tab", "tabs", "background"])?;
            let action = arg_str(&input, "action")?.to_string();
            let tab = || input["tab"].as_u64().map(|t| t as u32).ok_or("Invalid input: `tab` is required".to_string());
            let background = input["background"].as_bool().unwrap_or(false);
            let op = match action.as_str() {
                "list" => TabOp::List,
                "new" => TabOp::New { url: input["url"].as_str().map(str::to_string), background },
                "switch" => TabOp::Switch { id: tab()? },
                "use" => TabOp::Use { id: tab()? },
                "close" => TabOp::Close { id: tab()? },
                "split" => TabOp::Split { ids: tab_ids(&input["tabs"], 2)? },
                "unsplit" => TabOp::Unsplit,
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
                ("split", _) => format!(
                    "The user now sees these tabs side by side. Your browser tools act on the focused one (→); \
                     browser_view_tabs shows you all of them at once.\n{LEGEND}\n{list}"
                ),
                _ => format!("{LEGEND}\n{list}"),
            };
            b.after_action(what, format!("{action} tab")).await
        })
    }
}

/// "3, 5": the ids in a `tabs` input, for a call's description.
fn ids_text(v: &Value) -> String {
    v.as_array().map(|a| a.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", ")).unwrap_or_default()
}

/// The `tabs` input: `min` to [`MAX_SPLIT`] different tab ids.
fn tab_ids(v: &Value, min: usize) -> Result<Vec<u32>, String> {
    let list = v.as_array().ok_or("Invalid input: `tabs` must be a list of tab ids")?;
    let mut ids = Vec::new();
    for x in list {
        let id = x.as_u64().and_then(|n| u32::try_from(n).ok()).ok_or("Invalid input: `tabs` must be a list of tab ids")?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.len() < min || ids.len() > MAX_SPLIT {
        return Err(format!("Invalid input: `tabs` needs {min} to {MAX_SPLIT} different tab ids"));
    }
    Ok(ids)
}

/// The page's text as it reads (`innerText`), blank lines folded.
const TEXT_JS: &str = r#"(() => {
  const t = document.body ? document.body.innerText : '';
  return t.replace(/[ \t]+\n/g, '\n').replace(/\n{3,}/g, '\n\n').trim();
})()"#;

/// How long to wait for a tab that was asleep (or still loading) to load.
const VIEW_LOAD_WAIT: Duration = Duration::from_secs(8);

/// Waits for tab `tb` to finish loading (up to [`VIEW_LOAD_WAIT`]).
async fn loaded_state(tb: &Browser) -> Result<PageState, String> {
    let start = std::time::Instant::now();
    loop {
        let st = tb.state().await?;
        if !st.loading || start.elapsed() > VIEW_LOAD_WAIT {
            return Ok(st);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

impl Tool for ViewTabs {
    fn name(&self) -> &'static str {
        "browser_view_tabs"
    }
    fn description(&self) -> &'static str {
        "Look at several open tabs at once without switching to them: for each tab id (1 to 4, from \
         browser_tabs), a screenshot of what it shows plus its title, address and text. Use it to compare \
         pages open side by side, or to check another tab while you work in yours. It doesn't change which tab \
         the user sees or which tab your other tools act on. Sensitive fields are masked in the screenshots."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "tabs": {"type": "array", "items": {"type": "integer"}, "minItems": 1, "maxItems": MAX_SPLIT,
                         "description": "Tab ids from browser_tabs."},
                "text": {"type": "boolean", "description": "Include each page's text (default true)."},
                "max_chars": {"type": "integer", "minimum": 500, "maximum": 10000, "description": "Most text per tab (default 3000)."}
            },
            "required": ["tabs"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Look at tabs {}", ids_text(&input["tabs"]))
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["tabs", "text", "max_chars"])?;
            let ids = tab_ids(&input["tabs"], 1)?;
            let with_text = input["text"].as_bool().unwrap_or(true);
            let max = input["max_chars"].as_u64().unwrap_or(3000).clamp(500, 10_000) as usize;
            let all = b.bridge.tabs(TabOp::List).await?;
            let mut parts = Vec::new();
            let mut images = Vec::new();
            let mut pictured = Vec::new();
            for id in ids {
                let Some(info) = all.iter().find(|t| t.id == id) else {
                    parts.push(format!("Tab {id}: there's no such tab."));
                    continue;
                };
                if crate::app_pages::is_app_page(&info.url) {
                    parts.push(format!("Tab {id}: the app's home page, which is the user's, not a website; not shown."));
                    continue;
                }
                let tb = Browser::new(b.bridge.for_tab(id));
                let st = match loaded_state(&tb).await {
                    Ok(st) => st,
                    Err(e) => {
                        parts.push(format!("Tab {id}: couldn't look at it: {e}"));
                        continue;
                    }
                };
                let marks = match (info.active, info.split, info.agent) {
                    (true, _, _) => " (the tab the user is looking at)",
                    (false, true, _) => " (on screen in split view)",
                    (false, false, true) => " (your tab, in the background)",
                    _ => " (in the background)",
                };
                let title = if st.title.is_empty() { "(untitled)" } else { st.title.as_str() };
                let mut part = format!("Tab {id}{marks}: {title} — {} ({}x{} CSS px)", st.url, st.width, st.height);
                if st.loading {
                    part.push_str("\nIt's still loading.");
                }
                let shot = json!({"format": "jpeg", "quality": 70, "captureBeyondViewport": false});
                match tb.capture_masked(shot).await {
                    Ok(data) => {
                        images.push(ToolImage { mime: "image/jpeg".into(), base64: data });
                        pictured.push(id.to_string());
                    }
                    Err(e) => part.push_str(&format!("\n(No screenshot: {e})")),
                }
                if with_text {
                    let r = tb.bridge.cdp("Runtime.evaluate", json!({"expression": TEXT_JS, "returnByValue": true, "timeout": 5000})).await;
                    let text = r.ok().and_then(|v| v["result"]["value"].as_str().map(str::to_string)).unwrap_or_default();
                    let shown: String = text.chars().take(max).collect();
                    let more = if text.chars().nth(max).is_some() { "\n[…cut; raise max_chars or use browser_read in that tab for more]" } else { "" };
                    part.push_str(&format!("\nText:\n{shown}{more}"));
                }
                let note = super::injection::note(&tb, None, &st.url).await;
                if !note.is_empty() {
                    part.push_str(&format!("\n{note}"));
                }
                parts.push(part);
            }
            let mut content = parts.join("\n\n");
            if !pictured.is_empty() {
                content.push_str(&format!("\n\nScreenshots, in order: tab {}.", pictured.join(", tab ")));
            }
            content.push_str(&format!("\n{}", super::extract::UNTRUSTED));
            Ok(ToolOutput { content, summary: format!("looked at {} tabs", parts.len()), images, ..Default::default() })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_marks_user_agent_and_popup_tabs() {
        let tab = |id, active, agent, opener, split| TabInfo {
            id,
            title: format!("T{id}"),
            url: format!("http://x/{id}"),
            active,
            agent,
            opener,
            split,
        };
        let got = tab_list(&[
            tab(1, true, false, None, true),
            tab(2, false, true, None, false),
            tab(3, false, false, Some(2), false),
            tab(4, false, false, None, true),
        ]);
        assert_eq!(
            got,
            "→  tab 1: T1 — http://x/1\n ★ tab 2: T2 — http://x/2\n   tab 3: T3 — http://x/3 (popup window opened by tab 2)\n   tab 4: T4 — http://x/4 (on screen in split view)"
        );
    }

    #[test]
    fn tab_ids_are_checked() {
        assert_eq!(tab_ids(&json!([3, 5, 3]), 2).unwrap(), vec![3, 5]);
        assert!(tab_ids(&json!([3]), 2).is_err());
        assert!(tab_ids(&json!([1, 2, 3, 4, 5]), 1).is_err());
        assert!(tab_ids(&json!(["3"]), 1).is_err());
        assert!(tab_ids(&json!(7), 1).is_err());
    }
}
