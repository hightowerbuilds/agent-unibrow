//! The agent's clipboard (roadmap 2.4): copy text from the page, paste it
//! into a field.
//!
//! It's the app's own clipboard, kept in memory for the agent, and never
//! the system one. The agent can't read what the user copied: that's often
//! a password or a one-time code, and the user can paste it into the chat
//! when they mean to share it. Copying doesn't overwrite what the user
//! copied either. (`browser_press` refuses Ctrl+C / Ctrl+V for the same
//! reason; see `clipboard_shortcut`.)
//!
//! Pasting sends the page a real `paste` event carrying the text, so pages
//! that handle paste (code boxes split over several fields, editors that
//! clean up pasted text) see a paste; if the page doesn't take it itself,
//! the text is inserted like typing.

use super::{Browser, Element, check_keys};
use futures::future::BoxFuture;
use router::{Effect, Tool, ToolContext, ToolResult};
use serde_json::{Value, json};
use std::sync::Arc;

/// Longest text the agent's clipboard holds.
const MAX_CLIP: usize = 100_000;

/// The text of an element: a field's value, a dropdown's choice, or what it shows.
const TEXT_OF_FN: &str = r#"function(){
  if (this.tagName === 'SELECT') return (this.selectedOptions[0] || {}).text || '';
  if (typeof this.value === 'string' && (this.tagName === 'INPUT' || this.tagName === 'TEXTAREA')) {
    const s = this.selectionStart, e = this.selectionEnd;
    return (typeof s === 'number' && e > s) ? this.value.slice(s, e) : this.value;
  }
  return this.innerText || this.textContent || '';
}"#;

/// What's selected on the page, including inside a text field.
const SELECTION_JS: &str = r#"(() => {
  const a = document.activeElement;
  if (a && (a.tagName === 'INPUT' || a.tagName === 'TEXTAREA') && typeof a.selectionStart === 'number' && a.selectionEnd > a.selectionStart) {
    return { text: a.value.slice(a.selectionStart, a.selectionEnd), sensitive: a.matches(SEL) };
  }
  const s = getSelection();
  const text = s ? s.toString() : '';
  let sensitive = false;
  if (s && s.rangeCount) { const n = s.getRangeAt(0).commonAncestorContainer; const e = n.nodeType === 1 ? n : n.parentElement; sensitive = !!(e && e.closest && e.closest(SEL)); }
  return { text, sensitive };
})()"#;

/// Sends a `paste` event with `text`; returns whether the page left it to
/// the browser (no `preventDefault`).
const PASTE_EVENT_FN: &str = r#"function(text){
  const dt = new DataTransfer();
  dt.setData('text/plain', text);
  const target = (this.contains(document.activeElement) ? document.activeElement : this);
  const ev = new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true, composed: true });
  return target.dispatchEvent(ev);
}"#;

/// Whether a key combination reaches the system clipboard (copy, cut,
/// paste), which the agent mustn't touch.
pub(super) fn clipboard_shortcut(key: &str, mods: i64) -> bool {
    let k = key.to_ascii_lowercase();
    let ctrl_or_meta = mods & (super::MOD_CTRL | super::MOD_META) != 0;
    (ctrl_or_meta && matches!(k.as_str(), "c" | "v" | "x")) || (mods & super::MOD_SHIFT != 0 && key == "Insert")
}

fn preview(s: &str) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 200 { flat.chars().take(199).collect::<String>() + "…" } else { flat }
}

impl Browser {
    async fn copy_text(&self, el: Option<&Element>) -> Result<String, String> {
        let text = match el {
            Some(el) => {
                if self.probe(el).await.sensitive {
                    return Err(format!("{} is a password, payment or verification-code field; it can't be copied.", el.label()));
                }
                self.call_on(el, TEXT_OF_FN).await?.as_str().unwrap_or("").to_string()
            }
            None => {
                let expr = SELECTION_JS.replace("SEL", &serde_json::to_string(super::SENSITIVE_SELECTOR).unwrap_or_default());
                let r = self.bridge.cdp("Runtime.evaluate", json!({"expression": expr, "returnByValue": true})).await?;
                let v = &r["result"]["value"];
                if v["sensitive"].as_bool().unwrap_or(false) {
                    return Err("The selection is in a password, payment or verification-code field; it can't be copied.".into());
                }
                v["text"].as_str().unwrap_or("").to_string()
            }
        };
        Ok(text.chars().take(MAX_CLIP).collect())
    }

    async fn paste_into(&self, el: &Element, text: &str, clear: bool) -> Result<&'static str, String> {
        self.cdp_el(el, "DOM.focus", json!({"backendNodeId": el.backend_id}))
            .await
            .map_err(|_| format!("{} can't take text. Pick a textbox or other field.", el.label()))?;
        let place = r#"function(clear){
  if (this.isContentEditable) { const r = document.createRange(); r.selectNodeContents(this); if (!clear) r.collapse(false); const s = getSelection(); s.removeAllRanges(); s.addRange(r); return; }
  if (clear && typeof this.select === 'function') this.select();
}"#;
        let _ = self.call_on_with(el, place, vec![json!(clear)]).await;
        let default_allowed = self.call_on_with(el, PASTE_EVENT_FN, vec![json!(text)]).await?.as_bool().unwrap_or(true);
        if default_allowed {
            self.bridge.cdp("Input.insertText", json!({"text": text})).await?;
            Ok("inserted")
        } else {
            Ok("handled by the page")
        }
    }
}

/// `browser_clipboard`.
pub struct Clipboard(pub(super) Arc<Browser>);

impl Tool for Clipboard {
    fn name(&self) -> &'static str {
        "browser_clipboard"
    }
    fn description(&self) -> &'static str {
        "Copy and paste on the page with your own clipboard (not the user's system clipboard, which you can't \
         read or change). copy: the text of an element by ref (a field's value, a paragraph), text found on \
         the page (find: copies the page's own characters, e.g. a code shown in plain text), or the page's \
         current selection. paste: into a field by ref, as a real paste the page \
         can react to. Use it to move text exactly (codes, addresses, long passages) instead of retyping it. \
         Ctrl+C / Ctrl+V in browser_press don't work; use this."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["copy", "paste", "show"], "description": "show says what your clipboard holds."},
                "ref": {"type": "string", "description": "copy: element to copy from (default: the selection). paste: the field to paste into (required)."},
                "find": {"type": "string", "description": "copy: select this text on the page (first match, not case-sensitive) and copy it."},
                "clear": {"type": "boolean", "description": "paste: replace what's in the field. Default false (paste at the end)."}
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Stage
    }
    // Copying and showing only read; pasting changes the page (6.3).
    fn site_acted_on(&self, input: &Value) -> BoxFuture<'static, Option<String>> {
        if input["action"].as_str() == Some("paste") {
            super::scope::current_site(&self.0)
        } else {
            Box::pin(async { None })
        }
    }
    fn describe_call(&self, input: &Value) -> String {
        match (input["action"].as_str().unwrap_or(""), input["ref"].as_str()) {
            ("copy", Some(r)) => format!("Copy {r}"),
            ("copy", None) => match input["find"].as_str() {
                Some(f) => format!("Copy “{f}”"),
                None => "Copy the selection".into(),
            },
            ("paste", Some(r)) => format!("Paste into {r}"),
            _ => "Show the clipboard".into(),
        }
    }
    fn call(&self, _input: Value) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { Err("browser_clipboard needs the tool context".to_string()) })
    }
    fn call_with(&self, input: Value, _ctx: ToolContext) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["action", "ref", "find", "clear"])?;
            let held = || b.clipboard.lock().ok().and_then(|c| c.clone());
            match input["action"].as_str().unwrap_or("") {
                "copy" => {
                    let el = match input["ref"].as_str() {
                        Some(r) => Some(b.element(r).await?),
                        None => None,
                    };
                    if let Some(el) = &el {
                        if let Ok(rect) = b.rect(el).await {
                            b.bridge.highlight(rect, format!("copy {}", el.label()));
                        }
                    }
                    if let (None, Some(f)) = (&el, input["find"].as_str().filter(|f| !f.trim().is_empty())) {
                        let expr = format!(
                            "(() => {{ try {{ getSelection().removeAllRanges(); return window.find({}, false, false, true); }} catch (e) {{ return false; }} }})()",
                            serde_json::to_string(f).unwrap_or_default()
                        );
                        let r = b.bridge.cdp("Runtime.evaluate", json!({"expression": expr, "returnByValue": true})).await?;
                        if !r["result"]["value"].as_bool().unwrap_or(false) {
                            return Err(format!("“{f}” isn't on this page."));
                        }
                    }
                    let text = b.copy_text(el.as_ref()).await?;
                    if text.trim().is_empty() {
                        return Err(match &el {
                            Some(el) => format!("{} has no text to copy.", el.label()),
                            None => "Nothing is selected on the page. Pass a ref to copy an element's text.".into(),
                        });
                    }
                    let n = text.chars().count();
                    let shown = preview(&text);
                    if let Ok(mut c) = b.clipboard.lock() {
                        *c = Some(text);
                    }
                    let from = el.as_ref().map(|e| e.label()).unwrap_or_else(|| "the selection".into());
                    Ok(router::ToolOutput::text(
                        format!("Copied {n} characters from {from} to your clipboard: “{shown}”"),
                        format!("copied {n} characters"),
                    ))
                }
                "paste" => {
                    let r = input["ref"].as_str().ok_or("Invalid input: `ref` is required to paste")?;
                    let text = held().ok_or("Your clipboard is empty: copy something first.")?;
                    let el = b.element(r).await?;
                    if b.probe(&el).await.sensitive {
                        return Err(format!(
                            "{} is a password, payment or verification-code field. The agent doesn't fill these; \
                             ask the user to fill it in, then continue.",
                            el.label()
                        ));
                    }
                    let rect = b.rect(&el).await?;
                    b.bridge.highlight(rect, format!("paste into {}", el.label()));
                    let how = b.paste_into(&el, &text, input["clear"].as_bool().unwrap_or(false)).await?;
                    b.after_action(
                        format!(
                            "Pasted {} characters into [{r}] {} ({how}).",
                            text.chars().count(),
                            el.label()
                        ),
                        format!("pasted into {}", el.label()),
                    )
                    .await
                }
                "show" => Ok(match held() {
                    Some(t) => router::ToolOutput::text(
                        format!("Your clipboard holds {} characters: “{}”", t.chars().count(), preview(&t)),
                        "clipboard",
                    ),
                    None => router::ToolOutput::text("Your clipboard is empty.", "clipboard empty"),
                }),
                other => Err(format!("Unknown action `{other}`: use copy, paste or show")),
            }
        })
    }
}
