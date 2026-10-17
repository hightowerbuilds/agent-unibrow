//! Raw-HTTP client for `POST /v1/messages` with streaming.
//!
//! Rust has no official Anthropic SDK, so this speaks the wire format
//! directly. Content blocks are rebuilt from stream events exactly as the
//! server sent them (including thinking signatures) because the history is
//! echoed back unchanged on the next request.

use crate::sse::SseDecoder;
use futures::StreamExt;
use serde_json::{Map, Value, json};
use std::time::Duration;

pub const API_URL: &str = "https://api.anthropic.com/v1/messages";
pub const API_VERSION: &str = "2023-06-01";
/// Server-side refusal fallback (scalar `"default"` form) and progress-note
/// summaries for thinking blocks.
pub const BETAS: &str = "server-side-fallback-2026-07-01,thinking-display-updates-2026-08-18";

#[derive(Clone, Debug)]
pub struct ModelConfig {
    pub model: String,
    pub effort: String,
    pub max_tokens: u32,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self { model: "claude-opus-5-5".into(), effort: "high".into(), max_tokens: 64_000 }
    }
}

/// A completed assistant message.
#[derive(Debug)]
pub struct Completed {
    pub content: Vec<Value>,
    pub stop_reason: Option<String>,
    pub stop_details: Option<Value>,
    pub model: Option<String>,
    pub usage: Value,
}

/// Things worth showing while the stream is still open.
pub enum Live<'a> {
    Text(&'a str),
    /// A progress note (non-empty thinking text under `display: "updates"`).
    Note(String),
}

#[derive(Debug)]
pub enum ApiError {
    /// Retrying won't help (bad request, auth, permission).
    Fatal(String),
    /// Rate limit, overload, server error, or network failure.
    Retryable(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Fatal(m) | ApiError::Retryable(m) => f.write_str(m),
        }
    }
}

pub struct Client {
    http: reqwest::Client,
    api_key: String,
    pub config: ModelConfig,
}

impl Client {
    pub fn new(api_key: String, config: ModelConfig) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            // Long agentic turns can run for minutes; the stream keeps the
            // connection busy, so only guard against a dead socket.
            .read_timeout(Duration::from_secs(300))
            .build()
            .expect("http client");
        Self { http, api_key, config }
    }

    fn body(&self, system: &str, tools: &[Value], messages: &[Value]) -> Value {
        json!({
            "model": self.config.model,
            "max_tokens": self.config.max_tokens,
            "stream": true,
            "system": system,
            "thinking": {"type": "adaptive", "display": "updates"},
            "output_config": {"effort": self.config.effort},
            "fallbacks": "default",
            "cache_control": {"type": "ephemeral"},
            "tools": tools,
            "messages": messages,
        })
    }

    /// Sends one request, retrying transient failures that happen before
    /// the stream opens.
    pub async fn stream(
        &self,
        system: &str,
        tools: &[Value],
        messages: &[Value],
        mut on_live: impl FnMut(Live<'_>),
    ) -> Result<Completed, ApiError> {
        let body = self.body(system, tools, messages);
        let mut attempt = 0u32;
        let response = loop {
            attempt += 1;
            match self.open(&body).await {
                Ok(r) => break r,
                Err(ApiError::Retryable(m)) if attempt < 4 => {
                    let wait = Duration::from_millis(1000 * 2u64.pow(attempt - 1));
                    eprintln!("agent-core: retrying in {wait:?} after: {m}");
                    tokio::time::sleep(wait).await;
                }
                Err(e) => return Err(e),
            }
        };
        read_stream(response, &mut on_live).await
    }

    async fn open(&self, body: &Value) -> Result<reqwest::Response, ApiError> {
        let resp = self
            .http
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("anthropic-beta", BETAS)
            .header("content-type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| ApiError::Retryable(format!("network error: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let text = resp.text().await.unwrap_or_default();
        let message = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
            .unwrap_or(text);
        let msg = format!("API {}: {message}", status.as_u16());
        if status.as_u16() == 429 || status.as_u16() == 529 || status.is_server_error() {
            Err(ApiError::Retryable(msg))
        } else {
            Err(ApiError::Fatal(msg))
        }
    }
}

async fn read_stream(
    response: reqwest::Response,
    on_live: &mut impl FnMut(Live<'_>),
) -> Result<Completed, ApiError> {
    let mut decoder = SseDecoder::default();
    let mut blocks: Vec<Value> = Vec::new();
    // Accumulated `input_json_delta` fragments, per block index.
    let mut partial_json: Vec<Option<String>> = Vec::new();
    let mut stop_reason = None;
    let mut stop_details = None;
    let mut model = None;
    let mut usage = Value::Object(Map::new());
    let mut finished = false;

    let mut bytes = response.bytes_stream();
    while let Some(chunk) = bytes.next().await {
        let chunk = chunk.map_err(|e| ApiError::Retryable(format!("stream interrupted: {e}")))?;
        for ev in decoder.push(&chunk) {
            if ev.data.is_empty() {
                continue;
            }
            let data: Value = serde_json::from_str(&ev.data)
                .map_err(|e| ApiError::Fatal(format!("bad event JSON ({e}): {}", ev.data)))?;
            match data["type"].as_str().unwrap_or(&ev.event) {
                "message_start" => {
                    model = data["message"]["model"].as_str().map(str::to_string);
                    merge(&mut usage, &data["message"]["usage"]);
                }
                "content_block_start" => {
                    let index = data["index"].as_u64().unwrap_or(0) as usize;
                    let block = data["content_block"].clone();
                    if blocks.len() <= index {
                        blocks.resize(index + 1, Value::Null);
                        partial_json.resize(index + 1, None);
                    }
                    blocks[index] = block;
                }
                "content_block_delta" => {
                    let index = data["index"].as_u64().unwrap_or(0) as usize;
                    let Some(block) = blocks.get_mut(index) else { continue };
                    let delta = &data["delta"];
                    match delta["type"].as_str() {
                        Some("text_delta") => {
                            let t = delta["text"].as_str().unwrap_or("");
                            append(block, "text", t);
                            on_live(Live::Text(t));
                        }
                        Some("thinking_delta") => append(block, "thinking", delta["thinking"].as_str().unwrap_or("")),
                        Some("signature_delta") => block["signature"] = delta["signature"].clone(),
                        Some("input_json_delta") => partial_json[index]
                            .get_or_insert_with(String::new)
                            .push_str(delta["partial_json"].as_str().unwrap_or("")),
                        Some("citations_delta") => {
                            if !block["citations"].is_array() {
                                block["citations"] = json!([]);
                            }
                            if let Some(a) = block["citations"].as_array_mut() {
                                a.push(delta["citation"].clone());
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    let index = data["index"].as_u64().unwrap_or(0) as usize;
                    let Some(block) = blocks.get_mut(index) else { continue };
                    if let Some(raw) = partial_json.get_mut(index).and_then(Option::take) {
                        // Strict parse. A failure is recorded on the block so
                        // the loop can answer with an INVALID_JSON error.
                        match serde_json::from_str::<Value>(if raw.trim().is_empty() { "{}" } else { &raw }) {
                            Ok(v) => block["input"] = v,
                            Err(_) => {
                                block["input"] = json!({});
                                block["__invalid_json"] = Value::String(raw);
                            }
                        }
                    }
                    if block["type"] == "thinking" {
                        if let Some(t) = block["thinking"].as_str().filter(|t| !t.trim().is_empty()) {
                            on_live(Live::Note(t.trim().to_string()));
                        }
                    }
                }
                "message_delta" => {
                    if let Some(r) = data["delta"]["stop_reason"].as_str() {
                        stop_reason = Some(r.to_string());
                    }
                    if !data["delta"]["stop_details"].is_null() {
                        stop_details = Some(data["delta"]["stop_details"].clone());
                    }
                    merge(&mut usage, &data["usage"]);
                }
                "message_stop" => finished = true,
                "error" => {
                    let m = data["error"]["message"].as_str().unwrap_or("unknown stream error");
                    let kind = data["error"]["type"].as_str().unwrap_or("");
                    let msg = format!("stream error ({kind}): {m}");
                    return Err(if kind == "overloaded_error" || kind == "api_error" {
                        ApiError::Retryable(msg)
                    } else {
                        ApiError::Fatal(msg)
                    });
                }
                _ => {} // ping and anything new
            }
        }
    }
    if !finished {
        return Err(ApiError::Retryable("stream ended before message_stop".into()));
    }
    blocks.retain(|b| !b.is_null());
    Ok(Completed { content: blocks, stop_reason, stop_details, model, usage })
}

fn append(block: &mut Value, field: &str, s: &str) {
    let cur = block[field].as_str().unwrap_or("").to_string();
    block[field] = Value::String(cur + s);
}

fn merge(into: &mut Value, from: &Value) {
    if let (Some(a), Some(b)) = (into.as_object_mut(), from.as_object()) {
        for (k, v) in b {
            if !v.is_null() {
                a.insert(k.clone(), v.clone());
            }
        }
    }
}
