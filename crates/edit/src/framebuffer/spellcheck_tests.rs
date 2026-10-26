// Licensed under the MIT License.
// Fork-only: spellcheck (not part of upstream microsoft/edit).

//! Tests for the SGR output of `Framebuffer::render`, in particular the curly underline.

use super::*;

/// Start of a full redraw: SGR reset and CUP to the first row.
const START: &str = "\x1b[m\x1b[1;1H";
/// `DEFAULT_THEME`'s background and foreground colors.
const COLORS: &str = "\x1b[48;2;0;0;0m\x1b[38;2;190;190;190m";
/// Curly underline in `DEFAULT_THEME`'s BrightRed (#ff3e30).
const CURLY: &str = "\x1b[4:3m\x1b[58:2::255:62:48m";
const CURLY_TO_NONE: &str = "\x1b[59m\x1b[24m";
const CURLY_TO_SINGLE: &str = "\x1b[59m\x1b[4m";
const SINGLE: &str = "\x1b[4m";
const SINGLE_TO_NONE: &str = "\x1b[24m";
const HIDE_CURSOR: &str = "\x1b[?25l";

/// Begins a new frame with one row per entry in `rows`, all of the same width.
fn draw(fb: &mut Framebuffer, rows: &[&str]) {
    let width = rows[0].len() as CoordType;
    fb.flip(Size { width, height: rows.len() as CoordType });
    for (y, text) in rows.iter().enumerate() {
        fb.replace_text(y as CoordType, 0, width, *text);
    }
}

/// The cells `left..right` in row `y`.
fn cells(y: CoordType, left: CoordType, right: CoordType) -> Rect {
    Rect { left, top: y, right, bottom: y + 1 }
}

/// Sets the attributes of the cells `left..right` in row `y`, clearing all others.
fn set_attr(
    fb: &mut Framebuffer,
    y: CoordType,
    left: CoordType,
    right: CoordType,
    attr: Attributes,
) {
    fb.replace_attr(cells(y, left, right), Attributes::All, attr);
}

fn render(fb: &mut Framebuffer) -> String {
    let scratch = scratch_arena(None);
    fb.render(&scratch).as_str().to_owned()
}

#[test]
fn test_render_attributes_regression() {
    let mut fb = Framebuffer::new();
    draw(&mut fb, &["0123456789"]);
    set_attr(&mut fb, 0, 0, 2, Attributes::Bold);
    set_attr(&mut fb, 0, 2, 4, Attributes::Bold | Attributes::Italic);
    set_attr(&mut fb, 0, 4, 6, Attributes::Underlined);
    set_attr(&mut fb, 0, 6, 8, Attributes::Underlined | Attributes::Strikethrough);

    let expected = [
        START,
        COLORS,
        "\x1b[1m",
        "01",
        "\x1b[3m",
        "23",
        "\x1b[22m\x1b[23m\x1b[4m",
        "45",
        "\x1b[9m",
        "67",
        "\x1b[24m\x1b[29m",
        "89",
        HIDE_CURSOR,
    ]
    .concat();
    assert_eq!(render(&mut fb), expected);
}

#[test]
fn test_render_underline_none_single_none() {
    let mut fb = Framebuffer::new();
    draw(&mut fb, &["abcdef"]);
    set_attr(&mut fb, 0, 2, 4, Attributes::Underlined);

    let expected = [START, COLORS, "ab", SINGLE, "cd", SINGLE_TO_NONE, "ef", HIDE_CURSOR].concat();
    assert_eq!(render(&mut fb), expected);
}

#[test]
fn test_render_undercurl_none_curly_none() {
    let mut fb = Framebuffer::new();
    draw(&mut fb, &["abcdef"]);
    set_attr(&mut fb, 0, 2, 4, Attributes::Undercurl);

    let expected = [START, COLORS, "ab", CURLY, "cd", CURLY_TO_NONE, "ef", HIDE_CURSOR].concat();
    assert_eq!(render(&mut fb), expected);
}

#[test]
fn test_render_undercurl_single_transitions() {
    let mut fb = Framebuffer::new();
    draw(&mut fb, &["abcdefghijkl"]);
    set_attr(&mut fb, 0, 2, 4, Attributes::Underlined);
    set_attr(&mut fb, 0, 4, 6, Attributes::Undercurl);
    set_attr(&mut fb, 0, 6, 8, Attributes::Underlined);
    set_attr(&mut fb, 0, 8, 10, Attributes::Undercurl);

    let expected = [
        START,
        COLORS,
        "ab",
        SINGLE,
        "cd",
        CURLY,
        "ef",
        CURLY_TO_SINGLE,
        "gh",
        CURLY,
        "ij",
        CURLY_TO_NONE,
        "kl",
        HIDE_CURSOR,
    ]
    .concat();
    assert_eq!(render(&mut fb), expected);
}

#[test]
fn test_render_undercurl_wins_over_single() {
    let mut fb = Framebuffer::new();
    draw(&mut fb, &["abcdefghijkl"]);
    set_attr(&mut fb, 0, 2, 4, Attributes::Underlined | Attributes::Undercurl);
    set_attr(&mut fb, 0, 4, 6, Attributes::Undercurl);
    set_attr(&mut fb, 0, 6, 8, Attributes::Underlined);
    set_attr(&mut fb, 0, 8, 10, Attributes::Underlined | Attributes::Undercurl);

    // Both bits set is curly, so "cd" -> "ef" emits nothing and "gh" -> "ij" emits curly.
    let expected = [
        START,
        COLORS,
        "ab",
        CURLY,
        "cdef",
        CURLY_TO_SINGLE,
        "gh",
        CURLY,
        "ij",
        CURLY_TO_NONE,
        "kl",
        HIDE_CURSOR,
    ]
    .concat();
    assert_eq!(render(&mut fb), expected);
}

#[test]
fn test_render_undercurl_with_bold() {
    let mut fb = Framebuffer::new();
    draw(&mut fb, &["abcdefghijkl"]);
    // Bold first, then Undercurl through its own mask, the way the spellcheck pass applies it.
    set_attr(&mut fb, 0, 2, 4, Attributes::Bold);
    set_attr(&mut fb, 0, 6, 10, Attributes::Bold);
    fb.replace_attr(cells(0, 2, 8), Attributes::Undercurl, Attributes::Undercurl);

    // Bold toggles on its own and never disturbs the curly underline, and vice versa.
    let expected = [
        START,
        COLORS,
        "ab",
        "\x1b[1m",
        CURLY,
        "cd",
        "\x1b[22m",
        "ef",
        "\x1b[1m",
        "gh",
        CURLY_TO_NONE,
        "ij",
        "\x1b[22m",
        "kl",
        HIDE_CURSOR,
    ]
    .concat();
    assert_eq!(render(&mut fb), expected);
}

#[test]
fn test_render_undercurl_across_rows() {
    let mut fb = Framebuffer::new();
    draw(&mut fb, &["abcd", "efgh"]);
    set_attr(&mut fb, 0, 2, 4, Attributes::Undercurl);
    set_attr(&mut fb, 1, 0, 2, Attributes::Undercurl);

    // The run continues into the next row, so the CUP isn't followed by a repeated SGR.
    let expected =
        [START, COLORS, "ab", CURLY, "cd", "\x1b[2;1H", "ef", CURLY_TO_NONE, "gh", HIDE_CURSOR]
            .concat();
    assert_eq!(render(&mut fb), expected);

    // Next frame: the first row is unchanged and skipped. Every render starts with
    // an SGR reset, so the second row's curly underline is emitted from scratch.
    draw(&mut fb, &["abcd", "efgh"]);
    set_attr(&mut fb, 0, 2, 4, Attributes::Undercurl);
    set_attr(&mut fb, 1, 2, 4, Attributes::Undercurl);

    let expected = ["\x1b[m\x1b[2;1H", COLORS, "ef", CURLY, "gh", HIDE_CURSOR].concat();
    assert_eq!(render(&mut fb), expected);

    // Next frame: the last frame ended inside a curly run, but the reset ends it.
    draw(&mut fb, &["abcd", "efgh"]);
    set_attr(&mut fb, 0, 2, 4, Attributes::Undercurl);

    let expected = ["\x1b[m\x1b[2;1H", COLORS, "efgh", HIDE_CURSOR].concat();
    assert_eq!(render(&mut fb), expected);
}

#[test]
fn test_render_undercurl_cleared_like_popups() {
    assert!(Attributes::All.is(Attributes::Undercurl));

    let mut fb = Framebuffer::new();
    draw(&mut fb, &["abcdefgh"]);
    fb.replace_attr(cells(0, 0, 8), Attributes::Undercurl, Attributes::Undercurl);
    // Same call that tui.rs makes for the area of a popup.
    fb.replace_attr(cells(0, 2, 6), Attributes::All, Attributes::None);

    let back = &fb.buffers[fb.frame_counter & 1];
    assert!(back.attributes.data[2..6].iter().all(|&a| a == Attributes::None));

    let expected =
        [START, COLORS, CURLY, "ab", CURLY_TO_NONE, "cdef", CURLY, "gh", HIDE_CURSOR].concat();
    assert_eq!(render(&mut fb), expected);
}

#[test]
fn test_render_undercurl_color_from_palette() {
    let mut fb = Framebuffer::new();
    draw(&mut fb, &["abcdef"]);
    set_attr(&mut fb, 0, 2, 4, Attributes::Undercurl);

    // The color is read when rendering, not when the attribute is set.
    let mut colors = DEFAULT_THEME;
    colors[IndexedColor::BrightRed as usize] = StraightRgba::from_rgba(0x123456ff);
    fb.set_indexed_colors(colors);

    let expected = [
        START,
        COLORS,
        "ab",
        "\x1b[4:3m\x1b[58:2::18:52:86m",
        "cd",
        CURLY_TO_NONE,
        "ef",
        HIDE_CURSOR,
    ]
    .concat();
    assert_eq!(render(&mut fb), expected);
}
