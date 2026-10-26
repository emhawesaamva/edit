// Licensed under the MIT License.
// Fork-only: spellcheck (not part of upstream microsoft/edit).

//! Tests for the textarea's right-click handling and the accessors the spelling popup uses.
//!
//! They drive a headless `Tui` with raw terminal input, the same way `main.rs` does.

use super::*;
use crate::vt;

const SIZE: Size = Size { width: 40, height: 10 };
/// The textarea leaves 2 rows below it, like the editor's statusbar.
const TEXTAREA_HEIGHT: CoordType = SIZE.height - 2;

// SGR mouse button codes.
const LEFT: u16 = 0;
const RIGHT: u16 = 2;
const MOTION: u16 = 32;
const WHEEL_DOWN: u16 = 65;

fn press(button: u16, x: CoordType, y: CoordType) -> String {
    format!("\x1b[<{button};{};{}M", x + 1, y + 1)
}

fn release(button: u16, x: CoordType, y: CoordType) -> String {
    format!("\x1b[<{button};{};{}m", x + 1, y + 1)
}

fn click(button: u16, x: CoordType, y: CoordType) -> String {
    press(button, x, y) + &release(button, x, y)
}

enum Widget {
    Textarea(RcTextBuffer),
    Editline(String),
    /// A textarea with an editline in the row below it, like the editor's Find bar.
    TextareaAndEditline(RcTextBuffer, String),
}

/// What the accessors returned during the frames of one [`Harness::feed`] call.
#[derive(Default)]
struct Report {
    /// Number of frames in which `textarea_right_clicked()` was true.
    right_clicks: usize,
    /// `textarea_scroll_offset()` of each frame.
    scroll_offsets: Vec<Option<Point>>,
    /// The same two, but queried after a non-textarea node was added.
    other_right_clicks: usize,
    other_scroll_offsets: Vec<Option<Point>>,
}

struct Harness {
    tui: Tui,
    vt: vt::Parser,
    input: input::Parser,
    widget: Widget,
}

impl Harness {
    fn textarea(text: &str, margin: bool) -> Self {
        Self::new(Widget::Textarea(Self::text_buffer(text, margin)))
    }

    fn editline(text: &str) -> Self {
        Self::new(Widget::Editline(text.to_string()))
    }

    /// The textarea has the focus initially.
    fn textarea_and_editline(text: &str, line: &str) -> Self {
        let tb = Self::text_buffer(text, false);
        Self::new(Widget::TextareaAndEditline(tb, line.to_string()))
    }

    fn text_buffer(text: &str, margin: bool) -> RcTextBuffer {
        let tb = TextBuffer::new_rc(false).unwrap();
        {
            let mut tb = tb.borrow_mut();
            tb.set_margin_enabled(margin);
            tb.write_raw(text.as_bytes());
            tb.cursor_move_to_logical(Point::default());
        }
        tb
    }

    fn new(widget: Widget) -> Self {
        let mut tui = Tui::new().unwrap();
        tui.set_size(SIZE);
        let mut h = Self { tui, vt: vt::Parser::new(), input: input::Parser::new(), widget };
        // The first frames create the node, focus it and lay it out.
        h.feed("");
        assert_eq!(h.tui.focused_node_path.len(), 2, "the widget should be focused");
        h
    }

    fn frame(tui: &mut Tui, widget: &mut Widget, input: Option<Input>, report: &mut Report) {
        let mut ctx = tui.create_context(input);

        match widget {
            Widget::Textarea(tb) | Widget::TextareaAndEditline(tb, _) => {
                ctx.textarea("textarea", tb.clone())
            }
            Widget::Editline(text) => _ = ctx.editline("editline", text),
        }
        ctx.inherit_focus();
        ctx.attr_intrinsic_size(Size { width: 0, height: TEXTAREA_HEIGHT });
        report.right_clicks += ctx.textarea_right_clicked() as usize;
        report.scroll_offsets.push(ctx.textarea_scroll_offset());

        if let Widget::TextareaAndEditline(_, text) = widget {
            ctx.editline("editline", text);
            ctx.inherit_focus();
        }

        ctx.block_begin("other");
        ctx.block_end();
        report.other_right_clicks += ctx.textarea_right_clicked() as usize;
        report.other_scroll_offsets.push(ctx.textarea_scroll_offset());
    }

    /// Processes one batch of terminal input like `main.rs`: one frame per input event,
    /// a trailing frame without input, more frames until the layout settled, then a render.
    /// An empty `text` is what happens when the input read times out.
    fn feed(&mut self, text: &str) -> Report {
        let mut report = Report::default();
        let Self { tui, vt, input, widget } = self;

        for event in input.parse(vt.parse(text)) {
            Self::frame(tui, widget, Some(event), &mut report);
        }
        Self::frame(tui, widget, None, &mut report);
        while tui.needs_settling() {
            Self::frame(tui, widget, None, &mut report);
        }

        let scratch = scratch_arena(None);
        _ = tui.render(&scratch);
        report
    }

    fn buffer(&self) -> &TextBufferCell {
        &self.tui.cached_text_buffers.first().unwrap().editor
    }

    fn cursor(&self) -> Point {
        self.buffer().borrow().cursor_logical_pos()
    }

    /// The classname of the focused node in the tree that was rendered last.
    fn focused(&self) -> &'static str {
        let id = *self.tui.focused_node_path.last().unwrap();
        self.tui.prev_node_map.get(id).map_or("", |node| node.borrow().classname)
    }

    /// The textarea's scroll offset in the tree that was rendered last.
    fn rendered_scroll_offset(&self) -> Point {
        let mut offset = None;
        for root in self.tui.prev_tree.iterate_roots() {
            Tree::visit_all(root, root, true, |node| {
                if let NodeContent::Textarea(tc) = &node.borrow().content {
                    offset = Some(tc.scroll_offset);
                }
                VisitControl::Continue
            });
        }
        offset.unwrap()
    }
}

fn numbered_lines(count: usize) -> String {
    (0..count).map(|i| format!("line {i}\n")).collect()
}

#[test]
fn test_right_click_moves_cursor_once() {
    let mut h = Harness::textarea("hello world\nsecond line\n", false);

    let r = h.feed(&press(RIGHT, 8, 1));
    assert_eq!(r.right_clicks, 1);
    assert_eq!(h.cursor(), Point { x: 8, y: 1 });

    let r = h.feed(&release(RIGHT, 8, 1));
    assert_eq!(r.right_clicks, 0);
    assert_eq!(h.cursor(), Point { x: 8, y: 1 });

    // The preferred column was updated as well: Up keeps the column.
    h.feed("\x1b[A");
    assert_eq!(h.cursor(), Point { x: 8, y: 0 });

    // Every new press fires again, even within a single input batch.
    let r = h.feed(&(click(RIGHT, 2, 1) + &click(RIGHT, 4, 0)));
    assert_eq!(r.right_clicks, 2);
    assert_eq!(h.cursor(), Point { x: 4, y: 0 });

    // Clicks beyond the end of a line clamp to it, just like left-clicks.
    let r = h.feed(&click(RIGHT, 30, 1));
    assert_eq!(r.right_clicks, 1);
    assert_eq!(h.cursor(), Point { x: 11, y: 1 });
}

#[test]
fn test_right_click_held_does_not_refire() {
    let mut h = Harness::textarea("hello world\nsecond line\n", false);

    let r = h.feed(&press(RIGHT, 2, 0));
    assert_eq!(r.right_clicks, 1);
    assert_eq!(h.cursor(), Point { x: 2, y: 0 });

    // `InputMouseState::Right` carries over between frames, but neither frames
    // without input (read timeouts), nor moving the mouse with the button held re-fire.
    let r = h.feed("");
    assert_eq!(r.right_clicks, 0);
    let r = h.feed(&press(RIGHT | MOTION, 7, 1));
    assert_eq!(r.right_clicks, 0);
    let r = h.feed(&release(RIGHT, 7, 1));
    assert_eq!(r.right_clicks, 0);
    assert_eq!(h.cursor(), Point { x: 2, y: 0 });
    assert!(!h.buffer().borrow().has_selection());
}

#[test]
fn test_left_click_never_sets_right_clicked() {
    let mut h = Harness::textarea("hello world\nsecond line\n", false);

    let r = h.feed(&click(LEFT, 4, 1));
    assert_eq!(r.right_clicks, 0);
    assert_eq!(h.cursor(), Point { x: 4, y: 1 });

    // Double-click selects the word.
    let r = h.feed(&(click(LEFT, 1, 0) + &click(LEFT, 1, 0)));
    assert_eq!(r.right_clicks, 0);
    assert!(h.buffer().borrow().has_selection());

    // Drag-select.
    let r = h.feed(&(press(LEFT, 0, 1) + &press(LEFT | MOTION, 6, 1) + &release(LEFT, 6, 1)));
    assert_eq!(r.right_clicks, 0);
    assert!(h.buffer().borrow().has_selection());

    // A right-click right after a left-click on the same spot is not a double-click.
    let r = h.feed(&(click(LEFT, 3, 0) + &click(RIGHT, 3, 0)));
    assert_eq!(r.right_clicks, 1);
    assert_eq!(h.cursor(), Point { x: 3, y: 0 });
    assert!(!h.buffer().borrow().has_selection());
}

#[test]
fn test_right_click_outside_text_is_ignored() {
    let mut h = Harness::textarea("hello world\nsecond line\n", true);
    let margin = h.buffer().borrow().margin_width();
    assert!(margin > 0);

    // Line number margin.
    let r = h.feed(&click(RIGHT, 0, 1));
    assert_eq!(r.right_clicks, 0);
    assert_eq!(h.cursor(), Point { x: 0, y: 0 });

    // Scrollbar.
    let r = h.feed(&click(RIGHT, SIZE.width - 1, 1));
    assert_eq!(r.right_clicks, 0);
    assert_eq!(h.cursor(), Point { x: 0, y: 0 });

    // Below the textarea.
    let r = h.feed(&click(RIGHT, margin + 2, TEXTAREA_HEIGHT));
    assert_eq!(r.right_clicks, 0);
    assert_eq!(h.cursor(), Point { x: 0, y: 0 });

    // The text itself still works and accounts for the margin.
    let r = h.feed(&click(RIGHT, margin + 2, 1));
    assert_eq!(r.right_clicks, 1);
    assert_eq!(h.cursor(), Point { x: 2, y: 1 });
}

#[test]
fn test_right_click_ignored_by_editline() {
    let mut h = Harness::editline("hello world");
    assert_eq!(h.cursor(), Point { x: 11, y: 0 });

    let r = h.feed(&click(RIGHT, 3, 0));
    assert_eq!(r.right_clicks, 0);
    assert_eq!(h.cursor(), Point { x: 11, y: 0 });

    // Whereas a left-click does reach it.
    h.feed(&click(LEFT, 3, 0));
    assert_eq!(h.cursor(), Point { x: 3, y: 0 });
}

#[test]
fn test_textarea_scroll_offset() {
    let mut h = Harness::textarea(&numbered_lines(50), false);

    let r = h.feed("");
    assert!(r.scroll_offsets.iter().all(|&s| s == Some(Point { x: 0, y: 0 })));

    // The wheel is applied in the frame that receives it.
    let r = h.feed(&press(WHEEL_DOWN, 5, 2));
    assert_eq!(r.scroll_offsets.first(), Some(&Some(Point { x: 0, y: 3 })));
    assert_eq!(r.scroll_offsets.last(), Some(&Some(h.rendered_scroll_offset())));
    assert_eq!(h.rendered_scroll_offset(), Point { x: 0, y: 3 });

    // Right-clicks account for the scroll offset.
    let r = h.feed(&click(RIGHT, 2, 1));
    assert_eq!(r.right_clicks, 1);
    assert_eq!(h.cursor(), Point { x: 2, y: 4 });
}

#[test]
fn test_accessors_ignore_other_nodes() {
    let mut h = Harness::textarea("hello world\nsecond line\n", false);

    let r = h.feed(&click(RIGHT, 2, 1));
    assert_eq!(r.right_clicks, 1);
    assert!(r.scroll_offsets.iter().all(Option::is_some));
    assert_eq!(r.other_right_clicks, 0);
    assert!(r.other_scroll_offsets.iter().all(Option::is_none));
}

#[test]
fn test_right_click_focuses_textarea() {
    let mut h = Harness::textarea_and_editline("hello world\nsecond line\n", "needle");
    assert_eq!(h.focused(), "textarea");

    // Focus the editline, like clicking into the Find bar.
    h.feed(&click(LEFT, 2, TEXTAREA_HEIGHT));
    assert_eq!(h.focused(), "editline");

    // A right-click on the text still reaches the textarea, and moves the focus to it.
    let r = h.feed(&click(RIGHT, 8, 1));
    assert_eq!(r.right_clicks, 1);
    assert_eq!(h.cursor(), Point { x: 8, y: 1 });
    assert_eq!(h.focused(), "textarea");

    // Editlines ignore right-clicks, so they don't take the focus with one either.
    let r = h.feed(&click(RIGHT, 2, TEXTAREA_HEIGHT));
    assert_eq!(r.right_clicks, 0);
    assert_eq!(h.focused(), "textarea");
    assert_eq!(h.cursor(), Point { x: 8, y: 1 });
}

#[test]
fn test_right_drag_never_selects() {
    let mut h = Harness::textarea_and_editline("hello world\nsecond line\n", "needle");
    h.feed(&click(LEFT, 2, TEXTAREA_HEIGHT));
    assert_eq!(h.focused(), "editline");

    // Only left-button motion is a drag in `Tui::create_context`, so the press that moved
    // the focus doesn't start a selection either. It's a single right-click.
    let drag = press(RIGHT, 2, 0)
        + &press(RIGHT | MOTION, 5, 0)
        + &press(RIGHT | MOTION, 7, 1)
        + &release(RIGHT, 7, 1);
    let r = h.feed(&drag);
    assert_eq!(r.right_clicks, 1);
    assert_eq!(h.focused(), "textarea");
    assert_eq!(h.cursor(), Point { x: 2, y: 0 });
    assert!(!h.buffer().borrow().has_selection());

    // A left-drag right after it still selects.
    let drag = press(LEFT, 0, 1) + &press(LEFT | MOTION, 6, 1) + &release(LEFT, 6, 1);
    let r = h.feed(&drag);
    assert_eq!(r.right_clicks, 0);
    assert!(h.buffer().borrow().has_selection());
}
