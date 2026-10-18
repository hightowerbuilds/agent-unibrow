//! Selectable chat text.
//!
//! Each message's text is drawn by a [`SelectableText`] element, which lays
//! out and paints like a plain string child (same wrapping, same look) and
//! also records its layout in a shared [`Registry`] as it's prepainted. The
//! conversation's mouse handlers hit-test that registry to turn a pointer
//! position into a [`ChatPoint`] (message index, byte offset), so a
//! selection can start in one message and end in another. [`ChatSelect`]
//! holds the selection and the drag/menu state; the shell owns it.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    App, AvailableSpace, Bounds, ClipboardItem, Context, DispatchPhase, ElementId, Focusable, GlobalElementId,
    KeyBinding, KeyDownEvent, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    SharedString, WhiteSpace, Window, WrappedLine, actions, anchored, canvas, deferred, div, fill, point,
    prelude::*, px, rgb, rgba,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::{Shell, theme};

actions!(chat_text, [CopySelection, SelectAll, PasteToInput]);

/// Key context of the focused conversation.
pub const CONTEXT: &str = "ChatLog";

/// Selection highlight: the accent at 25%, as in the text input.
pub fn selection_color() -> gpui::Rgba {
    rgba((theme::ACCENT << 8) | 0x40)
}

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-c", CopySelection, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-c", CopySelection, Some(CONTEXT)),
        KeyBinding::new("ctrl-insert", CopySelection, Some(CONTEXT)),
        KeyBinding::new("ctrl-a", SelectAll, Some(CONTEXT)),
        KeyBinding::new("ctrl-v", PasteToInput, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-v", PasteToInput, Some(CONTEXT)),
        KeyBinding::new("shift-insert", PasteToInput, Some(CONTEXT)),
    ]);
}

// ------------------------------------------------------------ selection

/// A position in the conversation: message `item`, byte `offset` into its
/// text. Orders by message, then offset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChatPoint {
    pub item: usize,
    pub offset: usize,
}

impl ChatPoint {
    pub fn new(item: usize, offset: usize) -> Self {
        Self { item, offset }
    }
}

/// Where the selection was started (`anchor`) and where it's been dragged
/// or extended to (`head`). Either may come first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: ChatPoint,
    pub head: ChatPoint,
}

impl Selection {
    pub fn caret(at: ChatPoint) -> Self {
        Self { anchor: at, head: at }
    }

    pub fn start(&self) -> ChatPoint {
        self.anchor.min(self.head)
    }

    pub fn end(&self) -> ChatPoint {
        self.anchor.max(self.head)
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// The selected part of message `item` (whose text is `text`), if any.
    pub fn range_in(&self, item: usize, text: &str) -> Option<Range<usize>> {
        if self.is_empty() {
            return None;
        }
        let (s, e) = (self.start(), self.end());
        if item < s.item || item > e.item {
            return None;
        }
        let a = if item == s.item { clamp(text, s.offset) } else { 0 };
        let b = if item == e.item { clamp(text, e.offset) } else { text.len() };
        (a < b).then_some(a..b)
    }
}

/// `offset` clamped to `text` and moved back onto a char boundary.
pub fn clamp(text: &str, offset: usize) -> usize {
    let mut o = offset.min(text.len());
    while !text.is_char_boundary(o) {
        o -= 1;
    }
    o
}

/// The selected text, given each message's text by index. Pieces from
/// different messages are separated by a blank line.
pub fn selected_text<'a>(sel: &Selection, text_of: impl Fn(usize) -> Option<&'a str>) -> String {
    let (s, e) = (sel.start(), sel.end());
    let mut pieces = vec![];
    for item in s.item..=e.item {
        let Some(text) = text_of(item) else { continue };
        if let Some(r) = sel.range_in(item, text) {
            pieces.push(&text[r]);
        }
    }
    pieces.join("\n\n")
}

/// The word (or run of spaces/punctuation) at `offset`, for double-click.
pub fn word_at(text: &str, offset: usize) -> Range<usize> {
    let offset = clamp(text, offset);
    let mut last = offset..offset;
    for (start, word) in text.split_word_bound_indices() {
        let end = start + word.len();
        if offset < end {
            // A click just past a word's end (on its right edge) picks that
            // word rather than the space after it.
            if offset == start && word.trim().is_empty() && last.end == start && last.start < last.end {
                return last;
            }
            return start..end;
        }
        last = start..end;
    }
    last
}

/// The line (between newlines) at `offset`, for triple-click.
pub fn line_at(text: &str, offset: usize) -> Range<usize> {
    let offset = clamp(text, offset);
    let start = text[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end = text[offset..].find('\n').map(|i| offset + i).unwrap_or(text.len());
    start..end
}

/// Where a vertical position falls among stacked spans (each `(top,
/// bottom)`, in order): inside span `i`, or in a gap, nearer the end of
/// one span or the start of the next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spot {
    Start(usize),
    Inside(usize),
    End(usize),
}

pub fn pick<T: PartialOrd + Copy + std::ops::Sub<Output = T>>(spans: &[(T, T)], y: T) -> Option<Spot> {
    if y < spans.first()?.0 {
        return Some(Spot::Start(0));
    }
    for (i, &(top, bottom)) in spans.iter().enumerate() {
        if y >= top && y <= bottom {
            return Some(Spot::Inside(i));
        }
        if let Some(&(next_top, _)) = spans.get(i + 1) {
            if y > bottom && y < next_top {
                return Some(if y - bottom <= next_top - y { Spot::End(i) } else { Spot::Start(i + 1) });
            }
        }
    }
    Some(Spot::End(spans.len() - 1))
}

/// A word/line selection `picked` (start, end) extended by a drag to `at`,
/// whose enclosing word/line is `unit`: whole units, keeping `picked`.
pub fn extend_by_unit(picked: (ChatPoint, ChatPoint), at: ChatPoint, unit: Range<usize>) -> Selection {
    let (a, b) = picked;
    if at < a {
        Selection { anchor: b, head: ChatPoint::new(at.item, unit.start) }
    } else {
        Selection { anchor: a, head: ChatPoint::new(at.item, unit.end).max(b) }
    }
}

/// The conversation's selection, drag and menu state.
#[derive(Default)]
pub struct ChatSelect {
    pub selection: Option<Selection>,
    /// The left button is down and dragging extends the selection.
    pub dragging: bool,
    /// What a drag extends by: chars, or whole words/lines after a double
    /// or triple click (the range first picked stays selected).
    granularity: Granularity,
    picked: Option<(ChatPoint, ChatPoint)>,
    /// The right-click menu: where, and the message under the pointer.
    pub menu: Option<(Point<Pixels>, Option<usize>)>,
    /// The message whose Copy button was just used ("Copied").
    pub copied: Option<usize>,
    pub registry: Registry,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Granularity {
    #[default]
    Char,
    Word,
    Line,
}

impl ChatSelect {
    pub fn clear(&mut self) {
        self.selection = None;
        self.dragging = false;
        self.picked = None;
        self.menu = None;
        self.copied = None;
    }

    pub fn has_selection(&self) -> bool {
        self.selection.is_some_and(|s| !s.is_empty())
    }

    pub fn range_for(&self, item: usize, text: &str) -> Option<Range<usize>> {
        self.selection.and_then(|s| s.range_in(item, text))
    }

    pub fn text<'a>(&self, text_of: impl Fn(usize) -> Option<&'a str>) -> Option<String> {
        let sel = self.selection.filter(|s| !s.is_empty())?;
        Some(selected_text(&sel, text_of)).filter(|t| !t.is_empty())
    }

    /// A left-button press. `click_count` 2 picks a word, 3 a line; shift
    /// extends the current selection.
    pub fn mouse_down<'a>(
        &mut self,
        position: Point<Pixels>,
        click_count: usize,
        shift: bool,
        text_of: impl Fn(usize) -> Option<&'a str>,
    ) {
        self.menu = None;
        let Some((at, _)) = self.registry.hit(position) else {
            self.selection = None;
            self.dragging = false;
            return;
        };
        self.dragging = true;
        if shift && click_count == 1 {
            if let Some(sel) = self.selection.as_mut() {
                sel.head = at;
                self.granularity = Granularity::Char;
                self.picked = None;
                return;
            }
        }
        let text = text_of(at.item).unwrap_or("");
        let range = match click_count {
            1 => {
                self.granularity = Granularity::Char;
                self.picked = None;
                self.selection = Some(Selection::caret(at));
                return;
            }
            2 => {
                self.granularity = Granularity::Word;
                word_at(text, at.offset)
            }
            _ => {
                self.granularity = Granularity::Line;
                line_at(text, at.offset)
            }
        };
        let (a, b) = (ChatPoint::new(at.item, range.start), ChatPoint::new(at.item, range.end));
        self.picked = Some((a, b));
        self.selection = Some(Selection { anchor: a, head: b });
    }

    /// The pointer moved with the button down. Returns whether the
    /// selection changed.
    pub fn drag_to<'a>(&mut self, position: Point<Pixels>, text_of: impl Fn(usize) -> Option<&'a str>) -> bool {
        if !self.dragging {
            return false;
        }
        let Some((at, _)) = self.registry.hit(position) else { return false };
        let before = self.selection;
        let Some(sel) = self.selection.as_mut() else { return false };
        match (self.granularity, self.picked) {
            (Granularity::Char, _) | (_, None) => sel.head = at,
            (g, Some((a, b))) => {
                let text = text_of(at.item).unwrap_or("");
                let unit = if g == Granularity::Word { word_at(text, at.offset) } else { line_at(text, at.offset) };
                *sel = extend_by_unit((a, b), at, unit);
            }
        }
        self.selection != before
    }

    pub fn mouse_up(&mut self) {
        self.dragging = false;
    }

    /// Everything from the first message to the end of the last.
    pub fn select_all(&mut self, lens: &[usize]) {
        let Some(last) = lens.len().checked_sub(1) else { return };
        self.selection = Some(Selection { anchor: ChatPoint::new(0, 0), head: ChatPoint::new(last, lens[last]) });
        self.menu = None;
    }

    /// The message right under a window position (for the right-click
    /// menu's Copy message).
    pub fn message_at(&self, position: Point<Pixels>) -> Option<usize> {
        self.registry.hit(position).and_then(|(at, inside)| inside.then_some(at.item))
    }
}

// --------------------------------------------------------------- layout

/// A message's shaped text: one wrapped line per paragraph.
struct Laid {
    lines: Vec<WrappedLine>,
    line_height: Pixels,
    wrap_width: Option<Pixels>,
    size: gpui::Size<Pixels>,
}

impl Laid {
    /// Byte offset where each paragraph starts.
    fn starts(&self) -> impl Iterator<Item = usize> + '_ {
        self.lines.iter().scan(0, |next, line| {
            let start = *next;
            *next += line.len() + 1;
            Some(start)
        })
    }

    fn len(&self) -> usize {
        (self.lines.iter().map(|l| l.len() + 1).sum::<usize>()).saturating_sub(1)
    }

    /// The text offset nearest a point relative to the text's top-left.
    fn index_for(&self, at: Point<Pixels>) -> usize {
        let lh = self.line_height;
        if at.y < px(0.) {
            return 0;
        }
        let mut top = px(0.);
        for (line, start) in self.lines.iter().zip(self.starts()) {
            let height = line.size(lh).height;
            if at.y < top + height {
                let local = point(at.x.max(px(0.)), at.y - top);
                return start + line.closest_index_for_position(local, lh).unwrap_or_else(|i| i).min(line.len());
            }
            top += height;
        }
        self.len()
    }

    /// Highlight quads for `range`, with the text's top-left at `bounds`.
    fn selection_quads(&self, range: &Range<usize>, bounds: Bounds<Pixels>) -> Vec<gpui::PaintQuad> {
        let lh = self.line_height;
        let color = selection_color();
        let (left, right) = (bounds.left(), bounds.right());
        let mut quads = vec![];
        let mut top = bounds.top();
        for (line, start) in self.lines.iter().zip(self.starts()) {
            let end = start + line.len();
            let height = line.size(lh).height;
            // The newline after this paragraph is selected too.
            let newline = range.start <= end && range.end > end;
            if range.end > start && range.start <= end && (range.start < range.end) {
                let a = range.start.max(start) - start;
                let b = range.end.min(end) - start;
                let pa = line.position_for_index(a, lh).unwrap_or_default();
                let pb = line.position_for_index(b, lh).unwrap_or_default();
                let tail = if newline { px(6.) } else { px(0.) };
                let quad = |x0: Pixels, x1: Pixels, y: Pixels| {
                    fill(Bounds::from_corners(point(x0, top + y), point(x1, top + y + lh)), color)
                };
                if pa.y == pb.y {
                    if b > a || newline {
                        quads.push(quad(left + pa.x, (left + pb.x + tail).min(right.max(left + pb.x)), pa.y));
                    }
                } else {
                    quads.push(quad(left + pa.x, right, pa.y));
                    if pb.y - pa.y > lh {
                        quads.push(fill(
                            Bounds::from_corners(point(left, top + pa.y + lh), point(right, top + pb.y)),
                            color,
                        ));
                    }
                    quads.push(quad(left, left + pb.x + tail, pb.y));
                }
            }
            top += height;
        }
        quads
    }
}

type SharedLaid = Rc<RefCell<Option<Laid>>>;

struct Entry {
    item: usize,
    bounds: Bounds<Pixels>,
    laid: SharedLaid,
}

/// The message texts as last prepainted, for hit-testing. Cleared each
/// time the conversation renders and refilled as its texts prepaint.
#[derive(Clone, Default)]
pub struct Registry(Rc<RefCell<Vec<Entry>>>);

impl Registry {
    pub fn clear(&self) {
        self.0.borrow_mut().clear();
    }


    /// Texts prepaint top to bottom, so entries stay in message order.
    fn record(&self, item: usize, bounds: Bounds<Pixels>, laid: SharedLaid) {
        self.0.borrow_mut().push(Entry { item, bounds, laid });
    }

    /// The conversation position under (or nearest) a window position, and
    /// whether it's right on a message's text.
    pub fn hit(&self, position: Point<Pixels>) -> Option<(ChatPoint, bool)> {
        let entries = self.0.borrow();
        let spans: Vec<(Pixels, Pixels)> = entries.iter().map(|e| (e.bounds.top(), e.bounds.bottom())).collect();
        let spot = pick(&spans, position.y)?;
        let (i, inside) = match spot {
            Spot::Start(i) | Spot::End(i) => (i, false),
            Spot::Inside(i) => (i, true),
        };
        let e = &entries[i];
        let laid = e.laid.borrow();
        let len = laid.as_ref().map(Laid::len).unwrap_or(0);
        let offset = match spot {
            Spot::Start(_) => 0,
            Spot::End(_) => len,
            Spot::Inside(_) => {
                let local = point(position.x - e.bounds.left(), position.y - e.bounds.top());
                laid.as_ref().map(|l| l.index_for(local)).unwrap_or(0)
            }
        };
        let inside = inside && position.x >= e.bounds.left() && position.x <= e.bounds.right();
        Some((ChatPoint::new(e.item, offset), inside))
    }
}

// -------------------------------------------------------------- element

/// A message's text, selectable. Draws exactly like a plain string child
/// when nothing in it is selected.
pub struct SelectableText {
    item: usize,
    text: SharedString,
    selected: Option<Range<usize>>,
    registry: Registry,
    laid: SharedLaid,
}

pub fn selectable(
    item: usize,
    text: impl Into<SharedString>,
    selected: Option<Range<usize>>,
    registry: Registry,
) -> SelectableText {
    SelectableText { item, text: text.into(), selected, registry, laid: Default::default() }
}

impl IntoElement for SelectableText {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SelectableText {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    // Mirrors GPUI's TextLayout, so a message wraps and sizes as before.
    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = style.line_height.to_pixels(font_size.into(), window.rem_size());
        let runs = vec![style.to_run(self.text.len())];
        let text = self.text.clone();
        let laid = self.laid.clone();
        let id = window.request_measured_layout(Default::default(), move |known, available, window, _| {
            let wrap_width = if style.white_space == WhiteSpace::Normal {
                known.width.or(match available.width {
                    AvailableSpace::Definite(w) => Some(w),
                    _ => None,
                })
            } else {
                None
            };
            if let Some(l) = laid.borrow().as_ref() {
                if wrap_width.is_none() || wrap_width == l.wrap_width {
                    return l.size;
                }
            }
            let lines: Vec<WrappedLine> = window
                .text_system()
                .shape_text(text.clone(), font_size, &runs, wrap_width, None)
                .map(|ls| ls.into_iter().collect())
                .unwrap_or_default();
            let mut size = gpui::Size::<Pixels>::default();
            for line in &lines {
                let s = line.size(line_height);
                size.height += s.height;
                size.width = size.width.max(s.width).ceil();
            }
            *laid.borrow_mut() = Some(Laid { lines, line_height, wrap_width, size });
            size
        });
        let _ = cx;
        (id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) {
        self.registry.record(self.item, bounds, self.laid.clone());
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let laid = self.laid.borrow();
        let Some(laid) = laid.as_ref() else { return };
        if let Some(range) = &self.selected {
            for quad in laid.selection_quads(range, bounds) {
                window.paint_quad(quad);
            }
        }
        let lh = laid.line_height;
        let align = window.text_style().text_align;
        let mut origin = bounds.origin;
        for line in &laid.lines {
            line.paint_background(origin, lh, align, Some(bounds), window, cx).ok();
            line.paint(origin, lh, align, Some(bounds), window, cx).ok();
            origin.y += line.size(lh).height;
        }
    }
}

// ------------------------------------------------------ shell handlers

impl Shell {
    pub(crate) fn chat_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.chat_focus);
        let items = &self.items;
        self.chat_select.mouse_down(ev.position, ev.click_count, ev.modifiers.shift, |i| {
            items.get(i).map(|x| x.text.as_str())
        });
        cx.notify();
    }

    pub(crate) fn chat_right_click(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.chat_focus);
        let item = self.chat_select.message_at(ev.position);
        self.chat_select.menu = Some((ev.position, item));
        cx.notify();
    }

    /// Follows a drag anywhere in the window (not just over the
    /// conversation) and ends it on release.
    pub(crate) fn chat_drag_tracker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let me = cx.entity().downgrade();
        canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                let m = me.clone();
                window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    let _ = m.update(cx, |this, cx| {
                        if !this.chat_select.dragging {
                            return;
                        }
                        if ev.pressed_button != Some(MouseButton::Left) {
                            // The release happened somewhere we didn't see.
                            this.chat_select.mouse_up();
                            return;
                        }
                        let items = &this.items;
                        if this.chat_select.drag_to(ev.position, |i| items.get(i).map(|x| x.text.as_str())) {
                            cx.notify();
                        }
                    });
                });
                let m = me.clone();
                window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, cx| {
                    if phase != DispatchPhase::Bubble || ev.button != MouseButton::Left {
                        return;
                    }
                    let _ = m.update(cx, |this, cx| {
                        if !this.chat_select.dragging {
                            return;
                        }
                        this.chat_select.mouse_up();
                        // Linux: a selection is also the primary selection
                        // (middle-click pastes it).
                        let items = &this.items;
                        if let Some(text) = this.chat_select.text(|i| items.get(i).map(|x| x.text.as_str())) {
                            cx.write_to_primary(ClipboardItem::new_string(text));
                        }
                    });
                });
            },
        )
        .absolute()
        .size_0()
    }

    /// Copies the selected chat text; false if nothing is selected.
    pub(crate) fn copy_chat_text(&mut self, cx: &mut Context<Self>) -> bool {
        let items = &self.items;
        match self.chat_select.text(|i| items.get(i).map(|x| x.text.as_str())) {
            Some(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                true
            }
            None => false,
        }
    }

    pub(crate) fn copy_chat_selection(&mut self, _: &CopySelection, _: &mut Window, cx: &mut Context<Self>) {
        if !self.copy_chat_text(cx) {
            cx.propagate();
        }
    }

    pub(crate) fn select_all_chat(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        let lens: Vec<usize> = self.items.iter().map(|i| i.text.len()).collect();
        self.chat_select.select_all(&lens);
        cx.notify();
    }

    /// Ctrl+V with the conversation focused pastes into the chat box.
    pub(crate) fn paste_into_chat_input(&mut self, _: &PasteToInput, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.input.focus_handle(cx));
        if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
            self.input.update(cx, |input, cx| input.insert_text(&text, cx));
        }
    }

    /// Typing with the conversation focused goes to the chat box.
    pub(crate) fn chat_key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = &ev.keystroke.modifiers;
        if m.control || m.alt || m.platform || m.function {
            return;
        }
        let Some(typed) = ev.keystroke.key_char.clone().filter(|c| !c.is_empty() && !c.chars().any(char::is_control))
        else {
            return;
        };
        window.focus(&self.input.focus_handle(cx));
        self.input.update(cx, |input, cx| input.insert_text(&typed, cx));
        cx.stop_propagation();
    }

    /// The Copy button / menu item: a message's whole text.
    pub(crate) fn copy_message(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(item) = self.items.get(ix) else { return };
        cx.write_to_clipboard(ClipboardItem::new_string(item.text.clone()));
        self.chat_select.copied = Some(ix);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(1500)).await;
            let _ = this.update(cx, |this, cx| {
                if this.chat_select.copied == Some(ix) {
                    this.chat_select.copied = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The right-click menu: Copy, Copy message, Select all.
    pub(crate) fn render_chat_menu(
        &self,
        at: Point<Pixels>,
        item: Option<usize>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let has_selection = self.chat_select.has_selection();
        let any = !self.items.is_empty();
        let row = |id: &'static str, label: &'static str, keys: &'static str, enabled: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_4()
                .px_3()
                .py_1()
                .rounded_sm()
                .text_sm()
                .text_color(rgb(if enabled { theme::FG } else { theme::MUTED }))
                .when(enabled, |d| d.cursor_pointer().hover(|s| s.bg(rgb(theme::RULE))))
                .child(div().flex_1().child(label))
                .child(div().text_xs().text_color(rgb(theme::MUTED)).child(keys))
        };
        fn close(this: &mut Shell, cx: &mut Context<Shell>) {
            this.chat_select.menu = None;
            cx.notify();
        }
        deferred(
            anchored().position(at).snap_to_window().child(
                div()
                    .id("chat-menu")
                    .occlude()
                    .min_w(px(180.))
                    .p_1()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(theme::RULE))
                    .bg(rgb(theme::RAISED))
                    .shadow_md()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| close(this, cx)))
                    .child(row("chat-menu-copy", "Copy", "Ctrl+C", has_selection).when(has_selection, |d| {
                        d.on_click(cx.listener(|this, _, _, cx| {
                            this.copy_chat_text(cx);
                            close(this, cx);
                        }))
                    }))
                    .when_some(item, |d, ix| {
                        d.child(row("chat-menu-copy-message", "Copy message", "", true).on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.copy_message(ix, cx);
                                close(this, cx);
                            },
                        )))
                    })
                    .child(row("chat-menu-select-all", "Select all", "Ctrl+A", any).when(any, |d| {
                        d.on_click(cx.listener(|this, _, window, cx| {
                            this.select_all_chat(&SelectAll, window, cx);
                            close(this, cx);
                        }))
                    })),
            ),
        )
        .with_priority(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(a: (usize, usize), h: (usize, usize)) -> Selection {
        Selection { anchor: ChatPoint::new(a.0, a.1), head: ChatPoint::new(h.0, h.1) }
    }

    #[test]
    fn range_within_one_message_either_direction() {
        let text = "hello world";
        assert_eq!(sel((0, 2), (0, 7)).range_in(0, text), Some(2..7));
        assert_eq!(sel((0, 7), (0, 2)).range_in(0, text), Some(2..7));
        assert_eq!(sel((0, 3), (0, 3)).range_in(0, text), None);
        assert_eq!(sel((0, 2), (0, 7)).range_in(1, text), None);
    }

    #[test]
    fn range_across_messages() {
        let s = sel((3, 4), (1, 2));
        assert_eq!(s.range_in(0, "abcdef"), None);
        assert_eq!(s.range_in(1, "abcdef"), Some(2..6));
        assert_eq!(s.range_in(2, "middle"), Some(0..6));
        assert_eq!(s.range_in(3, "abcdef"), Some(0..4));
        assert_eq!(s.range_in(4, "abcdef"), None);
        // Starting at the very end of a message selects nothing of it.
        assert_eq!(sel((1, 6), (2, 3)).range_in(1, "abcdef"), None);
    }

    #[test]
    fn offsets_clamp_to_text_and_char_boundaries() {
        assert_eq!(clamp("héllo", 2), 1); // inside 'é'
        assert_eq!(clamp("abc", 99), 3);
        // A message that got shorter (or a stale offset) doesn't panic.
        assert_eq!(sel((0, 0), (0, 50)).range_in(0, "abc"), Some(0..3));
    }

    #[test]
    fn copies_selected_text_joining_messages() {
        let items = ["first message", "second", "third one"];
        let text_of = |i: usize| items.get(i).copied();
        assert_eq!(selected_text(&sel((0, 6), (0, 13)), text_of), "message");
        assert_eq!(selected_text(&sel((2, 5), (0, 6)), text_of), "message\n\nsecond\n\nthird");
        // A message whose part is empty is skipped, not a blank piece.
        assert_eq!(selected_text(&sel((0, 13), (1, 3)), text_of), "sec");
    }

    #[test]
    fn word_boundaries() {
        let t = "copy the URL: https://x.io/a now";
        assert_eq!(&t[word_at(t, 0)], "copy");
        assert_eq!(&t[word_at(t, 2)], "copy");
        // Right at a word's end picks the word, not the following space.
        assert_eq!(&t[word_at(t, 4)], "copy");
        assert_eq!(&t[word_at(t, 6)], "the");
        assert_eq!(&t[word_at(t, 10)], "URL");
        assert_eq!(&t[word_at(t, t.len())], "now");
        assert_eq!(word_at("", 0), 0..0);
        let u = "naïve café";
        assert_eq!(&u[word_at(u, 3)], "naïve");
    }

    #[test]
    fn line_boundaries() {
        let t = "one\ntwo three\nfour";
        assert_eq!(&t[line_at(t, 0)], "one");
        assert_eq!(&t[line_at(t, 3)], "one");
        assert_eq!(&t[line_at(t, 4)], "two three");
        assert_eq!(&t[line_at(t, 9)], "two three");
        assert_eq!(&t[line_at(t, t.len())], "four");
        assert_eq!(line_at("single", 2), 0..6);
    }

    #[test]
    fn picks_spans_and_gaps() {
        let spans = [(0., 20.), (32., 72.), (84., 104.)];
        assert_eq!(pick::<f32>(&[], 5.), None);
        assert_eq!(pick(&spans, -3.), Some(Spot::Start(0)));
        assert_eq!(pick(&spans, 10.), Some(Spot::Inside(0)));
        assert_eq!(pick(&spans, 22.), Some(Spot::End(0)));
        assert_eq!(pick(&spans, 30.), Some(Spot::Start(1)));
        assert_eq!(pick(&spans, 50.), Some(Spot::Inside(1)));
        assert_eq!(pick(&spans, 104.), Some(Spot::Inside(2)));
        assert_eq!(pick(&spans, 400.), Some(Spot::End(2)));
    }

    #[test]
    fn word_drags_extend_by_whole_words() {
        let t = "alpha beta gamma";
        let picked = (ChatPoint::new(0, 6), ChatPoint::new(0, 10)); // "beta"
        // Forward into "gamma": through its end.
        let s = extend_by_unit(picked, ChatPoint::new(0, 12), word_at(t, 12));
        assert_eq!(s.range_in(0, t).map(|r| &t[r]), Some("beta gamma"));
        // Backward into "alpha": from its start, keeping "beta".
        let s = extend_by_unit(picked, ChatPoint::new(0, 2), word_at(t, 2));
        assert_eq!(s.range_in(0, t).map(|r| &t[r]), Some("alpha beta"));
        // Inside the picked word: just the word.
        let s = extend_by_unit(picked, ChatPoint::new(0, 7), word_at(t, 7));
        assert_eq!(s.range_in(0, t).map(|r| &t[r]), Some("beta"));
        // Into a later message.
        let s = extend_by_unit(picked, ChatPoint::new(1, 1), word_at("next one", 1));
        assert_eq!((s.start(), s.end()), (ChatPoint::new(0, 6), ChatPoint::new(1, 4)));
    }

    #[test]
    fn select_all_spans_every_message() {
        let mut cs = ChatSelect::default();
        cs.select_all(&[]);
        assert!(cs.selection.is_none());
        cs.select_all(&[5, 0, 9]);
        let items = ["hello", "", "last item"];
        assert_eq!(cs.text(|i| items.get(i).copied()).as_deref(), Some("hello\n\nlast item"));
    }
}
