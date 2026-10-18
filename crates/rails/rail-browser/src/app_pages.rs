//! The app's own pages, served at `unibrow://` (homepage roadmap H1.1).
//!
//! The scheme is registered as standard, secure and display-isolated, and
//! not CORS-enabled: pages from other schemes can't link to it, frame it,
//! open it or fetch from it. What it serves comes from an [`AppPages`]
//! the shell provides; this module only does the CEF plumbing and the
//! rules every response follows (a strict content-security policy, no
//! paths outside the page's own files).

use cef::rc::Rc as _;
use cef::*;
use std::sync::{Arc, Mutex};

pub const SCHEME: &str = "unibrow";
/// The home page.
pub const HOME_URL: &str = "unibrow://home/";

/// Nothing from the web runs on the app's pages, and nothing frames them.
const CSP: &str = "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; \
                   form-action https://duckduckgo.com; frame-ancestors 'none'; base-uri 'none'";

/// Whether `url` is one of the app's own pages.
pub fn is_app_page(url: &str) -> bool {
    url.get(..SCHEME.len() + 1).is_some_and(|s| s.eq_ignore_ascii_case("unibrow:"))
}

/// Whether a frame on `from` may navigate to an app page, given the
/// navigation's CEF transition type. The app's own loads (address bar, new
/// tab, the shell: explicit), reloads and Back/Forward may; so may the
/// app's pages themselves. Links, scripts, forms and redirects from web
/// pages may not, and no subframe may.
pub fn may_open(from: &str, main_frame: bool, transition: u32) -> bool {
    const SOURCE_MASK: u32 = 0xFF;
    const EXPLICIT: u32 = 1;
    const RELOAD: u32 = 8;
    const FORWARD_BACK: u32 = 0x0100_0000;
    if !main_frame {
        return false;
    }
    is_app_page(from) || matches!(transition & SOURCE_MASK, EXPLICIT | RELOAD) || transition & FORWARD_BACK != 0
}

/// A response from an app page.
#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    pub status: i32,
    pub mime: &'static str,
    pub body: Vec<u8>,
}

impl Page {
    pub fn ok(mime: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self { status: 200, mime, body: body.into() }
    }

    pub fn not_found() -> Self {
        Self { status: 404, mime: "text/plain", body: b"Not found".to_vec() }
    }

    pub fn forbidden() -> Self {
        Self { status: 403, mime: "text/plain", body: b"Only the app's own pages can ask for this".to_vec() }
    }

    pub fn json(v: &serde_json::Value) -> Self {
        Self::ok("application/json", v.to_string())
    }

    pub fn bad_request(why: &str) -> Self {
        Self { status: 400, mime: "application/json", body: serde_json::json!({"error": why}).to_string().into_bytes() }
    }
}

/// A request for an app page.
#[derive(Clone, Debug, PartialEq)]
pub struct AppRequest {
    /// The page: "home".
    pub host: String,
    /// The file or API path within it: "/", "/home.css", "/api/state".
    pub path: String,
    /// Whether it comes from a main frame that's on an app page itself
    /// (the page fetching its data), not a navigation from elsewhere.
    /// Data and commands are only served to these.
    pub from_app_page: bool,
    /// "GET" or "POST".
    pub method: String,
    /// The `X-Unibrow-Token` header: the per-launch token the app's page
    /// sends with commands.
    pub token: String,
    /// A POST's body, up to [`MAX_BODY`].
    pub body: Vec<u8>,
}

/// The largest request body read.
pub const MAX_BODY: usize = 64 * 1024;

impl Default for AppRequest {
    fn default() -> Self {
        Self { host: String::new(), path: "/".into(), from_app_page: false, method: "GET".into(), token: String::new(), body: Vec::new() }
    }
}

/// What the app's pages serve. Called on CEF's IO thread.
pub trait AppPages: Send + Sync {
    fn get(&self, req: &AppRequest) -> Page;
}

/// Splits `unibrow://home/home.css?x` into ("home", "/home.css"). `None`
/// for other schemes and for paths that try to leave the page's files.
pub fn route(url: &str) -> Option<(String, String)> {
    if !is_app_page(url) {
        return None;
    }
    let rest = url[SCHEME.len() + 1..].trim_start_matches('/');
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let lower = path.to_ascii_lowercase();
    if host.is_empty() || lower.contains("..") || lower.contains("%2e") || lower.contains('\\') || lower.contains("%5c") {
        return None;
    }
    Some((host.to_ascii_lowercase(), path.to_string()))
}

/// The response for `url`, asked for by a frame on `from` (main frame or
/// not): what `pages` serves, or a 404. `req` carries the method, token
/// and body.
pub fn respond(pages: &dyn AppPages, url: &str, from: &str, main_frame: bool, req: AppRequest) -> Page {
    match route(url) {
        Some((host, path)) => pages.get(&AppRequest { host, path, from_app_page: main_frame && is_app_page(from), ..req }),
        None => Page::not_found(),
    }
}

/// A request's method, token header and body (bytes only, capped).
fn request_parts(r: &Request) -> AppRequest {
    let method = CefString::from(&r.method()).to_string().to_ascii_uppercase();
    let token = CefString::from(&r.header_by_name(Some(&"X-Unibrow-Token".into()))).to_string();
    let mut body = Vec::new();
    if let Some(post) = r.post_data() {
        let mut elements: Vec<Option<PostDataElement>> = vec![None; post.element_count()];
        post.elements(Some(&mut elements));
        for e in elements.into_iter().flatten() {
            let n = e.bytes_count().min(MAX_BODY.saturating_sub(body.len()));
            if n == 0 {
                continue;
            }
            let mut buf = vec![0u8; n];
            let got = e.bytes(n, buf.as_mut_ptr());
            buf.truncate(got);
            body.extend(buf);
        }
    }
    AppRequest { method, token, body, ..Default::default() }
}

/// Adds the scheme. Runs in every process (browser and renderers), from
/// `App::on_register_custom_schemes`.
pub(crate) fn register_scheme(registrar: &mut SchemeRegistrar) {
    let options = SchemeOptions::STANDARD.get_raw()
        | SchemeOptions::SECURE.get_raw()
        | SchemeOptions::DISPLAY_ISOLATED.get_raw()
        | SchemeOptions::FETCH_ENABLED.get_raw();
    registrar.add_custom_scheme(Some(&SCHEME.into()), options as i32);
}

/// Serves the app's pages from `pages`. Call once, after [`crate::initialize`].
pub fn serve(pages: Arc<dyn AppPages>) -> Result<(), String> {
    let mut factory = Factory::new(pages);
    match cef::register_scheme_handler_factory(Some(&SCHEME.into()), None, Some(&mut factory)) {
        1 => Ok(()),
        _ => Err("couldn't register the unibrow:// pages".into()),
    }
}

#[derive(Default)]
struct Body {
    page: Option<Page>,
    sent: usize,
}

wrap_scheme_handler_factory! {
    struct Factory {
        pages: Arc<dyn AppPages>,
    }

    impl SchemeHandlerFactory {
        fn create(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _scheme_name: Option<&CefString>,
            request: Option<&mut Request>,
        ) -> Option<ResourceHandler> {
            let (url, parts) = request
                .map(|r| (CefString::from(&r.url()).to_string(), request_parts(r)))
                .unwrap_or_default();
            let (from, main) = frame.map(|f| (CefString::from(&f.url()).to_string(), f.is_main() == 1)).unwrap_or_default();
            let page = respond(self.pages.as_ref(), &url, &from, main, parts);
            Some(Handler::new(Arc::new(Mutex::new(Body { page: Some(page), sent: 0 }))))
        }
    }
}

wrap_resource_handler! {
    struct Handler {
        body: Arc<Mutex<Body>>,
    }

    impl ResourceHandler {
        fn open(
            &self,
            _request: Option<&mut Request>,
            handle_request: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut Callback>,
        ) -> ::std::os::raw::c_int {
            // The response is ready already: handle it right away.
            if let Some(h) = handle_request {
                *h = 1;
            }
            1
        }

        fn response_headers(
            &self,
            response: Option<&mut Response>,
            response_length: Option<&mut i64>,
            _redirect_url: Option<&mut CefString>,
        ) {
            let body = self.body.lock().unwrap();
            let Some(page) = body.page.as_ref() else { return };
            if let Some(r) = response {
                r.set_status(page.status);
                let text = match page.status {
                    200 => "OK",
                    202 => "Accepted",
                    400 => "Bad Request",
                    403 => "Forbidden",
                    _ => "Not Found",
                };
                r.set_status_text(Some(&text.into()));
                r.set_mime_type(Some(&page.mime.into()));
                if page.mime.starts_with("text/") || page.mime.ends_with("javascript") || page.mime.ends_with("json") {
                    r.set_charset(Some(&"utf-8".into()));
                }
                r.set_header_by_name(Some(&"Content-Security-Policy".into()), Some(&CSP.into()), 1);
                r.set_header_by_name(Some(&"X-Content-Type-Options".into()), Some(&"nosniff".into()), 1);
                r.set_header_by_name(Some(&"Cache-Control".into()), Some(&"no-store".into()), 1);
            }
            if let Some(len) = response_length {
                *len = page.body.len() as i64;
            }
        }

        fn read(
            &self,
            data_out: *mut u8,
            bytes_to_read: ::std::os::raw::c_int,
            bytes_read: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut ResourceReadCallback>,
        ) -> ::std::os::raw::c_int {
            let mut body = self.body.lock().unwrap();
            let sent = body.sent;
            let Some(page) = body.page.as_ref() else { return 0 };
            let n = page.body.len().saturating_sub(sent).min(bytes_to_read.max(0) as usize);
            if n == 0 || data_out.is_null() {
                if let Some(b) = bytes_read {
                    *b = 0;
                }
                return 0;
            }
            // SAFETY: CEF hands a buffer of `bytes_to_read` bytes, and `n` is at most that.
            unsafe { std::ptr::copy_nonoverlapping(page.body[sent..].as_ptr(), data_out, n) };
            body.sent += n;
            if let Some(b) = bytes_read {
                *b = n as i32;
            }
            1
        }

        fn cancel(&self) {
            self.body.lock().unwrap().page = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed;
    impl AppPages for Fixed {
        fn get(&self, req: &AppRequest) -> Page {
            match (req.host.as_str(), req.path.as_str()) {
                ("home", "/") => Page::ok("text/html", "<h1>Home</h1>"),
                ("home", "/api/state") if req.from_app_page => Page::ok("application/json", "{}"),
                ("home", "/api/state") => Page::forbidden(),
                _ => Page::not_found(),
            }
        }
    }

    #[test]
    fn routes_pages_and_refuses_traversal() {
        assert_eq!(route("unibrow://home/").unwrap(), ("home".into(), "/".into()));
        assert_eq!(route("unibrow://home").unwrap(), ("home".into(), "/".into()));
        assert_eq!(route("UNIBROW://Home/home.css?v=1#x").unwrap(), ("home".into(), "/home.css".into()));
        assert!(route("unibrow://home/../etc/passwd").is_none());
        assert!(route("unibrow://home/%2e%2e/x").is_none());
        assert!(route("unibrow://home/a\\b").is_none());
        assert!(route("https://home/").is_none());
        assert!(route("unibrow:///").is_none());
    }

    #[test]
    fn serves_from_the_shells_pages() {
        let from_web = |url| respond(&Fixed, url, "https://example.com/", true, AppRequest::default());
        assert_eq!(from_web("unibrow://home/").status, 200);
        assert_eq!(from_web("unibrow://home/nope.js").status, 404);
        assert_eq!(from_web("unibrow://home/../x").status, 404);
        // Data only for the app's own page, and only its main frame.
        assert_eq!(from_web("unibrow://home/api/state").status, 403);
        assert_eq!(respond(&Fixed, "unibrow://home/api/state", "unibrow://home/", true, AppRequest::default()).status, 200);
        assert_eq!(respond(&Fixed, "unibrow://home/api/state", "unibrow://home/", false, AppRequest::default()).status, 403);
        assert!(is_app_page("unibrow://home/") && is_app_page("UNIBROW://x") && !is_app_page("https://unibrow.com"));
    }

    #[test]
    fn only_the_app_navigates_to_its_pages() {
        let (link, explicit, form, back_link) = (0, 1, 7, 0x0100_0000);
        assert!(may_open("https://example.com/", true, explicit));
        assert!(may_open("https://example.com/", true, back_link));
        assert!(may_open("unibrow://home/", true, link));
        assert!(!may_open("https://example.com/", true, link));
        assert!(!may_open("https://example.com/", true, form));
        assert!(!may_open("about:blank", true, link));
        assert!(!may_open("https://example.com/", false, explicit));
    }

    #[test]
    fn policy_keeps_the_web_out() {
        assert!(CSP.contains("default-src 'self'"));
        assert!(CSP.contains("frame-ancestors 'none'"));
        assert!(!CSP.contains("http:") && !CSP.contains("'unsafe-eval'"));
    }
}
