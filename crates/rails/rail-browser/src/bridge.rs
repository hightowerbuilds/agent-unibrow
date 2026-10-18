//! The channel between agent tools (any thread) and the browser (UI thread).
//!
//! Tools hold a [`BrowserBridge`] and await replies; the UI pump loop drains
//! [`Request`]s each tick and answers them against the live [`WebView`].
//! Nothing CEF-specific crosses the channel, so the bridge is `Send + Sync`.

use futures::channel::oneshot;
use serde_json::Value;
use std::time::Duration;

pub type Reply<T> = oneshot::Sender<Result<T, String>>;

/// What the agent can know about the page without asking Chromium.
#[derive(Clone, Debug, Default)]
pub struct PageState {
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    /// Viewport size in CSS pixels.
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavAction {
    Back,
    Forward,
    Reload,
}

/// A rectangle in page (CSS pixel) coordinates.
#[derive(Clone, Copy, Debug, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

pub enum Request {
    /// A raw DevTools Protocol command.
    Cdp { method: String, params: Value, reply: Reply<Value> },
    Navigate { url: String, reply: Reply<()> },
    Nav { action: NavAction, reply: Reply<()> },
    State { reply: Reply<PageState> },
    /// Outline an element the agent is about to act on.
    Highlight { rect: Rect, label: String },
}

#[derive(Clone)]
pub struct BrowserBridge {
    tx: async_channel::Sender<Request>,
}

/// Creates the bridge: tools get the [`BrowserBridge`], the UI drains the receiver.
pub fn channel() -> (BrowserBridge, async_channel::Receiver<Request>) {
    let (tx, rx) = async_channel::unbounded();
    (BrowserBridge { tx }, rx)
}

/// How long to wait for the UI to answer before giving up.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

impl BrowserBridge {
    async fn ask<T>(&self, make: impl FnOnce(Reply<T>) -> Request) -> Result<T, String> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(make(tx)).await.map_err(|_| "The browser isn't running.".to_string())?;
        match tokio::time::timeout(REPLY_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("The browser closed before answering.".into()),
            Err(_) => Err("The browser didn't answer within 30 seconds.".into()),
        }
    }

    /// Sends a DevTools command and returns its `result` object.
    pub async fn cdp(&self, method: &str, params: Value) -> Result<Value, String> {
        let method_owned = method.to_string();
        self.ask(|reply| Request::Cdp { method: method_owned, params, reply })
            .await
            .map_err(|e| format!("{method}: {e}"))
    }

    pub async fn navigate(&self, url: &str) -> Result<(), String> {
        let url = url.to_string();
        self.ask(|reply| Request::Navigate { url, reply }).await
    }

    pub async fn nav(&self, action: NavAction) -> Result<(), String> {
        self.ask(|reply| Request::Nav { action, reply }).await
    }

    pub async fn state(&self) -> Result<PageState, String> {
        self.ask(|reply| Request::State { reply }).await
    }

    pub fn highlight(&self, rect: Rect, label: impl Into<String>) {
        let _ = self.tx.try_send(Request::Highlight { rect, label: label.into() });
    }
}
