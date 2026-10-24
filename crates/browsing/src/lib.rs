//! Browsing history and focus sites, shared by the UI and the agents.
//!
//! - **History**: every page the embedded browser shows (time, URL, title,
//!   site), searchable by text, site and date.
//! - **Focus sites**: sites the user has singled out. Agents see this list
//!   with every message and can look it up with a tool. It is the scope later
//!   decision models (Jev) and long-context models (SubQ) work within.
//!
//! - **Watches**: pages the app re-checks on a schedule ("tell me when
//!   this price drops"). The app's checker loads them in the background and
//!   tells the user when the watched value changes the way they asked.
//!
//! Stored in SQLite at `~/.local/share/agent-unibrow/browsing.sqlite3`.
//! The store is `Send + Sync`: the UI writes visits, agent tools read.

pub mod favicons;
pub mod home_pins;
pub mod jev_sites;
pub mod playbook_versions;
pub mod sitemap;
mod tools;

pub use favicons::Favicon;
pub use tools::tools;

use chrono::{DateTime, Local, TimeZone};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::PathBuf;
use std::sync::Mutex;

/// Revisiting the same URL within this window updates the existing entry
/// instead of adding a new one (reloads, redirects, title arriving late).
const MERGE_WINDOW_SECS: i64 = 30 * 60;
/// A redirect on the same site this soon after the tab's previous visit
/// replaces that visit instead of adding a new one.
const REDIRECT_WINDOW_SECS: i64 = 2;

#[derive(Clone, Debug)]
pub struct Visit {
    pub id: i64,
    pub url: String,
    pub title: String,
    pub site: String,
    pub at: DateTime<Local>,
}

#[derive(Clone, Debug)]
pub struct Bookmark {
    pub url: String,
    pub title: String,
    pub site: String,
    pub added: DateTime<Local>,
}

#[derive(Clone, Debug)]
pub struct FocusSite {
    pub site: String,
    /// A readable name (the page title when it was added, or the user's).
    pub label: String,
    pub added: DateTime<Local>,
}

/// A page the app checks on a schedule (roadmap 9.4).
#[derive(Clone, Debug)]
pub struct Watch {
    pub id: i64,
    pub url: String,
    pub label: String,
    /// `price` or `text`.
    pub kind: String,
    /// CSS selector for the part of the page to read, if not the whole page.
    pub selector: Option<String>,
    /// For text watches: text whose presence is tracked ("In stock").
    pub needle: Option<String>,
    /// `changes`, `drops`, `rises`, `appears` or `disappears`.
    pub when: String,
    pub every_min: i64,
    pub last_value: Option<String>,
    pub last_checked: Option<DateTime<Local>>,
    pub last_changed: Option<DateTime<Local>>,
    pub last_error: Option<String>,
    pub created: DateTime<Local>,
}

/// A watch saw the change it was waiting for.
#[derive(Clone, Debug)]
pub struct WatchFired {
    pub watch: Watch,
    pub old: String,
    pub new: String,
}

#[derive(Clone, Debug, Default)]
pub struct NewWatch {
    pub url: String,
    pub label: String,
    pub kind: String,
    pub selector: Option<String>,
    pub needle: Option<String>,
    pub when: String,
    pub every_min: i64,
}

/// Most watches kept at once.
pub const MAX_WATCHES: usize = 50;

/// The number in a price string: "$1,299.00" → 1299.0, "13,98 €" → 13.98.
pub fn parse_price(text: &str) -> Option<f64> {
    let start = text.find(|c: char| c.is_ascii_digit())?;
    let num: String = text[start..].chars().take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',').collect();
    let num = num.trim_end_matches(['.', ',']);
    let normalized = match (num.rfind('.'), num.rfind(',')) {
        // "1.299,00": dots group, comma is the decimal point.
        (Some(d), Some(c)) if c > d => num.replace('.', "").replace(',', "."),
        (Some(_), Some(_)) => num.replace(',', ""),
        // "13,98": a comma with two digits after it is a decimal point.
        (None, Some(c)) if num.len() - c - 1 == 2 => num.replace(',', "."),
        _ => num.replace(',', ""),
    };
    normalized.parse().ok()
}

/// Whether going from `old` to `new` is what the watch waits for.
fn watch_fires(when: &str, old: &str, new: &str) -> bool {
    if old == new {
        return false;
    }
    match when {
        "drops" => matches!((parse_price(old), parse_price(new)), (Some(o), Some(n)) if n < o),
        "rises" => matches!((parse_price(old), parse_price(new)), (Some(o), Some(n)) if n > o),
        "appears" => new == "present",
        "disappears" => new == "absent",
        _ => true,
    }
}

#[derive(Default, Clone, Debug)]
pub struct HistoryQuery {
    /// Matches title or URL, case-insensitive.
    pub text: Option<String>,
    /// Only this site (e.g. `amazon.com`, subdomains included).
    pub site: Option<String>,
    /// Only visits in the last N days.
    pub since_days: Option<u32>,
    pub limit: usize,
}

pub struct Browsing {
    conn: Mutex<Connection>,
    /// Bumped when a site icon is saved (see [`favicons`]).
    favicon_version: std::sync::atomic::AtomicU64,
    /// Each tab's latest visit (tab id → visit id), so a redirect replaces
    /// the visit in its own tab and not another tab's.
    tab_visits: Mutex<std::collections::HashMap<u32, i64>>,
}

/// The site a URL belongs to: its host without a leading `www.`.
/// `None` for internal pages (about:, data:, chrome:, file:).
pub fn site_of(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let host = u.host_str()?.to_ascii_lowercase();
    Some(host.strip_prefix("www.").unwrap_or(&host).to_string())
}

/// The URL as history keeps it: without the `#fragment`, which only
/// scrolls within the same page.
pub fn history_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) => {
            u.set_fragment(None);
            u.to_string()
        }
        Err(_) => url.split('#').next().unwrap_or(url).to_string(),
    }
}

fn when(ts: i64) -> DateTime<Local> {
    Local.timestamp_opt(ts, 0).single().unwrap_or_else(Local::now)
}

impl Browsing {
    pub fn default_path() -> PathBuf {
        std::env::var_os("UNIBROW_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs::data_local_dir().unwrap_or_else(std::env::temp_dir).join("agent-unibrow"))
            .join("browsing.sqlite3")
    }

    pub fn open_default() -> Result<Self, String> {
        let path = Self::default_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        Self::open(Connection::open(&path).map_err(|e| format!("can't open {}: {e}", path.display()))?)
    }

    pub fn in_memory() -> Self {
        Self::open(Connection::open_in_memory().expect("in-memory sqlite")).expect("schema")
    }

    fn open(conn: Connection) -> Result<Self, String> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS visits (
                 id INTEGER PRIMARY KEY,
                 url TEXT NOT NULL,
                 title TEXT NOT NULL DEFAULT '',
                 site TEXT NOT NULL,
                 at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS visits_at ON visits(at DESC);
             CREATE INDEX IF NOT EXISTS visits_site ON visits(site, at DESC);
             CREATE TABLE IF NOT EXISTS bookmarks (
                 url TEXT PRIMARY KEY,
                 title TEXT NOT NULL DEFAULT '',
                 site TEXT NOT NULL,
                 added INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS focus_sites (
                 site TEXT PRIMARY KEY,
                 label TEXT NOT NULL DEFAULT '',
                 added INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS watches (
                 id INTEGER PRIMARY KEY,
                 url TEXT NOT NULL,
                 label TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 selector TEXT,
                 needle TEXT,
                 notify_when TEXT NOT NULL,
                 every_min INTEGER NOT NULL,
                 last_value TEXT,
                 last_checked INTEGER,
                 last_changed INTEGER,
                 last_error TEXT,
                 created INTEGER NOT NULL
             );",
        )
        .map_err(|e| format!("can't set up the history database: {e}"))?;
        sitemap::init(&conn).map_err(|e| format!("can't set up site maps: {e}"))?;
        playbook_versions::init(&conn).map_err(|e| format!("can't set up playbook versions: {e}"))?;
        jev_sites::init(&conn).map_err(|e| format!("can't set up Jev sites: {e}"))?;
        favicons::init(&conn).map_err(|e| format!("can't set up site icons: {e}"))?;
        home_pins::init(&conn).map_err(|e| format!("can't set up home pins: {e}"))?;
        Ok(Self { conn: Mutex::new(conn), favicon_version: Default::default(), tab_visits: Default::default() })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("browsing db lock")
    }

    /// Records that `url` is showing in tab `tab`. `redirect` says the page
    /// the tab is leaving never finished loading (a script or server sent
    /// it on): then, on the same site and within a moment, the new address
    /// replaces that tab's previous visit. Returns false for pages that
    /// aren't kept (internal pages).
    pub fn record_visit(&self, tab: u32, url: &str, title: &str, redirect: bool) -> bool {
        let Some(site) = site_of(url) else { return false };
        let url = history_url(url);
        let url = url.as_str();
        let now = Local::now().timestamp();
        let c = self.conn();
        let mut tab_visits = self.tab_visits.lock().expect("tab visits lock");
        let previous: Option<(i64, String, i64)> = tab_visits.get(&tab).and_then(|id| {
            c.query_row("SELECT id, site, at FROM visits WHERE id = ?1", params![id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()
            .unwrap_or(None)
        });
        if let Some((id, last_site, at)) = previous {
            if redirect && last_site == site && now - at <= REDIRECT_WINDOW_SECS {
                // If the page is changing back to a URL that already has a
                // recent entry, fold into that entry instead of duplicating it.
                let existing: Option<i64> = c
                    .query_row(
                        "SELECT id FROM visits WHERE url = ?1 AND id != ?2 AND at >= ?3 ORDER BY at DESC LIMIT 1",
                        params![url, id, now - MERGE_WINDOW_SECS],
                        |r| r.get(0),
                    )
                    .optional()
                    .unwrap_or(None);
                if let Some(keep) = existing {
                    let _ = c.execute("DELETE FROM visits WHERE id = ?1", params![id]);
                    tab_visits.insert(tab, keep);
                    let res = if title.is_empty() {
                        c.execute("UPDATE visits SET at = ?1 WHERE id = ?2", params![now, keep])
                    } else {
                        c.execute("UPDATE visits SET at = ?1, title = ?2 WHERE id = ?3", params![now, title, keep])
                    };
                    return res.is_ok();
                }
                let res = if title.is_empty() {
                    c.execute("UPDATE visits SET url = ?1, at = ?2 WHERE id = ?3", params![url, now, id])
                } else {
                    c.execute(
                        "UPDATE visits SET url = ?1, title = ?2, at = ?3 WHERE id = ?4",
                        params![url, title, now, id],
                    )
                };
                return res.is_ok();
            }
        }
        let recent: Option<i64> = c
            .query_row(
                "SELECT id FROM visits WHERE url = ?1 AND at >= ?2 ORDER BY at DESC LIMIT 1",
                params![url, now - MERGE_WINDOW_SECS],
                |r| r.get(0),
            )
            .optional()
            .unwrap_or(None);
        let res = match recent {
            Some(id) if title.is_empty() => c.execute("UPDATE visits SET at = ?1 WHERE id = ?2", params![now, id]),
            Some(id) => c.execute("UPDATE visits SET at = ?1, title = ?2 WHERE id = ?3", params![now, title, id]),
            None => c.execute(
                "INSERT INTO visits (url, title, site, at) VALUES (?1, ?2, ?3, ?4)",
                params![url, title, site, now],
            ),
        };
        if res.is_ok() {
            tab_visits.insert(tab, recent.unwrap_or_else(|| c.last_insert_rowid()));
        }
        res.is_ok()
    }

    /// Sets the title on the latest visit to `url` (titles arrive after the URL).
    pub fn update_title(&self, url: &str, title: &str) {
        if title.is_empty() {
            return;
        }
        let url = history_url(url);
        let _ = self.conn().execute(
            "UPDATE visits SET title = ?1 WHERE id = (SELECT id FROM visits WHERE url = ?2 ORDER BY at DESC LIMIT 1)",
            params![title, url.as_str()],
        );
    }

    pub fn history(&self, q: &HistoryQuery) -> Vec<Visit> {
        let mut sql = String::from("SELECT id, url, title, site, at FROM visits WHERE 1=1");
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(t) = q.text.as_ref().map(|t| t.trim()).filter(|t| !t.is_empty()) {
            sql.push_str(" AND (title LIKE ? ESCAPE '\\' OR url LIKE ? ESCAPE '\\')");
            let pat = format!("%{}%", t.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
            args.push(pat.clone().into());
            args.push(pat.into());
        }
        if let Some(site) = q.site.as_ref().map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()) {
            let site = site.strip_prefix("www.").unwrap_or(&site).to_string();
            sql.push_str(" AND (site = ? OR site LIKE ?)");
            args.push(site.clone().into());
            args.push(format!("%.{site}").into());
        }
        if let Some(days) = q.since_days {
            sql.push_str(" AND at >= ?");
            args.push((Local::now().timestamp() - i64::from(days) * 86_400).into());
        }
        sql.push_str(" ORDER BY at DESC, id DESC LIMIT ?");
        args.push((q.limit.clamp(1, 1000) as i64).into());

        let c = self.conn();
        let Ok(mut stmt) = c.prepare(&sql) else { return Vec::new() };
        stmt.query_map(rusqlite::params_from_iter(args), |r| {
            Ok(Visit { id: r.get(0)?, url: r.get(1)?, title: r.get(2)?, site: r.get(3)?, at: when(r.get(4)?) })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    pub fn clear_history(&self) {
        let _ = self.conn().execute("DELETE FROM visits", []);
        self.prune_favicons();
    }

    /// Bookmarks, newest first.
    pub fn bookmarks(&self) -> Vec<Bookmark> {
        let c = self.conn();
        let Ok(mut stmt) = c.prepare("SELECT url, title, site, added FROM bookmarks ORDER BY added DESC") else {
            return Vec::new();
        };
        stmt.query_map([], |r| {
            Ok(Bookmark { url: r.get(0)?, title: r.get(1)?, site: r.get(2)?, added: when(r.get(3)?) })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    pub fn is_bookmarked(&self, url: &str) -> bool {
        self.conn()
            .query_row("SELECT 1 FROM bookmarks WHERE url = ?1", params![history_url(url)], |_| Ok(()))
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    /// Bookmarks `url`. Returns false for pages that can't be bookmarked
    /// (internal pages).
    pub fn add_bookmark(&self, url: &str, title: &str) -> bool {
        let Some(site) = site_of(url) else { return false };
        self.conn()
            .execute(
                "INSERT INTO bookmarks (url, title, site, added) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(url) DO UPDATE SET title = excluded.title",
                params![history_url(url), title, site, Local::now().timestamp()],
            )
            .is_ok()
    }

    pub fn remove_bookmark(&self, url: &str) {
        let _ = self.conn().execute("DELETE FROM bookmarks WHERE url = ?1", params![history_url(url)]);
    }

    /// Bookmarks `url` if it isn't, removes the bookmark if it is. Returns
    /// whether it's bookmarked now.
    pub fn toggle_bookmark(&self, url: &str, title: &str) -> bool {
        if self.is_bookmarked(url) {
            self.remove_bookmark(url);
            false
        } else {
            self.add_bookmark(url, title)
        }
    }

    pub fn focus_sites(&self) -> Vec<FocusSite> {
        let c = self.conn();
        let Ok(mut stmt) = c.prepare("SELECT site, label, added FROM focus_sites ORDER BY added") else {
            return Vec::new();
        };
        stmt.query_map([], |r| Ok(FocusSite { site: r.get(0)?, label: r.get(1)?, added: when(r.get(2)?) }))
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    pub fn is_focused(&self, site: &str) -> bool {
        self.conn()
            .query_row("SELECT 1 FROM focus_sites WHERE site = ?1", params![site], |_| Ok(()))
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    pub fn add_focus(&self, site: &str, label: &str) {
        let site = site.trim().to_ascii_lowercase();
        if site.is_empty() {
            return;
        }
        let _ = self.conn().execute(
            "INSERT INTO focus_sites (site, label, added) VALUES (?1, ?2, ?3)
             ON CONFLICT(site) DO UPDATE SET label = excluded.label",
            params![site, label, Local::now().timestamp()],
        );
    }

    pub fn remove_focus(&self, site: &str) {
        let _ = self.conn().execute("DELETE FROM focus_sites WHERE site = ?1", params![site]);
    }

    /// Adds `site` to focus if it isn't, removes it if it is. Returns the new state.
    pub fn toggle_focus(&self, site: &str, label: &str) -> bool {
        if self.is_focused(site) {
            self.remove_focus(site);
            false
        } else {
            self.add_focus(site, label);
            true
        }
    }

    pub fn add_watch(&self, w: &NewWatch) -> Result<i64, String> {
        if self.watches().len() >= MAX_WATCHES {
            return Err(format!("There are already {MAX_WATCHES} watches; remove some first."));
        }
        let c = self.conn();
        c.execute(
            "INSERT INTO watches (url, label, kind, selector, needle, notify_when, every_min, created)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![w.url, w.label, w.kind, w.selector, w.needle, w.when, w.every_min, Local::now().timestamp()],
        )
        .map_err(|e| format!("couldn't save the watch: {e}"))?;
        Ok(c.last_insert_rowid())
    }

    pub fn remove_watch(&self, id: i64) -> bool {
        self.conn().execute("DELETE FROM watches WHERE id = ?1", params![id]).map(|n| n > 0).unwrap_or(false)
    }

    pub fn watches(&self) -> Vec<Watch> {
        self.query_watches("SELECT id, url, label, kind, selector, needle, notify_when, every_min, last_value,
                                   last_checked, last_changed, last_error, created FROM watches ORDER BY id", [])
    }

    /// Watches whose next check is due at `now` (Unix seconds).
    pub fn due_watches(&self, now: i64) -> Vec<Watch> {
        self.query_watches(
            "SELECT id, url, label, kind, selector, needle, notify_when, every_min, last_value,
                    last_checked, last_changed, last_error, created FROM watches
             WHERE last_checked IS NULL OR last_checked + every_min * 60 <= ?1 ORDER BY id",
            [now],
        )
    }

    fn query_watches<P: rusqlite::Params>(&self, sql: &str, p: P) -> Vec<Watch> {
        let c = self.conn();
        let Ok(mut stmt) = c.prepare(sql) else { return Vec::new() };
        stmt.query_map(p, |r| {
            Ok(Watch {
                id: r.get(0)?,
                url: r.get(1)?,
                label: r.get(2)?,
                kind: r.get(3)?,
                selector: r.get(4)?,
                needle: r.get(5)?,
                when: r.get(6)?,
                every_min: r.get(7)?,
                last_value: r.get(8)?,
                last_checked: r.get::<_, Option<i64>>(9)?.map(when),
                last_changed: r.get::<_, Option<i64>>(10)?.map(when),
                last_error: r.get(11)?,
                created: when(r.get(12)?),
            })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    /// Records a check. Returns the change when it's one the watch waits
    /// for. The first value seen is the baseline and never fires.
    pub fn record_watch_check(&self, id: i64, result: Result<String, String>) -> Option<WatchFired> {
        let now = Local::now().timestamp();
        let watch = self.watches().into_iter().find(|w| w.id == id)?;
        let c = self.conn();
        match result {
            Ok(value) => {
                let changed = watch.last_value.as_deref().is_some_and(|old| old != value);
                let _ = c.execute(
                    "UPDATE watches SET last_value = ?2, last_checked = ?3, last_error = NULL,
                         last_changed = CASE WHEN ?4 THEN ?3 ELSE last_changed END WHERE id = ?1",
                    params![id, value, now, changed],
                );
                let old = watch.last_value.clone()?;
                watch_fires(&watch.when, &old, &value).then(|| WatchFired { watch, old, new: value })
            }
            Err(e) => {
                let _ = c.execute(
                    "UPDATE watches SET last_checked = ?2, last_error = ?3 WHERE id = ?1",
                    params![id, now, e],
                );
                None
            }
        }
    }

    /// Makes a watch due now (the agent asked for a fresh check).
    pub fn check_watch_soon(&self, id: i64) {
        let _ = self.conn().execute("UPDATE watches SET last_checked = NULL WHERE id = ?1", params![id]);
    }

    /// One line naming the focus sites, for the context sent with each message.
    pub fn focus_summary(&self) -> Option<String> {
        let sites = self.focus_sites();
        let counts = self.playbook_counts();
        (!sites.is_empty()).then(|| {
            sites
                .iter()
                .map(|f| match counts.get(&f.site) {
                    Some(n) => format!("{} ({n} playbook{})", f.site, if *n == 1 { "" } else { "s" }),
                    None => f.site.clone(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sites() {
        assert_eq!(site_of("https://www.Amazon.com/dp/X").as_deref(), Some("amazon.com"));
        assert_eq!(site_of("http://localhost:3000/").as_deref(), Some("localhost"));
        assert_eq!(site_of("about:blank"), None);
        assert_eq!(site_of("data:text/html,hi"), None);
    }

    #[test]
    fn visits_merge_and_search() {
        let b = Browsing::in_memory();
        assert!(b.record_visit(1, "https://www.sweetwater.com/item", "", false));
        b.update_title("https://www.sweetwater.com/item", "TUSQ nut | Sweetwater");
        assert!(b.record_visit(1, "https://www.sweetwater.com/item", "", false)); // reload: merged
        assert!(b.record_visit(1, "https://duckduckgo.com/?q=bass", "bass at DuckDuckGo", false));
        assert!(!b.record_visit(1, "about:blank", "", false));
        let all = b.history(&HistoryQuery { limit: 50, ..Default::default() });
        assert_eq!(all.len(), 2);
        let tusq = b.history(&HistoryQuery { text: Some("tusq".into()), limit: 50, ..Default::default() });
        assert_eq!(tusq.len(), 1);
        assert_eq!(tusq[0].title, "TUSQ nut | Sweetwater");
        let site = b.history(&HistoryQuery { site: Some("www.sweetwater.com".into()), limit: 50, ..Default::default() });
        assert_eq!(site.len(), 1);
        // LIKE wildcards in the query are literal.
        assert!(b.history(&HistoryQuery { text: Some("%".into()), limit: 50, ..Default::default() }).is_empty());
    }

    #[test]
    fn redirects_on_same_site_merge() {
        let b = Browsing::in_memory();
        b.record_visit(1, "https://duckduckgo.com/?q=nut", "nut at DuckDuckGo", false);
        b.record_visit(1, "https://duckduckgo.com/?q=nut&ia=web", "nut at DuckDuckGo", true);
        let all = b.history(&HistoryQuery { limit: 10, ..Default::default() });
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].url, "https://duckduckgo.com/?q=nut&ia=web");
        // A different site right after is a new visit.
        b.record_visit(1, "https://sweetwater.com/x", "", true);
        assert_eq!(b.history(&HistoryQuery { limit: 10, ..Default::default() }).len(), 2);
    }

    #[test]
    fn tabs_on_one_site_keep_their_own_visits() {
        let b = Browsing::in_memory();
        b.record_visit(1, "https://amazon.com/s?k=nut", "Amazon.com : nut", false);
        b.record_visit(2, "https://amazon.com/dp/B0", "", true);
        let all = b.history(&HistoryQuery { limit: 10, ..Default::default() });
        assert_eq!(all.len(), 2, "{all:?}");
    }

    #[test]
    fn clicks_dont_replace_the_page_they_came_from() {
        let b = Browsing::in_memory();
        b.record_visit(1, "https://amazon.com/s?k=nut", "Amazon.com : nut", false);
        // A click on a result right away: the search page had loaded.
        b.record_visit(1, "https://amazon.com/dp/B0", "", false);
        assert_eq!(b.history(&HistoryQuery { limit: 10, ..Default::default() }).len(), 2);
        // A result 3 s later, even one the tab left before it loaded.
        {
            let c = b.conn();
            c.execute("UPDATE visits SET at = at - 3", []).unwrap();
        }
        b.record_visit(1, "https://amazon.com/dp/B1", "", true);
        let all = b.history(&HistoryQuery { limit: 10, ..Default::default() });
        assert_eq!(all.len(), 3, "{all:?}");
        assert!(all.iter().any(|v| v.url == "https://amazon.com/s?k=nut"));
    }

    #[test]
    fn fragments_and_round_trips_dont_duplicate() {
        let b = Browsing::in_memory();
        b.record_visit(1, "https://talk.example.io/", "Talk", false);
        {
            // Pretend that was a minute ago.
            let c = b.conn();
            c.execute("UPDATE visits SET at = at - 60", []).unwrap();
        }
        // In-page anchor: same page.
        b.record_visit(1, "https://talk.example.io/#a", "Talk", false);
        assert_eq!(b.history(&HistoryQuery { limit: 10, ..Default::default() }).len(), 1);
        // Path change and quick return.
        {
            let c = b.conn();
            c.execute("UPDATE visits SET at = at - 60", []).unwrap();
        }
        b.record_visit(1, "https://talk.example.io/x", "Talk", false);
        b.record_visit(1, "https://talk.example.io/", "Talk", true);
        let all = b.history(&HistoryQuery { limit: 10, ..Default::default() });
        assert_eq!(all.len(), 1, "{all:?}");
        assert_eq!(all[0].url, "https://talk.example.io/");
    }

    #[test]
    fn bookmarks() {
        let b = Browsing::in_memory();
        assert!(b.toggle_bookmark("https://www.sweetwater.com/item#reviews", "TUSQ nut"));
        assert!(b.is_bookmarked("https://www.sweetwater.com/item"));
        assert!(!b.add_bookmark("about:blank", "blank"));
        b.add_bookmark("https://bradwoods.io/", "Brad Woods");
        let all = b.bookmarks();
        assert_eq!(all.len(), 2);
        assert_eq!(all.iter().find(|m| m.site == "sweetwater.com").unwrap().url, "https://www.sweetwater.com/item");
        assert!(!b.toggle_bookmark("https://www.sweetwater.com/item", ""));
        assert_eq!(b.bookmarks().len(), 1);
    }

    #[test]
    fn prices_parse() {
        assert_eq!(parse_price("$1,299.00"), Some(1299.0));
        assert_eq!(parse_price("USD 13.98"), Some(13.98));
        assert_eq!(parse_price("13,98 €"), Some(13.98));
        assert_eq!(parse_price("1.299,50 €"), Some(1299.5));
        assert_eq!(parse_price("£1,299"), Some(1299.0));
        assert_eq!(parse_price("free"), None);
    }

    #[test]
    fn watches_fire_on_the_right_change() {
        let b = Browsing::in_memory();
        let id = b
            .add_watch(&NewWatch {
                url: "https://x.test/nut".into(),
                label: "TUSQ nut".into(),
                kind: "price".into(),
                when: "drops".into(),
                every_min: 60,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(b.due_watches(Local::now().timestamp()).len(), 1);
        // First value is the baseline.
        assert!(b.record_watch_check(id, Ok("$13.98".into())).is_none());
        assert!(b.due_watches(Local::now().timestamp()).is_empty());
        assert!(b.record_watch_check(id, Ok("$14.50".into())).is_none(), "a rise isn't a drop");
        let fired = b.record_watch_check(id, Ok("$11.00".into())).expect("drop fires");
        assert_eq!((fired.old.as_str(), fired.new.as_str()), ("$14.50", "$11.00"));
        assert!(b.record_watch_check(id, Err("offline".into())).is_none());
        let w = &b.watches()[0];
        assert_eq!(w.last_value.as_deref(), Some("$11.00"));
        assert_eq!(w.last_error.as_deref(), Some("offline"));
        assert!(b.remove_watch(id));
        assert!(b.watches().is_empty());
    }

    #[test]
    fn a_failing_watch_can_be_checked_soon() {
        let b = Browsing::in_memory();
        let id = b
            .add_watch(&NewWatch {
                url: "https://x.test/nut".into(),
                label: "TUSQ nut".into(),
                kind: "price".into(),
                when: "drops".into(),
                every_min: 60,
                ..Default::default()
            })
            .unwrap();
        // Every check so far failed: there's no value yet.
        assert!(b.record_watch_check(id, Err("offline".into())).is_none());
        assert!(b.due_watches(Local::now().timestamp()).is_empty());
        b.check_watch_soon(id);
        assert_eq!(b.due_watches(Local::now().timestamp()).len(), 1);
    }

    #[test]
    fn focus_toggle() {
        let b = Browsing::in_memory();
        assert!(b.toggle_focus("sweetwater.com", "Sweetwater"));
        assert!(b.is_focused("sweetwater.com"));
        assert_eq!(b.focus_summary().as_deref(), Some("sweetwater.com"));
        assert!(!b.toggle_focus("sweetwater.com", ""));
        assert!(b.focus_sites().is_empty());
    }
}
