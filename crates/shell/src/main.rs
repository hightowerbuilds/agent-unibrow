//! agent-unibrow shell.
//!
//! Three regions: conversation (left), content (center, where the web
//! engine composites in M2), and the trace pane (right). The agent runs on
//! its own thread; its events arrive here over a channel.

mod browser_pane;
mod explained;
mod text_input;
mod theme;

use agent_core::{AgentEvent, AgentHandle, Approval, BackendKind, Settings};
use gpui::{
    App, Application, Bounds, Context, Entity, FocusHandle, Focusable, ScrollHandle, SharedString,
    Subscription, Task, TitlebarOptions, Window, WindowBounds, WindowOptions, div, prelude::*, px,
    rgb, size,
};
use policy::Policy;
use router::Registry;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use browser_pane::{AgentControl, BrowserPane, BrowsingChanged};
use rail_browser::BrowserBridge;
use browsing::{Browsing, HistoryQuery, site_of};
use text_input::{Changed, Submit, TextInput};
use trace::{Kind, Trace};

gpui::actions!(
    shell,
    [ToggleSidebar, ShowChat, ShowHistory, ShowFocus, ShowTrace, ShowExplained, StopAgent, NewTab, CloseTab, NextTab, PrevTab, FocusAddress, FindInPage]
);

const KEYRING_SERVICE: &str = "agent-unibrow";
const KEYRING_API_KEY: &str = "anthropic-api-key";
const DETAIL_CHARS: usize = 6000;

#[derive(Clone, Copy, PartialEq)]
enum ItemKind {
    User,
    Assistant,
    Note,
    Error,
    System,
}

struct Item {
    kind: ItemKind,
    text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SidebarTab {
    Chat,
    History,
    Focus,
    Trace,
    Explained,
}

/// Width of the open sidebar and of the bumper strip that toggles it.
const SIDEBAR_WIDTH: f32 = 420.;
const BUMPER_WIDTH: f32 = 14.;

struct PendingApproval {
    id: String,
    name: String,
    description: String,
    reason: String,
    /// Screenshot saved with the trace entry, shown on the card.
    image: Option<std::path::PathBuf>,
    respond: Option<agent_core::ApprovalSender>,
}

struct Shell {
    focus_handle: FocusHandle,
    input: Entity<TextInput>,
    browser: Entity<BrowserPane>,
    bridge: BrowserBridge,
    store: Arc<Browsing>,
    history_filter: Entity<TextInput>,
    /// The user took over the page; offer to hand it back.
    can_continue: bool,
    /// Saved-login form in the Focus tab.
    login_form_open: bool,
    login_site: Entity<TextInput>,
    login_user: Entity<TextInput>,
    login_pass: Entity<TextInput>,
    login_error: Option<String>,
    items: Vec<Item>,
    /// Index of the assistant item currently receiving streamed text.
    streaming: Option<usize>,
    trace: Trace,
    expanded: HashSet<usize>,
    approvals: Vec<PendingApproval>,
    agent: Option<AgentHandle>,
    settings: Settings,
    policy: Arc<Mutex<Policy>>,
    busy: bool,
    status: SharedString,
    sidebar_open: bool,
    tab: SidebarTab,
    bookmarks_open: bool,
    needs_key: bool,
    conv_scroll: ScrollHandle,
    trace_scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
    _events: Option<Task<()>>,
}

impl Shell {
    fn new(cef: Result<(), String>, store: Arc<Browsing>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new("Ask Claude to do something…", cx));
        let (bridge, requests) = rail_browser::bridge::channel();
        let browser = cx.new(|cx| BrowserPane::new(cef, store.clone(), requests, window, cx));
        let control_sub = cx.subscribe(&browser, |this: &mut Self, _, ev: &AgentControl, cx| match ev {
            AgentControl::Stop => this.stop_agent("Stopped.", cx),
            AgentControl::TookOver => {
                this.stop_agent("You took over the page. Press Continue when you want the agent to pick up from there.", cx);
                this.can_continue = true;
            }
        });
        let history_filter = cx.new(|cx| TextInput::new("Search history", cx));
        let login_site = cx.new(|cx| TextInput::new("Site, e.g. sweetwater.com", cx));
        let login_user = cx.new(|cx| TextInput::new("Username or email", cx));
        let login_pass = cx.new(|cx| {
            let mut t = TextInput::new("Password", cx);
            t.set_masked(true, cx);
            t
        });
        let subscription = cx.subscribe(&input, |this: &mut Self, _, ev: &Submit, cx| this.submit(ev.0.clone(), cx));
        let browsing_sub = cx.subscribe(&browser, |this: &mut Self, _, _: &BrowsingChanged, cx| {
            if matches!(this.tab, SidebarTab::History | SidebarTab::Focus) {
                cx.notify();
            }
        });
        let filter_sub = cx.subscribe(&history_filter, |_: &mut Self, _, _: &Changed, cx| cx.notify());
        window.focus(&input.focus_handle(cx));

        let mut shell = Self {
            focus_handle: cx.focus_handle(),
            input,
            browser,
            bridge,
            store,
            history_filter,
            can_continue: false,
            login_form_open: false,
            login_site,
            login_user,
            login_pass,
            login_error: None,
            items: Vec::new(),
            streaming: None,
            trace: Trace::open_session(),
            expanded: HashSet::new(),
            approvals: Vec::new(),
            agent: None,
            settings: Settings::load(),
            policy: Arc::new(Mutex::new(Policy::load())),
            busy: false,
            status: "".into(),
            sidebar_open: true,
            tab: SidebarTab::Chat,
            bookmarks_open: false,
            needs_key: false,
            conv_scroll: ScrollHandle::new(),
            trace_scroll: ScrollHandle::new(),
            _subscriptions: vec![subscription, browsing_sub, filter_sub, control_sub],
            _events: None,
        };
        if let Some(p) = shell.trace.path().cloned() {
            shell.trace.push(Kind::Info, "Trace file opened", Some(p.display().to_string()));
        }
        shell.start_backend(cx);
        // Test hook: press Stop this many seconds after the first message.
        if let Some(secs) = std::env::var("UNIBROW_AUTOSTOP_SECS").ok().and_then(|s| s.parse::<u64>().ok()) {
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(std::time::Duration::from_millis(250)).await;
                    if this.update(cx, |s, _| s.busy).unwrap_or(false) {
                        break;
                    }
                }
                cx.background_executor().timer(std::time::Duration::from_secs(secs)).await;
                let _ = this.update(cx, |s, cx| s.stop_agent("Stopped.", cx));
            })
            .detach();
        }
        // Test hook: send a task automatically once the page has loaded.
        if let Ok(task) = std::env::var("UNIBROW_AUTOSEND") {
            cx.spawn(async move |this, cx| {
                for _ in 0..80 {
                    cx.background_executor().timer(std::time::Duration::from_millis(250)).await;
                    let ready = this
                        .update(cx, |s, cx| !s.browser.read(cx).location().0.is_empty())
                        .unwrap_or(false);
                    if ready {
                        break;
                    }
                }
                let _ = this.update(cx, |s, cx| s.submit(task, cx));
            })
            .detach();
        }
        shell
    }

    fn ask_for_key(&mut self, message: &str, cx: &mut Context<Self>) {
        self.needs_key = true;
        self.items.push(Item { kind: ItemKind::System, text: message.to_string() });
        self.input.update(cx, |i, cx| {
            i.set_masked(true, cx);
            i.set_placeholder("sk-ant-…", cx);
        });
        cx.notify();
    }

    /// Starts the selected backend, asking for an API key first if it needs one.
    fn start_backend(&mut self, cx: &mut Context<Self>) {
        let kind = self.settings.backend;
        if kind.needs_api_key() {
            match load_api_key() {
                Some(key) => self.start_agent(Some(key), cx),
                None => self.ask_for_key("Paste your Anthropic API key and press Enter. It's saved to your GNOME keyring.", cx),
            }
        } else {
            self.start_agent(None, cx);
        }
    }

    fn switch_backend(&mut self, kind: BackendKind, cx: &mut Context<Self>) {
        if kind == self.settings.backend && self.agent.is_some() {
            return;
        }
        // Dropping the handle ends the old agent; dropping pending approval
        // senders answers them with Deny.
        self.agent = None;
        self._events = None;
        self.approvals.clear();
        self.streaming = None;
        self.set_busy(false, "", cx);
        if self.needs_key {
            self.needs_key = false;
            self.input.update(cx, |i, cx| {
                i.set_masked(false, cx);
                i.set_placeholder("Ask Claude to do something…", cx);
            });
        }
        self.settings.backend = kind;
        self.settings.save();
        self.items.push(Item { kind: ItemKind::System, text: format!("Switched to {}. New conversation.", kind.label()) });
        self.trace.push(Kind::Info, format!("Switched agent to {}", kind.label()), None);
        self.start_backend(cx);
        cx.notify();
    }

    fn start_agent(&mut self, api_key: Option<String>, cx: &mut Context<Self>) {
        let (rail, notes) = rail_api::ApiRail::from_config();
        let mut registry = Registry::new();
        registry.add_rail(&rail);
        for tool in browsing::tools(self.store.clone()) {
            registry.add_tool(tool);
        }
        for tool in rail_browser::tools::tools(self.bridge.clone()) {
            registry.add_tool(tool);
        }
        for n in &notes {
            self.trace.push(Kind::Info, n.clone(), None);
        }
        if notes.iter().any(|n| n.starts_with("Gmail tools off")) {
            let path = rail_api::client_secret_path().map(|p| p.display().to_string()).unwrap_or_default();
            self.items.push(Item {
                kind: ItemKind::System,
                text: format!(
                    "Gmail isn't connected yet. Save a Google OAuth client (Desktop app) as {path} and restart to enable it."
                ),
            });
        }
        self.trace.push(Kind::Info, format!("Starting {}", self.settings.backend.label()), None);
        let (handle, events) = agent_core::spawn(self.settings.clone(), api_key, registry, self.policy.clone());
        self.agent = Some(handle);
        self._events = Some(cx.spawn(async move |this, cx| {
            while let Ok(ev) = events.recv().await {
                if this.update(cx, |shell, cx| shell.on_event(ev, cx)).is_err() {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn submit(&mut self, text: String, cx: &mut Context<Self>) {
        if self.needs_key {
            self.needs_key = false;
            self.input.update(cx, |i, cx| {
                i.set_masked(false, cx);
                i.set_placeholder("Ask Claude to do something…", cx);
            });
            match store_api_key(&text) {
                Ok(()) => self.trace.push(Kind::Info, "API key saved to keyring", None),
                Err(e) => self.trace.push(Kind::Error, format!("Couldn't save the API key ({e}); using it for this session only"), None),
            };
            self.items.push(Item { kind: ItemKind::System, text: "API key set.".into() });
            self.start_agent(Some(text), cx);
            return;
        }
        let Some(agent) = &self.agent else { return };
        if self.busy {
            return;
        }
        self.can_continue = false;
        self.items.push(Item { kind: ItemKind::User, text: text.clone() });
        self.trace.push(Kind::User, first_line(&text), Some(text.clone()));
        // The agent always knows which sites are in focus.
        let message = match self.store.focus_summary() {
            Some(sites) => format!("[Focus sites: {sites}]\n{text}"),
            None => text,
        };
        if agent.send(message) {
            self.set_busy(true, "Thinking…", cx);
        } else {
            self.items.push(Item { kind: ItemKind::Error, text: "The agent stopped. Restart the app.".into() });
        }
        self.scroll_to_bottom();
        cx.notify();
    }

    fn set_busy(&mut self, busy: bool, status: &str, cx: &mut Context<Self>) {
        self.busy = busy;
        self.status = status.to_string().into();
        self.input.update(cx, |i, cx| i.set_disabled(busy, cx));
        self.browser.update(cx, |b, cx| b.set_agent_busy(busy, cx));
    }

    /// Stops the agent's current turn (Stop button, Esc, or the user taking
    /// over the page).
    fn stop_agent(&mut self, note: &str, cx: &mut Context<Self>) {
        if !self.busy {
            return;
        }
        if let Some(agent) = &self.agent {
            agent.cancel();
        }
        // Dropping the senders answers pending approvals with Deny.
        self.approvals.clear();
        self.streaming = None;
        self.items.push(Item { kind: ItemKind::System, text: note.to_string() });
        self.trace.push(Kind::Info, note.to_string(), None);
        self.status = "Stopping…".into();
        cx.notify();
    }

    fn on_event(&mut self, ev: AgentEvent, cx: &mut Context<Self>) {
        match ev {
            AgentEvent::Info(s) => {
                self.trace.push(Kind::Info, s, None);
            }
            AgentEvent::TurnStarted => {
                self.streaming = None;
                self.status = "Thinking…".into();
            }
            AgentEvent::TextDelta(t) => {
                let idx = match self.streaming {
                    Some(i) => i,
                    None => {
                        self.items.push(Item { kind: ItemKind::Assistant, text: String::new() });
                        self.streaming = Some(self.items.len() - 1);
                        self.items.len() - 1
                    }
                };
                self.items[idx].text.push_str(&t);
            }
            AgentEvent::Note(n) => {
                self.streaming = None;
                self.items.push(Item { kind: ItemKind::Note, text: n.clone() });
                self.trace.push(Kind::Assistant, format!("Note: {}", first_line(&n)), Some(n));
            }
            AgentEvent::ToolCall { name, input, .. } => {
                self.streaming = None;
                self.status = format!("Running {name}").into();
                let pretty = serde_json::to_string_pretty(&input).unwrap_or_default();
                self.trace.push(Kind::ToolCall, format!("{name} {}", compact(&input)), Some(pretty));
            }
            AgentEvent::ToolResult { name, ok, summary, content, evidence, .. } => {
                let kind = if ok { Kind::ToolResult } else { Kind::Error };
                let images = evidence.iter().filter_map(rail_browser::tools::decode_image).collect();
                self.trace.push_with_images(kind, format!("{name}: {summary}"), Some(content), images);
                self.status = "Thinking…".into();
            }
            AgentEvent::ApprovalNeeded { id, name, description, reason, image, respond } => {
                self.status = "Waiting for your approval".into();
                self.sidebar_open = true;
                self.tab = SidebarTab::Chat;
                let shot: Vec<Vec<u8>> = image.iter().filter_map(rail_browser::tools::decode_image).collect();
                let entry =
                    self.trace.push_with_images(Kind::ApprovalRequested, format!("Approval needed: {reason}"), Some(description.clone()), shot);
                let image = entry.images.first().map(std::path::PathBuf::from);
                let auto = std::env::var("UNIBROW_AUTOAPPROVE").ok();
                match auto.as_deref() {
                    // Test hook: answer approval cards automatically.
                    Some("approve") => {
                        let _ = respond.send(Approval::Approve);
                    }
                    Some("deny") => {
                        let _ = respond.send(Approval::Deny);
                    }
                    _ => self.approvals.push(PendingApproval { id, name, description, reason, image, respond: Some(respond) }),
                }
            }
            AgentEvent::ApprovalDecided { name, approval, .. } => {
                let verb = match approval {
                    Approval::Approve => "approved",
                    Approval::AlwaysAllow => "approved (always allow these recipients)",
                    Approval::Deny => "denied",
                };
                self.trace.push(Kind::ApprovalDecision, format!("{name} {verb}"), None);
            }
            AgentEvent::Done { stop_reason, model, usage } => {
                self.streaming = None;
                self.approvals.clear();
                let tokens = format!(
                    "in {} · out {} · cache read {}",
                    usage["input_tokens"].as_u64().unwrap_or(0),
                    usage["output_tokens"].as_u64().unwrap_or(0),
                    usage["cache_read_input_tokens"].as_u64().unwrap_or(0)
                );
                // The reply text since the last user message, for the record.
                let reply: Vec<&str> = self
                    .items
                    .iter()
                    .rev()
                    .take_while(|i| i.kind != ItemKind::User)
                    .filter(|i| i.kind == ItemKind::Assistant)
                    .map(|i| i.text.as_str())
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                self.trace.push(
                    Kind::Assistant,
                    format!("Turn finished ({stop_reason})"),
                    Some(format!("{}\n\nmodel: {}\n{tokens}", reply.join("\n\n"), model.unwrap_or_default())),
                );
                self.set_busy(false, "", cx);
            }
            AgentEvent::Error(e) => {
                self.streaming = None;
                self.items.push(Item { kind: ItemKind::Error, text: e.clone() });
                self.trace.push(Kind::Error, first_line(&e), Some(e.clone()));
                self.set_busy(false, "", cx);
                if e.contains("API 401") && self.settings.backend.needs_api_key() {
                    let _ = delete_api_key();
                    self.agent = None;
                    self._events = None;
                    self.ask_for_key("That API key was rejected. Paste a valid key and press Enter.", cx);
                }
            }
        }
        self.scroll_to_bottom();
        cx.notify();
    }

    fn resolve(&mut self, id: &str, approval: Approval, cx: &mut Context<Self>) {
        if let Some(pos) = self.approvals.iter().position(|a| a.id == id) {
            let mut pending = self.approvals.remove(pos);
            if let Some(tx) = pending.respond.take() {
                let _ = tx.send(approval);
            }
            self.status = "Thinking…".into();
        }
        cx.notify();
    }

    fn scroll_to_bottom(&self) {
        self.conv_scroll.scroll_to_bottom();
        self.trace_scroll.scroll_to_bottom();
    }

    // ------------------------------------------------------------- views

    fn toggle_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        if self.sidebar_open && self.tab == SidebarTab::Chat {
            window.focus(&self.input.focus_handle(cx));
        } else if !self.sidebar_open {
            // The focused chat input is going away; hand focus to the page.
            window.focus(&self.browser.focus_handle(cx));
        }
        cx.notify();
    }

    /// Puts an example request from the guide in the chat box.
    fn use_example(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.tab = SidebarTab::Chat;
        self.sidebar_open = true;
        self.input.update(cx, |i, cx| i.set_text(text, cx));
        window.focus(&self.input.focus_handle(cx));
        cx.notify();
    }

    /// Opens the sidebar (if closed) on `tab`.
    fn show_tab(&mut self, tab: SidebarTab, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = true;
        self.select_tab(tab, window, cx);
    }

    fn select_tab(&mut self, tab: SidebarTab, window: &mut Window, cx: &mut Context<Self>) {
        self.tab = tab;
        match tab {
            SidebarTab::Chat => window.focus(&self.input.focus_handle(cx)),
            SidebarTab::History => window.focus(&self.history_filter.focus_handle(cx)),
            _ => {}
        }
        self.scroll_to_bottom();
        cx.notify();
    }

    /// Chat / Trace tabs, with the agent's status on the right.
    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = |id: &'static str, label: String, selected: bool, badge: Option<String>| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_1()
                .px_3()
                .py_2()
                .text_xs()
                .border_b_2()
                .border_color(rgb(if selected { theme::ACCENT } else { theme::PANE }))
                .text_color(rgb(if selected { theme::FG } else { theme::MUTED }))
                .when(!selected, |d| d.cursor_pointer().hover(|s| s.text_color(rgb(theme::FG))))
                .child(label)
                .children(badge.map(|b| {
                    div()
                        .px_1()
                        .rounded_sm()
                        .bg(rgb(theme::WARN))
                        .text_color(rgb(theme::BG))
                        .child(b)
                }))
        };
        let pending = (!self.approvals.is_empty() && self.tab != SidebarTab::Chat).then(|| self.approvals.len().to_string());
        div()
            .flex()
            .items_end()
            .justify_between()
            .pr_3()
            .border_b_1()
            .border_color(rgb(theme::RULE))
            .child(
                div()
                    .flex()
                    .child(tab("tab-chat", "Chat".into(), self.tab == SidebarTab::Chat, pending).on_click(
                        cx.listener(|this, _, window, cx| this.select_tab(SidebarTab::Chat, window, cx)),
                    ))
                    .child(tab("tab-history", "History".into(), self.tab == SidebarTab::History, None).on_click(
                        cx.listener(|this, _, window, cx| this.select_tab(SidebarTab::History, window, cx)),
                    ))
                    .child(
                        tab(
                            "tab-focus",
                            match self.store.focus_sites().len() {
                                0 => "Focus".to_string(),
                                n => format!("Focus · {n}"),
                            },
                            self.tab == SidebarTab::Focus,
                            None,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.select_tab(SidebarTab::Focus, window, cx))),
                    )
                    .child(
                        tab(
                            "tab-trace",
                            format!("Trace · {}", self.trace.entries().len()),
                            self.tab == SidebarTab::Trace,
                            None,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.select_tab(SidebarTab::Trace, window, cx))),
                    )
                    .child(tab("tab-explained", "Explained".into(), self.tab == SidebarTab::Explained, None).on_click(
                        cx.listener(|this, _, window, cx| this.select_tab(SidebarTab::Explained, window, cx)),
                    )),
            )
    }

    /// The strip on the sidebar's edge that opens and closes it.
    fn render_bumper(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.sidebar_open;
        let attention = !open && !self.approvals.is_empty();
        div()
            .id("sidebar-bumper")
            .group("bumper")
            .w(px(BUMPER_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(theme::PANE))
            .border_r_1()
            .border_color(rgb(theme::RULE))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(theme::RAISED)))
            .on_click(cx.listener(|this, _, window, cx| this.toggle_sidebar(window, cx)))
            .child(
                div()
                    .w(px(4.))
                    .h(px(44.))
                    .rounded_full()
                    .bg(rgb(if attention { theme::WARN } else { theme::RULE }))
                    .group_hover("bumper", |s| s.bg(rgb(theme::ACCENT))),
            )
    }

    fn render_chat(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let approvals: Vec<_> = self.approvals.iter().map(|a| self.render_approval(a, cx).into_any_element()).collect();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .child(self.render_backend_bar(cx))
            .child(
                div()
                    .id("conversation")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .gap_3()
                    .p_3()
                    .overflow_y_scroll()
                    .track_scroll(&self.conv_scroll)
                    .when(self.items.iter().all(|i| i.kind == ItemKind::System), |d| {
                        d.child(
                            div()
                                .id("explained-hint")
                                .text_sm()
                                .text_color(rgb(theme::MUTED))
                                .cursor_pointer()
                                .hover(|s| s.text_color(rgb(theme::ACCENT)))
                                .on_click(cx.listener(|this, _, window, cx| this.select_tab(SidebarTab::Explained, window, cx)))
                                .child("New here? See how to get the most out of the agent → Explained (Alt+5)"),
                        )
                    })
                    .children(self.items.iter().map(Self::render_item))
                    .children(approvals),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .border_t_1()
                    .border_color(rgb(theme::RULE))
                    .when(self.can_continue && !self.busy, |d| {
                        d.child(
                            div()
                                .id("continue-agent")
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .bg(rgb(theme::ACCENT))
                                .text_sm()
                                .text_color(rgb(theme::BG))
                                .cursor_pointer()
                                .hover(|s| s.opacity(0.85))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.submit(
                                        "I've done my part on the page. Carry on from where the page is now.".into(),
                                        cx,
                                    )
                                }))
                                .child("Continue: hand the page back to the agent"),
                        )
                    })
                    .child(self.input.clone()),
            )
    }

    fn open_url(&mut self, url: String, cx: &mut Context<Self>) {
        self.browser.update(cx, |b, cx| b.navigate(&url, cx));
    }

    fn small_button(id: SharedString, label: &'static str, active: bool) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .flex_none()
            .px_2()
            .py(px(2.))
            .rounded_md()
            .text_xs()
            .border_1()
            .border_color(rgb(if active { theme::ACCENT } else { theme::RULE }))
            .text_color(rgb(if active { theme::ACCENT } else { theme::MUTED }))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(theme::RAISED)).text_color(rgb(theme::FG)))
            .child(label)
    }

    /// The "Bookmarks" dropdown at the top of the History tab.
    fn render_bookmarks(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let marks = self.store.bookmarks();
        let open = self.bookmarks_open;
        let header = div()
            .id("bookmarks-toggle")
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .cursor_pointer()
            .hover(|s| s.bg(rgb(theme::RAISED)))
            .on_click(cx.listener(|this, _, _, cx| {
                this.bookmarks_open = !this.bookmarks_open;
                cx.notify();
            }))
            .child(div().text_color(rgb(theme::WARN)).child("★"))
            .child(div().flex_1().text_sm().text_color(rgb(theme::FG)).child(match marks.len() {
                0 => "Bookmarks".to_string(),
                n => format!("Bookmarks · {n}"),
            }))
            .child(div().text_xs().text_color(rgb(theme::MUTED)).child(if open { "▴" } else { "▾" }));

        let list = open.then(|| {
            let empty = marks.is_empty();
            div()
                .id("bookmarks-list")
                .flex()
                .flex_col()
                .max_h(px(280.))
                .overflow_y_scroll()
                .pb_1()
                .when(empty, |d| {
                    d.child(
                        div()
                            .px_3()
                            .py_2()
                            .text_xs()
                            .text_color(rgb(theme::MUTED))
                            .child("No bookmarks yet. Use ☆ in the browser toolbar to bookmark the page you're on."),
                    )
                })
                .children(marks.into_iter().enumerate().map(|(i, m)| {
                    let url = m.url.clone();
                    let remove = m.url.clone();
                    let title = if m.title.is_empty() { m.url.clone() } else { m.title.clone() };
                    div()
                        .id(("bookmark", i))
                        .flex()
                        .items_center()
                        .gap_2()
                        .pl(px(34.))
                        .pr_3()
                        .py_1()
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(theme::RAISED)))
                        .on_click(cx.listener(move |this, _, _, cx| this.open_url(url.clone(), cx)))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w(px(0.))
                                .child(div().text_sm().text_color(rgb(theme::FG)).truncate().child(title))
                                .child(div().text_xs().text_color(rgb(theme::MUTED)).truncate().child(m.site)),
                        )
                        .child(
                            div()
                                .id(("bookmark-remove", i))
                                .px_1()
                                .rounded_sm()
                                .text_xs()
                                .text_color(rgb(theme::MUTED))
                                .cursor_pointer()
                                .hover(|s| s.text_color(rgb(theme::ERROR)).bg(rgb(theme::PANE)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.store.remove_bookmark(&remove);
                                    cx.notify();
                                }))
                                .child("✕"),
                        )
                }))
        });

        div().flex().flex_col().border_b_1().border_color(rgb(theme::RULE)).child(header).children(list)
    }

    fn render_history(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let filter = self.history_filter.read(cx).text().to_string();
        let visits = self.store.history(&HistoryQuery {
            text: (!filter.trim().is_empty()).then_some(filter.clone()),
            limit: 300,
            ..Default::default()
        });
        let focused: std::collections::HashSet<String> =
            self.store.focus_sites().into_iter().map(|f| f.site).collect();
        let today = chrono::Local::now().date_naive();

        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        let mut last_day = None;
        for v in visits {
            let day = v.at.date_naive();
            if last_day != Some(day) {
                last_day = Some(day);
                let label = match (today - day).num_days() {
                    0 => "Today".to_string(),
                    1 => "Yesterday".to_string(),
                    _ => v.at.format("%A, %B %-d").to_string(),
                };
                rows.push(
                    div()
                        .px_3()
                        .pt_3()
                        .pb_1()
                        .text_xs()
                        .text_color(rgb(theme::ACCENT))
                        .child(label)
                        .into_any_element(),
                );
            }
            let url = v.url.clone();
            let site = v.site.clone();
            let label = if v.title.is_empty() { v.site.clone() } else { v.title.clone() };
            let is_focused = focused.contains(&v.site);
            rows.push(
                div()
                    .id(("visit", v.id as usize))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::RAISED)))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_url(url.clone(), cx)))
                    .child(
                        div()
                            .w(px(56.))
                            .flex_none()
                            .text_xs()
                            .text_color(rgb(theme::MUTED))
                            .child(v.at.format("%-I:%M %p").to_string()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .child(div().text_sm().text_color(rgb(theme::FG)).truncate().child(label))
                            .child(div().text_xs().text_color(rgb(theme::MUTED)).truncate().child(v.site.clone())),
                    )
                    .child(
                        Self::small_button(
                            SharedString::from(format!("visit-focus-{}", v.id)),
                            if is_focused { "● Focus" } else { "Focus" },
                            is_focused,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.store.toggle_focus(&site, &site);
                            cx.notify();
                        })),
                    )
                    .into_any_element(),
            );
        }
        let empty = rows.is_empty();

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .child(self.render_bookmarks(cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .p_3()
                    .border_b_1()
                    .border_color(rgb(theme::RULE))
                    .child(div().flex_1().child(self.history_filter.clone()))
                    .child(
                        Self::small_button("history-clear".into(), "Clear", false)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.store.clear_history();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("history")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .pb_2()
                    .overflow_y_scroll()
                    .when(empty, |d| {
                        d.child(div().p_3().text_sm().text_color(rgb(theme::MUTED)).child(if filter.trim().is_empty() {
                            "Pages you visit in the browser show up here. Agents can search this history."
                        } else {
                            "No pages match."
                        }))
                    })
                    .children(rows),
            )
    }

    fn render_focus(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sites = self.store.focus_sites();
        let (current_url, current_title) = self.browser.read(cx).location();
        let current_site = site_of(&current_url);
        let current_in_focus = current_site.as_deref().is_some_and(|s| self.store.is_focused(s));

        let logins = self.render_logins(cx).into_any_element();
        let rows = sites.into_iter().map(|f| {
            let site = f.site.clone();
            div()
                .id(SharedString::from(format!("focus-{}", f.site)))
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(rgb(theme::RULE))
                .child(div().text_color(rgb(theme::ACCENT)).child("●"))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.))
                        .child(div().text_sm().text_color(rgb(theme::FG)).child(f.site.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::MUTED))
                                .truncate()
                                .child(format!("{} · added {}", f.label, f.added.format("%b %-d"))),
                        ),
                )
                .child(
                    Self::small_button(SharedString::from(format!("focus-open-{}", f.site)), "Open", false).on_click(
                        cx.listener({
                            let site = site.clone();
                            move |this, _, _, cx| this.open_url(format!("https://{site}"), cx)
                        }),
                    ),
                )
                .child(
                    Self::small_button(SharedString::from(format!("focus-remove-{}", f.site)), "Remove", false)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.store.remove_focus(&site);
                            cx.notify();
                        })),
                )
        });

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .border_b_1()
                    .border_color(rgb(theme::RULE))
                    .child(div().text_xs().text_color(rgb(theme::MUTED)).child(
                        "Sites in focus are the ones agents concentrate on. The list goes along with every message, \
                         and agents can look it up with a tool.",
                    ))
                    .children(current_site.map(|site| {
                        let label = if current_title.is_empty() { site.clone() } else { current_title.clone() };
                        Self::small_button(
                            "focus-current".into(),
                            if current_in_focus { "Remove current site" } else { "Add current site" },
                            current_in_focus,
                        )
                        .child(div().text_color(rgb(theme::MUTED)).child(format!(" {site}")))
                        .flex()
                        .gap_1()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.store.toggle_focus(&site, &label);
                            cx.notify();
                        }))
                    })),
            )
            .child(logins)
            .child(
                div()
                    .id("focus-list")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .children(rows)
                    .when(self.store.focus_sites().is_empty(), |d| {
                        d.child(div().p_3().text_sm().text_color(rgb(theme::MUTED)).child(
                            "No sites in focus yet. Use ◎ Focus in the browser toolbar, or ◎ next to any page in History.",
                        ))
                    }),
            )
    }

    fn save_login(&mut self, cx: &mut Context<Self>) {
        let site = self.login_site.read(cx).text().to_string();
        let user = self.login_user.read(cx).text().to_string();
        let pass = self.login_pass.read(cx).text().to_string();
        match rail_browser::vault::save(&site, &user, &pass) {
            Ok(()) => {
                self.login_error = None;
                self.login_form_open = false;
                for input in [&self.login_site, &self.login_user, &self.login_pass] {
                    input.update(cx, |i, cx| i.set_text("", cx));
                }
                self.trace.push(Kind::Info, format!("Saved a login for {}", rail_browser::vault::normalize_site(&site)), None);
            }
            Err(e) => self.login_error = Some(e),
        }
        cx.notify();
    }

    /// Saved logins (roadmap 5.1): the agent can fill these with
    /// browser_sign_in but never sees the passwords.
    fn render_logins(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let logins = rail_browser::vault::list();
        let form = self.login_form_open.then(|| {
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(self.login_site.clone())
                .child(self.login_user.clone())
                .child(self.login_pass.clone())
                .children(self.login_error.clone().map(|e| div().text_xs().text_color(rgb(theme::ERROR)).child(e)))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            Self::small_button("login-save".into(), "Save", true)
                                .on_click(cx.listener(|this, _, _, cx| this.save_login(cx))),
                        )
                        .child(Self::small_button("login-cancel".into(), "Cancel", false).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.login_form_open = false;
                                this.login_error = None;
                                this.login_pass.update(cx, |i, cx| i.set_text("", cx));
                                cx.notify();
                            },
                        ))),
                )
                .child(div().text_xs().text_color(rgb(theme::MUTED)).child(
                    "The password goes into your keyring. The agent can fill it on this site with browser_sign_in, but never sees it.",
                ))
        });
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .border_b_1()
            .border_color(rgb(theme::RULE))
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(div().flex_1().text_xs().text_color(rgb(theme::ACCENT)).child("SAVED LOGINS"))
                    .when(!self.login_form_open, |d| {
                        d.child(Self::small_button("login-add".into(), "Add login", false).on_click(cx.listener(
                            |this, _, window, cx| {
                                let (url, _) = this.browser.read(cx).location();
                                let site = site_of(&url).unwrap_or_default();
                                this.login_site.update(cx, |i, cx| i.set_text(&site, cx));
                                this.login_form_open = true;
                                window.focus(&this.login_user.focus_handle(cx));
                                cx.notify();
                            },
                        )))
                    }),
            )
            .when(logins.is_empty() && !self.login_form_open, |d| {
                d.child(div().text_xs().text_color(rgb(theme::MUTED)).child("None yet. Add one to let the agent sign in for you."))
            })
            .children(logins.into_iter().enumerate().map(|(i, l)| {
                let (site, user) = (l.site.clone(), l.username.clone());
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .child(div().flex_1().min_w(px(0.)).truncate().text_color(rgb(theme::FG)).child(format!("{} · {}", l.site, l.username)))
                    .child(div().text_xs().text_color(rgb(theme::MUTED)).child("••••••"))
                    .child(Self::small_button(SharedString::from(format!("login-remove-{i}")), "Remove", false).on_click(
                        cx.listener(move |this, _, _, cx| {
                            let _ = rail_browser::vault::remove(&site, &user);
                            this.trace.push(Kind::Info, format!("Removed the saved login for {site}"), None);
                            cx.notify();
                        }),
                    ))
            }))
            .children(form)
    }

    fn render_trace(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = self
            .trace
            .entries()
            .iter()
            .enumerate()
            .map(|(i, e)| self.render_trace_row(i, e, cx).into_any_element())
            .collect();
        div()
            .id("trace")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .py_2()
            .overflow_y_scroll()
            .track_scroll(&self.trace_scroll)
            .children(rows)
    }

    fn render_backend_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.settings.backend;
        let status: Option<SharedString> = (!self.status.is_empty()).then(|| self.status.clone());
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(rgb(theme::RULE))
            .children(BackendKind::ALL.into_iter().map(|kind| {
                let selected = kind == current;
                div()
                    .id(SharedString::from(format!("backend-{}", kind.label())))
                    .px_2()
                    .py(px(2.))
                    .rounded_md()
                    .text_xs()
                    .border_1()
                    .border_color(rgb(if selected { theme::ACCENT } else { theme::RULE }))
                    .bg(rgb(if selected { theme::RAISED } else { theme::PANE }))
                    .text_color(rgb(if selected { theme::ACCENT } else { theme::MUTED }))
                    .when(!selected, |d| d.cursor_pointer().hover(|s| s.text_color(rgb(theme::FG))))
                    .on_click(cx.listener(move |this, _, _, cx| this.switch_backend(kind, cx)))
                    .child(kind.label())
            }))
            .child(div().flex_1())
            .children(status.map(|s| div().text_xs().text_color(rgb(theme::MUTED)).child(s)))
    }

    fn render_item(item: &Item) -> impl IntoElement {
        let base = div().text_sm().line_height(px(20.));
        match item.kind {
            ItemKind::User => div().flex().justify_end().child(
                base.max_w(px(360.))
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(theme::RAISED))
                    .text_color(rgb(theme::FG))
                    .child(item.text.clone()),
            ),
            ItemKind::Assistant => base.text_color(rgb(theme::FG)).child(item.text.clone()),
            ItemKind::Note => base.text_xs().italic().text_color(rgb(theme::MUTED)).child(item.text.clone()),
            ItemKind::Error => base.text_color(rgb(theme::ERROR)).child(item.text.clone()),
            ItemKind::System => base
                .px_3()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(rgb(theme::RULE))
                .text_color(rgb(theme::MUTED))
                .child(item.text.clone()),
        }
    }

    fn render_approval(&self, a: &PendingApproval, cx: &mut Context<Self>) -> impl IntoElement {
        let button = |id: String, label: &'static str, primary: bool| {
            div()
                .id(SharedString::from(id))
                .px_3()
                .py_1()
                .rounded_md()
                .text_xs()
                .cursor_pointer()
                .border_1()
                .border_color(rgb(if primary { theme::ACCENT } else { theme::RULE }))
                .text_color(rgb(if primary { theme::BG } else { theme::FG }))
                .bg(rgb(if primary { theme::ACCENT } else { theme::PANE }))
                .hover(|s| s.opacity(0.85))
                .child(label)
        };
        let (id1, id2, id3) = (a.id.clone(), a.id.clone(), a.id.clone());
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::WARN))
            .bg(rgb(theme::RAISED))
            .child(div().text_xs().text_color(rgb(theme::WARN)).child(format!("Approve {}?", a.name)))
            .child(div().text_xs().text_color(rgb(theme::MUTED)).child(a.reason.clone()))
            .child(
                div()
                    .p_2()
                    .rounded_sm()
                    .bg(rgb(theme::BG))
                    .text_sm()
                    .text_color(rgb(theme::FG))
                    .child(a.description.clone()),
            )
            .children(a.image.clone().map(|p| {
                gpui::img(p).w_full().rounded_sm().border_1().border_color(rgb(theme::RULE))
            }))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(button(format!("approve-{}", a.id), "Approve", true).on_click(
                        cx.listener(move |this, _, _, cx| this.resolve(&id1, Approval::Approve, cx)),
                    ))
                    .child(button(format!("always-{}", a.id), if a.name.starts_with("browser_") { "Always allow on this site" } else { "Always allow these recipients" }, false).on_click(
                        cx.listener(move |this, _, _, cx| this.resolve(&id2, Approval::AlwaysAllow, cx)),
                    ))
                    .child(button(format!("deny-{}", a.id), "Deny", false).on_click(
                        cx.listener(move |this, _, _, cx| this.resolve(&id3, Approval::Deny, cx)),
                    )),
            )
    }

    fn render_trace_row(&self, i: usize, e: &trace::Entry, cx: &mut Context<Self>) -> impl IntoElement {
        let (label, color) = match e.kind {
            Kind::User => ("YOU", theme::FG),
            Kind::Assistant => ("MODEL", theme::MUTED),
            Kind::ToolCall => ("CALL", theme::ACCENT),
            Kind::ToolResult => ("RESULT", theme::OK),
            Kind::ApprovalRequested => ("ASK", theme::WARN),
            Kind::ApprovalDecision => ("DECIDED", theme::WARN),
            Kind::Error => ("ERROR", theme::ERROR),
            Kind::Info => ("INFO", theme::MUTED),
        };
        let open = self.expanded.contains(&i);
        let thumbs: Vec<_> = if open {
            e.images
                .iter()
                .map(|p| {
                    gpui::img(std::path::PathBuf::from(p))
                        .w(px(340.))
                        .rounded_sm()
                        .border_1()
                        .border_color(rgb(theme::RULE))
                        .into_any_element()
                })
                .collect()
        } else {
            Vec::new()
        };
        let has_images = !e.images.is_empty();
        let detail = e.detail.clone().filter(|_| open).map(|d| {
            let d = if d.chars().count() > DETAIL_CHARS {
                d.chars().take(DETAIL_CHARS).collect::<String>() + "\n…"
            } else {
                d
            };
            div()
                .mt_1()
                .p_2()
                .rounded_sm()
                .bg(rgb(theme::BG))
                .text_xs()
                .text_color(rgb(theme::MUTED))
                .child(d)
        });
        div()
            .id(("trace-row", i))
            .flex()
            .flex_col()
            .px_3()
            .py_1()
            .when(e.detail.is_some() || has_images, |d| d.cursor_pointer().hover(|s| s.bg(rgb(theme::RAISED))))
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.expanded.remove(&i) {
                    this.expanded.insert(i);
                }
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .text_xs()
                    .child(div().text_color(rgb(theme::MUTED)).child(e.at.format("%H:%M:%S").to_string()))
                    .child(div().w(px(52.)).flex_none().text_color(rgb(color)).child(label))
                    .child(div().flex_1().min_w(px(0.)).text_color(rgb(theme::FG)).child(e.summary.clone())),
            )
            .children(thumbs.into_iter().map(|t| div().mt_1().child(t)))
            .children(detail)
    }
}

impl Focusable for Shell {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sidebar = self.sidebar_open.then(|| {
            let body = match self.tab {
                SidebarTab::Chat => self.render_chat(cx).into_any_element(),
                SidebarTab::History => self.render_history(cx).into_any_element(),
                SidebarTab::Focus => self.render_focus(cx).into_any_element(),
                SidebarTab::Trace => self.render_trace(cx).into_any_element(),
                SidebarTab::Explained => {
                    let me = cx.entity().downgrade();
                    explained::render(move |text, window, cx| {
                        let _ = me.update(cx, |this, cx| this.use_example(text, window, cx));
                    })
                    .into_any_element()
                }
            };
            div()
                .flex()
                .flex_col()
                .w(px(SIDEBAR_WIDTH))
                .h_full()
                .flex_none()
                .bg(rgb(theme::PANE))
                .child(self.render_tabs(cx))
                .child(body)
        });

        div()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &ToggleSidebar, window, cx| this.toggle_sidebar(window, cx)))
            .on_action(cx.listener(|this, _: &StopAgent, _, cx| {
                if this.busy {
                    this.stop_agent("Stopped.", cx);
                } else {
                    // Not ours: let Escape reach the page or the focused field.
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &NewTab, window, cx| this.browser.update(cx, |b, cx| b.new_tab(window, cx))))
            .on_action(cx.listener(|this, _: &CloseTab, _, cx| this.browser.update(cx, |b, cx| b.close_active_tab(cx))))
            .on_action(cx.listener(|this, _: &NextTab, _, cx| this.browser.update(cx, |b, cx| b.cycle_tab(true, cx))))
            .on_action(cx.listener(|this, _: &PrevTab, _, cx| this.browser.update(cx, |b, cx| b.cycle_tab(false, cx))))
            .on_action(cx.listener(|this, _: &FindInPage, window, cx| this.browser.update(cx, |b, cx| b.open_find(window, cx))))
            .on_action(cx.listener(|this, _: &FocusAddress, window, cx| {
                this.browser.update(cx, |b, cx| b.focus_address(window, cx))
            }))
            .on_action(cx.listener(|this, _: &ShowChat, window, cx| this.show_tab(SidebarTab::Chat, window, cx)))
            .on_action(cx.listener(|this, _: &ShowHistory, window, cx| this.show_tab(SidebarTab::History, window, cx)))
            .on_action(cx.listener(|this, _: &ShowFocus, window, cx| this.show_tab(SidebarTab::Focus, window, cx)))
            .on_action(cx.listener(|this, _: &ShowTrace, window, cx| this.show_tab(SidebarTab::Trace, window, cx)))
            .on_action(cx.listener(|this, _: &ShowExplained, window, cx| this.show_tab(SidebarTab::Explained, window, cx)))
            .flex()
            .flex_row()
            .size_full()
            .bg(rgb(theme::BG))
            .text_color(rgb(theme::FG))
            .children(sidebar)
            .child(self.render_bumper(cx))
            .child(self.browser.clone())
    }
}

fn load_api_key() -> Option<String> {
    if let Ok(k) = std::env::var("ANTHROPIC_API_KEY") {
        if !k.trim().is_empty() {
            return Some(k.trim().to_string());
        }
    }
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_API_KEY).ok()?.get_password().ok()
}

fn store_api_key(key: &str) -> Result<(), String> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_API_KEY)
        .and_then(|e| e.set_password(key.trim()))
        .map_err(|e| e.to_string())
}

fn delete_api_key() -> Result<(), String> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_API_KEY)
        .and_then(|e| e.delete_credential())
        .map_err(|e| e.to_string())
}

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.chars().count() > 90 { line.chars().take(87).collect::<String>() + "…" } else { line.to_string() }
}

/// Short single-line rendering of a tool input for the trace row.
fn compact(v: &serde_json::Value) -> String {
    let s = v.to_string();
    if s == "{}" { String::new() } else { first_line(&s) }
}

fn platform_label() -> String {
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unknown".into());
    format!("{} ({session})", std::env::consts::OS)
}

/// `agent-unibrow --google-signin`: run Google's consent flow from the
/// terminal and save the token, without opening the app window.
fn google_signin() -> std::process::ExitCode {
    let auth = match rail_api::GoogleAuth::from_client_file() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Can't sign in: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    match rt.block_on(auth.sign_in()) {
        Ok(()) => {
            println!("Signed in to Gmail. The token is saved in your keyring; the app will use it on its next Gmail call.");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Sign-in failed: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn main() -> std::process::ExitCode {
    // Chromium re-runs this binary for its renderer/GPU/utility processes.
    // Those must hand off to CEF before anything else starts.
    if let Some(code) = rail_browser::run_subprocess_if_needed() {
        return std::process::ExitCode::from(code.clamp(0, 255) as u8);
    }
    if std::env::args().any(|a| a == "--google-signin") {
        return google_signin();
    }
    let store = Arc::new(Browsing::open_default().unwrap_or_else(|e| {
        eprintln!("agent-unibrow: history unavailable ({e}); using a temporary store");
        Browsing::in_memory()
    }));
    // Test hook (isolated test instances only): pre-save a login, "site|user|password".
    if std::env::var_os("UNIBROW_DATA_DIR").is_some() {
        if let Ok(spec) = std::env::var("UNIBROW_TEST_LOGIN") {
            let parts: Vec<&str> = spec.splitn(3, '|').collect();
            if let [site, user, pass] = parts[..] {
                if let Err(e) = rail_browser::vault::save(site, user, pass) {
                    eprintln!("agent-unibrow: test login not saved: {e}");
                }
            }
        }
    }
    let cef = rail_browser::initialize();
    if let Err(e) = &cef {
        eprintln!("agent-unibrow: browser unavailable: {e}");
    }
    Application::new().run(move |cx: &mut App| {
        text_input::bind_keys(cx);
        cx.bind_keys([
            gpui::KeyBinding::new("ctrl-b", ToggleSidebar, None),
            gpui::KeyBinding::new("escape", StopAgent, None),
            gpui::KeyBinding::new("ctrl-t", NewTab, None),
            gpui::KeyBinding::new("ctrl-w", CloseTab, None),
            gpui::KeyBinding::new("ctrl-tab", NextTab, None),
            gpui::KeyBinding::new("ctrl-shift-tab", PrevTab, None),
            gpui::KeyBinding::new("ctrl-l", FocusAddress, None),
            gpui::KeyBinding::new("ctrl-f", FindInPage, None),
            gpui::KeyBinding::new("alt-1", ShowChat, None),
            gpui::KeyBinding::new("alt-2", ShowHistory, None),
            gpui::KeyBinding::new("alt-3", ShowFocus, None),
            gpui::KeyBinding::new("alt-4", ShowTrace, None),
            gpui::KeyBinding::new("alt-5", ShowExplained, None),
        ]);
        let bounds = Bounds::centered(None, size(px(1400.), px(860.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("agent-unibrow".into()), ..Default::default() }),
                // A test instance can run under its own class (window rules).
                app_id: Some(std::env::var("UNIBROW_APP_ID").unwrap_or_else(|_| "agent-unibrow".into())),
                ..Default::default()
            },
            |window, cx| {
                // TitlebarOptions alone doesn't reach the Wayland toplevel title.
                window.set_window_title("agent-unibrow");
                cx.new(|cx| Shell::new(cef.clone(), store.clone(), window, cx))
            },
        )
        .expect("failed to open window");
        cx.activate(true);
        eprintln!("agent-unibrow: window opened on {}", platform_label());
    });
    rail_browser::shutdown();
    std::process::ExitCode::SUCCESS
}
