//! Pop out: the sidebar as a tab of its own, first in the tab strip.
//!
//! The shell hands over its sidebar view ([`BrowserPane::set_sidebar_tab`]);
//! while that tab is selected the pane shows it in place of the toolbar and
//! page, and the web tabs keep running (live, off screen). Clicking a web
//! tab shows pages again; the sidebar tab's × emits [`DockSidebar`] and the
//! shell puts the sidebar back beside the page. The page comes forward on
//! its own when it needs the user (a handoff) or the user opens an address.

use super::*;
use gpui::{AnyElement, AnyView, StyleRefinement};

impl BrowserPane {
    /// Pop out (`Some`): the sidebar becomes a tab, shown at once. `None`
    /// takes the tab away.
    pub fn set_sidebar_tab(&mut self, view: Option<AnyView>, cx: &mut Context<Self>) {
        self.sidebar_shown = view.is_some();
        self.sidebar_tab = view;
        self.sync_visibility();
        cx.notify();
    }

    /// The sidebar tab is the one on screen.
    pub fn sidebar_tab_shown(&self) -> bool {
        self.sidebar_shown && self.sidebar_tab.is_some()
    }

    /// Selects the sidebar tab.
    fn show_sidebar_tab(&mut self, cx: &mut Context<Self>) {
        if self.sidebar_tab.is_some() && !self.sidebar_shown {
            self.sidebar_shown = true;
            self.close_menu(cx);
            self.sync_visibility();
            cx.notify();
        }
    }

    /// Back to the web pages from the sidebar tab.
    pub(super) fn show_page(&mut self, cx: &mut Context<Self>) {
        if self.sidebar_shown {
            self.sidebar_shown = false;
            self.sync_visibility();
            cx.notify();
        }
    }

    /// The sidebar tab in the strip, while the sidebar is popped out.
    pub(super) fn sidebar_tab_view(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        self.sidebar_tab.as_ref()?;
        let shown = self.sidebar_shown;
        Some(
            div()
                .id("tab-sidebar")
                .flex()
                .items_center()
                .gap_1()
                .w(px(150.))
                .min_w(px(60.))
                .flex_shrink()
                .pl_2()
                .pr_1()
                .py_1()
                .rounded_t_md()
                .text_xs()
                .bg(rgb(if shown { theme::PANE } else { theme::BG }))
                .text_color(rgb(if shown { theme::FG } else { theme::MUTED }))
                .border_t_2()
                .border_color(rgb(theme::ACCENT))
                .when(!shown, |d| d.cursor_pointer().hover(|s| s.bg(rgb(theme::RAISED))))
                .on_click(cx.listener(|this, _, _, cx| this.show_sidebar_tab(cx)))
                .child(div().flex_none().text_color(rgb(theme::ACCENT)).child("☰"))
                .child(div().flex_1().min_w(px(0.)).truncate().child("Sidebar"))
                .child(
                    div()
                        .id("tab-sidebar-close")
                        .px_1()
                        .rounded_sm()
                        .text_color(rgb(theme::MUTED))
                        .hover(|s| s.bg(rgb(theme::RULE)).text_color(rgb(theme::FG)))
                        .on_click(cx.listener(|_, _, _, cx| {
                            cx.stop_propagation();
                            cx.emit(DockSidebar);
                        }))
                        .child("×"),
                ),
        )
    }

    /// The sidebar filling the page area, while its tab is selected.
    pub(super) fn sidebar_tab_body(&self) -> Option<AnyElement> {
        let view = self.sidebar_tab.clone().filter(|_| self.sidebar_shown)?;
        Some(div().flex_1().min_h(px(0.)).w_full().child(view.cached(StyleRefinement::default().size_full())).into_any_element())
    }
}
