//! Verification codes from the user's email (roadmap 5.2).
//!
//! `browser_email_code` finds the code a site just emailed, in the user's
//! Gmail, and puts it on screen for the user: a card above the page with
//! Copy and "Fill the code field". Then it hands the page to the user and
//! waits. The model never receives the code: it's told only who sent it
//! and when, so a page can't talk it into reading a code out.

use super::{Browser, check_keys, site};
use crate::bridge::CodeCard;
use futures::future::BoxFuture;
use router::{CodeEmail, CodeSource, Effect, Tool, ToolOutput, ToolResult};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Codes older than this aren't offered.
const LOOK_BACK_SECS: i64 = 15 * 60;
/// A code this recent is taken to be the one just requested.
const FRESH_SECS: i64 = 3 * 60;
/// How long to wait for a fresh code before offering an older one.
const PREFER_FRESH_FOR: Duration = Duration::from_secs(30);
const POLL_EVERY: Duration = Duration::from_secs(5);
/// How long the user has to enter it.
const HANDOFF_FOR: Duration = Duration::from_secs(10 * 60);

pub struct EmailCode(pub(super) Arc<Browser>, pub(super) Option<Arc<dyn CodeSource>>);

/// "Netflix <info@netflix.com>" → "Netflix".
fn sender_name(from: &str) -> String {
    let name = from.split('<').next().unwrap_or("").trim().trim_matches('"');
    if name.is_empty() { from.trim().to_string() } else { name.to_string() }
}

/// What the model may know about the email: never the code.
fn describe(c: &CodeEmail, now: i64) -> String {
    let mins = (now - c.received).max(0) / 60;
    let age = if mins == 0 { "just now".to_string() } else { format!("{mins} min ago") };
    let subject = c.subject.replace(&c.code, "••••••");
    format!("{} (“{subject}”, received {age})", sender_name(&c.from))
}

impl Tool for EmailCode {
    fn name(&self) -> &'static str {
        "browser_email_code"
    }
    fn description(&self) -> &'static str {
        "When a page asks for a verification or sign-in code that the site sent by email, find that email in the \
         user's Gmail and show the code to the user above the page, with Copy and \"Fill the code field\" buttons, \
         then wait while they enter it. You never see the code; you're told who sent it and when, and whether the \
         user finished. Waits for the email to arrive (default 90 s). Use it right after the site says it sent a \
         code. For codes by text message or an authenticator app, use browser_handoff instead."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "site": {"type": "string", "description": "The site that sent the code. Default: the current page's site."},
                "wait_seconds": {"type": "integer", "minimum": 0, "maximum": 180, "description": "How long to wait for the email. Default 90."}
            },
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, _: &Value) -> String {
        "Find the verification code in Gmail and show it to the user".into()
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let (b, source) = (self.0.clone(), self.1.clone());
        Box::pin(async move {
            check_keys(&input, &["site", "wait_seconds"])?;
            let Some(source) = source else {
                return Err("Gmail isn't connected, so the code can't be read from email. Use browser_handoff so the \
                            user enters it."
                    .into());
            };
            let page = b.state().await?.url;
            let hint = match input["site"].as_str() {
                Some(s) => Some(s.to_string()),
                None => browsing::site_of(&page),
            };
            let wait = Duration::from_secs(input["wait_seconds"].as_u64().unwrap_or(90).min(180));
            let started = Instant::now();
            let found = loop {
                let now = chrono::Utc::now().timestamp();
                let codes = source.recent_codes(now - LOOK_BACK_SECS, hint.clone()).await?;
                // Just asked for a new code? Give it a moment to arrive
                // before falling back to an older one.
                if let Some(c) = codes.iter().find(|c| now - c.received <= FRESH_SECS) {
                    break Some(c.clone());
                }
                if started.elapsed() >= PREFER_FRESH_FOR.min(wait) {
                    if let Some(c) = codes.first() {
                        break Some(c.clone());
                    }
                }
                if started.elapsed() >= wait {
                    break None;
                }
                tokio::time::sleep(POLL_EVERY).await;
            };
            let Some(code) = found else {
                return Ok(ToolOutput::text(
                    format!(
                        "No email with a verification code arrived in the last {} minutes (waited {} s). The site may \
                         have sent it by text message, or it's still on its way: use browser_handoff so the user can \
                         enter it, or ask whether they got it.",
                        LOOK_BACK_SECS / 60,
                        started.elapsed().as_secs()
                    ),
                    "no code email found",
                ));
            };
            let now = chrono::Utc::now().timestamp();
            let about = describe(&code, now);
            let sender = sender_name(&code.from);
            b.bridge.show_code(CodeCard { code: code.code.clone(), from: code.from.clone(), received: code.received });
            let message = format!(
                "Your code from {sender} is on screen. Copy it or press Fill, submit the form, then press Done."
            );
            let done = b.bridge.handoff(message, HANDOFF_FOR).await;
            let outcome = match done {
                Ok(true) => "The user says they've entered it. Read the page to see whether it was accepted.",
                Ok(false) => "The user chose not to enter it now. Ask them how they'd like to continue.",
                Err(e) => return Err(format!("The code from {about} was shown, but the user didn't finish: {e}")),
            };
            Ok(ToolOutput::text(
                format!("Showed the user the verification code from {about}. You can't see the code. {outcome}"),
                format!("code from {sender} shown to the user on {}", site(&page)),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_never_gets_the_code() {
        let c = CodeEmail {
            code: "482913".into(),
            from: "\"Netflix\" <info@account.netflix.com>".into(),
            subject: "482913 is your Netflix code".into(),
            received: 1_000,
        };
        let text = describe(&c, 1_000 + 130);
        assert!(!text.contains("482913"), "{text}");
        assert!(text.starts_with("Netflix (“•••••• is your Netflix code”, received 2 min ago)"), "{text}");
        assert_eq!(sender_name("info@x.com"), "info@x.com");
    }
}
