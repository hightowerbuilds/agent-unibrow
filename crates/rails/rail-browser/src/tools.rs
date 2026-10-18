//! Agent tools for the embedded browser (roadmap Phase 1.3).
//!
//! The agent drives the same page the user is looking at. Elements are
//! addressed by refs from `browser_read` (`e1234`). Every action highlights
//! its target on screen, waits for the page to settle, and returns the page
//! as it is afterwards, plus a small screenshot kept for the trace.

use crate::bridge::{BrowserBridge, NavAction, PageState, Rect};
use base64::Engine;
use futures::future::BoxFuture;
use page_model::describe_single;
use router::{Effect, Tool, ToolContext, ToolImage, ToolOutput, ToolResult};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Characters of page outline returned after an action.
const AFTER_ACTION_CHARS: usize = 6_000;
/// Characters returned by `browser_read` unless asked otherwise.
const READ_CHARS: usize = 15_000;
/// Bounds on the "Changes:" section after an action.
const CHANGES_LINES: usize = 30;
const CHANGES_CHARS: usize = 2_500;

/// Words on a button that mean "this does something you can't take back".
/// Clicking one asks the user first. Matched as whole words.
const COMMIT_WORDS: &[&str] = &[
    "buy", "buy now", "purchase", "pay", "pay now", "place order", "complete order", "submit order",
    "confirm order", "order now", "send", "submit", "post", "publish", "delete", "remove", "unsubscribe",
    "cancel subscription", "cancel order", "cancel membership", "confirm", "sign up", "register", "book",
    "reserve", "transfer", "donate", "subscribe", "authorize", "withdraw", "deposit",
];

/// Links usually just navigate, so only clearly destructive or final
/// wording on a link asks first.
const LINK_COMMIT_WORDS: &[&str] = &[
    "delete", "remove", "unsubscribe", "cancel subscription", "cancel order", "cancel membership", "confirm",
    "place order", "pay now", "transfer", "withdraw",
];

/// Whether `text` contains any of `words` as whole words ("pay" matches
/// "Pay now" but not "PayPal" or "payment").
fn has_word(text: &str, words: &[&str]) -> bool {
    let norm: String = text.to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { ' ' }).collect();
    let padded = format!(" {} ", norm.split_whitespace().collect::<Vec<_>>().join(" "));
    words.iter().any(|w| padded.contains(&format!(" {w} ")))
}

/// Fields whose contents the agent never types or sees: passwords, payment
/// cards, government ids, one-time codes. (Roadmap 6.4.)
const SENSITIVE_SELECTOR: &str = "input[type=password], [autocomplete^='cc-' i], [autocomplete*=' cc-' i], \
    [autocomplete='one-time-code' i], input[name*='card' i][name*='num' i], input[id*='card' i][id*='num' i], \
    input[name*='cvv' i], input[name*='cvc' i], input[id*='cvv' i], input[id*='cvc' i], input[name*='ssn' i], \
    input[id*='ssn' i], input[name*='iban' i], input[name*='security' i][name*='code' i], \
    input[name*='otp' i], input[id*='otp' i], input[maxlength='1'][inputmode='numeric'], \
    input[maxlength='1'][type='tel'], input[maxlength='1'][type='number']";

/// Run with an element as `this`: is it sensitive, and what form (if any)
/// would it submit? Values of sensitive fields come back as `[hidden]`.
const PROBE_FN: &str = r#"function(sel){
  const el = this;
  const isSensitive = (x) => { try { return x.matches && x.matches(sel); } catch (e) { return false; } };
  const tag = (el.tagName || '').toLowerCase();
  const type = ((el.getAttribute && el.getAttribute('type')) || '').toLowerCase();
  const form = el.form || (el.closest && el.closest('form')) || null;
  const isSubmit = !!form && ((tag === 'button' && (type === '' || type === 'submit')) || (tag === 'input' && (type === 'submit' || type === 'image')));
  const fields = form ? Array.from(form.elements) : [];
  const action = form ? String(form.getAttribute('action') || '') : '';
  const isSearch = !!form && (form.getAttribute('role') === 'search' || /search/i.test(action)
      || fields.some(x => (x.type || '').toLowerCase() === 'search' || /^(q|query|search|s|keywords?)$/i.test(x.name || '')));
  const values = fields
    .filter(x => !['hidden','submit','button','image','reset','fieldset'].includes((x.type || '').toLowerCase()))
    .slice(0, 25)
    .map(x => {
      const label = ((x.labels && x.labels[0] && x.labels[0].innerText) || x.getAttribute('aria-label') || x.placeholder || x.name || x.id || x.type || '').trim();
      let v = (x.type === 'checkbox' || x.type === 'radio') ? (x.checked ? 'checked' : 'unchecked')
            : (x.tagName === 'SELECT' ? ((x.selectedOptions[0] || {}).text || '') : (x.value || ''));
      if (isSensitive(x)) v = v ? '[hidden]' : '';
      return [label.slice(0, 50), String(v).slice(0, 100)];
    });
  return { tag, type, sensitive: isSensitive(el), isSubmit, inForm: !!form,
           method: form ? String(form.method || 'get').toLowerCase() : '',
           isSearch, hasSensitive: fields.some(isSensitive), values };
}"#;

/// What the probe found out about an element and its form.
#[derive(Default)]
struct Probe {
    tag: String,
    sensitive: bool,
    is_submit: bool,
    in_form: bool,
    method: String,
    is_search: bool,
    has_sensitive: bool,
    values: Vec<(String, String)>,
}

impl Probe {
    fn from(v: &Value) -> Self {
        Self {
            tag: v["tag"].as_str().unwrap_or("").to_string(),
            sensitive: v["sensitive"].as_bool().unwrap_or(false),
            is_submit: v["isSubmit"].as_bool().unwrap_or(false),
            in_form: v["inForm"].as_bool().unwrap_or(false),
            method: v["method"].as_str().unwrap_or("").to_string(),
            is_search: v["isSearch"].as_bool().unwrap_or(false),
            has_sensitive: v["hasSensitive"].as_bool().unwrap_or(false),
            values: v["values"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|p| Some((p[0].as_str()?.to_string(), p[1].as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// Submitting this form sends something somewhere: it posts data or
    /// carries passwords/payment details, and it isn't a search. (6.1)
    fn submit_commits(&self) -> bool {
        self.in_form && !self.is_search && (self.method == "post" || self.has_sensitive)
    }

    /// The form's current values, one per line, sensitive ones hidden.
    fn summary(&self) -> String {
        self.values
            .iter()
            .filter(|(l, _)| !l.is_empty())
            .map(|(l, v)| format!("• {l}: {}", if v.is_empty() { "(empty)" } else { v }))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

mod extract;
mod clipboard;
mod codes;
mod forms;
mod frames;
mod history;
mod jev_sites;
mod injection;
mod playbooks;
mod pointer;
mod scope;
mod screenshot;
mod scrolling;
mod spend;
mod tabs;
mod verify;

pub use extract::watch_value;

pub use playbooks::{Decider, Decision, WordMatch};
pub use jev_sites::{Handled, handle as jev_handle, learn as jev_learn, site_words as jev_site_words};

/// The browser tools. Page-changing ones on focus sites are recorded in
/// `store` for site maps and playbooks (Phase 8).
/// `codes` is where verification codes can be read (the user's Gmail).
pub fn tools(
    bridge: BrowserBridge,
    store: Arc<browsing::Browsing>,
    codes: Option<Arc<dyn router::CodeSource>>,
) -> Vec<Arc<dyn Tool>> {
    let b = Arc::new(Browser::new(bridge));
    let rec = |t: Arc<dyn Tool>| playbooks::Recorded::wrap(t, b.clone(), store.clone());
    let mut all: Vec<Arc<dyn Tool>> = extract::tools(b.clone());
    all.extend::<Vec<Arc<dyn Tool>>>(vec![
        rec(Arc::new(Open(b.clone()))),
        Arc::new(Read(b.clone())),
        Arc::new(Screenshot(b.clone())),
        rec(Arc::new(Click(b.clone()))),
        rec(Arc::new(Type(b.clone()))),
        rec(Arc::new(Press(b.clone()))),
        Arc::new(Scroll(b.clone())),
        Arc::new(Wait(b.clone())),
        rec(Arc::new(Select(b.clone()))),
        Arc::new(Hover(b.clone())),
        rec(Arc::new(forms::FillForm(b.clone()))),
        Arc::new(playbooks::PlaybookSave(b.clone(), store.clone())),
        Arc::new(playbooks::PlaybookRun(b.clone(), store.clone())),
        Arc::new(pointer::Drag(b.clone())),
        Arc::new(clipboard::Clipboard(b.clone())),
        Arc::new(Dialog(b.clone())),
        Arc::new(tabs::Tabs(b.clone())),
        Arc::new(history::HistoryList(b.clone())),
        Arc::new(Find(b.clone())),
        Arc::new(Downloads(b.clone())),
        Arc::new(ReadDownload),
        Arc::new(Upload(b.clone())),
        Arc::new(SavePdf(b.clone())),
        Arc::new(SignIn(b.clone())),
        Arc::new(Handoff(b.clone())),
        Arc::new(codes::EmailCode(b.clone(), codes)),
        rec(Arc::new(History(b.clone(), NavAction::Back))),
        Arc::new(History(b.clone(), NavAction::Forward)),
        Arc::new(History(b, NavAction::Reload)),
    ]);
    all
}

// ------------------------------------------------------------- helpers

struct Browser {
    bridge: BrowserBridge,
    /// Out-of-process frames seen in the outline (refs like `f2e45`).
    frames: std::sync::Mutex<frames::FrameRegistry>,
    /// The outline the agent last saw, to say what an action changed.
    last_seen: std::sync::Mutex<Option<Snapshot>>,
    /// The agent's own clipboard (browser_clipboard). Never the system one.
    clipboard: std::sync::Mutex<Option<String>>,
}

/// Checks to make after an action (7.1), and what the tool already found.
#[derive(Default)]
struct Verify {
    /// Lines already worked out: retries made, a click that did nothing.
    lines: Vec<String>,
    /// The tool already waited for the page to settle.
    settled: bool,
    /// Fields typed into: (field, its ref, the text), read back afterwards.
    typed: Vec<(Element, String, String)>,
    /// A submit happened: from this element (None: the focused one), and
    /// the URL before it.
    form: Option<(Option<Element>, String)>,
}

/// A page outline as the agent saw it.
#[derive(Clone)]
struct Snapshot {
    url: String,
    title: String,
    lines: Vec<String>,
}

#[derive(Clone)]
struct Element {
    backend_id: i64,
    role: String,
    name: String,
    /// Out-of-process frame the element is in (None: the page's process).
    frame: Option<u32>,
}

impl Element {
    fn label(&self) -> String {
        match (self.role.is_empty(), self.name.is_empty()) {
            (false, false) => format!("{} \"{}\"", self.role, self.name),
            (false, true) => self.role.clone(),
            (true, false) => format!("\"{}\"", self.name),
            (true, true) => Browser::ref_text(self),
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
    fn new(bridge: BrowserBridge) -> Self {
        Self { bridge, frames: Default::default(), last_seen: Default::default(), clipboard: Default::default() }
    }

    async fn state(&self) -> Result<PageState, String> {
        self.bridge.state().await
    }

    /// The page outline.
    async fn outline(&self, from_line: usize, max_chars: usize) -> Result<String, String> {
        Ok(self.outline_with_snapshot(from_line, max_chars).await?.0)
    }

    /// The page outline, and the snapshot it was made from (None while a
    /// dialog is open). The snapshot becomes what the agent last saw.
    async fn outline_with_snapshot(&self, from_line: usize, max_chars: usize) -> Result<(String, Option<Snapshot>), String> {
        // While a JavaScript dialog is open the page is frozen; asking it
        // anything would hang, and the dialog is what matters.
        let st = self.state().await?;
        if let Some(d) = st.dialog {
            return Ok((dialog_notice(&d), None));
        }
        // A crashed page has nothing to read, and while a site waits for an
        // HTTP sign-in the old page is still showing: say so plainly.
        if st.crashed || st.auth.is_some() {
            let text = format!(
                "Page: {}\nURL: {}\nProblem: {}.\n",
                if st.title.is_empty() { "(untitled)" } else { &st.title },
                st.url,
                st.load_error.unwrap_or_default()
            );
            return Ok((text, None));
        }
        let model = self.page_model().await?;
        let st = self.state().await?;
        let snap = Snapshot { url: st.url.clone(), title: st.title.clone(), lines: model.lines.clone() };
        if let Ok(mut last) = self.last_seen.lock() {
            *last = Some(snap.clone());
        }
        let notes = format!("{}{}", st.tab_note.as_ref().map(|n| format!("{n}\n")).unwrap_or_default(), self.page_notes(&st.url).await);
        let mut out = format!(
            "Page: {}\nURL: {}\n{}{}{}{} actionable elements. Refs like [e123] can be passed to browser_click / browser_type.\n\n",
            if st.title.is_empty() { "(untitled)" } else { &st.title },
            st.url,
            if st.loading { "Still loading.\n" } else { "" },
            st.load_error.as_ref().map(|e| format!("The page failed to load: {e}\n")).unwrap_or_default(),
            notes,
            model.interactive,
        );
        if model.lines.is_empty() {
            out.push_str("(The page has no readable content yet. Try browser_wait or browser_screenshot.)\n");
        } else {
            out.push_str(&model.render(from_line, max_chars));
        }
        Ok((out, Some(snap)))
    }

    /// What changed since the agent last looked (roadmap 1.2), as a short
    /// "Changes:" section; empty when there's nothing to compare with.
    fn changes_since(before: Option<&Snapshot>, after: Option<&Snapshot>) -> String {
        let (Some(b), Some(a)) = (before, after) else { return String::new() };
        let mut out = String::from("Changes since you last looked at the page:\n");
        if b.url != a.url {
            out.push_str(&format!("URL: {} → {}\n", b.url, a.url));
        }
        if b.title != a.title {
            out.push_str(&format!("Title: “{}” → “{}”\n", b.title, a.title));
        }
        let doc = |u: &str| u.split('#').next().unwrap_or("").to_string();
        if doc(&b.url) != doc(&a.url) {
            out.push_str("A different page is showing now; read the outline below.\n");
            return out;
        }
        let c = page_model::diff(&b.lines, &a.lines);
        if c.is_empty() {
            out.push_str("Nothing on the page changed.\n");
        } else if c.mostly_new() {
            out.push_str(&format!(
                "Much of the page changed ({} lines added, {} removed); the outline below has all of it.\n",
                c.added.len(),
                c.removed.len()
            ));
        } else {
            out.push_str(&c.render(CHANGES_LINES, CHANGES_CHARS));
        }
        out
    }

    /// Waits until the page has stopped loading (or `timeout` passes).
    async fn settle(&self, timeout: Duration) {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let start = Instant::now();
        while start.elapsed() < timeout {
            let st = self.state().await.unwrap_or_default();
            if st.dialog.is_some() || st.crashed || st.auth.is_some() {
                return;
            }
            let loading = st.loading;
            let ready = self
                .bridge
                .cdp("Runtime.evaluate", json!({"expression": "document.readyState", "returnByValue": true}))
                .await
                .ok()
                .and_then(|r| r["result"]["value"].as_str().map(str::to_string));
            if !loading && ready.as_deref() == Some("complete") {
                self.network_quiet(timeout.saturating_sub(start.elapsed())).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    /// After load, waits until the page stops fetching (no new resource
    /// entries across two checks), so content loaded by scripts is there
    /// before the agent reads the page. (Roadmap 7.2.)
    async fn network_quiet(&self, budget: Duration) {
        let start = Instant::now();
        let budget = budget.min(Duration::from_secs(4));
        let count = || async {
            self.bridge
                .cdp(
                    "Runtime.evaluate",
                    json!({"expression": "performance.getEntriesByType('resource').length", "returnByValue": true}),
                )
                .await
                .ok()
                .and_then(|r| r["result"]["value"].as_i64())
        };
        let mut last = count().await;
        let mut stable = 0;
        while start.elapsed() < budget && stable < 2 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let now = count().await;
            if now == last { stable += 1 } else { stable = 0 }
            last = now;
        }
    }

    /// JPEG of the viewport. `scale` < 1 for trace thumbnails.
    async fn screenshot(&self, quality: u32, scale: f64) -> Result<ToolImage, String> {
        let st = self.state().await?;
        let mut params = json!({"format": "jpeg", "quality": quality, "captureBeyondViewport": false});
        if scale < 1.0 {
            params["clip"] = json!({"x": 0, "y": 0, "width": st.width.max(1), "height": st.height.max(1), "scale": scale});
        }
        // Sensitive fields are masked as dots for the capture (6.4).
        let data = self.capture_masked(params).await?;
        Ok(ToolImage { mime: "image/jpeg".into(), base64: data })
    }

    /// The page after an action: outline for the model, thumbnail for the trace.
    async fn after_action(&self, what: String, summary: String) -> ToolResult {
        self.after_action_expecting(what, summary, None).await
    }

    /// Like [`Browser::after_action`], also checking that `expect` text
    /// appeared (roadmap 7.1: act, then verify).
    async fn after_action_expecting(&self, what: String, summary: String, expect: Option<String>) -> ToolResult {
        self.after_action_full(what, summary, expect, Verify::default()).await
    }

    /// The page after an action, with its checks (7.1): `verify` carries
    /// lines the tool already worked out (retries, a dead click) and what
    /// to check once the page has settled (fields typed into, a submit).
    async fn after_action_full(&self, what: String, summary: String, expect: Option<String>, verify: Verify) -> ToolResult {
        if !verify.settled {
            self.settle(Duration::from_secs(10)).await;
        }
        let mut lines = verify.lines;
        for (el, r, text) in &verify.typed {
            lines.extend(self.typed_check(el, r, text).await);
        }
        if let Some((el, url_before)) = &verify.form {
            lines.extend(self.form_check(el.as_ref(), url_before).await);
        }
        let what = if lines.is_empty() { what } else { format!("{what}\n{}", lines.join("\n")) };
        let what = match expect.filter(|e| !e.trim().is_empty()) {
            Some(e) => format!("{what}\n{}", self.check_expect(&e).await),
            None => what,
        };
        let before = self.last_seen.lock().ok().and_then(|l| l.clone());
        let (page, after) = self
            .outline_with_snapshot(0, AFTER_ACTION_CHARS)
            .await
            .unwrap_or_else(|e| (format!("(couldn't read the page: {e})"), None));
        let changes = Self::changes_since(before.as_ref(), after.as_ref());
        let dialog_open = self.state().await.map(|s| s.dialog.is_some()).unwrap_or(false);
        // A frozen page can't produce a screenshot.
        let evidence = if dialog_open { Vec::new() } else { self.screenshot(55, 0.5).await.into_iter().collect() };
        let changes = if changes.is_empty() { String::new() } else { format!("\n\n{changes}") };
        Ok(ToolOutput { content: format!("{what}{changes}\n\n{page}"), summary, evidence, ..Default::default() })
    }

    async fn element(&self, r: &str) -> Result<Element, String> {
        let (frame, backend_id) = self.parse_any_ref(r)?;
        let probe = Element { backend_id, role: String::new(), name: String::new(), frame };
        let ax = self
            .cdp_el(&probe, "Accessibility.getPartialAXTree", json!({"backendNodeId": backend_id, "fetchRelatives": false}))
            .await
            .map_err(|_| format!("[{r}] is no longer on the page. Call browser_read for fresh refs."))?;
        let (role, mut name) = describe_single(ax["nodes"].as_array().map(Vec::as_slice).unwrap_or(&[])).unwrap_or_default();
        if name.is_empty() && matches!(role.as_str(), "group" | "generic" | "") {
            // A plain box (a draggable card): call it by its text.
            let text = self
                .call_on(&probe, "function(){ return (this.innerText || '').trim().replace(/\\s+/g, ' ').slice(0, 60); }")
                .await
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            name = text;
        }
        Ok(Element { backend_id, role, name, frame })
    }

    /// Scrolls the element into view and returns its box in viewport CSS pixels.
    async fn rect(&self, el: &Element) -> Result<Rect, String> {
        if el.frame.is_some() {
            self.scroll_frame_into_view(el.frame).await;
        }
        let _ = self.cdp_el(el, "DOM.scrollIntoViewIfNeeded", json!({"backendNodeId": el.backend_id})).await;
        let (ox, oy) = self.frame_offset(el.frame).await?;
        let bm = self
            .cdp_el(el, "DOM.getBoxModel", json!({"backendNodeId": el.backend_id}))
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
        Ok(Rect { x: x0 + ox, y: y0 + oy, width: x1 - x0, height: y1 - y0 })
    }

    /// Sends a command that might make the page open a dialog (a click or a
    /// key). Returns Ok(false) if a dialog opened before Chromium answered:
    /// the page is then frozen and the command finishes once the dialog is
    /// answered.
    async fn cdp_watch(&self, method: &str, params: Value) -> Result<bool, String> {
        let fut = self.bridge.cdp(method, params);
        tokio::pin!(fut);
        loop {
            tokio::select! {
                r = &mut fut => return r.map(|_| true),
                _ = tokio::time::sleep(Duration::from_millis(150)) => {
                    if self.state().await.map(|s| s.dialog.is_some()).unwrap_or(false) {
                        return Ok(false);
                    }
                }
            }
        }
    }

    /// A full click at (x, y). Returns false if a dialog interrupted it.
    async fn click_at(&self, x: f64, y: f64, button: &str, clicks: u32) -> Result<bool, String> {
        self.mouse("mouseMoved", x, y, "none", 0).await?;
        for n in 1..=clicks {
            let ev = json!({"type": "mousePressed", "x": x, "y": y, "button": button, "clickCount": n});
            if !self.cdp_watch("Input.dispatchMouseEvent", ev).await? {
                return Ok(false);
            }
            let ev = json!({"type": "mouseReleased", "x": x, "y": y, "button": button, "clickCount": n});
            if !self.cdp_watch("Input.dispatchMouseEvent", ev).await? {
                return Ok(false);
            }
        }
        Ok(true)
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
        self.call_on_with(el, function, Vec::new()).await
    }

    /// Like [`Browser::call_on`], passing `args` (JSON values) to the function.
    async fn call_on_with(&self, el: &Element, function: &str, args: Vec<Value>) -> Result<Value, String> {
        let node = self.cdp_el(el, "DOM.resolveNode", json!({"backendNodeId": el.backend_id})).await?;
        let object_id = node["object"]["objectId"].as_str().ok_or("element has no handle")?.to_string();
        let args: Vec<Value> = args.into_iter().map(|v| json!({"value": v})).collect();
        let r = self
            .cdp_el(
                el,
                "Runtime.callFunctionOn",
                json!({"objectId": object_id, "functionDeclaration": function, "arguments": args, "returnByValue": true}),
            )
            .await;
        let _ = self.cdp_el(el, "Runtime.releaseObject", json!({"objectId": object_id})).await;
        let r = r?;
        if let Some(e) = r["exceptionDetails"].as_object() {
            let msg = e.get("exception").and_then(|x| x["description"].as_str()).unwrap_or("script error");
            return Err(msg.lines().next().unwrap_or("script error").to_string());
        }
        Ok(r["result"]["value"].clone())
    }

    /// Things the agent should know before acting: a sign-in form (and
    /// whether a saved login exists), a verification-code field, a CAPTCHA.
    async fn page_notes(&self, url: &str) -> String {
        let probe = r#"(() => {
  const vis = e => { const r = e.getBoundingClientRect(); return r.width > 0 && r.height > 0; };
  const pw = Array.from(document.querySelectorAll('input[type=password]')).some(vis);
  const otp = Array.from(document.querySelectorAll('input[autocomplete="one-time-code" i], input[name*="otp" i], input[id*="otp" i]')).some(vis)
    || /verification code|security code|enter the code|2-step|two-factor|authenticator/i.test(document.body ? document.body.innerText.slice(0, 5000) : '');
  const captcha = !!document.querySelector('iframe[src*="recaptcha"], iframe[src*="hcaptcha"], iframe[src*="challenges.cloudflare"], .g-recaptcha, .h-captcha, .cf-turnstile')
    || /verify you are (a )?human|i'm not a robot|are you a robot/i.test(document.body ? document.body.innerText.slice(0, 5000) : '');
  const cookie = Array.from(document.querySelectorAll('[id*="cookie" i], [class*="cookie" i], [id*="consent" i], [class*="consent" i], [aria-label*="cookie" i]'))
    .some(e => vis(e) && e.querySelector('button, a[role="button"]') && /accept|reject|agree|allow|necessary/i.test(e.innerText || ''));
  return JSON.stringify({pw, otp, captcha, cookie});
})()"#;
        let v: Value = self
            .bridge
            .cdp("Runtime.evaluate", json!({"expression": probe, "returnByValue": true}))
            .await
            .ok()
            .and_then(|r| r["result"]["value"].as_str().and_then(|s| serde_json::from_str(s).ok()))
            .unwrap_or(Value::Null);
        // Text planted for AI agents (6.5); never removes anything.
        let mut notes = injection::note(&self.bridge, None, url).await;
        // A permission request waits for the user (4.2); the agent can't answer it.
        if let Some(p) = self.state().await.ok().and_then(|s| s.permission_prompt) {
            notes.push_str(&format!(
                "The site is waiting for a permission ({p}). Only the user can answer it, on the card over the page; \
                 you can't. Tell the user if the task needs it.\n"
            ));
        }
        if v["cookie"].as_bool().unwrap_or(false) {
            notes.push_str("A cookie consent banner is showing. If it's in the way, prefer “Reject” or “Necessary only”; don't accept tracking for the user.\n");
        }
        if v["captcha"].as_bool().unwrap_or(false) {
            notes.push_str("This page shows a CAPTCHA or bot check. Don't try to solve it: use browser_handoff to ask the user.\n");
        }
        if v["otp"].as_bool().unwrap_or(false) {
            notes.push_str(
                "This page asks for a verification code. If the site emailed it, use browser_email_code: it shows the \
                 code to the user to paste. For codes by text message or an app, use browser_handoff.\n",
            );
        }
        if v["pw"].as_bool().unwrap_or(false) {
            let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
            let saved = crate::vault::for_host(&host);
            if saved.is_empty() {
                notes.push_str("This is a sign-in form and there's no saved login for this site: use browser_handoff to let the user sign in.\n");
            } else {
                let users: Vec<&str> = saved.iter().map(|l| l.username.as_str()).collect();
                notes.push_str(&format!(
                    "This is a sign-in form. Saved login(s) for this site: {}. Use browser_sign_in to fill it.\n",
                    users.join(", ")
                ));
            }
        }
        notes
    }

    /// Waits up to 5 s for `text` to be on the page and says whether it is.
    async fn check_expect(&self, text: &str) -> String {
        let expr = format!(
            "(document.body ? document.body.innerText : '').toLowerCase().includes({})",
            serde_json::to_string(&text.to_lowercase()).unwrap_or_default()
        );
        let start = Instant::now();
        loop {
            if self.state().await.map(|s| s.dialog.is_some()).unwrap_or(false) {
                return format!("Expected “{text}”: can't check while a dialog is open.");
            }
            let found = self
                .bridge
                .cdp("Runtime.evaluate", json!({"expression": expr, "returnByValue": true}))
                .await
                .ok()
                .and_then(|r| r["result"]["value"].as_bool())
                .unwrap_or(false);
            if found {
                return format!("Expected “{text}”: found on the page.");
            }
            if start.elapsed() > Duration::from_secs(5) {
                return format!("Expected “{text}”: NOT on the page. The action may not have worked; check before going on.");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn probe_object(&self, object_id: &str) -> Probe {
        self.probe_object_in(None, object_id).await
    }

    async fn probe_object_in(&self, session: Option<&str>, object_id: &str) -> Probe {
        self.cdp_in(
                session,
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration": PROBE_FN,
                    "arguments": [{"value": SENSITIVE_SELECTOR}],
                    "returnByValue": true,
                }),
            )
            .await
            .map(|r| Probe::from(&r["result"]["value"]))
            .unwrap_or_default()
    }

    async fn probe(&self, el: &Element) -> Probe {
        let session = match self.route(el.frame) {
            Ok(r) => r.map(|r| r.session),
            Err(_) => return Probe::default(),
        };
        match self.cdp_in(session.as_deref(), "DOM.resolveNode", json!({"backendNodeId": el.backend_id})).await {
            Ok(n) => match n["object"]["objectId"].as_str() {
                Some(id) => self.probe_object_in(session.as_deref(), id).await,
                None => Probe::default(),
            },
            Err(_) => Probe::default(),
        }
    }

    /// The element that has keyboard focus.
    async fn probe_focused(&self) -> Probe {
        match self.bridge.cdp("Runtime.evaluate", json!({"expression": "document.activeElement"})).await {
            Ok(r) => match r["result"]["objectId"].as_str() {
                Some(id) => self.probe_object(id).await,
                None => Probe::default(),
            },
            Err(_) => Probe::default(),
        }
    }

    /// Asks the user before an action that commits, showing the form's
    /// values and a screenshot. Returns an error if they decline. (6.2)
    async fn confirm(&self, ctx: &ToolContext, action: String, why: String, probe: &Probe) -> Result<(), String> {
        let st = self.state().await?;
        let place = site(&st.url);
        let shot = self.screenshot(70, 1.0).await.ok();
        let values = probe.summary();
        let description = if values.is_empty() {
            format!("{action} on {place}\n{}", st.url)
        } else {
            format!("{action} on {place}\n{}\n\n{values}", st.url)
        };
        // Spending guard (6.6): purchases carry their amount to the card.
        let purchase =
            if spend::is_purchase(&action, probe) { Some(spend::detect(self, &action, probe).await) } else { None };
        let ok = ctx
            .approver
            .approve_request(router::ApprovalRequest {
                tool: "browser_click",
                description,
                reason: format!("{why} on {place}"),
                counterparties: vec![place],
                image: shot,
                purchase,
            })
            .await;
        if ok { Ok(()) } else { Err(format!("The user declined: {action}.")) }
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
        if !self.cdp_watch("Input.dispatchKeyEvent", down).await? {
            return Ok(()); // a dialog opened; its card says what happened
        }
        self.cdp_watch(
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
browser_tool!(Select);
browser_tool!(Hover);
browser_tool!(Dialog);
browser_tool!(Find);
browser_tool!(Downloads);
/// Reads files from the downloads folder; needs no browser access.
pub struct ReadDownload;
browser_tool!(Upload);
browser_tool!(SavePdf);
browser_tool!(SignIn);
browser_tool!(Handoff);
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

// browser_screenshot is in tools/screenshot.rs.

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
                "double": {"type": "boolean", "description": "Double-click. Default false."},
                "expect": {"type": "string", "description": "Optional: text that should appear if this worked; it's checked and reported."}
            },
            "required": ["ref"],
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
        format!("Click {}", input["ref"].as_str().unwrap_or(""))
    }
    fn call(&self, _input: Value) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { Err("browser_click needs the tool context".to_string()) })
    }
    fn call_with(&self, input: Value, ctx: ToolContext) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["ref", "button", "double", "expect"])?;
            let r = arg_str(&input, "ref")?;
            let button = input["button"].as_str().unwrap_or("left");
            if !["left", "right", "middle"].contains(&button) {
                return Err("Invalid input: `button` must be left, right or middle".into());
            }
            let clicks = if input["double"].as_bool().unwrap_or(false) { 2 } else { 1 };
            let (mut el, stale, mut r_now) = b.resolve(r).await?;
            let mut notes: Vec<String> = stale.into_iter().collect();
            // A click on a page that's still loading can land on something
            // that isn't wired up yet, or about to move: wait first (7.3).
            if b.state().await.map(|s| s.loading).unwrap_or(false) {
                b.settle(Duration::from_secs(8)).await;
                notes.push("The page was still loading; waited for it to finish before clicking.".into());
                let (e, n, rr) = b.resolve(&r_now).await?;
                (el, r_now) = (e, rr);
                notes.extend(n);
            }

            let worded = match el.role.as_str() {
                "link" => has_word(&el.name, LINK_COMMIT_WORDS),
                "button" | "menuitem" | "" => has_word(&el.name, COMMIT_WORDS),
                _ => false,
            };
            let probe = b.probe(&el).await;
            let submits = probe.is_submit && probe.submit_commits();
            // Something over the element (a cookie banner, a pop-up) would
            // take the click: say what, rather than click it (7.3).
            b.rect(&el).await?; // scrolls it into view first
            let info = b.target_info(&el).await;
            if let (true, Some(cover)) = (info.blocking, &info.covered) {
                return Err(format!(
                    "Not clicked: [{r_now}] {} is covered by {cover}. Close or answer that first (for a cookie \
                     banner prefer Reject or Necessary only), then click again.",
                    el.label()
                ));
            }
            if worded || submits {
                let why = if worded {
                    "This looks like it could buy, send, post, delete or confirm something".to_string()
                } else {
                    "This submits a form that sends information".to_string()
                };
                b.confirm(&ctx, format!("Click {}", el.label()), why, &probe).await?;
            }
            if let Some(cover) = info.covered.as_ref() {
                notes.push(format!("Check: {cover} is on top of {}, so the click went to it.", el.label()));
            }

            let url_before = b.state().await.map(|s| s.url).unwrap_or_default();
            let mut info = info;
            let mut retried = false;
            let mut settled = false;
            let mut attempt = 0;
            let effect = loop {
                attempt += 1;
                let rect = match b.rect(&el).await {
                    Ok(rect) => rect,
                    // Re-rendered again since it was found: once more by role and name.
                    Err(e) => match b.resolve(r).await {
                        Ok((fresh, Some(n), rr)) => {
                            let rect = b.rect(&fresh).await.map_err(|_| e)?;
                            (el, r_now) = (fresh, rr);
                            if !notes.iter().any(|x| x.starts_with("Retried: [")) {
                                notes.push(n);
                            }
                            rect
                        }
                        _ => return Err(e),
                    },
                };
                b.bridge.highlight(rect, format!("click {}", el.label()));
                tokio::time::sleep(Duration::from_millis(350)).await;
                // Clicks into text fields only focus them; nothing to watch.
                let base = if info.field { None } else { Some(b.watch(&el).await) };
                let (x, y) = (rect.x + rect.width * info.point.0, rect.y + rect.height * info.point.1);
                if !b.click_at(x, y, button, clicks).await? {
                    break None; // a dialog opened: its notice says what happened
                }
                b.settle(Duration::from_secs(10)).await;
                settled = true;
                let Some(base) = base else { break None };
                let effect = b.click_effect(&el, &base).await;
                // Once, and only for clicks that can't do anything twice.
                let again = attempt == 1
                    && effect == verify::ClickEffect::Nothing
                    && info.young
                    && !info.disabled
                    && button == "left"
                    && clicks == 1
                    && verify::safe_to_retry(&el.role, &el.name, worded || submits, probe.is_submit, probe.in_form, info.navigates());
                if !again {
                    break Some(effect);
                }
                b.network_quiet(Duration::from_secs(3)).await;
                tokio::time::sleep(Duration::from_millis(500)).await;
                let Ok((e, n, rr)) = b.resolve(&r_now).await else { break Some(effect) };
                if b.rect(&e).await.is_err() {
                    break Some(effect);
                }
                let fresh = b.target_info(&e).await;
                if fresh.blocking {
                    break Some(effect);
                }
                (el, r_now, info) = (e, rr, fresh);
                notes.extend(n);
                notes.push(
                    "Retried once: the first click came while the page was still starting up and did nothing; \
                     waited for it to settle and clicked again."
                        .into(),
                );
                retried = true;
            };
            if let Some(effect) = effect {
                let form = if verify::submitish(&el.role, &el.name, probe.is_submit, probe.in_form) {
                    b.form_check(Some(&el), &url_before).await
                } else {
                    None
                };
                let errors = form.as_ref().is_some_and(|l| !l.contains("no error messages"));
                if effect == verify::ClickEffect::Nothing && !errors {
                    notes.push(verify::dead_click_line(&el.label(), &info, &url_before, retried));
                } else {
                    notes.extend(form);
                }
            }
            let expect = input["expect"].as_str().map(str::to_string);
            let verify = Verify { lines: notes, settled, ..Default::default() };
            b.after_action_full(format!("Clicked [{r_now}] {}.", el.label()), format!("clicked {}", el.label()), expect, verify)
                .await
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
                "submit": {"type": "boolean", "description": "Press Enter after typing. Default false."},
                "expect": {"type": "string", "description": "Optional: text that should appear if this worked; it's checked and reported."}
            },
            "required": ["ref", "text"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Stage
    }
    fn site_acted_on(&self, _: &Value) -> BoxFuture<'static, Option<String>> {
        scope::current_site(&self.0)
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Type into {}", input["ref"].as_str().unwrap_or(""))
    }
    fn call(&self, _input: Value) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { Err("browser_type needs the tool context".to_string()) })
    }
    fn call_with(&self, input: Value, ctx: ToolContext) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["ref", "text", "clear", "submit", "expect"])?;
            let r = arg_str(&input, "ref")?;
            let text = input["text"].as_str().ok_or("Invalid input: `text` is required")?.to_string();
            let clear = input["clear"].as_bool().unwrap_or(true);
            let submit = input["submit"].as_bool().unwrap_or(false);
            let (el, stale, r_now) = b.resolve(r).await?;
            let probe = b.probe(&el).await;
            if probe.sensitive {
                return Err(format!(
                    "{} is a password, payment or verification-code field. The agent doesn't type these; \
                     ask the user to fill it in, then continue.",
                    el.label()
                ));
            }
            let rect = b.rect(&el).await?;
            b.bridge.highlight(rect, format!("type into {}", el.label()));
            // Text fields get the text typed; date, time and similar inputs
            // get their value set; rich text editors get it line by line.
            let note = b.type_into(&el, &text, clear).await?;
            let url_before = b.state().await.map(|s| s.url).unwrap_or_default();
            if submit {
                let probe = b.probe(&el).await;
                if probe.submit_commits() {
                    b.confirm(&ctx, format!("Submit the form from {}", el.label()), "Pressing Enter submits a form that sends information".into(), &probe).await?;
                }
                b.key(&parse_keys("Enter")?.0, 0).await?;
            }
            let shown: String = text.chars().take(60).collect();
            let what = format!("Typed \"{shown}\" into [{r_now}] {}{}.{note}", el.label(), if submit { " and pressed Enter" } else { "" });
            let summary = format!("typed into {}", el.label());
            // Read the field back once the page has had its say (7.1); after
            // Enter many fields clear on purpose (search, chat), so check the
            // form's response instead.
            let verify = Verify {
                lines: stale.into_iter().collect(),
                typed: if submit { Vec::new() } else { vec![(el.clone(), r_now.clone(), text.clone())] },
                form: (submit && probe.in_form).then(|| (Some(el.clone()), url_before)),
                ..Default::default()
            };
            b.after_action_full(what, summary, input["expect"].as_str().map(str::to_string), verify).await
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
                "times": {"type": "integer", "minimum": 1, "maximum": 50, "description": "Repeat count. Default 1."},
                "expect": {"type": "string", "description": "Optional: text that should appear if this worked; it's checked and reported."}
            },
            "required": ["keys"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Stage
    }
    fn site_acted_on(&self, _: &Value) -> BoxFuture<'static, Option<String>> {
        scope::current_site(&self.0)
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Press {}", input["keys"].as_str().unwrap_or(""))
    }
    fn call(&self, _input: Value) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { Err("browser_press needs the tool context".to_string()) })
    }
    fn call_with(&self, input: Value, ctx: ToolContext) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["keys", "times", "expect"])?;
            let combo = arg_str(&input, "keys")?.to_string();
            let times = input["times"].as_u64().unwrap_or(1).clamp(1, 50);
            let (key, mods) = parse_keys(&combo)?;
            if clipboard::clipboard_shortcut(&key.key, mods) {
                return Err(format!(
                    "{combo} would use the user's system clipboard, which the agent doesn't touch. \
                     Use browser_clipboard to copy and paste."
                ));
            }
            let mut in_form = false;
            if key.key == "Enter" && mods == 0 {
                let probe = b.probe_focused().await;
                if probe.submit_commits() {
                    b.confirm(&ctx, "Submit the form (Enter)".into(), "Pressing Enter submits a form that sends information".into(), &probe).await?;
                }
                // Enter in a text area is a new line, not a submit.
                in_form = probe.in_form && probe.tag != "textarea";
            }
            let url_before = b.state().await.map(|s| s.url).unwrap_or_default();
            for _ in 0..times {
                b.key(&key, mods).await?;
            }
            let what = if times > 1 { format!("Pressed {combo} {times} times.") } else { format!("Pressed {combo}.") };
            let verify = Verify { form: in_form.then(|| (None, url_before)), ..Default::default() };
            b.after_action_full(what, format!("pressed {combo}"), input["expect"].as_str().map(str::to_string), verify).await
        })
    }
}

impl Tool for Scroll {
    fn name(&self) -> &'static str {
        "browser_scroll"
    }
    fn description(&self) -> &'static str {
        "Scroll the current page, or bring an element into view by its ref. Use it to see more of a long page. \
         For feeds and results that load as you scroll (infinite scroll), set load_more: it keeps scrolling to \
         the end until nothing new appears (or until min_items entries are there) and says how much loaded."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "direction": {"type": "string", "enum": ["down", "up", "left", "right"], "description": "Default down."},
                "pages": {"type": "number", "minimum": 0.1, "maximum": 20, "description": "How far, in screenfuls. Default 1."},
                "ref": {"type": "string", "description": "Scroll this element into view instead."},
                "to": {"type": "string", "enum": ["top", "bottom"], "description": "Jump to the top or bottom of the page."},
                "load_more": {"type": "boolean", "description": "Keep scrolling to the end until no new content loads."},
                "min_items": {"type": "integer", "minimum": 1, "description": "With load_more: stop once this many entries are on the page."},
                "max_rounds": {"type": "integer", "minimum": 1, "maximum": 40, "description": "With load_more: most scrolls to the end. Default 12."}
            },
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        if input["load_more"].as_bool().unwrap_or(false) || input["min_items"].is_u64() {
            return "Scroll to load more".into();
        }
        match (input["ref"].as_str(), input["to"].as_str()) {
            (Some(r), _) => format!("Scroll to {r}"),
            (_, Some(t)) => format!("Scroll to the {t}"),
            _ => format!("Scroll {}", input["direction"].as_str().unwrap_or("down")),
        }
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["direction", "pages", "ref", "to", "load_more", "min_items", "max_rounds"])?;
            let st = b.state().await?;
            let what;
            if input["load_more"].as_bool().unwrap_or(false) || input["min_items"].is_u64() {
                let rounds = input["max_rounds"].as_u64().unwrap_or(scrolling::LOAD_ROUNDS).clamp(1, scrolling::MAX_LOAD_ROUNDS);
                let report = b.load_more(input["min_items"].as_u64(), rounds).await;
                return b.after_action(report, "scrolled to load more".into()).await;
            } else if let Some(r) = input["ref"].as_str() {
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

fn dialog_notice(d: &crate::DialogInfo) -> String {
    let what = match d.kind.as_str() {
        "confirm" => "A confirm dialog is open (OK / Cancel)",
        "prompt" => "A prompt dialog is open (asks for text)",
        "beforeunload" => "The page is asking whether to leave",
        _ => "An alert dialog is open",
    };
    let mut s = format!("{what}. The page is frozen until it's answered.\nMessage: \"{}\"", d.message);
    if d.kind == "prompt" && !d.default_text.is_empty() {
        s.push_str(&format!("\nDefault text: \"{}\"", d.default_text));
    }
    s.push_str("\nAnswer it with browser_dialog.");
    s
}

impl Tool for Dialog {
    fn name(&self) -> &'static str {
        "browser_dialog"
    }
    fn description(&self) -> &'static str {
        "Answer a JavaScript dialog the page opened (alert, confirm, prompt, or 'leave this page?'). While one \
         is open the page is frozen. accept=true presses OK, false presses Cancel; text answers a prompt. \
         Accepting a confirm that deletes, pays or sends asks the user first."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "accept": {"type": "boolean", "description": "OK (true) or Cancel (false)."},
                "text": {"type": "string", "description": "Text for a prompt dialog."}
            },
            "required": ["accept"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Stage
    }
    fn site_acted_on(&self, _: &Value) -> BoxFuture<'static, Option<String>> {
        scope::current_site(&self.0)
    }
    fn describe_call(&self, input: &Value) -> String {
        if input["accept"].as_bool().unwrap_or(false) { "Answer dialog: OK".into() } else { "Answer dialog: Cancel".into() }
    }
    fn call(&self, _input: Value) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { Err("browser_dialog needs the tool context".to_string()) })
    }
    fn call_with(&self, input: Value, ctx: ToolContext) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["accept", "text"])?;
            let accept = input["accept"].as_bool().ok_or("Invalid input: `accept` is required")?;
            let text = input["text"].as_str().map(str::to_string);
            let st = b.state().await?;
            let d = st.dialog.ok_or("No dialog is open.")?;
            if accept && d.kind == "confirm" && has_word(&d.message, COMMIT_WORDS) {
                let place = site(&st.url);
                let ok = ctx
                    .approver
                    .approve(
                        "browser_click",
                        format!("Press OK on: \"{}\"\n{}", d.message, st.url),
                        format!("The page is asking to confirm something that may not be undoable on {place}"),
                        vec![place],
                        None,
                    )
                    .await;
                if !ok {
                    return Err("The user declined confirming this dialog. Answer it with accept=false.".into());
                }
            }
            b.bridge.answer_dialog(accept, text).await?;
            let what = format!("Answered the {} dialog with {}.", d.kind, if accept { "OK" } else { "Cancel" });
            b.after_action(what.clone(), what.to_lowercase()).await
        })
    }
}

impl Tool for Select {
    fn name(&self) -> &'static str {
        "browser_select"
    }
    fn description(&self) -> &'static str {
        "Choose an option in a dropdown (<select>) by its visible label or value, by the dropdown's ref from \
         browser_read. For custom dropdowns that aren't real <select> elements, click the dropdown and then \
         click the option instead."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ref": {"type": "string", "description": "The dropdown's ref, like e123."},
                "option": {"type": "string", "description": "Option label (or value) to select."}
            },
            "required": ["ref", "option"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Stage
    }
    fn site_acted_on(&self, _: &Value) -> BoxFuture<'static, Option<String>> {
        scope::current_site(&self.0)
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Select “{}” in {}", input["option"].as_str().unwrap_or(""), input["ref"].as_str().unwrap_or(""))
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["ref", "option"])?;
            let r = arg_str(&input, "ref")?;
            let option = arg_str(&input, "option")?.to_string();
            let (el, stale, r) = b.resolve(r).await?;
            let r = r.as_str();
            let rect = b.rect(&el).await?;
            b.bridge.highlight(rect, format!("select “{option}”"));
            let f = format!(
                r#"function(){{
  if (this.tagName !== 'SELECT') return {{ok:false, error:'not a <select>'}};
  const want = {want}.trim().toLowerCase();
  const opts = Array.from(this.options);
  let o = opts.find(x => x.text.trim().toLowerCase() === want) || opts.find(x => x.value.toLowerCase() === want)
       || opts.find(x => x.text.trim().toLowerCase().includes(want));
  if (!o) return {{ok:false, error:'no option matching', options: opts.map(x => x.text.trim()).slice(0, 40)}};
  if (o.disabled) return {{ok:false, error:'that option is disabled'}};
  this.focus(); this.value = o.value;
  this.dispatchEvent(new Event('input', {{bubbles:true}}));
  this.dispatchEvent(new Event('change', {{bubbles:true}}));
  return {{ok:true, chosen:o.text.trim()}};
}}"#,
                want = serde_json::to_string(&option).unwrap_or_default()
            );
            let v = b.call_on(&el, &f).await?;
            if !v["ok"].as_bool().unwrap_or(false) {
                let err = v["error"].as_str().unwrap_or("couldn't select");
                if err == "not a <select>" {
                    return Err(format!(
                        "{} isn't a native dropdown. Click it to open it, then click the option.",
                        el.label()
                    ));
                }
                let opts = v["options"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" | "))
                    .unwrap_or_default();
                return Err(format!("{err} “{option}”. Options: {opts}"));
            }
            let chosen = v["chosen"].as_str().unwrap_or(&option).to_string();
            let what = format!("Selected “{chosen}” in [{r}] {}.", el.label());
            let verify = Verify { lines: stale.into_iter().collect(), ..Default::default() };
            b.after_action_full(what, format!("selected {chosen}"), None, verify).await
        })
    }
}

impl Tool for Hover {
    fn name(&self) -> &'static str {
        "browser_hover"
    }
    fn description(&self) -> &'static str {
        "Move the mouse over an element by its ref, without clicking: for menus that open on hover, tooltips, \
         and controls that only appear when hovered. Returns the page afterwards."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"ref": {"type": "string", "description": "Element ref, like e123."}},
            "required": ["ref"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Hover over {}", input["ref"].as_str().unwrap_or(""))
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["ref"])?;
            let (el, stale, r) = b.resolve(arg_str(&input, "ref")?).await?;
            let rect = b.rect(&el).await?;
            b.bridge.highlight(rect, format!("hover {}", el.label()));
            b.mouse("mouseMoved", rect.x + rect.width / 2.0, rect.y + rect.height / 2.0, "none", 0).await?;
            tokio::time::sleep(Duration::from_millis(400)).await;
            let verify = Verify { lines: stale.into_iter().collect(), ..Default::default() };
            b.after_action_full(format!("Hovered over [{r}] {}.", el.label()), format!("hovered {}", el.label()), None, verify).await
        })
    }
}

impl Tool for Find {
    fn name(&self) -> &'static str {
        "browser_find"
    }
    fn description(&self) -> &'static str {
        "Find text on the current page (like Ctrl+F): how many times it appears, a snippet around each match, \
         and the page scrolled to the first one. Faster than reading a long page when you know what to look for."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"text": {"type": "string", "description": "Text to look for (not case-sensitive)."}},
            "required": ["text"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Find “{}”", input["text"].as_str().unwrap_or(""))
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["text"])?;
            let text = arg_str(&input, "text")?.to_string();
            let q = serde_json::to_string(&text).unwrap_or_default();
            let expr = format!(
                r#"(() => {{
  const orig = {q}; const t = orig.toLowerCase();
  const body = document.body ? document.body.innerText : ''; const lower = body.toLowerCase();
  let i = 0, n = 0; const snips = [];
  while (t && (i = lower.indexOf(t, i)) !== -1) {{
    n++; if (snips.length < 8) snips.push(body.slice(Math.max(0, i - 70), i + t.length + 70).replace(/\s+/g, ' ').trim());
    i += t.length;
  }}
  try {{ getSelection().removeAllRanges(); window.find(orig, false, false, true); }} catch (e) {{}}
  return JSON.stringify({{n, snips}});
}})()"#
            );
            let r = b.bridge.cdp("Runtime.evaluate", json!({"expression": expr, "returnByValue": true})).await?;
            let v: Value = r["result"]["value"].as_str().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null);
            let n = v["n"].as_u64().unwrap_or(0);
            if n == 0 {
                return Ok(ToolOutput::text(format!("“{text}” isn't on this page."), format!("“{text}”: no matches")));
            }
            let snips: Vec<String> =
                v["snips"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).map(|x| format!("• …{x}…")).collect();
            let more = if n as usize > snips.len() { format!("\n(and {} more)", n as usize - snips.len()) } else { String::new() };
            let evidence = b.screenshot(55, 0.5).await.into_iter().collect();
            Ok(ToolOutput {
                content: format!("“{text}” appears {n} time(s); scrolled to the first.\n{}{more}", snips.join("\n")),
                summary: format!("“{text}”: {n} matches"),
                evidence,
                ..Default::default()
            })
        })
    }
}

impl Tool for Downloads {
    fn name(&self) -> &'static str {
        "browser_downloads"
    }
    fn description(&self) -> &'static str {
        "List files downloaded in the app's browser: name, where it was saved, progress, and whether it finished. \
         Downloads save automatically to the agent-unibrow downloads folder. Use browser_read_download to read one."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, _: &Value) -> String {
        "List downloads".into()
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &[])?;
            let list = b.bridge.downloads().await?;
            if list.is_empty() {
                return Ok(ToolOutput::text("No downloads yet.", "no downloads"));
            }
            let lines: Vec<String> = list
                .iter()
                .map(|d| {
                    let status = if d.complete {
                        "done".to_string()
                    } else if d.canceled {
                        "failed or cancelled".to_string()
                    } else {
                        format!("{}%", d.percent.max(0))
                    };
                    format!("- {} ({status}, {} bytes) → {}", d.name, d.received, d.path)
                })
                .collect();
            Ok(ToolOutput::text(lines.join("\n"), format!("{} downloads", list.len())))
        })
    }
}

/// Text from a downloaded file: text-like files directly, PDFs through
/// `pdftotext` when it's installed.
fn read_download_text(path: &std::path::Path) -> Result<String, String> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if ext == "pdf" {
        let out = std::process::Command::new("pdftotext")
            .args(["-layout"])
            .arg(path)
            .arg("-")
            .output()
            .map_err(|_| "Reading PDFs needs `pdftotext` (poppler), which isn't installed.".to_string())?;
        if !out.status.success() {
            return Err("pdftotext couldn't read this PDF.".into());
        }
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let bytes = std::fs::read(path).map_err(|e| format!("Couldn't read the file: {e}"))?;
    if bytes.iter().take(4096).filter(|b| **b == 0).count() > 0 {
        return Err("That file isn't text (it looks binary).".into());
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

impl Tool for ReadDownload {
    fn name(&self) -> &'static str {
        "browser_read_download"
    }
    fn description(&self) -> &'static str {
        "Read a downloaded file's text (CSV, TXT, JSON, HTML, Markdown, and PDF when pdftotext is installed), \
         by its name or path from browser_downloads. Only files in the downloads folder can be read."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file": {"type": "string", "description": "File name or path from browser_downloads."},
                "max_chars": {"type": "integer", "minimum": 1000, "maximum": 100000, "description": "Default 20000."}
            },
            "required": ["file"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Read download {}", input["file"].as_str().unwrap_or(""))
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        Box::pin(async move {
            check_keys(&input, &["file", "max_chars"])?;
            let file = arg_str(&input, "file")?;
            let max = input["max_chars"].as_u64().unwrap_or(20_000).clamp(1000, 100_000) as usize;
            let dir = crate::downloads_dir().canonicalize().map_err(|e| e.to_string())?;
            let candidate = if file.contains('/') { std::path::PathBuf::from(file) } else { dir.join(file) };
            let path = candidate.canonicalize().map_err(|_| format!("No downloaded file “{file}”."))?;
            if !path.starts_with(&dir) {
                return Err("Only files in the downloads folder can be read.".into());
            }
            let text = tokio::task::spawn_blocking(move || read_download_text(&path))
                .await
                .map_err(|e| e.to_string())??;
            let total = text.chars().count();
            let shown: String = text.chars().take(max).collect();
            let more = if total > max { format!("\n… ({} more characters)", total - max) } else { String::new() };
            Ok(ToolOutput::text(format!("{shown}{more}"), format!("read {file} ({total} chars)")))
        })
    }
}

impl Tool for Upload {
    fn name(&self) -> &'static str {
        "browser_upload"
    }
    fn description(&self) -> &'static str {
        "Attach a file from the user's computer to a file field on the page (e.g. a résumé or a photo). Give the \
         full path; the user must have told you which file. Always asks the user first. If you don't pass a ref, \
         the page's first file field is used."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Absolute path of the file to attach."},
                "ref": {"type": "string", "description": "File field ref, if you have one."}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Stage
    }
    fn site_acted_on(&self, _: &Value) -> BoxFuture<'static, Option<String>> {
        scope::current_site(&self.0)
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Upload {}", input["path"].as_str().unwrap_or(""))
    }
    fn call(&self, _input: Value) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { Err("browser_upload needs the tool context".to_string()) })
    }
    fn call_with(&self, input: Value, ctx: ToolContext) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["path", "ref"])?;
            let raw = arg_str(&input, "path")?;
            let expanded = match raw.strip_prefix("~/") {
                Some(rest) => dirs::home_dir().map(|h| h.join(rest)).ok_or("no home directory")?,
                None => std::path::PathBuf::from(raw),
            };
            let path = expanded.canonicalize().map_err(|_| format!("There's no file at {raw}."))?;
            if !path.is_file() {
                return Err(format!("{} isn't a file.", path.display()));
            }
            // Never upload from hidden folders (~/.ssh, ~/.config, keyrings...).
            if path.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')) {
                return Err("Files in hidden folders (like ~/.ssh or ~/.config) can't be uploaded.".into());
            }
            let mut field: Option<Element> = None;
            let backend_id = match input["ref"].as_str() {
                Some(r) => {
                    let el = b.element(r).await?;
                    let id = el.backend_id;
                    field = Some(el);
                    id
                }
                None => {
                    let doc = b.bridge.cdp("DOM.getDocument", json!({"depth": 0})).await?;
                    let root = doc["root"]["nodeId"].as_i64().ok_or("no document")?;
                    let q = b
                        .bridge
                        .cdp("DOM.querySelector", json!({"nodeId": root, "selector": "input[type=file]"}))
                        .await?;
                    let node = q["nodeId"].as_i64().filter(|n| *n != 0).ok_or("This page has no file field.")?;
                    let d = b.bridge.cdp("DOM.describeNode", json!({"nodeId": node})).await?;
                    d["node"]["backendNodeId"].as_i64().ok_or("file field has no id")?
                }
            };
            let st = b.state().await?;
            let place = site(&st.url);
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let ok = ctx
                .approver
                .approve(
                    "browser_upload",
                    format!("Attach {} ({size} bytes) on {place}\n{}", path.display(), st.url),
                    format!("Uploading sends this file to {place}"),
                    vec![format!("{place}:{}", path.display())],
                    b.screenshot(70, 1.0).await.ok(),
                )
                .await;
            if !ok {
                return Err(format!("The user declined uploading {}.", path.display()));
            }
            let params = json!({"files": [path.to_string_lossy()], "backendNodeId": backend_id});
            match &field {
                Some(el) => b.cdp_el(el, "DOM.setFileInputFiles", params).await,
                None => b.bridge.cdp("DOM.setFileInputFiles", params).await,
            }
                .map_err(|e| format!("Couldn't attach the file: {e}"))?;
            b.after_action(
                format!("Attached {}.", path.display()),
                format!("attached {}", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
            )
            .await
        })
    }
}

impl Tool for SavePdf {
    fn name(&self) -> &'static str {
        "browser_save_pdf"
    }
    fn description(&self) -> &'static str {
        "Save the current page as a PDF in the downloads folder (receipts, confirmations, articles). Returns the \
         file's path."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"name": {"type": "string", "description": "File name without .pdf (default: the page title)."}},
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, _: &Value) -> String {
        "Save page as PDF".into()
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["name"])?;
            let st = b.state().await?;
            let base = input["name"].as_str().map(str::to_string).unwrap_or_else(|| {
                if st.title.is_empty() { "page".into() } else { st.title.clone() }
            });
            let clean: String =
                base.chars().map(|c| if c.is_alphanumeric() || " -_.".contains(c) { c } else { '_' }).take(80).collect();
            let dir = crate::downloads_dir();
            let mut path = dir.join(format!("{}.pdf", clean.trim()));
            let mut n = 1;
            while path.exists() {
                path = dir.join(format!("{} ({n}).pdf", clean.trim()));
                n += 1;
            }
            let ok = b.bridge.save_pdf(path.to_string_lossy().into_owned()).await?;
            if !ok {
                return Err("Chromium couldn't save this page as a PDF.".into());
            }
            Ok(ToolOutput::text(format!("Saved the page as {}.", path.display()), "saved PDF"))
        })
    }
}

/// Finds the visible username (`kind` = "user") or password field.
const LOGIN_FIELD_JS: &str = r#"(kind) => {
  const vis = e => { const r = e.getBoundingClientRect(); const s = getComputedStyle(e); return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && !e.disabled; };
  const pw = Array.from(document.querySelectorAll('input[type=password]')).find(vis) || null;
  if (kind === 'pw') return pw;
  const sel = 'input[autocomplete~="username" i], input[autocomplete~="email" i], input[type=email], input[name*="user" i], input[name*="login" i], input[name*="email" i], input[id*="user" i], input[id*="email" i], input[id*="login" i]';
  const ok = e => vis(e) && !['password', 'hidden', 'submit', 'checkbox', 'radio'].includes((e.type || '').toLowerCase());
  let user = Array.from(document.querySelectorAll(sel)).find(ok) || null;
  if (!user && pw && pw.form) user = Array.from(pw.form.querySelectorAll('input[type=text], input:not([type])')).find(ok) || null;
  return user;
}"#;

impl Browser {
    /// Backend id of the visible username or password field, if any.
    async fn login_field(&self, kind: &str) -> Option<i64> {
        let expr = format!("({LOGIN_FIELD_JS})({})", serde_json::to_string(kind).ok()?);
        let r = self.bridge.cdp("Runtime.evaluate", json!({"expression": expr})).await.ok()?;
        let object_id = r["result"]["objectId"].as_str()?.to_string();
        let d = self.bridge.cdp("DOM.describeNode", json!({"objectId": object_id})).await.ok()?;
        d["node"]["backendNodeId"].as_i64()
    }

    /// Focuses a field, selects what's in it, and types `text` over it.
    /// Used for vault values: the text goes to Chromium, never to a model.
    async fn fill(&self, backend_id: i64, text: &str) -> Result<(), String> {
        self.bridge.cdp("DOM.focus", json!({"backendNodeId": backend_id})).await?;
        let el = Element { backend_id, role: String::new(), name: String::new(), frame: None };
        let _ = self
            .call_on(&el, "function(){ if (typeof this.select === 'function') this.select(); }")
            .await;
        self.bridge.cdp("Input.insertText", json!({"text": text})).await.map(|_| ())
    }
}

impl Tool for SignIn {
    fn name(&self) -> &'static str {
        "browser_sign_in"
    }
    fn description(&self) -> &'static str {
        "Fill the sign-in form on the current page with a login the user saved in the app. You never see the \
         password; the app types it. Only works on the site the login was saved for. Pass username when the \
         site has more than one saved login. Then click the page's sign-in button (that asks the user first). \
         For two-step sign-ins, call it again on the password page."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"username": {"type": "string", "description": "Which saved login, if there are several."}},
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Stage
    }
    fn site_acted_on(&self, _: &Value) -> BoxFuture<'static, Option<String>> {
        scope::current_site(&self.0)
    }
    fn describe_call(&self, _: &Value) -> String {
        "Fill a saved login".into()
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["username"])?;
            let st = b.state().await?;
            let host = url::Url::parse(&st.url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
            let saved = crate::vault::for_host(&host);
            let login = match (input["username"].as_str(), saved.len()) {
                (_, 0) => {
                    return Err(format!(
                        "There's no saved login for {}. Ask the user to sign in (browser_handoff), or to save a login in the Focus tab.",
                        site(&st.url)
                    ))
                }
                (Some(u), _) => saved
                    .iter()
                    .find(|l| l.username.eq_ignore_ascii_case(u))
                    .cloned()
                    .ok_or_else(|| format!("No saved login “{u}” for this site."))?,
                (None, 1) => saved[0].clone(),
                (None, _) => {
                    let names: Vec<&str> = saved.iter().map(|l| l.username.as_str()).collect();
                    return Err(format!("Several saved logins for this site: {}. Say which username.", names.join(", ")));
                }
            };
            let user_field = b.login_field("user").await;
            let pw_field = b.login_field("pw").await;
            if user_field.is_none() && pw_field.is_none() {
                return Err("There's no sign-in form on this page.".into());
            }
            let mut filled = Vec::new();
            if let Some(id) = user_field {
                b.fill(id, &login.username).await?;
                filled.push(format!("username ({})", login.username));
            }
            if let Some(id) = pw_field {
                let login2 = login.clone();
                let password = tokio::task::spawn_blocking(move || crate::vault::password(&login2))
                    .await
                    .map_err(|e| e.to_string())??;
                b.fill(id, &password).await?;
                filled.push("password".to_string());
            }
            let next = if pw_field.is_none() {
                " There's no password field yet: continue to the next step, then call browser_sign_in again."
            } else {
                " Now click the sign-in button."
            };
            b.after_action(
                format!("Filled the saved {} for {}.{next}", filled.join(" and "), login.site),
                format!("filled saved login for {}", login.site),
            )
            .await
        })
    }
}

impl Tool for Handoff {
    fn name(&self) -> &'static str {
        "browser_handoff"
    }
    fn description(&self) -> &'static str {
        "Hand the page to the user for something only they should do: sign in without a saved login, enter a \
         verification or two-factor code, solve a CAPTCHA, or make a choice. Shows them your message and waits \
         until they click Done (up to 15 minutes), then returns the page as it is."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"message": {"type": "string", "description": "What you need them to do, in one sentence."}},
            "required": ["message"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Ask the user: {}", input["message"].as_str().unwrap_or(""))
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["message"])?;
            let message = arg_str(&input, "message")?.to_string();
            let done = b.bridge.handoff(message.clone(), Duration::from_secs(15 * 60)).await?;
            if !done {
                return Err(format!("The user chose not to do this now: {message}"));
            }
            b.after_action("The user says they're done.".into(), "user finished handoff".into()).await
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
    fn system_clipboard_shortcuts_are_caught() {
        let blocked = |combo: &str| {
            let (k, m) = parse_keys(combo).unwrap();
            clipboard::clipboard_shortcut(&k.key, m)
        };
        assert!(blocked("Control+v"));
        assert!(blocked("Control+Shift+V"));
        assert!(blocked("Meta+c"));
        assert!(blocked("Control+x"));
        assert!(!blocked("Control+a"));
        assert!(!blocked("v"));
        assert!(!blocked("Shift+V"));
    }

    #[test]
    fn changes_section_reports_url_and_lines() {
        let snap = |url: &str, lines: &[&str]| Snapshot {
            url: url.into(),
            title: "Shop".into(),
            lines: lines.iter().map(|s| s.to_string()).collect(),
        };
        let a = snap("https://shop.test/cart", &["[e1] button \"Add\"", "text \"Cart is empty\""]);
        let b = snap("https://shop.test/cart", &["[e1] button \"Add\"", "text \"1 item\""]);
        let out = Browser::changes_since(Some(&a), Some(&b));
        assert!(out.contains("+ text \"1 item\"") && out.contains("- text \"Cart is empty\""), "{out}");
        let c = snap("https://shop.test/checkout", &["text \"Pay\""]);
        let out = Browser::changes_since(Some(&a), Some(&c));
        assert!(out.contains("URL: https://shop.test/cart → https://shop.test/checkout"), "{out}");
        assert!(out.contains("A different page"), "{out}");
        assert!(Browser::changes_since(None, Some(&a)).is_empty());
        assert!(Browser::changes_since(Some(&a), Some(&a)).contains("Nothing on the page changed"));
    }

    #[test]
    fn commit_words_catch_risky_buttons() {
        let risky = |n: &str| has_word(n, COMMIT_WORDS);
        assert!(risky("Place order"));
        assert!(risky("Send"));
        assert!(risky("Pay $13.98"));
        assert!(!risky("Add payment method"));
        assert!(!risky("Proceed to checkout"));
        assert!(!risky("Accept all cookies"));
        assert!(!risky("Preview"));
        assert!(!risky("Next page"));
        let link = |n: &str| has_word(n, LINK_COMMIT_WORDS);
        assert!(!link("Open checkout in a new tab"));
        assert!(!link("Buy now"));
        assert!(link("Delete account"));
    }
}
