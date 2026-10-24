//! Google Calendar over its REST API, signed in with the same Google
//! account as Gmail.
//!
//! A sign-in from before calendar access was asked for lacks its scopes:
//! Google answers 403, so the consent screen opens once more and the call
//! is retried. A Google Cloud project without the Calendar API turned on
//! gets a message with the link to turn it on.

use super::{Backend, EventQuery};
use crate::google_auth::GoogleAuth;
use futures::future::BoxFuture;
use serde_json::Value;
use std::sync::Arc;

const API: &str = "https://www.googleapis.com/calendar/v3";
const ENABLE_URL: &str = "https://console.cloud.google.com/apis/library/calendar-json.googleapis.com";

pub struct GoogleCalendar {
    auth: Arc<GoogleAuth>,
    http: reqwest::Client,
}

/// What a failed call means.
enum Failure {
    /// The sign-in doesn't cover the calendar yet.
    Scope,
    Other(String),
}

/// Reads Google's error body.
fn failure(status: u16, body: &str) -> Failure {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let message = v["error"]["message"].as_str().unwrap_or(body).trim().to_string();
    let reasons: Vec<&str> = v["error"]["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(v["error"]["details"].as_array().into_iter().flatten())
        .filter_map(|e| e["reason"].as_str())
        .collect();
    if status == 403 && (reasons.contains(&"insufficientPermissions") || reasons.contains(&"ACCESS_TOKEN_SCOPE_INSUFFICIENT")) {
        return Failure::Scope;
    }
    if status == 403 && (reasons.contains(&"accessNotConfigured") || reasons.contains(&"SERVICE_DISABLED")) {
        // Google's message carries the project's own link when it can.
        let link = message.split_whitespace().find(|w| w.starts_with("https://")).map(|w| w.trim_end_matches('.')).unwrap_or(ENABLE_URL);
        return Failure::Other(format!(
            "The Google Calendar API isn't turned on for the app's Google Cloud project. The user can turn it on at \
             {link} (then wait a minute and try again)."
        ));
    }
    if status == 404 {
        return Failure::Other(format!("Google Calendar couldn't find that ({message}). Check the event_id or calendar id."));
    }
    Failure::Other(format!("Google Calendar said HTTP {status}: {}", message.chars().take(300).collect::<String>()))
}

/// A calendar or event id as a URL path segment.
fn seg(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>().replace('+', "%20")
}

fn notify_param(notify: bool) -> (&'static str, &'static str) {
    ("sendUpdates", if notify { "all" } else { "none" })
}

impl GoogleCalendar {
    pub fn new(auth: Arc<GoogleAuth>) -> Self {
        Self { auth, http: reqwest::Client::new() }
    }

    /// One call, signing in again once if the sign-in lacks calendar access.
    async fn call(&self, method: reqwest::Method, path: &str, query: &[(&str, &str)], body: Option<&Value>) -> Result<Value, String> {
        for attempt in 0..2 {
            let token = self.auth.access_token().await?;
            let mut req = self.http.request(method.clone(), format!("{API}{path}")).bearer_auth(token).query(query);
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = req.send().await.map_err(|e| format!("Google Calendar request failed: {e}"))?;
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            if (200..300).contains(&status) {
                return Ok(if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text).map_err(|e| e.to_string())? });
            }
            match failure(status, &text) {
                Failure::Scope if attempt == 0 => {
                    eprintln!("Google Calendar: the saved sign-in has no calendar access; asking again");
                    self.auth.sign_in().await.map_err(|e| format!("Calendar access needs Google sign-in again: {e}"))?;
                }
                Failure::Scope => return Err("Google sign-in still doesn't allow the calendar.".into()),
                Failure::Other(e) => return Err(e),
            }
        }
        unreachable!("the loop returns")
    }
}

impl Backend for GoogleCalendar {
    fn calendars(&self) -> BoxFuture<'_, Result<Vec<Value>, String>> {
        Box::pin(async move {
            let v = self.call(reqwest::Method::GET, "/users/me/calendarList", &[], None).await?;
            Ok(v["items"].as_array().cloned().unwrap_or_default())
        })
    }

    fn events<'a>(&'a self, calendar: &'a str, q: &'a EventQuery) -> BoxFuture<'a, Result<Vec<Value>, String>> {
        Box::pin(async move {
            let (from, to, max) = (q.from.to_rfc3339(), q.to.to_rfc3339(), q.max.to_string());
            let mut query = vec![
                ("timeMin", from.as_str()),
                ("timeMax", to.as_str()),
                ("singleEvents", "true"),
                ("orderBy", "startTime"),
                ("maxResults", max.as_str()),
            ];
            if let Some(t) = &q.text {
                query.push(("q", t.as_str()));
            }
            let v = self.call(reqwest::Method::GET, &format!("/calendars/{}/events", seg(calendar)), &query, None).await?;
            Ok(v["items"].as_array().cloned().unwrap_or_default())
        })
    }

    fn event<'a>(&'a self, calendar: &'a str, id: &'a str) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            let path = format!("/calendars/{}/events/{}", seg(calendar), seg(id));
            self.call(reqwest::Method::GET, &path, &[], None).await
        })
    }

    fn insert<'a>(&'a self, calendar: &'a str, event: Value, notify: bool) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            let path = format!("/calendars/{}/events", seg(calendar));
            self.call(reqwest::Method::POST, &path, &[notify_param(notify)], Some(&event)).await
        })
    }

    fn patch<'a>(&'a self, calendar: &'a str, id: &'a str, patch: Value, notify: bool) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            let path = format!("/calendars/{}/events/{}", seg(calendar), seg(id));
            self.call(reqwest::Method::PATCH, &path, &[notify_param(notify)], Some(&patch)).await
        })
    }

    fn delete<'a>(&'a self, calendar: &'a str, id: &'a str, notify: bool) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let path = format!("/calendars/{}/events/{}", seg(calendar), seg(id));
            self.call(reqwest::Method::DELETE, &path, &[notify_param(notify)], None).await.map(|_| ())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_say_what_to_do() {
        let scope = r#"{"error":{"code":403,"message":"Request had insufficient authentication scopes.","errors":[{"reason":"insufficientPermissions"}]}}"#;
        assert!(matches!(failure(403, scope), Failure::Scope));
        let off = r#"{"error":{"code":403,"message":"Google Calendar API has not been used in project 123 before or it is disabled. Enable it by visiting https://console.developers.google.com/apis/api/calendar-json.googleapis.com/overview?project=123 then retry.","errors":[{"reason":"accessNotConfigured"}]}}"#;
        let Failure::Other(m) = failure(403, off) else { panic!() };
        assert!(m.contains("overview?project=123") && m.contains("turn it on"), "{m}");
        let Failure::Other(m) = failure(404, r#"{"error":{"message":"Not Found"}}"#) else { panic!() };
        assert!(m.contains("event_id"));
        assert_eq!(seg("me@x.com"), "me%40x.com");
        assert_eq!(seg("a b"), "a%20b");
    }
}
