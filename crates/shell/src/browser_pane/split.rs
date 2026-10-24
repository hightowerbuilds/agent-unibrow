//! Split view: 2 to 4 tabs on screen at once, in a grid (two side by side,
//! three in columns, four as 2×2), each under a small header with its
//! title and a ✕ that takes it out of the split.
//!
//! The split is a group of tabs. It's on screen while the active tab is one
//! of them; clicking a tab outside it shows that tab alone, and clicking a
//! tab in it brings the grid back. The active tab is the focused pane: it
//! gets the keyboard and drives the toolbar, and a click in another pane
//! focuses that one. Ctrl+click in the tab strip adds a tab to the split or
//! takes it out; the toolbar's split button starts or ends one. The agent
//! sets one up with `browser_tabs` (split / unsplit).
//!
//! Each tab is laid out at its own pane's size, so the pages reflow to fit.

use super::*;
use gpui::AnyElement;

/// Most tabs in a split.
pub(super) const MAX_SPLIT: usize = 4;
/// Height of a pane's header, logical px.
const HEADER: f32 = 24.0;
/// Space between panes.
const GAP: f32 = 2.0;

/// Where a pane's page goes, in logical px from the page area's corner
/// (its header sits just above).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Cell {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Cell {
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// The grid for `n` panes (2 to 4) in a `w` × `h` page area.
pub(super) fn grid(n: usize, w: f32, h: f32) -> Vec<Cell> {
    let (cols, rows) = match n {
        2 => (2, 1),
        3 => (3, 1),
        _ => (2, 2),
    };
    let cw = ((w - GAP * (cols - 1) as f32) / cols as f32).floor().max(1.0);
    let ch = ((h - GAP * (rows - 1) as f32) / rows as f32).floor().max(1.0);
    (0..n.min(cols * rows))
        .map(|i| {
            let (c, r) = ((i % cols) as f32, (i / cols) as f32);
            Cell { x: c * (cw + GAP), y: r * (ch + GAP) + HEADER, w: cw, h: (ch - HEADER).max(1.0) }
        })
        .collect()
}

/// Paints a page frame (device pixels) at its logical size at `bounds`'
/// corner, and an open dropdown over it.
pub(super) fn paint_frame(
    bounds: Bounds<Pixels>,
    image: Option<Arc<RenderImage>>,
    popup: Option<(f32, f32, Arc<RenderImage>)>,
    window: &mut Window,
) {
    let Some(img) = image else { return };
    let s = window.scale_factor().max(0.01);
    let fs = img.size(0);
    let logical = size(px(fs.width.0 as f32 / s), px(fs.height.0 as f32 / s));
    let _ = window.paint_image(Bounds::new(bounds.origin, logical), Corners::default(), img, 0, false);
    if let Some((px_, py_, pimg)) = popup {
        let ps = pimg.size(0);
        let psize = size(px(ps.width.0 as f32 / s), px(ps.height.0 as f32 / s));
        let at = point(bounds.origin.x + px(px_), bounds.origin.y + px(py_));
        let _ = window.paint_image(Bounds::new(at, psize), Corners::default(), pimg, 0, false);
    }
}

impl BrowserPane {
    /// The tabs on screen side by side, in grid order; empty when one tab
    /// fills the page.
    pub(super) fn split_shown(&self) -> &[u32] {
        let active = self.tabs.get(self.active).map(|t| t.id);
        if self.split.len() >= 2 && !self.fullscreen && active.is_some_and(|a| self.split.contains(&a)) {
            &self.split
        } else {
            &[]
        }
    }

    /// Whether tab `id` is on screen (the active tab, or in the split shown).
    pub(super) fn on_screen(&self, id: u32) -> bool {
        self.tabs.get(self.active).is_some_and(|t| t.id == id) || self.split_shown().contains(&id)
    }

    /// Each tab on screen in split view, with its pane.
    pub(super) fn split_cells(&self) -> Vec<(usize, Cell)> {
        let shown = self.split_shown();
        let Some(b) = self.geometry.get().bounds.filter(|_| !shown.is_empty()) else { return Vec::new() };
        let cells = grid(shown.len(), f32::from(b.size.width), f32::from(b.size.height));
        shown.iter().zip(cells).filter_map(|(id, c)| Some((self.index_of(*id)?, c))).collect()
    }

    /// Tab `i`'s pane, or the whole page area.
    fn cell_of(&self, i: usize) -> Option<Cell> {
        let cells = self.split_cells();
        if cells.is_empty() {
            let b = self.geometry.get().bounds?;
            return (i == self.active).then(|| Cell { x: 0.0, y: 0.0, w: f32::from(b.size.width), h: f32::from(b.size.height) });
        }
        cells.into_iter().find(|(j, _)| *j == i).map(|(_, c)| c)
    }

    /// The tab under window point `p` and the point in its page, or `None`
    /// over a header or the space between panes.
    pub(super) fn hit(&self, p: gpui::Point<Pixels>) -> Option<(usize, f32, f32)> {
        let origin = self.geometry.get().bounds.map(|b| b.origin).unwrap_or_default();
        let (x, y) = (f32::from(p.x - origin.x), f32::from(p.y - origin.y));
        let cells = self.split_cells();
        if cells.is_empty() {
            return (self.active < self.tabs.len()).then_some((self.active, x, y));
        }
        cells.into_iter().find(|(_, c)| c.contains(x, y)).map(|(i, c)| (i, x - c.x, y - c.y))
    }

    /// Window point `p` in tab `i`'s page.
    pub(super) fn local_in(&self, i: usize, p: gpui::Point<Pixels>) -> (f32, f32) {
        let origin = self.geometry.get().bounds.map(|b| b.origin).unwrap_or_default();
        let c = self.cell_of(i).unwrap_or(Cell { x: 0.0, y: 0.0, w: 0.0, h: 0.0 });
        (f32::from(p.x - origin.x) - c.x, f32::from(p.y - origin.y) - c.y)
    }

    /// Lays each tab out at its size: its pane's in split view, the whole
    /// page area otherwise.
    pub(super) fn resize_tabs(&mut self, w: i32, h: i32, scale: f32) {
        let pct = (scale * 100.0) as u32;
        let cells = self.split_cells();
        for (i, t) in self.tabs.iter_mut().enumerate() {
            let wanted = match cells.iter().find(|(j, _)| *j == i) {
                Some((_, c)) => (c.w as i32, c.h as i32, pct),
                None => (w, h, pct),
            };
            if t.applied != wanted {
                t.view.resize(wanted.0, wanted.1, scale);
                t.applied = wanted;
            }
        }
    }

    /// After what's on screen changed: tabs that left the screen drop their
    /// last frame, so they show a fresh one (at the right size) next time.
    pub(super) fn screen_changed(&mut self, cx: &mut Context<Self>) {
        self.sync_visibility();
        let shown: Vec<u32> = self.tabs.iter().map(|t| t.id).filter(|id| self.on_screen(*id)).collect();
        for t in self.tabs.iter_mut().filter(|t| !shown.contains(&t.id)) {
            if let Some(old) = t.image.take() {
                self.retired.push(old);
            }
            t.last_seq = u64::MAX;
        }
        // Panes change size: lay the tabs out again on the next tick.
        if let Some((w, h, scale)) = self.size {
            self.resize_tabs(w, h, scale);
        }
        cx.notify();
    }

    /// Shows `ids` side by side (the agent's split, or the toolbar's),
    /// focusing the first.
    pub(super) fn set_split(&mut self, ids: Vec<u32>, cx: &mut Context<Self>) -> Result<(), String> {
        if ids.len() < 2 || ids.len() > MAX_SPLIT {
            return Err(format!("Split view shows 2 to {MAX_SPLIT} tabs."));
        }
        let first = self.index_of(ids[0]).ok_or_else(|| format!("No tab {}.", ids[0]))?;
        if let Some(id) = ids.iter().find(|id| self.index_of(**id).is_none()) {
            return Err(format!("No tab {id}."));
        }
        self.split = ids;
        if first != self.active {
            self.activate(first, cx);
        }
        self.screen_changed(cx);
        Ok(())
    }

    /// Back to one tab on screen, the focused one.
    pub(super) fn unsplit(&mut self, cx: &mut Context<Self>) {
        self.split.clear();
        self.screen_changed(cx);
    }

    /// A tab closed: it leaves the split, which ends with fewer than two.
    pub(super) fn split_forget(&mut self, id: u32) {
        self.split.retain(|s| *s != id);
        if self.split.len() < 2 {
            self.split.clear();
        }
    }

    /// Ctrl+click in the tab strip: tab `id` joins the split shown (or
    /// starts one with the active tab), or leaves it.
    pub(super) fn toggle_split_member(&mut self, id: u32, cx: &mut Context<Self>) {
        let Some(active) = self.tabs.get(self.active).map(|t| t.id) else { return };
        if self.split_shown().contains(&id) {
            self.split_forget(id);
            if id == active {
                // The focus goes to a pane still on screen.
                let next = self.split.first().copied().or_else(|| self.tabs.iter().map(|t| t.id).find(|t| *t != id));
                if let Some(i) = next.and_then(|n| self.index_of(n)) {
                    self.activate(i, cx);
                }
            }
        } else if id != active {
            if self.split_shown().is_empty() {
                self.split = vec![active];
            }
            if self.split.len() >= MAX_SPLIT {
                self.notice = Some((format!("Split view shows up to {MAX_SPLIT} tabs"), Instant::now()));
            } else {
                self.split.push(id);
            }
        }
        self.screen_changed(cx);
    }

    /// The toolbar's split button: ends the split shown, or shows the
    /// active tab next to its neighbor (a new tab if it's the only one).
    pub(super) fn toggle_split(&mut self, cx: &mut Context<Self>) {
        if !self.split_shown().is_empty() {
            self.unsplit(cx);
            return;
        }
        let Some(active) = self.tabs.get(self.active).map(|t| t.id) else { return };
        let other = self.tabs.get(self.active + 1).or_else(|| self.active.checked_sub(1).and_then(|i| self.tabs.get(i))).map(|t| t.id);
        let other = match other {
            Some(o) => o,
            None => match self.open_tab(HOME, false, cx) {
                Ok(o) => o,
                Err(_) => return,
            },
        };
        let _ = self.set_split(vec![active, other], cx);
    }

    /// The toolbar's split button: two boxes side by side.
    pub(super) fn split_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let on = !self.split_shown().is_empty();
        let color = rgb(if on { theme::ACCENT } else { theme::MUTED });
        let half = || div().w(px(7.)).h(px(12.)).border_1().border_color(color).rounded_sm();
        div()
            .id("split-view")
            .w(px(28.))
            .h(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .gap(px(2.))
            .rounded_md()
            .cursor_pointer()
            .when(on, |d| d.bg(rgb(theme::RAISED)))
            .hover(|s| s.bg(rgb(theme::RAISED)))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_split(cx)))
            .child(half())
            .child(half())
    }

    /// The panes of the split shown: each a header and the tab's page.
    pub(super) fn split_panes(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.split_cells()
            .into_iter()
            .map(|(i, c)| {
                let t = &self.tabs[i];
                let id = t.id;
                let focused = i == self.active;
                let cover = rail_browser::app_pages::is_app_page(&t.chrome.url) && !t.painted;
                let image = if cover { None } else { t.image.clone() };
                let popup = if focused { self.popup.clone() } else { None };
                let title = if t.chrome.title.is_empty() { t.chrome.url.clone() } else { t.chrome.title.clone() };
                let header = div()
                    .id(("split-header", id as usize))
                    .h(px(HEADER))
                    .flex()
                    .items_center()
                    .gap_1()
                    .pl_2()
                    .pr_1()
                    .text_xs()
                    .bg(rgb(if focused { theme::RAISED } else { theme::PANE }))
                    .text_color(rgb(if focused { theme::FG } else { theme::MUTED }))
                    .border_b_2()
                    .border_color(rgb(if focused { theme::ACCENT } else { theme::PANE }))
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(i) = this.index_of(id) {
                            this.activate(i, cx);
                        }
                    }))
                    .child(div().flex_1().min_w(px(0.)).truncate().child(SharedString::from(title)))
                    .child(
                        div()
                            .id(("split-close", id as usize))
                            .px_1()
                            .rounded_sm()
                            .text_color(rgb(theme::MUTED))
                            .hover(|s| s.bg(rgb(theme::RULE)).text_color(rgb(theme::FG)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.toggle_split_member(id, cx);
                            }))
                            .child("✕"),
                    );
                div()
                    .absolute()
                    .left(px(c.x))
                    .top(px(c.y - HEADER))
                    .w(px(c.w))
                    .h(px(c.h + HEADER))
                    .flex()
                    .flex_col()
                    .bg(rgb(theme::BG))
                    .child(header)
                    .child(
                        div().flex_1().bg(rgb(if cover { theme::BG } else { 0xffffff })).child(
                            canvas(|_, _, _| {}, move |bounds, _, window, _| paint_frame(bounds, image, popup, window))
                                .size_full(),
                        ),
                    )
                    .into_any_element()
            })
            .collect()
    }

    /// Wraps overlays that belong to the focused tab (dialogs, the crash
    /// screen, the context menu) so they sit over its pane.
    pub(super) fn over_focused(&self, children: Vec<AnyElement>) -> AnyElement {
        let cell = self.split_cells().into_iter().find(|(i, _)| *i == self.active).map(|(_, c)| c);
        match cell {
            Some(c) => div().absolute().left(px(c.x)).top(px(c.y)).w(px(c.w)).h(px(c.h)).children(children).into_any_element(),
            None => div().absolute().top_0().left_0().size_full().children(children).into_any_element(),
        }
    }

    /// Where tab `i`'s page starts in the page area (its pane's corner).
    pub(super) fn page_origin(&self, i: usize) -> Option<(f32, f32)> {
        self.cell_of(i).map(|c| (c.x, c.y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grids() {
        let two = grid(2, 1002., 800.);
        assert_eq!(two.len(), 2);
        assert_eq!((two[0].x, two[0].w), (0., 500.));
        assert_eq!((two[1].x, two[1].y, two[1].h), (502., HEADER, 800. - HEADER));
        let three = grid(3, 904., 600.);
        assert_eq!(three.iter().map(|c| c.x).collect::<Vec<_>>(), [0., 302., 604.]);
        let four = grid(4, 802., 602.);
        assert_eq!(four.iter().map(|c| (c.x, c.y)).collect::<Vec<_>>(), [(0., HEADER), (402., HEADER), (0., 302. + HEADER), (402., 302. + HEADER)]);
        assert!(four.iter().all(|c| c.w == 400. && c.h == 300. - HEADER));
        // Point lookups: the header and gaps aren't in any page.
        assert!(two[1].contains(600., 100.));
        assert!(!two[1].contains(600., 10.));
        assert!(!two[0].contains(501., 100.) && !two[1].contains(501., 100.));
    }
}
