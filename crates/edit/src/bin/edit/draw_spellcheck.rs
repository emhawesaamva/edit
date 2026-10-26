// Licensed under the MIT License.
// Fork-only: spellcheck (not part of upstream microsoft/edit).

//! The spelling suggestions popup.
//!
//! F7, Ctrl+T, Edit > Spelling Suggestions, or a right-click on a misspelled word
//! open a list of replacements below the word at the cursor.

use std::mem;
use std::rc::Rc;

use edit::buffer::{RcTextBuffer, SpellAt, SpellWord};
use edit::helpers::*;
use edit::input::{kbmod, vk};
use edit::tui::*;
use edit::{spellcheck, unicode};

use crate::localization::*;
use crate::state::*;

/// Spell checking UI state. Lives in `State::spell`.
#[derive(Default)]
pub struct SpellUi {
    /// Set by Edit > Spelling Suggestions. Opens the popup during the next [`draw_spellcheck`].
    pub request: bool,
    pub popup: Option<SpellPopup>,
}

pub struct SpellPopup {
    /// The document the popup belongs to. It closes when another document becomes active.
    pub buffer: RcTextBuffer,
    /// The misspelled word, if any.
    pub word: Option<SpellWord>,
    /// Replacements for `word`. Computed once when the popup opens, because suggesting is slow.
    pub items: Vec<String>,
    /// Shown as the only item instead of `items`, e.g. if there's nothing to suggest.
    pub message: Option<LocId>,
    /// The cursor offset when the popup opened.
    cursor: usize,
    /// Whether the list still needs to take the focus. True until the first draw.
    focus: bool,
    /// The size of the textarea during the first draw, i.e. when the popup opened.
    /// The popup closes when it changes, because a resize may have re-wrapped the word elsewhere.
    textarea: Option<Size>,
}

impl SpellPopup {
    /// The labels of the list items.
    fn labels(&self) -> impl Iterator<Item = &str> {
        let items = if self.message.is_some() { &[][..] } else { &self.items[..] };
        self.message.map(loc).into_iter().chain(items.iter().map(String::as_str))
    }
}

/// Handles the spell check shortcuts of the document's textarea and draws the suggestions popup.
///
/// Must be called right after the textarea and its attributes, because it queries the
/// textarea as the last node, and because the popup is anchored to it.
/// `textarea` is the size of the textarea.
pub fn draw_spellcheck(ctx: &mut Context, state: &mut State, textarea: Size) {
    if state.documents.active().is_none() {
        state.spell = Default::default();
        return;
    }

    // These query the textarea, so they must run before the popup is created.
    let scroll = ctx.textarea_scroll_offset().unwrap_or_default();
    let request = mem::take(&mut state.spell.request);

    if spellcheck::is_enabled() {
        if request
            || (ctx.is_focused()
                && (ctx.consume_shortcut(vk::F7) || ctx.consume_shortcut(kbmod::CTRL | vk::T)))
        {
            spell_open(state, false);
        } else if ctx.textarea_right_clicked() {
            spell_open(state, true);
        }
    }

    draw_spell_popup(ctx, state, textarea, scroll);
}

/// Opens the popup for the misspelled word at the cursor of the active document.
///
/// Without such a word, the popup shows a message instead (e.g. "Loading dictionary…"),
/// except in `quiet` mode (right-click), where nothing opens.
pub fn spell_open(state: &mut State, quiet: bool) {
    let Some(doc) = state.documents.active() else {
        return;
    };

    let mut at = doc.buffer.borrow_mut().spell_word_at_cursor();
    // The cursor is also "at" a word that ends right before it, so that F7 works right after
    // typing one. A right-click must hit the word itself though, not the cell after it,
    // nor the empty space past the end of a line that ends in it.
    if quiet
        && let SpellAt::Word(word) = &at
        && doc.buffer.borrow().cursor_offset() >= word.range.end
    {
        at = SpellAt::NoMisspelling;
    }
    let popup =
        spell_popup_new(doc.buffer.clone(), at, quiet, |word| match spellcheck::dictionary() {
            Some(dict) => spellcheck::suggest(dict, word),
            None => Vec::new(),
        });

    if let Some(popup) = popup {
        // The popup is shown below the word, so it must be in view.
        popup.buffer.borrow_mut().make_cursor_visible();
        state.spell.popup = Some(popup);
    }
}

/// Decides what the popup shows. Returns `None` if it shouldn't open at all.
/// `suggest` is called at most once.
fn spell_popup_new(
    buffer: RcTextBuffer,
    at: SpellAt,
    quiet: bool,
    suggest: impl FnOnce(&str) -> Vec<String>,
) -> Option<SpellPopup> {
    let (word, items, message) = match at {
        SpellAt::Disabled => return None,
        SpellAt::Word(word) => {
            let items = suggest(&word.text);
            let message = if items.is_empty() { Some(LocId::SpellNoSuggestions) } else { None };
            (Some(word), items, message)
        }
        _ if quiet => return None,
        SpellAt::NoMisspelling => (None, Vec::new(), Some(LocId::SpellNoMisspelling)),
        SpellAt::Loading => (None, Vec::new(), Some(LocId::SpellLoading)),
        SpellAt::Unavailable => (None, Vec::new(), Some(LocId::SpellUnavailable)),
    };
    let cursor = buffer.borrow().cursor_offset();
    Some(SpellPopup { buffer, word, items, message, cursor, focus: true, textarea: None })
}

fn draw_spell_popup(ctx: &mut Context, state: &mut State, textarea: Size, scroll: Point) {
    let Some(popup) = &mut state.spell.popup else {
        return;
    };

    let same_document =
        state.documents.active().is_some_and(|doc| Rc::ptr_eq(&doc.buffer, &popup.buffer));
    let same_size = *popup.textarea.get_or_insert(textarea) == textarea;
    if !same_document || !same_size {
        state.spell.popup = None;
        ctx.needs_rerender();
        return;
    }

    let steal_focus = mem::take(&mut popup.focus);
    let popup = &*popup;

    let pos = {
        let tb = popup.buffer.borrow();
        let margin_width = tb.margin_width();
        let cursor = tb.cursor_visual_pos();
        let word_pos = popup.word.as_ref().map_or(cursor, |w| w.visual_pos);
        let size = Size {
            // +2 for the list item prefix ("> ") and +2 for the border.
            width: popup.labels().map(label_width).max().unwrap_or(0) + 4,
            height: popup.labels().count() as CoordType + 2,
        };
        // The popup closes once the cursor leaves the word (e.g. F3 moved it to the next match),
        // so that typing and Enter don't go to a word elsewhere, maybe out of view.
        // F7 and right-clicks open it with the cursor in or right after the word.
        // A message has no word, so it closes once the cursor moves at all.
        let offset = tb.cursor_offset();
        let on_word = match &popup.word {
            Some(w) => w.range.start <= offset && offset <= w.range.end,
            None => offset == popup.cursor,
        };
        // Once the word was scrolled out of view (e.g. with the mouse wheel), the popup closes
        // instead of covering the menubar or statusbar. The cursor's row counts, too, because
        // the cursor is in or right after the word (see above), which may be wrapped.
        // Not during the first draw, whose `scroll` predates `make_cursor_visible()` in `spell_open`.
        let visible = |pos| is_visible(textarea, margin_width, pos, scroll);
        if on_word && (steal_focus || visible(word_pos) || visible(cursor)) {
            Some(popup_position(textarea, margin_width, word_pos, scroll, size))
        } else {
            None
        }
    };

    let Some(pos) = pos else {
        state.spell.popup = None;
        ctx.needs_rerender();
        return;
    };

    let mut activated = None;

    ctx.list_begin("spell-suggestions");
    ctx.attr_float(FloatSpec {
        anchor: Anchor::Last,
        gravity_x: 0.0,
        gravity_y: 0.0,
        offset_x: pos.x as f32,
        offset_y: pos.y as f32,
    });
    ctx.attr_border();
    if steal_focus {
        ctx.steal_focus();
    }
    {
        for (idx, label) in popup.labels().enumerate() {
            if ctx.list_item(idx == 0, label) == ListSelection::Activated {
                activated = Some(idx);
            }
        }
    }
    ctx.list_end();

    let close = if let Some(idx) = activated {
        if popup.message.is_none()
            && let Some(word) = &popup.word
            && let Some(item) = popup.items.get(idx)
        {
            // Does nothing if the text changed in the meantime.
            popup.buffer.borrow_mut().spell_replace(word, item);
        }
        true
    } else {
        ctx.contains_focus() && ctx.consume_shortcut(vk::ESCAPE)
    };

    if close {
        ctx.toss_focus_up();
        ctx.needs_rerender();
        state.spell.popup = None;
    } else if !ctx.contains_focus() {
        // The user clicked elsewhere or moved the focus to another control.
        ctx.needs_rerender();
        state.spell.popup = None;
    }
}

fn label_width(text: &str) -> CoordType {
    unicode::MeasurementConfig::new(&text.as_bytes())
        .goto_visual(Point { x: CoordType::MAX, y: 0 })
        .visual_pos
        .x
}

/// Computes the popup position relative to the textarea's top-left corner.
///
/// * `textarea`: size of the textarea, including the scrollbar in its last column.
/// * `margin_width`: width of the line number margin inside the textarea.
/// * `word_pos`: visual position of the word's first character.
/// * `scroll`: scroll offset of the textarea.
/// * `popup`: size of the popup, including its border.
///
/// The popup goes below the word, or above it if it only fits there. It's moved left if it would
/// cover the scrollbar. Floats don't do either on their own: They're only clipped to the viewport.
fn popup_position(
    textarea: Size,
    margin_width: CoordType,
    word_pos: Point,
    scroll: Point,
    popup: Size,
) -> Point {
    let row = word_pos.y - scroll.y;
    let below = row + 1;
    let above = row - popup.height;
    let y = if below + popup.height > textarea.height && above >= 0 { above } else { below };

    let x = margin_width + word_pos.x - scroll.x;
    let x = x.min(textarea.width - 1 - popup.width).max(0);

    Point { x, y }
}

/// Whether the visual position `pos` is in view, i.e. neither scrolled out of the textarea,
/// nor hidden behind the line number margin or the scrollbar. Arguments as for [`popup_position`].
fn is_visible(textarea: Size, margin_width: CoordType, pos: Point, scroll: Point) -> bool {
    let text_width = textarea.width - 1 - margin_width;
    (0..text_width).contains(&(pos.x - scroll.x))
        && (0..textarea.height).contains(&(pos.y - scroll.y))
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard, PoisonError};
    use std::time::{Duration, Instant};

    use edit::arena::scratch_arena;
    use edit::buffer::TextBuffer;
    use edit::input::{Input, InputKey, InputMouse, InputMouseState};
    use edit::spellcheck::DictionaryStatus;

    use super::*;

    const TEXTAREA: Size = Size { width: 80, height: 20 };
    const POPUP: Size = Size { width: 12, height: 5 };

    fn pt(x: CoordType, y: CoordType) -> Point {
        Point { x, y }
    }

    #[test]
    fn test_position_below() {
        // Below the word, after the margin.
        assert_eq!(popup_position(TEXTAREA, 6, pt(10, 3), pt(0, 0), POPUP), pt(16, 4));
        // Moves along with the scroll offset.
        assert_eq!(popup_position(TEXTAREA, 6, pt(10, 30), pt(2, 25), POPUP), pt(14, 6));
        // Ends exactly at the bottom edge (rows 15-19), so it stays below.
        assert_eq!(popup_position(TEXTAREA, 0, pt(0, 14), pt(0, 0), POPUP), pt(0, 15));
    }

    #[test]
    fn test_position_flip_above() {
        // One row short below, so it goes above (rows 10-14), with the word on row 15.
        assert_eq!(popup_position(TEXTAREA, 0, pt(0, 15), pt(0, 0), POPUP), pt(0, 10));
        // Word on the last row.
        assert_eq!(popup_position(TEXTAREA, 6, pt(3, 19), pt(0, 0), POPUP), pt(9, 14));
        // Same, but scrolled down.
        assert_eq!(popup_position(TEXTAREA, 6, pt(3, 119), pt(0, 100), POPUP), pt(9, 14));
        // Fits exactly above (rows 0-14).
        let tall = Size { width: 12, height: 15 };
        assert_eq!(popup_position(TEXTAREA, 0, pt(0, 15), pt(0, 0), tall), pt(0, 0));
        // Doesn't fit above either (one row short), so it stays below and gets clipped.
        assert_eq!(popup_position(TEXTAREA, 0, pt(0, 14), pt(0, 0), tall), pt(0, 15));
    }

    #[test]
    fn test_position_clamp_right() {
        // Ends right before the scrollbar in column 79 (columns 67-78).
        assert_eq!(popup_position(TEXTAREA, 6, pt(61, 0), pt(0, 0), POPUP), pt(67, 1));
        // Would cover the scrollbar, so it moves left.
        assert_eq!(popup_position(TEXTAREA, 6, pt(62, 0), pt(0, 0), POPUP), pt(67, 1));
        // Would stick out, so it moves left.
        assert_eq!(popup_position(TEXTAREA, 6, pt(70, 0), pt(0, 0), POPUP), pt(67, 1));
        // Scrolled horizontally, so it fits again.
        assert_eq!(popup_position(TEXTAREA, 6, pt(100, 0), pt(50, 0), POPUP), pt(56, 1));
        // Moved left and flipped above at the same time.
        assert_eq!(popup_position(TEXTAREA, 6, pt(78, 19), pt(0, 0), POPUP), pt(67, 14));
    }

    #[test]
    fn test_is_visible() {
        // 80 columns minus a 6 column margin and the scrollbar leave the text columns 0-72.
        for (pos, scroll, visible) in [
            (pt(0, 0), pt(0, 0), true),
            (pt(72, 19), pt(0, 0), true),
            // Behind the scrollbar or below the textarea.
            (pt(73, 0), pt(0, 0), false),
            (pt(0, 20), pt(0, 0), false),
            // Scrolled out at the top, and at the left (behind the margin).
            (pt(0, 4), pt(0, 5), false),
            (pt(4, 0), pt(5, 0), false),
            // Scrolled into view.
            (pt(80, 30), pt(10, 25), true),
            (pt(5, 5), pt(5, 5), true),
        ] {
            assert_eq!(is_visible(TEXTAREA, 6, pos, scroll), visible, "{pos:?} {scroll:?}");
        }
        // No textarea at all, e.g. in a tiny terminal.
        assert!(!is_visible(Size::default(), 0, pt(0, 0), pt(0, 0)));
        assert!(!is_visible(Size { width: 1, height: 1 }, 0, pt(0, 0), pt(0, 0)));
    }

    #[test]
    fn test_position_tiny_textarea() {
        // Fits neither below, above nor horizontally: stays below, at the left edge.
        let textarea = Size { width: 8, height: 3 };
        assert_eq!(popup_position(textarea, 2, pt(1, 1), pt(0, 0), POPUP), pt(0, 2));
        // No textarea at all, e.g. in a tiny terminal.
        assert_eq!(popup_position(Size::default(), 0, pt(0, 0), pt(0, 0), POPUP), pt(0, 1));
    }

    fn word(text: &str, visual_pos: Point, generation: u32) -> SpellWord {
        SpellWord { range: 0..text.len(), text: text.to_string(), visual_pos, generation }
    }

    /// Calls `spell_popup_new` and returns the popup and how often `suggest` was called.
    fn popup_new(at: SpellAt, quiet: bool, suggestions: &[&str]) -> (Option<SpellPopup>, usize) {
        let mut calls = 0;
        let popup = spell_popup_new(TextBuffer::new_rc(false).unwrap(), at, quiet, |text| {
            assert_eq!(text, "teh");
            calls += 1;
            suggestions.iter().map(|s| s.to_string()).collect()
        });
        (popup, calls)
    }

    #[test]
    fn test_popup_new_word() {
        let teh = word("teh", pt(4, 2), 7);

        for quiet in [false, true] {
            let (popup, calls) = popup_new(SpellAt::Word(teh.clone()), quiet, &["the", "tea"]);
            let popup = popup.unwrap();
            assert_eq!(calls, 1);
            assert_eq!(popup.word.as_ref(), Some(&teh));
            assert_eq!(popup.items, ["the", "tea"]);
            assert!(popup.message.is_none());
            assert!(popup.focus);
            assert_eq!(popup.labels().collect::<Vec<_>>(), ["the", "tea"]);

            // A misspelled word without suggestions opens even in quiet mode.
            let (popup, calls) = popup_new(SpellAt::Word(teh.clone()), quiet, &[]);
            let popup = popup.unwrap();
            assert_eq!(calls, 1);
            assert_eq!(popup.word.as_ref(), Some(&teh));
            assert!(popup.items.is_empty());
            assert!(popup.message == Some(LocId::SpellNoSuggestions));
            assert_eq!(popup.labels().collect::<Vec<_>>(), [loc(LocId::SpellNoSuggestions)]);
        }
    }

    #[test]
    fn test_popup_new_messages() {
        for (at, message) in [
            (SpellAt::NoMisspelling, LocId::SpellNoMisspelling),
            (SpellAt::Loading, LocId::SpellLoading),
            (SpellAt::Unavailable, LocId::SpellUnavailable),
        ] {
            let (popup, calls) = popup_new(at.clone(), false, &["unused"]);
            let popup = popup.unwrap();
            assert_eq!(calls, 0);
            assert!(popup.word.is_none());
            assert!(popup.items.is_empty());
            assert!(popup.message == Some(message));
            assert_eq!(popup.labels().collect::<Vec<_>>(), [loc(message)]);

            // Quiet mode only opens for misspelled words.
            let (popup, calls) = popup_new(at, true, &["unused"]);
            assert!(popup.is_none());
            assert_eq!(calls, 0);
        }

        for quiet in [false, true] {
            let (popup, calls) = popup_new(SpellAt::Disabled, quiet, &["unused"]);
            assert!(popup.is_none());
            assert_eq!(calls, 0);
        }
    }

    #[test]
    fn test_popup_labels_prefer_message() {
        let (popup, _) = popup_new(SpellAt::NoMisspelling, false, &[]);
        let mut popup = popup.unwrap();
        popup.items = vec!["ignored".to_string()];
        assert_eq!(popup.labels().collect::<Vec<_>>(), [loc(LocId::SpellNoMisspelling)]);
    }

    /// Drives the real UI (`crate::draw`) with a single untitled document.
    struct Harness {
        tui: Tui,
        state: State,
    }

    impl Harness {
        fn new(text: &str) -> Self {
            let mut tui = Tui::new().unwrap();
            tui.set_size(Size { width: 80, height: 24 });

            let mut state = State::new().unwrap();
            let doc = state.documents.add_untitled().unwrap();
            {
                let mut tb = doc.buffer.borrow_mut();
                // Windows defaults to CRLF, which would change the expected contents.
                tb.set_crlf(false);
                tb.write_raw(text.as_bytes());
                tb.cursor_move_to_logical(Point::default());
            }

            let mut h = Self { tui, state };
            h.frame(None);
            h
        }

        /// Same as the main loop: One pass with the input, then more until the layout has settled.
        fn frame(&mut self, input: Option<Input>) {
            crate::draw(&mut self.tui, input, &mut self.state);
            while self.tui.needs_settling() {
                crate::draw(&mut self.tui, None, &mut self.state);
            }
        }

        fn key(&mut self, key: InputKey) {
            self.frame(Some(Input::Keyboard(key)));
        }

        fn type_text(&mut self, text: &str) {
            self.frame(Some(Input::Text(text)));
        }

        fn mouse(&mut self, state: InputMouseState, position: Point) {
            self.frame(Some(Input::Mouse(InputMouse {
                state,
                modifiers: kbmod::NONE,
                position,
                scroll: Point::default(),
                drag: false,
            })));
        }

        /// Turns the mouse wheel by `notches` (3 rows each) at `position`.
        fn wheel(&mut self, position: Point, notches: CoordType) {
            self.frame(Some(Input::Mouse(InputMouse {
                state: InputMouseState::Scroll,
                modifiers: kbmod::NONE,
                position,
                scroll: pt(0, notches),
                drag: false,
            })));
        }

        /// The screen position of the text at the visual position `pos`, while not scrolled.
        fn screen_pos(&mut self, pos: Point) -> Point {
            let textarea = self.outer("textarea");
            let margin = self.buffer().borrow().margin_width();
            pt(textarea.left + margin + pos.x, textarea.top + pos.y)
        }

        /// Right-clicks the text at the visual position `pos`, while not scrolled.
        fn right_click(&mut self, pos: Point) {
            let pos = self.screen_pos(pos);
            self.mouse(InputMouseState::Right, pos);
            self.mouse(InputMouseState::None, pos);
        }

        /// The misspelled word of the open popup, if any.
        fn popup_word(&self) -> Option<String> {
            Some(self.state.spell.popup.as_ref()?.word.as_ref()?.text.clone())
        }

        fn cursor(&self) -> Point {
            self.buffer().borrow().cursor_logical_pos()
        }

        fn buffer(&self) -> RcTextBuffer {
            self.state.documents.active().unwrap().buffer.clone()
        }

        fn text_length(&self) -> usize {
            self.buffer().borrow().text_length()
        }

        fn contents(&self) -> String {
            let buffer = self.buffer();
            let tb = buffer.borrow();
            let mut text = Vec::new();
            loop {
                let chunk = tb.read_forward(text.len());
                if chunk.is_empty() {
                    break;
                }
                text.extend_from_slice(chunk);
            }
            String::from_utf8(text).unwrap()
        }

        /// The suggestions, if a popup for a misspelled word is open.
        fn suggestions(&self) -> Option<Vec<String>> {
            let popup = self.state.spell.popup.as_ref()?;
            popup.word.as_ref()?;
            Some(popup.items.clone())
        }

        fn is_open(&self) -> bool {
            self.state.spell.popup.is_some()
        }

        /// Renders the UI like the main loop does and returns the text of every curly underlined run.
        fn curly_runs(&mut self) -> Vec<String> {
            let scratch = scratch_arena(None);
            let output = self.tui.render(&scratch);
            let mut runs = Vec::new();
            let mut rest = output.as_str();
            // `Framebuffer::render` emits `ESC[4:3m ESC[58:2::R:G:Bm`, the text, then `ESC[59m`.
            while let Some(beg) = rest.find("\x1b[4:3m") {
                rest = &rest[beg..];
                let end = rest.find("\x1b[59m").expect("curly underline is closed");
                let run = &rest[..end];
                // Drop the escape sequences in front of and inside the run (SGR and CUP).
                let mut text = String::new();
                let mut chars = run.chars();
                while let Some(c) = chars.next() {
                    if c == '\x1b' {
                        for c in chars.by_ref() {
                            if c.is_ascii_alphabetic() {
                                break;
                            }
                        }
                    } else {
                        text.push(c);
                    }
                }
                runs.push(text);
                rest = &rest[end..];
            }
            runs
        }

        /// Injects a popup, bypassing `spell_word_at_cursor()`.
        fn open(&mut self, word: Option<SpellWord>, items: &[&str], message: Option<LocId>) {
            self.state.spell.popup = Some(SpellPopup {
                buffer: self.buffer(),
                word,
                items: items.iter().map(|s| s.to_string()).collect(),
                message,
                cursor: self.buffer().borrow().cursor_offset(),
                focus: true,
                textarea: None,
            });
            self.frame(None);
            assert!(self.is_open());
        }

        /// Injects a popup for "teh" at the start of the document.
        fn open_teh(&mut self, items: &[&str]) {
            let generation = self.buffer().borrow().generation();
            self.open(Some(word("teh", pt(0, 0), generation)), items, None);
        }

        /// Returns the outer rect of the node with the given classname from the last layout.
        fn outer(&mut self, classname: &str) -> Rect {
            let scratch = scratch_arena(None);
            let layout = self.tui.debug_layout(&scratch);
            let needle = format!("classname:    {classname}");
            let mut lines = layout.lines().map(str::trim);
            lines.find(|l| *l == needle).unwrap_or_else(|| panic!("no {classname} node"));
            let line = lines.find(|l| l.starts_with("outer:")).unwrap();
            let v: Vec<CoordType> = line["outer:".len()..]
                .trim()
                .trim_matches(['{', '}'])
                .split(", ")
                .map(|s| s.parse().unwrap())
                .collect();
            Rect { left: v[0], top: v[1], right: v[2], bottom: v[3] }
        }

        fn layout_contains(&mut self, text: &str) -> bool {
            let scratch = scratch_arena(None);
            self.tui.debug_layout(&scratch).contains(text)
        }

        /// Renders the UI like the main loop does and returns the screen cells with a curly underline.
        /// Only complete if nothing was rendered before, because the output only contains changed rows.
        fn curly_cells(&mut self) -> Vec<Point> {
            let scratch = scratch_arena(None);
            let output = self.tui.render(&scratch);
            let mut cells = Vec::new();
            let mut curly = false;
            let mut pos = pt(0, 0);
            let mut rest = output.as_str();

            while let Some(c) = rest.chars().next() {
                if c != '\x1b' {
                    let width = label_width(c.encode_utf8(&mut [0; 4]));
                    if curly {
                        cells.extend((0..width).map(|i| pt(pos.x + i, pos.y)));
                    }
                    pos.x += width;
                    rest = &rest[c.len_utf8()..];
                    continue;
                }

                let body = rest.strip_prefix("\x1b[").expect("only CSI sequences are expected");
                let end = body.find(|c: char| ('\x40'..='\x7e').contains(&c)).unwrap();
                let params = &body[..end];
                match &body[end..end + 1] {
                    "H" => {
                        let (row, column) = params.split_once(';').unwrap();
                        pos = pt(
                            column.parse::<CoordType>().unwrap() - 1,
                            row.parse::<CoordType>().unwrap() - 1,
                        );
                    }
                    "m" => {
                        let mut params = params.split(';');
                        while let Some(param) = params.next() {
                            match param {
                                "" | "0" | "4" | "24" | "4:0" | "4:1" => curly = false,
                                "4:3" => curly = true,
                                // `38;2;r;g;b` or `38;5;n`.
                                "38" | "48" => {
                                    let n = if params.next() == Some("2") { 3 } else { 1 };
                                    params.nth(n - 1);
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                rest = &body[end + 1..];
            }

            cells
        }

        /// Left-clicks the label with exactly the given text.
        fn click_label(&mut self, text: &str) {
            let pos = self.label_pos(text);
            self.mouse(InputMouseState::Left, pos);
            self.mouse(InputMouseState::None, pos);
        }

        /// The top-left corner of the label with exactly the given text.
        fn label_pos(&mut self, text: &str) -> Point {
            {
                let scratch = scratch_arena(None);
                let layout = self.tui.debug_layout(&scratch);
                let needle = format!("text:         \"{text}\"");
                let mut outer = "";
                let mut found = None;
                for line in layout.lines().map(str::trim) {
                    if line.starts_with("outer:") {
                        outer = line;
                    } else if line == needle {
                        found = Some(outer);
                        break;
                    }
                }
                let found = found.unwrap_or_else(|| panic!("no {text:?} label"));
                let v: Vec<CoordType> = found["outer:".len()..]
                    .trim()
                    .trim_matches(['{', '}'])
                    .split(", ")
                    .map(|s| s.parse().unwrap())
                    .collect();
                pt(v[0], v[1])
            }
        }
    }

    /// Serializes the tests that depend on the global spell check switch.
    static GLOBAL_SWITCH: Mutex<()> = Mutex::new(());

    /// Enables spell checking and waits for the dictionary,
    /// so that `spell_word_at_cursor()` gives stable results.
    fn enable_spellcheck() -> MutexGuard<'static, ()> {
        let guard = GLOBAL_SWITCH.lock().unwrap_or_else(PoisonError::into_inner);
        spellcheck::set_enabled(true);
        let deadline = Instant::now() + Duration::from_secs(60);
        while spellcheck::dictionary_status() == DictionaryStatus::Loading {
            assert!(Instant::now() < deadline, "dictionary didn't load");
            std::thread::sleep(Duration::from_millis(5));
        }
        guard
    }

    // The tests below run against the real dictionary, buffer and tui code and assert concrete
    // results (popup open, suggestions, replaced text), so that a broken hook fails loudly.

    /// The message of the open popup, if any.
    fn message(h: &Harness) -> Option<LocId> {
        h.state.spell.popup.as_ref()?.message
    }

    #[test]
    fn test_popup_layout() {
        let mut h = Harness::new("teh x\n");
        h.open_teh(&["zebrafish", "quokka"]);

        let textarea = h.outer("textarea");
        let margin = h.buffer().borrow().margin_width();
        // "zebrafish" + 4 by 2 items + 2.
        let (w, hgt) = (9 + 4, 2 + 2);
        let at = |x, y| Rect { left: x, top: y, right: x + w, bottom: y + hgt };

        // Below the word.
        let left = textarea.left + margin;
        assert_eq!(h.outer("spell-suggestions"), at(left, textarea.top + 1));
        assert!(h.layout_contains("\"> zebrafish\""));
        assert!(h.layout_contains("\"  quokka\""));

        // Above a word on the second to last row.
        let row = textarea.height() - 2;
        h.state.spell.popup.as_mut().unwrap().word.as_mut().unwrap().visual_pos = pt(0, row);
        h.frame(None);
        assert_eq!(h.outer("spell-suggestions"), at(left, textarea.top + row - hgt));

        // Moved left at the right edge, so that it doesn't cover the scrollbar.
        h.state.spell.popup.as_mut().unwrap().word.as_mut().unwrap().visual_pos = pt(75, 0);
        h.frame(None);
        assert_eq!(h.outer("spell-suggestions"), at(textarea.right - 1 - w, textarea.top + 1));

        assert!(h.is_open());
    }

    #[test]
    fn test_popup_focus_and_escape() {
        let mut h = Harness::new("teh x\n");
        h.open_teh(&["zebrafish", "quokka"]);
        let len = h.text_length();

        // The popup has the focus, so typing doesn't reach the textarea.
        h.type_text("a");
        assert!(h.is_open());
        assert_eq!(h.text_length(), len);

        // Up/Down move the selection.
        h.key(vk::DOWN);
        assert!(h.layout_contains("\"> quokka\""));
        assert!(h.is_open());

        // Escape closes it and returns the focus to the textarea.
        h.key(vk::ESCAPE);
        assert!(!h.is_open());
        h.type_text("a");
        assert_eq!(h.text_length(), len + 1);
    }

    #[test]
    fn test_popup_enter_activates() {
        let mut h = Harness::new("teh x\n");
        h.open_teh(&["zebrafish", "quokka"]);

        h.key(vk::DOWN);
        h.key(vk::RETURN);
        assert!(!h.is_open());

        // The selected item replaces the word.
        assert_eq!(h.contents(), "quokka x\n");

        // The focus is back in the textarea.
        let len = h.text_length();
        h.type_text("a");
        assert_eq!(h.text_length(), len + 1);
    }

    #[test]
    fn test_popup_stale_word() {
        let mut h = Harness::new("teh x\n");
        h.open_teh(&["zebrafish"]);

        // The text changes while the popup is open: The word is no longer replaced.
        h.buffer().borrow_mut().write_raw(b"!");
        h.key(vk::RETURN);
        assert!(!h.is_open());
        assert_eq!(h.contents(), "!teh x\n");
    }

    #[test]
    fn test_popup_message() {
        let mut h = Harness::new("teh x\n");
        h.open(None, &[], Some(LocId::SpellNoMisspelling));

        let label = format!("\"> {}\"", loc(LocId::SpellNoMisspelling));
        assert!(h.layout_contains(&label));

        // Anchored to the cursor.
        let textarea = h.outer("textarea");
        let popup = h.outer("spell-suggestions");
        assert_eq!(popup.left, textarea.left + h.buffer().borrow().margin_width());
        assert_eq!(popup.top, textarea.top + 1);
        assert_eq!(popup.width(), label_width(loc(LocId::SpellNoMisspelling)) + 4);
        assert_eq!(popup.height(), 3);

        // Activating the message only closes the popup.
        let generation = h.buffer().borrow().generation();
        h.key(vk::RETURN);
        assert!(!h.is_open());
        assert_eq!(h.buffer().borrow().generation(), generation);
    }

    #[test]
    fn test_popup_closes() {
        // When the focus moves elsewhere.
        let mut h = Harness::new("teh x\n");
        h.open_teh(&["zebrafish"]);
        h.key(vk::F10);
        assert!(!h.is_open());

        // When another document becomes active.
        let mut h = Harness::new("teh x\n");
        h.open_teh(&["zebrafish"]);
        h.state.documents.add_untitled().unwrap();
        h.frame(None);
        assert!(!h.is_open());

        // When the document is closed.
        let mut h = Harness::new("teh x\n");
        h.open_teh(&["zebrafish"]);
        h.state.spell.request = true;
        h.state.documents.remove_active();
        h.frame(None);
        assert!(!h.is_open());
        assert!(!h.state.spell.request);
    }

    #[test]
    fn test_shortcuts_open_popup() {
        let _lock = enable_spellcheck();

        for key in [vk::F7, kbmod::CTRL | vk::T] {
            // Cursor inside the misspelled word.
            let mut h = Harness::new("a teh x\n");
            h.buffer().borrow_mut().cursor_move_to_logical(pt(3, 0));
            h.key(key);
            let items = h.suggestions().expect("popup for the misspelled word");
            assert!(items.iter().any(|s| s == "the"), "{items:?}");

            // Enter replaces the word with the first suggestion, as a single undo step.
            h.key(vk::RETURN);
            assert!(!h.is_open());
            assert_eq!(h.contents(), format!("a {} x\n", items[0]));
            h.key(kbmod::CTRL | vk::Z);
            assert_eq!(h.contents(), "a teh x\n");
            h.key(kbmod::CTRL | vk::Y);
            assert_eq!(h.contents(), format!("a {} x\n", items[0]));

            // On a correct word: A message instead of suggestions.
            let mut h = Harness::new("a teh x\n");
            h.key(key);
            assert!(message(&h) == Some(LocId::SpellNoMisspelling));
        }

        // Edit > Spelling Suggestions sets `request`.
        let mut h = Harness::new("a teh x\n");
        h.buffer().borrow_mut().cursor_move_to_logical(pt(3, 0));
        h.state.spell.request = true;
        h.frame(None);
        assert!(!h.state.spell.request);
        assert!(h.suggestions().is_some_and(|items| items.iter().any(|s| s == "the")));
    }

    #[test]
    fn test_menus() {
        let _lock = enable_spellcheck();
        let mut h = Harness::new("a teh x\n");
        h.buffer().borrow_mut().cursor_move_to_logical(pt(3, 0));

        // View > Spell Check reflects and toggles the global switch (accelerator C).
        h.click_label("View");
        assert!(h.layout_contains("\"◼ Spell Check\""));
        h.type_text("c");
        assert!(!spellcheck::is_enabled());
        h.click_label("View");
        assert!(h.layout_contains("\"  Spell Check\""));

        // Edit > Spelling Suggestions is hidden while spell checking is off.
        h.click_label("Edit");
        assert!(h.layout_contains("\"  Select All\""));
        assert!(!h.layout_contains("Spelling Suggestions"));

        h.click_label("View");
        h.type_text("c");
        assert!(spellcheck::is_enabled());

        // Otherwise it's shown with its Ctrl+T shortcut and works like it (accelerator S).
        h.click_label("Edit");
        assert!(h.layout_contains("\"  Spelling Suggestions…\""));
        assert!(h.layout_contains("\"Ctrl+T\""));
        h.type_text("s");
        assert!(!h.state.spell.request);
        assert!(h.suggestions().is_some_and(|items| items.iter().any(|s| s == "the")));
    }

    #[test]
    fn test_shortcuts_ignored_without_spellcheck() {
        let _lock = enable_spellcheck();

        // Editlines and other buffers without spell checking never open the popup.
        let mut h = Harness::new("a teh x\n");
        h.buffer().borrow_mut().set_spellcheck_enabled(false);
        h.buffer().borrow_mut().cursor_move_to_logical(pt(3, 0));
        h.key(vk::F7);
        assert!(!h.is_open());
        h.key(kbmod::CTRL | vk::T);
        assert!(!h.is_open());
        h.state.spell.request = true;
        h.frame(None);
        assert!(!h.is_open());
        assert!(!h.state.spell.request);
    }

    // Integration: The whole path from typing in a document to the VT output,
    // i.e. `create_buffer()`, the render hook after the highlights, and the framebuffer's SGR.
    #[test]
    fn test_underline_end_to_end() {
        let _lock = enable_spellcheck();
        let mut h = Harness::new("");
        assert!(h.curly_runs().is_empty());

        // A word that's still being typed isn't underlined yet...
        h.type_text("a teh");
        assert!(h.curly_runs().is_empty());

        // ...but it is once it's finished.
        h.type_text(" x");
        assert_eq!(h.curly_runs(), ["teh"]);

        // Picking "the" from the popup removes the underline.
        h.buffer().borrow_mut().cursor_move_to_logical(pt(3, 0));
        h.key(vk::F7);
        let items = h.suggestions().expect("popup for the misspelled word");
        let idx = items.iter().position(|s| s == "the").expect("\"the\" is suggested");
        for _ in 0..idx {
            h.key(vk::DOWN);
        }
        h.key(vk::RETURN);
        assert_eq!(h.contents().lines().next(), Some("a the x"));
        assert!(h.curly_runs().is_empty());
    }

    #[test]
    fn test_right_click() {
        let _lock = enable_spellcheck();

        let mut h = Harness::new("a teh x\n");
        let textarea = h.outer("textarea");
        let margin = h.buffer().borrow().margin_width();
        let click = |x| pt(textarea.left + margin + x, textarea.top);

        // On the misspelled word: Moves the cursor and opens the popup (quiet mode).
        h.mouse(InputMouseState::Right, click(3));
        h.mouse(InputMouseState::None, click(3));
        assert_eq!(h.buffer().borrow().cursor_logical_pos(), pt(3, 0));
        assert!(h.suggestions().is_some_and(|items| items.iter().any(|s| s == "the")));
        // Below the word, not below the click.
        let popup = h.outer("spell-suggestions");
        assert_eq!((popup.left, popup.top), (textarea.left + margin + 2, textarea.top + 1));
        h.state.spell.popup = None;
        h.frame(None);

        // On a correct word: Only moves the cursor. Quiet mode shows no message.
        h.mouse(InputMouseState::Right, click(0));
        h.mouse(InputMouseState::None, click(0));
        assert_eq!(h.buffer().borrow().cursor_logical_pos(), pt(0, 0));
        assert!(!h.is_open());

        // On its last letter: Still the word.
        h.right_click(pt(4, 0));
        assert_eq!(h.popup_word().as_deref(), Some("teh"));
        h.key(vk::ESCAPE);
        assert!(!h.is_open());
    }

    #[test]
    fn test_right_click_next_to_word() {
        let _lock = enable_spellcheck();
        let mut h = Harness::new("a teh x\nhello teh\n");

        // The cell right after the misspelled word: Only moves the cursor,
        // even though the cursor is now directly after the word.
        h.right_click(pt(5, 0));
        assert_eq!(h.cursor(), pt(5, 0));
        assert!(!h.is_open());

        // Past the end of a line that ends in a misspelled word: Same, at the end of the line.
        h.right_click(pt(40, 1));
        assert_eq!(h.cursor(), pt(9, 1));
        assert!(!h.is_open());

        // Whereas F7 accepts the word right before the cursor, e.g. right after typing it.
        h.key(vk::F7);
        assert_eq!(h.popup_word().as_deref(), Some("teh"));
    }

    // Right-clicks reach the textarea even while another control has the focus.
    #[test]
    fn test_right_click_while_popup_open() {
        let _lock = enable_spellcheck();
        let mut h = Harness::new("a teh x wrod y\n");

        h.right_click(pt(3, 0));
        assert_eq!(h.popup_word().as_deref(), Some("teh"));

        // Another misspelled word: The popup switches to it.
        h.right_click(pt(9, 0));
        assert_eq!(h.popup_word().as_deref(), Some("wrod"));
        assert_eq!(h.cursor(), pt(9, 0));
        let popup = h.outer("spell-suggestions");
        assert_eq!(pt(popup.left, popup.top), h.screen_pos(pt(8, 1)));

        // A correct word: The popup closes, the cursor moves and the textarea has the focus.
        // A drag with the right button held doesn't select anything.
        let (x, end) = (h.screen_pos(pt(6, 0)), h.screen_pos(pt(12, 0)));
        h.mouse(InputMouseState::Right, x);
        h.mouse(InputMouseState::Right, end);
        h.mouse(InputMouseState::None, end);
        assert!(!h.is_open());
        assert_eq!(h.cursor(), pt(6, 0));
        assert!(!h.buffer().borrow().has_selection());
        h.type_text("z");
        assert_eq!(h.contents(), "a teh zx wrod y\n");
    }

    #[test]
    fn test_right_click_while_find_focused() {
        let _lock = enable_spellcheck();
        let mut h = Harness::new("a teh x\n");
        h.key(kbmod::CTRL | vk::F);
        h.type_text("q");
        assert_eq!(h.state.search_needle, "q");

        h.right_click(pt(3, 0));
        assert_eq!(h.cursor(), pt(3, 0));
        assert_eq!(h.popup_word().as_deref(), Some("teh"));

        // Escape closes the popup and the textarea keeps the focus: Typing goes there.
        h.key(vk::ESCAPE);
        assert!(!h.is_open());
        h.type_text("z");
        assert_eq!(h.contents(), "a tzeh x\n");
        assert_eq!(h.state.search_needle, "q");
    }

    #[test]
    fn test_right_click_with_modal_or_menu() {
        let _lock = enable_spellcheck();

        // A modal swallows the click: Nothing happens.
        let mut h = Harness::new("a teh x\n");
        h.state.wants_about = true;
        h.frame(None);
        let about = h.outer("about");
        let click = h.screen_pos(pt(3, 0));
        assert!(!about.contains(click));
        h.right_click(pt(3, 0));
        assert!(h.state.wants_about);
        assert!(!h.is_open());
        assert_eq!(h.cursor(), pt(0, 0));

        // A menu closes, like on a left-click, and the click reaches the text.
        let mut h = Harness::new(&format!("{}teh x\n", "a ".repeat(30)));
        h.click_label("Edit");
        assert!(h.layout_contains("\"  Select All\""));
        let flyout = h.outer("flyout");
        assert!(!flyout.contains(h.screen_pos(pt(61, 0))));
        h.right_click(pt(61, 0));
        assert!(!h.layout_contains("\"  Select All\""));
        assert_eq!(h.cursor(), pt(61, 0));
        assert_eq!(h.popup_word().as_deref(), Some("teh"));
    }

    // Word wrap may move the word elsewhere when the terminal is resized.
    #[test]
    fn test_popup_closes_on_resize() {
        let _lock = enable_spellcheck();
        let mut h = Harness::new(&format!("{}teh x\n", "aa ".repeat(23)));
        h.buffer().borrow_mut().set_word_wrap(true);
        h.buffer().borrow_mut().cursor_move_to_logical(pt(70, 0));
        h.key(vk::F7);
        assert_eq!(h.popup_word().as_deref(), Some("teh"));

        // A resize event that doesn't change the size keeps it open.
        h.frame(Some(Input::Resize(Size { width: 80, height: 24 })));
        assert!(h.is_open());

        // Narrower: "teh" is wrapped onto the next row, so the popup closes.
        h.frame(Some(Input::Resize(Size { width: 60, height: 24 })));
        assert!(!h.is_open());
        h.type_text("z");
        assert!(h.contents().ends_with("tzeh x\n"));

        // Lower, without re-wrapping: Closes as well.
        h.key(vk::F7);
        assert!(h.is_open());
        h.frame(Some(Input::Resize(Size { width: 60, height: 20 })));
        assert!(!h.is_open());
    }

    // The popup follows the word while scrolling, but doesn't cover the menubar or statusbar.
    #[test]
    fn test_popup_closes_when_scrolled_out() {
        let _lock = enable_spellcheck();
        let mut h = Harness::new(&"teh x\n".repeat(100));
        h.buffer().borrow_mut().cursor_move_to_logical(pt(1, 10));
        h.key(vk::F7);
        assert_eq!(h.popup_word().as_deref(), Some("teh"));
        let textarea = h.outer("textarea");
        let popup = h.outer("spell-suggestions");
        assert_eq!(popup.top, textarea.top + 11);

        // Away from the popup, which would swallow the wheel otherwise.
        let wheel_at = pt(textarea.right - 10, textarea.top + 2);
        h.wheel(wheel_at, 1);
        assert!(h.is_open());
        assert_eq!(h.outer("spell-suggestions").top, textarea.top + 8);

        // Rows 10 -> 7 -> 4 -> 1 -> -2: Closes once the word is above the textarea.
        h.wheel(wheel_at, 2);
        assert!(h.is_open());
        assert_eq!(h.outer("spell-suggestions").top, textarea.top + 2);
        h.wheel(wheel_at, 1);
        assert!(!h.is_open());

        // The textarea still has the focus.
        h.type_text("z");
        assert_eq!(h.contents().lines().nth(10), Some("tzeh x"));

        // F7 for a word that was scrolled out of view scrolls it back into view, and stays open.
        h.buffer().borrow_mut().cursor_move_to_logical(pt(1, 20));
        h.wheel(wheel_at, 10);
        h.key(vk::F7);
        assert_eq!(h.popup_word().as_deref(), Some("teh"));
        assert_eq!(h.outer("spell-suggestions").top, textarea.top + 1);
        h.key(vk::ESCAPE);

        // On the last row it's above the word. Scrolling the word out at the bottom closes it too.
        let last = textarea.height() - 1;
        h.buffer().borrow_mut().cursor_move_to_logical(pt(1, 60));
        h.key(vk::F7);
        let popup = h.outer("spell-suggestions");
        assert_eq!(popup.bottom, textarea.top + last);
        h.wheel(wheel_at, -1);
        assert!(!h.is_open());
    }

    // A word that's wrapped across two rows is still in view while its second row is.
    #[test]
    fn test_popup_wrapped_word_partly_scrolled_out() {
        let _lock = enable_spellcheck();
        let word = "tehteh".repeat(4);
        let mut h = Harness::new(&format!("a\nb\n{word} x\n{}", "x\n".repeat(30)));
        h.frame(Some(Input::Resize(Size { width: 20, height: 24 })));
        h.buffer().borrow_mut().set_word_wrap(true);
        h.buffer().borrow_mut().cursor_move_to_logical(pt(23, 2));
        h.key(vk::F7);
        assert_eq!(h.popup_word().as_deref(), Some(word.as_str()));
        let start = h.state.spell.popup.as_ref().unwrap().word.as_ref().unwrap().visual_pos;
        assert_eq!(start, pt(0, 2));
        assert_eq!(h.buffer().borrow().cursor_visual_pos().y, 3);

        // Scrolled by 3 rows: The word's first row is out of view, but the cursor's isn't.
        let textarea = h.outer("textarea");
        let wheel_at = pt(textarea.left + 5, textarea.bottom - 1);
        h.wheel(wheel_at, 1);
        assert!(h.is_open());
        assert_eq!(h.outer("spell-suggestions").top, textarea.top);

        h.wheel(wheel_at, 1);
        assert!(!h.is_open());
    }

    // Regression: F3 (Find Next) moved the cursor to the next match, but the popup stayed open
    // for the word it left. With the match far below, the popup was clipped to nothing, but still
    // had the focus: Typing was swallowed, and Enter replaced the word out of view.
    #[test]
    fn test_popup_closes_when_cursor_leaves_word() {
        let _lock = enable_spellcheck();

        // The match is out of view.
        let mut h = Harness::new(&format!("{}needle\n", "teh x\n".repeat(100)));
        h.buffer().borrow_mut().cursor_move_to_logical(pt(1, 10));
        h.key(vk::F7);
        assert_eq!(h.popup_word().as_deref(), Some("teh"));
        h.state.search_needle = "needle".to_string();
        h.key(vk::F3);
        assert_eq!(h.cursor(), pt(6, 100));
        assert!(!h.is_open());
        // The textarea has the focus: Typing replaces the selected match.
        h.type_text("z");
        assert_eq!(h.contents().lines().nth(100), Some("z"));
        assert_eq!(h.contents().lines().nth(10), Some("teh x"));

        // The match is in view, two rows below the word.
        let mut h = Harness::new("a teh x\nb\nc needle d\n");
        h.buffer().borrow_mut().cursor_move_to_logical(pt(3, 0));
        h.key(vk::F7);
        assert_eq!(h.popup_word().as_deref(), Some("teh"));
        h.state.search_needle = "needle".to_string();
        h.key(vk::F3);
        assert_eq!(h.cursor(), pt(8, 2));
        assert!(!h.is_open());
        // Enter replaces the selected match with a newline, and leaves the misspelled word alone.
        h.key(vk::RETURN);
        assert_eq!(h.contents(), "a teh x\nb\nc \n d\n");

        // A message has no word to leave, but it closes too, instead of following the cursor.
        let mut h = Harness::new("a the x\nb\nc needle d\n");
        h.buffer().borrow_mut().cursor_move_to_logical(pt(3, 0));
        h.key(vk::F7);
        assert!(message(&h) == Some(LocId::SpellNoMisspelling));
        h.frame(None);
        assert!(h.is_open());
        h.state.search_needle = "needle".to_string();
        h.key(vk::F3);
        assert_eq!(h.cursor(), pt(8, 2));
        assert!(!h.is_open());
        h.type_text("z");
        assert_eq!(h.contents(), "a the x\nb\nc z d\n");
    }

    #[test]
    fn test_popup_double_click_activates() {
        let mut h = Harness::new("teh x\n");
        h.open_teh(&["zebrafish", "quokka"]);
        let pos = h.label_pos("  quokka");

        // The first click only selects the item.
        h.mouse(InputMouseState::Left, pos);
        h.mouse(InputMouseState::None, pos);
        assert!(h.is_open());
        assert!(h.layout_contains("\"> quokka\""));
        assert_eq!(h.contents(), "teh x\n");

        // The second one replaces the word with it.
        h.mouse(InputMouseState::Left, pos);
        h.mouse(InputMouseState::None, pos);
        assert!(!h.is_open());
        assert_eq!(h.contents(), "quokka x\n");
    }

    #[test]
    fn test_shortcuts_ignored_while_disabled_globally() {
        let _lock = enable_spellcheck();
        spellcheck::set_enabled(false);

        let mut h = Harness::new("a teh x\n");
        h.buffer().borrow_mut().cursor_move_to_logical(pt(3, 0));
        h.key(vk::F7);
        let f7 = h.is_open();
        h.key(kbmod::CTRL | vk::T);
        let ctrl_t = h.is_open();
        h.state.spell.request = true;
        h.frame(None);
        let request = h.is_open();

        let textarea = h.outer("textarea");
        let margin = h.buffer().borrow().margin_width();
        let click = pt(textarea.left + margin + 4, textarea.top);
        h.mouse(InputMouseState::Right, click);
        h.mouse(InputMouseState::None, click);
        let right_click = h.is_open();
        let cursor = h.buffer().borrow().cursor_logical_pos();
        let runs = h.curly_runs();

        spellcheck::set_enabled(true);
        assert!(!f7 && !ctrl_t && !request && !right_click);
        assert!(!h.state.spell.request);
        // The right-click still moves the cursor, like with spell checking enabled.
        assert_eq!(cursor, pt(4, 0));
        assert!(runs.is_empty(), "{runs:?}");
    }

    // The Find and Go to Line boxes are editlines, which never get spell checked.
    #[test]
    fn test_editlines_never_underline() {
        let _lock = enable_spellcheck();

        let mut h = Harness::new("teh x\n");
        h.key(kbmod::CTRL | vk::F);
        h.type_text("wrod speling ");
        assert_eq!(h.state.search_needle, "wrod speling ");
        assert_eq!(h.curly_runs(), ["teh"]);

        let mut h = Harness::new("teh x\n");
        h.key(kbmod::CTRL | vk::G);
        h.type_text("wrod speling ");
        assert_eq!(h.state.goto_target, "wrod speling ");
        assert_eq!(h.curly_runs(), ["teh"]);
    }

    // Menus, dialogs and the popup itself clear the underline of the text below them.
    #[test]
    fn test_no_underline_under_floats() {
        let _lock = enable_spellcheck();
        let text = format!("{}\n", "teh ".repeat(19)).repeat(30);
        let contains =
            |r: Rect, p: &Point| r.left <= p.x && p.x < r.right && r.top <= p.y && p.y < r.bottom;

        // Without floats, every "teh" on screen is underlined (19 per row, 3 cells each).
        let mut h = Harness::new(&text);
        let all = h.curly_cells();
        let textarea = h.outer("textarea");
        assert!(all.len() >= 19 * 3 * (textarea.height() as usize - 1), "{}", all.len());

        type Open = fn(&mut Harness);
        let floats: [(&str, Open); 4] = [
            ("flyout", |h| h.click_label("File")),
            ("flyout", |h| h.click_label("Edit")),
            ("about", |h| {
                h.state.wants_about = true;
                h.frame(None);
            }),
            ("spell-suggestions", |h| h.key(vk::F7)),
        ];

        for (classname, open) in floats {
            let mut h = Harness::new(&text);
            open(&mut h);
            let float = h.outer(classname);
            assert!(float.width() > 0 && float.height() > 0, "{classname}");

            let cells = h.curly_cells();
            // The rest of the text is still underlined.
            assert!(cells.len() >= all.len() / 2, "{classname}: {}", cells.len());
            let under: Vec<_> = cells.iter().filter(|p| contains(float, p)).collect();
            assert!(under.is_empty(), "{classname} {float:?}: {under:?}");
        }
    }
}
