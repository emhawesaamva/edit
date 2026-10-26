// Licensed under the MIT License.
// Fork-only: tests a fork fix in upstream code (not part of upstream microsoft/edit).

//! Replace All when the last match ends at the end of the file.
//!
//! Upstream's `TextBuffer::find_and_replace_all()` resumed the search at the end of the text,
//! and ICU then searched again from the start. So if the replacement matched again
//! (same text, another case, or containing the pattern), it never returned.
//! Each test runs on a helper thread with a timeout, so that a regression fails instead of hanging.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use edit::buffer::{SearchOptions, TextBuffer};

const TIMEOUT: Duration = Duration::from_secs(5);

/// Replaces all `pattern` with `replacement` in a new buffer containing `text`, and returns the
/// result. `final_newline` is the "insert final newline" setting, which is only set after
/// writing `text`, so that `text` is stored as-is, like a file opened without a final newline.
fn replace_all(
    text: &'static str,
    final_newline: bool,
    pattern: &'static str,
    replacement: &'static str,
) -> String {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut tb = TextBuffer::new(false).unwrap();
        tb.set_crlf(false);
        tb.write_raw(text.as_bytes());
        tb.set_insert_final_newline(final_newline);
        tb.find_and_replace_all(pattern, SearchOptions::default(), replacement.as_bytes()).unwrap();
        _ = tx.send(contents(&tb));
    });

    match rx.recv_timeout(TIMEOUT) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => panic!(
            "Replace All {pattern:?} -> {replacement:?} in {text:?} didn't return within {TIMEOUT:?}"
        ),
        Err(RecvTimeoutError::Disconnected) => panic!("Replace All panicked"),
    }
}

fn contents(tb: &TextBuffer) -> String {
    let mut text = Vec::new();
    loop {
        let chunk = tb.read_forward(text.len());
        if chunk.is_empty() {
            break;
        }
        text.extend_from_slice(chunk);
    }
    String::from_utf8(text).unwrap()
}

// The search ignores case by default, so "Foo" matches "foo" again.
#[test]
fn test_case_change_at_eof() {
    assert_eq!(replace_all("x foo", false, "foo", "Foo"), "x Foo");
    assert_eq!(replace_all("foo foo", false, "foo", "Foo"), "Foo Foo");
}

#[test]
fn test_replacement_contains_pattern_at_eof() {
    assert_eq!(replace_all("x a", false, "a", "aa"), "x aa");
}

#[test]
fn test_replacement_equals_pattern_at_eof() {
    assert_eq!(replace_all("x a", false, "a", "a"), "x a");
}

// Inserting the final newline moved the search to the end of the text, so the same match
// was replaced again ("x aaaa\n").
#[test]
fn test_final_newline_after_match_at_eof() {
    assert_eq!(replace_all("x a", true, "a", "aa"), "x aa\n");
}

// These already worked: the last match is followed by a newline, or the replacement doesn't match.
#[test]
fn test_unaffected() {
    assert_eq!(replace_all("foo x\nfoo y\n", false, "foo", "Foo"), "Foo x\nFoo y\n");
    assert_eq!(replace_all("x foo", false, "foo", "bar"), "x bar");
}
