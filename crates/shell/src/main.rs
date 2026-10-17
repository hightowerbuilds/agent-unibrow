//! agent-unibrow shell.
//!
//! Three regions: conversation (left), content (center, where the web
//! engine composites in M2), and the trace pane (right). The agent runs on
//! its own thread; its events arrive here over a channel.

mod text_input;
mod theme;

use agent_core::{AgentEvent, AgentHandle, Approval, ModelConfig};
use gpui::{
    App, Application, Bounds, Context, Entity, FocusHandle, Focusable, ScrollHandle, SharedString,
    Subscription, Task, TitlebarOptions, Window, WindowBounds, WindowOptions, div, prelude::*, px,
    rgb, size,
};
use policy::Policy;
use router::Registry;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use text_input::{Submit, TextInput};
use trace::{Kind, Trace};

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

struct PendingApproval {
    id: String,
    name: String,
    description: String,
    reason: String,
    respond: Option<agent_core::ApprovalSender>,
}

struct Shell {
    focus_handle: FocusHandle,
    input: Entity<TextInput>,
    items: Vec<Item>,
    /// Index of the assistant item currently receiving streamed text.
    streaming: Option<usize>,
    trace: Trace,
    expanded: HashSet<usize>,
    approvals: Vec<PendingApproval>,
    agent: Option<AgentHandle>,
    busy: bool,
    status: SharedString,
    needs_key: bool,
    conv_scroll: ScrollHandle,
    trace_scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
    _events: Option<Task<()>>,
}

impl Shell {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new("Ask Claude to do something…", cx));
        let subscription = cx.subscribe(&input, |this: &mut Self, _, ev: &Submit, cx| this.submit(ev.0.clone(), cx));
        window.focus(&input.focus_handle(cx));

        let mut shell = Self {
            focus_handle: cx.focus_handle(),
            input,
            items: Vec::new(),
            streaming: None,
            trace: Trace::open_session(),
            expanded: HashSet::new(),
            approvals: Vec::new(),
            agent: None,
            busy: false,
            status: "".into(),
            needs_key: false,
            conv_scroll: ScrollHandle::new(),
            trace_scroll: ScrollHandle::new(),
            _subscriptions: vec![subscription],
            _events: None,
        };
        if let Some(p) = shell.trace.path().cloned() {
            shell.trace.push(Kind::Info, "Trace file opened", Some(p.display().to_string()));
        }
        match load_api_key() {
            Some(key) => shell.start_agent(key, cx),
            None => shell.ask_for_key("Paste your Anthropic API key and press Enter. It's saved to your GNOME keyring.", cx),
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

    fn start_agent(&mut self, api_key: String, cx: &mut Context<Self>) {
        let (rail, notes) = rail_api::ApiRail::from_config();
        let mut registry = Registry::new();
        registry.add_rail(&rail);
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
        let config = ModelConfig::default();
        self.trace.push(Kind::Info, format!("Agent ready: {} at {} effort", config.model, config.effort), None);
        let (handle, events) = agent_core::spawn(api_key, config, registry, Arc::new(Mutex::new(Policy::load())));
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
            self.start_agent(text, cx);
            return;
        }
        let Some(agent) = &self.agent else { return };
        if self.busy {
            return;
        }
        self.items.push(Item { kind: ItemKind::User, text: text.clone() });
        self.trace.push(Kind::User, first_line(&text), Some(text.clone()));
        if agent.send(text) {
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
    }

    fn on_event(&mut self, ev: AgentEvent, cx: &mut Context<Self>) {
        match ev {
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
            AgentEvent::ToolResult { name, ok, summary, content, .. } => {
                let kind = if ok { Kind::ToolResult } else { Kind::Error };
                self.trace.push(kind, format!("{name}: {summary}"), Some(content));
                self.status = "Thinking…".into();
            }
            AgentEvent::ApprovalNeeded { id, name, description, reason, respond } => {
                self.status = "Waiting for your approval".into();
                self.trace.push(Kind::ApprovalRequested, format!("Approval needed: {reason}"), Some(description.clone()));
                self.approvals.push(PendingApproval { id, name, description, reason, respond: Some(respond) });
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
                let tokens = format!(
                    "in {} · out {} · cache read {}",
                    usage["input_tokens"].as_u64().unwrap_or(0),
                    usage["output_tokens"].as_u64().unwrap_or(0),
                    usage["cache_read_input_tokens"].as_u64().unwrap_or(0)
                );
                self.trace.push(
                    Kind::Assistant,
                    format!("Turn finished ({stop_reason})"),
                    Some(format!("model: {}\n{tokens}", model.unwrap_or_default())),
                );
                self.set_busy(false, "", cx);
            }
            AgentEvent::Error(e) => {
                self.streaming = None;
                self.items.push(Item { kind: ItemKind::Error, text: e.clone() });
                self.trace.push(Kind::Error, first_line(&e), Some(e.clone()));
                self.set_busy(false, "", cx);
                if e.contains("API 401") {
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

    fn header(label: &'static str, right: Option<SharedString>) -> impl IntoElement {
        div()
            .flex()
            .justify_between()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(rgb(theme::RULE))
            .text_xs()
            .child(div().text_color(rgb(theme::ACCENT)).child(label))
            .children(right.map(|r| div().text_color(rgb(theme::MUTED)).child(r)))
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
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(button(format!("approve-{}", a.id), "Approve", true).on_click(
                        cx.listener(move |this, _, _, cx| this.resolve(&id1, Approval::Approve, cx)),
                    ))
                    .child(button(format!("always-{}", a.id), "Always allow these recipients", false).on_click(
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
            .when(e.detail.is_some(), |d| d.cursor_pointer().hover(|s| s.bg(rgb(theme::RAISED))))
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
        let status: Option<SharedString> = (!self.status.is_empty()).then(|| self.status.clone());
        let approvals: Vec<_> = self.approvals.iter().map(|a| self.render_approval(a, cx).into_any_element()).collect();

        let conversation = div()
            .flex()
            .flex_col()
            .w(px(440.))
            .h_full()
            .flex_none()
            .bg(rgb(theme::PANE))
            .border_r_1()
            .border_color(rgb(theme::RULE))
            .child(Self::header("CONVERSATION", status))
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
                    .children(self.items.iter().map(Self::render_item))
                    .children(approvals),
            )
            .child(div().p_3().border_t_1().border_color(rgb(theme::RULE)).child(self.input.clone()));

        let content = div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .justify_center()
            .items_center()
            .gap_2()
            .child(div().text_xl().text_color(rgb(theme::FG)).child("agent-unibrow"))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(theme::MUTED))
                    .child("Web pages will render here once the browser engine lands (M2)."),
            );

        let rows: Vec<_> = self
            .trace
            .entries()
            .iter()
            .enumerate()
            .map(|(i, e)| self.render_trace_row(i, e, cx).into_any_element())
            .collect();
        let trace = div()
            .flex()
            .flex_col()
            .w(px(380.))
            .h_full()
            .flex_none()
            .bg(rgb(theme::PANE))
            .border_l_1()
            .border_color(rgb(theme::RULE))
            .child(Self::header("TRACE", Some(format!("{} entries", rows.len()).into())))
            .child(
                div()
                    .id("trace")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .py_2()
                    .overflow_y_scroll()
                    .track_scroll(&self.trace_scroll)
                    .children(rows),
            );

        div()
            .track_focus(&self.focus_handle)
            .flex()
            .flex_row()
            .size_full()
            .bg(rgb(theme::BG))
            .text_color(rgb(theme::FG))
            .child(conversation)
            .child(content)
            .child(trace)
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

fn main() {
    Application::new().run(|cx: &mut App| {
        text_input::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(1400.), px(860.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("agent-unibrow".into()), ..Default::default() }),
                app_id: Some("agent-unibrow".into()),
                ..Default::default()
            },
            |window, cx| {
                // TitlebarOptions alone doesn't reach the Wayland toplevel title.
                window.set_window_title("agent-unibrow");
                cx.new(|cx| Shell::new(window, cx))
            },
        )
        .expect("failed to open window");
        cx.activate(true);
        eprintln!("agent-unibrow: window opened on {}", platform_label());
    });
}
