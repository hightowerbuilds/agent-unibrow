//! Chat extras (chats roadmap "Later"): renaming a chat, searching across
//! chats, and exporting one to Markdown. Also draws the All chats list.
//!
//! - **Rename**: double-click a chat's title in the strip, or Rename in All
//!   chats. Enter or clicking away keeps the new name; Escape cancels.
//! - **Search**: the box at the top of All chats matches titles and message
//!   text (`chats::Chats::search`).
//! - **Export**: Export in All chats writes the chat as Markdown to the
//!   downloads folder (`~/Downloads/agent-unibrow`, or inside
//!   `$UNIBROW_DATA_DIR` for test instances).
//!
//! Test hook (isolated instances only): `UNIBROW_TEST_SHELL` is a list of
//! `;`-separated steps run once the first turn is over, each logged to the
//! trace as "Shell test: …": `rename:<title>`, `export`, `search:<text>`,
//! and `icons` (which visited sites have an icon for the History list).

use crate::text_input::{Changed, Submit, TextInput};
use crate::{Shell, StopAgent, theme};
use gpui::{
    AnyElement, Context, Entity, Focusable, MouseButton, SharedString, Subscription, Window, div, prelude::*, px, rgb,
};
use trace::Kind;

/// Most chats listed for a search.
const SEARCH_LIMIT: usize = 200;

pub struct ChatTools {
    rename_input: Entity<TextInput>,
    /// The chat whose title is being edited.
    renaming: Option<chats::ChatId>,
    /// ...in the All chats list (else in the strip).
    rename_in_list: bool,
    search: Entity<TextInput>,
    /// What the last export did, shown at the top of All chats.
    status: Option<String>,
}

impl ChatTools {
    pub fn new(window: &mut Window, cx: &mut Context<Shell>) -> (Self, Vec<Subscription>) {
        let rename_input = cx.new(|cx| TextInput::new("Chat name", cx));
        let search = cx.new(|cx| TextInput::new("Search chats: titles and messages", cx));
        let focus = rename_input.focus_handle(cx);
        let subs = vec![
            cx.subscribe_in(&rename_input, window, |this: &mut Shell, _, ev: &Submit, window, cx| {
                this.finish_rename(Some(ev.0.clone()), cx);
                window.focus(&this.input.focus_handle(cx));
            }),
            // Clicking away keeps what was typed.
            cx.on_blur(&focus, window, |this: &mut Shell, _, cx| {
                let text = this.chat_tools.rename_input.read(cx).text().to_string();
                this.finish_rename(Some(text), cx);
            }),
            cx.subscribe(&search, |_: &mut Shell, _, _: &Changed, cx| cx.notify()),
        ];
        (Self { rename_input, renaming: None, rename_in_list: false, search, status: None }, subs)
    }
}

impl Shell {
    /// Starts editing chat `id`'s title, in the list or in the strip.
    pub(crate) fn start_rename(&mut self, id: chats::ChatId, in_list: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(chat) = self.chats.get(id) else { return };
        let t = &mut self.chat_tools;
        t.renaming = Some(id);
        t.rename_in_list = in_list;
        t.rename_input.update(cx, |i, cx| i.set_text_selected(&chat.title, cx));
        window.focus(&t.rename_input.focus_handle(cx));
        cx.notify();
    }

    /// Ends a rename: keeps `title` (if it isn't blank), or cancels with `None`.
    pub(crate) fn finish_rename(&mut self, title: Option<String>, cx: &mut Context<Self>) {
        let Some(id) = self.chat_tools.renaming.take() else { return };
        if let Some(title) = title {
            if self.chats.rename(id, &title) {
                self.trace.push(Kind::Info, format!("Renamed a chat to “{}”", title.trim()), None);
            }
        }
        cx.notify();
    }

    /// Saves chat `id` as Markdown in the downloads folder and says where.
    pub(crate) fn export_chat(&mut self, id: chats::ChatId, cx: &mut Context<Self>) -> Result<std::path::PathBuf, String> {
        let r = self.chats.export_to(id, &rail_browser::downloads_dir());
        let text = match &r {
            Ok(path) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.trace.push(Kind::Info, format!("Exported a chat to {name}"), Some(path.display().to_string()));
                format!("Exported “{name}” to {}", path.parent().map(|p| p.display().to_string()).unwrap_or_default())
            }
            Err(e) => format!("Couldn't export the chat: {e}"),
        };
        self.chat_tools.status = Some(text.clone());
        self.notice(text);
        self.scroll_to_bottom();
        cx.notify();
        r
    }

    /// A chat's title in the strip: the title, or the rename box.
    pub(crate) fn strip_title(&self, id: chats::ChatId, title: String, cx: &mut Context<Self>) -> AnyElement {
        let t = &self.chat_tools;
        if t.renaming == Some(id) && !t.rename_in_list {
            self.rename_box(140., cx)
        } else {
            div().child(title).into_any_element()
        }
    }

    fn rename_box(&self, width: f32, cx: &mut Context<Self>) -> AnyElement {
        div()
            .w(px(width))
            .flex_none()
            // Clicks in the box edit text; they don't switch chats.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_action(cx.listener(|this, _: &StopAgent, window, cx| {
                this.finish_rename(None, cx);
                window.focus(&this.input.focus_handle(cx));
            }))
            .child(self.chat_tools.rename_input.clone())
            .into_any_element()
    }

    /// Every saved chat, newest first (C2.2), or the ones matching the search.
    pub(crate) fn render_chat_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.chat_tools.search.read(cx).text().trim().to_string();
        let hits = self.chats.search(&query, if query.is_empty() { usize::MAX >> 1 } else { SEARCH_LIMIT });
        let today = chrono::Local::now().date_naive();
        let empty = if !query.is_empty() {
            "No chats match."
        } else {
            "No chats yet."
        };
        let status = self.chat_tools.status.clone();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_3()
                    .border_b_1()
                    .border_color(rgb(theme::RULE))
                    .child(self.chat_tools.search.clone())
                    .children(status.map(|s| div().text_xs().text_color(rgb(theme::OK)).child(s))),
            )
            .child(
                div()
                    .id("chat-list")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .when(hits.is_empty(), |d| d.child(div().p_3().text_sm().text_color(rgb(theme::MUTED)).child(empty)))
                    .children(hits.into_iter().map(|hit| self.chat_row(hit, today, cx))),
            )
    }

    fn chat_row(&self, hit: chats::SearchHit, today: chrono::NaiveDate, cx: &mut Context<Self>) -> AnyElement {
        let c = hit.chat;
        let id = c.id;
        let active = id == self.chat_id;
        let confirming = self.confirm_delete == Some(id);
        let renaming = self.chat_tools.renaming == Some(id) && self.chat_tools.rename_in_list;
        let when = if c.updated.date_naive() == today {
            c.updated.format("%-I:%M %p").to_string()
        } else {
            c.updated.format("%b %-d").to_string()
        };
        let meta = format!(
            "{} message{} · {when}{}",
            c.items,
            if c.items == 1 { "" } else { "s" },
            if active { " · open now" } else if c.open { " · in the strip" } else { "" }
        );
        let title: AnyElement = if renaming {
            self.rename_box(220., cx)
        } else {
            div()
                .id(SharedString::from(format!("chat-row-{id}")))
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.switch_chat(id, cx)))
                .child(
                    div()
                        .text_sm()
                        .truncate()
                        .text_color(rgb(if active { theme::ACCENT } else { theme::FG }))
                        .child(c.title.clone()),
                )
                .children(hit.snippet.map(|s| div().text_xs().text_color(rgb(theme::FG)).opacity(0.8).truncate().child(s)))
                .child(div().text_xs().text_color(rgb(theme::MUTED)).child(meta))
                .into_any_element()
        };
        div()
            .flex()
            .items_center()
            .gap_1()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(rgb(theme::RULE))
            .child(title)
            .when(renaming, |d| d.child(div().flex_1()))
            .when(!renaming && !confirming, |d| {
                d.child(
                    Self::small_button(SharedString::from(format!("chat-rename-{id}")), "Rename", false)
                        .on_click(cx.listener(move |this, _, window, cx| this.start_rename(id, true, window, cx))),
                )
                .child(
                    Self::small_button(SharedString::from(format!("chat-export-{id}")), "Export", false)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let _ = this.export_chat(id, cx);
                        })),
                )
            })
            .child(
                Self::small_button(
                    SharedString::from(format!("chat-delete-{id}")),
                    if confirming { "Confirm delete" } else { "Delete" },
                    confirming,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.confirm_delete == Some(id) {
                        this.close_chat(id, true, cx);
                        this.chat_list_open = true;
                    } else {
                        this.confirm_delete = Some(id);
                    }
                    cx.notify();
                })),
            )
            .into_any_element()
    }

    /// `UNIBROW_TEST_SHELL` (see the module docs): runs the steps once the
    /// first message's turn is over.
    pub(crate) fn spawn_chat_test(&mut self, cx: &mut Context<Self>) {
        let Some(spec) = crate::test_hooks::var("UNIBROW_TEST_SHELL") else { return };
        let steps: Vec<String> = spec.split(';').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
                // The first message is saved and its turn is over.
                let ready = this.update(cx, |s, _| {
                    !s.busy && s.items.iter().any(|i| i.kind == crate::ItemKind::User && i.db.is_some())
                });
                match ready {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(_) => return,
                }
            }
            for step in steps {
                // `icons` waits (up to 15 s) for the page to be in history with its icon.
                for _ in 0..30 {
                    let waiting = step == "icons"
                        && this
                            .update(cx, |s, _| {
                                let visits = s.store.history(&browsing::HistoryQuery { limit: 50, ..Default::default() });
                                !visits.iter().any(|v| s.store.favicon(&v.site).is_some())
                            })
                            .unwrap_or(false);
                    if !waiting {
                        break;
                    }
                    cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
                }
                let _ = this.update(cx, |s, cx| s.run_chat_test_step(&step, cx));
                cx.background_executor().timer(std::time::Duration::from_millis(300)).await;
            }
            let _ = this.update(cx, |s, cx| {
                s.trace.push(Kind::Info, "Shell test: done", None);
                cx.notify();
            });
        })
        .detach();
    }

    fn run_chat_test_step(&mut self, step: &str, cx: &mut Context<Self>) {
        let (name, arg) = step.split_once(':').unwrap_or((step, ""));
        let id = self.chat_id;
        let line = match name {
            "rename" => {
                // As the user would: open the box, type, press Enter.
                self.chat_tools.renaming = Some(id);
                self.chat_tools.rename_in_list = false;
                self.finish_rename(Some(arg.to_string()), cx);
                let strip: Vec<String> = self.chats.open_chats().into_iter().map(|c| c.title).collect();
                format!("renamed; strip shows {strip:?}")
            }
            "export" => match self.export_chat(id, cx) {
                Ok(path) => {
                    let text = std::fs::read_to_string(&path).unwrap_or_default();
                    let first = text.lines().next().unwrap_or("").to_string();
                    format!("exported {} ({} bytes, first line {first:?})", path.display(), text.len())
                }
                Err(e) => format!("export failed: {e}"),
            },
            "search" => {
                self.chat_list_open = true;
                self.chat_tools.search.update(cx, |s, cx| s.set_text(arg, cx));
                let hits = self.chats.search(arg, SEARCH_LIMIT);
                let shown: Vec<String> = hits
                    .iter()
                    .map(|h| format!("{} | {}", h.chat.title, h.snippet.clone().unwrap_or_default()))
                    .collect();
                format!("search {arg:?}: {} hit(s) {shown:?}", hits.len())
            }
            "icons" => {
                let mut sites: Vec<String> = Vec::new();
                for v in self.store.history(&browsing::HistoryQuery { limit: 50, ..Default::default() }) {
                    if !sites.contains(&v.site) {
                        sites.push(v.site);
                    }
                }
                let with: Vec<&String> = sites.iter().filter(|s| self.site_icons.has_icon(&self.store, s)).collect();
                format!("icons: {} of {} visited sites have one {with:?}", with.len(), sites.len())
            }
            _ => format!("unknown step {step:?}"),
        };
        eprintln!("shell test: {line}");
        self.trace.push(Kind::Info, format!("Shell test: {line}"), None);
        cx.notify();
    }
}
