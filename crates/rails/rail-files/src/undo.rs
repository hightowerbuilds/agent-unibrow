//! Taking back a change from the change log ([`router::FileChange`]): the
//! shell keeps each change with the chat message that caused it, and the
//! user can undo a message's changes, or one change.
//!
//! An undo only goes ahead if the change's result is still there as the
//! tool left it (same contents, nothing new in the way); otherwise the file
//! is left alone and the reason given. Nothing is erased: a created file
//! goes to the Trash, a trashed one comes back out of it. Paths pass the
//! same checks as the file tools'.

use crate::paths::{self, Config, Use};
use crate::{first_missing_parent, replace_contents, trash};
use router::FileChange;
use std::path::Path;

/// SHA-256 of `bytes`, in hex.
pub fn hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// The hash of the file at `path`, if it's a regular file.
fn file_hash(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    std::fs::read(path).ok().map(|b| hash(&b))
}

/// The highest folder that creating `path` (a folder) would make.
fn first_missing(path: &Path) -> std::path::PathBuf {
    first_missing_parent(path).unwrap_or_else(|| path.to_path_buf())
}

/// Undoes `change`. Returns the change the undo made (for the log) and a
/// line saying what happened.
pub fn undo(cfg: &Config, change: &FileChange) -> Result<(FileChange, String), String> {
    let changed_since = |p: &Path| format!("{} has changed since, so it was left alone.", cfg.show(p));
    match change {
        FileChange::Created { path, hash: h } => {
            paths::check(cfg, path, Use::Remove)?;
            if file_hash(path).as_deref() != Some(h.as_str()) {
                return Err(if std::fs::symlink_metadata(path).is_err() {
                    format!("{} isn't there any more.", cfg.show(path))
                } else {
                    changed_since(path)
                });
            }
            let trashed_as = match trash::trash(path, &cfg.trash_dir, !cfg.sandboxed)? {
                trash::Trashed::At(p) => Some(p),
                trash::Trashed::Gio => None,
            };
            let line = format!("Moved {} (which it created) to the Trash.", cfg.show(path));
            Ok((FileChange::Trashed { path: path.clone(), trashed_as }, line))
        }
        FileChange::Replaced { path, before, hash: h } => {
            paths::check(cfg, path, Use::Change)?;
            let Some(before) = before else {
                return Err(format!("{}'s old contents were too big to keep, so this can't be undone.", cfg.show(path)));
            };
            let now = std::fs::read(path).ok().filter(|_| std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file()));
            let Some(now) = now else { return Err(format!("{} isn't there any more.", cfg.show(path))) };
            if hash(&now) != *h {
                return Err(changed_since(path));
            }
            replace_contents(path, before)?;
            let line = format!("Put back {}'s earlier contents.", cfg.show(path));
            Ok((FileChange::Replaced { path: path.clone(), before: Some(now), hash: hash(before) }, line))
        }
        FileChange::MadeDir { path, top } => {
            paths::check(cfg, path, Use::Remove)?;
            if !path.starts_with(top) {
                return Err("That folder change is malformed.".into());
            }
            paths::check(cfg, top, Use::Remove)?;
            // From the new folder up to the highest one made, while empty.
            let mut removed = Vec::new();
            let mut dir = path.as_path();
            loop {
                if std::fs::remove_dir(dir).is_err() {
                    break;
                }
                removed.push(dir.to_path_buf());
                if dir == top.as_path() {
                    break;
                }
                match dir.parent() {
                    Some(p) => dir = p,
                    None => break,
                }
            }
            let Some(highest) = removed.last().cloned() else {
                return Err(format!("{} isn't empty (or isn't there), so it was left alone.", cfg.show(path)));
            };
            let line = format!("Removed the empty folder {}.", cfg.show(&highest));
            Ok((FileChange::RemovedDir { path: highest }, line))
        }
        FileChange::Moved { from, to } => {
            paths::check(cfg, to, Use::Remove)?;
            paths::check(cfg, from, Use::Change)?;
            if std::fs::symlink_metadata(to).is_err() {
                return Err(format!("{} isn't there any more.", cfg.show(to)));
            }
            if std::fs::symlink_metadata(from).is_ok() {
                return Err(format!("Something is at {} now, so it wasn't moved back.", cfg.show(from)));
            }
            if let Some(dir) = from.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("Couldn't recreate the folder: {e}"))?;
            }
            std::fs::rename(to, from).map_err(|e| format!("Couldn't move it back: {e}"))?;
            let line = format!("Moved {} back to {}.", cfg.show(to), cfg.show(from));
            Ok((FileChange::Moved { from: to.clone(), to: from.clone() }, line))
        }
        FileChange::Trashed { path, trashed_as } => {
            paths::check(cfg, path, Use::Change)?;
            let Some(item) = trashed_as else {
                return Err(format!("{} went to another drive's Trash; restore it from your file manager.", cfg.show(path)));
            };
            trash::restore(item, path)?;
            let line = format!("Took {} back out of the Trash.", cfg.show(path));
            Ok((FileChange::Moved { from: item.clone(), to: path.clone() }, line))
        }
        FileChange::RemovedDir { path } => {
            paths::check(cfg, path, Use::Change)?;
            if std::fs::symlink_metadata(path).is_ok() {
                return Err(format!("Something is at {} now.", cfg.show(path)));
            }
            let top = first_missing(path);
            std::fs::create_dir_all(path).map_err(|e| format!("Couldn't recreate {}: {e}", cfg.show(path)))?;
            let line = format!("Recreated the folder {}.", cfg.show(path));
            Ok((FileChange::MadeDir { path: path.clone(), top }, line))
        }
    }
}
