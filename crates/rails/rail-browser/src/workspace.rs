//! The workspace: one folder for everything the app saves for the user,
//! `~/Desktop/unibrow-workspace` (`$UNIBROW_DATA_DIR/workspace` in a test
//! instance), with a subfolder per kind of save. The shell's Workspace tab
//! browses it.

use std::path::PathBuf;

/// The workspace folder's name on the Desktop.
pub const NAME: &str = "unibrow-workspace";

/// A kind of save, each with its own subfolder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Folder {
    /// The toolbar camera button and the page menu's Save screenshot.
    Screenshots,
    /// Files downloaded in the browser.
    Downloads,
    /// Pages saved as PDF, by the user or the agent.
    Pages,
    /// Chats exported as Markdown.
    Chats,
    /// New files the agent writes, which need no approval here.
    Files,
}

impl Folder {
    pub const ALL: [Folder; 5] = [Folder::Screenshots, Folder::Downloads, Folder::Pages, Folder::Chats, Folder::Files];

    /// The subfolder's name, also shown in the app.
    pub fn name(self) -> &'static str {
        match self {
            Folder::Screenshots => "Screenshots",
            Folder::Downloads => "Downloads",
            Folder::Pages => "Pages",
            Folder::Chats => "Chats",
            Folder::Files => "Files",
        }
    }

    /// What goes in it, for the Workspace tab.
    pub fn about(self) -> &'static str {
        match self {
            Folder::Screenshots => "Pictures of pages, from the camera button",
            Folder::Downloads => "Files downloaded in the browser",
            Folder::Pages => "Pages saved as PDF",
            Folder::Chats => "Chats exported as Markdown",
            Folder::Files => "Files the agent made",
        }
    }

    /// The subfolder, made if it's missing.
    pub fn dir(self) -> PathBuf {
        let dir = root().join(self.name());
        let _ = std::fs::create_dir_all(&dir);
        dir
    }
}

/// The workspace folder (not made; [`Folder::dir`] makes what it needs).
/// Worked out once: finding the Desktop reads the user's folder settings.
pub fn root() -> PathBuf {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| match std::env::var_os("UNIBROW_DATA_DIR") {
        Some(d) => PathBuf::from(d).join("workspace"),
        None => dirs::desktop_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join("Desktop")))
            .unwrap_or_else(crate::data_dir)
            .join(NAME),
    })
    .clone()
}

/// Makes the workspace and all its subfolders.
pub fn ensure() -> PathBuf {
    for f in Folder::ALL {
        f.dir();
    }
    root()
}

/// `s` as a plain file name stem: no slashes or odd characters, not hidden,
/// at most 80 characters; "untitled" if nothing is left.
pub fn clean_stem(s: &str) -> String {
    let s: String = s.chars().map(|c| if c.is_alphanumeric() || " -_.".contains(c) { c } else { '_' }).take(80).collect();
    let s = s.trim().trim_start_matches('.').trim();
    if s.is_empty() { "untitled".into() } else { s.to_string() }
}

/// `<site>-<date>_<time>`, the name for a picture of a page on `site`.
pub fn stamped(site: &str) -> String {
    format!("{}-{}", clean_stem(site), chrono::Local::now().format("%Y-%m-%d_%H-%M-%S"))
}

/// Writes `bytes` as a new file in `folder`: `<stem>.<ext>`, or
/// `<stem>-2.<ext>`... when that's taken. Never replaces a file.
pub fn write_new(folder: Folder, stem: &str, ext: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    use std::io::Write;
    let dir = folder.dir();
    let stem = clean_stem(stem);
    for n in 1..10_000 {
        let name = if n == 1 { format!("{stem}.{ext}") } else { format!("{stem}-{n}.{ext}") };
        let path = dir.join(name);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                return match f.write_all(bytes) {
                    Ok(()) => Ok(path),
                    Err(e) => {
                        let _ = std::fs::remove_file(&path);
                        Err(format!("couldn't save it: {e}"))
                    }
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("couldn't save it in {}: {e}", dir.display())),
        }
    }
    Err("too many files with that name".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_are_plain_names() {
        assert_eq!(clean_stem("polish check"), "polish check");
        assert_eq!(clean_stem("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(clean_stem(".hidden"), "hidden");
        assert_eq!(clean_stem("  "), "untitled");
        assert_eq!(clean_stem(&"x".repeat(200)).len(), 80);
        assert!(stamped("www.nasa.gov").starts_with("www.nasa.gov-20"));
    }
}
