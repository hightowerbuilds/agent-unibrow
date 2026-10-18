//! Frames and shadow DOM in the page outline (roadmap 2.6).
//!
//! `Accessibility.getFullAXTree` only covers one document. The outline
//! stitches in every `<iframe>`'s document where the iframe is:
//!
//! - frames in the page's own process (same-site iframes) are asked for by
//!   `frameId` on the page's DevTools session, and their refs are ordinary
//!   `e123` refs: backend node ids are unique within a renderer process, so
//!   clicking and typing work on them unchanged;
//! - out-of-process iframes (other sites, under site isolation) have their
//!   own DevTools target. `Target.setAutoAttach` with `flatten` gives each one
//!   a child session on the same in-process DevTools connection; commands
//!   for it carry that `sessionId`. Their refs look like `f2e123` (frame 2),
//!   because backend ids from another process can collide with the page's.
//!   Their boxes are relative to the frame, so clicks add the `<iframe>`'s
//!   position in its parent.
//!
//! Shadow DOM needs nothing special for reading: the accessibility tree is
//! built from the flattened tree, so open and closed shadow roots both show
//! up, and their backend ids resolve like any other node. Sensitive fields
//! are looked for inside open shadow roots too.

use super::{Browser, Element, SENSITIVE_SELECTOR};
use page_model::{FrameInput, PageModel, from_frames, parse_frame_ref, ref_for};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// Frames beyond this many are left out (ad-heavy pages have dozens).
const MAX_FRAMES: usize = 12;
/// Frames smaller than this (CSS px²) are trackers or spacers.
const MIN_FRAME_AREA: f64 = 2_500.0;
/// Time budget for fetching frame trees on top of the page's own.
const FRAME_BUDGET: Duration = Duration::from_secs(4);

/// How to reach an out-of-process frame.
#[derive(Clone, Debug)]
pub(super) struct FrameRoute {
    /// DevTools child session for the frame's target.
    pub session: String,
    /// The `<iframe>` element showing it: (its frame index, None for the
    /// page's process; its backend id there).
    pub owner: (Option<u32>, i64),
}

/// Out-of-process frames the agent has seen, by the index used in refs.
#[derive(Default)]
pub(super) struct FrameRegistry {
    /// targetId (= frameId) → ref index, stable while the app runs.
    index: HashMap<String, u32>,
    next: u32,
    /// Child sessions attached so far: targetId → (sessionId, url).
    sessions: HashMap<String, (String, String)>,
    /// ref index → route, as of the last outline.
    routes: HashMap<u32, FrameRoute>,
}

impl FrameRegistry {
    fn index_for(&mut self, target: &str) -> u32 {
        if let Some(i) = self.index.get(target) {
            return *i;
        }
        self.next += 1;
        self.index.insert(target.to_string(), self.next);
        self.next
    }
}

fn host_of(url: &str) -> String {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default()
}

/// Finds sensitive fields and draggable boxes, looking inside open shadow
/// roots. Returns one array: the sensitive fields, the string "|", then the
/// draggable elements.
fn marks_js() -> String {
    format!(
        r#"(() => {{
  const sel = {sel};
  const sens = [], drag = [];
  const visit = (root) => {{
    try {{ root.querySelectorAll(sel).forEach(e => sens.push(e)); }} catch (e) {{}}
    try {{ root.querySelectorAll('[draggable=true]').forEach(e => drag.push(e)); }} catch (e) {{}}
    const w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT);
    for (let n = w.currentNode; n; n = w.nextNode()) {{ if (n.shadowRoot) visit(n.shadowRoot); }}
  }};
  visit(document);
  return [...sens.slice(0, 200), '|', ...drag.slice(0, 60)];
}})()"#,
        sel = serde_json::to_string(SENSITIVE_SELECTOR).unwrap_or_default()
    )
}

/// One document to fetch: where its commands go and what it's called.
struct Doc {
    /// Index in the `FrameInput` list being built.
    slot: usize,
    session: Option<String>,
    /// For out-of-process frames, their ref index.
    oopif: Option<u32>,
}

impl Browser {
    /// Sends a command to the page, or to an out-of-process frame's session.
    pub(super) async fn cdp_in(&self, session: Option<&str>, method: &str, mut params: Value) -> Result<Value, String> {
        if let Some(s) = session {
            params["__session"] = json!(s);
        }
        self.bridge.cdp(method, params).await
    }

    /// Sends a command about `el` to the session its frame lives in.
    pub(super) async fn cdp_el(&self, el: &Element, method: &str, params: Value) -> Result<Value, String> {
        let session = self.route(el.frame)?.map(|r| r.session);
        self.cdp_in(session.as_deref(), method, params).await
    }

    pub(super) fn route(&self, frame: Option<u32>) -> Result<Option<FrameRoute>, String> {
        let Some(f) = frame else { return Ok(None) };
        let reg = self.frames.lock().map_err(|_| "frame registry poisoned")?;
        reg.routes.get(&f).cloned().map(Some).ok_or_else(|| {
            format!("Frame f{f} is no longer on the page. Call browser_read for fresh refs.")
        })
    }

    /// Splits a ref into its frame (for out-of-process frames) and backend id.
    pub(super) fn parse_any_ref(&self, r: &str) -> Result<(Option<u32>, i64), String> {
        parse_frame_ref(r).ok_or_else(|| format!("`{r}` isn't a ref. Use a ref like e123 from browser_read."))
    }

    /// The ref the agent sees for an element.
    pub(super) fn ref_text(el: &Element) -> String {
        match el.frame {
            Some(f) => format!("f{f}{}", ref_for(el.backend_id)),
            None => ref_for(el.backend_id),
        }
    }

    /// Where an out-of-process frame's (0, 0) is in the page viewport: the
    /// content box of each `<iframe>` up the chain, added up.
    pub(super) async fn frame_offset(&self, frame: Option<u32>) -> Result<(f64, f64), String> {
        let mut x = 0.0;
        let mut y = 0.0;
        let mut cur = frame;
        let mut hops = 0;
        while let Some(route) = self.route(cur)? {
            let parent_session = self.route(route.owner.0)?.map(|r| r.session);
            let bm = self
                .cdp_in(parent_session.as_deref(), "DOM.getBoxModel", json!({"backendNodeId": route.owner.1}))
                .await
                .map_err(|_| "The frame isn't visible on the page.".to_string())?;
            let q: Vec<f64> = bm["model"]["content"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_f64).collect())
                .unwrap_or_default();
            if q.len() >= 2 {
                x += q[0];
                y += q[1];
            }
            cur = route.owner.0;
            hops += 1;
            if hops > 8 {
                break;
            }
        }
        Ok((x, y))
    }

    /// Scrolls the `<iframe>`s holding an out-of-process frame into view,
    /// outermost first.
    pub(super) async fn scroll_frame_into_view(&self, frame: Option<u32>) {
        let mut chain = Vec::new();
        let mut cur = frame;
        while let Ok(Some(route)) = self.route(cur) {
            chain.push(route.owner);
            cur = route.owner.0;
            if chain.len() > 8 {
                break;
            }
        }
        for (parent, backend) in chain.into_iter().rev() {
            let session = self.route(parent).ok().flatten().map(|r| r.session);
            let _ = self
                .cdp_in(session.as_deref(), "DOM.scrollIntoViewIfNeeded", json!({"backendNodeId": backend}))
                .await;
        }
    }

    /// Keeps the child-session list current from DevTools events, and asks
    /// for out-of-process frames to be attached (on the page and in every
    /// frame, for frames nested in frames).
    async fn attach_frames(&self) {
        let _ = self
            .bridge
            .cdp("Target.setAutoAttach", json!({"autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true}))
            .await;
        for _ in 0..3 {
            let new = self.take_target_events().await;
            if new.is_empty() {
                break;
            }
            for s in new {
                let _ = self
                    .cdp_in(
                        Some(&s),
                        "Target.setAutoAttach",
                        json!({"autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true}),
                    )
                    .await;
            }
        }
    }

    /// Applies kept `Target.*` events; returns newly attached session ids.
    async fn take_target_events(&self) -> Vec<String> {
        let events = self
            .bridge
            .cdp("Unibrow.takeEvents", json!({"method": "Target."}))
            .await
            .ok()
            .map(|v| v["events"].as_array().cloned().unwrap_or_default())
            .unwrap_or_default();
        let mut new = Vec::new();
        let Ok(mut reg) = self.frames.lock() else { return new };
        for e in events {
            let p = &e["params"];
            match e["method"].as_str().unwrap_or("") {
                "Target.attachedToTarget" => {
                    let info = &p["targetInfo"];
                    if info["type"].as_str() != Some("iframe") {
                        continue;
                    }
                    let (Some(target), Some(session)) = (info["targetId"].as_str(), p["sessionId"].as_str()) else {
                        continue;
                    };
                    reg.sessions.insert(target.to_string(), (session.to_string(), info["url"].as_str().unwrap_or("").to_string()));
                    new.push(session.to_string());
                }
                "Target.detachedFromTarget" => {
                    if let Some(session) = p["sessionId"].as_str() {
                        reg.sessions.retain(|_, (s, _)| s != session);
                    }
                }
                _ => {}
            }
        }
        new
    }

    /// Backend ids of sensitive fields, and of draggable elements, in one
    /// document (`frame_id` for a frame in the page's process; None for the
    /// page or a frame session's own document).
    async fn marks_in(&self, session: Option<&str>, frame_id: Option<&str>) -> (HashSet<i64>, HashSet<i64>) {
        let mut out = (HashSet::new(), HashSet::new());
        let mut params = json!({"expression": marks_js()});
        if let Some(f) = frame_id {
            // A private script world in that frame: the page's own scripts
            // can't see or tamper with what runs there.
            let Ok(w) = self.cdp_in(session, "Page.createIsolatedWorld", json!({"frameId": f, "worldName": "unibrow"})).await
            else {
                return out;
            };
            let Some(ctx) = w["executionContextId"].as_i64() else { return out };
            params["contextId"] = json!(ctx);
        }
        let Ok(r) = self.cdp_in(session, "Runtime.evaluate", params).await else { return out };
        let Some(array) = r["result"]["objectId"].as_str() else { return out };
        let Ok(props) = self.cdp_in(session, "Runtime.getProperties", json!({"objectId": array, "ownProperties": true})).await
        else {
            return out;
        };
        let mut entries: Vec<(u32, &Value)> = props["result"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| Some((p["name"].as_str()?.parse::<u32>().ok()?, &p["value"])))
            .collect();
        entries.sort_by_key(|(i, _)| *i);
        let mut past_separator = false;
        for (_, v) in entries {
            if v["type"] == "string" {
                past_separator = true;
                continue;
            }
            let Some(obj) = v["objectId"].as_str() else { continue };
            if let Ok(d) = self.cdp_in(session, "DOM.describeNode", json!({"objectId": obj})).await {
                if let Some(b) = d["node"]["backendNodeId"].as_i64() {
                    if past_separator { out.1.insert(b) } else { out.0.insert(b) };
                }
            }
        }
        let _ = self.cdp_in(session, "Runtime.releaseObject", json!({"objectId": array})).await;
        out
    }


    /// The `<iframe>` elements in a tree worth descending into: (backend id
    /// of the element, the frame id it shows).
    async fn iframes_in(&self, session: Option<&str>, nodes: &[Value]) -> Vec<(i64, String)> {
        let mut out = Vec::new();
        for n in nodes {
            let role = n["role"]["value"].as_str().unwrap_or("");
            if !matches!(role, "Iframe" | "IframePresentational") || n["ignored"].as_bool().unwrap_or(false) {
                continue;
            }
            let Some(b) = n["backendDOMNodeId"].as_i64() else { continue };
            let Ok(d) = self.cdp_in(session, "DOM.describeNode", json!({"backendNodeId": b})).await else { continue };
            let Some(frame_id) = d["node"]["frameId"].as_str().map(str::to_string) else { continue };
            // Skip invisible and tiny frames (trackers, spacers).
            let big = match self.cdp_in(session, "DOM.getBoxModel", json!({"backendNodeId": b})).await {
                Ok(bm) => bm["model"]["width"].as_f64().unwrap_or(0.0) * bm["model"]["height"].as_f64().unwrap_or(0.0) >= MIN_FRAME_AREA,
                Err(_) => false,
            };
            if big {
                out.push((b, frame_id));
            }
        }
        out
    }

    /// The page outline model, with frames stitched in and sensitive values
    /// hidden in each. Also refreshes the routes to out-of-process frames.
    pub(super) async fn page_model(&self) -> Result<PageModel, String> {
        let _ = self.bridge.cdp("Accessibility.enable", json!({})).await;
        let tree = self.bridge.cdp("Accessibility.getFullAXTree", json!({})).await?;
        let nodes = tree["nodes"].as_array().cloned().unwrap_or_default();
        let (hidden, draggable) = self.marks_in(None, None).await;
        let has_frames = nodes.iter().any(|n| matches!(n["role"]["value"].as_str(), Some("Iframe" | "IframePresentational")));
        let mut inputs = vec![FrameInput { nodes, hidden, draggable, ..Default::default() }];
        if !has_frames {
            if let Ok(mut reg) = self.frames.lock() {
                reg.routes.clear();
            }
            return Ok(from_frames(&inputs));
        }

        self.attach_frames().await;
        let start = Instant::now();
        let mut routes: HashMap<u32, FrameRoute> = HashMap::new();
        // Breadth-first over documents: (doc, its frame index for refs).
        let mut queue = vec![Doc { slot: 0, session: None, oopif: None }];
        let mut seen_frames: HashSet<String> = HashSet::new();
        while let Some(doc) = queue.pop() {
            if start.elapsed() > FRAME_BUDGET {
                break;
            }
            let frames = self.iframes_in(doc.session.as_deref(), &inputs[doc.slot].nodes.clone()).await;
            for (owner, frame_id) in frames {
                if inputs.len() > MAX_FRAMES || start.elapsed() > FRAME_BUDGET || !seen_frames.insert(frame_id.clone()) {
                    continue;
                }
                // Same process first: the frame's tree on this session.
                let same = self
                    .cdp_in(doc.session.as_deref(), "Accessibility.getFullAXTree", json!({"frameId": frame_id}))
                    .await
                    .ok()
                    .and_then(|t| t["nodes"].as_array().cloned())
                    .filter(|n| !n.is_empty());
                let slot = inputs.len();
                if let Some(nodes) = same {
                    let (hidden, draggable) = self.marks_in(doc.session.as_deref(), Some(&frame_id)).await;
                    let prefix = doc.oopif.map(|f| format!("f{f}")).unwrap_or_default();
                    inputs.push(FrameInput { nodes, hidden, draggable, ref_prefix: prefix, ..Default::default() });
                    inputs[doc.slot].children.insert(owner, slot);
                    queue.push(Doc { slot, session: doc.session.clone(), oopif: doc.oopif });
                    continue;
                }
                // Out of process: its own session (target id = frame id).
                let found = self.frames.lock().ok().and_then(|reg| reg.sessions.get(&frame_id).cloned());
                let Some((session, url)) = found else { continue };
                let _ = self.cdp_in(Some(&session), "Accessibility.enable", json!({})).await;
                let Ok(t) = self.cdp_in(Some(&session), "Accessibility.getFullAXTree", json!({})).await else { continue };
                let nodes = t["nodes"].as_array().cloned().unwrap_or_default();
                let index = self.frames.lock().map(|mut r| r.index_for(&frame_id)).unwrap_or(0);
                routes.insert(index, FrameRoute { session: session.clone(), owner: (doc.oopif, owner) });
                let (hidden, draggable) = self.marks_in(Some(&session), None).await;
                let host = host_of(&url);
                inputs.push(FrameInput {
                    nodes,
                    hidden,
                    draggable,
                    ref_prefix: format!("f{index}"),
                    label: if host.is_empty() { String::new() } else { format!("from {host}") },
                    ..Default::default()
                });
                inputs[doc.slot].children.insert(owner, slot);
                queue.push(Doc { slot, session: Some(session), oopif: Some(index) });
            }
        }
        if let Ok(mut reg) = self.frames.lock() {
            reg.routes = routes;
        }
        Ok(from_frames(&inputs))
    }
}
