//! The agent's task checklist, shown small above the conversation
//! (roadmap 7.4). The agent keeps it with the `task_progress` tool; it's
//! saved with the chat (see `chats::progress`).

use crate::theme;
use gpui::{AnyElement, div, prelude::*, px, rgb};

/// Steps shown at once; the rest are summed up as "+N more".
const SHOWN: usize = 7;

/// Which steps to show: a window around the next step, so the current one
/// is always visible on long plans.
fn window(total: usize, next: Option<usize>) -> (usize, usize) {
    if total <= SHOWN {
        return (0, total);
    }
    let start = next.unwrap_or(total).saturating_sub(2).min(total - SHOWN);
    (start, start + SHOWN)
}

pub fn render(plan: &chats::TaskPlan) -> AnyElement {
    let total = plan.steps.len();
    let done = plan.done_count();
    let next = plan.next().map(|(i, _)| i);
    let goal = if plan.goal.is_empty() { "Task plan".to_string() } else { plan.goal.clone() };
    let head = div()
        .flex()
        .gap_2()
        .text_xs()
        .child(div().flex_1().text_color(rgb(theme::FG)).child(goal))
        .child(
            div()
                .flex_none()
                .text_color(rgb(if next.is_none() { theme::OK } else { theme::MUTED }))
                .child(if next.is_none() { format!("done · {total}/{total}") } else { format!("{done}/{total}") }),
        );
    let mut card = div()
        .id("task-plan")
        .flex()
        .flex_col()
        .gap(px(2.))
        .mx_3()
        .mt_2()
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::RULE))
        .bg(rgb(theme::PANE))
        .child(head);
    // A finished plan stays as one line until the agent clears or replaces it.
    if next.is_some() {
        let (start, end) = window(total, next);
        if start > 0 {
            card = card.child(div().text_xs().text_color(rgb(theme::MUTED)).child(format!("✓ {start} earlier steps done")));
        }
        for (i, step) in plan.steps.iter().enumerate().take(end).skip(start) {
            let (mark, color) = if step.done {
                ("✓", theme::MUTED)
            } else if Some(i) == next {
                ("→", theme::ACCENT)
            } else {
                ("○", theme::MUTED)
            };
            let row = div()
                .flex()
                .gap_2()
                .text_xs()
                .text_color(rgb(color))
                .child(div().flex_none().w(px(12.)).child(mark))
                .child(div().flex_1().child(step.text.clone()));
            card = card.child(if step.done { row.line_through() } else { row });
        }
        if end < total {
            card = card.child(div().text_xs().text_color(rgb(theme::MUTED)).child(format!("+{} more", total - end)));
        }
    }
    card.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::window;

    #[test]
    fn the_next_step_stays_in_view() {
        assert_eq!(window(5, Some(4)), (0, 5));
        assert_eq!(window(20, Some(0)), (0, 7));
        assert_eq!(window(20, Some(10)), (8, 15));
        assert_eq!(window(20, Some(19)), (13, 20));
    }
}
