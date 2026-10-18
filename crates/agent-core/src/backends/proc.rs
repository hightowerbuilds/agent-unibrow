//! Child-process plumbing for the CLI backends: JSONL on stdout, stderr
//! kept for error messages.

use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout, Command};

const STDERR_TAIL: usize = 4000;

pub struct JsonLines {
    lines: Lines<BufReader<ChildStdout>>,
    stderr: Arc<Mutex<String>>,
}

pub enum Line {
    Json(Value),
    /// The process closed stdout (exited or crashed).
    Eof,
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
        Ok((child, JsonLines { lines: BufReader::new(stdout).lines(), stderr }))
    }

    /// The next JSON line. Non-JSON lines (banners, warnings) are skipped.
    pub async fn next(&mut self) -> Line {
        loop {
            match self.lines.next_line().await {
                Ok(Some(l)) => {
                    let l = l.trim();
                    if l.is_empty() {
                        continue;
                    }
                    if let Ok(v) = serde_json::from_str::<Value>(l) {
                        return Line::Json(v);
                    }
                }
                Ok(None) | Err(_) => return Line::Eof,
            }
        }
    }

    /// The last part of stderr, cleaned of terminal color codes.
    pub fn stderr_tail(&self) -> String {
        let s = self.stderr.lock().expect("stderr lock").clone();
        strip_ansi(&s).trim().chars().rev().take(STDERR_TAIL).collect::<Vec<_>>().into_iter().rev().collect()
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

    #[test]
    fn strips_color_codes() {
        assert_eq!(strip_ansi("\u{1b}[31merror\u{1b}[0m: x"), "error: x");
    }
}
