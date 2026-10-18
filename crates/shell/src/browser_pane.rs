//! The center pane: an embedded Chromium page with a toolbar.
//!
//! Chromium renders off-screen; each new frame becomes a GPUI image painted
//! in the page area. Mouse, wheel and keyboard input on the page are
//! forwarded to Chromium. One pump loop drives CEF from the main thread.

use crate::text_input::{Changed, Submit, TextInput};
use crate::theme;
use gpui::{
    App, Bounds, ClipboardItem, Context, Corners, CursorStyle, Entity, FocusHandle, Focusable, KeyDownEvent,
    KeyUpEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, RenderImage, ScrollDelta,
    ScrollWheelEvent, SharedString, Subscription, Task, Window, canvas, div, img, point, prelude::*, px, rgb, size,
};
use browsing::{Browsing, site_of};
use gpui::EventEmitter;
use rail_browser::bridge::Rect;
use rail_browser::bridge::{TabInfo, TabOp};
use rail_browser::{
    ContextMenuInfo, Cursor, DialogInfo, EditCommand, KeyInput, Modifiers, NavAction, Request, WebView,
    normalize_address, zoom_factor, zoom_level_for,
};
use std::cell::Cell;
use std::collections::HashMap;
use std::time::Instant;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

const HOME: &str = "https://duckduckgo.com/";
/// Reader views are `data:` pages starting with this.
const READER_PREFIX: &str = "data:text/html;charset=utf-8;base64,";
/// How long the agent's highlight box stays on screen.
const HIGHLIGHT_FOR: Duration = Duration::from_millis(1600);
/// How often CEF gets to run. 8 ms keeps input and video smooth.
const PUMP_INTERVAL: Duration = Duration::from_millis(8);
/// Wheel "lines" to pixels. Chromium scrolls about this far per notch.
const PIXELS_PER_LINE: f32 = 40.0;
/// How soon a zoom level Chromium didn't take is applied again.
const ZOOM_RETRY: Duration = Duration::from_millis(250);
/// How often a tab that's making sound is checked for whether it stopped.
const AUDIO_CHECK: Duration = Duration::from_millis(1200);
/// A context menu Chromium asks for this soon after the user's own right
/// click is the user's; any other (the agent's CDP right click, a script)
/// is dismissed so no menu sits over the page the agent is driving.
const USER_MENU_WINDOW: Duration = Duration::from_millis(1500);
const MENU_WIDTH: f32 = 230.0;
const MENU_ITEM_HEIGHT: f32 = 26.0;
const MENU_SEPARATOR_HEIGHT: f32 = 9.0;
/// Whether media on the page (and same-origin frames) is playing out loud:
/// `playing`, `paused` (there's media, all quiet) or `none` (no media
/// elements: Web Audio, or a cross-origin frame we can't see into).
const AUDIO_JS: &str = r#"(() => {
  let any = false, playing = false;
  const scan = (d, depth) => {
    for (const m of d.querySelectorAll('audio, video')) {
      any = true;
      if (!m.paused && !m.ended && !m.muted && m.volume > 0 && m.readyState > 2) playing = true;
    }
    if (depth < 3) for (const f of d.querySelectorAll('iframe')) {
      try { if (f.contentDocument) scan(f.contentDocument, depth + 1); } catch (e) {}
    }
  };
  scan(document, 0);
  return playing ? 'playing' : (any ? 'paused' : 'none');
})()"#;

/// Logs pane events (zoom, favicons, sound, menus) for test instances.
fn debug(msg: impl FnOnce() -> String) {
    if std::env::var_os("UNIBROW_DEBUG_PANE").is_some() {
        eprintln!("pane: {}", msg());
    }
}

/// What a session's per-site zoom is keyed by: the site, or the scheme for
/// pages without one (reader pages, files).
fn zoom_key(url: &str) -> String {
    site_of(url).unwrap_or_else(|| url.split(':').next().unwrap_or("").to_string())
}

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
    /// The renderer died (why) or hung; the page area shows an overlay.
    crashed: Option<String>,
    unresponsive: bool,
    auth: Option<rail_browser::AuthRequest>,
    permission: Option<rail_browser::permissions::PermissionPrompt>,
    /// The page is this tab's reader view of this address.
    reader_of: Option<String>,
}

impl Chrome {
    /// The address to show: the article's own for a reader view.
    fn shown_url(&self) -> String {
        self.reader_of.clone().unwrap_or_else(|| self.url.clone())
    }
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

/// A hidden page the agent reads without touching the user's tabs.
struct BgPage {
    id: u32,
    view: WebView,
    opened: Instant,
}

/// Most background pages open at once.
const MAX_BG_PAGES: usize = 8;
/// A background page left open this long (a tool that died) is closed.
const BG_PAGE_MAX_AGE: Duration = Duration::from_secs(300);
/// Background pages lay out like a desktop window, whatever the pane size.
const BG_PAGE_SIZE: (i32, i32) = (1280, 900);

/// One open tab: its own Chromium page and toolbar state.
struct Tab {
    id: u32,
    view: WebView,
    chrome: Chrome,
    favicon: Option<Arc<RenderImage>>,
    favicon_seq: u64,
    /// Chromium zoom level (0 = 100%).
    zoom: f64,
    /// The level last applied to Chromium, and when.
    zoom_asked: Option<(f64, Instant)>,
    audio: Audio,
    /// The reader view this tab last opened.
    reader: Option<Reader>,
}

/// A tab's sound indicator.
#[derive(Default)]
struct Audio {
    audible: bool,
    muted: bool,
    checking: bool,
    last_check: Option<Instant>,
}

struct Reader {
    data_url: String,
    source: String,
}

/// The app's context menu, open over a tab's page.
struct OpenMenu {
    tab_id: u32,
    info: ContextMenuInfo,
    /// Where it opens, in page-area pixels.
    x: f32,
    y: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuCmd {
    Back,
    Forward,
    Reload,
    OpenLink,
    CopyLink,
    OpenImage,
    CopyImage,
    Cut,
    Copy,
    Paste,
    SelectAll,
    SavePdf,
    ViewSource,
}

/// The context menu for what was right-clicked: `None` is a separator.
fn menu_entries(info: &ContextMenuInfo, chrome: &Chrome) -> Vec<Option<(&'static str, bool, MenuCmd)>> {
    let mut out = Vec::new();
    if !info.link_url.is_empty() {
        out.push(Some(("Open link in new tab", true, MenuCmd::OpenLink)));
        out.push(Some(("Copy link address", true, MenuCmd::CopyLink)));
        out.push(None);
    }
    if !info.image_url.is_empty() {
        out.push(Some(("Open image in new tab", true, MenuCmd::OpenImage)));
        out.push(Some(("Copy image address", true, MenuCmd::CopyImage)));
        out.push(None);
    }
    let has_selection = !info.selection.trim().is_empty();
    if info.editable {
        out.push(Some(("Cut", info.can_cut || has_selection, MenuCmd::Cut)));
        out.push(Some(("Copy", info.can_copy || has_selection, MenuCmd::Copy)));
        out.push(Some(("Paste", info.can_paste, MenuCmd::Paste)));
        out.push(Some(("Select all", info.can_select_all, MenuCmd::SelectAll)));
        out.push(None);
    } else if has_selection {
        out.push(Some(("Copy", true, MenuCmd::Copy)));
        out.push(None);
    }
    out.push(Some(("Back", chrome.can_go_back, MenuCmd::Back)));
    out.push(Some(("Forward", chrome.can_go_forward, MenuCmd::Forward)));
    out.push(Some(("Reload", true, MenuCmd::Reload)));
    out.push(None);
    if !info.editable {
        out.push(Some(("Select all", true, MenuCmd::SelectAll)));
    }
    out.push(Some(("Save page as PDF", true, MenuCmd::SavePdf)));
    let is_web = chrome.url.starts_with("http://") || chrome.url.starts_with("https://");
    out.push(Some(("View page source", is_web, MenuCmd::ViewSource)));
    out
}

/// A free file name in the downloads folder for a PDF of the page `title`.
fn pdf_path(dir: &std::path::Path, title: &str) -> std::path::PathBuf {
    let stem: String = title
        .chars()
        .map(|c| if c == '/' || c == '\\' || c.is_control() { '_' } else { c })
        .take(80)
        .collect();
    let stem = if stem.trim().is_empty() { "page".to_string() } else { stem.trim().to_string() };
    (0..1000)
        .map(|n| dir.join(if n == 0 { format!("{stem}.pdf") } else { format!("{stem} ({n}).pdf") }))
        .find(|p| !p.exists())
        .unwrap_or_else(|| dir.join(format!("{stem}.pdf")))
}

/// Test hook: writes a BGRA frame as a binary PPM.
fn save_ppm(path: &std::path::Path, frame: &rail_browser::Frame) {
    let mut out = format!("P6\n{} {}\n255\n", frame.width, frame.height).into_bytes();
    for px in frame.bgra.as_chunks::<4>().0 {
        out.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    let r = std::fs::write(path, out);
    eprintln!("pane test: saved {} ({}x{}): {r:?}", path.display(), frame.width, frame.height);
}

/// A BGRA frame as an image GPUI can paint.
fn render_image(frame: rail_browser::Frame) -> Option<Arc<RenderImage>> {
    let buf = image::RgbaImage::from_raw(frame.width, frame.height, frame.bgra)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buf)])))
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
    background: Vec<BgPage>,
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
    /// Zoom levels the user set this session, by site.
    site_zoom: HashMap<String, f64>,
    /// Ctrl+wheel distance not yet turned into a zoom step.
    zoom_wheel: f32,
    menu: Option<OpenMenu>,
    /// The user's last right click on the page: when and where.
    user_right_click: Option<(Instant, f32, f32)>,
    /// Test hook (`UNIBROW_TEST_PANE`): steps acting as the user would.
    test_steps: std::collections::VecDeque<String>,
    test_next: Instant,
    /// `shot:<name>`: the next page frame is saved under the data dir.
    test_shot: Option<String>,
    /// The active tab's site asked for an HTTP sign-in.
    auth: Option<AuthPrompt>,
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
                let url = this.chrome.shown_url();
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
            background: Vec::new(),
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
            site_zoom: HashMap::new(),
            zoom_wheel: 0.0,
            menu: None,
            user_right_click: None,
            test_steps: std::env::var("UNIBROW_TEST_PANE")
                .map(|s| s.split(';').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
                .unwrap_or_default(),
            test_next: Instant::now() + Duration::from_secs(2),
            test_shot: None,
            auth: None,
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
            if self.agent_busy && !self.agent_driving && !req.is_background() {
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
                    debug(|| {
                        let z = self.tabs.get(self.active).map(|t| zoom_factor(t.zoom)).unwrap_or(1.0);
                        format!(
                            "highlight {label:?} css ({:.0},{:.0} {:.0}x{:.0}) drawn at ({:.0},{:.0} {:.0}x{:.0}) zoom {:.0}%",
                            rect.x, rect.y, rect.width, rect.height,
                            rect.x * z, rect.y * z, rect.width * z, rect.height * z, z * 100.0
                        )
                    });
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
                Request::BgOpen { url, reply } => {
                    let r = if self.background.len() >= MAX_BG_PAGES {
                        Err(format!("{MAX_BG_PAGES} background pages are already open; wait for those to finish."))
                    } else {
                        WebView::create(&url, BG_PAGE_SIZE.0, BG_PAGE_SIZE.1, 1.0).map(|view| {
                            view.set_hidden(true);
                            let id = self.next_tab_id;
                            self.next_tab_id += 1;
                            self.background.push(BgPage { id, view, opened: Instant::now() });
                            id
                        })
                    };
                    let _ = reply.send(r);
                    changed = true;
                }
                Request::BgCdp { id, method, params, reply } => match self.background.iter().find(|p| p.id == id) {
                    Some(p) => p.view.cdp(&method, params, reply),
                    None => {
                        let _ = reply.send(Err("That background page is closed.".into()));
                    }
                },
                Request::BgState { id, reply } => {
                    let r = self.background.iter().find(|p| p.id == id).map(|p| p.view.page_state());
                    let _ = reply.send(r.ok_or_else(|| "That background page is closed.".to_string()));
                }
                Request::BgClose { id } => {
                    if let Some(i) = self.background.iter().position(|p| p.id == id) {
                        self.background.remove(i);
                        changed = true;
                    }
                }
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
        // Background pages: nobody is there to answer dialogs, pickers or
        // popups, so turn them down; close pages a tool left behind.
        for p in &self.background {
            if p.view.dialog().is_some() {
                p.view.answer_dialog(false, None);
            }
            // Sign-ins are the user's; a hung page would only time out.
            if p.view.auth_request().is_some() {
                p.view.answer_auth(None);
            }
            p.view.stop_unresponsive();
            if p.view.file_dialog().is_some() {
                p.view.answer_file_dialog(None);
            }
            let _ = p.view.take_new_tab_requests();
            let _ = p.view.take_blocked();
            if p.view.permission_prompt().is_some() {
                p.view.answer_permission(false, false);
            }
            let _ = p.view.take_frame();
        }
        let before = self.background.len();
        self.background.retain(|p| p.opened.elapsed() < BG_PAGE_MAX_AGE);
        changed |= before != self.background.len();
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
                if let Some(name) = self.test_shot.take() {
                    save_ppm(&rail_browser::data_dir().join(format!("{name}.ppm")), &frame);
                }
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
                    permission: s.permission_prompt.clone(),
                    reader_of: t
                        .reader
                        .as_ref()
                        .filter(|r| r.data_url == s.url || s.url.starts_with(READER_PREFIX))
                        .map(|r| r.source.clone()),
                    crashed: s.crashed.clone(),
                    unresponsive: s.unresponsive,
                    auth: s.auth.clone(),
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
        changed |= self.tick_tabs(cx);
        if !self.test_steps.is_empty() && Instant::now() >= self.test_next {
            let loading = self.tabs.get(self.active).is_some_and(|t| t.chrome.loading);
            if !loading {
                let step = self.test_steps.pop_front().unwrap_or_default();
                self.test_next = Instant::now() + Duration::from_millis(1500);
                self.run_test_step(&step, cx);
                changed = true;
            }
        }
        changed |= self.discard_idle_tabs();
        self.sync_auth_prompt(cx);
        if let Some(active) = self.tabs.get(self.active).map(|t| t.chrome.clone()) {
            if active != self.chrome {
                if active.shown_url() != self.chrome.shown_url() && !self.editing_address {
                    let url = active.shown_url();
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

    /// Per-tab upkeep: zoom, favicons, the sound indicator and context
    /// menus. Returns whether anything visible changed.
    fn tick_tabs(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        let now = Instant::now();
        let mut audio_checks = Vec::new();
        for t in self.tabs.iter_mut() {
            // Zoom: the level the user chose for this site this session.
            // Re-applied when Chromium resets it (a navigation to another site).
            let want = self.site_zoom.get(&zoom_key(&t.chrome.url)).copied().unwrap_or(0.0);
            let have = t.view.zoom_level();
            // While a navigation is committing Chromium may not take the
            // level at once; don't ask again on every tick.
            let asked_lately =
                t.zoom_asked.is_some_and(|(l, at)| (l - want).abs() < 1e-6 && now.duration_since(at) < ZOOM_RETRY);
            if (have - want).abs() > 1e-6 && !asked_lately {
                t.zoom_asked = Some((want, now));
                t.view.set_zoom_level(want);
                debug(|| format!("tab {} zoom {have:.3} -> {want:.3} ({})", t.id, t.chrome.url));
            }
            if t.zoom != want {
                t.zoom = want;
                changed = true;
            }

            let seq = t.view.state().favicon_seq;
            if seq != t.favicon_seq {
                t.favicon_seq = seq;
                let icon = t.view.take_favicon();
                debug(|| format!("tab {} favicon {:?}", t.id, icon.as_ref().map(|f| (f.width, f.height))));
                if let Some(old) = std::mem::replace(&mut t.favicon, icon.and_then(render_image)) {
                    self.retired.push(old);
                }
                changed = true;
            }

            let muted = t.view.is_audio_muted();
            if muted != t.audio.muted {
                t.audio.muted = muted;
                changed = true;
            }
            if t.view.audible_hint() && !t.audio.audible {
                debug(|| format!("tab {} started making sound", t.id));
                t.audio.audible = true;
                changed = true;
            }
            // A check whose answer never came (the page navigated away
            // mid-call) doesn't block the next one for long.
            if t.audio.checking && t.audio.last_check.is_some_and(|at| now.duration_since(at) > Duration::from_secs(10)) {
                t.audio.checking = false;
            }
            let due = t.audio.last_check.is_none_or(|at| now.duration_since(at) >= AUDIO_CHECK);
            if t.audio.audible && !t.audio.checking && due && t.chrome.dialog.is_none() {
                t.audio.checking = true;
                t.audio.last_check = Some(now);
                audio_checks.push((t.id, t.view.evaluate(AUDIO_JS)));
            }
        }
        for (id, rx) in audio_checks {
            cx.spawn(async move |this, cx| {
                let verdict = rx.await.ok().and_then(Result::ok).and_then(|v| v["result"]["value"].as_str().map(str::to_string));
                let _ = this.update(cx, |pane, cx| pane.audio_checked(id, verdict, cx));
            })
            .detach();
        }

        // Context menus: show the user's, dismiss everyone else's.
        let user_click = self.user_right_click.filter(|(at, _, _)| at.elapsed() < USER_MENU_WINDOW);
        let active_id = self.tabs.get(self.active).map(|t| t.id);
        for t in &self.tabs {
            let Some(info) = t.view.context_menu() else {
                if self.menu.as_ref().is_some_and(|m| m.tab_id == t.id) {
                    self.menu = None;
                    changed = true;
                }
                continue;
            };
            if self.menu.as_ref().is_some_and(|m| m.tab_id == t.id && m.info.seq == info.seq) {
                continue;
            }
            match user_click {
                Some((_, x, y)) if Some(t.id) == active_id => {
                    debug(|| format!("tab {} context menu {info:?}", t.id));
                    if let Some(old) = self.menu.take() {
                        if old.tab_id != t.id {
                            if let Some(o) = self.tabs.iter().find(|o| o.id == old.tab_id) {
                                o.view.close_context_menu();
                            }
                        }
                    }
                    self.user_right_click = None;
                    self.menu = Some(OpenMenu { tab_id: t.id, info, x, y });
                }
                _ => {
                    debug(|| format!("tab {} context menu not from the user: dismissed", t.id));
                    t.view.close_context_menu();
                }
            }
            changed = true;
        }
        changed
    }

    fn audio_checked(&mut self, id: u32, verdict: Option<String>, cx: &mut Context<Self>) {
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) else { return };
        t.audio.checking = false;
        let audible = match verdict.as_deref() {
            Some("playing") => true,
            Some("paused") => {
                t.view.clear_audible_hint();
                false
            }
            // No media elements it can see: trust Chromium's hint, which
            // is cleared when the tab goes to another site.
            _ => t.view.audible_hint(),
        };
        if audible != t.audio.audible {
            debug(|| format!("tab {id} sound {verdict:?} -> audible {audible}"));
            t.audio.audible = audible;
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
        self.tabs.push(Tab {
            id,
            view,
            chrome,
            favicon: None,
            favicon_seq: 0,
            zoom: 0.0,
            zoom_asked: None,
            audio: Audio::default(),
            reader: None,
        });
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
        self.close_menu(cx);
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
            let url = self.chrome.shown_url();
            self.address.update(cx, |a, cx| a.set_text(&url, cx));
        }
        cx.notify();
    }

    fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        let closing_active = index == self.active;
        if self.menu.as_ref().is_some_and(|m| m.tab_id == self.tabs[index].id) {
            self.close_menu(cx);
        }
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

    // ------------------------------------------------- zoom, sound, reader

    /// Zooms the active tab's site in (`direction` > 0), out (< 0) or back
    /// to 100% (0). Every tab on the site follows, for this session.
    pub fn zoom(&mut self, direction: i32, cx: &mut Context<Self>) {
        let Some(t) = self.tabs.get(self.active) else { return };
        let key = zoom_key(&t.chrome.url);
        let next = rail_browser::next_zoom(zoom_factor(t.zoom), direction);
        let level = zoom_level_for(next);
        if (next - 1.0).abs() < 1e-9 {
            self.site_zoom.remove(&key);
        } else {
            self.site_zoom.insert(key, level);
        }
        t.view.set_zoom_level(level);
        debug(|| format!("zoom {key} to {:.0}%", next * 100.0, key = zoom_key(&t.chrome.url)));
        cx.notify();
    }

    fn toggle_mute(&mut self, id: u32, cx: &mut Context<Self>) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
            let muted = !t.view.is_audio_muted();
            t.view.set_audio_muted(muted);
            t.audio.muted = muted;
            cx.notify();
        }
    }

    /// Shows the active page's article as a plain reader page, or goes back
    /// from the reader page to the original.
    pub fn toggle_reader(&mut self, cx: &mut Context<Self>) {
        let Some(t) = self.tabs.get(self.active) else { return };
        if t.chrome.reader_of.is_some() {
            t.view.go_back();
            return;
        }
        if t.chrome.dialog.is_some() {
            return;
        }
        let (id, source) = (t.id, t.chrome.url.clone());
        let rx = t.view.evaluate(&rail_browser::reader::article_expression());
        cx.spawn(async move |this, cx| {
            let article = rx.await.ok().and_then(Result::ok).map(|v| v["result"]["value"].clone());
            let _ = this.update(cx, |pane, cx| pane.show_reader(id, source, article, cx));
        })
        .detach();
    }

    fn show_reader(&mut self, id: u32, source: String, article: Option<serde_json::Value>, cx: &mut Context<Self>) {
        let html = article.as_ref().and_then(rail_browser::reader::reader_html);
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) else { return };
        if t.chrome.url != source {
            return; // the tab moved on while the article was read
        }
        match html {
            Some(html) => {
                let data_url = rail_browser::reader::data_url(&html);
                debug(|| format!("reader view of {source}: {} bytes", data_url.len()));
                t.view.navigate(&data_url);
                t.reader = Some(Reader { data_url, source });
            }
            None => {
                debug(|| format!("no article on {source}: {article:?}"));
                self.notice = Some(("This page doesn't have an article to show in reader view.".into(), Instant::now()));
            }
        }
        cx.notify();
    }

    // ---------------------------------------------------- context menu

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if let Some(m) = self.menu.take() {
            if let Some(t) = self.tabs.iter().find(|t| t.id == m.tab_id) {
                t.view.close_context_menu();
            }
            cx.notify();
        }
    }

    fn run_menu(&mut self, cmd: MenuCmd, cx: &mut Context<Self>) {
        let Some(m) = self.menu.take() else { return };
        let Some(i) = self.index_of(m.tab_id) else { return };
        let (view, chrome) = (&self.tabs[i].view, self.tabs[i].chrome.clone());
        view.close_context_menu();
        debug(|| format!("menu command {cmd:?}"));
        let info = m.info;
        match cmd {
            MenuCmd::Back => view.go_back(),
            MenuCmd::Forward => view.go_forward(),
            MenuCmd::Reload => view.reload(),
            MenuCmd::CopyLink => cx.write_to_clipboard(ClipboardItem::new_string(info.link_url)),
            MenuCmd::CopyImage => cx.write_to_clipboard(ClipboardItem::new_string(info.image_url)),
            MenuCmd::Cut | MenuCmd::Copy => {
                // The desktop clipboard gets the text directly; Chromium's
                // own clipboard may not reach it.
                if !info.selection.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(info.selection));
                }
                view.edit(if cmd == MenuCmd::Cut { EditCommand::Cut } else { EditCommand::Copy });
            }
            MenuCmd::Paste => match cx.read_from_clipboard().and_then(|c| c.text()) {
                Some(text) => view.insert_text(&text),
                None => view.edit(EditCommand::Paste),
            },
            MenuCmd::SelectAll => view.edit(EditCommand::SelectAll),
            MenuCmd::OpenLink => {
                let _ = self.open_tab(&info.link_url, false, cx);
            }
            MenuCmd::OpenImage => {
                let _ = self.open_tab(&info.image_url, false, cx);
            }
            MenuCmd::ViewSource => {
                let _ = self.open_tab(&format!("view-source:{}", chrome.url), true, cx);
            }
            MenuCmd::SavePdf => {
                let title = if chrome.title.is_empty() { site_of(&chrome.url).unwrap_or_default() } else { chrome.title };
                let path = pdf_path(&rail_browser::downloads_dir(), &title);
                let rx = view.save_pdf(&path.to_string_lossy());
                cx.spawn(async move |this, cx| {
                    let ok = rx.await.unwrap_or(false);
                    let _ = this.update(cx, |pane, cx| {
                        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        let text = if ok { format!("Saved {name} to Downloads") } else { "Couldn't save the page as a PDF".into() };
                        debug(|| format!("pdf {ok} {}", path.display()));
                        pane.notice = Some((text, Instant::now()));
                        cx.notify();
                    });
                })
                .detach();
            }
        }
        cx.notify();
    }

    fn context_menu_view(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let m = self.menu.as_ref()?;
        let chrome = &self.tabs.iter().find(|t| t.id == m.tab_id)?.chrome;
        let entries = menu_entries(&m.info, chrome);
        let bounds = self.geometry.get().bounds?;
        let (pw, ph) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let height: f32 =
            entries.iter().map(|e| if e.is_some() { MENU_ITEM_HEIGHT } else { MENU_SEPARATOR_HEIGHT }).sum::<f32>() + 8.0;
        // Open right and down from the pointer, flipped where it would run off.
        let x = if m.x + MENU_WIDTH > pw { (m.x - MENU_WIDTH).max(0.0) } else { m.x };
        let y = if m.y + height > ph { (m.y - height).max(0.0) } else { m.y };
        Some(
            div()
                .id("context-menu")
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(MENU_WIDTH))
                .py_1()
                .rounded_md()
                .bg(rgb(theme::PANE))
                .border_1()
                .border_color(rgb(theme::RULE))
                .shadow_lg()
                .occlude()
                .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_menu(cx)))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .children(entries.into_iter().enumerate().map(|(i, e)| match e {
                    None => div()
                        .h(px(MENU_SEPARATOR_HEIGHT))
                        .flex()
                        .items_center()
                        .child(div().w_full().h(px(1.)).bg(rgb(theme::RULE)))
                        .into_any_element(),
                    Some((label, enabled, cmd)) => div()
                        .id(("menu-item", i))
                        .h(px(MENU_ITEM_HEIGHT))
                        .mx_1()
                        .px_2()
                        .flex()
                        .items_center()
                        .rounded_sm()
                        .text_sm()
                        .text_color(rgb(if enabled { theme::FG } else { theme::RULE }))
                        .when(enabled, |d| {
                            d.cursor_pointer()
                                .hover(|s| s.bg(rgb(theme::RAISED)))
                                .on_click(cx.listener(move |this, _, _, cx| this.run_menu(cmd, cx)))
                        })
                        .child(label)
                        .into_any_element(),
                })),
        )
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
                let icon = match &t.favicon {
                    Some(image) => img(image.clone()).size(px(14.)).flex_none().into_any_element(),
                    None => div()
                        .size(px(14.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(rgb(theme::MUTED))
                        .child(if t.chrome.loading { "◌" } else { "○" })
                        .into_any_element(),
                };
                let sound = (t.audio.audible || t.audio.muted).then(|| {
                    let muted = t.audio.muted;
                    div()
                        .id(("tab-sound", t.id as usize))
                        .flex_none()
                        .px_1()
                        .rounded_sm()
                        .text_color(rgb(if muted { theme::MUTED } else { theme::ACCENT }))
                        .when(muted, |d| d.line_through())
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(theme::RULE)))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle_mute(id, cx);
                        }))
                        .child("♪")
                });
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
                    // Middle click closes, like Chrome.
                    .on_mouse_up(MouseButton::Middle, cx.listener(move |this, _, _, cx| {
                        if let Some(i) = this.index_of(id) {
                            this.close_tab(i, cx);
                        }
                    }))
                    .child(icon)
                    .child(div().flex_1().min_w(px(0.)).overflow_hidden().truncate().child(SharedString::from(title)))
                    .children(sound)
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
        if button == rail_browser::MouseButton::Right {
            self.user_right_click = Some((Instant::now(), x, y));
        }
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

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.close_menu(cx);
        let (dx, dy) = match ev.delta {
            ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
            ScrollDelta::Lines(l) => (l.x * PIXELS_PER_LINE, l.y * PIXELS_PER_LINE),
        };
        // Ctrl+wheel zooms, a step per notch (or per notch's worth of
        // touchpad travel), like Chrome.
        if ev.modifiers.control {
            self.zoom_wheel += dy;
            if self.zoom_wheel.abs() >= PIXELS_PER_LINE {
                let dir = if self.zoom_wheel > 0.0 { 1 } else { -1 };
                self.zoom_wheel = 0.0;
                self.zoom(dir, cx);
            }
            return;
        }
        let Some(v) = self.view() else { return };
        let (x, y) = self.local(ev.position);
        v.wheel(x, y, dx, dy, mods(&ev.modifiers));
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.user_took_over(cx);
        if self.menu.is_some() {
            self.close_menu(cx);
            if ev.keystroke.key == "escape" {
                cx.stop_propagation();
                return;
            }
        }
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
            .children(self.zoom_badge(cx))
            .child(self.reader_button(cx))
            .child(self.bookmark_button(cx))
            .child(self.focus_button(cx))
            .children(self.downloads_button(cx))
    }

    /// The page's zoom when it isn't 100%; clicking resets it.
    fn zoom_badge(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let t = self.tabs.get(self.active)?;
        if t.zoom.abs() < 1e-6 {
            return None;
        }
        let percent = (zoom_factor(t.zoom) * 100.0).round();
        Some(
            div()
                .id("zoom-reset")
                .h(px(28.))
                .flex()
                .items_center()
                .px_2()
                .rounded_md()
                .text_xs()
                .border_1()
                .border_color(rgb(theme::RULE))
                .text_color(rgb(theme::MUTED))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(theme::RAISED)).text_color(rgb(theme::FG)))
                .on_click(cx.listener(|this, _, _, cx| this.zoom(0, cx)))
                .child(SharedString::from(format!("{percent}%"))),
        )
    }

    fn reader_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let on = self.chrome.reader_of.is_some();
        let can = on || self.chrome.url.starts_with("http://") || self.chrome.url.starts_with("https://");
        div()
            .id("reader-view")
            .h(px(28.))
            .flex()
            .items_center()
            .px_2()
            .rounded_md()
            .text_xs()
            .border_1()
            .border_color(rgb(if on { theme::ACCENT } else { theme::RULE }))
            .text_color(rgb(if !can {
                theme::RULE
            } else if on {
                theme::ACCENT
            } else {
                theme::MUTED
            }))
            .when(can, |d| {
                d.cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::RAISED)))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_reader(cx)))
            })
            .child(if on { "Exit reader" } else { "Reader" })
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
        // The agent's rect is in CSS pixels; at page zoom Z each is Z of ours.
        let z = self.tabs.get(self.active).map(|t| zoom_factor(t.zoom) as f32).unwrap_or(1.0);
        let highlight = self.highlight.as_ref().map(|(r, label, _)| {
            let (x, y) = (r.x as f32 * z, r.y as f32 * z);
            let (w, h) = ((r.width as f32 * z).max(4.), (r.height as f32 * z).max(4.));
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
        let bg = self.background.len();
        let bar_label = match (self.agent_driving, bg) {
            (true, 0) => "Agent is using the browser".to_string(),
            (true, n) => format!("Agent is using the browser · reading {n} page{} in the background", if n == 1 { "" } else { "s" }),
            (false, n) => format!("Agent is reading {n} page{} in the background", if n == 1 { "" } else { "s" }),
        };
        let driving = self.agent_driving;
        let agent_bar = (self.agent_busy && (self.agent_driving || bg > 0) && self.handoff.is_none()).then(|| {
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
                        .child(bar_label)
                        .when(driving, |d| d.child(
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
                        ))
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
            .children(self.permission_card(cx))
            .children(self.crash_overlay(cx))
            .children(self.handoff_banner(cx))
            .children(self.context_menu_view(cx))
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

    /// A site asks for the camera, microphone, location or notifications
    /// (4.2). Only these buttons answer it; the agent has no way to.
    fn permission_card(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let p = self.chrome.permission.clone()?;
        let what = rail_browser::permissions::describe(&p.kinds);
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
        let answer = |allow: bool| {
            cx.listener(move |this: &mut Self, _, _, cx| {
                if let Some(v) = this.view() {
                    v.answer_permission(allow, true);
                }
                cx.notify();
            })
        };
        Some(
            div().absolute().top(px(64.)).left_0().right_0().flex().justify_center().child(
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
                    .child(div().text_xs().text_color(rgb(theme::WARN)).child("Permission request"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(theme::FG))
                            .child(SharedString::from(format!("{} wants to use your {what}.", p.origin))),
                    )
                    .child(div().text_xs().text_color(rgb(theme::MUTED)).child(
                        "Your answer is remembered for this site; change it under Focus › Site permissions. \
                         Agents can't answer this for you.",
                    ))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(button("permission-deny", "Deny", false).on_click(answer(false)))
                            .child(button("permission-allow", "Allow", true).on_click(answer(true))),
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

impl BrowserPane {
    /// One `UNIBROW_TEST_PANE` step, done the way the user's input would
    /// do it, with the outcome logged to stderr. Steps: `zoom+`, `zoom-`,
    /// `zoom0`, `zoom:<percent>`, `click:X:Y`, `rclick:X:Y` (page pixels), `menu:<label>`,
    /// `reader`, `mute`, `tab:<n>`, `wait`, `log`.
    fn run_test_step(&mut self, step: &str, cx: &mut Context<Self>) {
        let (name, arg) = step.split_once(':').unwrap_or((step, ""));
        let xy = || {
            let mut it = arg.split(':').filter_map(|v| v.parse::<f32>().ok());
            Some((it.next()?, it.next()?))
        };
        eprintln!("pane test: {step}");
        match name {
            "zoom+" => self.zoom(1, cx),
            "zoom-" => self.zoom(-1, cx),
            "zoom0" => self.zoom(0, cx),
            "zoom" => {
                // zoom:150 sets the active site's zoom to 150% at once.
                if let (Ok(percent), Some(t)) = (arg.parse::<f64>(), self.tabs.get(self.active)) {
                    let key = zoom_key(&t.chrome.url);
                    debug(|| format!("zoom {key} to {percent}%"));
                    self.site_zoom.insert(key, zoom_level_for(percent / 100.0));
                }
            }
            "click" | "rclick" => {
                if let (Some(v), Some((x, y))) = (self.view(), xy()) {
                    let b = if name == "click" { rail_browser::MouseButton::Left } else { rail_browser::MouseButton::Right };
                    v.set_focus(true);
                    v.mouse_move(x, y, Modifiers::default());
                    v.mouse_button(x, y, b, false, 1, Modifiers::default());
                    v.mouse_button(x, y, b, true, 1, Modifiers::default());
                    if name == "rclick" {
                        self.user_right_click = Some((Instant::now(), x, y));
                    }
                }
            }
            "menu" => {
                let found = self.menu.as_ref().and_then(|m| {
                    let chrome = &self.tabs.iter().find(|t| t.id == m.tab_id)?.chrome;
                    menu_entries(&m.info, chrome).into_iter().flatten().find(|(l, e, _)| *e && l.eq_ignore_ascii_case(arg))
                });
                match found {
                    Some((_, _, cmd)) => self.run_menu(cmd, cx),
                    None => eprintln!("pane test: no enabled menu item {arg:?} (menu open: {})", self.menu.is_some()),
                }
            }
            "reader" => self.toggle_reader(cx),
            "shot" => {
                self.test_shot = Some(if arg.is_empty() { "shot".into() } else { arg.to_string() });
                if let Some(v) = self.view() {
                    v.invalidate();
                }
            }
            "mute" => {
                if let Some(id) = self.tabs.get(self.active).map(|t| t.id) {
                    self.toggle_mute(id, cx);
                }
            }
            "tab" => {
                if let Ok(i) = arg.parse::<usize>() {
                    self.activate(i, cx);
                }
            }
            "log" => {
                for (i, t) in self.tabs.iter().enumerate() {
                    let st = t.view.page_state();
                    let url: String = t.chrome.url.chars().take(90).collect();
                    eprintln!(
                        "pane test: tab {i} id={} active={} url={url} title={:?} zoom={:.0}% css={}x{} favicon={:?} audible={} muted={} reader_of={:?}",
                        t.id,
                        i == self.active,
                        t.chrome.title,
                        zoom_factor(t.zoom) * 100.0,
                        st.width,
                        st.height,
                        t.favicon.as_ref().map(|f| f.size(0)),
                        t.audio.audible,
                        t.audio.muted,
                        t.chrome.reader_of,
                    );
                }
                let menu = self.menu.as_ref().map(|m| (m.x, m.y, m.info.clone()));
                eprintln!("pane test: menu={menu:?} notice={:?}", self.notice.as_ref().map(|n| &n.0));
            }
            _ => {}
        }
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
        root.children(self.auth_bar(cx)).child(self.page(window, cx))
    }
}

/// The sign-in a site asked for (HTTP basic/digest auth), answered by the
/// user in a bar above the page. The agent is only told it's there.
struct AuthPrompt {
    tab_id: u32,
    request: rail_browser::AuthRequest,
    user: Entity<TextInput>,
    pass: Entity<TextInput>,
    _subscriptions: Vec<Subscription>,
}

// Robustness: crashed and hung pages, HTTP sign-ins, discarding idle tabs.
impl BrowserPane {
    /// Closes the pages of tabs that have been in the background too long
    /// (they reload when shown). Returns true if any were discarded.
    fn discard_idle_tabs(&mut self) -> bool {
        let Some(after) = rail_browser::discard_after() else { return false };
        let mut any = false;
        for (i, t) in self.tabs.iter().enumerate() {
            if i != self.active && t.view.discard_if_idle(after) {
                eprintln!("discarded idle tab {} ({})", t.id, t.chrome.url);
                any = true;
            }
        }
        any
    }

    /// Shows the sign-in bar while the active tab's site waits for one.
    fn sync_auth_prompt(&mut self, cx: &mut Context<Self>) {
        for t in &self.tabs {
            t.view.finish_auth();
        }
        let Some(tab) = self.tabs.get(self.active) else { return };
        let (tab_id, request) = (tab.id, tab.chrome.auth.clone());
        let current = self.auth.as_ref().map(|a| (a.tab_id, a.request.clone()));
        match request {
            None if self.auth.is_some() => {
                self.auth = None;
                cx.notify();
            }
            Some(req) if current != Some((tab_id, req.clone())) => {
                let user = cx.new(|cx| TextInput::new("Username", cx));
                let pass = cx.new(|cx| {
                    let mut t = TextInput::new("Password", cx);
                    t.set_masked(true, cx);
                    t
                });
                let subs = vec![
                    cx.subscribe(&user, |this: &mut Self, _, _: &Submit, cx| this.answer_auth(true, cx)),
                    cx.subscribe(&pass, |this: &mut Self, _, _: &Submit, cx| this.answer_auth(true, cx)),
                ];
                self.auth = Some(AuthPrompt { tab_id, request: req, user: user.clone(), pass: pass.clone(), _subscriptions: subs });
                // Test hook: sign in as `user:password` without the UI.
                if let Some((u, p)) = std::env::var("UNIBROW_TEST_AUTH").ok().as_deref().and_then(|v| v.split_once(':')) {
                    user.update(cx, |t, cx| t.set_text(u, cx));
                    pass.update(cx, |t, cx| t.set_text(p, cx));
                    self.answer_auth(true, cx);
                }
                cx.notify();
            }
            _ => {}
        }
    }

    fn answer_auth(&mut self, sign_in: bool, cx: &mut Context<Self>) {
        let Some(prompt) = self.auth.take() else { return };
        if let Some(t) = self.tabs.iter().find(|t| t.id == prompt.tab_id) {
            if sign_in {
                let user = prompt.user.read(cx).text().to_string();
                let pass = prompt.pass.read(cx).text().to_string();
                t.view.answer_auth(Some((&user, &pass)));
            } else {
                t.view.answer_auth(None);
            }
        }
        cx.notify();
    }

    /// "example.com asks you to sign in", with username and password
    /// fields. Outside the page area, so typing here never reaches the page.
    fn auth_bar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let prompt = self.auth.as_ref()?;
        if self.tabs.get(self.active).map(|t| t.id) != Some(prompt.tab_id) {
            return None;
        }
        let r = &prompt.request;
        let who = if r.is_proxy { format!("The proxy {}", r.site()) } else { r.site() };
        let mut label = format!("{who} asks you to sign in");
        if !r.realm.is_empty() {
            label.push_str(&format!(" (“{}”)", r.realm));
        }
        if !r.is_proxy && self.chrome.url.starts_with("http://") {
            label.push_str(" · not a secure connection");
        }
        let button = |id: &'static str, text: &'static str, primary: bool| {
            div()
                .id(id)
                .px_3()
                .py(px(2.))
                .rounded_md()
                .text_xs()
                .border_1()
                .border_color(rgb(if primary { theme::ACCENT } else { theme::RULE }))
                .bg(rgb(if primary { theme::ACCENT } else { theme::PANE }))
                .text_color(rgb(if primary { theme::BG } else { theme::FG }))
                .cursor_pointer()
                .hover(|s| s.opacity(0.85))
                .child(text)
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_1()
                .bg(rgb(theme::RAISED))
                .border_b_1()
                .border_color(rgb(theme::WARN))
                .child(div().text_xs().text_color(rgb(theme::WARN)).child(SharedString::from(label)))
                .child(div().w(px(180.)).child(prompt.user.clone()))
                .child(div().w(px(180.)).child(prompt.pass.clone()))
                .child(button("auth-cancel", "Cancel", false).on_click(cx.listener(|this, _, _, cx| this.answer_auth(false, cx))))
                .child(button("auth-sign-in", "Sign in", true).on_click(cx.listener(|this, _, _, cx| this.answer_auth(true, cx)))),
        )
    }

    /// "This page crashed" (or isn't responding) over the page area.
    fn crash_overlay(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let crashed = self.chrome.crashed.clone();
        if crashed.is_none() && !self.chrome.unresponsive {
            return None;
        }
        let (title, detail) = match &crashed {
            Some(why) => ("This page crashed".to_string(), format!("Chromium's page process stopped: {why}.")),
            None => ("This page isn't responding".to_string(), "A script on the page may be stuck.".to_string()),
        };
        let button = |id: &'static str, text: &'static str, primary: bool| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded_md()
                .text_sm()
                .border_1()
                .border_color(rgb(if primary { theme::ACCENT } else { theme::RULE }))
                .bg(rgb(if primary { theme::ACCENT } else { theme::PANE }))
                .text_color(rgb(if primary { theme::BG } else { theme::FG }))
                .cursor_pointer()
                .hover(|s| s.opacity(0.85))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(text)
        };
        let actions = match crashed {
            Some(_) => div().flex().gap_2().child(button("crash-reload", "Reload", true).on_click(cx.listener(
                |this, _, _, cx| {
                    if let Some(v) = this.view() {
                        v.reload();
                    }
                    cx.notify();
                },
            ))),
            None => div()
                .flex()
                .gap_2()
                .child(button("hung-wait", "Wait", false).on_click(cx.listener(|this, _, _, cx| {
                    if let Some(v) = this.view() {
                        v.wait_unresponsive();
                    }
                    cx.notify();
                })))
                .child(button("hung-stop", "Stop page", true).on_click(cx.listener(|this, _, _, cx| {
                    if let Some(v) = this.view() {
                        v.stop_unresponsive();
                    }
                    cx.notify();
                }))),
        };
        Some(
            div()
                .id("crash-overlay")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .bg(rgb(theme::BG))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(div().text_xl().text_color(rgb(theme::FG)).child(SharedString::from(title)))
                .child(div().text_sm().text_color(rgb(theme::MUTED)).child(SharedString::from(detail)))
                .child(div().text_xs().text_color(rgb(theme::MUTED)).child(SharedString::from(self.chrome.url.clone())))
                .child(actions),
        )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(info: &ContextMenuInfo, chrome: &Chrome) -> Vec<String> {
        menu_entries(info, chrome)
            .into_iter()
            .map(|e| match e {
                None => "-".to_string(),
                Some((l, enabled, _)) => if enabled { l.to_string() } else { format!("({l})") },
            })
            .collect()
    }

    #[test]
    fn page_menu() {
        let chrome = Chrome { url: "https://example.com/".into(), can_go_back: true, ..Default::default() };
        let got = labels(&ContextMenuInfo::default(), &chrome);
        assert_eq!(
            got,
            ["Back", "(Forward)", "Reload", "-", "Select all", "Save page as PDF", "View page source"]
        );
    }

    #[test]
    fn link_image_and_selection_menu() {
        let chrome = Chrome { url: "data:text/html,x".into(), ..Default::default() };
        let info = ContextMenuInfo {
            link_url: "https://a.example/".into(),
            image_url: "https://a.example/i.png".into(),
            selection: "hello".into(),
            ..Default::default()
        };
        let got = labels(&info, &chrome);
        assert_eq!(&got[..7], ["Open link in new tab", "Copy link address", "-", "Open image in new tab", "Copy image address", "-", "Copy"]);
        assert_eq!(got.last().unwrap(), "(View page source)");
    }

    #[test]
    fn editable_menu() {
        let chrome = Chrome::default();
        let info = ContextMenuInfo { editable: true, can_paste: true, can_select_all: true, ..Default::default() };
        let got = labels(&info, &chrome);
        assert_eq!(&got[..5], ["(Cut)", "(Copy)", "Paste", "Select all", "-"]);
        // No second "Select all" further down for an editable field.
        assert_eq!(got.iter().filter(|l| l.contains("Select all")).count(), 1);
    }

    #[test]
    fn zoom_keys() {
        assert_eq!(zoom_key("https://www.example.com/a"), "example.com");
        assert_eq!(zoom_key("http://localhost:8000/x.html"), "localhost");
        assert_eq!(zoom_key("data:text/html;base64,xyz"), "data");
    }

    #[test]
    fn pdf_names_are_safe() {
        let dir = std::env::temp_dir().join(format!("unibrow-pdf-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = pdf_path(&dir, "a/b\\c");
        assert_eq!(p.file_name().unwrap().to_string_lossy(), "a_b_c.pdf");
        std::fs::write(&p, b"x").unwrap();
        assert_eq!(pdf_path(&dir, "a/b\\c").file_name().unwrap().to_string_lossy(), "a_b_c (1).pdf");
        assert_eq!(pdf_path(&dir, "  ").file_name().unwrap().to_string_lossy(), "page.pdf");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
