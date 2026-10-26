// Licensed under the MIT License.
// Fork-only: reopens the last files (not part of upstream microsoft/edit).

//! File > Reopen Files at Startup: if `edit` is started without files, it reopens the files that
//! were open last time, with their cursor positions.
//!
//! While the option is on, the list is kept in `session.json` next to settings.json. It's written
//! when the open files change, at most once a second when only a cursor moved, and when exiting
//! starts. (Closing the terminal window kills `edit` without an exit, so waiting for one isn't
//! enough. And File > Exit closes the documents one by one, so the list must be saved before.)

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use std::{fs, io};

use edit::arena::scratch_arena;
use edit::helpers::{CoordType, Point};
use edit::json;

use crate::documents::DocumentManager;

const FILE_NAME: &str = "session.json";
/// How often a cursor move alone is written.
const CURSOR_SAVE_INTERVAL: Duration = Duration::from_secs(1);

static STORE: Mutex<Store> = Mutex::new(Store::new());

/// Remembers where `session.json` goes. `settings_path` is settings.json's path, empty if there's none.
pub fn load(settings_path: &Path) {
    lock().path =
        (!settings_path.as_os_str().is_empty()).then(|| settings_path.with_file_name(FILE_NAME));
}

/// Opens the files from the last session, if the option is on and `edit` was started without
/// arguments. Returns whether it opened any.
pub fn restore(documents: &mut DocumentManager) -> bool {
    if !crate::view_state::reopen_files() || std::env::args_os().len() > 1 {
        return false;
    }
    let Some(text) = lock().path.as_deref().and_then(|path| fs::read_to_string(path).ok()) else {
        return false;
    };

    let mut opened = false;
    for file in parse(&text) {
        // Files that were deleted or moved in the meantime are skipped.
        if !file.path.is_file() {
            continue;
        }
        if let Ok(doc) = documents.add_file_path(&file.path) {
            doc.cursor_move_to_goto(Point { x: file.column, y: file.line });
            opened = true;
        }
    }
    opened
}

/// Saves the open files if they changed. Call it with `exiting` before the first document
/// gets closed for exiting: that saves right away, and ignores the dwindling list after it,
/// until a call without `exiting` (the exit was canceled).
pub fn update(documents: &DocumentManager, exiting: bool) {
    if !crate::view_state::reopen_files() {
        return;
    }
    lock().sync(snapshot(documents), Instant::now(), exiting);
}

/// Turns the option on (saving the open files right away) or off (forgetting them).
pub fn set_enabled(documents: &DocumentManager, enabled: bool) {
    crate::view_state::set_reopen_files(enabled);
    let mut store = lock();
    if enabled {
        store.update(snapshot(documents), Instant::now(), true);
    } else {
        store.forget();
    }
}

fn lock() -> MutexGuard<'static, Store> {
    STORE.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct SessionFile {
    path: PathBuf,
    /// 1-based, like `edit --goto FILE:LINE:CHARACTER`.
    line: CoordType,
    column: CoordType,
}

/// The documents that have a path, oldest first. The last one was the active one.
fn snapshot(documents: &DocumentManager) -> Vec<SessionFile> {
    documents
        .iter()
        .filter_map(|doc| {
            let path = doc.path.clone()?;
            let pos = doc.buffer.borrow().cursor_logical_pos();
            Some(SessionFile { path, line: pos.y + 1, column: pos.x + 1 })
        })
        .collect()
}

struct Store {
    /// `None` if there's no settings directory. Then nothing is remembered.
    path: Option<PathBuf>,
    saved: Vec<SessionFile>,
    saved_at: Option<Instant>,
    /// Whether exiting has started, see [`update`].
    exiting: bool,
}

impl Store {
    const fn new() -> Self {
        Self { path: None, saved: Vec::new(), saved_at: None, exiting: false }
    }

    /// See [`update`].
    fn sync(&mut self, files: Vec<SessionFile>, now: Instant, exiting: bool) {
        if exiting {
            if !self.exiting {
                self.update(files, now, true);
                self.exiting = true;
            }
        } else {
            self.exiting = false;
            self.update(files, now, false);
        }
    }

    fn update(&mut self, files: Vec<SessionFile>, now: Instant, exiting: bool) {
        if files == self.saved && self.saved_at.is_some() {
            return;
        }
        let same_files = files.len() == self.saved.len()
            && files.iter().zip(&self.saved).all(|(a, b)| a.path == b.path);
        let recently =
            self.saved_at.is_some_and(|at| now.duration_since(at) < CURSOR_SAVE_INTERVAL);
        if same_files && recently && !exiting {
            return;
        }
        if let Some(path) = &self.path {
            // Losing the list isn't worth an error message.
            _ = save(path, &files);
        }
        self.saved = files;
        self.saved_at = Some(now);
    }

    fn forget(&mut self) {
        if let Some(path) = &self.path {
            _ = fs::remove_file(path);
        }
        self.saved.clear();
        self.saved_at = None;
    }
}

fn parse(text: &str) -> Vec<SessionFile> {
    let scratch = scratch_arena(None);
    let Some(files) = json::parse(&scratch, text)
        .ok()
        .and_then(|v| v.as_object())
        .and_then(|root| root.get_array("files"))
    else {
        return Vec::new();
    };
    let position =
        |n: Option<f64>| n.filter(|n| n.is_finite() && *n >= 1.0).map_or(1, |n| n as CoordType);
    files
        .iter()
        .filter_map(|file| {
            let file = file.as_object()?;
            Some(SessionFile {
                path: PathBuf::from(file.get_str("path")?),
                line: position(file.get_number("line")),
                column: position(file.get_number("column")),
            })
        })
        .collect()
}

fn serialize(files: &[SessionFile]) -> String {
    let mut out = String::from("{\n    \"files\": [");
    let mut first = true;
    for file in files {
        // JSON can only hold UTF-8 paths. Others aren't remembered.
        let Some(path) = file.path.to_str() else {
            continue;
        };
        out.push_str(if first { "\n" } else { ",\n" });
        first = false;
        out.push_str("        { \"path\": ");
        push_json_string(&mut out, path);
        _ = write!(out, ", \"line\": {}, \"column\": {} }}", file.line, file.column);
    }
    out.push_str(if first { "]\n}\n" } else { "\n    ]\n}\n" });
    out
}

fn push_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => _ = write!(out, "\\u{:04x}", c as u32),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn save(path: &Path, files: &[SessionFile]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    // Write a temporary file and rename it over the old one, so that it's never half written.
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serialize(files))?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, line: CoordType, column: CoordType) -> SessionFile {
        SessionFile { path: PathBuf::from(path), line, column }
    }

    /// A fresh directory under the system's temp directory, removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("edit-session-{}-{name}", std::process::id()));
            _ = fs::remove_dir_all(&dir);
            Self(dir)
        }

        fn store(&self) -> Store {
            let mut store = Store::new();
            store.path = Some(self.0.join("config").join(FILE_NAME));
            store
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn test_serialize_and_parse() {
        assert_eq!(serialize(&[]), "{\n    \"files\": []\n}\n");
        let files = [file("/notes/a.md", 3, 7), file("/notes/b \"q\" \\ é\t.txt", 1, 1)];
        let text = serialize(&files);
        assert_eq!(
            text,
            "{\n    \"files\": [\n        \
             { \"path\": \"/notes/a.md\", \"line\": 3, \"column\": 7 },\n        \
             { \"path\": \"/notes/b \\\"q\\\" \\\\ é\\u0009.txt\", \"line\": 1, \"column\": 1 }\n    \
             ]\n}\n"
        );
        assert_eq!(parse(&text), files);
    }

    #[test]
    fn test_parse_ignores_junk() {
        for text in ["", "{", "[]", r#"{ "files": 1 }"#, r#"{ "files": [1, "a", {}] }"#] {
            assert_eq!(parse(text), [], "{text:?}");
        }
        // Missing or invalid positions start at the top.
        let text =
            r#"{ "files": [{ "path": "/a" }, { "path": "/b", "line": -4, "column": 2.5 }] }"#;
        assert_eq!(parse(text), [file("/a", 1, 1), file("/b", 1, 2)]);
    }

    // A different list is written at once, a cursor move at most once a second, or when exiting.
    #[test]
    fn test_update_throttles_cursor_moves() {
        let dir = TempDir::new("throttle");
        let mut store = dir.store();
        let path = store.path.clone().unwrap();
        let read = || parse(&fs::read_to_string(&path).unwrap());
        let t0 = Instant::now();

        store.update(vec![file("/a", 1, 1)], t0, false);
        assert_eq!(read(), [file("/a", 1, 1)]);
        store.update(vec![file("/a", 1, 1), file("/b", 2, 2)], t0, false);
        assert_eq!(read(), [file("/a", 1, 1), file("/b", 2, 2)]);

        store.update(
            vec![file("/a", 1, 1), file("/b", 5, 1)],
            t0 + Duration::from_millis(300),
            false,
        );
        assert_eq!(read(), [file("/a", 1, 1), file("/b", 2, 2)]);
        store.update(
            vec![file("/a", 1, 1), file("/b", 6, 1)],
            t0 + Duration::from_millis(1100),
            false,
        );
        assert_eq!(read(), [file("/a", 1, 1), file("/b", 6, 1)]);
        store.update(
            vec![file("/a", 1, 1), file("/b", 7, 1)],
            t0 + Duration::from_millis(1200),
            true,
        );
        assert_eq!(read(), [file("/a", 1, 1), file("/b", 7, 1)]);

        store.forget();
        assert!(!path.exists());
        assert!(!path.with_extension("json.tmp").exists());
    }

    // File > Exit closes the documents one by one. The list from before that is what's kept,
    // unless the exit gets canceled (e.g. at an unsaved document).
    #[test]
    fn test_exiting_keeps_the_full_list() {
        let dir = TempDir::new("exiting");
        let mut store = dir.store();
        let path = store.path.clone().unwrap();
        let read = || parse(&fs::read_to_string(&path).unwrap());
        let t0 = Instant::now();
        let both = vec![file("/a", 1, 1), file("/b", 4, 2)];

        store.sync(vec![file("/a", 1, 1), file("/b", 1, 1)], t0, false);
        // The cursor move is saved right away when exiting starts.
        store.sync(both.clone(), t0, true);
        assert_eq!(read(), both);
        store.sync(vec![file("/a", 1, 1)], t0, true);
        store.sync(vec![], t0, true);
        assert_eq!(read(), both);

        // Canceled: back to saving what's open.
        store.sync(vec![file("/a", 1, 1)], t0, false);
        assert_eq!(read(), [file("/a", 1, 1)]);
    }
}
