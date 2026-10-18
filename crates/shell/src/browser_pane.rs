//! The center pane: an embedded Chromium page with a toolbar.
//!
//! Chromium renders off-screen; each new frame becomes a GPUI image painted
//! in the page area. Mouse, wheel and keyboard input on the page are
//! forwarded to Chromium. One pump loop drives CEF from the main thread.

use crate::text_input::{Changed, Submit, TextInput};
use crate::theme;
use gpui::{
    App, Bounds, Context, Corners, CursorStyle, Entity, FocusHandle, Focusable, KeyDownEvent, KeyUpEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, RenderImage, ScrollDelta, ScrollWheelEvent,
    SharedString, Subscription, Task, Window, canvas, div, point, prelude::*, px, rgb, size,
};
use browsing::{Browsing, site_of};
use gpui::EventEmitter;
use rail_browser::bridge::Rect;
use rail_browser::bridge::{TabInfo, TabOp};
use rail_browser::{Cursor, DialogInfo, KeyInput, Modifiers, NavAction, Request, WebView, normalize_address};
use std::time::Instant;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

const HOME: &str = "https://duckduckgo.com/";
/// How long the agent's highlight box stays on screen.
const HIGHLIGHT_FOR: Duration = Duration::from_millis(1600);
/// How often CEF gets to run. 8 ms keeps input and video smooth.
const PUMP_INTERVAL: Duration = Duration::from_millis(8);
/// Wheel "lines" to pixels. Chromium scrolls about this far per notch.
const PIXELS_PER_LINE: f32 = 40.0;

/// Page area geometry measured during layout, read by the pump loop.
#[derive(Clone, Copy, Default, PartialEq)]
struct Geometry {
    bounds: Option<Bounds<Pixels>>,
    scale: f32,
}

/// What the toolbar shows, so a change can trigger a re-render.
#[derive(Clone, Default, PartialEq)]
struct Chrome {
    url: String,
    title: String,
    loading: bool,
    can_go_back: bool,
    can_go_forward: bool,
    cursor: Cursor,
    load_error: Option<String>,
    dialog: Option<DialogInfo>,
}

/// History or focus changed (a visit was recorded, focus toggled).
pub struct BrowsingChanged;

/// The user wants the agent to stop: the Stop button, or they started
/// using the page themselves while the agent was driving it.
pub enum AgentControl {
    Stop,
    TookOver,
}

/// The agent asked the user to do something on the page (browser_handoff).
struct Handoff {
    message: String,
    reply: Option<rail_browser::bridge::Reply<bool>>,
}

/// One open tab: its own Chromium page and toolbar state.
struct Tab {
    id: u32,
    view: WebView,
    chrome: Chrome,
}

pub struct BrowserPane {
    store: Arc<Browsing>,
    /// Requests from agent tools (DevTools commands, navigation, highlights).
    requests: async_channel::Receiver<Request>,
    /// The element the agent is about to act on.
    highlight: Option<(Rect, String, Instant)>,
    /// An agent turn is running.
    agent_busy: bool,
    /// ...and it has used the browser during this turn.
    agent_driving: bool,
    tabs: Vec<Tab>,
    active: usize,
    next_tab_id: u32,
    /// Page size and scale every tab is laid out at.
    size: Option<(i32, i32, f32)>,
    /// Why Chromium couldn't start, if it couldn't.
    unavailable: Option<String>,
    image: Option<Arc<RenderImage>>,
    /// An open dropdown list: where it goes (CSS px) and its pixels.
    popup: Option<(f32, f32, Arc<RenderImage>)>,
    /// Old frames waiting to be released from the GPU atlas.
    retired: Vec<Arc<RenderImage>>,
    last_seq: u64,
    applied: (i32, i32, u32),
    geometry: Rc<Cell<Geometry>>,
    chrome: Chrome,
    address: Entity<TextInput>,
    editing_address: bool,
    /// Ctrl+F find bar.
    find_open: bool,
    find_input: Entity<TextInput>,
    /// The desktop file picker is open for a page's file field.
    file_prompt_open: bool,
    downloads_open: bool,
    /// A short notice over the page (blocked permission requests).
    notice: Option<(String, Instant)>,
    handoff: Option<Handoff>,
    page_focus: FocusHandle,
    _pump: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<BrowsingChanged> for BrowserPane {}
impl EventEmitter<AgentControl> for BrowserPane {}

impl BrowserPane {
    pub fn new(
        cef: Result<(), String>,
        store: Arc<Browsing>,
        requests: async_channel::Receiver<Request>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let address = cx.new(|cx| TextInput::new("Search or enter address", cx));
        let find_input = cx.new(|cx| TextInput::new("Find in page", cx));
        let page_focus = cx.focus_handle();
        let address_focus = address.focus_handle(cx);
        let subscriptions = vec![
            cx.subscribe(&address, |this: &mut Self, _, ev: &Submit, cx| this.navigate(&ev.0, cx)),
            cx.subscribe(&find_input, |this: &mut Self, input, _: &Changed, cx| {
                let text = input.read(cx).text().to_string();
                if let Some(v) = this.view() {
                    if text.is_empty() { v.stop_finding() } else { v.find(&text, true, false) }
                }
                cx.notify();
            }),
            cx.subscribe(&find_input, |this: &mut Self, _, ev: &Submit, cx| {
                if let Some(v) = this.view() {
                    v.find(&ev.0, true, true);
                }
                cx.notify();
            }),
            cx.on_focus(&address_focus, window, |this, _, _| this.editing_address = true),
            cx.on_blur(&address_focus, window, |this, _, cx| {
                this.editing_address = false;
                let url = this.chrome.url.clone();
                this.address.update(cx, |a, cx| a.set_text(&url, cx));
            }),
            cx.on_focus(&page_focus, window, |this, _, _| {
                if let Some(v) = this.view() {
                    v.set_focus(true);
                }
            }),
            cx.on_blur(&page_focus, window, |this, _, _| {
                if let Some(v) = this.view() {
                    v.set_focus(false);
                }
            }),
        ];

        let pump = if cef.is_ok() {
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(PUMP_INTERVAL).await;
                    rail_browser::pump();
                    if this.update(cx, |pane, cx| pane.tick(cx)).is_err() {
                        break;
                    }
                }
            })
        } else {
            Task::ready(())
        };

        Self {
            store,
            requests,
            highlight: None,
            agent_busy: false,
            agent_driving: false,
            tabs: Vec::new(),
            active: 0,
            next_tab_id: 1,
            size: None,
            unavailable: cef.err(),
            image: None,
            popup: None,
            retired: Vec::new(),
            last_seq: 0,
            applied: (0, 0, 0),
            geometry: Rc::new(Cell::new(Geometry::default())),
            chrome: Chrome::default(),
            address,
            editing_address: false,
            find_open: false,
            find_input,
            file_prompt_open: false,
            downloads_open: false,
            notice: None,
            handoff: None,
            page_focus,
            _pump: pump,
            _subscriptions: subscriptions,
        }
    }

    /// One pump step: create/resize the view to fit, pick up new frames and
    /// toolbar state.
    fn tick(&mut self, cx: &mut Context<Self>) {
        let g = self.geometry.get();
        let Some(bounds) = g.bounds else { return };
        let (w, h) = (f32::from(bounds.size.width).round() as i32, f32::from(bounds.size.height).round() as i32);
        if w < 2 || h < 2 {
            return;
        }
        let scale = if g.scale > 0.0 { g.scale } else { 1.0 };
        let wanted = (w, h, (scale * 100.0) as u32);

        self.size = Some((w, h, scale));
        if self.tabs.is_empty() {
            let start = std::env::var("UNIBROW_START_URL").unwrap_or_else(|_| HOME.into());
            if let Err(e) = self.open_tab(&start, true, cx) {
                self.unavailable = Some(e);
                cx.notify();
                return;
            }
            self.applied = wanted;
        } else if self.applied != wanted {
            for t in &self.tabs {
                t.view.resize(w, h, scale);
            }
            self.applied = wanted;
        }

        // Links that asked for new tabs.
        let mut opens = Vec::new();
        for t in &self.tabs {
            opens.extend(t.view.take_new_tab_requests());
        }
        for (url, foreground) in opens {
            let _ = self.open_tab(&url, foreground, cx);
        }

        let mut changed = false;
        let mut tab_ops: Vec<(TabOp, rail_browser::bridge::Reply<Vec<TabInfo>>)> = Vec::new();
        let Some(view) = self.tabs.get(self.active).map(|t| &t.view) else { return };
        while let Ok(req) = self.requests.try_recv() {
            if self.agent_busy && !self.agent_driving {
                self.agent_driving = true;
                changed = true;
            }
            match req {
                Request::Cdp { method, params, reply } => view.cdp(&method, params, reply),
                Request::Navigate { url, reply } => {
                    view.navigate(&url);
                    let _ = reply.send(Ok(()));
                }
                Request::Nav { action, reply } => {
                    match action {
                        NavAction::Back => view.go_back(),
                        NavAction::Forward => view.go_forward(),
                        NavAction::Reload => view.reload(),
                    }
                    let _ = reply.send(Ok(()));
                }
                Request::State { reply } => {
                    let _ = reply.send(Ok(view.page_state()));
                }
                Request::Highlight { rect, label } => {
                    self.highlight = Some((rect, label, Instant::now()));
                    changed = true;
                }
                Request::Dialog { accept, text, reply } => {
                    let r = if view.answer_dialog(accept, text.as_deref()) { Ok(()) } else { Err("No dialog is open.".into()) };
                    let _ = reply.send(r);
                    changed = true;
                }
                Request::Tabs { op, reply } => tab_ops.push((op, reply)),
                Request::Downloads { reply } => {
                    let all: Vec<_> = self.tabs.iter().flat_map(|t| t.view.downloads()).collect();
                    let _ = reply.send(Ok(all));
                }
                Request::SavePdf { path, done } => view.print_to_pdf(&path, done),
                Request::Handoff { message, reply } => {
                    // Test hook: finish handoffs automatically.
                    let auto = std::env::var("UNIBROW_AUTOHANDOFF").ok();
                    if let Some(answer) = auto.as_deref().filter(|a| *a == "done" || *a == "later") {
                        let _ = reply.send(Ok(answer == "done"));
                    } else {
                        if let Some(old) = self.handoff.take().and_then(|h| h.reply) {
                            let _ = old.send(Ok(false));
                        }
                        self.handoff = Some(Handoff { message, reply: Some(reply) });
                        changed = true;
                    }
                }
            }
        }
        for (op, reply) in tab_ops {
            let r = self.apply_tab_op(op, cx).map(|_| self.tab_infos());
            let _ = reply.send(r);
            changed = true;
        }
        if self.highlight.as_ref().is_some_and(|(_, _, at)| at.elapsed() > HIGHLIGHT_FOR) {
            self.highlight = None;
            changed = true;
        }
        // A page's file field opened its chooser: show the desktop picker.
        if !self.file_prompt_open {
            if let Some((tab_id, req)) = self.tabs.iter().find_map(|t| t.view.file_dialog().map(|r| (t.id, r))) {
                self.file_prompt_open = true;
                let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
                    files: true,
                    directories: false,
                    multiple: req.multiple,
                    prompt: None,
                });
                cx.spawn(async move |this, cx| {
                    let picked = rx.await.ok().and_then(|r| r.ok()).flatten();
                    let _ = this.update(cx, |pane, cx| {
                        pane.file_prompt_open = false;
                        if let Some(t) = pane.tabs.iter().find(|t| t.id == tab_id) {
                            t.view.answer_file_dialog(
                                picked.map(|ps| ps.into_iter().map(|p| p.to_string_lossy().into_owned()).collect()),
                            );
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
        }
        let blocked: Vec<String> = self.tabs.iter().flat_map(|t| t.view.take_blocked()).collect();
        if let Some(last) = blocked.last() {
            self.notice = Some((format!("Blocked: {last}"), Instant::now()));
            changed = true;
        }
        if self.notice.as_ref().is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(6)) {
            self.notice = None;
            changed = true;
        }
        let Some(view) = self.tabs.get(self.active).map(|t| &t.view) else { return };
        let seq = view.state().frame_seq;
        if seq != self.last_seq {
            self.last_seq = seq;
            if let Some(frame) = view.take_frame() {
                if let Some(buf) = image::RgbaImage::from_raw(frame.width, frame.height, frame.bgra) {
                    // GPUI images are BGRA, which is what Chromium paints.
                    let img = Arc::new(RenderImage::new(vec![image::Frame::new(buf)]));
                    if let Some(old) = self.image.replace(img) {
                        self.retired.push(old);
                    }
                    changed = true;
                }
            }
        }

        // Open dropdown list (painted separately by Chromium).
        if let Some((x, y, _w, _h, frame)) = view.take_popup_frame() {
            if let Some(buf) = image::RgbaImage::from_raw(frame.width, frame.height, frame.bgra) {
                let img = Arc::new(RenderImage::new(vec![image::Frame::new(buf)]));
                if let Some((_, _, old)) = self.popup.replace((x as f32, y as f32, img)) {
                    self.retired.push(old);
                }
                changed = true;
            }
        } else if !view.popup_visible() {
            if let Some((_, _, old)) = self.popup.take() {
                self.retired.push(old);
                changed = true;
            }
        }

        // Every tab records history; the active one drives the toolbar.
        let mut browsing_changed = false;
        for t in self.tabs.iter_mut() {
            let chrome = {
                let s = t.view.state();
                Chrome {
                    url: s.url.clone(),
                    title: s.title.clone(),
                    loading: s.loading,
                    can_go_back: s.can_go_back,
                    can_go_forward: s.can_go_forward,
                    cursor: s.cursor,
                    load_error: s.load_error.clone(),
                    dialog: s.dialog.clone(),
                }
            };
            if chrome != t.chrome {
                if chrome.url != t.chrome.url {
                    browsing_changed |= self.store.record_visit(&chrome.url, &chrome.title);
                } else if chrome.title != t.chrome.title && !chrome.title.is_empty() {
                    self.store.update_title(&chrome.url, &chrome.title);
                    browsing_changed = true;
                }
                t.chrome = chrome;
                changed = true;
            }
        }
        if browsing_changed {
            cx.emit(BrowsingChanged);
        }
        if let Some(active) = self.tabs.get(self.active).map(|t| t.chrome.clone()) {
            if active != self.chrome {
                if active.url != self.chrome.url && !self.editing_address {
                    let url = active.url.clone();
                    self.address.update(cx, |a, cx| a.set_text(&url, cx));
                }
                self.chrome = active;
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }

    /// The active tab's page.
    fn view(&self) -> Option<&WebView> {
        self.tabs.get(self.active).map(|t| &t.view)
    }

    /// Opens a tab at `url`. Background tabs start hidden.
    fn open_tab(&mut self, url: &str, activate: bool, cx: &mut Context<Self>) -> Result<u32, String> {
        let (w, h, scale) = self.size.ok_or("The browser isn't laid out yet.")?;
        let view = WebView::create(url, w, h, scale)?;
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let chrome = Chrome { url: url.to_string(), ..Default::default() };
        self.tabs.push(Tab { id, view, chrome });
        if activate {
            self.activate(self.tabs.len() - 1, cx);
        } else {
            self.tabs.last().expect("just pushed").view.set_hidden(true);
            cx.notify();
        }
        Ok(id)
    }

    fn activate(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        if index != self.active {
            if let Some(old) = self.tabs.get(self.active) {
                old.view.set_hidden(true);
            }
        }
        self.active = index;
        let tab = &self.tabs[index];
        tab.view.set_hidden(false);
        // Show the new tab's next frame, not the old tab's last one.
        if let Some(old) = self.image.take() {
            self.retired.push(old);
        }
        if let Some((_, _, old)) = self.popup.take() {
            self.retired.push(old);
        }
        self.last_seq = u64::MAX;
        self.chrome = tab.chrome.clone();
        if !self.editing_address {
            let url = self.chrome.url.clone();
            self.address.update(cx, |a, cx| a.set_text(&url, cx));
        }
        cx.notify();
    }

    fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        let closing_active = index == self.active;
        self.tabs.remove(index); // dropping the WebView closes its page
        if self.tabs.is_empty() {
            let _ = self.open_tab(HOME, true, cx);
            return;
        }
        if index < self.active || (closing_active && self.active >= self.tabs.len()) {
            self.active = self.active.saturating_sub(1);
        }
        if closing_active {
            self.activate(self.active, cx);
        }
        cx.notify();
    }

    fn index_of(&self, id: u32) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    fn apply_tab_op(&mut self, op: TabOp, cx: &mut Context<Self>) -> Result<(), String> {
        match op {
            TabOp::List => Ok(()),
            TabOp::New { url } => {
                let url = url.map(|u| normalize_address(&u)).unwrap_or_else(|| HOME.to_string());
                self.open_tab(&url, true, cx).map(|_| ())
            }
            TabOp::Switch { id } => {
                let i = self.index_of(id).ok_or_else(|| format!("No tab {id}."))?;
                self.activate(i, cx);
                Ok(())
            }
            TabOp::Close { id } => {
                let i = self.index_of(id).ok_or_else(|| format!("No tab {id}."))?;
                self.close_tab(i, cx);
                Ok(())
            }
        }
    }

    fn tab_infos(&self) -> Vec<TabInfo> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(i, t)| TabInfo {
                id: t.id,
                title: t.chrome.title.clone(),
                url: t.chrome.url.clone(),
                active: i == self.active,
            })
            .collect()
    }

    // ---------------------------------------------------- tab actions

    pub fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_tab(HOME, true, cx).is_ok() {
            window.focus(&self.address.focus_handle(cx));
            self.address.update(cx, |a, cx| a.set_text("", cx));
        }
    }

    pub fn close_active_tab(&mut self, cx: &mut Context<Self>) {
        self.close_tab(self.active, cx);
    }

    pub fn cycle_tab(&mut self, forward: bool, cx: &mut Context<Self>) {
        let n = self.tabs.len();
        if n > 1 {
            let next = if forward { (self.active + 1) % n } else { (self.active + n - 1) % n };
            self.activate(next, cx);
        }
    }

    pub fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find_open = true;
        window.focus(&self.find_input.focus_handle(cx));
        cx.notify();
    }

    fn close_find(&mut self, cx: &mut Context<Self>) {
        self.find_open = false;
        if let Some(v) = self.view() {
            v.stop_finding();
        }
        self.find_input.update(cx, |i, cx| i.set_text("", cx));
        cx.notify();
    }

    pub fn focus_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.address.focus_handle(cx));
    }

    fn tab_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_end()
            .gap_1()
            .px_2()
            .pt_1()
            .bg(rgb(theme::BG))
            .children(self.tabs.iter().enumerate().map(|(i, t)| {
                let active = i == self.active;
                let title = if t.chrome.title.is_empty() {
                    if t.chrome.loading { "Loading…".to_string() } else { t.chrome.url.clone() }
                } else {
                    t.chrome.title.clone()
                };
                let id = t.id;
                div()
                    .id(("tab", t.id as usize))
                    .group("tab")
                    .flex()
                    .items_center()
                    .gap_1()
                    .w(px(190.))
                    .min_w(px(60.))
                    .flex_shrink()
                    .pl_2()
                    .pr_1()
                    .py_1()
                    .rounded_t_md()
                    .text_xs()
                    .bg(rgb(if active { theme::PANE } else { theme::BG }))
                    .text_color(rgb(if active { theme::FG } else { theme::MUTED }))
                    .when(!active, |d| d.cursor_pointer().hover(|s| s.bg(rgb(theme::RAISED))))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(i) = this.index_of(id) {
                            this.activate(i, cx);
                        }
                    }))
                    .child(div().flex_1().min_w(px(0.)).truncate().child(SharedString::from(title)))
                    .child(
                        div()
                            .id(("tab-close", t.id as usize))
                            .px_1()
                            .rounded_sm()
                            .text_color(rgb(theme::MUTED))
                            .hover(|s| s.bg(rgb(theme::RULE)).text_color(rgb(theme::FG)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                if let Some(i) = this.index_of(id) {
                                    this.close_tab(i, cx);
                                }
                            }))
                            .child("×"),
                    )
            }))
            .child(
                div()
                    .id("tab-new")
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .text_color(rgb(theme::MUTED))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::RAISED)).text_color(rgb(theme::FG)))
                    .on_click(cx.listener(|this, _, window, cx| this.new_tab(window, cx)))
                    .child("+"),
            )
    }

    pub fn navigate(&mut self, input: &str, cx: &mut Context<Self>) {
        if let Some(v) = self.view() {
            v.navigate(&normalize_address(input));
        }
        cx.notify();
    }

    /// Tells the pane whether an agent turn is running (for the agent bar).
    pub fn set_agent_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.agent_busy = busy;
        if !busy {
            self.agent_driving = false;
            self.highlight = None;
            if let Some(reply) = self.handoff.take().and_then(|h| h.reply) {
                let _ = reply.send(Ok(false));
            }
        }
        cx.notify();
    }

    /// The user is acting on the page while the agent drives it. (Not
    /// during a handoff: then the agent is waiting for exactly that.)
    fn user_took_over(&mut self, cx: &mut Context<Self>) {
        if self.agent_busy && self.agent_driving && self.handoff.is_none() {
            cx.emit(AgentControl::TookOver);
        }
    }

    /// The page's current URL and title.
    pub fn location(&self) -> (String, String) {
        (self.chrome.url.clone(), self.chrome.title.clone())
    }

    /// Bookmarks the current page, or removes its bookmark.
    pub fn toggle_bookmark_current(&mut self, cx: &mut Context<Self>) {
        let (url, title) = (self.chrome.url.clone(), self.chrome.title.clone());
        if site_of(&url).is_some() {
            let label = if title.is_empty() { url.clone() } else { title };
            self.store.toggle_bookmark(&url, &label);
            cx.emit(BrowsingChanged);
            cx.notify();
        }
    }

    /// Puts the current page's site in focus, or takes it out.
    pub fn toggle_focus_current(&mut self, cx: &mut Context<Self>) {
        if let Some(site) = site_of(&self.chrome.url) {
            let label = if self.chrome.title.is_empty() { site.clone() } else { self.chrome.title.clone() };
            self.store.toggle_focus(&site, &label);
            cx.emit(BrowsingChanged);
            cx.notify();
        }
    }

    // ---------------------------------------------------------- input

    fn local(&self, p: gpui::Point<Pixels>) -> (f32, f32) {
        let origin = self.geometry.get().bounds.map(|b| b.origin).unwrap_or_default();
        (f32::from(p.x - origin.x), f32::from(p.y - origin.y))
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.user_took_over(cx);
        window.focus(&self.page_focus);
        let Some(v) = self.view() else { return };
        v.set_focus(true);
        let Some(button) = map_button(ev.button) else { return };
        let (x, y) = self.local(ev.position);
        v.mouse_button(x, y, button, false, ev.click_count, mods(&ev.modifiers));
    }

    fn on_mouse_up(&mut self, ev: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        let (Some(v), Some(button)) = (self.view(), map_button(ev.button)) else { return };
        let (x, y) = self.local(ev.position);
        v.mouse_button(x, y, button, true, ev.click_count, mods(&ev.modifiers));
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, _: &mut Context<Self>) {
        let Some(v) = self.view() else { return };
        let (x, y) = self.local(ev.position);
        v.mouse_move(x, y, mods(&ev.modifiers));
    }

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, _: &mut Window, _: &mut Context<Self>) {
        let Some(v) = self.view() else { return };
        let (x, y) = self.local(ev.position);
        let (dx, dy) = match ev.delta {
            ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
            ScrollDelta::Lines(l) => (l.x * PIXELS_PER_LINE, l.y * PIXELS_PER_LINE),
        };
        v.wheel(x, y, dx, dy, mods(&ev.modifiers));
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.user_took_over(cx);
        let Some(v) = self.view() else { return };
        let k = &ev.keystroke;
        if std::env::var_os("UNIBROW_DEBUG_KEYS").is_some() {
            eprintln!("page key down: key={:?} char={:?} mods={:?}", k.key, k.key_char, k.modifiers);
        }
        let handled = v.key(&KeyInput {
            key: &k.key,
            text: k.key_char.as_deref(),
            modifiers: mods(&k.modifiers),
            down: true,
        });
        if handled {
            cx.stop_propagation();
        }
    }

    fn on_key_up(&mut self, ev: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(v) = self.view() else { return };
        let k = &ev.keystroke;
        if v.key(&KeyInput { key: &k.key, text: None, modifiers: mods(&k.modifiers), down: false }) {
            cx.stop_propagation();
        }
    }

    // --------------------------------------------------------- views

    fn toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let nav = |id: &'static str, label: &'static str, enabled: bool| {
            div()
                .id(id)
                .w(px(28.))
                .h(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .text_sm()
                .text_color(rgb(if enabled { theme::FG } else { theme::RULE }))
                .when(enabled, |d| d.cursor_pointer().hover(|s| s.bg(rgb(theme::RAISED))))
                .child(label)
        };
        let loading = self.chrome.loading;
        div()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_2()
            .bg(rgb(theme::PANE))
            .border_b_1()
            .border_color(rgb(theme::RULE))
            .child(nav("back", "←", self.chrome.can_go_back).on_click(cx.listener(|this, _, _, _| {
                if let Some(v) = this.view() {
                    v.go_back();
                }
            })))
            .child(nav("forward", "→", self.chrome.can_go_forward).on_click(cx.listener(|this, _, _, _| {
                if let Some(v) = this.view() {
                    v.go_forward();
                }
            })))
            .child(nav("reload", if loading { "✕" } else { "↻" }, !self.tabs.is_empty()).on_click(cx.listener(
                move |this, _, _, _| {
                    if let Some(v) = this.view() {
                        if loading { v.stop() } else { v.reload() }
                    }
                },
            )))
            .child(div().flex_1().min_w(px(0.)).child(self.address.clone()))
            .child(self.bookmark_button(cx))
            .child(self.focus_button(cx))
            .children(self.downloads_button(cx))
    }

    fn downloads_button(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let all: Vec<_> = self.tabs.iter().flat_map(|t| t.view.downloads()).collect();
        if all.is_empty() {
            return None;
        }
        let active = all.iter().filter(|d| !d.complete && !d.canceled).count();
        let label = if active > 0 { format!("↓ {active}") } else { format!("↓ {}", all.len()) };
        Some(
            div()
                .id("downloads")
                .h(px(28.))
                .flex()
                .items_center()
                .px_2()
                .rounded_md()
                .text_xs()
                .border_1()
                .border_color(rgb(if active > 0 { theme::ACCENT } else { theme::RULE }))
                .text_color(rgb(theme::FG))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(theme::RAISED)))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.downloads_open = !this.downloads_open;
                    cx.notify();
                }))
                .child(label),
        )
    }

    /// The find bar (Ctrl+F) and the downloads list, under the toolbar.
    fn panels(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let find = self.find_open.then(|| {
            let count = self
                .view()
                .and_then(|v| v.find_result())
                .map(|(n, i)| if n == 0 { "No matches".to_string() } else { format!("{i} of {n}") })
                .unwrap_or_default();
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .bg(rgb(theme::PANE))
                .border_b_1()
                .border_color(rgb(theme::RULE))
                .child(div().w(px(280.)).child(self.find_input.clone()))
                .child(div().text_xs().text_color(rgb(theme::MUTED)).child(count))
                .child(
                    div()
                        .id("find-close")
                        .px_2()
                        .rounded_sm()
                        .text_sm()
                        .text_color(rgb(theme::MUTED))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(theme::RAISED)))
                        .on_click(cx.listener(|this, _, _, cx| this.close_find(cx)))
                        .child("×"),
                )
        });
        let downloads = self.downloads_open.then(|| {
            let all: Vec<_> = self.tabs.iter().flat_map(|t| t.view.downloads()).collect();
            div()
                .flex()
                .flex_col()
                .gap_1()
                .px_3()
                .py_2()
                .bg(rgb(theme::PANE))
                .border_b_1()
                .border_color(rgb(theme::RULE))
                .children(all.into_iter().rev().take(8).map(|d| {
                    let status = if d.complete {
                        "done".to_string()
                    } else if d.canceled {
                        "failed".to_string()
                    } else {
                        format!("{}%", d.percent.max(0))
                    };
                    div()
                        .flex()
                        .gap_2()
                        .text_xs()
                        .child(div().flex_1().min_w(px(0.)).truncate().text_color(rgb(theme::FG)).child(d.name))
                        .child(div().text_color(rgb(theme::MUTED)).child(status))
                }))
                .child(
                    div()
                        .id("downloads-folder")
                        .text_xs()
                        .text_color(rgb(theme::ACCENT))
                        .cursor_pointer()
                        .on_click(|_, _, _| {
                            let _ = std::process::Command::new("xdg-open").arg(rail_browser::downloads_dir()).spawn();
                        })
                        .child(SharedString::from(format!("Open folder: {}", rail_browser::downloads_dir().display()))),
                )
        });
        div().flex().flex_col().children(find).children(downloads)
    }

    fn bookmark_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let can = site_of(&self.chrome.url).is_some();
        let marked = can && self.store.is_bookmarked(&self.chrome.url);
        div()
            .id("bookmark-page")
            .w(px(28.))
            .h(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_base()
            .text_color(rgb(if !can {
                theme::RULE
            } else if marked {
                theme::WARN
            } else {
                theme::MUTED
            }))
            .when(can, |d| {
                d.cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::RAISED)))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_bookmark_current(cx)))
            })
            .child(if marked { "★" } else { "☆" })
    }

    fn focus_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let site = site_of(&self.chrome.url);
        let focused = site.as_deref().is_some_and(|s| self.store.is_focused(s));
        div()
            .id("focus-site")
            .flex()
            .items_center()
            .gap_1()
            .h(px(28.))
            .px_2()
            .rounded_md()
            .text_xs()
            .border_1()
            .border_color(rgb(if focused { theme::ACCENT } else { theme::RULE }))
            .text_color(rgb(if site.is_none() {
                theme::RULE
            } else if focused {
                theme::ACCENT
            } else {
                theme::MUTED
            }))
            .when(site.is_some(), |d| {
                d.cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::RAISED)))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_focus_current(cx)))
            })
            .child(if focused { "● In focus" } else { "◎ Focus" })
    }

    fn page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for old in self.retired.drain(..) {
            let _ = window.drop_image(old);
        }
        let geometry = self.geometry.clone();
        let image = self.image.clone();
        let popup = self.popup.clone();
        let cursor = match self.chrome.cursor {
            Cursor::Arrow => CursorStyle::Arrow,
            Cursor::Pointer => CursorStyle::PointingHand,
            Cursor::Text => CursorStyle::IBeam,
            Cursor::Wait => CursorStyle::Arrow,
            Cursor::Crosshair => CursorStyle::Crosshair,
            Cursor::Move => CursorStyle::ClosedHand,
            Cursor::NotAllowed => CursorStyle::OperationNotAllowed,
            Cursor::ResizeColumn => CursorStyle::ResizeLeftRight,
            Cursor::ResizeRow => CursorStyle::ResizeUpDown,
        };
        let highlight = self.highlight.as_ref().map(|(r, label, _)| {
            let (x, y, w, h) = (r.x as f32, r.y as f32, r.width.max(4.) as f32, r.height.max(4.) as f32);
            vec![
                div()
                    .absolute()
                    .left(px(x - 2.))
                    .top(px(y - 2.))
                    .w(px(w + 4.))
                    .h(px(h + 4.))
                    .border_2()
                    .rounded_sm()
                    .border_color(rgb(theme::ACCENT))
                    .bg(gpui::rgba(0x5fc8c022))
                    .into_any_element(),
                div()
                    .absolute()
                    .left(px(x - 2.))
                    .top(px((y - 22.).max(0.)))
                    .px_1()
                    .rounded_sm()
                    .bg(rgb(theme::ACCENT))
                    .text_xs()
                    .text_color(rgb(theme::BG))
                    .child(SharedString::from(label.clone()))
                    .into_any_element(),
            ]
        });
        let agent_bar = (self.agent_busy && self.agent_driving && self.handoff.is_none()).then(|| {
            div()
                .absolute()
                .top(px(8.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .pl_3()
                        .pr_1()
                        .py_1()
                        .rounded_full()
                        .bg(rgb(theme::PANE))
                        .border_1()
                        .border_color(rgb(theme::ACCENT))
                        .text_xs()
                        .text_color(rgb(theme::FG))
                        .child(div().text_color(rgb(theme::ACCENT)).child("●"))
                        .child("Agent is using the browser")
                        .child(
                            div()
                                .id("agent-takeover")
                                .px_2()
                                .py(px(2.))
                                .rounded_full()
                                .border_1()
                                .border_color(rgb(theme::RULE))
                                .text_color(rgb(theme::FG))
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(theme::RAISED)))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(cx.listener(|_, _, _, cx| cx.emit(AgentControl::TookOver)))
                                .child("Take over"),
                        )
                        .child(
                            div()
                                .id("agent-stop")
                                .px_2()
                                .py(px(2.))
                                .rounded_full()
                                .bg(rgb(theme::ERROR))
                                .text_color(rgb(theme::BG))
                                .cursor_pointer()
                                .hover(|s| s.opacity(0.85))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(cx.listener(|_, _, _, cx| cx.emit(AgentControl::Stop)))
                                .child("Stop · Esc"),
                        ),
                )
        });
        div()
            .id("page")
            .relative()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .bg(rgb(0xffffff))
            .track_focus(&self.page_focus)
            .key_context("WebPage")
            .cursor(cursor)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_key_up(cx.listener(Self::on_key_up))
            .child(
                canvas(
                    move |bounds, window, _| {
                        geometry.set(Geometry { bounds: Some(bounds), scale: window.scale_factor() });
                    },
                    move |bounds, _, window, _| {
                        let Some(img) = image else { return };
                        // Frames are device pixels; draw them at logical size.
                        let s = window.scale_factor().max(0.01);
                        let fs = img.size(0);
                        let logical =
                            size(px(fs.width.0 as f32 / s), px(fs.height.0 as f32 / s));
                        let target = Bounds::new(point(bounds.origin.x, bounds.origin.y), logical);
                        let _ = window.paint_image(target, Corners::default(), img, 0, false);
                        if let Some((px_, py_, pimg)) = popup {
                            let ps = pimg.size(0);
                            let psize = size(px(ps.width.0 as f32 / s), px(ps.height.0 as f32 / s));
                            let at = point(bounds.origin.x + px(px_), bounds.origin.y + px(py_));
                            let _ = window.paint_image(Bounds::new(at, psize), Corners::default(), pimg, 0, false);
                        }
                    },
                )
                .size_full(),
            )
            .children(highlight.into_iter().flatten())
            .children(agent_bar)
            .children(self.dialog_card(cx))
            .children(self.handoff_banner(cx))
            .children(self.notice.as_ref().map(|(text, _)| {
                div()
                    .absolute()
                    .bottom(px(12.))
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .px_3()
                            .py_1()
                            .rounded_full()
                            .bg(rgb(theme::PANE))
                            .border_1()
                            .border_color(rgb(theme::RULE))
                            .text_xs()
                            .text_color(rgb(theme::MUTED))
                            .child(SharedString::from(text.clone())),
                    )
            }))
    }
}

impl BrowserPane {
    fn finish_handoff(&mut self, done: bool, cx: &mut Context<Self>) {
        if let Some(reply) = self.handoff.take().and_then(|h| h.reply) {
            let _ = reply.send(Ok(done));
        }
        cx.notify();
    }

    /// "Your turn": the agent is waiting for the user to do something.
    fn handoff_banner(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let h = self.handoff.as_ref()?;
        Some(
            div()
                .absolute()
                .top(px(8.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .max_w(px(560.))
                        .flex()
                        .items_center()
                        .gap_3()
                        .pl_3()
                        .pr_1()
                        .py_1()
                        .rounded_md()
                        .bg(rgb(theme::PANE))
                        .border_1()
                        .border_color(rgb(theme::WARN))
                        .shadow_lg()
                        .child(div().text_xs().text_color(rgb(theme::WARN)).child("Your turn"))
                        .child(div().flex_1().text_sm().text_color(rgb(theme::FG)).child(SharedString::from(h.message.clone())))
                        .child(
                            div()
                                .id("handoff-later")
                                .px_2()
                                .py(px(2.))
                                .rounded_md()
                                .text_xs()
                                .text_color(rgb(theme::MUTED))
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(theme::RAISED)))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(cx.listener(|this, _, _, cx| this.finish_handoff(false, cx)))
                                .child("Not now"),
                        )
                        .child(
                            div()
                                .id("handoff-done")
                                .px_3()
                                .py(px(2.))
                                .rounded_md()
                                .text_xs()
                                .bg(rgb(theme::ACCENT))
                                .text_color(rgb(theme::BG))
                                .cursor_pointer()
                                .hover(|s| s.opacity(0.85))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(cx.listener(|this, _, _, cx| this.finish_handoff(true, cx)))
                                .child("Done"),
                        ),
                ),
        )
    }

    /// The page's JavaScript dialog, shown to the user as a card.
    fn dialog_card(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let d = self.chrome.dialog.clone()?;
        let title = match d.kind.as_str() {
            "confirm" => "The page asks you to confirm",
            "prompt" => "The page asks for input",
            "beforeunload" => "Leave this page?",
            _ => "The page says",
        };
        let has_cancel = d.kind != "alert";
        let default_text = d.default_text.clone();
        let button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded_md()
                .text_sm()
                .cursor_pointer()
                .border_1()
                .border_color(rgb(if primary { theme::ACCENT } else { theme::RULE }))
                .bg(rgb(if primary { theme::ACCENT } else { theme::PANE }))
                .text_color(rgb(if primary { theme::BG } else { theme::FG }))
                .hover(|s| s.opacity(0.85))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(label)
        };
        Some(
            div()
                .absolute()
                .top(px(64.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .w(px(420.))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_4()
                        .rounded_md()
                        .bg(rgb(theme::PANE))
                        .border_1()
                        .border_color(rgb(theme::WARN))
                        .shadow_lg()
                        .child(div().text_xs().text_color(rgb(theme::WARN)).child(title))
                        .child(div().text_sm().text_color(rgb(theme::FG)).child(SharedString::from(d.message.clone())))
                        .when(d.kind == "prompt" && !d.default_text.is_empty(), |c| {
                            c.child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme::MUTED))
                                    .child(SharedString::from(format!("OK sends: “{}”", d.default_text))),
                            )
                        })
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .when(has_cancel, |r| {
                                    r.child(button("dialog-cancel", "Cancel", false).on_click(cx.listener(
                                        |this, _, _, cx| {
                                            if let Some(v) = this.view() {
                                                v.answer_dialog(false, None);
                                            }
                                            cx.notify();
                                        },
                                    )))
                                })
                                .child(button("dialog-ok", "OK", true).on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(v) = this.view() {
                                        v.answer_dialog(true, Some(&default_text));
                                    }
                                    cx.notify();
                                }))),
                        ),
                ),
        )
    }
}

impl Focusable for BrowserPane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.page_focus.clone()
    }
}

impl Render for BrowserPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div().flex().flex_col().flex_1().h_full().min_w(px(0.)).bg(rgb(theme::BG));
        if let Some(reason) = &self.unavailable {
            return root.justify_center().items_center().gap_2().child(
                div()
                    .max_w(px(520.))
                    .p_4()
                    .text_sm()
                    .text_color(rgb(theme::ERROR))
                    .child(SharedString::from(format!("The browser couldn't start: {reason}"))),
            );
        }
        root = root.child(self.tab_strip(cx)).child(self.toolbar(cx)).child(self.panels(cx));
        if let Some(err) = &self.chrome.load_error {
            root = root.child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .bg(rgb(theme::RAISED))
                    .text_color(rgb(theme::ERROR))
                    .child(SharedString::from(format!("Couldn't load the page: {err}"))),
            );
        }
        root.child(self.page(window, cx))
    }
}

fn mods(m: &gpui::Modifiers) -> Modifiers {
    Modifiers { shift: m.shift, control: m.control, alt: m.alt, command: m.platform }
}

fn map_button(b: MouseButton) -> Option<rail_browser::MouseButton> {
    match b {
        MouseButton::Left => Some(rail_browser::MouseButton::Left),
        MouseButton::Middle => Some(rail_browser::MouseButton::Middle),
        MouseButton::Right => Some(rail_browser::MouseButton::Right),
        MouseButton::Navigate(_) => None,
    }
}
