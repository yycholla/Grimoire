use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, KeyBinding,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ShapedLine, SharedString, Style, TextRun, UTF16Selection, Window, actions, div, fill, point,
    prelude::*, px, relative, rgba, size,
};

actions!(
    peer_text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        WordLeft,
        WordRight,
        SelectWordLeft,
        SelectWordRight,
        SelectAll,
        Home,
        End,
        SelectHome,
        SelectEnd,
        Paste,
        Cut,
        Copy,
        Undo,
        Redo,
        Submit,
    ]
);

pub struct Submitted;

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some("PeerTextInput")),
        KeyBinding::new("delete", Delete, Some("PeerTextInput")),
        KeyBinding::new("left", Left, Some("PeerTextInput")),
        KeyBinding::new("right", Right, Some("PeerTextInput")),
        KeyBinding::new("shift-left", SelectLeft, Some("PeerTextInput")),
        KeyBinding::new("shift-right", SelectRight, Some("PeerTextInput")),
        KeyBinding::new("alt-left", WordLeft, Some("PeerTextInput")),
        KeyBinding::new("alt-right", WordRight, Some("PeerTextInput")),
        KeyBinding::new("ctrl-left", WordLeft, Some("PeerTextInput")),
        KeyBinding::new("ctrl-right", WordRight, Some("PeerTextInput")),
        KeyBinding::new("alt-shift-left", SelectWordLeft, Some("PeerTextInput")),
        KeyBinding::new("alt-shift-right", SelectWordRight, Some("PeerTextInput")),
        KeyBinding::new("ctrl-shift-left", SelectWordLeft, Some("PeerTextInput")),
        KeyBinding::new("ctrl-shift-right", SelectWordRight, Some("PeerTextInput")),
        KeyBinding::new("home", Home, Some("PeerTextInput")),
        KeyBinding::new("end", End, Some("PeerTextInput")),
        KeyBinding::new("cmd-left", Home, Some("PeerTextInput")),
        KeyBinding::new("cmd-right", End, Some("PeerTextInput")),
        KeyBinding::new("shift-home", SelectHome, Some("PeerTextInput")),
        KeyBinding::new("shift-end", SelectEnd, Some("PeerTextInput")),
        KeyBinding::new("ctrl-a", SelectAll, Some("PeerTextInput")),
        KeyBinding::new("cmd-a", SelectAll, Some("PeerTextInput")),
        KeyBinding::new("ctrl-v", Paste, Some("PeerTextInput")),
        KeyBinding::new("cmd-v", Paste, Some("PeerTextInput")),
        KeyBinding::new("ctrl-c", Copy, Some("PeerTextInput")),
        KeyBinding::new("cmd-c", Copy, Some("PeerTextInput")),
        KeyBinding::new("ctrl-x", Cut, Some("PeerTextInput")),
        KeyBinding::new("cmd-x", Cut, Some("PeerTextInput")),
        KeyBinding::new("ctrl-z", Undo, Some("PeerTextInput")),
        KeyBinding::new("ctrl-shift-z", Redo, Some("PeerTextInput")),
        KeyBinding::new("cmd-z", Undo, Some("PeerTextInput")),
        KeyBinding::new("cmd-shift-z", Redo, Some("PeerTextInput")),
        KeyBinding::new("enter", Submit, Some("PeerTextInput")),
    ]);
}

pub struct TextInput {
    focus: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    masked: bool,
    selected: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    selecting: bool,
    undo: Vec<(SharedString, Range<usize>, bool)>,
    redo: Vec<(SharedString, Range<usize>, bool)>,
}

impl TextInput {
    pub fn new(cx: &mut Context<Self>, placeholder: impl Into<SharedString>) -> Self {
        Self {
            focus: cx.focus_handle(),
            content: "".into(),
            placeholder: placeholder.into(),
            masked: false,
            selected: 0..0,
            reversed: false,
            marked: None,
            last_layout: None,
            last_bounds: None,
            selecting: false,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    pub fn password(cx: &mut Context<Self>, placeholder: impl Into<SharedString>) -> Self {
        let mut input = Self::new(cx, placeholder);
        input.masked = true;
        input
    }

    pub fn with_value(
        cx: &mut Context<Self>,
        placeholder: impl Into<SharedString>,
        value: impl Into<SharedString>,
    ) -> Self {
        let mut input = Self::new(cx, placeholder);
        input.content = value.into();
        input.selected = input.content.len()..input.content.len();
        input
    }

    pub fn value(&self) -> String {
        self.content.to_string()
    }

    pub fn take(&mut self, cx: &mut Context<Self>) -> String {
        let content = self.content.to_string();
        self.content = "".into();
        self.selected = 0..0;
        self.reversed = false;
        self.marked = None;
        self.undo.clear();
        self.redo.clear();
        cx.notify();
        content
    }

    fn cursor(&self) -> usize {
        if self.reversed {
            self.selected.start
        } else {
            self.selected.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected = offset..offset;
        self.reversed = false;
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.reversed {
            self.selected.start = offset;
        } else {
            self.selected.end = offset;
        }
        if self.selected.end < self.selected.start {
            self.reversed = !self.reversed;
            self.selected = self.selected.end..self.selected.start;
        }
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content[..offset]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content[offset..]
            .char_indices()
            .nth(1)
            .map_or(self.content.len(), |(index, _)| offset + index)
    }

    fn previous_word_boundary(&self, offset: usize) -> usize {
        previous_word_boundary(&self.content, offset)
    }

    fn next_word_boundary(&self, offset: usize) -> usize {
        next_word_boundary(&self.content, offset)
    }
}

fn previous_word_boundary(content: &str, offset: usize) -> usize {
    let mut boundary = offset;
    let mut in_word = false;
    for (index, character) in content[..offset].char_indices().rev() {
        if in_word && character.is_whitespace() {
            break;
        }
        boundary = index;
        in_word |= !character.is_whitespace();
    }
    boundary
}

fn next_word_boundary(content: &str, offset: usize) -> usize {
    let mut boundary = content.len();
    let mut left_word = false;
    for (relative, character) in content[offset..].char_indices() {
        if left_word && !character.is_whitespace() {
            boundary = offset + relative;
            break;
        }
        left_word |= character.is_whitespace();
    }
    boundary
}

fn replace_content(content: &str, range: Range<usize>, text: &str) -> (SharedString, usize) {
    let mut start = range.start.min(content.len());
    let mut end = range.end.min(content.len());
    while !content.is_char_boundary(start) {
        start -= 1;
    }
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    end = end.max(start);
    let content = (content[..start].to_owned() + text + &content[end..]).into();
    (content, start + text.len())
}

impl TextInput {
    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        let target = if self.selected.is_empty() {
            self.previous_boundary(self.cursor())
        } else {
            self.selected.start
        };
        self.move_to(target, cx);
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        let target = if self.selected.is_empty() {
            self.next_boundary(self.cursor())
        } else {
            self.selected.end
        };
        self.move_to(target, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor()), cx);
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.previous_word_boundary(self.cursor()), cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.next_word_boundary(self.cursor()), cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_word_boundary(self.cursor()), cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_word_boundary(self.cursor()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selected = 0..self.content.len();
        cx.notify();
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }

    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.content.len(), cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            self.select_to(self.previous_boundary(self.cursor()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            self.select_to(self.next_boundary(self.cursor()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text.replace('\n', " "), window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        self.copy(&Copy, window, cx);
        if !self.selected.is_empty() {
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(previous) = self.undo.pop() {
            self.redo
                .push((self.content.clone(), self.selected.clone(), self.reversed));
            (self.content, self.selected, self.reversed) = previous;
            self.marked = None;
            cx.notify();
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(next) = self.redo.pop() {
            self.undo
                .push((self.content.clone(), self.selected.clone(), self.reversed));
            (self.content, self.selected, self.reversed) = next;
            self.marked = None;
            cx.notify();
        }
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        if !self.content.is_empty() {
            cx.emit(Submitted);
        }
    }

    fn index_for_point(&self, position: Point<Pixels>) -> usize {
        let (Some(bounds), Some(line)) = (&self.last_bounds, &self.last_layout) else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        line.closest_index_for_x(position.x - bounds.left())
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.selecting = true;
        self.move_to(self.index_for_point(event.position), cx);
    }

    fn mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            self.select_to(self.index_for_point(event.position), cx);
        }
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        self.content
            .chars()
            .scan((0, 0), |(utf8, utf16), ch| {
                let current = (*utf8, *utf16);
                *utf8 += ch.len_utf8();
                *utf16 += ch.len_utf16();
                Some(current)
            })
            .find_map(|(utf8, utf16)| (utf16 >= offset).then_some(utf8))
            .unwrap_or(self.content.len())
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        self.content[..offset].encode_utf16().count()
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }
}

impl EventEmitter<Submitted> for TextInput {}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range);
        actual.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected),
            reversed: self.reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.undo
            .push((self.content.clone(), self.selected.clone(), self.reversed));
        if self.undo.len() > 100 {
            self.undo.remove(0);
        }
        self.redo.clear();
        let range = range
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or(self.marked.clone())
            .unwrap_or(self.selected.clone());
        let (content, cursor) = replace_content(&self.content, range, text);
        self.content = content;
        self.selected = cursor..cursor;
        self.marked = None;
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_text_in_range(range, text, window, cx);
        if !text.is_empty() {
            let end = self.cursor();
            self.marked = Some(end - text.len()..end);
        }
        if let Some(selected) = selected {
            let selected = self.range_from_utf16(&selected);
            let start = self
                .marked
                .as_ref()
                .map_or(self.cursor(), |range| range.start);
            self.selected = start + selected.start..start + selected.end;
        }
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let line = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range);
        Some(Bounds::from_corners(
            point(bounds.left() + line.x_for_index(range.start), bounds.top()),
            point(bounds.left() + line.x_for_index(range.end), bounds.bottom()),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let bounds = self.last_bounds?;
        let line = self.last_layout.as_ref()?;
        line.index_for_x(point.x - bounds.left())
            .map(|index| self.offset_to_utf16(index))
    }
}

struct TextElement(Entity<TextInput>);

struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
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
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> PrepaintState {
        let input = self.0.read(cx);
        let content = input.content.clone();
        let display = if content.is_empty() {
            input.placeholder.clone()
        } else if input.masked {
            "*".repeat(content.len()).into()
        } else {
            content
        };
        let style = window.text_style();
        let line = window.text_system().shape_line(
            display.clone(),
            style.font_size.to_pixels(window.rem_size()),
            &[TextRun {
                len: display.len(),
                font: style.font(),
                color: style.color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );
        let cursor_x = line.x_for_index(input.cursor());
        let (selection, cursor) = if input.selected.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(bounds.left() + cursor_x, bounds.top()),
                        size(px(1.), bounds.size.height),
                    ),
                    gpui::green(),
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(
                            bounds.left() + line.x_for_index(input.selected.start),
                            bounds.top(),
                        ),
                        point(
                            bounds.left() + line.x_for_index(input.selected.end),
                            bounds.bottom(),
                        ),
                    ),
                    rgba(0x2dd4bf30),
                )),
                None,
            )
        };
        PrepaintState {
            line: Some(line),
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        state: &mut PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.0.read(cx).focus.clone();
        window.handle_input(&focus, ElementInputHandler::new(bounds, self.0.clone()), cx);
        if let Some(selection) = state.selection.take() {
            window.paint_quad(selection);
        }
        let line = state.line.take().expect("text input has a shaped line");
        let _ = line.paint(bounds.origin, window.line_height(), window, cx);
        if focus.is_focused(window)
            && let Some(cursor) = state.cursor.take()
        {
            window.paint_quad(cursor);
        }
        self.0.update(cx, |input, _| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("PeerTextInput")
            .track_focus(&self.focus)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::submit))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .w_full()
            .line_height(px(20.))
            .child(TextElement(cx.entity()))
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{next_word_boundary, previous_word_boundary, replace_content};

    #[test]
    fn stale_edit_range_is_clamped_after_submit() {
        let (content, cursor) = replace_content("", 0..20, "x");
        assert_eq!(content, "x");
        assert_eq!(cursor, 1);
        assert_eq!(replace_content("é", 1..2, "x").0, "x");
    }

    #[test]
    fn word_navigation_crosses_unicode_and_whitespace() {
        let content = "hello  wørld";
        assert_eq!(previous_word_boundary(content, content.len()), 7);
        assert_eq!(previous_word_boundary(content, 7), 0);
        assert_eq!(next_word_boundary(content, 0), 7);
        assert_eq!(next_word_boundary(content, 7), content.len());
    }
}
