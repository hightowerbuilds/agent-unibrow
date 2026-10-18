//! The center pane: an embedded Chromium page with a toolbar.
//!
//! Chromium renders off-screen; each new frame becomes a GPUI image painted
//! in the page area. Mouse, wheel and keyboard input on the page are
//! forwarded to Chromium. One pump loop drives CEF from the main thread.

use crate::text_input::{Submit, TextInput};
use crate::theme;
use gpui::{
    App, Bounds, Context, Corners, CursorStyle, Entity, FocusHandle, Focusable, KeyDownEvent, KeyUpEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, RenderImage, ScrollDelta, ScrollWheelEvent,
    SharedString, Subscription, Task, Window, canvas, div, point, prelude::*, px, rgb, size,
};
use browsing::{Browsing, site_of};
use gpui::EventEmitter;
use rail_browser::bridge::Rect;
use rail_browser::{Cursor, KeyInput, Modifiers, NavAction, Request, WebView, normalize_address};
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
}

/// History or focus changed (a visit was recorded, focus toggled).
pub struct BrowsingChanged;

/// The user wants the agent to stop: the Stop button, or they started
/// using the page themselves while the agent was driving it.
pub enum AgentControl {
    Stop,
    TookOver,
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
    view: Option<WebView>,
    /// Why Chromium couldn't start, if it couldn't.
    unavailable: Option<String>,
    image: Option<Arc<RenderImage>>,
    /// Old frames waiting to be released from the GPU atlas.
    retired: Vec<Arc<RenderImage>>,
    last_seq: u64,
    applied: (i32, i32, u32),
    geometry: Rc<Cell<Geometry>>,
    chrome: Chrome,
    address: Entity<TextInput>,
    editing_address: bool,
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
        let page_focus = cx.focus_handle();
        let address_focus = address.focus_handle(cx);
        let subscriptions = vec![
            cx.subscribe(&address, |this: &mut Self, _, ev: &Submit, cx| this.navigate(&ev.0, cx)),
            cx.on_focus(&address_focus, window, |this, _, _| this.editing_address = true),
            cx.on_blur(&address_focus, window, |this, _, cx| {
                this.editing_address = false;
                let url = this.chrome.url.clone();
                this.address.update(cx, |a, cx| a.set_text(&url, cx));
            }),
            cx.on_focus(&page_focus, window, |this, _, _| {
                if let Some(v) = &this.view {
                    v.set_focus(true);
                }
            }),
            cx.on_blur(&page_focus, window, |this, _, _| {
                if let Some(v) = &this.view {
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
            view: None,
            unavailable: cef.err(),
            image: None,
            retired: Vec::new(),
            last_seq: 0,
            applied: (0, 0, 0),
            geometry: Rc::new(Cell::new(Geometry::default())),
            chrome: Chrome::default(),
            address,
            editing_address: false,
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

        match &self.view {
            None => match WebView::create(&std::env::var("UNIBROW_START_URL").unwrap_or_else(|_| HOME.into()), w, h, scale) {
                Ok(v) => {
                    self.view = Some(v);
                    self.applied = wanted;
                }
                Err(e) => {
                    self.unavailable = Some(e);
                    cx.notify();
                    return;
                }
            },
            Some(v) if self.applied != wanted => {
                v.resize(w, h, scale);
                self.applied = wanted;
            }
            _ => {}
        }
        let Some(view) = &self.view else { return };

        let mut changed = false;
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
            }
        }
        if self.highlight.as_ref().is_some_and(|(_, _, at)| at.elapsed() > HIGHLIGHT_FOR) {
            self.highlight = None;
            changed = true;
        }
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

        let chrome = {
            let s = view.state();
            Chrome {
                url: s.url.clone(),
                title: s.title.clone(),
                loading: s.loading,
                can_go_back: s.can_go_back,
                can_go_forward: s.can_go_forward,
                cursor: s.cursor,
                load_error: s.load_error.clone(),
            }
        };
        if chrome != self.chrome {
            if chrome.url != self.chrome.url {
                if !self.editing_address {
                    let url = chrome.url.clone();
                    self.address.update(cx, |a, cx| a.set_text(&url, cx));
                }
                if self.store.record_visit(&chrome.url, &chrome.title) {
                    cx.emit(BrowsingChanged);
                }
            } else if chrome.title != self.chrome.title && !chrome.title.is_empty() {
                self.store.update_title(&chrome.url, &chrome.title);
                cx.emit(BrowsingChanged);
            }
            self.chrome = chrome;
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    pub fn navigate(&mut self, input: &str, cx: &mut Context<Self>) {
        if let Some(v) = &self.view {
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
        }
        cx.notify();
    }

    /// The user is acting on the page while the agent drives it.
    fn user_took_over(&mut self, cx: &mut Context<Self>) {
        if self.agent_busy && self.agent_driving {
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
        let Some(v) = &self.view else { return };
        v.set_focus(true);
        let Some(button) = map_button(ev.button) else { return };
        let (x, y) = self.local(ev.position);
        v.mouse_button(x, y, button, false, ev.click_count, mods(&ev.modifiers));
    }

    fn on_mouse_up(&mut self, ev: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        let (Some(v), Some(button)) = (&self.view, map_button(ev.button)) else { return };
        let (x, y) = self.local(ev.position);
        v.mouse_button(x, y, button, true, ev.click_count, mods(&ev.modifiers));
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, _: &mut Context<Self>) {
        let Some(v) = &self.view else { return };
        let (x, y) = self.local(ev.position);
        v.mouse_move(x, y, mods(&ev.modifiers));
    }

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, _: &mut Window, _: &mut Context<Self>) {
        let Some(v) = &self.view else { return };
        let (x, y) = self.local(ev.position);
        let (dx, dy) = match ev.delta {
            ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
            ScrollDelta::Lines(l) => (l.x * PIXELS_PER_LINE, l.y * PIXELS_PER_LINE),
        };
        v.wheel(x, y, dx, dy, mods(&ev.modifiers));
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.user_took_over(cx);
        let Some(v) = &self.view else { return };
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
        let Some(v) = &self.view else { return };
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
                if let Some(v) = &this.view {
                    v.go_back();
                }
            })))
            .child(nav("forward", "→", self.chrome.can_go_forward).on_click(cx.listener(|this, _, _, _| {
                if let Some(v) = &this.view {
                    v.go_forward();
                }
            })))
            .child(nav("reload", if loading { "✕" } else { "↻" }, self.view.is_some()).on_click(cx.listener(
                move |this, _, _, _| {
                    if let Some(v) = &this.view {
                        if loading { v.stop() } else { v.reload() }
                    }
                },
            )))
            .child(div().flex_1().min_w(px(0.)).child(self.address.clone()))
            .child(self.bookmark_button(cx))
            .child(self.focus_button(cx))
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
        let agent_bar = (self.agent_busy && self.agent_driving).then(|| {
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
                    },
                )
                .size_full(),
            )
            .children(highlight.into_iter().flatten())
            .children(agent_bar)
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
        root = root.child(self.toolbar(cx));
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
