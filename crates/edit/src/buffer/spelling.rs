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
use crate::spellcheck::scope::{MarkdownState, Scope};
use crate::spellcheck::{self, DictionaryStatus, tokenizer};
use crate::unicode::{self, Cursor};

/// Lines longer than this (including the newline) aren't spell checked.
/// The highlighter gives up at 32 KiB.
const MAX_LINE_LEN: usize = 16 * KIBI;

/// Logical lines between two [`MarkdownCheckpoints`].
#[cfg(debug_assertions)]
const MARKDOWN_INTERVAL: CoordType = 16;
#[cfg(not(debug_assertions))]
const MARKDOWN_INTERVAL: CoordType = 1024;

/// Per-buffer spell checking state. Lives in `TextBuffer::spell`.
#[derive(Default)]
pub(super) struct SpellState {
    /// Only the main document buffers have spell checking enabled; editlines don't.
    enabled: bool,
    /// [`TextBuffer::generation`] as of the last render.
    seen_generation: u32,
    /// The highest [`TextBuffer::generation`] seen by a render. New edits get a higher one,
    /// while undo and redo restore older ones.
    max_generation: u32,
    /// The cursor offset right after the last edit, for as long as the cursor stays there.
    /// A misspelled word that ends here is still being typed, so it isn't underlined yet.
    suppress_at: Option<usize>,
    markdown: MarkdownCheckpoints,
}

/// Markdown's block state at the start of every [`MARKDOWN_INTERVAL`]th line,
/// so that finding the state at the first visible line doesn't scan the whole file every frame.
#[derive(Default)]
struct MarkdownCheckpoints {
    /// The [`TextBuffer::generation`] the checkpoints are valid for.
    generation: u32,
    /// The first line changed since then, see [`SpellState::invalidate_from`].
    dirty_from: Option<CoordType>,
    /// The offset of line `i * MARKDOWN_INTERVAL`, and the state at its start.
    states: Vec<(usize, MarkdownState)>,
}

impl SpellState {
    /// `TextBuffer` calls this wherever a change invalidates its highlighter cache:
    /// Logical line `line` and everything after it may have changed.
    pub(super) fn invalidate_from(&mut self, line: CoordType) {
        let dirty = &mut self.markdown.dirty_from;
        *dirty = Some(dirty.map_or(line, |d| d.min(line)));
    }
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
            // Undo and redo aren't typing, so they don't hide the word at the cursor.
            // (Undoing a replacement leaves the cursor at the end of the misspelled word.)
            if generation > self.spell.max_generation {
                self.spell.max_generation = generation;
                self.spell.suppress_at = Some(self.cursor.offset);
            } else {
                self.spell.suppress_at = None;
            }
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

        let mut md = if scope.needs_markdown_state() {
            self.spell_markdown_state_at(arena, logical_y_range.start)
        } else {
            MarkdownState::default()
        };

        let mut highlighter = match self.language {
            Some(language) if scope.needs_highlighter() => {
                Some(Highlighter::new(&self.buffer, language))
            }
            _ => None,
        };

        // `is_idle()` right before parsing a line tells whether it starts fresh (e.g. outside a diff).
        // For the first line, that requires parsing the one before it. `parse_line()` seeks
        // on its own otherwise, so the other scopes skip this (it can cost up to a checkpoint interval).
        if let Some(highlighter) = &mut highlighter
            && scope.needs_idle_state()
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
            let starts_idle =
                !scope.needs_idle_state() || highlighter.as_ref().is_none_or(|h| h.is_idle());
            let highlights = match &mut highlighter {
                Some(highlighter) => {
                    self.highlighter_cache.parse_line(&scratch, highlighter, logical_y)
                }
                None => BVec::empty(),
            };

            let (line, line_len) = read_line(&self.buffer, &scratch, cursor.offset);
            let line = unicode::strip_newline(line);
            // Lines that are too long to check still go through `skip_line()`, to keep `md` in sync.
            if scope.skip_line(line, starts_idle, &mut md) || line_len > MAX_LINE_LEN {
                continue;
            }

            let line_offset = cursor.offset;
            scope.for_each_segment(line, line_offset, &highlights, |segment, kind| {
                let text = &line[segment.clone()];
                tokenizer::for_each_word_in(text, kind, |range| {
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

    /// Returns Markdown's block state at the start of logical line `y`.
    ///
    /// That depends on every line before it, so the states at every [`MARKDOWN_INTERVAL`]th line
    /// are kept, up to the first line an edit changes. Scrolling and typing then scan fewer than
    /// that many lines, unless an edit happened far above the first visible line.
    fn spell_markdown_state_at(&mut self, arena: &Arena, y: CoordType) -> MarkdownState {
        let cache = &mut self.spell.markdown;
        let generation = self.buffer.generation();
        // If the generation is the same, so is the text (e.g. after an edit and its undo).
        let dirty_from = cache.dirty_from.take();
        if cache.generation != generation {
            cache.generation = generation;
            // Like `HighlighterCache::invalidate_from()`. A change that didn't
            // report its first line (there shouldn't be any) drops all checkpoints.
            let keep = dirty_from
                .and_then(|line| usize::try_from(line).ok())
                .map_or(0, |line| line.div_ceil(MARKDOWN_INTERVAL as usize));
            cache.states.truncate(keep);
        }
        if cache.states.is_empty() {
            cache.states.push((0, MarkdownState::default()));
        }

        let n = usize::try_from(y / MARKDOWN_INTERVAL).unwrap_or(0).min(cache.states.len() - 1);
        let (mut offset, mut state) = cache.states[n];
        let mut line_y = n as CoordType * MARKDOWN_INTERVAL;
        // Only a line that spans both halves of the gap buffer is copied into the arena,
        // so one is enough for the whole scan.
        let scratch = scratch_arena(Some(arena));

        while line_y < y {
            let (line, line_len) = read_line(&self.buffer, &scratch, offset);
            if line_len == 0 {
                // Past the end of the buffer.
                break;
            }

            state.next_line(unicode::strip_newline(line));
            offset += line_len;
            line_y += 1;

            if line_y % MARKDOWN_INTERVAL == 0
                && cache.states.len() == (line_y / MARKDOWN_INTERVAL) as usize
            {
                cache.states.push((offset, state));
            }
        }

        state
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
/// Returns the line and its length. If it's longer than [`MAX_LINE_LEN`], only that much of it is returned.
/// The line is only copied into `arena` if it spans both halves of the gap buffer.
fn read_line<'a>(
    doc: &'a dyn ReadableDocument,
    arena: &'a Arena,
    mut offset: usize,
) -> (&'a [u8], usize) {
    let chunk = doc.read_forward(offset);
    let (end, line) = simd::lines_fwd(chunk, 0, 0, 1);
    let mut len = end;

    offset += end;
    if line == 1 || doc.read_forward(offset).is_empty() {
        return (&chunk[..end.min(MAX_LINE_LEN)], len);
    }

    let mut buf = BVec::empty();
    buf.extend_from_slice(arena, &chunk[..end.min(MAX_LINE_LEN)]);

    loop {
        let chunk = doc.read_forward(offset);
        if chunk.is_empty() {
            break;
        }

        let (end, line) = simd::lines_fwd(chunk, 0, 0, 1);
        let room = MAX_LINE_LEN.saturating_sub(buf.len());
        buf.extend_from_slice(arena, &chunk[..end.min(room)]);
        len += end;
        offset += end;

        if line == 1 {
            break;
        }
    }

    (buf.leak(), len)
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, SeekFrom, Write};
    use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

    use super::*;
    use crate::buffer::CursorMovement;
    use crate::lsh::{LANGUAGES, Language};
    use crate::spellcheck::dictionary_blocking;
    use crate::sys::memfd;

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

    // Words from the add-on dictionary aren't flagged, in prose or in code comments.
    #[test]
    fn test_collect_addon_words() {
        let _g = shared_globals();
        let mut tb = buffer(None, "Update the config in the repo, then rebase. Ok, teh end.\n");
        assert_eq!(collect(&mut tb), ["teh"]);
        let mut tb = buffer(Some("rust"), "// TODO: dedupe the configs on localhost, wrod\n");
        assert_eq!(collect(&mut tb), ["wrod"]);
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

    /// The misspelled words in the entire buffer, along with their logical line.
    fn collect_with_lines(tb: &mut TextBuffer) -> Vec<(CoordType, String)> {
        (0..tb.logical_line_count())
            .flat_map(|y| collect_lines(tb, y..y + 1).into_iter().map(move |w| (y, w)))
            .collect()
    }

    /// Asserts that collecting from any line to the end of the buffer finds `expected`'s words
    /// from that line on. Goes top to bottom and then bottom to top, so that the Markdown
    /// checkpoints get both created and reused.
    fn assert_every_window(tb: &mut TextBuffer, expected: &[(CoordType, &str)]) {
        let n = tb.logical_line_count();
        for y0 in (0..=n).chain((0..=n).rev()) {
            let words: Vec<_> =
                expected.iter().filter(|(y, _)| *y >= y0).map(|(_, w)| *w).collect();
            assert_eq!(collect_lines(tb, y0..n), words, "from line {y0}");
        }
    }

    #[test]
    fn test_collect_markdown_lost_fences() {
        let _g = shared_globals();
        // The highlighter loses track of the fences after the YAML block scalar (it ends at the
        // closing fence) and the unmatched `'` in sh. The checked lines must not depend on it.
        let doc = "\
Intro with a speling mistake.
```yaml
jobs:
  build:
    steps:
      - run: |
          echo wrod
```
Prose after the fence with recieve in it.
```sh
$ tool --check
You don't have a freind
```
Then fix teh setting.
~~~
tilde fence with untill
~~~
```rust
let s = \"wierd\";
```
Trailing coment prose.
";
        let expected = [(0, "speling"), (8, "recieve"), (13, "teh"), (20, "coment")];
        let mut tb = buffer(Some("markdown"), doc);
        assert_eq!(collect_with_lines(&mut tb), expected.map(|(y, w)| (y, w.to_string())));
        assert_every_window(&mut tb, &expected);
        let mut tb = buffer(Some("markdown"), doc);
        assert_every_window(&mut tb, &expected);

        // Rendering a window that starts inside the sh fence, which opened above it.
        let mut tb = buffer(Some("markdown"), doc);
        assert_eq!(
            render(&mut tb, Point { x: 0, y: 11 }, size(40, 10), false),
            [run(2, 9, Curly, "teh"), run(9, 9, Curly, "coment")]
        );
    }

    #[test]
    fn test_collect_markdown_edits() {
        let _g = shared_globals();
        let mut doc = String::new();
        for i in 0..40 {
            if i % 10 == 5 {
                doc.push_str("```\nwierd code\n```\n");
            } else {
                doc.push_str("Some teh prose.\n");
            }
        }
        let mut tb = buffer(Some("markdown"), &doc);
        let original = assert_like_fresh(&mut tb);
        assert_eq!(original.len(), 36);
        assert!(original.iter().all(|(_, w)| w == "teh"), "{original:?}");

        // Opening a fence at the top inverts all of them.
        tb.cursor_move_to_offset(0);
        tb.write_canon(b"```\n");
        let inverted = assert_like_fresh(&mut tb);
        assert_eq!(inverted.len(), 4);
        assert!(inverted.iter().all(|(_, w)| w == "wierd"), "{inverted:?}");

        // Undo and redo restore older generations.
        tb.undo();
        assert_eq!(contents(&tb), doc);
        assert_eq!(assert_like_fresh(&mut tb), original);
        tb.redo();
        assert_eq!(assert_like_fresh(&mut tb), inverted);
        tb.undo();

        // Deleting the third fence's opening line (29) with backspace inverts the ones after it.
        tb.cursor_move_to_logical(Point { x: 0, y: 30 });
        for _ in 0..4 {
            tb.delete(CursorMovement::Grapheme, -1);
            assert_like_fresh(&mut tb);
        }
        assert_ne!(assert_like_fresh(&mut tb), original);
        // Typing at the end, and undoing all of it.
        tb.cursor_move_to_offset(tb.text_length());
        tb.write_canon(b"teh");
        assert_like_fresh(&mut tb);
        while contents(&tb) != doc {
            tb.undo();
            assert_like_fresh(&mut tb);
        }
        assert_eq!(assert_like_fresh(&mut tb), original);

        // Loading a file, and then editing below its first change before the next render.
        let mut file = memfd().unwrap();
        file.write_all(format!("```\n{doc}").as_bytes()).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        tb.read_file(&mut file, Some("UTF-8")).unwrap();
        tb.cursor_move_to_logical(Point { x: 0, y: 45 });
        tb.write_canon(b"x");
        assert_eq!(assert_like_fresh(&mut tb), inverted);
    }

    /// Asserts that `tb` finds the same misspelled words from every line on as a fresh buffer
    /// with the same text (which has no Markdown checkpoints yet), and returns them.
    fn assert_like_fresh(tb: &mut TextBuffer) -> Vec<(CoordType, String)> {
        let mut fresh = buffer(Some("markdown"), &contents(tb));
        let expected = collect_with_lines(&mut fresh);
        let words: Vec<_> = expected.iter().map(|(y, w)| (*y, w.as_str())).collect();
        assert_every_window(tb, &words);
        expected
    }

    #[test]
    fn test_markdown_checkpoints() {
        let _g = shared_globals();
        let interval = MARKDOWN_INTERVAL;
        let mut tb = buffer(Some("markdown"), &"Prose.\n".repeat(6 * interval as usize + 4));
        let scratch = scratch_arena(None);
        let state_at = |tb: &mut TextBuffer, y| _ = tb.spell_markdown_state_at(&scratch, y);
        let checkpoints = |tb: &TextBuffer| tb.spell.markdown.states.clone();

        state_at(&mut tb, 6 * interval);
        assert_eq!(checkpoints(&tb).len(), 7);
        // Scrolling up reuses them.
        state_at(&mut tb, 2 * interval + 1);
        assert_eq!(checkpoints(&tb).len(), 7);
        // Mark the checkpoint at the start of line `interval`, to tell whether it gets recomputed.
        tb.spell.markdown.states[1].0 = usize::MAX;

        // Edits keep the checkpoints at their first changed line and above, like the highlighter's.
        tb.cursor_move_to_logical(Point { x: 3, y: 2 * interval + 8 });
        tb.write_canon(b"x");
        state_at(&mut tb, 2 * interval + 1);
        assert_eq!(checkpoints(&tb).len(), 3);
        assert_eq!(checkpoints(&tb)[1].0, usize::MAX);
        state_at(&mut tb, 6 * interval);
        assert_eq!(checkpoints(&tb).len(), 7);
        // So do undo and redo.
        tb.undo();
        tb.redo();
        tb.undo();
        state_at(&mut tb, 2 * interval + 1);
        assert_eq!(checkpoints(&tb).len(), 3);
        assert_eq!(checkpoints(&tb)[1].0, usize::MAX);

        // An edit and its undo before the next lookup don't change anything.
        state_at(&mut tb, 6 * interval);
        let before = checkpoints(&tb);
        tb.cursor_move_to_logical(Point { x: 0, y: 0 });
        tb.write_canon(b"x");
        tb.undo();
        state_at(&mut tb, 2 * interval + 1);
        assert_eq!(checkpoints(&tb), before);

        // A change that didn't report its first line drops all of them.
        tb.cursor_move_to_logical(Point { x: 0, y: 5 * interval });
        tb.write_canon(b"x");
        tb.spell.markdown.dirty_from = None;
        state_at(&mut tb, 2 * interval + 1);
        assert_eq!(checkpoints(&tb).len(), 3);
        assert_eq!(checkpoints(&tb)[1].0, interval as usize * "Prose.\n".len());
    }

    #[test]
    fn test_collect_markdown_long_lines() {
        let _g = shared_globals();
        // Lines too long to check still open fences and continue paragraphs.
        let info = "x".repeat(MAX_LINE_LEN);
        let long = "word ".repeat(MAX_LINE_LEN / 5 + 1);
        let doc =
            format!("```{info}\nwrod inside\n```\nteh after\n\n{long}\n    speling continues\n");
        let expected = [(3, "teh"), (6, "speling")];
        let mut tb = buffer(Some("markdown"), &doc);
        assert_every_window(&mut tb, &expected);

        // The same with the gap buffer's gap inside each of the long lines.
        for offset in [10, doc.find("word").unwrap() + 10] {
            let mut tb = buffer(Some("markdown"), &doc);
            tb.cursor_move_to_offset(offset);
            tb.write_canon(b"x");
            assert_every_window(&mut tb, &expected);
        }
    }

    #[test]
    fn test_collect_markdown_continuation_lines() {
        let _g = shared_globals();
        let doc = "\
- Top item
    - Nested item with a long sentence that
      wraps onto the next line with teh typo.

A paragraph that is hard wrapped
    and indented by four with wrod.

    indented code with untill
";
        let mut tb = buffer(Some("markdown"), doc);
        assert_eq!(collect(&mut tb), ["teh", "wrod"]);
        assert_every_window(&mut tb, &[(2, "teh"), (5, "wrod")]);
    }

    #[test]
    fn test_collect_markdown_list_fences() {
        let _g = shared_globals();
        // A fence on a list marker line used to be prose. Its closing fence then opened a fence,
        // which skipped prose until the next fence line, and checked the code after that.
        let doc = "\
# Setup

1. ```bash
   npm install
   ```
2. Run teh build.

## Usage

```
tool --flag
```

Then check the wrod output.

```
baz qux
```

More speling here.
";
        let expected = [(5, "teh"), (13, "wrod"), (19, "speling")];
        let mut tb = buffer(Some("markdown"), doc);
        assert_every_window(&mut tb, &expected);

        let doc = "Intro.\n\n- ```sh\n  npm install\n  ```\n- Then teh step.\n\nAfter wrod.\n";
        let mut tb = buffer(Some("markdown"), doc);
        assert_every_window(&mut tb, &[(5, "teh"), (7, "wrod")]);

        let doc = "+ ```\n  code wierd\n  ```\n\nAfter teh.\n";
        let mut tb = buffer(Some("markdown"), doc);
        assert_every_window(&mut tb, &[(4, "teh")]);

        // A fence opened inside an item is closed relative to the item's text.
        let doc = "- Step one:\n   ```sh\n    echo wierd\n    ```\nThen teh prose.\n";
        let mut tb = buffer(Some("markdown"), doc);
        assert_every_window(&mut tb, &[(4, "teh")]);

        // And in a nested item, indented by 4.
        let doc = "- a\n  - b\n    ```sh\n    echo wierd\n    ```\n  - c teh\n";
        let mut tb = buffer(Some("markdown"), doc);
        assert_every_window(&mut tb, &[(5, "teh")]);

        // A fence that's indented less than the item's text may be top-level. Its content at
        // column 0 used to be prose, and its closing fence then hid the rest of the file.
        let doc = "\
1. Create the settings file:

  ```yaml
name: wierd
on: push
  ```

2. Commit teh file.

More prose with wrod.
";
        let mut tb = buffer(Some("markdown"), doc);
        assert_every_window(&mut tb, &[(7, "teh"), (9, "wrod")]);
        let doc = "\
1. Step one
2. Step two

 ```
cat <<EOF > notes.txt
wierd text
EOF
 ```

Prose with teh typo.
";
        let mut tb = buffer(Some("markdown"), doc);
        assert_every_window(&mut tb, &[(9, "teh")]);
        let doc = "\
1. Step

  Explanation here.
  ```sh
cat <<EOF
hello wierd
EOF
  ```

Prose teh.
";
        let mut tb = buffer(Some("markdown"), doc);
        assert_every_window(&mut tb, &[(9, "teh")]);
        let doc = "1. Step\n\n  ```\n  code wierd\n     ```\n  still untill\n  ```\n\nAfter teh.\n";
        let mut tb = buffer(Some("markdown"), doc);
        assert_every_window(&mut tb, &[(8, "teh")]);
        // After a nested item, a fence that's indented by 4 still closes at its own indentation.
        let doc = "\
- Defines a variable:

    - Unadorned: shows the history.

    ```console
    irb> wierd
    ```

    Note teh output.
";
        let mut tb = buffer(Some("markdown"), doc);
        assert_every_window(&mut tb, &[(8, "teh")]);

        // As SPELLCHECK.md describes it: a fence in an item ends at its closing fence, and the
        // item's text after it is checked. One that's never closed ends at the first non-blank
        // line that's indented less than the item's text. Outside a list, it runs to the end,
        // and so does one after a nested item, where the item's column is a guess.
        let doc = "- Step:\n  ```sh\n  wierd\n  ```\n  More teh.\n- Next wrod.\n";
        let mut tb = buffer(Some("markdown"), doc);
        assert_every_window(&mut tb, &[(4, "teh"), (5, "wrod")]);
        let doc = "- Step:\n  ```sh\n  wierd\n\n  More teh.\nOutside wrod.\n";
        let mut tb = buffer(Some("markdown"), doc);
        assert_every_window(&mut tb, &[(5, "wrod")]);
        let doc = "- Step:\n  - Sub:\n\n  ```sh\n  wierd\nOutside wrod.\n";
        let mut tb = buffer(Some("markdown"), doc);
        assert_every_window(&mut tb, &[]);
        let doc = "Intro.\n```sh\nwierd\n\nOutside wrod.\n# Heading teh\n";
        let mut tb = buffer(Some("markdown"), doc);
        assert_every_window(&mut tb, &[]);
        // Known limitation: a tab after the marker counts as one space, so the item's text is at
        // column 2 instead of 4, and a paragraph indented by 6 is taken for code.
        let doc = "-\tTab item\n\n      six-space para teh\n\n    four-space para wrod\n";
        let mut tb = buffer(Some("markdown"), doc);
        assert_every_window(&mut tb, &[(4, "wrod")]);
    }

    #[test]
    fn test_collect_markdown_list_continuation() {
        let _g = shared_globals();
        // Paragraphs in list items after a blank line used to be indented code.
        let doc = "\
- First item

    Continuation paragraph with teh typo.

    - nested item with wrod

Back to top level speling.

1. Open the portal.

    Then recieve the key.

- item

\tTab continuation with acheive.

- a
  - b

  more a text

    continuation with freind

- item

      code wierd

After coment.
";
        let expected = [
            (2, "teh"),
            (4, "wrod"),
            (6, "speling"),
            (10, "recieve"),
            (14, "acheive"),
            (21, "freind"),
            (27, "coment"),
        ];
        let mut tb = buffer(Some("markdown"), doc);
        assert_every_window(&mut tb, &expected);
    }

    #[test]
    fn test_collect_markdown_front_matter() {
        let _g = shared_globals();
        // Obsidian notes, Jekyll pages, etc.
        let doc = "\
---
tags: [mispeled, wierd]
aliases:
  - Teh Alias
---
# A title with wrod

Body with speling.
";
        let mut tb = buffer(Some("markdown"), doc);
        assert_every_window(&mut tb, &[(5, "wrod"), (7, "speling")]);

        // A thematic break at the start isn't front matter.
        let doc = "---\n\nIntro wrod after a rule.\n\n---\n\nMore teh.\n";
        let mut tb = buffer(Some("markdown"), doc);
        assert_every_window(&mut tb, &[(2, "wrod"), (6, "teh")]);

        // Front matter that's never closed ends at the first line that can't be YAML.
        let doc = "---\ntitle: wierd\nSome intro with teh.\n";
        let mut tb = buffer(Some("markdown"), doc);
        assert_every_window(&mut tb, &[(2, "teh")]);
        // So does front matter that's closed later.
        let doc =
            "---\ntitle: x\nwierd line without colon\nauthor: Speling Person\n---\nBody teh.\n";
        let mut tb = buffer(Some("markdown"), doc);
        assert_every_window(&mut tb, &[(2, "wierd"), (3, "Speling"), (5, "teh")]);

        // Longer than the distance between two checkpoints.
        let mut doc = "---\n".to_string();
        for i in 0..3 * MARKDOWN_INTERVAL {
            doc.push_str(&format!("key{i}: wierd\n"));
        }
        doc.push_str("...\nBody with teh.\n");
        let mut tb = buffer(Some("markdown"), &doc);
        assert_every_window(&mut tb, &[(3 * MARKDOWN_INTERVAL + 2, "teh")]);
    }

    #[test]
    fn test_collect_markdown_list_edits() {
        let _g = shared_globals();
        // Front matter and lists across several checkpoints. The list column and front matter
        // state are part of each checkpoint, and edits must invalidate them like any other state.
        let mut doc = "---\n".to_string();
        for i in 0..MARKDOWN_INTERVAL + 4 {
            doc.push_str(&format!("key{i}: wierd\n"));
        }
        doc.push_str("---\n");
        for _ in 0..4 {
            doc.push_str("1. ```sh\n   wierd code\n   ```\n2. Run teh step.\n\n");
            doc.push_str("    Continuation teh.\n\n    - nested teh\n      ```\n      wierd\n");
            doc.push_str("      ```\n\n      More teh.\n\n");
        }
        let mut tb = buffer(Some("markdown"), &doc);
        let original = assert_like_fresh(&mut tb);
        assert_eq!(original.len(), 16);
        assert!(original.iter().all(|(_, w)| w == "teh"), "{original:?}");

        // Removing the front matter's opening line checks its lines.
        tb.cursor_move_to_offset(0);
        tb.delete(CursorMovement::Grapheme, 4);
        let without = assert_like_fresh(&mut tb);
        assert_eq!(
            without.iter().filter(|(_, w)| w == "wierd").count(),
            MARKDOWN_INTERVAL as usize + 4
        );
        tb.undo();
        assert_eq!(assert_like_fresh(&mut tb), original);

        // Unindenting the last list item's continuation (far below the first checkpoint)
        // ends the list, so the nested item's fence below it isn't one anymore.
        let lines: Vec<_> = doc.lines().collect();
        let y = lines.iter().rposition(|&l| l == "    Continuation teh.").unwrap() as CoordType;
        tb.cursor_move_to_logical(Point { x: 0, y });
        tb.delete(CursorMovement::Grapheme, 4);
        let unindented = assert_like_fresh(&mut tb);
        assert_eq!(unindented.last(), Some(&(y + 4, "wierd".to_string())));
        tb.undo();
        assert_eq!(assert_like_fresh(&mut tb), original);

        // Removing the first item's fence makes its code prose.
        // (The fence that its closing line opens ends at the next item.)
        let y = MARKDOWN_INTERVAL + 6;
        tb.cursor_move_to_logical(Point { x: 3, y });
        tb.delete(CursorMovement::Grapheme, 3);
        let removed = assert_like_fresh(&mut tb);
        assert_eq!(removed[0], (y + 1, "wierd".to_string()));
        assert_eq!(removed[1..], original);
        tb.undo();
        assert_eq!(assert_like_fresh(&mut tb), original);
        tb.redo();
        assert_eq!(assert_like_fresh(&mut tb), removed);
        tb.undo();
        assert_eq!(assert_like_fresh(&mut tb), original);
    }

    /// Asserts that each Markdown checkpoint holds the offset and the entire state that feeding
    /// the lines before it to a fresh [`MarkdownState`] gives.
    fn assert_checkpoints_match(tb: &TextBuffer) {
        let text = contents(tb);
        let mut lines = text.split_inclusive('\n');
        let mut md = MarkdownState::default();
        let mut offset = 0;
        for (i, &checkpoint) in tb.spell.markdown.states.iter().enumerate() {
            let skip = if i == 0 { 0 } else { MARKDOWN_INTERVAL as usize };
            for line in lines.by_ref().take(skip) {
                md.next_line(line.strip_suffix('\n').unwrap_or(line).as_bytes());
                offset += line.len();
            }
            assert_eq!(checkpoint, (offset, md), "checkpoint {i}");
        }
    }

    #[test]
    fn test_collect_markdown_fence_edits() {
        let _g = shared_globals();
        // Fences after a line that's indented less than the item's text, which only gives an upper
        // bound for the item's text column. In the middle, the paragraph with that guess and the
        // fence's content at column 0 are both longer than the distance between two checkpoints.
        let interval = MARKDOWN_INTERVAL as usize;
        let short = "1. Step\n\n  ```sh\ncat <<EOF\nEOF\n  ```\n\nShort teh.\n\n";
        let mut doc = short.to_string();
        doc.push_str("1. Step one\n\n  Explanation here.\n");
        doc.push_str(&"  More explanation.\n".repeat(interval));
        doc.push_str("  ```sh\ncat <<EOF\nhello wierd\n");
        doc.push_str(&"echo hello\n".repeat(interval));
        doc.push_str("EOF\n  ```\n\nProse teh.\n\n- Install:\n\n  ```sh\n  npm ci wierd\n");
        doc.push_str("  ```\n\nAfter teh.\n\n");
        doc.push_str(short);
        let lines: Vec<_> = doc.lines().collect();
        let line_of = |text: &str| lines.iter().position(|&l| l == text).unwrap() as CoordType;
        let mut tb = buffer(Some("markdown"), &doc);
        let original = assert_like_fresh(&mut tb);
        assert_checkpoints_match(&tb);
        let teh: Vec<_> = (0..lines.len())
            .filter(|&y| lines[y].ends_with(" teh."))
            .map(|y| (y as CoordType, "teh".to_string()))
            .collect();
        assert_eq!(original, teh);

        // `- ` instead of `1. ` puts the middle item's text at column 2, exactly. Its explanation
        // and fence are in the item then, and the fence's content at column 0 ends both.
        let y = line_of("1. Step one");
        tb.cursor_move_to_logical(Point { x: 0, y });
        tb.selection_update_logical(Point { x: 2, y });
        tb.write_raw(b"-");
        let exact = assert_like_fresh(&mut tb);
        assert_checkpoints_match(&tb);
        assert!(exact.contains(&(line_of("hello wierd"), "wierd".to_string())), "{exact:?}");
        assert!(!exact.contains(&(line_of("Prose teh."), "teh".to_string())), "{exact:?}");
        tb.undo();
        assert_eq!(assert_like_fresh(&mut tb), original);
        tb.redo();
        assert_eq!(assert_like_fresh(&mut tb), exact);
        tb.undo();
        assert_eq!(assert_like_fresh(&mut tb), original);

        // A list item pasted into the explanation, right below a checkpoint inside it
        // (which the edit keeps), does the same for the rest of the explanation.
        let y = (line_of("  Explanation here.") as usize).next_multiple_of(interval) + 1;
        tb.cursor_move_to_logical(Point { x: 0, y: y as CoordType });
        tb.write_raw(b"\n- Note:\n");
        let pasted = assert_like_fresh(&mut tb);
        assert_checkpoints_match(&tb);
        let y = line_of("hello wierd") + 2;
        assert!(pasted.contains(&(y, "wierd".to_string())), "{pasted:?}");
        tb.undo();
        assert_eq!(assert_like_fresh(&mut tb), original);
        tb.redo();
        assert_eq!(assert_like_fresh(&mut tb), pasted);
        tb.undo();
        assert_eq!(assert_like_fresh(&mut tb), original);
        assert_checkpoints_match(&tb);
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
 context with wierd text
";
        let mut tb = buffer(Some("git-commit"), doc);
        assert_eq!(collect(&mut tb), ["teh", "mispeled"]);
        // Starting inside the diff.
        assert_eq!(collect_lines(&mut tb, 10..12), [] as [&str; 0]);
        // Starting at any line. A window that starts on the context line needs to know it's in the diff.
        assert_every_window(&mut tb, &[(0, "teh"), (2, "mispeled")]);
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
    fn test_collect_xml_comments() {
        let _g = shared_globals();
        // A comment that ends on the line it starts on used to be masked like a tag.
        let doc = "\
<!-- a coment -->
<root><!-- teh note --><child attr=\"wrod\"/></root>
<!-- <div class=\"speling\">old</div> -->
<![CDATA[Some untill text]]>
<!DOCTYPE html>
<!--
multi line mesage
-->
<!-- a --><!-- freind -->
";
        let mut tb = buffer(Some("xml"), doc);
        assert_eq!(collect(&mut tb), ["coment", "teh", "mesage"]);

        // Markdown's single-line HTML comments stay unchecked, so that directives aren't flagged.
        let doc = "<!-- markdownlint-disable MD033 -->\n<!-- teh note -->\n";
        let mut tb = buffer(Some("markdown"), doc);
        assert_eq!(collect(&mut tb), [] as [&str; 0]);
    }

    /// Scope, highlighter and tokenizer together: `for_each_segment()` picks the `TextKind`,
    /// and the tokenizer reads `\`, `%` and `\'` by it. String literals in code are read as code
    /// (escapes and printf specs separate words), and everything else is read as prose
    /// (a run with `\` + letter is a path and is skipped).
    #[test]
    fn test_collect_text_kinds() {
        let _g = shared_globals();

        // Each of these lines gives a different result if the kinds are swapped.
        let doc = r#"let a = "\nteh line";
let b = "Error:\nPlese try again";
let c = "it isn\'t wierd";
let d = "%lu items, %zu bytes, %02X and \u{feff}\xde\xad wrod";
// see C:\Program Files\Microsoft Office\root and tools\bild\relese
// a coment with teh typo
"#;
        let mut tb = buffer(Some("rust"), doc);
        assert_eq!(collect(&mut tb), ["teh", "Plese", "wierd", "wrod", "coment", "teh"]);

        let mut tb = buffer(Some("c"), "printf(\"%lu itmes\\n\", n); // in tools\\bild\\relese\n");
        assert_eq!(collect(&mut tb), ["itmes"]);

        let doc = r"x = 'it\'s teh %s'  # see C:\Users\Admin\Documnets
";
        let mut tb = buffer(Some("python"), doc);
        assert_eq!(collect(&mut tb), ["teh"]);

        // Data formats check comments only, as prose.
        let doc = r#"{"path": "C:\\Program Files\\Microsft\\teh"} // see C:\Program Files\Microsft Ofice\root, a coment
"#;
        let mut tb = buffer(Some("json"), doc);
        assert_eq!(collect(&mut tb), ["coment"]);

        // Markdown is prose: paths are skipped, escaped punctuation isn't a path,
        // and accented spellings of dictionary words are correct.
        let doc = r"See tools\bild\relese and C:\Program Files\Microsft Office\root for teh files.
Escapes like \*wrod\* still work, and a naïve café résumé is fine.
In `code` and [link tetx](https://example.com/speling) too.
";
        let mut tb = buffer(Some("markdown"), doc);
        assert_eq!(collect(&mut tb), ["teh", "wrod", "tetx"]);

        // So are plain text and commit messages.
        let doc = "Output goes to tools\\bild\\relese, see teh log.\n";
        let mut tb = buffer(None, doc);
        assert_eq!(collect(&mut tb), ["teh"]);
        let mut tb = buffer(Some("git-commit"), doc);
        assert_eq!(collect(&mut tb), ["teh"]);

        // PowerShell and SQL strings are verbatim: `\` is part of a path in them, not an escape.
        // As code strings, these paths were flagged in pieces (ocuments, icrosoft, uarterly, ...).
        let doc = r#"$docs = "$env:USERPROFILE\Documents\Reports"
$p = 'C:\Program Files\Microsoft Office\root'
$key = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion'
$share = '\\fileserver\Finance\Quarterly Reports'
$h = @"
Logs go to C:\Temp\Log Files\archive with teh rest
"@
Write-Host "Found $count itmes in $path"
# a coment about C:\Program Files\Microsoft Office
<# a block coment #>
"#;
        let mut tb = buffer(Some("powershell"), doc);
        assert_eq!(collect(&mut tb), ["teh", "itmes", "coment", "coment"]);

        let doc = r"BULK INSERT dbo.Staging FROM 'C:\Data Files\Imports\vendor.csv';
SELECT * FROM t WHERE path = '\\fileserver\Finance\Quarterly Reports';
SELECT 'Domain\UserNmae teh' AS login;
-- a coment
/* a block coment */
";
        let mut tb = buffer(Some("sql"), doc);
        assert_eq!(collect(&mut tb), ["teh", "coment", "coment"]);
    }

    /// The rest of [`TextKind::VerbatimString`](tokenizer::TextKind::VerbatimString) (PowerShell)
    /// and [`TextKind::SqlString`](tokenizer::TextKind::SqlString): PowerShell escapes, doubled
    /// apostrophes, variables, T-SQL's `N'...'` and printf specs.
    #[test]
    fn test_collect_verbatim_strings() {
        let _g = shared_globals();

        // `` `n `` and `` `t `` are escapes. As prose, two of them mask the text between them
        // like Markdown inline code. `''` is an apostrophe, and `$usrname` a variable.
        let doc = r#"Write-Host "Line1`nThe quick fox jumpd`nLine3"
Write-Host "Done`nNext wrod`tTabbed"
$x = 'It isn''t wierd'
Write-Host "Hello $usrname, $cnt files"
"#;
        let mut tb = buffer(Some("powershell"), doc);
        assert_eq!(collect(&mut tb), ["jumpd", "wrod", "wierd"]);

        // `N'...'` is a Unicode string, `''` an apostrophe and `%ld` a printf spec (RAISERROR).
        let doc = r"SELECT N'Hello world', N'Welcome back';
SELECT 'It isn''t wierd';
RAISERROR('Found %ld itmes', 16, 1);
";
        let mut tb = buffer(Some("sql"), doc);
        assert_eq!(collect(&mut tb), ["wierd", "itmes"]);
    }

    /// One case per line (or a few lines), through scope, highlighter and tokenizer.
    #[test]
    fn test_collect_cross_part() {
        let _g = shared_globals();
        let cases: &[(&str, &str, &[&str])] = &[
            ("powershell", r#"$p = "C:\Program Files\Microsoft Office\root""#, &[]),
            ("powershell", r"$k = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Run'", &[]),
            ("powershell", r#"Write-Host "Line1`nThe quick fox jumpd""#, &["jumpd"]),
            ("powershell", r#"Write-Host "Done.`nplease restart""#, &[]),
            ("powershell", r#"Write-Host "Running on $env:COMPUTERNAME""#, &[]),
            (
                "powershell",
                r#"Write-Host "Installed in ${env:ProgramFiles(x86)}, teh end""#,
                &["teh"],
            ),
            // A backtick only escapes what PowerShell escapes. Before other letters, it's
            // punctuation, so `Get` isn't flagged as "et".
            ("powershell", r#"Write-Host "Run `Get-Help` for help""#, &[]),
            (
                "powershell",
                "$r = @'\n`Get-Report` creates teh report in `C:\\Reports`\n'@",
                &["teh"],
            ),
            (
                "powershell",
                "$r = @\"\n`Get-Report` creates teh report in `C:\\Reports`\n\"@",
                &["teh"],
            ),
            // Known limitation: in a single-quoted string, `n isn't an escape, but reads as one.
            ("powershell", "$m = 'Open `notepad` first'", &["otepad"]),
            ("sql", "SELECT N'Hello world', 'isn''t';", &[]),
            ("sql", "RAISERROR('%ld itmes', 16, 1);", &["itmes"]),
            ("rust", r#"let s = "\nhello teh";"#, &["teh"]),
            ("xml", "<!-- a coment -->", &["coment"]),
            // MySQL's quoted identifiers are names, in strings, too: in a SQL string, a backtick
            // is never an escape, so they're code spans.
            ("sql", "SELECT `Customers`, `Orders` FROM `Sales`;", &[]),
            ("sql", "SELECT `Customers`, `Orders` FROM t", &[]),
            ("sql", "SELECT `Custmers``Nme`, `my db`.`Ordrs`;", &[]),
            (
                "sql",
                "CREATE TABLE t (id INT COMMENT 'Foreign key to `customers`.`id`, see `invoices`');",
                &[],
            ),
            ("sql", "SET @s = 'ALTER TABLE `orders` ADD COLUMN `status` INT';", &[]),
            // A line inside a multi-line string is a string from its first column.
            (
                "sql",
                "SET @q = 'SELECT\n`id` AS row_id, `name` AS wrod, `email`\nFROM `users`';",
                &["wrod"],
            ),
            ("sql", "SELECT 'first wrod\n`name` is teh `col`\nend';", &["wrod", "teh"]),
            // A MySQL escape reads like a path in a SQL string, so the run is skipped.
            ("sql", "SELECT 'Helo\\nWrold';", &[]),
            // Commit subjects and exec lines are comments, read as prose.
            ("git-rebase", "pick 1a2b3c4 Fix teh widget", &["teh"]),
            ("git-rebase", "exec cargo tset --relese", &["tset", "relese"]),
        ];
        for &(language, line, expected) in cases {
            let mut tb = buffer(Some(language), &format!("{line}\n"));
            assert_eq!(collect(&mut tb), expected, "{language}: {line}");
        }
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

    #[test]
    fn test_finished_word_rule_undo_redo() {
        let _g = shared_globals();
        let screen = size(30, 1);
        let render = |tb: &mut TextBuffer| render(tb, Point::default(), screen, true);
        let wrod = [run(0, 6, Curly, "wrod")];

        // Undoing a replacement leaves the cursor at the end of the misspelled word.
        let mut tb = buffer(None, "hello wrod world");
        tb.cursor_move_to_offset(0);
        assert_eq!(render(&mut tb), wrod);
        assert!(replace_at(&mut tb, 8, "word"));
        assert_eq!(render(&mut tb), []);
        tb.undo();
        assert_eq!(contents(&tb), "hello wrod world");
        assert_eq!(tb.cursor_offset(), 10);
        assert_eq!(render(&mut tb), wrod);
        assert_eq!(render(&mut tb), wrod);
        tb.redo();
        assert_eq!(render(&mut tb), []);
        tb.undo();
        assert_eq!(render(&mut tb), wrod);

        // Undoing and redoing typing.
        let mut tb = buffer(None, "hello wrod");
        tb.undo_barrier();
        assert_eq!(render(&mut tb), []);
        tb.write_canon(b" now");
        assert_eq!(render(&mut tb), wrod);
        tb.undo();
        assert_eq!(contents(&tb), "hello wrod");
        assert_eq!(tb.cursor_offset(), 10);
        assert_eq!(render(&mut tb), wrod);
        tb.redo();
        assert_eq!(render(&mut tb), wrod);

        // Typing after an undo is typing again.
        tb.undo();
        assert_eq!(render(&mut tb), wrod);
        tb.write_canon(b"d");
        assert_eq!(contents(&tb), "hello wrodd");
        assert_eq!(render(&mut tb), []);
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
