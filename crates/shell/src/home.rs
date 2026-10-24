//! The home page at `unibrow://home/` (homepage roadmap H1–H2): what opens
//! on launch and in new tabs. Its files are built into the app; the
//! `unibrow://` plumbing and its rules are in `rail_browser::app_pages`.
//!
//! The page reads `/api/state` (H2): recent sites, bookmarks, focus sites
//! and their Jev status, watches, playbooks, recent chats, and the agent's
//! status. History and the rest come straight from the stores; what only
//! the shell knows (the agent, keys, Jev progress) is published here by
//! the shell with [`publish`]. Nothing secret is ever in it: keys are
//! "set" or "not set".
//!
//! The page acts through `POST /api/cmd` (H3): a short list of [`Cmd`]s,
//! each checked here and carried out by the shell. The request must come
//! from the home page itself and carry this launch's token, which only the
//! served page has. Nothing on the list loosens safety settings, touches
//! saved logins or keys, or clears history; the page links to those
//! controls instead.
//!
//! The page is the user's to change (H5): which sections show and in what
//! order, compact or roomy, the accent color (`home.json` in the config
//! folder), pinned sites, and their own `user.css` or whole `index.html`
//! in the config folder's `home/` (`unibrow://home/default` always shows
//! the built-in page).

use browsing::Browsing;
use browsing::jev_sites::JevSite;
use rail_browser::app_pages::{AppPages, AppRequest, Page};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

const INDEX: &str = include_str!("../home/index.html");
const CSS: &str = include_str!("../home/home.css");
const JS: &str = include_str!("../home/home.js");
const ICON: &str = include_str!("../home/favicon.svg");
const SETTINGS: &str = include_str!("../home/settings.html");
const SETTINGS_JS: &str = include_str!("../home/settings.js");
/// Eval results, in a source checkout (homepage H4.3).
const EVAL_RESULTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../evals/results");

/// What the shell knows that the stores don't.
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub agent: String,
    pub model: String,
    pub busy: bool,
    pub keys: Keys,
    /// Jev learning progress by site.
    pub jev_progress: HashMap<String, String>,
    /// The agents and their models, for the picker: (backend id, label,
    /// [(model id, label)]), and the one in use.
    pub choices: Vec<(String, String, Vec<(String, String)>)>,
    pub backend: String,
    pub model_id: String,
    /// The latest trace entries, newest last: (kind, summary, when).
    pub trace: Vec<(String, String, i64)>,
    /// The settings as saved (no keys are in them).
    pub settings: Value,
    /// The chat showing in the sidebar.
    pub chat_id: i64,
}

/// What the home page can ask the app to do. Anything else is refused.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    /// Open a web address in a new tab.
    NewTab { url: String },
    /// Show a sidebar tab: chat, history, bookmarks, trace, focus,
    /// explained (on one of its pages), or a focus site's view.
    Show { tab: String, site: Option<String>, page: Option<usize> },
    /// Open one of the app's folders in the file manager.
    OpenFolder { which: String },
    /// Show a saved chat in the sidebar.
    OpenChat { id: i64 },
    /// Put text in the chat box without sending it.
    FillChat { text: String },
    /// Send a chat message, as if typed.
    Ask { text: String },
    SetAgent { backend: String, model: String },
    LearnJev { site: String },
    ToggleJev { site: String },
    CheckWatch { id: i64 },
    RemoveWatch { id: i64 },
    RemoveBookmark { url: String },
    SaveLayout { layout: Layout },
    Pin { url: String, title: String },
    Unpin { url: String },
    MovePin { url: String, by: i64 },
    /// Not from the page: the agent changed the page (`home_customize`).
    Customized(String),
    /// Not a command: a refused one, so the shell can note it in the trace.
    Refused(String),
}

const MAX_TEXT: usize = 4000;
const TABS: &[&str] = &["chat", "history", "bookmarks", "trace", "focus", "explained", "site"];
const BACKENDS: &[&str] = &["claude_code", "codex", "open_code"];

/// Reads a command from the page, checking every field.
pub fn parse_cmd(v: &Value) -> Result<Cmd, String> {
    let text = |k: &str| -> Result<String, String> {
        let t = v[k].as_str().ok_or_else(|| format!("missing {k}"))?.trim().to_string();
        if t.is_empty() || t.chars().count() > MAX_TEXT {
            return Err(format!("{k} must be 1 to {MAX_TEXT} characters"));
        }
        Ok(t)
    };
    let url = |k: &str| -> Result<String, String> {
        let u = text(k)?;
        web_url(&u).map(str::to_string).ok_or_else(|| format!("{k} must be a web address"))
    };
    let site = |k: &str| -> Result<String, String> {
        let s = text(k)?.to_ascii_lowercase();
        let ok = s.len() <= 253 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'));
        if ok { Ok(s) } else { Err(format!("{k} must be a site name")) }
    };
    let id = |k: &str| v[k].as_i64().ok_or_else(|| format!("missing {k}"));
    Ok(match v["cmd"].as_str().unwrap_or("") {
        "new_tab" => Cmd::NewTab { url: url("url")? },
        "show" => {
            let tab = text("tab")?;
            if !TABS.contains(&tab.as_str()) {
                return Err(format!("unknown tab {tab}"));
            }
            let site = if tab == "site" { Some(site("site")?) } else { None };
            let page = match (tab.as_str(), v["page"].as_u64()) {
                ("explained", Some(n)) if (n as usize) < crate::explained::PAGES => Some(n as usize),
                ("explained", Some(n)) => return Err(format!("no guide page {n}")),
                _ => None,
            };
            Cmd::Show { tab, site, page }
        }
        "fill_chat" => Cmd::FillChat { text: text("text")? },
        "open_chat" => Cmd::OpenChat { id: id("id")? },
        "ask" => Cmd::Ask { text: text("text")? },
        "set_agent" => {
            let backend = text("backend")?;
            if !BACKENDS.contains(&backend.as_str()) {
                return Err(format!("unknown agent {backend}"));
            }
            Cmd::SetAgent { backend, model: v["model"].as_str().unwrap_or("").chars().take(200).collect() }
        }
        "learn_jev" => Cmd::LearnJev { site: site("site")? },
        "toggle_jev" => Cmd::ToggleJev { site: site("site")? },
        "check_watch" => Cmd::CheckWatch { id: id("id")? },
        "remove_watch" => Cmd::RemoveWatch { id: id("id")? },
        "remove_bookmark" => Cmd::RemoveBookmark { url: url("url")? },
        "save_layout" => Cmd::SaveLayout { layout: Layout::from_value(&v["layout"]) },
        "reset_layout" => Cmd::SaveLayout { layout: Layout::default() },
        "pin" => Cmd::Pin { url: url("url")?, title: v["title"].as_str().unwrap_or("").chars().take(200).collect() },
        "unpin" => Cmd::Unpin { url: url("url")? },
        "move_pin" => Cmd::MovePin { url: url("url")?, by: id("by")?.clamp(-1, 1) },
        "open_folder" => {
            let which = text("which")?;
            if folder(&which).is_none() {
                return Err(format!("no folder {which}"));
            }
            Cmd::OpenFolder { which }
        }
        other => return Err(format!("“{}” isn't something the home page can do", other.chars().take(40).collect::<String>())),
    })
}

/// The page's sections, in their default order.
pub const SECTIONS: &[&str] = &[
    "pins", "recent", "focus", "bookmarks", "watches", "playbooks", "chats", "ask", "tinker", "guide", "activity", "keys",
    "status",
];

/// How the user arranged the page (`home.json`). Always valid: anything
/// unknown is dropped when it's read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layout {
    pub hidden: Vec<String>,
    /// Sections in this order first, then the rest in the default order.
    pub order: Vec<String>,
    /// `compact` or `roomy` (the default).
    pub compact: bool,
    /// `#rrggbb`.
    pub accent: Option<String>,
    /// `dark` (the app's own colors, the default), `light`, or `auto`
    /// (follow the desktop's light or dark setting).
    pub theme: String,
    /// A picture in the user's home folder, shown behind the page.
    pub background: Option<String>,
}

/// The themes a layout can ask for.
pub const THEMES: &[&str] = &["dark", "light", "auto"];

/// Whether `name` could be a picture in the user's folder: a plain file
/// name with an image extension.
fn image_name(name: &str) -> bool {
    let plain = !name.is_empty()
        && !name.starts_with('.')
        && name.len() <= 100
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    plain && matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp")
}

/// The pictures in the user's folder, for the background choice.
pub fn user_images() -> Vec<String> {
    let Some(dir) = user_folder() else { return Vec::new() };
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| image_name(n))
        .collect();
    names.sort();
    names.truncate(50);
    names
}

impl Layout {
    pub fn from_value(v: &Value) -> Self {
        let ids = |k: &str| -> Vec<String> {
            let mut out: Vec<String> = Vec::new();
            for id in v[k].as_array().into_iter().flatten().filter_map(Value::as_str) {
                if SECTIONS.contains(&id) && !out.iter().any(|o| o == id) {
                    out.push(id.to_string());
                }
            }
            out
        };
        let accent = v["accent"].as_str().map(str::to_ascii_lowercase).filter(|a| {
            a.len() == 7 && a.starts_with('#') && a[1..].chars().all(|c| c.is_ascii_hexdigit())
        });
        let theme = v["theme"].as_str().filter(|t| THEMES.contains(t)).unwrap_or("dark").to_string();
        let background = v["background"].as_str().filter(|b| image_name(b)).map(str::to_string);
        Layout { hidden: ids("hidden"), order: ids("order"), compact: v["density"] == "compact", accent, theme, background }
    }

    pub fn to_value(&self) -> Value {
        json!({
            "hidden": self.hidden, "order": self.order,
            "density": if self.compact { "compact" } else { "roomy" }, "accent": self.accent,
            "theme": if self.theme.is_empty() { "dark" } else { &self.theme }, "background": self.background,
        })
    }

    fn path() -> Option<std::path::PathBuf> {
        folder("config").map(|d| d.join("home.json"))
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .map(|v| Self::from_value(&v))
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let p = Self::path().ok_or("no config folder")?;
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let text = serde_json::to_string_pretty(&self.to_value()).map_err(|e| e.to_string())?;
        std::fs::write(p, text).map_err(|e| e.to_string())
    }
}

/// The user's own files for the page (H5.4): `home/` in the config folder.
pub fn user_folder() -> Option<std::path::PathBuf> {
    folder("config").map(|d| d.join("home"))
}

/// The largest user file served.
const MAX_USER_FILE: u64 = 4 * 1024 * 1024;

/// A file from the user's folder, by plain name only ("user.css",
/// "bg.jpg"): no subfolders, no links out of the folder, size-capped.
fn user_file(name: &str) -> Option<(Vec<u8>, &'static str)> {
    if name.is_empty() || name.starts_with('.') || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')) {
        return None;
    }
    let mime = match name.rsplit('.').next()?.to_ascii_lowercase().as_str() {
        "css" => "text/css",
        "js" => "text/javascript",
        "html" => "text/html",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "woff2" => "font/woff2",
        _ => return None,
    };
    let dir = user_folder()?.canonicalize().ok()?;
    let path = dir.join(name);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAX_USER_FILE || path.canonicalize().ok()?.parent()? != dir {
        return None;
    }
    Some((std::fs::read(path).ok()?, mime))
}

/// The light palette (the app itself is dark only).
const LIGHT: [(&str, u32); 9] = [
    ("bg", 0xf4f6f8),
    ("pane", 0xffffff),
    ("raised", 0xe9eef2),
    ("rule", 0xd3dbe2),
    ("fg", 0x1b2733),
    ("muted", 0x5a6a77),
    ("ok", 0x2e7d32),
    ("warn", 0xa15c00),
    ("error", 0xc0392b),
];
/// The accent on light pages: the app's teal, darker so it reads on white.
const LIGHT_ACCENT: u32 = 0x1f8a82;

/// `/theme.css`: the shell's palette (`theme.rs`), so the page matches the
/// app, or the light one; with the user's accent color, density and
/// background picture.
fn theme_css(layout: &Layout) -> String {
    use crate::theme::*;
    let hex = |c: u32| format!("#{c:06x}");
    let vars = |pairs: &[(&str, u32)], accent: String, scheme: &str, veil: &str| {
        let mut out: String = pairs.iter().map(|(k, c)| format!("--{k}: {}; ", hex(*c))).collect();
        out.push_str(&format!("--accent: {accent}; --veil: {veil}; color-scheme: {scheme};"));
        out
    };
    let dark: [(&str, u32); 9] = [
        ("bg", BG),
        ("pane", PANE),
        ("raised", RAISED),
        ("rule", RULE),
        ("fg", FG),
        ("muted", MUTED),
        ("ok", OK),
        ("warn", WARN),
        ("error", ERROR),
    ];
    let dark_vars = vars(&dark, layout.accent.clone().unwrap_or_else(|| hex(ACCENT)), "dark", "rgba(17, 26, 33, 0.78)");
    let light_vars = vars(&LIGHT, layout.accent.clone().unwrap_or_else(|| hex(LIGHT_ACCENT)), "light", "rgba(244, 246, 248, 0.82)");
    let mut css = match layout.theme.as_str() {
        "light" => format!(":root {{ {light_vars} }}\n"),
        "auto" => format!(":root {{ {dark_vars} }}\n@media (prefers-color-scheme: light) {{ :root {{ {light_vars} }} }}\n"),
        _ => format!(":root {{ {dark_vars} }}\n"),
    };
    if layout.compact {
        css.push_str("body { font-size: 14px; } main { padding-top: 28px; } section { margin-top: 20px; } .rows .rowmain { padding: 3px 8px; }\n");
    }
    if let Some(bg) = layout.background.as_deref().filter(|b| image_name(b)) {
        // A veil of the page color over the picture keeps the text readable.
        css.push_str(&format!(
            "body {{ background-color: var(--bg); background-image: linear-gradient(var(--veil), var(--veil)), url(\"/user/{bg}\"); \
             background-size: cover; background-position: center; background-attachment: fixed; }}\n"
        ));
    }
    css
}

/// The app's folders the settings page can open: the workspace (saves),
/// data (history, chats, profile), traces, downloads, config (settings,
/// policy).
pub fn folder(which: &str) -> Option<std::path::PathBuf> {
    let data = rail_browser::data_dir();
    let test = std::env::var_os("UNIBROW_DATA_DIR").is_some();
    Some(match which {
        "workspace" => rail_browser::workspace::ensure(),
        "data" => data,
        "traces" => data.join("traces"),
        "downloads" => rail_browser::downloads_dir(),
        "config" if test => data,
        "config" => dirs::config_dir()?.join("agent-unibrow"),
        "home" => folder("config")?.join("home"),
        _ => return None,
    })
}

/// Where everything lives, the versions, the settings and the safety
/// rules (read-only), for `unibrow://home/settings` (homepage H4.2).
pub fn settings_state(status: &Status) -> Value {
    let path = |p: std::path::PathBuf| p.display().to_string();
    let policy_path = policy::Policy::default_path();
    let policy: Value = policy_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(json!({}));
    let cef = std::path::Path::new(rail_browser::CEF_DIR).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    json!({
        "settings": status.settings,
        "files": [
            {"what": "Settings", "path": folder("config").map(|d| path(d.join("settings.json")))},
            {"what": "Safety rules", "path": policy_path.map(path)},
            {"what": "History, bookmarks, focus sites, watches", "path": path(browsing::Browsing::default_path())},
            {"what": "Chats", "path": path(chats::Chats::default_path())},
            {"what": "Browser profile (cookies, logins, site data)", "path": path(rail_browser::profile_dir())},
            {"what": "Traces", "path": folder("traces").map(path)},
            {"what": "Workspace (screenshots, downloads, saved pages, exported chats, the agent's files)", "path": path(rail_browser::workspace::root())},
            {"what": "Your home page files (user.css, index.html, pictures)", "path": user_folder().map(path)},
        ],
        "folders": ["workspace", "data", "traces", "downloads", "config", "home"],
        "versions": {"app": env!("CARGO_PKG_VERSION"), "chromium": cef},
        "policy": policy,
    })
}

/// The newest eval run's result, in a source checkout.
fn latest_evals() -> Option<Value> {
    let dir = std::path::Path::new(EVAL_RESULTS);
    let newest = std::fs::read_dir(dir).ok()?.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).max()?;
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&newest).ok()?).ok()?;
    let failed: Vec<&str> = v["cases"].as_array()?.iter().filter(|c| c["passed"] == false).filter_map(|c| c["name"].as_str()).collect();
    Some(json!({
        "run": newest.file_stem().map(|s| s.to_string_lossy().into_owned()),
        "passed": v["passed"], "total": v["total"], "failed": failed,
    }))
}

/// This launch's token for commands, put in the served page.
pub fn token() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| {
        use std::io::Read;
        let mut bytes = [0u8; 16];
        let ok = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).is_ok();
        if !ok {
            // No urandom: the time and address still make it unguessable to a page.
            let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
            bytes.copy_from_slice(&(n ^ (u128::from(std::process::id()) << 64)).to_le_bytes());
        }
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    })
}

/// Where commands go: the shell takes the receiver once.
pub fn commands() -> async_channel::Receiver<Cmd> {
    let (tx, rx) = async_channel::unbounded();
    let _ = shared().cmds.set(tx);
    rx
}

/// Which keys are saved. Only whether: never a key.
#[derive(Clone, Copy, Debug, Default)]
pub struct Keys {
    pub anthropic: bool,
    pub jev: bool,
    pub subq: bool,
    pub gmail_client: bool,
    pub gmail_signed_in: bool,
}

impl Keys {
    /// Looks them up. Reads the keyring, so the shell calls this at startup
    /// and when a key changes, never per request.
    pub fn check(anthropic: bool) -> Self {
        use rail_browser::jev::{Model, key_for};
        Keys {
            anthropic,
            jev: key_for(Model::Jev).is_some(),
            subq: key_for(Model::SubQ).is_some(),
            gmail_client: rail_api::client_secret_path().is_some_and(|p| p.exists()),
            gmail_signed_in: rail_api::google_signed_in(),
        }
    }
}

struct Shared {
    store: OnceLock<Arc<Browsing>>,
    chats: OnceLock<Arc<chats::Chats>>,
    status: RwLock<Status>,
    cmds: OnceLock<async_channel::Sender<Cmd>>,
}

fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(|| Shared {
        store: OnceLock::new(),
        chats: OnceLock::new(),
        status: RwLock::new(Status::default()),
        cmds: OnceLock::new(),
    })
}

/// Gives the page the stores it reads. The shell calls this once.
pub fn attach(store: Arc<Browsing>, chats: Arc<chats::Chats>) {
    let _ = shared().store.set(store);
    let _ = shared().chats.set(chats);
}

/// The shell's latest status, for the next `/api/state`.
pub fn publish(status: Status) {
    if let Ok(mut s) = shared().status.write() {
        *s = status;
    }
}

/// The line under a focus site about Jev, shared by the Focus tab and the
/// home page: learning progress, ready (and what it can do), off, failed.
pub fn jev_line(progress: Option<&str>, learned: Option<&JevSite>) -> Option<String> {
    match (progress, learned) {
        (Some(p), _) => Some(format!("Jev learning: {p}")),
        (None, Some(j)) if j.status == "ready" && j.enabled => Some(format!("Jev ready: {}", j.skills.abilities().join(" · "))),
        (None, Some(j)) if j.status == "ready" => Some("Jev off for this site".into()),
        (None, Some(j)) if j.status == "failed" => Some("Jev couldn't learn this site".into()),
        _ => None,
    }
}

/// Only web addresses become links on the page.
fn web_url(u: &str) -> Option<&str> {
    (u.starts_with("https://") || u.starts_with("http://")).then_some(u)
}

/// Everything the page shows.
pub fn state(store: &Browsing, chats: Option<&chats::Chats>, status: &Status) -> Value {
    let q = browsing::HistoryQuery { text: None, site: None, since_days: None, limit: 300 };
    let mut seen = std::collections::HashSet::new();
    let recent: Vec<Value> = store
        .history(&q)
        .into_iter()
        .filter(|v| web_url(&v.url).is_some() && seen.insert(v.site.clone()))
        .take(12)
        .map(|v| json!({"site": v.site, "title": v.title, "url": v.url, "at": v.at.timestamp()}))
        .collect();
    let bookmarks: Vec<Value> = store
        .bookmarks()
        .into_iter()
        .filter(|b| web_url(&b.url).is_some())
        .take(12)
        .map(|b| json!({"site": b.site, "title": b.title, "url": b.url}))
        .collect();
    let jev: HashMap<String, JevSite> = store.jev_sites().into_iter().map(|j| (j.site.clone(), j)).collect();
    let playbooks = store.playbook_counts();
    let focus: Vec<Value> = store
        .focus_sites()
        .into_iter()
        .map(|f| {
            let key = f.site.trim_start_matches("www.").to_ascii_lowercase();
            let line = jev_line(status.jev_progress.get(&key).map(String::as_str), jev.get(&key));
            let pb = playbooks.get(&f.site).or_else(|| playbooks.get(&key)).copied().unwrap_or(0);
            json!({"site": f.site, "label": f.label, "url": format!("https://{}/", f.site), "jev": line, "playbooks": pb})
        })
        .collect();
    let watches: Vec<Value> = store
        .watches()
        .into_iter()
        .map(|w| {
            json!({
                "id": w.id, "label": w.label, "url": web_url(&w.url), "kind": w.kind, "when": w.when,
                "value": w.last_value, "checked": w.last_checked.map(|t| t.timestamp()),
                "error": w.last_error,
            })
        })
        .collect();
    let playbook_list: Vec<Value> = playbooks.iter().map(|(site, n)| json!({"site": site, "count": n})).collect();
    let recent_chats: Vec<Value> = chats
        .map(|c| c.all())
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.items > 0)
        .take(6)
        .map(|c| {
            json!({"id": c.id, "title": c.title, "updated": c.updated.timestamp(), "items": c.items, "current": c.id == status.chat_id})
        })
        .collect();
    let k = status.keys;
    let choices: Vec<Value> = status
        .choices
        .iter()
        .map(|(id, label, models)| {
            json!({"backend": id, "label": label, "models": models.iter().map(|(m, l)| json!({"id": m, "label": l})).collect::<Vec<_>>()})
        })
        .collect();
    let watch_ids: Vec<i64> = store.watches().into_iter().map(|w| w.id).collect();
    let trace: Vec<Value> =
        status.trace.iter().rev().map(|(kind, summary, at)| json!({"kind": kind, "summary": summary, "at": at})).collect();
    let pins: Vec<Value> = store.pins().into_iter().map(|p| json!({"url": p.url, "title": p.title, "site": p.site})).collect();
    json!({
        "layout": Layout::load().to_value(),
        "user_images": user_images(),
        "sections": SECTIONS,
        "pins": pins,
        "guide": crate::explained::guide_json(),
        "trace": trace,
        "evals": latest_evals(),
        "recent": recent,
        "bookmarks": bookmarks,
        "focus": focus,
        "watches": watches,
        "playbooks": playbook_list,
        "chats": recent_chats,
        "watch_ids": watch_ids,
        "status": {
            "agent": status.agent, "model": status.model, "busy": status.busy,
            "backend": status.backend, "model_id": status.model_id, "choices": choices,
            "keys": {
                "anthropic": k.anthropic, "jev": k.jev, "subq": k.subq,
                "gmail": if k.gmail_signed_in { "signed in" } else if k.gmail_client { "set up, not signed in" } else { "not set up" },
            },
        },
    })
}

/// A site's saved icon as a PNG.
fn icon_png(store: &Browsing, site: &str) -> Option<Vec<u8>> {
    let f = store.favicon(site)?;
    let mut rgba = f.bgra;
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let img = image::RgbaImage::from_raw(f.width, f.height, rgba)?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// `POST /api/cmd`: checks the request and the command, then hands it to
/// the shell.
fn command(req: &AppRequest) -> Page {
    if req.method != "POST" || req.token != token() {
        return Page::forbidden();
    }
    let refuse = |why: String| {
        eprintln!("agent-unibrow: the home page's command was refused: {why}");
        if let Some(tx) = shared().cmds.get() {
            let _ = tx.try_send(Cmd::Refused(why.clone()));
        }
        Page::bad_request(&why)
    };
    let v: Value = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(_) => return refuse("not a JSON command".into()),
    };
    match parse_cmd(&v) {
        Ok(cmd) => match shared().cmds.get().map(|tx| tx.try_send(cmd)) {
            Some(Ok(())) => Page { status: 202, ..Page::json(&json!({"ok": true})) },
            _ => Page::bad_request("the app isn't ready"),
        },
        Err(why) => refuse(why),
    }
}

pub struct Home;

impl AppPages for Home {
    fn get(&self, req: &AppRequest) -> Page {
        if req.host != "home" {
            return Page::not_found();
        }
        match req.path.as_str() {
            // The user's own page, if they made one; the built-in one otherwise.
            "/" | "/index.html" => match user_file("index.html") {
                Some((html, _)) => Page::ok("text/html", String::from_utf8_lossy(&html).replace("{{TOKEN}}", token())),
                None => Page::ok("text/html", INDEX.replace("{{TOKEN}}", token())),
            },
            "/default" => Page::ok("text/html", INDEX.replace("{{TOKEN}}", token())),
            "/theme.css" => Page::ok("text/css", theme_css(&Layout::load())),
            "/user.css" => Page::ok("text/css", user_file("user.css").map(|(b, _)| b).unwrap_or_default()),
            p if p.starts_with("/user/") => match user_file(&p["/user/".len()..]) {
                Some((bytes, mime)) => Page::ok(mime, bytes),
                None => Page::not_found(),
            },
            "/home.css" => Page::ok("text/css", CSS),
            "/home.js" => Page::ok("text/javascript", JS),
            "/favicon.svg" => Page::ok("image/svg+xml", ICON),
            "/settings" => Page::ok("text/html", SETTINGS.replace("{{TOKEN}}", token())),
            "/settings.js" => Page::ok("text/javascript", SETTINGS_JS),
            // Data is for the page itself only.
            p if p.starts_with("/api/") || p.starts_with("/icon/") => {
                if !req.from_app_page {
                    return Page::forbidden();
                }
                if p == "/api/cmd" {
                    return command(req);
                }
                if p == "/api/settings" {
                    let status = shared().status.read().map(|s| s.clone()).unwrap_or_default();
                    return Page::json(&settings_state(&status));
                }
                let Some(store) = shared().store.get() else { return Page::not_found() };
                if p == "/api/state" {
                    let status = shared().status.read().map(|s| s.clone()).unwrap_or_default();
                    return Page::json(&state(store, shared().chats.get().map(|c| c.as_ref()), &status));
                }
                let site = p.strip_prefix("/icon/").and_then(|s| s.strip_suffix(".png")).unwrap_or("");
                match icon_png(store, site) {
                    Some(png) => Page::ok("image/png", png),
                    None => Page::not_found(),
                }
            }
            _ => Page::not_found(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(path: &str, from_app_page: bool) -> AppRequest {
        AppRequest { host: "home".into(), path: path.into(), from_app_page, ..Default::default() }
    }

    #[test]
    fn commands_are_checked() {
        let p = |v: Value| parse_cmd(&v);
        assert_eq!(p(json!({"cmd": "ask", "text": " hi "})).unwrap(), Cmd::Ask { text: "hi".into() });
        assert_eq!(
            p(json!({"cmd": "show", "tab": "site", "site": "Sweetwater.com"})).unwrap(),
            Cmd::Show { tab: "site".into(), site: Some("sweetwater.com".into()), page: None }
        );
        assert!(p(json!({"cmd": "show", "tab": "explained", "page": 99})).is_err());
        assert_eq!(p(json!({"cmd": "open_chat", "id": 7})).unwrap(), Cmd::OpenChat { id: 7 });
        assert!(p(json!({"cmd": "open_chat", "id": "7; drop"})).is_err());
        assert!(p(json!({"cmd": "open_folder", "which": "traces"})).is_ok());
        assert!(p(json!({"cmd": "open_folder", "which": "/etc"})).is_err());
        assert!(p(json!({"cmd": "set_focus_only", "on": false})).is_err());
        assert!(p(json!({"cmd": "new_tab", "url": "unibrow://home/"})).is_err());
        assert!(p(json!({"cmd": "new_tab", "url": "javascript:alert(1)"})).is_err());
        assert!(p(json!({"cmd": "show", "tab": "keys"})).is_err());
        assert!(p(json!({"cmd": "set_agent", "backend": "anthropic_api"})).is_err());
        assert!(p(json!({"cmd": "learn_jev", "site": "a.com/../x"})).is_err());
        assert!(p(json!({"cmd": "ask", "text": "x".repeat(MAX_TEXT + 1)})).is_err());
    }

    #[test]
    fn layouts_drop_what_they_dont_know() {
        let l = Layout::from_value(&json!({
            "hidden": ["chats", "nope", "chats"], "order": ["focus", "<script>"], "density": "compact", "accent": "#FF8800",
        }));
        assert_eq!(l.hidden, ["chats"]);
        assert_eq!(l.order, ["focus"]);
        assert!(l.compact);
        assert_eq!(l.accent.as_deref(), Some("#ff8800"));
        assert_eq!(Layout::from_value(&json!({"accent": "red; }"})).accent, None);
        assert_eq!(l.theme, "dark");
        let light = Layout::from_value(&json!({"theme": "light", "background": "bg.jpg"}));
        assert!(theme_css(&light).contains("--bg: #f4f6f8") && theme_css(&light).contains("url(\"/user/bg.jpg\")"));
        let auto = theme_css(&Layout::from_value(&json!({"theme": "auto"})));
        assert!(auto.contains("prefers-color-scheme: light") && auto.contains("--bg: #111a21"));
        assert_eq!(Layout::from_value(&json!({"theme": "neon"})).theme, "dark");
        for bad in ["../x.jpg", "x.svg", "a\")}body{x.png", ".bg.png"] {
            assert_eq!(Layout::from_value(&json!({"background": bad})).background, None, "{bad}");
        }
        assert_eq!(Layout::from_value(&l.to_value()), l);
        let css = theme_css(&l);
        assert!(css.contains("--accent: #ff8800") && css.contains("--bg: #111a21"));
        assert!(theme_css(&Layout::default()).contains("--accent: #5fc8c0"));
    }

    #[test]
    fn user_files_stay_in_their_folder() {
        for bad in ["../settings.json", "a/b.css", ".hidden.css", "x.exe", "", "user.css/.."] {
            assert!(user_file(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn commands_need_the_page_and_its_token() {
        let _rx = commands();
        let post = |token: &str, from_app_page: bool| {
            Home.get(&AppRequest {
                method: "POST".into(),
                token: token.into(),
                body: br#"{"cmd":"fill_chat","text":"hello"}"#.to_vec(),
                ..req("/api/cmd", from_app_page)
            })
            .status
        };
        assert_eq!(post(token(), true), 202);
        assert_eq!(post("guess", true), 403);
        assert_eq!(post(token(), false), 403);
        assert_eq!(Home.get(&AppRequest { token: token().into(), ..req("/api/cmd", true) }).status, 403, "GET");
        // The token is in the page the app serves, and only there.
        assert!(String::from_utf8(Home.get(&req("/", false)).body).unwrap().contains(token()));
        assert!(!JS.contains(token()));
    }

    #[test]
    fn serves_the_home_files() {
        assert_eq!(Home.get(&req("/", false)).mime, "text/html");
        assert!(String::from_utf8(Home.get(&req("/", false)).body).unwrap().contains("<title>Home</title>"));
        assert_eq!(Home.get(&req("/home.css", false)).status, 200);
        assert_eq!(Home.get(&req("/home.js", false)).mime, "text/javascript");
        assert_eq!(Home.get(&req("/favicon.svg", false)).mime, "image/svg+xml");
        assert_eq!(Home.get(&req("/secret", false)).status, 404);
        assert_eq!(Home.get(&AppRequest { host: "settings".into(), ..req("/", false) }).status, 404);
        // Data only for the page itself.
        assert_eq!(Home.get(&req("/api/state", false)).status, 403);
        assert_eq!(Home.get(&req("/icon/a.com.png", false)).status, 403);
        assert_eq!(Home.get(&req("/api/settings", false)).status, 403);
        assert_eq!(Home.get(&req("/settings", false)).mime, "text/html");
    }

    #[test]
    fn page_loads_nothing_from_the_web() {
        assert!(!SETTINGS.contains("<script>") && !SETTINGS.contains("http"));
        assert!(!SETTINGS_JS.contains("innerHTML"));
        // The only outside address is the search form's target.
        let html = INDEX.replace("action=\"https://duckduckgo.com/\"", "");
        assert!(!html.contains("http://") && !html.contains("https://"), "{html}");
        assert!(!html.contains("<script>"), "inline scripts are blocked by the CSP");
        let handler = html.split_whitespace().any(|w| w.starts_with("on") && w.contains("=\""));
        assert!(!handler, "inline handlers are blocked by the CSP");
        // Page data is put in with textContent, never as HTML.
        assert!(!JS.contains("innerHTML") && !JS.contains("insertAdjacentHTML") && !JS.contains("eval("));
    }

    #[test]
    fn state_has_the_stores_and_no_secrets() {
        let store = Browsing::in_memory();
        store.record_visit(1, "https://www.sweetwater.com/store/detail/x", "TUSQ nut", false);
        store.record_visit(1, "https://www.sweetwater.com/store/detail/y", "Bone nut", false);
        store.record_visit(1, "https://example.com/", "Example", false);
        store.add_focus("www.sweetwater.com", "Sweetwater");
        store.toggle_bookmark("https://example.com/", "Example");
        let status = Status {
            agent: "Claude Code".into(),
            model: "Opus 5.5".into(),
            keys: Keys { jev: true, ..Default::default() },
            ..Default::default()
        };
        let v = state(&store, None, &status);
        // Recent is one row per site, newest first.
        let recent: Vec<&str> = v["recent"].as_array().unwrap().iter().map(|r| r["site"].as_str().unwrap()).collect();
        assert_eq!(recent, ["example.com", "sweetwater.com"]);
        assert_eq!(v["focus"][0]["site"], "www.sweetwater.com");
        assert_eq!(v["bookmarks"][0]["url"], "https://example.com/");
        assert_eq!(v["status"]["keys"]["jev"], true);
        assert_eq!(v["status"]["keys"]["gmail"], "not set up");
        // Keys are only ever yes or no.
        for k in ["anthropic", "jev", "subq"] {
            assert!(v["status"]["keys"][k].is_boolean());
        }
    }
}

// ------------------------------------------------- the agent's tool (H5.5)

/// `home_customize`: the agent changes the home page when asked ("hide the
/// watches", "make it orange"). It can only change the layout and the
/// user's `user.css`, and the user approves each change.
pub struct Customize;

const MAX_CSS: usize = 64 * 1024;

impl Customize {
    /// What a call would change, in words, for the approval card.
    fn describe(input: &Value) -> String {
        let list = |k: &str| -> Option<String> {
            let v: Vec<&str> = input[k].as_array()?.iter().filter_map(Value::as_str).collect();
            (!v.is_empty()).then(|| v.join(", "))
        };
        let mut parts = Vec::new();
        if input["reset"] == true {
            parts.push("reset the layout".to_string());
        }
        if let Some(h) = list("hide") {
            parts.push(format!("hide {h}"));
        }
        if let Some(sh) = list("show") {
            parts.push(format!("show {sh}"));
        }
        if let Some(o) = list("order") {
            parts.push(format!("put {o} first"));
        }
        if let Some(d) = input["density"].as_str() {
            parts.push(format!("make it {d}"));
        }
        if let Some(a) = input["accent"].as_str() {
            parts.push(format!("accent color {a}"));
        }
        if let Some(t) = input["theme"].as_str() {
            parts.push(format!("{t} theme"));
        }
        if let Some(b) = input["background"].as_str() {
            parts.push(if b.is_empty() { "no background picture".into() } else { format!("background picture {b}") });
        }
        if let Some(css) = input["css"].as_str() {
            parts.push(if css.trim().is_empty() {
                "remove your user.css".into()
            } else {
                format!("replace your user.css ({} lines):\n{css}", css.lines().count())
            });
        }
        if parts.is_empty() { "Change nothing on the home page".into() } else { format!("Home page: {}", parts.join("; ")) }
    }

    fn apply(input: &Value) -> Result<String, String> {
        let mut layout = if input["reset"] == true { Layout::default() } else { Layout::load() };
        let ids = |k: &str| Layout::from_value(&json!({"hidden": input[k]})).hidden;
        for id in ids("hide") {
            if !layout.hidden.contains(&id) {
                layout.hidden.push(id);
            }
        }
        let show = ids("show");
        layout.hidden.retain(|h| !show.contains(h));
        let order = ids("order");
        if !order.is_empty() {
            layout.order = order;
        }
        match input["density"].as_str() {
            Some("compact") => layout.compact = true,
            Some("roomy") => layout.compact = false,
            Some(other) => return Err(format!("density must be compact or roomy, not {other}")),
            None => {}
        }
        if let Some(a) = input["accent"].as_str() {
            layout.accent = Some(Layout::from_value(&json!({"accent": a})).accent.ok_or("accent must be a color like #ff8800")?);
        }
        if let Some(t) = input["theme"].as_str() {
            if !THEMES.contains(&t) {
                return Err(format!("theme must be dark, light or auto, not {t}"));
            }
            layout.theme = t.to_string();
        }
        match input["background"].as_str() {
            Some("") => layout.background = None,
            Some(b) if user_images().iter().any(|i| i == b) => layout.background = Some(b.to_string()),
            Some(b) => return Err(format!("{b} isn't a picture in the home folder (there: {})", user_images().join(", "))),
            None => {}
        }
        layout.save()?;
        if let Some(css) = input["css"].as_str() {
            if css.len() > MAX_CSS {
                return Err(format!("user.css can be at most {MAX_CSS} bytes"));
            }
            let dir = user_folder().ok_or("no config folder")?;
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            std::fs::write(dir.join("user.css"), css).map_err(|e| e.to_string())?;
        }
        Ok(Self::describe(input))
    }
}

impl router::Tool for Customize {
    fn name(&self) -> &'static str {
        "home_customize"
    }
    fn description(&self) -> &'static str {
        "Change the app's home page (unibrow://home/) when the user asks: hide or show sections, put sections \
         first, compact or roomy spacing, the accent color, a dark, light or automatic theme, a background \
         picture from the home folder, or the user's own CSS (replaces their user.css; \
         an empty string removes it). Sections: pins, recent, focus, bookmarks, watches, playbooks, chats, ask, \
         tinker, guide, activity, keys, status. The user approves each change."
    }
    fn input_schema(&self) -> Value {
        let ids = json!({"type": "array", "items": {"type": "string", "enum": SECTIONS}});
        json!({
            "type": "object",
            "properties": {
                "hide": ids, "show": ids,
                "order": {"type": "array", "items": {"type": "string", "enum": SECTIONS}, "description": "Sections to put first, in this order."},
                "density": {"type": "string", "enum": ["compact", "roomy"]},
                "accent": {"type": "string", "description": "A color like #ff8800."},
                "theme": {"type": "string", "enum": THEMES, "description": "dark (the app's colors), light, or auto (follow the desktop)."},
                "background": {"type": "string", "description": "A picture's file name in the home folder (empty: none)."},
                "css": {"type": "string", "description": "The whole new user.css."},
                "reset": {"type": "boolean", "description": "Start from the default layout."}
            },
            "additionalProperties": false
        })
    }
    fn effect(&self) -> router::Effect {
        // It changes the user's own page: they see and approve each change.
        router::Effect::Commit
    }
    fn describe_call(&self, input: &Value) -> String {
        Self::describe(input)
    }
    fn call(&self, input: Value) -> futures::future::BoxFuture<'static, router::ToolResult> {
        Box::pin(async move {
            let done = Self::apply(&input)?;
            if let Some(tx) = shared().cmds.get() {
                let _ = tx.try_send(Cmd::Customized(done.clone()));
            }
            Ok(router::ToolOutput::text(format!("Done. {done}. Open home pages show it now."), "home page changed".to_string()))
        })
    }
}
