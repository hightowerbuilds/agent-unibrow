//! Text input, adapted from GPUI's `input` example.
//!
//! Changes from the example: Enter emits [`Submit`], Linux (`ctrl-`)
//! shortcuts, theme colors, and a masked mode for secrets. Masked text is
//! drawn as one bullet per character; offsets are mapped between the
//! content and the drawn line so the cursor and mouse still line up.
//!
//! A wrapping input ([`TextInput::set_wrap`], the chat box) breaks long text
//! onto new lines and grows up to [`MAX_LINES`] tall, then scrolls to keep
//! the cursor in view.

use std::ops::Range;

use gpui::{
    App, AvailableSpace, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, KeyBinding, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ShapedLine,
    SharedString, Style, TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window, WrappedLine, actions,
    div, fill, point, prelude::*, px, relative, rgb, rgba, size,
};
use unicode_segmentation::*;

use crate::theme;

actions!(
    text_input,
    [
        Backspace, Delete, Left, Right, Up, Down, SelectLeft, SelectRight, SelectAll, Home, End, SelectHome,
        SelectEnd, Paste, Cut, Copy, Enter
    ]
);

const BULLET: &str = "•";
/// A wrapping input grows to this many lines, then scrolls.
pub const MAX_LINES: usize = 8;

/// Emitted when the user presses Enter with non-empty text.
pub struct Submit(pub String);

/// Emitted whenever the text changes (typing, deleting, pasting).
pub struct Changed;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some("TextInput")),
        KeyBinding::new("delete", Delete, Some("TextInput")),
        KeyBinding::new("left", Left, Some("TextInput")),
        KeyBinding::new("right", Right, Some("TextInput")),
        KeyBinding::new("up", Up, Some("TextInput")),
        KeyBinding::new("down", Down, Some("TextInput")),
        KeyBinding::new("shift-left", SelectLeft, Some("TextInput")),
        KeyBinding::new("shift-right", SelectRight, Some("TextInput")),
        KeyBinding::new("ctrl-a", SelectAll, Some("TextInput")),
        KeyBinding::new("ctrl-v", Paste, Some("TextInput")),
        KeyBinding::new("ctrl-shift-v", Paste, Some("TextInput")),
        KeyBinding::new("shift-insert", Paste, Some("TextInput")),
        KeyBinding::new("ctrl-c", Copy, Some("TextInput")),
        KeyBinding::new("ctrl-insert", Copy, Some("TextInput")),
        KeyBinding::new("ctrl-x", Cut, Some("TextInput")),
        KeyBinding::new("shift-delete", Cut, Some("TextInput")),
        KeyBinding::new("home", Home, Some("TextInput")),
        KeyBinding::new("end", End, Some("TextInput")),
        KeyBinding::new("shift-home", SelectHome, Some("TextInput")),
        KeyBinding::new("shift-end", SelectEnd, Some("TextInput")),
        KeyBinding::new("enter", Enter, Some("TextInput")),
    ]);
}

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    masked: bool,
    disabled: bool,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
    /// Break long text onto new lines instead of running off the edge.
    wrap: bool,
    /// The wrapped layout last painted, its line height, and how far it's
    /// scrolled up (when taller than [`MAX_LINES`]).
    last_wrapped: Option<WrappedLine>,
    line_height: Pixels,
    scroll_top: Pixels,
}

impl EventEmitter<Submit> for TextInput {}
impl EventEmitter<Changed> for TextInput {}

impl TextInput {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: "".into(),
            placeholder: placeholder.into(),
            masked: false,
            disabled: false,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
            wrap: false,
            last_wrapped: None,
            line_height: px(20.),
            scroll_top: px(0.),
        }
    }

    /// Wrap long text onto new lines (the chat box).
    pub fn set_wrap(&mut self, wrap: bool, cx: &mut Context<Self>) {
        self.wrap = wrap;
        cx.notify();
    }

    fn wraps(&self) -> bool {
        self.wrap && !self.masked
    }

    pub fn set_placeholder(&mut self, p: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.placeholder = p.into();
        cx.notify();
    }

    pub fn set_masked(&mut self, masked: bool, cx: &mut Context<Self>) {
        self.masked = masked;
        cx.notify();
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    /// Replaces the contents (e.g. the address bar following navigation).
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.reset();
        self.content = text.to_string().into();
        cx.notify();
    }

    /// Replaces the contents with `text`, all of it selected (typing
    /// replaces it), e.g. a title being renamed.
    pub fn set_text_selected(&mut self, text: &str, cx: &mut Context<Self>) {
        self.set_text(text, cx);
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    #[allow(dead_code)] // the chat box stays enabled now; other inputs may use it
    pub fn set_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        self.disabled = disabled;
        cx.notify();
    }

    fn enter(&mut self, _: &Enter, _: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        let text = self.content.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.reset();
        cx.emit(Submit(text));
        cx.notify();
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    /// Up and Down move between wrapped lines; a single-line input passes
    /// them on (e.g. to a suggestions list).
    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(-1., cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(1., cx);
    }

    fn move_vertically(&mut self, lines: f32, cx: &mut Context<Self>) {
        let Some(line) = self.last_wrapped.as_ref().filter(|_| self.wraps()) else {
            cx.propagate();
            return;
        };
        let lh = self.line_height;
        let Some(at) = line.position_for_index(self.cursor_offset(), lh) else { return };
        let y = at.y + lh * lines;
        let target = if y < px(0.) {
            0
        } else if y >= lh * wrapped_line_count(line) as f32 {
            self.content.len()
        } else {
            index_at(line, point(at.x, y + lh / 2.), lh)
        };
        self.move_to(target, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx)
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }

    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.content.len(), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        let at = self.index_for_mouse_position(event.position);
        // Double-click selects a word, triple-click everything (masked
        // text has no words to show).
        if event.click_count >= 3 || (event.click_count == 2 && self.masked) {
            self.is_selecting = false;
            self.move_to(0, cx);
            self.select_to(self.content.len(), cx);
            return;
        }
        if event.click_count == 2 {
            self.is_selecting = false;
            let word = crate::chat_text::word_at(&self.content, at);
            self.move_to(word.start, cx);
            self.select_to(word.end, cx);
            return;
        }
        self.is_selecting = true;
        if event.modifiers.shift {
            self.select_to(at, cx);
        } else {
            self.move_to(at, cx)
        }
    }

    /// Middle-click pastes the primary selection (Linux) where clicked.
    fn on_middle_click(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_primary().and_then(|item| item.text()) else { return };
        window.focus(&self.focus_handle);
        let at = self.index_for_mouse_position(event.position);
        self.move_to(at, cx);
        self.replace_text_in_range(None, &single_line(&text), window, cx);
    }

    /// Inserts `text` at the cursor (replacing any selection), as if
    /// typed or pasted.
    pub fn insert_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        let text = single_line(text);
        let range = self.marked_range.take().unwrap_or(self.selected_range.clone());
        let range = clamp_range(&self.content, range);
        self.content = (self.content[..range.start].to_owned() + &text + &self.content[range.end..]).into();
        let at = range.start + text.len();
        self.selected_range = at..at;
        self.selection_reversed = false;
        cx.emit(Changed);
        cx.notify();
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &single_line(&text), window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() && !self.masked {
            cx.write_to_clipboard(ClipboardItem::new_string(self.content[self.selected_range.clone()].to_string()));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() && !self.masked {
            cx.write_to_clipboard(ClipboardItem::new_string(self.content[self.selected_range.clone()].to_string()));
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed { self.selected_range.start } else { self.selected_range.end }
    }

    /// Content byte offset → byte offset in the drawn line.
    fn display_offset(&self, offset: usize) -> usize {
        if self.masked { self.content[..offset].chars().count() * BULLET.len() } else { offset }
    }

    /// Byte offset in the drawn line → content byte offset.
    fn content_offset(&self, display: usize) -> usize {
        if !self.masked {
            return display;
        }
        let nth = display / BULLET.len();
        self.content.char_indices().nth(nth).map(|(i, _)| i).unwrap_or(self.content.len())
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        if let (true, Some(bounds), Some(line)) = (self.wraps(), self.last_bounds.as_ref(), self.last_wrapped.as_ref()) {
            let local = point(position.x - bounds.left(), position.y - bounds.top() + self.scroll_top);
            return index_at(line, local, self.line_height);
        }
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref()) else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        self.content_offset(line.closest_index_for_x(position.x - bounds.left()))
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify()
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    fn reset(&mut self) {
        self.content = "".into();
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.marked_range = None;
        self.last_layout = None;
        self.last_bounds = None;
        self.is_selecting = false;
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: self.range_to_utf16(&self.selected_range), reversed: self.selection_reversed })
    }

    fn marked_text_range(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        self.content = (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..]).into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.marked_range.take();
        cx.emit(Changed);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        self.content = (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..]).into();
        self.marked_range =
            if new_text.is_empty() { None } else { Some(range.start..range.start + new_text.len()) };
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .map(|new_range| new_range.start + range.start..new_range.end + range.end)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range_utf16);
        if let (true, Some(line)) = (self.wraps(), self.last_wrapped.as_ref()) {
            let lh = self.line_height;
            let start = line.position_for_index(range.start, lh)?;
            let end = line.position_for_index(range.end, lh)?;
            let top = bounds.top() - self.scroll_top;
            return Some(Bounds::from_corners(
                point(bounds.left() + start.x, top + start.y),
                point(bounds.left() + end.x, top + end.y + lh),
            ));
        }
        let last_layout = self.last_layout.as_ref()?;
        Some(Bounds::from_corners(
            point(bounds.left() + last_layout.x_for_index(self.display_offset(range.start)), bounds.top()),
            point(bounds.left() + last_layout.x_for_index(self.display_offset(range.end)), bounds.bottom()),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        if let (true, Some(line)) = (self.wraps(), self.last_wrapped.as_ref()) {
            let local = gpui::point(line_point.x, line_point.y + self.scroll_top);
            return Some(self.offset_to_utf16(index_at(line, local, self.line_height)));
        }
        let last_layout = self.last_layout.as_ref()?;
        let display_index = last_layout.index_for_x(point.x - line_point.x)?;
        Some(self.offset_to_utf16(self.content_offset(display_index)))
    }
}

/// Pasted text as one line: the input draws a single paragraph, so line
/// breaks (`\n`, `\r\n` or `\r`) become spaces.
pub fn single_line(text: &str) -> String {
    text.replace("\r\n", " ").replace(['\n', '\r'], " ")
}

fn clamp_range(text: &str, range: Range<usize>) -> Range<usize> {
    let end = crate::chat_text::clamp(text, range.end);
    crate::chat_text::clamp(text, range.start).min(end)..end
}

/// How many lines a wrapped layout takes.
fn wrapped_line_count(line: &WrappedLine) -> usize {
    line.wrap_boundaries().len() + 1
}

/// The text index nearest a point in a wrapped layout (relative to its
/// top-left), clamped to the text.
fn index_at(line: &WrappedLine, at: Point<Pixels>, line_height: Pixels) -> usize {
    if at.y < px(0.) {
        return 0;
    }
    if at.y >= line_height * wrapped_line_count(line) as f32 {
        return line.len();
    }
    let at = point(at.x.max(px(0.)), at.y);
    line.closest_index_for_position(at, line_height).unwrap_or_else(|i| i)
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    wrapped: Option<WrappedLine>,
    cursor: Option<PaintQuad>,
    selection: Vec<PaintQuad>,
    scroll_top: Pixels,
}

/// The text to draw (content, bullets or the placeholder) and its runs.
fn display_runs(input: &TextInput, window: &Window) -> (SharedString, Vec<TextRun>) {
    let content = input.content.clone();
    let style = window.text_style();
    let (display_text, text_color): (SharedString, _) = if content.is_empty() {
        (input.placeholder.clone(), rgb(theme::MUTED).into())
    } else if input.masked {
        (BULLET.repeat(content.chars().count()).into(), style.color)
    } else {
        (content, style.color)
    };
    // One paragraph: a stray newline is drawn as a space (same length).
    let display_text: SharedString =
        if display_text.contains('\n') { display_text.replace('\n', " ").into() } else { display_text };
    let run = TextRun {
        len: display_text.len(),
        font: style.font(),
        color: text_color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let runs = match input.marked_range.as_ref().filter(|_| !input.masked && !input.content.is_empty()) {
        Some(marked) => vec![
            TextRun { len: marked.start, ..run.clone() },
            TextRun {
                len: marked.end - marked.start,
                underline: Some(UnderlineStyle { color: Some(run.color), thickness: px(1.0), wavy: false }),
                ..run.clone()
            },
            TextRun { len: display_text.len() - marked.end, ..run },
        ]
        .into_iter()
        .filter(|r| r.len > 0)
        .collect(),
        None => vec![run],
    };
    (display_text, runs)
}

fn shape_wrapped(window: &Window, text: SharedString, runs: &[TextRun], width: Pixels) -> Option<WrappedLine> {
    let font_size = window.text_style().font_size.to_pixels(window.rem_size());
    window.text_system().shape_text(text, font_size, runs, Some(width), None).ok()?.into_iter().next()
}

impl IntoElement for TextElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let line_height = window.line_height();
        let input = self.input.read(cx);
        if !input.wraps() {
            style.size.height = line_height.into();
            return (window.request_layout(style, [], cx), ());
        }
        // As tall as the wrapped text, up to MAX_LINES.
        let (text, runs) = display_runs(input, window);
        let font_size = window.text_style().font_size.to_pixels(window.rem_size());
        let id = window.request_measured_layout(style, move |known, available, window, _| {
            let width = known.width.or(match available.width {
                AvailableSpace::Definite(w) => Some(w),
                _ => None,
            });
            let lines = width
                .and_then(|w| window.text_system().shape_text(text.clone(), font_size, &runs, Some(w), None).ok())
                .map(|ls| ls.iter().map(wrapped_line_count).sum::<usize>())
                .unwrap_or(1);
            size(width.unwrap_or_default(), line_height * lines.clamp(1, MAX_LINES) as f32)
        });
        (id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let (display_text, runs) = display_runs(input, window);
        let (sel_start, sel_end, cursor) = if input.content.is_empty() {
            (0, 0, 0)
        } else {
            (
                input.display_offset(input.selected_range.start),
                input.display_offset(input.selected_range.end),
                input.display_offset(input.cursor_offset()),
            )
        };
        let selection_color = rgba(0x5fc8c040);

        if input.wraps() {
            let lh = window.line_height();
            let mut scroll_top = input.scroll_top;
            let Some(line) = shape_wrapped(window, display_text, &runs, bounds.size.width) else {
                return PrepaintState { line: None, wrapped: None, cursor: None, selection: vec![], scroll_top };
            };
            let pos = |i: usize| line.position_for_index(i, lh).unwrap_or_default();
            // Keep the cursor's line in view.
            let at = pos(cursor);
            let total = lh * wrapped_line_count(&line) as f32;
            if at.y < scroll_top {
                scroll_top = at.y;
            } else if at.y + lh > scroll_top + bounds.size.height {
                scroll_top = at.y + lh - bounds.size.height;
            }
            scroll_top = scroll_top.min((total - bounds.size.height).max(px(0.))).max(px(0.));
            let top = bounds.top() - scroll_top;
            let left = bounds.left();
            let (cursor, selection) = if sel_start == sel_end {
                let c = fill(Bounds::new(point(left + at.x, top + at.y), size(px(2.), lh)), rgb(theme::ACCENT));
                (Some(c), vec![])
            } else {
                let (a, b) = (pos(sel_start), pos(sel_end));
                let right = bounds.right();
                let quad = |x0: Pixels, x1: Pixels, y: Pixels| {
                    fill(Bounds::from_corners(point(x0, top + y), point(x1, top + y + lh)), selection_color)
                };
                let mut quads = vec![];
                if a.y == b.y {
                    quads.push(quad(left + a.x, left + b.x, a.y));
                } else {
                    quads.push(quad(left + a.x, right, a.y));
                    if b.y - a.y > lh {
                        quads.push(fill(
                            Bounds::from_corners(point(left, top + a.y + lh), point(right, top + b.y)),
                            selection_color,
                        ));
                    }
                    quads.push(quad(left, left + b.x, b.y));
                }
                (None, quads)
            };
            return PrepaintState { line: None, wrapped: Some(line), cursor, selection, scroll_top };
        }

        let font_size = window.text_style().font_size.to_pixels(window.rem_size());
        let line = window.text_system().shape_line(display_text, font_size, &runs, None);
        let (selection, cursor) = if sel_start == sel_end {
            let x = line.x_for_index(cursor);
            (
                vec![],
                Some(fill(
                    Bounds::new(point(bounds.left() + x, bounds.top()), size(px(2.), bounds.bottom() - bounds.top())),
                    rgb(theme::ACCENT),
                )),
            )
        } else {
            (
                vec![fill(
                    Bounds::from_corners(
                        point(bounds.left() + line.x_for_index(sel_start), bounds.top()),
                        point(bounds.left() + line.x_for_index(sel_end), bounds.bottom()),
                    ),
                    selection_color,
                )],
                None,
            )
        };
        PrepaintState { line: Some(line), wrapped: None, cursor, selection, scroll_top: px(0.) }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(&focus_handle, ElementInputHandler::new(bounds, self.input.clone()), cx);
        let lh = window.line_height();
        let scroll_top = prepaint.scroll_top;
        let line = prepaint.line.take();
        let wrapped = prepaint.wrapped.take();
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for quad in prepaint.selection.drain(..) {
                window.paint_quad(quad)
            }
            if let Some(line) = &line {
                line.paint(bounds.origin, lh, window, cx).ok();
            }
            if let Some(wrapped) = &wrapped {
                let origin = point(bounds.left(), bounds.top() - scroll_top);
                wrapped.paint(origin, lh, TextAlign::Left, None, window, cx).ok();
            }
            if focus_handle.is_focused(window) {
                if let Some(cursor) = prepaint.cursor.take() {
                    window.paint_quad(cursor);
                }
            }
        });
        self.input.update(cx, |input, _cx| {
            if line.is_some() {
                input.last_layout = line;
            }
            input.last_wrapped = wrapped;
            input.last_bounds = Some(bounds);
            input.line_height = lh;
            input.scroll_top = scroll_top;
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        div()
            .flex()
            .key_context("TextInput")
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::enter))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_middle_click))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .w_full()
            .overflow_hidden()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(rgb(theme::BG))
            .border_1()
            .border_color(rgb(if focused { theme::ACCENT } else { theme::RULE }))
            .text_sm()
            .text_color(rgb(if self.disabled { theme::MUTED } else { theme::FG }))
            .line_height(px(20.))
            .child(TextElement { input: cx.entity() })
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_line_breaks_become_spaces() {
        assert_eq!(single_line("one\ntwo"), "one two");
        assert_eq!(single_line("one\r\ntwo\rthree"), "one two three");
        assert_eq!(single_line("no breaks"), "no breaks");
    }

    #[test]
    fn insert_range_is_clamped_to_the_text() {
        assert_eq!(clamp_range("héllo", 2..9), 1..6);
        assert_eq!(clamp_range("abc", 1..2), 1..2);
        assert_eq!(clamp_range("abc", 5..9), 3..3);
    }
}
