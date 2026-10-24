//! Gmail API client and the tools the agent calls.

use crate::google_auth::GoogleAuth;
use crate::mime::{self, Outgoing};
use futures::future::BoxFuture;
use router::{Effect, Tool, ToolOutput, ToolResult};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::OnceCell;

const API: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
/// Per-message body cap when reading a thread, so one newsletter can't
/// flood the context window.
const BODY_CHARS: usize = 6000;

pub struct Gmail {
    auth: Arc<GoogleAuth>,
    http: reqwest::Client,
    me: OnceCell<String>,
}

impl Gmail {
    pub fn new(auth: Arc<GoogleAuth>) -> Self {
        Self { auth, http: reqwest::Client::new(), me: OnceCell::new() }
    }

    pub(crate) async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, String> {
        let token = self.auth.access_token().await?;
        let resp = self
            .http
            .get(format!("{API}{path}"))
            .bearer_auth(token)
            .query(query)
            .send()
            .await
            .map_err(|e| format!("Gmail request failed: {e}"))?;
        decode(resp).await
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        let token = self.auth.access_token().await?;
        let resp = self
            .http
            .post(format!("{API}{path}"))
            .bearer_auth(token)
            .json(body)
            .send()
            .await
            .map_err(|e| format!("Gmail request failed: {e}"))?;
        decode(resp).await
    }

    async fn my_address(&self) -> Result<String, String> {
        self.me
            .get_or_try_init(|| async {
                let p = self.get("/profile", &[]).await?;
                p["emailAddress"].as_str().map(|s| s.to_lowercase()).ok_or_else(|| "no address in profile".to_string())
            })
            .await
            .cloned()
    }

    async fn search(&self, query: &str, max: u32) -> Result<ToolOutput, String> {
        let me = self.my_address().await?;
        let max_s = max.clamp(1, 25).to_string();
        let list = self.get("/threads", &[("q", query), ("maxResults", &max_s)]).await?;
        let threads = list["threads"].as_array().cloned().unwrap_or_default();
        if threads.is_empty() {
            return Ok(ToolOutput { content: format!("No threads match `{query}`."), summary: "0 threads".into(), ..Default::default() });
        }
        let mut lines = vec![format!("{} thread(s) for `{query}` (newest first). Your address: {me}", threads.len())];
        for t in &threads {
            let id = t["id"].as_str().unwrap_or_default();
            let detail = self
                .get(
                    &format!("/threads/{id}"),
                    &[("format", "metadata"), ("metadataHeaders", "From"), ("metadataHeaders", "Subject"), ("metadataHeaders", "Date")],
                )
                .await?;
            let msgs = detail["messages"].as_array().cloned().unwrap_or_default();
            let (Some(first), Some(last)) = (msgs.first(), msgs.last()) else { continue };
            let subject = mime::header(&first["payload"], "Subject").unwrap_or("(no subject)");
            let last_from = mime::header(&last["payload"], "From").unwrap_or("?");
            let last_date = mime::header(&last["payload"], "Date").unwrap_or("?");
            let last_is_me = is_me(last_from, &me);
            let unread = last["labelIds"].as_array().is_some_and(|l| l.iter().any(|x| x == "UNREAD"));
            lines.push(format!(
                "- thread_id={id} | subject: {subject} | {n} msg | last from: {last_from} | {last_date}{who}{unread} | snippet: {snippet}",
                n = msgs.len(),
                who = if last_is_me { " | last message is yours" } else { "" },
                unread = if unread { " | unread" } else { "" },
                snippet = last["snippet"].as_str().unwrap_or("").replace('\n', " "),
            ));
        }
        Ok(ToolOutput { summary: format!("{} threads for `{query}`", threads.len()), content: lines.join("\n"), ..Default::default() })
    }

    async fn read_thread(&self, id: &str) -> Result<ToolOutput, String> {
        let t = self.get(&format!("/threads/{id}"), &[("format", "full")]).await?;
        let msgs = t["messages"].as_array().cloned().unwrap_or_default();
        let subject = msgs.first().and_then(|m| mime::header(&m["payload"], "Subject")).unwrap_or("(no subject)").to_string();
        let mut out = vec![format!("Thread {id}: {subject} ({} messages)", msgs.len())];
        for (i, m) in msgs.iter().enumerate() {
            let p = &m["payload"];
            let mut body = mime::body_text(p);
            if body.chars().count() > BODY_CHARS {
                body = body.chars().take(BODY_CHARS).collect::<String>() + "\n[…truncated]";
            }
            out.push(format!(
                "\n--- message {} of {} ---\nFrom: {}\nTo: {}\nCc: {}\nDate: {}\n\n{}",
                i + 1,
                msgs.len(),
                mime::header(p, "From").unwrap_or(""),
                mime::header(p, "To").unwrap_or(""),
                mime::header(p, "Cc").unwrap_or(""),
                mime::header(p, "Date").unwrap_or(""),
                body.trim()
            ));
        }
        Ok(ToolOutput { summary: format!("read “{subject}” ({} msgs)", msgs.len()), content: out.join("\n"), ..Default::default() })
    }

    /// Threading headers and default recipients for a reply to `thread_id`.
    async fn reply_context(&self, thread_id: &str) -> Result<ReplyContext, String> {
        let me = self.my_address().await?;
        let t = self
            .get(
                &format!("/threads/{thread_id}"),
                &[
                    ("format", "metadata"),
                    ("metadataHeaders", "From"),
                    ("metadataHeaders", "To"),
                    ("metadataHeaders", "Cc"),
                    ("metadataHeaders", "Reply-To"),
                    ("metadataHeaders", "Subject"),
                    ("metadataHeaders", "Message-ID"),
                    ("metadataHeaders", "References"),
                ],
            )
            .await?;
        let msgs = t["messages"].as_array().cloned().unwrap_or_default();
        let last = msgs.last().ok_or("thread has no messages")?;
        let p = &last["payload"];
        let (to, cc) = reply_recipients(
            &me,
            mime::header(p, "From").unwrap_or(""),
            mime::header(p, "Reply-To"),
            mime::header(p, "To").unwrap_or(""),
            mime::header(p, "Cc").unwrap_or(""),
        );
        let subject = mime::header(p, "Subject").unwrap_or("").to_string();
        let subject = if subject.to_lowercase().starts_with("re:") { subject } else { format!("Re: {subject}") };
        let message_id = mime::header(p, "Message-ID").map(str::to_string);
        let references = match (mime::header(p, "References"), &message_id) {
            (Some(r), Some(id)) => Some(format!("{r} {id}")),
            (None, Some(id)) => Some(id.clone()),
            _ => None,
        };
        Ok(ReplyContext { to, cc, subject, message_id, references })
    }

    async fn create_reply_draft(&self, a: ReplyDraftArgs) -> Result<ToolOutput, String> {
        let ctx = self.reply_context(&a.thread_id).await?;
        let cc = if a.reply_all.unwrap_or(false) { ctx.cc.clone() } else { Vec::new() };
        let raw = mime::build_raw(&Outgoing {
            to: &ctx.to,
            cc: &cc,
            subject: &ctx.subject,
            body: &a.body,
            in_reply_to: ctx.message_id.as_deref(),
            references: ctx.references.as_deref(),
        });
        let d = self.post("/drafts", &json!({"message": {"raw": raw, "threadId": a.thread_id}})).await?;
        let draft_id = d["id"].as_str().unwrap_or("?");
        Ok(ToolOutput {
            summary: format!("draft reply to {} — {}", ctx.to.join(", "), ctx.subject),
            content: format!(
                "Draft saved (draft_id={draft_id}) in thread {}. To: {}{}. Subject: {}. It is in the user's Drafts folder and has NOT been sent.",
                a.thread_id,
                ctx.to.join(", "),
                if cc.is_empty() { String::new() } else { format!(" Cc: {}", cc.join(", ")) },
                ctx.subject
            ), ..Default::default()
        })
    }

    async fn create_draft(&self, a: NewDraftArgs) -> Result<ToolOutput, String> {
        let cc = a.cc.unwrap_or_default();
        check_recipients(&a.to, &cc)?;
        let raw = mime::build_raw(&Outgoing {
            to: &a.to,
            cc: &cc,
            subject: &a.subject,
            body: &a.body,
            in_reply_to: None,
            references: None,
        });
        let d = self.post("/drafts", &json!({"message": {"raw": raw}})).await?;
        Ok(ToolOutput {
            summary: format!("draft to {} — {}", a.to.join(", "), a.subject),
            content: format!(
                "Draft saved (draft_id={}). It is in the user's Drafts folder and has NOT been sent.",
                d["id"].as_str().unwrap_or("?")
            ), ..Default::default()
        })
    }

    async fn send(&self, a: SendArgs) -> Result<ToolOutput, String> {
        let cc = a.cc.unwrap_or_default();
        check_recipients(&a.to, &cc)?;
        let (subject, in_reply_to, references) = match &a.thread_id {
            Some(tid) => {
                let ctx = self.reply_context(tid).await?;
                (a.subject.clone().unwrap_or(ctx.subject), ctx.message_id, ctx.references)
            }
            None => (a.subject.clone().ok_or("subject is required for a new email")?, None, None),
        };
        let raw = mime::build_raw(&Outgoing {
            to: &a.to,
            cc: &cc,
            subject: &subject,
            body: &a.body,
            in_reply_to: in_reply_to.as_deref(),
            references: references.as_deref(),
        });
        let mut msg = json!({"raw": raw});
        if let Some(tid) = &a.thread_id {
            msg["threadId"] = json!(tid);
        }
        let sent = self.post("/messages/send", &msg).await?;
        Ok(ToolOutput {
            summary: format!("sent to {} — {subject}", a.to.join(", ")),
            content: format!("Sent (message_id={}).", sent["id"].as_str().unwrap_or("?")), ..Default::default()
        })
    }
}

struct ReplyContext {
    to: Vec<String>,
    cc: Vec<String>,
    subject: String,
    message_id: Option<String>,
    references: Option<String>,
}

async fn decode(resp: reqwest::Response) -> Result<Value, String> {
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        let msg = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
            .unwrap_or(text);
        return Err(format!("Gmail API {}: {msg}", status.as_u16()));
    }
    serde_json::from_str(&text).map_err(|e| format!("bad Gmail response: {e}"))
}

/// Splits an address header on commas outside quotes.
fn split_addresses(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            ',' if !quoted => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// The bare address of a `to`/`cc` entry that is exactly one address,
/// `user@host` or `Name <user@host>` (the name may be quoted). Anything
/// else (lists, groups, comments, empty `<>`) is `None`, so what the
/// approval check sees is always what goes in the header.
fn single_address(entry: &str) -> Option<String> {
    let s = entry.trim();
    let (name, addr) = match s.strip_suffix('>') {
        Some(rest) => {
            let a = rest.rfind('<')?;
            (rest[..a].trim(), &rest[a + 1..])
        }
        None => ("", s),
    };
    let name_ok = match name.strip_prefix('"').and_then(|n| n.strip_suffix('"')) {
        Some(inner) => !inner.contains(['"', '\\']),
        None => !name.contains([',', ';', '<', '>', '@', '"', ':', '(', ')', '\\']),
    };
    let (local, domain) = addr.split_once('@')?;
    let addr_ok = !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains('@')
        && !addr.chars().any(|c| c.is_whitespace() || c.is_control() || ",;<>\"()[]\\:".contains(c));
    (name_ok && addr_ok).then(|| addr.to_lowercase())
}

/// The approval check's view of a send's `to` and `cc`. An entry that isn't
/// one address is kept whole, so it can never match an always-allowed one.
fn recipients(input: &Value) -> Vec<String> {
    ["to", "cc"]
        .iter()
        .flat_map(|k| input[*k].as_array().cloned().unwrap_or_default())
        .filter_map(|v| v.as_str().map(|e| single_address(e).unwrap_or_else(|| format!("(not one address) {e}"))))
        .collect()
}

/// Refuses `to`/`cc` entries that aren't one address each.
fn check_recipients(to: &[String], cc: &[String]) -> Result<(), String> {
    match to.iter().chain(cc).find(|e| single_address(e).is_none()) {
        Some(bad) => Err(format!(
            "Each to/cc entry must be a single address, like \"user@host\" or \"Name <user@host>\"; \
             put each person in their own entry. Not valid: {bad:?}"
        )),
        None => Ok(()),
    }
}

/// The address in one entry of a header someone else wrote, for comparing:
/// lowercased, the part in `<…>` when there is one. Looser than
/// [`single_address`], which is for what the agent sends.
fn entry_address(entry: &str) -> String {
    single_address(entry).unwrap_or_else(|| {
        let s = entry.trim();
        match (s.rfind('<'), s.rfind('>')) {
            (Some(a), Some(b)) if b > a => s[a + 1..b].trim().to_lowercase(),
            _ => s.to_lowercase(),
        }
    })
}

/// Whether an address header names the user's address `me` exactly
/// (`ryan@x.com` is not `an@x.com`).
fn is_me(header: &str, me: &str) -> bool {
    split_addresses(header).iter().any(|e| entry_address(e) == me)
}

/// Default `to` and `cc` of a reply to a message with these headers.
/// Replying to someone else: to the sender (or their Reply-To), Cc the
/// message's other To and Cc recipients. Following up on the user's own
/// message: to its To, Cc its Cc. The user is left out (unless they wrote
/// only to themselves), and so is anyone already listed.
fn reply_recipients(me: &str, from: &str, reply_to: Option<&str>, to: &str, cc: &str) -> (Vec<String>, Vec<String>) {
    let mine = is_me(from, me);
    let (to_list, others) = if mine {
        (split_addresses(to), split_addresses(cc))
    } else {
        let sender = reply_to.filter(|r| !r.trim().is_empty()).unwrap_or(from);
        (split_addresses(sender), split_addresses(to).into_iter().chain(split_addresses(cc)).collect())
    };
    let mut seen = std::collections::HashSet::new();
    let mut to_out: Vec<String> =
        to_list.iter().filter(|e| entry_address(e) != me && seen.insert(entry_address(e))).cloned().collect();
    if to_out.is_empty() {
        // A note to self.
        to_out = to_list.into_iter().filter(|e| seen.insert(entry_address(e))).collect();
    }
    let cc_out = others.into_iter().filter(|e| entry_address(e) != me && seen.insert(entry_address(e))).collect();
    (to_out, cc_out)
}

// ---------------------------------------------------------------- tool args

/// Parses and validates tool input. Typed deserialization is the schema check.
fn args<T: for<'de> Deserialize<'de>>(input: Value) -> Result<T, String> {
    serde_json::from_value(input).map_err(|e| format!("Invalid input: {e}"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    max_results: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    thread_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyDraftArgs {
    thread_id: String,
    body: String,
    reply_all: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewDraftArgs {
    to: Vec<String>,
    cc: Option<Vec<String>>,
    subject: String,
    body: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    to: Vec<String>,
    cc: Option<Vec<String>>,
    subject: Option<String>,
    body: String,
    thread_id: Option<String>,
}

// -------------------------------------------------------------------- tools

macro_rules! gmail_tool {
    ($ty:ident) => {
        pub struct $ty(Arc<Gmail>);
    };
}

gmail_tool!(SearchThreads);
gmail_tool!(ReadThread);
gmail_tool!(CreateReplyDraft);
gmail_tool!(CreateDraft);
gmail_tool!(SendEmail);

pub fn tools(g: Arc<Gmail>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(SearchThreads(g.clone())),
        Arc::new(ReadThread(g.clone())),
        Arc::new(CreateReplyDraft(g.clone())),
        Arc::new(CreateDraft(g.clone())),
        Arc::new(SendEmail(g)),
    ]
}

impl Tool for SearchThreads {
    fn name(&self) -> &'static str {
        "gmail_search_threads"
    }
    fn description(&self) -> &'static str {
        "Search the user's Gmail and list matching threads with subject, last sender, date, unread state, \
         whether the last message is the user's own, and a snippet. Call this first whenever the user asks \
         about their email, to find threads before reading them. Uses Gmail search syntax, e.g. \
         `in:inbox newer_than:7d -category:promotions -category:social`, `from:zack`, `is:unread`."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Gmail search query."},
                "max_results": {"type": "integer", "description": "Threads to return, 1-25. Default 10."}
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Search Gmail: {}", input["query"].as_str().unwrap_or(""))
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let g = self.0.clone();
        Box::pin(async move {
            let a: SearchArgs = args(input)?;
            g.search(&a.query, a.max_results.unwrap_or(10)).await
        })
    }
}

impl Tool for ReadThread {
    fn name(&self) -> &'static str {
        "gmail_read_thread"
    }
    fn description(&self) -> &'static str {
        "Read every message in one Gmail thread: sender, recipients, date and body text. Call this before \
         judging whether a thread needs a reply or before drafting one, so the reply is grounded in what \
         was actually said."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"thread_id": {"type": "string", "description": "thread_id from gmail_search_threads."}},
            "required": ["thread_id"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Read thread {}", input["thread_id"].as_str().unwrap_or(""))
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let g = self.0.clone();
        Box::pin(async move {
            let a: ReadArgs = args(input)?;
            g.read_thread(&a.thread_id).await
        })
    }
}

impl Tool for CreateReplyDraft {
    fn name(&self) -> &'static str {
        "gmail_create_reply_draft"
    }
    fn description(&self) -> &'static str {
        "Save a reply to an existing thread as a Gmail draft, without sending it. Recipients, subject and \
         threading headers are filled in from the thread, so only write the body. Use this whenever the user \
         asks for a draft or reply and has not explicitly said to send it."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "thread_id": {"type": "string"},
                "body": {"type": "string", "description": "Plain-text reply body. No quoted history, no signature unless the user has one."},
                "reply_all": {"type": "boolean", "description": "Also Cc the other people on the last message. Default false."}
            },
            "required": ["thread_id", "body"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Stage
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Draft reply in thread {}", input["thread_id"].as_str().unwrap_or(""))
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let g = self.0.clone();
        Box::pin(async move { g.create_reply_draft(args(input)?).await })
    }
}

impl Tool for CreateDraft {
    fn name(&self) -> &'static str {
        "gmail_create_draft"
    }
    fn description(&self) -> &'static str {
        "Save a new email (not a reply) as a Gmail draft, without sending it. Use when the user asks you to \
         write or prepare an email to someone."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                "cc": {"type": "array", "items": {"type": "string"}},
                "subject": {"type": "string"},
                "body": {"type": "string"}
            },
            "required": ["to", "subject", "body"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Stage
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Draft to {}", join(&input["to"]))
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let g = self.0.clone();
        Box::pin(async move { g.create_draft(args(input)?).await })
    }
}

impl Tool for SendEmail {
    fn name(&self) -> &'static str {
        "gmail_send_email"
    }
    fn description(&self) -> &'static str {
        "Send an email immediately from the user's Gmail. Only call this when the user has explicitly asked \
         you to send (not draft). The user is asked to approve each send unless they've pre-approved the \
         recipients. Pass thread_id to send as a reply in that thread."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                "cc": {"type": "array", "items": {"type": "string"}},
                "subject": {"type": "string", "description": "Required for a new email; defaults to the thread's subject for replies."},
                "body": {"type": "string"},
                "thread_id": {"type": "string", "description": "Send as a reply in this thread."}
            },
            "required": ["to", "body"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Commit
    }
    fn counterparties(&self, input: &Value) -> Vec<String> {
        recipients(input)
    }
    fn describe_call(&self, input: &Value) -> String {
        let body = input["body"].as_str().unwrap_or("");
        let preview: String = body.chars().take(400).collect();
        format!(
            "Send email\nTo: {}{}\nSubject: {}\n\n{}{}",
            join(&input["to"]),
            if input["cc"].is_array() { format!("\nCc: {}", join(&input["cc"])) } else { String::new() },
            input["subject"].as_str().unwrap_or("(reply in thread)"),
            preview,
            if body.chars().count() > 400 { "…" } else { "" }
        )
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let g = self.0.clone();
        Box::pin(async move { g.send(args(input)?).await })
    }
}

fn join(v: &Value) -> String {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}

/// Current local date and time. Needs no account.
pub struct CurrentTime;

impl Tool for CurrentTime {
    fn name(&self) -> &'static str {
        "current_time"
    }
    fn description(&self) -> &'static str {
        "Get the current local date, time, weekday and timezone. Call this when you need to resolve a \
         relative date like \"this week\" or \"tomorrow\" and the message doesn't already say the time."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, _: &Value) -> String {
        "Check the time".into()
    }
    fn call(&self, _: Value) -> BoxFuture<'static, ToolResult> {
        Box::pin(async {
            let now = chrono::Local::now();
            let s = now.format("%A, %B %-d, %Y %-I:%M %p (UTC%:z)").to_string();
            Ok(ToolOutput { summary: s.clone(), content: s, ..Default::default() })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_quoted_names() {
        let v = split_addresses("\"Hightower, Luke\" <luke@x.com>, zack@y.com");
        assert_eq!(v, vec!["\"Hightower, Luke\" <luke@x.com>", "zack@y.com"]);
        assert_eq!(single_address(&v[0]).as_deref(), Some("luke@x.com"));
    }

    #[test]
    fn recognises_the_user_exactly() {
        let me = "an@gmail.com";
        assert!(is_me("an@gmail.com", me));
        assert!(is_me("An <AN@gmail.com>", me));
        assert!(is_me("\"Hightower, An\" <an@gmail.com>", me));
        assert!(!is_me("ryan@gmail.com", me));
        assert!(!is_me("Ryan <ryan@gmail.com>", me));
        assert!(!is_me("an@gmail.com.evil.net", me));
        assert!(!is_me("", me));
    }

    #[test]
    fn reply_all_keeps_everyone_but_the_user() {
        let me = "me@x.com";
        // From someone else: to the sender, Cc the rest of To and Cc.
        let (to, cc) = reply_recipients(
            me,
            "Ann <ann@x.com>",
            None,
            "Me <me@x.com>, Bob <bob@x.com>, ann@x.com",
            "Cat <cat@x.com>, BOB@x.com, ryan@x.com",
        );
        assert_eq!(to, vec!["Ann <ann@x.com>"]);
        assert_eq!(cc, vec!["Bob <bob@x.com>", "Cat <cat@x.com>", "ryan@x.com"]);
        // Reply-To wins over From.
        let (to, cc) = reply_recipients(me, "ann@x.com", Some("list@x.com"), "me@x.com", "");
        assert_eq!(to, vec!["list@x.com"]);
        assert!(cc.is_empty());
        // Following up on the user's own message: its To and Cc.
        let (to, cc) = reply_recipients(me, "Me <me@x.com>", None, "bob@x.com, me@x.com", "cat@x.com, bob@x.com");
        assert_eq!(to, vec!["bob@x.com"]);
        assert_eq!(cc, vec!["cat@x.com"]);
        // A note to self stays one.
        let (to, cc) = reply_recipients(me, "me@x.com", None, "Me <me@x.com>", "");
        assert_eq!(to, vec!["Me <me@x.com>"]);
        assert!(cc.is_empty());
        // `an@` is not the user `ryan@` sent to.
        let (to, _) = reply_recipients("an@x.com", "ryan@x.com", None, "an@x.com", "");
        assert_eq!(to, vec!["ryan@x.com"]);
    }

    #[test]
    fn one_address_per_entry() {
        assert_eq!(single_address("Bob <Bob@Y.com>").as_deref(), Some("bob@y.com"));
        assert_eq!(single_address("\"Hightower, Luke\" <luke@x.com>").as_deref(), Some("luke@x.com"));
        assert_eq!(single_address(" zack@y.co.uk ").as_deref(), Some("zack@y.co.uk"));
        for bad in [
            "evil@x.com, Bob <bob@y.com>",
            "<>, evil@x.com",
            "Bob <bob@y.com>, evil@x.com",
            "a@b.com b@c.com",
            "undisclosed:;",
            "evil@x.com <bob@y.com>",
            "Bob <bob@y.com> (evil@x.com)",
            "Bob <bob@y.com,evil@x.com>",
            "\"a\"b\" <bob@y.com>",
            "",
            "bob",
        ] {
            assert!(single_address(bad).is_none(), "{bad:?} should be refused");
        }
        // An entry that hides a second address is never just the allowed one.
        let parties = recipients(&json!({"to": ["evil@x.com, Bob <bob@y.com>"]}));
        assert_eq!(parties.len(), 1);
        assert_ne!(parties[0], "bob@y.com");
    }

    #[test]
    fn rejects_unknown_fields() {
        let r: Result<SearchArgs, _> = args(json!({"query": "x", "bcc": "evil"}));
        assert!(r.is_err());
    }
}
