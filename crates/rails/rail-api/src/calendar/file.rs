//! A calendar kept in a JSON file, standing in for Google Calendar in test
//! instances (`UNIBROW_TEST_CALENDAR`):
//!
//! ```json
//! {"calendars": [{"id": "primary", "summary": "Test", "primary": true, "accessRole": "owner"}],
//!  "events": [{"id": "e1", "calendar": "primary", "summary": "Dentist",
//!              "start": {"dateTime": "2026-10-28T14:00:00-07:00"}, "end": {...}}],
//!  "sent": []}
//! ```
//!
//! Events are Google's event JSON plus the calendar they're on. What Google
//! would have emailed is appended to `sent` (`{"action", "event", "to"}`),
//! so a test can check that nothing went out without approval.

use super::{Backend, EventQuery, When, guests};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Mutex;

pub struct FileCalendar {
    path: PathBuf,
    /// One change at a time (read, change, write).
    lock: Mutex<()>,
}

impl FileCalendar {
    pub fn new(path: PathBuf) -> Self {
        Self { path, lock: Mutex::new(()) }
    }

    fn load(&self) -> Value {
        let mut v: Value = std::fs::read_to_string(&self.path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(json!({}));
        if !v["calendars"].is_array() {
            v["calendars"] = json!([{"id": "primary", "summary": "Calendar", "primary": true, "accessRole": "owner"}]);
        }
        for key in ["events", "sent"] {
            if !v[key].is_array() {
                v[key] = json!([]);
            }
        }
        v
    }

    fn save(&self, v: &Value) -> Result<(), String> {
        std::fs::write(&self.path, serde_json::to_string_pretty(v).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
    }

    /// Runs `f` on the file's contents and saves them.
    fn change<T>(&self, f: impl FnOnce(&mut Value) -> Result<T, String>) -> Result<T, String> {
        let _held = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut v = self.load();
        let out = f(&mut v)?;
        self.save(&v)?;
        Ok(out)
    }

    fn known(v: &Value, calendar: &str) -> Result<(), String> {
        let ok = v["calendars"].as_array().is_some_and(|c| c.iter().any(|c| c["id"] == calendar));
        if ok { Ok(()) } else { Err(format!("There's no calendar “{calendar}”. Check calendar_list.")) }
    }
}

/// Where in the list event `id` on `calendar` is.
fn find(v: &Value, calendar: &str, id: &str) -> Result<usize, String> {
    v["events"]
        .as_array()
        .and_then(|e| e.iter().position(|e| e["calendar"] == calendar && e["id"] == id))
        .ok_or_else(|| format!("Google Calendar couldn't find that (no event {id}). Check the event_id or calendar id."))
}

fn sent(v: &mut Value, action: &str, event: &Value, to: Vec<String>) {
    if !to.is_empty() {
        if let Some(list) = v["sent"].as_array_mut() {
            list.push(json!({"action": action, "event": event["summary"], "to": to}));
        }
    }
}

impl Backend for FileCalendar {
    fn calendars(&self) -> BoxFuture<'_, Result<Vec<Value>, String>> {
        let v = self.load();
        Box::pin(async move { Ok(v["calendars"].as_array().cloned().unwrap_or_default()) })
    }

    fn events<'a>(&'a self, calendar: &'a str, q: &'a EventQuery) -> BoxFuture<'a, Result<Vec<Value>, String>> {
        Box::pin(async move {
            let v = self.load();
            Self::known(&v, calendar)?;
            let text = q.text.as_ref().map(|t| t.to_lowercase());
            let mut found: Vec<(chrono::DateTime<chrono::FixedOffset>, Value)> = v["events"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|e| e["calendar"] == calendar && e["status"] != "cancelled")
                .filter_map(|e| {
                    let start = When::from_json(&e["start"])?.instant();
                    let end = When::from_json(&e["end"]).map(When::instant).unwrap_or(start);
                    let overlaps = start < q.to && end > q.from;
                    let matches = text.as_ref().is_none_or(|t| {
                        ["summary", "location", "description"].iter().any(|k| e[*k].as_str().is_some_and(|s| s.to_lowercase().contains(t)))
                    });
                    (overlaps && matches).then(|| (start, e.clone()))
                })
                .collect();
            found.sort_by_key(|(s, _)| *s);
            Ok(found.into_iter().take(q.max as usize).map(|(_, e)| e).collect())
        })
    }

    fn event<'a>(&'a self, calendar: &'a str, id: &'a str) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            let v = self.load();
            let i = find(&v, calendar, id)?;
            Ok(v["events"][i].clone())
        })
    }

    fn insert<'a>(&'a self, calendar: &'a str, mut event: Value, notify: bool) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            self.change(|v| {
                Self::known(v, calendar)?;
                let n = v["events"].as_array().map(Vec::len).unwrap_or(0);
                event["id"] = json!(format!("evt{}", n + 1));
                event["calendar"] = json!(calendar);
                event["status"] = json!("confirmed");
                event["organizer"] = json!({"self": true});
                if notify {
                    sent(v, "invite", &event, guests(&event));
                }
                if let Some(list) = v["events"].as_array_mut() {
                    list.push(event.clone());
                }
                Ok(event)
            })
        })
    }

    fn patch<'a>(&'a self, calendar: &'a str, id: &'a str, patch: Value, notify: bool) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            self.change(|v| {
                let i = find(v, calendar, id)?;
                let before = guests(&v["events"][i]);
                if let (Some(e), Some(p)) = (v["events"][i].as_object_mut(), patch.as_object()) {
                    for (k, val) in p {
                        e.insert(k.clone(), val.clone());
                    }
                }
                let event = v["events"][i].clone();
                if notify {
                    let mut to = before;
                    for g in guests(&event) {
                        if !to.contains(&g) {
                            to.push(g);
                        }
                    }
                    sent(v, "update", &event, to);
                }
                Ok(event)
            })
        })
    }

    fn delete<'a>(&'a self, calendar: &'a str, id: &'a str, notify: bool) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.change(|v| {
                let i = find(v, calendar, id)?;
                let event = v["events"].as_array_mut().map(|l| l.remove(i)).unwrap_or(Value::Null);
                if notify {
                    sent(v, "cancel", &event, guests(&event));
                }
                Ok(())
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::When;

    #[tokio::test]
    async fn keeps_events_in_the_file() {
        let dir = std::env::temp_dir().join(format!("file-cal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("calendar.json");
        std::fs::write(&path, r#"{"events": [{"id": "e1", "calendar": "primary", "summary": "Dentist",
            "start": {"dateTime": "2026-10-28T14:00:00-07:00"}, "end": {"dateTime": "2026-10-28T15:00:00-07:00"}}]}"#)
            .unwrap();
        let cal = FileCalendar::new(path.clone());
        let q = |from: &str, to: &str| EventQuery {
            from: When::parse(from).unwrap().instant(),
            to: When::parse(to).unwrap().instant(),
            text: None,
            max: 10,
        };
        assert_eq!(cal.events("primary", &q("2026-10-28", "2026-10-29")).await.unwrap().len(), 1);
        assert!(cal.events("primary", &q("2026-10-29", "2026-10-30")).await.unwrap().is_empty());
        assert!(cal.events("work", &q("2026-10-28", "2026-10-29")).await.is_err());

        let made = cal
            .insert("primary", json!({"summary": "Lunch", "start": {"date": "2026-10-29"}, "end": {"date": "2026-10-30"},
                "attendees": [{"email": "ann@y.com"}]}), true)
            .await
            .unwrap();
        assert_eq!(made["id"], "evt2");
        cal.patch("primary", "evt2", json!({"summary": "Long lunch"}), false).await.unwrap();
        cal.delete("primary", "e1", false).await.unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["events"].as_array().unwrap().len(), 1);
        assert_eq!(v["events"][0]["summary"], "Long lunch");
        // Only the invitation was "emailed".
        assert_eq!(v["sent"], json!([{"action": "invite", "event": "Lunch", "to": ["ann@y.com"]}]));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
