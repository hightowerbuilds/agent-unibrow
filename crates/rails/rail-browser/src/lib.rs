//! Embedded Chromium (CEF), rendered off-screen into the app's own window.
//!
//! Chromium draws each frame into a BGRA buffer ([`Frame`]); the shell
//! paints it in the content pane and forwards mouse and keyboard input back.
//! There is no separate browser window: on Wayland a native child window
//! can't be embedded in another app's surface, so off-screen rendering is
//! the only way to put the page inside the app.
//!
//! Threading: CEF runs with an external message pump. [`initialize`] and
//! every [`pump`] call happen on the main thread (the GPUI thread), so all
//! CEF callbacks arrive there too and the view state needs no locking.

pub mod bridge;
mod handlers;
pub mod keys;
pub mod permissions;
pub mod reader;
pub mod tools;
pub mod vault;

use cef::args::Args;
use cef::{CefString, ImplBrowser, ImplBrowserHost, ImplCommandLine, ImplFrame};
use std::cell::{Cell, Ref, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

pub use bridge::{BrowserBridge, NavAction, PageState, Request};
pub use keys::KeyInput;

/// Directory holding libcef.so, the .pak resources and locales. Set by the
/// build script from the CEF download.
pub const CEF_DIR: &str = env!("UNIBROW_CEF_DIR");

/// One rendered frame, BGRA, `width * height * 4` bytes.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Cursor {
    #[default]
    Arrow,
    Pointer,
    Text,
    Wait,
    Crosshair,
    Move,
    NotAllowed,
    ResizeColumn,
    ResizeRow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// Modifier keys held during an input event.
#[derive(Clone, Copy, Debug, Default)]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub command: bool,
}

/// An open `<select>` list or similar, which Chromium paints separately.
pub struct PopupLayer {
    /// Position and size in view (CSS) pixels.
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub frame: Option<Frame>,
    pub frame_seq: u64,
}

/// A JavaScript dialog waiting for an answer.
#[derive(Clone, Debug, PartialEq)]
pub struct DialogInfo {
    /// `alert`, `confirm`, `prompt` or `beforeunload` ("leave this page?").
    pub kind: String,
    pub message: String,
    pub default_text: String,
}

/// A download, as reported by Chromium.
#[derive(Clone, Debug, PartialEq)]
pub struct DownloadInfo {
    pub id: u32,
    pub name: String,
    pub path: String,
    pub url: String,
    pub percent: i32,
    pub received: i64,
    pub total: i64,
    pub complete: bool,
    pub canceled: bool,
}

/// The page opened a file chooser (`<input type=file>`).
#[derive(Clone, Debug, PartialEq)]
pub struct FileDialogRequest {
    pub multiple: bool,
    pub title: String,
}

/// The CEF callback for a waiting permission request.
pub(crate) enum PendingPermission {
    Media { callback: cef::MediaAccessCallback, requested: u32 },
    Prompt { callback: cef::PermissionPromptCallback, id: u64 },
}

impl PendingPermission {
    pub(crate) fn answer(self, allow: bool) {
        use cef::{ImplMediaAccessCallback, ImplPermissionPromptCallback};
        use cef::sys::cef_permission_request_result_t as R;
        match self {
            PendingPermission::Media { callback, requested } => {
                if allow { callback.cont(requested) } else { callback.cancel() }
            }
            PendingPermission::Prompt { callback, .. } => callback.cont(cef::PermissionRequestResult::from(
                if allow { R::CEF_PERMISSION_RESULT_ACCEPT } else { R::CEF_PERMISSION_RESULT_DENY },
            )),
        }
    }
}

/// A site asked for an HTTP sign-in (basic or digest auth, or a proxy).
#[derive(Clone, Debug, PartialEq)]
pub struct AuthRequest {
    pub host: String,
    pub port: i32,
    pub realm: String,
    /// `basic`, `digest`, ...
    pub scheme: String,
    pub is_proxy: bool,
}

impl AuthRequest {
    /// "example.com", or "example.com:8080" for a non-default port.
    pub fn site(&self) -> String {
        if self.port > 0 && self.port != 80 && self.port != 443 {
            format!("{}:{}", self.host, self.port)
        } else {
            self.host.clone()
        }
    }
}

/// What was under the pointer when the page asked for a context menu
/// (right click). The UI draws its own menu from this.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContextMenuInfo {
    /// Increments for every menu, so the UI can tell a new one from an old one.
    pub seq: u64,
    /// Where the page put it, in view (DIP) pixels.
    pub x: i32,
    pub y: i32,
    pub link_url: String,
    /// The image's address, for a right click on an image.
    pub image_url: String,
    pub selection: String,
    pub editable: bool,
    pub can_cut: bool,
    pub can_copy: bool,
    pub can_paste: bool,
    pub can_select_all: bool,
    pub page_url: String,
}

/// Editing commands for the frame a context menu was opened on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditCommand {
    Cut,
    Copy,
    Paste,
    SelectAll,
}

/// Frame rate for the visible tab.
const VISIBLE_FRAME_RATE: i32 = 60;
/// Hidden tabs and background pages: Chromium doesn't paint them anyway,
/// so keep the compositor's clock as slow as it goes (scripts still run).
const HIDDEN_FRAME_RATE: i32 = 1;

/// How long a tab must stay in the background before it's discarded (its
/// page closed to free memory, then reloaded when the tab is shown).
/// `UNIBROW_DISCARD_AFTER_SECS` overrides the default of 30 minutes; 0
/// turns discarding off.
pub fn discard_after() -> Option<Duration> {
    static AFTER: std::sync::OnceLock<Option<Duration>> = std::sync::OnceLock::new();
    *AFTER.get_or_init(|| {
        let secs = std::env::var("UNIBROW_DISCARD_AFTER_SECS").ok().and_then(|v| v.trim().parse::<u64>().ok());
        match secs {
            Some(0) => None,
            Some(s) => Some(Duration::from_secs(s)),
            None => Some(Duration::from_secs(30 * 60)),
        }
    })
}

/// When a page went into the background, for discarding idle tabs.
#[derive(Clone, Copy, Debug, Default)]
pub struct Idle {
    hidden_since: Option<Instant>,
}

impl Idle {
    pub fn set_hidden(&mut self, hidden: bool, now: Instant) {
        if hidden {
            self.hidden_since.get_or_insert(now);
        } else {
            self.hidden_since = None;
        }
    }

    /// Hidden for at least `after`.
    pub fn due(&self, now: Instant, after: Duration) -> bool {
        self.hidden_since.is_some_and(|t| now.saturating_duration_since(t) >= after)
    }
}

/// Where downloads go: `~/Downloads/agent-unibrow`, or inside
/// `$UNIBROW_DATA_DIR` for test instances.
pub fn downloads_dir() -> PathBuf {
    let dir = match std::env::var_os("UNIBROW_DATA_DIR") {
        Some(d) => PathBuf::from(d).join("downloads"),
        None => dirs::download_dir().unwrap_or_else(|| data_dir().join("downloads")).join("agent-unibrow"),
    };
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Everything the UI shows about a page, updated by CEF callbacks.
#[derive(Default)]
pub struct ViewState {
    /// The page's favicon, BGRA, once downloaded; `favicon_seq` changes
    /// whenever it does (including when it's cleared).
    pub favicon: Option<Frame>,
    pub favicon_seq: u64,
    pub(crate) favicon_url: String,
    /// The last icon of each host this page visited, so going back (which
    /// doesn't announce icons again) still shows one.
    pub(crate) favicon_cache: std::collections::HashMap<String, (String, Frame)>,
    /// Chromium said the page started making sound since the hint was last
    /// cleared. (It doesn't say when it stops; the UI checks that.)
    pub audible_hint: bool,
    /// An open context menu, waiting for the UI to show and answer it.
    pub context_menu: Option<ContextMenuInfo>,
    pub(crate) context_menu_seq: u64,
    pub(crate) context_menu_callback: Option<cef::RunContextMenuCallback>,
    pub(crate) context_menu_frame: Option<cef::Frame>,
    /// Find-in-page result: (matches, active match ordinal).
    pub find: Option<(i32, i32)>,
    pub downloads: Vec<DownloadInfo>,
    pub file_dialog: Option<FileDialogRequest>,
    pub(crate) file_dialog_callback: Option<cef::FileDialogCallback>,
    /// Permission requests the browser turned down, for a notice.
    pub blocked: Vec<String>,
    /// A camera/mic/location/notifications request waiting for the user (4.2).
    pub permission_prompt: Option<permissions::PermissionPrompt>,
    pub(crate) permission_callback: Option<PendingPermission>,
    /// Links that asked for a new tab or window: (url, open in foreground).
    pub new_tabs: Vec<(String, bool)>,
    pub popup: Option<PopupLayer>,
    pub dialog: Option<DialogInfo>,
    pub(crate) dialog_callback: Option<cef::JsdialogCallback>,
    pub frame: Option<Frame>,
    /// Increments on every paint, so the UI can tell a new frame arrived.
    pub frame_seq: u64,
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub cursor: Cursor,
    pub load_error: Option<String>,
    /// The page's renderer process died (crash, out of memory, killed):
    /// why, in words. Cleared by reloading or navigating.
    pub crashed: Option<String>,
    /// The renderer stopped responding (a script stuck in a loop).
    pub unresponsive: bool,
    pub(crate) unresponsive_callback: Option<cef::UnresponsiveProcessCallback>,
    /// A site asked for an HTTP sign-in; the load waits for an answer.
    pub auth: Option<AuthRequest>,
    pub(crate) auth_callback: Option<cef::AuthCallback>,
    /// The user's answer, on its way to the site.
    pub(crate) auth_fetch: Option<handlers::AuthFetch>,
    /// The tab's page was closed to save memory; showing it reloads it.
    pub discarded: bool,
    /// Logical (DIP) size the page is laid out at.
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) scale: f32,
    /// Buttons currently held, for mouse-move modifier flags.
    pub(crate) buttons: u32,
}

/// Runs this process as a CEF helper (renderer, GPU, utility) if Chromium
/// launched it as one. Call first thing in `main`; if it returns
/// `Some(code)`, exit with that code immediately.
pub fn run_subprocess_if_needed() -> Option<i32> {
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
    let args = Args::new();
    let is_subprocess = args
        .as_cmd_line()
        .map(|cmd| cmd.has_switch(Some(&CefString::from("type"))) == 1)
        .unwrap_or(false);
    if !is_subprocess {
        return None;
    }
    let mut app = handlers::app();
    let code = cef::execute_process(Some(args.as_main_args()), Some(&mut app), std::ptr::null_mut());
    Some(code.max(0))
}

/// Where cookies, logins and site data persist between launches.
pub fn profile_dir() -> PathBuf {
    data_dir().join("browser")
}

/// App data root: `~/.local/share/agent-unibrow`, or `$UNIBROW_DATA_DIR`
/// (used to run an isolated test instance next to the user's).
pub fn data_dir() -> PathBuf {
    std::env::var_os("UNIBROW_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::data_local_dir().unwrap_or_else(std::env::temp_dir).join("agent-unibrow"))
}

/// Starts CEF in the browser (main) process. Call once on the main thread,
/// before the UI starts.
///
/// Identity (roadmap 6.7): the browser presents itself as what it is,
/// Chromium. The user agent is CEF's default Chrome one (no
/// `user_agent`/`user_agent_product` override), and nothing hides or fakes
/// automation: no stealth patches to `navigator`, no fingerprint changes.
/// `navigator.webdriver` is `false`, because the agent drives pages over
/// the DevTools protocol in-process rather than with `--enable-automation`
/// or a remote-debugging pipe. We don't force it to `true`: the user browses
/// in the same profile, and marking all of their own browsing as a bot would
/// get them blocked. Honesty is enforced in behavior instead: the agent
/// never solves CAPTCHAs or bot checks (it hands them to the user), and
/// the system prompt has it say it's an AI assistant whenever a person or
/// form asks who's writing.
pub fn initialize() -> Result<(), String> {
    let root = profile_dir();
    std::fs::create_dir_all(root.join("profile")).map_err(|e| format!("can't create browser profile dir: {e}"))?;
    let cef_dir = PathBuf::from(CEF_DIR);
    let settings = cef::Settings {
        windowless_rendering_enabled: 1,
        external_message_pump: 1,
        persist_session_cookies: 1,
        root_cache_path: CefString::from(root.to_string_lossy().as_ref()),
        cache_path: CefString::from(root.join("profile").to_string_lossy().as_ref()),
        resources_dir_path: CefString::from(cef_dir.to_string_lossy().as_ref()),
        locales_dir_path: CefString::from(cef_dir.join("locales").to_string_lossy().as_ref()),
        log_file: CefString::from(root.join("cef.log").to_string_lossy().as_ref()),
        log_severity: cef::LogSeverity::from(cef::sys::cef_log_severity_t::LOGSEVERITY_WARNING),
        ..Default::default()
    };
    let args = Args::new();
    let mut app = handlers::app();
    let ok = cef::initialize(Some(args.as_main_args()), Some(&settings), Some(&mut app), std::ptr::null_mut());
    if ok == 1 {
        Ok(())
    } else {
        Err(format!("Chromium failed to start; see {}", root.join("cef.log").display()))
    }
}

/// Lets CEF do its pending work. Call every few milliseconds on the main thread.
pub fn pump() {
    cef::do_message_loop_work();
}

/// Shuts CEF down. Call once, after every [`WebView`] is closed.
pub fn shutdown() {
    cef::shutdown();
}

/// A live Chromium page. A discarded tab has none.
struct Live {
    browser: cef::Browser,
    _cdp_registration: Option<cef::Registration>,
}

impl Live {
    fn open(
        url: &str,
        state: &Rc<RefCell<ViewState>>,
        pending: &handlers::Pending,
        events: &handlers::Events,
        rate: i32,
    ) -> Result<Self, String> {
        let window_info = cef::WindowInfo { windowless_rendering_enabled: 1, ..Default::default() };
        // Opaque white, like Chrome, so pages without a background aren't
        // transparent (they'd screenshot as black).
        let browser_settings =
            cef::BrowserSettings { windowless_frame_rate: rate, background_color: 0xFFFF_FFFF, ..Default::default() };
        let mut client = handlers::client(state.clone(), pending.clone());
        let browser = cef::browser_host_create_browser_sync(
            Some(&window_info),
            Some(&mut client),
            Some(&CefString::from(url)),
            Some(&browser_settings),
            None,
            None,
        )
        .ok_or("Chromium couldn't create the page view")?;
        let registration = browser
            .host()
            .and_then(|h| {
                h.add_dev_tools_message_observer(Some(&mut handlers::cdp_observer(pending.clone(), events.clone(), state.clone())))
            });
        Ok(Self { browser, _cdp_registration: registration })
    }
}

/// Profile settings that hold for every page: certificate warnings can't
/// be clicked through ("Proceed (unsafe)" is gone), by the user or the
/// agent. Needs a page's own request context, once it's initialized (the
/// first load), so each loading page tries until it has worked.
pub(crate) fn harden_profile(browser: &cef::Browser) {
    thread_local! { static DONE: Cell<bool> = const { Cell::new(false) }; }
    if DONE.with(|d| d.get()) {
        return;
    }
    use cef::{ImplPreferenceManager, ImplValue};
    let ctx = browser.host().and_then(|h| h.request_context());
    let (Some(ctx), Some(mut value)) = (ctx, cef::value_create()) else { return };
    value.set_bool(0);
    let name = CefString::from("ssl.error_override_allowed");
    // CEF wants somewhere to write an error (a null string is refused).
    let mut error = CefString::from("-");
    let r = ctx.set_preference(Some(&name), Some(&mut value), Some(&mut error));
    if r == 1 {
        DONE.with(|d| d.set(true));
    }
}

/// One embedded page.
pub struct WebView {
    live: RefCell<Option<Live>>,
    state: Rc<RefCell<ViewState>>,
    cdp_pending: handlers::Pending,
    cdp_events: handlers::Events,
    cdp_next_id: Cell<i32>,
    idle: Cell<Idle>,
    hidden: Cell<bool>,
}

impl WebView {
    /// Creates a page at `width` x `height` logical pixels.
    pub fn create(url: &str, width: i32, height: i32, scale: f32) -> Result<Self, String> {
        let state = Rc::new(RefCell::new(ViewState {
            url: url.to_string(),
            width: width.max(1),
            height: height.max(1),
            scale: if scale > 0.0 { scale } else { 1.0 },
            ..Default::default()
        }));
        let cdp_pending: handlers::Pending = Rc::default();
        let cdp_events: handlers::Events = Rc::default();
        let live = Live::open(url, &state, &cdp_pending, &cdp_events, VISIBLE_FRAME_RATE)?;
        Ok(Self {
            live: RefCell::new(Some(live)),
            state,
            cdp_pending,
            cdp_events,
            cdp_next_id: Cell::new(1),
            idle: Cell::new(Idle::default()),
            hidden: Cell::new(false),
        })
    }

    fn browser(&self) -> Option<cef::Browser> {
        self.live.borrow().as_ref().map(|l| l.browser.clone())
    }

    /// Sends a DevTools Protocol command; `reply` gets the `result` object.
    ///
    /// Two extensions for the agent tools, so the bridge stays a plain
    /// method/params channel: a `__session` param sends the command to a
    /// child session (an out-of-process iframe), and the local method
    /// `Unibrow.takeEvents` (`{"method": prefix}`) hands over the kept
    /// DevTools events whose method starts with `prefix`.
    pub fn cdp(&self, method: &str, mut params: serde_json::Value, reply: bridge::Reply<serde_json::Value>) {
        if method == "Unibrow.takeEvents" {
            let prefix = params["method"].as_str().unwrap_or("").to_string();
            let mut kept = self.cdp_events.borrow_mut();
            let (taken, rest): (Vec<_>, Vec<_>) =
                kept.drain(..).partition(|e| e["method"].as_str().is_some_and(|m| m.starts_with(&prefix)));
            kept.extend(rest);
            let _ = reply.send(Ok(serde_json::json!({ "events": taken })));
            return;
        }
        let session = params.as_object_mut().and_then(|o| o.remove("__session"));
        let id = self.cdp_next_id.get();
        self.cdp_next_id.set(id.wrapping_add(1).max(1));
        if let Some(why) = self.state.borrow().crashed.as_ref() {
            let _ = reply.send(Err(format!("the page crashed ({why}); reload it to recover")));
            return;
        }
        let mut msg = serde_json::json!({"id": id, "method": method, "params": params});
        if let Some(s) = session.filter(|s| s.is_string()) {
            msg["sessionId"] = s;
        }
        let msg = msg.to_string();
        let Some(host) = self.host() else {
            let _ = reply.send(Err("the page is closed".into()));
            return;
        };
        self.cdp_pending.borrow_mut().insert(id, reply);
        if host.send_dev_tools_message(Some(msg.as_bytes())) == 0 {
            if let Some(reply) = self.cdp_pending.borrow_mut().remove(&id) {
                let _ = reply.send(Err("Chromium rejected the DevTools message".into()));
            }
        }
    }

    /// The page state tools can read without a DevTools round trip.
    pub fn page_state(&self) -> PageState {
        // The view is laid out in DIPs; at page zoom Z a CSS pixel is Z DIPs.
        let zoom = zoom_factor(self.zoom_level());
        let s = self.state.borrow();
        PageState {
            url: s.url.clone(),
            title: s.title.clone(),
            loading: s.loading,
            can_go_back: s.can_go_back,
            can_go_forward: s.can_go_forward,
            width: (s.width as f64 / zoom).round().max(1.0) as i32,
            height: (s.height as f64 / zoom).round().max(1.0) as i32,
            dialog: s.dialog.clone(),
            load_error: page_problem(&s).or_else(|| s.load_error.clone()),
            crashed: s.crashed.is_some(),
            auth: s.auth.as_ref().map(|a| a.site()),
            permission_prompt: s.permission_prompt.as_ref().map(|p| format!("{} asks for {}", p.origin, permissions::describe(&p.kinds))),
        }
    }

    /// A site's request for the camera, microphone, location or
    /// notifications, waiting for the user's Allow or Deny.
    pub fn permission_prompt(&self) -> Option<permissions::PermissionPrompt> {
        self.state.borrow().permission_prompt.clone()
    }

    /// Answers the waiting permission request. `remember` keeps the answer
    /// for the site. Call only for the user's click (or to turn down a
    /// request nobody can see, without remembering).
    pub fn answer_permission(&self, allow: bool, remember: bool) {
        let (prompt, cb) = {
            let mut s = self.state.borrow_mut();
            (s.permission_prompt.take(), s.permission_callback.take())
        };
        if let (Some(p), true) = (&prompt, remember) {
            permissions::remember(&p.origin, &p.kinds, allow);
        }
        if let Some(cb) = cb {
            cb.answer(allow);
        }
    }

    pub fn state(&self) -> Ref<'_, ViewState> {
        self.state.borrow()
    }

    /// Takes the newest popup-layer frame (open dropdown list), if any.
    pub fn take_popup_frame(&self) -> Option<(i32, i32, i32, i32, Frame)> {
        let mut s = self.state.borrow_mut();
        let p = s.popup.as_mut()?;
        let f = p.frame.take()?;
        Some((p.x, p.y, p.width, p.height, f))
    }

    pub fn popup_visible(&self) -> bool {
        self.state.borrow().popup.is_some()
    }

    pub fn dialog(&self) -> Option<DialogInfo> {
        self.state.borrow().dialog.clone()
    }

    /// Answers the open JavaScript dialog: OK (`accept`) or Cancel, with
    /// `text` for prompts.
    pub fn answer_dialog(&self, accept: bool, text: Option<&str>) -> bool {
        let cb = {
            let mut s = self.state.borrow_mut();
            s.dialog = None;
            s.dialog_callback.take()
        };
        match cb {
            Some(cb) => {
                use cef::ImplJsdialogCallback;
                cb.cont(accept as i32, text.map(CefString::from).as_ref());
                true
            }
            None => false,
        }
    }

    /// Takes the newest frame, if one arrived since the last call.
    pub fn take_frame(&self) -> Option<Frame> {
        self.state.borrow_mut().frame.take()
    }

    fn host(&self) -> Option<cef::BrowserHost> {
        self.browser().and_then(|b| b.host())
    }

    /// A reload or navigation starts a new renderer after a crash.
    fn clear_crash(&self) {
        self.state.borrow_mut().crashed = None;
        // A hung renderer's question has to be answered; the new load decides.
        self.wait_unresponsive();
    }

    pub fn navigate(&self, url: &str) {
        self.clear_crash();
        self.state.borrow_mut().load_error = None;
        if let Some(frame) = self.browser().and_then(|b| b.main_frame()) {
            frame.load_url(Some(&CefString::from(url)));
        }
    }

    pub fn go_back(&self) {
        self.clear_crash();
        if let Some(b) = self.browser() {
            b.go_back();
        }
    }

    pub fn go_forward(&self) {
        self.clear_crash();
        if let Some(b) = self.browser() {
            b.go_forward();
        }
    }

    pub fn reload(&self) {
        let crashed = self.state.borrow().crashed.is_some();
        self.clear_crash();
        let Some(b) = self.browser() else { return };
        if crashed {
            // Chromium may not keep a committed entry for a renderer that died
            // mid-load; loading the URL again always brings it back.
            let url = self.state.borrow().url.clone();
            match b.main_frame().filter(|_| !url.is_empty()) {
                Some(frame) => frame.load_url(Some(&CefString::from(url.as_str()))),
                None => b.reload(),
            }
        } else {
            b.reload();
        }
    }

    pub fn stop(&self) {
        if let Some(b) = self.browser() {
            b.stop_load();
        }
    }

    /// Re-lays out the page for a new size or display scale.
    pub fn resize(&self, width: i32, height: i32, scale: f32) {
        let changed_scale = {
            let mut s = self.state.borrow_mut();
            let changed_scale = (s.scale - scale).abs() > f32::EPSILON;
            s.width = width.max(1);
            s.height = height.max(1);
            if scale > 0.0 {
                s.scale = scale;
            }
            changed_scale
        };
        if let Some(h) = self.host() {
            if changed_scale {
                h.notify_screen_info_changed();
            }
            h.was_resized();
        }
    }

    /// Find in page (the user's Ctrl+F). `find_next` continues the same search.
    pub fn find(&self, text: &str, forward: bool, find_next: bool) {
        if let Some(h) = self.host() {
            h.find(Some(&CefString::from(text)), forward as i32, 0, find_next as i32);
        }
    }

    pub fn stop_finding(&self) {
        if let Some(h) = self.host() {
            h.stop_finding(1);
        }
        self.state.borrow_mut().find = None;
    }

    pub fn find_result(&self) -> Option<(i32, i32)> {
        self.state.borrow().find
    }

    pub fn downloads(&self) -> Vec<DownloadInfo> {
        self.state.borrow().downloads.clone()
    }

    pub fn file_dialog(&self) -> Option<FileDialogRequest> {
        self.state.borrow().file_dialog.clone()
    }

    /// Answers the page's file chooser: the chosen paths, or `None` to cancel.
    pub fn answer_file_dialog(&self, paths: Option<Vec<String>>) {
        let cb = {
            let mut s = self.state.borrow_mut();
            s.file_dialog = None;
            s.file_dialog_callback.take()
        };
        if let Some(cb) = cb {
            use cef::ImplFileDialogCallback;
            match paths {
                Some(paths) if !paths.is_empty() => {
                    let mut list = cef::CefStringList::new();
                    for p in &paths {
                        list.append(p);
                    }
                    cb.cont(Some(&mut list));
                }
                _ => cb.cancel(),
            }
        }
    }

    pub fn take_blocked(&self) -> Vec<String> {
        std::mem::take(&mut self.state.borrow_mut().blocked)
    }

    /// Saves the page as a PDF at `path`; `done` gets whether it worked.
    pub fn print_to_pdf(&self, path: &str, done: futures::channel::oneshot::Sender<bool>) {
        if let Some(h) = self.host() {
            let settings = cef::PdfPrintSettings { print_background: 1, ..Default::default() };
            let mut cb = handlers::pdf_callback(done);
            h.print_to_pdf(Some(&CefString::from(path)), Some(&settings), Some(&mut cb));
        }
    }

    /// Asks Chromium to paint the whole view again.
    pub fn invalidate(&self) {
        if let Some(h) = self.host() {
            h.invalidate(cef::PaintElementType::from(cef::sys::cef_paint_element_type_t::PET_VIEW));
        }
    }

    /// Background tabs are hidden: Chromium stops painting them and
    /// throttles their timers, and their frame rate drops to the minimum.
    /// Showing a discarded tab loads its page again.
    pub fn set_hidden(&self, hidden: bool) {
        if !hidden {
            self.restore();
        }
        self.hidden.set(hidden);
        let mut idle = self.idle.get();
        idle.set_hidden(hidden, Instant::now());
        self.idle.set(idle);
        if let Some(h) = self.host() {
            if !hidden {
                h.set_windowless_frame_rate(VISIBLE_FRAME_RATE);
            }
            h.was_hidden(hidden as i32);
            if hidden {
                h.set_windowless_frame_rate(HIDDEN_FRAME_RATE);
            }
        }
    }

    // ------------------------------------------------ crashes, sign-ins

    /// The renderer is hung: give up on it, which shows the crash overlay
    /// (and Reload). Returns false if it wasn't hung.
    pub fn stop_unresponsive(&self) -> bool {
        let cb = {
            let mut s = self.state.borrow_mut();
            s.unresponsive = false;
            s.unresponsive_callback.take()
        };
        match cb {
            Some(cb) => {
                use cef::ImplUnresponsiveProcessCallback;
                cb.terminate();
                true
            }
            None => false,
        }
    }

    /// The renderer is hung: keep waiting for it.
    pub fn wait_unresponsive(&self) {
        let cb = {
            let mut s = self.state.borrow_mut();
            s.unresponsive = false;
            s.unresponsive_callback.take()
        };
        if let Some(cb) = cb {
            use cef::ImplUnresponsiveProcessCallback;
            cb.wait();
        }
    }

    pub fn auth_request(&self) -> Option<AuthRequest> {
        self.state.borrow().auth.clone()
    }

    /// Answers the site's HTTP sign-in: `(username, password)` from the
    /// user, or `None` to cancel (the site then shows its error page).
    pub fn answer_auth(&self, credentials: Option<(&str, &str)>) {
        let (cb, req) = {
            let mut s = self.state.borrow_mut();
            (s.auth_callback.take(), s.auth.take())
        };
        if let (None, Some(req)) = (credentials, &req) {
            self.state.borrow_mut().load_error = Some(format!(
                "{} needs a sign-in (HTTP {} authentication), which was cancelled. Only the user can \
                 sign in here: in the user's tab, use browser_handoff and reload the page so they get the prompt",
                req.site(),
                req.scheme
            ));
        }
        match (cb, credentials) {
            (Some(cb), Some((user, pass))) => {
                use cef::ImplAuthCallback;
                cb.cont(Some(&CefString::from(user)), Some(&CefString::from(pass)));
            }
            (Some(cb), None) => {
                use cef::ImplAuthCallback;
                cb.cancel();
            }
            // A main-frame sign-in (no callback): load the page again with
            // DevTools answering the site's challenge with these.
            (None, Some((user, pass))) => {
                let Some(req) = req else { return };
                let url = self.state.borrow().url.clone();
                self.state.borrow_mut().auth_fetch = Some(handlers::AuthFetch {
                    host: req.host,
                    username: user.to_string(),
                    password: pass.to_string(),
                    answered: false,
                    saw_loading: false,
                    next_id: handlers::AUTH_MESSAGE_IDS,
                });
                let ignore = || futures::channel::oneshot::channel().0;
                self.cdp(
                    "Fetch.enable",
                    serde_json::json!({"handleAuthRequests": true, "patterns": [{"urlPattern": "*"}]}),
                    ignore(),
                );
                self.cdp("Page.navigate", serde_json::json!({"url": url}), ignore());
            }
            (None, None) => {}
        }
        self.state.borrow_mut().frame_seq += 1;
    }

    /// Once the page loaded with the user's sign-in, stop intercepting:
    /// Chromium's auth cache has it for the rest of the session. Call from
    /// the pump loop.
    pub fn finish_auth(&self) {
        let done = {
            let mut s = self.state.borrow_mut();
            let loading = s.loading;
            let pending_auth = s.auth.is_some();
            match s.auth_fetch.as_mut() {
                Some(f) if loading => {
                    f.saw_loading = true;
                    false
                }
                // Loaded, or the site asked again (wrong password).
                Some(f) => f.saw_loading || pending_auth,
                None => false,
            }
        };
        if done {
            self.state.borrow_mut().auth_fetch = None;
            self.cdp("Fetch.disable", serde_json::json!({}), futures::channel::oneshot::channel().0);
        }
    }

    // ------------------------------------------------------- discarding

    pub fn is_discarded(&self) -> bool {
        self.live.borrow().is_none()
    }

    /// Something on the page would be lost or left hanging if it closed:
    /// an open dialog, file chooser or sign-in, or a download in progress.
    fn busy(&self) -> bool {
        let s = self.state.borrow();
        s.dialog.is_some()
            || s.file_dialog.is_some()
            || s.auth.is_some()
            || s.downloads.iter().any(|d| !d.complete && !d.canceled)
    }

    /// Discards the page if it has been hidden for `after` and nothing on
    /// it is in progress. Returns true if it was discarded now.
    pub fn discard_if_idle(&self, after: Duration) -> bool {
        if !self.hidden.get() || self.is_discarded() || !self.idle.get().due(Instant::now(), after) || self.busy() {
            return false;
        }
        self.discard();
        true
    }

    /// Closes the Chromium page to free its memory, keeping the URL and
    /// title. [`set_hidden`](Self::set_hidden)`(false)` brings it back.
    pub fn discard(&self) {
        let Some(live) = self.live.borrow_mut().take() else { return };
        if let Some(h) = live.browser.host() {
            h.close_browser(1);
        }
        for (_, reply) in self.cdp_pending.borrow_mut().drain() {
            let _ = reply.send(Err("the tab was put to sleep to save memory".into()));
        }
        let mut s = self.state.borrow_mut();
        s.discarded = true;
        s.frame = None;
        s.popup = None;
        s.loading = false;
        s.cursor = Cursor::Arrow;
        s.crashed = None;
        s.unresponsive = false;
        s.unresponsive_callback = None;
    }

    /// Recreates a discarded page at its last URL.
    fn restore(&self) {
        if !self.is_discarded() {
            return;
        }
        let url = {
            let s = self.state.borrow();
            if s.url.is_empty() { "about:blank".to_string() } else { s.url.clone() }
        };
        let rate = if self.hidden.get() { HIDDEN_FRAME_RATE } else { VISIBLE_FRAME_RATE };
        match Live::open(&url, &self.state, &self.cdp_pending, &self.cdp_events, rate) {
            Ok(live) => {
                *self.live.borrow_mut() = Some(live);
                let mut s = self.state.borrow_mut();
                s.discarded = false;
                s.load_error = None;
                s.loading = true;
            }
            Err(e) => self.state.borrow_mut().load_error = Some(format!("couldn't reopen the tab: {e}")),
        }
    }

    /// New-tab requests from links on this page since the last call.
    pub fn take_new_tab_requests(&self) -> Vec<(String, bool)> {
        std::mem::take(&mut self.state.borrow_mut().new_tabs)
    }

    pub fn set_focus(&self, focused: bool) {
        if let Some(h) = self.host() {
            h.set_focus(focused as i32);
        }
    }

    fn mouse_event(&self, x: f32, y: f32, m: Modifiers) -> cef::MouseEvent {
        let buttons = self.state.borrow().buttons;
        cef::MouseEvent { x: x.round() as i32, y: y.round() as i32, modifiers: keys::flags(m) | buttons }
    }

    pub fn mouse_move(&self, x: f32, y: f32, m: Modifiers) {
        let ev = self.mouse_event(x, y, m);
        if let Some(h) = self.host() {
            h.send_mouse_move_event(Some(&ev), 0);
        }
    }

    pub fn mouse_leave(&self) {
        let ev = self.mouse_event(-1.0, -1.0, Modifiers::default());
        if let Some(h) = self.host() {
            h.send_mouse_move_event(Some(&ev), 1);
        }
    }

    pub fn mouse_button(&self, x: f32, y: f32, button: MouseButton, up: bool, click_count: usize, m: Modifiers) {
        let (kind, flag) = match button {
            MouseButton::Left => (cef::sys::cef_mouse_button_type_t::MBT_LEFT, keys::LEFT_BUTTON),
            MouseButton::Middle => (cef::sys::cef_mouse_button_type_t::MBT_MIDDLE, keys::MIDDLE_BUTTON),
            MouseButton::Right => (cef::sys::cef_mouse_button_type_t::MBT_RIGHT, keys::RIGHT_BUTTON),
        };
        {
            let mut s = self.state.borrow_mut();
            if up {
                s.buttons &= !flag;
            } else {
                s.buttons |= flag;
            }
        }
        let ev = self.mouse_event(x, y, m);
        if let Some(h) = self.host() {
            h.send_mouse_click_event(
                Some(&ev),
                cef::MouseButtonType::from(kind),
                up as i32,
                click_count.clamp(1, 3) as i32,
            );
        }
    }

    /// Scrolls by `dx`, `dy` pixels (positive `dy` scrolls the page up).
    pub fn wheel(&self, x: f32, y: f32, dx: f32, dy: f32, m: Modifiers) {
        let ev = self.mouse_event(x, y, m);
        if let Some(h) = self.host() {
            h.send_mouse_wheel_event(Some(&ev), dx.round() as i32, dy.round() as i32);
        }
    }

    /// Sends a key press or release. Returns false for keys it can't map.
    pub fn key(&self, input: &KeyInput<'_>) -> bool {
        let Some(events) = keys::translate(input) else { return false };
        if let Some(h) = self.host() {
            for ev in &events {
                h.send_key_event(Some(ev));
            }
        }
        true
    }

    /// Types text as if from the keyboard (used for IME commits and pastes).
    pub fn insert_text(&self, text: &str) {
        if let Some(h) = self.host() {
            for ev in keys::text_events(text) {
                h.send_key_event(Some(&ev));
            }
        }
    }

    /// Chromium's zoom level for this page: 0 is 100%, each step is 1.2x.
    pub fn zoom_level(&self) -> f64 {
        self.host().map(|h| h.zoom_level()).unwrap_or(0.0)
    }

    pub fn set_zoom_level(&self, level: f64) {
        if let Some(h) = self.host() {
            h.set_zoom_level(level);
        }
    }

    /// Takes the downloaded favicon (`None` if the page has none or it was
    /// cleared). Check `state().favicon_seq` to know when to call this.
    pub fn take_favicon(&self) -> Option<Frame> {
        self.state.borrow_mut().favicon.take()
    }

    pub fn audible_hint(&self) -> bool {
        self.state.borrow().audible_hint
    }

    pub fn clear_audible_hint(&self) {
        self.state.borrow_mut().audible_hint = false;
    }

    pub fn set_audio_muted(&self, muted: bool) {
        if let Some(h) = self.host() {
            h.set_audio_muted(muted as i32);
        }
    }

    pub fn is_audio_muted(&self) -> bool {
        self.host().map(|h| h.is_audio_muted() == 1).unwrap_or(false)
    }

    /// Evaluates `expression` in the page (DevTools `Runtime.evaluate`, by
    /// value). The receiver gets the `result` object.
    pub fn evaluate(
        &self,
        expression: &str,
    ) -> futures::channel::oneshot::Receiver<Result<serde_json::Value, String>> {
        let (tx, rx) = futures::channel::oneshot::channel();
        let params = serde_json::json!({"expression": expression, "returnByValue": true, "timeout": 15000});
        self.cdp("Runtime.evaluate", params, tx);
        rx
    }

    /// Like [`WebView::print_to_pdf`], returning a receiver for the outcome.
    pub fn save_pdf(&self, path: &str) -> futures::channel::oneshot::Receiver<bool> {
        let (tx, rx) = futures::channel::oneshot::channel();
        self.print_to_pdf(path, tx);
        rx
    }

    /// The page's open context menu request, if any.
    pub fn context_menu(&self) -> Option<ContextMenuInfo> {
        self.state.borrow().context_menu.clone()
    }

    /// Dismisses the context menu request (the UI's menu closed or ran a
    /// command itself). Returns false if none was open.
    pub fn close_context_menu(&self) -> bool {
        let cb = {
            let mut s = self.state.borrow_mut();
            s.context_menu = None;
            s.context_menu_callback.take()
        };
        // Outside the borrow: cancelling calls back into the handler.
        match cb {
            Some(cb) => {
                use cef::ImplRunContextMenuCallback;
                cb.cancel();
                true
            }
            None => false,
        }
    }

    /// Cut, copy, paste or select all in the frame the last context menu
    /// was opened on (or the focused frame).
    pub fn edit(&self, command: EditCommand) {
        let frame = self.state.borrow().context_menu_frame.clone().or_else(|| self.browser().and_then(|b| b.focused_frame()));
        if let Some(f) = frame {
            match command {
                EditCommand::Cut => f.cut(),
                EditCommand::Copy => f.copy(),
                EditCommand::Paste => f.paste(),
                EditCommand::SelectAll => f.select_all(),
            }
        }
    }

    pub fn close(&self) {
        if let Some(h) = self.host() {
            h.close_browser(1);
        }
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        self.close();
    }
}

/// Chromium's zoom presets, as page zoom factors (like Chrome's menu).
pub const ZOOM_STEPS: [f64; 17] =
    [0.25, 0.33, 0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0, 5.0];

/// Page zoom factor for a Chromium zoom level (each level is 1.2x).
pub fn zoom_factor(level: f64) -> f64 {
    1.2f64.powf(level)
}

/// The Chromium zoom level for a zoom factor.
pub fn zoom_level_for(factor: f64) -> f64 {
    if factor <= 0.0 { 0.0 } else { factor.ln() / 1.2f64.ln() }
}

/// The next preset zoom factor in `direction` (+1 in, -1 out) from `factor`.
pub fn next_zoom(factor: f64, direction: i32) -> f64 {
    const EPS: f64 = 0.005;
    if direction > 0 {
        ZOOM_STEPS.iter().copied().find(|z| *z > factor + EPS).unwrap_or(ZOOM_STEPS[ZOOM_STEPS.len() - 1])
    } else if direction < 0 {
        ZOOM_STEPS.iter().rev().copied().find(|z| *z < factor - EPS).unwrap_or(ZOOM_STEPS[0])
    } else {
        1.0
    }
}

/// What's wrong with the page beyond a failed load, worded for the agent.
fn page_problem(s: &ViewState) -> Option<String> {
    if let Some(why) = &s.crashed {
        return Some(format!(
            "the page crashed ({why}). Its content is gone; reload it with browser_reload (or browser_open the URL again) to recover"
        ));
    }
    if let Some(a) = &s.auth {
        let realm = if a.realm.is_empty() { String::new() } else { format!(", realm “{}”", a.realm) };
        let who = if a.is_proxy { format!("The proxy {}", a.site()) } else { a.site() };
        return Some(format!(
            "{who} asks for a username and password (HTTP {} sign-in{realm}). You can't enter these: use \
             browser_handoff so the user signs in with the prompt shown above the page",
            a.scheme
        ));
    }
    if s.unresponsive {
        return Some("the page stopped responding (a script may be stuck); wait a little, or reload it".into());
    }
    None
}

/// Turns what the user typed in the address bar into a URL: adds `https://`
/// to things that look like hosts, and searches everything else.
pub fn normalize_address(input: &str) -> String {
    let s = input.trim();
    if s.contains("://") || s.starts_with("about:") || s.starts_with("data:") {
        return s.to_string();
    }
    let looks_like_host = !s.contains(' ')
        && (s.contains('.') || s.starts_with("localhost"))
        && !s.ends_with('.');
    if looks_like_host {
        let scheme = if s.starts_with("localhost") || s.starts_with("127.") { "http" } else { "https" };
        format!("{scheme}://{s}")
    } else {
        let mut url = url::Url::parse("https://duckduckgo.com/").expect("static url");
        url.query_pairs_mut().append_pair("q", s);
        url.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthRequest, Idle, next_zoom, normalize_address, zoom_factor, zoom_level_for};
    use std::time::{Duration, Instant};

    #[test]
    fn zoom_steps() {
        assert_eq!(next_zoom(1.0, 1), 1.1);
        assert_eq!(next_zoom(1.0, -1), 0.9);
        assert_eq!(next_zoom(1.0, 0), 1.0);
        assert_eq!(next_zoom(5.0, 1), 5.0);
        assert_eq!(next_zoom(0.25, -1), 0.25);
        // Off-preset factors (from Chromium's own 1.2x levels) snap to the next preset.
        assert_eq!(next_zoom(1.2, 1), 1.25);
        assert_eq!(next_zoom(1.2, -1), 1.1);
        for z in [0.5, 1.0, 1.25, 3.0] {
            assert!((zoom_factor(zoom_level_for(z)) - z).abs() < 1e-9);
        }
        assert_eq!(zoom_level_for(1.0), 0.0);
    }

    #[test]
    fn idle_tabs_come_due() {
        let t0 = Instant::now();
        let after = Duration::from_secs(60);
        let mut idle = Idle::default();
        assert!(!idle.due(t0 + after * 2, after), "a visible tab is never due");
        idle.set_hidden(true, t0);
        assert!(!idle.due(t0 + Duration::from_secs(59), after));
        assert!(idle.due(t0 + after, after));
        // Hiding again doesn't restart the clock.
        idle.set_hidden(true, t0 + Duration::from_secs(30));
        assert!(idle.due(t0 + after, after));
        // Showing it does.
        idle.set_hidden(false, t0 + Duration::from_secs(61));
        assert!(!idle.due(t0 + after * 10, after));
        idle.set_hidden(true, t0 + Duration::from_secs(100));
        assert!(!idle.due(t0 + Duration::from_secs(150), after));
        assert!(idle.due(t0 + Duration::from_secs(160), after));
        // A clock that goes backwards doesn't panic.
        assert!(!idle.due(t0, after));
    }

    #[test]
    fn auth_sites() {
        let mut a = AuthRequest { host: "example.com".into(), port: 443, realm: String::new(), scheme: "basic".into(), is_proxy: false };
        assert_eq!(a.site(), "example.com");
        a.port = 8080;
        assert_eq!(a.site(), "example.com:8080");
    }

    #[test]
    fn addresses() {
        assert_eq!(normalize_address("example.com"), "https://example.com");
        assert_eq!(normalize_address("localhost:3000"), "http://localhost:3000");
        assert_eq!(normalize_address("https://a.b/c"), "https://a.b/c");
        assert!(normalize_address("bass guitar nut").starts_with("https://duckduckgo.com/?q=bass+guitar+nut"));
    }
}
