// Licensed under the MIT License.
// Fork-only: spellcheck (not part of upstream microsoft/edit).

//! Spell checking for [`TextBuffer`]: underlining misspelled words while rendering,
//! finding the misspelled word at the cursor, and replacing it with a suggestion.
//!
//! This is a child module of `buffer` so that it can use the buffer's private helpers
//! without widening their visibility in upstream code.

use std::ops::Range;
use std::str;

use super::TextBuffer;
use crate::arena::{Arena, scratch_arena};
use crate::collections::BVec;
use crate::document::ReadableDocument;
use crate::framebuffer::Framebuffer;
use crate::helpers::*;
use crate::lsh::Highlighter;
use crate::simd;
use crate::spellcheck::scope::Scope;
use crate::spellcheck::{self, DictionaryStatus, tokenizer};
use crate::unicode::{self, Cursor};

/// Lines longer than this (including the newline) aren't spell checked.
/// The highlighter gives up at 32 KiB.
const MAX_LINE_LEN: usize = 16 * KIBI;

/// Per-buffer spell checking state. Lives in `TextBuffer::spell`.
#[derive(Default)]
pub(super) struct SpellState {
    /// Only the main document buffers have spell checking enabled; editlines don't.
    enabled: bool,
    /// [`TextBuffer::generation`] as of the last render.
    seen_generation: u32,
    /// The cursor offset right after the last edit, for as long as the cursor stays there.
    /// A misspelled word that ends here is still being typed, so it isn't underlined yet.
    suppress_at: Option<usize>,
}

/// The misspelled word at the cursor, see [`TextBuffer::spell_word_at_cursor`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpellWord {
    /// Byte range of the word in the buffer.
    pub range: Range<usize>,
    /// The word itself.
    pub text: String,
    /// Visual position (buffer coordinates, after word wrap) of the word's first character.
    pub visual_pos: Point,
    /// [`TextBuffer::generation`] at the time of the lookup.
    pub generation: u32,
}

/// Result of [`TextBuffer::spell_word_at_cursor`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpellAt {
    /// Spell checking is disabled globally or for this buffer, or doesn't apply to this language.
    Disabled,
    /// The dictionary is still loading.
    Loading,
    /// The dictionary failed to load.
    Unavailable,
    /// There's no misspelled word at the cursor.
    NoMisspelling,
    /// The cursor is inside or directly after this misspelled word.
    Word(SpellWord),
}

impl TextBuffer {
    /// Enables spell checking for this buffer. Only document buffers should enable it.
    pub fn set_spellcheck_enabled(&mut self, enabled: bool) {
        self.spell.enabled = enabled;
    }

    pub fn is_spellcheck_enabled(&self) -> bool {
        self.spell.enabled
    }

    /// Finds the misspelled word the cursor is in or directly after.
    pub fn spell_word_at_cursor(&mut self) -> SpellAt {
        if !self.spell.enabled
            || !spellcheck::is_enabled()
            || Scope::for_language(self.language) == Scope::Off
        {
            return SpellAt::Disabled;
        }
        if spellcheck::dictionary().is_none() {
            return spell_at_not_ready(spellcheck::dictionary_status());
        }

        let offset = self.cursor.offset;
        let y = self.cursor.logical_pos.y;
        let scratch = scratch_arena(None);
        let ranges = self.spell_collect(&scratch, y..y + 1);

        // Prefer the word the cursor is in over the one it's directly after.
        // The ranges are ascending, so that's the last match.
        let Some(range) = ranges.iter().rev().find(|r| r.start <= offset && offset <= r.end) else {
            return SpellAt::NoMisspelling;
        };

        let mut text = Vec::with_capacity(range.len());
        self.buffer.extract_raw(range.clone(), &mut text, 0);

        let (beg, _) = self.spell_range_cursors(self.cursor, range.clone());

        SpellAt::Word(SpellWord {
            range: range.clone(),
            // The tokenizer only yields valid UTF-8.
            text: String::from_utf8(text).unwrap_or_default(),
            visual_pos: beg.visual_pos,
            generation: self.generation(),
        })
    }

    /// Replaces `word` with `replacement` as a single undo step.
    /// Returns false (and does nothing) if the buffer changed since `word` was looked up.
    pub fn spell_replace(&mut self, word: &SpellWord, replacement: &str) -> bool {
        if self.generation() != word.generation
            || word.range.start > word.range.end
            || word.range.end > self.text_length()
        {
            return false;
        }

        self.cursor_move_to_offset(word.range.start);
        self.selection_update_offset(word.range.end);

        // Same as `find_and_replace()`: The barriers ensure that the replacement is its own
        // undo step, and that typing afterwards doesn't get merged into it.
        self.undo_barrier();
        self.write_raw(replacement.as_bytes());
        self.undo_barrier();
        true
    }

    /// Underlines misspelled words. Called by `render()` after `render_apply_highlights()`.
    pub(super) fn render_apply_spellcheck(
        &mut self,
        origin: Point,
        destination: Rect,
        logical_y_range: Range<CoordType>,
        focused: bool,
        fb: &mut Framebuffer,
    ) {
        if !self.spell.enabled {
            return;
        }

        // A word gets underlined once it's finished: After a space or punctuation was typed
        // after it, or after the cursor moved away from its end.
        let generation = self.generation();
        if self.spell.seen_generation != generation {
            self.spell.seen_generation = generation;
            self.spell.suppress_at = Some(self.cursor.offset);
        } else if self.spell.suppress_at != Some(self.cursor.offset) {
            self.spell.suppress_at = None;
        }

        if !spellcheck::is_enabled()
            || spellcheck::dictionary().is_none()
            || Scope::for_language(self.language) == Scope::Off
        {
            return;
        }

        let scratch = scratch_arena(None);
        let ranges = self.spell_collect(&scratch, logical_y_range.clone());
        if ranges.is_empty() {
            return;
        }

        let suppress_at = if focused { self.spell.suppress_at } else { None };
        // Only set our bit, so that bold, italic, link underlines, etc., are kept.
        let attr = spellcheck::underline_attr();

        // Ranges are ascending, so we can chain the offset-to-position conversions,
        // starting at the beginning of the first visible line.
        let mut cursor =
            self.goto_line_start(self.cursor_for_rendering.unwrap(), logical_y_range.start);

        // Visible vertical range in visual coordinates.
        let visible_top = origin.y;
        let visible_bottom = origin.y + destination.height();

        // Text area boundaries in screen coordinates (excluding margin).
        let text_left = destination.left + self.margin_width;
        let text_right = destination.right;

        for range in ranges.iter() {
            if suppress_at == Some(range.end) {
                continue;
            }

            let (beg, end) = self.spell_range_cursors(cursor, range.clone());
            cursor = end;

            // NOTE: The rest of this loop is copied from `render_apply_highlights()` in mod.rs,
            // so that the upstream file stays unchanged. Keep the two in sync.
            //
            // Handle the case where the highlight spans multiple visual lines
            // due to word wrapping. The range is [beg, end) in terms of offsets,
            // which maps to visual lines [beg.visual_pos.y, end.visual_pos.y].
            //
            // When beg and end are on the same visual line, we highlight
            // [beg.visual_pos.x, end.visual_pos.x).
            //
            // When they span multiple lines:
            // - First line: [beg.visual_pos.x, end_of_line)
            // - Middle lines: [0, end_of_line)
            // - Last line: [0, end.visual_pos.x)
            //
            // However, if end.visual_pos.x == 0, the last line has no content
            // to highlight (the span ends exactly at the line boundary).
            let visual_y_end = if end.visual_pos.x == 0 && end.visual_pos.y > beg.visual_pos.y {
                // The span ends at position 0 of a new visual line, meaning
                // it actually ends at the end of the previous visual line.
                end.visual_pos.y - 1
            } else {
                end.visual_pos.y
            };

            // Use min/max to skip visual lines outside the visible vertical range.
            for visual_y in
                beg.visual_pos.y.max(visible_top)..(visual_y_end + 1).min(visible_bottom)
            {
                let vis_left = if visual_y == beg.visual_pos.y {
                    beg.visual_pos.x
                } else {
                    // Wrapped continuation lines start at visual x=0.
                    0
                };
                let vis_right = if visual_y == end.visual_pos.y {
                    end.visual_pos.x
                } else {
                    // Line extends to the word wrap column or beyond.
                    COORD_TYPE_SAFE_MAX
                };

                // Convert to screen coordinates.
                let screen_left = text_left + vis_left - origin.x;
                let screen_right = (text_left + vis_right - origin.x).min(text_right);
                let screen_y = destination.top + visual_y - origin.y;

                // Create the target rectangle, clamped to the text area.
                let rect = Rect {
                    left: screen_left.max(text_left),
                    top: screen_y,
                    right: screen_right,
                    bottom: screen_y + 1,
                };

                // Skip empty or invalid rectangles.
                if rect.left >= rect.right {
                    continue;
                }

                fb.replace_attr(rect, attr, attr);
            }
        }
    }

    /// Returns the cursors at the start and end of `range`, measured from `cursor`.
    fn spell_range_cursors(&self, cursor: Cursor, range: Range<usize>) -> (Cursor, Cursor) {
        let mut beg = self.cursor_move_to_offset_internal(cursor, range.start);
        let end = self.cursor_move_to_offset_internal(beg, range.end);

        // Words usually start at a wrap opportunity, and the cursor there is ambiguous:
        // It's measured at the end of the previous row, even if the word got wrapped onto the next one.
        // Underlining from there would underline the empty space at the end of the previous row.
        if end.visual_pos.y > beg.visual_pos.y {
            let next_row =
                self.cursor_move_to_visual_internal(beg, Point { x: 0, y: beg.visual_pos.y + 1 });
            if next_row.offset == range.start {
                beg = next_row;
            }
        }

        (beg, end)
    }

    /// Returns the absolute byte ranges of all misspelled words in the given logical lines, ascending.
    fn spell_collect<'a>(
        &mut self,
        arena: &'a Arena,
        logical_y_range: Range<CoordType>,
    ) -> BVec<'a, Range<usize>> {
        let mut res = BVec::empty();
        let Some(dict) = spellcheck::dictionary() else {
            return res;
        };
        let scope = Scope::for_language(self.language);
        if scope == Scope::Off || logical_y_range.is_empty() {
            return res;
        }

        let mut highlighter = match self.language {
            Some(language) if scope.needs_highlighter() => {
                Some(Highlighter::new(&self.buffer, language))
            }
            _ => None,
        };

        // Whether a line starts fresh (outside of fenced code blocks, etc.) is `is_idle()`
        // right before parsing it. For the first line, that requires parsing the one before it.
        if let Some(highlighter) = &mut highlighter
            && logical_y_range.start > 0
        {
            let scratch = scratch_arena(Some(arena));
            _ = self.highlighter_cache.parse_line(&scratch, highlighter, logical_y_range.start - 1);
        }

        // Start from whichever cursor is closer, like `render()` does.
        let mut cursor = {
            let a = self.cursor;
            let b = self.cursor_for_rendering.unwrap_or_default();
            let da = (a.logical_pos.y - logical_y_range.start).abs();
            let db = (b.logical_pos.y - logical_y_range.start).abs();
            if da < db { a } else { b }
        };

        for logical_y in logical_y_range {
            cursor = self.goto_line_start(cursor, logical_y);
            if cursor.logical_pos.y != logical_y {
                // Past the end of the buffer.
                break;
            }

            let scratch = scratch_arena(Some(arena));
            let starts_idle = highlighter.as_ref().is_none_or(|h| h.is_idle());
            let highlights = match &mut highlighter {
                Some(highlighter) => {
                    self.highlighter_cache.parse_line(&scratch, highlighter, logical_y)
                }
                None => BVec::empty(),
            };

            let Some(line) = read_line(&self.buffer, &scratch, cursor.offset) else {
                continue;
            };
            let line = unicode::strip_newline(line);
            if scope.skip_line(line, starts_idle) {
                continue;
            }

            let line_offset = cursor.offset;
            scope.for_each_segment(line, line_offset, &highlights, |segment| {
                let text = &line[segment.clone()];
                tokenizer::for_each_word(text, |range| {
                    // SAFETY: The tokenizer only yields ranges of valid UTF-8.
                    let word = unsafe { str::from_utf8_unchecked(&text[range.clone()]) };
                    if !spellcheck::is_word_correct(dict, word) {
                        let beg = line_offset + segment.start + range.start;
                        res.push(arena, beg..beg + word.len());
                    }
                });
            });
        }

        res
    }
}

/// [`SpellAt`] for a dictionary that isn't available (yet).
fn spell_at_not_ready(status: DictionaryStatus) -> SpellAt {
    match status {
        DictionaryStatus::Unavailable => SpellAt::Unavailable,
        // `Ready` only happens if loading finished in the meantime. It'll work on the next try.
        DictionaryStatus::NotStarted | DictionaryStatus::Loading | DictionaryStatus::Ready => {
            SpellAt::Loading
        }
    }
}

/// Reads the line starting at `offset`, including its newline, like `Highlighter::read_next_line()`.
/// The line is only copied into `arena` if it spans both halves of the gap buffer.
/// Returns `None` if the line is longer than [`MAX_LINE_LEN`].
fn read_line<'a>(
    doc: &'a dyn ReadableDocument,
    arena: &'a Arena,
    mut offset: usize,
) -> Option<&'a [u8]> {
    let chunk = doc.read_forward(offset);
    let (end, line) = simd::lines_fwd(chunk, 0, 0, 1);
    if end > MAX_LINE_LEN {
        return None;
    }

    offset += end;
    if line == 1 || doc.read_forward(offset).is_empty() {
        return Some(&chunk[..end]);
    }

    let mut buf = BVec::empty();
    buf.extend_from_slice(arena, &chunk[..end]);

    loop {
        let chunk = doc.read_forward(offset);
        if chunk.is_empty() {
            break;
        }

        let (end, line) = simd::lines_fwd(chunk, 0, 0, 1);
        if buf.len() + end > MAX_LINE_LEN {
            return None;
        }

        buf.extend_from_slice(arena, &chunk[..end]);
        offset += end;

        if line == 1 {
            break;
        }
    }

    Some(buf.leak())
}

#[cfg(test)]
mod tests {
    use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

    use super::*;
    use crate::buffer::CursorMovement;
    use crate::lsh::{LANGUAGES, Language};
    use crate::spellcheck::dictionary_blocking;

    /// The spell checking switches are process-wide, but tests run in parallel.
    /// Tests that need the defaults share this lock, tests that change them take it exclusively.
    static GLOBALS: RwLock<()> = RwLock::new(());

    /// Enables spell checking and loads the dictionary. The curly underline is the default.
    fn shared_globals() -> RwLockReadGuard<'static, ()> {
        let guard = GLOBALS.read().unwrap_or_else(PoisonError::into_inner);
        spellcheck::set_enabled(true);
        dictionary_blocking().unwrap();
        guard
    }

    /// Like [`shared_globals`], but lets the test change the switches.
    /// They're restored to the defaults on drop.
    struct ExclusiveGlobals {
        _guard: RwLockWriteGuard<'static, ()>,
    }

    impl Drop for ExclusiveGlobals {
        fn drop(&mut self) {
            spellcheck::configure(true, true);
        }
    }

    fn exclusive_globals() -> ExclusiveGlobals {
        let guard = GLOBALS.write().unwrap_or_else(PoisonError::into_inner);
        spellcheck::configure(true, true);
        dictionary_blocking().unwrap();
        ExclusiveGlobals { _guard: guard }
    }

    fn language(id: &str) -> &'static Language {
        LANGUAGES.iter().find(|l| l.id == id).unwrap_or_else(|| panic!("no language {id:?}"))
    }

    fn buffer(language_id: Option<&str>, text: &str) -> TextBuffer {
        let mut tb = TextBuffer::new(false).unwrap();
        // Keep newlines as-is on Windows, too.
        tb.set_crlf(false);
        tb.set_language(language_id.map(language));
        tb.set_spellcheck_enabled(true);
        tb.write_raw(text.as_bytes());
        tb
    }

    fn text(tb: &TextBuffer, range: Range<usize>) -> String {
        let mut text = Vec::new();
        tb.buffer.extract_raw(range, &mut text, 0);
        String::from_utf8(text).unwrap()
    }

    fn contents(tb: &TextBuffer) -> String {
        text(tb, 0..tb.text_length())
    }

    /// The misspelled words in the given logical lines.
    fn collect_lines(tb: &mut TextBuffer, lines: Range<CoordType>) -> Vec<String> {
        let scratch = scratch_arena(None);
        let ranges = tb.spell_collect(&scratch, lines);
        assert!(ranges.windows(2).all(|w| w[0].end <= w[1].start), "not ascending: {ranges:?}");
        ranges.iter().map(|r| text(tb, r.clone())).collect()
    }

    /// The misspelled words in the entire buffer.
    fn collect(tb: &mut TextBuffer) -> Vec<String> {
        let lines = 0..tb.logical_line_count();
        collect_lines(tb, lines)
    }

    #[test]
    fn test_dictionary_assumptions() {
        // The tests below rely on these being (in)correct.
        let dict = dictionary_blocking().unwrap();
        for word in [
            "acheive",
            "beleive",
            "coment",
            "definately",
            "freind",
            "mesage",
            "mispeled",
            "occured",
            "recieve",
            "seperate",
            "speling",
            "strng",
            "teh",
            "untill",
            "wierd",
            "wrod",
        ] {
            assert!(!spellcheck::is_word_correct(dict, word), "{word}");
        }
        for word in ["word", "words", "the", "hello", "world", "spelling", "misspelled"] {
            assert!(spellcheck::is_word_correct(dict, word), "{word}");
        }
    }

    #[test]
    fn test_collect_plain_text() {
        let _g = shared_globals();
        let mut tb = buffer(None, "Hello wrod, this is teh test.\n\nAnother line: recieve it.\n");
        assert_eq!(collect(&mut tb), ["wrod", "teh", "recieve"]);
        assert_eq!(collect_lines(&mut tb, 1..3), ["recieve"]);
        // Past the end of the buffer.
        assert_eq!(collect_lines(&mut tb, 3..10), [] as [&str; 0]);
    }

    #[test]
    fn test_collect_line_across_gap() {
        let _g = shared_globals();
        let mut tb = buffer(None, "hello wrd world\nteh");
        // Inserting moves the gap into the middle of "wrod", which then spans both chunks.
        tb.cursor_move_to_offset(8);
        tb.write_canon(b"o");
        assert_eq!(contents(&tb), "hello wrod world\nteh");
        assert_eq!(collect(&mut tb), ["wrod", "teh"]);
    }

    #[test]
    fn test_collect_markdown() {
        let _g = shared_globals();
        let doc = "\
Some prose with a speling mistake.
Read [teh docs](wrod) and [more](https://example.com/mispeled).
Use `wrod` and ``seperate`` code.
```python
def recieve(): # definately a comment
    return \"occured\"
```
```rust
// wierd comment
let s = \"wierd string\";
```
```wierd
untill the end
```
    indented code with beleive
- a list item with acheive
    - a nested item with freind
Back to coment prose.
";
        let mut tb = buffer(Some("markdown"), doc);
        assert_eq!(collect(&mut tb), ["speling", "teh", "acheive", "freind", "coment"]);

        // Starting inside a fenced code block still knows that it's code.
        assert_eq!(collect_lines(&mut tb, 4..6), [] as [&str; 0]);
        assert_eq!(collect_lines(&mut tb, 8..10), [] as [&str; 0]);
        assert_eq!(collect_lines(&mut tb, 12..13), [] as [&str; 0]);
        assert_eq!(collect_lines(&mut tb, 5..18), ["acheive", "freind", "coment"]);
    }

    #[test]
    fn test_collect_git_commit() {
        let _g = shared_globals();
        let doc = "\
Fix teh widget

The body has a mispeled word.
# Please enter the commit mesage for your changes.
# ------------------------ >8 ------------------------
diff --git a/src/main.rs b/src/main.rs
index 1234567..89abcde 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1 +1 @@
-old wrod of text
+new speling of text
";
        let mut tb = buffer(Some("git-commit"), doc);
        assert_eq!(collect(&mut tb), ["teh", "mispeled"]);
        // Starting inside the diff.
        assert_eq!(collect_lines(&mut tb, 10..12), [] as [&str; 0]);
    }

    #[test]
    fn test_collect_rust() {
        let _g = shared_globals();
        let doc = "\
// A coment with teh typo.
/* A block comment
   that spans wrod lines
   untill here */
fn mispeled_fn(speling: u32) {
    let wierd = \"a strng value\";
}
";
        let mut tb = buffer(Some("rust"), doc);
        assert_eq!(collect(&mut tb), ["coment", "teh", "wrod", "untill", "strng"]);
        // Starting inside the block comment.
        assert_eq!(collect_lines(&mut tb, 2..4), ["wrod", "untill"]);
    }

    #[test]
    fn test_collect_json() {
        let _g = shared_globals();
        let doc = "\
{
    \"mispeled\": \"a strng with teh typo\",
    // a coment
    /* a wierd
       block */
}
";
        let mut tb = buffer(Some("json"), doc);
        assert_eq!(collect(&mut tb), ["coment", "wierd"]);
    }

    #[test]
    fn test_collect_long_line() {
        let _g = shared_globals();
        let long = "teh ".repeat(MAX_LINE_LEN / 4 + 1);
        let mut tb = buffer(None, &format!("wrod\n{long}\nspeling"));
        assert_eq!(collect(&mut tb), ["wrod", "speling"]);

        // The same, with the long line spanning both halves of the gap buffer.
        tb.cursor_move_to_logical(Point { x: 10, y: 1 });
        tb.write_canon(b"a");
        assert_eq!(collect(&mut tb), ["wrod", "speling"]);

        // Lines up to the limit are checked, longer ones aren't.
        let mut tb = buffer(None, &format!("{}teh", " ".repeat(MAX_LINE_LEN - 3)));
        assert_eq!(collect(&mut tb), ["teh"]);
        let mut tb = buffer(None, &format!("{}teh", " ".repeat(MAX_LINE_LEN - 2)));
        assert_eq!(collect(&mut tb), [] as [&str; 0]);
    }

    #[test]
    fn test_collect_disabled() {
        let _g = shared_globals();
        let mut tb = buffer(Some("diff"), "+teh wrod\n");
        assert_eq!(collect(&mut tb), [] as [&str; 0]);
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Underline {
        Single,
        Curly,
    }

    use Underline::*;

    type Run = (CoordType, CoordType, Underline, String);

    fn run(row: CoordType, column: CoordType, style: Underline, text: &str) -> Run {
        (row, column, style, text.to_string())
    }

    /// Renders `tb` into a fresh framebuffer of the given size and returns the VT output.
    fn render_vt(tb: &mut TextBuffer, origin: Point, size: Size, focused: bool) -> String {
        let mut fb = Framebuffer::new();
        fb.flip(size);
        tb.set_width(size.width);
        tb.render(origin, size.as_rect(), focused, &mut fb);

        let scratch = scratch_arena(None);
        fb.render(&scratch).as_str().to_owned()
    }

    /// Like [`render_vt`], but returns the underlined text as `(row, column, style, text)`.
    fn render(tb: &mut TextBuffer, origin: Point, size: Size, focused: bool) -> Vec<Run> {
        underlined_runs(&render_vt(tb, origin, size, focused))
    }

    /// Interprets the VT output of `Framebuffer::render()` just enough
    /// to return every run of underlined text. Assumes 1 column per `char`.
    fn underlined_runs(vt: &str) -> Vec<Run> {
        let mut runs: Vec<Run> = Vec::new();
        let mut style = None;
        let mut row = 0;
        let mut column = 0;
        let mut chars = vt.chars();

        while let Some(ch) = chars.next() {
            if ch != '\x1b' {
                if let Some(style) = style {
                    match runs.last_mut() {
                        Some((r, c, s, text))
                            if *r == row
                                && *s == style
                                && *c + text.chars().count() as CoordType == column =>
                        {
                            text.push(ch)
                        }
                        _ => runs.push((row, column, style, ch.to_string())),
                    }
                }
                column += 1;
                continue;
            }

            assert_eq!(chars.next(), Some('['), "only CSI sequences are expected");
            let mut params = String::new();
            let final_byte = loop {
                let ch = chars.next().unwrap();
                if ('\x40'..='\x7e').contains(&ch) {
                    break ch;
                }
                params.push(ch);
            };

            match final_byte {
                'H' => {
                    let (r, c) = params.split_once(';').unwrap();
                    row = r.parse::<CoordType>().unwrap() - 1;
                    column = c.parse::<CoordType>().unwrap() - 1;
                }
                'm' => {
                    let mut params = params.split(';');
                    while let Some(param) = params.next() {
                        match param {
                            "" | "0" | "24" | "4:0" => style = None,
                            "4" | "4:1" => style = Some(Single),
                            "4:3" => style = Some(Curly),
                            // `38;2;r;g;b` or `38;5;n`, etc.
                            "38" | "48" | "58" => {
                                let n = if params.next() == Some("2") { 3 } else { 1 };
                                params.nth(n - 1);
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }

        runs
    }

    fn size(width: CoordType, height: CoordType) -> Size {
        Size { width, height }
    }

    #[test]
    fn test_underlined_runs() {
        let vt = "\x1b[m\x1b[1;1H\x1b[48;2;0;0;0m\x1b[38;2;1;2;3mab\x1b[4:3m\x1b[58:2::255:62:48mcd\
                  \x1b[38;5;1mef\x1b[59m\x1b[24mgh\x1b[2;1Hi\x1b[4mjk\x1b[24ml\x1b[1;5H\x1b[5 q\x1b[?25h";
        assert_eq!(underlined_runs(vt), [run(0, 2, Curly, "cdef"), run(1, 1, Single, "jk")]);
    }

    #[test]
    fn test_render_plain_text() {
        let _g = shared_globals();
        let mut tb = buffer(None, "Hello wrod, this is teh test.\nOK speling");
        let vt = render_vt(&mut tb, Point::default(), size(40, 3), false);
        assert_eq!(
            underlined_runs(&vt),
            [run(0, 6, Curly, "wrod"), run(0, 20, Curly, "teh"), run(1, 3, Curly, "speling")]
        );
        // The exact sequences, see `Framebuffer::format_underline()`.
        assert!(vt.contains("\x1b[4:3m\x1b[58:2::255:62:48mwrod\x1b[59m\x1b[24m,"), "{vt:?}");
    }

    #[test]
    fn test_render_keeps_other_attributes() {
        let _g = shared_globals();
        let mut tb = buffer(Some("markdown"), "Some **wrod** here\n");
        let vt = render_vt(&mut tb, Point::default(), size(20, 2), false);
        assert_eq!(underlined_runs(&vt), [run(0, 7, Curly, "wrod")]);
        // Still bold.
        assert!(vt.contains("\x1b[1m"), "{vt:?}");
    }

    #[test]
    fn test_render_code_comment() {
        let _g = shared_globals();
        let mut tb = buffer(Some("rust"), "let x = 1; // A wrod\n");
        assert_eq!(
            render(&mut tb, Point::default(), size(30, 2), false),
            [run(0, 16, Curly, "wrod")]
        );
    }

    #[test]
    fn test_render_word_wrap() {
        let _g = shared_globals();

        // "mispeled" doesn't fit on a row, so it's split across two.
        let mut tb = buffer(None, "an mispeled teh");
        tb.set_word_wrap(true);
        assert_eq!(
            render(&mut tb, Point::default(), size(6, 4), false),
            [run(1, 0, Curly, "mispel"), run(2, 0, Curly, "ed"), run(2, 3, Curly, "teh")]
        );

        // "wrod" is wrapped onto the next row as a whole. The end of the first row stays clear.
        let mut tb = buffer(None, "hello wrod");
        tb.set_word_wrap(true);
        assert_eq!(
            render(&mut tb, Point::default(), size(8, 3), false),
            [run(1, 0, Curly, "wrod")]
        );

        // The same, because the space after "wrod" doesn't fit into the first row.
        let mut tb = buffer(None, "hello wrod more");
        tb.set_word_wrap(true);
        assert_eq!(
            render(&mut tb, Point::default(), size(10, 3), false),
            [run(1, 0, Curly, "wrod")]
        );

        // The word ends exactly at the end of a row. The row after it must stay clear.
        let mut tb = buffer(None, "mispeled\nteh");
        tb.set_word_wrap(true);
        assert_eq!(
            render(&mut tb, Point::default(), size(4, 4), false),
            [run(0, 0, Curly, "misp"), run(1, 0, Curly, "eled"), run(2, 0, Curly, "teh")]
        );

        // Scrolled down, so that the word starts above the visible area.
        let mut tb = buffer(None, "an mispeled teh");
        tb.set_word_wrap(true);
        assert_eq!(
            render(&mut tb, Point { x: 0, y: 2 }, size(6, 2), false),
            [run(0, 0, Curly, "ed"), run(0, 3, Curly, "teh")]
        );
    }

    #[test]
    fn test_render_horizontal_scroll() {
        let _g = shared_globals();
        let mut tb = buffer(None, "hello teh world wrod\nso");

        // "teh" is fully visible, "wrod" is out of view on the right.
        assert_eq!(
            render(&mut tb, Point { x: 4, y: 0 }, size(10, 2), false),
            [run(0, 2, Curly, "teh")]
        );

        // "teh" is cut off on the left, "wrod" on the right.
        assert_eq!(
            render(&mut tb, Point { x: 7, y: 0 }, size(10, 2), false),
            [run(0, 0, Curly, "eh"), run(0, 9, Curly, "w")]
        );

        // Both are out of view on the left.
        assert_eq!(render(&mut tb, Point { x: 20, y: 0 }, size(10, 2), false), []);
    }

    #[test]
    fn test_render_margin() {
        let _g = shared_globals();
        let mut tb = buffer(None, "teh\nso wrod");
        tb.set_margin_enabled(true);
        // The margin is "1 │ ", 4 columns wide.
        assert_eq!(tb.margin_width(), 4);
        assert_eq!(
            render(&mut tb, Point::default(), size(20, 3), false),
            [run(0, 4, Curly, "teh"), run(1, 7, Curly, "wrod")]
        );

        // With horizontal scrolling, the underline must not extend into the margin.
        assert_eq!(
            render(&mut tb, Point { x: 1, y: 0 }, size(20, 3), false),
            [run(0, 4, Curly, "eh"), run(1, 6, Curly, "wrod")]
        );
    }

    #[test]
    fn test_render_tabs() {
        let _g = shared_globals();
        // Tabs expand to the next tab stop, which moves the underline along with the word.
        for (tab_size, expected) in [(4, 4), (8, 8)] {
            let mut tb = buffer(None, "\tteh x\nab\twrod");
            tb.set_tab_size(tab_size);
            assert_eq!(
                render(&mut tb, Point::default(), size(30, 2), false),
                [run(0, expected, Curly, "teh"), run(1, expected, Curly, "wrod")],
                "tab size {tab_size}"
            );
        }
    }

    #[test]
    fn test_render_plain_underline() {
        let _g = exclusive_globals();
        spellcheck::configure(true, false);
        let mut tb = buffer(None, "Hello wrod, this is teh test.");
        assert_eq!(
            render(&mut tb, Point::default(), size(40, 1), false),
            [run(0, 6, Single, "wrod"), run(0, 20, Single, "teh")]
        );
    }

    #[test]
    fn test_render_disabled() {
        let _g = exclusive_globals();
        let mut tb = buffer(None, "Hello wrod.");
        let screen = size(20, 1);

        assert_eq!(render(&mut tb, Point::default(), screen, false), [run(0, 6, Curly, "wrod")]);

        tb.set_spellcheck_enabled(false);
        assert_eq!(render(&mut tb, Point::default(), screen, false), []);
        tb.set_spellcheck_enabled(true);

        spellcheck::set_enabled(false);
        assert_eq!(render(&mut tb, Point::default(), screen, false), []);
        spellcheck::set_enabled(true);

        assert_eq!(render(&mut tb, Point::default(), screen, false), [run(0, 6, Curly, "wrod")]);
    }

    #[test]
    fn test_finished_word_rule() {
        let _g = shared_globals();
        let mut tb = buffer(None, "");
        let screen = size(30, 1);
        let render = |tb: &mut TextBuffer, focused| render(tb, Point::default(), screen, focused);

        // The word is still being typed.
        tb.write_canon(b"hello wrod");
        assert_eq!(render(&mut tb, true), []);
        // Not while unfocused, though.
        assert_eq!(render(&mut tb, false), [run(0, 6, Curly, "wrod")]);
        // The cursor is still at its end.
        assert_eq!(render(&mut tb, true), []);

        // Finished with a space.
        tb.write_canon(b" ");
        assert_eq!(render(&mut tb, true), [run(0, 6, Curly, "wrod")]);

        // Finished with punctuation.
        tb.write_canon(b"teh,");
        assert_eq!(render(&mut tb, true), [run(0, 6, Curly, "wrod"), run(0, 11, Curly, "teh")]);

        // Finished by moving the cursor away and back.
        tb.write_canon(b" speling");
        assert_eq!(render(&mut tb, true), [run(0, 6, Curly, "wrod"), run(0, 11, Curly, "teh")]);
        tb.cursor_move_delta(CursorMovement::Grapheme, -1);
        let all =
            [run(0, 6, Curly, "wrod"), run(0, 11, Curly, "teh"), run(0, 16, Curly, "speling")];
        assert_eq!(render(&mut tb, true), all);
        tb.cursor_move_delta(CursorMovement::Grapheme, 1);
        assert_eq!(render(&mut tb, true), all);

        // Editing a finished word makes it the word being typed again.
        tb.delete(CursorMovement::Grapheme, -1);
        assert_eq!(render(&mut tb, true), all[..2]);
    }

    fn word(tb: &TextBuffer, range: Range<usize>, visual_pos: Point) -> SpellAt {
        SpellAt::Word(SpellWord {
            text: text(tb, range.clone()),
            range,
            visual_pos,
            generation: tb.generation(),
        })
    }

    fn word_at(tb: &mut TextBuffer, offset: usize) -> SpellAt {
        tb.cursor_move_to_offset(offset);
        tb.spell_word_at_cursor()
    }

    #[test]
    fn test_word_at_cursor() {
        let _g = shared_globals();
        let mut tb = buffer(None, "hello wrod  world\nnext teh");
        let wrod = word(&tb, 6..10, Point { x: 6, y: 0 });

        // Inside the word, at its start and at its end.
        assert_eq!(word_at(&mut tb, 8), wrod);
        assert_eq!(word_at(&mut tb, 6), wrod);
        assert_eq!(word_at(&mut tb, 10), wrod);

        // On a correct word.
        assert_eq!(word_at(&mut tb, 2), SpellAt::NoMisspelling);
        assert_eq!(word_at(&mut tb, 14), SpellAt::NoMisspelling);
        // On whitespace that doesn't touch a misspelled word.
        assert_eq!(word_at(&mut tb, 11), SpellAt::NoMisspelling);
        assert_eq!(word_at(&mut tb, 5), SpellAt::NoMisspelling);

        // On another line.
        assert_eq!(word_at(&mut tb, 24), word(&tb, 23..26, Point { x: 5, y: 1 }));
        assert_eq!(word_at(&mut tb, 26), word(&tb, 23..26, Point { x: 5, y: 1 }));
    }

    #[test]
    fn test_word_at_cursor_ignores_finished_word_rule() {
        let _g = shared_globals();
        let mut tb = buffer(None, "");
        tb.write_canon(b"hello wrod");
        assert_eq!(render(&mut tb, Point::default(), size(20, 1), true), []);
        assert_eq!(tb.spell_word_at_cursor(), word(&tb, 6..10, Point { x: 6, y: 0 }));
    }

    #[test]
    fn test_word_at_cursor_word_wrap() {
        let _g = shared_globals();
        let mut tb = buffer(None, "so hello teh");
        tb.set_word_wrap(true);
        tb.set_width(10);
        // "teh" wraps onto the second row.
        assert_eq!(word_at(&mut tb, 10), word(&tb, 9..12, Point { x: 0, y: 1 }));
    }

    #[test]
    fn test_word_at_cursor_disabled() {
        let _g = exclusive_globals();

        let mut tb = buffer(None, "wrod");
        tb.set_spellcheck_enabled(false);
        assert_eq!(tb.spell_word_at_cursor(), SpellAt::Disabled);

        let mut tb = buffer(Some("diff"), "wrod");
        assert_eq!(tb.spell_word_at_cursor(), SpellAt::Disabled);

        let mut tb = buffer(None, "wrod");
        spellcheck::set_enabled(false);
        assert_eq!(tb.spell_word_at_cursor(), SpellAt::Disabled);
        spellcheck::set_enabled(true);
        assert_eq!(tb.spell_word_at_cursor(), word(&tb, 0..4, Point::default()));
    }

    #[test]
    fn test_word_at_cursor_loading() {
        let _g = shared_globals();

        // The dictionary is shared by all tests and can't be unloaded,
        // so this only tests the mapping from the dictionary's status.
        assert_eq!(spell_at_not_ready(DictionaryStatus::NotStarted), SpellAt::Loading);
        assert_eq!(spell_at_not_ready(DictionaryStatus::Loading), SpellAt::Loading);
        assert_eq!(spell_at_not_ready(DictionaryStatus::Unavailable), SpellAt::Unavailable);

        let mut tb = buffer(None, "fine");
        assert_eq!(tb.spell_word_at_cursor(), SpellAt::NoMisspelling);
    }

    fn replace_at(tb: &mut TextBuffer, offset: usize, replacement: &str) -> bool {
        let SpellAt::Word(word) = word_at(tb, offset) else {
            panic!("no misspelled word at {offset}");
        };
        tb.spell_replace(&word, replacement)
    }

    #[test]
    fn test_replace() {
        let _g = shared_globals();
        let mut tb = buffer(None, "hello wrod world\nteh end");

        assert!(replace_at(&mut tb, 8, "word"));
        assert_eq!(contents(&tb), "hello word world\nteh end");
        assert_eq!(tb.cursor_offset(), 10);
        assert!(!tb.has_selection());

        // A different length on another line.
        assert!(replace_at(&mut tb, 20, "the"));
        assert_eq!(contents(&tb), "hello word world\nthe end");

        for _ in 0..2 {
            tb.undo();
            assert_eq!(contents(&tb), "hello word world\nteh end");
            tb.undo();
            assert_eq!(contents(&tb), "hello wrod world\nteh end");
            tb.redo();
            assert_eq!(contents(&tb), "hello word world\nteh end");
            tb.redo();
            assert_eq!(contents(&tb), "hello word world\nthe end");
        }
    }

    #[test]
    fn test_replace_then_type() {
        let _g = shared_globals();
        let mut tb = buffer(None, "hello wrod world");

        assert!(replace_at(&mut tb, 10, "word"));
        tb.write_canon(b"s");
        tb.write_canon(b"!");
        assert_eq!(contents(&tb), "hello words! world");

        tb.undo();
        assert_eq!(contents(&tb), "hello word world");
        tb.undo();
        assert_eq!(contents(&tb), "hello wrod world");
        tb.redo();
        tb.redo();
        assert_eq!(contents(&tb), "hello words! world");
    }

    #[test]
    fn test_replace_stale() {
        let _g = shared_globals();
        let mut tb = buffer(None, "hello wrod world");
        let SpellAt::Word(word) = word_at(&mut tb, 8) else { panic!() };

        tb.cursor_move_to_offset(0);
        tb.write_canon(b"Oh, ");
        assert!(!tb.spell_replace(&word, "word"));
        assert_eq!(contents(&tb), "Oh, hello wrod world");

        // Undo restores the generation the word was looked up in.
        tb.undo();
        assert!(tb.spell_replace(&word, "word"));
        assert_eq!(contents(&tb), "hello word world");

        // Out of bounds.
        let bogus = SpellWord { range: 6..100, generation: tb.generation(), ..word };
        assert!(!tb.spell_replace(&bogus, "word"));
    }
}
