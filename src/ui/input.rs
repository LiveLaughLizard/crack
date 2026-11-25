//! A text field that takes input through the platform's input methods
//! (`EntityInputHandler`), with multiple lines for the prompt and a single-line mode
//! for dialog search boxes. Lines don't soft-wrap.

use gpui::{
    Animation, AnimationExt as _, AnyElement, App, Bounds, ClipboardItem, Context, CursorStyle,
    Element, ElementId, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    Focusable, GlobalElementId, InspectorElementId, InteractiveElement, IntoElement, KeyBinding,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad,
    ParentElement as _, Pixels, Point, Render, ShapedLine, SharedString, Style, Styled, TextRun,
    UTF16Selection, UnderlineStyle, Window, actions, div, fill, point, px, size,
};
use std::ops::Range;
use std::time::Duration;

use super::theme;

actions!(
    input,
    [
        Backspace,
        Delete,
        DeleteWordLeft,
        DeleteToStart,
        Left,
        Right,
        WordLeft,
        WordRight,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        Paste,
        Cut,
        Copy,
        Submit,
        Newline,
    ]
);

pub fn bind_keys(cx: &mut App) {
    let ctx = Some("TextInput");
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("shift-backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("ctrl-backspace", DeleteWordLeft, ctx),
        KeyBinding::new("alt-backspace", DeleteWordLeft, ctx),
        KeyBinding::new("ctrl-w", DeleteWordLeft, ctx),
        KeyBinding::new("ctrl-u", DeleteToStart, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("ctrl-left", WordLeft, ctx),
        KeyBinding::new("alt-left", WordLeft, ctx),
        KeyBinding::new("ctrl-right", WordRight, ctx),
        KeyBinding::new("alt-right", WordRight, ctx),
        KeyBinding::new("up", Up, ctx),
        KeyBinding::new("down", Down, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("ctrl-a", Home, ctx),
        KeyBinding::new("end", End, ctx),
        KeyBinding::new("ctrl-e", End, ctx),
        KeyBinding::new("secondary-shift-a", SelectAll, ctx),
        KeyBinding::new("secondary-v", Paste, ctx),
        KeyBinding::new("ctrl-shift-v", Paste, ctx),
        // ctrl-x is the leader key, so cut is ctrl-shift-x.
        KeyBinding::new("ctrl-shift-x", Cut, ctx),
        KeyBinding::new("ctrl-shift-c", Copy, ctx),
        KeyBinding::new("enter", Submit, ctx),
        KeyBinding::new("shift-enter", Newline, ctx),
        KeyBinding::new("ctrl-enter", Newline, ctx),
        KeyBinding::new("alt-enter", Newline, ctx),
        KeyBinding::new("ctrl-j", Newline, ctx),
    ]);
}

#[derive(Clone, Debug)]
pub enum InputEvent {
    Submit,
    Changed,
    /// Up on the first line (history, list navigation).
    Up,
    /// Down on the last line.
    Down,
    /// An image was pasted (the prompt attaches it).
    PasteImage(gpui::Image),
}

const CARET_BLINK: Duration = Duration::from_millis(1100);

pub struct TextInput {
    focus_handle: FocusHandle,
    placeholder: SharedString,
    multiline: bool,
    /// Draw every character as a bullet (API keys).
    masked: bool,
    content: String,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_lines: Vec<(usize, ShapedLine)>,
    last_bounds: Option<Bounds<Pixels>>,
    last_line_height: Pixels,
    is_selecting: bool,
}

impl EventEmitter<InputEvent> for TextInput {}

impl TextInput {
    pub fn new(
        multiline: bool,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            placeholder: placeholder.into(),
            multiline,
            masked: false,
            content: String::new(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_lines: Vec::new(),
            last_bounds: None,
            last_line_height: px(20.),
            is_selecting: false,
        }
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.content = text.into();
        if !self.multiline {
            self.content = self.content.replace('\n', " ");
        }
        self.marked_range = None;
        self.move_to(self.content.len(), cx);
        cx.emit(InputEvent::Changed);
    }

    pub fn set_masked(&mut self, masked: bool, cx: &mut Context<Self>) {
        self.masked = masked;
        cx.notify();
    }

    pub fn is_masked(&self) -> bool {
        self.masked
    }

    /// The text as drawn: bullets in place of characters when masked.
    fn display_text(&self) -> String {
        if self.masked {
            "•".repeat(self.content.chars().count())
        } else {
            self.content.clone()
        }
    }

    /// A content offset as an offset in the drawn text.
    fn display_offset(&self, offset: usize) -> usize {
        if self.masked {
            self.content[..offset.min(self.content.len())]
                .chars()
                .count()
                * '•'.len_utf8()
        } else {
            offset
        }
    }

    /// An offset in the drawn text as a content offset.
    fn content_offset(&self, offset: usize) -> usize {
        if self.masked {
            let chars = offset / '•'.len_utf8();
            self.content
                .char_indices()
                .nth(chars)
                .map_or(self.content.len(), |(i, _)| i)
        } else {
            offset
        }
    }

    fn line_starts(&self) -> Vec<usize> {
        let mut starts = vec![0];
        for (i, ch) in self.content.char_indices() {
            if ch == '\n' {
                starts.push(i + 1);
            }
        }
        starts
    }

    /// (line index, line start) of an offset.
    fn line_of(&self, offset: usize) -> (usize, usize) {
        let starts = self.line_starts();
        let line = starts.iter().rposition(|s| *s <= offset).unwrap_or(0);
        (line, starts[line])
    }

    fn line_end(&self, start: usize) -> usize {
        self.content[start..]
            .find('\n')
            .map_or(self.content.len(), |i| start + i)
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let target = self.previous_boundary(self.cursor_offset());
            self.select_to(target, cx);
        }
        self.replace_selection("", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let target = self.next_boundary(self.cursor_offset());
            self.select_to(target, cx);
        }
        self.replace_selection("", window, cx);
    }

    fn delete_word_left(
        &mut self,
        _: &DeleteWordLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_range.is_empty() {
            let target = self.word_left(self.cursor_offset());
            self.select_to(target, cx);
        }
        self.replace_selection("", window, cx);
    }

    fn delete_to_start(&mut self, _: &DeleteToStart, window: &mut Window, cx: &mut Context<Self>) {
        let (_, start) = self.line_of(self.cursor_offset());
        self.select_to(start, cx);
        self.replace_selection("", window, cx);
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let target = self.previous_boundary(self.cursor_offset());
            self.move_to(target, cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let target = self.next_boundary(self.selected_range.end);
            self.move_to(target, cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn word_left_action(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.word_left(self.cursor_offset());
        self.move_to(target, cx);
    }

    fn word_right_action(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.word_right(self.cursor_offset());
        self.move_to(target, cx);
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor_offset();
        let (line, start) = self.line_of(cursor);
        if !self.multiline || line == 0 {
            cx.emit(InputEvent::Up);
            return;
        }
        let column = self.content[start..cursor].chars().count();
        let starts = self.line_starts();
        let prev = starts[line - 1];
        let prev_end = self.line_end(prev);
        let target = self.content[prev..prev_end]
            .char_indices()
            .nth(column)
            .map_or(prev_end, |(i, _)| prev + i);
        self.move_to(target, cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor_offset();
        let (line, start) = self.line_of(cursor);
        let starts = self.line_starts();
        if !self.multiline || line + 1 >= starts.len() {
            cx.emit(InputEvent::Down);
            return;
        }
        let column = self.content[start..cursor].chars().count();
        let next = starts[line + 1];
        let next_end = self.line_end(next);
        let target = self.content[next..next_end]
            .char_indices()
            .nth(column)
            .map_or(next_end, |(i, _)| next + i);
        self.move_to(target, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.previous_boundary(self.cursor_offset());
        self.select_to(target, cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.next_boundary(self.cursor_offset());
        self.select_to(target, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        let end = self.content.len();
        self.select_to(end, cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let (_, start) = self.line_of(self.cursor_offset());
        self.move_to(start, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let (_, start) = self.line_of(self.cursor_offset());
        let end = self.line_end(start);
        self.move_to(end, cx);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        // A pasted screenshot goes to whoever listens (the prompt attaches it).
        if !self.masked
            && let Some(image) = item.entries().iter().find_map(|e| match e {
                gpui::ClipboardEntry::Image(image) => Some(image.clone()),
                _ => None,
            })
        {
            cx.emit(InputEvent::PasteImage(image));
            return;
        }
        if let Some(text) = item.text() {
            self.replace_selection(&text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.masked && !self.selected_range.is_empty() {
            let selected = self.content[self.selected_range.clone()].to_string();
            cx.write_to_clipboard(ClipboardItem::new_string(selected));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.masked && !self.selected_range.is_empty() {
            let selected = self.content[self.selected_range.clone()].to_string();
            cx.write_to_clipboard(ClipboardItem::new_string(selected));
            self.replace_selection("", window, cx);
        }
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(InputEvent::Submit);
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        if self.multiline {
            self.replace_selection("\n", window, cx);
        }
    }

    fn replace_selection(&mut self, text: &str, _window: &mut Window, cx: &mut Context<Self>) {
        self.replace(None, text, None, cx);
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        self.is_selecting = true;
        let offset = self.index_for_position(event.position);
        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_selecting {
            let offset = self.index_for_position(event.position);
            self.select_to(offset, cx);
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        cx.notify();
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn index_for_position(&self, position: Point<Pixels>) -> usize {
        let Some(bounds) = self.last_bounds.as_ref() else {
            return 0;
        };
        if self.last_lines.is_empty() {
            return 0;
        }
        if position.y < bounds.top() {
            return 0;
        }
        let row = ((position.y - bounds.top()) / self.last_line_height)
            .floor()
            .max(0.) as usize;
        let Some((start, line)) = self.last_lines.get(row) else {
            return self.content.len();
        };
        self.content_offset(start + line.closest_index_for_x(position.x - bounds.left()))
            .min(self.content.len())
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content[..offset]
            .chars()
            .next_back()
            .map(|c| offset - c.len_utf8())
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content[offset..]
            .chars()
            .next()
            .map(|c| offset + c.len_utf8())
            .unwrap_or(self.content.len())
    }

    fn word_left(&self, offset: usize) -> usize {
        let before = &self.content[..offset];
        let trimmed = before.trim_end_matches(|c: char| c.is_whitespace());
        let start = trimmed
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace() || (!c.is_alphanumeric() && *c != '_'))
            .map_or(0, |(i, c)| i + c.len_utf8());
        if start == trimmed.len() && start > 0 {
            self.previous_boundary(trimmed.len())
        } else {
            start
        }
    }

    fn word_right(&self, offset: usize) -> usize {
        let after = &self.content[offset..];
        let skip_space = after.len() - after.trim_start().len();
        let rest = &after[skip_space..];
        let word = rest
            .char_indices()
            .find(|(_, c)| c.is_whitespace() || (!c.is_alphanumeric() && *c != '_'))
            .map_or(rest.len(), |(i, c)| if i == 0 { c.len_utf8() } else { i });
        offset + skip_space + word
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8 = 0;
        let mut utf16 = 0;
        for ch in self.content.chars() {
            if utf16 >= offset {
                break;
            }
            utf16 += ch.len_utf16();
            utf8 += ch.len_utf8();
        }
        utf8
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16 = 0;
        let mut utf8 = 0;
        for ch in self.content.chars() {
            if utf8 >= offset {
                break;
            }
            utf8 += ch.len_utf8();
            utf16 += ch.len_utf16();
        }
        utf16
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    fn replace(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        marked: Option<Range<usize>>,
        cx: &mut Context<Self>,
    ) {
        let new_text: String = new_text
            .replace("\r\n", "\n")
            .chars()
            .map(|c| if c == '\n' && !self.multiline { ' ' } else { c })
            .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
            .collect();
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        self.content = format!(
            "{}{}{}",
            &self.content[..range.start],
            new_text,
            &self.content[range.end..]
        );
        self.marked_range =
            (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        self.selected_range = match marked {
            Some(r) => {
                let r = self.range_from_utf16(&r);
                range.start + r.start..range.start + r.end
            }
            None => {
                self.marked_range = None;
                let caret = range.start + new_text.len();
                caret..caret
            }
        };
        self.selection_reversed = false;
        cx.emit(InputEvent::Changed);
        cx.notify();
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
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|r| self.range_to_utf16(r))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace(range_utf16, new_text, None, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace(range_utf16, new_text, new_selected_range_utf16, cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range_utf16);
        let range = self.display_offset(range.start)..self.display_offset(range.end);
        let (row, (start, line)) = self
            .last_lines
            .iter()
            .enumerate()
            .rev()
            .find(|(_, (s, _))| *s <= range.start)?;
        let top = bounds.top() + self.last_line_height * row as f32;
        Some(Bounds::from_corners(
            point(bounds.left() + line.x_for_index(range.start - start), top),
            point(
                bounds.left() + line.x_for_index((range.end - start).min(line.len())),
                top + self.last_line_height,
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.offset_to_utf16(self.index_for_position(position)))
    }
}

struct TextElement {
    input: Entity<TextInput>,
    blink: Option<f32>,
}

struct PrepaintState {
    lines: Vec<(usize, ShapedLine)>,
    selections: Vec<PaintQuad>,
    caret: Option<PaintQuad>,
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
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let lines = self.input.read(cx).content.split('\n').count().max(1);
        let mut style = Style::default();
        style.size.width = gpui::relative(1.).into();
        style.size.height = (window.line_height() * lines as f32).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let empty = input.content.is_empty();
        let color = if empty { theme::muted() } else { style.color };

        let mut lines = Vec::new();
        let source: String = if empty {
            input.placeholder.to_string()
        } else {
            input.display_text()
        };
        // Offsets below are in drawn text, which differs from the content when masked.
        let marked_range = input
            .marked_range
            .clone()
            .map(|r| input.display_offset(r.start)..input.display_offset(r.end));
        let selected_range = input.display_offset(input.selected_range.start)
            ..input.display_offset(input.selected_range.end);
        let cursor_offset = input.display_offset(input.cursor_offset());
        let mut start = 0;
        for text in source.split('\n') {
            let mut runs = vec![TextRun {
                len: text.len(),
                font: style.font(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }];
            // Underline text being composed by an input method.
            if !empty && let Some(marked) = marked_range.clone() {
                let line_range = start..start + text.len();
                let a = marked.start.clamp(line_range.start, line_range.end) - start;
                let b = marked.end.clamp(line_range.start, line_range.end) - start;
                if b > a {
                    let base = runs[0].clone();
                    runs = vec![
                        TextRun {
                            len: a,
                            ..base.clone()
                        },
                        TextRun {
                            len: b - a,
                            underline: Some(UnderlineStyle {
                                color: Some(color),
                                thickness: px(1.),
                                wavy: false,
                            }),
                            ..base.clone()
                        },
                        TextRun {
                            len: text.len() - b,
                            ..base
                        },
                    ]
                    .into_iter()
                    .filter(|r| r.len > 0)
                    .collect();
                }
            }
            let shaped = window.text_system().shape_line(
                SharedString::from(text.to_owned()),
                font_size,
                &runs,
                None,
            );
            lines.push((start, shaped));
            start += text.len() + 1;
        }

        let mut selections = Vec::new();
        let mut caret = None;
        if !empty && !selected_range.is_empty() {
            let sel = selected_range.clone();
            for (row, (line_start, line)) in lines.iter().enumerate() {
                let line_end = line_start + line.len();
                let a = sel.start.max(*line_start);
                let b = sel.end.min(line_end);
                if a > b || (a == b && sel.end <= *line_start) {
                    continue;
                }
                let top = bounds.top() + line_height * row as f32;
                let mut right = bounds.left() + line.x_for_index(b - line_start);
                if sel.end > line_end {
                    right += px(6.);
                }
                selections.push(fill(
                    Bounds::from_corners(
                        point(bounds.left() + line.x_for_index(a - line_start), top),
                        point(right, top + line_height),
                    ),
                    theme::selection(),
                ));
            }
        } else {
            let cursor = if empty { 0 } else { cursor_offset };
            let (row, (line_start, line)) = lines
                .iter()
                .enumerate()
                .rev()
                .find(|(_, (s, _))| *s <= cursor)
                .unwrap_or((0, &lines[0]));
            let x = bounds.left() + line.x_for_index((cursor - line_start).min(line.len()));
            let x = if empty { bounds.left() } else { x };
            caret = Some(fill(
                Bounds::new(
                    point(x, bounds.top() + line_height * row as f32 + px(2.)),
                    size(px(2.), line_height - px(4.)),
                ),
                theme::text(),
            ));
        }
        let caret = caret.filter(|_| self.blink.is_none_or(|b| (b * 2.0) % 1.0 < 0.5));
        PrepaintState {
            lines,
            selections,
            caret,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        for quad in prepaint.selections.drain(..) {
            window.paint_quad(quad);
        }
        let line_height = window.line_height();
        for (row, (_, line)) in prepaint.lines.iter().enumerate() {
            let origin = point(bounds.left(), bounds.top() + line_height * row as f32);
            line.paint(origin, line_height, window, cx).ok();
        }
        if focus_handle.is_focused(window)
            && let Some(caret) = prepaint.caret.take()
        {
            window.paint_quad(caret);
        }
        let lines = std::mem::take(&mut prepaint.lines);
        let is_placeholder = self.input.read(cx).content.is_empty();
        self.input.update(cx, |input, _| {
            input.last_lines = if is_placeholder { Vec::new() } else { lines };
            input.last_bounds = Some(bounds);
            input.last_line_height = line_height;
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let element = TextElement {
            input: cx.entity(),
            blink: None,
        };
        let text: AnyElement = if self.focus_handle.is_focused(window) {
            element
                .with_animation(
                    "caret",
                    Animation::new(CARET_BLINK).repeat(),
                    |element, blink| TextElement {
                        input: element.input,
                        blink: Some(blink),
                    },
                )
                .into_any_element()
        } else {
            element.into_any_element()
        };

        div()
            .id("text-input")
            .flex()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .key_context("TextInput")
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .text_color(theme::text())
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_to_start))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::word_left_action))
            .on_action(cx.listener(Self::word_right_action))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::newline))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(text)
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
