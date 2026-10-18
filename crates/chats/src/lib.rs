//! Saved chats (chats roadmap C1): every conversation, kept on disk as it
//! happens so a crash doesn't lose it.
//!
//! - A **chat** has a title, whether it's open in the chat strip, when it
//!   was last used, and the backend's **resume handle**: what that backend
//!   needs to pick the conversation back up (a Claude Code session id, a
//!   Codex thread id, an OpenCode session id, or the API message list).
//! - **Items** are what the chat shows: the user's messages, replies,
//!   notes, errors. Written the moment they appear; a reply being streamed
//!   is updated in place.
//!
//! - A **task plan** (7.4, [`progress`]) is the agent's checklist for a long
//!   task in the chat, so it can pick up where it left off.
//!
//! Stored in SQLite (WAL) at `~/.local/share/agent-unibrow/chats.sqlite3`.

use chrono::{DateTime, Local, TimeZone};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Mutex;

pub mod progress;
pub use progress::{TaskPlan, TaskProgress, TaskStep};

pub type ChatId = i64;
pub type ItemId = i64;

#[derive(Clone, Debug)]
pub struct Chat {
    pub id: ChatId,
    pub title: String,
    pub created: DateTime<Local>,
    pub updated: DateTime<Local>,
    /// Shown in the chat strip.
    pub open: bool,
    /// Which backend the resume handle belongs to (`claude_code`, …).
    pub backend: Option<String>,
    pub resume: Option<Value>,
    pub items: usize,
}

/// A chat matching a search, with where it matched.
#[derive(Clone, Debug)]
pub struct SearchHit {
    pub chat: Chat,
    /// A piece of the first message that matched, around the match; `None`
    /// when only the title did.
    pub snippet: Option<String>,
}

#[derive(Clone, Debug)]
pub struct StoredItem {
    pub id: ItemId,
    /// `user`, `assistant`, `note`, `error`, `system`, `steer`.
    pub kind: String,
    pub text: String,
    pub at: DateTime<Local>,
}

pub struct Chats {
    conn: Mutex<Connection>,
}

fn when(ts: i64) -> DateTime<Local> {
    Local.timestamp_opt(ts, 0).single().unwrap_or_else(Local::now)
}

fn now() -> i64 {
    Local::now().timestamp()
}

/// A title from the first message: its first line, cut at a word near 48
/// characters.
pub fn title_from(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("New chat");
    if line.chars().count() <= 48 {
        return line.to_string();
    }
    let cut: String = line.chars().take(48).collect();
    let cut = cut.rsplit_once(' ').map(|(a, _)| a).filter(|a| a.len() > 20).unwrap_or(&cut);
    format!("{}…", cut.trim_end_matches([',', '.', ';', ':']))
}

/// `text` as a `LIKE` pattern matching it anywhere, with `%`, `_` and the
/// escape character itself taken literally (`ESCAPE '\\'`).
fn like_pattern(text: &str) -> String {
    let mut out = String::from("%");
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// About `width` characters of `text` around the first case-insensitive
/// match of `needle`, on one line, with … where it was cut.
pub fn snippet(text: &str, needle: &str, width: usize) -> String {
    let flat: Vec<char> = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().collect();
    let lower: Vec<char> = flat.iter().flat_map(|c| c.to_lowercase()).collect();
    let n: Vec<char> = needle.trim().chars().flat_map(|c| c.to_lowercase()).collect();
    // Lowercasing can change lengths (rare); fall back to the start then.
    let at = if lower.len() == flat.len() && !n.is_empty() {
        lower.windows(n.len()).position(|w| w == n.as_slice()).unwrap_or(0)
    } else {
        0
    };
    let start = at.saturating_sub(width / 3);
    let end = (start + width).min(flat.len());
    let start = end.saturating_sub(width).min(start);
    let mut out: String = flat[start..end].iter().collect();
    if start > 0 {
        out.insert(0, '…');
    }
    if end < flat.len() {
        out.push('…');
    }
    out
}

/// What each kind of item is called in an export; `None`: left out
/// (app notices, not part of the conversation).
fn export_label(kind: &str) -> Option<&'static str> {
    match kind {
        "user" => Some("You"),
        "steer" | "steer_queued" | "steer_not_sent" => Some("You (while it worked)"),
        "assistant" => Some("Claude"),
        "error" => Some("Error"),
        _ => None,
    }
}

/// A file name for an exported chat: its title, made safe, as `.md`.
pub fn export_file_name(title: &str) -> String {
    let stem: String = title
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { '_' } else { c })
        .take(80)
        .collect();
    let stem = stem.trim().trim_matches('.').trim();
    format!("{}.md", if stem.is_empty() { "chat" } else { stem })
}

/// A free path in `dir` for `name` (`name (1).md`, … when taken).
pub fn free_path(dir: &std::path::Path, name: &str) -> PathBuf {
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    let ext = if ext.is_empty() { String::new() } else { format!(".{ext}") };
    (0..1000)
        .map(|n| dir.join(if n == 0 { format!("{stem}{ext}") } else { format!("{stem} ({n}){ext}") }))
        .find(|p| !p.exists())
        .unwrap_or_else(|| dir.join(name))
}

impl Chats {
    pub fn default_path() -> PathBuf {
        std::env::var_os("UNIBROW_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs::data_local_dir().unwrap_or_else(std::env::temp_dir).join("agent-unibrow"))
            .join("chats.sqlite3")
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
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS chats (
                 id INTEGER PRIMARY KEY,
                 title TEXT NOT NULL DEFAULT 'New chat',
                 created INTEGER NOT NULL,
                 updated INTEGER NOT NULL,
                 last_active INTEGER NOT NULL,
                 open INTEGER NOT NULL DEFAULT 1,
                 backend TEXT,
                 resume TEXT
             );
             CREATE TABLE IF NOT EXISTS items (
                 id INTEGER PRIMARY KEY,
                 chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
                 kind TEXT NOT NULL,
                 text TEXT NOT NULL,
                 at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS items_chat ON items(chat_id, id);
             CREATE TABLE IF NOT EXISTS task_plans (
                 chat_id INTEGER PRIMARY KEY REFERENCES chats(id) ON DELETE CASCADE,
                 goal TEXT NOT NULL DEFAULT '',
                 steps TEXT NOT NULL,
                 updated INTEGER NOT NULL
             );",
        )
        .map_err(|e| format!("can't set up the chat database: {e}"))?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn create(&self) -> ChatId {
        let c = self.conn();
        let t = now();
        let _ = c.execute("INSERT INTO chats (created, updated, last_active) VALUES (?1, ?1, ?1)", params![t]);
        c.last_insert_rowid()
    }

    fn query(&self, filter: &str) -> Vec<Chat> {
        let sql = format!(
            "SELECT c.id, c.title, c.created, c.updated, c.open, c.backend, c.resume,
                    (SELECT COUNT(*) FROM items i WHERE i.chat_id = c.id)
             FROM chats c {filter}"
        );
        let c = self.conn();
        let Ok(mut stmt) = c.prepare(&sql) else { return Vec::new() };
        stmt.query_map([], |r| {
            let resume: Option<String> = r.get(6)?;
            Ok(Chat {
                id: r.get(0)?,
                title: r.get(1)?,
                created: when(r.get(2)?),
                updated: when(r.get(3)?),
                open: r.get::<_, i64>(4)? != 0,
                backend: r.get(5)?,
                resume: resume.and_then(|s| serde_json::from_str(&s).ok()),
                items: r.get::<_, i64>(7)? as usize,
            })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    /// Every chat, most recently updated first.
    pub fn all(&self) -> Vec<Chat> {
        self.query("ORDER BY c.updated DESC, c.id DESC")
    }

    /// Chats in the strip, oldest first (the order they were opened in).
    pub fn open_chats(&self) -> Vec<Chat> {
        self.query("WHERE c.open = 1 ORDER BY c.id")
    }

    pub fn get(&self, id: ChatId) -> Option<Chat> {
        self.query(&format!("WHERE c.id = {id}")).into_iter().next()
    }

    /// The chat used most recently, if it's still open.
    pub fn last_active(&self) -> Option<ChatId> {
        self.conn()
            .query_row("SELECT id FROM chats WHERE open = 1 ORDER BY last_active DESC, id DESC LIMIT 1", [], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn mark_active(&self, id: ChatId) {
        let _ = self.conn().execute("UPDATE chats SET last_active = ?2, open = 1 WHERE id = ?1", params![id, now()]);
    }

    pub fn set_open(&self, id: ChatId, open: bool) {
        let _ = self.conn().execute("UPDATE chats SET open = ?2 WHERE id = ?1", params![id, open as i64]);
    }

    /// Gives the chat a title (trimmed, one line; blank ones are ignored).
    /// Returns whether it changed.
    pub fn rename(&self, id: ChatId, title: &str) -> bool {
        let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
        if title.is_empty() {
            return false;
        }
        let title: String = title.chars().take(200).collect();
        self.conn().execute("UPDATE chats SET title = ?2 WHERE id = ?1", params![id, title]).unwrap_or(0) > 0
    }

    /// Chats whose title or messages contain `text` (case-insensitive for
    /// ASCII), most recently updated first, at most `limit`. Notices the
    /// app added (`system`, `note`) aren't searched.
    pub fn search(&self, text: &str, limit: usize) -> Vec<SearchHit> {
        let text = text.trim();
        if text.is_empty() {
            return self.all().into_iter().take(limit).map(|chat| SearchHit { chat, snippet: None }).collect();
        }
        let pattern = like_pattern(text);
        let ids: Vec<(ChatId, Option<String>)> = {
            let c = self.conn();
            let Ok(mut stmt) = c.prepare(
                "SELECT c.id,
                        (SELECT i.text FROM items i
                         WHERE i.chat_id = c.id AND i.kind NOT IN ('system', 'note')
                           AND i.text LIKE ?1 ESCAPE '\\'
                         ORDER BY i.id LIMIT 1)
                 FROM chats c
                 WHERE c.title LIKE ?1 ESCAPE '\\'
                    OR EXISTS (SELECT 1 FROM items i
                               WHERE i.chat_id = c.id AND i.kind NOT IN ('system', 'note')
                                 AND i.text LIKE ?1 ESCAPE '\\')
                 ORDER BY c.updated DESC, c.id DESC
                 LIMIT ?2",
            ) else {
                return Vec::new();
            };
            stmt.query_map(params![pattern, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))
                .map(|rows| rows.filter_map(Result::ok).collect())
                .unwrap_or_default()
        };
        ids.into_iter()
            .filter_map(|(id, hit)| {
                Some(SearchHit { chat: self.get(id)?, snippet: hit.map(|t| snippet(&t, text, 90)) })
            })
            .collect()
    }

    /// The chat as Markdown: its title, when it was exported, then each
    /// message with who said it. App notices are left out.
    pub fn export_markdown(&self, id: ChatId) -> Option<String> {
        let chat = self.get(id)?;
        let mut out = format!(
            "# {}\n\n_Chat from agent-unibrow · started {} · exported {}_\n",
            chat.title.trim(),
            chat.created.format("%B %-d, %Y %-I:%M %p"),
            Local::now().format("%B %-d, %Y %-I:%M %p"),
        );
        for item in self.items(id) {
            let Some(label) = export_label(&item.kind) else { continue };
            out.push_str(&format!("\n## {label} · {}\n\n{}\n", item.at.format("%-I:%M %p"), item.text.trim_end()));
        }
        Some(out)
    }

    /// Writes the chat as Markdown to a free file in `dir` (created if
    /// needed). Returns the file's path.
    pub fn export_to(&self, id: ChatId, dir: &std::path::Path) -> Result<PathBuf, String> {
        let chat = self.get(id).ok_or("That chat no longer exists.")?;
        let text = self.export_markdown(id).ok_or("That chat no longer exists.")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
        let path = free_path(dir, &export_file_name(&chat.title));
        std::fs::write(&path, text).map_err(|e| format!("can't write {}: {e}", path.display()))?;
        Ok(path)
    }

    /// Deletes the chat and everything in it.
    pub fn delete(&self, id: ChatId) {
        let _ = self.conn().execute("DELETE FROM chats WHERE id = ?1", params![id]);
    }

    pub fn set_resume(&self, id: ChatId, backend: &str, resume: Option<&Value>) {
        let _ = self.conn().execute(
            "UPDATE chats SET backend = ?2, resume = ?3 WHERE id = ?1",
            params![id, backend, resume.map(Value::to_string)],
        );
    }

    pub fn items(&self, id: ChatId) -> Vec<StoredItem> {
        let c = self.conn();
        let Ok(mut stmt) = c.prepare("SELECT id, kind, text, at FROM items WHERE chat_id = ?1 ORDER BY id") else {
            return Vec::new();
        };
        stmt.query_map(params![id], |r| Ok(StoredItem { id: r.get(0)?, kind: r.get(1)?, text: r.get(2)?, at: when(r.get(3)?) }))
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    /// Adds an item. The chat's first user message becomes its title.
    pub fn append(&self, chat: ChatId, kind: &str, text: &str) -> ItemId {
        let c = self.conn();
        let t = now();
        let _ = c.execute("INSERT INTO items (chat_id, kind, text, at) VALUES (?1, ?2, ?3, ?4)", params![chat, kind, text, t]);
        let id = c.last_insert_rowid();
        let _ = c.execute("UPDATE chats SET updated = ?2 WHERE id = ?1", params![chat, t]);
        if kind == "user" {
            let _ = c.execute(
                "UPDATE chats SET title = ?2 WHERE id = ?1 AND title = 'New chat'",
                params![chat, title_from(text)],
            );
        }
        id
    }

    /// Replaces an item's text (a streamed reply as it grows).
    pub fn update_text(&self, item: ItemId, text: &str) {
        let _ = self.conn().execute("UPDATE items SET text = ?2 WHERE id = ?1", params![item, text]);
    }

    pub fn update_kind(&self, item: ItemId, kind: &str) {
        let _ = self.conn().execute("UPDATE items SET kind = ?2 WHERE id = ?1", params![item, kind]);
    }

    /// A recap of the chat for a model that can't resume it: the user's
    /// messages and the replies, newest last, cut to `max_chars` from the
    /// end.
    pub fn recap(&self, id: ChatId, max_chars: usize) -> Option<String> {
        let mut parts: Vec<String> = self
            .items(id)
            .into_iter()
            .filter_map(|i| match i.kind.as_str() {
                "user" | "steer" => Some(format!("User: {}", i.text.trim())),
                "assistant" => Some(format!("Assistant: {}", i.text.trim())),
                _ => None,
            })
            .collect();
        if parts.is_empty() {
            return None;
        }
        let mut total = 0;
        let mut keep = Vec::new();
        while let Some(p) = parts.pop() {
            total += p.len() + 2;
            if total > max_chars && !keep.is_empty() {
                break;
            }
            keep.push(p);
        }
        keep.reverse();
        Some(keep.join("\n\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn titles() {
        assert_eq!(title_from("Find a TUSQ nut"), "Find a TUSQ nut");
        assert_eq!(title_from("\n  hello\nworld"), "hello");
        let long = title_from("Find a TUSQ XL jazz bass nut on Sweetwater and tell me the price and stock");
        assert!(long.ends_with('…') && long.chars().count() <= 49, "{long}");
    }

    #[test]
    fn chats_round_trip() {
        let s = Chats::in_memory();
        let a = s.create();
        let b = s.create();
        let i = s.append(a, "user", "Find a TUSQ nut on Sweetwater");
        let r = s.append(a, "assistant", "Look");
        s.update_text(r, "Looking at Sweetwater now.");
        s.append(a, "user", "Second message");
        s.set_resume(a, "claude_code", Some(&json!({"session": "abc"})));
        let chat = s.get(a).unwrap();
        assert_eq!(chat.title, "Find a TUSQ nut on Sweetwater");
        assert_eq!(chat.items, 3);
        assert_eq!(chat.resume, Some(json!({"session": "abc"})));
        assert_eq!(s.items(a)[1].text, "Looking at Sweetwater now.");
        assert!(i > 0);

        s.mark_active(b);
        assert_eq!(s.last_active(), Some(b));
        s.set_open(b, false);
        assert_eq!(s.last_active(), Some(a));
        assert_eq!(s.open_chats().len(), 1);
        assert_eq!(s.all().len(), 2);

        s.delete(a);
        assert!(s.items(a).is_empty());
        assert_eq!(s.all().len(), 1);
    }

    #[test]
    fn renames() {
        let s = Chats::in_memory();
        let a = s.create();
        s.append(a, "user", "Find a TUSQ nut");
        assert!(s.rename(a, "  Guitar\n parts  "));
        assert_eq!(s.get(a).unwrap().title, "Guitar parts");
        assert!(!s.rename(a, "   "), "blank titles are ignored");
        assert_eq!(s.get(a).unwrap().title, "Guitar parts");
        // A renamed chat keeps its name when more messages come.
        s.append(a, "user", "another");
        assert_eq!(s.get(a).unwrap().title, "Guitar parts");
        assert!(!s.rename(9999, "nope"));
    }

    #[test]
    fn searches_titles_and_messages() {
        let s = Chats::in_memory();
        let a = s.create();
        s.append(a, "user", "Find a TUSQ nut on Sweetwater");
        s.append(a, "assistant", "The Graph Tech TUSQ XL nut is $14.99 and in stock.");
        let b = s.create();
        s.append(b, "user", "Book a table for two");
        s.append(b, "system", "Watch fired: price of tusq dropped");
        s.append(b, "assistant", "Done: 100% booked at 7_30.");

        let hits = s.search("tusq", 10);
        assert_eq!(hits.len(), 1, "system notices aren't searched");
        assert_eq!(hits[0].chat.id, a);
        assert!(hits[0].snippet.as_deref().unwrap().contains("TUSQ"));

        // Message text only, not the title.
        let hits = s.search("in stock", 10);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.as_deref().unwrap().contains("in stock"));

        // LIKE wildcards are literal.
        assert_eq!(s.search("100%", 10).len(), 1);
        assert_eq!(s.search("7_30", 10).len(), 1);
        assert_eq!(s.search("7x30", 10).len(), 0);
        assert!(s.search("%", 10).iter().all(|h| h.chat.id == b));

        // A title-only match has no snippet.
        s.rename(b, "Dinner plans");
        let hits = s.search("dinner", 10);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.is_none());

        // Blank: everything, newest first.
        assert_eq!(s.search("  ", 10).len(), 2);
        assert_eq!(s.search("", 1).len(), 1);
    }

    #[test]
    fn snippets_center_the_match() {
        let text = format!("{} needle here {}", "a ".repeat(100), "b ".repeat(100));
        let sn = snippet(&text, "NEEDLE", 40);
        assert!(sn.starts_with('…') && sn.ends_with('…') && sn.contains("needle"), "{sn}");
        assert_eq!(snippet("short\ntext", "text", 40), "short text");
    }

    #[test]
    fn exports_markdown() {
        let s = Chats::in_memory();
        let a = s.create();
        s.append(a, "system", "Claude Code is ready.");
        s.append(a, "user", "Find a TUSQ nut");
        s.append(a, "note", "thinking");
        s.append(a, "assistant", "It's $14.99.");
        s.append(a, "steer", "also check stock");
        let md = s.export_markdown(a).unwrap();
        assert!(md.starts_with("# Find a TUSQ nut\n"), "{md}");
        assert!(md.contains("## You · ") && md.contains("Find a TUSQ nut\n"));
        assert!(md.contains("## Claude · ") && md.contains("It's $14.99."));
        assert!(md.contains("## You (while it worked) · "));
        assert!(!md.contains("ready") && !md.contains("thinking"), "notices are left out");
        assert!(s.export_markdown(9999).is_none());

        let dir = std::env::temp_dir().join(format!("chats-export-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p1 = s.export_to(a, &dir).unwrap();
        let p2 = s.export_to(a, &dir).unwrap();
        assert_eq!(p1.file_name().unwrap(), "Find a TUSQ nut.md");
        assert_eq!(p2.file_name().unwrap(), "Find a TUSQ nut (1).md");
        assert_eq!(std::fs::read_to_string(&p1).unwrap(), md);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_names_are_safe() {
        assert_eq!(export_file_name("a/b: c?"), "a_b_ c_.md");
        assert_eq!(export_file_name("  "), "chat.md");
        assert_eq!(export_file_name("..."), "chat.md");
    }

    #[test]
    fn recaps_keep_the_end() {
        let s = Chats::in_memory();
        let a = s.create();
        assert!(s.recap(a, 1000).is_none());
        s.append(a, "user", "first");
        s.append(a, "note", "thinking");
        s.append(a, "assistant", "one");
        s.append(a, "user", "second");
        assert_eq!(s.recap(a, 1000).unwrap(), "User: first\n\nAssistant: one\n\nUser: second");
        assert_eq!(s.recap(a, 15).unwrap(), "User: second");
    }
}
