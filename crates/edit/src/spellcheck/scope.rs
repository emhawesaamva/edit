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

use std::mem;
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
        let Some(language) = language else {
            // Files without a syntax definition (.txt, but also Makefile, .log, etc.).
            return Self::Prose;
        };

        // The ids are the .lsh entrypoint names, with `_` replaced by `-`.
        match language.id {
            "markdown" => Self::Markdown,
            "git-commit" => Self::GitCommit,
            // Patches are mostly code and other people's prose. Underlining them is just noise.
            "diff" => Self::Off,
            // Data formats: Their strings are mostly keys, paths and identifiers.
            "git-rebase" | "ignore" | "json" | "properties" | "toml" | "xml" | "yaml" => {
                Self::CommentsOnly
            }
            _ => Self::Code,
        }
    }

    /// Whether highlight spans are needed to decide what to check.
    pub fn needs_highlighter(self) -> bool {
        !matches!(self, Self::Off | Self::Prose)
    }

    /// Whether text with the given highlight kind gets checked.
    pub fn is_checkable(self, kind: HighlightKind) -> bool {
        // NOTE: Always use `matches!` here and never an exhaustive match over `HighlightKind`.
        // The kinds are generated from the .lsh files, and new ones in upstream must not break the build.
        match self {
            Self::Off => false,
            Self::Prose => true,
            // `String` is link text and inline code (which the tokenizer masks).
            // `MarkupLink` is the link URL and never checked.
            Self::Markdown => matches!(
                kind,
                HighlightKind::Other
                    | HighlightKind::Comment
                    | HighlightKind::String
                    | HighlightKind::MarkupHeading
                    | HighlightKind::MarkupBold
                    | HighlightKind::MarkupItalic
                    | HighlightKind::MarkupStrikethrough
                    | HighlightKind::MarkupList
            ),
            // `#` lines are comments, and `diff --git` is a header.
            Self::GitCommit => matches!(kind, HighlightKind::Other),
            Self::Code => matches!(kind, HighlightKind::Comment | HighlightKind::String),
            Self::CommentsOnly => matches!(kind, HighlightKind::Comment),
        }
    }

    /// Whether the entire line should be skipped.
    pub fn skip_line(self, line: &[u8], starts_idle: bool) -> bool {
        match self {
            Self::Off => true,
            // Inside a fenced code block (of any language), the highlighter isn't idle.
            // That covers the closing fence as well, but not the opening one.
            Self::Markdown => !starts_idle || is_markdown_code_line(line),
            // A `diff --git` line (`git commit -v`) keeps the highlighter busy until the end of the file.
            Self::GitCommit => !starts_idle,
            Self::Prose | Self::Code | Self::CommentsOnly => false,
        }
    }

    /// Calls `f` with every maximal range of checkable text in `line`.
    pub fn for_each_segment(
        self,
        line: &[u8],
        line_offset: usize,
        highlights: &[Highlight<HighlightKind>],
        mut f: impl FnMut(Range<usize>),
    ) {
        match self {
            Self::Off => return,
            Self::Prose => {
                if !line.is_empty() {
                    f(0..line.len());
                }
                return;
            }
            _ => {}
        }

        let to_line = |off: usize| off.saturating_sub(line_offset).min(line.len());
        // How far we got. Clamping each span's start to it keeps the output ascending,
        // even if the spans aren't (they always are, but this costs nothing).
        let mut pos = 0;
        // Adjacent checkable spans are merged, so that words that cross
        // span boundaries (e.g. `**bold**ness`) don't get split in two.
        let mut seg = 0..0;

        for (i, h) in highlights.iter().enumerate() {
            // A span extends to the start of the next one. The last one (normally the sentinel) extends to EOL.
            let beg = to_line(h.start).max(pos);
            let end = highlights.get(i + 1).map_or(line.len(), |next| to_line(next.start));
            if beg >= end {
                continue;
            }
            pos = end;

            if !self.is_checkable(h.kind) {
                if !seg.is_empty() {
                    f(mem::take(&mut seg));
                }
            } else if seg.is_empty() {
                seg = beg..end;
            } else {
                seg.end = end;
            }
        }

        if !seg.is_empty() {
            f(seg);
        }
    }
}

/// Whether a Markdown line that starts outside a fenced code block is code:
/// an opening code fence, or an indented code block.
fn is_markdown_code_line(line: &[u8]) -> bool {
    let trimmed = line.trim_ascii_start();
    if trimmed.starts_with(b"```") || trimmed.starts_with(b"~~~") {
        return true;
    }

    // Nested list items are indented, too, but they aren't code.
    let indented = line.starts_with(b"\t") || line.starts_with(b"    ");
    indented && !is_markdown_list_item(trimmed)
}

/// Whether `line` (with leading whitespace already trimmed) starts with a list marker
/// (`-`, `*`, `+`, `1.` or `1)`), followed by a space or tab.
fn is_markdown_list_item(line: &[u8]) -> bool {
    let digits = line.iter().take_while(|b| b.is_ascii_digit()).count();
    let marker_len = match line.get(digits) {
        Some(b'-' | b'*' | b'+') if digits == 0 => 1,
        Some(b'.' | b')') if digits > 0 => digits + 1,
        _ => return false,
    };
    matches!(line.get(marker_len), Some(b' ' | b'\t'))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::arena::scratch_arena;
    use crate::collections::BVec;
    use crate::lsh::{Highlighter, LANGUAGES};

    fn language(id: &str) -> &'static Language {
        LANGUAGES.iter().find(|l| l.id == id).unwrap_or_else(|| panic!("no language {id:?}"))
    }

    fn span(start: usize, kind: HighlightKind) -> Highlight<HighlightKind> {
        Highlight { start, kind }
    }

    fn segments(
        scope: Scope,
        line: &str,
        line_offset: usize,
        highlights: &[Highlight<HighlightKind>],
    ) -> Vec<Range<usize>> {
        let mut res = Vec::new();
        scope.for_each_segment(line.as_bytes(), line_offset, highlights, |r| res.push(r));
        res
    }

    /// Spell checks `doc` the way `TextBuffer` does, with the real highlighter.
    /// Returns one entry per line: `None` if the line is skipped, otherwise the checked text.
    fn check_doc(language: Option<&'static Language>, doc: &str) -> Vec<Option<Vec<String>>> {
        let scope = Scope::for_language(language);
        let bytes = doc.as_bytes();
        let mut highlighter = match language {
            Some(language) if scope.needs_highlighter() => Some(Highlighter::new(&bytes, language)),
            _ => None,
        };
        let mut res = Vec::new();
        let mut line_offset = 0;

        for raw in doc.split_inclusive('\n') {
            let line = raw.strip_suffix('\n').unwrap_or(raw);
            let scratch = scratch_arena(None);
            let starts_idle = highlighter.as_ref().is_none_or(|h| h.is_idle());
            let highlights = match &mut highlighter {
                Some(h) => h.parse_next_line(&scratch),
                None => BVec::empty(),
            };

            if scope.skip_line(line.as_bytes(), starts_idle) {
                res.push(None);
            } else {
                let mut checked = Vec::new();
                scope.for_each_segment(line.as_bytes(), line_offset, &highlights, |r| {
                    checked.push(line[r].to_string());
                });
                res.push(Some(checked));
            }

            line_offset += raw.len();
        }

        res
    }

    /// Like [`check_doc`], but returns the lines' text (or `None` if skipped)
    /// for asserting which lines are checked at all.
    fn checked_lines<'d>(
        language: Option<&'static Language>,
        doc: &'d str,
    ) -> Vec<Option<&'d str>> {
        check_doc(language, doc)
            .iter()
            .zip(doc.lines())
            .map(|(checked, line)| checked.as_ref().map(|_| line))
            .collect()
    }

    fn strs(v: &[&str]) -> Option<Vec<String>> {
        Some(v.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn test_for_language_covers_all_languages() {
        // NOTE: If this fails after merging upstream, a language was added or renamed.
        // Decide whether its strings are prose (Code) or data (CommentsOnly) and update
        // both `Scope::for_language` and this table.
        let expected = [
            ("c", Scope::Code),
            ("cpp", Scope::Code),
            ("csharp", Scope::Code),
            ("diff", Scope::Off),
            ("fsharp", Scope::Code),
            ("git-commit", Scope::GitCommit),
            ("git-rebase", Scope::CommentsOnly),
            ("go", Scope::Code),
            ("ignore", Scope::CommentsOnly),
            ("java", Scope::Code),
            ("javascript", Scope::Code),
            ("json", Scope::CommentsOnly),
            ("lsh", Scope::Code),
            ("lua", Scope::Code),
            ("markdown", Scope::Markdown),
            ("odin", Scope::Code),
            ("powershell", Scope::Code),
            ("properties", Scope::CommentsOnly),
            ("python", Scope::Code),
            ("ruby", Scope::Code),
            ("rust", Scope::Code),
            ("shellscript", Scope::Code),
            ("sql", Scope::Code),
            ("toml", Scope::CommentsOnly),
            ("xml", Scope::CommentsOnly),
            ("yaml", Scope::CommentsOnly),
            ("zig", Scope::Code),
        ];

        let actual_ids: BTreeSet<_> = LANGUAGES.iter().map(|l| l.id).collect();
        let expected_ids: BTreeSet<_> = expected.iter().map(|&(id, _)| id).collect();
        assert_eq!(actual_ids, expected_ids);

        for (id, scope) in expected {
            assert_eq!(Scope::for_language(Some(language(id))), scope, "{id}");
        }
    }

    #[test]
    fn test_for_language_specific_ids() {
        // The mapping goes by id, not by pointer identity with `LANGUAGES`.
        static MARKDOWN: Language = Language { id: "markdown", name: "Markdown", entrypoint: 0 };
        static GIT_COMMIT: Language = Language { id: "git-commit", name: "Commit", entrypoint: 0 };
        static UNDERSCORE: Language = Language { id: "git_commit", name: "Commit", entrypoint: 0 };
        static UNKNOWN: Language = Language { id: "made-up", name: "Made Up", entrypoint: 0 };

        assert_eq!(Scope::for_language(None), Scope::Prose);
        assert_eq!(Scope::for_language(Some(&MARKDOWN)), Scope::Markdown);
        assert_eq!(Scope::for_language(Some(&GIT_COMMIT)), Scope::GitCommit);
        // Ids use dashes, never underscores.
        assert_eq!(Scope::for_language(Some(&UNDERSCORE)), Scope::Code);
        assert_eq!(Scope::for_language(Some(&UNKNOWN)), Scope::Code);
    }

    #[test]
    fn test_needs_highlighter() {
        assert!(!Scope::Off.needs_highlighter());
        assert!(!Scope::Prose.needs_highlighter());
        assert!(Scope::Markdown.needs_highlighter());
        assert!(Scope::GitCommit.needs_highlighter());
        assert!(Scope::Code.needs_highlighter());
        assert!(Scope::CommentsOnly.needs_highlighter());
    }

    #[test]
    fn test_is_checkable() {
        use HighlightKind::*;

        let markdown = [
            Other,
            Comment,
            String,
            MarkupHeading,
            MarkupBold,
            MarkupItalic,
            MarkupStrikethrough,
            MarkupList,
        ];
        let table: [(Scope, &[HighlightKind]); 4] = [
            (Scope::Markdown, &markdown),
            (Scope::GitCommit, &[Other]),
            (Scope::Code, &[Comment, String]),
            (Scope::CommentsOnly, &[Comment]),
        ];

        // The last kind (in declaration order) is `StorageType`.
        let all_kinds = (0..=StorageType as u32).map(|v| HighlightKind::try_from(v).unwrap());
        for kind in all_kinds {
            assert!(Scope::Prose.is_checkable(kind), "{kind:?}");
            assert!(!Scope::Off.is_checkable(kind), "{kind:?}");
            for (scope, checked) in &table {
                assert_eq!(scope.is_checkable(kind), checked.contains(&kind), "{scope:?} {kind:?}");
            }
        }

        // The link URL in particular.
        assert!(!Scope::Markdown.is_checkable(MarkupLink));
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn test_segments_prose_and_off() {
        let hl = [span(0, HighlightKind::Comment), span(5, HighlightKind::Other)];

        assert_eq!(segments(Scope::Prose, "hello world", 100, &[]), [0..11]);
        // Prose ignores highlights.
        assert_eq!(segments(Scope::Prose, "hello world", 0, &hl), [0..11]);
        assert_eq!(segments(Scope::Prose, "", 0, &[]), []);

        assert_eq!(segments(Scope::Off, "hello world", 0, &[]), []);
        assert_eq!(segments(Scope::Off, "hello world", 0, &hl), []);
    }

    #[test]
    fn test_segments_empty_highlights() {
        for scope in [Scope::Markdown, Scope::GitCommit, Scope::Code, Scope::CommentsOnly] {
            assert_eq!(segments(scope, "hello world", 0, &[]), [], "{scope:?}");
        }
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn test_segments_merge_adjacent() {
        use HighlightKind::*;

        // "say **bold**ness and [text](url) ok"
        //  0   4       12   17  21   26  30 34
        let line = "say **bold**ness and [text](url) ok";
        let off = 1000;
        let hl = [
            span(off, Other),
            span(off + 4, MarkupBold),
            span(off + 12, Other),
            span(off + 22, String),
            span(off + 26, Other),
            span(off + 28, MarkupLink),
            span(off + 31, Other),
            span(off + line.len(), Other), // sentinel
        ];

        // Everything up to the URL is one segment, so "**bold**ness" stays intact.
        assert_eq!(segments(Scope::Markdown, line, off, &hl), [0..28, 31..35]);
        assert_eq!(&line[0..28], "say **bold**ness and [text](");
        assert_eq!(&line[31..35], ") ok");

        // Code only checks the link text, which is a string.
        assert_eq!(segments(Scope::Code, line, off, &hl), [22..26]);
        assert_eq!(segments(Scope::CommentsOnly, line, off, &hl), []);
        assert_eq!(segments(Scope::GitCommit, line, off, &hl), [0..4, 12..22, 26..28, 31..35]);
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn test_segments_code_comment_and_string() {
        use HighlightKind::*;

        // `let s = "some text"; // a comment`
        let line = r#"let s = "some text"; // a comment"#;
        let hl = [
            span(0, KeywordOther),
            span(3, Other),
            span(8, String),
            span(19, Other),
            span(21, Comment),
            span(line.len(), Other),
        ];
        assert_eq!(segments(Scope::Code, line, 0, &hl), [8..19, 21..33]);
        assert_eq!(segments(Scope::CommentsOnly, line, 0, &hl), [21..33]);

        // A string directly followed by a comment merges into one segment.
        let hl = [span(0, String), span(4, Comment), span(9, Other)];
        assert_eq!(segments(Scope::Code, "\"ab\"// cd", 0, &hl), [0..9]);
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn test_segments_clamping() {
        use HighlightKind::*;

        let line = "abc def";

        // Spans past the end of the line are clamped to it.
        let hl = [span(10, Other), span(14, Comment), span(100, Other)];
        assert_eq!(segments(Scope::Code, line, 10, &hl), [4..7]);

        // Spans before the line offset are clamped to the start of the line.
        let hl = [span(0, Comment), span(12, Other), span(17, Other)];
        assert_eq!(segments(Scope::Code, line, 10, &hl), [0..2]);

        // Without a sentinel, the last span extends to the end of the line.
        let hl = [span(10, Other), span(14, Comment)];
        assert_eq!(segments(Scope::Code, line, 10, &hl), [4..7]);
        assert_eq!(segments(Scope::Code, line, 10, &[span(10, String)]), [0..7]);

        // Empty spans don't break up a segment.
        let hl = [span(0, Comment), span(3, Other), span(3, KeywordOther), span(3, String)];
        assert_eq!(segments(Scope::Code, line, 0, &hl), [0..7]);

        // Unsorted spans still produce ascending, non-overlapping ranges.
        let hl = [span(0, Comment), span(5, Other), span(2, String), span(7, Other)];
        let res = segments(Scope::Code, line, 0, &hl);
        assert!(res.windows(2).all(|w| w[0].end < w[1].start), "{res:?}");
        assert!(res.iter().all(|r| r.start < r.end && r.end <= line.len()), "{res:?}");

        // An empty line yields nothing.
        assert_eq!(segments(Scope::Code, "", 10, &[span(10, Comment), span(10, Other)]), []);
    }

    #[test]
    fn test_skip_line_markdown() {
        let md = Scope::Markdown;

        // Inside a fenced code block, including the closing fence.
        assert!(md.skip_line(b"plain prose", false));
        assert!(md.skip_line(b"```", false));

        // Opening fences.
        assert!(md.skip_line(b"```", true));
        assert!(md.skip_line(b"```python", true));
        assert!(md.skip_line(b"  ```rust", true));
        assert!(md.skip_line(b"~~~", true));
        assert!(md.skip_line(b"~~~ text", true));

        // Indented code.
        assert!(md.skip_line(b"    let x = 1;", true));
        assert!(md.skip_line(b"\tlet x = 1;", true));
        assert!(md.skip_line(b"        deeper", true));
        assert!(md.skip_line(b"    -not a list item", true));
        assert!(md.skip_line(b"    1.5 is a number", true));
        assert!(md.skip_line(b"    a. lettered", true));
        assert!(md.skip_line(b"    ", true));

        // Indented list items.
        assert!(!md.skip_line(b"    - nested", true));
        assert!(!md.skip_line(b"    * nested", true));
        assert!(!md.skip_line(b"    + nested", true));
        assert!(!md.skip_line(b"\t- nested", true));
        assert!(!md.skip_line(b"    1. nested", true));
        assert!(!md.skip_line(b"    12) nested", true));
        assert!(!md.skip_line(b"\t\t-\tnested", true));

        // Everything else.
        assert!(!md.skip_line(b"", true));
        assert!(!md.skip_line(b"plain prose", true));
        assert!(!md.skip_line(b"   three spaces", true));
        assert!(!md.skip_line(b"# Heading", true));
        assert!(!md.skip_line(b"> quote", true));
        assert!(!md.skip_line(b"- item", true));
        assert!(!md.skip_line(b"`code` and ``more``", true));
        assert!(!md.skip_line(b"~~strike~~", true));
    }

    #[test]
    fn test_skip_line_other_scopes() {
        let lines: [&[u8]; 6] =
            [b"", b"text", b"```", b"    indented", b"# comment", b"diff --git"];

        for line in lines {
            for idle in [true, false] {
                assert!(Scope::Off.skip_line(line, idle));
                assert!(!Scope::Prose.skip_line(line, idle));
                assert!(!Scope::Code.skip_line(line, idle));
                assert!(!Scope::CommentsOnly.skip_line(line, idle));
            }
            assert!(!Scope::GitCommit.skip_line(line, true));
            assert!(Scope::GitCommit.skip_line(line, false));
        }
    }

    #[test]
    fn test_highlighter_markdown_fences() {
        let doc = "\
Intro prose here.
```python
def spam(): # python comment
    return \"python string\"
```
Between fences.
```rust
// rust comment
let s = \"rust string\";
```
After rust.
```
no language here
```
  ```
indented fence body
  ```
Closing prose.
";
        assert_eq!(
            checked_lines(Some(language("markdown")), doc),
            [
                Some("Intro prose here."),
                None,
                None,
                None,
                None,
                Some("Between fences."),
                None,
                None,
                None,
                None,
                Some("After rust."),
                None,
                None,
                None,
                None,
                None,
                None,
                Some("Closing prose."),
            ]
        );
    }

    #[test]
    fn test_highlighter_markdown_indented_and_lists() {
        let doc = "\
Some prose:

    indented code block
\tcode with a tab

- first item
    - nested item
        - deeper item
    1. numbered nested
  * two spaces
> quoted wisdom
>     quoted code
";
        let res = check_doc(Some(language("markdown")), doc);
        assert_eq!(
            res,
            [
                strs(&["Some prose:"]),
                strs(&[]),
                None,
                None,
                strs(&[]),
                strs(&["- first item"]),
                strs(&["    - nested item"]),
                strs(&["        - deeper item"]),
                strs(&["    1. numbered nested"]),
                strs(&["  * two spaces"]),
                strs(&["> quoted wisdom"]),
                strs(&[">     quoted code"]),
            ]
        );
    }

    #[test]
    fn test_highlighter_markdown_links_and_inline() {
        let doc = "\
Read [the docs](https://example.com/page) today.
See [titled](http://example.com \"A title\") here.
Use `some_code()` and **bold** and _italic_ and ~~gone~~ words.
# A heading
";
        let res = check_doc(Some(language("markdown")), doc);
        assert_eq!(
            res,
            [
                // The URL is excluded; the link text stays connected to the surrounding prose.
                strs(&["Read [the docs](", ") today."]),
                strs(&["See [titled](", " \"A title\") here."]),
                // Inline code stays in (with its backticks), the tokenizer masks it.
                strs(&["Use `some_code()` and **bold** and _italic_ and ~~gone~~ words."]),
                strs(&["# A heading"]),
            ]
        );
    }

    #[test]
    fn test_highlighter_git_commit() {
        let doc = "\
Fix the frobnicator

The body explains things.
# Please enter the commit message for your changes.
#\tmodified:   src/main.rs
# ------------------------ >8 ------------------------
diff --git a/src/main.rs b/src/main.rs
index 1234567..89abcde 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1 +1 @@
-old line of text
+new line of text
";
        let res = check_doc(Some(language("git-commit")), doc);
        assert_eq!(
            res,
            [
                strs(&["Fix the frobnicator"]),
                strs(&[]),
                strs(&["The body explains things."]),
                strs(&[]),
                strs(&[]),
                strs(&[]),
                strs(&[]),
                None,
                None,
                None,
                None,
                None,
                None,
            ]
        );
    }

    #[test]
    fn test_highlighter_rust() {
        let doc = "\
// line comment
fn main() {
    let s = \"string literal\";
    /* block comment
       spanning lines */ let x = 1;
    call(\"text\", 'c'); /* tail */
}
";
        let res = check_doc(Some(language("rust")), doc);
        assert_eq!(
            res,
            [
                strs(&["// line comment"]),
                strs(&[]),
                strs(&["\"string literal\""]),
                strs(&["/* block comment"]),
                strs(&["       spanning lines */"]),
                strs(&["\"text\"", "'c'", "/* tail */"]),
                strs(&[]),
            ]
        );
    }

    #[test]
    fn test_highlighter_data_and_diff() {
        let json = "{\n    \"key\": \"value text\" // comment text\n}\n";
        assert_eq!(
            check_doc(Some(language("json")), json),
            [strs(&[]), strs(&["// comment text"]), strs(&[])]
        );

        let diff = "--- a/file\n+++ b/file\n@@ -1 +1 @@\n-some text\n+more text\n";
        assert_eq!(check_doc(Some(language("diff")), diff), [None, None, None, None, None]);

        let prose = "Plain text.\n\n  Indented text.\n";
        assert_eq!(
            check_doc(None, prose),
            [strs(&["Plain text."]), strs(&[]), strs(&["  Indented text."])]
        );
    }
}
