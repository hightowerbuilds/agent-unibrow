//! Sites pinned to the home page (homepage roadmap H5.2), in the order the
//! user put them.

use crate::{Browsing, site_of};
use rusqlite::{Connection, params};

/// At most this many pins.
pub const MAX_PINS: usize = 24;

pub(crate) fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS home_pins (
             url TEXT PRIMARY KEY,
             title TEXT NOT NULL,
             site TEXT NOT NULL,
             position INTEGER NOT NULL
         );",
    )
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pin {
    pub url: String,
    pub title: String,
    pub site: String,
}

impl Browsing {
    /// Pins, in order.
    pub fn pins(&self) -> Vec<Pin> {
        let c = self.conn();
        let Ok(mut stmt) = c.prepare("SELECT url, title, site FROM home_pins ORDER BY position, url") else {
            return Vec::new();
        };
        stmt.query_map([], |r| Ok(Pin { url: r.get(0)?, title: r.get(1)?, site: r.get(2)? }))
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    /// Pins a web page at the end. Already pinned, or full: nothing changes.
    pub fn pin(&self, url: &str, title: &str) -> bool {
        let Some(site) = site_of(url) else { return false };
        if self.pins().len() >= MAX_PINS {
            return false;
        }
        let c = self.conn();
        let next: i64 = c.query_row("SELECT COALESCE(MAX(position), -1) + 1 FROM home_pins", [], |r| r.get(0)).unwrap_or(0);
        let title: String = title.chars().take(200).collect();
        c.execute(
            "INSERT OR IGNORE INTO home_pins (url, title, site, position) VALUES (?1, ?2, ?3, ?4)",
            params![url, title, site, next],
        )
        .map(|n| n > 0)
        .unwrap_or(false)
    }

    pub fn unpin(&self, url: &str) -> bool {
        self.conn().execute("DELETE FROM home_pins WHERE url = ?1", params![url]).map(|n| n > 0).unwrap_or(false)
    }

    /// Moves a pin one place earlier (`by` -1) or later (1).
    pub fn move_pin(&self, url: &str, by: i64) -> bool {
        let mut pins = self.pins();
        let Some(i) = pins.iter().position(|p| p.url == url) else { return false };
        let j = i as i64 + by;
        if j < 0 || j >= pins.len() as i64 {
            return false;
        }
        pins.swap(i, j as usize);
        let c = self.conn();
        for (n, p) in pins.iter().enumerate() {
            let _ = c.execute("UPDATE home_pins SET position = ?1 WHERE url = ?2", params![n as i64, p.url]);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use crate::Browsing;

    #[test]
    fn pins_keep_their_order() {
        let b = Browsing::in_memory();
        assert!(b.pin("https://a.com/", "A"));
        assert!(b.pin("https://b.com/x", "B"));
        assert!(!b.pin("https://a.com/", "A again"));
        assert!(!b.pin("unibrow://home/", "Home"));
        assert!(b.move_pin("https://b.com/x", -1));
        let urls: Vec<String> = b.pins().into_iter().map(|p| p.url).collect();
        assert_eq!(urls, ["https://b.com/x", "https://a.com/"]);
        assert!(!b.move_pin("https://b.com/x", -1));
        assert!(b.unpin("https://b.com/x"));
        assert_eq!(b.pins().len(), 1);
        assert_eq!(b.pins()[0].site, "a.com");
    }
}
