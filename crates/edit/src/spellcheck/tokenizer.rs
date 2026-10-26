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
//! The rules, in order:
//! 1. Masks: code spans (a run of backticks through the next run of equally many),
//!    HTML/XML tags (`<` plus an ASCII letter, `/` or `!`, through the next `>`) and escapes
//!    (`\` plus an ASCII letter) are never checked. Unmatched backticks and `<` are punctuation.
//!    Code spans and tags separate runs like whitespace does. Escapes stay part of their run,
//!    so that `C:\Users` is still recognized as a path.
//! 2. Runs: whole runs between whitespace that look like URLs, email addresses, paths or
//!    file names are skipped (see `is_skipped_run`).
//! 3. Words: letters, combining marks, ASCII digits and `_`, plus apostrophes between two letters.
//!    Leading and trailing `_` are stripped (Markdown's `_italic_`).
//! 4. Words with digits, an internal `_`, camelCase, ALL CAPS, non-Latin letters, fewer than
//!    2 characters or more than [`super::MAX_WORD_LEN`] bytes are skipped as a whole.
//!
//! Everything runs in linear time without allocating.

use std::ops::Range;

use super::MAX_WORD_LEN;
use crate::simd::memchr2;
use crate::unicode::Utf8Chars;

/// What kind of text a segment is. It decides how backslashes are read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextKind {
    /// Prose, comments, Markdown and commit messages: a `\` inside a run is part of a path,
    /// a registry key or a LaTeX command.
    Prose,
    /// A string literal in code: `\n`, `\x41`, `\u{...}`, `\'` are escape sequences.
    CodeString,
}

/// Calls `f` with the byte range of every candidate word in `text`, reading it as `kind`.
// SKELETON (round 1 fixes): `kind` is ignored until the tokenizer part implements it.
pub fn for_each_word_in(text: &[u8], kind: TextKind, f: impl FnMut(Range<usize>)) {
    _ = kind;
    for_each_word(text, f)
}

/// Calls `f` with the byte range of every candidate word in `text`.
pub fn for_each_word(text: &[u8], mut f: impl FnMut(Range<usize>)) {
    let len = text.len();
    // Start of the current run.
    let mut run_beg = 0;
    // Position of the next `>`, or `len` if there's none.
    // Caching it keeps many `<` without a `>` linear.
    let mut next_gt = 0;
    // Bit `n - 1` is set once there's no run of `n` backticks left. Same reason.
    let mut unclosed_backticks = 0u64;
    let mut i = 0;

    while i < len {
        let beg = i;
        let b = text[i];
        i += 1;

        let separates = match b {
            b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c' => true,
            b'\\' => {
                // The escaped character can't open a mask (Markdown's \` and \<).
                // The escape itself is masked by `for_each_word_in_run`.
                if i < len && text[i].is_ascii_graphic() {
                    i += 1;
                }
                false
            }
            b'`' => {
                while i < len && text[i] == b'`' {
                    i += 1;
                }
                match find_closing_backticks(text, i, i - beg, &mut unclosed_backticks) {
                    Some(end) => {
                        i = end;
                        true
                    }
                    None => false,
                }
            }
            b'<' if i < len
                && (text[i].is_ascii_alphabetic() || matches!(text[i], b'/' | b'!')) =>
            {
                if next_gt < i {
                    next_gt = memchr2(b'>', b'>', text, i);
                }
                if next_gt < len {
                    i = next_gt + 1;
                    true
                } else {
                    false
                }
            }
            0x80.. => {
                let mut chars = Utf8Chars::new(text, beg);
                let c = chars.next().unwrap_or_default();
                i = chars.offset();
                is_unicode_space(c)
            }
            _ => false,
        };

        if separates {
            if run_beg < beg {
                for_each_word_in_run(text, run_beg..beg, &mut f);
            }
            run_beg = i;
        }
    }

    if run_beg < len {
        for_each_word_in_run(text, run_beg..len, &mut f);
    }
}

/// Whitespace besides ASCII: NBSP, the U+2000 block spaces (incl. ZWSP), NNBSP and the ideographic space.
fn is_unicode_space(c: char) -> bool {
    matches!(c, '\u{00A0}' | '\u{2000}'..='\u{200B}' | '\u{202F}' | '\u{3000}')
}

fn is_combining_mark(c: char) -> bool {
    matches!(c, '\u{0300}'..='\u{036F}')
}

/// Basic Latin through Latin Extended-B, and Latin Extended Additional.
fn is_latin(c: char) -> bool {
    matches!(c, '\0'..='\u{024F}' | '\u{1E00}'..='\u{1EFF}')
}

/// Finds the run of exactly `n` backticks that closes a code span, searching from `beg`.
/// Returns the end of that run. `unclosed` remembers failed searches, see [`for_each_word`].
fn find_closing_backticks(
    text: &[u8],
    mut beg: usize,
    n: usize,
    unclosed: &mut u64,
) -> Option<usize> {
    // Runs this long don't occur in practice. Calling them unmatched keeps the cache a single `u64`.
    if n > 64 {
        return None;
    }

    let bit = 1u64 << (n - 1);
    if *unclosed & bit != 0 {
        return None;
    }

    loop {
        let start = memchr2(b'`', b'`', text, beg);
        if start >= text.len() {
            break;
        }

        let mut end = start + 1;
        while end < text.len() && text[end] == b'`' {
            end += 1;
        }
        if end - start == n {
            return Some(end);
        }
        beg = end;
    }

    *unclosed |= bit;
    None
}

/// Whether the run looks like a URL, an email address, a path or a file name.
fn is_skipped_run(run: &[u8]) -> bool {
    // Opening punctuation doesn't hide a path, as in (/usr/bin) or "~/notes".
    let mut trimmed = run;
    loop {
        match trimmed {
            [b'(' | b'[' | b'{' | b'"' | b'\'' | b'*' | b'_', rest @ ..] => trimmed = rest,
            // U+2018 and U+201C, the opening typographic quotes.
            [0xE2, 0x80, 0x98 | 0x9C, rest @ ..] => trimmed = rest,
            _ => break,
        }
    }
    if [&b"www."[..], b"/", b"./", b"../", b"~/"].iter().any(|p| trimmed.starts_with(p)) {
        return true;
    }

    let mut prev2 = ' ';
    let mut prev = ' ';
    for c in Utf8Chars::new(run, 0) {
        let skip = match c {
            '@' => true,
            '/' => prev2 == ':' && prev == '/',
            '\\' => prev == ':',
            // main.rs, e.g., example.com, 3.14
            _ => prev == '.' && prev2.is_alphanumeric() && c.is_alphanumeric(),
        };
        if skip {
            return true;
        }
        prev2 = prev;
        prev = c;
    }

    false
}

/// Splits a run (text between whitespace) into words and passes the checkable ones to `f`.
fn for_each_word_in_run(text: &[u8], run: Range<usize>, f: &mut impl FnMut(Range<usize>)) {
    if is_skipped_run(&text[run.clone()]) {
        return;
    }

    let text = &text[..run.end];
    let mut chars = Utf8Chars::new(text, run.start);
    let mut word_beg = None;
    // Whether the previous character, ignoring combining marks, is a letter.
    let mut after_letter = false;

    loop {
        let pos = chars.offset();
        let Some(c) = chars.next() else {
            break;
        };

        let joins = if is_combining_mark(c) {
            true
        } else {
            let joins = match c {
                '\'' | '\u{2019}' => {
                    after_letter
                        && Utf8Chars::new(text, chars.offset())
                            .next()
                            .is_some_and(char::is_alphabetic)
                }
                '\\' => {
                    // Mask escapes like \n. In \\n the backslash is escaped, not the n.
                    if text.get(pos + 1).is_some_and(|&b| b.is_ascii_alphabetic() || b == b'\\') {
                        chars.seek(pos + 2);
                    }
                    false
                }
                _ => c.is_alphabetic() || c.is_ascii_digit() || c == '_',
            };
            after_letter = c.is_alphabetic();
            joins
        };

        match (joins, word_beg) {
            (true, None) => word_beg = Some(pos),
            (false, Some(beg)) => {
                emit_word(text, beg..pos, f);
                word_beg = None;
            }
            _ => {}
        }
    }

    if let Some(beg) = word_beg {
        emit_word(text, beg..run.end, f);
    }
}

/// Strips leading and trailing `_` from the word and passes it to `f` if it's worth checking.
/// Apostrophes are never leading or trailing, because they only join between two letters.
fn emit_word(text: &[u8], range: Range<usize>, f: &mut impl FnMut(Range<usize>)) {
    let Range { mut start, mut end } = range;
    while start < end && text[start] == b'_' {
        start += 1;
    }
    while start < end && text[end - 1] == b'_' {
        end -= 1;
    }
    if is_checkable_word(&text[start..end]) {
        f(start..end);
    }
}

/// Applies the word skipping rules (rule 4 in the module docs).
/// `word` consists of letters, combining marks, digits, `_` and apostrophes.
fn is_checkable_word(word: &[u8]) -> bool {
    if word.len() > MAX_WORD_LEN {
        return false;
    }

    let mut chars = 0;
    let mut uppers = 0;
    let mut has_lower = false;
    let mut after_lower = false;

    for c in Utf8Chars::new(word, 0) {
        // Combining marks belong to the previous letter.
        if is_combining_mark(c) {
            continue;
        }

        chars += 1;

        if c.is_ascii_digit() || c == '_' {
            return false;
        }

        if c.is_alphabetic() {
            if !is_latin(c) {
                return false;
            }
            if c.is_uppercase() {
                // camelCase, iPhone
                if after_lower {
                    return false;
                }
                uppers += 1;
            }
            after_lower = c.is_lowercase();
            has_lower |= after_lower;
        } else {
            // An apostrophe: d'Artagnan is fine.
            after_lower = false;
        }
    }

    // Acronyms: NASA, OK.
    chars >= 2 && (has_lower || uppers < 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &[u8]) -> Vec<&str> {
        let mut words = Vec::new();
        for_each_word(text, |r| words.push(str::from_utf8(&text[r]).unwrap()));
        words
    }

    fn check(cases: &[(&str, &[&str])]) {
        for &(text, expected) in cases {
            assert_eq!(words(text.as_bytes()), expected, "{text:?}");
        }
    }

    /// Asserts the CONTRACT and returns the number of words.
    fn check_contract(text: &[u8]) -> usize {
        let mut count = 0;
        let mut prev_end = 0;
        for_each_word(text, |r| {
            assert!(r.start >= prev_end, "{text:x?}: {r:?} overlaps or isn't ascending");
            assert!(r.start < r.end && r.end <= text.len(), "{text:x?}: {r:?}");
            assert!(r.len() <= MAX_WORD_LEN, "{text:x?}: {r:?}");
            let word = str::from_utf8(&text[r.clone()]).expect("valid UTF-8");
            assert!(word.chars().count() >= 2, "{text:x?}: {word:?}");
            assert!(
                word.chars().all(|c| c.is_alphabetic()
                    || is_combining_mark(c)
                    || c == '\''
                    || c == '\u{2019}'),
                "{text:x?}: {word:?}"
            );
            prev_end = r.end;
            count += 1;
        });
        count
    }

    #[test]
    fn test_basics() {
        check(&[
            ("", &[]),
            (" \t ", &[]),
            ("hello", &["hello"]),
            ("hello world", &["hello", "world"]),
            ("  leading and trailing  ", &["leading", "and", "trailing"]),
            ("Hello, world! How are you?", &["Hello", "world", "How", "are", "you"]),
            ("(parens) [brackets] {braces}", &["parens", "brackets", "braces"]),
            ("\"double\" quotes", &["double", "quotes"]),
        ]);
    }

    #[test]
    fn test_word_at_start_and_end() {
        let text = b"first middle last";
        let mut ranges = Vec::new();
        for_each_word(text, |r| ranges.push(r));
        assert_eq!(ranges, [0..5, 6..12, 13..17]);
    }

    #[test]
    fn test_mask_backticks() {
        check(&[
            ("use `foo bar` here", &["use", "here"]),
            ("one `two` three `four` five", &["one", "three", "five"]),
            ("`code`suffix", &["suffix"]),
            ("prefix`code`", &["prefix"]),
            ("`https://example.com`next", &["next"]),
            ("unmatched `backtick here", &["unmatched", "backtick", "here"]),
            ("``double code`` outside", &["outside"]),
            ("``has ` inside`` outside", &["outside"]),
            ("`one ``two`` three` four", &["four"]),
            ("open ``` fence", &["open", "fence"]),
            ("```rust", &["rust"]),
            ("``` ```", &[]),
            ("`a` `b` stray ` tick", &["stray", "tick"]),
            // An escaped backtick doesn't open a code span.
            ("\\`not code` here", &["not", "code", "here"]),
        ]);
    }

    #[test]
    fn test_mask_tags() {
        check(&[
            ("<a href=\"x\">link</a>", &["link"]),
            ("<b>bold</b> text", &["bold", "text"]),
            ("<a href=\"https://example.com\">here</a> now", &["here", "now"]),
            ("<!-- comment --> after", &["after"]),
            ("<br/>line<br />", &["line"]),
            ("Vec<String> holds", &["Vec", "holds"]),
            // Not tags.
            ("one < two and three > four", &["one", "two", "and", "three", "four"]),
            ("less <than without closing", &["less", "than", "without", "closing"]),
            ("arrow <- left", &["arrow", "left"]),
            ("end<", &["end"]),
            ("\\<b> escaped", &["escaped"]),
        ]);
    }

    #[test]
    fn test_mask_escapes() {
        check(&[
            ("\\nhello", &["hello"]),
            ("line one\\nline two", &["line", "one", "line", "two"]),
            ("tab\\there", &["tab", "here"]),
            ("\\\\note", &["note"]),
            ("\\\\\\nfoo", &["foo"]),
            ("word\\", &["word"]),
            ("\\(paren\\)", &["paren"]),
            ("\\u00e9 accent", &["accent"]),
        ]);
    }

    #[test]
    fn test_skip_runs() {
        check(&[
            ("see https://example.com/path now", &["see", "now"]),
            ("ftp://host and", &["and"]),
            ("mail me@example.com please", &["mail", "please"]),
            ("@mention here", &["here"]),
            ("visit www.example now", &["visit", "now"]),
            ("open /usr/local/bin now", &["open", "now"]),
            ("run ./configure first", &["run", "first"]),
            ("go ../parent dir", &["go", "dir"]),
            ("edit ~/notes today", &["edit", "today"]),
            ("path C:\\Users\\someone here", &["path", "here"]),
            ("drive D:\\ here", &["drive", "here"]),
            ("file main.rs here", &["file", "here"]),
            ("e.g. this", &["this"]),
            ("i.e., that", &["that"]),
            ("pi is 3.14 ok", &["pi", "is", "ok"]),
            ("visit example.com today", &["visit", "today"]),
            ("café.txt opened", &["opened"]),
            ("(/usr/bin) paren", &["paren"]),
            ("\"~/notes\" quoted", &["quoted"]),
            ("\u{201C}/usr/bin\u{201D} typographic", &["typographic"]),
            // Not skipped.
            ("end. Next", &["end", "Next"]),
            ("trailing dot.", &["trailing", "dot"]),
            ("ellipsis... yes", &["ellipsis", "yes"]),
            (".hidden dotfile", &["hidden", "dotfile"]),
            ("either/or", &["either", "or"]),
            ("time: now", &["time", "now"]),
            ("ratio 1:2", &["ratio"]),
        ]);
    }

    #[test]
    fn test_whitespace() {
        check(&[
            ("one\ttwo", &["one", "two"]),
            ("one\u{00A0}two", &["one", "two"]),
            (
                "one\u{2000}two\u{2003}three\u{200A}four\u{200B}five\u{202F}six\u{3000}seven",
                &["one", "two", "three", "four", "five", "six", "seven"],
            ),
            // NBSP splits runs, so the URL doesn't take the neighbors with it.
            ("see\u{00A0}http://example.com\u{00A0}now", &["see", "now"]),
            ("main.rs\u{3000}after", &["after"]),
            // CR from CRLF line endings.
            ("crlf\r", &["crlf"]),
            // Not whitespace: ZWJ separates words, but not runs.
            ("one\u{200D}two", &["one", "two"]),
            ("example.com\u{200D}word", &[]),
        ]);
    }

    #[test]
    fn test_words() {
        check(&[
            ("don't it's don\u{2019}t", &["don't", "it's", "don\u{2019}t"]),
            ("James' dogs' cat's", &["James", "dogs", "cat's"]),
            ("'tis the season", &["tis", "the", "season"]),
            ("\u{2018}quoted\u{2019} words", &["quoted", "words"]),
            ("'single' quotes", &["single", "quotes"]),
            ("rock 'n' roll", &["rock", "roll"]),
            ("don''t", &["don"]),
            ("rock'n'roll", &["rock'n'roll"]),
            ("well-known", &["well", "known"]),
            ("en\u{2013}dash em\u{2014}dash", &["en", "dash", "em", "dash"]),
            ("either/or and/or", &["either", "or", "and", "or"]),
            ("semi;colon comma,separated", &["semi", "colon", "comma", "separated"]),
            ("foo_bar", &[]),
            ("_italic_", &["italic"]),
            ("__dunder__", &["dunder"]),
            ("_don't_", &["don't"]),
            ("**bold**", &["bold"]),
            ("*emph* ~~strike~~", &["emph", "strike"]),
            ("# Heading", &["Heading"]),
            ("- [ ] task item", &["task", "item"]),
        ]);
    }

    #[test]
    fn test_skip_words() {
        check(&[
            ("abc123 a1b 2nd 42", &[]),
            // Skipped words don't merge with their neighbors.
            ("foo1bar", &[]),
            ("foo_bar baz", &["baz"]),
            ("snake_case_name", &[]),
            ("camelCase iPhone McDonald XMLHttpRequest", &[]),
            ("NASA OK DON'T", &[]),
            ("a I x", &[]),
            ("Ab ab AB", &["Ab", "ab"]),
            ("Hello", &["Hello"]),
            ("d'Artagnan O'Neil", &["d'Artagnan", "O'Neil"]),
            ("fooBar-baz", &["baz"]),
            ("Ελληνικά greek", &["greek"]),
            ("日本語 cjk", &["cjk"]),
            ("café\u{03BB} mixed", &["mixed"]),
            ("\u{0283}ip ipa", &["ipa"]),
            ("Привет cyrillic", &["cyrillic"]),
        ]);
    }

    #[test]
    fn test_latin() {
        check(&[
            ("naïve café", &["naïve", "café"]),
            // Decomposed: combining marks join and don't count as characters.
            ("nai\u{0308}ve cafe\u{0301}", &["nai\u{0308}ve", "cafe\u{0301}"]),
            ("e\u{0301} single", &["single"]),
            ("cafe\u{0301}'s", &["cafe\u{0301}'s"]),
            ("Straße Æsir œuvre", &["Straße", "Æsir", "œuvre"]),
            ("\u{1E81}ord", &["\u{1E81}ord"]),
            ("ÉCOLE", &[]),
            ("Éa", &["Éa"]),
            ("aÉ", &[]),
        ]);
    }

    #[test]
    fn test_long_words() {
        let max = "a".repeat(MAX_WORD_LEN);
        let over = "a".repeat(MAX_WORD_LEN + 1);
        let text = format!("short {max} {over} end");
        assert_eq!(words(text.as_bytes()), ["short", &max, "end"]);

        // MAX_WORD_LEN is in bytes, not characters.
        let accented = "é".repeat(MAX_WORD_LEN / 2);
        let accented_over = format!("{accented}a");
        let text = format!("{accented} {accented_over}");
        assert_eq!(words(text.as_bytes()), [accented.as_str()]);

        // Stripped underscores don't count.
        let text = format!("_{max}_");
        assert_eq!(words(text.as_bytes()), [max.as_str()]);
    }

    #[test]
    fn test_long_lines() {
        // 10k bytes of words.
        let text = "word ".repeat(2000);
        assert_eq!(text.len(), 10_000);
        assert_eq!(check_contract(text.as_bytes()), 2000);

        // A single 10k byte word.
        let text = "a".repeat(10_000);
        assert_eq!(check_contract(text.as_bytes()), 0);

        // Mixed content, repeated.
        let text = "Teh `code` <b>tag</b> http://x.y naïve foo_bar don't ".repeat(200);
        assert_eq!(check_contract(text.as_bytes()), 200 * 4);

        // Pathological inputs for the mask searches.
        let text = "<a ".repeat(4000);
        assert_eq!(check_contract(text.as_bytes()), 0);
        let text = "<b word ".repeat(2000);
        assert_eq!(check_contract(text.as_bytes()), 2000);
        let mut text = String::new();
        for n in 1..=100 {
            text.push_str(&"`".repeat(n));
            text.push_str(" ab ");
        }
        // Runs of 1..=64 backticks are each unmatched and >64 are always unmatched.
        assert_eq!(check_contract(text.as_bytes()), 100);
    }

    #[test]
    fn test_invalid_utf8() {
        #[allow(clippy::type_complexity)]
        let cases: &[(&[u8], &[&str])] = &[
            (b"\xFF", &[]),
            (b"\xFF\xFE", &[]),
            (b"ab\xFFcd", &["ab", "cd"]),
            (b"caf\xC3", &["caf"]),
            (b"caf\xC3 next", &["caf", "next"]),
            (b"\xE2\x80 word", &["word"]),
            (b"hel\xC0\x80lo", &["hel", "lo"]),
            (b"don\xE2\x80t", &["don"]),
            (b"\xED\xA0\x80surrogate", &["surrogate"]),
            (b"\x80\x80\x80word\x80", &["word"]),
            (b"`\xFF` after", &["after"]),
            (b"<\xFF> after", &["after"]),
            (b"\\\xFFword", &["word"]),
            (b"www.\xFF skipped", &["skipped"]),
        ];
        for &(text, expected) in cases {
            assert_eq!(words(text), expected, "{text:x?}");
            check_contract(text);
        }
    }

    #[test]
    fn test_contract_on_generated_input() {
        const FRAGMENTS: &[&[u8]] = &[
            b"a",
            b"b",
            b"Z",
            b"word",
            b"teh",
            "é".as_bytes(),
            "e\u{0301}".as_bytes(),
            "\u{2019}".as_bytes(),
            "\u{00A0}".as_bytes(),
            "\u{3000}".as_bytes(),
            "λ".as_bytes(),
            "日".as_bytes(),
            "\u{FFFD}".as_bytes(),
            b"'",
            b"_",
            b"`",
            b"``",
            b"<",
            b"<a",
            b"</",
            b"<!",
            b">",
            b"/",
            b"\\",
            b":",
            b".",
            b"@",
            b"-",
            b"1",
            b" ",
            b"\t",
            b"://",
            b"www.",
            b"\x80",
            b"\xC3",
            b"\xE2\x80",
            b"\xF0\x9F",
            b"\xFF",
        ];

        // Knuth's MMIX LCG.
        let mut state = 1442695040888963407u64;
        let mut rng = move || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) as usize
        };

        let mut text = Vec::new();
        let mut actual = Vec::new();
        let mut expected = Vec::new();
        let mut total = 0;

        for _ in 0..5000 {
            text.clear();
            let fragments = rng() % 64;
            for _ in 0..fragments {
                if rng() % 8 == 0 {
                    // Any byte at all.
                    text.push(rng() as u8);
                } else {
                    text.extend_from_slice(FRAGMENTS[rng() % FRAGMENTS.len()]);
                }
            }

            total += check_contract(&text);

            // The cached mask searches must not change the result.
            actual.clear();
            for_each_word(&text, |r| actual.push(r));
            expected.clear();
            for run in reference_runs(&text) {
                for_each_word_in_run(&text, run, &mut |r| expected.push(r));
            }
            assert_eq!(actual, expected, "{text:x?}");
        }

        // Make sure the generator isn't so noisy that nothing gets tested.
        assert!(total > 1000, "{total}");
    }

    /// A quadratic version of the run splitting in `for_each_word`, without its caches.
    fn reference_runs(text: &[u8]) -> Vec<Range<usize>> {
        let backticks_at = |i: usize| text[i..].iter().take_while(|&&b| b == b'`').count();
        let mut runs = Vec::new();
        let mut run_beg = 0;
        let mut i = 0;

        while i < text.len() {
            let beg = i;
            let mut separator_end = None;

            match text[i] {
                b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c' => separator_end = Some(i + 1),
                b'\\' => i += if text.get(i + 1).is_some_and(u8::is_ascii_graphic) { 2 } else { 1 },
                b'`' => {
                    let n = backticks_at(i);
                    i += n;
                    let mut j = i;
                    while j < text.len() {
                        if text[j] != b'`' {
                            j += 1;
                            continue;
                        }
                        let m = backticks_at(j);
                        if m == n && n <= 64 {
                            separator_end = Some(j + m);
                            break;
                        }
                        j += m;
                    }
                }
                b'<' if text
                    .get(i + 1)
                    .is_some_and(|&b| b.is_ascii_alphabetic() || b == b'/' || b == b'!') =>
                {
                    match text[i..].iter().position(|&b| b == b'>') {
                        Some(p) => separator_end = Some(i + p + 1),
                        None => i += 1,
                    }
                }
                0x80.. => {
                    let mut chars = Utf8Chars::new(text, i);
                    let c = chars.next().unwrap();
                    i = chars.offset();
                    if is_unicode_space(c) {
                        separator_end = Some(i);
                    }
                }
                _ => i += 1,
            }

            if let Some(end) = separator_end {
                if run_beg < beg {
                    runs.push(run_beg..beg);
                }
                i = end;
                run_beg = end;
            }
        }

        if run_beg < text.len() {
            runs.push(run_beg..text.len());
        }
        runs
    }
}
