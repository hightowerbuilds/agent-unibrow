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
use std::collections::HashMap;

/// Roles the agent can act on; these get refs.
const INTERACTIVE: &[&str] = &[
    "button", "link", "textbox", "searchbox", "combobox", "checkbox", "radio", "switch", "slider", "spinbutton",
    "menuitem", "menuitemcheckbox", "menuitemradio", "tab", "option", "listbox", "treeitem", "textarea",
    "ListBoxOption", "MenuListOption", "PopUpButton", "ToggleButton", "disclosuretriangle", "searchbox",
];

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
pub fn parse_ref(r: &str) -> Option<i64> {
    r.trim().trim_start_matches('[').trim_end_matches(']').strip_prefix('e')?.parse().ok()
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
        for p in n["properties"].as_array().into_iter().flatten() {
            let name = p["name"].as_str().unwrap_or("");
            let val = &p["value"]["value"];
            match name {
                "level" => level = val.as_i64(),
                "url" => url = val.as_str().map(str::to_string),
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
            },
        );
    }
    (map, root)
}

struct Walker<'a> {
    nodes: &'a HashMap<String, Node>,
    lines: Vec<String>,
    interactive: usize,
}

impl Walker<'_> {
    fn describe(&self, n: &Node, with_ref: bool) -> String {
        let mut s = String::new();
        if with_ref {
            if let Some(b) = n.backend_id {
                s.push_str(&format!("[{}] ", ref_for(b)));
            }
        }
        s.push_str(&n.role);
        if let Some(l) = n.level.filter(|_| n.role == "heading") {
            s.push_str(&format!("({l})"));
        }
        if !n.name.is_empty() {
            s.push(' ');
            s.push_str(&quote(&n.name));
        }
        if !n.value.is_empty() && n.value != n.name {
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
        let Some(n) = self.nodes.get(id) else { return };
        let role = n.role.as_str();

        if n.ignored || TRANSPARENT.contains(&role) || role.is_empty() {
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

        let interactive = INTERACTIVE.contains(&role);
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
        let Some(n) = self.nodes.get(id) else { return };
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
    let (map, root) = parse_nodes(nodes);
    let mut w = Walker { nodes: &map, lines: Vec::new(), interactive: 0 };
    if let Some(root) = root {
        w.walk(&root, 0, "");
    }
    PageModel { lines: w.lines, interactive: w.interactive }
}

/// Role and name of one node from `Accessibility.getPartialAXTree`, used to
/// describe an element before acting on it.
pub fn describe_single(nodes: &[Value]) -> Option<(String, String)> {
    nodes.iter().find(|n| !n["ignored"].as_bool().unwrap_or(false)).map(|n| {
        (n["role"]["value"].as_str().unwrap_or("").to_string(), clean(&text_of(&n["name"]["value"])))
    })
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
}
