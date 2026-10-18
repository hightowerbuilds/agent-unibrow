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
//! Stored in SQLite (WAL) at `~/.local/share/agent-unibrow/chats.sqlite3`.

use chrono::{DateTime, Local, TimeZone};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Mutex;

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
             CREATE INDEX IF NOT EXISTS items_chat ON items(chat_id, id);",
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

    pub fn rename(&self, id: ChatId, title: &str) {
        let _ = self.conn().execute("UPDATE chats SET title = ?2 WHERE id = ?1", params![id, title]);
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
