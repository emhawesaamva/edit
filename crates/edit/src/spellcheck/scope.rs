// Licensed under the MIT License.
// Fork-only: spellcheck (not part of upstream microsoft/edit).

//! Decides which parts of a line get spell checked.
//!
//! CONTRACT (other modules rely on this):
//! * [`Scope::for_language`] maps the buffer's language to a scope.
//! * If [`Scope::needs_highlighter`] is false, the caller passes an empty `highlights` slice.
//! * [`Scope::skip_line`] is called first, for every line in order, including lines that end up
//!   not being checked (e.g. because they're too long). It gets the line without its trailing
//!   newline (for a line that's too long to check, a prefix of it is enough), and:
//!   * `starts_idle`: If [`Scope::needs_idle_state`], `Highlighter::is_idle()` sampled right before
//!     parsing this line. For the first line, that requires parsing the one before it.
//!     Otherwise always `true`.
//!   * `md`: If [`Scope::needs_markdown_state`], Markdown's block state at the start of this line,
//!     which `skip_line` advances past it. For the first line, the caller gets it by feeding every
//!     line before it to [`MarkdownState::next_line`] (and may cache that). Otherwise ignored.
//! * [`Scope::for_each_segment`] gets the line without its trailing newline, the line's absolute
//!   buffer offset, and the line's highlight spans (absolute offsets, sentinel at the end, possibly
//!   empty). It calls `f` with ranges relative to `line`, ascending and non-overlapping,
//!   along with the [`TextKind`] the tokenizer should read the range as.

use std::mem;
use std::ops::Range;

use lsh::runtime::Highlight;

use super::tokenizer::TextKind;
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

    /// Whether [`Scope::skip_line`] reads `starts_idle`.
    pub fn needs_idle_state(self) -> bool {
        // A `diff --git` line (`git commit -v`) keeps the highlighter busy until the end of the file.
        // Markdown doesn't use this: Its highlighter hands fenced code blocks to the fence's language,
        // which doesn't always hand control back at the closing fence (e.g. an unclosed `'` in sh).
        matches!(self, Self::GitCommit)
    }

    /// Whether [`Scope::skip_line`] reads `md`.
    pub fn needs_markdown_state(self) -> bool {
        matches!(self, Self::Markdown)
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
    pub fn skip_line(self, line: &[u8], starts_idle: bool, md: &mut MarkdownState) -> bool {
        match self {
            Self::Off => true,
            Self::Markdown => md.next_line(line),
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
        mut f: impl FnMut(Range<usize>, TextKind),
    ) {
        match self {
            Self::Off => return,
            Self::Prose => {
                if !line.is_empty() {
                    f(0..line.len(), TextKind::Prose);
                }
                return;
            }
            _ => {}
        }

        let to_line = |off: usize| off.saturating_sub(line_offset).min(line.len());
        // Only code has string literals. Markdown's strings are link text and inline code, which are prose.
        // A string that runs into a comment (`"a"// b`) is one segment, and comments are prose, too.
        let kind = |string: bool, comment: bool| {
            if self == Self::Code && string && !comment {
                TextKind::CodeString
            } else {
                TextKind::Prose
            }
        };
        // How far we got. Clamping each span's start to it keeps the output ascending,
        // even if the spans aren't (they always are, but this costs nothing).
        let mut pos = 0;
        // Adjacent checkable spans are merged, so that words that cross
        // span boundaries (e.g. `**bold**ness`) don't get split in two.
        let mut seg = 0..0;
        // Whether `seg` contains a string or comment span.
        let mut string = false;
        let mut comment = false;

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
                    f(mem::take(&mut seg), kind(string, comment));
                }
                continue;
            }

            if seg.is_empty() {
                seg = beg..end;
                string = false;
                comment = false;
            } else {
                seg.end = end;
            }
            string |= h.kind == HighlightKind::String;
            comment |= h.kind == HighlightKind::Comment;
        }

        if !seg.is_empty() {
            f(seg, kind(string, comment));
        }
    }
}

/// Markdown's block structure at the start of a line, as far as spell checking cares:
/// whether the line is inside a fenced code block, and whether an indented line would be code.
///
/// It follows CommonMark, minus containers: Fences and indented code blocks are recognized
/// at the start of a line only (not after `>`), and list items don't shift the indentation.
/// Also, anything that isn't blank or code counts as paragraph text (headings, too).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct MarkdownState {
    /// The fenced code block we're in, if any.
    fence: Option<Fence>,
    /// What an indented line would continue.
    block: Block,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Fence {
    /// `` ` `` or `~`.
    ch: u8,
    /// The length of the opening fence. The closing one must be at least as long.
    len: usize,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
enum Block {
    /// The start of the file, or after a closing fence or a blank line that ended a paragraph.
    /// An indented line starts indented code here.
    #[default]
    Fresh,
    /// Paragraph text, which includes list items, headings, etc.
    /// Indented code can't interrupt a paragraph, so an indented line continues it.
    Paragraph,
    /// Indented code. Blank lines don't end it, only a line that isn't indented does.
    IndentedCode,
}

impl MarkdownState {
    /// Advances the state past `line` (without its trailing newline).
    /// Returns whether the line is code: a fence, inside a fenced code block, or indented code.
    pub fn next_line(&mut self, line: &[u8]) -> bool {
        let (indent, rest) = split_indent(line);

        if let Some(fence) = self.fence {
            // A closing fence has up to 3 spaces of indentation, at least as many
            // of the same character as the opening one, and nothing but whitespace after them.
            if indent < 4
                && let Some((ch, len)) = fence_run(rest)
                && ch == fence.ch
                && len >= fence.len
                && rest[len..].iter().all(|&b| b == b' ' || b == b'\t')
            {
                self.fence = None;
                self.block = Block::Fresh;
            }
            return true;
        }

        if rest.is_empty() {
            if self.block == Block::Paragraph {
                self.block = Block::Fresh;
            }
            return false;
        }

        if indent >= 4 {
            let code = match self.block {
                // Nested list items (after a blank line) are indented, too, but they aren't code.
                Block::Fresh => !is_markdown_list_item(rest),
                Block::Paragraph => false,
                Block::IndentedCode => true,
            };
            self.block = if code { Block::IndentedCode } else { Block::Paragraph };
            return code;
        }

        if let Some((ch, len)) = fence_run(rest)
            // The info string of a backtick fence can't contain backticks. Otherwise it's inline code.
            && (ch == b'~' || !rest[len..].contains(&b'`'))
        {
            self.fence = Some(Fence { ch, len });
            return true;
        }

        self.block = Block::Paragraph;
        false
    }
}

/// Splits `line` into the width of its indentation (tabs advance to the next multiple of 4)
/// and the rest.
fn split_indent(line: &[u8]) -> (usize, &[u8]) {
    let mut width = 0;
    let mut i = 0;
    while let Some(&b) = line.get(i) {
        match b {
            b' ' => width += 1,
            b'\t' => width += 4 - width % 4,
            _ => break,
        }
        i += 1;
    }
    (width, &line[i..])
}

/// If `text` starts with a code fence (3 or more `` ` `` or `~`), returns its character and length.
fn fence_run(text: &[u8]) -> Option<(u8, usize)> {
    let ch = *text.first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let len = text.iter().take_while(|&&b| b == ch).count();
    (len >= 3).then_some((ch, len))
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

    fn segments_with_kinds(
        scope: Scope,
        line: &str,
        line_offset: usize,
        highlights: &[Highlight<HighlightKind>],
    ) -> Vec<(Range<usize>, TextKind)> {
        let mut res = Vec::new();
        scope.for_each_segment(line.as_bytes(), line_offset, highlights, |r, k| res.push((r, k)));
        res
    }

    fn segments(
        scope: Scope,
        line: &str,
        line_offset: usize,
        highlights: &[Highlight<HighlightKind>],
    ) -> Vec<Range<usize>> {
        segments_with_kinds(scope, line, line_offset, highlights)
            .into_iter()
            .map(|(r, _)| r)
            .collect()
    }

    /// Spell checks `doc` the way `TextBuffer` does, with the real highlighter.
    /// Returns one entry per line: `None` if the line is skipped, otherwise the checked text and its kind.
    fn check_doc_with_kinds(
        language: Option<&'static Language>,
        doc: &str,
    ) -> Vec<Option<Vec<(String, TextKind)>>> {
        let scope = Scope::for_language(language);
        let bytes = doc.as_bytes();
        let mut highlighter = match language {
            Some(language) if scope.needs_highlighter() => Some(Highlighter::new(&bytes, language)),
            _ => None,
        };
        let mut md = MarkdownState::default();
        let mut res = Vec::new();
        let mut line_offset = 0;

        for raw in doc.split_inclusive('\n') {
            let line = raw.strip_suffix('\n').unwrap_or(raw);
            let scratch = scratch_arena(None);
            let starts_idle =
                !scope.needs_idle_state() || highlighter.as_ref().is_none_or(|h| h.is_idle());
            let highlights = match &mut highlighter {
                Some(h) => h.parse_next_line(&scratch),
                None => BVec::empty(),
            };

            if scope.skip_line(line.as_bytes(), starts_idle, &mut md) {
                res.push(None);
            } else {
                let mut checked = Vec::new();
                scope.for_each_segment(line.as_bytes(), line_offset, &highlights, |r, k| {
                    checked.push((line[r].to_string(), k));
                });
                res.push(Some(checked));
            }

            line_offset += raw.len();
        }

        res
    }

    /// Like [`check_doc_with_kinds`], without the kinds.
    fn check_doc(language: Option<&'static Language>, doc: &str) -> Vec<Option<Vec<String>>> {
        check_doc_with_kinds(language, doc)
            .into_iter()
            .map(|line| line.map(|segs| segs.into_iter().map(|(s, _)| s).collect()))
            .collect()
    }

    /// Feeds `doc` to a fresh [`MarkdownState`] and returns the non-blank lines that aren't code.
    fn markdown_prose(doc: &str) -> Vec<&str> {
        let mut md = MarkdownState::default();
        doc.lines()
            .filter(|line| !md.next_line(line.as_bytes()) && !line.trim().is_empty())
            .collect()
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
    fn test_needs_state() {
        use Scope::*;

        for scope in [Off, Prose, Markdown, GitCommit, Code, CommentsOnly] {
            assert_eq!(scope.needs_idle_state(), scope == GitCommit, "{scope:?}");
            assert_eq!(scope.needs_markdown_state(), scope == Markdown, "{scope:?}");
            // Both states are only meaningful along with highlights.
            assert!(!scope.needs_idle_state() || scope.needs_highlighter(), "{scope:?}");
        }
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

        // Plain text is prose, even where a highlighter would see a string.
        let hl = [span(0, HighlightKind::String), span(11, HighlightKind::Other)];
        assert_eq!(
            segments_with_kinds(Scope::Prose, "hello world", 0, &hl),
            [(0..11, TextKind::Prose)]
        );
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
    fn test_segments_kinds() {
        use HighlightKind::*;
        use TextKind::{CodeString, Prose};

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
        assert_eq!(
            segments_with_kinds(Scope::Code, line, 0, &hl),
            [(8..19, CodeString), (21..33, Prose)]
        );
        assert_eq!(segments_with_kinds(Scope::CommentsOnly, line, 0, &hl), [(21..33, Prose)]);

        // A string merged with a comment is read as a comment, in either order.
        let hl = [span(0, String), span(4, Comment), span(9, Other)];
        assert_eq!(segments_with_kinds(Scope::Code, "\"ab\"// cd", 0, &hl), [(0..9, Prose)]);
        let hl = [span(0, Comment), span(5, String), span(9, Other)];
        assert_eq!(segments_with_kinds(Scope::Code, "/* */\"ab\"", 0, &hl), [(0..9, Prose)]);

        // Each segment gets its own kind.
        // `"ab" + /* c */ + "d"`
        let line = r#""ab" + /* c */ + "d""#;
        let hl = [
            span(0, String),
            span(4, Other),
            span(7, Comment),
            span(14, Other),
            span(17, String),
            span(line.len(), Other),
        ];
        assert_eq!(
            segments_with_kinds(Scope::Code, line, 0, &hl),
            [(0..4, CodeString), (7..14, Prose), (17..20, CodeString)]
        );

        // Empty spans don't change the kind.
        let hl = [span(0, String), span(2, Comment), span(2, String), span(4, Other)];
        assert_eq!(segments_with_kinds(Scope::Code, "\"ab\"", 0, &hl), [(0..4, CodeString)]);

        // Markdown's strings are link text and inline code. Commit messages have none.
        let line = "see [the docs](url) now";
        let hl = [
            span(0, Other),
            span(5, String),
            span(13, Other),
            span(15, MarkupLink),
            span(18, Other),
            span(line.len(), Other),
        ];
        assert_eq!(
            segments_with_kinds(Scope::Markdown, line, 0, &hl),
            [(0..15, Prose), (18..23, Prose)]
        );
        assert_eq!(
            segments_with_kinds(Scope::GitCommit, line, 0, &hl),
            [(0..5, Prose), (13..15, Prose), (18..23, Prose)]
        );
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
        // Whether `line` is skipped at the start of the file (or after a blank line).
        let skip = |line: &[u8]| {
            let mut md = MarkdownState::default();
            // The highlighter's state doesn't matter.
            let res = Scope::Markdown.skip_line(line, false, &mut md);
            assert_eq!(res, Scope::Markdown.skip_line(line, true, &mut MarkdownState::default()));
            res
        };

        // Opening fences.
        assert!(skip(b"```"));
        assert!(skip(b"```python"));
        assert!(skip(b"   ```rust"));
        assert!(skip(b"````"));
        assert!(skip(b"~~~"));
        assert!(skip(b"~~~ text"));
        assert!(skip(b"~~~ tilde info can have ` backticks"));

        // Indented code.
        assert!(skip(b"    let x = 1;"));
        assert!(skip(b"\tlet x = 1;"));
        assert!(skip(b"  \tlet x = 1;"));
        assert!(skip(b"        deeper"));
        assert!(skip(b"    -not a list item"));
        assert!(skip(b"    1.5 is a number"));
        assert!(skip(b"    a. lettered"));
        assert!(skip(b"    ```"));

        // Indented list items.
        assert!(!skip(b"    - nested"));
        assert!(!skip(b"    * nested"));
        assert!(!skip(b"    + nested"));
        assert!(!skip(b"\t- nested"));
        assert!(!skip(b"    1. nested"));
        assert!(!skip(b"    12) nested"));
        assert!(!skip(b"\t\t-\tnested"));

        // Everything else.
        assert!(!skip(b""));
        assert!(!skip(b"    "));
        assert!(!skip(b"plain prose"));
        assert!(!skip(b"   three spaces"));
        assert!(!skip(b"# Heading"));
        assert!(!skip(b"> quote"));
        assert!(!skip(b"- item"));
        assert!(!skip(b"`code` and ``more``"));
        assert!(!skip(b"``two backticks``"));
        assert!(!skip(b"```inline``` code"));
        assert!(!skip(b"~~strike~~"));
    }

    #[test]
    fn test_skip_line_other_scopes() {
        let lines: [&[u8]; 6] =
            [b"", b"text", b"```", b"    indented", b"# comment", b"diff --git"];

        for line in lines {
            let mut md = MarkdownState::default();
            for idle in [true, false] {
                assert!(Scope::Off.skip_line(line, idle, &mut md));
                assert!(!Scope::Prose.skip_line(line, idle, &mut md));
                assert!(!Scope::Code.skip_line(line, idle, &mut md));
                assert!(!Scope::CommentsOnly.skip_line(line, idle, &mut md));
            }
            assert!(!Scope::GitCommit.skip_line(line, true, &mut md));
            assert!(Scope::GitCommit.skip_line(line, false, &mut md));
            // Only Markdown uses the state.
            assert_eq!(md, MarkdownState::default());
        }
    }

    #[test]
    fn test_markdown_fences() {
        // Backticks and tildes, each closed only by its own kind.
        let doc = "\
Before.
```
inside backticks
~~~
still inside backticks
```
Between.
~~~ text
inside tildes
```
still inside tildes
~~~
After.
";
        assert_eq!(markdown_prose(doc), ["Before.", "Between.", "After."]);

        // The closing fence must be at least as long as the opening one.
        let doc =
            "````md\n```\nstill inside\n````\nAfter.\n~~~~~\n~~~~\nstill inside\n~~~~~~\nEnd.\n";
        assert_eq!(markdown_prose(doc), ["After.", "End."]);

        // Up to 3 spaces of indentation on either fence. More is content (or indented code).
        let doc = "   ```\n    ```\nstill inside\n   ```\nAfter.\n";
        assert_eq!(markdown_prose(doc), ["After."]);
        let doc = "```\n\t```\nstill inside\n  \t```\n```\nAfter.\n";
        assert_eq!(markdown_prose(doc), ["After."]);

        // Nothing but whitespace after the closing fence.
        let doc = "```\n``` sh\nstill inside\n```  \t\nAfter.\n";
        assert_eq!(markdown_prose(doc), ["After."]);
        let doc = "~~~\n~~~ x\nstill inside\n~~~ \nAfter.\n";
        assert_eq!(markdown_prose(doc), ["After."]);

        // A backtick fence's info string can't contain backticks. That's inline code instead.
        let doc = "```not `a` fence\nProse.\n~~~ a `tilde` fence\ninside\n~~~\nAfter.\n";
        assert_eq!(markdown_prose(doc), ["```not `a` fence", "Prose.", "After."]);

        // Fewer than 3 characters aren't a fence.
        let doc = "``\nProse.\n~~\nMore.\n";
        assert_eq!(markdown_prose(doc), ["``", "Prose.", "~~", "More."]);

        // An unclosed fence runs to the end of the file.
        let doc = "Prose.\n```\nfn main() {}\n\nMore code.\n";
        assert_eq!(markdown_prose(doc), ["Prose."]);
    }

    #[test]
    fn test_markdown_indented_code() {
        // Indented code at the start of the file and after a blank line, but not after a paragraph.
        let doc = "    code at the start
Paragraph text
    continues the paragraph
\tthis, too

    code after a blank line
\tmore code

    code after a blank line inside the code
    - not a list item inside code
Paragraph.
";
        assert_eq!(
            markdown_prose(doc),
            ["Paragraph text", "    continues the paragraph", "\tthis, too", "Paragraph.",]
        );

        // A closing fence ends the paragraph, too.
        let doc = "```\ncode\n```\n    indented code\nParagraph.\n";
        assert_eq!(markdown_prose(doc), ["Paragraph."]);

        // Lists: hard-wrapped items and their continuation lines are paragraph text.
        let doc = "\
- Top item
    - Nested item that is long and
      wraps onto the next line.
        - Deeper item that also
          wraps.

    - Nested item after a blank line.
1. Numbered item
       with a deeply indented continuation.
";
        assert_eq!(markdown_prose(doc), doc.lines().filter(|l| !l.is_empty()).collect::<Vec<_>>());

        // Headings, quotes and list items count as paragraphs.
        let doc = "# Heading\n    not code\n> Quote\n    not code\n- Item\n    not code\n";
        assert_eq!(markdown_prose(doc), doc.lines().collect::<Vec<_>>());
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
    fn test_highlighter_markdown_lost_fences() {
        // The highlighter hands fenced code to the fence's language. Some don't return at the
        // closing fence (a YAML block scalar at the end, an unmatched `'` in sh), which leaves the
        // highlighter inside a fence for the rest of the file, and inverts every later fence.
        let doc = "\
Setup:
```yaml
- run: |
    cargo build
```
Prose after yaml.
```sh
echo it's fine
```
Prose after sh.
~~~
tilde fence body
~~~
```rust
fn main() {}
```
Closing prose.
";
        assert_eq!(
            check_doc(Some(language("markdown")), doc),
            [
                strs(&["Setup:"]),
                None,
                None,
                None,
                None,
                strs(&["Prose after yaml."]),
                None,
                None,
                None,
                strs(&["Prose after sh."]),
                None,
                None,
                None,
                None,
                None,
                None,
                strs(&["Closing prose."]),
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

    #[test]
    fn test_highlighter_kinds() {
        use TextKind::{CodeString, Prose};

        let kinds = |language: Option<&'static Language>, line: &str| {
            let mut res = check_doc_with_kinds(language, line);
            assert_eq!(res.len(), 1);
            res.pop().flatten().unwrap_or_default()
        };
        let owned = |v: &[(&str, TextKind)]| -> Vec<(String, TextKind)> {
            v.iter().map(|&(s, k)| (s.to_string(), k)).collect()
        };

        // Only string literals in code are code strings.
        let rust = Some(language("rust"));
        assert_eq!(
            kinds(rust, "let s = \"a string\"; // a comment"),
            owned(&[("\"a string\"", CodeString), ("// a comment", Prose)])
        );
        assert_eq!(
            kinds(rust, "/// Docs with `code`."),
            owned(&[("/// Docs with `code`.", Prose)])
        );
        let python = Some(language("python"));
        assert_eq!(
            kinds(python, "x = 'text' # note"),
            owned(&[("'text'", CodeString), ("# note", Prose)])
        );

        // Data formats only check comments.
        assert_eq!(
            kinds(Some(language("json")), "{\"key\": \"value\"} // note"),
            owned(&[("// note", Prose)])
        );

        // Markdown's link text and inline code are prose.
        assert_eq!(
            kinds(Some(language("markdown")), "Read [the docs](https://example.com) and `code`."),
            owned(&[("Read [the docs](", Prose), (") and `code`.", Prose)])
        );

        // So are commit messages and plain text.
        assert_eq!(
            kinds(Some(language("git-commit")), "Fix \"quoted\" text"),
            owned(&[("Fix \"quoted\" text", Prose)])
        );
        assert_eq!(kinds(None, "Plain \"text\""), owned(&[("Plain \"text\"", Prose)]));
    }
}
