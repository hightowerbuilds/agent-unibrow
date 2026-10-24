//! The Workspace tab: browses the app's workspace folder
//! (`~/Desktop/unibrow-workspace`, see `rail_browser::workspace`), where
//! screenshots, downloads, saved pages, exported chats and the agent's
//! files go.
//!
//! - Click a folder to look inside, a file to open it in its usual app.
//! - Each row has Rename, Delete (to the Trash, after a confirm click) and
//!   Show (the file manager, with the item selected). The workspace's own
//!   folders can't be renamed or deleted, since saves would recreate them.
//! - Pictures show a thumbnail, made once in the background and kept in the
//!   app's data folder (`thumbs/`).
//! - While the tab is open the folder is read again every couple of seconds,
//!   so new screenshots and downloads appear on their own.
//!
//! Test steps (`UNIBROW_TEST_SHELL`, see chat_tools.rs): `workspace:<sub>`
//! shows a folder (`workspace:` the top) and logs what's in it, waiting for
//! a file and its thumbnails below the top; `workspace_rename:<name>`
//! renames the folder's first file; `workspace_delete` moves it to the
//! (test instance's) Trash.

use crate::text_input::{Submit, TextInput};
use crate::{Shell, SidebarTab, StopAgent, theme};
use gpui::{
    AnyElement, Context, Entity, Focusable, MouseButton, ObjectFit, SharedString, Subscription, Window, div, prelude::*,
    px, rgb,
};
use rail_browser::workspace::{self, Folder};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use trace::Kind;

/// Most items listed in one folder.
const MAX_ENTRIES: usize = 500;
/// How often an open tab reads its folder again.
const POLL: Duration = Duration::from_millis(1500);
/// Thumbnail size on screen; the file is made at twice that.
const THUMB_W: f32 = 56.;
const THUMB_H: f32 = 36.;
/// Pictures bigger than this get no thumbnail.
const MAX_THUMB_SOURCE: u64 = 64 << 20;

/// One item in the folder shown.
#[derive(Clone, PartialEq)]
pub struct Entry {
    path: PathBuf,
    name: String,
    dir: bool,
    size: u64,
    modified: Option<SystemTime>,
    /// Items inside, for a folder.
    items: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum Thumb {
    Making,
    Ready,
    Failed,
}

pub struct WorkspaceTab {
    /// The folder shown, inside the workspace.
    dir: PathBuf,
    entries: Vec<Entry>,
    rename_input: Entity<TextInput>,
    /// The item whose name is being edited.
    renaming: Option<PathBuf>,
    /// An item whose Delete was clicked once and awaits the confirm click.
    confirm_delete: Option<PathBuf>,
    /// The row under the pointer, which shows its buttons.
    hovered: Option<PathBuf>,
    /// What the last action did, and whether it failed.
    status: Option<(String, bool)>,
    /// Thumbnails by their cache file.
    thumbs: HashMap<PathBuf, Thumb>,
    /// The Trash deleted items go to, and whether `gio trash` may be used
    /// (not from a test instance).
    trash: (PathBuf, bool),
}

impl WorkspaceTab {
    pub fn new(window: &mut Window, cx: &mut Context<Shell>) -> (Self, Vec<Subscription>) {
        let rename_input = cx.new(|cx| TextInput::new("Name", cx));
        let focus = rename_input.focus_handle(cx);
        let subs = vec![
            cx.subscribe_in(&rename_input, window, |this: &mut Shell, _, ev: &Submit, window, cx| {
                this.finish_workspace_rename(Some(ev.0.clone()), cx);
                window.focus(&this.focus_handle);
            }),
            // Clicking away keeps what was typed.
            cx.on_blur(&focus, window, |this: &mut Shell, _, cx| {
                let text = this.workspace.rename_input.read(cx).text().to_string();
                this.finish_workspace_rename(Some(text), cx);
            }),
        ];
        // New saves show up while the tab is open.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                let Ok(dir) = this.update(cx, |s, _| (s.sidebar_open && s.tab == SidebarTab::Workspace).then(|| s.workspace.dir.clone()))
                else {
                    break;
                };
                let Some(dir) = dir else { continue };
                let shown = dir.clone();
                let entries = cx.background_executor().spawn(async move { list(&dir) }).await;
                let _ = this.update(cx, |s, cx| {
                    if s.workspace.dir == shown && s.workspace.entries != entries {
                        s.workspace.entries = entries;
                        s.make_thumbs(cx);
                        cx.notify();
                    }
                });
            }
        })
        .detach();
        let cfg = rail_files::paths::Config::from_env(workspace::root(), Folder::Files.dir());
        let tab = Self {
            dir: workspace::root(),
            entries: Vec::new(),
            rename_input,
            renaming: None,
            confirm_delete: None,
            hovered: None,
            status: None,
            thumbs: HashMap::new(),
            trash: (cfg.trash_dir, !cfg.sandboxed),
        };
        (tab, subs)
    }
}

/// The folder's items: at the top, the workspace's own folders first in
/// their usual order; then folders by name and files newest first. Hidden
/// items are left out.
fn list(dir: &Path) -> Vec<Entry> {
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };
    let visible = |e: &std::fs::DirEntry| !e.file_name().to_string_lossy().starts_with('.');
    let mut out: Vec<Entry> = read
        .filter_map(Result::ok)
        .filter(visible)
        .filter_map(|e| {
            let path = e.path();
            let meta = std::fs::metadata(&path).ok()?;
            let items = if meta.is_dir() {
                std::fs::read_dir(&path).map(|r| r.filter_map(Result::ok).filter(visible).count()).unwrap_or(0)
            } else {
                0
            };
            Some(Entry {
                name: e.file_name().to_string_lossy().into_owned(),
                path,
                dir: meta.is_dir(),
                size: meta.len(),
                modified: meta.modified().ok(),
                items,
            })
        })
        .collect();
    let top = dir == workspace::root();
    let rank = |e: &Entry| Folder::ALL.iter().position(|f| top && e.dir && e.name == f.name()).unwrap_or(usize::MAX);
    out.sort_by(|a, b| {
        rank(a).cmp(&rank(b)).then(b.dir.cmp(&a.dir)).then_with(|| {
            if a.dir {
                a.name.to_lowercase().cmp(&b.name.to_lowercase())
            } else {
                b.modified.cmp(&a.modified).then_with(|| a.name.cmp(&b.name))
            }
        })
    });
    out.truncate(MAX_ENTRIES);
    out
}

/// One of the workspace's own folders (at its top), which stay put.
fn own_folder(e: &Entry) -> Option<Folder> {
    if !e.dir || e.path.parent() != Some(workspace::root().as_path()) {
        return None;
    }
    Folder::ALL.into_iter().find(|f| f.name() == e.name)
}

fn is_picture(name: &str) -> bool {
    let ext = Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    ["png", "jpg", "jpeg", "gif", "webp", "bmp"].contains(&ext.as_str())
}

/// The cache file for `e`'s thumbnail, named for its path, size and time,
/// so a changed file gets a new one.
fn thumb_file(e: &Entry) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    e.path.hash(&mut h);
    e.size.hash(&mut h);
    e.modified.hash(&mut h);
    rail_browser::data_dir().join("thumbs").join(format!("{:016x}.png", h.finish()))
}

/// Writes a small copy of the picture at `src` to `dest`.
fn make_thumb(src: &Path, dest: &Path) -> bool {
    let Ok(picture) = image::open(src) else { return false };
    let small = picture.thumbnail((THUMB_W * 2.) as u32, (THUMB_H * 2.) as u32);
    let Some(dir) = dest.parent() else { return false };
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    // Written aside and moved in, so a half-made file is never shown.
    let part = dest.with_extension("part.png");
    small.save(&part).is_ok() && std::fs::rename(&part, dest).is_ok()
}

/// A name the user typed, checked: one plain name in the same folder.
fn rename_entry(path: &Path, name: &str) -> Result<PathBuf, String> {
    let name = name.trim();
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err("A name can't be empty or contain “/”.".into());
    }
    if name.starts_with('.') {
        return Err("A name starting with “.” would hide it.".into());
    }
    let dest = path.with_file_name(name);
    if dest == path {
        return Ok(dest);
    }
    if std::fs::symlink_metadata(&dest).is_ok() {
        return Err(format!("There's already an item called “{name}” here."));
    }
    std::fs::rename(path, &dest).map_err(|e| format!("Couldn't rename it: {e}"))?;
    Ok(dest)
}

fn human_size(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.;
    let mut unit = 0;
    while v >= 1024. && unit < UNITS.len() - 1 {
        v /= 1024.;
        unit += 1;
    }
    if v < 10. { format!("{v:.1} {}", UNITS[unit]) } else { format!("{v:.0} {}", UNITS[unit]) }
}

fn when(t: Option<SystemTime>) -> String {
    let Some(t) = t else { return String::new() };
    let t: chrono::DateTime<chrono::Local> = t.into();
    let now = chrono::Local::now();
    if t.date_naive() == now.date_naive() {
        t.format("Today %-I:%M %p").to_string()
    } else if t.format("%Y").to_string() == now.format("%Y").to_string() {
        t.format("%b %-d, %-I:%M %p").to_string()
    } else {
        t.format("%b %-d, %Y").to_string()
    }
}

fn plural(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

impl Shell {
    /// The tab was opened: make sure the folders exist and read the one shown.
    pub(crate) fn open_workspace(&mut self, cx: &mut Context<Self>) {
        workspace::ensure();
        if !self.workspace.dir.exists() {
            self.workspace.dir = workspace::root();
        }
        self.refresh_workspace(cx);
    }

    /// Shows folder `dir` (inside the workspace).
    fn workspace_go(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if !dir.starts_with(workspace::root()) {
            return;
        }
        let ws = &mut self.workspace;
        ws.dir = dir;
        ws.renaming = None;
        ws.confirm_delete = None;
        ws.hovered = None;
        ws.status = None;
        self.refresh_workspace(cx);
    }

    fn refresh_workspace(&mut self, cx: &mut Context<Self>) {
        self.workspace.entries = list(&self.workspace.dir);
        self.make_thumbs(cx);
        cx.notify();
    }

    /// Starts a thumbnail for each picture listed that has none yet.
    fn make_thumbs(&mut self, cx: &mut Context<Self>) {
        for e in &self.workspace.entries {
            if e.dir || e.size > MAX_THUMB_SOURCE || !is_picture(&e.name) {
                continue;
            }
            let key = thumb_file(e);
            if self.workspace.thumbs.contains_key(&key) {
                continue;
            }
            if key.exists() {
                self.workspace.thumbs.insert(key, Thumb::Ready);
                continue;
            }
            self.workspace.thumbs.insert(key.clone(), Thumb::Making);
            let src = e.path.clone();
            cx.spawn(async move |this, cx| {
                let dest = key.clone();
                let ok = cx.background_executor().spawn(async move { make_thumb(&src, &dest) }).await;
                let _ = this.update(cx, |s, cx| {
                    s.workspace.thumbs.insert(key, if ok { Thumb::Ready } else { Thumb::Failed });
                    cx.notify();
                });
            })
            .detach();
        }
    }

    fn start_workspace_rename(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let ws = &mut self.workspace;
        ws.renaming = Some(path);
        ws.confirm_delete = None;
        ws.rename_input.update(cx, |i, cx| i.set_text_selected(&name, cx));
        window.focus(&ws.rename_input.focus_handle(cx));
        cx.notify();
    }

    /// Ends a rename: keeps `name`, or cancels with `None`.
    fn finish_workspace_rename(&mut self, name: Option<String>, cx: &mut Context<Self>) {
        let Some(path) = self.workspace.renaming.take() else { return };
        if let Some(name) = name {
            let old = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            self.workspace.status = Some(match rename_entry(&path, &name) {
                Ok(new) if new == path => return self.refresh_workspace(cx),
                Ok(new) => {
                    self.trace.push(Kind::Info, format!("Renamed {old} to {}", name.trim()), Some(new.display().to_string()));
                    (format!("Renamed “{old}” to “{}”", name.trim()), false)
                }
                Err(e) => (e, true),
            });
        }
        self.refresh_workspace(cx);
    }

    /// Moves `path` to the Trash.
    fn delete_workspace_item(&mut self, path: &Path, cx: &mut Context<Self>) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let (trash, gio) = &self.workspace.trash;
        self.workspace.confirm_delete = None;
        self.workspace.status = Some(match rail_files::trash::trash(path, trash, *gio) {
            Ok(_) => {
                self.trace.push(Kind::Info, format!("Moved {name} to the Trash"), Some(path.display().to_string()));
                (format!("Moved “{name}” to the Trash"), false)
            }
            Err(e) => (e, true),
        });
        self.refresh_workspace(cx);
    }

    /// Opens a file in its usual app, or shows a folder.
    fn open_workspace_item(&mut self, e: &Entry, cx: &mut Context<Self>) {
        if e.dir {
            self.workspace_go(e.path.clone(), cx);
        } else {
            cx.open_with_system(&e.path);
        }
    }

    pub(crate) fn render_workspace(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ws = &self.workspace;
        let root = workspace::root();
        // unibrow-workspace / Screenshots / ...
        let mut crumbs = vec![(workspace::NAME.to_string(), root.clone())];
        let mut at = root.clone();
        for part in ws.dir.strip_prefix(&root).map(|r| r.components().collect::<Vec<_>>()).unwrap_or_default() {
            at = at.join(part);
            crumbs.push((part.as_os_str().to_string_lossy().into_owned(), at.clone()));
        }
        let last = crumbs.len() - 1;
        let shown = ws.dir.clone();
        let header = div()
            .flex()
            .items_center()
            .gap_1()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(rgb(theme::RULE))
            .child(div().flex().flex_1().min_w(px(0.)).flex_wrap().items_center().gap_1().text_xs().children(
                crumbs.into_iter().enumerate().flat_map(|(i, (label, path))| {
                    let here = i == last;
                    let sep = (i > 0).then(|| div().text_color(rgb(theme::MUTED)).child("/").into_any_element());
                    let crumb = div()
                        .id(SharedString::from(format!("ws-crumb-{i}")))
                        .text_color(rgb(if here { theme::FG } else { theme::ACCENT }))
                        .when(!here, |d| {
                            d.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| this.workspace_go(path.clone(), cx)))
                        })
                        .child(label)
                        .into_any_element();
                    sep.into_iter().chain([crumb])
                }),
            ))
            .child(Self::small_button("ws-open-folder".into(), "Open folder", false).on_click(cx.listener(
                move |_, _, _, cx| {
                    workspace::ensure();
                    cx.open_with_system(&shown);
                },
            )));
        let status = ws.status.clone().map(|(text, failed)| {
            div()
                .px_3()
                .py_1()
                .text_xs()
                .text_color(rgb(if failed { theme::ERROR } else { theme::OK }))
                .child(text)
        });
        let empty = ws.entries.is_empty().then(|| {
            let about = Folder::ALL
                .into_iter()
                .find(|f| ws.dir == root.join(f.name()))
                .map(|f| format!("Nothing here yet. {}.", f.about()))
                .unwrap_or_else(|| "Nothing here yet.".into());
            div().p_3().text_sm().text_color(rgb(theme::MUTED)).child(about)
        });
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .child(header)
            .children(status)
            .child(
                div()
                    .id("workspace-list")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .children(empty)
                    .children(ws.entries.iter().enumerate().map(|(i, e)| self.workspace_row(i, e, cx))),
            )
    }

    fn workspace_row(&self, i: usize, e: &Entry, cx: &mut Context<Self>) -> AnyElement {
        let ws = &self.workspace;
        let own = own_folder(e);
        let renaming = ws.renaming.as_ref() == Some(&e.path);
        let confirming = ws.confirm_delete.as_ref() == Some(&e.path);
        let actions = !renaming && (confirming || ws.hovered.as_ref() == Some(&e.path));
        let meta = if e.dir {
            let items = plural(e.items, "item");
            match own {
                Some(f) => format!("{} · {items}", f.about()),
                None => items,
            }
        } else {
            format!("{} · {}", human_size(e.size), when(e.modified))
        };
        let name: AnyElement = if renaming {
            div()
                .flex_1()
                .min_w(px(0.))
                // Clicks in the box edit text; they don't open the item.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_action(cx.listener(|this, _: &StopAgent, window, cx| {
                    this.finish_workspace_rename(None, cx);
                    window.focus(&this.focus_handle);
                }))
                .child(ws.rename_input.clone())
                .into_any_element()
        } else {
            let entry = e.clone();
            div()
                .id(SharedString::from(format!("ws-open-{i}")))
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.open_workspace_item(&entry, cx)))
                .child(div().text_sm().truncate().text_color(rgb(theme::FG)).child(e.name.clone()))
                .child(div().text_xs().truncate().text_color(rgb(theme::MUTED)).child(meta))
                .into_any_element()
        };
        let hover_path = e.path.clone();
        let (p1, p2, p3) = (e.path.clone(), e.path.clone(), e.path.clone());
        div()
            .id(SharedString::from(format!("ws-row-{i}")))
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(rgb(theme::RULE))
            .hover(|s| s.bg(rgb(theme::RAISED)))
            .on_hover(cx.listener(move |this, over: &bool, _, cx| {
                let ws = &mut this.workspace;
                if *over {
                    ws.hovered = Some(hover_path.clone());
                } else if ws.hovered.as_ref() == Some(&hover_path) {
                    ws.hovered = None;
                } else {
                    return;
                }
                cx.notify();
            }))
            .child(self.workspace_icon(e))
            .child(name)
            .when(actions && own.is_none() && !confirming, |d| {
                d.child(Self::small_button(SharedString::from(format!("ws-rename-{i}")), "Rename", false).on_click(
                    cx.listener(move |this, _, window, cx| this.start_workspace_rename(p1.clone(), window, cx)),
                ))
            })
            .when(actions && own.is_none(), |d| {
                d.child(
                    Self::small_button(
                        SharedString::from(format!("ws-delete-{i}")),
                        if confirming { "Confirm delete" } else { "Delete" },
                        confirming,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.workspace.confirm_delete.as_ref() == Some(&p2) {
                            this.delete_workspace_item(&p2.clone(), cx);
                        } else {
                            this.workspace.confirm_delete = Some(p2.clone());
                            cx.notify();
                        }
                    })),
                )
            })
            .when(actions && !confirming, |d| {
                d.child(
                    Self::small_button(SharedString::from(format!("ws-show-{i}")), "Show", false)
                        .on_click(cx.listener(move |_, _, _, cx| cx.reveal_path(&p3))),
                )
            })
            .into_any_element()
    }

    /// A picture's thumbnail, a folder drawn with boxes, or the file's
    /// extension.
    fn workspace_icon(&self, e: &Entry) -> AnyElement {
        let frame = || {
            div()
                .w(px(THUMB_W))
                .h(px(THUMB_H))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .overflow_hidden()
        };
        if e.dir {
            let folder = div()
                .relative()
                .w(px(26.))
                .h(px(19.))
                .child(div().absolute().top_0().left_0().w(px(11.)).h(px(6.)).rounded_t_sm().bg(rgb(theme::ACCENT)))
                .child(div().absolute().top(px(3.)).left_0().w(px(26.)).h(px(16.)).rounded_sm().bg(rgb(theme::ACCENT)).opacity(0.8));
            return frame().child(folder).into_any_element();
        }
        let thumb = thumb_file(e);
        if self.workspace.thumbs.get(&thumb) == Some(&Thumb::Ready) {
            return frame()
                .border_1()
                .border_color(rgb(theme::RULE))
                .child(gpui::img(thumb).w(px(THUMB_W)).h(px(THUMB_H)).object_fit(ObjectFit::Cover))
                .into_any_element();
        }
        let ext = Path::new(&e.name).extension().map(|x| x.to_string_lossy().to_uppercase()).unwrap_or_default();
        let ext: String = ext.chars().take(4).collect();
        frame()
            .bg(rgb(theme::RAISED))
            .border_1()
            .border_color(rgb(theme::RULE))
            .text_xs()
            .text_color(rgb(theme::MUTED))
            .child(if ext.is_empty() { "FILE".to_string() } else { ext })
            .into_any_element()
    }

    /// Before a `workspace:` test step runs: shows the folder, then waits
    /// (true) until it has a file and its thumbnails are done.
    pub(crate) fn workspace_test_waiting(&mut self, step: &str, cx: &mut Context<Self>) -> bool {
        let Some(sub) = step.strip_prefix("workspace:") else { return false };
        let dir = workspace::root().join(sub);
        if self.tab != SidebarTab::Workspace || self.workspace.dir != dir {
            self.tab = SidebarTab::Workspace;
            self.open_workspace(cx);
            self.workspace_go(dir, cx);
        } else {
            self.refresh_workspace(cx);
        }
        if sub.is_empty() {
            return false;
        }
        let ws = &self.workspace;
        !ws.entries.iter().any(|e| !e.dir) || ws.thumbs.values().any(|t| *t == Thumb::Making)
    }

    /// Runs a `workspace…` test step (see the module docs); `None` if it
    /// isn't one.
    pub(crate) fn workspace_test_step(&mut self, name: &str, arg: &str, cx: &mut Context<Self>) -> Option<String> {
        let first = self.workspace.entries.iter().find(|e| !e.dir).cloned();
        let where_ = self.workspace.dir.strip_prefix(workspace::root()).map(|p| p.display().to_string()).unwrap_or_default();
        Some(match name {
            "workspace" => {
                let shown: Vec<String> = self
                    .workspace
                    .entries
                    .iter()
                    .map(|e| {
                        let thumb = self.workspace.thumbs.get(&thumb_file(e)) == Some(&Thumb::Ready);
                        format!("{}{}{}", e.name, if e.dir { "/" } else { "" }, if thumb { " (thumbnail)" } else { "" })
                    })
                    .collect();
                format!("workspace /{where_}: {} [{}]", plural(shown.len(), "item"), shown.join(", "))
            }
            "workspace_rename" => {
                let Some(e) = first else { return Some("workspace rename: no file".into()) };
                self.workspace.renaming = Some(e.path.clone());
                self.finish_workspace_rename(Some(arg.to_string()), cx);
                let now: Vec<&str> = self.workspace.entries.iter().map(|e| e.name.as_str()).collect();
                format!("workspace renamed {} to {arg}; folder has {now:?}", e.name)
            }
            "workspace_delete" => {
                let Some(e) = first else { return Some("workspace delete: no file".into()) };
                self.delete_workspace_item(&e.path, cx);
                let in_trash = self.workspace.trash.0.join("files").join(&e.name).exists();
                format!(
                    "workspace deleted {}; folder has {}; in Trash: {in_trash}",
                    e.name,
                    plural(self.workspace.entries.len(), "item")
                )
            }
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(20 * 1024 * 1024), "20 MB");
    }

    #[test]
    fn rename_keeps_to_one_folder() {
        let dir = std::env::temp_dir().join(format!("ws-rename-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.png");
        std::fs::write(&a, "x").unwrap();
        std::fs::write(dir.join("taken.png"), "y").unwrap();
        for bad in ["", "  ", "..", "x/y.png", ".hidden", "taken.png"] {
            assert!(rename_entry(&a, bad).is_err(), "{bad:?}");
        }
        let b = rename_entry(&a, " b.png ").unwrap();
        assert_eq!(b, dir.join("b.png"));
        assert!(b.exists() && !a.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folders_first_then_newest_files() {
        let dir = std::env::temp_dir().join(format!("ws-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("zeta")).unwrap();
        std::fs::write(dir.join("old.txt"), "1").unwrap();
        std::fs::write(dir.join(".hidden"), "1").unwrap();
        let old = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options().write(true).open(dir.join("old.txt")).unwrap().set_modified(old).unwrap();
        std::fs::write(dir.join("new.txt"), "2").unwrap();
        let names: Vec<String> = list(&dir).into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["zeta", "new.txt", "old.txt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
