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
    /// Comments and strings, in PowerShell, whose strings don't use `\` escapes.
    CodeVerbatim,
    /// Comments and strings, in SQL, whose strings don't use `\` escapes.
    CodeSql,
    /// Comments only (data formats, whose strings are mostly identifiers).
    CommentsOnly,
    /// A `git rebase -i` todo list: Commit subjects and `exec` lines (which its highlighter
    /// yields as comments), minus git's `#` help text.
    RebaseTodo,
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
            // `\` is a path separator in their strings, not an escape.
            "powershell" => Self::CodeVerbatim,
            "sql" => Self::CodeSql,
            "git-rebase" => Self::RebaseTodo,
            // Data formats: Their strings are mostly keys, paths and identifiers.
            "ignore" | "json" | "properties" | "toml" | "xml" | "yaml" => Self::CommentsOnly,
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
            Self::Code | Self::CodeVerbatim | Self::CodeSql => {
                matches!(kind, HighlightKind::Comment | HighlightKind::String)
            }
            Self::CommentsOnly | Self::RebaseTodo => matches!(kind, HighlightKind::Comment),
        }
    }

    /// Whether the entire line should be skipped.
    pub fn skip_line(self, line: &[u8], starts_idle: bool, md: &mut MarkdownState) -> bool {
        match self {
            Self::Off => true,
            Self::Markdown => md.next_line(line),
            Self::GitCommit => !starts_idle,
            // git's help text. (An indented `#` line isn't a comment to the highlighter.)
            Self::RebaseTodo => line.first() == Some(&b'#'),
            Self::Prose | Self::Code | Self::CodeVerbatim | Self::CodeSql | Self::CommentsOnly => {
                false
            }
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
        let kind = |string: bool, comment: bool| match self {
            _ if !string || comment => TextKind::Prose,
            Self::Code => TextKind::CodeString,
            Self::CodeVerbatim => TextKind::VerbatimString,
            Self::CodeSql => TextKind::SqlString,
            _ => TextKind::Prose,
        };
        // The tokenizer masks `<!` through the next `>` like a tag, which would hide an XML comment
        // that ends on the line it starts on. So its `<!--` is cut off. (Markdown's HTML comments
        // stay masked, so that directives like `<!-- markdownlint-disable -->` aren't flagged.)
        // (MySQL's quoted identifiers are string highlights here. In a SQL string, the tokenizer
        // reads `` `Customers` `` as a code span and masks it.)
        let mut f = |mut seg: Range<usize>, kind: TextKind| {
            if self == Self::CommentsOnly && line[seg.clone()].starts_with(b"<!--") {
                seg.start += 4;
            }
            if !seg.is_empty() {
                f(seg, kind);
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
/// whether the line is in YAML front matter or a fenced code block, and whether an indented line
/// would be code.
///
/// It follows CommonMark, minus block quotes: Fences and indented code blocks are recognized at the
/// start of a line and after a list marker (not after `>`). Their indentation is measured from the
/// content column of the list item they're in, but only the innermost item's column is known.
/// A line that's indented less than that (and doesn't continue a paragraph) measures from its own
/// indentation instead. That's only an upper bound, so a fence there measures from the lowest
/// column its indentation allows. Also, anything that isn't blank or code counts as paragraph text
/// (headings, too).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct MarkdownState {
    /// Where we are relative to the YAML front matter.
    front: FrontMatter,
    /// The fenced code block we're in, if any.
    fence: Option<Fence>,
    /// What an indented line would continue.
    block: Block,
    /// The content column of the list item we're in (the indentation of its text), or 0.
    list: usize,
    /// Whether `list` came from a list marker. After a line that's indented less,
    /// it's only an upper bound.
    exact: bool,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
enum FrontMatter {
    /// The next line is the first line of the file.
    #[default]
    Start,
    /// The first line was `---`.
    Opened,
    /// After the first line of front matter.
    Inside,
    /// After the front matter, or there's none.
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Fence {
    /// `` ` `` or `~`.
    ch: u8,
    /// The length of the opening fence. The closing one must be at least as long.
    len: usize,
    /// The column the closing fence's indentation is measured from (the list item's content column,
    /// or the lowest it can be if that isn't known exactly). A line indented less ends the block.
    base: usize,
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
    /// Indented code. Blank lines don't end it, only a line that's indented less does.
    IndentedCode,
}

impl MarkdownState {
    /// Advances the state past `line` (without its trailing newline). Returns whether the line
    /// is code: front matter, a fence, inside a fenced code block, or indented code.
    pub fn next_line(&mut self, line: &[u8]) -> bool {
        if self.front != FrontMatter::Done && self.next_front_matter_line(line) {
            return true;
        }

        let (indent, rest) = split_indent(line);

        if let Some(fence) = self.fence {
            // A closing fence has up to 3 spaces of indentation (beyond the list item's text),
            // at least as many of the same character as the opening one,
            // and nothing but whitespace after them.
            if indent < fence.base + 4
                && let Some((ch, len)) = fence_run(rest)
                && ch == fence.ch
                && len >= fence.len
                && rest[len..].iter().all(|&b| b == b' ' || b == b'\t')
            {
                self.fence = None;
                self.block = Block::Fresh;
                return true;
            }
            // A line that's indented less than the list item's text ends the item,
            // and the fence along with it (e.g. a fence that was never closed).
            if rest.is_empty() || indent >= fence.base {
                return true;
            }
            self.fence = None;
            self.block = Block::Fresh;
        }

        if rest.is_empty() {
            if self.block == Block::Paragraph {
                self.block = Block::Fresh;
            }
            return false;
        }

        let item = list_marker_width(rest).filter(|_| {
            // Outside of a list, only a bullet or `1.` can start one in the middle of a paragraph.
            // Otherwise, hard-wrapped text like "since\n2024. Then" would be a list item.
            self.block != Block::Paragraph || self.list > 0 || can_interrupt_paragraph(rest)
        });
        // Indented code needs 4 columns more than the list item's text.
        let code = indent >= self.list + 4
            && match self.block {
                // Nested list items (after a blank line) are indented, too, but they aren't code.
                Block::Fresh => item.is_none(),
                Block::Paragraph => false,
                Block::IndentedCode => true,
            };
        if code {
            self.block = Block::IndentedCode;
            return true;
        }

        if let Some(width) = item {
            self.block = Block::Paragraph;
            // A list marker that's indented 4 columns more than the item's text isn't one.
            // It's paragraph text (or code after a blank line, but it's checked anyway).
            if indent >= self.list + 4 {
                return false;
            }
            self.list = indent + width;
            self.exact = true;
            // `- ```sh`: A fence right after the list marker.
            if let Some((ch, len)) = fence_open(&rest[width..]) {
                self.fence = Some(Fence { ch, len, base: self.list });
                return true;
            }
            return false;
        }

        let fence = if indent < self.list + 4 { fence_open(rest) } else { None };
        // A line indented less than the item's text isn't part of the item, unless it continues
        // the item's paragraph (which a fence, heading or thematic break can't). The item it's part
        // of instead (if any) has its text at this indentation at most. Close enough.
        if indent < self.list
            && (self.block != Block::Paragraph
                || fence.is_some()
                || is_atx_heading(rest)
                || is_thematic_break(rest))
        {
            self.list = indent;
            self.exact = false;
        }
        if let Some((ch, len)) = fence {
            // Without an exact column, the fence may be in an outer item or top-level (like one
            // indented by 2 after `1. `), where its content can be indented less, and its closing
            // fence more. A fence is indented at most 3 more than its item's text, so measuring
            // from there neither ends nor closes the block too early.
            let base = if self.exact { self.list } else { indent.saturating_sub(3) };
            self.fence = Some(Fence { ch, len, base });
            return true;
        }

        self.block = Block::Paragraph;
        false
    }

    /// YAML front matter is a `---` line at the start of the file, up to and including the next
    /// `---` or `...` line. Returns whether `line` is part of it.
    fn next_front_matter_line(&mut self, line: &[u8]) -> bool {
        let is_delimiter = |delimiter: &[u8]| {
            line.strip_prefix(delimiter)
                .is_some_and(|rest| rest.iter().all(|&b| b == b' ' || b == b'\t'))
        };

        self.front = match self.front {
            FrontMatter::Start if is_delimiter(b"---") => FrontMatter::Opened,
            FrontMatter::Opened | FrontMatter::Inside
                if is_delimiter(b"---") || is_delimiter(b"...") =>
            {
                FrontMatter::Done
            }
            // `---` and a blank line is a thematic break. Front matter starts with a key.
            FrontMatter::Opened if split_indent(line).1.is_empty() => {
                self.front = FrontMatter::Done;
                return false;
            }
            // Front matter that's never closed ends at the first line that can't be YAML.
            FrontMatter::Opened | FrontMatter::Inside if is_front_matter_like(line) => {
                FrontMatter::Inside
            }
            _ => {
                self.front = FrontMatter::Done;
                return false;
            }
        };
        true
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

/// Like [`fence_run`], but only if `text` is an opening fence:
/// The info string of a backtick fence can't contain backticks. Otherwise it's inline code.
fn fence_open(text: &[u8]) -> Option<(u8, usize)> {
    fence_run(text).filter(|&(ch, len)| ch == b'~' || !text[len..].contains(&b'`'))
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

/// If `line` (with leading whitespace already trimmed) is a list item, returns the width of its
/// marker and the whitespace after it, which is where the item's text starts.
fn list_marker_width(line: &[u8]) -> Option<usize> {
    // `* * *` is a thematic break, not a list item.
    if !is_markdown_list_item(line) || is_thematic_break(line) {
        return None;
    }
    let marker = line.iter().take_while(|b| b.is_ascii_digit()).count() + 1;
    let spaces = line[marker..].iter().take_while(|&&b| b == b' ').count();
    // After 5 or more spaces (or a tab), the text starts 1 column after the marker.
    // It's indented code then, which is good enough for a tab, too.
    Some(marker + if (1..=4).contains(&spaces) { spaces } else { 1 })
}

/// Whether the list item `line` (with leading whitespace already trimmed) can start a list in the
/// middle of a paragraph: if it's a bullet, or if it's numbered 1.
fn can_interrupt_paragraph(line: &[u8]) -> bool {
    let digits = line.iter().take_while(|b| b.is_ascii_digit()).count();
    match line[..digits].split_last() {
        None => true,
        Some((&last, zeros)) => last == b'1' && zeros.iter().all(|&b| b == b'0'),
    }
}

/// Whether `line` (with leading whitespace already trimmed) is a thematic break:
/// 3 or more `-`, `*` or `_`, and nothing else but spaces and tabs.
fn is_thematic_break(line: &[u8]) -> bool {
    let Some(&ch @ (b'-' | b'*' | b'_')) = line.first() else {
        return false;
    };
    line.iter().all(|&b| b == ch || b == b' ' || b == b'\t')
        && line.iter().filter(|&&b| b == ch).count() >= 3
}

/// Whether `line` (with leading whitespace already trimmed) is an ATX heading (`# Heading`).
fn is_atx_heading(line: &[u8]) -> bool {
    let hashes = line.iter().take_while(|&&b| b == b'#').count();
    (1..=6).contains(&hashes) && matches!(line.get(hashes), None | Some(b' ' | b'\t'))
}

/// Whether `line` can be part of YAML front matter: blank, indented, a comment,
/// a sequence item, or a line with a key.
fn is_front_matter_like(line: &[u8]) -> bool {
    match line.first() {
        None | Some(b' ' | b'\t' | b'#' | b'-') => true,
        Some(_) => line.contains(&b':'),
    }
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
        // Decide whether its strings are prose (Code, or CodeVerbatim/CodeSql if `\` isn't an
        // escape in them) or data (CommentsOnly) and update both `Scope::for_language` and this table.
        let expected = [
            ("c", Scope::Code),
            ("cpp", Scope::Code),
            ("csharp", Scope::Code),
            ("diff", Scope::Off),
            ("fsharp", Scope::Code),
            ("git-commit", Scope::GitCommit),
            ("git-rebase", Scope::RebaseTodo),
            ("go", Scope::Code),
            ("ignore", Scope::CommentsOnly),
            ("java", Scope::Code),
            ("javascript", Scope::Code),
            ("json", Scope::CommentsOnly),
            ("lsh", Scope::Code),
            ("lua", Scope::Code),
            ("markdown", Scope::Markdown),
            ("odin", Scope::Code),
            ("powershell", Scope::CodeVerbatim),
            ("properties", Scope::CommentsOnly),
            ("python", Scope::Code),
            ("ruby", Scope::Code),
            ("rust", Scope::Code),
            ("shellscript", Scope::Code),
            ("sql", Scope::CodeSql),
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
    fn test_rebase_todo() {
        // Commit subjects and `exec` lines are checked, git's help text isn't.
        let doc = "pick 1a2b3c4 Fix teh widget\n\
                   exec make tset\n\
                   # Rebase 1a2b3c4..5d6e7f8 onto 1a2b3c4 (2 commands)\n\
                   #\n\
                   # Commands:\n\
                   # p, pick <commit> = use commit\n\
                   # x, exec <command> = run command (the rest of the line) using shell\n";
        let lines = checked_lines(Some(language("git-rebase")), doc);
        assert_eq!(&lines[..2], [Some("pick 1a2b3c4 Fix teh widget"), Some("exec make tset")]);
        assert!(lines[2..].iter().all(Option::is_none), "{lines:?}");
    }

    #[test]
    fn test_needs_highlighter() {
        assert!(!Scope::Off.needs_highlighter());
        assert!(!Scope::Prose.needs_highlighter());
        assert!(Scope::Markdown.needs_highlighter());
        assert!(Scope::GitCommit.needs_highlighter());
        assert!(Scope::Code.needs_highlighter());
        assert!(Scope::CodeVerbatim.needs_highlighter());
        assert!(Scope::CodeSql.needs_highlighter());
        assert!(Scope::CommentsOnly.needs_highlighter());
        assert!(Scope::RebaseTodo.needs_highlighter());
    }

    #[test]
    fn test_needs_state() {
        use Scope::*;

        for scope in
            [Off, Prose, Markdown, GitCommit, Code, CodeVerbatim, CodeSql, CommentsOnly, RebaseTodo]
        {
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
        let table: [(Scope, &[HighlightKind]); 7] = [
            (Scope::Markdown, &markdown),
            (Scope::GitCommit, &[Other]),
            (Scope::Code, &[Comment, String]),
            (Scope::CodeVerbatim, &[Comment, String]),
            (Scope::CodeSql, &[Comment, String]),
            (Scope::CommentsOnly, &[Comment]),
            (Scope::RebaseTodo, &[Comment]),
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
        for scope in [
            Scope::Markdown,
            Scope::GitCommit,
            Scope::Code,
            Scope::CodeVerbatim,
            Scope::CommentsOnly,
        ] {
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
        assert_eq!(segments(Scope::CodeVerbatim, line, off, &hl), [22..26]);
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
        use TextKind::{CodeString, Prose, VerbatimString};

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

        // PowerShell and SQL strings are verbatim. Comments are prose, as in all code.
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
            segments_with_kinds(Scope::CodeVerbatim, line, 0, &hl),
            [(0..4, VerbatimString), (7..14, Prose), (17..20, VerbatimString)]
        );
        let hl = [span(0, String), span(4, Comment), span(9, Other)];
        assert_eq!(
            segments_with_kinds(Scope::CodeVerbatim, "\"ab\"// cd", 0, &hl),
            [(0..9, Prose)]
        );

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
        assert!(!skip(b"----"));

        // `---` at the start of the file may open front matter. It has no words either way.
        assert!(skip(b"---"));
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
                assert!(!Scope::CodeVerbatim.skip_line(line, idle, &mut md));
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
    fn test_markdown_list_fences() {
        // A fence right after a list marker. Its closing fence used to open another one,
        // which skipped the prose after it and checked the code after that.
        let doc = "\
# Setup

1. ```bash
   npm install
   ```
2. Run the build.

```sh
tool --flag
```

Then check the output.
";
        assert_eq!(markdown_prose(doc), ["# Setup", "2. Run the build.", "Then check the output."]);

        let doc = "- ```sh\n  echo hi\n  ```\n\nThen prose after the list.\n";
        assert_eq!(markdown_prose(doc), ["Then prose after the list."]);
        let doc = "+ ```\n  code\n  ```\n\nAfter.\n";
        assert_eq!(markdown_prose(doc), ["After."]);
        let doc = "* ~~~ text\n  code\n  ~~~\n10) ```\n    code\n    ```\nAfter.\n";
        assert_eq!(markdown_prose(doc), ["After."]);

        // Later fences without an info string keep their parity.
        let doc = "\
1. ```bash
   npm ci
   ```
2. Run the build.

```
foo --bar
```

Check this.

```
baz qux
```

End.
";
        assert_eq!(markdown_prose(doc), ["2. Run the build.", "Check this.", "End."]);

        // A closing fence's indentation is measured from the item's text.
        let doc = "- Step one:\n   ```sh\n    echo hi\n    ```\nThen prose.\n";
        assert_eq!(markdown_prose(doc), ["- Step one:", "Then prose."]);
        let doc = "- item\nlazy line\n  ```\n  code\n    ```\nAfter.\n";
        assert_eq!(markdown_prose(doc), ["- item", "lazy line", "After."]);
        // The closing fence of a top-level fence still can't be indented 4 or more.
        let doc = "- item\n\n```\ncode\n    ```\nstill code\n```\nAfter.\n";
        assert_eq!(markdown_prose(doc), ["- item", "After."]);

        // A fence in a nested item, indented 4 or more.
        let doc = "- a\n  - b\n    ```sh\n    echo hi\n    ```\n  - c\n";
        assert_eq!(markdown_prose(doc), ["- a", "  - b", "  - c"]);
        let doc = "- a\n    - ```sh\n      echo hi\n      ```\n    - c\n";
        assert_eq!(markdown_prose(doc), ["- a", "    - c"]);
        let doc = "1. a\n\n    ```\n    code\n    ```\n\n    More a.\n";
        assert_eq!(markdown_prose(doc), ["1. a", "    More a."]);

        // A line that's indented less than the item's text ends the item and its fence.
        let doc = "- ```sh\n  echo hi\n- Next item.\n\nMore prose.\n";
        assert_eq!(markdown_prose(doc), ["- Next item.", "More prose."]);
        let doc = "- Install:\n\n  ```sh\nnpm ci\n  ```\n\nAfter teh.\n";
        assert_eq!(markdown_prose(doc), ["- Install:", "npm ci"]);
        // But a closing fence closes it.
        let doc = "1. Run:\n   ```sh\n   npm ci\n```\n2. Next.\n";
        assert_eq!(markdown_prose(doc), ["1. Run:", "2. Next."]);

        // A fence that's indented less than the item's text isn't in the item. It may be top-level,
        // where its content can have any indentation. Content at column 0 used to end it, and its
        // closing fence then opened another one, which hid the rest of the file.
        let doc = "\
1. Create the config file:

  ```yaml
name: tset
on: push
  ```

2. Commit teh file.

More prose with wrod.
";
        assert_eq!(
            markdown_prose(doc),
            ["1. Create the config file:", "2. Commit teh file.", "More prose with wrod."]
        );
        let doc = "\
1. Step one
2. Step two

 ```
cat <<EOF > fiel.txt
EOF
 ```

Prose teh.
";
        assert_eq!(markdown_prose(doc), ["1. Step one", "2. Step two", "Prose teh."]);
        let doc = "1. Run:\n  ```sh\nnpm ci\n  ```\nAfter teh.\n";
        assert_eq!(markdown_prose(doc), ["1. Run:", "After teh."]);
        // The same after a paragraph that's indented less than the item's text.
        let doc = "\
1. Step

  Explanation here.
  ```sh
cat <<EOF
hello wrld
EOF
  ```

Prose teh.
";
        assert_eq!(markdown_prose(doc), ["1. Step", "  Explanation here.", "Prose teh."]);
        // Its closing fence can't be indented 4 or more, although the guessed column is 2.
        let doc = "1. Step\n\n  ```\n  code\n     ```\n  still code\n  ```\n\nAfter teh.\n";
        assert_eq!(markdown_prose(doc), ["1. Step", "After teh."]);
        // But after a nested item, a fence indented by 4 or more is in an outer item, which has its
        // text 3 columns before the fence at most. So it closes at its own indentation, and a line
        // indented less than that column ends it.
        let doc = "\
- Defines a variable:

    - Unadorned: the history.

    ```console
    irb> foo
    ```

    Note teh output.
";
        assert_eq!(
            markdown_prose(doc),
            ["- Defines a variable:", "    - Unadorned: the history.", "    Note teh output."]
        );
        let doc = "- a\n  - b\n\n  para\n     ```\n     code\n     ```\n\nAfter teh.\n";
        assert_eq!(markdown_prose(doc), ["- a", "  - b", "  para", "After teh."]);
        let doc = "- a\n  - b\n    - c\n\n    ```sh\n    code\nAfter teh.\n";
        assert_eq!(markdown_prose(doc), ["- a", "  - b", "    - c", "After teh."]);

        // Not fences.
        let doc = "- ```code``` and text\n\nAfter.\n";
        assert_eq!(markdown_prose(doc), ["- ```code``` and text", "After."]);
        let doc = "-     ```\n  text\n\nAfter.\n";
        assert_eq!(markdown_prose(doc), ["-     ```", "  text", "After."]);
        let doc = "Since\n2024. ```\nThat was text.\n";
        assert_eq!(markdown_prose(doc), doc.lines().collect::<Vec<_>>());
    }

    #[test]
    fn test_markdown_list_continuation() {
        // Paragraphs in a list item after a blank line, indented by 4 or a tab (Python-Markdown,
        // MkDocs). They used to be indented code, and so did everything up to an unindented line.
        let doc = "\
- First item

    Continuation paragraph.

    - nested item

Back to top level.

1. Open the portal.

    Then select the subscription.

2. Click Save.

- item

\tTab continuation.
";
        assert_eq!(markdown_prose(doc), doc.lines().filter(|l| !l.is_empty()).collect::<Vec<_>>());

        // From a nested item back to the parent's text, and then the parent's indented code.
        let doc = "- a\n  - b\n\n  more a text\n\n    continuation\n\n      code in a\n";
        assert_eq!(markdown_prose(doc), ["- a", "  - b", "  more a text", "    continuation"]);
        let doc = "- a\n    - b\n\n    more a text\n\n        code in a\n";
        assert_eq!(markdown_prose(doc), ["- a", "    - b", "    more a text"]);

        // Indented code needs 4 columns more than the item's text.
        let doc = "- item\n\n      code\n\n1. item\n\n       code\n\n    1. item\n\nAfter.\n";
        assert_eq!(markdown_prose(doc), ["- item", "1. item", "    1. item", "After."]);
        let doc = "1. item\n\n    $ npm text\n";
        assert_eq!(markdown_prose(doc), ["1. item", "    $ npm text"]);
        let doc = "- item\n\n      code\n    more text\n";
        assert_eq!(markdown_prose(doc), ["- item", "    more text"]);

        // A line indented less than the item's text after a blank line ends the item.
        let doc = "- item\n\nParagraph.\n\n    code\n";
        assert_eq!(markdown_prose(doc), ["- item", "Paragraph."]);
        // So do headings and thematic breaks, even without one.
        let doc = "- item\n# Heading\n\n    code\n";
        assert_eq!(markdown_prose(doc), ["- item", "# Heading"]);
        let doc = "- item\n***\n\n    code\n";
        assert_eq!(markdown_prose(doc), ["- item", "***"]);
        // Lazy continuation lines don't.
        let doc = "- item\nlazy\n\n    more item\n";
        assert_eq!(markdown_prose(doc), ["- item", "lazy", "    more item"]);

        // Not list items: a thematic break, a marker that's indented 4 more than the item's text
        // (or than nothing), and a number in the middle of a paragraph.
        let doc = "* * *\n\n    code\n";
        assert_eq!(markdown_prose(doc), ["* * *"]);
        let doc = "Para\n    - text\n\n    code\n";
        assert_eq!(markdown_prose(doc), ["Para", "    - text"]);
        let doc = "Since\n2024. A year.\n\n    code\n";
        assert_eq!(markdown_prose(doc), ["Since", "2024. A year."]);
        let doc = "Steps:\n1. A step.\n\n    More step.\n";
        assert_eq!(markdown_prose(doc), doc.lines().filter(|l| !l.is_empty()).collect::<Vec<_>>());
    }

    #[test]
    fn test_markdown_front_matter() {
        // Obsidian, Jekyll, etc.
        let doc = "\
---
tags: [projectx, meeting]
aliases:
  - Some Alias
---
# Title
Body text.
";
        assert_eq!(markdown_prose(doc), ["# Title", "Body text."]);

        // Closed by `...`, trailing whitespace on either line, and empty.
        assert_eq!(markdown_prose("--- \ntitle: x\n... \t\nBody.\n"), ["Body."]);
        assert_eq!(markdown_prose("---\n---\nBody.\n"), ["Body."]);

        // `---` and a blank line is a thematic break, not front matter.
        // (The `---` itself is skipped either way. It has no words.)
        let doc = "---\n\nIntro text.\n\n---\n\nMore.\n";
        assert_eq!(markdown_prose(doc), ["Intro text.", "---", "More."]);

        // Front matter that's never closed ends at the first line that can't be YAML.
        let doc = "---\ntitle: x\n# comment\n  indented: y\n- item\nSome intro text.\n\nMore.\n";
        assert_eq!(markdown_prose(doc), ["Some intro text.", "More."]);

        // Only at the very start of the file, and only with exactly 3 dashes.
        let doc = "Intro.\n---\ntitle: x\n---\n";
        assert_eq!(markdown_prose(doc), doc.lines().collect::<Vec<_>>());
        let doc = "\n---\ntags: x\n---\n";
        assert_eq!(markdown_prose(doc), ["---", "tags: x", "---"]);
        let doc = " ---\ntags: x\n";
        assert_eq!(markdown_prose(doc), doc.lines().collect::<Vec<_>>());
        let doc = "----\ntags: x\n";
        assert_eq!(markdown_prose(doc), doc.lines().collect::<Vec<_>>());

        // Fences in front matter don't count. After it, they do.
        let doc = "---\nnote: |\n  ```\n---\nBody.\n```\ncode\n```\nAfter.\n";
        assert_eq!(markdown_prose(doc), ["Body.", "After."]);
    }

    #[test]
    fn test_markdown_state_never_panics() {
        // Lines made of the characters the state looks at, in every state it gets into.
        const CHARS: &[u8] = b"  \t\t--**++_01.)`~~#:a\r\xc3\xa9";
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut md = MarkdownState::default();
        let mut line = Vec::new();
        for i in 0..50_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            line.clear();
            line.extend(
                (0..seed % 12).map(|j| CHARS[(seed >> (5 * j + 4)) as usize % CHARS.len()]),
            );
            md.next_line(&line);
            if i % 500 == 0 {
                md = MarkdownState::default();
            }
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
    #[allow(clippy::single_range_in_vec_init)]
    fn test_highlighter_xml_comments() {
        let doc = "\
<!-- a coment -->
  <root><!-- teh note --><child/></root>
<!-- <b>x</b> -->
<![CDATA[Some teh text]]>
<!DOCTYPE html>
<!---->
<!--
multi line coment
-->
<!-- a --><!-- teh -->
";
        assert_eq!(
            check_doc(Some(language("xml")), doc),
            [
                // A comment's `<!--` is cut off, so that the tokenizer doesn't mask it like a tag.
                strs(&[" a coment -->"]),
                strs(&[" teh note -->"]),
                // Tags in it are still masked.
                strs(&[" <b>x</b> -->"]),
                // Neither CDATA nor a DOCTYPE is a comment.
                strs(&[]),
                strs(&[]),
                strs(&["-->"]),
                strs(&[]),
                strs(&["multi line coment"]),
                strs(&["-->"]),
                // Only the first one. The tokenizer masks the second.
                strs(&[" a --><!-- teh -->"]),
            ]
        );

        // Other scopes keep the `<!--`. In Markdown, the tokenizer masks single-line HTML comments,
        // so that `<!-- markdownlint-disable -->` and the like aren't flagged.
        let line = "<!-- markdownlint-disable -->";
        assert_eq!(check_doc(Some(language("markdown")), line), [strs(&[line])]);
        assert_eq!(check_doc(None, line), [strs(&[line])]);
        let hl = [span(0, HighlightKind::Comment), span(line.len(), HighlightKind::Other)];
        for scope in [Scope::Markdown, Scope::Code, Scope::CodeVerbatim] {
            assert_eq!(segments(scope, line, 0, &hl), [0..line.len()], "{scope:?}");
        }
        assert_eq!(segments(Scope::CommentsOnly, line, 0, &hl), [4..line.len()]);
        assert_eq!(segments(Scope::CommentsOnly, "<!--", 0, &hl[..1]), []);
    }

    #[test]
    fn test_highlighter_kinds() {
        use TextKind::{CodeString, Prose, SqlString, VerbatimString};

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

        // PowerShell's and SQL's are verbatim: `\` isn't an escape in them.
        let powershell = Some(language("powershell"));
        assert_eq!(
            kinds(powershell, r#"$p = "$env:TEMP\Logs`n" # note"#),
            owned(&[(r#""$env:TEMP\Logs`n""#, VerbatimString), ("# note", Prose)])
        );
        assert_eq!(
            kinds(powershell, r"Get-Item 'HKLM:\SOFTWARE' <# note #>"),
            owned(&[(r"'HKLM:\SOFTWARE'", VerbatimString), ("<# note #>", Prose)])
        );
        let sql = Some(language("sql"));
        assert_eq!(
            kinds(sql, r"SELECT N'C:\Data', 'it''s' -- note"),
            owned(&[(r"N'C:\Data'", SqlString), ("'it''s'", SqlString), ("-- note", Prose)])
        );
        assert_eq!(kinds(sql, "/* note */"), owned(&[("/* note */", Prose)]));
        // MySQL's quoted identifiers are string highlights, too. As SQL strings, the tokenizer
        // reads their backticks as code spans and masks them.
        assert_eq!(
            kinds(sql, "SELECT `Customers`, \"Orders\" FROM `my db`.`t` -- `note`"),
            owned(&[
                ("`Customers`", SqlString),
                ("\"Orders\"", SqlString),
                ("`my db`", SqlString),
                ("`t`", SqlString),
                ("-- `note`", Prose)
            ])
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
