//! Calendar tools: the user's calendars and events (Google Calendar; a
//! file stands in for it in test instances).
//!
//! Both backends speak Google Calendar's event JSON, so the tools build and
//! read that. Approvals: reading is free, and creating or changing the
//! user's own events just happens (they're logged in the trace, like Gmail
//! drafts). Anything that emails other people (invitations, updates,
//! cancellations) asks first, with "always allow these recipients"; deleting
//! an event always asks.

pub mod file;
pub mod google;

use chrono::{DateTime, Duration, FixedOffset, Local, NaiveDate, NaiveDateTime, TimeZone};
use futures::future::BoxFuture;
use router::{Effect, Tool, ToolContext, ToolOutput, ToolResult};
use serde_json::{Value, json};
use std::sync::Arc;

/// Most events listed in one call.
const MAX_EVENTS: u64 = 100;
/// The range listed when none is given.
const DEFAULT_DAYS: i64 = 7;

/// Which events to list.
#[derive(Clone, Debug)]
pub struct EventQuery {
    pub from: DateTime<FixedOffset>,
    pub to: DateTime<FixedOffset>,
    pub text: Option<String>,
    pub max: u64,
}

/// Where the calendar lives: Google, or a file in a test instance.
pub trait Backend: Send + Sync {
    /// The user's calendars (Google's calendarList entries).
    fn calendars(&self) -> BoxFuture<'_, Result<Vec<Value>, String>>;
    /// Events overlapping the query's range, recurring ones expanded, by
    /// start time.
    fn events<'a>(&'a self, calendar: &'a str, q: &'a EventQuery) -> BoxFuture<'a, Result<Vec<Value>, String>>;
    fn event<'a>(&'a self, calendar: &'a str, id: &'a str) -> BoxFuture<'a, Result<Value, String>>;
    /// `notify`: email the guests (Google's sendUpdates=all).
    fn insert<'a>(&'a self, calendar: &'a str, event: Value, notify: bool) -> BoxFuture<'a, Result<Value, String>>;
    fn patch<'a>(&'a self, calendar: &'a str, id: &'a str, patch: Value, notify: bool) -> BoxFuture<'a, Result<Value, String>>;
    fn delete<'a>(&'a self, calendar: &'a str, id: &'a str, notify: bool) -> BoxFuture<'a, Result<(), String>>;
}

/// A start or end: a whole day, or a moment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum When {
    Day(NaiveDate),
    At(DateTime<FixedOffset>),
}

impl When {
    /// From what the model wrote: `2026-10-28` (a day), `2026-10-28T14:00`
    /// or `2026-10-28 14:00` (local time), or RFC 3339 with an offset.
    pub fn parse(s: &str) -> Result<When, String> {
        let s = s.trim();
        if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            return Ok(When::Day(d));
        }
        if let Ok(t) = DateTime::parse_from_rfc3339(s) {
            return Ok(When::At(t));
        }
        for f in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
            if let Ok(n) = NaiveDateTime::parse_from_str(s, f) {
                return local(n).map(When::At).ok_or_else(|| format!("{s} doesn't exist in the local time zone (a clock change)."));
            }
        }
        Err(format!("Couldn't read the time “{s}”: use 2026-10-28 for a whole day, or 2026-10-28T14:00 (local time)."))
    }

    /// From Google's `{date}` or `{dateTime}`.
    pub fn from_json(v: &Value) -> Option<When> {
        if let Some(d) = v["date"].as_str() {
            return NaiveDate::parse_from_str(d, "%Y-%m-%d").ok().map(When::Day);
        }
        v["dateTime"].as_str().and_then(|t| DateTime::parse_from_rfc3339(t).ok()).map(When::At)
    }

    pub fn to_json(self) -> Value {
        match self {
            When::Day(d) => json!({"date": d.format("%Y-%m-%d").to_string()}),
            When::At(t) => json!({"dateTime": t.to_rfc3339()}),
        }
    }

    /// The moment it starts (a day: local midnight).
    pub fn instant(self) -> DateTime<FixedOffset> {
        match self {
            When::Day(d) => local(d.and_hms_opt(0, 0, 0).expect("midnight")).unwrap_or_else(|| Local::now().fixed_offset()),
            When::At(t) => t,
        }
    }
}

/// A local wall-clock time with its offset (the earlier one when a clock
/// change makes it ambiguous).
fn local(n: NaiveDateTime) -> Option<DateTime<FixedOffset>> {
    Local.from_local_datetime(&n).earliest().map(|t| t.fixed_offset())
}

/// "Tue Oct 28, 2:00–3:00 PM", "Tue Oct 28 (all day)", in local time.
pub fn span(start: When, end: When) -> String {
    match (start, end) {
        (When::Day(a), When::Day(b)) => {
            let last = b.pred_opt().unwrap_or(b);
            if last <= a {
                format!("{} (all day)", a.format("%a %b %-d, %Y"))
            } else {
                format!("{} – {} (all day)", a.format("%a %b %-d"), last.format("%a %b %-d, %Y"))
            }
        }
        (a, b) => {
            let (a, b) = (a.instant().with_timezone(&Local), b.instant().with_timezone(&Local));
            if a.date_naive() == b.date_naive() {
                format!("{}–{}", a.format("%a %b %-d, %Y, %-I:%M %p"), b.format("%-I:%M %p"))
            } else {
                format!("{} – {}", a.format("%a %b %-d, %-I:%M %p"), b.format("%a %b %-d, %Y, %-I:%M %p"))
            }
        }
    }
}

/// The guests who'd get email about an event: attendees other than the
/// user and rooms.
pub fn guests(event: &Value) -> Vec<String> {
    event["attendees"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|g| !g["self"].as_bool().unwrap_or(false) && !g["resource"].as_bool().unwrap_or(false))
                .filter_map(|g| g["email"].as_str().map(|e| e.to_lowercase()))
                .collect()
        })
        .unwrap_or_default()
}

/// One line for an event.
pub fn describe(event: &Value) -> String {
    let title = event["summary"].as_str().filter(|s| !s.is_empty()).unwrap_or("(no title)");
    let when = match (When::from_json(&event["start"]), When::from_json(&event["end"])) {
        (Some(a), Some(b)) => span(a, b),
        (Some(a), None) => span(a, a),
        _ => "(no time)".into(),
    };
    let mut line = format!("{title} | {when} | event_id={}", event["id"].as_str().unwrap_or("?"));
    if let Some(l) = event["location"].as_str().filter(|s| !s.is_empty()) {
        line.push_str(&format!(" | at {l}"));
    }
    let g = guests(event);
    if !g.is_empty() {
        line.push_str(&format!(" | guests: {}", g.join(", ")));
    }
    // Google marks the user's own events `organizer.self`; an event with no
    // organizer at all (a test calendar's) is the user's too.
    let mine = !event["organizer"].is_object() || event["organizer"]["self"].as_bool().unwrap_or(false);
    if !mine {
        if let Some(o) = event["organizer"]["email"].as_str() {
            line.push_str(&format!(" | organized by {o}"));
        }
        let me = event["attendees"].as_array().and_then(|a| a.iter().find(|g| g["self"].as_bool().unwrap_or(false)));
        if let Some(r) = me.and_then(|m| m["responseStatus"].as_str()) {
            line.push_str(&format!(" | your answer: {r}"));
        }
    }
    if event["recurringEventId"].is_string() || event["recurrence"].is_array() {
        line.push_str(" | repeats");
    }
    if event["status"].as_str() == Some("tentative") {
        line.push_str(" | tentative");
    }
    line
}

fn check_keys(input: &Value, allowed: &[&str]) -> Result<(), String> {
    if let Some(o) = input.as_object() {
        if let Some(k) = o.keys().find(|k| !allowed.contains(&k.as_str())) {
            return Err(format!("Invalid input: unknown field `{k}`"));
        }
    }
    Ok(())
}

fn calendar_of(input: &Value) -> String {
    input["calendar"].as_str().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("primary").to_string()
}

/// Email addresses from a list input, checked.
fn addresses(v: &Value, key: &str) -> Result<Vec<String>, String> {
    let Some(list) = v[key].as_array() else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for x in list {
        let s = x.as_str().map(str::trim).unwrap_or("");
        let at = s.find('@');
        if !at.is_some_and(|i| i > 0 && i < s.len() - 1) || s.contains(char::is_whitespace) || s.contains(',') {
            return Err(format!("Invalid input: `{key}` takes plain email addresses (“{s}” isn't one)."));
        }
        let s = s.to_lowercase();
        if !out.contains(&s) {
            out.push(s);
        }
    }
    Ok(out)
}

/// The end for a start when none is given: an hour later, or the next day.
fn default_end(start: When) -> When {
    match start {
        When::Day(d) => When::Day(d.succ_opt().unwrap_or(d)),
        When::At(t) => When::At(t + Duration::hours(1)),
    }
}

/// Start and end from the input, checked: the same kind, end after start.
fn times(input: &Value) -> Result<(When, When), String> {
    let start = When::parse(input["start"].as_str().ok_or("Invalid input: `start` is required")?)?;
    let end = match input["end"].as_str() {
        Some(e) => When::parse(e)?,
        None => default_end(start),
    };
    check_order(start, end)?;
    Ok((start, end))
}

fn check_order(start: When, end: When) -> Result<(), String> {
    match (start, end) {
        (When::Day(a), When::Day(b)) if b > a => Ok(()),
        (When::Day(_), When::Day(_)) => Err("An all-day event's end must be after its start (end is the day after the last day).".into()),
        (When::At(a), When::At(b)) if b > a => Ok(()),
        (When::At(_), When::At(_)) => Err("The end must be after the start.".into()),
        _ => Err("Give both start and end as days (all day) or both as times.".into()),
    }
}

pub fn tools(backend: Arc<dyn Backend>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ListCalendars(backend.clone())),
        Arc::new(ListEvents(backend.clone())),
        Arc::new(CreateEvent(backend.clone())),
        Arc::new(UpdateEvent(backend.clone())),
        Arc::new(DeleteEvent(backend)),
    ]
}

pub struct ListCalendars(Arc<dyn Backend>);
pub struct ListEvents(Arc<dyn Backend>);
pub struct CreateEvent(Arc<dyn Backend>);
pub struct UpdateEvent(Arc<dyn Backend>);
pub struct DeleteEvent(Arc<dyn Backend>);

impl Tool for ListCalendars {
    fn name(&self) -> &'static str {
        "calendar_list"
    }
    fn description(&self) -> &'static str {
        "List the user's calendars (id, name, which is primary, and whether events can be added). Most tools \
         default to the primary calendar; pass another's id as `calendar`."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, _: &Value) -> String {
        "List calendars".into()
    }
    fn call(&self, _: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            let cals = b.calendars().await?;
            let lines: Vec<String> = cals
                .iter()
                .map(|c| {
                    format!(
                        "- {} | calendar={}{}{}",
                        c["summaryOverride"].as_str().or(c["summary"].as_str()).unwrap_or("(unnamed)"),
                        c["id"].as_str().unwrap_or("?"),
                        if c["primary"].as_bool().unwrap_or(false) { " | primary" } else { "" },
                        match c["accessRole"].as_str() {
                            Some("owner") | Some("writer") => "",
                            _ => " | read only",
                        }
                    )
                })
                .collect();
            Ok(ToolOutput::text(format!("{} calendar(s):\n{}", cals.len(), lines.join("\n")), format!("{} calendars", cals.len())))
        })
    }
}

impl Tool for ListEvents {
    fn name(&self) -> &'static str {
        "calendar_events"
    }
    fn description(&self) -> &'static str {
        "List events on the user's calendar between two times (default: now to 7 days ahead), recurring ones \
         expanded, optionally matching text. Times: 2026-10-28 (a day), 2026-10-28T14:00 (local time) or RFC 3339. \
         The answer starts with the current local time. Use it to see what's planned or find free time."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "from": {"type": "string", "description": "Start of the range (default: now)."},
                "to": {"type": "string", "description": "End of the range (default: 7 days after `from`). A day means the end of that day."},
                "query": {"type": "string", "description": "Only events whose title, place or notes contain this."},
                "calendar": {"type": "string", "description": "Calendar id (default: primary)."},
                "max": {"type": "integer", "minimum": 1, "maximum": MAX_EVENTS}
            },
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Check calendar {}–{}", input["from"].as_str().unwrap_or("now"), input["to"].as_str().unwrap_or("next week"))
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["from", "to", "query", "calendar", "max"])?;
            let now = Local::now().fixed_offset();
            let from = match input["from"].as_str() {
                Some(s) => When::parse(s)?.instant(),
                None => now,
            };
            let to = match input["to"].as_str() {
                // A day as the end means through that day.
                Some(s) => match When::parse(s)? {
                    When::Day(d) => When::Day(d.succ_opt().unwrap_or(d)).instant(),
                    w => w.instant(),
                },
                None => from + Duration::days(DEFAULT_DAYS),
            };
            if to <= from {
                return Err("`to` must be after `from`.".into());
            }
            let cal = calendar_of(&input);
            let q = EventQuery {
                from,
                to,
                text: input["query"].as_str().map(str::to_string).filter(|s| !s.trim().is_empty()),
                max: input["max"].as_u64().unwrap_or(50).clamp(1, MAX_EVENTS),
            };
            let events = b.events(&cal, &q).await?;
            let range = span(When::At(from), When::At(to));
            let head = format!("Now: {}\nCalendar {cal}, {range}", Local::now().format("%A, %B %-d, %Y %-I:%M %p (UTC%:z)"));
            if events.is_empty() {
                return Ok(ToolOutput::text(format!("{head}: no events."), format!("no events {range}")));
            }
            let lines: Vec<String> = events.iter().map(|e| format!("- {}", describe(e))).collect();
            Ok(ToolOutput::text(
                format!("{head}: {} event(s):\n{}", events.len(), lines.join("\n")),
                format!("{} events {range}", events.len()),
            ))
        })
    }
}

impl Tool for CreateEvent {
    fn name(&self) -> &'static str {
        "calendar_create_event"
    }
    fn description(&self) -> &'static str {
        "Add an event to the user's calendar. Times: 2026-10-28 for all day (end: the day after the last day), \
         or 2026-10-28T14:00 local time (end defaults to an hour later). With guests, Google emails them \
         invitations, so the user approves first; without guests it's just added."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": {"type": "string"},
                "start": {"type": "string"},
                "end": {"type": "string"},
                "location": {"type": "string"},
                "description": {"type": "string"},
                "guests": {"type": "array", "items": {"type": "string"}, "description": "Email addresses to invite."},
                "calendar": {"type": "string", "description": "Calendar id (default: primary)."}
            },
            "required": ["title", "start"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Stage
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Add “{}” to the calendar", input["title"].as_str().unwrap_or(""))
    }
    fn counterparties(&self, input: &Value) -> Vec<String> {
        addresses(input, "guests").unwrap_or_default()
    }
    fn call(&self, _: Value) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { Err("calendar_create_event needs the tool context".to_string()) })
    }
    fn call_with(&self, input: Value, ctx: ToolContext) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["title", "start", "end", "location", "description", "guests", "calendar"])?;
            let title = input["title"].as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or("Invalid input: `title` is required")?;
            let (start, end) = times(&input)?;
            let guests = addresses(&input, "guests")?;
            let cal = calendar_of(&input);
            let mut event = json!({"summary": title, "start": start.to_json(), "end": end.to_json()});
            for key in ["location", "description"] {
                if let Some(v) = input[key].as_str().filter(|s| !s.trim().is_empty()) {
                    event[key] = json!(v);
                }
            }
            let notify = !guests.is_empty();
            if notify {
                event["attendees"] = json!(guests.iter().map(|g| json!({"email": g})).collect::<Vec<_>>());
                let card = format!("Add “{title}” to calendar {cal}: {}\nInvitations will be emailed to: {}", span(start, end), guests.join(", "));
                let reason = format!("calendar_create_event emails invitations to {}", guests.join(", "));
                if !ctx.approver.approve("calendar_create_event", card, reason, guests.clone(), None).await {
                    return Err("The user declined this action. Nothing was added.".into());
                }
            }
            let made = b.insert(&cal, event, notify).await?;
            let sent = if notify { format!(" Invitations were emailed to {}.", guests.join(", ")) } else { String::new() };
            Ok(ToolOutput::text(format!("Added: {}.{sent}", describe(&made)), format!("added “{title}” {}", span(start, end))))
        })
    }
}

impl Tool for UpdateEvent {
    fn name(&self) -> &'static str {
        "calendar_update_event"
    }
    fn description(&self) -> &'static str {
        "Change an event: title, time, place, notes or guests. Give only what changes; a new start without an \
         end keeps the length. If the event has guests (before or after), Google emails them the update, so the \
         user approves first. Use the event_id from calendar_events."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "event_id": {"type": "string"},
                "calendar": {"type": "string", "description": "Calendar id (default: primary)."},
                "title": {"type": "string"},
                "start": {"type": "string"},
                "end": {"type": "string"},
                "location": {"type": "string"},
                "description": {"type": "string"},
                "add_guests": {"type": "array", "items": {"type": "string"}},
                "remove_guests": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["event_id"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Stage
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Change event {}", input["event_id"].as_str().unwrap_or(""))
    }
    fn call(&self, _: Value) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { Err("calendar_update_event needs the tool context".to_string()) })
    }
    fn call_with(&self, input: Value, ctx: ToolContext) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(
                &input,
                &["event_id", "calendar", "title", "start", "end", "location", "description", "add_guests", "remove_guests"],
            )?;
            let id = input["event_id"].as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or("Invalid input: `event_id` is required")?;
            let cal = calendar_of(&input);
            let old = b.event(&cal, id).await?;
            let (patch, changes) = plan_update(&old, &input)?;
            if changes.is_empty() {
                return Err("Nothing to change: give a new title, time, place, notes or guests.".into());
            }
            let before = guests(&old);
            let after = guests(&with_attendees(&old, &patch));
            let mut told: Vec<String> = before.clone();
            for g in &after {
                if !told.contains(g) {
                    told.push(g.clone());
                }
            }
            let notify = !told.is_empty();
            if notify {
                let card = format!("Change {}\n{}\nAn update will be emailed to: {}", describe(&old), changes.join("\n"), told.join(", "));
                let reason = format!("calendar_update_event emails an update to {}", told.join(", "));
                if !ctx.approver.approve("calendar_update_event", card, reason, told.clone(), None).await {
                    return Err("The user declined this action. Nothing was changed.".into());
                }
            }
            let new = b.patch(&cal, id, patch, notify).await?;
            let sent = if notify { format!(" The update was emailed to {}.", told.join(", ")) } else { String::new() };
            Ok(ToolOutput::text(
                format!("Changed ({}). Now: {}.{sent}", changes.join("; "), describe(&new)),
                format!("changed “{}”", new["summary"].as_str().unwrap_or("event")),
            ))
        })
    }
}

/// `old` with the attendees a patch would give it.
fn with_attendees(old: &Value, patch: &Value) -> Value {
    let mut e = old.clone();
    if patch.get("attendees").is_some() {
        e["attendees"] = patch["attendees"].clone();
    }
    e
}

/// The patch for an update and a line per change.
fn plan_update(old: &Value, input: &Value) -> Result<(Value, Vec<String>), String> {
    let mut patch = json!({});
    let mut changes = Vec::new();
    if let Some(t) = input["title"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
        patch["summary"] = json!(t);
        changes.push(format!("Title: “{}” → “{t}”", old["summary"].as_str().unwrap_or("")));
    }
    for (key, label) in [("location", "Place"), ("description", "Notes")] {
        if let Some(v) = input[key].as_str() {
            patch[key] = json!(v);
            changes.push(format!("{label}: {}", if v.trim().is_empty() { "removed".to_string() } else { v.to_string() }));
        }
    }
    if input["start"].is_string() || input["end"].is_string() {
        let (old_start, old_end) = (When::from_json(&old["start"]), When::from_json(&old["end"]));
        let start = match input["start"].as_str() {
            Some(s) => When::parse(s)?,
            None => old_start.ok_or("The event has no start time to keep.")?,
        };
        let end = match (input["end"].as_str(), old_start, old_end) {
            (Some(e), _, _) => When::parse(e)?,
            // Keep the length.
            (None, Some(When::At(a)), Some(When::At(b))) if matches!(start, When::At(_)) => When::At(start.instant() + (b - a)),
            (None, Some(When::Day(a)), Some(When::Day(b))) if matches!(start, When::Day(_)) => match start {
                When::Day(d) => When::Day(d + (b - a)),
                w => w,
            },
            (None, ..) => default_end(start),
        };
        check_order(start, end)?;
        patch["start"] = start.to_json();
        patch["end"] = end.to_json();
        let was = match (old_start, old_end) {
            (Some(a), Some(b)) => span(a, b),
            _ => "(no time)".into(),
        };
        changes.push(format!("Time: {was} → {}", span(start, end)));
    }
    let add = addresses(input, "add_guests")?;
    let remove = addresses(input, "remove_guests")?;
    if !add.is_empty() || !remove.is_empty() {
        let mut list: Vec<Value> = old["attendees"].as_array().cloned().unwrap_or_default();
        let had = |l: &[Value], e: &str| l.iter().any(|g| g["email"].as_str().is_some_and(|x| x.eq_ignore_ascii_case(e)));
        for g in &remove {
            if !had(&list, g) {
                return Err(format!("{g} isn't a guest of this event."));
            }
        }
        list.retain(|g| !g["email"].as_str().is_some_and(|x| remove.iter().any(|r| r.eq_ignore_ascii_case(x))));
        for g in &add {
            if !had(&list, g) {
                list.push(json!({"email": g}));
            }
        }
        patch["attendees"] = json!(list);
        if !add.is_empty() {
            changes.push(format!("Invite: {}", add.join(", ")));
        }
        if !remove.is_empty() {
            changes.push(format!("Remove: {}", remove.join(", ")));
        }
    }
    Ok((patch, changes))
}

impl Tool for DeleteEvent {
    fn name(&self) -> &'static str {
        "calendar_delete_event"
    }
    fn description(&self) -> &'static str {
        "Delete an event from the user's calendar. The user always approves first; if the event has guests, \
         Google emails them a cancellation. Use the event_id from calendar_events (for a repeating event, that \
         one occurrence)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "event_id": {"type": "string"},
                "calendar": {"type": "string", "description": "Calendar id (default: primary)."}
            },
            "required": ["event_id"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Stage
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Delete event {}", input["event_id"].as_str().unwrap_or(""))
    }
    fn call(&self, _: Value) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { Err("calendar_delete_event needs the tool context".to_string()) })
    }
    fn call_with(&self, input: Value, ctx: ToolContext) -> BoxFuture<'static, ToolResult> {
        let b = self.0.clone();
        Box::pin(async move {
            check_keys(&input, &["event_id", "calendar"])?;
            let id = input["event_id"].as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or("Invalid input: `event_id` is required")?;
            let cal = calendar_of(&input);
            let old = b.event(&cal, id).await?;
            let told = guests(&old);
            let notify = !told.is_empty();
            let mut card = format!("Delete {}", describe(&old));
            if notify {
                card.push_str(&format!("\nA cancellation will be emailed to: {}", told.join(", ")));
            }
            // Always asks: an empty list of recipients can't be pre-allowed.
            if !ctx.approver.approve("calendar_delete_event", card, "calendar_delete_event deletes an event".into(), Vec::new(), None).await
            {
                return Err("The user declined this action. Nothing was deleted.".into());
            }
            b.delete(&cal, id, notify).await?;
            let sent = if notify { format!(" A cancellation was emailed to {}.", told.join(", ")) } else { String::new() };
            Ok(ToolOutput::text(
                format!("Deleted “{}”.{sent}", old["summary"].as_str().unwrap_or("(no title)")),
                format!("deleted “{}”", old["summary"].as_str().unwrap_or("event")),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_as_days_or_local_moments() {
        assert_eq!(When::parse("2026-10-28").unwrap(), When::Day(NaiveDate::from_ymd_opt(2026, 10, 28).unwrap()));
        let When::At(t) = When::parse("2026-10-28T14:30").unwrap() else { panic!() };
        assert_eq!(t.with_timezone(&Local).format("%H:%M").to_string(), "14:30");
        assert_eq!(When::parse("2026-10-28 14:30").unwrap(), When::parse("2026-10-28T14:30").unwrap());
        let When::At(z) = When::parse("2026-10-28T14:30:00Z").unwrap() else { panic!() };
        assert_eq!(z.offset().local_minus_utc(), 0);
        assert!(When::parse("next tuesday").is_err());
    }

    #[test]
    fn ends_default_and_must_follow_starts() {
        let input = json!({"start": "2026-10-28T14:00"});
        let (a, b) = times(&input).unwrap();
        assert_eq!(b.instant() - a.instant(), Duration::hours(1));
        let (a, b) = times(&json!({"start": "2026-10-28"})).unwrap();
        assert_eq!((a, b), (When::parse("2026-10-28").unwrap(), When::parse("2026-10-29").unwrap()));
        assert!(times(&json!({"start": "2026-10-28T14:00", "end": "2026-10-28T13:00"})).is_err());
        assert!(times(&json!({"start": "2026-10-28", "end": "2026-10-28T13:00"})).is_err());
        assert!(times(&json!({"start": "2026-10-28", "end": "2026-10-28"})).is_err());
    }

    #[test]
    fn spans_read_naturally() {
        let d = |s| When::parse(s).unwrap();
        assert_eq!(span(d("2026-10-28"), d("2026-10-29")), "Wed Oct 28, 2026 (all day)");
        assert_eq!(span(d("2026-10-28"), d("2026-10-31")), "Wed Oct 28 – Fri Oct 30, 2026 (all day)");
        assert_eq!(span(d("2026-10-28T14:00"), d("2026-10-28T15:30")), "Wed Oct 28, 2026, 2:00 PM–3:30 PM");
    }

    #[test]
    fn guests_leave_out_the_user_and_rooms() {
        let e = json!({"attendees": [
            {"email": "Me@x.com", "self": true},
            {"email": "Ann@Y.com"},
            {"email": "room-1@resource.calendar.google.com", "resource": true}
        ]});
        assert_eq!(guests(&e), ["ann@y.com"]);
        assert!(addresses(&json!({"g": ["ann@y.com", "Ann@y.com"]}), "g").unwrap() == ["ann@y.com"]);
        assert!(addresses(&json!({"g": ["Ann <ann@y.com>"]}), "g").is_err());
        assert!(addresses(&json!({"g": ["a@b.com, c@d.com"]}), "g").is_err());
    }

    #[test]
    fn updates_keep_the_length_and_list_what_changes() {
        let old = json!({
            "summary": "Dentist",
            "start": {"dateTime": "2026-10-28T14:00:00-07:00"},
            "end": {"dateTime": "2026-10-28T15:30:00-07:00"},
            "attendees": [{"email": "me@x.com", "self": true}, {"email": "ann@y.com"}]
        });
        let (patch, changes) = plan_update(&old, &json!({"start": "2026-10-29T09:00:00-07:00", "remove_guests": ["ann@y.com"]})).unwrap();
        assert_eq!(patch["end"]["dateTime"], "2026-10-29T10:30:00-07:00");
        assert_eq!(patch["attendees"].as_array().unwrap().len(), 1);
        assert_eq!(changes.len(), 2);
        assert!(plan_update(&old, &json!({"remove_guests": ["bob@z.com"]})).unwrap_err().contains("isn't a guest"));
        // The update emails Ann, who was a guest before the change.
        assert!(guests(&with_attendees(&old, &patch)).is_empty() && guests(&old) == ["ann@y.com"]);
    }

    #[test]
    fn events_describe_in_one_line() {
        let e = json!({
            "id": "e1", "summary": "Lunch", "location": "Cafe",
            "start": {"date": "2026-10-28"}, "end": {"date": "2026-10-29"},
            "organizer": {"email": "ann@y.com"},
            "attendees": [{"email": "me@x.com", "self": true, "responseStatus": "needsAction"}, {"email": "ann@y.com"}],
            "recurringEventId": "base"
        });
        assert_eq!(
            describe(&e),
            "Lunch | Wed Oct 28, 2026 (all day) | event_id=e1 | at Cafe | guests: ann@y.com | organized by ann@y.com | your answer: needsAction | repeats"
        );
    }
}
