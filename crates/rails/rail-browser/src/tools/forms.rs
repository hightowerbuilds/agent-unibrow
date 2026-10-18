//! Filling fields (roadmap 2.1): one field or a whole form, including the
//! inputs typing can't fill (date, time, number, range, color) and rich
//! text editors (contenteditable, as in Gmail's compose box or Notion).

use super::{Browser, Element, arg_str, check_keys, parse_keys};
use futures::future::BoxFuture;
use router::{Effect, Tool, ToolContext, ToolResult};
use serde_json::{Value, json};
use std::sync::Arc;

/// Most fields one browser_fill_form call fills.
const MAX_FIELDS: usize = 30;

/// What kind of field an element is, for choosing how to fill it.
const KIND_FN: &str = r#"function(){
  const tag = (this.tagName || '').toLowerCase();
  const type = ((this.getAttribute && this.getAttribute('type')) || '').toLowerCase();
  let kind = 'other';
  if (tag === 'select') kind = 'select';
  else if (tag === 'textarea') kind = 'text';
  else if (tag === 'input') {
    if (['checkbox', 'radio'].includes(type)) kind = 'check';
    else if (['date', 'time', 'datetime-local', 'month', 'week', 'range', 'color'].includes(type)) kind = 'value';
    else if (type === 'number') kind = 'number';
    else if (type === 'file') kind = 'file';
    else if (['submit', 'button', 'reset', 'image', 'hidden'].includes(type)) kind = 'button';
    else kind = 'text';
  } else if (this.isContentEditable) kind = 'rich';
  else {
    // A wrapper (role=textbox, a label) around the real field or editor.
    const inner = this.querySelector && this.querySelector('input:not([type=hidden]), textarea, select, [contenteditable=""], [contenteditable=true]');
    if (inner) kind = 'wrapper';
  }
  return { kind, type: type || tag, disabled: !!this.disabled, readOnly: !!this.readOnly };
}"#;

/// Sets an input's value the way a person's edit would land: through the
/// native setter (so frameworks like React see it), then input and change
/// events. Dates and times in other spellings are converted first.
const SET_VALUE_FN: &str = r#"function(raw){
  const type = (this.getAttribute('type') || '').toLowerCase();
  const pad = n => String(n).padStart(2, '0');
  const ymd = d => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
  const hm = s => {
    const m = String(s).trim().match(/^(\d{1,2})(?::(\d{2}))?(?::(\d{2}))?\s*([ap]\.?m\.?)?$/i);
    if (!m) return null;
    let h = Number(m[1]); const mi = m[2] || '00';
    if (m[4]) { const pm = /p/i.test(m[4]); if (h === 12) h = pm ? 12 : 0; else if (pm) h += 12; }
    return `${pad(h)}:${mi}${m[3] ? ':' + m[3] : ''}`;
  };
  let v = String(raw).trim();
  if (type === 'date' && !/^\d{4}-\d{2}-\d{2}$/.test(v)) { const d = new Date(v); if (!isNaN(d)) v = ymd(d); }
  if (type === 'time' && !/^\d{2}:\d{2}(:\d{2})?$/.test(v)) { v = hm(v) || v; }
  if (type === 'datetime-local' && !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}/.test(v)) {
    const m = v.match(/^(\d{4}-\d{2}-\d{2})[ T,]+(.+)$/);
    if (m && hm(m[2])) v = `${m[1]}T${hm(m[2])}`;
    else { const d = new Date(v); if (!isNaN(d)) v = `${ymd(d)}T${pad(d.getHours())}:${pad(d.getMinutes())}`; }
  }
  if (type === 'month' && !/^\d{4}-\d{2}$/.test(v)) { const d = new Date(v); if (!isNaN(d)) v = ymd(d).slice(0, 7); }
  if (type === 'number' || type === 'range') v = v.replace(/[,\s]/g, '');
  const proto = this instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
  const setter = Object.getOwnPropertyDescriptor(proto, 'value').set;
  const put = (x) => {
    setter.call(this, x);
    this.dispatchEvent(new Event('input', { bubbles: true, composed: true }));
    this.dispatchEvent(new Event('change', { bubbles: true, composed: true }));
  };
  const formats = { date: 'YYYY-MM-DD', time: 'HH:MM (24-hour)', 'datetime-local': 'YYYY-MM-DDTHH:MM', month: 'YYYY-MM', week: 'YYYY-Www', color: '#rrggbb' };
  const before = this.value;
  this.focus();
  put(v);
  // Rejected (the browser cleared it): put back what was there.
  if (this.value === '' && v !== '') {
    put(before);
    return { ok: false, kept: this.value, wanted: v, format: formats[type] || '' };
  }
  const bad = [];
  const val = this.validity || {};
  if (val.rangeUnderflow) bad.push(`below the minimum ${this.min}`);
  if (val.rangeOverflow) bad.push(`above the maximum ${this.max}`);
  if (val.stepMismatch) bad.push(`not a multiple of the step ${this.step}`);
  if (val.badInput) bad.push('not accepted by the field');
  // A slider moves to the nearest value it allows.
  return { ok: true, value: this.value, wanted: v, adjusted: this.value !== v, problems: bad };
}"#;

/// Selects what's in a field or editor so typing replaces it, or puts the
/// caret at the end to add to it.
const PLACE_CARET_FN: &str = r#"function(clear){
  const ce = this.isContentEditable;
  if (!ce && typeof this.value === 'string') {
    try {
      if (clear) this.select(); else { const n = this.value.length; this.setSelectionRange(n, n); }
    } catch (e) { if (clear && typeof this.select === 'function') this.select(); }
    return 'field';
  }
  if (ce) {
    const r = document.createRange();
    r.selectNodeContents(this);
    if (!clear) r.collapse(false);
    const s = getSelection(); s.removeAllRanges(); s.addRange(r);
    return 'rich';
  }
  return 'other';
}"#;

/// The real field inside a wrapper element (a labelled container, a
/// role=textbox shell around an editor). Returned as the element itself.
const INNER_FIELD_FN: &str = r#"function(){
  return this.querySelector('[contenteditable=""], [contenteditable=true], textarea, input:not([type=hidden]), select');
}"#;

/// What a field shows now (sensitive fields aren't filled, so never read).
const READ_BACK_FN: &str = r#"function(){
  if (this.isContentEditable) return (this.innerText || '').replace(/\s+/g, ' ').trim();
  if (this.tagName === 'SELECT') return (this.selectedOptions[0] || {}).text || '';
  if (this.type === 'checkbox' || this.type === 'radio') return this.checked ? 'checked' : 'unchecked';
  return String(this.value || '');
}"#;

fn shorten(s: &str, n: usize) -> String {
    if s.chars().count() > n { s.chars().take(n - 1).collect::<String>() + "…" } else { s.to_string() }
}

impl Browser {
    pub(super) async fn field_kind(&self, el: &Element) -> Value {
        self.call_on(el, KIND_FN).await.unwrap_or(Value::Null)
    }

    /// The editable element inside a wrapper, as its own [`Element`].
    pub(super) async fn inner_field(&self, el: &Element) -> Option<Element> {
        let node = self.cdp_el(el, "DOM.resolveNode", json!({"backendNodeId": el.backend_id})).await.ok()?;
        let object_id = node["object"]["objectId"].as_str()?.to_string();
        let r = self
            .cdp_el(el, "Runtime.callFunctionOn", json!({"objectId": object_id, "functionDeclaration": INNER_FIELD_FN}))
            .await
            .ok()?;
        let inner = r["result"]["objectId"].as_str()?.to_string();
        let d = self.cdp_el(el, "DOM.describeNode", json!({"objectId": inner})).await.ok()?;
        let backend_id = d["node"]["backendNodeId"].as_i64()?;
        Some(Element { backend_id, role: el.role.clone(), name: el.name.clone(), frame: el.frame })
    }

    /// Fills one text-like field, date/number input or rich text editor
    /// with `text` (replacing what's there unless `clear` is false). Never
    /// presses Enter on its own. Returns a note on what the field shows, or
    /// a warning when it didn't take.
    pub(super) async fn type_into(&self, el: &Element, text: &str, clear: bool) -> Result<String, String> {
        let mut kind = self.field_kind(el).await;
        let mut target = None;
        if kind["kind"] == "wrapper" {
            if let Some(inner) = self.inner_field(el).await {
                kind = self.field_kind(&inner).await;
                // The probe ran on the wrapper; check the real field too.
                if self.probe(&inner).await.sensitive {
                    return Err(format!("{} holds a password, payment or verification-code field.", el.label()));
                }
                target = Some(inner);
            }
        }
        let el = target.as_ref().unwrap_or(el);
        if kind["disabled"].as_bool().unwrap_or(false) {
            return Err(format!("{} is disabled.", el.label()));
        }
        match kind["kind"].as_str().unwrap_or("other") {
            "file" => return Err(format!("{} is a file field: use browser_upload.", el.label())),
            "select" => return Err(format!("{} is a dropdown: use browser_select (or browser_fill_form).", el.label())),
            "check" => return Err(format!("{} is a checkbox or radio button: click it (or use browser_fill_form).", el.label())),
            "button" => return Err(format!("{} is a button, not a field.", el.label())),
            "value" => return self.set_value(el, text).await,
            _ => {}
        }
        if kind["readOnly"].as_bool().unwrap_or(false) {
            return Err(format!("{} is read-only.", el.label()));
        }
        self.cdp_el(el, "DOM.focus", json!({"backendNodeId": el.backend_id}))
            .await
            .map_err(|_| format!("{} can't take text. Pick a textbox or other field.", el.label()))?;
        let placed = self.call_on_with(el, PLACE_CARET_FN, vec![json!(clear)]).await.unwrap_or(Value::Null);
        let rich = placed == "rich" || kind["kind"] == "rich";
        if text.is_empty() {
            if clear {
                self.key(&parse_keys("Backspace")?.0, 0).await?;
            }
        } else if rich {
            // Line by line, with Shift+Enter between lines: a soft line
            // break in editors, and never "send" in chat boxes where a plain
            // Enter sends.
            let (enter, _) = parse_keys("Shift+Enter")?;
            for (i, line) in text.split('\n').enumerate() {
                if i > 0 {
                    self.key(&enter, super::MOD_SHIFT).await?;
                }
                if !line.is_empty() {
                    self.bridge.cdp("Input.insertText", json!({"text": line})).await?;
                }
            }
        } else {
            self.bridge.cdp("Input.insertText", json!({"text": text})).await?;
        }
        if kind["kind"] == "number" {
            // Typing into number fields fails on some spellings ("1,200").
            let now = self.call_on(el, READ_BACK_FN).await.unwrap_or(Value::Null);
            if now.as_str().is_none_or(str::is_empty) && !text.is_empty() {
                return self.set_value(el, text).await;
            }
        }
        let shown = self.call_on(el, READ_BACK_FN).await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        let last_line = text.lines().last().unwrap_or("").trim();
        let mut squashed: String = last_line.split_whitespace().collect::<Vec<_>>().join(" ");
        if kind["kind"] == "number" {
            squashed.retain(|c| c != ',');
        }
        if !squashed.is_empty() && !shown.contains(&squashed) {
            return Ok(format!(
                " Warning: the field now shows “{}”, not what was typed; check it before going on.",
                shorten(&shown, 120)
            ));
        }
        if rich {
            return Ok(format!(" The editor now shows “{}”.", shorten(&shown, 120)));
        }
        if shown != text {
            return Ok(format!(" The field shows “{}”.", shorten(&shown, 120)));
        }
        Ok(String::new())
    }

    /// Sets a date, time, number, range or color input directly.
    async fn set_value(&self, el: &Element, value: &str) -> Result<String, String> {
        let v = self.call_on_with(el, SET_VALUE_FN, vec![json!(value)]).await?;
        let now = v["value"].as_str().unwrap_or("");
        if !v["ok"].as_bool().unwrap_or(false) {
            let format = v["format"].as_str().filter(|f| !f.is_empty()).map(|f| format!(" It takes {f}.")).unwrap_or_default();
            let kept = match v["kept"].as_str() {
                Some("") | None => " It's still empty.".to_string(),
                Some(k) => format!(" It still holds “{k}”."),
            };
            return Err(format!("{} didn't accept “{value}”.{kept}{format}", el.label()));
        }
        let problems: Vec<&str> =
            v["problems"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let nearest = if v["adjusted"].as_bool().unwrap_or(false) {
            format!(" (the nearest value it allows to {})", v["wanted"].as_str().unwrap_or(value))
        } else {
            String::new()
        };
        if problems.is_empty() {
            Ok(format!(" The field is set to {now}{nearest}."))
        } else {
            Ok(format!(" The field is set to {now}{nearest}, but the page says it's {}.", problems.join(" and ")))
        }
    }

    /// Ticks or unticks a checkbox (or picks a radio button) by clicking it,
    /// like a person would, if it isn't already that way.
    async fn set_checked(&self, el: &Element, want: bool) -> Result<String, String> {
        let now = self.call_on(el, "function(){ return !!this.checked; }").await?.as_bool().unwrap_or(false);
        if now == want {
            return Ok(String::new());
        }
        let is_radio = self.call_on(el, "function(){ return this.type === 'radio'; }").await?.as_bool().unwrap_or(false);
        if is_radio && !want {
            return Err(format!("{} is a radio button: choose another option instead of unticking it.", el.label()));
        }
        let rect = self.rect(el).await?;
        self.click_at(rect.x + rect.width / 2.0, rect.y + rect.height / 2.0, "left", 1).await?;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let after = self.call_on(el, "function(){ return !!this.checked; }").await?.as_bool().unwrap_or(false);
        if after != want {
            // Something covers it (a styled label); toggle it directly.
            self.call_on(el, "function(){ this.click(); }").await?;
        }
        Ok(String::new())
    }

    /// Chooses an option in a `<select>` by label or value.
    async fn choose_option(&self, el: &Element, option: &str) -> Result<String, String> {
        let f = r#"function(want){
  want = String(want).trim().toLowerCase();
  const opts = Array.from(this.options);
  const o = opts.find(x => x.text.trim().toLowerCase() === want) || opts.find(x => x.value.toLowerCase() === want)
       || opts.find(x => x.text.trim().toLowerCase().includes(want));
  if (!o) return { ok: false, options: opts.map(x => x.text.trim()).slice(0, 40) };
  if (o.disabled) return { ok: false, disabled: true };
  this.focus(); this.value = o.value;
  this.dispatchEvent(new Event('input', { bubbles: true }));
  this.dispatchEvent(new Event('change', { bubbles: true }));
  return { ok: true, chosen: o.text.trim() };
}"#;
        let v = self.call_on_with(el, f, vec![json!(option)]).await?;
        if v["ok"].as_bool().unwrap_or(false) {
            return Ok(format!(" Chose “{}”.", v["chosen"].as_str().unwrap_or(option)));
        }
        if v["disabled"].as_bool().unwrap_or(false) {
            return Err(format!("“{option}” is disabled in {}.", el.label()));
        }
        let opts: Vec<&str> = v["options"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        Err(format!("{} has no option “{option}”. Options: {}", el.label(), opts.join(" | ")))
    }

    /// Fills one field of any kind with `value`.
    async fn fill_any(&self, el: &Element, value: &Value) -> Result<String, String> {
        let kind = self.field_kind(el).await;
        let text = match value {
            Value::String(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            _ => return Err("values must be text, a number, or true/false".into()),
        };
        match kind["kind"].as_str().unwrap_or("other") {
            "select" => self.choose_option(el, &text).await,
            "check" => {
                let want = match text.trim().to_lowercase().as_str() {
                    "true" | "yes" | "on" | "checked" | "check" | "tick" | "ticked" | "1" | "x" => true,
                    "false" | "no" | "off" | "unchecked" | "uncheck" | "untick" | "0" | "" => false,
                    other => return Err(format!("for a checkbox use true or false, not “{other}”")),
                };
                self.set_checked(el, want).await
            }
            _ => self.type_into(el, &text, true).await,
        }
    }
}

/// `browser_fill_form`: several fields in one call.
pub struct FillForm(pub(super) Arc<Browser>);

impl Tool for FillForm {
    fn name(&self) -> &'static str {
        "browser_fill_form"
    }
    fn description(&self) -> &'static str {
        "Fill several fields of a form in one call, by their refs from browser_read: text fields, text areas, \
         rich text editors, dropdowns (by option label), checkboxes (true/false), radio buttons (true), and date, \
         time and number inputs (e.g. 2026-11-03, 14:30). Replaces what's in each field. It never submits: click \
         the form's button afterwards."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "fields": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_FIELDS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "ref": {"type": "string", "description": "Field ref from browser_read, like e123."},
                            "value": {"description": "Text, a number, or true/false for a checkbox.", "type": ["string", "number", "boolean"]}
                        },
                        "required": ["ref", "value"],
                        "additionalProperties": false
                    }
                },
                "expect": {"type": "string", "description": "Optional: text that should appear once the fields are filled; it's checked and reported."}
            },
            "required": ["fields"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Stage
    }
    fn site_acted_on(&self, _: &Value) -> BoxFuture<'static, Option<String>> {
        super::scope::current_site(&self.0)
    }
    fn describe_call(&self, input: &Value) -> String {
        let n = input["fields"].as_array().map(Vec::len).unwrap_or(0);
        format!("Fill {n} field{}", if n == 1 { "" } else { "s" })
    }
    fn call(&self, _input: Value) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { Err("browser_fill_form needs the tool context".to_string()) })
    }
    fn call_with(&self, input: Value, _ctx: ToolContext) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["fields", "expect"])?;
            let fields = input["fields"].as_array().cloned().ok_or("Invalid input: `fields` is required")?;
            if fields.is_empty() || fields.len() > MAX_FIELDS {
                return Err(format!("Invalid input: give 1 to {MAX_FIELDS} fields"));
            }
            // Look at every field first: refuse sensitive ones before typing
            // anything, so a form is never left half-filled by a refusal.
            let mut plan = Vec::new();
            let mut notes: Vec<String> = Vec::new();
            for f in &fields {
                check_keys(f, &["ref", "value"])?;
                let (el, stale, r) = b.resolve(arg_str(f, "ref")?).await?;
                notes.extend(stale);
                if b.probe(&el).await.sensitive {
                    return Err(format!(
                        "{} [{r}] is a password, payment or verification-code field. The agent doesn't fill these; \
                         leave it out and ask the user to fill it in. Nothing was filled.",
                        el.label()
                    ));
                }
                plan.push((r, el, f["value"].clone()));
            }
            let mut report = Vec::new();
            let mut failed = 0;
            let mut typed = Vec::new();
            for (r, el, value) in &plan {
                if let Ok(rect) = b.rect(el).await {
                    b.bridge.highlight(rect, format!("fill {}", el.label()));
                }
                let shown = match value {
                    Value::String(s) => format!("“{}”", shorten(s, 60)),
                    other => other.to_string(),
                };
                match b.fill_any(el, value).await {
                    Ok(note) => {
                        report.push(format!("• [{r}] {} ← {shown}.{note}", el.label()));
                        if let Value::String(text) = value {
                            typed.push((el.clone(), r.clone(), text.clone()));
                        }
                    }
                    Err(e) => {
                        failed += 1;
                        report.push(format!("• [{r}] {}: NOT filled. {e}", el.label()));
                    }
                }
            }
            let head = if failed == 0 {
                format!("Filled {} field(s). Nothing was submitted.", plan.len())
            } else {
                format!("Filled {} of {} field(s); {failed} failed. Nothing was submitted.", plan.len() - failed, plan.len())
            };
            // Each text field is read back once the page has settled (7.1).
            let verify = super::Verify { lines: notes, typed, ..Default::default() };
            b.after_action_full(
                format!("{head}\n{}", report.join("\n")),
                format!("filled {} fields", plan.len() - failed),
                input["expect"].as_str().map(str::to_string),
                verify,
            )
            .await
        })
    }
}
