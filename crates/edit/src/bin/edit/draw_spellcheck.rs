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
    /// Whether the list still needs to take the focus. True until the first draw.
    focus: bool,
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

    let at = doc.buffer.borrow_mut().spell_word_at_cursor();
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
    Some(SpellPopup { buffer, word, items, message, focus: true })
}

fn draw_spell_popup(ctx: &mut Context, state: &mut State, textarea: Size, scroll: Point) {
    let Some(popup) = &mut state.spell.popup else {
        return;
    };

    if !state.documents.active().is_some_and(|doc| Rc::ptr_eq(&doc.buffer, &popup.buffer)) {
        state.spell.popup = None;
        ctx.needs_rerender();
        return;
    }

    let steal_focus = mem::take(&mut popup.focus);
    let popup = &*popup;

    let pos = {
        let tb = popup.buffer.borrow();
        let word_pos = popup.word.as_ref().map_or_else(|| tb.cursor_visual_pos(), |w| w.visual_pos);
        let size = Size {
            // +2 for the list item prefix ("> ") and +2 for the border.
            width: popup.labels().map(label_width).max().unwrap_or(0) + 4,
            height: popup.labels().count() as CoordType + 2,
        };
        popup_position(textarea, tb.margin_width(), word_pos, scroll, size)
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
/// * `textarea`: size of the textarea.
/// * `margin_width`: width of the line number margin inside the textarea.
/// * `word_pos`: visual position of the word's first character.
/// * `scroll`: scroll offset of the textarea.
/// * `popup`: size of the popup, including its border.
///
/// The popup goes below the word, or above it if it only fits there. It's moved left if it would
/// stick out on the right. Floats don't do either on their own: They're only clipped to the viewport.
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
    let x = x.min(textarea.width - popup.width).max(0);

    Point { x, y }
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
        // Ends exactly at the right edge (columns 68-79).
        assert_eq!(popup_position(TEXTAREA, 6, pt(62, 0), pt(0, 0), POPUP), pt(68, 1));
        // Would stick out, so it moves left.
        assert_eq!(popup_position(TEXTAREA, 6, pt(70, 0), pt(0, 0), POPUP), pt(68, 1));
        // Scrolled horizontally, so it fits again.
        assert_eq!(popup_position(TEXTAREA, 6, pt(100, 0), pt(50, 0), POPUP), pt(56, 1));
        // Moved left and flipped above at the same time.
        assert_eq!(popup_position(TEXTAREA, 6, pt(78, 19), pt(0, 0), POPUP), pt(68, 14));
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

        /// Injects a popup, bypassing `spell_word_at_cursor()`.
        fn open(&mut self, word: Option<SpellWord>, items: &[&str], message: Option<LocId>) {
            self.state.spell.popup = Some(SpellPopup {
                buffer: self.buffer(),
                word,
                items: items.iter().map(|s| s.to_string()).collect(),
                message,
                focus: true,
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

        /// Left-clicks the label with exactly the given text.
        fn click_label(&mut self, text: &str) {
            let pos = {
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
            };
            self.mouse(InputMouseState::Left, pos);
            self.mouse(InputMouseState::None, pos);
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

    /// Whether `spell_open()` would open a popup right now.
    fn applicable(h: &Harness, quiet: bool) -> bool {
        match h.buffer().borrow_mut().spell_word_at_cursor() {
            SpellAt::Disabled => false,
            SpellAt::Word(_) => true,
            _ => !quiet,
        }
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

        // Moved left at the right edge.
        h.state.spell.popup.as_mut().unwrap().word.as_mut().unwrap().visual_pos = pt(75, 0);
        h.frame(None);
        assert_eq!(h.outer("spell-suggestions"), at(textarea.right - w, textarea.top + 1));

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

        // The selected item replaces the word. The skeleton's `spell_replace()` does nothing.
        let text = h.contents();
        assert!(text == "quokka x\n" || text == "teh x\n", "{text:?}");

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
            assert_eq!(h.is_open(), applicable(&h, false));

            // Once `spell_word_at_cursor()` finds words (not in the skeleton):
            // Enter replaces the word with the first suggestion, as a single undo step.
            if let Some(items) = h.suggestions()
                && let Some(first) = items.first()
            {
                h.key(vk::RETURN);
                assert!(!h.is_open());
                assert_eq!(h.contents(), format!("a {first} x\n"));
                h.key(kbmod::CTRL | vk::Z);
                assert_eq!(h.contents(), "a teh x\n");
            }

            // On a correct word.
            let mut h = Harness::new("a teh x\n");
            h.key(key);
            assert_eq!(h.is_open(), applicable(&h, false));
        }

        // Edit > Spelling Suggestions sets `request`.
        let mut h = Harness::new("a teh x\n");
        h.buffer().borrow_mut().cursor_move_to_logical(pt(3, 0));
        h.state.spell.request = true;
        h.frame(None);
        assert!(!h.state.spell.request);
        assert_eq!(h.is_open(), applicable(&h, false));
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
        assert_eq!(h.is_open(), applicable(&h, false));
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

    #[test]
    fn test_right_click() {
        let _lock = enable_spellcheck();

        let mut h = Harness::new("a teh x\n");
        let textarea = h.outer("textarea");
        let margin = h.buffer().borrow().margin_width();
        let click = |x| pt(textarea.left + margin + x, textarea.top);

        // On the misspelled word: opens only if there's a word (quiet mode).
        h.mouse(InputMouseState::Right, click(3));
        h.mouse(InputMouseState::None, click(3));
        assert_eq!(h.is_open(), applicable(&h, true));
        if h.is_open() {
            // Below the word, not below the click.
            let popup = h.outer("spell-suggestions");
            assert_eq!((popup.left, popup.top), (textarea.left + margin + 2, textarea.top + 1));
        }
        h.state.spell.popup = None;
        h.frame(None);

        // On a correct word: never opens.
        h.mouse(InputMouseState::Right, click(0));
        h.mouse(InputMouseState::None, click(0));
        assert!(!h.is_open());
        assert!(!applicable(&h, true));
    }
}
