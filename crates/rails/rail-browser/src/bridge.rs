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
    /// The last page load failed (DNS, offline, refused, a bad
    /// certificate), or the page has a problem: its renderer crashed or
    /// hung, or the site wants an HTTP sign-in.
    pub load_error: Option<String>,
    /// A permission request waiting for the user ("https://x asks for
    /// location"). Only the user can answer it.
    pub permission_prompt: Option<String>,
    /// The page's renderer crashed; only a reload or navigation helps.
    pub crashed: bool,
    /// A site (shown as host[:port]) is waiting for an HTTP sign-in that
    /// only the user can give.
    pub auth: Option<String>,
    /// Which tab this is, when that matters to the agent: it's working in a
    /// tab the user isn't looking at (3.1), or the tab is a popup window
    /// another tab opened (3.2). Filled in by the UI.
    pub tab_note: Option<String>,
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
    /// The tab the user is looking at.
    pub active: bool,
    /// The tab the agent's browser tools act on (roadmap 3.1). Usually the
    /// active one; a background tab once the agent opened or picked one.
    pub agent: bool,
    /// A popup window (`window.open`) and the tab that opened it, which it
    /// can talk back to (3.2).
    pub opener: Option<u32>,
    /// On screen in split view, next to the active tab.
    pub split: bool,
}

pub enum TabOp {
    List,
    /// Open a tab (at `url`, or the home page). It becomes the active tab,
    /// or with `background` the agent's own tab: the agent works in it
    /// while the user keeps the tab they're on.
    New { url: Option<String>, background: bool },
    /// Show a tab to the user; the agent then works in the active tab again.
    Switch { id: u32 },
    /// Work in this tab without showing it to the user.
    Use { id: u32 },
    Close { id: u32 },
    /// Show these tabs side by side (2 to 4, the first is focused). The
    /// agent then works in the focused one, as after a switch.
    Split { ids: Vec<u32> },
    /// Back to one tab on screen (the focused one).
    Unsplit,
}


/// A verification code from the user's email, shown to the user only.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeCard {
    pub code: String,
    /// Sender as the email shows it ("Netflix <info@netflix.com>").
    pub from: String,
    /// Unix seconds.
    pub received: i64,
}

pub enum Request {
    /// List, open, switch or close tabs; replies with the tabs afterwards.
    Tabs { op: TabOp, reply: Reply<Vec<TabInfo>> },
    /// A raw DevTools Protocol command, to the agent's tab or to `tab`.
    Cdp { tab: Option<u32>, method: String, params: Value, reply: Reply<Value> },
    Navigate { url: String, reply: Reply<()> },
    Nav { action: NavAction, reply: Reply<()> },
    /// The agent's tab's state, or `tab`'s.
    State { tab: Option<u32>, reply: Reply<PageState> },
    /// Outline an element the agent is about to act on.
    Highlight { rect: Rect, label: String },
    /// Downloads from every tab.
    Downloads { reply: Reply<Vec<crate::DownloadInfo>> },
    /// Save the active page as a PDF; `done` gets whether it worked.
    SavePdf { path: String, done: oneshot::Sender<bool> },
    /// Hand the page to the user ("enter the code, then click Done").
    /// Replies true when they click Done, false for "Not now".
    Handoff { message: String, reply: Reply<bool> },
    /// Show a verification code above the page for the user to copy or
    /// fill (5.2). Cleared when the handoff that follows ends.
    ShowCode { card: CodeCard },
    /// Answer the open JavaScript dialog.
    Dialog { accept: bool, text: Option<String>, reply: Reply<()> },
    /// Background pages (roadmap 9.x, 3.1): hidden pages the agent loads to
    /// read or compare without touching the user's tabs. They share the
    /// profile (cookies, sign-ins) and close when the tool is done.
    BgOpen { url: String, reply: Reply<u32> },
    BgCdp { id: u32, method: String, params: Value, reply: Reply<Value> },
    BgState { id: u32, reply: Reply<PageState> },
    BgClose { id: u32 },
}

impl Request {
    /// Requests that don't act on the agent's tab: background pages, and
    /// looking at a tab by id (`browser_view_tabs`).
    pub fn is_background(&self) -> bool {
        matches!(
            self,
            Request::BgOpen { .. }
                | Request::BgCdp { .. }
                | Request::BgState { .. }
                | Request::BgClose { .. }
                | Request::Cdp { tab: Some(_), .. }
                | Request::State { tab: Some(_), .. }
        )
    }
}

#[derive(Clone)]
pub struct BrowserBridge {
    tx: async_channel::Sender<Request>,
    /// DevTools commands and state go to this tab instead of the agent's
    /// ([`BrowserBridge::for_tab`]).
    tab: Option<u32>,
}

/// Creates the bridge: tools get the [`BrowserBridge`], the UI drains the receiver.
pub fn channel() -> (BrowserBridge, async_channel::Receiver<Request>) {
    let (tx, rx) = async_channel::unbounded();
    (BrowserBridge { tx, tab: None }, rx)
}

/// How long to wait for the UI to answer before giving up.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

impl BrowserBridge {
    /// The same bridge, with DevTools commands and state going to tab `id`
    /// (to look at it without making it the agent's tab).
    pub fn for_tab(&self, id: u32) -> Self {
        BrowserBridge { tx: self.tx.clone(), tab: Some(id) }
    }

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
        let tab = self.tab;
        self.ask(|reply| Request::Cdp { tab, method: method_owned, params, reply })
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
        let tab = self.tab;
        self.ask(|reply| Request::State { tab, reply }).await
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

    pub async fn bg_open(&self, url: &str) -> Result<u32, String> {
        let url = url.to_string();
        self.ask(|reply| Request::BgOpen { url, reply }).await
    }

    pub async fn bg_cdp(&self, id: u32, method: &str, params: Value) -> Result<Value, String> {
        let method_owned = method.to_string();
        self.ask(|reply| Request::BgCdp { id, method: method_owned, params, reply })
            .await
            .map_err(|e| format!("{method}: {e}"))
    }

    pub async fn bg_state(&self, id: u32) -> Result<PageState, String> {
        self.ask(|reply| Request::BgState { id, reply }).await
    }

    /// Closes a background page. Never blocks, so it's safe from `Drop`.
    pub fn bg_close(&self, id: u32) {
        let _ = self.tx.try_send(Request::BgClose { id });
    }

    pub fn show_code(&self, card: CodeCard) {
        let _ = self.tx.try_send(Request::ShowCode { card });
    }

    pub fn highlight(&self, rect: Rect, label: impl Into<String>) {
        let _ = self.tx.try_send(Request::Highlight { rect, label: label.into() });
    }
}
