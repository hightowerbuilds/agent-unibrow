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
pub mod tools;

use cef::args::Args;
use cef::{CefString, ImplBrowser, ImplBrowserHost, ImplCommandLine, ImplFrame};
use std::cell::{Cell, Ref, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

pub use bridge::{BrowserBridge, NavAction, PageState, Request};
pub use keys::KeyInput;

/// Directory holding libcef.so, the .pak resources and locales. Set by the
/// build script from the CEF download.
pub const CEF_DIR: &str = env!("UNIBROW_CEF_DIR");

/// One rendered frame, BGRA, `width * height * 4` bytes.
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

/// Everything the UI shows about a page, updated by CEF callbacks.
#[derive(Default)]
pub struct ViewState {
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

/// One embedded page.
pub struct WebView {
    browser: cef::Browser,
    state: Rc<RefCell<ViewState>>,
    cdp_pending: handlers::Pending,
    cdp_next_id: Cell<i32>,
    _cdp_registration: Option<cef::Registration>,
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
        let window_info = cef::WindowInfo { windowless_rendering_enabled: 1, ..Default::default() };
        // Opaque white, like Chrome, so pages without a background aren't
        // transparent (they'd screenshot as black).
        let browser_settings =
            cef::BrowserSettings { windowless_frame_rate: 60, background_color: 0xFFFF_FFFF, ..Default::default() };
        let mut client = handlers::client(state.clone());
        let browser = cef::browser_host_create_browser_sync(
            Some(&window_info),
            Some(&mut client),
            Some(&CefString::from(url)),
            Some(&browser_settings),
            None,
            None,
        )
        .ok_or("Chromium couldn't create the page view")?;
        let cdp_pending: handlers::Pending = Rc::default();
        let registration = browser
            .host()
            .and_then(|h| h.add_dev_tools_message_observer(Some(&mut handlers::cdp_observer(cdp_pending.clone()))));
        Ok(Self { browser, state, cdp_pending, cdp_next_id: Cell::new(1), _cdp_registration: registration })
    }

    /// Sends a DevTools Protocol command; `reply` gets the `result` object.
    pub fn cdp(&self, method: &str, params: serde_json::Value, reply: bridge::Reply<serde_json::Value>) {
        let id = self.cdp_next_id.get();
        self.cdp_next_id.set(id.wrapping_add(1).max(1));
        let msg = serde_json::json!({"id": id, "method": method, "params": params}).to_string();
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
        let s = self.state.borrow();
        PageState {
            url: s.url.clone(),
            title: s.title.clone(),
            loading: s.loading,
            can_go_back: s.can_go_back,
            can_go_forward: s.can_go_forward,
            width: s.width,
            height: s.height,
        }
    }

    pub fn state(&self) -> Ref<'_, ViewState> {
        self.state.borrow()
    }

    /// Takes the newest frame, if one arrived since the last call.
    pub fn take_frame(&self) -> Option<Frame> {
        self.state.borrow_mut().frame.take()
    }

    fn host(&self) -> Option<cef::BrowserHost> {
        self.browser.host()
    }

    pub fn navigate(&self, url: &str) {
        self.state.borrow_mut().load_error = None;
        if let Some(frame) = self.browser.main_frame() {
            frame.load_url(Some(&CefString::from(url)));
        }
    }

    pub fn go_back(&self) {
        self.browser.go_back();
    }

    pub fn go_forward(&self) {
        self.browser.go_forward();
    }

    pub fn reload(&self) {
        self.browser.reload();
    }

    pub fn stop(&self) {
        self.browser.stop_load();
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
    use super::normalize_address;

    #[test]
    fn addresses() {
        assert_eq!(normalize_address("example.com"), "https://example.com");
        assert_eq!(normalize_address("localhost:3000"), "http://localhost:3000");
        assert_eq!(normalize_address("https://a.b/c"), "https://a.b/c");
        assert!(normalize_address("bass guitar nut").starts_with("https://duckduckgo.com/?q=bass+guitar+nut"));
    }
}
