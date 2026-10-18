//! The Focus tab's Safety section: site scope (6.3), the purchase limit
//! and per-turn caps (6.6), and remembered site permissions (4.2).

use crate::{Shell, theme};
use gpui::{Context, IntoElement, SharedString, div, prelude::*, px, rgb};
use rail_browser::permissions;

/// Choices for "Always ask for purchases over".
const PURCHASE_LIMITS: [(Option<f64>, &str); 5] =
    [(None, "Off"), (Some(0.0), "$0"), (Some(50.0), "$50"), (Some(200.0), "$200"), (Some(1000.0), "$1000")];

impl Shell {
    pub(crate) fn render_safety(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (focus_only, limit, limits) = {
            let p = self.policy.lock().unwrap_or_else(|e| e.into_inner());
            (p.focus_only(), p.purchase_limit(), p.limits())
        };
        let no_focus = self.store.focus_sites().is_empty();
        let perms = permissions::list();

        let scope = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                Self::small_button("safety-focus-only".into(), if focus_only { "● Only act on focus sites" } else { "Only act on focus sites" }, focus_only)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let now = {
                            let mut p = this.policy.lock().unwrap_or_else(|e| e.into_inner());
                            p.set_focus_only(!focus_only);
                            p.focus_only()
                        };
                        this.trace.push(
                            trace::Kind::Info,
                            if now { "Site scope: only act on focus sites" } else { "Site scope: act on any site" },
                            None,
                        );
                        cx.notify();
                    })),
            );
        let scope_help = if focus_only && no_focus {
            "On, but no sites are in focus yet: every click or entry asks first."
        } else if focus_only {
            "Agents can read and open any site, but clicking, typing or signing in elsewhere asks you first."
        } else {
            "Off: agents may act on any site (risky actions still ask)."
        };

        let chips = PURCHASE_LIMITS.iter().map(|(value, label)| {
            let value = *value;
            Self::small_button(SharedString::from(format!("safety-limit-{label}")), label, limit == value).on_click(
                cx.listener(move |this, _, _, cx| {
                    this.policy.lock().unwrap_or_else(|e| e.into_inner()).set_purchase_limit(value);
                    cx.notify();
                }),
            )
        });

        let perm_rows = perms.into_iter().map(|(origin, kind, allowed)| {
            let o = origin.clone();
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .text_xs()
                        .text_color(rgb(theme::FG))
                        .child(format!("{origin} · {} · {}", kind.label(), if allowed { "allowed" } else { "denied" })),
                )
                .child(
                    Self::small_button(SharedString::from(format!("perm-forget-{origin}-{}", kind.label())), "Forget", false)
                        .on_click(cx.listener(move |_, _, _, cx| {
                            permissions::forget(&o);
                            cx.notify();
                        })),
                )
        });
        let no_perms = permissions::list().is_empty();

        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .border_b_1()
            .border_color(rgb(theme::RULE))
            .child(div().text_xs().text_color(rgb(theme::ACCENT)).child("SAFETY"))
            .child(scope)
            .child(div().text_xs().text_color(rgb(theme::MUTED)).child(scope_help))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .child(div().text_xs().text_color(rgb(theme::FG)).child("Always ask for purchases over"))
                    .children(chips),
            )
            .child(div().text_xs().text_color(rgb(theme::MUTED)).child(format!(
                "Even on sites you've always allowed. Each turn stops after {} tool calls or {} actions that send, \
                 buy, post or delete (policy.json).",
                limits.max_calls_per_turn, limits.max_commits_per_turn
            )))
            .child(div().pt_1().text_xs().text_color(rgb(theme::FG)).child("Site permissions"))
            .when(no_perms, |d| {
                d.child(div().text_xs().text_color(rgb(theme::MUTED)).child(
                    "None yet. When a site asks for the camera, microphone, location or notifications, you choose on a card over the page.",
                ))
            })
            .children(perm_rows)
    }
}
