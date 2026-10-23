//! Recording and replaying routes on focus sites (roadmap 8.1, 8.2, 8.3, 8.5).
//!
//! - [`Recorded`] wraps the page-changing tools. After each successful
//!   action on a focus site it logs what was done, by role and name rather
//!   than by ref (refs change between visits), plus the page before and after.
//! - `playbook_save` turns the latest run of those actions into a named
//!   playbook, with literal values made into parameters.
//! - `playbook_run` replays one step by step without the model. Before each
//!   step it checks the page is the kind of page the recording started on,
//!   and after each step that it landed where the recording did. The moment
//!   anything differs it stops ("fell off the map") and hands the page back
//!   to the model with what happened. Steps go through the same tools, so
//!   anything that buys, sends or deletes still asks the user.
//! - Closed choices along the way ("which search result matches?") go to a
//!   [`Decider`], whose answer and confidence are reported: Jev, TypeSafe's
//!   decision model, when a key is set (see `crate::jev`), else a
//!   word-overlap match.

use super::{Browser, Click, History, Open, Press, Select, Type, check_keys, forms::FillForm, site};
use crate::bridge::NavAction;
use browsing::Browsing;
use browsing::sitemap::{Step, Target, page_kind, url_pattern};
use futures::future::BoxFuture;
use router::{Effect, Tool, ToolContext, ToolOutput, ToolResult};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Longest playbook that will be replayed.
const MAX_STEPS: usize = 40;

// ------------------------------------------------------------ outline lines

/// One element line of the outline: `[e17] link "Name" -> …`.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ElementLine {
    pub r: String,
    pub role: String,
    pub name: String,
}

pub(super) fn parse_line(line: &str) -> Option<ElementLine> {
    let t = line.trim_start();
    let rest = t.strip_prefix('[')?;
    let end = rest.find(']')?;
    let r = rest[..end].to_string();
    if !r.contains('e') || !r.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let rest = rest[end + 1..].trim_start();
    let role_end = rest.find([' ', '(']).unwrap_or(rest.len());
    let role = rest[..role_end].to_string();
    let after = &rest[role_end..];
    let name = after
        .find('"')
        .and_then(|q| after[q + 1..].find('"').map(|e| after[q + 1..q + 1 + e].to_string()))
        .unwrap_or_default();
    Some(ElementLine { r, role, name })
}

fn elements(lines: &[String]) -> Vec<ElementLine> {
    lines.iter().filter_map(|l| parse_line(l)).collect()
}

/// How to find the element `r` again: its role, name, and which of the
/// elements with that role and name it is.
fn target_for(lines: &[String], r: &str) -> Option<Target> {
    let els = elements(lines);
    let at = els.iter().position(|e| e.r == r)?;
    let el = &els[at];
    let nth = els[..at].iter().filter(|e| e.role == el.role && e.name == el.name).count();
    Some(Target { role: el.role.clone(), name: el.name.clone(), nth, matches_params: false })
}

/// Where the element `r` from the outline `before` is in the outline
/// `after`, for a ref that went stale because the page re-rendered (7.3):
/// the element with the same role and name, found the way replays find
/// recorded steps. Only exact matches count, and only when the page has as
/// many elements with that role and name as before, so "the second Save
/// button" still means the same one. Unnamed elements aren't guessed at.
pub(super) fn refind(before: &[String], after: &[String], r: &str) -> Option<String> {
    let t = target_for(before, r)?;
    if t.name.trim().is_empty() {
        return None;
    }
    let count = |lines: &[String]| elements(lines).iter().filter(|e| e.role == t.role && e.name == t.name).count();
    if count(before) != count(after) {
        return None;
    }
    find_by_words(after, &t, "", &BTreeMap::new()).filter(|f| f.confidence >= 1.0).map(|f| f.r)
}

// ------------------------------------------------------------ decisions (8.3)

/// A [`Decider`]'s pick: which option, how sure, and who decided.
#[derive(Clone, Debug)]
pub struct Decision {
    pub index: usize,
    pub confidence: f32,
    /// Shown in the step log, e.g. "Jev (jev-1.13.0)" or "word match".
    pub by: String,
}

/// Answers closed questions inside known pages. Jev (`crate::jev`)
/// implements this; [`WordMatch`] is the built-in stand-in.
pub trait Decider: Send + Sync {
    /// Which option best matches `wanted`? `None` if none is close enough.
    fn choose(&self, question: &str, wanted: &str, options: &[String]) -> BoxFuture<'static, Option<Decision>>;
}

/// Picks the option sharing the most words with what's wanted.
pub struct WordMatch;

fn words(s: &str) -> Vec<String> {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 1).map(str::to_string).collect()
}

impl WordMatch {
    fn pick(&self, wanted: &str, options: &[String]) -> Option<Decision> {
        let want = words(wanted);
        if want.is_empty() {
            return None;
        }
        let mut best: Option<(usize, f32)> = None;
        for (i, o) in options.iter().enumerate() {
            let have = words(o);
            let hits = want.iter().filter(|w| have.contains(w)).count();
            let score = hits as f32 / want.len() as f32;
            if best.is_none_or(|(_, b)| score > b) {
                best = Some((i, score));
            }
        }
        best.filter(|(_, s)| *s >= 0.6).map(|(index, confidence)| Decision { index, confidence, by: "word match".into() })
    }
}

impl Decider for WordMatch {
    fn choose(&self, _question: &str, wanted: &str, options: &[String]) -> BoxFuture<'static, Option<Decision>> {
        let d = self.pick(wanted, options);
        Box::pin(async move { d })
    }
}

/// Where a step's element is on the page now, and how sure that is.
struct Found {
    r: String,
    confidence: f32,
    /// Set when a decision was needed (the name didn't match exactly).
    note: Option<String>,
}

/// A step's element: found outright, or a question for a [`Decider`].
enum Lookup<'a> {
    Found(Found),
    Ask { question: &'static str, wanted: String, candidates: Vec<&'a ElementLine> },
}

fn lookup<'a>(els: &'a [ElementLine], t: &Target, page: &str, values: &BTreeMap<String, String>) -> Lookup<'a> {
    let same_role: Vec<&ElementLine> = els.iter().filter(|e| e.role == t.role).collect();
    let by_params = t.matches_params && !values.is_empty();
    let exact: Vec<&&ElementLine> =
        if by_params { Vec::new() } else { same_role.iter().filter(|e| e.name == t.name).collect() };
    if !exact.is_empty() {
        let e = exact.get(t.nth).or(exact.last()).expect("not empty");
        return Lookup::Found(Found { r: e.r.clone(), confidence: 1.0, note: None });
    }
    if !by_params {
        if let Some(e) = same_role.iter().find(|e| e.name.eq_ignore_ascii_case(&t.name)) {
            return Lookup::Found(Found { r: e.r.clone(), confidence: 0.95, note: None });
        }
    }
    // The result for what's being searched now, or the recorded element
    // under a slightly different name.
    let on_results = page_kind(page, "") == "search" && t.role == "link";
    let (question, wanted) = if (by_params || on_results) && !values.is_empty() {
        ("Which search result matches?", values.values().cloned().collect::<Vec<_>>().join(" "))
    } else {
        ("Which element is the recorded one?", t.name.clone())
    };
    Lookup::Ask { question, wanted, candidates: same_role }
}

fn decided(question: &str, wanted: &str, candidates: &[&ElementLine], d: Option<Decision>) -> Option<Found> {
    let d = d?;
    let e = candidates.get(d.index)?;
    Some(Found {
        r: e.r.clone(),
        confidence: d.confidence,
        note: Some(format!(
            "{question} Picked {} “{}” for “{wanted}” ({}, confidence {:.2})",
            e.role, e.name, d.by, d.confidence
        )),
    })
}

async fn find(
    lines: &[String],
    t: &Target,
    page: &str,
    values: &BTreeMap<String, String>,
    decider: &dyn Decider,
) -> Option<Found> {
    let els = elements(lines);
    match lookup(&els, t, page, values) {
        Lookup::Found(f) => Some(f),
        Lookup::Ask { question, wanted, candidates } => {
            let names: Vec<String> = candidates.iter().map(|e| e.name.clone()).collect();
            let d = decider.choose(question, &wanted, &names).await;
            decided(question, &wanted, &candidates, d)
        }
    }
}

/// [`find`] with the word match, without waiting on anything.
fn find_by_words(lines: &[String], t: &Target, page: &str, values: &BTreeMap<String, String>) -> Option<Found> {
    let els = elements(lines);
    match lookup(&els, t, page, values) {
        Lookup::Found(f) => Some(f),
        Lookup::Ask { question, wanted, candidates } => {
            let names: Vec<String> = candidates.iter().map(|e| e.name.clone()).collect();
            decided(question, &wanted, &candidates, WordMatch.pick(&wanted, &names))
        }
    }
}

/// Where a step's element is now, noting any decision made.
async fn locate(
    lines: &[String],
    t: &Target,
    from: &str,
    values: &BTreeMap<String, String>,
    decider: &dyn Decider,
    notes: &mut Vec<String>,
) -> Result<String, String> {
    match find(lines, t, from, values, decider).await {
        Some(f) => {
            match f.note {
                Some(n) => notes.push(n),
                None if f.confidence < 1.0 => notes.push(format!("Matched {} ignoring case", t.label())),
                None => {}
            }
            Ok(f.r)
        }
        None => Err(format!("{} isn't on the page", t.label())),
    }
}

// ------------------------------------------------------------ recording (8.1)

/// Wraps a page-changing tool so its successful actions on focus sites are
/// logged for the site map and for playbooks.
pub struct Recorded {
    inner: Arc<dyn Tool>,
    b: Arc<Browser>,
    store: Arc<Browsing>,
}

impl Recorded {
    pub(super) fn wrap(inner: Arc<dyn Tool>, b: Arc<Browser>, store: Arc<Browsing>) -> Arc<dyn Tool> {
        Arc::new(Self { inner, b, store })
    }
}

/// The step an action is, from its input and the outline the agent saw.
fn step_for(tool: &str, input: &Value, lines: &[String]) -> Option<Step> {
    let target = |key: &str| input[key].as_str().and_then(|r| target_for(lines, r));
    let mut step = Step { action: String::new(), ..Default::default() };
    match tool {
        "browser_open" => {
            step.action = "open".into();
            step.value = Some(crate::normalize_address(input["url"].as_str()?));
        }
        "browser_click" => {
            // Right, middle and double clicks are rare in routes; keep it simple.
            if input["button"].as_str().is_some_and(|b| b != "left") || input["double"].as_bool() == Some(true) {
                return None;
            }
            step.action = "click".into();
            step.target = Some(target("ref")?);
        }
        "browser_type" => {
            step.action = "type".into();
            step.target = Some(target("ref")?);
            step.value = Some(input["text"].as_str()?.to_string());
            step.submit = input["submit"].as_bool().unwrap_or(false);
        }
        "browser_select" => {
            step.action = "select".into();
            step.target = Some(target("ref")?);
            step.value = Some(input["option"].as_str()?.to_string());
        }
        "browser_press" => {
            step.action = "press".into();
            step.value = Some(input["keys"].as_str()?.to_string());
        }
        "browser_fill_form" => {
            step.action = "fill".into();
            for f in input["fields"].as_array()? {
                let t = f["ref"].as_str().and_then(|r| target_for(lines, r))?;
                let v = match &f["value"] {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                step.fields.push((t, v));
            }
        }
        "browser_back" => step.action = "back".into(),
        _ => return None,
    }
    Some(step)
}

impl Tool for Recorded {
    fn name(&self) -> &'static str {
        self.inner.name()
    }
    fn description(&self) -> &'static str {
        self.inner.description()
    }
    fn input_schema(&self) -> Value {
        self.inner.input_schema()
    }
    fn effect(&self) -> Effect {
        self.inner.effect()
    }
    fn describe_call(&self, input: &Value) -> String {
        self.inner.describe_call(input)
    }
    fn counterparties(&self, input: &Value) -> Vec<String> {
        self.inner.counterparties(input)
    }
    fn site_acted_on(&self, input: &Value) -> BoxFuture<'static, Option<String>> {
        self.inner.site_acted_on(input)
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        self.inner.call(input)
    }
    fn call_with(&self, input: Value, ctx: ToolContext) -> BoxFuture<'static, ToolResult> {
        let (inner, b, store) = (self.inner.clone(), self.b.clone(), self.store.clone());
        Box::pin(async move {
            let before = b.state().await.map(|s| s.url).unwrap_or_default();
            let lines = b.last_seen.lock().ok().and_then(|s| s.as_ref().map(|s| s.lines.clone())).unwrap_or_default();
            let step = step_for(inner.name(), &input, &lines);
            let out = inner.call_with(input, ctx).await;
            if let (Ok(_), Some(step)) = (&out, step) {
                let after = b.state().await.map(|s| s.url).unwrap_or_default();
                store.record_action(&before, &step, &after);
            }
            out
        })
    }
}

// ------------------------------------------------------------ playbook_save (8.2)

pub struct PlaybookSave(pub(super) Arc<Browser>, pub(super) Arc<Browsing>);

impl Tool for PlaybookSave {
    fn name(&self) -> &'static str {
        "playbook_save"
    }
    fn description(&self) -> &'static str {
        "Save what you just did on a focus site as a playbook: a named route the app can replay later with \
         playbook_run, without working it out again. It takes your latest run of actions on the site (opens, \
         clicks, typing, choices). Turn the values that change between uses into parameters with `params`, \
         mapping the literal you used to a name: {\"TUSQ nut\": \"query\"}. Save only routes the user wants \
         to repeat (they asked, or agreed when you offered). Saving under an existing name replaces it."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Short, e.g. \"find a part\" or \"reorder strings\"."},
                "description": {"type": "string", "description": "What the route does, in a sentence."},
                "site": {"type": "string", "description": "Default: the current page's site."},
                "params": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Literal value used → parameter name."}
            },
            "required": ["name", "description"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Stage
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Save playbook “{}”", input["name"].as_str().unwrap_or(""))
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let (b, store) = (self.0.clone(), self.1.clone());
        Box::pin(async move {
            check_keys(&input, &["name", "description", "site", "params"])?;
            let name = input["name"].as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or("Invalid input: `name` is required")?;
            let description = input["description"].as_str().unwrap_or("").trim();
            let site_name = match input["site"].as_str() {
                Some(s) => browsing::site_of(&crate::normalize_address(s)).unwrap_or_else(|| s.to_string()),
                None => browsing::site_of(&b.state().await?.url).ok_or("The current page isn't a website; pass `site`.")?,
            };
            if !store.is_focused(&site_name) {
                return Err(format!(
                    "{site_name} isn't a focus site. Playbooks are for focus sites; ask the user to add it with ◎ Focus first."
                ));
            }
            let literals: Vec<(String, String)> = input["params"]
                .as_object()
                .map(|o| o.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
                .unwrap_or_default();
            let run = store.latest_run(&site_name);
            let pb = store.save_playbook(&site_name, name, description, &run, &literals)?;
            let missing: Vec<&str> =
                literals.iter().filter(|(_, p)| !pb.params.contains(p)).map(|(l, _)| l.as_str()).collect();
            let mut out = format!(
                "Saved playbook “{}” for {} ({} steps{}):\n",
                pb.name,
                pb.site,
                pb.steps.len(),
                if pb.params.is_empty() { String::new() } else { format!("; parameters: {}", pb.params.join(", ")) }
            );
            for (i, s) in pb.steps.iter().enumerate() {
                out.push_str(&format!("{}. {}\n", i + 1, s.describe()));
            }
            if !missing.is_empty() {
                out.push_str(&format!("Note: these values never appeared in the steps, so they aren't parameters: {}\n", missing.join(", ")));
            }
            Ok(ToolOutput::text(out, format!("saved “{}” ({} steps)", pb.name, pb.steps.len())))
        })
    }
}

// ------------------------------------------------------------ playbook_run (8.2, 8.5)

pub struct PlaybookRun(pub(super) Arc<Browser>, pub(super) Arc<Browsing>);

impl PlaybookRun {
    fn lookup(store: &Browsing, input: &Value) -> Result<browsing::sitemap::Playbook, String> {
        let name = input["name"].as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or("Invalid input: `name` is required")?;
        if let Some(site) = input["site"].as_str() {
            let site = browsing::site_of(&crate::normalize_address(site)).unwrap_or_else(|| site.to_string());
            return store.playbook(&site, name).ok_or_else(|| format!("There's no playbook “{name}” for {site}. Check site_map."));
        }
        let found = store.find_playbook(name);
        match found.len() {
            0 => Err(format!("There's no playbook named “{name}”. Check site_map for the site's playbooks.")),
            1 => Ok(found.into_iter().next().expect("one")),
            _ => Err(format!("More than one site has a playbook “{name}”; pass `site`.")),
        }
    }
}

impl Tool for PlaybookRun {
    fn name(&self) -> &'static str {
        "playbook_run"
    }
    fn description(&self) -> &'static str {
        "Replay a saved playbook (see site_map) on the user's page, step by step, with its parameters filled in. \
         Each step checks the page is the kind of page the recording was on and lands where the recording \
         did; if anything differs (a redesign, a new pop-up, a sign-in), it stops and tells you where, and \
         you carry on yourself from the page it returns. Steps that buy, send or delete still ask the user."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "site": {"type": "string", "description": "Needed only when two sites have a playbook with this name."},
                "params": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Parameter name → value."}
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        // Each step gates itself the way the tool it uses does.
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Run playbook “{}”", input["name"].as_str().unwrap_or(""))
    }
    fn site_acted_on(&self, input: &Value) -> BoxFuture<'static, Option<String>> {
        let site = Self::lookup(&self.1, input).ok().map(|p| p.site);
        Box::pin(async move { site })
    }
    fn call(&self, _input: Value) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { Err("playbook_run needs the tool host's approval context.".to_string()) })
    }
    fn call_with(&self, input: Value, ctx: ToolContext) -> BoxFuture<'static, ToolResult> {
        let (b, store) = (self.0.clone(), self.1.clone());
        Box::pin(async move {
            check_keys(&input, &["name", "site", "params"])?;
            let pb = Self::lookup(&store, &input)?;
            let values: BTreeMap<String, String> = input["params"]
                .as_object()
                .map(|o| o.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
                .unwrap_or_default();
            let missing: Vec<&String> = pb.params.iter().filter(|p| !values.contains_key(*p)).collect();
            if !missing.is_empty() {
                return Err(format!(
                    "Playbook “{}” needs: {}.",
                    pb.name,
                    missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                ));
            }
            if pb.steps.len() > MAX_STEPS {
                return Err(format!("Playbook “{}” has more than {MAX_STEPS} steps; it won't be replayed.", pb.name));
            }
            let decider = crate::jev::decider();
            let mut log: Vec<String> = Vec::new();
            let mut last: Option<ToolOutput> = None;

            for (i, recorded) in pb.steps.iter().enumerate() {
                let step = recorded.fill(&values);
                let n = i + 1;
                let here = b.state().await?.url;
                // A playbook only ever acts on its own site: never on the
                // app's pages or another site that happens to share a path,
                // since each step runs without the per-tool checks for those.
                if step.action == "open" {
                    let to = crate::normalize_address(step.value.as_deref().unwrap_or_default());
                    if browsing::site_of(&to).as_deref() != Some(pb.site.as_str()) {
                        let why = format!("step {n} ({}) would leave {} for {to}", step.describe(), pb.site);
                        return Self::fell_off(&b, &store, &pb, &log, why).await;
                    }
                } else if browsing::site_of(&here).as_deref() != Some(pb.site.as_str()) {
                    let why = format!("step {n} ({}) is for {}, but the browser is on {}", step.describe(), pb.site, site(&here));
                    return Self::fell_off(&b, &store, &pb, &log, why).await;
                }
                // 8.5: is this the page the recording was on? (Opening an
                // address works from anywhere on the site.)
                if step.action != "open" && !step.from.is_empty() && url_pattern(&here) != step.from {
                    let why = format!(
                        "step {n} ({}) expects a {} page ({}), but the browser is on {}",
                        step.describe(),
                        page_kind(&step.from, ""),
                        step.from,
                        url_pattern(&here)
                    );
                    return Self::fell_off(&b, &store, &pb, &log, why).await;
                }
                let lines = if step.target.is_some() || !step.fields.is_empty() {
                    b.page_model().await.map(|m| m.lines).unwrap_or_default()
                } else {
                    Vec::new()
                };
                let mut notes: Vec<String> = Vec::new();
                let target = match &step.target {
                    Some(t) => locate(&lines, t, &step.from, &values, &*decider, &mut notes).await,
                    None => Err("the step has no target".to_string()),
                };
                let mut fields = Vec::new();
                for (t, v) in &step.fields {
                    fields.push(locate(&lines, t, &step.from, &values, &*decider, &mut notes).await.map(|r| json!({"ref": r, "value": v})));
                }
                let call: Result<(Arc<dyn Tool>, Value), String> = match step.action.as_str() {
                    "open" => Ok((Arc::new(Open(b.clone())), json!({"url": step.value.clone().unwrap_or_default()}))),
                    "back" => Ok((Arc::new(History(b.clone(), NavAction::Back)), json!({}))),
                    "press" => Ok((Arc::new(Press(b.clone())), json!({"keys": step.value.clone().unwrap_or_default()}))),
                    "click" => target.map(|r| (Arc::new(Click(b.clone())) as Arc<dyn Tool>, json!({"ref": r}))),
                    "type" => target.map(|r| {
                        let args = json!({"ref": r, "text": step.value.clone().unwrap_or_default(), "submit": step.submit});
                        (Arc::new(Type(b.clone())) as Arc<dyn Tool>, args)
                    }),
                    "select" => target.map(|r| {
                        (Arc::new(Select(b.clone())) as Arc<dyn Tool>, json!({"ref": r, "option": step.value.clone().unwrap_or_default()}))
                    }),
                    "fill" => fields
                        .into_iter()
                        .collect::<Result<Vec<_>, _>>()
                        .map(|fields| (Arc::new(FillForm(b.clone())) as Arc<dyn Tool>, json!({"fields": fields}))),
                    other => Err(format!("unknown step `{other}`")),
                };
                let (tool, args) = match call {
                    Ok(c) => c,
                    Err(e) => {
                        let why = format!("step {n} ({}): {e}", step.describe());
                        return Self::fell_off(&b, &store, &pb, &log, why).await;
                    }
                };
                match tool.call_with(args, ctx.clone()).await {
                    Ok(out) => last = Some(out),
                    Err(e) => {
                        let why = format!("step {n} ({}) failed: {e}", step.describe());
                        return Self::fell_off(&b, &store, &pb, &log, why).await;
                    }
                }
                // 8.5: did it land where the recording did?
                let landed_url = b.state().await?.url;
                let landed = url_pattern(&landed_url);
                if !step.to.is_empty() && step.to != step.from && !lands_like(&step, &landed, &landed_url) {
                    let why = format!(
                        "after step {n} ({}) the recording went to {} ({}), but this run is on {landed}",
                        step.describe(),
                        page_kind(&step.to, ""),
                        step.to
                    );
                    log.push(format!("{n}. {} — done, but landed somewhere else", step.describe()));
                    return Self::fell_off(&b, &store, &pb, &log, why).await;
                }
                let mut line = format!("{n}. {} ✓", step.describe());
                for note in notes {
                    line.push_str(&format!("\n   {note}"));
                }
                log.push(line);
            }

            store.record_playbook_run(pb.id, Ok(()));
            let last = last.unwrap_or_default();
            Ok(ToolOutput {
                content: format!(
                    "Playbook “{}” finished, {} steps:\n{}\n\nThe page now:\n{}",
                    pb.name,
                    pb.steps.len(),
                    log.join("\n"),
                    last.content
                ),
                summary: format!("ran “{}” ({} steps)", pb.name, pb.steps.len()),
                evidence: last.evidence,
                ..Default::default()
            })
        })
    }
}

/// Whether a step landed where the recording did. Picking "the result
/// matching the parameters" opens a different item than the recording did,
/// so there the same kind of page is enough.
fn lands_like(step: &Step, landed: &str, landed_url: &str) -> bool {
    if landed == step.to {
        return true;
    }
    let picked_by_params = step.target.as_ref().is_some_and(|t| t.matches_params);
    picked_by_params && page_kind(landed, "") == page_kind(&step.to, "") && !landed_url.is_empty()
}

impl PlaybookRun {
    /// Stops the replay (8.5): records why, and gives the model the page so
    /// it can carry on itself.
    async fn fell_off(
        b: &Arc<Browser>,
        store: &Browsing,
        pb: &browsing::sitemap::Playbook,
        log: &[String],
        why: String,
    ) -> ToolResult {
        store.record_playbook_run(pb.id, Err(why.clone()));
        let page = b.outline(0, 6000).await.unwrap_or_default();
        let st = b.state().await.unwrap_or_default();
        let done = if log.is_empty() { "No steps ran.".to_string() } else { format!("Steps done:\n{}", log.join("\n")) };
        Ok(ToolOutput::text(
            format!(
                "Playbook “{}” stopped: it fell off the map at {why}.\n{done}\n\nThe site may have changed. Carry on \
                 from here yourself (the page is below). If it keeps happening, tell the user the playbook needs \
                 to be recorded again.\n\n{page}",
                pb.name
            ),
            format!("“{}” stopped on {}", pb.name, site(&st.url)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines() -> Vec<String> {
        [
            "heading(1) \"Bass nuts\"",
            "  [e5] searchbox \"Search\"",
            "  [e17] link \"TUSQ XL Jazz Bass nut\" -> http://x/p-tusq.html",
            "  [e24] link \"Bone Jazz Bass nut\" -> http://x/p-bone.html",
            "  [f1e8] button \"Add to cart\"",
            "  [e30] button \"Add to cart\"",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn lines_parse() {
        assert_eq!(
            parse_line("  [e17] link \"TUSQ\" -> http://x").unwrap(),
            ElementLine { r: "e17".into(), role: "link".into(), name: "TUSQ".into() }
        );
        assert_eq!(parse_line("[f1e8] button \"Go\"").unwrap().r, "f1e8");
        assert_eq!(parse_line("[e3] heading(2) \"Specs\"").unwrap().role, "heading");
        assert!(parse_line("text \"hello\"").is_none());
    }

    #[test]
    fn targets_record_nth() {
        let t = target_for(&lines(), "e30").unwrap();
        assert!(!t.matches_params);
        assert_eq!((t.role.as_str(), t.name.as_str(), t.nth), ("button", "Add to cart", 1));
        let f = find_by_words(&lines(), &t, "/item", &BTreeMap::new()).unwrap();
        assert_eq!((f.r.as_str(), f.confidence), ("e30", 1.0));
    }

    #[test]
    fn stale_refs_are_found_again_by_role_and_name() {
        // The page re-rendered: same elements, new refs.
        let after: Vec<String> = lines().iter().map(|l| l.replace("[e", "[e10")).collect();
        assert_eq!(refind(&lines(), &after, "e24").as_deref(), Some("e1024"));
        assert_eq!(refind(&lines(), &after, "e30").as_deref(), Some("e1030"), "the second Add to cart stays the second");
        // One of two same-named buttons gone: which one is ambiguous.
        let fewer: Vec<String> = after.iter().filter(|l| !l.contains("[e1030]")).cloned().collect();
        assert!(refind(&lines(), &fewer, "e30").is_none());
        // Gone entirely, or renamed: no guessing.
        let renamed: Vec<String> = after.iter().map(|l| l.replace("Bone Jazz", "Bone Precision")).collect();
        assert!(refind(&lines(), &renamed, "e24").is_none());
        assert!(refind(&lines(), &after, "e99").is_none(), "never seen");
    }

    #[test]
    fn results_are_picked_by_what_was_searched() {
        // Recorded on a search for TUSQ; replayed searching for bone. The
        // TUSQ result is still on the page, but the bone one is wanted.
        let t = Target { role: "link".into(), name: "TUSQ XL Jazz Bass nut".into(), nth: 0, matches_params: true };
        let mut values = BTreeMap::new();
        values.insert("query".to_string(), "bone jazz nut".to_string());
        let f = find_by_words(&lines(), &t, "/search?q", &values).unwrap();
        assert_eq!(f.r, "e24");
        assert!(f.note.unwrap().contains("Which search result matches?"));
        // Nothing close: no guess.
        values.insert("query".to_string(), "brass saddle".to_string());
        assert!(find_by_words(&lines(), &t, "/search?q", &values).is_none());
    }

    #[test]
    fn result_picks_land_on_the_same_kind_of_page() {
        let mut step = Step { action: "click".into(), from: "/search?q".into(), to: "/dp/{id}".into(), ..Default::default() };
        step.target = Some(Target { role: "link".into(), name: "x".into(), nth: 0, matches_params: true });
        assert!(lands_like(&step, "/{slug}/dp/{id}", "https://x/a/dp/B1"), "another product page");
        assert!(!lands_like(&step, "/ap/signin", "https://x/ap/signin"), "a sign-in page is off the map");
        step.target.as_mut().unwrap().matches_params = false;
        assert!(!lands_like(&step, "/{slug}/dp/{id}", "https://x/a/dp/B1"));
    }

    #[test]
    fn actions_become_steps() {
        let s = step_for("browser_type", &json!({"ref": "e5", "text": "TUSQ", "submit": true}), &lines()).unwrap();
        assert_eq!(s.action, "type");
        assert!(s.submit, "Enter after typing is part of the step");
        assert_eq!(s.target.unwrap().name, "Search");
        assert!(step_for("browser_click", &json!({"ref": "e99"}), &lines()).is_none(), "unknown ref");
        assert!(step_for("browser_click", &json!({"ref": "e17", "button": "right"}), &lines()).is_none());
        let f = step_for("browser_fill_form", &json!({"fields": [{"ref": "e5", "value": "x"}]}), &lines()).unwrap();
        assert_eq!(f.fields.len(), 1);
    }
}
