//! Child-process plumbing for the CLI backends: JSONL on stdout, stderr
//! kept for error messages.

use rand::Rng;
use rand::distr::Alphanumeric;
use serde_json::Value;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};

const STDERR_TAIL: usize = 4000;

pub struct JsonLines {
    out: BufReader<ChildStdout>,
    /// The line being read; kept here so a cancelled read loses nothing.
    buf: Vec<u8>,
    stderr: Arc<Mutex<String>>,
}

pub enum Line {
    Json(Value),
    /// The process closed stdout (exited or crashed).
    Eof,
    /// Reading stdout failed. The process may still be running, so kill it
    /// rather than wait for it to exit.
    Failed(String),
}

impl JsonLines {
    /// Spawns `cmd` with stdout and stderr piped. The caller decides stdin.
    pub fn spawn(cmd: &mut Command) -> Result<(Child, JsonLines), String> {
        cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
        let program = cmd.as_std().get_program().to_string_lossy().into_owned();
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!("`{program}` isn't installed or isn't on PATH.")
            } else {
                format!("Couldn't start `{program}`: {e}")
            }
        })?;
        let stdout = child.stdout.take().expect("piped stdout");
        let mut err = child.stderr.take().expect("piped stderr");
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = stderr.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            while let Ok(n) = err.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let mut s = sink.lock().expect("stderr lock");
                s.push_str(&String::from_utf8_lossy(&buf[..n]));
                if s.len() > STDERR_TAIL * 2 {
                    let cut = s.len() - STDERR_TAIL;
                    let cut = (cut..s.len()).find(|i| s.is_char_boundary(*i)).unwrap_or(cut);
                    s.drain(..cut);
                }
            }
        });
        Ok((child, JsonLines { out: BufReader::new(stdout), buf: Vec::new(), stderr }))
    }

    /// The next JSON line. Non-JSON lines (banners, warnings) are skipped.
    pub async fn next(&mut self) -> Line {
        next_json(&mut self.out, &mut self.buf).await
    }

    /// The last part of stderr, cleaned of terminal color codes.
    pub fn stderr_tail(&self) -> String {
        let s = self.stderr.lock().expect("stderr lock").clone();
        strip_ansi(&s).trim().chars().rev().take(STDERR_TAIL).collect::<Vec<_>>().into_iter().rev().collect()
    }
}

/// The next JSON line from `r`, using `buf` for the partial line. Read as
/// bytes, so output that isn't valid UTF-8 is skipped instead of ending the
/// stream.
async fn next_json<R: AsyncBufRead + Unpin>(r: &mut R, buf: &mut Vec<u8>) -> Line {
    loop {
        match r.read_until(b'\n', buf).await {
            Ok(0) if buf.is_empty() => return Line::Eof,
            Ok(_) => {
                let parsed = serde_json::from_str::<Value>(String::from_utf8_lossy(buf).trim());
                buf.clear();
                if let Ok(v) = parsed {
                    return Line::Json(v);
                }
            }
            Err(e) => {
                buf.clear();
                return Line::Failed(e.to_string());
            }
        }
    }
}

/// A file only this user can read, alone in a new 0700 directory; both are
/// removed when this is dropped. For configs that carry the MCP token, which
/// mustn't go on a command line where other local users can read it.
pub struct PrivateFile {
    dir: PathBuf,
    pub path: PathBuf,
}

impl PrivateFile {
    /// Writes `contents` to `name` under the user's runtime directory (or
    /// local data directory where there isn't one).
    pub fn create(name: &str, contents: &str) -> std::io::Result<Self> {
        let base = dirs::runtime_dir()
            .or_else(dirs::data_local_dir)
            .ok_or_else(|| std::io::Error::other("no private directory to write to"))?
            .join("agent-unibrow");
        Self::create_in(&base, name, contents)
    }

    fn create_in(base: &Path, name: &str, contents: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(base)?;
        let id: String = rand::rng().sample_iter(&Alphanumeric).take(16).map(char::from).collect();
        let dir = base.join(format!("run-{id}"));
        // Fails if it already exists, so the directory is always a new one of ours.
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        let file = Self { path: dir.join(name), dir };
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&file.path)?;
        f.write_all(contents.as_bytes())?;
        Ok(file)
    }
}

impl Drop for PrivateFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A one-paragraph error for a process that ended mid-turn.
pub fn died(name: &str, status: Option<std::process::ExitStatus>, stderr: &str) -> String {
    let code = status.and_then(|s| s.code()).map(|c| format!(" (exit {c})")).unwrap_or_default();
    if stderr.is_empty() {
        format!("{name} stopped unexpectedly{code}.")
    } else {
        format!("{name} stopped unexpectedly{code}:\n{stderr}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_color_codes() {
        assert_eq!(strip_ansi("\u{1b}[31merror\u{1b}[0m: x"), "error: x");
    }

    #[tokio::test]
    async fn bad_utf8_doesnt_end_the_stream() {
        let mut r: &[u8] = b"banner\n{\"a\":1}\n\xff\xfe junk\n\n{\"b\":\"\xc3\"}\n{\"c\":3}";
        let mut buf = Vec::new();
        let mut got = Vec::new();
        loop {
            match next_json(&mut r, &mut buf).await {
                Line::Json(v) => got.push(v),
                Line::Eof => break,
                Line::Failed(e) => panic!("read failed: {e}"),
            }
        }
        assert_eq!(got, vec![json!({"a": 1}), json!({"b": "\u{fffd}"}), json!({"c": 3})]);
    }

    #[test]
    fn private_file_is_owner_only_and_removed_on_drop() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("agent-unibrow-test-{}", std::process::id()));
        let file = PrivateFile::create_in(&base, "mcp.json", "{\"token\":1}").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file.path), 0o600);
        assert_eq!(mode(&file.dir), 0o700);
        assert_eq!(std::fs::read_to_string(&file.path).unwrap(), "{\"token\":1}");
        let (path, dir) = (file.path.clone(), file.dir.clone());
        drop(file);
        assert!(!path.exists() && !dir.exists());
        let _ = std::fs::remove_dir(&base);
    }
}
