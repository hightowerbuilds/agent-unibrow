//! Just enough MIME to write plain-text mail and read Gmail payloads.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use serde_json::Value;

pub struct Outgoing<'a> {
    pub to: &'a [String],
    pub cc: &'a [String],
    pub subject: &'a str,
    pub body: &'a str,
    pub in_reply_to: Option<&'a str>,
    pub references: Option<&'a str>,
}

/// RFC 5322 message, base64url-encoded for Gmail's `raw` field.
pub fn build_raw(m: &Outgoing<'_>) -> String {
    let mut out = String::new();
    out.push_str(&format!("To: {}\r\n", sanitize_header(&m.to.join(", "))));
    if !m.cc.is_empty() {
        out.push_str(&format!("Cc: {}\r\n", sanitize_header(&m.cc.join(", "))));
    }
    out.push_str(&format!("Subject: {}\r\n", encode_header(&sanitize_header(m.subject))));
    if let Some(id) = m.in_reply_to {
        out.push_str(&format!("In-Reply-To: {}\r\n", sanitize_header(id)));
    }
    if let Some(r) = m.references {
        out.push_str(&format!("References: {}\r\n", sanitize_header(r)));
    }
    out.push_str("MIME-Version: 1.0\r\n");
    out.push_str("Content-Type: text/plain; charset=\"UTF-8\"\r\n");
    out.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
    let body = m.body.replace("\r\n", "\n").replace('\n', "\r\n");
    let encoded = STANDARD.encode(body.as_bytes());
    for chunk in encoded.as_bytes().chunks(76) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ascii"));
        out.push_str("\r\n");
    }
    URL_SAFE.encode(out.as_bytes())
}

/// Header values can't contain line breaks (header injection).
fn sanitize_header(s: &str) -> String {
    s.replace(['\r', '\n'], " ")
}

/// RFC 2047 encoded-word for non-ASCII header text.
fn encode_header(s: &str) -> String {
    if s.is_ascii() { s.to_string() } else { format!("=?UTF-8?B?{}?=", STANDARD.encode(s.as_bytes())) }
}

pub fn header<'a>(payload: &'a Value, name: &str) -> Option<&'a str> {
    payload["headers"]
        .as_array()?
        .iter()
        .find(|h| h["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(name)))
        .and_then(|h| h["value"].as_str())
}

/// Best-effort readable body: the first text/plain part, else stripped HTML.
pub fn body_text(payload: &Value) -> String {
    if let Some(t) = find_part(payload, "text/plain") {
        return t;
    }
    if let Some(h) = find_part(payload, "text/html") {
        return strip_html(&h);
    }
    String::new()
}

fn find_part(part: &Value, mime: &str) -> Option<String> {
    if part["mimeType"].as_str() == Some(mime) {
        if let Some(data) = part["body"]["data"].as_str() {
            return decode_b64url(data);
        }
    }
    part["parts"].as_array()?.iter().find_map(|p| find_part(p, mime))
}

fn decode_b64url(data: &str) -> Option<String> {
    let trimmed = data.trim_end_matches('=');
    let bytes = URL_SAFE_NO_PAD.decode(trimmed).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut tag = String::new();
    let mut skip_until: Option<&str> = None;
    for c in html.chars() {
        if in_tag {
            if c == '>' {
                in_tag = false;
                let t = tag.trim().to_lowercase();
                let name = t.trim_start_matches('/').split_whitespace().next().unwrap_or("").to_string();
                if let Some(end) = skip_until {
                    if t.starts_with('/') && name == end {
                        skip_until = None;
                    }
                } else if name == "style" || name == "script" {
                    skip_until = Some(if name == "style" { "style" } else { "script" });
                } else if matches!(name.as_str(), "br" | "p" | "div" | "tr" | "li" | "h1" | "h2" | "h3") {
                    out.push('\n');
                }
                tag.clear();
            } else {
                tag.push(c);
            }
        } else if c == '<' {
            in_tag = true;
        } else if skip_until.is_none() {
            out.push(c);
        }
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    // Collapse runs of blank lines left by layout tables.
    let mut collapsed = String::new();
    let mut blank = 0;
    for line in out.lines() {
        let l = line.trim();
        if l.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        collapsed.push_str(l);
        collapsed.push('\n');
    }
    collapsed.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_threaded_reply() {
        let to = vec!["zack@example.com".to_string()];
        let raw = build_raw(&Outgoing {
            to: &to,
            cc: &[],
            subject: "Re: Shingles — Tuesday",
            body: "Sounds good.\nSee you then.",
            in_reply_to: Some("<abc@mail>"),
            references: Some("<root@mail> <abc@mail>"),
        });
        let decoded = String::from_utf8(URL_SAFE.decode(raw).unwrap()).unwrap();
        assert!(decoded.contains("To: zack@example.com\r\n"));
        assert!(decoded.contains("Subject: =?UTF-8?B?"));
        assert!(decoded.contains("In-Reply-To: <abc@mail>\r\n"));
        let body_b64 = decoded.split("\r\n\r\n").nth(1).unwrap().replace("\r\n", "");
        assert_eq!(String::from_utf8(STANDARD.decode(body_b64).unwrap()).unwrap(), "Sounds good.\r\nSee you then.");
    }

    #[test]
    fn header_injection_is_flattened() {
        let to = vec!["a@b.com\r\nBcc: evil@x.com".to_string()];
        let raw = build_raw(&Outgoing { to: &to, cc: &[], subject: "hi", body: "", in_reply_to: None, references: None });
        let decoded = String::from_utf8(URL_SAFE.decode(raw).unwrap()).unwrap();
        assert!(!decoded.contains("\r\nBcc:"));
    }

    #[test]
    fn strips_html() {
        let t = strip_html("<style>p{}</style><p>Hi&nbsp;Luke</p><p>Roof <b>done</b></p>");
        assert_eq!(t, "Hi Luke\n\nRoof done");
    }
}
