//! Drag and drop (roadmap 2.2): sliders, sortable lists, kanban cards,
//! drop zones.
//!
//! A drag is a real mouse press, a run of moves and a release, which is all
//! pointer-based widgets (sliders, most sortable libraries) need. HTML5
//! drag and drop (`draggable=true` with dragstart/drop handlers) starts a
//! system drag instead, which an off-screen page can't run; so drags are
//! intercepted (`Input.setInterceptDrags`) and finished with
//! `Input.dispatchDragEvent` over the drop point, the way automation tools
//! drive Chrome. If the page started no drag at all from a draggable
//! element, the drag events are sent from script as a last resort.

use super::{Browser, COMMIT_WORDS, Element, LINK_COMMIT_WORDS, check_keys, has_word};
use crate::bridge::Rect;
use futures::future::BoxFuture;
use router::{Effect, Tool, ToolContext, ToolResult};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// Drop targets whose name means the drop deletes or sends something.
const DROP_COMMIT_WORDS: &[&str] = &["trash", "bin", "delete", "remove", "archive", "send", "publish"];

/// Sends the HTML5 drag events from script: dragstart on the source,
/// dragenter/dragover/drop on whatever is under the drop point, dragend.
const SYNTH_DND_FN: &str = r#"function(x, y){
  const dt = new DataTransfer();
  const opts = (extra) => Object.assign({ bubbles: true, cancelable: true, composed: true, dataTransfer: dt, clientX: x, clientY: y }, extra || {});
  const src = this.closest('[draggable=true]') || this;
  src.dispatchEvent(new DragEvent('dragstart', opts()));
  const target = document.elementFromPoint(x, y);
  if (!target) { src.dispatchEvent(new DragEvent('dragend', opts())); return { ok: false, why: 'nothing under the drop point' }; }
  target.dispatchEvent(new DragEvent('dragenter', opts()));
  target.dispatchEvent(new DragEvent('dragover', opts()));
  target.dispatchEvent(new DragEvent('drop', opts()));
  src.dispatchEvent(new DragEvent('dragend', opts()));
  return { ok: true };
}"#;

/// The element a drag starts or ends on, named by its text or label: an
/// exact aria-label, else the innermost element whose text is exactly it,
/// else the smallest one containing it. Looks inside open shadow roots.
const BY_TEXT_JS: &str = r#"(t) => {
  t = String(t).trim().toLowerCase();
  const all = [];
  const visit = root => { for (const e of root.querySelectorAll('*')) { all.push(e); if (e.shadowRoot) visit(e.shadowRoot); } };
  visit(document);
  const vis = e => { const r = e.getBoundingClientRect(); return r.width > 0 && r.height > 0; };
  const label = e => (e.getAttribute('aria-label') || '').trim().toLowerCase();
  const text = e => (e.innerText || '').trim().replace(/\s+/g, ' ').toLowerCase();
  const shown = all.filter(e => !['SCRIPT', 'STYLE', 'HTML', 'BODY', 'HEAD'].includes(e.tagName) && vis(e));
  let hit = shown.find(e => label(e) === t);
  if (!hit) { const ex = shown.filter(e => text(e) === t); hit = ex.find(e => !ex.some(o => o !== e && e.contains(o))); }
  if (!hit) {
    const area = e => { const r = e.getBoundingClientRect(); return r.width * r.height; };
    const inc = shown.filter(e => label(e).includes(t) || text(e).includes(t)).sort((a, b) => area(a) - area(b));
    hit = inc[0];
  }
  return hit || null;
}"#;

/// What a press at one point and a release at another would click: the
/// page fires `click` when both land on the same element, however far
/// apart, so a "drag" within one button is a click on it. Returns
/// `{frame: true}` when either point is in a frame (its inside can't be
/// seen from here), else `{same, name, link}` for the clickable element
/// under both points.
const CLICK_BETWEEN_JS: &str = r#"(x1, y1, x2, y2) => {
  const at = (x, y) => {
    let e = document.elementFromPoint(x, y);
    while (e && e.shadowRoot) { const inner = e.shadowRoot.elementFromPoint(x, y); if (!inner || inner === e) break; e = inner; }
    if (!e) return null;
    if (e.tagName === 'IFRAME' || e.tagName === 'FRAME') return 'frame';
    return e.closest('button, a[href], [role=button], [role=link], [role=menuitem], input[type=submit], input[type=button], input[type=image], summary, label') || e;
  };
  const a = at(x1, y1), b = at(x2, y2);
  if (a === 'frame' || b === 'frame') return { frame: true };
  if (!a || a !== b) return { same: false };
  const name = (a.getAttribute('aria-label') || a.value || a.innerText || a.textContent || '').trim().replace(/\s+/g, ' ').slice(0, 200);
  return { same: true, name, link: a.matches('a[href], [role=link]') };
}"#;

fn point_arg(v: &Value) -> Option<(f64, f64)> {
    Some((v["x"].as_f64()?, v["y"].as_f64()?))
}

fn center(r: &Rect) -> (f64, f64) {
    (r.x + r.width / 2.0, r.y + r.height / 2.0)
}

impl Browser {
    /// An element by ref, or else by its visible text or label.
    async fn element_or_text(&self, what: &str) -> Result<Element, String> {
        if page_model::parse_frame_ref(what).is_some() {
            return self.element(what).await;
        }
        let expr = format!("({BY_TEXT_JS})({})", serde_json::to_string(what).unwrap_or_default());
        let r = self.bridge.cdp("Runtime.evaluate", json!({"expression": expr})).await?;
        let object_id = r["result"]["objectId"]
            .as_str()
            .ok_or_else(|| format!("Nothing on the page says “{what}”. Use a ref from browser_read."))?
            .to_string();
        let d = self.bridge.cdp("DOM.describeNode", json!({"objectId": object_id})).await?;
        let _ = self.bridge.cdp("Runtime.releaseObject", json!({"objectId": object_id})).await;
        let backend_id = d["node"]["backendNodeId"].as_i64().ok_or("that element has no id")?;
        let el = self.element(&page_model::ref_for(backend_id)).await?;
        let name = if el.name.is_empty() { what.to_string() } else { el.name };
        Ok(Element { name, ..el })
    }

    async fn mouse_buttons(&self, kind: &str, x: f64, y: f64, buttons: i64) -> Result<(), String> {
        let button = if kind == "mouseMoved" && buttons == 0 { "none" } else { "left" };
        self.bridge
            .cdp(
                "Input.dispatchMouseEvent",
                json!({"type": kind, "x": x, "y": y, "button": button, "buttons": buttons, "clickCount": if kind == "mouseMoved" { 0 } else { 1 }}),
            )
            .await
            .map(|_| ())
    }

    async fn take_drag_intercepted(&self) -> Option<Value> {
        let r = self.bridge.cdp("Unibrow.takeEvents", json!({"method": "Input.dragIntercepted"})).await.ok()?;
        r["events"].as_array()?.last().map(|e| e["params"]["data"].clone())
    }

    /// Presses at `from`, moves to `to` in `steps`, releases. Returns how the
    /// drop was delivered.
    async fn drag_path(&self, from: (f64, f64), to: (f64, f64), steps: u32) -> Result<&'static str, String> {
        let intercept = self.bridge.cdp("Input.setInterceptDrags", json!({"enabled": true})).await.is_ok();
        let _ = self.take_drag_intercepted().await; // drop stale ones
        self.mouse_buttons("mouseMoved", from.0, from.1, 0).await?;
        self.mouse_buttons("mousePressed", from.0, from.1, 1).await?;
        tokio::time::sleep(Duration::from_millis(60)).await;
        let mut dragged = None;
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            let (x, y) = (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
            self.mouse_buttons("mouseMoved", x, y, 1).await?;
            tokio::time::sleep(Duration::from_millis(25)).await;
            if intercept && dragged.is_none() {
                dragged = self.take_drag_intercepted().await;
            }
        }
        if intercept && dragged.is_none() {
            tokio::time::sleep(Duration::from_millis(100)).await;
            dragged = self.take_drag_intercepted().await;
        }
        let how = if let Some(data) = dragged {
            // An HTML5 drag: hover the drop point, then drop there.
            for kind in ["dragEnter", "dragOver", "dragOver", "drop"] {
                self.bridge
                    .cdp("Input.dispatchDragEvent", json!({"type": kind, "x": to.0, "y": to.1, "data": data}))
                    .await?;
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
            "html5"
        } else {
            "mouse"
        };
        self.mouse_buttons("mouseReleased", to.0, to.1, 0).await?;
        if intercept {
            let _ = self.bridge.cdp("Input.setInterceptDrags", json!({"enabled": false})).await;
        }
        Ok(how)
    }
}

/// `browser_drag`.
pub struct Drag(pub(super) Arc<Browser>);

impl Tool for Drag {
    fn name(&self) -> &'static str {
        "browser_drag"
    }
    fn description(&self) -> &'static str {
        "Drag with the mouse: from an element (by ref) or a point to another element or point, or by an offset. \
         For sliders, reordering lists, moving cards between columns, and drop zones. Points are viewport CSS \
         pixels, as in screenshots. Returns the page afterwards. Dropping onto something that deletes or sends \
         asks the user first."
    }
    fn input_schema(&self) -> Value {
        let point = json!({"type": "object", "properties": {"x": {"type": "number"}, "y": {"type": "number"}}, "required": ["x", "y"], "additionalProperties": false});
        json!({
            "type": "object",
            "properties": {
                "from": {"type": "string", "description": "The element to drag (its center is grabbed): a ref like e12, or its visible text."},
                "from_point": point,
                "to": {"type": "string", "description": "The element to drop on (its center): a ref, or its visible text or label."},
                "to_point": point,
                "by": {"type": "object", "description": "Drag this far from the start instead (e.g. a slider thumb by {x: 120, y: 0}).", "properties": {"x": {"type": "number"}, "y": {"type": "number"}}, "additionalProperties": false},
                "steps": {"type": "integer", "minimum": 1, "maximum": 50, "description": "Mouse moves along the way. Default 12."},
                "expect": {"type": "string", "description": "Optional: text that should appear if this worked; it's checked and reported."}
            },
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
        let from = input["from"].as_str().map(str::to_string).unwrap_or_else(|| "a point".into());
        let to = input["to"].as_str().map(str::to_string).unwrap_or_else(|| "a point".into());
        format!("Drag {from} to {to}")
    }
    fn call(&self, _input: Value) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { Err("browser_drag needs the tool context".to_string()) })
    }
    fn call_with(&self, input: Value, ctx: ToolContext) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["from", "from_point", "to", "to_point", "by", "steps", "expect"])?;
            let steps = input["steps"].as_u64().unwrap_or(12).clamp(1, 50) as u32;
            let mut source: Option<Element> = None;
            let from = if let Some(r) = input["from"].as_str() {
                let el = b.element_or_text(r).await?;
                let rect = b.rect(&el).await?;
                b.bridge.highlight(rect, format!("drag {}", el.label()));
                source = Some(el);
                center(&rect)
            } else {
                point_arg(&input["from_point"]).ok_or("Invalid input: give `from` (a ref) or `from_point`")?
            };
            let mut target: Option<Element> = None;
            let to = if let Some(r) = input["to"].as_str() {
                let el = b.element_or_text(r).await?;
                let rect = b.rect(&el).await?;
                target = Some(el);
                center(&rect)
            } else if let Some(p) = point_arg(&input["to_point"]) {
                p
            } else if input["by"].is_object() {
                (from.0 + input["by"]["x"].as_f64().unwrap_or(0.0), from.1 + input["by"]["y"].as_f64().unwrap_or(0.0))
            } else {
                return Err("Invalid input: give `to` (a ref), `to_point` or `by`".into());
            };
            // Scrolling the target into view can move the source: measure again.
            let from = match &source {
                Some(el) if target.is_some() => b.rect(el).await.map(|r| center(&r)).unwrap_or(from),
                _ => from,
            };
            if let Some(t) = &target {
                if has_word(&t.name, DROP_COMMIT_WORDS) || has_word(&t.name, LINK_COMMIT_WORDS) {
                    let what = source.as_ref().map(|s| s.label()).unwrap_or_else(|| "something".into());
                    b.confirm(
                        &ctx,
                        format!("Drag {what} onto {}", t.label()),
                        "Dropping here could delete or send something".into(),
                        &Default::default(),
                    )
                    .await?;
                }
            }
            // A press and release on one element is a click on it: ask
            // first just as browser_click would.
            let expr = format!("({CLICK_BETWEEN_JS})({}, {}, {}, {})", from.0, from.1, to.0, to.1);
            let between = b
                .bridge
                .cdp("Runtime.evaluate", json!({"expression": expr, "returnByValue": true}))
                .await
                .map(|r| r["result"]["value"].clone())
                .unwrap_or(Value::Null);
            let clicks = if between["frame"].as_bool().unwrap_or(false) {
                Some(("something inside a frame".to_string(), "This drag is inside a frame, where it could click a button".to_string()))
            } else if between["same"].as_bool().unwrap_or(false) {
                let name = between["name"].as_str().unwrap_or_default().to_string();
                let words = if between["link"].as_bool().unwrap_or(false) { LINK_COMMIT_WORDS } else { COMMIT_WORDS };
                has_word(&name, words).then(|| {
                    (format!("“{name}”"), "This drag starts and ends on the same button, which clicks it; it looks like it could buy, send, post, delete or confirm something".to_string())
                })
            } else {
                None
            };
            if let Some((what, why)) = clicks {
                b.confirm(&ctx, format!("Drag on {what}"), why, &Default::default()).await?;
            }
            let how = b.drag_path(from, to, steps).await?;
            let mut what = format!(
                "Dragged {} from ({:.0}, {:.0}) to {} ({:.0}, {:.0}).",
                source.as_ref().map(|s| s.label()).unwrap_or_else(|| "the mouse".into()),
                from.0,
                from.1,
                target.as_ref().map(|t| t.label()).unwrap_or_else(|| "the point".into()),
                to.0,
                to.1
            );
            if how == "html5" {
                what.push_str(" The page handled it as a drag-and-drop.");
            } else if let Some(src) = &source {
                // A draggable element that started no drag: the page expects
                // HTML5 drag events, so send them from script.
                let draggable = b
                    .call_on(src, "function(){ return !!(this.closest && this.closest('[draggable=true]')); }")
                    .await
                    .ok()
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if draggable && src.frame.is_none() {
                    let v = b.call_on_with(src, SYNTH_DND_FN, vec![json!(to.0), json!(to.1)]).await?;
                    if v["ok"].as_bool().unwrap_or(false) {
                        what.push_str(" Sent the drag-and-drop events from script.");
                    }
                }
            }
            b.after_action_expecting(what, "dragged".into(), input["expect"].as_str().map(str::to_string)).await
        })
    }
}
