//! Moving files to the desktop Trash, per the freedesktop.org Trash
//! specification: the item goes to `<trash>/files/<name>` and a
//! `<trash>/info/<name>.trashinfo` records where it came from and when, so
//! the file manager can put it back.
//!
//! The home Trash (`~/.local/share/Trash`) only takes items on the same
//! drive; for others the app asks `gio trash`, which knows each drive's own
//! Trash folder. Nothing is ever unlinked.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Percent-encodes a path for a `.trashinfo` `Path=` line.
fn encode(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::new();
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Longest name for an item in the Trash: its `<name>.trashinfo` must fit
/// in a file name too (255 bytes on Linux file systems).
const MAX_NAME: usize = 255 - ".trashinfo".len();

/// `report.pdf`, then `report.2.pdf`, `report.3.pdf`... A name too long
/// for the Trash keeps its extension and number and loses the end of its
/// stem.
fn candidate(name: &str, n: u32) -> String {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 32 => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    let number = if n < 2 { String::new() } else { format!(".{n}") };
    let room = MAX_NAME.saturating_sub(number.len() + ext.len());
    let stem = &stem[..stem.floor_char_boundary(room)];
    format!("{stem}{number}{ext}")
}

/// Where an item went.
#[derive(Debug)]
pub enum Trashed {
    /// Into the given Trash folder, as this file.
    At(PathBuf),
    /// Through `gio trash` (another drive's Trash).
    Gio,
}

/// Moves `path` (a file, folder or link, which must exist) to the Trash at
/// `trash_dir`. With `allow_gio`, items on another drive go through
/// `gio trash`.
pub fn trash(path: &Path, trash_dir: &Path, allow_gio: bool) -> Result<Trashed, String> {
    let name = path.file_name().ok_or("That path has no file name.")?.to_string_lossy().into_owned();
    let files = trash_dir.join("files");
    let info = trash_dir.join("info");
    if !trash_dir.exists() {
        use std::os::unix::fs::DirBuilderExt;
        // The spec's Trash is private to the user.
        let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(trash_dir);
    }
    for d in [&files, &info] {
        std::fs::create_dir_all(d).map_err(|e| format!("Couldn't open the Trash folder: {e}"))?;
    }
    for n in 1..10_000 {
        let item = candidate(&name, n);
        let info_path = info.join(format!("{item}.trashinfo"));
        let dest = files.join(&item);
        if std::fs::symlink_metadata(&dest).is_ok() {
            continue;
        }
        // Claiming the name: the info file is created exclusively first.
        let mut f = match std::fs::OpenOptions::new().write(true).create_new(true).open(&info_path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Couldn't write to the Trash: {e}")),
        };
        let date = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
        let written = write!(f, "[Trash Info]\nPath={}\nDeletionDate={date}\n", encode(path));
        drop(f);
        if let Err(e) = written {
            let _ = std::fs::remove_file(&info_path);
            return Err(format!("Couldn't write to the Trash: {e}"));
        }
        return match std::fs::rename(path, &dest) {
            Ok(()) => Ok(Trashed::At(dest)),
            Err(e) => {
                let _ = std::fs::remove_file(&info_path);
                // EXDEV: the item is on another drive.
                if e.raw_os_error() == Some(18) && allow_gio {
                    gio_trash(path).map(|_| Trashed::Gio)
                } else if e.raw_os_error() == Some(18) {
                    Err("That file is on another drive than the Trash folder; nothing was deleted.".into())
                } else {
                    Err(format!("Couldn't move it to the Trash: {e}. Nothing was deleted."))
                }
            }
        };
    }
    Err("The Trash has too many items with that name.".into())
}

/// Puts an item [`trash`] moved to `<trash>/files/<name>` back at `to`,
/// and removes its `.trashinfo`. Refuses if something is at `to` now, or
/// if `trashed_as` isn't an item in a Trash folder.
pub fn restore(trashed_as: &Path, to: &Path) -> Result<(), String> {
    let name = trashed_as.file_name().ok_or("That isn't an item in the Trash.")?;
    let files = trashed_as.parent().filter(|d| d.file_name().is_some_and(|n| n == "files")).ok_or("That isn't an item in the Trash.")?;
    let info = files.parent().ok_or("That isn't an item in the Trash.")?.join("info").join(format!("{}.trashinfo", name.to_string_lossy()));
    if std::fs::symlink_metadata(trashed_as).is_err() {
        return Err("It's no longer in the Trash.".into());
    }
    if std::fs::symlink_metadata(to).is_ok() {
        return Err("Something is at its old place now; it stays in the Trash.".into());
    }
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("Couldn't recreate its folder: {e}"))?;
    }
    std::fs::rename(trashed_as, to).map_err(|e| format!("Couldn't take it out of the Trash: {e}"))?;
    let _ = std::fs::remove_file(info);
    Ok(())
}

fn gio_trash(path: &Path) -> Result<(), String> {
    let out = std::process::Command::new("gio")
        .args(["trash", "--"])
        .arg(path)
        .output()
        .map_err(|_| "That file is on another drive, and `gio` (to use that drive's Trash) isn't installed. Nothing was deleted.".to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "Couldn't move it to that drive's Trash ({}). Nothing was deleted.",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::tests::scratch;

    #[test]
    fn moves_files_and_folders_to_the_trash_with_info() {
        let dir = scratch("trash");
        let trash_dir = dir.join("Trash");
        let file = dir.join("old draft.txt");
        std::fs::write(&file, "bye").unwrap();
        let Trashed::At(dest) = trash(&file, &trash_dir, false).unwrap() else { panic!("gio") };
        assert!(!file.exists());
        assert_eq!(dest, trash_dir.join("files/old draft.txt"));
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "bye");
        let info = std::fs::read_to_string(trash_dir.join("info/old draft.txt.trashinfo")).unwrap();
        assert!(info.starts_with("[Trash Info]\nPath="), "{info}");
        assert!(info.contains("old%20draft.txt\nDeletionDate=20"), "{info}");

        // The same name again gets a new one.
        std::fs::write(&file, "again").unwrap();
        let Trashed::At(dest) = trash(&file, &trash_dir, false).unwrap() else { panic!("gio") };
        assert_eq!(dest, trash_dir.join("files/old draft.2.txt"));
        assert!(trash_dir.join("info/old draft.2.txt.trashinfo").exists());

        let folder = dir.join("photos");
        std::fs::create_dir_all(folder.join("2026")).unwrap();
        std::fs::write(folder.join("2026/a.jpg"), "x").unwrap();
        trash(&folder, &trash_dir, false).unwrap();
        assert!(!folder.exists());
        assert!(trash_dir.join("files/photos/2026/a.jpg").exists());
    }

    #[test]
    fn names_and_encoding() {
        assert_eq!(candidate("a.txt", 1), "a.txt");
        assert_eq!(candidate("a.txt", 3), "a.3.txt");
        assert_eq!(candidate("Makefile", 2), "Makefile.2");
        assert_eq!(candidate(".env", 2), ".env.2");
        assert_eq!(encode(Path::new("/home/a b/ü.txt")), "/home/a%20b/%C3%BC.txt");
        // Long names are cut on a character boundary, keeping the number
        // and extension.
        let long = format!("{}.txt", "é".repeat(125));
        for n in [1, 2, 300] {
            let c = candidate(&long, n);
            assert!(c.len() <= MAX_NAME, "{n}: {} bytes", c.len());
            assert!(c.ends_with(&if n < 2 { ".txt".to_string() } else { format!(".{n}.txt") }), "{c}");
        }
        assert_ne!(candidate(&long, 2), candidate(&long, 3));
    }

    #[test]
    fn long_names_fit_in_the_trash() {
        let dir = scratch("trash-long");
        let trash_dir = dir.join("Trash");
        // 254 bytes: fits as a file name, but not with ".trashinfo" added.
        let name = format!("{}.txt", "é".repeat(125));
        for round in 0..2 {
            let file = dir.join(&name);
            std::fs::write(&file, format!("v{round}")).unwrap();
            let Trashed::At(dest) = trash(&file, &trash_dir, false).unwrap() else { panic!("gio") };
            assert!(!file.exists());
            assert_eq!(std::fs::read_to_string(&dest).unwrap(), format!("v{round}"));
            let item = dest.file_name().unwrap().to_str().unwrap().to_string();
            assert!(trash_dir.join("info").join(format!("{item}.trashinfo")).exists());
        }
        assert_eq!(std::fs::read_dir(trash_dir.join("files")).unwrap().count(), 2);
    }
}
