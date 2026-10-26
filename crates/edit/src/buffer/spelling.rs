// Licensed under the MIT License.
// Fork-only: spellcheck (not part of upstream microsoft/edit).

//! Spell checking for [`TextBuffer`]: underlining misspelled words while rendering,
//! finding the misspelled word at the cursor, and replacing it with a suggestion.
//!
//! This is a child module of `buffer` so that it can use the buffer's private helpers
//! without widening their visibility in upstream code.
//!
//! SKELETON: the public API below is final; the bodies are placeholders.

use std::ops::Range;

use super::TextBuffer;
use crate::framebuffer::Framebuffer;
use crate::helpers::*;

/// Per-buffer spell checking state. Lives in `TextBuffer::spell`.
#[derive(Default)]
pub(super) struct SpellState {
    /// Only the main document buffers have spell checking enabled; editlines don't.
    enabled: bool,
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
        SpellAt::Disabled
    }

    /// Replaces `word` with `replacement` as a single undo step.
    /// Returns false (and does nothing) if the buffer changed since `word` was looked up.
    pub fn spell_replace(&mut self, word: &SpellWord, replacement: &str) -> bool {
        _ = (word, replacement);
        false
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
        _ = (origin, destination, logical_y_range, focused, fb);
    }
}
