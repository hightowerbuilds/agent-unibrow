//! The change log: every change the agent made to the user's files
//! ([`router::FileChange`]), kept with the chat and message that caused it,
//! so a message's changes can be listed (blame) and taken back (undo, done
//! by the files rail).
//!
//! Each record, like each chat item, also gets an operation id
//! ([`OpId`]): this install's replica id and a Lamport clock value, the
//! ordering a later sync between devices needs (DeltaDB-style).
//!
//! Paths are kept as raw bytes, so a name that isn't UTF-8 comes back
//! exactly. A replaced file's old contents (up to the files rail's 5 MB
//! limit) are kept for undo; lists leave them out ([`ChangeRecord::undoable`]
//! says whether there are any).

use super::{ChatId, Chats, ItemId, now, when};
use chrono::{DateTime, Local};
use router::FileChange;
use rusqlite::{OptionalExtension, Row, params};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

pub(crate) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS file_changes (
        id INTEGER PRIMARY KEY,
        chat_id INTEGER REFERENCES chats(id) ON DELETE SET NULL,
        item_id INTEGER,
        tool_use TEXT NOT NULL DEFAULT '',
        op TEXT NOT NULL,
        path BLOB NOT NULL,
        other BLOB,
        hash TEXT,
        before BLOB,
        undoes INTEGER,
        undone_by INTEGER,
        replica TEXT NOT NULL,
        lamport INTEGER NOT NULL,
        at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS file_changes_item ON file_changes(item_id);
    CREATE INDEX IF NOT EXISTS file_changes_chat ON file_changes(chat_id, id);";

/// Where an operation came from and its place in order: this install's
/// replica id and a Lamport clock value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpId {
    pub replica: String,
    pub lamport: i64,
}

/// A logged change.
#[derive(Clone, Debug)]
pub struct ChangeRecord {
    pub id: i64,
    pub chat_id: Option<ChatId>,
    /// The chat's title (`None` once the chat is deleted).
    pub chat_title: Option<String>,
    /// The user's message the change answered.
    pub item_id: Option<ItemId>,
    pub tool_use: String,
    /// In lists, a replace's old contents are left out (see `undoable`).
    pub change: FileChange,
    /// The change can be undone as far as the log goes: it isn't undone
    /// yet, and a replace kept the old contents.
    pub undoable: bool,
    /// This change was an undo of that one.
    pub undoes: Option<i64>,
    /// ...and this one was undone by that one.
    pub undone_by: Option<i64>,
    pub op: OpId,
    pub at: DateTime<Local>,
}

fn raw(p: &Path) -> Vec<u8> {
    p.as_os_str().as_bytes().to_vec()
}

fn path_of(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

/// (op, path, other path, hash, old contents) for a row.
fn columns(c: &FileChange) -> (&'static str, Vec<u8>, Option<Vec<u8>>, Option<String>, Option<Vec<u8>>) {
    match c {
        FileChange::Created { path, hash } => (c.op(), raw(path), None, Some(hash.clone()), None),
        FileChange::Replaced { path, before, hash } => (c.op(), raw(path), None, Some(hash.clone()), before.clone()),
        FileChange::MadeDir { path, top } => (c.op(), raw(path), Some(raw(top)), None, None),
        FileChange::Moved { from, to } => (c.op(), raw(to), Some(raw(from)), None, None),
        FileChange::Trashed { path, trashed_as } => (c.op(), raw(path), trashed_as.as_deref().map(raw), None, None),
        FileChange::RemovedDir { path } => (c.op(), raw(path), None, None, None),
    }
}

fn change_of(op: &str, path: PathBuf, other: Option<PathBuf>, hash: Option<String>, before: Option<Vec<u8>>) -> Option<FileChange> {
    Some(match op {
        "created" => FileChange::Created { path, hash: hash? },
        "replaced" => FileChange::Replaced { path, before, hash: hash? },
        "made_dir" => FileChange::MadeDir { top: other.unwrap_or_else(|| path.clone()), path },
        "moved" => FileChange::Moved { from: other?, to: path },
        "trashed" => FileChange::Trashed { path, trashed_as: other },
        "removed_dir" => FileChange::RemovedDir { path },
        _ => return None,
    })
}

/// The columns a [`ChangeRecord`] is read from; `before` only with
/// `contents`.
fn select(contents: bool) -> String {
    format!(
        "SELECT f.id, f.chat_id, c.title, f.item_id, f.tool_use, f.op, f.path, f.other, f.hash, {}, \
         f.before IS NOT NULL, f.undoes, f.undone_by, f.replica, f.lamport, f.at \
         FROM file_changes f LEFT JOIN chats c ON c.id = f.chat_id",
        if contents { "f.before" } else { "NULL" }
    )
}

fn record(r: &Row) -> rusqlite::Result<Option<ChangeRecord>> {
    let op: String = r.get(5)?;
    let other: Option<Vec<u8>> = r.get(7)?;
    let has_before: bool = r.get(10)?;
    let undone_by: Option<i64> = r.get(12)?;
    let Some(change) = change_of(&op, path_of(r.get(6)?), other.map(path_of), r.get(8)?, r.get(9)?) else {
        return Ok(None);
    };
    let undoable = undone_by.is_none() && (op != "replaced" || has_before);
    Ok(Some(ChangeRecord {
        id: r.get(0)?,
        chat_id: r.get(1)?,
        chat_title: r.get(2)?,
        item_id: r.get(3)?,
        tool_use: r.get(4)?,
        change,
        undoable,
        undoes: r.get(11)?,
        undone_by,
        op: OpId { replica: r.get(13)?, lamport: r.get(14)? },
        at: when(r.get(15)?),
    }))
}

/// A new replica id: random enough to tell installs apart.
fn new_replica() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    h.write_u32(std::process::id());
    format!("{:016x}", h.finish())
}

impl Chats {
    /// The next operation id: this install's replica and the Lamport clock,
    /// one past the last.
    pub(crate) fn next_op_in(c: &rusqlite::Connection) -> OpId {
        let replica: String = match c.query_row("SELECT value FROM meta WHERE key = 'replica'", [], |r| r.get(0)).optional() {
            Ok(Some(r)) => r,
            _ => {
                let r = new_replica();
                let _ = c.execute("INSERT OR IGNORE INTO meta (key, value) VALUES ('replica', ?1)", params![r]);
                c.query_row("SELECT value FROM meta WHERE key = 'replica'", [], |row| row.get(0)).unwrap_or(r)
            }
        };
        let _ = c.execute(
            "INSERT INTO meta (key, value) VALUES ('lamport', '1') \
             ON CONFLICT(key) DO UPDATE SET value = CAST(value AS INTEGER) + 1",
            [],
        );
        let lamport = c
            .query_row("SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'lamport'", [], |r| r.get(0))
            .unwrap_or(0);
        OpId { replica, lamport }
    }

    /// Logs a change made in `chat`, answering the user's message `item`,
    /// by tool call `tool_use`. `undoes`: the change this one took back,
    /// which is then marked undone.
    pub fn record_change(
        &self,
        chat: Option<ChatId>,
        item: Option<ItemId>,
        tool_use: &str,
        change: &FileChange,
        undoes: Option<i64>,
    ) -> i64 {
        let c = self.conn();
        let op = Self::next_op_in(&c);
        let (kind, path, other, hash, before) = columns(change);
        let _ = c.execute(
            "INSERT INTO file_changes (chat_id, item_id, tool_use, op, path, other, hash, before, undoes, replica, lamport, at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![chat, item, tool_use, kind, path, other, hash, before, undoes, op.replica, op.lamport, now()],
        );
        let id = c.last_insert_rowid();
        if let Some(u) = undoes {
            let _ = c.execute("UPDATE file_changes SET undone_by = ?2 WHERE id = ?1", params![u, id]);
        }
        id
    }

    fn query_changes(&self, sql_tail: &str, args: &[&dyn rusqlite::ToSql], contents: bool) -> Vec<ChangeRecord> {
        let c = self.conn();
        let Ok(mut stmt) = c.prepare(&format!("{} {sql_tail}", select(contents))) else { return Vec::new() };
        stmt.query_map(args, record).map(|rows| rows.filter_map(|r| r.ok().flatten()).collect()).unwrap_or_default()
    }

    /// A chat's changes, oldest first.
    pub fn changes_for_chat(&self, chat: ChatId) -> Vec<ChangeRecord> {
        self.query_changes("WHERE f.chat_id = ?1 ORDER BY f.id", &[&chat], false)
    }

    /// The changes answering the user's message `item`, oldest first, with
    /// old contents (for undo).
    pub fn changes_for_item(&self, item: ItemId) -> Vec<ChangeRecord> {
        self.query_changes("WHERE f.item_id = ?1 ORDER BY f.id", &[&item], true)
    }

    /// The latest `limit` changes across chats, newest first.
    pub fn recent_changes(&self, limit: usize) -> Vec<ChangeRecord> {
        let limit = limit as i64;
        self.query_changes("ORDER BY f.id DESC LIMIT ?1", &[&limit], false)
    }

    /// One change, with old contents (for undo).
    pub fn change(&self, id: i64) -> Option<ChangeRecord> {
        self.query_changes("WHERE f.id = ?1", &[&id], true).into_iter().next()
    }

    /// A chat item's operation id, if it has one (items from before the
    /// change log don't).
    pub fn item_op(&self, item: ItemId) -> Option<OpId> {
        self.conn()
            .query_row("SELECT replica, lamport FROM items WHERE id = ?1 AND lamport IS NOT NULL", params![item], |r| {
                Ok(OpId { replica: r.get(0)?, lamport: r.get(1)? })
            })
            .optional()
            .ok()
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_are_kept_with_their_message() {
        let chats = Chats::in_memory();
        let chat = chats.create();
        let msg = chats.append(chat, "user", "tidy my notes");
        let made = FileChange::Created { path: "/home/u/new.txt".into(), hash: "ab".into() };
        let edit = FileChange::Replaced { path: "/home/u/todo.txt".into(), before: Some(b"old".to_vec()), hash: "cd".into() };
        let big = FileChange::Replaced { path: "/home/u/big.bin".into(), before: None, hash: "ef".into() };
        let odd = PathBuf::from(std::ffi::OsString::from_vec(b"/home/u/caf\xe9".to_vec()));
        let moved = FileChange::Moved { from: odd.clone(), to: "/home/u/cafe".into() };
        let a = chats.record_change(Some(chat), Some(msg), "t1", &made, None);
        let b = chats.record_change(Some(chat), Some(msg), "t2", &edit, None);
        chats.record_change(Some(chat), Some(msg), "t3", &big, None);
        chats.record_change(Some(chat), Some(msg), "t4", &moved, None);

        let all = chats.changes_for_item(msg);
        assert_eq!(all.iter().map(|r| r.change.clone()).collect::<Vec<_>>(), [made.clone(), edit.clone(), big, moved.clone()]);
        assert_eq!(all[0].chat_title.as_deref(), Some("tidy my notes"));
        assert!(all[0].undoable && all[1].undoable && !all[2].undoable);
        // Lists leave the old contents out but still know they're there.
        let listed = chats.changes_for_chat(chat);
        assert!(matches!(&listed[1].change, FileChange::Replaced { before: None, .. }) && listed[1].undoable);
        // A path that isn't UTF-8 comes back byte for byte.
        assert_eq!(chats.change(all[3].id).unwrap().change, moved);

        // An undo is logged too, and marks what it undid.
        let undo = FileChange::Trashed { path: "/home/u/new.txt".into(), trashed_as: Some("/home/u/.Trash/files/new.txt".into()) };
        let u = chats.record_change(Some(chat), Some(msg), "", &undo, Some(a));
        let first = chats.change(a).unwrap();
        assert_eq!(first.undone_by, Some(u));
        assert!(!first.undoable);
        assert_eq!(chats.change(u).unwrap().undoes, Some(a));
        assert_eq!(chats.recent_changes(2).iter().map(|r| r.id).collect::<Vec<_>>(), [u, u - 1]);

        // Operation ids: one replica, a clock that only goes up, shared
        // with chat items.
        let ops: Vec<OpId> = [a, b, u].iter().map(|id| chats.change(*id).unwrap().op).collect();
        assert!(ops.windows(2).all(|w| w[0].replica == w[1].replica && w[0].lamport < w[1].lamport));
        let item_op = chats.item_op(msg).unwrap();
        assert_eq!(item_op.replica, ops[0].replica);
        assert!(item_op.lamport < ops[0].lamport);

        // Deleting the chat keeps the log (for blame), without the chat.
        chats.delete(chat);
        let after = chats.change(b).unwrap();
        assert_eq!((after.chat_id, after.chat_title), (None, None));
    }
}
