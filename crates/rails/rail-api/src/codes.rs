//! Verification codes from the user's email (roadmap 5.2).
//!
//! When a site emails a sign-in or verification code, the app can find it in
//! Gmail and show it to the user to paste. The code itself never goes to a
//! model: only the user sees it.

use crate::gmail::Gmail;
use crate::mime;
use futures::future::BoxFuture;
use router::{CodeEmail, CodeSource};
use std::sync::Arc;

/// Words that mark a message as carrying a code, used in the Gmail search.
const SEARCH: &str =
    "(code OR verification OR verify OR passcode OR \"one-time\" OR OTP OR \"security code\" OR \"sign in\" OR login OR confirm)";
/// Words that, just before a number, make it a code.
const KEYWORDS: &[&str] = &[
    "code", "passcode", "otp", "one-time", "one time", "verification", "verify", "security", "pin", "sign in",
    "sign-in", "login", "log in", "confirm", "authenticat", "2fa", "token",
];

/// A candidate number in the text and how code-like it is.
struct Candidate {
    code: String,
    score: i32,
}

/// The verification code in a message, if it has one: a 4–8 digit number
/// near words like "code" or "verification", 6 digits preferred. Years,
/// prices, phone numbers and long reference numbers are passed over.
pub fn extract_code(subject: &str, body: &str) -> Option<String> {
    let mut best: Option<Candidate> = None;
    for (text, in_subject) in [(subject, true), (body, false)] {
        for c in candidates(text, in_subject) {
            if best.as_ref().is_none_or(|b| c.score > b.score) {
                best = Some(c);
            }
        }
    }
    best.filter(|b| b.score >= 4).map(|b| b.code)
}

fn candidates(text: &str, in_subject: bool) -> Vec<Candidate> {
    let chars: Vec<char> = text.chars().collect();
    let lower: Vec<char> = text.to_lowercase().chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        // A run of digits, allowing one space or hyphen between groups
        // ("123 456", "123-456").
        let start = i;
        let mut digits = String::new();
        let mut j = i;
        while j < chars.len() {
            if chars[j].is_ascii_digit() {
                digits.push(chars[j]);
                j += 1;
            } else if (chars[j] == ' ' || chars[j] == '-')
                && j + 1 < chars.len()
                && chars[j + 1].is_ascii_digit()
                && digits.len() % 3 == 0
                && digits.len() < 8
            {
                j += 1;
            } else {
                break;
            }
        }
        i = j;
        let len = digits.len();
        if !(4..=8).contains(&len) {
            continue;
        }
        let before = start.checked_sub(1).map(|k| chars[k]);
        let after = chars.get(j).copied();
        let then = chars.get(j + 1).copied();
        // Part of a price, a decimal, a time, a longer id, or a phone number.
        // A full stop or comma that ends the sentence is fine.
        let ends_sentence = then.is_none_or(char::is_whitespace);
        let bad_before = before.is_some_and(|c| c.is_alphanumeric() || "$€£.,:/#+(".contains(c));
        let bad_after = after.is_some_and(|c| {
            c.is_alphanumeric() || ":/%)".contains(c) || (".,".contains(c) && !ends_sentence)
        });
        if bad_before || bad_after {
            continue;
        }
        let mut score = match len {
            6 => 3,
            8 => 1,
            _ => 2,
        };
        if len == 4 && (digits.starts_with("19") || digits.starts_with("20")) {
            score -= 4; // a year
        }
        let window: String = lower[start.saturating_sub(60)..start].iter().collect();
        let ahead: String = lower[j.min(lower.len())..(j + 40).min(lower.len())].iter().collect();
        if KEYWORDS.iter().any(|k| window.contains(k)) {
            score += 3;
        } else if KEYWORDS.iter().any(|k| ahead.contains(k)) {
            score += 2; // "Use 7731 as your one-time PIN"
        }
        if in_subject {
            score += 1;
        }
        out.push(Candidate { code: digits, score });
    }
    out
}

impl Gmail {
    /// Messages since `since` that carry a code, newest first.
    pub async fn recent_codes(&self, since: i64, hint: Option<&str>) -> Result<Vec<CodeEmail>, String> {
        let q = format!("after:{since} {SEARCH}");
        let list = self.get("/messages", &[("q", &q), ("maxResults", "10")]).await?;
        let mut out = Vec::new();
        for m in list["messages"].as_array().cloned().unwrap_or_default() {
            let Some(id) = m["id"].as_str() else { continue };
            let msg = self.get(&format!("/messages/{id}"), &[("format", "full")]).await?;
            let payload = &msg["payload"];
            let subject = mime::header(payload, "Subject").unwrap_or("").to_string();
            let from = mime::header(payload, "From").unwrap_or("").to_string();
            let received = msg["internalDate"].as_str().and_then(|d| d.parse::<i64>().ok()).map(|ms| ms / 1000).unwrap_or(0);
            if received < since {
                continue;
            }
            if let Some(code) = extract_code(&subject, &mime::body_text(payload)) {
                out.push(CodeEmail { code, from, subject, received });
            }
        }
        rank(&mut out, hint);
        Ok(out)
    }
}

/// Newest first, but a sender matching the site asking comes first.
fn rank(codes: &mut [CodeEmail], hint: Option<&str>) {
    let name = hint.map(|h| h.trim_start_matches("www.").split('.').next().unwrap_or("").to_lowercase());
    codes.sort_by_key(|c| {
        let matches = name.as_deref().is_some_and(|n| n.len() > 2 && (c.from.to_lowercase().contains(n) || c.subject.to_lowercase().contains(n)));
        (!matches, -c.received)
    });
}

/// The user's Gmail as a [`CodeSource`].
pub struct GmailCodes(pub Arc<Gmail>);

impl CodeSource for GmailCodes {
    fn recent_codes(&self, since: i64, hint: Option<String>) -> BoxFuture<'static, Result<Vec<CodeEmail>, String>> {
        let g = self.0.clone();
        Box::pin(async move { g.recent_codes(since, hint.as_deref()).await })
    }
}

/// A fake inbox for isolated test instances: a JSON file of
/// `[{"from", "subject", "body", "seconds_ago"}]`, read on every check so a
/// test can add a message while the app waits.
pub struct FileCodes(pub std::path::PathBuf);

impl CodeSource for FileCodes {
    fn recent_codes(&self, since: i64, hint: Option<String>) -> BoxFuture<'static, Result<Vec<CodeEmail>, String>> {
        let path = self.0.clone();
        Box::pin(async move {
            let now = chrono::Utc::now().timestamp();
            let text = std::fs::read_to_string(&path).unwrap_or_else(|_| "[]".into());
            let msgs: Vec<serde_json::Value> = serde_json::from_str(&text).map_err(|e| format!("test inbox: {e}"))?;
            let mut out: Vec<CodeEmail> = msgs
                .iter()
                .filter_map(|m| {
                    let subject = m["subject"].as_str().unwrap_or("").to_string();
                    let code = extract_code(&subject, m["body"].as_str().unwrap_or(""))?;
                    let received = now - m["seconds_ago"].as_i64().unwrap_or(0);
                    (received >= since).then(|| CodeEmail { code, from: m["from"].as_str().unwrap_or("").into(), subject, received })
                })
                .collect();
            rank(&mut out, hint.as_deref());
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_codes() {
        assert_eq!(extract_code("Your Netflix sign-in code", "Enter this code to sign in: 482913"), Some("482913".into()));
        assert_eq!(extract_code("", "Your verification code is 123 456. It expires in 10 minutes."), Some("123456".into()));
        assert_eq!(extract_code("", "Use 7731 as your one-time PIN"), Some("7731".into()));
        assert_eq!(extract_code("482913 is your Instagram code", "Don't share it."), Some("482913".into()));
    }

    #[test]
    fn passes_over_lookalikes() {
        // Years, prices, phone numbers, order numbers with no code wording.
        assert_eq!(extract_code("Your receipt", "Order 2026 total $1234.56. Call +1 555 0100 2000."), None);
        assert_eq!(extract_code("Newsletter", "Since 1998 we've shipped 50000 parts."), None);
        // The code wins over a year and an order number in the same message.
        assert_eq!(
            extract_code("Confirm your email", "Order #A1234567 from 2026. Your confirmation code: 905521"),
            Some("905521".into())
        );
    }

    #[test]
    fn site_sender_ranks_first() {
        let mut codes = vec![
            CodeEmail { code: "1".into(), from: "Bank <a@bank.com>".into(), subject: "code".into(), received: 200 },
            CodeEmail { code: "2".into(), from: "Netflix <info@netflix.com>".into(), subject: "code".into(), received: 100 },
        ];
        rank(&mut codes, Some("www.netflix.com"));
        assert_eq!(codes[0].code, "2");
        rank(&mut codes, None);
        assert_eq!(codes[0].code, "1", "newest first without a hint");
    }
}
