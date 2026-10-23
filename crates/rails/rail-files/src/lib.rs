//! Files on the user's computer: read, list and search them, and write,
//! edit, move, create folders and delete (to the Trash).
//!
//! Reading runs on its own. Every change asks the user first, on an
//! approval card that shows exactly what will happen (the path, the size,
//! and a short diff of the lines that change), except creating a brand-new
//! file or folder in the app's downloads folder, which can't overwrite or
//! lose anything. Deleting never unlinks: items go to the desktop Trash.
//!
//! The changing tools declare [`Effect::Stage`] and ask through the tool
//! context once the path has passed its checks and the change is worked
//! out, so a refused path never shows the user a card, and the card shows
//! the real diff. What may be touched at all is in [`paths`].

pub mod diff;
pub mod paths;
pub mod trash;

use futures::future::BoxFuture;
use paths::{Config, Last, Use};
use router::{Effect, Rail, Tool, ToolContext, ToolOutput, ToolResult};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Files bigger than this aren't written, edited or diffed.
const MAX_TEXT: u64 = 5 * 1024 * 1024;
/// Files bigger than this aren't read at all.
const MAX_READ: u64 = 50 * 1024 * 1024;
/// Lines of diff on an approval card.
const CARD_LINES: usize = 30;

pub struct FilesRail {
    cfg: Arc<Config>,
}

impl FilesRail {
    pub fn new(cfg: Config) -> Self {
        Self { cfg: Arc::new(cfg) }
    }
}

impl Rail for FilesRail {
    fn name(&self) -> &'static str {
        "files"
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let c = || self.cfg.clone();
        vec![
            Arc::new(ReadFile(c())),
            Arc::new(ListDir(c())),
            Arc::new(Search(c())),
            Arc::new(WriteFile(c())),
            Arc::new(EditFile(c())),
            Arc::new(MakeDir(c())),
            Arc::new(MoveFile(c())),
            Arc::new(DeleteFile(c())),
        ]
    }
}

// ------------------------------------------------------------ helpers

fn check_keys(input: &Value, allowed: &[&str]) -> Result<(), String> {
    match input.as_object().and_then(|o| o.keys().find(|k| !allowed.contains(&k.as_str()))) {
        Some(k) => Err(format!("Unknown parameter “{k}”. Allowed: {}.", allowed.join(", "))),
        None => Ok(()),
    }
}

fn arg<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input[key].as_str().ok_or_else(|| format!("Missing “{key}”."))
}

fn shown(cfg: &Config, input: &Value, key: &str) -> String {
    let raw = input[key].as_str().unwrap_or("");
    paths::resolve(cfg, raw, Last::Keep, Use::Read).map(|p| cfg.show(&p)).unwrap_or_else(|_| raw.to_string())
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

fn line_count(text: &str) -> usize {
    text.lines().count()
}

/// A change worked out and ready to apply once the user agrees.
struct Plan {
    /// For the approval card: what changes, exactly.
    card: String,
    /// Why it asks (the card's reason line).
    reason: String,
    /// `false` for new files and folders in the downloads folder.
    ask: bool,
    apply: Box<dyn FnOnce() -> ToolResult + Send>,
}

/// Works out the change, asks the user if it needs to, then applies it.
fn run_plan(
    tool: &'static str,
    plan: impl FnOnce() -> Result<Plan, String> + Send + 'static,
    ctx: ToolContext,
) -> BoxFuture<'static, ToolResult> {
    Box::pin(async move {
        let plan = blocking(plan).await?;
        if plan.ask && !ctx.approver.approve(tool, plan.card.clone(), plan.reason.clone(), Vec::new(), None).await {
            return Err("The user declined this action. Nothing was changed.".into());
        }
        let mut out = blocking(plan.apply).await?;
        if !plan.ask {
            out.content.push_str("\n(New items in the downloads folder don't need the user's approval.)");
        }
        Ok(out)
    })
}

fn needs_context(tool: &str) -> BoxFuture<'static, ToolResult> {
    let msg = format!("{tool} needs the tool context");
    Box::pin(async move { Err(msg) })
}

/// For a pipe, socket or device: reading one could wait forever.
fn not_a_file(cfg: &Config, path: &Path) -> String {
    format!("{} isn't a regular file (it's a pipe, socket or device), so it can't be read or changed here.", cfg.show(path))
}

/// Reads a text file whole for changing it, refusing what can't be
/// changed safely as text.
fn read_text_for_change(cfg: &Config, path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|_| format!("There's no file at {}.", cfg.show(path)))?;
    if meta.is_dir() {
        return Err(format!("{} is a folder, not a file.", cfg.show(path)));
    }
    if !meta.is_file() {
        return Err(not_a_file(cfg, path));
    }
    if meta.len() > MAX_TEXT {
        return Err(format!("{} is too big to change here ({}).", cfg.show(path), diff::size(meta.len())));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("Couldn't read {}: {e}", cfg.show(path)))?;
    if looks_binary(&bytes) {
        return Err(format!("{} isn't a text file, so it can't be edited as text.", cfg.show(path)));
    }
    String::from_utf8(bytes).map_err(|_| format!("{} isn't UTF-8 text, so it can't be edited safely.", cfg.show(path)))
}

/// Replaces `path`'s contents in one step (a temporary file in the same
/// folder, renamed over it), keeping its permissions.
fn replace_contents(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let dir = path.parent().ok_or("no parent folder")?;
    let (tmp, mut f) = create_temp(dir).map_err(|e| format!("Couldn't write the file: {e}"))?;
    let result = (|| {
        f.write_all(bytes)?;
        f.sync_all()?;
        if let Ok(meta) = std::fs::metadata(path) {
            std::fs::set_permissions(&tmp, meta.permissions())?;
        }
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|e| format!("Couldn't write the file: {e}"))
}

/// A new, empty hidden file in `dir` to write before renaming it into
/// place. The name is short and random, not built from the target's, so it
/// fits wherever the target's name does.
fn create_temp(dir: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    use std::hash::{BuildHasher, Hasher};
    let mut tries = 0;
    loop {
        // A fresh RandomState is randomly keyed; the time and process mix
        // in more.
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        h.write_u32(std::process::id());
        let tmp = dir.join(format!(".unibrow-{:016x}.tmp", h.finish()));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 8 => tries += 1,
            Err(e) => return Err(e),
        }
    }
}

/// Moves a file to another drive: copies it to a temporary name next to
/// `to`, puts that in place only if nothing has appeared at `to`, then
/// removes the original. A failed copy leaves nothing behind.
fn move_across(from: &Path, to: &Path) -> Result<(), String> {
    let dir = to.parent().ok_or("no parent folder")?;
    let (tmp, mut dst) = create_temp(dir).map_err(|e| format!("Couldn't copy it there: {e}. Nothing was moved."))?;
    let copied = (|| -> std::io::Result<()> {
        let mut src = std::fs::File::open(from)?;
        std::io::copy(&mut src, &mut dst)?;
        let meta = src.metadata()?;
        dst.set_permissions(meta.permissions())?;
        if let Ok(t) = meta.modified() {
            let _ = dst.set_modified(t);
        }
        dst.sync_all()
    })();
    drop(dst);
    let placed = copied
        .map_err(|e| format!("Couldn't copy it there: {e}. Nothing was moved."))
        .and_then(|()| place_new(&tmp, to));
    if let Err(e) = placed {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::remove_file(from).map_err(|e| format!("Copied, but couldn't remove the original: {e}"))
}

/// Renames `tmp` to `to`, refusing if something is at `to` already.
fn place_new(tmp: &Path, to: &Path) -> Result<(), String> {
    let appeared = || "Something appeared at the destination while copying; nothing was moved.".to_string();
    // A hard link fails if `to` exists, with no gap between check and move.
    match std::fs::hard_link(tmp, to) {
        Ok(()) => {
            let _ = std::fs::remove_file(tmp);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(appeared()),
        // Drives without hard links (FAT, exFAT): check, then rename.
        Err(_) => {
            if std::fs::symlink_metadata(to).is_ok() {
                return Err(appeared());
            }
            std::fs::rename(tmp, to).map_err(|e| format!("Couldn't put it in place: {e}. Nothing was moved."))
        }
    }
}

/// Writes a file that mustn't exist yet.
fn create_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("Couldn't create the folder: {e}"))?;
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => "A file appeared there while waiting for approval; nothing was written.".to_string(),
        _ => format!("Couldn't create the file: {e}"),
    })?;
    f.write_all(bytes).map_err(|e| format!("Couldn't write the file: {e}"))
}

/// The nearest missing folder on the way to `path`, if any must be created.
fn first_missing_parent(path: &Path) -> Option<PathBuf> {
    let mut missing = None;
    let mut p = path.parent();
    while let Some(dir) = p {
        if dir.exists() {
            break;
        }
        missing = Some(dir.to_path_buf());
        p = dir.parent();
    }
    missing
}

/// Replaces `old` with `new` in `content`: exactly one occurrence, or every
/// one with `all`. Returns the new text and how many were replaced. Files
/// with Windows line endings match text written with plain newlines.
pub fn replace_text(content: &str, old: &str, new: &str, all: bool) -> Result<(String, usize), String> {
    if old.is_empty() {
        return Err("“old_text” is empty. Give the exact text to replace (read the file first).".into());
    }
    if old == new {
        return Err("“old_text” and “new_text” are the same; nothing would change.".into());
    }
    let (old, new) = if !content.contains(old) && content.contains("\r\n") && old.contains('\n') && !old.contains("\r\n") {
        (old.replace('\n', "\r\n"), new.replace("\r\n", "\n").replace('\n', "\r\n"))
    } else {
        (old.to_string(), new.to_string())
    };
    let count = content.matches(old.as_str()).count();
    match count {
        0 => Err("The text to replace wasn't found in the file. Read it again with file_read and copy the text \
                  exactly, including spaces and line breaks (without the line numbers)."
            .into()),
        n if n > 1 && !all => Err(format!(
            "That text appears {n} times in the file. Include more of the surrounding text so it matches once, \
             or pass replace_all: true to change every one."
        )),
        n if all => Ok((content.replace(old.as_str(), &new), n)),
        _ => Ok((content.replacen(old.as_str(), &new, 1), 1)),
    }
}

/// Files, folders and bytes under a folder (counting stops at `cap` entries).
fn tally(dir: &Path, cap: usize) -> (usize, usize, u64, bool) {
    let (mut files, mut dirs, mut bytes) = (0, 0, 0u64);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            if files + dirs >= cap {
                return (files, dirs, bytes, true);
            }
            match e.file_type() {
                Ok(t) if t.is_dir() => {
                    dirs += 1;
                    stack.push(e.path());
                }
                _ => {
                    files += 1;
                    bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
    }
    (files, dirs, bytes, false)
}

/// `*.pdf`, `invoice-??.txt`: `*` any run of characters, `?` one. Case
/// doesn't matter.
fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let (mut pi, mut ni, mut star, mut mark) = (0, 0, None, 0);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

fn name_matches(pattern: &str, name: &str) -> bool {
    if pattern.contains('*') || pattern.contains('?') {
        glob_match(pattern, name)
    } else {
        name.to_lowercase().contains(&pattern.to_lowercase())
    }
}

fn modified(meta: &std::fs::Metadata) -> String {
    meta.modified()
        .ok()
        .map(|t| chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// Text of a file for reading: text files directly, PDFs through
/// `pdftotext` when it's installed.
fn read_text(cfg: &Config, path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|_| format!("There's no file at {}.", cfg.show(path)))?;
    if meta.is_dir() {
        return Err(format!("{} is a folder. Use file_list to see what's in it.", cfg.show(path)));
    }
    if !meta.is_file() {
        return Err(not_a_file(cfg, path));
    }
    if meta.len() > MAX_READ {
        return Err(format!("{} is too big to read ({}).", cfg.show(path), diff::size(meta.len())));
    }
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if ext == "pdf" {
        let out = std::process::Command::new("pdftotext")
            .arg("-layout")
            .arg(path)
            .arg("-")
            .output()
            .map_err(|_| "Reading PDFs needs `pdftotext` (poppler), which isn't installed.".to_string())?;
        if !out.status.success() {
            return Err("pdftotext couldn't read this PDF.".into());
        }
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let bytes = std::fs::read(path).map_err(|e| format!("Couldn't read {}: {e}", cfg.show(path)))?;
    if looks_binary(&bytes) {
        return Err(format!(
            "{} isn't a text file ({}{}), so it can't be shown as text.",
            cfg.show(path),
            if ext.is_empty() { String::new() } else { format!(".{ext}, ") },
            diff::size(meta.len())
        ));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

// ---------------------------------------------------------- read tools

struct ReadFile(Arc<Config>);
struct ListDir(Arc<Config>);
struct Search(Arc<Config>);

impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "file_read"
    }
    fn description(&self) -> &'static str {
        "Read a text file on the user's computer (text, Markdown, CSV, JSON, code, and PDFs when pdftotext is \
         installed). Lines come back numbered; read long files in parts with offset and limit. Paths: absolute, \
         or ~/… for the home folder (relative paths start there too). Hidden folders (~/.ssh, ~/.config…), \
         the app's own data, keys and password files, and system folders are off limits. Runs without asking."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The file, e.g. ~/Documents/notes.txt."},
                "offset": {"type": "integer", "minimum": 1, "description": "First line to show (default 1)."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 5000, "description": "Lines to show (default 2000)."},
                "max_chars": {"type": "integer", "minimum": 1000, "maximum": 100000, "description": "Default 30000."}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Read {}", shown(&self.0, input, "path"))
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let cfg = self.0.clone();
        Box::pin(blocking(move || {
            check_keys(&input, &["path", "offset", "limit", "max_chars"])?;
            let path = paths::resolve(&cfg, arg(&input, "path")?, Last::Follow, Use::Read)?;
            let text = read_text(&cfg, &path)?;
            let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
            let limit = input["limit"].as_u64().unwrap_or(2000).clamp(1, 5000) as usize;
            let max_chars = input["max_chars"].as_u64().unwrap_or(30_000).clamp(1000, 100_000) as usize;
            let lines: Vec<&str> = text.lines().collect();
            let total = lines.len();
            let show = cfg.show(&path);
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let mut out = format!("{show} · {total} lines, {}\n", diff::size(size));
            if total == 0 {
                out.push_str("(empty file)");
                return Ok(ToolOutput::text(out, format!("read {show} (empty)")));
            }
            if offset > total {
                return Err(format!("{show} has only {total} lines."));
            }
            let mut last = offset - 1;
            for (i, line) in lines.iter().enumerate().skip(offset - 1).take(limit) {
                let row = format!("{:>6}\t{line}\n", i + 1);
                if out.len() + row.len() > max_chars {
                    if i + 1 > offset {
                        break;
                    }
                    // The first line alone is too long (minified code, a
                    // PDF without line breaks): show the start of it.
                    // Leaves space for the line number and the notes below.
                    let room = max_chars.saturating_sub(out.len() + 200);
                    let cut = line.floor_char_boundary(room);
                    out.push_str(&format!("{:>6}\t{}…\n", i + 1, &line[..cut]));
                    out.push_str(&format!(
                        "(line {} was cut: only its first {} of {} are shown.)\n",
                        i + 1,
                        diff::size(cut as u64),
                        diff::size(line.len() as u64)
                    ));
                    last = i + 1;
                    break;
                }
                out.push_str(&row);
                last = i + 1;
            }
            if last < total {
                out.push_str(&format!("… lines {offset}–{last} of {total} shown. Pass offset: {} to read on.", last + 1));
            }
            Ok(ToolOutput::text(out, format!("read {show} (lines {offset}–{last} of {total})")))
        }))
    }
}

impl Tool for ListDir {
    fn name(&self) -> &'static str {
        "file_list"
    }
    fn description(&self) -> &'static str {
        "List a folder on the user's computer: subfolders, then files with their size and last change. Default: \
         the home folder (~). Hidden and private items aren't listed. Runs without asking."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The folder, e.g. ~/Documents. Default ~."}
            },
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("List {}", if input["path"].is_string() { shown(&self.0, input, "path") } else { "~".into() })
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let cfg = self.0.clone();
        Box::pin(blocking(move || {
            check_keys(&input, &["path"])?;
            let raw = input["path"].as_str().unwrap_or("~");
            let dir = paths::resolve(&cfg, raw, Last::Follow, Use::Read)?;
            let show = cfg.show(&dir);
            if !dir.is_dir() {
                return Err(if dir.exists() {
                    format!("{show} is a file. Use file_read to read it.")
                } else {
                    format!("There's no folder at {show}.")
                });
            }
            let rd = std::fs::read_dir(&dir).map_err(|e| format!("Couldn't open {show}: {e}"))?;
            let (mut folders, mut files, mut skipped) = (Vec::new(), Vec::new(), 0);
            for e in rd.flatten() {
                let p = e.path();
                if paths::hidden_entry(&cfg, &p) {
                    skipped += 1;
                    continue;
                }
                let name = e.file_name().to_string_lossy().into_owned();
                let Ok(lmeta) = std::fs::symlink_metadata(&p) else { continue };
                if lmeta.file_type().is_symlink() {
                    let target = std::fs::read_link(&p).map(|t| t.display().to_string()).unwrap_or_default();
                    files.push((name.to_lowercase(), format!("{name} → {target} (link)")));
                } else if lmeta.is_dir() {
                    folders.push((name.to_lowercase(), format!("{name}/")));
                } else {
                    files.push((name.to_lowercase(), format!("{name}  ({}, {})", diff::size(lmeta.len()), modified(&lmeta))));
                }
            }
            folders.sort();
            files.sort();
            let (nf, nd) = (files.len(), folders.len());
            let mut out = format!("{show}: {nd} folders, {nf} files\n");
            if skipped > 0 {
                out.push_str(&format!("({skipped} hidden or private items not shown)\n"));
            }
            const CAP: usize = 400;
            for (_, line) in folders.iter().chain(&files).take(CAP) {
                out.push_str(line);
                out.push('\n');
            }
            if nd + nf > CAP {
                out.push_str(&format!("… {} more not shown. Use file_search to narrow it down.", nd + nf - CAP));
            }
            Ok(ToolOutput::text(out.trim_end().to_string(), format!("listed {show} ({nd} folders, {nf} files)")))
        }))
    }
}

impl Tool for Search {
    fn name(&self) -> &'static str {
        "file_search"
    }
    fn description(&self) -> &'static str {
        "Find files on the user's computer by name (a part of it, or a pattern like *.pdf) and/or by text they \
         contain, under a folder (default ~, searched up to 8 levels deep; hidden and private folders are \
         skipped). Runs without asking."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Folder to search in. Default ~."},
                "name": {"type": "string", "description": "Part of the name, or a pattern with * and ?. Case doesn't matter."},
                "contains": {"type": "string", "description": "Only text files containing this (case doesn't matter)."},
                "max_results": {"type": "integer", "minimum": 1, "maximum": 200, "description": "Default 50."}
            },
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn describe_call(&self, input: &Value) -> String {
        let what = [input["name"].as_str(), input["contains"].as_str()].into_iter().flatten().collect::<Vec<_>>().join(" / ");
        format!("Search {} for {what}", if input["path"].is_string() { shown(&self.0, input, "path") } else { "~".into() })
    }
    fn call(&self, input: Value) -> BoxFuture<'static, ToolResult> {
        let cfg = self.0.clone();
        Box::pin(blocking(move || {
            check_keys(&input, &["path", "name", "contains", "max_results"])?;
            let name = input["name"].as_str().map(str::trim).filter(|s| !s.is_empty());
            let contains = input["contains"].as_str().filter(|s| !s.is_empty()).map(str::to_lowercase);
            if name.is_none() && contains.is_none() {
                return Err("Give “name”, “contains”, or both.".into());
            }
            let max = input["max_results"].as_u64().unwrap_or(50).clamp(1, 200) as usize;
            let root = paths::resolve(&cfg, input["path"].as_str().unwrap_or("~"), Last::Follow, Use::Read)?;
            if !root.is_dir() {
                return Err(format!("There's no folder at {}.", cfg.show(&root)));
            }
            const MAX_DEPTH: usize = 8;
            const MAX_VISITS: usize = 100_000;
            let mut queue = std::collections::VecDeque::from([(root.clone(), 0usize)]);
            let (mut hits, mut visits, mut cut) = (Vec::new(), 0usize, false);
            'walk: while let Some((dir, depth)) = queue.pop_front() {
                let Ok(rd) = std::fs::read_dir(&dir) else { continue };
                let mut entries: Vec<_> = rd.flatten().collect();
                entries.sort_by_key(|e| e.file_name());
                for e in entries {
                    visits += 1;
                    if visits > MAX_VISITS {
                        cut = true;
                        break 'walk;
                    }
                    let p = e.path();
                    if paths::hidden_entry(&cfg, &p) {
                        continue;
                    }
                    let Ok(t) = e.file_type() else { continue };
                    let fname = e.file_name().to_string_lossy().into_owned();
                    let named = name.is_none_or(|n| name_matches(n, &fname));
                    if t.is_dir() {
                        if named && contains.is_none() {
                            hits.push(format!("{}/", cfg.show(&p)));
                        }
                        if depth + 1 < MAX_DEPTH {
                            queue.push_back((p, depth + 1));
                        }
                    } else if t.is_file() && named {
                        match &contains {
                            None => hits.push(cfg.show(&p)),
                            Some(needle) => {
                                if e.metadata().map(|m| m.len()).unwrap_or(u64::MAX) > 2 * 1024 * 1024 {
                                    continue;
                                }
                                let Ok(bytes) = std::fs::read(&p) else { continue };
                                if looks_binary(&bytes) {
                                    continue;
                                }
                                let text = String::from_utf8_lossy(&bytes);
                                if let Some((i, line)) =
                                    text.lines().enumerate().find(|(_, l)| l.to_lowercase().contains(needle.as_str()))
                                {
                                    let line: String = line.trim().chars().take(160).collect();
                                    hits.push(format!("{}:{}: {line}", cfg.show(&p), i + 1));
                                }
                            }
                        }
                    }
                    if hits.len() >= max {
                        cut = true;
                        break 'walk;
                    }
                }
            }
            let show = cfg.show(&root);
            if hits.is_empty() {
                let note = if cut { " (stopped early: too many files; search a smaller folder)" } else { "" };
                return Ok(ToolOutput::text(format!("Nothing found under {show}{note}."), format!("search {show}: nothing")));
            }
            let mut out = hits.join("\n");
            if cut {
                out.push_str("\n… stopped early; there may be more. Narrow the search or the folder.");
            }
            Ok(ToolOutput::text(out, format!("search {show}: {} found", hits.len())))
        }))
    }
}

// -------------------------------------------------------- change tools

struct WriteFile(Arc<Config>);
struct EditFile(Arc<Config>);
struct MakeDir(Arc<Config>);
struct MoveFile(Arc<Config>);
struct DeleteFile(Arc<Config>);

impl WriteFile {
    fn plan(cfg: Arc<Config>, input: Value) -> Result<Plan, String> {
        check_keys(&input, &["path", "content", "overwrite"])?;
        let path = paths::resolve(&cfg, arg(&input, "path")?, Last::Follow, Use::Change)?;
        let content = arg(&input, "content")?.to_string();
        let overwrite = input["overwrite"].as_bool().unwrap_or(false);
        if content.len() as u64 > MAX_TEXT {
            return Err(format!("That's too much to write in one file ({}).", diff::size(content.len() as u64)));
        }
        let show = cfg.show(&path);
        let new_size = diff::size(content.len() as u64);
        match std::fs::metadata(&path) {
            Ok(m) if m.is_dir() => Err(format!("{show} is a folder.")),
            Ok(m) if !m.is_file() => Err(not_a_file(&cfg, &path)),
            Ok(m) => {
                let old_size = diff::size(m.len());
                if !overwrite {
                    return Err(format!(
                        "{show} already exists ({old_size}). To replace all of it, call again with overwrite: true \
                         (the user is asked first), or use file_edit to change part of it."
                    ));
                }
                // A big file isn't read for the diff; a change to it while
                // waiting shows in its size and time instead.
                let (old, shown_diff) = if m.len() > MAX_TEXT {
                    (None, format!("(this replaces a {old_size} file; its contents aren't shown)"))
                } else {
                    let old = std::fs::read(&path).map_err(|e| format!("Couldn't read {show}: {e}"))?;
                    let shown_diff = match std::str::from_utf8(&old) {
                        Ok(text) if !looks_binary(&old) => diff::preview(text, &content, CARD_LINES),
                        _ => "(the current contents aren't text; not shown)".to_string(),
                    };
                    (Some(old), shown_diff)
                };
                let stamp = (m.len(), m.modified().ok());
                let card = format!("Replace {show} ({old_size} → {new_size})\n{shown_diff}");
                let (p, c) = (path.clone(), content.clone());
                Ok(Plan {
                    card,
                    reason: "file_write replaces a file on your computer".into(),
                    ask: true,
                    apply: Box::new(move || {
                        let now = std::fs::metadata(&p).ok().filter(|n| n.is_file());
                        let unchanged = match &old {
                            Some(old) => {
                                now.is_some_and(|n| n.len() == old.len() as u64)
                                    && std::fs::read(&p).ok().as_deref() == Some(&old[..])
                            }
                            None => now.is_some_and(|n| (n.len(), n.modified().ok()) == stamp),
                        };
                        if !unchanged {
                            return Err("The file changed while waiting for approval; nothing was written. Read it again.".into());
                        }
                        replace_contents(&p, c.as_bytes())?;
                        let show = cfg.show(&p);
                        Ok(ToolOutput::text(
                            format!("Replaced {show} ({new_size}, {} lines).", line_count(&c)),
                            format!("replaced {show} ({new_size})"),
                        ))
                    }),
                })
            }
            Err(_) => {
                let folder = first_missing_parent(&path).map(|d| format!(", creating the folder {}", cfg.show(&d))).unwrap_or_default();
                let card = format!(
                    "Create {show} ({new_size}, {} lines{folder})\n{}",
                    line_count(&content),
                    diff::head(&content, 15)
                );
                let ask = !cfg.in_free_dir(&path);
                let p = path.clone();
                Ok(Plan {
                    card,
                    reason: "file_write creates a file on your computer".into(),
                    ask,
                    apply: Box::new(move || {
                        create_new(&p, content.as_bytes())?;
                        let show = cfg.show(&p);
                        Ok(ToolOutput::text(
                            format!("Created {show} ({new_size}, {} lines).", line_count(&content)),
                            format!("created {show} ({new_size})"),
                        ))
                    }),
                })
            }
        }
    }
}

impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "file_write"
    }
    fn description(&self) -> &'static str {
        "Create a text file on the user's computer, or replace one (overwrite: true). Missing folders on the way \
         are created. The user approves each write on a card showing the path and the content or a diff, except \
         new files in the app's downloads folder (~/Downloads/agent-unibrow). To change part of an existing \
         file, use file_edit instead. Paths: absolute or ~/…; hidden folders, the app's own data, keys and \
         system folders are off limits."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The file, e.g. ~/Documents/notes.txt."},
                "content": {"type": "string", "description": "The whole text of the file."},
                "overwrite": {"type": "boolean", "description": "Replace the file if it exists (default false)."}
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        // Asks through the tool context once the change is worked out.
        Effect::Stage
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Write {}", shown(&self.0, input, "path"))
    }
    fn call(&self, _: Value) -> BoxFuture<'static, ToolResult> {
        needs_context(self.name())
    }
    fn call_with(&self, input: Value, ctx: ToolContext) -> BoxFuture<'static, ToolResult> {
        let cfg = self.0.clone();
        run_plan(self.name(), move || Self::plan(cfg, input), ctx)
    }
}

impl EditFile {
    fn plan(cfg: Arc<Config>, input: Value) -> Result<Plan, String> {
        check_keys(&input, &["path", "old_text", "new_text", "replace_all"])?;
        let path = paths::resolve(&cfg, arg(&input, "path")?, Last::Follow, Use::Change)?;
        let old_text = arg(&input, "old_text")?;
        let new_text = arg(&input, "new_text")?;
        let all = input["replace_all"].as_bool().unwrap_or(false);
        let before = read_text_for_change(&cfg, &path)?;
        let (after, n) = replace_text(&before, old_text, new_text, all)?;
        let show = cfg.show(&path);
        let card = format!(
            "Edit {show}: {} ({} → {})\n{}",
            if n == 1 { "1 change".to_string() } else { format!("{n} changes") },
            diff::size(before.len() as u64),
            diff::size(after.len() as u64),
            diff::preview(&before, &after, CARD_LINES)
        );
        Ok(Plan {
            card,
            reason: "file_edit changes a file on your computer".into(),
            ask: true,
            apply: Box::new(move || {
                if std::fs::read_to_string(&path).ok().as_deref() != Some(before.as_str()) {
                    return Err("The file changed while waiting for approval; nothing was written. Read it again.".into());
                }
                replace_contents(&path, after.as_bytes())?;
                let show = cfg.show(&path);
                let made = if n == 1 { "1 change".to_string() } else { format!("{n} changes") };
                Ok(ToolOutput::text(format!("Edited {show}: {made}."), format!("edited {show} ({made})")))
            }),
        })
    }
}

impl Tool for EditFile {
    fn name(&self) -> &'static str {
        "file_edit"
    }
    fn description(&self) -> &'static str {
        "Change part of a text file on the user's computer: replaces old_text, which must match the file exactly \
         (spaces and line breaks included, without file_read's line numbers) and only once, with new_text. Pass \
         replace_all: true to change every match. Read the file first. The user approves each edit on a card \
         showing the changed lines."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The file to change."},
                "old_text": {"type": "string", "description": "The exact text to replace."},
                "new_text": {"type": "string", "description": "What to put there instead (empty to remove it)."},
                "replace_all": {"type": "boolean", "description": "Replace every match (default: exactly one)."}
            },
            "required": ["path", "old_text", "new_text"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Stage
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Edit {}", shown(&self.0, input, "path"))
    }
    fn call(&self, _: Value) -> BoxFuture<'static, ToolResult> {
        needs_context(self.name())
    }
    fn call_with(&self, input: Value, ctx: ToolContext) -> BoxFuture<'static, ToolResult> {
        let cfg = self.0.clone();
        run_plan(self.name(), move || Self::plan(cfg, input), ctx)
    }
}

impl MakeDir {
    fn plan(cfg: Arc<Config>, input: Value) -> Result<Plan, String> {
        check_keys(&input, &["path"])?;
        let path = paths::resolve(&cfg, arg(&input, "path")?, Last::Follow, Use::Change)?;
        let show = cfg.show(&path);
        if std::fs::symlink_metadata(&path).is_ok() {
            return Err(format!("{show} already exists."));
        }
        let ask = !cfg.in_free_dir(&path);
        Ok(Plan {
            card: format!("Create the folder {show}"),
            reason: "file_mkdir creates a folder on your computer".into(),
            ask,
            apply: Box::new(move || {
                std::fs::create_dir_all(&path).map_err(|e| format!("Couldn't create {}: {e}", cfg.show(&path)))?;
                let show = cfg.show(&path);
                Ok(ToolOutput::text(format!("Created the folder {show}."), format!("created folder {show}")))
            }),
        })
    }
}

impl Tool for MakeDir {
    fn name(&self) -> &'static str {
        "file_mkdir"
    }
    fn description(&self) -> &'static str {
        "Create a folder (and any missing folders above it) on the user's computer. The user approves it, \
         except inside the app's downloads folder."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"path": {"type": "string", "description": "The new folder, e.g. ~/Documents/Taxes 2026."}},
            "required": ["path"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Stage
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Create folder {}", shown(&self.0, input, "path"))
    }
    fn call(&self, _: Value) -> BoxFuture<'static, ToolResult> {
        needs_context(self.name())
    }
    fn call_with(&self, input: Value, ctx: ToolContext) -> BoxFuture<'static, ToolResult> {
        let cfg = self.0.clone();
        run_plan(self.name(), move || Self::plan(cfg, input), ctx)
    }
}

impl MoveFile {
    fn plan(cfg: Arc<Config>, input: Value) -> Result<Plan, String> {
        check_keys(&input, &["from", "to"])?;
        let from = paths::resolve(&cfg, arg(&input, "from")?, Last::Keep, Use::Remove)?;
        let meta = std::fs::symlink_metadata(&from).map_err(|_| format!("There's nothing at {}.", cfg.show(&from)))?;
        let mut to = paths::resolve(&cfg, arg(&input, "to")?, Last::Follow, Use::Change)?;
        if to.is_dir() {
            to = to.join(from.file_name().ok_or("no file name")?);
            paths::check(&cfg, &to, Use::Change)?;
        }
        let (sf, st) = (cfg.show(&from), cfg.show(&to));
        if std::fs::symlink_metadata(&to).is_ok() {
            return Err(format!("{st} already exists. Choose another name, or delete it first."));
        }
        if to.starts_with(&from) {
            return Err("A folder can't be moved into itself.".into());
        }
        let verb = if from.parent() == to.parent() { "Rename" } else { "Move" };
        let what = if meta.is_dir() { "folder" } else { "file" };
        Ok(Plan {
            card: format!("{verb} the {what} {sf}\n→ {st}"),
            reason: format!("file_move {}s a {what} on your computer", verb.to_lowercase()),
            ask: true,
            apply: Box::new(move || {
                if std::fs::symlink_metadata(&to).is_ok() {
                    return Err("Something appeared at the destination while waiting for approval; nothing was moved.".into());
                }
                if let Some(dir) = to.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| format!("Couldn't create the folder: {e}"))?;
                }
                match std::fs::rename(&from, &to) {
                    Ok(()) => {}
                    // Another drive: copy, then remove the original.
                    Err(e) if e.raw_os_error() == Some(18) && meta.is_file() => move_across(&from, &to)?,
                    Err(e) if e.raw_os_error() == Some(18) => {
                        return Err("Folders can't be moved to another drive here; nothing was moved.".into());
                    }
                    Err(e) => return Err(format!("Couldn't move it: {e}")),
                }
                let (sf, st) = (cfg.show(&from), cfg.show(&to));
                Ok(ToolOutput::text(format!("Moved {sf} to {st}."), format!("moved {sf} → {st}")))
            }),
        })
    }
}

impl Tool for MoveFile {
    fn name(&self) -> &'static str {
        "file_move"
    }
    fn description(&self) -> &'static str {
        "Move or rename a file or folder on the user's computer. If “to” is an existing folder, the item goes \
         inside it. Never overwrites: fails if the destination exists. The user approves each move."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "from": {"type": "string", "description": "What to move."},
                "to": {"type": "string", "description": "The new path, or a folder to move it into."}
            },
            "required": ["from", "to"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Stage
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Move {} → {}", shown(&self.0, input, "from"), shown(&self.0, input, "to"))
    }
    fn call(&self, _: Value) -> BoxFuture<'static, ToolResult> {
        needs_context(self.name())
    }
    fn call_with(&self, input: Value, ctx: ToolContext) -> BoxFuture<'static, ToolResult> {
        let cfg = self.0.clone();
        run_plan(self.name(), move || Self::plan(cfg, input), ctx)
    }
}

impl DeleteFile {
    fn plan(cfg: Arc<Config>, input: Value) -> Result<Plan, String> {
        check_keys(&input, &["path"])?;
        let path = paths::resolve(&cfg, arg(&input, "path")?, Last::Keep, Use::Remove)?;
        let show = cfg.show(&path);
        let meta = std::fs::symlink_metadata(&path).map_err(|_| format!("There's nothing at {show}."))?;
        let what = if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&path).map(|t| t.display().to_string()).unwrap_or_default();
            format!("a link to {target}; the target stays")
        } else if meta.is_dir() {
            let (files, dirs, bytes, more) = tally(&path, 20_000);
            let plus = if more { "+" } else { "" };
            format!("folder with {files}{plus} files and {dirs}{plus} folders, {}", diff::size(bytes))
        } else {
            format!("file, {}, changed {}", diff::size(meta.len()), modified(&meta))
        };
        Ok(Plan {
            card: format!("Move to the Trash: {show}\n({what})"),
            reason: "file_delete deletes from your computer (to the Trash)".into(),
            ask: true,
            apply: Box::new(move || {
                let show = cfg.show(&path);
                let r#where = match trash::trash(&path, &cfg.trash_dir, !cfg.sandboxed)? {
                    trash::Trashed::At(p) => cfg.show(&p),
                    trash::Trashed::Gio => "that drive's Trash".into(),
                };
                Ok(ToolOutput::text(
                    format!("Moved {show} to the Trash ({where}). The user can restore it from there."),
                    format!("trashed {show}"),
                ))
            }),
        })
    }
}

impl Tool for DeleteFile {
    fn name(&self) -> &'static str {
        "file_delete"
    }
    fn description(&self) -> &'static str {
        "Delete a file or folder on the user's computer by moving it to the desktop Trash (it can be restored \
         from there; nothing is erased). The user approves each delete on a card showing what goes."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"path": {"type": "string", "description": "The file or folder to delete."}},
            "required": ["path"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::Stage
    }
    fn describe_call(&self, input: &Value) -> String {
        format!("Move {} to the Trash", shown(&self.0, input, "path"))
    }
    fn call(&self, _: Value) -> BoxFuture<'static, ToolResult> {
        needs_context(self.name())
    }
    fn call_with(&self, input: Value, ctx: ToolContext) -> BoxFuture<'static, ToolResult> {
        let cfg = self.0.clone();
        run_plan(self.name(), move || Self::plan(cfg, input), ctx)
    }
}

#[cfg(test)]
mod tests;
