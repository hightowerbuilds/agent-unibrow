//! Append-only record of every tool call, approval, and decision.
//!
//! Entries are kept in memory for the UI and appended to a JSONL file so a
//! session can be audited after the app closes. Nothing is ever edited or
//! removed; a correction is a new entry.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A message the user typed.
    User,
    /// The model finished a turn.
    Assistant,
    /// The model asked to run a tool.
    ToolCall,
    /// A tool returned a result.
    ToolResult,
    /// A tool call is waiting on the user.
    ApprovalRequested,
    /// The user approved or denied a gated call.
    ApprovalDecision,
    /// Something went wrong.
    Error,
    /// Informational (startup, auth, configuration).
    Info,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub at: DateTime<Local>,
    pub kind: Kind,
    /// One line, shown in the trace pane.
    pub summary: String,
    /// Full payload (tool input, result text), shown on expand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Screenshots saved alongside the trace (absolute paths).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
}

pub struct Trace {
    entries: Vec<Entry>,
    file: Option<File>,
    path: Option<PathBuf>,
}

impl Trace {
    /// Opens a new session log under `~/.local/share/agent-unibrow/traces/`.
    /// If the file can't be created, the trace still works in memory.
    pub fn open_session() -> Self {
        let root = std::env::var_os("UNIBROW_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::data_local_dir().map(|d| d.join("agent-unibrow")));
        let dir = root.map(|d| d.join("traces"));
        let path = dir.and_then(|d| {
            std::fs::create_dir_all(&d).ok()?;
            Some(d.join(format!("{}.jsonl", Local::now().format("%Y-%m-%d_%H-%M-%S"))))
        });
        let file = path
            .as_ref()
            .and_then(|p| OpenOptions::new().create(true).append(true).open(p).ok());
        Self { entries: Vec::new(), file, path }
    }

    /// A trace that is never written to disk (tests).
    pub fn in_memory() -> Self {
        Self { entries: Vec::new(), file: None, path: None }
    }

    pub fn push(&mut self, kind: Kind, summary: impl Into<String>, detail: Option<String>) -> &Entry {
        self.push_with_images(kind, summary, detail, Vec::new())
    }

    /// Saves `images` (JPEG bytes) next to the trace file and records them
    /// with the entry.
    pub fn push_with_images(
        &mut self,
        kind: Kind,
        summary: impl Into<String>,
        detail: Option<String>,
        images: Vec<Vec<u8>>,
    ) -> &Entry {
        let mut paths = Vec::new();
        if let Some(dir) = self.path.as_ref().map(|p| p.with_extension("")) {
            if !images.is_empty() && std::fs::create_dir_all(&dir).is_ok() {
                for (i, bytes) in images.iter().enumerate() {
                    let file = dir.join(format!("{:04}-{i}.jpg", self.entries.len()));
                    if std::fs::write(&file, bytes).is_ok() {
                        paths.push(file.to_string_lossy().into_owned());
                    }
                }
            }
        }
        let entry = Entry { at: Local::now(), kind, summary: summary.into(), detail, images: paths };
        if let Some(f) = self.file.as_mut() {
            if let Ok(line) = serde_json::to_string(&entry) {
                let _ = writeln!(f, "{line}");
            }
        }
        self.entries.push(entry);
        self.entries.last().expect("just pushed")
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_in_order() {
        let mut t = Trace::in_memory();
        t.push(Kind::User, "hi", None);
        t.push(Kind::ToolCall, "gmail_search_threads", Some("{}".into()));
        let kinds: Vec<_> = t.entries().iter().map(|e| e.kind).collect();
        assert_eq!(kinds, vec![Kind::User, Kind::ToolCall]);
    }
}
