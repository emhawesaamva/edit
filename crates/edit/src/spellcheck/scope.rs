// Licensed under the MIT License.
// Fork-only: spellcheck (not part of upstream microsoft/edit).

//! Decides which parts of a line get spell checked.
//!
//! CONTRACT (other modules rely on this):
//! * [`Scope::for_language`] maps the buffer's language to a scope.
//! * If [`Scope::needs_highlighter`] is false, the caller passes an empty `highlights` slice.
//! * [`Scope::skip_line`] is called first; `starts_idle` is `Highlighter::is_idle()` sampled
//!   right before parsing this line (always `true` if no highlighter is needed).
//! * [`Scope::for_each_segment`] gets the line without its trailing newline, the line's absolute
//!   buffer offset, and the line's highlight spans (absolute offsets, sentinel at the end, possibly
//!   empty). It calls `f` with ranges relative to `line`, ascending and non-overlapping.
//!
//! SKELETON: this is a minimal placeholder, to be replaced by the full rules.

use std::ops::Range;

use lsh::runtime::Highlight;

use crate::lsh::{HighlightKind, Language};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    /// Nothing is checked.
    Off,
    /// Every word is checked (plain text).
    Prose,
    /// Prose, minus code, links, etc.
    Markdown,
    /// The commit message, minus `#` comments and diffs.
    GitCommit,
    /// Comments and strings.
    Code,
    /// Comments only (data formats, whose strings are mostly identifiers).
    CommentsOnly,
}

impl Scope {
    pub fn for_language(language: Option<&'static Language>) -> Self {
        match language {
            None => Self::Prose,
            Some(_) => Self::Code,
        }
    }

    /// Whether highlight spans are needed to decide what to check.
    pub fn needs_highlighter(self) -> bool {
        !matches!(self, Self::Off | Self::Prose)
    }

    /// Whether text with the given highlight kind gets checked.
    pub fn is_checkable(self, kind: HighlightKind) -> bool {
        match self {
            Self::Off => false,
            Self::Prose => true,
            _ => matches!(kind, HighlightKind::Comment | HighlightKind::String),
        }
    }

    /// Whether the entire line should be skipped.
    pub fn skip_line(self, _line: &[u8], _starts_idle: bool) -> bool {
        self == Self::Off
    }

    /// Calls `f` with every maximal range of checkable text in `line`.
    pub fn for_each_segment(
        self,
        line: &[u8],
        line_offset: usize,
        highlights: &[Highlight<HighlightKind>],
        mut f: impl FnMut(Range<usize>),
    ) {
        if !self.needs_highlighter() {
            if self == Self::Prose && !line.is_empty() {
                f(0..line.len());
            }
            return;
        }

        for pair in highlights.windows(2) {
            if self.is_checkable(pair[0].kind) {
                let beg = pair[0].start.saturating_sub(line_offset).min(line.len());
                let end = pair[1].start.saturating_sub(line_offset).min(line.len());
                if beg < end {
                    f(beg..end);
                }
            }
        }
    }
}
