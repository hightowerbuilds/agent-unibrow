//! A minimal MCP server (Streamable HTTP, JSON responses) on 127.0.0.1.
//!
//! The agent CLIs connect here to get the app's tools. Every request must
//! carry the per-launch bearer token, so other local processes can't call
//! the user's Gmail through it.

use crate::host::ToolHost;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use rand::Rng;
use rand::distr::Alphanumeric;
use serde_json::{Value, json};

pub const SERVER_NAME: &str = "unibrow";
const DEFAULT_PROTOCOL: &str = "2025-06-18";

pub struct McpServer {
    pub url: String,
    pub token: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for McpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone)]
struct AppState {
    host: ToolHost,
    token: String,
}

pub async fn start(host: ToolHost) -> std::io::Result<McpServer> {
    let token: String = rand::rng().sample_iter(&Alphanumeric).take(40).map(char::from).collect();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let state = AppState { host, token: token.clone() };
    let app = Router::new()
        .route("/mcp", post(handle).get(no_stream).delete(no_stream))
        .with_state(state);
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("agent-core: MCP server stopped: {e}");
        }
    });
    Ok(McpServer { url: format!("http://127.0.0.1:{port}/mcp"), token, task })
}

/// No server-initiated stream; clients fall back to plain POST.
async fn no_stream() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

async fn handle(State(state): State<AppState>, headers: HeaderMap, body: String) -> Response {
    let authorized = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {}", state.token));
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(msg) = serde_json::from_str::<Value>(&body) else {
        return json_response(error(Value::Null, -32700, "Parse error"));
    };

    let responses: Vec<Value> = match msg {
        Value::Array(batch) => {
            let mut out = Vec::new();
            for m in batch {
                if let Some(r) = dispatch(&state, m).await {
                    out.push(r);
                }
            }
            out
        }
        single => dispatch(&state, single).await.into_iter().collect(),
    };

    match responses.len() {
        0 => StatusCode::ACCEPTED.into_response(),
        1 => json_response(responses.into_iter().next().expect("one")),
        _ => json_response(Value::Array(responses)),
    }
}

fn json_response(v: Value) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response()
}

/// Handles one JSON-RPC message. Notifications (no id) get no response.
async fn dispatch(state: &AppState, msg: Value) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let method = msg["method"].as_str().unwrap_or("");
    let params = &msg["params"];
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or(DEFAULT_PROTOCOL),
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "agent-unibrow", "version": env!("CARGO_PKG_VERSION")},
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": state.host.mcp_tool_list()}),
        "tools/call" => {
            let Some(name) = params["name"].as_str() else {
                return Some(error(id, -32602, "tools/call needs a name"));
            };
            let args = match &params["arguments"] {
                Value::Null => json!({}),
                a => a.clone(),
            };
            let call_id = state.host.fresh_id("mcp");
            let out = state.host.execute(call_id, name, args).await;
            let mut content = vec![json!({"type": "text", "text": out.content})];
            for img in out.images {
                content.push(json!({"type": "image", "data": img.base64, "mimeType": img.mime}));
            }
            json!({"content": content, "isError": out.is_error})
        }
        "resources/list" => json!({"resources": []}),
        "prompts/list" => json!({"prompts": []}),
        _ => return Some(error(id, -32601, &format!("Method not found: {method}"))),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}
