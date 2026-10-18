//! A small icon per site, for the History and Bookmarks lists.
//!
//! The browser pane saves each site's favicon here as it gets one (BGRA
//! pixels, at most [`MAX_SIDE`] px a side, so a few KB per site). Lists look
//! them up by site; [`Browsing::favicon_version`] changes whenever one is
//! saved, so a UI cache knows when to look again.

use crate::{Browsing, site_of};
use chrono::Local;
use rusqlite::{Connection, OptionalExtension, params};
use std::sync::atomic::Ordering;

/// Largest icon side kept; bigger ones are scaled down.
pub const MAX_SIDE: u32 = 32;

/// A site's icon: straight-alpha BGRA rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Favicon {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

pub(crate) fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS favicons (
             site TEXT PRIMARY KEY,
             width INTEGER NOT NULL,
             height INTEGER NOT NULL,
             bgra BLOB NOT NULL,
             updated INTEGER NOT NULL
         );",
    )
}

/// `icon` scaled down (box filter) to fit in `MAX_SIDE`, or as it is.
pub fn shrink(icon: Favicon) -> Option<Favicon> {
    let (w, h) = (icon.width, icon.height);
    if w == 0 || h == 0 || icon.bgra.len() != (w * h * 4) as usize {
        return None;
    }
    if w <= MAX_SIDE && h <= MAX_SIDE {
        return Some(icon);
    }
    let scale = (w.max(h) as f64 / MAX_SIDE as f64).ceil() as u32;
    let (nw, nh) = (w.div_ceil(scale), h.div_ceil(scale));
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    for y in 0..nh {
        for x in 0..nw {
            let mut sum = [0u32; 4];
            let mut n = 0;
            for sy in y * scale..((y + 1) * scale).min(h) {
                for sx in x * scale..((x + 1) * scale).min(w) {
                    let i = ((sy * w + sx) * 4) as usize;
                    for c in 0..4 {
                        sum[c] += icon.bgra[i + c] as u32;
                    }
                    n += 1;
                }
            }
            let o = ((y * nw + x) * 4) as usize;
            for c in 0..4 {
                out[o + c] = (sum[c] / n.max(1)) as u8;
            }
        }
    }
    Some(Favicon { width: nw, height: nh, bgra: out })
}

impl Browsing {
    /// Saves the icon of the site `url` is on. Returns whether anything
    /// changed (a new site, or a different icon).
    pub fn save_favicon(&self, url: &str, icon: Favicon) -> bool {
        let (Some(site), Some(icon)) = (site_of(url), shrink(icon)) else { return false };
        let changed = self
            .conn()
            .execute(
                "INSERT INTO favicons (site, width, height, bgra, updated) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(site) DO UPDATE SET width = excluded.width, height = excluded.height,
                     bgra = excluded.bgra, updated = excluded.updated
                 WHERE favicons.bgra != excluded.bgra OR favicons.width != excluded.width",
                params![site, icon.width, icon.height, icon.bgra, Local::now().timestamp()],
            )
            .unwrap_or(0)
            > 0;
        if changed {
            self.favicon_version.fetch_add(1, Ordering::Relaxed);
        }
        changed
    }

    /// The icon saved for `site` (as [`site_of`] names it), if any.
    pub fn favicon(&self, site: &str) -> Option<Favicon> {
        self.conn()
            .query_row("SELECT width, height, bgra FROM favicons WHERE site = ?1", params![site], |r| {
                Ok(Favicon { width: r.get(0)?, height: r.get(1)?, bgra: r.get(2)? })
            })
            .optional()
            .ok()
            .flatten()
    }

    /// Changes whenever an icon is saved or removed.
    pub fn favicon_version(&self) -> u64 {
        self.favicon_version.load(Ordering::Relaxed)
    }

    /// Forgets the icons of sites that are neither bookmarked nor in focus
    /// (after the history is cleared).
    pub(crate) fn prune_favicons(&self) {
        let n = self
            .conn()
            .execute(
                "DELETE FROM favicons WHERE site NOT IN (SELECT site FROM bookmarks)
                     AND site NOT IN (SELECT site FROM focus_sites)",
                [],
            )
            .unwrap_or(0);
        if n > 0 {
            self.favicon_version.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, px: [u8; 4]) -> Favicon {
        Favicon { width: w, height: h, bgra: px.repeat((w * h) as usize) }
    }

    #[test]
    fn saves_and_finds_by_site() {
        let s = Browsing::in_memory();
        let v0 = s.favicon_version();
        assert!(s.save_favicon("https://www.example.com/a", solid(16, 16, [1, 2, 3, 255])));
        assert_eq!(s.favicon("example.com"), Some(solid(16, 16, [1, 2, 3, 255])));
        assert!(s.favicon_version() > v0);
        // The same icon again changes nothing.
        let v1 = s.favicon_version();
        assert!(!s.save_favicon("https://example.com/b", solid(16, 16, [1, 2, 3, 255])));
        assert_eq!(s.favicon_version(), v1);
        // A new one replaces it.
        assert!(s.save_favicon("https://example.com/", solid(16, 16, [9, 9, 9, 255])));
        assert_eq!(s.favicon("example.com").unwrap().bgra[0], 9);
        // Internal pages and broken icons aren't kept.
        assert!(!s.save_favicon("about:blank", solid(16, 16, [0; 4])));
        assert!(!s.save_favicon("https://x.org/", Favicon { width: 4, height: 4, bgra: vec![0; 3] }));
        assert!(s.favicon("nowhere.org").is_none());
    }

    #[test]
    fn big_icons_shrink() {
        let icon = shrink(solid(64, 48, [10, 20, 30, 255])).unwrap();
        assert_eq!((icon.width, icon.height), (32, 24));
        assert_eq!(&icon.bgra[..4], &[10, 20, 30, 255]);
        let odd = shrink(solid(100, 100, [0, 0, 0, 255])).unwrap();
        assert!(odd.width <= MAX_SIDE && odd.height <= MAX_SIDE);
        assert_eq!(odd.bgra.len(), (odd.width * odd.height * 4) as usize);
    }

    #[test]
    fn clearing_history_keeps_bookmarked_icons() {
        let s = Browsing::in_memory();
        s.record_visit("https://kept.com/", "Kept");
        s.record_visit("https://gone.com/", "Gone");
        s.add_bookmark("https://kept.com/", "Kept");
        s.save_favicon("https://kept.com/", solid(16, 16, [1, 1, 1, 255]));
        s.save_favicon("https://gone.com/", solid(16, 16, [2, 2, 2, 255]));
        s.clear_history();
        assert!(s.favicon("kept.com").is_some());
        assert!(s.favicon("gone.com").is_none());
    }
}
