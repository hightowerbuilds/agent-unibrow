//! Browsing history and focus sites, shared by the UI and the agents.
//!
//! - **History**: every page the embedded browser shows (time, URL, title,
//!   site), searchable by text, site and date.
//! - **Focus sites**: sites the user has singled out. Agents see this list
//!   with every message and can look it up with a tool. It is the scope later
//!   decision models (Jev) and long-context models (SubQ) work within.
//!
//! Stored in SQLite at `~/.local/share/agent-unibrow/browsing.sqlite3`.
//! The store is `Send + Sync`: the UI writes visits, agent tools read.

mod tools;

pub use tools::tools;

use chrono::{DateTime, Local, TimeZone};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::PathBuf;
use std::sync::Mutex;

/// Revisiting the same URL within this window updates the existing entry
/// instead of adding a new one (reloads, redirects, title arriving late).
const MERGE_WINDOW_SECS: i64 = 30 * 60;
/// A URL change on the same site this soon after the previous visit is a
/// redirect or an in-page URL rewrite, not a new visit.
const REDIRECT_WINDOW_SECS: i64 = 5;

#[derive(Clone, Debug)]
pub struct Visit {
    pub id: i64,
    pub url: String,
    pub title: String,
    pub site: String,
    pub at: DateTime<Local>,
}

#[derive(Clone, Debug)]
pub struct FocusSite {
    pub site: String,
    /// A readable name (the page title when it was added, or the user's).
    pub label: String,
    pub added: DateTime<Local>,
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
        dirs::data_local_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("agent-unibrow")
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
             CREATE TABLE IF NOT EXISTS focus_sites (
                 site TEXT PRIMARY KEY,
                 label TEXT NOT NULL DEFAULT '',
                 added INTEGER NOT NULL
             );",
        )
        .map_err(|e| format!("can't set up the history database: {e}"))?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("browsing db lock")
    }

    /// Records that `url` is showing. Returns false for pages that aren't
    /// kept (internal pages).
    pub fn record_visit(&self, url: &str, title: &str) -> bool {
        let Some(site) = site_of(url) else { return false };
        let url = history_url(url);
        let url = url.as_str();
        let now = Local::now().timestamp();
        let c = self.conn();
        let latest: Option<(i64, String, i64)> = c
            .query_row("SELECT id, site, at FROM visits ORDER BY at DESC, id DESC LIMIT 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()
            .unwrap_or(None);
        if let Some((id, last_site, at)) = latest {
            if last_site == site && now - at <= REDIRECT_WINDOW_SECS {
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
        sql.push_str(" ORDER BY at DESC LIMIT ?");
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

    /// One line naming the focus sites, for the context sent with each message.
    pub fn focus_summary(&self) -> Option<String> {
        let sites = self.focus_sites();
        (!sites.is_empty()).then(|| sites.iter().map(|f| f.site.as_str()).collect::<Vec<_>>().join(", "))
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
        assert!(b.record_visit("https://www.sweetwater.com/item", ""));
        b.update_title("https://www.sweetwater.com/item", "TUSQ nut | Sweetwater");
        assert!(b.record_visit("https://www.sweetwater.com/item", "")); // reload: merged
        assert!(b.record_visit("https://duckduckgo.com/?q=bass", "bass at DuckDuckGo"));
        assert!(!b.record_visit("about:blank", ""));
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
    fn rewrites_on_same_site_merge() {
        let b = Browsing::in_memory();
        b.record_visit("https://duckduckgo.com/?q=nut", "nut at DuckDuckGo");
        b.record_visit("https://duckduckgo.com/?q=nut&ia=web", "nut at DuckDuckGo");
        let all = b.history(&HistoryQuery { limit: 10, ..Default::default() });
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].url, "https://duckduckgo.com/?q=nut&ia=web");
        // A different site right after is a new visit.
        b.record_visit("https://sweetwater.com/x", "");
        assert_eq!(b.history(&HistoryQuery { limit: 10, ..Default::default() }).len(), 2);
    }

    #[test]
    fn fragments_and_round_trips_dont_duplicate() {
        let b = Browsing::in_memory();
        b.record_visit("https://talk.example.io/", "Talk");
        {
            // Pretend that was a minute ago.
            let c = b.conn();
            c.execute("UPDATE visits SET at = at - 60", []).unwrap();
        }
        // In-page anchor: same page.
        b.record_visit("https://talk.example.io/#a", "Talk");
        assert_eq!(b.history(&HistoryQuery { limit: 10, ..Default::default() }).len(), 1);
        // Path change and quick return.
        {
            let c = b.conn();
            c.execute("UPDATE visits SET at = at - 60", []).unwrap();
        }
        b.record_visit("https://talk.example.io/x", "Talk");
        b.record_visit("https://talk.example.io/", "Talk");
        let all = b.history(&HistoryQuery { limit: 10, ..Default::default() });
        assert_eq!(all.len(), 1, "{all:?}");
        assert_eq!(all[0].url, "https://talk.example.io/");
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
