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
    /// A JavaScript dialog is open (the page is frozen until it's answered).
    pub dialog: Option<crate::DialogInfo>,
    /// The last page load failed (DNS, offline, refused...).
    pub load_error: Option<String>,
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

/// One tab, as the agent sees it.
#[derive(Clone, Debug)]
pub struct TabInfo {
    pub id: u32,
    pub title: String,
    pub url: String,
    pub active: bool,
}

pub enum TabOp {
    List,
    /// Open a tab (at `url`, or the home page) and make it active.
    New { url: Option<String> },
    Switch { id: u32 },
    Close { id: u32 },
}

pub enum Request {
    /// List, open, switch or close tabs; replies with the tabs afterwards.
    Tabs { op: TabOp, reply: Reply<Vec<TabInfo>> },
    /// A raw DevTools Protocol command.
    Cdp { method: String, params: Value, reply: Reply<Value> },
    Navigate { url: String, reply: Reply<()> },
    Nav { action: NavAction, reply: Reply<()> },
    State { reply: Reply<PageState> },
    /// Outline an element the agent is about to act on.
    Highlight { rect: Rect, label: String },
    /// Downloads from every tab.
    Downloads { reply: Reply<Vec<crate::DownloadInfo>> },
    /// Save the active page as a PDF; `done` gets whether it worked.
    SavePdf { path: String, done: oneshot::Sender<bool> },
    /// Hand the page to the user ("enter the code, then click Done").
    /// Replies true when they click Done, false for "Not now".
    Handoff { message: String, reply: Reply<bool> },
    /// Answer the open JavaScript dialog.
    Dialog { accept: bool, text: Option<String>, reply: Reply<()> },
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

    pub async fn downloads(&self) -> Result<Vec<crate::DownloadInfo>, String> {
        self.ask(|reply| Request::Downloads { reply }).await
    }

    pub async fn save_pdf(&self, path: String) -> Result<bool, String> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Request::SavePdf { path, done: tx }).await.map_err(|_| "The browser isn't running.".to_string())?;
        match tokio::time::timeout(Duration::from_secs(60), rx).await {
            Ok(Ok(ok)) => Ok(ok),
            Ok(Err(_)) => Err("The page closed before the PDF was saved.".into()),
            Err(_) => Err("Saving the PDF took longer than a minute.".into()),
        }
    }

    /// Waits (up to `timeout`) for the user to finish a handoff.
    pub async fn handoff(&self, message: String, timeout: Duration) -> Result<bool, String> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Request::Handoff { message, reply: tx })
            .await
            .map_err(|_| "The browser isn't running.".to_string())?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("The handoff was closed.".into()),
            Err(_) => Err(format!("The user didn't finish within {} minutes.", timeout.as_secs() / 60)),
        }
    }

    pub async fn tabs(&self, op: TabOp) -> Result<Vec<TabInfo>, String> {
        self.ask(|reply| Request::Tabs { op, reply }).await
    }

    pub async fn answer_dialog(&self, accept: bool, text: Option<String>) -> Result<(), String> {
        self.ask(|reply| Request::Dialog { accept, text, reply }).await
    }

    pub fn highlight(&self, rect: Rect, label: impl Into<String>) {
        let _ = self.tx.try_send(Request::Highlight { rect, label: label.into() });
    }
}
