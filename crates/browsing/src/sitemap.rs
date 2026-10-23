//! Site maps and playbooks (roadmap Phase 8): knowing focus sites like the
//! back of its hand.
//!
//! - **Page patterns.** URLs are grouped by shape: `/dp/B0ABC12345` and
//!   `/dp/B0XYZ98765` are both `/dp/{id}`; query values are dropped, keys
//!   kept. Each pattern gets a kind (search, product, cart, checkout,
//!   sign-in…) from its URL and title.
//! - **Actions.** Every page-changing action the agent takes on a focus site
//!   is logged: which page it was on, what it clicked or typed (by role and
//!   name, not by ref), and where it ended up. Elements that matter and the
//!   paths between pages come from this log; page visits come from history.
//! - **Playbooks.** A run of recorded actions saved under a name, with the
//!   literal values turned into parameters (`{query}`). The browser rail
//!   replays them step by step and checks each page against the recording.

use crate::{Browsing, site_of};
use chrono::{DateTime, Local};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Query keys that only track where a visit came from.
const TRACKING_KEYS: &[&str] = &[
    "ref", "ref_", "tag", "gclid", "fbclid", "msclkid", "_encoding", "crid", "sprefix", "qid", "psc", "pd_rd_r",
    "pd_rd_w", "pd_rd_wg", "pf_rd_r", "pf_rd_p", "content-id", "dib", "dib_tag", "keywords", "sr", "th", "smid",
];
/// Actions kept in the log (oldest dropped first).
const MAX_ACTIONS: i64 = 2_000;
/// A gap this long ends a run of actions on a site.
const RUN_GAP_SECS: i64 = 30 * 60;

pub(crate) fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS site_actions (
             id INTEGER PRIMARY KEY,
             site TEXT NOT NULL,
             from_url TEXT NOT NULL,
             from_pattern TEXT NOT NULL,
             step TEXT NOT NULL,
             to_url TEXT NOT NULL,
             to_pattern TEXT NOT NULL,
             at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS site_actions_site ON site_actions(site, id);
         CREATE TABLE IF NOT EXISTS playbooks (
             id INTEGER PRIMARY KEY,
             site TEXT NOT NULL,
             name TEXT NOT NULL,
             description TEXT NOT NULL DEFAULT '',
             params TEXT NOT NULL,
             steps TEXT NOT NULL,
             created INTEGER NOT NULL,
             runs INTEGER NOT NULL DEFAULT 0,
             successes INTEGER NOT NULL DEFAULT 0,
             last_run INTEGER,
             last_error TEXT,
             UNIQUE(site, name)
         );",
    )
}

/// The shape of a URL's path and query, values dropped:
/// `https://www.amazon.com/Some-Nut/dp/B0ABC12345/ref=sr_1_1?k=nut&crid=X`
/// → `/{slug}/dp/{id}?k`.
pub fn url_pattern(url: &str) -> String {
    let Ok(u) = url::Url::parse(url) else { return url.to_string() };
    let mut out = String::new();
    for seg in u.path_segments().into_iter().flatten().filter(|s| !s.is_empty()) {
        // Amazon-style `ref=…` path segments are tracking, not pages.
        if seg.starts_with("ref=") {
            continue;
        }
        out.push('/');
        out.push_str(&segment_pattern(seg));
    }
    if out.is_empty() {
        out.push('/');
    }
    let mut keys: Vec<String> = u
        .query_pairs()
        .map(|(k, _)| k.to_ascii_lowercase())
        .filter(|k| !TRACKING_KEYS.contains(&k.as_str()) && !k.starts_with("utm_"))
        .collect();
    keys.sort();
    keys.dedup();
    if !keys.is_empty() {
        out.push('?');
        out.push_str(&keys.join("&"));
    }
    out
}

fn segment_pattern(seg: &str) -> String {
    let digits = seg.chars().filter(char::is_ascii_digit).count();
    let letters = seg.chars().filter(|c| c.is_alphabetic()).count();
    if digits > 0 && letters == 0 && seg.chars().all(|c| c.is_ascii_digit() || matches!(c, '-' | '_' | '.')) {
        return "{n}".into();
    }
    // Product and order ids: long, mixed letters and digits, no word breaks.
    if seg.len() >= 6 && digits >= 2 && letters > 0 && !seg.contains('-') {
        return "{id}".into();
    }
    if seg.len() >= 8 && digits >= 3 {
        return "{id}".into();
    }
    // Readable slugs ("Graph-Tech-TUSQ-XL-Jazz-Bass-Nut").
    if seg.matches('-').count() >= 3 || seg.len() > 40 {
        return "{slug}".into();
    }
    seg.to_ascii_lowercase()
}

/// What kind of page a pattern is, from its URL and title.
pub fn page_kind(pattern: &str, title: &str) -> &'static str {
    let p = pattern.to_ascii_lowercase();
    let t = title.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| p.contains(w));
    if has(&["checkout", "/payment", "/buy/"]) || t.contains("checkout") {
        "checkout"
    } else if has(&["/cart", "/basket", "/bag"]) || t.contains("shopping cart") || t.starts_with("cart") {
        "cart"
    } else if has(&["signin", "sign-in", "/login", "/ap/", "/auth"]) || t.contains("sign in") || t.contains("log in") {
        "sign-in"
    } else if has(&["/order", "/purchases", "your-orders"]) || t.contains("your orders") || t.contains("order history") {
        "orders"
    } else if has(&["/account", "/profile", "/settings"]) {
        "account"
    } else if has(&["?q", "&q", "?k", "&k", "search", "query", "/s?", "/s/"]) || t.contains("search") || t.contains("results") {
        "search"
    } else if has(&["/dp/", "/product", "/item", "/p/", "/gp/product", "/listing"]) {
        "product"
    } else if p == "/" {
        "home"
    } else {
        "page"
    }
}

/// One element the agent acted on, as it would find it again: by role and
/// accessible name (`button "Add to cart"`), the nth of that kind on the page.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Target {
    pub role: String,
    pub name: String,
    #[serde(default)]
    pub nth: usize,
    /// The element was picked because it matched the parameters (a search
    /// result for the query), so a replay picks whichever matches the new
    /// values instead of the recorded name.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub matches_params: bool,
}

fn words(s: &str) -> Vec<String> {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 1).map(str::to_string).collect()
}

/// Part of a step's text: as recorded, or a `{param}` already put in, so a
/// later literal can't match inside it.
enum Piece {
    Text(String),
    Param(String),
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whether `lit` at byte `start` of `s` is a whole token: not inside a word
/// (`AB12345`, `sr_1_2`) or a number (`2.5`, `1,200`).
fn whole_token(s: &str, start: usize, lit: &str) -> bool {
    // Whether the literal's edge character runs on into `near` (then `far`).
    let joins = |edge: Option<char>, near: Option<char>, far: Option<char>| match (edge, near) {
        (Some(e), Some(n)) if word_char(e) && word_char(n) => true,
        (Some(e), Some('.' | ',')) => e.is_ascii_digit() && far.is_some_and(|f| f.is_ascii_digit()),
        _ => false,
    };
    let mut before = s[..start].chars().rev();
    let mut after = s[start + lit.len()..].chars();
    !joins(lit.chars().next(), before.next(), before.next())
        && !joins(lit.chars().next_back(), after.next(), after.next())
}

/// Puts `{name}` in place of each whole-token `lit` in the text pieces.
fn sub_tokens(pieces: Vec<Piece>, lit: &str, name: &str) -> Vec<Piece> {
    let mut out = Vec::new();
    for p in pieces {
        let Piece::Text(text) = p else {
            out.push(p);
            continue;
        };
        let (mut kept, mut from) = (0, 0);
        while let Some(i) = text[from..].find(lit).map(|i| from + i) {
            if whole_token(&text, i, lit) {
                if i > kept {
                    out.push(Piece::Text(text[kept..i].to_string()));
                }
                out.push(Piece::Param(name.to_string()));
                kept = i + lit.len();
                from = kept;
            } else {
                from = i + text[i..].chars().next().map_or(1, char::len_utf8);
            }
        }
        if kept < text.len() {
            out.push(Piece::Text(text[kept..].to_string()));
        }
    }
    out
}

/// The pieces as a string, with the recorded text passed through `enc`.
fn render(pieces: &[Piece], enc: impl Fn(&str) -> String) -> String {
    pieces
        .iter()
        .map(|p| match p {
            Piece::Text(t) => enc(t),
            Piece::Param(n) => format!("{{{n}}}"),
        })
        .collect()
}

/// Typed, chosen or filled text: each literal where it is a whole word.
fn sub_text(s: &str, literals: &[&(String, String)]) -> String {
    let mut pieces = vec![Piece::Text(s.to_string())];
    for (lit, name) in literals {
        pieces = sub_tokens(pieces, lit, name);
    }
    render(&pieces, str::to_string)
}

/// An element's name: the whole name, or a whole word of it when the
/// literal is long enough not to turn up by chance ("Page 2", "Cart (2)").
fn sub_name(s: &str, literals: &[&(String, String)]) -> String {
    if let Some((_, name)) = literals.iter().find(|(lit, _)| s.trim() == lit) {
        return format!("{{{name}}}");
    }
    let mut pieces = vec![Piece::Text(s.to_string())];
    for (lit, name) in literals {
        if lit.chars().count() >= 3 && !lit.chars().all(|c| c.is_ascii_digit()) {
            pieces = sub_tokens(pieces, lit, name);
        }
    }
    render(&pieces, str::to_string)
}

/// A percent- or form-encoded address part as text.
fn decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("v={s}").as_bytes()).next().map(|(_, v)| v.into_owned()).unwrap_or_default()
}

/// An `open` address: path segments and query values that are a literal,
/// and literal words in a free-text query value (`q=TUSQ+nut+black`).
fn sub_url(addr: &str, literals: &[&(String, String)]) -> String {
    let (head, frag) = match addr.split_once('#') {
        Some((h, f)) => (h, Some(f)),
        None => (addr, None),
    };
    let (head, query) = match head.split_once('?') {
        Some((h, q)) => (h, Some(q)),
        None => (head, None),
    };
    let host = head.find("://").map_or(0, |i| i + 3);
    let path_start = head[host..].find('/').map_or(head.len(), |i| host + i);
    let mut out = head[..path_start].to_string();
    let path: Vec<String> = head[path_start..]
        .split('/')
        .map(|seg| {
            let text = decode(&seg.replace('+', "%2B"));
            match literals.iter().find(|(lit, _)| *lit == text) {
                Some((_, name)) => format!("{{{name}}}"),
                None => seg.to_string(),
            }
        })
        .collect();
    out.push_str(&path.join("/"));
    if let Some(q) = query {
        let pairs: Vec<String> = q
            .split('&')
            .map(|pair| {
                let Some((k, v)) = pair.split_once('=') else { return pair.to_string() };
                let text = decode(v);
                if let Some((_, name)) = literals.iter().find(|(lit, _)| *lit == text) {
                    return format!("{k}={{{name}}}");
                }
                // Words of a search, not an id like `sr_1_2`.
                if text.contains(char::is_whitespace) {
                    let mut pieces = vec![Piece::Text(text)];
                    for (lit, name) in literals {
                        pieces = sub_tokens(pieces, lit, name);
                    }
                    if pieces.iter().any(|p| matches!(p, Piece::Param(_))) {
                        let enc = |t: &str| url::form_urlencoded::byte_serialize(t.as_bytes()).collect::<String>();
                        return format!("{k}={}", render(&pieces, enc));
                    }
                }
                pair.to_string()
            })
            .collect();
        out.push('?');
        out.push_str(&pairs.join("&"));
    }
    if let Some(f) = frag {
        out.push('#');
        out.push_str(f);
    }
    out
}

impl Target {
    pub fn label(&self) -> String {
        if self.name.is_empty() { self.role.clone() } else { format!("{} \"{}\"", self.role, self.name) }
    }
}

/// One recorded or replayable step.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Step {
    /// `open`, `click`, `type`, `press`, `select`, `fill`, `back`.
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Target>,
    /// Typed text, chosen option, keys, or the address for `open`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// For `type`: press Enter afterwards.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub submit: bool,
    /// For `fill`: each field and its value.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<(Target, String)>,
    /// The page pattern the step starts on, and the one it led to.
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
}

impl Step {
    pub fn describe(&self) -> String {
        let target = match &self.target {
            Some(t) if t.matches_params => format!("the {} that best matches the parameters (was {})", t.role, t.label()),
            Some(t) => t.label(),
            None => String::new(),
        };
        match self.action.as_str() {
            "open" => format!("open {}", self.value.as_deref().unwrap_or("")),
            "type" => format!(
                "type “{}” into {target}{}",
                self.value.as_deref().unwrap_or(""),
                if self.submit { " and press Enter" } else { "" }
            ),
            "select" => format!("choose “{}” in {target}", self.value.as_deref().unwrap_or("")),
            "press" => format!("press {}{}", self.value.as_deref().unwrap_or(""), if target.is_empty() { String::new() } else { format!(" in {target}") }),
            "fill" => format!(
                "fill {}",
                self.fields.iter().map(|(t, v)| format!("{} = “{v}”", t.label())).collect::<Vec<_>>().join(", ")
            ),
            "back" => "go back".into(),
            other => format!("{other} {target}"),
        }
    }

    /// Replaces each literal with its `{param}` where it is the value itself:
    /// a whole path segment or query value of an `open` address, a whole
    /// word of typed or chosen text, and an element's whole name (or a whole
    /// word of it, for literals of three or more characters that aren't just
    /// digits). The `2` in `AB12345` or `2.5` stays as it is.
    fn parameterize(&mut self, literals: &[(String, String)]) {
        let literals: Vec<&(String, String)> = literals.iter().filter(|(lit, _)| !lit.is_empty()).collect();
        if let Some(v) = self.value.as_mut() {
            *v = if self.action == "open" { sub_url(v, &literals) } else { sub_text(v, &literals) };
        }
        let from = self.from.clone();
        if let Some(t) = self.target.as_mut() {
            t.name = sub_name(&t.name, &literals);
            // A result on a search page that matches the searched words
            // ("TUSQ XL Jazz Bass nut" for "TUSQ nut") stands for "the
            // result matching the query", not that exact title.
            if !t.name.contains('{') && page_kind(&from, "") == "search" {
                let have = words(&t.name);
                t.matches_params = literals.iter().any(|(lit, _)| {
                    let want = words(lit);
                    !want.is_empty() && want.iter().filter(|w| have.contains(w)).count() * 2 >= want.len()
                });
            }
        }
        for (t, v) in self.fields.iter_mut() {
            t.name = sub_name(&t.name, &literals);
            *v = sub_text(v, &literals);
        }
    }

    /// The step with `{param}`s filled in.
    pub fn fill(&self, values: &BTreeMap<String, String>) -> Step {
        let put = |s: &str, encode: bool| {
            let mut s = s.to_string();
            for (k, v) in values {
                let v = if encode { url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>() } else { v.clone() };
                s = s.replace(&format!("{{{k}}}"), &v);
            }
            s
        };
        let mut out = self.clone();
        let is_open = self.action == "open";
        out.value = self.value.as_deref().map(|v| put(v, is_open));
        if let Some(t) = out.target.as_mut() {
            t.name = put(&t.name, false);
        }
        for (t, v) in out.fields.iter_mut() {
            t.name = put(&t.name, false);
            *v = put(v, false);
        }
        out
    }

    /// The `{param}` names the step uses.
    fn params(&self) -> Vec<String> {
        let mut text = self.value.clone().unwrap_or_default();
        if let Some(t) = &self.target {
            text.push_str(&t.name);
        }
        for (t, v) in &self.fields {
            text.push_str(&t.name);
            text.push_str(v);
        }
        let mut out = Vec::new();
        let mut rest = text.as_str();
        while let Some(start) = rest.find('{') {
            let Some(len) = rest[start..].find('}') else { break };
            let name = &rest[start + 1..start + len];
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') && !out.iter().any(|n| n == name) {
                out.push(name.to_string());
            }
            rest = &rest[start + len + 1..];
        }
        out
    }
}

#[derive(Clone, Debug)]
pub struct RecordedAction {
    pub id: i64,
    pub site: String,
    pub from_url: String,
    pub step: Step,
    pub to_url: String,
    pub at: DateTime<Local>,
}

#[derive(Clone, Debug)]
pub struct Playbook {
    pub id: i64,
    pub site: String,
    pub name: String,
    pub description: String,
    pub params: Vec<String>,
    pub steps: Vec<Step>,
    pub created: DateTime<Local>,
    pub runs: i64,
    pub successes: i64,
    pub last_run: Option<DateTime<Local>>,
    pub last_error: Option<String>,
}

/// A page pattern on a site and what's known about it.
#[derive(Clone, Debug)]
pub struct MapPage {
    pub pattern: String,
    pub kind: &'static str,
    pub title: String,
    pub visits: usize,
    /// Elements the agent used here, most used first: (label, uses).
    pub elements: Vec<(String, usize)>,
}

#[derive(Clone, Debug)]
pub struct SiteMap {
    pub site: String,
    pub pages: Vec<MapPage>,
    /// (from pattern, how, to pattern, count), most common first.
    pub paths: Vec<(String, String, String, usize)>,
    pub playbooks: Vec<Playbook>,
    pub actions: usize,
}

fn when(ts: i64) -> DateTime<Local> {
    crate::when(ts)
}

impl Browsing {
    /// Logs a page-changing action, if it happened on a focus site.
    pub fn record_action(&self, from_url: &str, step: &Step, to_url: &str) {
        let Some(site) = site_of(from_url).or_else(|| site_of(to_url)) else { return };
        if !self.is_focused(&site) {
            return;
        }
        let mut step = step.clone();
        step.from = url_pattern(from_url);
        step.to = url_pattern(to_url);
        let Ok(json) = serde_json::to_string(&step) else { return };
        let c = self.conn();
        let _ = c.execute(
            "INSERT INTO site_actions (site, from_url, from_pattern, step, to_url, to_pattern, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![site, from_url, step.from, json, to_url, step.to, Local::now().timestamp()],
        );
        let _ = c.execute(
            "DELETE FROM site_actions WHERE id <= (SELECT MAX(id) FROM site_actions) - ?1",
            params![MAX_ACTIONS],
        );
    }

    /// Recorded actions on `site`, oldest first (the last `limit`).
    pub fn site_actions(&self, site: &str, limit: usize) -> Vec<RecordedAction> {
        let c = self.conn();
        let Ok(mut stmt) = c.prepare(
            "SELECT id, site, from_url, step, to_url, at FROM site_actions WHERE site = ?1 ORDER BY id DESC LIMIT ?2",
        ) else {
            return Vec::new();
        };
        let mut out: Vec<RecordedAction> = stmt
            .query_map(params![site, limit as i64], |r| {
                let step: String = r.get(3)?;
                Ok(RecordedAction {
                    id: r.get(0)?,
                    site: r.get(1)?,
                    from_url: r.get(2)?,
                    step: serde_json::from_str(&step).unwrap_or_default(),
                    to_url: r.get(4)?,
                    at: when(r.get(5)?),
                })
            })
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default();
        out.reverse();
        out
    }

    /// The latest unbroken run of actions on `site`: no gap over 30 minutes,
    /// starting after the last `open` of another site's page.
    pub fn latest_run(&self, site: &str) -> Vec<RecordedAction> {
        let all = self.site_actions(site, 200);
        let mut start = all.len();
        for i in (0..all.len()).rev() {
            if i + 1 < all.len() && (all[i + 1].at - all[i].at).num_seconds() > RUN_GAP_SECS {
                break;
            }
            start = i;
        }
        all[start..].to_vec()
    }

    /// Everything known about a site (8.1).
    pub fn site_map(&self, site: &str) -> SiteMap {
        let site = site.trim().trim_start_matches("www.").to_ascii_lowercase();
        // Pages from history (the user's visits and the agent's).
        let visits = self.history(&crate::HistoryQuery { site: Some(site.clone()), limit: 2_000, ..Default::default() });
        let mut pages: BTreeMap<String, MapPage> = BTreeMap::new();
        for v in &visits {
            let pattern = url_pattern(&v.url);
            let page = pages.entry(pattern.clone()).or_insert_with(|| MapPage {
                kind: page_kind(&pattern, &v.title),
                pattern,
                title: v.title.clone(),
                visits: 0,
                elements: Vec::new(),
            });
            page.visits += 1;
            if page.title.is_empty() {
                page.title = v.title.clone();
            }
        }
        // Elements and paths from the agent's actions.
        let actions = self.site_actions(&site, MAX_ACTIONS as usize);
        let mut used: BTreeMap<(String, String), usize> = BTreeMap::new();
        let mut paths: BTreeMap<(String, String, String), usize> = BTreeMap::new();
        for a in &actions {
            let how = a.step.describe();
            if let Some(t) = &a.step.target {
                *used.entry((a.step.from.clone(), t.label())).or_default() += 1;
            }
            for (t, _) in &a.step.fields {
                *used.entry((a.step.from.clone(), t.label())).or_default() += 1;
            }
            pages.entry(a.step.from.clone()).or_insert_with(|| MapPage {
                kind: page_kind(&a.step.from, ""),
                pattern: a.step.from.clone(),
                title: String::new(),
                visits: 0,
                elements: Vec::new(),
            });
            if a.step.from != a.step.to {
                let how = if a.step.action == "type" || a.step.action == "fill" { a.step.action.clone() } else { how };
                *paths.entry((a.step.from.clone(), how, a.step.to.clone())).or_default() += 1;
            }
        }
        for ((pattern, label), n) in used {
            if let Some(p) = pages.get_mut(&pattern) {
                p.elements.push((label, n));
            }
        }
        let mut pages: Vec<MapPage> = pages.into_values().collect();
        for p in pages.iter_mut() {
            p.elements.sort_by(|a, b| b.1.cmp(&a.1));
            p.elements.truncate(8);
        }
        pages.sort_by(|a, b| (b.visits + b.elements.len()).cmp(&(a.visits + a.elements.len())));
        let mut paths: Vec<(String, String, String, usize)> =
            paths.into_iter().map(|((f, h, t), n)| (f, h, t, n)).collect();
        paths.sort_by(|a, b| b.3.cmp(&a.3));
        SiteMap { playbooks: self.playbooks(Some(&site)), site, pages, paths, actions: actions.len() }
    }

    /// Saves recorded actions as a playbook (8.2). `literals` maps values
    /// seen in the recording to parameter names ("TUSQ nut" → "query").
    pub fn save_playbook(
        &self,
        site: &str,
        name: &str,
        description: &str,
        actions: &[RecordedAction],
        literals: &[(String, String)],
    ) -> Result<Playbook, String> {
        if actions.is_empty() {
            return Err(format!("No recorded actions on {site} to save. Actions are recorded on focus sites only."));
        }
        // Longest literal first, so "TUSQ XL nut" wins over "TUSQ".
        let mut lits = literals.to_vec();
        lits.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        let mut steps: Vec<Step> = actions.iter().map(|a| a.step.clone()).collect();
        // The first step starts the route at its page, so a replay can begin anywhere.
        if steps.first().is_some_and(|s| s.action != "open") {
            let first = &actions[0];
            steps.insert(0, Step {
                action: "open".into(),
                value: Some(first.from_url.clone()),
                from: String::new(),
                to: first.step.from.clone(),
                ..Default::default()
            });
        }
        for s in steps.iter_mut() {
            s.parameterize(&lits);
        }
        let mut params: Vec<String> = Vec::new();
        for s in &steps {
            for p in s.params() {
                if !params.contains(&p) {
                    params.push(p);
                }
            }
        }
        let steps_json = serde_json::to_string(&steps).map_err(|e| e.to_string())?;
        let params_json = serde_json::to_string(&params).map_err(|e| e.to_string())?;
        let site = site.trim_start_matches("www.").to_ascii_lowercase();
        self.conn()
            .execute(
                "INSERT INTO playbooks (site, name, description, params, steps, created) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(site, name) DO UPDATE SET description = excluded.description, params = excluded.params,
                     steps = excluded.steps, runs = 0, successes = 0, last_run = NULL, last_error = NULL",
                params![site, name, description, params_json, steps_json, Local::now().timestamp()],
            )
            .map_err(|e| format!("couldn't save the playbook: {e}"))?;
        self.playbook(&site, name).ok_or_else(|| "The playbook wasn't saved.".to_string())
    }

    pub fn playbooks(&self, site: Option<&str>) -> Vec<Playbook> {
        let c = self.conn();
        let sql = "SELECT id, site, name, description, params, steps, created, runs, successes, last_run, last_error
                   FROM playbooks WHERE (?1 IS NULL OR site = ?1) ORDER BY site, name";
        let Ok(mut stmt) = c.prepare(sql) else { return Vec::new() };
        stmt.query_map(params![site], |r| {
            let params: String = r.get(4)?;
            let steps: String = r.get(5)?;
            Ok(Playbook {
                id: r.get(0)?,
                site: r.get(1)?,
                name: r.get(2)?,
                description: r.get(3)?,
                params: serde_json::from_str(&params).unwrap_or_default(),
                steps: serde_json::from_str(&steps).unwrap_or_default(),
                created: when(r.get(6)?),
                runs: r.get(7)?,
                successes: r.get(8)?,
                last_run: r.get::<_, Option<i64>>(9)?.map(when),
                last_error: r.get(10)?,
            })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    pub fn playbook(&self, site: &str, name: &str) -> Option<Playbook> {
        let site = site.trim_start_matches("www.").to_ascii_lowercase();
        self.playbooks(Some(&site)).into_iter().find(|p| p.name.eq_ignore_ascii_case(name))
    }

    /// Finds a playbook by name on any site.
    pub fn find_playbook(&self, name: &str) -> Vec<Playbook> {
        self.playbooks(None).into_iter().filter(|p| p.name.eq_ignore_ascii_case(name)).collect()
    }

    pub fn remove_playbook(&self, id: i64) -> bool {
        self.conn().execute("DELETE FROM playbooks WHERE id = ?1", params![id]).map(|n| n > 0).unwrap_or(false)
    }

    /// Records how a replay went: `Ok(())`, or where it fell off the map.
    pub fn record_playbook_run(&self, id: i64, result: Result<(), String>) {
        let ok = result.is_ok();
        let _ = self.conn().execute(
            "UPDATE playbooks SET runs = runs + 1, successes = successes + ?2, last_run = ?3, last_error = ?4 WHERE id = ?1",
            params![id, ok as i64, Local::now().timestamp(), result.err()],
        );
    }

    /// Playbook counts per focus site, for the context sent with messages.
    pub fn playbook_counts(&self) -> BTreeMap<String, usize> {
        let mut out = BTreeMap::new();
        for p in self.playbooks(None) {
            *out.entry(p.site).or_insert(0) += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_group_urls() {
        assert_eq!(
            url_pattern("https://www.amazon.com/Graph-Tech-TUSQ-XL-Nut/dp/B0ABC12345/ref=sr_1_1?k=nut&crid=X&qid=1"),
            "/{slug}/dp/{id}?k"
        );
        assert_eq!(url_pattern("https://www.amazon.com/dp/B0XYZ98765"), "/dp/{id}");
        assert_eq!(url_pattern("https://shop.example/orders/1234"), "/orders/{n}");
        assert_eq!(url_pattern("https://duckduckgo.com/?q=bass&ia=web"), "/?ia&q");
        assert_eq!(url_pattern("https://shop.example/"), "/");
        assert_eq!(url_pattern("https://shop.example/cart?utm_source=x"), "/cart");
    }

    #[test]
    fn kinds() {
        assert_eq!(page_kind("/s?k", "Amazon.com : nut"), "search");
        assert_eq!(page_kind("/{slug}/dp/{id}", "Graph Tech nut"), "product");
        assert_eq!(page_kind("/gp/cart/view.html", ""), "cart");
        assert_eq!(page_kind("/checkout", ""), "checkout");
        assert_eq!(page_kind("/ap/signin", ""), "sign-in");
        assert_eq!(page_kind("/", "Home"), "home");
    }

    fn step(action: &str, role: &str, name: &str, value: Option<&str>) -> Step {
        Step {
            action: action.into(),
            target: (!role.is_empty()).then(|| Target { role: role.into(), name: name.into(), ..Default::default() }),
            value: value.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn steps_parameterize_and_fill() {
        let mut s = step("open", "", "", Some("https://shop.example/search?q=TUSQ+nut"));
        s.parameterize(&[("TUSQ nut".into(), "query".into())]);
        assert_eq!(s.value.as_deref(), Some("https://shop.example/search?q={query}"));
        let mut values = BTreeMap::new();
        values.insert("query".to_string(), "bone saddle".to_string());
        assert_eq!(s.fill(&values).value.as_deref(), Some("https://shop.example/search?q=bone+saddle"));

        let mut r = step("click", "link", "TUSQ XL Jazz Bass nut", None);
        r.from = "/search?q".into();
        r.parameterize(&[("TUSQ nut".into(), "query".into())]);
        assert!(r.target.as_ref().unwrap().matches_params, "a search result for the query");
        let mut other = step("click", "link", "In stock only", None);
        other.from = "/search?q".into();
        other.parameterize(&[("TUSQ nut".into(), "query".into())]);
        assert!(!other.target.as_ref().unwrap().matches_params);

        let mut t = step("click", "link", "TUSQ nut, black", None);
        t.parameterize(&[("TUSQ nut".into(), "query".into())]);
        assert_eq!(t.target.as_ref().unwrap().name, "{query}, black");
        assert_eq!(t.params(), vec!["query".to_string()]);
        assert_eq!(t.fill(&values).target.unwrap().name, "bone saddle, black");
    }

    fn qty() -> Vec<(String, String)> {
        vec![("2".into(), "quantity".into())]
    }

    #[test]
    fn short_literals_stay_out_of_ids() {
        let mut s = step("open", "", "", Some("https://shop.example/item/AB12345"));
        s.parameterize(&qty());
        assert_eq!(s.value.as_deref(), Some("https://shop.example/item/AB12345"));
        assert!(s.params().is_empty());

        let mut s = step("open", "", "", Some("https://shop.example/item/AB12345/2?ref=sr_1_2&v=2.5#2"));
        s.parameterize(&qty());
        assert_eq!(s.value.as_deref(), Some("https://shop.example/item/AB12345/{quantity}?ref=sr_1_2&v=2.5#2"));
    }

    #[test]
    fn query_values_parameterize() {
        let mut s = step("open", "", "", Some("https://shop.example/cart/add?id=AB12345&qty=2"));
        s.parameterize(&qty());
        assert_eq!(s.value.as_deref(), Some("https://shop.example/cart/add?id=AB12345&qty={quantity}"));
        let values = BTreeMap::from([("quantity".to_string(), "3".to_string())]);
        assert_eq!(s.fill(&values).value.as_deref(), Some("https://shop.example/cart/add?id=AB12345&qty=3"));

        // Words of a search, alongside others.
        let mut s = step("open", "", "", Some("https://shop.example/search?q=TUSQ+nut+black&page=2"));
        s.parameterize(&[("TUSQ nut".into(), "query".into())]);
        assert_eq!(s.value.as_deref(), Some("https://shop.example/search?q={query}+black&page=2"));
        let values = BTreeMap::from([("query".to_string(), "bone saddle".to_string())]);
        assert_eq!(s.fill(&values).value.as_deref(), Some("https://shop.example/search?q=bone+saddle+black&page=2"));
    }

    #[test]
    fn names_parameterize_only_whole() {
        let lits = vec![("2".to_string(), "size".to_string()), ("Medium".to_string(), "fit".to_string())];
        let name = |n: &str| {
            let mut s = step("click", "button", n, None);
            s.parameterize(&lits);
            s.target.unwrap().name
        };
        assert_eq!(name("2"), "{size}");
        assert_eq!(name("Size 2 (US)"), "Size 2 (US)", "too short to be sure it's the size");
        assert_eq!(name("Page 12"), "Page 12");
        assert_eq!(name("Medium (US)"), "{fit} (US)");
        assert_eq!(name("Mediumweight"), "Mediumweight");
    }

    #[test]
    fn typed_text_parameterizes_by_word() {
        let lits = vec![("TUSQ nut".to_string(), "query".to_string()), ("2".to_string(), "quantity".to_string())];
        let mut s = step("type", "searchbox", "Search", Some("TUSQ nut for 2 basses, 2.5 mm, B2B"));
        s.submit = true;
        s.parameterize(&lits);
        assert_eq!(s.value.as_deref(), Some("{query} for {quantity} basses, 2.5 mm, B2B"));
        assert_eq!(s.params(), vec!["query".to_string(), "quantity".to_string()]);

        let mut f = Step {
            action: "fill".into(),
            fields: vec![
                (Target { role: "spinbutton".into(), name: "Qty".into(), ..Default::default() }, "2".into()),
                (Target { role: "textbox".into(), name: "Address line 2".into(), ..Default::default() }, "12 Elm St".into()),
            ],
            ..Default::default()
        };
        f.parameterize(&lits);
        assert_eq!(f.fields[0].1, "{quantity}");
        assert_eq!(f.fields[1].0.name, "Address line 2");
        assert_eq!(f.fields[1].1, "12 Elm St");
    }

    #[test]
    fn actions_become_playbooks_on_focus_sites_only() {
        let b = Browsing::in_memory();
        let search = step("type", "searchbox", "Search", Some("TUSQ nut"));
        b.record_action("https://shop.example/", &search, "https://shop.example/");
        assert!(b.site_actions("shop.example", 10).is_empty(), "not a focus site yet");

        b.add_focus("shop.example", "Shop");
        b.record_action("https://shop.example/", &search, "https://shop.example/");
        b.record_action("https://shop.example/", &step("press", "", "", Some("Enter")), "https://shop.example/search?q=TUSQ+nut");
        b.record_action(
            "https://shop.example/search?q=TUSQ+nut",
            &step("click", "link", "TUSQ nut XL", None),
            "https://shop.example/item/AB12345",
        );
        let run = b.latest_run("shop.example");
        assert_eq!(run.len(), 3);
        let pb = b
            .save_playbook("shop.example", "find a part", "Search and open the first match", &run, &[("TUSQ nut".into(), "query".into())])
            .unwrap();
        assert_eq!(pb.params, vec!["query".to_string()]);
        // An open step was added so the route starts at its first page.
        assert_eq!(pb.steps[0].action, "open");
        assert_eq!(pb.steps[1].value.as_deref(), Some("{query}"));
        assert_eq!(pb.steps[3].from, "/search?q");
        assert_eq!(pb.steps[3].to, "/item/{id}");

        let map = b.site_map("shop.example");
        assert_eq!(map.playbooks.len(), 1);
        assert!(map.paths.iter().any(|(f, _, t, _)| f == "/search?q" && t == "/item/{id}"));
        assert!(map.pages.iter().any(|p| p.pattern == "/search?q" && p.kind == "search"));
        assert_eq!(b.playbook_counts().get("shop.example"), Some(&1));

        b.record_playbook_run(pb.id, Err("step 2: page didn't match".into()));
        let pb = b.playbook("shop.example", "Find a part").unwrap();
        assert_eq!((pb.runs, pb.successes), (1, 0));
        assert!(b.remove_playbook(pb.id));
    }
}
