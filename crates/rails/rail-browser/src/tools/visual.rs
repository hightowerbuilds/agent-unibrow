//! `browser_click_point`: a click at a point from a screenshot, for what
//! the page outline can't list: maps, canvas apps, charts, games (roadmap
//! 2.7). The result carries a fresh screenshot, so the agent checks each
//! click before the next.
//!
//! The same checks as `browser_click` apply to whatever is at the point:
//! the clickable element under it (the button a label sits in, the link an
//! image is in) is probed, and one that would buy, send, post, delete or
//! submit a form asks the user first. Something embedded from another site
//! (a frame) always asks. A drawn button (pixels on a canvas) can't be
//! read, so on a page that looks like a checkout a click on a drawing asks
//! too.

use super::{Browser, COMMIT_WORDS, LINK_COMMIT_WORDS, check_keys, has_word, scope, site};
use crate::bridge::Rect;
use futures::future::BoxFuture;
use router::{Effect, Tool, ToolContext, ToolResult};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

pub struct ClickPoint(pub(super) Arc<Browser>);

/// Walks from the element at the point to the one that takes the click.
const PRESSABLE_FN: &str = r#"function() {
  const el = this.nodeType === 1 ? this : this.parentElement;
  if (!el) return this;
  return el.closest('a[href], button, input, select, textarea, label, summary, [role=button], [role=link], [role=menuitem], [role=tab], [role=checkbox], [role=radio], [onclick], [tabindex]') || el;
}"#;

/// What's at the point, in words, and whether it's a drawing or a frame.
const WHAT_FN: &str = r#"function() {
  const t = (this.tagName || '').toLowerCase();
  const text = (this.getAttribute && (this.getAttribute('aria-label') || this.getAttribute('title') || this.getAttribute('alt'))) || (this.innerText || this.value || '');
  return {tag: t, text: String(text).trim().replace(/\s+/g, ' ').slice(0, 60)};
}"#;

/// Words in a page's address or title that say it takes payment or orders.
const CHECKOUT_WORDS: &[&str] = &["checkout", "check out", "cart", "basket", "payment", "pay", "billing", "order", "purchase"];

fn looks_like_checkout(url: &str, title: &str) -> bool {
    let hay = format!("{} {}", url.to_lowercase(), title.to_lowercase());
    CHECKOUT_WORDS.iter().any(|w| hay.split(|c: char| !c.is_alphanumeric()).any(|t| t == *w) || (w.contains(' ') && hay.contains(w)))
}

impl Tool for ClickPoint {
    fn name(&self) -> &'static str {
        "browser_click_point"
    }
    fn description(&self) -> &'static str {
        "Click at a point on the page, in viewport CSS pixels as in browser_screenshot: for maps, canvas apps, \
         charts, games and anything else browser_read can't list. Returns a screenshot afterwards: look at it to \
         check the click did what you meant before the next one. Use browser_click with a ref whenever the thing \
         has one. Clicking something that buys, sends, posts, deletes or submits asks the user first."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "x": {"type": "number", "description": "From the left of the visible page, CSS pixels."},
                "y": {"type": "number", "description": "From the top of the visible page, CSS pixels."},
                "button": {"type": "string", "enum": ["left", "right", "middle"], "description": "Default left."},
                "double": {"type": "boolean", "description": "Double-click (to zoom a map, open an item). Default false."}
            },
            "required": ["x", "y"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        // Ordinary clicks run on their own; risky ones ask mid-call.
        Effect::Read
    }
    fn site_acted_on(&self, _: &Value) -> BoxFuture<'static, Option<String>> {
        scope::current_site(&self.0)
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Click at ({}, {})", input["x"], input["y"])
    }
    fn call(&self, _: Value) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { Err("browser_click_point needs the tool context".to_string()) })
    }
    fn call_with(&self, input: Value, ctx: ToolContext) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["x", "y", "button", "double"])?;
            let num = |k: &str| input[k].as_f64().filter(|v| v.is_finite()).ok_or_else(|| format!("Invalid input: `{k}` must be a number"));
            let (x, y) = (num("x")?, num("y")?);
            let button = input["button"].as_str().unwrap_or("left");
            if !["left", "right", "middle"].contains(&button) {
                return Err("Invalid input: `button` must be left, right or middle".into());
            }
            let clicks = if input["double"].as_bool().unwrap_or(false) { 2 } else { 1 };
            let st = b.state().await?;
            if x < 0.0 || y < 0.0 || x >= st.width as f64 || y >= st.height as f64 {
                return Err(format!(
                    "({x}, {y}) is outside the visible page ({}x{} CSS px). Scroll to it first, then take a new screenshot.",
                    st.width, st.height
                ));
            }

            // What's there, and what would take the click.
            let at = b.thing_at(x, y).await;
            let label = match &at {
                Some(t) if !t.text.is_empty() => format!("{} “{}”", t.tag, t.text),
                Some(t) => t.tag.clone(),
                None => "nothing the page reports".into(),
            };
            let probe = match at.as_ref().and_then(|t| t.object.clone()) {
                Some(id) => b.probe_object(&id).await,
                None => Default::default(),
            };
            let words = if probe.link { LINK_COMMIT_WORDS } else { COMMIT_WORDS };
            let named = probe.name.clone();
            let worded = has_word(&named, words) || at.as_ref().is_some_and(|t| has_word(&t.text, words));
            let submits = probe.is_submit && probe.submit_commits();
            let tag = at.as_ref().map(|t| t.tag.as_str()).unwrap_or("");
            let framed = tag == "iframe" || tag == "frame";
            let drawn = matches!(tag, "canvas" | "svg" | "img" | "") && looks_like_checkout(&st.url, &st.title);
            if worded || submits || framed || drawn {
                let why = if framed {
                    "This clicks inside something embedded from another site, which can't be checked first".to_string()
                } else if drawn {
                    "This clicks a drawing on a page that takes orders or payment, which can't be read first".to_string()
                } else if worded {
                    "This looks like it could buy, send, post, delete or confirm something".to_string()
                } else {
                    "This submits a form that sends information".to_string()
                };
                if clicks > 1 {
                    return Err(format!("Not clicked: {label} needs the user's approval, and a double click would press it twice. Click it once."));
                }
                b.confirm(&ctx, format!("Click {label} at ({x:.0}, {y:.0})"), why, &probe).await?;
            }
            if let Some(id) = at.and_then(|t| t.object) {
                let _ = b.bridge.cdp("Runtime.releaseObject", json!({"objectId": id})).await;
            }

            b.bridge.highlight(Rect { x: x - 6.0, y: y - 6.0, width: 12.0, height: 12.0 }, format!("click {label}"));
            tokio::time::sleep(Duration::from_millis(350)).await;
            let url_before = st.url.clone();
            if !b.click_at(x, y, button, clicks).await? {
                return b.after_action(format!("Clicked at ({x:.0}, {y:.0}) on {label}; a dialog opened."), format!("clicked at ({x:.0}, {y:.0})")).await;
            }
            b.settle(Duration::from_secs(5)).await;
            let now = b.state().await.map(|s| s.url).unwrap_or_default();
            let moved = if now != url_before { format!(" The page went to {now}.") } else { String::new() };
            let mut out = b
                .after_action(
                    format!(
                        "Clicked at ({x:.0}, {y:.0}) on {label}.{moved} The screenshot shows the page now: check the \
                         click did what you meant before the next one."
                    ),
                    format!("clicked at ({x:.0}, {y:.0}) on {}", site(&now)),
                )
                .await?;
            out.images.extend(b.screenshot(70, 1.0).await.ok());
            Ok(out)
        })
    }
}

/// The element under a point.
pub(super) struct ThingAt {
    pub tag: String,
    pub text: String,
    /// A handle on the element that takes the click (release it after).
    pub object: Option<String>,
}

impl Browser {
    /// The clickable element at (x, y) in the page (viewport CSS px).
    pub(super) async fn thing_at(&self, x: f64, y: f64) -> Option<ThingAt> {
        let _ = self.bridge.cdp("DOM.getDocument", json!({"depth": 0})).await;
        let node = self
            .bridge
            .cdp("DOM.getNodeForLocation", json!({"x": x.round() as i64, "y": y.round() as i64, "ignorePointerEventsNone": true}))
            .await
            .ok()?;
        let backend = node["backendNodeId"].as_i64()?;
        let resolved = self.bridge.cdp("DOM.resolveNode", json!({"backendNodeId": backend})).await.ok()?;
        let leaf = resolved["object"]["objectId"].as_str()?.to_string();
        let up = self
            .bridge
            .cdp("Runtime.callFunctionOn", json!({"objectId": leaf, "functionDeclaration": PRESSABLE_FN, "returnByValue": false}))
            .await
            .ok();
        let _ = self.bridge.cdp("Runtime.releaseObject", json!({"objectId": leaf})).await;
        let object = up.and_then(|r| r["result"]["objectId"].as_str().map(str::to_string))?;
        let what = self
            .bridge
            .cdp("Runtime.callFunctionOn", json!({"objectId": object, "functionDeclaration": WHAT_FN, "returnByValue": true}))
            .await
            .ok()
            .map(|r| r["result"]["value"].clone())
            .unwrap_or(Value::Null);
        Some(ThingAt {
            tag: what["tag"].as_str().unwrap_or("").to_string(),
            text: what["text"].as_str().unwrap_or("").to_string(),
            object: Some(object),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkout_pages_are_spotted() {
        assert!(looks_like_checkout("https://shop.example/checkout/step2", ""));
        assert!(looks_like_checkout("https://shop.example/x", "Your cart"));
        assert!(looks_like_checkout("https://shop.example/x", "Check out"));
        assert!(!looks_like_checkout("https://maps.example/@47.6,-122.3", "Seattle – Maps"));
        // A word inside another isn't one ("paypal", "orderly" pages aren't a checkout by that alone).
        assert!(!looks_like_checkout("https://example.com/orderly-notes", "Payday"));
    }
}
