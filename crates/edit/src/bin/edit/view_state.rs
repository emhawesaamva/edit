// Licensed under the MIT License.
// Fork-only: remembers View menu choices (not part of upstream microsoft/edit).

//! Remembers the View menu's Word Wrap and Spell Check choices between sessions.
//!
//! They're kept in `view-state.json` next to settings.json, which `edit` rewrites whenever one of
//! them is toggled. settings.json itself is never written, because it may hold the user's comments.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::{fs, io};

use edit::arena::scratch_arena;
use edit::json;

const FILE_NAME: &str = "view-state.json";

static STORE: Mutex<Store> = Mutex::new(Store::new());

/// Reads the remembered choices. `settings_path` is settings.json's path, empty if there's none.
pub fn load(settings_path: &Path) {
    lock().load(settings_path);
}

/// Whether new documents start with word wrap on.
pub fn word_wrap() -> bool {
    lock().state.word_wrap.unwrap_or(false)
}

/// Whether spell checking was last turned on or off, if it was ever toggled.
pub fn spell_check() -> Option<bool> {
    lock().state.spell_check
}

pub fn set_word_wrap(on: bool) {
    lock().update(|s| s.word_wrap = Some(on));
}

pub fn set_spell_check(on: bool) {
    lock().update(|s| s.spell_check = Some(on));
}

fn lock() -> MutexGuard<'static, Store> {
    STORE.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct ViewState {
    word_wrap: Option<bool>,
    spell_check: Option<bool>,
}

struct Store {
    /// `None` if there's no settings directory. Then nothing is remembered.
    path: Option<PathBuf>,
    state: ViewState,
}

impl Store {
    const fn new() -> Self {
        Self { path: None, state: ViewState { word_wrap: None, spell_check: None } }
    }

    fn load(&mut self, settings_path: &Path) {
        self.path = (!settings_path.as_os_str().is_empty())
            .then(|| settings_path.with_file_name(FILE_NAME));
        // A missing or broken file just means that nothing is remembered yet.
        self.state = self
            .path
            .as_deref()
            .and_then(|path| fs::read_to_string(path).ok())
            .map(|text| parse(&text))
            .unwrap_or_default();
    }

    fn update(&mut self, f: impl FnOnce(&mut ViewState)) {
        let before = self.state;
        f(&mut self.state);
        if self.state != before
            && let Some(path) = &self.path
        {
            // Forgetting a View choice isn't worth an error message.
            _ = save(path, &self.state);
        }
    }
}

fn parse(text: &str) -> ViewState {
    let scratch = scratch_arena(None);
    let Some(root) = json::parse(&scratch, text).ok().and_then(|v| v.as_object()) else {
        return ViewState::default();
    };
    ViewState { word_wrap: root.get_bool("wordWrap"), spell_check: root.get_bool("spellCheck") }
}

fn serialize(state: &ViewState) -> String {
    let entries: Vec<_> = [("wordWrap", state.word_wrap), ("spellCheck", state.spell_check)]
        .into_iter()
        .filter_map(|(key, value)| Some((key, value?)))
        .collect();
    let mut out = String::from("{\n");
    for (i, (key, value)) in entries.iter().enumerate() {
        let comma = if i + 1 < entries.len() { "," } else { "" };
        out.push_str(&format!("    \"{key}\": {value}{comma}\n"));
    }
    out.push_str("}\n");
    out
}

fn save(path: &Path, state: &ViewState) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    // Write a temporary file and rename it over the old one, so that it's never half written.
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serialize(state))?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory under the system's temp directory, removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("edit-view-state-{}-{name}", std::process::id()));
            _ = fs::remove_dir_all(&dir);
            Self(dir)
        }

        fn settings_path(&self) -> PathBuf {
            self.0.join("config").join("settings.json")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn test_parse() {
        let on_off = ViewState { word_wrap: Some(true), spell_check: Some(false) };
        assert_eq!(parse(r#"{ "wordWrap": true, "spellCheck": false }"#), on_off);
        assert_eq!(
            parse("{\n// A comment.\n\"wordWrap\": true, \"spellCheck\": false,\n}"),
            on_off
        );
        assert_eq!(
            parse(r#"{ "spellCheck": true }"#),
            ViewState { word_wrap: None, spell_check: Some(true) }
        );
        // Anything else is ignored.
        for text in ["", "{", "[]", "true", r#"{ "wordWrap": "on", "spellCheck": 1 }"#] {
            assert_eq!(parse(text), ViewState::default(), "{text:?}");
        }
    }

    #[test]
    fn test_serialize() {
        assert_eq!(serialize(&ViewState::default()), "{\n}\n");
        let state = ViewState { word_wrap: Some(true), spell_check: Some(false) };
        assert_eq!(serialize(&state), "{\n    \"wordWrap\": true,\n    \"spellCheck\": false\n}\n");
        let state = ViewState { word_wrap: None, spell_check: Some(true) };
        assert_eq!(serialize(&state), "{\n    \"spellCheck\": true\n}\n");
        for state in [
            ViewState::default(),
            ViewState { word_wrap: Some(false), spell_check: None },
            ViewState { word_wrap: Some(true), spell_check: Some(true) },
        ] {
            assert_eq!(parse(&serialize(&state)), state);
        }
    }

    // Toggling writes the file (creating the directory), and the next session reads it back.
    #[test]
    fn test_remembered_across_sessions() {
        let dir = TempDir::new("sessions");
        let settings = dir.settings_path();

        let mut store = Store::new();
        store.load(&settings);
        assert_eq!(store.state, ViewState::default());
        store.update(|s| s.word_wrap = Some(true));
        store.update(|s| s.spell_check = Some(false));

        let path = settings.with_file_name(FILE_NAME);
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, "{\n    \"wordWrap\": true,\n    \"spellCheck\": false\n}\n");
        assert!(!path.with_extension("json.tmp").exists());

        let mut next = Store::new();
        next.load(&settings);
        assert_eq!(next.state, ViewState { word_wrap: Some(true), spell_check: Some(false) });
        next.update(|s| s.word_wrap = Some(false));
        let mut last = Store::new();
        last.load(&settings);
        assert_eq!(last.state, ViewState { word_wrap: Some(false), spell_check: Some(false) });
    }

    // Without a settings directory, nothing is written anywhere.
    #[test]
    fn test_no_settings_path() {
        let mut store = Store::new();
        store.load(Path::new(""));
        assert_eq!(store.path, None);
        store.update(|s| s.word_wrap = Some(true));
        assert_eq!(store.state.word_wrap, Some(true));
    }

    // A broken file is ignored, and replaced on the next toggle.
    #[test]
    fn test_broken_file() {
        let dir = TempDir::new("broken");
        let settings = dir.settings_path();
        let path = settings.with_file_name(FILE_NAME);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{ not json").unwrap();

        let mut store = Store::new();
        store.load(&settings);
        assert_eq!(store.state, ViewState::default());
        store.update(|s| s.spell_check = Some(true));
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\n    \"spellCheck\": true\n}\n");
    }
}
