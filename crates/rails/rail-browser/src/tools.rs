//! Agent tools for the embedded browser (roadmap Phase 1.3).
//!
//! The agent drives the same page the user is looking at. Elements are
//! addressed by refs from `browser_read` (`e1234`). Every action highlights
//! its target on screen, waits for the page to settle, and returns the page
//! as it is afterwards, plus a small screenshot kept for the trace.

use crate::bridge::{BrowserBridge, NavAction, PageState, Rect};
use base64::Engine;
use futures::future::BoxFuture;
use page_model::{describe_single, from_ax_tree, parse_ref, ref_for};
use router::{Effect, Tool, ToolContext, ToolImage, ToolOutput, ToolResult};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Characters of page outline returned after an action.
const AFTER_ACTION_CHARS: usize = 6_000;
/// Characters returned by `browser_read` unless asked otherwise.
const READ_CHARS: usize = 15_000;

/// Words on a button or link that mean "this does something you can't take
/// back". Clicking one asks the user first.
const COMMIT_WORDS: &[&str] = &[
    "buy", "purchase", "pay", "place order", "order now", "checkout", "check out", "send", "submit", "post",
    "publish", "delete", "remove", "unsubscribe", "cancel subscription", "cancel order", "confirm", "sign up",
    "register", "book", "reserve", "transfer", "donate", "subscribe", "apply", "accept", "agree", "authorize",
    "withdraw", "deposit",
];

pub fn tools(bridge: BrowserBridge) -> Vec<Arc<dyn Tool>> {
    let b = Arc::new(Browser { bridge });
    vec![
        Arc::new(Open(b.clone())),
        Arc::new(Read(b.clone())),
        Arc::new(Screenshot(b.clone())),
        Arc::new(Click(b.clone())),
        Arc::new(Type(b.clone())),
        Arc::new(Press(b.clone())),
        Arc::new(Scroll(b.clone())),
        Arc::new(Wait(b.clone())),
        Arc::new(History(b.clone(), NavAction::Back)),
        Arc::new(History(b.clone(), NavAction::Forward)),
        Arc::new(History(b, NavAction::Reload)),
    ]
}

// ------------------------------------------------------------- helpers

struct Browser {
    bridge: BrowserBridge,
}

struct Element {
    backend_id: i64,
    role: String,
    name: String,
}

impl Element {
    fn label(&self) -> String {
        match (self.role.is_empty(), self.name.is_empty()) {
            (false, false) => format!("{} \"{}\"", self.role, self.name),
            (false, true) => self.role.clone(),
            (true, false) => format!("\"{}\"", self.name),
            (true, true) => ref_for(self.backend_id),
        }
    }
}

fn site(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_string()))
        .unwrap_or_else(|| "this page".into())
}

fn arg_str<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input[key].as_str().filter(|s| !s.is_empty()).ok_or_else(|| format!("Invalid input: `{key}` is required"))
}

fn check_keys(input: &Value, allowed: &[&str]) -> Result<(), String> {
    if !(input.is_object() || input.is_null()) {
        return Err("Invalid input: expected an object".into());
    }
    for k in input.as_object().into_iter().flat_map(|o| o.keys()) {
        if !allowed.contains(&k.as_str()) {
            return Err(format!("Invalid input: unknown field `{k}`"));
        }
    }
    Ok(())
}

impl Browser {
    async fn state(&self) -> Result<PageState, String> {
        self.bridge.state().await
    }

    /// The page outline.
    async fn outline(&self, from_line: usize, max_chars: usize) -> Result<String, String> {
        let _ = self.bridge.cdp("Accessibility.enable", json!({})).await;
        let tree = self.bridge.cdp("Accessibility.getFullAXTree", json!({})).await?;
        let nodes = tree["nodes"].as_array().cloned().unwrap_or_default();
        let model = from_ax_tree(&nodes);
        let st = self.state().await?;
        let mut out = format!(
            "Page: {}\nURL: {}\n{}{} actionable elements. Refs like [e123] can be passed to browser_click / browser_type.\n\n",
            if st.title.is_empty() { "(untitled)" } else { &st.title },
            st.url,
            if st.loading { "Still loading.\n" } else { "" },
            model.interactive,
        );
        if model.lines.is_empty() {
            out.push_str("(The page has no readable content yet. Try browser_wait or browser_screenshot.)\n");
        } else {
            out.push_str(&model.render(from_line, max_chars));
        }
        Ok(out)
    }

    /// Waits until the page has stopped loading (or `timeout` passes).
    async fn settle(&self, timeout: Duration) {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let start = Instant::now();
        while start.elapsed() < timeout {
            let loading = self.state().await.map(|s| s.loading).unwrap_or(false);
            let ready = self
                .bridge
                .cdp("Runtime.evaluate", json!({"expression": "document.readyState", "returnByValue": true}))
                .await
                .ok()
                .and_then(|r| r["result"]["value"].as_str().map(str::to_string));
            if !loading && ready.as_deref() == Some("complete") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    /// JPEG of the viewport. `scale` < 1 for trace thumbnails.
    async fn screenshot(&self, quality: u32, scale: f64) -> Result<ToolImage, String> {
        let st = self.state().await?;
        let mut params = json!({"format": "jpeg", "quality": quality, "captureBeyondViewport": false});
        if scale < 1.0 {
            params["clip"] = json!({"x": 0, "y": 0, "width": st.width.max(1), "height": st.height.max(1), "scale": scale});
        }
        let r = self.bridge.cdp("Page.captureScreenshot", params).await?;
        let data = r["data"].as_str().ok_or("no screenshot data")?.to_string();
        Ok(ToolImage { mime: "image/jpeg".into(), base64: data })
    }

    /// The page after an action: outline for the model, thumbnail for the trace.
    async fn after_action(&self, what: String, summary: String) -> ToolResult {
        self.settle(Duration::from_secs(10)).await;
        let page = self.outline(0, AFTER_ACTION_CHARS).await.unwrap_or_else(|e| format!("(couldn't read the page: {e})"));
        let evidence = self.screenshot(55, 0.5).await.into_iter().collect();
        Ok(ToolOutput { content: format!("{what}\n\n{page}"), summary, evidence, ..Default::default() })
    }

    async fn element(&self, r: &str) -> Result<Element, String> {
        let backend_id =
            parse_ref(r).ok_or_else(|| format!("`{r}` isn't a ref. Use a ref like e123 from browser_read."))?;
        let ax = self
            .bridge
            .cdp("Accessibility.getPartialAXTree", json!({"backendNodeId": backend_id, "fetchRelatives": false}))
            .await
            .map_err(|_| format!("[{r}] is no longer on the page. Call browser_read for fresh refs."))?;
        let (role, name) = describe_single(ax["nodes"].as_array().map(Vec::as_slice).unwrap_or(&[])).unwrap_or_default();
        Ok(Element { backend_id, role, name })
    }

    /// Scrolls the element into view and returns its box in viewport CSS pixels.
    async fn rect(&self, el: &Element) -> Result<Rect, String> {
        let _ = self.bridge.cdp("DOM.scrollIntoViewIfNeeded", json!({"backendNodeId": el.backend_id})).await;
        let bm = self
            .bridge
            .cdp("DOM.getBoxModel", json!({"backendNodeId": el.backend_id}))
            .await
            .map_err(|_| format!("{} isn't visible on the page.", el.label()))?;
        let q: Vec<f64> = bm["model"]["border"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_f64).collect())
            .unwrap_or_default();
        if q.len() < 8 {
            return Err(format!("{} has no size on the page.", el.label()));
        }
        let xs = [q[0], q[2], q[4], q[6]];
        let ys = [q[1], q[3], q[5], q[7]];
        let (x0, x1) = (xs.iter().cloned().fold(f64::MAX, f64::min), xs.iter().cloned().fold(f64::MIN, f64::max));
        let (y0, y1) = (ys.iter().cloned().fold(f64::MAX, f64::min), ys.iter().cloned().fold(f64::MIN, f64::max));
        Ok(Rect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 })
    }

    async fn mouse(&self, kind: &str, x: f64, y: f64, button: &str, clicks: u32) -> Result<(), String> {
        self.bridge
            .cdp(
                "Input.dispatchMouseEvent",
                json!({"type": kind, "x": x, "y": y, "button": button, "clickCount": clicks}),
            )
            .await
            .map(|_| ())
    }

    /// Runs `function` with the element as `this`.
    async fn call_on(&self, el: &Element, function: &str) -> Result<Value, String> {
        let node = self.bridge.cdp("DOM.resolveNode", json!({"backendNodeId": el.backend_id})).await?;
        let object_id = node["object"]["objectId"].as_str().ok_or("element has no handle")?.to_string();
        let r = self
            .bridge
            .cdp(
                "Runtime.callFunctionOn",
                json!({"objectId": object_id, "functionDeclaration": function, "returnByValue": true}),
            )
            .await?;
        Ok(r["result"]["value"].clone())
    }

    async fn key(&self, key: &KeySpec, modifiers: i64) -> Result<(), String> {
        let mut down = json!({
            "type": if key.text.is_some() && modifiers & !8 == 0 { "keyDown" } else { "rawKeyDown" },
            "key": key.key, "code": key.code, "windowsVirtualKeyCode": key.vk, "modifiers": modifiers,
        });
        if let Some(t) = key.text.as_ref().filter(|_| modifiers & !8 == 0) {
            down["text"] = json!(t);
            down["unmodifiedText"] = json!(t);
        }
        self.bridge.cdp("Input.dispatchKeyEvent", down).await?;
        self.bridge
            .cdp(
                "Input.dispatchKeyEvent",
                json!({"type": "keyUp", "key": key.key, "code": key.code, "windowsVirtualKeyCode": key.vk, "modifiers": modifiers}),
            )
            .await
            .map(|_| ())
    }
}

// --------------------------------------------------------------- keys

struct KeySpec {
    key: String,
    code: String,
    vk: i64,
    text: Option<String>,
}

const MOD_ALT: i64 = 1;
const MOD_CTRL: i64 = 2;
const MOD_META: i64 = 4;
const MOD_SHIFT: i64 = 8;

/// Parses "Enter", "Control+a", "Shift+Tab", "Meta+Shift+K" into a key and
/// a CDP modifier mask.
fn parse_keys(combo: &str) -> Result<(KeySpec, i64), String> {
    let parts: Vec<&str> = combo.split('+').map(str::trim).filter(|p| !p.is_empty()).collect();
    let (last, mods) = parts.split_last().ok_or("Invalid input: `keys` is empty")?;
    let mut m = 0;
    for p in mods {
        m |= match p.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => MOD_CTRL,
            "alt" | "option" => MOD_ALT,
            "shift" => MOD_SHIFT,
            "meta" | "cmd" | "command" | "super" => MOD_META,
            other => return Err(format!("Unknown modifier `{other}`")),
        };
    }
    let named = |key: &str, code: &str, vk: i64, text: Option<&str>| KeySpec {
        key: key.into(),
        code: code.into(),
        vk,
        text: text.map(str::to_string),
    };
    let k = match last.to_ascii_lowercase().as_str() {
        "enter" | "return" => named("Enter", "Enter", 13, Some("\r")),
        "tab" => named("Tab", "Tab", 9, None),
        "escape" | "esc" => named("Escape", "Escape", 27, None),
        "backspace" => named("Backspace", "Backspace", 8, None),
        "delete" | "del" => named("Delete", "Delete", 46, None),
        "space" => named(" ", "Space", 32, Some(" ")),
        "arrowup" | "up" => named("ArrowUp", "ArrowUp", 38, None),
        "arrowdown" | "down" => named("ArrowDown", "ArrowDown", 40, None),
        "arrowleft" | "left" => named("ArrowLeft", "ArrowLeft", 37, None),
        "arrowright" | "right" => named("ArrowRight", "ArrowRight", 39, None),
        "home" => named("Home", "Home", 36, None),
        "end" => named("End", "End", 35, None),
        "pageup" => named("PageUp", "PageUp", 33, None),
        "pagedown" => named("PageDown", "PageDown", 34, None),
        f if f.len() >= 2 && f.starts_with('f') && f[1..].parse::<i64>().is_ok_and(|n| (1..=12).contains(&n)) => {
            let n: i64 = f[1..].parse().unwrap_or(1);
            named(&format!("F{n}"), &format!("F{n}"), 111 + n, None)
        }
        single if single.chars().count() == 1 => {
            let c = last.chars().next().unwrap_or(' ');
            let upper = c.to_ascii_uppercase();
            let (code, vk) = if c.is_ascii_alphabetic() {
                (format!("Key{upper}"), upper as i64)
            } else if c.is_ascii_digit() {
                (format!("Digit{c}"), c as i64)
            } else {
                (String::new(), 0)
            };
            let shown = if m & MOD_SHIFT != 0 { upper.to_string() } else { c.to_string() };
            KeySpec { key: shown.clone(), code, vk, text: Some(shown) }
        }
        other => return Err(format!("Unknown key `{other}`. Use names like Enter, Tab, Escape, ArrowDown, or one character.")),
    };
    Ok((k, m))
}

// -------------------------------------------------------------- tools

macro_rules! browser_tool {
    ($ty:ident) => {
        pub struct $ty(Arc<Browser>);
    };
}
browser_tool!(Open);
browser_tool!(Read);
browser_tool!(Screenshot);
browser_tool!(Click);
browser_tool!(Type);
browser_tool!(Press);
browser_tool!(Scroll);
browser_tool!(Wait);
pub struct History(Arc<Browser>, NavAction);

impl Tool for Open {
    fn name(&self) -> &'static str {
        "browser_open"
    }
    fn description(&self) -> &'static str {
        "Open a URL in the app's browser (the page the user is looking at), or search the web if given words \
         instead of a URL. Returns the page outline once it has loaded. Use this to go to a site; use \
         browser_click to follow links on the current page."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"url": {"type": "string", "description": "A URL (https://…), a host (example.com), or search words."}},
            "required": ["url"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Open {}", input["url"].as_str().unwrap_or(""))
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["url"])?;
            let url = crate::normalize_address(arg_str(&input, "url")?);
            b.bridge.navigate(&url).await?;
            // Give the navigation a moment to start before waiting on it.
            tokio::time::sleep(Duration::from_millis(400)).await;
            b.after_action(format!("Opened {url}."), format!("opened {}", site(&url))).await
        })
    }
}

impl Tool for Read {
    fn name(&self) -> &'static str {
        "browser_read"
    }
    fn description(&self) -> &'static str {
        "Read the current page in the app's browser: an outline of headings, text, and every link, button and \
         field, each with a ref like [e123] to use with browser_click and browser_type. Call this before acting \
         on a page, and again after the page changes. Long pages come in parts; pass from_line to continue."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "from_line": {"type": "integer", "minimum": 0, "description": "Start at this outline line (for long pages)."},
                "max_chars": {"type": "integer", "minimum": 1000, "maximum": 60000, "description": "Size limit, default 15000."}
            },
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, _: &Value) -> String {
        "Read the page".into()
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["from_line", "max_chars"])?;
            let from = input["from_line"].as_u64().unwrap_or(0) as usize;
            let max = input["max_chars"].as_u64().unwrap_or(READ_CHARS as u64).clamp(1000, 60_000) as usize;
            let page = b.outline(from, max).await?;
            let st = b.state().await.unwrap_or_default();
            Ok(ToolOutput::text(page, format!("read {}", site(&st.url))))
        })
    }
}

impl Tool for Screenshot {
    fn name(&self) -> &'static str {
        "browser_screenshot"
    }
    fn description(&self) -> &'static str {
        "Take a screenshot of what's visible in the app's browser. Use it when the outline from browser_read \
         isn't enough: images, charts, maps, canvas apps, or checking how something looks."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, _: &Value) -> String {
        "Screenshot the page".into()
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &[])?;
            let img = b.screenshot(75, 1.0).await?;
            let st = b.state().await.unwrap_or_default();
            Ok(ToolOutput {
                content: format!("Screenshot of {} ({}x{} CSS px).", st.url, st.width, st.height),
                summary: format!("screenshot of {}", site(&st.url)),
                images: vec![img],
                ..Default::default()
            })
        })
    }
}

impl Tool for Click {
    fn name(&self) -> &'static str {
        "browser_click"
    }
    fn description(&self) -> &'static str {
        "Click an element on the current page by its ref from browser_read (e.g. e123). Scrolls it into view \
         first and returns the page afterwards. Clicking something that buys, sends, posts, deletes or \
         confirms asks the user to approve first."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ref": {"type": "string", "description": "Element ref from browser_read, like e123."},
                "button": {"type": "string", "enum": ["left", "right", "middle"], "description": "Default left."},
                "double": {"type": "boolean", "description": "Double-click. Default false."}
            },
            "required": ["ref"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        // Ordinary clicks run on their own; risky ones ask mid-call.
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Click {}", input["ref"].as_str().unwrap_or(""))
    }
    fn call(&self, _input: Value) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { Err("browser_click needs the tool context".to_string()) })
    }
    fn call_with(&self, input: Value, ctx: ToolContext) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["ref", "button", "double"])?;
            let r = arg_str(&input, "ref")?;
            let button = input["button"].as_str().unwrap_or("left");
            if !["left", "right", "middle"].contains(&button) {
                return Err("Invalid input: `button` must be left, right or middle".into());
            }
            let clicks = if input["double"].as_bool().unwrap_or(false) { 2 } else { 1 };
            let el = b.element(r).await?;
            let st = b.state().await?;

            let lower = el.name.to_lowercase();
            let risky = matches!(el.role.as_str(), "button" | "link" | "menuitem" | "")
                && COMMIT_WORDS.iter().any(|w| lower.contains(w));
            if risky {
                let place = site(&st.url);
                let ok = ctx
                    .approver
                    .approve(
                        "browser_click",
                        format!("Click {} on {place}\n{}", el.label(), st.url),
                        format!("This looks like it could buy, send, post, delete or confirm something on {place}"),
                        vec![place],
                    )
                    .await;
                if !ok {
                    return Err(format!("The user declined clicking {}.", el.label()));
                }
            }

            let rect = b.rect(&el).await?;
            b.bridge.highlight(rect, format!("click {}", el.label()));
            tokio::time::sleep(Duration::from_millis(350)).await;
            let (x, y) = (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
            b.mouse("mouseMoved", x, y, "none", 0).await?;
            for n in 1..=clicks {
                b.mouse("mousePressed", x, y, button, n).await?;
                b.mouse("mouseReleased", x, y, button, n).await?;
            }
            b.after_action(format!("Clicked [{r}] {}.", el.label()), format!("clicked {}", el.label())).await
        })
    }
}

impl Tool for Type {
    fn name(&self) -> &'static str {
        "browser_type"
    }
    fn description(&self) -> &'static str {
        "Type text into a field on the current page, by its ref from browser_read. Replaces what's in the field \
         unless clear is false. Set submit to press Enter afterwards (e.g. to run a search). Returns the page \
         afterwards."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ref": {"type": "string", "description": "Field ref from browser_read, like e123."},
                "text": {"type": "string"},
                "clear": {"type": "boolean", "description": "Replace existing text. Default true."},
                "submit": {"type": "boolean", "description": "Press Enter after typing. Default false."}
            },
            "required": ["ref", "text"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Stage
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Type into {}", input["ref"].as_str().unwrap_or(""))
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["ref", "text", "clear", "submit"])?;
            let r = arg_str(&input, "ref")?;
            let text = input["text"].as_str().ok_or("Invalid input: `text` is required")?.to_string();
            let clear = input["clear"].as_bool().unwrap_or(true);
            let submit = input["submit"].as_bool().unwrap_or(false);
            let el = b.element(r).await?;
            let rect = b.rect(&el).await?;
            b.bridge.highlight(rect, format!("type into {}", el.label()));
            b.bridge.cdp("DOM.focus", json!({"backendNodeId": el.backend_id})).await.map_err(|_| {
                format!("{} can't take text. Pick a textbox or other field.", el.label())
            })?;
            // Select existing content so the typed text replaces it, or put
            // the caret at the end to append.
            let f = if clear {
                "function(){ if (typeof this.select === 'function') { this.select(); } else if (this.isContentEditable) { const r=document.createRange(); r.selectNodeContents(this); const s=getSelection(); s.removeAllRanges(); s.addRange(r); } }"
            } else {
                "function(){ if (typeof this.setSelectionRange === 'function' && typeof this.value === 'string') { const n=this.value.length; this.setSelectionRange(n,n); } }"
            };
            let _ = b.call_on(&el, f).await;
            if clear && text.is_empty() {
                b.key(&parse_keys("Backspace")?.0, 0).await?;
            } else {
                b.bridge.cdp("Input.insertText", json!({"text": text})).await?;
            }
            if submit {
                b.key(&parse_keys("Enter")?.0, 0).await?;
            }
            let shown: String = text.chars().take(60).collect();
            b.after_action(
                format!("Typed \"{shown}\" into [{r}] {}{}.", el.label(), if submit { " and pressed Enter" } else { "" }),
                format!("typed into {}", el.label()),
            )
            .await
        })
    }
}

impl Tool for Press {
    fn name(&self) -> &'static str {
        "browser_press"
    }
    fn description(&self) -> &'static str {
        "Press a key or shortcut in the page, sent to whatever has focus: Enter, Tab, Escape, ArrowDown, \
         PageDown, Backspace, or combinations like Control+a and Shift+Tab. Use browser_type for typing text."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "keys": {"type": "string", "description": "Key name or combination, e.g. Enter or Control+a."},
                "times": {"type": "integer", "minimum": 1, "maximum": 50, "description": "Repeat count. Default 1."}
            },
            "required": ["keys"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Stage
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Press {}", input["keys"].as_str().unwrap_or(""))
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["keys", "times"])?;
            let combo = arg_str(&input, "keys")?.to_string();
            let times = input["times"].as_u64().unwrap_or(1).clamp(1, 50);
            let (key, mods) = parse_keys(&combo)?;
            for _ in 0..times {
                b.key(&key, mods).await?;
            }
            let what = if times > 1 { format!("Pressed {combo} {times} times.") } else { format!("Pressed {combo}.") };
            b.after_action(what, format!("pressed {combo}")).await
        })
    }
}

impl Tool for Scroll {
    fn name(&self) -> &'static str {
        "browser_scroll"
    }
    fn description(&self) -> &'static str {
        "Scroll the current page, or bring an element into view by its ref. Use it to see more of a long page \
         or to load more results on pages that load as you scroll."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "direction": {"type": "string", "enum": ["down", "up", "left", "right"], "description": "Default down."},
                "pages": {"type": "number", "minimum": 0.1, "maximum": 20, "description": "How far, in screenfuls. Default 1."},
                "ref": {"type": "string", "description": "Scroll this element into view instead."},
                "to": {"type": "string", "enum": ["top", "bottom"], "description": "Jump to the top or bottom of the page."}
            },
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        match (input["ref"].as_str(), input["to"].as_str()) {
            (Some(r), _) => format!("Scroll to {r}"),
            (_, Some(t)) => format!("Scroll to the {t}"),
            _ => format!("Scroll {}", input["direction"].as_str().unwrap_or("down")),
        }
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["direction", "pages", "ref", "to"])?;
            let st = b.state().await?;
            let what;
            if let Some(r) = input["ref"].as_str() {
                let el = b.element(r).await?;
                let rect = b.rect(&el).await?;
                b.bridge.highlight(rect, el.label());
                what = format!("Scrolled [{r}] {} into view.", el.label());
            } else if let Some(to) = input["to"].as_str() {
                let y = if to == "top" { "0" } else { "document.documentElement.scrollHeight" };
                b.bridge
                    .cdp("Runtime.evaluate", json!({"expression": format!("window.scrollTo(0, {y})")}))
                    .await?;
                what = format!("Scrolled to the {to}.");
            } else {
                let dir = input["direction"].as_str().unwrap_or("down");
                let pages = input["pages"].as_f64().unwrap_or(1.0).clamp(0.1, 20.0);
                let (dx, dy) = match dir {
                    "up" => (0.0, -(st.height as f64) * 0.85 * pages),
                    "left" => (-(st.width as f64) * 0.85 * pages, 0.0),
                    "right" => ((st.width as f64) * 0.85 * pages, 0.0),
                    _ => (0.0, (st.height as f64) * 0.85 * pages),
                };
                b.bridge
                    .cdp(
                        "Input.dispatchMouseEvent",
                        json!({
                            "type": "mouseWheel", "x": st.width as f64 / 2.0, "y": st.height as f64 / 2.0,
                            "deltaX": dx, "deltaY": dy,
                        }),
                    )
                    .await?;
                what = format!("Scrolled {dir} {pages} screen(s).");
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            let pos = b
                .bridge
                .cdp(
                    "Runtime.evaluate",
                    json!({
                        "expression": "JSON.stringify([Math.round(scrollY), Math.round(document.documentElement.scrollHeight - innerHeight)])",
                        "returnByValue": true,
                    }),
                )
                .await
                .ok()
                .and_then(|r| r["result"]["value"].as_str().map(str::to_string))
                .and_then(|s| serde_json::from_str::<Vec<i64>>(&s).ok())
                .map(|v| format!(" Scroll position {} of {} px.", v[0], v[1].max(0)))
                .unwrap_or_default();
            b.after_action(format!("{what}{pos}"), what.to_lowercase()).await
        })
    }
}

impl Tool for Wait {
    fn name(&self) -> &'static str {
        "browser_wait"
    }
    fn description(&self) -> &'static str {
        "Wait for the current page: until it finishes loading, until some text appears or disappears, or for a \
         fixed number of seconds. Use after actions that start slow work (searches, checkouts, uploads)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "for": {"type": "string", "enum": ["load", "text", "text_gone", "seconds"], "description": "Default load."},
                "text": {"type": "string", "description": "Text to wait for (for text / text_gone)."},
                "seconds": {"type": "number", "minimum": 0.1, "maximum": 30, "description": "For `seconds`."},
                "timeout": {"type": "number", "minimum": 1, "maximum": 60, "description": "Give up after this many seconds. Default 15."}
            },
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        match input["for"].as_str().unwrap_or("load") {
            "text" => format!("Wait for “{}”", input["text"].as_str().unwrap_or("")),
            "text_gone" => format!("Wait for “{}” to go", input["text"].as_str().unwrap_or("")),
            "seconds" => format!("Wait {}s", input["seconds"].as_f64().unwrap_or(1.0)),
            _ => "Wait for the page to load".into(),
        }
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["for", "text", "seconds", "timeout"])?;
            let kind = input["for"].as_str().unwrap_or("load");
            let timeout = Duration::from_secs_f64(input["timeout"].as_f64().unwrap_or(15.0).clamp(1.0, 60.0));
            let start = Instant::now();
            let outcome = match kind {
                "seconds" => {
                    let s = input["seconds"].as_f64().unwrap_or(1.0).clamp(0.1, 30.0);
                    tokio::time::sleep(Duration::from_secs_f64(s)).await;
                    format!("Waited {s} seconds.")
                }
                "text" | "text_gone" => {
                    let text = arg_str(&input, "text")?.to_string();
                    let want_present = kind == "text";
                    let expr = format!(
                        "(document.body ? document.body.innerText : '').toLowerCase().includes({})",
                        serde_json::to_string(&text.to_lowercase()).unwrap_or_default()
                    );
                    loop {
                        let present = b
                            .bridge
                            .cdp("Runtime.evaluate", json!({"expression": expr, "returnByValue": true}))
                            .await
                            .ok()
                            .and_then(|r| r["result"]["value"].as_bool())
                            .unwrap_or(false);
                        if present == want_present {
                            break format!(
                                "“{text}” {} after {:.1}s.",
                                if want_present { "appeared" } else { "is gone" },
                                start.elapsed().as_secs_f64()
                            );
                        }
                        if start.elapsed() > timeout {
                            return Err(format!(
                                "Timed out after {}s: “{text}” {}.",
                                timeout.as_secs(),
                                if want_present { "never appeared" } else { "is still there" }
                            ));
                        }
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                }
                _ => {
                    b.settle(timeout).await;
                    format!("Page loaded after {:.1}s.", start.elapsed().as_secs_f64())
                }
            };
            b.after_action(outcome.clone(), outcome.to_lowercase()).await
        })
    }
}

impl Tool for History {
    fn name(&self) -> &'static str {
        match self.1 {
            NavAction::Back => "browser_back",
            NavAction::Forward => "browser_forward",
            NavAction::Reload => "browser_reload",
        }
    }
    fn description(&self) -> &'static str {
        match self.1 {
            NavAction::Back => "Go back to the previous page in the app's browser, like the Back button.",
            NavAction::Forward => "Go forward again after going back, like the Forward button.",
            NavAction::Reload => "Reload the current page.",
        }
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, _: &Value) -> String {
        match self.1 {
            NavAction::Back => "Go back",
            NavAction::Forward => "Go forward",
            NavAction::Reload => "Reload",
        }
        .into()
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        let action = self.1;
        Box::pin(async move {
            check_keys(&input, &[])?;
            let st = b.state().await?;
            match action {
                NavAction::Back if !st.can_go_back => return Err("There's no previous page.".into()),
                NavAction::Forward if !st.can_go_forward => return Err("There's no next page.".into()),
                _ => {}
            }
            b.bridge.nav(action).await?;
            tokio::time::sleep(Duration::from_millis(400)).await;
            let what = match action {
                NavAction::Back => "Went back.",
                NavAction::Forward => "Went forward.",
                NavAction::Reload => "Reloaded.",
            };
            b.after_action(what.into(), what.to_lowercase()).await
        })
    }
}

/// Decodes a base64 image (for saving trace evidence).
pub fn decode_image(img: &ToolImage) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(&img.base64).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_parsing() {
        let (k, m) = parse_keys("Enter").unwrap();
        assert_eq!((k.key.as_str(), k.vk, m), ("Enter", 13, 0));
        let (k, m) = parse_keys("Control+a").unwrap();
        assert_eq!((k.code.as_str(), k.vk, m), ("KeyA", 65, MOD_CTRL));
        let (k, m) = parse_keys("Shift+Tab").unwrap();
        assert_eq!((k.key.as_str(), m), ("Tab", MOD_SHIFT));
        assert!(parse_keys("Hyper+x").is_err());
        assert!(parse_keys("Bogus").is_err());
    }

    #[test]
    fn commit_words_catch_risky_buttons() {
        let risky = |n: &str| COMMIT_WORDS.iter().any(|w| n.to_lowercase().contains(w));
        assert!(risky("Place order"));
        assert!(risky("Send"));
        assert!(!risky("Preview"));
        assert!(!risky("Next page"));
    }
}
