// Licensed under the MIT License.
// Fork-only: spellcheck (not part of upstream microsoft/edit).

//! Splits text into words worth spell checking.
//!
//! CONTRACT (other modules rely on this):
//! * `text` is one segment of a single line, without the trailing newline. It may contain invalid UTF-8.
//! * `f` is called with byte ranges relative to `text`, in ascending order, non-overlapping.
//! * Every range is valid UTF-8 and at most [`super::MAX_WORD_LEN`] bytes long,
//!   so callers may use `str::from_utf8_unchecked(&text[range])`.
//!
//! SKELETON: this is a minimal placeholder (runs of ASCII letters), to be replaced by the full rules.

use std::ops::Range;

/// Calls `f` with the byte range of every candidate word in `text`.
pub fn for_each_word(text: &[u8], mut f: impl FnMut(Range<usize>)) {
    let mut beg = None;
    for (i, &b) in text.iter().chain(std::iter::once(&b' ')).enumerate() {
        match (b.is_ascii_alphabetic(), beg) {
            (true, None) => beg = Some(i),
            (false, Some(start)) => {
                if i - start >= 2 && i - start <= super::MAX_WORD_LEN {
                    f(start..i);
                }
                beg = None;
            }
            _ => {}
        }
    }
}
