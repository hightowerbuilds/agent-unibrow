//! agent-unibrow shell.
//!
//! Three regions: conversation (left), content (center, where the web
//! engine composites in M2), and the trace pane (right). The agent runs on
//! its own thread; its events arrive here over a channel.

mod browser_pane;
mod chat_text;
mod chat_tools;
mod explained;
mod home;
mod jev_worker;
mod safety;
mod site_icons;
mod task_plan;
mod text_input;
mod theme;
mod watcher;

use agent_core::{AgentEvent, AgentHandle, Approval, BackendKind, Settings};
use gpui::{
    AnyView, App, Application, Bounds, Context, Entity, FocusHandle, Focusable, ScrollHandle, SharedString,
    StyleRefinement, Subscription, Task, TitlebarOptions, WeakEntity, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, rgb, size,
};
use policy::Policy;
use router::Registry;
use agent_core::models::ModelChoice;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use browser_pane::{AgentControl, BrowserPane, BrowsingChanged, FullscreenChanged};
use rail_browser::BrowserBridge;
use browsing::{Browsing, HistoryQuery, site_of};
use text_input::{Changed, Submit, TextInput};
use trace::{Kind, Trace};

gpui::actions!(
    shell,
    [ToggleSidebar, ShowChat, ShowHistory, ShowFocus, ShowTrace, ShowExplained, StopAgent, NewTab, CloseTab, NextTab, PrevTab, FocusAddress, FindInPage, ZoomIn, ZoomOut, ZoomReset]
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
    /// Typed while the agent was working (chats roadmap C3).
    Steer(SteerState),
}

#[derive(Clone, Copy, PartialEq)]
enum SteerState {
    /// Waiting for the agent's next step.
    Queued,
    Delivered,
    /// The turn was stopped before it got there.
    NotSent,
}

impl ItemKind {
    /// How the chat store names it.
    fn key(self) -> &'static str {
        match self {
            ItemKind::User => "user",
            ItemKind::Assistant => "assistant",
            ItemKind::Note => "note",
            ItemKind::Error => "error",
            ItemKind::System => "system",
            ItemKind::Steer(SteerState::Queued) => "steer_queued",
            ItemKind::Steer(SteerState::Delivered) => "steer",
            ItemKind::Steer(SteerState::NotSent) => "steer_not_sent",
        }
    }

    fn from_key(key: &str) -> Self {
        match key {
            "user" => ItemKind::User,
            "assistant" => ItemKind::Assistant,
            "note" => ItemKind::Note,
            "error" => ItemKind::Error,
            "steer" => ItemKind::Steer(SteerState::Delivered),
            // A queued message from a session that ended never arrived.
            "steer_queued" | "steer_not_sent" => ItemKind::Steer(SteerState::NotSent),
            _ => ItemKind::System,
        }
    }
}

const IDLE_PLACEHOLDER: &str = "Ask Claude to do something…";
const BUSY_PLACEHOLDER: &str = "Steer: add to what it's doing… (Enter)";

/// The name a backend's resume handle is saved under.
fn backend_key(kind: BackendKind) -> String {
    serde_json::to_value(kind).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

fn load_items(chats: &chats::Chats, id: chats::ChatId) -> Vec<Item> {
    chats
        .items(id)
        .into_iter()
        .map(|i| Item { kind: ItemKind::from_key(&i.kind), text: i.text, db: Some(i.id) })
        .collect()
}

struct Item {
    kind: ItemKind,
    text: String,
    /// Its row in the chat store; `None` for notices that aren't kept.
    db: Option<chats::ItemId>,
}

/// How often a streaming reply is saved while it grows.
const STREAM_SAVE_EVERY: Duration = Duration::from_secs(1);
/// Longest recap given to a model that can't resume a chat.
const RECAP_CHARS: usize = 12_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SidebarTab {
    Chat,
    /// History, Bookmarks and Trace, as sub-tabs ([`HistorySub`]).
    History,
    Focus,
    Explained,
}

/// The sub-tabs of the History tab.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HistorySub {
    Visits,
    Bookmarks,
    Trace,
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
    /// The Focus tab is showing one site's view (roadmap 8.6).
    site_view: Option<String>,
    login_site: Entity<TextInput>,
    login_user: Entity<TextInput>,
    login_pass: Entity<TextInput>,
    login_error: Option<String>,
    /// Saved chats (chats roadmap C1/C2).
    chats: Arc<chats::Chats>,
    /// The chat on screen, which is also the one the agent works in.
    chat_id: chats::ChatId,
    /// The All chats list is showing instead of the conversation.
    chat_list_open: bool,
    /// The model menu above the Model button.
    model_menu_open: bool,
    /// The agent whose models the menu is showing; None lists agents only.
    model_menu_agent: Option<BackendKind>,
    /// OpenCode's models, listed by `opencode models` when the menu first
    /// opens; None until then.
    opencode_models: Option<Vec<ModelChoice>>,
    /// Start the next agent without resuming the saved conversation (the
    /// API model changed, and a conversation can't move between models).
    skip_resume: bool,
    /// Jev (Phase 11): learning focus sites and handling requests about them.
    jev_jobs: async_channel::Sender<jev_worker::Job>,
    /// The latest learning line per site, while it learns.
    jev_progress: std::collections::HashMap<String, String>,
    /// The request Jev is working on: (id, text, site).
    jev_pending: Option<(u64, String, String)>,
    jev_next_id: u64,
    /// Requests Jev answered, for Claude's context with the next message.
    jev_context: Vec<String>,
    /// A chat whose Delete was clicked once and awaits the confirm click.
    confirm_delete: Option<chats::ChatId>,
    /// Rename, search and export (chat_tools.rs).
    chat_tools: chat_tools::ChatTools,
    /// The active chat's messages.
    items: Vec<Item>,
    /// The agent's checklist for this chat (7.4), shown above the messages.
    task_plan: Option<chats::TaskPlan>,
    /// Index of the assistant item currently receiving streamed text.
    streaming: Option<usize>,
    stream_saved: Instant,
    /// Context for a model that couldn't resume this chat; goes with the
    /// next message.
    pending_recap: Option<String>,
    /// The last message sent, for resending after a failed resume.
    last_sent: Option<String>,
    /// Interrupt was pressed: send the queued steering once stopped.
    interrupt_pending: bool,
    trace: Trace,
    expanded: HashSet<usize>,
    approvals: Vec<PendingApproval>,
    agent: Option<AgentHandle>,
    settings: Settings,
    policy: Arc<Mutex<Policy>>,
    busy: bool,
    /// Which keys are saved, for the home page (never the keys).
    home_keys: home::Keys,
    /// The home page asked for the chat box: focus it on the next render.
    focus_chat_pending: bool,
    status: SharedString,
    sidebar_open: bool,
    /// The sidebar, as its own cached view (see [`SidebarView`]).
    sidebar_view: Option<Entity<SidebarView>>,
    tab: SidebarTab,
    history_sub: HistorySub,
    /// The Explained tab's sub-tab.
    explained_page: usize,
    /// Favicons for History and Bookmarks rows.
    site_icons: site_icons::SiteIcons,
    needs_key: bool,
    conv_scroll: ScrollHandle,
    /// Text selection in the conversation, and its focus (for Ctrl+C).
    chat_select: chat_text::ChatSelect,
    chat_focus: FocusHandle,
    trace_scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
    _events: Option<Task<()>>,
}

impl Shell {
    fn new(cef: Result<(), String>, store: Arc<Browsing>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let chats = Arc::new(chats::Chats::open_default().unwrap_or_else(|e| {
            eprintln!("agent-unibrow: chats unavailable ({e}); using a temporary store");
            chats::Chats::in_memory()
        }));
        // Reopen the chat used last (C1.3).
        let chat_id = chats
            .last_active()
            .or_else(|| chats.open_chats().last().map(|c| c.id))
            .unwrap_or_else(|| chats.create());
        chats.mark_active(chat_id);
        let items = load_items(&chats, chat_id);
        let task_plan = chats.plan(chat_id);
        let input = cx.new(|cx| {
            let mut t = TextInput::new(IDLE_PLACEHOLDER, cx);
            t.set_wrap(true, cx);
            t
        });
        let (bridge, requests) = rail_browser::bridge::channel();
        let (jev_jobs, jev_events) = jev_worker::spawn(store.clone(), bridge.clone());
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
        let fullscreen_sub = cx.subscribe(&browser, |_: &mut Self, _, _: &FullscreenChanged, cx| cx.notify());
        window.focus(&input.focus_handle(cx));
        let (chat_tools, chat_tools_subs) = chat_tools::ChatTools::new(window, cx);

        // Site scope (6.3) checks against the focus sites.
        let mut policy = Policy::load();
        let focus = store.clone();
        policy.set_focus_source(Arc::new(move || focus.focus_sites().into_iter().map(|f| f.site).collect()));
        let policy = Arc::new(Mutex::new(policy));

        let mut shell = Self {
            focus_handle: cx.focus_handle(),
            input,
            browser,
            bridge,
            store,
            history_filter,
            can_continue: false,
            login_form_open: false,
            site_view: None,
            login_site,
            login_user,
            login_pass,
            login_error: None,
            chats,
            chat_id,
            chat_list_open: false,
            model_menu_open: false,
            model_menu_agent: None,
            opencode_models: None,
            skip_resume: false,
            jev_jobs,
            jev_progress: Default::default(),
            jev_pending: None,
            jev_next_id: 1,
            jev_context: Vec::new(),
            confirm_delete: None,
            chat_tools,
            items,
            task_plan,
            streaming: None,
            stream_saved: Instant::now(),
            pending_recap: None,
            last_sent: None,
            interrupt_pending: false,
            trace: Trace::open_session(),
            expanded: HashSet::new(),
            approvals: Vec::new(),
            agent: None,
            settings: Settings::load(),
            policy,
            busy: false,
            home_keys: home::Keys::default(),
            focus_chat_pending: false,
            status: "".into(),
            sidebar_open: true,
            sidebar_view: None,
            tab: SidebarTab::Chat,
            history_sub: HistorySub::Visits,
            explained_page: 0,
            site_icons: Default::default(),
            needs_key: false,
            conv_scroll: ScrollHandle::new(),
            chat_select: Default::default(),
            chat_focus: cx.focus_handle(),
            trace_scroll: ScrollHandle::new(),
            _subscriptions: vec![subscription, browsing_sub, filter_sub, fullscreen_sub, control_sub],
            _events: None,
        };
        shell._subscriptions.extend(chat_tools_subs);
        shell.spawn_chat_test(cx);
        if let Some(p) = shell.trace.path().cloned() {
            shell.trace.push(Kind::Info, "Trace file opened", Some(p.display().to_string()));
        }
        // The home page (homepage H2) reads the stores and the shell's status.
        home::attach(shell.store.clone(), shell.chats.clone());
        shell.check_home_keys(cx);
        let home_cmds = home::commands();
        cx.spawn(async move |this, cx| {
            while let Ok(cmd) = home_cmds.recv().await {
                if this.update(cx, |s, cx| s.home_cmd(cmd, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        // Watches (9.4): changes arrive from the checker thread.
        let fired = watcher::spawn(shell.store.clone(), shell.bridge.clone());
        cx.spawn(async move |this, cx| {
            while let Ok(f) = fired.recv().await {
                let ok = this.update(cx, |s, cx| {
                    match f {
                        Some(f) => s.watch_fired(f, cx),
                        None if matches!(s.tab, SidebarTab::Focus) => cx.notify(),
                        None => {}
                    }
                    s.publish_home(cx);
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            while let Ok(ev) = jev_events.recv().await {
                if this.update(cx, |s, cx| s.jev_event(ev, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
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
        // Test hook: steer the agent ("secs|text") that long after it starts working.
        if let Ok(spec) = std::env::var("UNIBROW_AUTOSTEER") {
            if let Some((secs, text)) = spec.split_once('|').and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.to_string()))) {
                cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor().timer(std::time::Duration::from_millis(250)).await;
                        if this.update(cx, |s, _| s.busy).unwrap_or(false) {
                            break;
                        }
                    }
                    cx.background_executor().timer(std::time::Duration::from_secs(secs)).await;
                    let _ = this.update(cx, |s, cx| s.submit(text, cx));
                })
                .detach();
            }
        }
        // Test hook (isolated test instances only): learn a site with Jev
        // first, as "site|start address"; an automatic task waits for it.
        let jev_learn = std::env::var("UNIBROW_TEST_JEV_LEARN")
            .ok()
            .filter(|_| std::env::var_os("UNIBROW_DATA_DIR").is_some())
            .and_then(|v| v.split_once('|').map(|(a, b)| (a.to_string(), b.to_string())));
        if let Some((site, start)) = jev_learn.clone() {
            shell.learn_with_jev_from(site, Some(start), cx);
        }
        // Test hook: send a task automatically once the page has loaded.
        if let Ok(task) = std::env::var("UNIBROW_AUTOSEND") {
            let learning = jev_learn.map(|(site, _)| site);
            cx.spawn(async move |this, cx| {
                if let Some(site) = learning {
                    for _ in 0..720 {
                        cx.background_executor().timer(std::time::Duration::from_millis(250)).await;
                        if !this.update(cx, |s, _| s.jev_progress.contains_key(&site)).unwrap_or(false) {
                            break;
                        }
                    }
                }
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
        self.notice(message.to_string());
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
        self.end_agent(cx);
        self.settings.backend = kind;
        self.settings.save();
        // The chat stays; the new backend gets a recap with the next message.
        self.pending_recap = self.chats.recap(self.chat_id, RECAP_CHARS);
        let model = self.model_label(kind);
        self.push(
            ItemKind::System,
            format!("Switched to {} · {model}. It gets a recap of this chat with your next message.", kind.label()),
        );
        self.trace.push(Kind::Info, format!("Switched agent to {} · {model}", kind.label()), None);
        self.start_backend(cx);
        cx.notify();
    }

    /// Picks a backend and model from the model menu.
    fn select_model(&mut self, kind: BackendKind, id: String, cx: &mut Context<Self>) {
        self.model_menu_open = false;
        if kind != self.settings.backend {
            self.settings.set_model(kind, id);
            self.switch_backend(kind, cx);
            return;
        }
        if self.settings.model(kind) == id && self.agent.is_some() {
            cx.notify();
            return;
        }
        self.end_agent(cx);
        self.settings.set_model(kind, id);
        self.settings.save();
        let model = self.model_label(kind);
        if kind == BackendKind::AnthropicApi {
            // The API conversation is tied to the model that wrote it.
            self.skip_resume = true;
            self.pending_recap = self.chats.recap(self.chat_id, RECAP_CHARS);
            self.push(ItemKind::System, format!("Switched to {model}. It gets a recap of this chat with your next message."));
        } else {
            self.push(ItemKind::System, format!("Switched to {model}. It carries on with this chat."));
        }
        self.trace.push(Kind::Info, format!("Switched model to {} · {model}", kind.label()), None);
        self.start_backend(cx);
        cx.notify();
    }

    fn model_label(&self, kind: BackendKind) -> String {
        agent_core::models::label(kind, self.settings.model(kind), self.opencode_models.as_deref().unwrap_or(&[]))
    }

    fn toggle_model_menu(&mut self, cx: &mut Context<Self>) {
        self.model_menu_open = !self.model_menu_open;
        self.model_menu_agent = None;
        if self.model_menu_open && self.opencode_models.is_none() {
            self.opencode_models = Some(vec![]);
            let task = cx.background_spawn(async { agent_core::models::opencode_models() });
            cx.spawn(async move |this, cx| {
                let models = task.await;
                let _ = this.update(cx, |this, cx| {
                    this.opencode_models = Some(models);
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }

    /// The Jev-ready site a request is about: one it names, or the one the
    /// page is on.
    fn jev_route(&self, text: &str, cx: &mut Context<Self>) -> Option<browsing::jev_sites::JevSite> {
        let ready: Vec<_> = self.store.jev_sites().into_iter().filter(|s| s.ready()).collect();
        if ready.is_empty() {
            return None;
        }
        let words: Vec<String> =
            text.to_lowercase().split(|c: char| !c.is_alphanumeric() && c != '.' && c != '-').map(str::to_string).collect();
        let named = ready.iter().find(|s| {
            rail_browser::tools::jev_site_words(&s.site).iter().any(|w| words.iter().any(|t| t == w || t.starts_with(&format!("{w}."))))
        });
        if let Some(s) = named {
            return Some(s.clone());
        }
        let (url, _) = self.browser.read(cx).location();
        let here = site_of(&url)?;
        let here = here.trim_start_matches("www.");
        ready.into_iter().find(|s| s.site == here)
    }

    fn learn_with_jev(&mut self, site: String, cx: &mut Context<Self>) {
        self.learn_with_jev_from(site, None, cx);
    }

    fn learn_with_jev_from(&mut self, site: String, start: Option<String>, cx: &mut Context<Self>) {
        // Test hook (isolated test instances only): where learning starts
        // when nothing else says, since test sites aren't at https://site/.
        let start = start.or_else(|| {
            std::env::var_os("UNIBROW_DATA_DIR").and(std::env::var("UNIBROW_TEST_JEV_START").ok())
        });
        self.jev_progress.insert(site.clone(), "Starting…".into());
        self.trace.push(Kind::Info, format!("Jev is learning {site}"), None);
        let _ = self.jev_jobs.try_send(jev_worker::Job::Learn { site, start });
        cx.notify();
    }

    fn jev_event(&mut self, ev: jev_worker::Event, cx: &mut Context<Self>) {
        use jev_worker::Event;
        match ev {
            Event::Progress { site, line } => {
                self.trace.push(Kind::Info, format!("Jev · {site}: {line}"), None);
                self.jev_progress.insert(site, line);
            }
            Event::Learned { site, result } => {
                self.jev_progress.remove(&site);
                let line = match result {
                    Ok(abilities) => format!("Jev learned {site}. It can now {} there without Claude.", abilities.join(", ")),
                    Err(e) => format!("Jev couldn't learn {site}: {e}"),
                };
                self.trace.push(Kind::Info, line.clone(), None);
                self.notice(line);
            }
            Event::Answer { id, text, calls, subq_calls, secs } => {
                let Some((_, asked, site)) = self.jev_pending.take().filter(|p| p.0 == id) else { return };
                self.push(ItemKind::Assistant, text.clone());
                let (by, decisions, model) = match (calls - subq_calls, subq_calls) {
                    (_, 0) => ("Jev", format!("{calls} decisions"), "jev"),
                    (0, s) => ("SubQ", format!("{s} decisions"), "subq"),
                    (j, s) => ("Jev and SubQ", format!("{j} Jev + {s} SubQ decisions"), "jev+subq"),
                };
                self.push(ItemKind::Note, format!("Answered by {by} on {site} · {decisions} · {secs:.1} s · no Claude tokens"));
                self.trace.push(
                    Kind::Assistant,
                    format!("Turn finished (jev, {decisions}, {secs:.1} s on {site})"),
                    Some(format!("{text}\n\nmodel: {model}")),
                );
                self.jev_context.push(format!("User: {asked}\nApp: {text}"));
                self.set_busy(false, "", cx);
                self.scroll_to_bottom();
            }
            Event::Fallback { id, reason } => {
                let Some((_, asked, _)) = self.jev_pending.take().filter(|p| p.0 == id) else { return };
                self.set_busy(false, "", cx);
                self.push(ItemKind::Note, format!("Jev: {reason}. Passing this to Claude."));
                self.trace.push(Kind::Info, format!("Jev passed to Claude: {reason}"), None);
                if self.agent.is_some() {
                    self.send_to_agent(asked, cx);
                } else {
                    self.push(ItemKind::Error, "No agent is running to take it.".into());
                }
                self.scroll_to_bottom();
            }
        }
        self.publish_home(cx);
        cx.notify();
    }

    /// Ends the running agent before another starts.
    fn end_agent(&mut self, cx: &mut Context<Self>) {
        // Dropping the handle ends the old agent; dropping pending approval
        // senders answers them with Deny.
        self.agent = None;
        self._events = None;
        self.approvals.clear();
        self.end_stream();
        self.set_busy(false, "", cx);
        if self.needs_key {
            self.needs_key = false;
            self.input.update(cx, |i, cx| {
                i.set_masked(false, cx);
                i.set_placeholder(IDLE_PLACEHOLDER, cx);
            });
        }
    }

    fn start_agent(&mut self, api_key: Option<String>, cx: &mut Context<Self>) {
        let mut registry = Registry::new();
        // Isolated test instances never get the user's real Gmail: they'd
        // be signed in through the shared keyring otherwise.
        let test_instance = std::env::var_os("UNIBROW_DATA_DIR").is_some()
            && std::env::var("UNIBROW_ALLOW_GMAIL").as_deref() != Ok("1");
        let mut codes: Option<Arc<dyn router::CodeSource>> = None;
        let notes = if test_instance {
            // Test hook: a fake inbox file stands in for Gmail's codes.
            if let Ok(path) = std::env::var("UNIBROW_TEST_INBOX") {
                codes = Some(Arc::new(rail_api::codes::FileCodes(path.into())));
            }
            vec!["Gmail tools off in this test instance".to_string()]
        } else {
            let (rail, notes) = rail_api::ApiRail::from_config();
            registry.add_rail(&rail);
            codes = rail.code_source();
            notes
        };
        for tool in browsing::tools(self.store.clone()) {
            registry.add_tool(tool);
        }
        for tool in rail_browser::tools::tools(self.bridge.clone(), self.store.clone(), codes) {
            registry.add_tool(tool);
        }
        // Files on the user's computer (a sandbox folder in test instances).
        registry.add_rail(&rail_files::FilesRail::new(rail_files::paths::Config::from_env(rail_browser::downloads_dir())));
        registry.add_tool(chats::TaskProgress::new(self.chats.clone(), self.chat_id));
        registry.add_tool(Arc::new(home::Customize));
        for n in &notes {
            self.trace.push(Kind::Info, n.clone(), None);
        }
        if !test_instance && notes.iter().any(|n| n.starts_with("Gmail tools off")) {
            let path = rail_api::client_secret_path().map(|p| p.display().to_string()).unwrap_or_default();
            self.notice(format!(
                "Gmail isn't connected yet. Save a Google OAuth client (Desktop app) as {path} and restart to enable it."
            ));
        }
        self.trace.push(Kind::Info, format!("Starting {}", self.settings.backend.label()), None);
        // Pick the chat up where it left off (C1.2), or recap it for a
        // backend that has never seen it.
        let backend_key = backend_key(self.settings.backend);
        let chat = self.chats.get(self.chat_id);
        let fresh = std::mem::take(&mut self.skip_resume);
        let resume = chat
            .as_ref()
            .filter(|c| !fresh && c.backend.as_deref() == Some(backend_key.as_str()))
            .and_then(|c| c.resume.clone());
        if resume.is_none() && self.pending_recap.is_none() {
            self.pending_recap = self.chats.recap(self.chat_id, RECAP_CHARS);
        }
        let (handle, events) =
            agent_core::spawn(self.settings.clone(), api_key, registry, self.policy.clone(), resume);
        self.agent = Some(handle);
        self._events = Some(cx.spawn(async move |this, cx| {
            while let Ok(ev) = events.recv().await {
                if this.update(cx, |shell, cx| shell.on_event(ev, cx)).is_err() {
                    break;
                }
            }
        }));
        self.publish_home(cx);
        cx.notify();
    }

    fn submit(&mut self, text: String, cx: &mut Context<Self>) {
        if self.needs_key {
            self.needs_key = false;
            self.input.update(cx, |i, cx| {
                i.set_masked(false, cx);
                i.set_placeholder(IDLE_PLACEHOLDER, cx);
            });
            match store_api_key(&text) {
                Ok(()) => self.trace.push(Kind::Info, "API key saved to keyring", None),
                Err(e) => self.trace.push(Kind::Error, format!("Couldn't save the API key ({e}); using it for this session only"), None),
            };
            self.notice("API key set.".into());
            self.start_agent(Some(text), cx);
            self.check_home_keys(cx);
            return;
        }
        if self.jev_pending.is_some() {
            self.notice("Jev is still on your last request. One moment.".into());
            cx.notify();
            return;
        }
        if !self.busy {
            if let Some(site) = self.jev_route(&text, cx) {
                self.can_continue = false;
                self.chat_list_open = false;
                self.push(ItemKind::User, text.clone());
                self.trace.push(Kind::User, first_line(&text), Some(text.clone()));
                let id = self.jev_next_id;
                self.jev_next_id += 1;
                self.trace.push(Kind::Info, format!("Jev handling a request on {}", site.site), None);
                self.jev_pending = Some((id, text.clone(), site.site.clone()));
                let _ = self.jev_jobs.try_send(jev_worker::Job::Handle { id, site, text });
                self.set_busy(true, "Jev is on it…", cx);
                self.scroll_to_bottom();
                cx.notify();
                return;
            }
        }
        if self.agent.is_none() {
            return;
        }
        if self.busy {
            // Steering (C3.1): goes to the agent mid-task.
            self.push(ItemKind::Steer(SteerState::Queued), text.clone());
            self.trace.push(Kind::User, format!("Steer: {}", first_line(&text)), Some(text.clone()));
            if let Some(agent) = &self.agent {
                agent.steer(text);
            }
            self.scroll_to_bottom();
            cx.notify();
            return;
        }
        self.can_continue = false;
        self.chat_list_open = false;
        self.push(ItemKind::User, text.clone());
        self.trace.push(Kind::User, first_line(&text), Some(text.clone()));
        self.send_to_agent(text, cx);
        self.scroll_to_bottom();
        cx.notify();
    }

    /// Sends a message the chat already shows.
    fn send_to_agent(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(agent) = &self.agent else { return };
        self.last_sent = Some(text.clone());
        // The agent always knows which sites are in focus.
        let mut message = match self.store.focus_summary() {
            Some(sites) => format!("[Focus sites: {sites}]\n{text}"),
            None => text,
        };
        // An unfinished task plan: say where it stands (7.4).
        if let Some(line) = self.chats.plan_reminder(self.chat_id) {
            message = format!("{line}\n{message}");
        }
        if !self.jev_context.is_empty() {
            message = format!(
                "[The app answered these on its own with Jev, before this message:]\n{}\n[End.]\n\n{message}",
                std::mem::take(&mut self.jev_context).join("\n")
            );
        }
        if let Some(recap) = self.pending_recap.take() {
            message = format!(
                "[Earlier in this chat, before you joined it. You can't see these messages otherwise:]\n{recap}\n[End of the earlier messages.]\n\n{message}"
            );
        }
        if agent.send(message) {
            self.set_busy(true, "Thinking…", cx);
        } else {
            self.push(ItemKind::Error, "The agent stopped. Restart the app.".into());
        }
    }

    /// Adds an item to the chat and saves it.
    fn push(&mut self, kind: ItemKind, text: String) {
        let db = Some(self.chats.append(self.chat_id, kind.key(), &text));
        self.items.push(Item { kind, text, db });
    }

    /// Shows a notice in the chat that isn't saved with it.
    fn notice(&mut self, text: String) {
        self.items.push(Item { kind: ItemKind::System, text, db: None });
    }

    /// Ends the streamed reply, saving its final text.
    fn end_stream(&mut self) {
        if let Some(i) = self.streaming.take() {
            if let Some(item) = self.items.get(i) {
                if let Some(db) = item.db {
                    self.chats.update_text(db, &item.text);
                }
            }
        }
    }

    /// Marks the oldest `count` queued steering messages as delivered.
    fn mark_steered(&mut self, count: usize, state: SteerState) {
        let mut left = count;
        for item in self.items.iter_mut() {
            if left == 0 {
                break;
            }
            if item.kind == ItemKind::Steer(SteerState::Queued) {
                item.kind = ItemKind::Steer(state);
                if let Some(db) = item.db {
                    self.chats.update_kind(db, item.kind.key());
                }
                left -= 1;
            }
        }
    }

    fn queued_steering(&self) -> Vec<String> {
        self.items.iter().filter(|i| i.kind == ItemKind::Steer(SteerState::Queued)).map(|i| i.text.clone()).collect()
    }

    /// Interrupt (C3.3): stop the current step and send what's in the box
    /// (and anything queued) as a new message straight away.
    fn interrupt(&mut self, cx: &mut Context<Self>) {
        if !self.busy {
            return;
        }
        let text = self.input.read(cx).text().trim().to_string();
        if !text.is_empty() {
            self.input.update(cx, |i, cx| i.set_text("", cx));
            self.push(ItemKind::Steer(SteerState::Queued), text.clone());
            self.trace.push(Kind::User, format!("Interrupt: {}", first_line(&text)), Some(text));
        }
        if self.queued_steering().is_empty() {
            self.stop_agent("Stopped.", cx);
            return;
        }
        self.interrupt_pending = true;
        self.stop_agent("Interrupted. Sending your message now.", cx);
    }

    fn set_busy(&mut self, busy: bool, status: &str, cx: &mut Context<Self>) {
        self.busy = busy;
        self.status = status.to_string().into();
        // The box stays open while the agent works: Enter steers it.
        self.input.update(cx, |i, cx| i.set_placeholder(if busy { BUSY_PLACEHOLDER } else { IDLE_PLACEHOLDER }, cx));
        self.browser.update(cx, |b, cx| b.set_agent_busy(busy, cx));
        self.publish_home(cx);
    }

    /// Tells the home page what the shell knows (agent, model, keys, Jev
    /// progress) and has open home tabs show it.
    fn publish_home(&mut self, cx: &mut Context<Self>) {
        let kind = self.settings.backend;
        let choices = BackendKind::ALL
            .into_iter()
            .filter(|k| *k != BackendKind::AnthropicApi)
            .map(|k| {
                let models = match k {
                    BackendKind::OpenCode => self.opencode_models.clone().unwrap_or_default(),
                    _ => agent_core::models::known(k),
                };
                (backend_id(k), k.label().to_string(), models.into_iter().map(|m| (m.id, m.label)).collect())
            })
            .collect();
        home::publish(home::Status {
            agent: kind.label().to_string(),
            model: self.model_label(kind),
            busy: self.busy,
            keys: self.home_keys,
            jev_progress: self.jev_progress.clone(),
            choices,
            backend: backend_id(kind),
            model_id: self.settings.model(kind).to_string(),
            trace: self
                .trace
                .entries()
                .iter()
                .rev()
                .take(10)
                .rev()
                .map(|e| {
                    let kind = serde_json::to_value(e.kind).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
                    (kind, e.summary.clone(), e.at.timestamp())
                })
                .collect(),
            settings: serde_json::to_value(&self.settings).unwrap_or_default(),
        });
        self.browser.update(cx, |b, _| b.refresh_home());
    }

    /// Does what the home page asked (homepage H3). The command was checked
    /// by `home::parse_cmd`; here it's checked against the app's state too
    /// (the site is a focus site, the model is one on offer).
    fn home_cmd(&mut self, cmd: home::Cmd, cx: &mut Context<Self>) {
        use home::Cmd;
        let done: Result<String, String> = match cmd {
            Cmd::Refused(why) => Err(why),
            Cmd::NewTab { url } => {
                self.browser.update(cx, |b, cx| b.open_in_new_tab(&url, cx));
                Ok(format!("opened {url} in a new tab"))
            }
            Cmd::OpenFolder { which } => match home::folder(&which) {
                // Test instances never open windows on the desktop.
                Some(dir) if std::env::var_os("UNIBROW_DATA_DIR").is_some() => {
                    Ok(format!("opened the {which} folder (test instance: not shown) {}", dir.display()))
                }
                Some(dir) => {
                    let _ = std::fs::create_dir_all(&dir);
                    match std::process::Command::new("xdg-open").arg(&dir).spawn() {
                        Ok(_) => Ok(format!("opened the {which} folder")),
                        Err(e) => Err(format!("couldn't open {}: {e}", dir.display())),
                    }
                }
                None => Err(format!("no folder {which}")),
            },
            Cmd::Show { tab, site, page } => {
                self.sidebar_open = true;
                self.site_view = None;
                match tab.as_str() {
                    "chat" => {
                        self.tab = SidebarTab::Chat;
                        self.focus_chat_pending = true;
                    }
                    "history" | "bookmarks" | "trace" => {
                        self.tab = SidebarTab::History;
                        self.history_sub = match tab.as_str() {
                            "bookmarks" => HistorySub::Bookmarks,
                            "trace" => HistorySub::Trace,
                            _ => HistorySub::Visits,
                        };
                    }
                    "explained" => {
                        self.tab = SidebarTab::Explained;
                        if let Some(p) = page {
                            self.explained_page = p;
                        }
                    }
                    _ => {
                        self.tab = SidebarTab::Focus;
                        if let Some(site) = site.filter(|s| self.store.is_focused(s)) {
                            self.site_view = Some(site);
                        }
                    }
                }
                Ok(format!("showed {tab}"))
            }
            Cmd::FillChat { text } => {
                self.tab = SidebarTab::Chat;
                self.sidebar_open = true;
                self.input.update(cx, |i, cx| i.set_text(&text, cx));
                self.focus_chat_pending = true;
                Ok("filled the chat box".into())
            }
            Cmd::Ask { text } => {
                self.tab = SidebarTab::Chat;
                self.sidebar_open = true;
                self.submit(text, cx);
                Ok("asked the agent".into())
            }
            Cmd::SetAgent { backend, model } => {
                match serde_json::from_value::<BackendKind>(serde_json::json!(backend)) {
                    Ok(kind) if kind != BackendKind::AnthropicApi => {
                        let offered = match kind {
                            BackendKind::OpenCode => self.opencode_models.clone().unwrap_or_default(),
                            _ => agent_core::models::known(kind),
                        };
                        if model.is_empty() {
                            self.model_menu_open = false;
                            self.switch_backend(kind, cx);
                            Ok(format!("set the agent to {}", kind.label()))
                        } else if offered.iter().any(|m| m.id == model) {
                            self.select_model(kind, model.clone(), cx);
                            Ok(format!("set the agent to {} · {model}", kind.label()))
                        } else {
                            Err(format!("{model} isn't a {} model on offer", kind.label()))
                        }
                    }
                    _ => Err(format!("unknown agent {backend}")),
                }
            }
            Cmd::LearnJev { site } if self.store.is_focused(&site) || self.store.is_focused(&format!("www.{site}")) => {
                let key = site.trim_start_matches("www.").to_string();
                self.learn_with_jev(key.clone(), cx);
                Ok(format!("learn_jev {key}"))
            }
            Cmd::ToggleJev { site } => {
                let key = site.trim_start_matches("www.").to_string();
                match self.store.jev_site(&key).filter(|j| j.status == "ready") {
                    Some(j) => {
                        self.store.set_jev_enabled(&key, !j.enabled);
                        Ok(format!("turned Jev {} for {key}", if j.enabled { "off" } else { "on" }))
                    }
                    None => Err(format!("Jev hasn't learned {key}")),
                }
            }
            Cmd::LearnJev { site } => Err(format!("{site} isn't a focus site")),
            Cmd::CheckWatch { id } => {
                self.store.check_watch_soon(id);
                Ok(format!("check_watch #{id}"))
            }
            Cmd::RemoveWatch { id } => {
                self.store.remove_watch(id);
                Ok(format!("removed watch #{id}"))
            }
            Cmd::RemoveBookmark { url } => {
                self.store.remove_bookmark(&url);
                Ok(format!("removed the bookmark {url}"))
            }
            Cmd::SaveLayout { layout } => match layout.save() {
                Ok(()) => Ok("saved the layout".into()),
                Err(e) => Err(format!("couldn't save the layout: {e}")),
            },
            Cmd::Pin { url, title } => {
                if self.store.pin(&url, &title) {
                    Ok(format!("pinned {url}"))
                } else {
                    Err(format!("{url} is already pinned, or there are {} pins", browsing::home_pins::MAX_PINS))
                }
            }
            Cmd::Unpin { url } => {
                self.store.unpin(&url);
                Ok(format!("unpinned {url}"))
            }
            Cmd::MovePin { url, by } => {
                self.store.move_pin(&url, by);
                Ok(format!("moved the pin {url}"))
            }
            Cmd::Customized(what) => Ok(format!("the agent changed the page: {what}")),
        };
        match done {
            Ok(what) => self.trace.push(Kind::Info, format!("Home: {what}"), None),
            Err(why) => self.trace.push(Kind::Info, format!("Home: refused: {why}"), None),
        };
        self.publish_home(cx);
        cx.notify();
    }

    /// Looks up which keys are saved (off the UI thread: it reads the
    /// keyring), then updates the home page.
    fn check_home_keys(&mut self, cx: &mut Context<Self>) {
        let task = cx.background_spawn(async { home::Keys::check(load_api_key().is_some()) });
        cx.spawn(async move |this, cx| {
            let keys = task.await;
            let _ = this.update(cx, |this, cx| {
                this.home_keys = keys;
                this.publish_home(cx);
            });
        })
        .detach();
    }

    /// Stops the agent's current turn (Stop button, Esc, or the user taking
    /// over the page).
    fn stop_agent(&mut self, note: &str, cx: &mut Context<Self>) {
        if !self.busy {
            return;
        }
        // Jev's request: its answer is dropped when it arrives.
        if self.jev_pending.take().is_some() {
            self.push(ItemKind::System, "Stopped.".into());
            self.trace.push(Kind::Info, "Stopped Jev's request".to_string(), None);
            self.set_busy(false, "", cx);
            cx.notify();
            return;
        }
        if let Some(agent) = &self.agent {
            agent.cancel();
        }
        // Dropping the senders answers pending approvals with Deny.
        self.approvals.clear();
        self.end_stream();
        self.push(ItemKind::System, note.to_string());
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
                self.end_stream();
                if !self.busy {
                    // A follow-up turn carrying steering.
                    self.set_busy(true, "Thinking…", cx);
                }
                self.status = "Thinking…".into();
            }
            AgentEvent::TextDelta(t) => {
                let idx = match self.streaming {
                    Some(i) => i,
                    None => {
                        self.push(ItemKind::Assistant, String::new());
                        self.stream_saved = Instant::now();
                        self.streaming = Some(self.items.len() - 1);
                        self.items.len() - 1
                    }
                };
                self.items[idx].text.push_str(&t);
                // Saved as it grows, so a crash loses at most a second.
                if self.stream_saved.elapsed() >= STREAM_SAVE_EVERY {
                    self.stream_saved = Instant::now();
                    if let Some(db) = self.items[idx].db {
                        self.chats.update_text(db, &self.items[idx].text);
                    }
                }
            }
            AgentEvent::Resume(handle) => {
                self.chats.set_resume(self.chat_id, &backend_key(self.settings.backend), Some(&handle));
            }
            AgentEvent::ResumeFailed => {
                // The saved session is gone: resend the last message with a recap.
                self.end_stream();
                if let Some(text) = self.last_sent.clone() {
                    let recap = self.chats.recap(self.chat_id, RECAP_CHARS);
                    self.pending_recap = recap.map(|r| format!("{r}\n(Answer the last user message.)"));
                    self.send_to_agent(text, cx);
                }
            }
            AgentEvent::Steered { count } => {
                self.mark_steered(count, SteerState::Delivered);
                self.trace.push(Kind::Info, format!("Steering delivered ({count} message{})", if count == 1 { "" } else { "s" }), None);
            }
            AgentEvent::Note(n) => {
                self.end_stream();
                self.push(ItemKind::Note, n.clone());
                self.trace.push(Kind::Assistant, format!("Note: {}", first_line(&n)), Some(n));
            }
            AgentEvent::ToolCall { name, input, .. } => {
                self.end_stream();
                self.status = format!("Running {name}").into();
                let pretty = serde_json::to_string_pretty(&input).unwrap_or_default();
                self.trace.push(Kind::ToolCall, format!("{name} {}", compact(&input)), Some(pretty));
            }
            AgentEvent::ToolResult { name, ok, summary, content, evidence, .. } => {
                if name == "task_progress" {
                    self.task_plan = self.chats.plan(self.chat_id);
                }
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
                self.end_stream();
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
                let queued = self.queued_steering();
                if stop_reason == "cancelled" {
                    if std::mem::take(&mut self.interrupt_pending) && !queued.is_empty() {
                        // Interrupt: what was queued goes as the next message.
                        self.mark_steered(queued.len(), SteerState::Delivered);
                        self.set_busy(false, "", cx);
                        self.send_to_agent(queued.join("\n\n"), cx);
                    } else {
                        self.mark_steered(queued.len(), SteerState::NotSent);
                        self.set_busy(false, "", cx);
                    }
                } else if queued.is_empty() {
                    self.set_busy(false, "", cx);
                }
                // Otherwise the agent sends the queued messages as its next
                // turn and stays busy.
            }
            AgentEvent::Error(e) => {
                self.end_stream();
                self.push(ItemKind::Error, e.clone());
                self.trace.push(Kind::Error, first_line(&e), Some(e.clone()));
                self.set_busy(false, "", cx);
                if e.contains("API 401") && self.settings.backend.needs_api_key() {
                    let _ = delete_api_key();
                    self.check_home_keys(cx);
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
            SidebarTab::History if self.history_sub == HistorySub::Visits => {
                window.focus(&self.history_filter.focus_handle(cx))
            }
            _ => {}
        }
        self.scroll_to_bottom();
        cx.notify();
    }

    /// The sidebar tabs (Chat, History, Focus, Explained).
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
        // Refilled as this frame's message texts prepaint.
        self.chat_select.registry.clear();
        let approvals: Vec<_> = self.approvals.iter().map(|a| self.render_approval(a, cx).into_any_element()).collect();
        let items: Vec<_> =
            self.items.iter().enumerate().map(|(i, item)| self.render_item(i, item, cx).into_any_element()).collect();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .child(self.render_chat_strip(cx))
            .when(self.chat_list_open, |d| d.child(self.render_chat_list(cx)))
            .when_some(self.task_plan.as_ref().filter(|_| !self.chat_list_open), |d, p| d.child(task_plan::render(p)))
            .when(!self.chat_list_open, |d| d.child(
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
                    .track_focus(&self.chat_focus)
                    .key_context(chat_text::CONTEXT)
                    .on_action(cx.listener(Self::copy_chat_selection))
                    .on_action(cx.listener(Self::select_all_chat))
                    .on_action(cx.listener(Self::paste_into_chat_input))
                    .on_key_down(cx.listener(Self::chat_key_down))
                    .on_mouse_down(gpui::MouseButton::Left, cx.listener(Self::chat_mouse_down))
                    .on_mouse_down(gpui::MouseButton::Right, cx.listener(Self::chat_right_click))
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
                    .children(items)
                    .children(approvals)
                    .child(self.chat_drag_tracker(cx)),
            ))
            .when_some(self.chat_select.menu, |d, (at, item)| d.child(self.render_chat_menu(at, item, cx)))
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
                    .child(self.input.clone())
                    .when(self.busy, |d| {
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(div().flex_1().text_xs().text_color(rgb(theme::MUTED)).child(
                                    "Enter adds to what it's doing; it reads it at its next step.",
                                ))
                                .child(
                                    Self::small_button("interrupt".into(), "Interrupt & send", false)
                                        .on_click(cx.listener(|this, _, _, cx| this.interrupt(cx))),
                                ),
                        )
                    }),
            )
    }

    fn watch_fired(&mut self, f: browsing::WatchFired, cx: &mut Context<Self>) {
        let line = watcher::describe(&f);
        self.push(ItemKind::System, format!("◷ Watch: {line}\n{}", f.watch.url));
        self.trace.push(Kind::Info, format!("Watch fired: {line}"), Some(f.watch.url.clone()));
        cx.notify();
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

    /// The Bookmarks sub-tab.
    fn render_bookmarks(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let marks = self.store.bookmarks();
        let empty = marks.is_empty();
        div()
            .id("bookmarks-list")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .py_1()
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
                    .pl(px(10.))
                    .pr_3()
                    .py_1()
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::RAISED)))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_url(url.clone(), cx)))
                    .child(self.site_icons.view(&self.store, &m.site))
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
    }

    /// The History tab: History, Bookmarks and Trace sub-tabs.
    fn render_history_tab(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let sub = self.history_sub;
        let marks = self.store.bookmarks().len();
        let pill = |key: HistorySub, id: &'static str, label: String| {
            let selected = sub == key;
            div()
                .id(id)
                .px_3()
                .py(px(3.))
                .rounded_md()
                .text_xs()
                .text_color(rgb(if selected { theme::FG } else { theme::MUTED }))
                .when(selected, |d| d.bg(rgb(theme::RAISED)))
                .when(!selected, |d| d.cursor_pointer().hover(|s| s.text_color(rgb(theme::FG))))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.history_sub = key;
                    if key == HistorySub::Visits {
                        window.focus(&this.history_filter.focus_handle(cx));
                    }
                    if key == HistorySub::Trace {
                        this.trace_scroll.scroll_to_bottom();
                    }
                    cx.notify();
                }))
                .child(label)
        };
        let count = |n: usize, label: &str| if n == 0 { label.to_string() } else { format!("{label} · {n}") };
        let bar = div()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(rgb(theme::RULE))
            .child(pill(HistorySub::Visits, "history-sub-visits", "History".into()))
            .child(pill(HistorySub::Bookmarks, "history-sub-bookmarks", count(marks, "Bookmarks")))
            .child(pill(HistorySub::Trace, "history-sub-trace", count(self.trace.entries().len(), "Trace")));
        let body = match sub {
            HistorySub::Visits => self.render_history(cx).into_any_element(),
            HistorySub::Bookmarks => self.render_bookmarks(cx).into_any_element(),
            HistorySub::Trace => self.render_trace(cx).into_any_element(),
        };
        div().flex().flex_col().flex_1().min_h(px(0.)).child(bar).child(body).into_any_element()
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
                    .child(self.site_icons.view(&self.store, &v.site))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .child(div().text_sm().text_color(rgb(theme::FG)).truncate().child(label))
                            .child(div().text_xs().text_color(rgb(theme::MUTED)).truncate().child(v.site.clone())),
                    )
                    .child(self.site_icons.view(&self.store, &v.site))
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

    fn render_focus(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if let Some(site) = self.site_view.clone() {
            return self.render_site_view(&site, cx).into_any_element();
        }
        let sites = self.store.focus_sites();
        let (current_url, current_title) = self.browser.read(cx).location();
        let current_site = site_of(&current_url);
        let current_in_focus = current_site.as_deref().is_some_and(|s| self.store.is_focused(s));

        let safety = self.render_safety(cx).into_any_element();
        let logins = self.render_logins(cx).into_any_element();
        let watches = self.render_watches(cx).into_any_element();
        let jev: std::collections::HashMap<String, browsing::jev_sites::JevSite> =
            self.store.jev_sites().into_iter().map(|j| (j.site.clone(), j)).collect();
        let rows = sites.into_iter().map(|f| {
            let site = f.site.clone();
            let key = f.site.trim_start_matches("www.").to_ascii_lowercase();
            let learned = jev.get(&key).cloned();
            let progress = self.jev_progress.get(&key).cloned();
            let jev_line = home::jev_line(progress.as_deref(), learned.as_ref());
            let (jev_label, jev_on) = match (&progress, &learned) {
                (Some(_), _) => ("Learning…", true),
                (None, Some(j)) if j.status == "ready" && j.enabled => ("Jev ✓", true),
                (None, Some(j)) if j.status == "ready" => ("Jev off", false),
                (None, Some(j)) if j.status == "failed" => ("Retry Jev", false),
                _ => ("Learn with Jev", false),
            };
            let jev_button = Self::small_button(SharedString::from(format!("focus-jev-{}", f.site)), jev_label, jev_on)
                .on_click(cx.listener({
                    let (key, learning) = (key.clone(), progress.is_some());
                    let state = learned.as_ref().map(|j| (j.status.clone(), j.enabled));
                    move |this, _, _, cx| {
                        if learning {
                            return;
                        }
                        match &state {
                            Some((status, enabled)) if status == "ready" => {
                                this.store.set_jev_enabled(&key, !enabled);
                                cx.notify();
                            }
                            _ => this.learn_with_jev(key.clone(), cx),
                        }
                    }
                }));
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
                        )
                        .children(jev_line.map(|l| div().text_xs().text_color(rgb(theme::ACCENT)).truncate().child(l))),
                )
                .child(jev_button)
                .child(
                    Self::small_button(SharedString::from(format!("focus-view-{}", f.site)), "View", false).on_click(
                        cx.listener({
                            let site = site.clone();
                            move |this, _, _, cx| {
                                this.site_view = Some(site.clone());
                                cx.notify();
                            }
                        }),
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
            .child(safety)
            .child(logins)
            .child(watches)
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
            .into_any_element()
    }

    /// Everything about one focus site in one place (8.6): its playbooks,
    /// the site map's pages, the agent's recent actions, and history.
    fn render_site_view(&self, site: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let map = self.store.site_map(site);
        let actions = self.store.site_actions(site, 25);
        let visits =
            self.store.history(&HistoryQuery { site: Some(site.to_string()), limit: 40, ..Default::default() });
        let heading = |t: &'static str| div().pt_2().text_xs().text_color(rgb(theme::ACCENT)).child(t);
        let muted = |t: String| div().text_xs().text_color(rgb(theme::MUTED)).child(t);
        let mut col = div()
            .id("site-view")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .p_3()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Self::small_button("site-view-back".into(), "← Focus sites", false).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.site_view = None;
                            cx.notify();
                        },
                    )))
                    .child(div().flex_1().text_sm().text_color(rgb(theme::FG)).child(map.site.clone())),
            );

        col = col.child(heading("JEV"));
        let key = site.trim_start_matches("www.").to_ascii_lowercase();
        match (self.jev_progress.get(&key), self.store.jev_site(&key)) {
            (Some(p), _) => col = col.child(muted(format!("Learning… {p}"))),
            (None, None) => {
                col = col.child(muted(
                    "Not learned yet. Learn with Jev explores the site in the background so Jev can handle requests \
                     about it (find, price, stock, open, cart) without Claude."
                        .into(),
                ));
            }
            (None, Some(j)) => {
                let status = match (j.status.as_str(), j.enabled) {
                    ("ready", true) => format!("Ready: {}", j.skills.abilities().join(" · ")),
                    ("ready", false) => "Learned, but off: requests go to Claude".to_string(),
                    _ => "Learning failed".to_string(),
                };
                col = col.child(div().text_sm().text_color(rgb(theme::FG)).child(status));
                for (label, value) in [
                    ("Search", j.skills.search_url.clone()),
                    ("Results", j.skills.result_pattern.clone()),
                    ("Products", j.skills.product_pattern.clone()),
                    ("Cart", j.skills.cart_url.clone()),
                ] {
                    col = col.child(muted(format!("{label}: {}", value.unwrap_or_else(|| "—".into()))));
                }
                for line in &j.log {
                    col = col.child(div().pl_2().text_xs().text_color(rgb(theme::MUTED)).child(format!("· {line}")));
                }
            }
        }
        {
            let (k1, k2) = (key.clone(), key.clone());
            let learned = self.store.jev_site(&key).is_some();
            col = col.child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Self::small_button("site-jev-learn".into(), if learned { "Relearn" } else { "Learn with Jev" }, false)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.jev_progress.contains_key(&k1) {
                                    this.learn_with_jev(k1.clone(), cx);
                                }
                            })),
                    )
                    .when(learned, |d| {
                        d.child(Self::small_button("site-jev-forget".into(), "Forget", false).on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.store.remove_jev_site(&k2);
                                cx.notify();
                            },
                        )))
                    }),
            );
        }

        col = col.child(heading("PLAYBOOKS"));
        if map.playbooks.is_empty() {
            col = col.child(muted(
                "None yet. After the agent does something here you'll want again, ask it to save that as a playbook.".into(),
            ));
        }
        for p in &map.playbooks {
            let id = p.id;
            let record = if p.runs == 0 { "never run".to_string() } else { format!("ran {}×, {} finished", p.runs, p.successes) };
            let params = if p.params.is_empty() { String::new() } else { format!(" · needs {}", p.params.join(", ")) };
            let steps: Vec<String> = p.steps.iter().enumerate().map(|(i, s)| format!("{}. {}", i + 1, s.describe())).collect();
            col = col.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(theme::RULE))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().flex_1().text_sm().text_color(rgb(theme::FG)).child(format!("“{}”", p.name)))
                            .child(Self::small_button(SharedString::from(format!("playbook-remove-{id}")), "Remove", false).on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.store.remove_playbook(id);
                                    this.trace.push(Kind::Info, format!("Removed playbook #{id}"), None);
                                    cx.notify();
                                }),
                            )),
                    )
                    .child(muted(format!("{}{params} · {record}", p.description)))
                    .children(p.last_error.clone().map(|e| {
                        div().text_xs().text_color(rgb(theme::WARN)).child(format!("Last run stopped: {e}"))
                    }))
                    .children(steps.into_iter().map(|t| div().text_xs().text_color(rgb(theme::FG)).child(t))),
            );
        }

        col = col.child(heading("PAGES"));
        if map.pages.is_empty() {
            col = col.child(muted("No pages known yet.".into()));
        }
        for p in map.pages.iter().take(15) {
            let extra = if p.visits > 0 { format!(" · {} visits", p.visits) } else { String::new() };
            col = col.child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().text_sm().truncate().text_color(rgb(theme::FG)).child(format!("{} ({}){extra}", p.pattern, p.kind)))
                    .when(!p.elements.is_empty(), |d| {
                        d.child(muted(format!(
                            "used: {}",
                            p.elements.iter().map(|(l, n)| format!("{l} ×{n}")).collect::<Vec<_>>().join(", ")
                        )))
                    }),
            );
        }

        col = col.child(heading("AGENT'S RECENT ACTIONS"));
        if actions.is_empty() {
            col = col.child(muted("None recorded. Actions are recorded while a site is in focus.".into()));
        }
        for a in actions.iter().rev() {
            col = col.child(
                div()
                    .flex()
                    .gap_2()
                    .text_xs()
                    .child(div().flex_none().text_color(rgb(theme::MUTED)).child(a.at.format("%b %-d %-I:%M %p").to_string()))
                    .child(div().flex_1().min_w(px(0.)).truncate().text_color(rgb(theme::FG)).child(format!(
                        "{} · {} → {}",
                        a.step.describe(),
                        a.step.from,
                        a.step.to
                    ))),
            );
        }

        col = col.child(heading("HISTORY ON THIS SITE"));
        if visits.is_empty() {
            col = col.child(muted("No visits yet.".into()));
        }
        for v in visits {
            let url = v.url.clone();
            let title = if v.title.is_empty() { v.url.clone() } else { v.title.clone() };
            col = col.child(
                div()
                    .id(SharedString::from(format!("site-visit-{}", v.id)))
                    .flex()
                    .gap_2()
                    .text_xs()
                    .cursor_pointer()
                    .hover(|s| s.text_color(rgb(theme::ACCENT)))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_url(url.clone(), cx)))
                    .child(div().flex_none().text_color(rgb(theme::MUTED)).child(v.at.format("%b %-d %-I:%M %p").to_string()))
                    .child(div().flex_1().min_w(px(0.)).truncate().text_color(rgb(theme::FG)).child(title)),
            );
        }
        col
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

    /// Watches (roadmap 9.4): pages the app re-checks for the user. Agents
    /// add them with browser_watch; the user can remove them here.
    fn render_watches(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let watches = self.store.watches();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .border_b_1()
            .border_color(rgb(theme::RULE))
            .child(div().text_xs().text_color(rgb(theme::ACCENT)).child("WATCHES"))
            .when(watches.is_empty(), |d| {
                d.child(div().text_xs().text_color(rgb(theme::MUTED)).child(
                    "None yet. Ask the agent things like \"tell me when this price drops\" or \"let me know when it's back in stock\".",
                ))
            })
            .children(watches.into_iter().map(|w| {
                let id = w.id;
                let url = w.url.clone();
                let mut status = match (&w.last_value, &w.last_checked) {
                    (Some(v), Some(at)) => format!("{v} · checked {}", at.format("%b %-d %-I:%M %p")),
                    _ => "not checked yet".to_string(),
                };
                if let Some(e) = &w.last_error {
                    status = format!("{status} · last check failed: {e}");
                }
                let every = if w.every_min % 60 == 0 { format!("{}h", w.every_min / 60) } else { format!("{}m", w.every_min) };
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id(("watch-open", id as usize))
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| this.open_url(url.clone(), cx)))
                            .child(div().text_sm().truncate().text_color(rgb(theme::FG)).child(format!(
                                "{} · when it {} · every {every}",
                                w.label, w.when
                            )))
                            .child(div().text_xs().truncate().text_color(if w.last_error.is_some() {
                                rgb(theme::ERROR)
                            } else {
                                rgb(theme::MUTED)
                            }).child(status)),
                    )
                    .child(Self::small_button(SharedString::from(format!("watch-remove-{id}")), "Remove", false).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.store.remove_watch(id);
                            this.trace.push(Kind::Info, format!("Removed watch #{id}"), None);
                            cx.notify();
                        }),
                    ))
            }))
    }

    /// The sidebar's contents. Rendered by [`SidebarView`].
    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.tab {
            SidebarTab::Chat => self.render_chat(cx).into_any_element(),
            SidebarTab::History => self.render_history_tab(cx),
            SidebarTab::Focus => self.render_focus(cx).into_any_element(),
            SidebarTab::Explained => {
                let me = cx.entity().downgrade();
                let me2 = me.clone();
                explained::render(
                    self.explained_page,
                    move |page, _, cx| {
                        let _ = me2.update(cx, |this, cx| {
                            this.explained_page = page;
                            cx.notify();
                        });
                    },
                    move |text, window, cx| {
                        let _ = me.update(cx, |this, cx| this.use_example(text, window, cx));
                    },
                )
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
            .relative()
            .child(self.render_tabs(cx))
            .child(div().flex().flex_col().flex_1().min_h(px(0.)).child(body))
            .child(self.render_model_bar(cx))
            .when(self.model_menu_open, |d| {
                d.child(
                    // Clicking anywhere else closes the menu.
                    div()
                        .id("model-menu-backdrop")
                        .absolute()
                        .inset_0()
                        .occlude()
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.model_menu_open = false;
                            cx.notify();
                        }))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.model_menu_open = false;
                            cx.notify();
                        })),
                )
                .child(self.render_model_menu(cx))
            })
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

    /// The Model button: which agent and model, opening the model menu.
    fn render_model_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let kind = self.settings.backend;
        let status: Option<SharedString> = (!self.status.is_empty()).then(|| self.status.clone());
        let open = self.model_menu_open;
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .flex_none()
            .border_t_1()
            .border_color(rgb(theme::RULE))
            .child(
                div()
                    .id("model-button")
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py(px(3.))
                    .rounded_md()
                    .text_xs()
                    .border_1()
                    .border_color(rgb(if open { theme::ACCENT } else { theme::RULE }))
                    .bg(rgb(theme::RAISED))
                    .cursor_pointer()
                    .hover(|s| s.border_color(rgb(theme::ACCENT)))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_model_menu(cx)))
                    .child(div().text_color(rgb(theme::MUTED)).child("Model"))
                    .child(
                        div()
                            .text_color(rgb(theme::FG))
                            .child(format!("{} · {}", kind.label(), self.model_label(kind))),
                    )
                    .child(div().text_color(rgb(theme::MUTED)).child(if open { "▾" } else { "▴" })),
            )
            .child(div().flex_1())
            .children(status.map(|s| div().text_xs().text_color(rgb(theme::MUTED)).child(s)))
    }

    /// The agents; clicking one shows its models, and picking a model
    /// switches to it.
    fn render_model_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = (self.settings.backend, self.settings.model(self.settings.backend).to_string());
        let mut rows: Vec<gpui::AnyElement> = vec![];
        // The API-key backend is left out of the menu for now.
        for kind in BackendKind::ALL.into_iter().filter(|k| *k != BackendKind::AnthropicApi) {
            let expanded = self.model_menu_agent == Some(kind);
            let in_use = current.0 == kind;
            let plan = match kind {
                BackendKind::ClaudeCode => "Your Claude plan",
                BackendKind::Codex => "Your ChatGPT plan",
                BackendKind::OpenCode => "Your providers",
                BackendKind::AnthropicApi => "Billed to your API key",
            };
            let summary = if in_use { format!("In use · {}", self.model_label(kind)) } else { plan.to_string() };
            rows.push(
                div()
                    .id(SharedString::from(format!("model-agent-{}", kind.label())))
                    .mx_1()
                    .px_2()
                    .py(px(6.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(expanded, |d| d.bg(rgb(theme::RAISED)))
                    .hover(|s| s.bg(rgb(theme::RAISED)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.model_menu_agent = if this.model_menu_agent == Some(kind) { None } else { Some(kind) };
                        cx.notify();
                    }))
                    .child(div().w(px(12.)).text_xs().text_color(rgb(theme::MUTED)).child(if expanded { "▾" } else { "▸" }))
                    .child(div().text_sm().text_color(rgb(if in_use { theme::ACCENT } else { theme::FG })).child(kind.label()))
                    .child(div().flex_1())
                    .child(div().text_xs().text_color(rgb(theme::MUTED)).child(summary))
                    .into_any_element(),
            );
            if !expanded {
                continue;
            }
            let models = match kind {
                BackendKind::OpenCode => self.opencode_models.clone().unwrap_or_default(),
                _ => agent_core::models::known(kind),
            };
            if models.is_empty() {
                let note = if kind == BackendKind::OpenCode {
                    "Loading… (or none yet: sign in with `opencode auth login`)"
                } else {
                    "No models found"
                };
                rows.push(div().pl(px(36.)).pr_3().py_1().text_xs().italic().text_color(rgb(theme::MUTED)).child(note).into_any_element());
            }
            for m in models {
                let selected = in_use && current.1 == m.id;
                let id = m.id.clone();
                rows.push(
                    div()
                        .id(SharedString::from(format!("model-{}-{}", kind.label(), m.id)))
                        .mx_1()
                        .pl(px(28.))
                        .pr_2()
                        .py(px(4.))
                        .flex()
                        .items_center()
                        .gap_2()
                        .rounded_sm()
                        .text_sm()
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(theme::RAISED)))
                        .on_click(cx.listener(move |this, _, _, cx| this.select_model(kind, id.clone(), cx)))
                        .child(div().w(px(12.)).text_color(rgb(theme::ACCENT)).child(if selected { "✓" } else { "" }))
                        .child(div().text_color(rgb(if selected { theme::ACCENT } else { theme::FG })).child(m.label))
                        .child(div().flex_1())
                        .child(div().text_xs().text_color(rgb(theme::MUTED)).child(m.note))
                        .into_any_element(),
                );
            }
        }
        div()
            .id("model-menu")
            .absolute()
            .left(px(8.))
            .right(px(8.))
            .bottom(px(48.))
            .max_h(px(520.))
            .overflow_y_scroll()
            .py_1()
            .rounded_md()
            .bg(rgb(theme::PANE))
            .border_1()
            .border_color(rgb(theme::RULE))
            .shadow_lg()
            .occlude()
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .children(rows)
    }

    fn render_item(&self, ix: usize, item: &Item, cx: &mut Context<Self>) -> impl IntoElement {
        let base = div().text_sm().line_height(px(20.)).cursor_text();
        let text = chat_text::selectable(
            ix,
            item.text.clone(),
            self.chat_select.range_for(ix, &item.text),
            self.chat_select.registry.clone(),
        );
        match item.kind {
            ItemKind::User => div().flex().justify_end().child(
                base.max_w(px(360.))
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(theme::RAISED))
                    .text_color(rgb(theme::FG))
                    .child(text),
            ),
            ItemKind::Assistant => {
                let group = SharedString::from(format!("chat-item-{ix}"));
                let copied = self.chat_select.copied == Some(ix);
                // Reaches up into the gap above (same layout), so the
                // button there stays inside the hovered area.
                div()
                    .relative()
                    .mt(px(-12.))
                    .pt(px(12.))
                    .group(group.clone())
                    .child(base.text_color(rgb(theme::FG)).child(text))
                    .child(
                        // Shown on hover: copies the whole reply.
                        div()
                            .id(SharedString::from(format!("copy-item-{ix}")))
                            .absolute()
                            .top_0()
                            .right_0()
                            .px_2()
                            .py(px(1.))
                            .rounded_md()
                            .text_xs()
                            .border_1()
                            .border_color(rgb(theme::RULE))
                            .bg(rgb(theme::PANE))
                            .text_color(rgb(if copied { theme::ACCENT } else { theme::MUTED }))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(theme::RAISED)).text_color(rgb(theme::FG)))
                            .when(!copied, |d| d.invisible().group_hover(group, |s| s.visible()))
                            .on_click(cx.listener(move |this, _, _, cx| this.copy_message(ix, cx)))
                            .child(if copied { "Copied" } else { "Copy" }),
                    )
            }
            ItemKind::Note => base.text_xs().italic().text_color(rgb(theme::MUTED)).child(text),
            ItemKind::Error => base.text_color(rgb(theme::ERROR)).child(text),
            ItemKind::System => base
                .px_3()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(rgb(theme::RULE))
                .text_color(rgb(theme::MUTED))
                .child(text),
            ItemKind::Steer(state) => {
                let (label, color) = match state {
                    SteerState::Queued => ("While it works · waiting for its next step", theme::WARN),
                    SteerState::Delivered => ("While it worked · delivered", theme::ACCENT),
                    SteerState::NotSent => ("While it worked · not sent (stopped)", theme::MUTED),
                };
                div().flex().flex_col().items_end().gap_1().child(
                    base.max_w(px(360.))
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(color))
                        .bg(rgb(theme::RAISED))
                        .text_color(rgb(theme::FG))
                        .child(text),
                )
                .child(div().text_xs().text_color(rgb(color)).child(label))
            }
        }
    }

    // ------------------------------------------------------------- chats

    /// Why the agent can't leave its chat right now, if it can't.
    fn busy_guard(&mut self, cx: &mut Context<Self>) -> bool {
        if self.busy {
            self.notice("The agent is working in this chat. Let it finish, or press Stop, before switching chats.".into());
            self.chat_list_open = false;
            self.scroll_to_bottom();
            cx.notify();
            return true;
        }
        false
    }

    /// Shows chat `id` and points the agent at it (C2.3).
    fn switch_chat(&mut self, id: chats::ChatId, cx: &mut Context<Self>) {
        if id == self.chat_id {
            self.chat_list_open = false;
            cx.notify();
            return;
        }
        if self.busy_guard(cx) {
            return;
        }
        self.end_stream();
        self.chat_id = id;
        self.chats.mark_active(id);
        self.items = load_items(&self.chats, id);
        self.chat_select.clear();
        self.task_plan = self.chats.plan(id);
        self.chat_list_open = false;
        self.confirm_delete = None;
        self.can_continue = false;
        self.pending_recap = None;
        self.last_sent = None;
        // A fresh agent on this chat's saved conversation.
        self.agent = None;
        self._events = None;
        self.approvals.clear();
        let title = self.chats.get(id).map(|c| c.title).unwrap_or_default();
        self.trace.push(Kind::Info, format!("Opened chat “{title}”"), None);
        self.start_backend(cx);
        self.scroll_to_bottom();
        cx.notify();
    }

    fn new_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // An empty chat is already a new chat.
        if self.items.iter().all(|i| i.db.is_none()) {
            self.chat_list_open = false;
            window.focus(&self.input.focus_handle(cx));
            cx.notify();
            return;
        }
        if self.busy_guard(cx) {
            return;
        }
        let id = self.chats.create();
        self.switch_chat(id, cx);
        window.focus(&self.input.focus_handle(cx));
    }

    /// Takes a chat out of the strip (it stays in All chats), or deletes it.
    fn close_chat(&mut self, id: chats::ChatId, delete: bool, cx: &mut Context<Self>) {
        if id == self.chat_id && self.busy_guard(cx) {
            return;
        }
        let empty = self.chats.get(id).is_some_and(|c| c.items == 0);
        if id == self.chat_id {
            let open: Vec<chats::ChatId> = self.chats.open_chats().iter().map(|c| c.id).filter(|c| *c != id).collect();
            let next = open.last().copied().unwrap_or_else(|| self.chats.create());
            self.switch_chat(next, cx);
        }
        if delete || empty {
            self.chats.delete(id);
            self.trace.push(Kind::Info, "Deleted a chat", None);
        } else {
            self.chats.set_open(id, false);
        }
        self.confirm_delete = None;
        cx.notify();
    }

    /// Open chats as tabs, with new chat and All chats (C2.1).
    fn render_chat_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.chats.open_chats();
        let chip = |id: SharedString| {
            div().id(id).flex().items_center().gap_1().px_2().py(px(3.)).rounded_md().text_xs().border_1()
        };
        div()
            .flex()
            .items_center()
            .gap_1()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(rgb(theme::RULE))
            .child(
                div()
                    .id("chat-tabs")
                    .flex()
                    .flex_1()
                    .min_w(px(0.))
                    .gap_1()
                    .overflow_x_scroll()
                    .children(open.into_iter().map(|c| {
                        let active = c.id == self.chat_id;
                        let id = c.id;
                        let title: String = if c.title.chars().count() > 22 {
                            c.title.chars().take(21).collect::<String>() + "…"
                        } else {
                            c.title.clone()
                        };
                        chip(SharedString::from(format!("chat-{id}")))
                            .flex_none()
                            .border_color(rgb(if active { theme::ACCENT } else { theme::RULE }))
                            .bg(rgb(if active { theme::RAISED } else { theme::PANE }))
                            .text_color(rgb(if active { theme::FG } else { theme::MUTED }))
                            .when(!active, |d| d.cursor_pointer().hover(|s| s.text_color(rgb(theme::FG))))
                            // Double-click the title to rename the chat.
                            .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, window, cx| {
                                if ev.click_count() >= 2 {
                                    this.start_rename(id, false, window, cx);
                                } else {
                                    this.switch_chat(id, cx);
                                }
                            }))
                            .when(active && self.busy, |d| d.child(div().text_color(rgb(theme::ACCENT)).child("●")))
                            .child(self.strip_title(id, title, cx))
                            .child(
                                div()
                                    .id(SharedString::from(format!("chat-close-{id}")))
                                    .px(px(3.))
                                    .rounded_sm()
                                    .text_color(rgb(theme::MUTED))
                                    .cursor_pointer()
                                    .hover(|s| s.bg(rgb(theme::RULE)).text_color(rgb(theme::FG)))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.close_chat(id, false, cx);
                                    }))
                                    .child("×"),
                            )
                    })),
            )
            .child(
                chip("chat-new".into())
                    .flex_none()
                    .border_color(rgb(theme::RULE))
                    .text_color(rgb(theme::FG))
                    .cursor_pointer()
                    .hover(|s| s.border_color(rgb(theme::ACCENT)))
                    .on_click(cx.listener(|this, _, window, cx| this.new_chat(window, cx)))
                    .child("+ New"),
            )
            .child(
                chip("chat-all".into())
                    .flex_none()
                    .border_color(rgb(if self.chat_list_open { theme::ACCENT } else { theme::RULE }))
                    .text_color(rgb(if self.chat_list_open { theme::ACCENT } else { theme::FG }))
                    .cursor_pointer()
                    .hover(|s| s.border_color(rgb(theme::ACCENT)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.chat_list_open = !this.chat_list_open;
                        this.confirm_delete = None;
                        cx.notify();
                    }))
                    .child("All chats"),
            )
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.site_icons.release(window);
        if std::mem::take(&mut self.focus_chat_pending) {
            window.focus(&self.input.focus_handle(cx));
        }
        if self.sidebar_view.is_none() {
            let shell = cx.entity();
            self.sidebar_view = Some(cx.new(|cx| SidebarView {
                _observe: cx.observe(&shell, |_, _, cx| cx.notify()),
                shell: shell.downgrade(),
            }));
        }
        // Cached: page frames redraw the browser pane (and so this view),
        // but the sidebar only re-renders when the shell itself changes.
        // A fullscreen video gets the whole window.
        let fullscreen = self.browser.read(cx).is_fullscreen();
        let sidebar = (self.sidebar_open && !fullscreen).then(|| {
            AnyView::from(self.sidebar_view.clone().expect("created above"))
                .cached(StyleRefinement::default().w(px(SIDEBAR_WIDTH)).h_full().flex_none())
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
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.browser.update(cx, |b, cx| b.zoom(1, cx))))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.browser.update(cx, |b, cx| b.zoom(-1, cx))))
            .on_action(cx.listener(|this, _: &ZoomReset, _, cx| this.browser.update(cx, |b, cx| b.zoom(0, cx))))
            .on_action(cx.listener(|this, _: &FocusAddress, window, cx| {
                this.browser.update(cx, |b, cx| b.focus_address(window, cx))
            }))
            .on_action(cx.listener(|this, _: &ShowChat, window, cx| this.show_tab(SidebarTab::Chat, window, cx)))
            .on_action(cx.listener(|this, _: &ShowHistory, window, cx| this.show_tab(SidebarTab::History, window, cx)))
            .on_action(cx.listener(|this, _: &ShowFocus, window, cx| this.show_tab(SidebarTab::Focus, window, cx)))
            .on_action(cx.listener(|this, _: &ShowTrace, window, cx| {
                this.history_sub = HistorySub::Trace;
                this.show_tab(SidebarTab::History, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ShowExplained, window, cx| this.show_tab(SidebarTab::Explained, window, cx)))
            .flex()
            .flex_row()
            .size_full()
            .bg(rgb(theme::BG))
            .text_color(rgb(theme::FG))
            .children(sidebar)
            .when(!fullscreen, |d| d.child(self.render_bumper(cx)))
            .child(self.browser.clone())
    }
}

/// A backend's settings name ("claude_code"), as the home page sends it.
fn backend_id(kind: BackendKind) -> String {
    serde_json::to_value(kind).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
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

/// The sidebar as its own view, so it can be cached. GPUI redraws every
/// view above one that changed; without this, each frame the web page
/// paints would rebuild the whole sidebar (history rows and all). It
/// renders through the shell, which owns the state, and re-renders when
/// the shell notifies.
struct SidebarView {
    shell: WeakEntity<Shell>,
    _observe: Subscription,
}

impl Render for SidebarView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.shell
            .update(cx, |shell, cx| shell.render_sidebar(cx).into_any_element())
            .unwrap_or_else(|_| div().into_any_element())
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
    // Test hooks (isolated test instances only): pre-save a login,
    // "site|user|password"; put sites in focus, "site,site".
    if std::env::var_os("UNIBROW_DATA_DIR").is_some() {
        if let Ok(sites) = std::env::var("UNIBROW_TEST_FOCUS") {
            for site in sites.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                store.add_focus(site, site);
            }
        }
        // A watch that has been checked once: "url|label|value".
        if let Ok(spec) = std::env::var("UNIBROW_TEST_WATCH") {
            let parts: Vec<&str> = spec.splitn(3, '|').collect();
            if let [url, label, value] = parts[..] {
                let w = browsing::NewWatch {
                    url: url.into(),
                    label: label.into(),
                    kind: "text".into(),
                    when: "changes".into(),
                    every_min: 60,
                    ..Default::default()
                };
                match store.add_watch(&w) {
                    Ok(id) => {
                        store.record_watch_check(id, Ok(value.to_string()));
                    }
                    Err(e) => eprintln!("agent-unibrow: test watch not added: {e}"),
                }
            }
        }
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
    match &cef {
        Err(e) => eprintln!("agent-unibrow: browser unavailable: {e}"),
        Ok(()) => {
            if let Err(e) = rail_browser::app_pages::serve(Arc::new(home::Home)) {
                eprintln!("agent-unibrow: {e}");
            }
        }
    }
    Application::new().run(move |cx: &mut App| {
        text_input::bind_keys(cx);
        chat_text::bind_keys(cx);
        cx.bind_keys([
            gpui::KeyBinding::new("ctrl-b", ToggleSidebar, None),
            gpui::KeyBinding::new("escape", StopAgent, None),
            gpui::KeyBinding::new("ctrl-t", NewTab, None),
            gpui::KeyBinding::new("ctrl-w", CloseTab, None),
            gpui::KeyBinding::new("ctrl-tab", NextTab, None),
            gpui::KeyBinding::new("ctrl-shift-tab", PrevTab, None),
            gpui::KeyBinding::new("ctrl-l", FocusAddress, None),
            gpui::KeyBinding::new("ctrl-f", FindInPage, None),
            // Ctrl+Shift+= arrives as ctrl-+; the keypad keys as add/subtract.
            gpui::KeyBinding::new("ctrl-=", ZoomIn, None),
            gpui::KeyBinding::new("ctrl-+", ZoomIn, None),
            gpui::KeyBinding::new("ctrl-add", ZoomIn, None),
            gpui::KeyBinding::new("ctrl--", ZoomOut, None),
            gpui::KeyBinding::new("ctrl-subtract", ZoomOut, None),
            gpui::KeyBinding::new("ctrl-0", ZoomReset, None),
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
