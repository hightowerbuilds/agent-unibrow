//! Playbook versions (the change log's step 2): saving a playbook under a
//! name it already has keeps the old steps as an earlier version, removing
//! one keeps its last version, and each replay records which version ran.
//! Any version can be put back ([`Browsing::restore_playbook`]), which saves
//! it as the newest.

use crate::sitemap::Playbook;
use crate::{Browsing, sitemap};
use chrono::{DateTime, Local, TimeZone};
use rusqlite::{Connection, OptionalExtension, params};

pub(crate) fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS playbook_versions (
             id INTEGER PRIMARY KEY,
             site TEXT NOT NULL,
             name TEXT NOT NULL,
             version INTEGER NOT NULL,
             kind TEXT NOT NULL,
             from_version INTEGER,
             description TEXT NOT NULL,
             params TEXT NOT NULL,
             steps TEXT NOT NULL,
             at INTEGER NOT NULL,
             UNIQUE(site, name, version)
         );
         CREATE TABLE IF NOT EXISTS playbook_runs (
             id INTEGER PRIMARY KEY,
             site TEXT NOT NULL,
             name TEXT NOT NULL,
             version INTEGER NOT NULL,
             ok INTEGER NOT NULL,
             error TEXT,
             at INTEGER NOT NULL
         );",
    )?;
    // Older databases: the playbook's current version, and a first version
    // for each playbook saved before versions were kept.
    let _ = conn.execute("ALTER TABLE playbooks ADD COLUMN version INTEGER NOT NULL DEFAULT 1", []);
    conn.execute(
        "INSERT OR IGNORE INTO playbook_versions (site, name, version, kind, description, params, steps, at)
         SELECT site, name, version, 'saved', description, params, steps, created FROM playbooks",
        [],
    )?;
    Ok(())
}

/// How a version came about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionKind {
    Saved,
    /// An earlier version put back (`from_version`).
    Restored,
    /// The playbook was removed; this is what it was.
    Removed,
}

impl VersionKind {
    fn key(self) -> &'static str {
        match self {
            VersionKind::Saved => "saved",
            VersionKind::Restored => "restored",
            VersionKind::Removed => "removed",
        }
    }

    fn from_key(k: &str) -> Self {
        match k {
            "restored" => VersionKind::Restored,
            "removed" => VersionKind::Removed,
            _ => VersionKind::Saved,
        }
    }
}

/// One version of a playbook.
#[derive(Clone, Debug)]
pub struct PlaybookVersion {
    pub site: String,
    pub name: String,
    pub version: i64,
    pub kind: VersionKind,
    pub from_version: Option<i64>,
    pub description: String,
    pub params: Vec<String>,
    pub steps: Vec<sitemap::Step>,
    pub at: DateTime<Local>,
}

fn when(ts: i64) -> DateTime<Local> {
    Local.timestamp_opt(ts, 0).single().unwrap_or_else(Local::now)
}

fn version_row(r: &rusqlite::Row) -> rusqlite::Result<PlaybookVersion> {
    let kind: String = r.get(3)?;
    let params: String = r.get(6)?;
    let steps: String = r.get(7)?;
    Ok(PlaybookVersion {
        site: r.get(0)?,
        name: r.get(1)?,
        version: r.get(2)?,
        kind: VersionKind::from_key(&kind),
        from_version: r.get(4)?,
        description: r.get(5)?,
        params: serde_json::from_str(&params).unwrap_or_default(),
        steps: serde_json::from_str(&steps).unwrap_or_default(),
        at: when(r.get(8)?),
    })
}

const VERSION_COLUMNS: &str = "site, name, version, kind, from_version, description, params, steps, at";

/// The number the next version of `site`/`name` gets.
pub(crate) fn next_version(conn: &Connection, site: &str, name: &str) -> i64 {
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) + 1 FROM playbook_versions WHERE site = ?1 AND name = ?2 COLLATE NOCASE",
        params![site, name],
        |r| r.get(0),
    )
    .unwrap_or(1)
}

/// Keeps a version.
#[allow(clippy::too_many_arguments)]
pub(crate) fn add_version(
    conn: &Connection,
    site: &str,
    name: &str,
    version: i64,
    kind: VersionKind,
    from_version: Option<i64>,
    description: &str,
    params_json: &str,
    steps_json: &str,
) {
    let _ = conn.execute(
        "INSERT OR REPLACE INTO playbook_versions (site, name, version, kind, from_version, description, params, steps, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![site, name, version, kind.key(), from_version, description, params_json, steps_json, Local::now().timestamp()],
    );
}

impl Browsing {
    /// Every version of a site's playbook, newest first.
    pub fn playbook_versions(&self, site: &str, name: &str) -> Vec<PlaybookVersion> {
        let site = site.trim_start_matches("www.").to_ascii_lowercase();
        let c = self.conn();
        let sql = format!(
            "SELECT {VERSION_COLUMNS} FROM playbook_versions WHERE site = ?1 AND name = ?2 COLLATE NOCASE ORDER BY version DESC"
        );
        let Ok(mut stmt) = c.prepare(&sql) else { return Vec::new() };
        stmt.query_map(params![site, name], version_row).map(|rows| rows.filter_map(Result::ok).collect()).unwrap_or_default()
    }

    /// A site's removed playbooks: the last version of each name that has
    /// no playbook now.
    pub fn removed_playbooks(&self, site: &str) -> Vec<PlaybookVersion> {
        let site = site.trim_start_matches("www.").to_ascii_lowercase();
        let c = self.conn();
        let sql = format!(
            "SELECT {VERSION_COLUMNS} FROM playbook_versions v
             WHERE site = ?1 AND kind = 'removed'
               AND version = (SELECT MAX(version) FROM playbook_versions w WHERE w.site = v.site AND w.name = v.name)
               AND NOT EXISTS (SELECT 1 FROM playbooks p WHERE p.site = v.site AND p.name = v.name COLLATE NOCASE)
             ORDER BY at DESC"
        );
        let Ok(mut stmt) = c.prepare(&sql) else { return Vec::new() };
        stmt.query_map(params![site], version_row).map(|rows| rows.filter_map(Result::ok).collect()).unwrap_or_default()
    }

    /// Puts version `version` of a playbook back, as its newest version
    /// (the playbook comes back if it was removed). Its run record starts
    /// over.
    pub fn restore_playbook(&self, site: &str, name: &str, version: i64) -> Result<Playbook, String> {
        let site = site.trim_start_matches("www.").to_ascii_lowercase();
        {
            let c = self.conn();
            let old = c
                .query_row(
                    &format!("SELECT {VERSION_COLUMNS} FROM playbook_versions WHERE site = ?1 AND name = ?2 COLLATE NOCASE AND version = ?3"),
                    params![site, name, version],
                    version_row,
                )
                .optional()
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("“{name}” has no version {version}."))?;
            let params_json = serde_json::to_string(&old.params).map_err(|e| e.to_string())?;
            let steps_json = serde_json::to_string(&old.steps).map_err(|e| e.to_string())?;
            let next = next_version(&c, &site, &old.name);
            c.execute(
                "INSERT INTO playbooks (site, name, description, params, steps, created, version) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(site, name) DO UPDATE SET description = excluded.description, params = excluded.params,
                     steps = excluded.steps, version = excluded.version, runs = 0, successes = 0, last_run = NULL, last_error = NULL",
                params![site, old.name, old.description, params_json, steps_json, Local::now().timestamp(), next],
            )
            .map_err(|e| format!("couldn't restore the playbook: {e}"))?;
            add_version(&c, &site, &old.name, next, VersionKind::Restored, Some(version), &old.description, &params_json, &steps_json);
        }
        self.playbook(&site, name).ok_or_else(|| "The playbook wasn't restored.".to_string())
    }

    /// Replays of a playbook, newest first: (version, finished, why it
    /// stopped, when).
    pub fn playbook_runs(&self, site: &str, name: &str, limit: usize) -> Vec<(i64, bool, Option<String>, DateTime<Local>)> {
        let site = site.trim_start_matches("www.").to_ascii_lowercase();
        let c = self.conn();
        let Ok(mut stmt) = c.prepare(
            "SELECT version, ok, error, at FROM playbook_runs WHERE site = ?1 AND name = ?2 COLLATE NOCASE ORDER BY id DESC LIMIT ?3",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![site, name, limit as i64], |r| Ok((r.get(0)?, r.get::<_, bool>(1)?, r.get(2)?, when(r.get(3)?))))
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sitemap::Step;

    fn click(name: &str) -> Step {
        Step {
            action: "click".into(),
            target: Some(sitemap::Target { role: "link".into(), name: name.into(), ..Default::default() }),
            ..Default::default()
        }
    }

    /// Saves `name` on shop.example from one recorded click on `link`.
    fn save(b: &Browsing, name: &str, link: &str) -> Playbook {
        b.record_action("https://shop.example/", &click(link), "https://shop.example/item/1");
        let run = b.latest_run("shop.example");
        b.save_playbook("shop.example", name, link, &run[run.len() - 1..], &[]).unwrap()
    }

    #[test]
    fn saving_again_keeps_the_old_version() {
        let b = Browsing::in_memory();
        b.add_focus("shop.example", "Shop");
        assert_eq!(save(&b, "buy", "First way").version, 1);
        let pb = save(&b, "buy", "Second way");
        assert_eq!(pb.version, 2);
        let versions = b.playbook_versions("shop.example", "BUY");
        assert_eq!(versions.iter().map(|v| (v.version, v.description.as_str())).collect::<Vec<_>>(), [(2, "Second way"), (1, "First way")]);

        // Each run records the version that ran.
        b.record_playbook_run(pb.id, Ok(()));
        b.record_playbook_run(pb.id, Err("the page changed".into()));
        let runs = b.playbook_runs("shop.example", "buy", 10);
        assert_eq!(runs.iter().map(|r| (r.0, r.1)).collect::<Vec<_>>(), [(2, false), (2, true)]);

        // Restoring v1 makes it v3, with the old steps and a fresh record.
        let back = b.restore_playbook("shop.example", "buy", 1).unwrap();
        assert_eq!((back.version, back.description.as_str(), back.runs), (3, "First way", 0));
        let v3 = &b.playbook_versions("shop.example", "buy")[0];
        assert_eq!((v3.kind, v3.from_version), (VersionKind::Restored, Some(1)));
        assert!(b.restore_playbook("shop.example", "buy", 9).is_err());
    }

    #[test]
    fn removed_playbooks_can_come_back() {
        let b = Browsing::in_memory();
        b.add_focus("shop.example", "Shop");
        let pb = save(&b, "buy", "First way");
        assert!(b.remove_playbook(pb.id));
        assert!(b.playbook("shop.example", "buy").is_none());
        let gone = b.removed_playbooks("shop.example");
        assert_eq!(gone.iter().map(|v| (v.name.as_str(), v.version, v.kind)).collect::<Vec<_>>(), [("buy", 2, VersionKind::Removed)]);
        let back = b.restore_playbook("shop.example", "buy", 2).unwrap();
        assert_eq!((back.version, back.description.as_str()), (3, "First way"));
        assert!(b.removed_playbooks("shop.example").is_empty());
        // Saving the name anew after a removal carries on the numbering.
        assert!(b.remove_playbook(back.id));
        assert_eq!(save(&b, "buy", "Third way").version, 5);
    }

    #[test]
    fn playbooks_from_before_versions_get_a_first_one() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE playbooks (id INTEGER PRIMARY KEY, site TEXT NOT NULL, name TEXT NOT NULL,
                 description TEXT NOT NULL DEFAULT '', params TEXT NOT NULL, steps TEXT NOT NULL, created INTEGER NOT NULL,
                 runs INTEGER NOT NULL DEFAULT 0, successes INTEGER NOT NULL DEFAULT 0, last_run INTEGER, last_error TEXT,
                 UNIQUE(site, name));
             INSERT INTO playbooks (site, name, params, steps, created) VALUES ('shop.example', 'old', '[]', '[]', 1);",
        )
        .unwrap();
        init(&conn).unwrap();
        init(&conn).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM playbook_versions WHERE name = 'old' AND version = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }
}
