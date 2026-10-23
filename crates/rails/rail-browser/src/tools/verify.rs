//! Act, then verify (roadmap 7.1), and recover from transient failures
//! (7.3).
//!
//! - **Checks.** After a click, the tool says plainly when nothing happened
//!   (same URL, not one DOM change, no new tab or download), and that a link
//!   didn't navigate. After typing it reads the field back once the page has
//!   settled and says when the value didn't stick or the page flags the
//!   field. After a submit it looks for the page's error messages:
//!   `aria-invalid`, `role=alert`, error boxes, and the browser's own
//!   validation that blocks a submit without changing the page. These come
//!   back as short "Check:" lines, and only when something needs attention
//!   (plus one line saying a submit showed no errors).
//! - **Retries.** A ref that went stale because the page re-rendered is
//!   found again by role and name, the way playbooks find recorded steps. A
//!   click that something covers waits a moment for it to go, and otherwise
//!   isn't made: the tool says what covers it ("a cookie banner"). A click
//!   while the page is loading waits for it first; one that lands while the
//!   page is still starting up and does nothing is made once more. Clicks
//!   that could buy, send, submit or toggle something are never repeated.
//!   Every retry is reported.

use super::{Browser, Element, has_word};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// How long a dead-looking click gets to show an effect before it's called dead.
const DEAD_GRACE: Duration = Duration::from_millis(1500);
/// A page younger than this may still be wiring up its buttons.
const YOUNG_PAGE_MS: f64 = 10_000.0;

/// Words on a control that mean doing it twice could do twice the thing:
/// add two items, toggle back, apply twice. Clicks on these are never
/// repeated, on top of everything [`super::COMMIT_WORDS`] already covers.
const NO_RETRY_WORDS: &[&str] = &[
    "add", "cart", "bag", "basket", "checkout", "save", "apply", "like", "unlike", "follow", "unfollow", "join",
    "vote", "upvote", "downvote", "accept", "agree", "decline", "reject", "toggle", "increase", "decrease", "plus",
    "minus", "more", "less", "next", "previous", "prev", "load", "refresh", "retry", "undo", "redo", "star", "pin",
];

/// Labels on buttons that usually send their form even when they're not
/// `type=submit` (forms run by scripts).
const SUBMITISH_WORDS: &[&str] = &[
    "continue", "next", "submit", "save", "send", "sign in", "log in", "login", "sign up", "register", "apply",
    "confirm", "create", "update", "done", "finish", "book", "pay", "checkout", "place order", "review",
];

/// Whether a click may be repeated automatically. Never for anything that
/// could commit (buy, send, submit) or that toggles or adds.
pub(super) fn safe_to_retry(role: &str, name: &str, commits: bool, is_submit: bool, in_form: bool, navigates: bool) -> bool {
    if commits || is_submit {
        return false;
    }
    // Following a link again only goes to the same place.
    if role == "link" && navigates {
        return !has_word(name, super::LINK_COMMIT_WORDS);
    }
    if matches!(role, "checkbox" | "radio" | "switch" | "menuitemcheckbox" | "menuitemradio" | "option" | "tab" | "slider" | "spinbutton") {
        return false;
    }
    if in_form && role == "button" {
        return false;
    }
    !has_word(name, super::COMMIT_WORDS) && !has_word(name, NO_RETRY_WORDS)
}

/// Whether clicking this button probably sends its form.
pub(super) fn submitish(role: &str, name: &str, is_submit: bool, in_form: bool) -> bool {
    is_submit || (in_form && role == "button" && has_word(name, SUBMITISH_WORDS))
}

/// Before a click, run on the target: is it covered at the points a click
/// could use, what covers it, and a few facts about the target.
const TARGET_FN: &str = r#"function(){
  const el = this, doc = el.ownerDocument;
  const root = (el.getRootNode && el.getRootNode().elementFromPoint) ? el.getRootNode() : doc;
  const up = n => n.parentNode || n.host || null;
  const inside = (n, a) => { while (n) { if (n === a) return true; n = up(n); } return false; };
  const tag = (el.tagName || '').toLowerCase();
  const type = ((el.getAttribute && el.getAttribute('type')) || '').toLowerCase();
  const link = el.closest ? el.closest('a[href]') : null;
  const href = link ? link.getAttribute('href') : null;
  const field = el.isContentEditable || tag === 'textarea' || tag === 'select'
    || (tag === 'input' && !['checkbox','radio','submit','button','reset','image','file','hidden','range','color'].includes(type));
  const disabled = !!el.disabled || el.getAttribute('aria-disabled') === 'true';
  const young = doc.readyState !== 'complete' || performance.now() < __YOUNG__;
  const r = el.getBoundingClientRect();
  const base = { href, field, disabled, young };
  if (r.width <= 0 || r.height <= 0) return Object.assign(base, { point: [0.5, 0.5] });
  const pts = [[0.5, 0.5], [0.25, 0.5], [0.75, 0.5], [0.5, 0.25], [0.5, 0.75]];
  let hit = null;
  for (const [fx, fy] of pts) {
    const x = r.left + r.width * fx, y = r.top + r.height * fy;
    if (x < 0 || y < 0 || x >= innerWidth || y >= innerHeight) return Object.assign(base, { point: [fx, fy] });
    const h = root.elementFromPoint(x, y);
    if (!h || inside(h, el)) return Object.assign(base, { point: [fx, fy] });
    // An ancestor that takes the click (a label, the button around an icon) is fine, and so is the field's own label.
    if (inside(el, h) && h !== doc.body && h !== doc.documentElement) return Object.assign(base, { point: [fx, fy] });
    if (el.labels && Array.from(el.labels).some(l => inside(h, l))) return Object.assign(base, { point: [fx, fy] });
    hit = hit || h;
  }
  // The covering thing: the outermost box around the hit that doesn't hold the target.
  let cover = hit;
  while (up(cover) && up(cover).nodeType === 1 && !inside(el, up(cover)) && up(cover) !== doc.body) cover = up(cover);
  const words = n => [n.id, typeof n.className === 'string' ? n.className : '', n.getAttribute && n.getAttribute('aria-label'), n.getAttribute && n.getAttribute('role')].join(' ');
  const all = [cover, ...cover.querySelectorAll('[id], [class], [role], [aria-label]')].slice(0, 60).map(words).join(' ');
  const text = (cover.innerText || '').replace(/\s+/g, ' ').trim();
  const pos = getComputedStyle(cover).position;
  let what = 'another element';
  if (/cookie|consent|gdpr|privacy/i.test(all) || /\bcookies?\b/i.test(text.slice(0, 400))) what = 'a cookie banner';
  else if (/dialog|alertdialog|modal|popup|pop-up|overlay|backdrop|lightbox|newsletter|interstitial/i.test(all) || cover.tagName === 'DIALOG' || cover.getAttribute('aria-modal') === 'true') what = 'a pop-up';
  else if (/chat|messenger|intercom|drift|zendesk/i.test(all)) what = 'a chat widget';
  else if (pos === 'fixed' || pos === 'sticky') what = 'a fixed bar or panel';
  const buttons = Array.from(cover.querySelectorAll('button, [role=button], a[href]'))
    .map(b => (b.innerText || b.getAttribute('aria-label') || '').replace(/\s+/g, ' ').trim()).filter(Boolean).slice(0, 4);
  const id = cover.id ? '#' + cover.id : '';
  // Clear overlays stop the click; a small thing on top (a styled span over a
  // custom checkbox) just gets it, as it would for a person.
  const cr = cover.getBoundingClientRect();
  const big = cr.width * cr.height >= 3 * r.width * r.height && (pos === 'fixed' || pos === 'absolute' || pos === 'sticky');
  const blocking = what !== 'another element' || big;
  return Object.assign(base, { point: [0.5, 0.5], covered: { what, tag: cover.tagName.toLowerCase() + id, text: text.slice(0, 90), buttons, blocking } });
}"#;

/// Starts watching for any effect of the click about to happen: DOM
/// changes, scrolling, requests, the target's own state.
const WATCH_FN: &str = r#"function(){
  const el = this, w = el.ownerDocument.defaultView || window;
  const sig = x => { const c = x.control || (x.querySelector && x.querySelector('input, select, textarea')) || {};
    return JSON.stringify([x.checked, x.value, x.selectedIndex, x.open, c.checked, c.value]); };
  try { if (w.__unibrowWatch) w.__unibrowWatch.obs.disconnect(); } catch (e) {}
  const st = { n: 0, res: w.performance.getEntriesByType('resource').length, sx: w.scrollX, sy: w.scrollY, sig: sig(el), href: w.location.href };
  st.obs = new MutationObserver(ms => { st.n += ms.length; });
  st.obs.observe(el.ownerDocument, { subtree: true, childList: true, attributes: true, characterData: true });
  w.__unibrowWatch = st;
  return true;
}"#;

/// What the watch saw. `finish` stops it.
const WATCHED_FN: &str = r#"function(finish){
  const el = this, w = el.ownerDocument.defaultView || window;
  const st = w.__unibrowWatch;
  if (!st) return null;
  const sig = x => { const c = x.control || (x.querySelector && x.querySelector('input, select, textarea')) || {};
    return JSON.stringify([x.checked, x.value, x.selectedIndex, x.open, c.checked, c.value]); };
  const out = { n: st.n, res: w.performance.getEntriesByType('resource').length - st.res,
    scrolled: w.scrollX !== st.sx || w.scrollY !== st.sy, state: sig(el) !== st.sig, href: w.location.href !== st.href };
  if (finish) { try { st.obs.disconnect(); } catch (e) {} delete w.__unibrowWatch; }
  return out;
}"#;

/// What a field holds now and whether the page flags it. Only run on
/// fields that aren't sensitive.
const FIELD_STATE_FN: &str = r#"function(){
  const el = this, doc = el.ownerDocument;
  const vis = e => { const r = e.getBoundingClientRect(); return r.width > 0 && r.height > 0 && getComputedStyle(e).visibility !== 'hidden'; };
  const squash = s => String(s || '').replace(/\s+/g, ' ').trim().slice(0, 140);
  const value = el.isContentEditable ? squash(el.innerText) : String(el.value == null ? '' : el.value);
  const msgs = [];
  const add = m => { m = squash(m); if (m && !msgs.includes(m)) msgs.push(m); };
  const ids = ((el.getAttribute('aria-describedby') || '') + ' ' + (el.getAttribute('aria-errormessage') || '')).split(/\s+/).filter(Boolean);
  const invalid = el.getAttribute('aria-invalid') === 'true';
  for (const id of ids) { const d = doc.getElementById(id); if (d && vis(d) && (invalid || /error|invalid|must|required|not valid|isn't|is not/i.test(d.innerText || ''))) add(d.innerText); }
  if (value && el.validity && !el.validity.valid && el.validationMessage) add(el.validationMessage);
  // The field's own box: the nearest few ancestors holding no other field.
  let box = null;
  for (let p = el.parentElement, i = 0; p && i < 4; p = p.parentElement, i++) {
    if (p.querySelectorAll('input:not([type=hidden]), select, textarea').length > 1) break;
    box = p;
  }
  if (box) for (const e of box.querySelectorAll('[role=alert], .error, .error-message, .invalid-feedback, .field-error, [class*="error" i]'))
    if (e !== el && !e.contains(el) && vis(e)) add(e.innerText);
  return { value, invalid, messages: msgs.slice(0, 3) };
}"#;

/// The page's error messages after a submit, for the form `this` is in
/// (or the whole page).
const FORM_ERRORS_FN: &str = r#"function(){
  const el = this, doc = el.ownerDocument || document;
  const form = el.form || (el.closest && el.closest('form')) || null;
  const vis = e => { const r = e.getBoundingClientRect(); return r.width > 0 && r.height > 0 && getComputedStyle(e).visibility !== 'hidden'; };
  const squash = s => String(s || '').replace(/\s+/g, ' ').trim().slice(0, 140);
  const label = x => squash((x.labels && x.labels[0] && x.labels[0].innerText) || x.getAttribute('aria-label') || x.placeholder || x.name || x.id || 'A field').slice(0, 40);
  const msgs = [];
  const add = m => { m = squash(m); if (m && !msgs.includes(m)) msgs.push(m); };
  let blocked = false;
  // The browser's own validation stops the submit and shows a bubble the page outline can't see.
  if (form && !form.noValidate) for (const x of form.elements)
    if (x.willValidate && x.validity && !x.validity.valid) { blocked = true; add(label(x) + ': ' + x.validationMessage); }
  const scope = form || doc;
  for (const x of scope.querySelectorAll('[aria-invalid=true]')) {
    if (!vis(x)) continue;
    const ids = ((x.getAttribute('aria-describedby') || '') + ' ' + (x.getAttribute('aria-errormessage') || '')).split(/\s+/).filter(Boolean);
    const said = ids.map(id => doc.getElementById(id)).filter(d => d && vis(d)).map(d => squash(d.innerText)).filter(Boolean)[0];
    add(label(x) + (said ? ': ' + said : ' is marked invalid'));
  }
  for (const a of doc.querySelectorAll('[role=alert], [aria-live=assertive], .error, .errors, .error-message, .invalid-feedback, .field-error, .form-error, .alert-danger, .alert-error'))
    if (vis(a) && squash(a.innerText)) add(a.innerText);
  return { messages: msgs.slice(0, 4), blocked, inForm: !!form };
}"#;

/// Facts about a click target, from [`TARGET_FN`].
#[derive(Default, Debug)]
pub(super) struct TargetInfo {
    pub href: Option<String>,
    pub field: bool,
    pub disabled: bool,
    /// The page may still be starting up (loading, or only just loaded).
    pub young: bool,
    /// Where to click, as fractions of the element's box.
    pub point: (f64, f64),
    /// What covers the element at every point tried.
    pub covered: Option<String>,
    /// The cover is an overlay (a banner, a pop-up): don't click through it.
    pub blocking: bool,
}

impl TargetInfo {
    fn from(v: &Value) -> Self {
        let covered = v.get("covered").filter(|c| c.is_object()).map(|c| {
            let text = c["text"].as_str().unwrap_or("");
            let buttons: Vec<&str> = c["buttons"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let mut s = format!("{} ({}", c["what"].as_str().unwrap_or("another element"), c["tag"].as_str().unwrap_or("?"));
            if !text.is_empty() {
                s.push_str(&format!(" “{text}”"));
            }
            s.push(')');
            if !buttons.is_empty() {
                s.push_str(&format!("; its buttons: {}", buttons.join(", ")));
            }
            s
        });
        let p = v["point"].as_array();
        let at = |i: usize| p.and_then(|p| p.get(i)).and_then(Value::as_f64).unwrap_or(0.5);
        Self {
            href: v["href"].as_str().map(str::to_string),
            field: v["field"].as_bool().unwrap_or(false),
            disabled: v["disabled"].as_bool().unwrap_or(false),
            young: v["young"].as_bool().unwrap_or(false),
            point: (at(0), at(1)),
            blocking: covered.is_some() && v["covered"]["blocking"].as_bool().unwrap_or(true),
            covered,
        }
    }

    /// A link that should take the browser somewhere when clicked.
    pub fn navigates(&self) -> bool {
        self.href.as_deref().is_some_and(|h| {
            let h = h.trim();
            !h.is_empty() && !h.starts_with('#') && !h.to_ascii_lowercase().starts_with("javascript:")
        })
    }
}

/// Everything outside the page's DOM a click could change.
pub(super) struct Baseline {
    url: String,
    tabs: usize,
    downloads: usize,
    watching: bool,
}

/// What a click did, as far as the tool can tell.
#[derive(Debug, PartialEq)]
pub(super) enum ClickEffect {
    /// Something changed (or a dialog opened, or the page went away).
    Some,
    /// Nothing at all changed.
    Nothing,
    /// The tool couldn't watch (a frame it can't reach).
    Unknown,
}

/// The watch's report, read as an effect.
pub(super) fn effect_of(watched: &Value) -> ClickEffect {
    if !watched.is_object() {
        return ClickEffect::Some; // the document it watched is gone: the page changed
    }
    let n = watched["n"].as_i64().unwrap_or(1);
    let res = watched["res"].as_i64().unwrap_or(1);
    let flags = ["scrolled", "state", "href"].iter().any(|k| watched[*k].as_bool().unwrap_or(true));
    if n == 0 && res <= 0 && !flags { ClickEffect::Nothing } else { ClickEffect::Some }
}

/// The "Check:" line for a click that did nothing.
pub(super) fn dead_click_line(label: &str, info: &TargetInfo, url: &str, retried: bool) -> String {
    let again = if retried { " (also after one retry)" } else { "" };
    if info.disabled {
        return format!(
            "Check: nothing happened{again}: {label} is disabled. The page may want something else done first (a required field, a choice)."
        );
    }
    if info.navigates() {
        return format!(
            "Check: the link didn't navigate{again}: still on {url}, and nothing on the page changed. It may be broken, \
             or need something else done first; don't assume you're on the next page."
        );
    }
    format!(
        "Check: nothing happened{again}: same URL and not one change on the page. The click may have missed, or {label} \
         doesn't do anything; don't assume it worked."
    )
}

/// The "Check:" line for a submit: the errors the page shows, or that it
/// shows none.
/// Messages as quoted text, dropping ones another already says (“Postcode:
/// Postcode must be 5 digits” and “Postcode must be 5 digits”).
fn quote_all(v: &Value) -> Vec<String> {
    let all: Vec<&str> = v["messages"].as_array().into_iter().flatten().filter_map(Value::as_str).map(|m| m.trim().trim_end_matches('.')).filter(|m| !m.is_empty()).collect();
    let mut out: Vec<&str> = Vec::new();
    for m in &all {
        if all.iter().any(|o| o != m && o.contains(m)) || out.contains(m) {
            continue;
        }
        out.push(m);
    }
    out.iter().map(|m| format!("“{m}”")).collect()
}

pub(super) fn form_line(v: &Value, url_changed: bool) -> Option<String> {
    let msgs = quote_all(v);
    if msgs.is_empty() {
        return (!url_changed).then(|| "Check: no error messages showed after submitting.".to_string());
    }
    let quoted = msgs.join("; ");
    Some(if v["blocked"].as_bool().unwrap_or(false) {
        format!("Check: the form wasn't sent: the browser stopped it because of invalid fields: {quoted}. Fix them and submit again.")
    } else {
        format!("Check: the page reported a problem after submitting: {quoted}. It probably wasn't accepted.")
    })
}

/// The "Check:" line for a field read back after typing, if it needs one.
pub(super) fn typed_line(r: &str, label: &str, typed: &str, v: &Value) -> Option<String> {
    let now = v["value"].as_str()?;
    let msgs = quote_all(v);
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let want = squash(typed.lines().last().unwrap_or(""));
    let kept = want.is_empty() || squash(now).contains(&want) || squash(now).replace(',', "").contains(&want.replace(',', ""));
    let mut out = Vec::new();
    if !kept {
        let short: String = now.chars().take(80).collect();
        out.push(if now.trim().is_empty() {
            format!("Check: [{r}] {label} is empty now: the page didn't keep what you typed.")
        } else {
            format!("Check: [{r}] {label} now shows “{short}”, not what you typed: the page changed or rejected it.")
        });
    }
    if !msgs.is_empty() || v["invalid"].as_bool().unwrap_or(false) {
        let said = if msgs.is_empty() {
            "it's marked invalid".to_string()
        } else {
            msgs.join("; ")
        };
        out.push(format!("Check: the page flags [{r}] {label}: {said}."));
    }
    (!out.is_empty()).then(|| out.join("\n"))
}

impl Browser {
    /// The element for `r`, finding it again by role and name if the ref
    /// went stale (7.3). The second value says so when that happened; the
    /// third is the ref that worked.
    pub(super) async fn resolve(&self, r: &str) -> Result<(Element, Option<String>, String), String> {
        let first = self.element(r).await;
        if let Ok(el) = &first {
            let connected = self.call_on(el, "function(){ return this.isConnected; }").await;
            if connected.map(|v| v != Value::Bool(false)).unwrap_or(false) && self.still_what_was_seen(r, el).await {
                return Ok((first.expect("ok"), None, r.to_string()));
            }
        }
        let gone = || format!("[{r}] is no longer on the page. Call browser_read for fresh refs.");
        let before = self.last_seen.lock().ok().and_then(|s| s.as_ref().map(|s| s.lines.clone())).unwrap_or_default();
        if super::playbooks::refind(&before, &before, r).is_none() {
            return Err(gone());
        }
        let model = self.page_model().await.map_err(|_| gone())?;
        let Some(new_r) = super::playbooks::refind(&before, &model.lines, r) else {
            return Err(format!(
                "[{r}] is no longer on the page, and the page has no single element with the same role and name now. \
                 Call browser_read for fresh refs."
            ));
        };
        let el = self.element(&new_r).await.map_err(|_| gone())?;
        let note = format!("Retried: [{r}] had gone stale (the page re-rendered it); found {} again as [{new_r}].", el.label());
        Ok((el, Some(note), new_r))
    }

    /// Whether `r` still names what the agent saw. Element ids are only
    /// unique within one renderer process, so once the page has moved on
    /// (another site, a tab switch, a popup closing) an old ref can resolve
    /// to some unrelated element: then it must have the same role and name
    /// as in the last outline.
    async fn still_what_was_seen(&self, r: &str, el: &Element) -> bool {
        let Some((url, seen)) = self
            .last_seen
            .lock()
            .ok()
            .and_then(|s| s.as_ref().map(|s| (s.url.clone(), super::playbooks::seen_as(&s.lines, r))))
        else {
            return true;
        };
        let now = self.state().await.map(|s| s.url).unwrap_or_default();
        if now == url {
            return true;
        }
        let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        seen.is_some_and(|(role, name)| role == el.role && squash(&name) == squash(&el.name))
    }

    /// Facts about the click target, waiting up to a second for something
    /// covering it to go away (a toast, an animation).
    pub(super) async fn target_info(&self, el: &Element) -> TargetInfo {
        let f = TARGET_FN.replace("__YOUNG__", &format!("{YOUNG_PAGE_MS}"));
        let mut info = TargetInfo::from(&self.call_on(el, &f).await.unwrap_or(Value::Null));
        if info.blocking {
            tokio::time::sleep(Duration::from_millis(900)).await;
            info = TargetInfo::from(&self.call_on(el, &f).await.unwrap_or(Value::Null));
        }
        info
    }

    /// Starts watching for the click's effects.
    pub(super) async fn watch(&self, el: &Element) -> Baseline {
        let url = self.state().await.map(|s| s.url).unwrap_or_default();
        let tabs = self.bridge.tabs(crate::bridge::TabOp::List).await.map(|t| t.len()).unwrap_or(0);
        let downloads = self.bridge.downloads().await.map(|d| d.len()).unwrap_or(0);
        let watching = self.call_on(el, WATCH_FN).await.map(|v| v == Value::Bool(true)).unwrap_or(false);
        Baseline { url, tabs, downloads, watching }
    }

    /// What the click did. A click that seems to have done nothing gets a
    /// short grace period to show an effect (a slow handler, a navigation
    /// about to start). Stops the watch.
    pub(super) async fn click_effect(&self, el: &Element, base: &Baseline) -> ClickEffect {
        let start = Instant::now();
        loop {
            let st = self.state().await.unwrap_or_default();
            // A dialog freezes the page (asking it anything would hang); it's an effect.
            if st.dialog.is_some() || st.crashed || st.auth.is_some() {
                return ClickEffect::Some;
            }
            let outside = st.url != base.url
                || self.bridge.tabs(crate::bridge::TabOp::List).await.map(|t| t.len()).unwrap_or(base.tabs) != base.tabs
                || self.bridge.downloads().await.map(|d| d.len()).unwrap_or(base.downloads) != base.downloads;
            if outside {
                self.settle(Duration::from_secs(8)).await;
                return ClickEffect::Some;
            }
            if !base.watching {
                return ClickEffect::Unknown;
            }
            let done = start.elapsed() >= DEAD_GRACE;
            let watched = match self.call_on_with(el, WATCHED_FN, vec![json!(false)]).await {
                Ok(v) => v,
                Err(_) => return ClickEffect::Some, // the element's document went away
            };
            let effect = effect_of(&watched);
            if effect != ClickEffect::Nothing || done {
                let _ = self.call_on_with(el, WATCHED_FN, vec![json!(true)]).await;
                if effect == ClickEffect::Some {
                    // Let whatever started finish before the page is read.
                    self.settle(Duration::from_secs(8)).await;
                }
                return effect;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// The form check after a submit from `el` (the field or button).
    pub(super) async fn form_check(&self, el: Option<&Element>, url_before: &str) -> Option<String> {
        let st = self.state().await.ok()?;
        if st.dialog.is_some() || st.crashed {
            return None;
        }
        let url_changed = st.url != url_before;
        let v = match el {
            Some(el) => match self.call_on(el, FORM_ERRORS_FN).await {
                Ok(v) => v,
                // The element is gone with its page: look at the page there is now.
                Err(_) => self.eval_on_focused(FORM_ERRORS_FN).await?,
            },
            None => self.eval_on_focused(FORM_ERRORS_FN).await?,
        };
        form_line(&v, url_changed)
    }

    /// Runs `function` with the focused element (or the body) as `this`.
    async fn eval_on_focused(&self, function: &str) -> Option<Value> {
        let expr = format!("({function}).call(document.activeElement || document.body)");
        let r = self.bridge.cdp("Runtime.evaluate", json!({"expression": expr, "returnByValue": true})).await.ok()?;
        Some(r["result"]["value"].clone()).filter(|v| v.is_object())
    }

    /// The field read back after typing, once the page has had its say.
    pub(super) async fn typed_check(&self, el: &Element, r: &str, typed: &str) -> Option<String> {
        if self.state().await.ok()?.dialog.is_some() {
            return None;
        }
        // Never read back sensitive fields (the tool won't type into them,
        // but a page can swap a field's type).
        if self.probe(el).await.sensitive {
            return None;
        }
        // Only fields that hold what was typed as typed: dates, dropdowns
        // and checkboxes show it differently.
        let mut target = el.clone();
        let mut kind = self.field_kind(el).await;
        if kind["kind"] == "wrapper" {
            target = self.inner_field(el).await?;
            kind = self.field_kind(&target).await;
        }
        if !matches!(kind["kind"].as_str(), Some("text" | "number" | "rich")) || self.probe(&target).await.sensitive {
            return None;
        }
        let v = self.call_on(&target, FIELD_STATE_FN).await.ok()?;
        typed_line(r, &el.label(), typed, &v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_harmless_clicks_are_retried() {
        assert!(safe_to_retry("link", "Next page", false, false, false, true), "a link only navigates");
        assert!(!safe_to_retry("link", "Next page", false, false, false, false), "a scripted link may step a carousel");
        assert!(!safe_to_retry("link", "Delete account", false, false, false, true));
        assert!(safe_to_retry("button", "Show details", false, false, false, false));
        assert!(!safe_to_retry("button", "Show details", true, false, false, false), "anything that asked the user");
        assert!(!safe_to_retry("button", "Go", false, true, true, false), "form submits");
        assert!(!safe_to_retry("button", "Show details", false, false, true, false), "buttons inside forms");
        assert!(!safe_to_retry("button", "Add to cart", false, false, false, false));
        assert!(!safe_to_retry("button", "Send", false, false, false, false));
        assert!(!safe_to_retry("button", "Pay now", false, false, false, false));
        assert!(!safe_to_retry("checkbox", "Gift wrap", false, false, false, false), "toggles");
        assert!(!safe_to_retry("button", "Like", false, false, false, false));
    }

    #[test]
    fn submit_buttons_are_recognised() {
        assert!(submitish("button", "Anything", true, true));
        assert!(submitish("button", "Continue", false, true));
        assert!(!submitish("button", "Continue", false, false), "not in a form");
        assert!(!submitish("button", "Preview", false, true));
    }

    #[test]
    fn effects_are_read_from_the_watch() {
        let quiet = json!({"n": 0, "res": 0, "scrolled": false, "state": false, "href": false});
        assert_eq!(effect_of(&quiet), ClickEffect::Nothing);
        assert_eq!(effect_of(&json!({"n": 3, "res": 0, "scrolled": false, "state": false, "href": false})), ClickEffect::Some);
        assert_eq!(effect_of(&json!({"n": 0, "res": 1, "scrolled": false, "state": false, "href": false})), ClickEffect::Some);
        assert_eq!(effect_of(&json!({"n": 0, "res": 0, "scrolled": false, "state": true, "href": false})), ClickEffect::Some, "a checkbox ticked");
        assert_eq!(effect_of(&Value::Null), ClickEffect::Some, "the document went away");
    }

    #[test]
    fn dead_clicks_are_said_plainly() {
        let link = TargetInfo { href: Some("page2.html".into()), ..Default::default() };
        let l = dead_click_line("link \"Next\"", &link, "http://x/1", false);
        assert!(l.starts_with("Check: the link didn't navigate: still on http://x/1"), "{l}");
        let hash = TargetInfo { href: Some("#".into()), ..Default::default() };
        assert!(!hash.navigates());
        assert!(dead_click_line("button \"Go\"", &hash, "u", true).contains("nothing happened (also after one retry)"));
        let off = TargetInfo { disabled: true, ..Default::default() };
        assert!(dead_click_line("button \"Go\"", &off, "u", false).contains("is disabled"));
    }

    #[test]
    fn target_info_describes_covers() {
        let v = json!({"href": null, "field": false, "disabled": false, "young": false, "point": [0.5, 0.5],
            "covered": {"what": "a cookie banner", "tag": "div#consent", "text": "We use cookies", "buttons": ["Accept all", "Reject all"]}});
        let i = TargetInfo::from(&v);
        assert_eq!(i.covered.as_deref(), Some("a cookie banner (div#consent “We use cookies”); its buttons: Accept all, Reject all"));
        assert!(i.blocking, "blocking unless the page says otherwise");
        let i = TargetInfo::from(&json!({"point": [0.25, 0.5]}));
        assert!(i.covered.is_none());
        assert_eq!(i.point, (0.25, 0.5));
    }

    #[test]
    fn form_lines() {
        let blocked = json!({"messages": ["Postcode: Please fill out this field."], "blocked": true});
        assert!(form_line(&blocked, false).unwrap().contains("the browser stopped it"));
        let shown = json!({"messages": ["Postcode: Postcode must be 5 digits.", "Postcode must be 5 digits."], "blocked": false});
        assert_eq!(
            form_line(&shown, false).unwrap(),
            "Check: the page reported a problem after submitting: “Postcode: Postcode must be 5 digits”. It probably wasn't accepted."
        );
        let none = json!({"messages": [], "blocked": false});
        assert_eq!(form_line(&none, false).unwrap(), "Check: no error messages showed after submitting.");
        assert!(form_line(&none, true).is_none(), "a new page speaks for itself");
    }

    #[test]
    fn typed_lines() {
        let ok = json!({"value": "Ada Lovelace", "invalid": false, "messages": []});
        assert!(typed_line("e1", "textbox \"Name\"", "Ada Lovelace", &ok).is_none());
        let commas = json!({"value": "1200", "invalid": false, "messages": []});
        assert!(typed_line("e1", "x", "1,200", &commas).is_none());
        let cleared = json!({"value": "", "invalid": true, "messages": ["Membership numbers start with M-"]});
        let l = typed_line("e1", "textbox \"Membership number\"", "12345", &cleared).unwrap();
        assert!(l.contains("is empty now") && l.ends_with("“Membership numbers start with M-”."), "{l}");
        let changed = json!({"value": "123", "invalid": false, "messages": []});
        assert!(typed_line("e1", "x", "12345", &changed).unwrap().contains("now shows “123”"));
        assert!(typed_line("e1", "x", "abc", &json!(null)).is_none());
    }
}
