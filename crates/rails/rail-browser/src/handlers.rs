//! CEF callback handlers. Each writes into the shared [`ViewState`].

use crate::{
    AuthRequest, ContextMenuInfo, Cursor, DialogInfo, DownloadInfo, FileDialogRequest, Frame as PixelFrame, PopupLayer,
    ViewState,
};
use cef::rc::Rc as _;
use cef::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

// ------------------------------------------------------------------- app

#[derive(Clone)]
pub struct UnibrowApp;

wrap_app! {
    pub struct AppBuilder {
        app: UnibrowApp,
    }

    impl App {
        fn on_register_custom_schemes(&self, registrar: Option<&mut SchemeRegistrar>) {
            if let Some(r) = registrar {
                crate::app_pages::register_scheme(r);
            }
        }

        fn on_before_command_line_processing(
            &self,
            process_type: Option<&CefStringUtf16>,
            command_line: Option<&mut CommandLine>,
        ) {
            let Some(cl) = command_line else { return };
            // Pages follow the desktop's dark preference
            // (`prefers-color-scheme`). Chromium asks GTK, which a CEF built
            // without GTK (Arch's, the one with H.264) can't, so ask the
            // desktop portal here, in the browser process, at startup.
            let browser_process = process_type.is_none_or(|t| t.to_string().is_empty());
            if browser_process && desktop_prefers_dark() {
                cl.append_switch(Some(&"force-dark-mode".into()));
            }
            cl.append_switch(Some(&"noerrdialogs".into()));
            cl.append_switch(Some(&"hide-crash-restore-bubble".into()));
            // Store passwords in Chromium's own encrypted file, not the
            // desktop keyring (avoids keyring prompts from a subprocess).
            cl.append_switch_with_value(Some(&"password-store".into()), Some(&"basic".into()));
            // Test instances: extra Chromium switches, comma-separated
            // (`mute-audio`, `name=value`), so real sites can be checked
            // without sound on the user's speakers. Only with
            // `$UNIBROW_DATA_DIR` set, like the shell's other test hooks.
            let extra = std::env::var_os("UNIBROW_DATA_DIR").and_then(|_| std::env::var("UNIBROW_TEST_SWITCHES").ok());
            if let Some(extra) = extra {
                for s in extra.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    match s.split_once('=') {
                        Some((k, v)) => cl.append_switch_with_value(Some(&k.into()), Some(&v.into())),
                        None => cl.append_switch(Some(&s.into())),
                    }
                }
            }
        }
    }
}

/// The desktop's color-scheme preference (xdg-desktop-portal: 1 is
/// "prefer dark"). Anything that goes wrong reads as no preference.
fn desktop_prefers_dark() -> bool {
    let out = std::process::Command::new("busctl")
        .args([
            "--user",
            "--timeout=1",
            "call",
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Settings",
            "ReadOne",
            "ss",
            "org.freedesktop.appearance",
            "color-scheme",
        ])
        .stderr(std::process::Stdio::null())
        .output();
    out.is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "v u 1")
}

pub fn app() -> cef::App {
    AppBuilder::new(UnibrowApp)
}

// ---------------------------------------------------------------- render

#[derive(Clone)]
pub struct Render {
    state: Rc<RefCell<ViewState>>,
}

wrap_render_handler! {
    pub struct RenderHandlerBuilder {
        handler: Render,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            if let Some(rect) = rect {
                let s = self.handler.state.borrow();
                rect.x = 0;
                rect.y = 0;
                rect.width = s.width.max(1);
                rect.height = s.height.max(1);
            }
        }

        fn screen_info(
            &self,
            _browser: Option<&mut Browser>,
            screen_info: Option<&mut ScreenInfo>,
        ) -> ::std::os::raw::c_int {
            let Some(info) = screen_info else { return 0 };
            let s = self.handler.state.borrow();
            info.device_scale_factor = s.scale;
            let r = Rect { x: 0, y: 0, width: s.width.max(1), height: s.height.max(1) };
            info.rect = r.clone();
            info.available_rect = r;
            1
        }

        fn on_popup_show(&self, _browser: Option<&mut Browser>, show: ::std::os::raw::c_int) {
            let mut s = self.handler.state.borrow_mut();
            if show == 0 {
                s.popup = None;
            } else if s.popup.is_none() {
                s.popup = Some(PopupLayer { x: 0, y: 0, width: 0, height: 0, frame: None, frame_seq: 0 });
            }
            s.frame_seq += 1; // repaint either way
        }

        fn on_popup_size(&self, _browser: Option<&mut Browser>, rect: Option<&Rect>) {
            let Some(r) = rect else { return };
            let mut s = self.handler.state.borrow_mut();
            let p = s.popup.get_or_insert(PopupLayer { x: 0, y: 0, width: 0, height: 0, frame: None, frame_seq: 0 });
            p.x = r.x;
            p.y = r.y;
            p.width = r.width;
            p.height = r.height;
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            if buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            let len = width as usize * height as usize * 4;
            // SAFETY: CEF guarantees `buffer` holds width*height BGRA pixels
            // for the duration of this call.
            let pixels = unsafe { std::slice::from_raw_parts(buffer, len) };
            let frame = PixelFrame { width: width as u32, height: height as u32, bgra: pixels.to_vec() };
            let mut s = self.handler.state.borrow_mut();
            if type_ == PaintElementType::from(cef::sys::cef_paint_element_type_t::PET_VIEW) {
                s.frame = Some(frame);
                s.frame_seq += 1;
            } else if let Some(p) = s.popup.as_mut() {
                // An open dropdown list, drawn over the page by the UI.
                p.frame = Some(frame);
                p.frame_seq += 1;
                s.frame_seq += 1;
            }
        }
    }
}

// --------------------------------------------------------------- display

#[derive(Clone)]
pub struct Display {
    state: Rc<RefCell<ViewState>>,
}

wrap_display_handler! {
    pub struct DisplayHandlerBuilder {
        handler: Display,
    }

    impl DisplayHandler {
        fn on_address_change(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            url: Option<&CefString>,
        ) {
            let is_main = frame.map(|f| f.is_main() == 1).unwrap_or(true);
            if let (true, Some(url)) = (is_main, url) {
                let url = url.to_string();
                let mut s = self.handler.state.borrow_mut();
                // A different site: its icon and sound aren't this page's.
                let host = host_of(&url);
                if host_of(&s.url) != host {
                    let cached = host.and_then(|h| s.favicon_cache.get(&h).cloned());
                    s.favicon_url = cached.as_ref().map(|(u, _)| u.clone()).unwrap_or_default();
                    s.favicon = cached.map(|(_, f)| f);
                    s.favicon_seq += 1;
                    s.audible_hint = false;
                }
                s.url = url;
            }
        }

        /// The page's icons changed: download the first one, small.
        fn on_favicon_urlchange(&self, browser: Option<&mut Browser>, icon_urls: Option<&mut CefStringList>) {
            // Iterate the borrowed list itself: `CefStringList::clone` copies
            // the opaque C struct and comes back empty.
            let urls: Vec<String> = icon_urls.map(|l| std::mem::take(l).into_iter().collect()).unwrap_or_default();
            debug(|| format!("favicon urls {urls:?}"));
            let Some(url) = pick_favicon(&urls) else { return };
            {
                let mut s = self.handler.state.borrow_mut();
                if s.favicon_url == url {
                    return;
                }
                s.favicon_url = url.clone();
            }
            let Some(host) = browser.and_then(|b| b.host()) else { return };
            let mut cb = FaviconCallbackBuilder::new(FaviconDone { state: self.handler.state.clone(), url: url.clone() });
            // 32 px covers a 16 px icon on a 2x display.
            host.download_image(Some(&CefString::from(url.as_str())), 1, 32, 0, Some(&mut cb));
        }

        /// The page put an element in fullscreen (a video's fullscreen
        /// button) or left it. Off-screen, Chromium only lays the element
        /// out over the whole view; the UI makes the view fill the screen.
        fn on_fullscreen_mode_change(&self, _browser: Option<&mut Browser>, fullscreen: ::std::os::raw::c_int) {
            debug(|| format!("fullscreen {fullscreen}"));
            let mut s = self.handler.state.borrow_mut();
            s.fullscreen = fullscreen != 0;
            s.frame_seq += 1;
        }

        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            if let Some(t) = title {
                self.handler.state.borrow_mut().title = t.to_string();
            }
        }

        fn on_cursor_change(
            &self,
            _browser: Option<&mut Browser>,
            _cursor: ::std::os::raw::c_ulong,
            type_: CursorType,
            _custom_cursor_info: Option<&CursorInfo>,
        ) -> ::std::os::raw::c_int {
            use cef::sys::cef_cursor_type_t as C;
            let raw: C = type_.into();
            let cursor = match raw {
                C::CT_HAND => Cursor::Pointer,
                C::CT_IBEAM | C::CT_VERTICALTEXT => Cursor::Text,
                C::CT_WAIT | C::CT_PROGRESS => Cursor::Wait,
                C::CT_CROSS => Cursor::Crosshair,
                C::CT_MOVE | C::CT_GRAB | C::CT_GRABBING => Cursor::Move,
                C::CT_NOTALLOWED | C::CT_NODROP => Cursor::NotAllowed,
                C::CT_COLUMNRESIZE | C::CT_EASTWESTRESIZE | C::CT_EASTRESIZE | C::CT_WESTRESIZE => {
                    Cursor::ResizeColumn
                }
                C::CT_ROWRESIZE | C::CT_NORTHSOUTHRESIZE | C::CT_NORTHRESIZE | C::CT_SOUTHRESIZE => {
                    Cursor::ResizeRow
                }
                _ => Cursor::Arrow,
            };
            self.handler.state.borrow_mut().cursor = cursor;
            1
        }
    }
}

/// Logs handler events for test instances (`UNIBROW_DEBUG_PANE`).
fn debug(msg: impl FnOnce() -> String) {
    if std::env::var_os("UNIBROW_DEBUG_PANE").is_some() {
        eprintln!("cef: {}", msg());
    }
}

fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string))
}

/// The icon to show from a page's candidates: a raster one if there is a
/// choice (SVG icons don't always decode), else the first.
fn pick_favicon(urls: &[String]) -> Option<String> {
    let raster = urls.iter().find(|u| !u.to_ascii_lowercase().split('?').next().unwrap_or("").ends_with(".svg"));
    raster.or(urls.first()).cloned()
}

#[derive(Clone)]
pub struct FaviconDone {
    state: Rc<RefCell<ViewState>>,
    url: String,
}

wrap_download_image_callback! {
    pub struct FaviconCallbackBuilder {
        handler: FaviconDone,
    }

    impl DownloadImageCallback {
        fn on_download_image_finished(
            &self,
            _image_url: Option<&CefString>,
            _http_status_code: ::std::os::raw::c_int,
            image: Option<&mut Image>,
        ) {
            let frame = image.and_then(favicon_pixels);
            debug(|| format!("favicon {} status {_http_status_code}: {:?}", self.handler.url, frame.as_ref().map(|f| (f.width, f.height, f.bgra[..4].to_vec()))));
            let mut s = self.handler.state.borrow_mut();
            if s.favicon_url != self.handler.url {
                return; // the page moved on to another icon
            }
            if let (Some(host), Some(f)) = (host_of(&s.url), frame.as_ref()) {
                if s.favicon_cache.len() > 64 {
                    s.favicon_cache.clear();
                }
                s.favicon_cache.insert(host, (self.handler.url.clone(), f.clone()));
            }
            s.favicon = frame;
            s.favicon_seq += 1;
        }
    }
}

/// An image's pixels as straight-alpha BGRA.
fn favicon_pixels(img: &mut Image) -> Option<PixelFrame> {
    if img.is_empty() == 1 {
        return None;
    }
    let (mut w, mut h) = (0, 0);
    let bitmap = img.as_bitmap(
        1.0,
        ColorType::from(cef::sys::cef_color_type_t::CEF_COLOR_TYPE_BGRA_8888),
        AlphaType::from(cef::sys::cef_alpha_type_t::CEF_ALPHA_TYPE_POSTMULTIPLIED),
        Some(&mut w),
        Some(&mut h),
    )?;
    if w <= 0 || h <= 0 {
        return None;
    }
    let mut buf = vec![0u8; bitmap.size()];
    bitmap.data(Some(&mut buf), 0);
    (buf.len() == w as usize * h as usize * 4).then_some(PixelFrame { width: w as u32, height: h as u32, bgra: buf })
}

// ------------------------------------------------------------------ load

#[derive(Clone)]
pub struct Load {
    state: Rc<RefCell<ViewState>>,
}

wrap_load_handler! {
    pub struct LoadHandlerBuilder {
        handler: Load,
    }

    impl LoadHandler {
        fn on_loading_state_change(
            &self,
            browser: Option<&mut Browser>,
            is_loading: ::std::os::raw::c_int,
            can_go_back: ::std::os::raw::c_int,
            can_go_forward: ::std::os::raw::c_int,
        ) {
            if let Some(b) = browser {
                crate::harden_profile(b);
                crate::spelling::enable(b);
            }
            let mut s = self.handler.state.borrow_mut();
            s.loading = is_loading == 1;
            s.can_go_back = can_go_back == 1;
            s.can_go_forward = can_go_forward == 1;
        }

        /// A page started loading (Chromium doesn't call this for failed
        /// loads): earlier errors, crashes and sign-in requests are over.
        fn on_load_start(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, _transition_type: TransitionType) {
            if !frame.map(|f| f.is_main() == 1).unwrap_or(false) {
                return;
            }
            let mut s = self.handler.state.borrow_mut();
            s.load_error = None;
            s.crashed = None;
            if s.auth_callback.is_none() {
                s.auth = None;
            }
        }

        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            error_code: Errorcode,
            error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            let is_main = frame.map(|f| f.is_main() == 1).unwrap_or(true);
            let code: cef::sys::cef_errorcode_t = error_code.into();
            // ERR_ABORTED (-3) fires for every navigation the user cancels.
            if !is_main || code == cef::sys::cef_errorcode_t::ERR_ABORTED {
                return;
            }
            let text = error_text.map(|t| t.to_string()).unwrap_or_default();
            let url = failed_url.map(|u| u.to_string()).unwrap_or_default();
            let mut s = self.handler.state.borrow_mut();
            if code == cef::sys::cef_errorcode_t::ERR_INVALID_AUTH_CREDENTIALS {
                // Chromium's own flow for a main-frame HTTP sign-in: the 401
                // page commits and a browser prompt would follow, which an
                // off-screen view never shows. The app asks the user instead.
                s.auth = Some(auth_request_for(&url));
                s.frame_seq += 1;
                return;
            }
            if is_cert_error(code) {
                // on_certificate_error already said it in more words.
                if !s.load_error.as_deref().is_some_and(|e| e.starts_with("certificate error")) {
                    s.load_error = Some(cert_error_text(code, &url));
                }
                return;
            }
            s.load_error = Some(format!("{text} ({url})"));
        }
    }
}

// --------------------------------------------- requests: crashes, certs, auth

fn is_cert_error(code: cef::sys::cef_errorcode_t) -> bool {
    (-220..=-200).contains(&(code as i32))
}

/// "certificate error: ..." for the agent and the error strip.
fn cert_error_text(code: cef::sys::cef_errorcode_t, url: &str) -> String {
    use cef::sys::cef_errorcode_t as E;
    let why = match code {
        E::ERR_CERT_COMMON_NAME_INVALID => "the certificate is for a different site",
        E::ERR_CERT_DATE_INVALID => "the certificate has expired or isn't valid yet",
        E::ERR_CERT_AUTHORITY_INVALID => "the certificate isn't from a trusted authority (self-signed or unknown issuer)",
        E::ERR_CERT_REVOKED => "the certificate was revoked",
        E::ERR_CERT_WEAK_SIGNATURE_ALGORITHM | E::ERR_CERT_WEAK_KEY => "the certificate uses weak cryptography",
        _ => "the site's identity couldn't be verified",
    };
    let host = text_host(url);
    format!(
        "certificate error: {why} ({code:?}) for {host}. The page was blocked, since the connection may not be \
         private. Don't try to get around this; tell the user"
    )
}

fn termination_text(status: TerminationStatus, code: i32) -> String {
    use cef::sys::cef_termination_status_t as T;
    let raw: T = status.into();
    let why = match raw {
        T::TS_PROCESS_WAS_KILLED => "its process was killed",
        T::TS_PROCESS_CRASHED => "its process crashed",
        T::TS_PROCESS_OOM => "it ran out of memory",
        T::TS_LAUNCH_FAILED => "its process couldn't start",
        T::TS_INTEGRITY_FAILURE => "a code integrity check failed",
        _ => "its process exited unexpectedly",
    };
    if code != 0 { format!("{why}, code {code}") } else { why.to_string() }
}

#[derive(Clone)]
pub struct Requests {
    state: Rc<RefCell<ViewState>>,
    /// DevTools calls in flight, failed at once when the renderer dies.
    pending: Pending,
}

wrap_request_handler! {
    pub struct RequestHandlerBuilder {
        handler: Requests,
    }

    impl RequestHandler {
        /// Web pages can't navigate to the app's own pages (homepage H1.4);
        /// see [`crate::app_pages::may_open`].
        fn on_before_browse(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _user_gesture: ::std::os::raw::c_int,
            _is_redirect: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let Some(request) = request else { return 0 };
            let url = CefString::from(&request.url()).to_string();
            if !crate::app_pages::is_app_page(&url) {
                return 0;
            }
            let (from, main) = frame.map(|f| (CefString::from(&f.url()).to_string(), f.is_main() == 1)).unwrap_or_default();
            if crate::app_pages::may_open(&from, main, request.transition_type().get_raw()) {
                return 0;
            }
            eprintln!("agent-unibrow: refused a page's navigation from {from:?} to {url}");
            1
        }

        /// The page's renderer died. The view keeps its last frame; the UI
        /// covers it with "This page crashed" and the agent is told.
        fn on_render_process_terminated(
            &self,
            _browser: Option<&mut Browser>,
            status: TerminationStatus,
            error_code: ::std::os::raw::c_int,
            _error_string: Option<&CefString>,
        ) {
            let why = termination_text(status, error_code);
            let mut s = self.handler.state.borrow_mut();
            eprintln!("renderer for {} terminated: {why}", s.url);
            s.crashed = Some(why);
            s.fullscreen = false;
            s.loading = false;
            s.unresponsive = false;
            s.unresponsive_callback = None;
            s.dialog = None;
            s.dialog_callback = None;
            s.popup = None;
            s.frame_seq += 1;
            drop(s);
            let waiting: Vec<_> = self.handler.pending.borrow_mut().drain().map(|(_, r)| r).collect();
            for reply in waiting {
                let _ = reply.send(Err("the page crashed; reload it to recover".into()));
            }
        }

        /// A hung renderer: wait (the default) until the user or the app
        /// decides; holding the callback lets the UI offer "Stop page".
        fn on_render_process_unresponsive(
            &self,
            _browser: Option<&mut Browser>,
            callback: Option<&mut UnresponsiveProcessCallback>,
        ) -> ::std::os::raw::c_int {
            let Some(cb) = callback else { return 0 };
            let mut s = self.handler.state.borrow_mut();
            s.unresponsive = true;
            s.unresponsive_callback = Some(cb.clone());
            s.frame_seq += 1;
            1
        }

        fn on_render_process_responsive(&self, _browser: Option<&mut Browser>) {
            let mut s = self.handler.state.borrow_mut();
            s.unresponsive = false;
            s.unresponsive_callback = None;
            s.frame_seq += 1;
        }

        /// Bad certificates are never accepted. The request is cancelled
        /// here and now: returning 0 would hand it to Chrome's warning page,
        /// whose "Proceed (unsafe)" link anyone (the agent too) could click.
        /// The agent and the user are told why.
        fn on_certificate_error(
            &self,
            _browser: Option<&mut Browser>,
            cert_error: Errorcode,
            request_url: Option<&CefString>,
            _ssl_info: Option<&mut Sslinfo>,
            callback: Option<&mut Callback>,
        ) -> ::std::os::raw::c_int {
            let code: cef::sys::cef_errorcode_t = cert_error.into();
            let url = request_url.map(|u| u.to_string()).unwrap_or_default();
            let text = cert_error_text(code, &url);
            let mut s = self.handler.state.borrow_mut();
            s.blocked.push(format!("{} has an invalid security certificate", text_host(&url)));
            s.load_error = Some(text);
            drop(s);
            match callback {
                Some(cb) => {
                    cb.cancel();
                    1
                }
                None => 0,
            }
        }

        /// HTTP basic/digest (or proxy) sign-in. Held for the user to
        /// answer in the app; the agent is told it can't do this itself.
        fn auth_credentials(
            &self,
            _browser: Option<&mut Browser>,
            _origin_url: Option<&CefString>,
            is_proxy: ::std::os::raw::c_int,
            host: Option<&CefString>,
            port: ::std::os::raw::c_int,
            realm: Option<&CefString>,
            scheme: Option<&CefString>,
            callback: Option<&mut AuthCallback>,
        ) -> ::std::os::raw::c_int {
            // Documented as maybe the IO thread; the view state is UI-only.
            if cef::currently_on(ThreadId::from(cef::sys::cef_thread_id_t::TID_UI)) == 0 {
                return 0;
            }
            let Some(cb) = callback else { return 0 };
            let mut s = self.handler.state.borrow_mut();
            if let Some(old) = s.auth_callback.take() {
                old.cancel();
            }
            s.auth = Some(AuthRequest {
                host: host.map(|h| h.to_string()).unwrap_or_default(),
                port,
                realm: realm.map(|r| r.to_string()).unwrap_or_default(),
                scheme: scheme.map(|r| r.to_string()).filter(|r| !r.is_empty()).unwrap_or_else(|| "basic".into()),
                is_proxy: is_proxy == 1,
            });
            s.auth_callback = Some(cb.clone());
            s.frame_seq += 1;
            1
        }
    }
}

/// The sign-in a main-frame 401 asked for (Chromium doesn't say the realm).
fn auth_request_for(url: &str) -> AuthRequest {
    let u = url::Url::parse(url).ok();
    AuthRequest {
        host: u.as_ref().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_else(|| url.to_string()),
        port: u.as_ref().and_then(|u| u.port_or_known_default()).map(i32::from).unwrap_or(0),
        realm: String::new(),
        scheme: "basic or digest".into(),
        is_proxy: false,
    }
}

fn text_host(url: &str) -> String {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_else(|| url.to_string())
}

// ------------------------------------------------------------- life span

/// A popup's page, waiting to be handed to the tab that opened it once
/// Chromium has created it.
pub(crate) struct Adoption {
    opener: Rc<RefCell<ViewState>>,
    pending: Pending,
    events: Events,
}

#[derive(Clone)]
pub struct LifeSpan {
    state: Rc<RefCell<ViewState>>,
    /// Set on a popup's own client until its page exists.
    adoption: Rc<RefCell<Option<Adoption>>>,
}

/// Whether a request for a new window is created through Chromium's popup
/// path, so the new page can have the requester as its `window.opener`
/// (roadmap 3.2): sign-in popups ("Sign in with Google") post their result
/// back that way. Ctrl/middle clicks (background tabs) never are. CEF
/// doesn't say whether the opener was suppressed (`target=_blank` links
/// imply `noopener`), so links come this way too; the UI asks the new page
/// whether it got an opener and shows it as a plain tab if not.
pub(crate) fn wants_popup(disposition: cef::sys::cef_window_open_disposition_t, scriptable: bool) -> bool {
    use cef::sys::cef_window_open_disposition_t as D;
    scriptable && matches!(disposition, D::CEF_WOD_NEW_POPUP | D::CEF_WOD_NEW_FOREGROUND_TAB | D::CEF_WOD_NEW_WINDOW)
}

wrap_life_span_handler! {
    pub struct LifeSpanHandlerBuilder {
        handler: LifeSpan,
    }

    impl LifeSpanHandler {
        /// `window.open` with an opener becomes a popup page Chromium
        /// creates off screen like every other (never a native window),
        /// shown by the UI as a tab marked as a popup. Everything else that
        /// wants a new window or tab becomes a plain tab (the UI opens it)
        /// and Chromium's own window is cancelled.
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: ::std::os::raw::c_int,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            target_disposition: WindowOpenDisposition,
            user_gesture: ::std::os::raw::c_int,
            _popup_features: Option<&PopupFeatures>,
            window_info: Option<&mut WindowInfo>,
            client: Option<&mut Option<Client>>,
            settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            no_javascript_access: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            use cef::sys::cef_window_open_disposition_t as D;
            let url = target_url.map(|u| u.to_string()).unwrap_or_default();
            let raw: D = target_disposition.into();
            let scriptable = no_javascript_access.as_deref().is_some_and(|v| *v == 0);
            let refused = self.handler.state.borrow().no_popups;
            // Only the app's own pages may open them in a new tab (H1.4).
            let opener = _frame.as_deref().map(|f| CefString::from(&f.url()).to_string()).unwrap_or_default();
            if crate::app_pages::is_app_page(&url) && !crate::app_pages::is_app_page(&opener) {
                eprintln!("agent-unibrow: refused a page's new tab from {opener:?} to {url}");
                return 1;
            }
            debug(|| format!("popup request {url:?} disposition {raw:?} scriptable {scriptable} gesture {user_gesture}"));
            if refused {
                return 1;
            }
            if let (true, Some(wi), Some(client), Some(settings)) = (wants_popup(raw, scriptable), window_info, client, settings) {
                // Off screen, like every page here: no native window, ever.
                wi.windowless_rendering_enabled = 1;
                wi.parent_window = 0;
                wi.shared_texture_enabled = 0;
                wi.external_begin_frame_enabled = 0;
                settings.windowless_frame_rate = crate::VISIBLE_FRAME_RATE;
                settings.background_color = 0xFFFF_FFFF;
                let (width, height, scale) = {
                    let s = self.handler.state.borrow();
                    (s.width, s.height, s.scale)
                };
                let state = Rc::new(RefCell::new(ViewState { url: url.clone(), width, height, scale, loading: true, ..Default::default() }));
                let pending: Pending = Rc::default();
                let events: Events = Rc::default();
                let adoption = Adoption { opener: self.handler.state.clone(), pending: pending.clone(), events };
                *client = Some(crate::handlers::client(state, pending, Some(adoption)));
                return 0;
            }
            if !url.is_empty() {
                let foreground = raw != D::CEF_WOD_NEW_BACKGROUND_TAB;
                self.handler.state.borrow_mut().new_tabs.push((url, foreground));
            }
            1 // cancel Chromium's own popup window
        }

        /// A popup's page exists: hand it to its opener's tab.
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            let Some(browser) = browser else { return };
            let Some(a) = self.handler.adoption.borrow_mut().take() else { return };
            let view = crate::WebView::adopt(browser.clone(), self.handler.state.clone(), a.pending, a.events);
            a.opener.borrow_mut().new_popups.push(view);
        }

        /// A page went away. If it's the live one and we didn't close it,
        /// it closed itself (`window.close()`, as sign-in popups do).
        fn on_before_close(&self, browser: Option<&mut Browser>) {
            let Some(id) = browser.map(|b| b.identifier()) else { return };
            let mut s = self.handler.state.borrow_mut();
            if id != 0 && s.browser_id == id {
                debug(|| format!("page {id} closed itself ({})", s.url));
                s.browser_id = 0;
                s.closed_by_page = true;
                s.frame_seq += 1;
            }
        }
    }
}

// ------------------------------------------------------------ js dialogs

#[derive(Clone)]
pub struct Dialogs {
    state: Rc<RefCell<ViewState>>,
}

wrap_jsdialog_handler! {
    pub struct JsdialogHandlerBuilder {
        handler: Dialogs,
    }

    impl JsdialogHandler {
        /// Every alert/confirm/prompt is answered later, by the user (a card
        /// over the page) or the agent (`browser_dialog`).
        fn on_jsdialog(
            &self,
            _browser: Option<&mut Browser>,
            _origin_url: Option<&CefString>,
            dialog_type: JsdialogType,
            message_text: Option<&CefString>,
            default_prompt_text: Option<&CefString>,
            callback: Option<&mut JsdialogCallback>,
            _suppress_message: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            use cef::sys::cef_jsdialog_type_t as T;
            let Some(cb) = callback else { return 0 };
            let raw: T = dialog_type.into();
            let kind = match raw {
                T::JSDIALOGTYPE_CONFIRM => "confirm",
                T::JSDIALOGTYPE_PROMPT => "prompt",
                _ => "alert",
            };
            let mut s = self.handler.state.borrow_mut();
            s.dialog = Some(DialogInfo {
                kind: kind.into(),
                message: message_text.map(|m| m.to_string()).unwrap_or_default(),
                default_text: default_prompt_text.map(|m| m.to_string()).unwrap_or_default(),
            });
            s.dialog_callback = Some(cb.clone());
            s.frame_seq += 1;
            1
        }

        fn on_before_unload_dialog(
            &self,
            _browser: Option<&mut Browser>,
            message_text: Option<&CefString>,
            _is_reload: ::std::os::raw::c_int,
            callback: Option<&mut JsdialogCallback>,
        ) -> ::std::os::raw::c_int {
            let Some(cb) = callback else { return 0 };
            let mut s = self.handler.state.borrow_mut();
            s.dialog = Some(DialogInfo {
                kind: "beforeunload".into(),
                message: message_text
                    .map(|m| m.to_string())
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| "Leave this page? Changes you made may not be saved.".into()),
                default_text: String::new(),
            });
            s.dialog_callback = Some(cb.clone());
            s.frame_seq += 1;
            1
        }

        fn on_reset_dialog_state(&self, _browser: Option<&mut Browser>) {
            let mut s = self.handler.state.borrow_mut();
            s.dialog = None;
            s.dialog_callback = None;
        }

        fn on_dialog_closed(&self, _browser: Option<&mut Browser>) {
            let mut s = self.handler.state.borrow_mut();
            s.dialog = None;
            s.dialog_callback = None;
        }
    }
}

// ------------------------------------------------- find, files, downloads

#[derive(Clone)]
pub struct Misc {
    state: Rc<RefCell<ViewState>>,
}

wrap_find_handler! {
    pub struct FindHandlerBuilder {
        handler: Misc,
    }

    impl FindHandler {
        fn on_find_result(
            &self,
            _browser: Option<&mut Browser>,
            _identifier: ::std::os::raw::c_int,
            count: ::std::os::raw::c_int,
            _selection_rect: Option<&Rect>,
            active_match_ordinal: ::std::os::raw::c_int,
            _final_update: ::std::os::raw::c_int,
        ) {
            let mut s = self.handler.state.borrow_mut();
            s.find = Some((count, active_match_ordinal));
            s.frame_seq += 1;
        }
    }
}

/// A file name that doesn't overwrite anything already in `dir`.
fn unique_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let clean: String = name.chars().map(|c| if c == '/' || c == '\\' || c.is_control() { '_' } else { c }).collect();
    let clean = if clean.trim().is_empty() { "download".to_string() } else { clean };
    let first = dir.join(&clean);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match clean.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (clean.clone(), String::new()),
    };
    (1..1000).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).unwrap_or(first)
}

wrap_download_handler! {
    pub struct DownloadHandlerBuilder {
        handler: Misc,
    }

    impl DownloadHandler {
        /// Allow downloads. (The binding's default returns 0, which would
        /// cancel every download once this handler exists.)
        fn can_download(
            &self,
            _browser: Option<&mut Browser>,
            _url: Option<&CefString>,
            _request_method: Option<&CefString>,
        ) -> ::std::os::raw::c_int {
            1
        }

        /// Downloads go straight to the agent-unibrow downloads folder.
        fn on_before_download(
            &self,
            _browser: Option<&mut Browser>,
            _download_item: Option<&mut DownloadItem>,
            suggested_name: Option<&CefString>,
            callback: Option<&mut BeforeDownloadCallback>,
        ) -> ::std::os::raw::c_int {
            let Some(cb) = callback else { return 0 };
            let name = suggested_name.map(|n| n.to_string()).unwrap_or_default();
            let path = unique_path(&crate::downloads_dir(), &name);
            cb.cont(Some(&CefString::from(path.to_string_lossy().as_ref())), 0);
            1
        }

        fn on_download_updated(
            &self,
            _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            _callback: Option<&mut DownloadItemCallback>,
        ) {
            let Some(item) = download_item else { return };
            let text = |u: CefStringUserfree| CefString::from(&u).to_string();
            let path = text(item.full_path());
            let info = DownloadInfo {
                id: item.id(),
                name: std::path::Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| text(item.suggested_file_name())),
                path,
                url: text(item.url()),
                percent: item.percent_complete(),
                received: item.received_bytes(),
                total: item.total_bytes(),
                complete: item.is_complete() == 1,
                canceled: item.is_canceled() == 1 || item.is_interrupted() == 1,
            };
            let mut s = self.handler.state.borrow_mut();
            match s.downloads.iter_mut().find(|d| d.id == info.id) {
                Some(d) => *d = info,
                None => s.downloads.push(info),
            }
            s.frame_seq += 1;
        }
    }
}

wrap_dialog_handler! {
    pub struct DialogHandlerBuilder {
        handler: Misc,
    }

    impl DialogHandler {
        /// The page's file chooser: the UI shows the desktop's file picker.
        fn on_file_dialog(
            &self,
            _browser: Option<&mut Browser>,
            mode: FileDialogMode,
            title: Option<&CefString>,
            _default_file_path: Option<&CefString>,
            _accept_filters: Option<&mut CefStringList>,
            _accept_extensions: Option<&mut CefStringList>,
            _accept_descriptions: Option<&mut CefStringList>,
            callback: Option<&mut FileDialogCallback>,
        ) -> ::std::os::raw::c_int {
            use cef::sys::cef_file_dialog_mode_t as M;
            let Some(cb) = callback else { return 0 };
            let raw: M = mode.into();
            let mut s = self.handler.state.borrow_mut();
            s.file_dialog = Some(FileDialogRequest {
                multiple: raw == M::FILE_DIALOG_OPEN_MULTIPLE,
                title: title.map(|t| t.to_string()).unwrap_or_default(),
            });
            s.file_dialog_callback = Some(cb.clone());
            s.frame_seq += 1;
            1
        }
    }
}

wrap_permission_handler! {
    pub struct PermissionHandlerBuilder {
        handler: Misc,
    }

    // Roadmap 4.2: camera, mic, location and notifications follow the
    // user's per-site answer, or wait for it on a card over the page
    // (see `crate::permissions`). Everything else is denied.
    impl PermissionHandler {
        fn on_request_media_access_permission(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            requesting_origin: Option<&CefString>,
            requested_permissions: u32,
            callback: Option<&mut MediaAccessCallback>,
        ) -> ::std::os::raw::c_int {
            let Some(cb) = callback else { return 0 };
            let origin = requesting_origin.map(|o| o.to_string()).unwrap_or_default();
            let pending = crate::PendingPermission::Media { callback: cb.clone(), requested: requested_permissions };
            let kinds = crate::permissions::media_kinds(requested_permissions);
            self.handler.permission_request(origin, kinds, pending, "the camera or microphone");
            1
        }

        fn on_show_permission_prompt(
            &self,
            _browser: Option<&mut Browser>,
            prompt_id: u64,
            requesting_origin: Option<&CefString>,
            requested_permissions: u32,
            callback: Option<&mut PermissionPromptCallback>,
        ) -> ::std::os::raw::c_int {
            let Some(cb) = callback else { return 0 };
            let origin = requesting_origin.map(|o| o.to_string()).unwrap_or_default();
            let pending = crate::PendingPermission::Prompt { callback: cb.clone(), id: prompt_id };
            let kinds = crate::permissions::prompt_kinds(requested_permissions);
            self.handler.permission_request(origin, kinds, pending, "a permission");
            1
        }

        fn on_dismiss_permission_prompt(
            &self,
            _browser: Option<&mut Browser>,
            prompt_id: u64,
            _result: PermissionRequestResult,
        ) {
            let mut s = self.handler.state.borrow_mut();
            if matches!(s.permission_callback, Some(crate::PendingPermission::Prompt { id, .. }) if id == prompt_id) {
                s.permission_callback = None;
                s.permission_prompt = None;
            }
        }
    }
}

impl Misc {
    /// Answers from what the user decided for the site, or holds the
    /// request for the user. `kinds` is `None` for requests never offered.
    fn permission_request(
        &self,
        origin: String,
        kinds: Option<Vec<crate::permissions::Kind>>,
        pending: crate::PendingPermission,
        what: &str,
    ) {
        let Some(kinds) = kinds else {
            pending.answer(false);
            self.state.borrow_mut().blocked.push(format!("{origin} asked for {what}"));
            return;
        };
        match crate::permissions::decision(&origin, &kinds) {
            Some(true) => pending.answer(true),
            Some(false) => {
                pending.answer(false);
                let asked = crate::permissions::describe(&kinds);
                self.state.borrow_mut().blocked.push(format!("{origin} asked for {asked} (you chose Deny)"));
            }
            None => {
                // One card at a time: an older unanswered request is turned down.
                let old = {
                    let mut s = self.state.borrow_mut();
                    s.permission_prompt = Some(crate::permissions::PermissionPrompt {
                        origin: crate::permissions::normalize_origin(&origin),
                        kinds,
                    });
                    s.frame_seq += 1;
                    s.permission_callback.replace(pending)
                };
                if let Some(old) = old {
                    old.answer(false);
                }
            }
        }
    }
}

wrap_audio_handler! {
    pub struct AudioHandlerBuilder {
        handler: Misc,
    }

    impl AudioHandler {
        /// Chromium asks this (on the UI thread) each time the page starts
        /// making sound. Returning 0 declines capture, so the sound still
        /// plays; capturing would mute it. The UI shows a speaker.
        fn audio_parameters(
            &self,
            _browser: Option<&mut Browser>,
            _params: Option<&mut AudioParameters>,
        ) -> ::std::os::raw::c_int {
            let mut s = self.handler.state.borrow_mut();
            s.audible_hint = true;
            s.frame_seq += 1;
            0
        }
    }
}

wrap_context_menu_handler! {
    pub struct ContextMenuHandlerBuilder {
        handler: Misc,
    }

    impl ContextMenuHandler {
        /// The app draws its own menu: keep the request and the callback;
        /// the UI answers it (see `WebView::close_context_menu`).
        fn run_context_menu(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            params: Option<&mut ContextMenuParams>,
            _model: Option<&mut MenuModel>,
            callback: Option<&mut RunContextMenuCallback>,
        ) -> ::std::os::raw::c_int {
            let (Some(p), Some(cb)) = (params, callback) else { return 0 };
            use cef::sys::{
                cef_context_menu_edit_state_flags_t as E, cef_context_menu_media_type_t as M,
                cef_context_menu_type_flags_t as T,
            };
            let text = |u: CefStringUserfree| CefString::from(&u).to_string();
            let types: T = p.type_flags().into();
            let edit: E = p.edit_state_flags().into();
            let media: M = p.media_type().into();
            let has = |flag: E| edit.0 & flag.0 != 0;
            let mut s = self.handler.state.borrow_mut();
            s.context_menu_seq += 1;
            let info = ContextMenuInfo {
                seq: s.context_menu_seq,
                x: p.xcoord(),
                y: p.ycoord(),
                link_url: if types.0 & T::CM_TYPEFLAG_LINK.0 != 0 { text(p.link_url()) } else { String::new() },
                image_url: if media == M::CM_MEDIATYPE_IMAGE { text(p.source_url()) } else { String::new() },
                selection: text(p.selection_text()),
                editable: p.is_editable() == 1,
                can_cut: has(E::CM_EDITFLAG_CAN_CUT),
                can_copy: has(E::CM_EDITFLAG_CAN_COPY),
                can_paste: has(E::CM_EDITFLAG_CAN_PASTE),
                can_select_all: has(E::CM_EDITFLAG_CAN_SELECT_ALL),
                page_url: text(p.page_url()),
                misspelled_word: text(p.misspelled_word()),
                suggestions: {
                    let mut list = CefStringList::new();
                    p.dictionary_suggestions(Some(&mut list));
                    list.into_iter().collect()
                },
            };
            s.context_menu = Some(info);
            s.context_menu_callback = Some(cb.clone());
            s.context_menu_frame = frame.map(|f| f.clone());
            s.frame_seq += 1;
            1
        }

        fn on_context_menu_dismissed(&self, _browser: Option<&mut Browser>, _frame: Option<&mut Frame>) {
            let mut s = self.handler.state.borrow_mut();
            s.context_menu = None;
            s.context_menu_callback = None;
            s.frame_seq += 1;
        }
    }
}

#[derive(Clone)]
pub struct PdfDone {
    done: Rc<RefCell<Option<futures::channel::oneshot::Sender<bool>>>>,
}

wrap_pdf_print_callback! {
    pub struct PdfCallbackBuilder {
        handler: PdfDone,
    }

    impl PdfPrintCallback {
        fn on_pdf_print_finished(&self, _path: Option<&CefString>, ok: ::std::os::raw::c_int) {
            if let Some(tx) = self.handler.done.borrow_mut().take() {
                let _ = tx.send(ok == 1);
            }
        }
    }
}

pub fn pdf_callback(done: futures::channel::oneshot::Sender<bool>) -> cef::PdfPrintCallback {
    PdfCallbackBuilder::new(PdfDone { done: Rc::new(RefCell::new(Some(done))) })
}

// ---------------------------------------------------------------- client

wrap_client! {
    pub struct ClientBuilder {
        render: RenderHandler,
        display: DisplayHandler,
        load: LoadHandler,
        life_span: LifeSpanHandler,
        dialogs: JsdialogHandler,
        find: FindHandler,
        downloads: DownloadHandler,
        files: DialogHandler,
        permissions: PermissionHandler,
        audio: AudioHandler,
        context_menu: ContextMenuHandler,
        requests: RequestHandler,
    }

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(self.render.clone())
        }
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.display.clone())
        }
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load.clone())
        }
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span.clone())
        }
        fn jsdialog_handler(&self) -> Option<JsdialogHandler> {
            Some(self.dialogs.clone())
        }
        fn find_handler(&self) -> Option<FindHandler> {
            Some(self.find.clone())
        }
        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(self.downloads.clone())
        }
        fn dialog_handler(&self) -> Option<DialogHandler> {
            Some(self.files.clone())
        }
        fn permission_handler(&self) -> Option<PermissionHandler> {
            Some(self.permissions.clone())
        }
        fn audio_handler(&self) -> Option<AudioHandler> {
            Some(self.audio.clone())
        }
        fn context_menu_handler(&self) -> Option<ContextMenuHandler> {
            Some(self.context_menu.clone())
        }
        fn request_handler(&self) -> Option<RequestHandler> {
            Some(self.requests.clone())
        }
    }
}

pub(crate) fn client(state: Rc<RefCell<ViewState>>, pending: Pending, adoption: Option<Adoption>) -> cef::Client {
    ClientBuilder::new(
        RenderHandlerBuilder::new(Render { state: state.clone() }),
        DisplayHandlerBuilder::new(Display { state: state.clone() }),
        LoadHandlerBuilder::new(Load { state: state.clone() }),
        LifeSpanHandlerBuilder::new(LifeSpan { state: state.clone(), adoption: Rc::new(RefCell::new(adoption)) }),
        JsdialogHandlerBuilder::new(Dialogs { state: state.clone() }),
        FindHandlerBuilder::new(Misc { state: state.clone() }),
        DownloadHandlerBuilder::new(Misc { state: state.clone() }),
        DialogHandlerBuilder::new(Misc { state: state.clone() }),
        PermissionHandlerBuilder::new(Misc { state: state.clone() }),
        AudioHandlerBuilder::new(Misc { state: state.clone() }),
        ContextMenuHandlerBuilder::new(Misc { state: state.clone() }),
        RequestHandlerBuilder::new(Requests { state, pending }),
    )
}

// --------------------------------------------------------------- history

#[derive(Clone)]
pub struct HistoryCollector {
    out: Rc<RefCell<crate::TabHistory>>,
}

wrap_navigation_entry_visitor! {
    pub struct HistoryVisitorBuilder {
        collector: HistoryCollector,
    }

    impl NavigationEntryVisitor {
        fn visit(
            &self,
            entry: Option<&mut NavigationEntry>,
            current: ::std::os::raw::c_int,
            index: ::std::os::raw::c_int,
            _total: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let text = |u: CefStringUserfree| CefString::from(&u).to_string();
            if let Some(e) = entry.filter(|e| e.is_valid() == 1) {
                let mut out = self.collector.out.borrow_mut();
                if current == 1 {
                    out.current = index.max(0) as usize;
                }
                out.entries.push(crate::HistoryEntry { url: text(e.url()), title: text(e.title()) });
            }
            1 // keep going
        }
    }
}

/// Collects a tab's navigation entries into `out`.
pub(crate) fn history_visitor(out: Rc<RefCell<crate::TabHistory>>) -> cef::NavigationEntryVisitor {
    HistoryVisitorBuilder::new(HistoryCollector { out })
}

// -------------------------------------------------------------- devtools

/// Replies waiting for DevTools method results, by message id.
pub type Pending = Rc<RefCell<HashMap<i32, crate::bridge::Reply<serde_json::Value>>>>;

/// DevTools events kept for tools to collect (child-frame sessions, drags).
pub type Events = Rc<RefCell<std::collections::VecDeque<serde_json::Value>>>;

/// Events worth keeping; the rest are dropped.
const KEPT_EVENTS: &[&str] = &["Target.attachedToTarget", "Target.detachedFromTarget", "Input.dragIntercepted"];
const MAX_EVENTS: usize = 200;

#[derive(Clone)]
pub struct Cdp {
    pending: Pending,
    events: Events,
    state: Rc<RefCell<ViewState>>,
}

/// Credentials the user typed for a site's HTTP sign-in, handed to
/// Chromium through the DevTools `Fetch` domain (the only way in for a
/// main-frame sign-in here). Never shown to the agent.
pub(crate) struct AuthFetch {
    pub host: String,
    pub username: String,
    pub password: String,
    /// Already answered once: a second challenge means they were wrong.
    pub answered: bool,
    /// The reload with the credentials has started loading.
    pub saw_loading: bool,
    pub next_id: i32,
}

/// Ids for messages the observer sends itself; replies to them are ignored.
pub(crate) const AUTH_MESSAGE_IDS: i32 = 1 << 30;

fn send_cdp(browser: Option<&mut Browser>, id: i32, method: &str, params: serde_json::Value) {
    let msg = serde_json::json!({"id": id, "method": method, "params": params}).to_string();
    if let Some(host) = browser.and_then(|b| b.host()) {
        host.send_dev_tools_message(Some(msg.as_bytes()));
    }
}

wrap_dev_tools_message_observer! {
    pub struct CdpObserverBuilder {
        handler: Cdp,
    }

    impl DevToolsMessageObserver {
        // Raw messages are parsed here rather than by CEF, so replies from
        // child sessions (out-of-process iframes, which carry a `sessionId`)
        // come back intact, and the events tools need are kept.
        fn on_dev_tools_message(&self, _browser: Option<&mut Browser>, message: Option<&[u8]>) -> ::std::os::raw::c_int {
            let Some(msg) = message.and_then(|m| serde_json::from_slice::<serde_json::Value>(m).ok()) else { return 0 };
            if let Some(id) = msg["id"].as_i64() {
                let Some(reply) = self.handler.pending.borrow_mut().remove(&(id as i32)) else { return 1 };
                let outcome = match msg.get("error") {
                    Some(e) => Err(e["message"].as_str().map(str::to_string).unwrap_or_else(|| e.to_string())),
                    None => Ok(msg.get("result").cloned().unwrap_or(serde_json::Value::Null)),
                };
                let _ = reply.send(outcome);
                return 1;
            }
            if let Some(method) = msg["method"].as_str() {
                // The sign-in interception's events go on to
                // `on_dev_tools_event`, which CEF only calls for messages
                // this method leaves unhandled.
                if method.starts_with("Fetch.") {
                    return 0;
                }
                if KEPT_EVENTS.contains(&method) {
                    let mut ev = self.handler.events.borrow_mut();
                    if ev.len() >= MAX_EVENTS {
                        ev.pop_front();
                    }
                    ev.push_back(msg);
                }
                return 1;
            }
            0
        }

        fn on_dev_tools_method_result(
            &self,
            _browser: Option<&mut Browser>,
            message_id: ::std::os::raw::c_int,
            success: ::std::os::raw::c_int,
            result: Option<&[u8]>,
        ) {
            let Some(reply) = self.handler.pending.borrow_mut().remove(&message_id) else { return };
            let parsed: serde_json::Value =
                result.and_then(|r| serde_json::from_slice(r).ok()).unwrap_or(serde_json::Value::Null);
            let outcome = if success == 1 {
                Ok(parsed)
            } else {
                Err(parsed["message"].as_str().map(str::to_string).unwrap_or_else(|| parsed.to_string()))
            };
            let _ = reply.send(outcome);
        }

        /// While the user's sign-in is being delivered: answer the site's
        /// challenge with it, once.
        fn on_dev_tools_event(
            &self,
            browser: Option<&mut Browser>,
            method: Option<&CefString>,
            params: Option<&[u8]>,
        ) {
            let method = method.map(|m| m.to_string()).unwrap_or_default();
            if method != "Fetch.authRequired" && method != "Fetch.requestPaused" {
                return;
            }
            let params: serde_json::Value =
                params.and_then(|p| serde_json::from_slice(p).ok()).unwrap_or(serde_json::Value::Null);
            let request_id = params["requestId"].as_str().unwrap_or_default().to_string();
            let mut s = self.handler.state.borrow_mut();
            let Some(fetch) = s.auth_fetch.as_mut() else {
                drop(s);
                // Not ours to hold up.
                let (m, p) = if method == "Fetch.requestPaused" {
                    ("Fetch.continueRequest", serde_json::json!({"requestId": request_id}))
                } else {
                    ("Fetch.continueWithAuth", serde_json::json!({"requestId": request_id, "authChallengeResponse": {"response": "Default"}}))
                };
                send_cdp(browser, AUTH_MESSAGE_IDS, m, p);
                return;
            };
            fetch.next_id = fetch.next_id.wrapping_add(1).max(AUTH_MESSAGE_IDS);
            let id = fetch.next_id;
            if method == "Fetch.requestPaused" {
                drop(s);
                send_cdp(browser, id, "Fetch.continueRequest", serde_json::json!({"requestId": request_id}));
                return;
            }
            let origin = params["authChallenge"]["origin"].as_str().unwrap_or_default();
            let host = url::Url::parse(origin).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
            let response = if host == fetch.host && !fetch.answered && params["authChallenge"]["source"] != "Proxy" {
                fetch.answered = true;
                serde_json::json!({"response": "ProvideCredentials", "username": fetch.username, "password": fetch.password})
            } else {
                // Wrong password, or another site: Chromium's own handling,
                // which comes back to the app as a new sign-in request.
                serde_json::json!({"response": "Default"})
            };
            drop(s);
            send_cdp(browser, id, "Fetch.continueWithAuth", serde_json::json!({"requestId": request_id, "authChallengeResponse": response}));
        }
    }
}

pub fn cdp_observer(pending: Pending, events: Events, state: Rc<RefCell<ViewState>>) -> cef::DevToolsMessageObserver {
    CdpObserverBuilder::new(Cdp { pending, events, state })
}

#[cfg(test)]
mod tests {
    #[test]
    fn popup_path_is_for_scriptable_new_windows() {
        use cef::sys::cef_window_open_disposition_t as D;
        assert!(super::wants_popup(D::CEF_WOD_NEW_POPUP, true));
        assert!(super::wants_popup(D::CEF_WOD_NEW_FOREGROUND_TAB, true));
        assert!(!super::wants_popup(D::CEF_WOD_NEW_POPUP, false));
        assert!(!super::wants_popup(D::CEF_WOD_NEW_BACKGROUND_TAB, true));
    }

    use super::{cert_error_text, is_cert_error, termination_text};
    use cef::sys::{cef_errorcode_t as E, cef_termination_status_t as T};

    #[test]
    fn certificate_errors_are_named() {
        assert!(is_cert_error(E::ERR_CERT_AUTHORITY_INVALID));
        assert!(is_cert_error(E::ERR_CERT_DATE_INVALID));
        assert!(!is_cert_error(E::ERR_NAME_NOT_RESOLVED));
        let t = cert_error_text(E::ERR_CERT_AUTHORITY_INVALID, "https://self-signed.example:8443/x");
        assert!(t.starts_with("certificate error: "), "{t}");
        assert!(t.contains("ERR_CERT_AUTHORITY_INVALID") && t.contains("self-signed.example"), "{t}");
    }

    #[test]
    fn terminations_are_worded() {
        assert_eq!(termination_text(T::TS_PROCESS_OOM.into(), 0), "it ran out of memory");
        assert_eq!(termination_text(T::TS_PROCESS_WAS_KILLED.into(), 9), "its process was killed, code 9");
    }
}
