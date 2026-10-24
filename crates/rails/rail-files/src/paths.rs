//! Where the file tools may work, and turning what the model wrote into a
//! checked path.
//!
//! Every path is resolved before use: `~` and relative paths start at the
//! base folder, `.` and `..` are folded, and every part that exists is run
//! through `canonicalize`, so a symlink can't lead anywhere the check
//! didn't see. The tools then act on the resolved path, never on the text
//! the model sent.
//!
//! Refused, for reading as well as changing:
//! - anything outside the allowed roots (the home folder, temporary and
//!   removable-drive folders; only the sandbox in a test instance);
//! - hidden files and folders (`~/.ssh`, `~/.config`, `~/.local`...), where
//!   credentials, the keyring and other apps' settings live;
//! - the app's own data and settings, wherever they are;
//! - files named like private keys or password databases.

use std::path::{Component, Path, PathBuf};

/// Where the file tools work. Built once by the shell.
#[derive(Clone, Debug)]
pub struct Config {
    /// Where `~` and relative paths start: the home folder, or the sandbox.
    pub base: PathBuf,
    /// Paths must be inside one of these (canonical).
    pub roots: Vec<PathBuf>,
    /// Never read, listed or changed (canonical where they exist).
    pub protected: Vec<PathBuf>,
    /// New files and folders here don't need approval: the Files folder
    /// in the app's workspace, where the user expects the agent's files.
    pub free_dir: Option<PathBuf>,
    /// The freedesktop Trash folder deleted files go to.
    pub trash_dir: PathBuf,
    /// A test instance: confined to its sandbox, never the user's Trash.
    pub sandboxed: bool,
    /// Shown as `~` in descriptions (the user's home, not in a sandbox).
    pub home: Option<PathBuf>,
}

/// Names the app keeps its own state under.
const APP: &str = "agent-unibrow";

/// File names that are private keys or password stores, wherever they are.
fn secret_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    let keys = ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"];
    if keys.iter().any(|k| lower.starts_with(k)) {
        return true;
    }
    let exact = ["credentials", "credentials.json", "client_secret.json", "google_client.json", "secrets.json"];
    if exact.contains(&lower.as_str()) {
        return true;
    }
    let ext = Path::new(&lower).extension().and_then(|e| e.to_str()).unwrap_or("");
    ["kdbx", "kdb", "pem", "p12", "pfx", "ppk", "gpg", "keystore", "jks"].contains(&ext)
}

fn canon_or_self(p: PathBuf) -> PathBuf {
    p.canonicalize().unwrap_or(p)
}

impl Config {
    /// The user's own computer: home and the usual shared folders.
    pub fn for_user(free_dir: PathBuf) -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/nonexistent-home"));
        let mut roots = vec![home.clone(), "/tmp".into(), "/var/tmp".into(), "/media".into(), "/run/media".into(), "/mnt".into()];
        roots.retain(|r| r.exists());
        let roots = roots.into_iter().map(canon_or_self).collect();
        let mut protected: Vec<PathBuf> = vec![
            home.join(".ssh"),
            home.join(".gnupg"),
            home.join(".local/share/keyrings"),
            home.join(".password-store"),
            home.join(".pki"),
            home.join(".netrc"),
            home.join(".git-credentials"),
            home.join(".aws"),
            home.join(".kube"),
            home.join(".docker"),
            home.join(".claude"),
            home.join(".claude.json"),
            home.join(".codex"),
        ];
        if let Some(d) = dirs::data_local_dir() {
            protected.push(d.join(APP));
            protected.push(d.join("keyrings"));
            protected.push(d.join("opencode"));
        }
        if let Some(d) = dirs::config_dir() {
            protected.push(d.join(APP));
        }
        if let Some(d) = dirs::cache_dir() {
            protected.push(d.join(APP));
        }
        let protected = protected.into_iter().map(canon_or_self).collect();
        let trash_dir = dirs::data_dir().unwrap_or_else(|| home.join(".local/share")).join("Trash");
        let _ = std::fs::create_dir_all(&free_dir);
        Self {
            base: canon_or_self(home.clone()),
            roots,
            protected,
            free_dir: Some(canon_or_self(free_dir)),
            trash_dir,
            sandboxed: false,
            home: Some(canon_or_self(home)),
        }
    }

    /// An isolated test instance: only `data_dir/files` (its home) and its
    /// workspace, with its own Trash, so nothing reaches the user's files.
    /// `free_dir` is inside `workspace`.
    pub fn sandbox(data_dir: &Path, workspace: PathBuf, free_dir: PathBuf) -> Self {
        let sandbox = data_dir.join("files");
        let _ = std::fs::create_dir_all(&sandbox);
        let _ = std::fs::create_dir_all(&free_dir);
        let sandbox = canon_or_self(sandbox);
        Self {
            base: sandbox.clone(),
            roots: vec![sandbox, canon_or_self(workspace)],
            protected: Vec::new(),
            free_dir: Some(canon_or_self(free_dir)),
            trash_dir: data_dir.join("Trash"),
            sandboxed: true,
            home: None,
        }
    }

    /// [`Config::sandbox`] when `$UNIBROW_DATA_DIR` is set, otherwise
    /// [`Config::for_user`].
    pub fn from_env(workspace: PathBuf, free_dir: PathBuf) -> Self {
        match std::env::var_os("UNIBROW_DATA_DIR") {
            Some(d) => Self::sandbox(Path::new(&d), workspace, free_dir),
            None => Self::for_user(free_dir),
        }
    }

    /// A path as the user reads it: `~/Documents/a.txt`.
    pub fn show(&self, p: &Path) -> String {
        if let Some(rest) = self.home.as_ref().and_then(|h| p.strip_prefix(h).ok()) {
            return if rest.as_os_str().is_empty() { "~".into() } else { format!("~/{}", rest.display()) };
        }
        p.display().to_string()
    }

    fn root_of(&self, p: &Path) -> Option<&PathBuf> {
        self.roots.iter().filter(|r| p.starts_with(r)).max_by_key(|r| r.components().count())
    }

    /// Whether `p` is a new entry's spot inside the free folder.
    pub fn in_free_dir(&self, p: &Path) -> bool {
        self.free_dir.as_ref().is_some_and(|d| p.starts_with(d) && p != d)
    }
}

/// How the last part of a path is treated when it's a symlink.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Last {
    /// Follow it: reading or writing a file goes to the link's target.
    Follow,
    /// Keep it: moving or deleting acts on the link itself.
    Keep,
}

/// What the call does, for the checks that depend on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Use {
    Read,
    /// Writing, editing, creating.
    Change,
    /// Moving or deleting: the path and everything under it goes.
    Remove,
}

/// Folds `.` and `..` without touching the disk.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `p` with every existing part canonicalized (symlinks resolved); the
/// parts that don't exist yet are appended as written.
fn resolve_existing(p: &Path) -> Result<PathBuf, String> {
    let mut existing = p.to_path_buf();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    while std::fs::symlink_metadata(&existing).is_err() {
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let mut out = existing
        .canonicalize()
        .map_err(|_| format!("{} is a broken link or can't be opened.", existing.display()))?;
    for part in rest.into_iter().rev() {
        out.push(part);
    }
    Ok(out)
}

/// Turns the model's path into the one the tool will use, and checks it.
pub fn resolve(cfg: &Config, raw: &str, last: Last, how: Use) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("No path given.".into());
    }
    if raw.contains('\0') {
        return Err("That path isn't valid.".into());
    }
    let expanded = if raw == "~" {
        cfg.base.clone()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        cfg.base.join(rest)
    } else if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        cfg.base.join(raw)
    };
    let lexical = normalize(&expanded);
    let path = match last {
        Last::Follow => resolve_existing(&lexical)?,
        Last::Keep => match (lexical.parent(), lexical.file_name()) {
            (Some(parent), Some(name)) => resolve_existing(parent)?.join(name),
            _ => resolve_existing(&lexical)?,
        },
    };
    check(cfg, &path, how)?;
    Ok(path)
}

/// The rules every resolved path must pass.
pub fn check(cfg: &Config, path: &Path, how: Use) -> Result<(), String> {
    let shown = cfg.show(path);
    let Some(root) = cfg.root_of(path) else {
        return Err(if cfg.sandboxed {
            format!("{shown} is outside this test instance's sandbox folder; file tools only work inside it.")
        } else {
            format!(
                "{shown} is outside the folders the file tools can use (your home folder, /tmp, and removable \
                 drives). System folders are off limits."
            )
        });
    };
    if let Some(p) = cfg.protected.iter().find(|p| path.starts_with(p)) {
        return Err(format!(
            "{shown} is off limits: {} holds the app's own data or sign-in credentials.",
            cfg.show(p)
        ));
    }
    let below = path.strip_prefix(root).unwrap_or(path);
    if below.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')) {
        return Err(format!(
            "{shown} is in a hidden file or folder (like ~/.ssh or ~/.config, where passwords, keys and app \
             settings live). The file tools never read or change those; the user can do it themselves."
        ));
    }
    if path.file_name().is_some_and(|n| secret_name(&n.to_string_lossy())) {
        return Err(format!("{shown} looks like a private key or password file, which the file tools never open."));
    }
    if how == Use::Remove {
        if cfg.roots.iter().any(|r| r == path) || cfg.home.as_deref() == Some(path) {
            return Err(format!("{shown} is a top-level folder; it can't be moved or deleted."));
        }
        if let Some(p) = cfg.protected.iter().find(|p| p.starts_with(path)) {
            return Err(format!("{shown} contains {}, which is off limits.", cfg.show(p)));
        }
    }
    Ok(())
}

/// Whether a directory entry is one the tools hide from listings and
/// searches (hidden, protected or a secret's name).
pub fn hidden_entry(cfg: &Config, path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    name.starts_with('.') || secret_name(&name) || cfg.protected.iter().any(|p| path.starts_with(p))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A throwaway folder for one test.
    pub(crate) fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rail-files-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    pub(crate) fn sandbox(tag: &str) -> (PathBuf, Config) {
        let data = scratch(tag);
        let cfg = Config::sandbox(&data, data.join("workspace"), data.join("workspace/Files"));
        (data, cfg)
    }

    #[test]
    fn relative_and_tilde_paths_start_in_the_sandbox() {
        let (data, cfg) = sandbox("rel");
        let files = data.join("files");
        assert_eq!(resolve(&cfg, "notes.txt", Last::Follow, Use::Change).unwrap(), files.join("notes.txt"));
        assert_eq!(resolve(&cfg, "~/a/b.txt", Last::Follow, Use::Change).unwrap(), files.join("a/b.txt"));
        assert_eq!(resolve(&cfg, "~", Last::Follow, Use::Read).unwrap(), files);
        assert_eq!(resolve(&cfg, "./x/../y.txt", Last::Follow, Use::Change).unwrap(), files.join("y.txt"));
    }

    #[test]
    fn escapes_from_the_sandbox_are_refused() {
        let (data, cfg) = sandbox("escape");
        assert!(resolve(&cfg, "../policy.json", Last::Follow, Use::Read).unwrap_err().contains("sandbox"));
        assert!(resolve(&cfg, "~/../../etc/passwd", Last::Follow, Use::Read).is_err());
        assert!(resolve(&cfg, "/etc/passwd", Last::Follow, Use::Read).is_err());
        let policy = data.join("policy.json");
        std::fs::write(&policy, "{}").unwrap();
        assert!(resolve(&cfg, policy.to_str().unwrap(), Last::Follow, Use::Read).is_err());
    }

    #[test]
    fn symlinks_out_of_the_sandbox_are_refused() {
        let (data, cfg) = sandbox("symlink");
        let files = data.join("files");
        std::fs::write(data.join("settings.json"), "secret").unwrap();
        std::os::unix::fs::symlink(data.join("settings.json"), files.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(&data, files.join("up")).unwrap();
        assert!(resolve(&cfg, "link.txt", Last::Follow, Use::Read).is_err());
        assert!(resolve(&cfg, "up/settings.json", Last::Follow, Use::Read).is_err());
        assert!(resolve(&cfg, "up/new.txt", Last::Follow, Use::Change).is_err());
        // Deleting the link itself is fine: it goes, its target stays.
        assert_eq!(resolve(&cfg, "link.txt", Last::Keep, Use::Remove).unwrap(), files.join("link.txt"));
    }

    #[test]
    fn hidden_secret_and_top_level_paths_are_refused() {
        let (_data, cfg) = sandbox("hidden");
        assert!(resolve(&cfg, "~/.ssh/id_ed25519", Last::Follow, Use::Read).unwrap_err().contains("hidden"));
        assert!(resolve(&cfg, ".bashrc", Last::Follow, Use::Change).is_err());
        assert!(resolve(&cfg, "keys/id_rsa", Last::Follow, Use::Read).unwrap_err().contains("private key"));
        assert!(resolve(&cfg, "vault.kdbx", Last::Follow, Use::Read).is_err());
        assert!(resolve(&cfg, "~", Last::Keep, Use::Remove).unwrap_err().contains("top-level"));
        assert!(resolve(&cfg, "report.pdf", Last::Follow, Use::Read).is_ok());
    }

    #[test]
    fn protected_folders_and_their_parents() {
        let (data, mut cfg) = sandbox("protected");
        let files = data.join("files");
        std::fs::create_dir_all(files.join("app/state")).unwrap();
        cfg.protected.push(files.join("app/state"));
        assert!(resolve(&cfg, "app/state/db.sqlite", Last::Follow, Use::Read).unwrap_err().contains("off limits"));
        assert!(resolve(&cfg, "app", Last::Follow, Use::Read).is_ok());
        assert!(resolve(&cfg, "app", Last::Keep, Use::Remove).unwrap_err().contains("contains"));
    }

    #[test]
    fn the_users_own_secrets_are_protected() {
        let dl = scratch("user-downloads");
        let cfg = Config::for_user(dl.clone());
        let _ = std::fs::remove_dir_all(dl);
        for p in ["~/.ssh/id_ed25519", "~/.config/agent-unibrow/policy.json", "~/.local/share/agent-unibrow/browser/Cookies", "~/.gnupg/pubring.kbx", "/etc/shadow", "/usr/bin/ls"] {
            assert!(resolve(&cfg, p, Last::Follow, Use::Read).is_err(), "{p} should be refused");
        }
        assert!(resolve(&cfg, "~/Documents/a-file-that-may-not-exist.txt", Last::Follow, Use::Change).is_ok());
        assert_eq!(cfg.show(&cfg.base.join("Documents/a.txt")), "~/Documents/a.txt");
    }
}
