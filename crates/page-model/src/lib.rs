//! Turns Chromium's accessibility tree into the page view agents read.
//!
//! Input: the `nodes` array from CDP `Accessibility.getFullAXTree`.
//! Output: an indented outline of what's on the page, where everything the
//! agent can act on carries a ref like `[e1234]`. The number is the node's
//! `backendDOMNodeId`, so a ref stays valid for as long as the element exists
//! and needs no lookup table.
//!
//! ```text
//! heading(1) "Checkout"
//! [e88] textbox "Email" value="ada@example.com" required
//! [e91] combobox "Country" value="Canada" expanded=false
//! [e97] button "Place order"
//! text "Total: $42.00"
//! ```

use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Roles the agent can act on; these get refs.
const INTERACTIVE: &[&str] = &[
    "button", "link", "textbox", "searchbox", "combobox", "checkbox", "radio", "switch", "slider", "spinbutton",
    "menuitem", "menuitemcheckbox", "menuitemradio", "tab", "option", "listbox", "treeitem", "textarea",
    "ListBoxOption", "MenuListOption", "PopUpButton", "ToggleButton", "disclosuretriangle", "searchbox",
];

/// Date, time and color inputs. Chromium exposes their parts (month, day,
/// year spinners) as children; the agent fills the whole input instead
/// (browser_type / browser_fill_form set its value), so they get one ref and
/// their parts are left out.
const PICKERS: &[(&str, &str)] = &[("Date", "date"), ("DateTime", "datetime"), ("InputTime", "time"), ("ColorWell", "color")];

/// Structural roles worth showing (with their name) even though they aren't
/// clickable: they tell the agent where it is on the page.
const STRUCTURE: &[&str] = &[
    "heading", "dialog", "alertdialog", "alert", "navigation", "main", "banner", "contentinfo", "complementary",
    "search", "form", "region", "table", "row", "columnheader", "rowheader", "cell", "gridcell", "list",
    "listitem", "img", "image", "figure", "status", "progressbar", "tablist", "menu", "menubar", "toolbar",
    "group", "radiogroup", "article", "tabpanel",
];

/// Roles that only add noise; their children are shown in their place.
const TRANSPARENT: &[&str] = &[
    "generic", "none", "presentation", "LineBreak", "InlineTextBox", "paragraph", "Section", "div",
    "LayoutTableCell", "LayoutTableRow", "LayoutTable", "Pre", "Abbr", "time", "emphasis", "strong",
    "superscript", "subscript", "mark", "code", "label", "legend", "caption", "RootWebArea", "WebArea",
    "Iframe", "IframePresentational", "Canvas", "DescriptionList", "DescriptionListTerm", "DescriptionListDetail",
    "blockquote", "Ruby", "insertion", "deletion", "ListMarker",
];

const PROPS: &[&str] =
    &["checked", "selected", "expanded", "pressed", "disabled", "required", "invalid", "focused", "readonly"];

#[derive(Debug, Clone)]
struct Node {
    role: String,
    name: String,
    value: String,
    ignored: bool,
    backend_id: Option<i64>,
    children: Vec<String>,
    props: Vec<(String, String)>,
    level: Option<i64>,
    url: Option<String>,
    /// "plaintext" or "richtext" for editable content (contenteditable).
    editable: Option<String>,
}

/// The page as the agent sees it.
#[derive(Debug, Clone)]
pub struct PageModel {
    /// One element or text run per line, indented by nesting.
    pub lines: Vec<String>,
    /// Number of elements with refs.
    pub interactive: usize,
}

impl PageModel {
    /// Renders lines `from..` within `max_chars`, saying what was left out.
    pub fn render(&self, from_line: usize, max_chars: usize) -> String {
        let mut out = String::new();
        let mut shown = 0;
        for line in self.lines.iter().skip(from_line) {
            if out.len() + line.len() + 1 > max_chars && shown > 0 {
                break;
            }
            out.push_str(line);
            out.push('\n');
            shown += 1;
        }
        let end = from_line + shown;
        if end < self.lines.len() {
            out.push_str(&format!(
                "… {} more lines. Call browser_read with from_line={end} to see them.\n",
                self.lines.len() - end
            ));
        }
        out
    }
}

/// Parses a ref (`e1234` or `[e1234]`) into a backend DOM node id.
/// Refs inside out-of-process frames (`f2e1234`) aren't plain refs; see
/// [`parse_frame_ref`].
pub fn parse_ref(r: &str) -> Option<i64> {
    r.trim().trim_start_matches('[').trim_end_matches(']').strip_prefix('e')?.parse().ok()
}

/// Parses any ref: `e1234` gives `(None, 1234)`, `f2e1234` (an element in
/// out-of-process frame 2) gives `(Some(2), 1234)`.
pub fn parse_frame_ref(r: &str) -> Option<(Option<u32>, i64)> {
    let r = r.trim().trim_start_matches('[').trim_end_matches(']');
    if let Some(rest) = r.strip_prefix('f') {
        let (frame, id) = rest.split_once('e')?;
        return Some((Some(frame.parse().ok()?), id.parse().ok()?));
    }
    Some((None, r.strip_prefix('e')?.parse().ok()?))
}

pub fn ref_for(backend_id: i64) -> String {
    format!("e{backend_id}")
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

fn clean(s: &str) -> String {
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > 160 {
        collapsed.chars().take(157).collect::<String>() + "…"
    } else {
        collapsed
    }
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "'"))
}

fn parse_nodes(nodes: &[Value]) -> (HashMap<String, Node>, Option<String>) {
    let mut map = HashMap::new();
    let mut root = None;
    for n in nodes {
        let Some(id) = n["nodeId"].as_str().map(str::to_string) else { continue };
        if root.is_none() && n.get("parentId").is_none_or(Value::is_null) {
            root = Some(id.clone());
        }
        let mut props = Vec::new();
        let mut level = None;
        let mut url = None;
        let mut editable = None;
        for p in n["properties"].as_array().into_iter().flatten() {
            let name = p["name"].as_str().unwrap_or("");
            let val = &p["value"]["value"];
            match name {
                "level" => level = val.as_i64(),
                "url" => url = val.as_str().map(str::to_string),
                "editable" => editable = val.as_str().filter(|v| !v.is_empty()).map(str::to_string),
                _ if PROPS.contains(&name) => {
                    let v = text_of(val);
                    if !v.is_empty() && v != "false" {
                        props.push((name.to_string(), v));
                    } else if name == "expanded" || name == "checked" || name == "selected" || name == "pressed" {
                        props.push((name.to_string(), "false".into()));
                    }
                }
                _ => {}
            }
        }
        map.insert(
            id,
            Node {
                role: n["role"]["value"].as_str().unwrap_or("").to_string(),
                name: clean(&text_of(&n["name"]["value"])),
                value: clean(&text_of(&n["value"]["value"])),
                ignored: n["ignored"].as_bool().unwrap_or(false),
                backend_id: n["backendDOMNodeId"].as_i64(),
                children: n["childIds"]
                    .as_array()
                    .map(|c| c.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default(),
                props,
                level,
                url,
                editable,
            },
        );
    }
    (map, root)
}

/// One document's accessibility tree: the page itself, or a frame in it.
#[derive(Debug, Clone, Default)]
pub struct FrameInput {
    /// `nodes` from `Accessibility.getFullAXTree` for this document.
    pub nodes: Vec<Value>,
    /// Backend ids of sensitive fields (passwords, card numbers, codes).
    pub hidden: HashSet<i64>,
    /// Backend ids of `draggable=true` elements (cards, list items), which
    /// get refs even though they're plain boxes.
    pub draggable: HashSet<i64>,
    /// Put before refs in this document: empty for the page's own process,
    /// `f2` for an out-of-process frame (refs like `f2e45`).
    pub ref_prefix: String,
    /// `<iframe>` elements in this document (by backend id) and the index
    /// of the frame they show, in the slice given to [`from_frames`].
    pub children: HashMap<i64, usize>,
    /// Shown on the frame's line, e.g. `from pay.example.com`.
    pub label: String,
}

struct Parsed {
    map: HashMap<String, Node>,
    root: Option<String>,
}

struct Walker<'a> {
    frames: &'a [Parsed],
    inputs: &'a [FrameInput],
    cur: usize,
    entered: HashSet<usize>,
    lines: Vec<String>,
    interactive: usize,
}

/// Roles that are only a wrapper when they're an editable root
/// (`<div contenteditable>`); shown as a textbox.
fn editable_root(n: &Node) -> bool {
    n.editable.is_some()
        && !INTERACTIVE.contains(&n.role.as_str())
        && matches!(n.role.as_str(), "generic" | "none" | "paragraph" | "Section" | "div" | "article" | "region" | "main" | "group" | "document" | "")
}

impl Walker<'_> {
    fn hidden(&self) -> &HashSet<i64> {
        &self.inputs[self.cur].hidden
    }

    fn is_draggable(&self, n: &Node) -> bool {
        n.backend_id.is_some_and(|b| self.inputs[self.cur].draggable.contains(&b))
    }

    /// The text inside a node, for naming a box that has no name.
    fn inner_text(&self, n: &Node, out: &mut String) {
        for c in &n.children {
            let Some(cn) = self.frames[self.cur].map.get(c) else { continue };
            if cn.role == "StaticText" || cn.role == "text" {
                if !cn.name.is_empty() {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(&cn.name);
                }
            } else if !cn.ignored || !cn.children.is_empty() {
                self.inner_text(cn, out);
            }
            if out.len() > 200 {
                return;
            }
        }
    }

    fn ref_of(&self, backend: i64) -> String {
        format!("{}{}", self.inputs[self.cur].ref_prefix, ref_for(backend))
    }

    /// The frame an `<iframe>` element shows, if its tree was fetched.
    fn child_frame(&self, n: &Node) -> Option<usize> {
        let b = n.backend_id?;
        let i = *self.inputs[self.cur].children.get(&b)?;
        (i < self.frames.len() && !self.entered.contains(&i)).then_some(i)
    }

    fn walk_frame(&mut self, n: &Node, frame: usize, depth: usize) {
        let mut line = format!("{}frame", "  ".repeat(depth));
        if !n.name.is_empty() {
            line.push(' ');
            line.push_str(&quote(&n.name));
        }
        let label = &self.inputs[frame].label;
        if !label.is_empty() {
            line.push(' ');
            line.push_str(label);
        }
        self.lines.push(line);
        let prev = self.cur;
        self.entered.insert(frame);
        self.cur = frame;
        if let Some(root) = self.frames[frame].root.clone() {
            self.walk(&root, depth + 1, "");
        }
        self.cur = prev;
    }

    fn describe(&self, n: &Node, with_ref: bool) -> String {
        let mut s = String::new();
        if with_ref {
            if let Some(b) = n.backend_id {
                s.push_str(&format!("[{}] ", self.ref_of(b)));
            }
        }
        let rich = editable_root(n);
        let picker = PICKERS.iter().find(|(r, _)| *r == n.role).map(|(_, shown)| *shown);
        let drag = self.is_draggable(n);
        let plain_box = TRANSPARENT.contains(&n.role.as_str()) || n.role.is_empty();
        s.push_str(if rich {
            "textbox"
        } else if drag && plain_box {
            "draggable"
        } else {
            picker.unwrap_or(&n.role)
        });
        if let Some(l) = n.level.filter(|_| n.role == "heading") {
            s.push_str(&format!("({l})"));
        }
        if !n.name.is_empty() {
            s.push(' ');
            s.push_str(&quote(&n.name));
        } else if drag {
            let mut t = String::new();
            self.inner_text(n, &mut t);
            if !t.is_empty() {
                s.push(' ');
                s.push_str(&quote(&clean(&t)));
            }
        }
        if drag && !plain_box {
            s.push_str(" draggable");
        }
        if rich || (n.editable.as_deref() == Some("richtext") && n.role == "textbox") {
            s.push_str(" richtext");
        }
        let sensitive = n.backend_id.is_some_and(|b| self.hidden().contains(&b));
        if sensitive {
            if !n.value.is_empty() {
                s.push_str(" value=[hidden]");
            }
            s.push_str(" sensitive");
        } else if !n.value.is_empty() && n.value != n.name {
            s.push_str(&format!(" value={}", quote(&n.value)));
        }
        for (k, v) in &n.props {
            match v.as_str() {
                "true" => s.push_str(&format!(" {k}")),
                "false" if k == "expanded" || k == "checked" || k == "pressed" => s.push_str(&format!(" {k}=false")),
                "false" => {}
                other => s.push_str(&format!(" {k}={other}")),
            }
        }
        if n.role == "link" {
            if let Some(u) = n.url.as_ref().filter(|u| !u.starts_with("javascript:")) {
                let short: String = u.chars().take(100).collect();
                s.push_str(&format!(" -> {short}"));
            }
        }
        s
    }

    /// Text inside an element that the element's name already says.
    fn walk(&mut self, id: &str, depth: usize, parent_name: &str) {
        let frames = self.frames;
        let Some(n) = frames[self.cur].map.get(id) else { return };
        let role = n.role.as_str();

        if !n.ignored && matches!(role, "Iframe" | "IframePresentational") {
            if let Some(f) = self.child_frame(n) {
                self.walk_frame(n, f, depth);
                return;
            }
        }

        let rich = !n.ignored && editable_root(n);
        let drag = !n.ignored && self.is_draggable(n);
        if !rich && !drag && (n.ignored || TRANSPARENT.contains(&role) || role.is_empty()) {
            for c in &n.children {
                self.walk(c, depth, parent_name);
            }
            return;
        }

        if role == "StaticText" || role == "text" {
            if !n.name.is_empty() && !parent_name.contains(&n.name) {
                // Merge adjacent text runs at the same depth.
                let indent = "  ".repeat(depth);
                let prefix = format!("{indent}text ");
                if let Some(last) = self.lines.last_mut().filter(|l| l.starts_with(&prefix) && l.len() < 300) {
                    last.pop(); // closing quote
                    last.push(' ');
                    last.push_str(&n.name.replace('"', "'"));
                    last.push('"');
                } else {
                    self.lines.push(format!("{indent}text {}", quote(&n.name)));
                }
            }
            return;
        }

        let picker = PICKERS.iter().any(|(r, _)| *r == role);
        let interactive = INTERACTIVE.contains(&role) || rich || picker || drag;
        let structural = STRUCTURE.contains(&role);
        if interactive || (structural && (!n.name.is_empty() || n.level.is_some() || !n.children.is_empty())) {
            // Skip unnamed wrappers that would only add a line.
            let unnamed_wrapper = structural
                && !interactive
                && n.name.is_empty()
                && matches!(role, "group" | "list" | "listitem" | "row" | "cell" | "gridcell" | "region" | "figure");
            let mut next_depth = depth;
            if !unnamed_wrapper {
                self.lines.push(format!("{}{}", "  ".repeat(depth), self.describe(n, interactive)));
                if interactive {
                    self.interactive += 1;
                }
                next_depth = depth + 1;
            }
            let name = if n.name.is_empty() { parent_name.to_string() } else { n.name.clone() };
            if n.backend_id.is_some_and(|b| self.hidden().contains(&b)) {
                return; // never show what's inside a sensitive field
            }
            if picker {
                return;
            }
            // Links and buttons say everything in their name.
            if interactive && !matches!(role, "listbox" | "combobox" | "menu" | "tablist" | "radiogroup") {
                for c in &n.children {
                    self.walk_inside_control(c, next_depth, &name);
                }
            } else {
                for c in &n.children {
                    self.walk(c, next_depth, &name);
                }
            }
            return;
        }

        // Any other named role: show it plainly.
        if !n.name.is_empty() && !parent_name.contains(&n.name) {
            self.lines.push(format!("{}{} {}", "  ".repeat(depth), role, quote(&n.name)));
        }
        for c in &n.children {
            self.walk(c, depth, parent_name);
        }
    }

    /// Inside a link or button only nested controls are interesting.
    fn walk_inside_control(&mut self, id: &str, depth: usize, parent_name: &str) {
        let frames = self.frames;
        let Some(n) = frames[self.cur].map.get(id) else { return };
        if !n.ignored && INTERACTIVE.contains(&n.role.as_str()) {
            self.walk(id, depth, parent_name);
            return;
        }
        for c in &n.children {
            self.walk_inside_control(c, depth, parent_name);
        }
    }
}

/// Builds the page model from `Accessibility.getFullAXTree`'s `nodes`.
pub fn from_ax_tree(nodes: &[Value]) -> PageModel {
    from_ax_tree_hiding(nodes, &HashSet::new())
}

/// Like [`from_ax_tree`], with the values of `hidden` fields (by backend
/// DOM node id) replaced by `[hidden]`. Sensitive text a field holds also
/// shows up as its child text, so that's suppressed too.
pub fn from_ax_tree_hiding(nodes: &[Value], hidden: &HashSet<i64>) -> PageModel {
    from_frames(&[FrameInput { nodes: nodes.to_vec(), hidden: hidden.clone(), ..Default::default() }])
}

/// Builds the page model from several documents: `frames[0]` is the page,
/// the others are frames shown where their `<iframe>` is (see
/// [`FrameInput::children`]). Frames nobody points to are left out.
pub fn from_frames(frames: &[FrameInput]) -> PageModel {
    let parsed: Vec<Parsed> = frames
        .iter()
        .map(|f| {
            let (map, root) = parse_nodes(&f.nodes);
            Parsed { map, root }
        })
        .collect();
    let mut w = Walker { frames: &parsed, inputs: frames, cur: 0, entered: HashSet::new(), lines: Vec::new(), interactive: 0 };
    if let Some(root) = parsed.first().and_then(|p| p.root.clone()) {
        w.entered.insert(0);
        w.walk(&root, 0, "");
    }
    PageModel { lines: w.lines, interactive: w.interactive }
}

/// Role and name of one node from `Accessibility.getPartialAXTree`, used to
/// describe an element before acting on it.
pub fn describe_single(nodes: &[Value]) -> Option<(String, String)> {
    let (map, _) = parse_nodes(nodes);
    let n = nodes.iter().find(|n| !n["ignored"].as_bool().unwrap_or(false))?;
    let parsed = map.get(n["nodeId"].as_str()?)?;
    // The same role names the outline shows.
    let role = if editable_root(parsed) {
        "textbox".to_string()
    } else {
        PICKERS.iter().find(|(r, _)| *r == parsed.role).map(|(_, s)| s.to_string()).unwrap_or_else(|| parsed.role.clone())
    };
    Some((role, parsed.name.clone()))
}

// ---------------------------------------------------------------- diffs

/// What changed between two outlines of the same page (roadmap 1.2), so
/// after an action the agent sees the difference instead of re-reading.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Changes {
    /// Lines only in the new outline, in page order.
    pub added: Vec<String>,
    /// Lines only in the old outline.
    pub removed: Vec<String>,
    /// Elements (same ref) whose line changed: (before, after).
    pub changed: Vec<(String, String)>,
    /// Lines in both outlines together, to judge how much changed.
    pub total: usize,
}

/// The ref a line starts with (`[e12]`, `[f2e12]`), if any.
fn line_ref(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let rest = t.strip_prefix('[')?;
    let end = rest.find(']')?;
    let r = &rest[..end];
    parse_frame_ref(r).map(|_| r)
}

/// A line without the `focused` flag: focus moving isn't a change worth
/// reporting.
fn without_focus(line: &str) -> String {
    line.trim().replace(" focused", "")
}

/// A bare role with nothing else (`status`, `form`): no news on its own.
fn bare(line: &str) -> bool {
    let t = line.trim();
    !t.contains(' ') && !t.contains('"')
}

/// Compares outlines line by line. Elements are matched by ref, so a field
/// whose value changed shows as changed, not as one line removed and one
/// added; other lines are matched by their text, ignoring indentation.
pub fn diff(before: &[String], after: &[String]) -> Changes {
    let mut before_refs: HashMap<&str, &str> = HashMap::new();
    let mut before_text: HashMap<&str, usize> = HashMap::new();
    for l in before {
        match line_ref(l) {
            Some(r) => {
                before_refs.insert(r, l.trim());
            }
            None => *before_text.entry(l.trim()).or_default() += 1,
        }
    }
    let mut after_refs: HashSet<&str> = HashSet::new();
    let mut after_text: HashMap<&str, usize> = HashMap::new();
    let mut c = Changes { total: before.len() + after.len(), ..Default::default() };
    for l in after.iter().filter(|l| !bare(l)) {
        let t = l.trim();
        match line_ref(l) {
            Some(r) => {
                after_refs.insert(r);
                match before_refs.get(r) {
                    Some(old) if without_focus(old) == without_focus(t) => {}
                    Some(old) => c.changed.push((old.to_string(), t.to_string())),
                    None => c.added.push(t.to_string()),
                }
            }
            None => {
                *after_text.entry(t).or_default() += 1;
                match before_text.get_mut(t) {
                    Some(n) if *n > 0 => *n -= 1,
                    _ => c.added.push(t.to_string()),
                }
            }
        }
    }
    let mut seen_text: HashMap<&str, usize> = HashMap::new();
    for l in before.iter().filter(|l| !bare(l)) {
        let t = l.trim();
        match line_ref(l) {
            Some(r) if !after_refs.contains(r) => c.removed.push(t.to_string()),
            Some(_) => {}
            None => {
                let n = seen_text.entry(t).or_default();
                *n += 1;
                if *n > after_text.get(t).copied().unwrap_or(0) {
                    c.removed.push(t.to_string());
                }
            }
        }
    }
    c
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() > max { s.chars().take(max - 1).collect::<String>() + "…" } else { s.to_string() }
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    pub fn count(&self) -> usize {
        self.added.len() + self.removed.len() + self.changed.len()
    }

    /// Most of the page is different (a new page, or a big re-render).
    pub fn mostly_new(&self) -> bool {
        // Over 60% of all lines are on one side only.
        self.total >= 20 && (self.added.len() + self.removed.len()) * 10 > self.total * 6
    }

    /// The changes as `+` (added), `-` (removed) and `~` (changed) lines,
    /// at most `max_lines` lines and about `max_chars` characters.
    pub fn render(&self, max_lines: usize, max_chars: usize) -> String {
        let mut out = format!(
            "{} added, {} removed, {} changed.\n",
            self.added.len(),
            self.removed.len(),
            self.changed.len()
        );
        let mut items: Vec<String> = Vec::new();
        for (old, new) in &self.changed {
            // The ref is on the new line already; say what it was.
            let was = line_ref(old).map(|r| old.trim_start().trim_start_matches(&format!("[{r}]")).trim()).unwrap_or(old);
            items.push(format!("~ {}  (was: {})", clip(new, 200), clip(was, 100)));
        }
        items.extend(self.added.iter().map(|l| format!("+ {}", clip(l, 200))));
        items.extend(self.removed.iter().map(|l| format!("- {}", clip(l, 200))));
        let total = items.len();
        let mut shown = 0;
        for it in items {
            if shown >= max_lines || (out.len() + it.len() + 1 > max_chars && shown > 0) {
                break;
            }
            out.push_str(&it);
            out.push('\n');
            shown += 1;
        }
        if shown < total {
            out.push_str(&format!("… and {} more changes (the outline below has everything).\n", total - shown));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: &str, parent: Option<&str>, role: &str, name: &str, backend: i64, children: &[&str]) -> Value {
        let mut n = json!({
            "nodeId": id,
            "ignored": false,
            "role": {"type": "role", "value": role},
            "name": {"type": "computedString", "value": name},
            "backendDOMNodeId": backend,
            "childIds": children,
        });
        if let Some(p) = parent {
            n["parentId"] = json!(p);
        }
        n
    }

    fn sample() -> Vec<Value> {
        let mut nodes = vec![
            node("1", None, "RootWebArea", "Checkout", 1, &["2", "3", "6", "8", "10"]),
            node("2", Some("1"), "heading", "Checkout", 2, &["2t"]),
            node("2t", Some("2"), "StaticText", "Checkout", 20, &[]),
            node("3", Some("1"), "generic", "", 3, &["4", "5"]),
            node("4", Some("3"), "StaticText", "Total:", 40, &[]),
            node("5", Some("3"), "StaticText", "$42.00", 50, &[]),
            node("6", Some("1"), "textbox", "Email", 88, &["7"]),
            node("7", Some("6"), "StaticText", "ada@example.com", 70, &[]),
            node("8", Some("1"), "button", "Place order", 97, &["9"]),
            node("9", Some("8"), "StaticText", "Place order", 90, &[]),
            node("10", Some("1"), "link", "Help", 101, &[]),
        ];
        nodes[1]["properties"] = json!([{"name": "level", "value": {"type": "integer", "value": 1}}]);
        nodes[6]["value"] = json!({"type": "string", "value": "ada@example.com"});
        nodes[6]["properties"] = json!([{"name": "required", "value": {"type": "boolean", "value": true}}]);
        nodes[10]["properties"] = json!([{"name": "url", "value": {"type": "string", "value": "https://shop.test/help"}}]);
        nodes
    }

    #[test]
    fn outline_with_refs() {
        let m = from_ax_tree(&sample());
        let text = m.lines.join("\n");
        assert_eq!(
            text,
            "heading(1) \"Checkout\"\n\
             text \"Total: $42.00\"\n\
             [e88] textbox \"Email\" value=\"ada@example.com\" required\n\
             [e97] button \"Place order\"\n\
             [e101] link \"Help\" -> https://shop.test/help"
        );
        assert_eq!(m.interactive, 3);
    }

    #[test]
    fn sensitive_values_are_hidden() {
        let mut nodes = sample();
        nodes[6]["value"] = json!({"type": "string", "value": "4111 1111 1111 1111"});
        nodes[6]["name"] = json!({"type": "computedString", "value": "Card number"});
        nodes[7]["name"] = json!({"type": "computedString", "value": "4111 1111 1111 1111"});
        let hidden: HashSet<i64> = [88].into_iter().collect();
        let text = from_ax_tree_hiding(&nodes, &hidden).lines.join("\n");
        assert!(text.contains("[e88] textbox \"Card number\" value=[hidden] sensitive"), "{text}");
        assert!(!text.contains("4111"), "{text}");
    }

    #[test]
    fn refs_round_trip() {
        assert_eq!(parse_ref("e97"), Some(97));
        assert_eq!(parse_ref("[e97]"), Some(97));
        assert_eq!(parse_ref("97"), None);
        assert_eq!(ref_for(97), "e97");
    }

    #[test]
    fn render_pages_long_output() {
        let m = PageModel { lines: (0..100).map(|i| format!("text \"line {i}\"")).collect(), interactive: 0 };
        let first = m.render(0, 200);
        assert!(first.contains("line 0"));
        assert!(first.contains("more lines. Call browser_read with from_line="));
        let rest = m.render(95, 10_000);
        assert!(rest.contains("line 99") && !rest.contains("more lines"));
    }

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn diff_finds_added_removed_and_changed() {
        let before = lines(&[
            "heading(1) \"Cart\"",
            "text \"Your cart is empty\"",
            "[e10] textbox \"Coupon\"",
            "[e11] button \"Apply\"",
            "[e12] link \"Help\" -> https://shop.test/help",
        ]);
        let after = lines(&[
            "heading(1) \"Cart\"",
            "  text \"1 item: Bone nut\"",
            "[e10] textbox \"Coupon\" value=\"SPRING\"",
            "[e11] button \"Apply\"",
            "[e30] button \"Remove\"",
            "[e12] link \"Help\" -> https://shop.test/help",
        ]);
        let c = diff(&before, &after);
        assert_eq!(c.added, vec!["text \"1 item: Bone nut\"", "[e30] button \"Remove\""]);
        assert_eq!(c.removed, vec!["text \"Your cart is empty\""]);
        assert_eq!(c.changed, vec![("[e10] textbox \"Coupon\"".into(), "[e10] textbox \"Coupon\" value=\"SPRING\"".into())]);
        assert!(!c.mostly_new());
        let r = c.render(30, 2000);
        assert!(r.starts_with("2 added, 1 removed, 1 changed."), "{r}");
        assert!(r.contains("~ [e10] textbox \"Coupon\" value=\"SPRING\"  (was: textbox \"Coupon\")"), "{r}");
        assert!(r.contains("+ [e30] button \"Remove\""), "{r}");
        assert!(r.contains("- text \"Your cart is empty\""), "{r}");
    }

    #[test]
    fn diff_ignores_indentation_and_counts_repeats() {
        let before = lines(&["text \"Item\"", "text \"Item\"", "  [e1] button \"Go\"", "status"]);
        let after = lines(&["  text \"Item\"", "text \"Item\"", "text \"Item\"", "[e1] button \"Go\" focused"]);
        let c = diff(&before, &after);
        assert_eq!(c.added, vec!["text \"Item\""]);
        assert!(c.removed.is_empty() && c.changed.is_empty());
        assert!(diff(&before, &before).is_empty());
    }

    #[test]
    fn diff_render_is_bounded() {
        let before: Vec<String> = Vec::new();
        let after: Vec<String> = (0..500).map(|i| format!("[e{i}] link \"Result {i}\"")).collect();
        let c = diff(&before, &after);
        assert_eq!(c.added.len(), 500);
        assert!(c.mostly_new());
        let r = c.render(25, 1500);
        assert!(r.len() < 1700, "{}", r.len());
        assert!(r.lines().count() <= 27);
        assert!(r.contains("more changes"));
    }

    #[test]
    fn frame_refs_parse() {
        assert_eq!(parse_frame_ref("e97"), Some((None, 97)));
        assert_eq!(parse_frame_ref("[f2e97]"), Some((Some(2), 97)));
        assert_eq!(parse_frame_ref("f2x97"), None);
        assert_eq!(parse_ref("f2e97"), None);
    }

    #[test]
    fn frames_are_stitched_in_with_their_refs() {
        let top = vec![
            node("1", None, "RootWebArea", "Shop", 1, &["2", "3"]),
            node("2", Some("1"), "heading", "Order extras", 2, &[]),
            node("3", Some("1"), "Iframe", "Gift message", 30, &[]),
        ];
        let child = vec![
            node("1", None, "RootWebArea", "Gift", 1, &["2"]),
            node("2", Some("1"), "textbox", "Message for the card", 7, &[]),
        ];
        let frames = vec![
            FrameInput { nodes: top, children: [(30, 1)].into_iter().collect(), ..Default::default() },
            FrameInput { nodes: child, ref_prefix: "f1".into(), label: "from 127.0.0.1".into(), ..Default::default() },
        ];
        let m = from_frames(&frames);
        assert_eq!(
            m.lines.join("\n"),
            "heading \"Order extras\"\nframe \"Gift message\" from 127.0.0.1\n  [f1e7] textbox \"Message for the card\""
        );
        assert_eq!(m.interactive, 1);
    }

    #[test]
    fn draggable_boxes_get_refs_and_their_text() {
        let nodes = vec![
            node("1", None, "RootWebArea", "Board", 1, &["2", "4"]),
            node("2", Some("1"), "generic", "", 5, &["3"]),
            node("3", Some("2"), "StaticText", "Order nuts", 6, &[]),
            node("4", Some("1"), "listitem", "Clean frets", 8, &[]),
        ];
        let frames = vec![FrameInput { nodes, draggable: [5, 8].into_iter().collect(), ..Default::default() }];
        let m = from_frames(&frames);
        assert_eq!(m.lines, vec!["[e5] draggable \"Order nuts\"", "[e8] listitem \"Clean frets\" draggable"]);
        assert_eq!(m.interactive, 2);
    }

    #[test]
    fn date_inputs_are_one_field() {
        let mut nodes = vec![
            node("1", None, "RootWebArea", "Book", 1, &["2"]),
            node("2", Some("1"), "Date", "Lesson date", 5, &["3", "4"]),
            node("3", Some("2"), "spinbutton", "Month", 6, &[]),
            node("4", Some("2"), "button", "Show date picker", 7, &[]),
        ];
        nodes[1]["value"] = json!({"type": "string", "value": "2026-11-03"});
        let m = from_ax_tree(&nodes);
        assert_eq!(m.lines, vec!["[e5] date \"Lesson date\" value=\"2026-11-03\""]);
    }

    #[test]
    fn contenteditable_roots_get_refs() {
        let mut nodes = vec![
            node("1", None, "RootWebArea", "Compose", 1, &["2"]),
            node("2", Some("1"), "generic", "Message body", 5, &["3"]),
            node("3", Some("2"), "paragraph", "", 6, &["4"]),
            node("4", Some("3"), "StaticText", "Hello", 7, &[]),
        ];
        nodes[1]["properties"] = json!([{"name": "editable", "value": {"type": "token", "value": "richtext"}}]);
        nodes[2]["properties"] = json!([{"name": "editable", "value": {"type": "token", "value": "richtext"}}]);
        let m = from_ax_tree(&nodes);
        assert_eq!(m.lines[0], "[e5] textbox \"Message body\" richtext");
        assert_eq!(m.interactive, 1);
    }
}
