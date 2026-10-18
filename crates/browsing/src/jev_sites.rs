//! Jev sites: focus sites that Jev, TypeSafe's decision model, has learned,
//! so requests about them can be handled without Claude.
//!
//! Learning (done by the browser rail in hidden pages) finds the site's
//! search address, what its search results and product pages look like, and
//! its cart. What it found is kept here, with a log of how it found it.

use crate::Browsing;
use chrono::Local;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

pub(crate) fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS jev_sites (
             site TEXT PRIMARY KEY,
             status TEXT NOT NULL,
             skills TEXT NOT NULL,
             log TEXT NOT NULL,
             updated INTEGER NOT NULL,
             enabled INTEGER NOT NULL DEFAULT 1
         );",
    )
}

/// What Jev learned about a site.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct JevSkills {
    /// The address learning started from.
    pub home: String,
    /// The search address with `{q}` where the words go.
    #[serde(default)]
    pub search_url: Option<String>,
    /// The URL pattern of search results (`/{slug}/dp/{id}`).
    #[serde(default)]
    pub result_pattern: Option<String>,
    /// The URL pattern of product pages, usually the same as results.
    #[serde(default)]
    pub product_pattern: Option<String>,
    /// Whether product pages give a price and stock in a machine-readable form.
    #[serde(default)]
    pub product_data: bool,
    #[serde(default)]
    pub cart_url: Option<String>,
    /// The search used while learning.
    #[serde(default)]
    pub sample_query: String,
}

impl JevSkills {
    /// The request types this site can handle without Claude.
    pub fn abilities(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.search_url.is_some() {
            out.extend(["find", "open", "price", "stock"]);
        }
        if self.cart_url.is_some() {
            out.push("cart");
        }
        out
    }

    /// The search address for `query`.
    pub fn search_for(&self, query: &str) -> Option<String> {
        let t = self.search_url.as_ref()?;
        let q: String = url::form_urlencoded::byte_serialize(query.trim().as_bytes()).collect();
        Some(t.replace("{q}", &q))
    }
}

/// `learning`, `ready` (some or all skills learned) or `failed`.
#[derive(Clone, Debug, PartialEq)]
pub struct JevSite {
    pub site: String,
    pub status: String,
    pub skills: JevSkills,
    /// How learning went, step by step, for the Focus tab.
    pub log: Vec<String>,
    pub updated: i64,
    /// Requests about the site go to Jev. Off keeps what was learned.
    pub enabled: bool,
}

impl JevSite {
    pub fn ready(&self) -> bool {
        self.status == "ready" && self.enabled
    }
}

fn norm(site: &str) -> String {
    site.trim_start_matches("www.").to_ascii_lowercase()
}

impl Browsing {
    pub fn jev_sites(&self) -> Vec<JevSite> {
        let c = self.conn();
        let Ok(mut stmt) = c.prepare("SELECT site, status, skills, log, updated, enabled FROM jev_sites ORDER BY site") else {
            return Vec::new();
        };
        stmt.query_map([], |r| {
            let skills: String = r.get(2)?;
            let log: String = r.get(3)?;
            Ok(JevSite {
                site: r.get(0)?,
                status: r.get(1)?,
                skills: serde_json::from_str(&skills).unwrap_or_default(),
                log: serde_json::from_str(&log).unwrap_or_default(),
                updated: r.get(4)?,
                enabled: r.get::<_, i64>(5)? != 0,
            })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    pub fn jev_site(&self, site: &str) -> Option<JevSite> {
        let site = norm(site);
        self.jev_sites().into_iter().find(|s| s.site == site)
    }

    /// Saves a site's status, skills and log (keeps whether it's enabled).
    pub fn save_jev_site(&self, site: &str, status: &str, skills: &JevSkills, log: &[String]) {
        let _ = self.conn().execute(
            "INSERT INTO jev_sites (site, status, skills, log, updated) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(site) DO UPDATE SET status = excluded.status, skills = excluded.skills,
                 log = excluded.log, updated = excluded.updated",
            params![
                norm(site),
                status,
                serde_json::to_string(skills).unwrap_or_default(),
                serde_json::to_string(log).unwrap_or_default(),
                Local::now().timestamp()
            ],
        );
    }

    pub fn set_jev_enabled(&self, site: &str, enabled: bool) {
        let _ = self.conn().execute("UPDATE jev_sites SET enabled = ?2 WHERE site = ?1", params![norm(site), enabled as i64]);
    }

    pub fn remove_jev_site(&self, site: &str) {
        let _ = self.conn().execute("DELETE FROM jev_sites WHERE site = ?1", params![norm(site)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_reads_back() {
        let b = Browsing::in_memory();
        let skills = JevSkills {
            home: "https://www.sweetwater.com/".into(),
            search_url: Some("https://www.sweetwater.com/store/search?s={q}".into()),
            result_pattern: Some("/store/detail/{slug}".into()),
            ..Default::default()
        };
        b.save_jev_site("www.Sweetwater.com", "ready", &skills, &["found search".into()]);
        let s = b.jev_site("sweetwater.com").unwrap();
        assert!(s.ready());
        assert_eq!(s.skills, skills);
        assert_eq!(s.log, vec!["found search".to_string()]);
        assert_eq!(s.skills.abilities(), vec!["find", "open", "price", "stock"]);
        assert_eq!(s.skills.search_for("tusq nut").unwrap(), "https://www.sweetwater.com/store/search?s=tusq+nut");
        b.set_jev_enabled("sweetwater.com", false);
        assert!(!b.jev_site("sweetwater.com").unwrap().ready());
        b.remove_jev_site("sweetwater.com");
        assert!(b.jev_site("sweetwater.com").is_none());
    }
}
