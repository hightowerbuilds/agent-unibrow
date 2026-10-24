//! The change log in the shell: the agent's changes to the user's files,
//! kept with the message that caused them (`chats::changes`).
//!
//! - **In the chat**, after each turn that changed files, a strip lists
//!   them, with Undo (click twice) for the turn's changes.
//! - **History → Changes** lists every change, newest first, filtered by
//!   path, with the chat that made it (blame) and Undo for one change.
//! - **Undo** goes through the files rail (`rail_files::undo`), which only
//!   takes a change back if the file is still as the agent left it; each
//!   undo is logged too. The agent hears about undone changes with the
//!   user's next message.
//!
//! Test steps (`UNIBROW_TEST_SHELL`, see chat_tools.rs): `changes` logs the
//! chat's changes, `undo_turn` undoes the last turn that changed files;
//! for playbook versions, `pbversions:<site>|<name>` logs a playbook's
//! versions and `pbrestore:<site>|<name>|<version>` restores one, as the
//! site view's Restore does.

use crate::text_input::TextInput;
use crate::{ItemKind, Shell, theme};
use chats::ChangeRecord;
use gpui::{AnyElement, Context, Entity, SharedString, div, prelude::*, px, rgb};
use router::FileChange;
use trace::Kind;

/// Most changes listed in a turn's strip.
const STRIP_LINES: usize = 6;
/// Most changes History → Changes reads.
const LIST_LIMIT: usize = 500;

/// What an Undo click is waiting to confirm.
#[derive(Clone, Copy, PartialEq)]
pub enum UndoTarget {
    /// Everything a turn (the user's message) changed.
    Turn(chats::ItemId),
    /// One change.
    Change(i64),
}

pub struct ChangeLog {
    /// The open chat's changes, oldest first.
    pub records: Vec<ChangeRecord>,
    /// An Undo clicked once, waiting for the confirming click.
    pub confirm: Option<UndoTarget>,
    /// What the user undid, for the agent with the next message.
    pub told_agent: Vec<String>,
    pub filter: Entity<TextInput>,
}

impl ChangeLog {
    pub fn new(cx: &mut Context<Shell>) -> Self {
        let filter = cx.new(|cx| TextInput::new("Filter by path", cx));
        Self { records: Vec::new(), confirm: None, told_agent: Vec::new(), filter }
    }
}

/// One line for a change, with paths as the user reads them.
pub fn describe(cfg: &rail_files::paths::Config, c: &FileChange) -> String {
    match c {
        FileChange::Created { path, .. } => format!("Created {}", cfg.show(path)),
        FileChange::Replaced { path, .. } => format!("Changed {}", cfg.show(path)),
        FileChange::MadeDir { path, .. } => format!("Made the folder {}", cfg.show(path)),
        FileChange::Moved { from, to } => format!("Moved {} → {}", cfg.show(from), cfg.show(to)),
        FileChange::Trashed { path, .. } => format!("Moved {} to the Trash", cfg.show(path)),
        FileChange::RemovedDir { path } => format!("Removed the folder {}", cfg.show(path)),
    }
}

impl Shell {
    /// Reads the open chat's changes again.
    pub(crate) fn reload_changes(&mut self) {
        self.change_log.records = self.chats.changes_for_chat(self.chat_id);
    }

    /// Logs the changes a tool call made, against the user's message the
    /// turn answers.
    pub(crate) fn log_changes(&mut self, tool_use: &str, changes: &[FileChange]) {
        if changes.is_empty() {
            return;
        }
        let item = self.items.iter().rev().find(|i| i.kind == ItemKind::User).and_then(|i| i.db);
        for c in changes {
            self.chats.record_change(Some(self.chat_id), item, tool_use, c, None);
        }
        self.reload_changes();
    }

    /// Undoes changes (newest first), logging each undo, and says what
    /// happened in the chat.
    fn undo_changes(&mut self, mut ids: Vec<i64>, cx: &mut Context<Self>) {
        ids.sort_unstable_by(|a, b| b.cmp(a));
        let (mut done, mut failed) = (Vec::new(), Vec::new());
        for id in ids {
            let Some(rec) = self.chats.change(id) else { continue };
            if rec.undone_by.is_some() || rec.undoes.is_some() {
                continue;
            }
            match rail_files::undo::undo(&self.files_cfg, &rec.change) {
                Ok((back, line)) => {
                    self.chats.record_change(rec.chat_id, rec.item_id, "", &back, Some(rec.id));
                    done.push(line);
                }
                Err(e) => failed.push(format!("{}: {e}", describe(&self.files_cfg, &rec.change))),
            }
        }
        self.change_log.confirm = None;
        self.reload_changes();
        let mut text = String::new();
        if !done.is_empty() {
            text.push_str(&format!("↶ Undid {}:\n{}", plural(done.len(), "change"), done.join("\n")));
            self.change_log.told_agent.extend(done.iter().cloned());
        }
        if !failed.is_empty() {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&format!("Couldn't undo {}:\n{}", plural(failed.len(), "change"), failed.join("\n")));
        }
        if text.is_empty() {
            text = "Nothing left to undo there.".into();
        }
        self.trace.push(Kind::Info, crate::first_line(&text), Some(text.clone()));
        self.push(ItemKind::System, text);
        self.scroll_to_bottom();
        cx.notify();
    }

    /// Undoes everything the turn answering message `item` changed.
    fn undo_turn(&mut self, item: chats::ItemId, cx: &mut Context<Self>) {
        let ids = self.chats.changes_for_item(item).into_iter().filter(|r| r.undoable && r.undoes.is_none()).map(|r| r.id).collect();
        self.undo_changes(ids, cx);
    }

    /// The strip after chat item `ix`, if it ends a turn that changed files.
    pub(crate) fn changes_after(&self, ix: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ends_turn = self.items.get(ix + 1).is_none_or(|next| next.kind == ItemKind::User);
        if !ends_turn {
            return None;
        }
        let item = self.items[..=ix].iter().rev().find(|i| i.kind == ItemKind::User)?.db?;
        let recs: Vec<&ChangeRecord> =
            self.change_log.records.iter().filter(|r| r.item_id == Some(item) && r.undoes.is_none()).collect();
        if recs.is_empty() {
            return None;
        }
        let open = recs.iter().filter(|r| r.undoable).count();
        let confirming = self.change_log.confirm == Some(UndoTarget::Turn(item));
        let lines = recs.iter().take(STRIP_LINES).map(|r| {
            let undone = r.undone_by.is_some();
            div()
                .flex()
                .gap_1()
                .text_color(rgb(if undone { theme::MUTED } else { theme::FG }))
                .child(div().flex_1().min_w(px(0.)).truncate().when(undone, |d| d.line_through()).child(describe(&self.files_cfg, &r.change)))
                .when(undone, |d| d.child(div().flex_none().text_color(rgb(theme::MUTED)).child("undone")))
        });
        let more = recs.len().saturating_sub(STRIP_LINES);
        Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(rgb(theme::RULE))
                .bg(rgb(theme::RAISED))
                .text_xs()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().text_color(rgb(theme::MUTED)).child(format!("✎ Changed {}", plural(recs.len(), "file"))))
                        .when(open > 0, |d| {
                            d.child(
                                Self::small_button(
                                    SharedString::from(format!("undo-turn-{item}")),
                                    if confirming { "Confirm undo" } else { "Undo" },
                                    confirming,
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if this.change_log.confirm == Some(UndoTarget::Turn(item)) {
                                        this.undo_turn(item, cx);
                                    } else {
                                        this.change_log.confirm = Some(UndoTarget::Turn(item));
                                        cx.notify();
                                    }
                                })),
                            )
                        }),
                )
                .children(lines)
                .when(more > 0, |d| d.child(div().text_color(rgb(theme::MUTED)).child(format!("+{more} more in History → Changes"))))
                .into_any_element(),
        )
    }

    /// History → Changes: every change, newest first (blame), with Undo.
    pub(crate) fn render_changes(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let needle = self.change_log.filter.read(cx).text().trim().to_lowercase();
        let rows: Vec<ChangeRecord> = self
            .chats
            .recent_changes(LIST_LIMIT)
            .into_iter()
            .filter(|r| needle.is_empty() || describe(&self.files_cfg, &r.change).to_lowercase().contains(&needle))
            .collect();
        let empty = rows.is_empty().then(|| {
            let text = if needle.is_empty() { "The agent hasn't changed any files yet." } else { "No changes match." };
            div().p_3().text_sm().text_color(rgb(theme::MUTED)).child(text)
        });
        let today = chrono::Local::now().date_naive();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .child(div().p_3().border_b_1().border_color(rgb(theme::RULE)).child(self.change_log.filter.clone()))
            .child(
                div()
                    .id("changes-list")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .children(empty)
                    .children(rows.into_iter().map(|r| self.change_row(r, today, cx))),
            )
    }

    fn change_row(&self, r: ChangeRecord, today: chrono::NaiveDate, cx: &mut Context<Self>) -> AnyElement {
        let id = r.id;
        let when = if r.at.date_naive() == today { r.at.format("%-I:%M %p").to_string() } else { r.at.format("%b %-d").to_string() };
        let what = describe(&self.files_cfg, &r.change);
        let what = if r.undoes.is_some() { format!("↶ Undo: {what}") } else { what };
        let chat = r.chat_title.clone().unwrap_or_else(|| "a deleted chat".into());
        let confirming = self.change_log.confirm == Some(UndoTarget::Change(id));
        let chat_id = r.chat_id;
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(rgb(theme::RULE))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .child(
                        div()
                            .text_sm()
                            .truncate()
                            .text_color(rgb(if r.undone_by.is_some() { theme::MUTED } else { theme::FG }))
                            .when(r.undone_by.is_some(), |d| d.line_through())
                            .child(what),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("change-chat-{id}")))
                            .text_xs()
                            .truncate()
                            .text_color(rgb(theme::MUTED))
                            .when_some(chat_id, |d, c| {
                                d.cursor_pointer()
                                    .hover(|s| s.text_color(rgb(theme::ACCENT)))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.switch_chat(c, cx);
                                        this.select_tab(crate::SidebarTab::Chat, window, cx);
                                    }))
                            })
                            .child(format!(
                                "{when} · {chat}{}",
                                if r.undone_by.is_some() { " · undone" } else { "" }
                            )),
                    ),
            )
            .when(r.undoable && r.undoes.is_none(), |d| {
                d.child(
                    Self::small_button(
                        SharedString::from(format!("undo-change-{id}")),
                        if confirming { "Confirm undo" } else { "Undo" },
                        confirming,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.change_log.confirm == Some(UndoTarget::Change(id)) {
                            this.undo_changes(vec![id], cx);
                        } else {
                            this.change_log.confirm = Some(UndoTarget::Change(id));
                            cx.notify();
                        }
                    })),
                )
            })
            .into_any_element()
    }

    /// A `changes` or `undo_turn` test step; `None` if it isn't one.
    pub(crate) fn change_log_test_step(&mut self, name: &str, arg: &str, cx: &mut Context<Self>) -> Option<String> {
        let parts: Vec<&str> = arg.split('|').collect();
        Some(match name {
            "pbversions" => {
                let (site, pb) = (parts.first().copied().unwrap_or(""), parts.get(1).copied().unwrap_or(""));
                let current = self.store.playbook(site, pb).map(|p| p.version);
                let versions: Vec<String> = self
                    .store
                    .playbook_versions(site, pb)
                    .iter()
                    .map(|v| format!("v{} {:?} {} steps", v.version, v.kind, v.steps.len()))
                    .collect();
                format!("pbversions {pb}: current {current:?} [{}]", versions.join("; "))
            }
            "pbrestore" => {
                let (site, pb) = (parts.first().copied().unwrap_or(""), parts.get(1).copied().unwrap_or(""));
                let v = parts.get(2).and_then(|v| v.parse().ok()).unwrap_or(0);
                match self.store.restore_playbook(site, pb, v) {
                    Ok(p) => format!("pbrestore {pb}: v{v} is now v{} with {} steps", p.version, p.steps.len()),
                    Err(e) => format!("pbrestore {pb}: {e}"),
                }
            }
            "changes" => {
                self.reload_changes();
                let lines: Vec<String> = self
                    .change_log
                    .records
                    .iter()
                    .map(|r| {
                        let mark = if r.undoes.is_some() { "undo " } else if r.undone_by.is_some() { "undone " } else { "" };
                        format!("{mark}{} [op {}]", describe(&self.files_cfg, &r.change), r.op.lamport)
                    })
                    .collect();
                format!("changes: {} [{}]", lines.len(), lines.join("; "))
            }
            "undo_turn" => {
                let item = self.change_log.records.iter().rev().find(|r| r.undoes.is_none() && r.undoable).and_then(|r| r.item_id);
                match item {
                    Some(item) => {
                        self.undo_turn(item, cx);
                        let said = self.items.last().map(|i| i.text.replace('\n', " | ")).unwrap_or_default();
                        format!("undo_turn: {said}")
                    }
                    None => "undo_turn: nothing to undo".into(),
                }
            }
            _ => return None,
        })
    }
}

fn plural(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}
