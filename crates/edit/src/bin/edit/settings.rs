use std::path::PathBuf;

use edit::arena::{arena_format, read_to_string, scratch_arena};
use edit::buffer::TextBuffer;
use edit::cell::{Ref, SemiRefCell};
use edit::json;
use edit::lsh::{LANGUAGES, Language};

use crate::apperr;

pub struct Settings {
    pub path: PathBuf,
    pub file_associations: Vec<(String, &'static Language)>,
    // Fork: spellcheck
    pub spellcheck_enabled: bool,
    pub spellcheck_curly: bool,
}

struct SettingsCell(SemiRefCell<Settings>);
unsafe impl Sync for SettingsCell {}
static SETTINGS: SettingsCell = SettingsCell(SemiRefCell::new(Settings::new()));

impl Settings {
    /// Fills the given settings.json text buffer with some initial contents for convenience.
    pub fn bootstrap(tb: &mut TextBuffer) {
        tb.set_crlf(false);
        tb.write_raw(b"{\n}\n");
        tb.cursor_move_to_logical(Default::default());
        tb.mark_as_clean();
    }

    const fn new() -> Self {
        Settings {
            path: PathBuf::new(),
            file_associations: Vec::new(),
            // Fork: spellcheck
            spellcheck_enabled: true,
            spellcheck_curly: true,
        }
    }

    pub fn borrow() -> Ref<'static, Settings> {
        SETTINGS.0.borrow()
    }

    pub fn reload() -> apperr::Result<()> {
        let s = &mut *SETTINGS.0.borrow_mut();

        // Reset all members if we had been loaded previously.
        if !s.path.as_os_str().is_empty() {
            *s = Settings::new();
        }

        s.load()
    }

    fn load(&mut self) -> apperr::Result<()> {
        self.path = match settings_json_path() {
            Some(p) => p,
            None => return Ok(()),
        };

        let scratch = scratch_arena(None);
        let str = match read_to_string(&scratch, &self.path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err.into()),
            Ok(str) => str,
        };
        let Ok(json) = json::parse(&scratch, &str) else {
            return Err(apperr::Error::SettingsInvalid("Invalid JSON"));
        };
        let Some(root) = json.as_object() else {
            return Err(apperr::Error::SettingsInvalid("Non-object root"));
        };

        if let Some(f) = root.get_object("files.associations") {
            for &(mut key, ref value) in f.iter() {
                if !key.contains('/') {
                    key = arena_format!(&*scratch, "**/{key}").leak();
                }

                let Some(id) = value.as_str() else {
                    return Err(apperr::Error::SettingsInvalid("files.associations"));
                };
                let Some(language) = LANGUAGES.iter().find(|lang| lang.id == id) else {
                    return Err(apperr::Error::SettingsInvalid("language ID"));
                };

                self.file_associations.push((key.to_string(), language));
            }
        }

        self.load_spellcheck(root)?; // Fork: spellcheck

        Ok(())
    }

    // Fork: spellcheck
    fn load_spellcheck(&mut self, root: json::Object) -> apperr::Result<()> {
        if let Some(value) = root.get("spellcheck.enabled") {
            let Some(enabled) = value.as_bool() else {
                return Err(apperr::Error::SettingsInvalid("spellcheck.enabled"));
            };
            self.spellcheck_enabled = enabled;
        }

        if let Some(value) = root.get("spellcheck.underline") {
            self.spellcheck_curly = match value.as_str() {
                Some("curly") => true,
                Some("plain") => false,
                _ => return Err(apperr::Error::SettingsInvalid("spellcheck.underline")),
            };
        }

        Ok(())
    }
}

fn settings_json_path() -> Option<PathBuf> {
    let mut config_dir = config_dir()?;
    config_dir.push("settings.json");
    Some(config_dir)
}

fn config_dir() -> Option<PathBuf> {
    fn var_path(key: &str) -> Option<PathBuf> {
        std::env::var_os(key).map(PathBuf::from)
    }

    fn push(mut path: PathBuf, suffix: &str) -> PathBuf {
        path.push(suffix);
        path
    }

    #[cfg(target_os = "windows")]
    {
        var_path("APPDATA").map(|p| push(p, "Microsoft\\Edit"))
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        var_path("HOME").map(|p| push(p, "Library/Application Support/com.microsoft.edit"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "ios")))]
    {
        var_path("XDG_CONFIG_HOME")
            .or_else(|| var_path("HOME").map(|p| push(p, ".config")))
            .map(|p| push(p, "msedit"))
    }
}

// Fork: spellcheck
#[cfg(test)]
mod tests {
    use edit::arena::scratch_arena;

    use super::*;

    // Applies `text` the way `load()` does, without touching the settings file.
    fn load_spellcheck(text: &str) -> apperr::Result<Settings> {
        let scratch = scratch_arena(None);
        let json = json::parse(&scratch, text).unwrap();
        let mut settings = Settings::new();
        settings.load_spellcheck(json.as_object().unwrap())?;
        Ok(settings)
    }

    #[test]
    fn test_spellcheck_defaults() {
        let s = Settings::new();
        assert!(s.spellcheck_enabled);
        assert!(s.spellcheck_curly);

        let s = load_spellcheck("{}").unwrap();
        assert!(s.spellcheck_enabled);
        assert!(s.spellcheck_curly);

        let s = load_spellcheck(r#"{ "files.associations": {}, "spellcheck": false }"#).unwrap();
        assert!(s.spellcheck_enabled);
        assert!(s.spellcheck_curly);
    }

    #[test]
    fn test_spellcheck_values() {
        let s = load_spellcheck(r#"{ "spellcheck.enabled": false }"#).unwrap();
        assert!(!s.spellcheck_enabled);
        assert!(s.spellcheck_curly);

        let s = load_spellcheck(r#"{ "spellcheck.underline": "plain" }"#).unwrap();
        assert!(s.spellcheck_enabled);
        assert!(!s.spellcheck_curly);

        let s = load_spellcheck(
            r#"{
                // Comments and trailing commas are allowed.
                "spellcheck.enabled": true,
                "spellcheck.underline": "curly",
            }"#,
        )
        .unwrap();
        assert!(s.spellcheck_enabled);
        assert!(s.spellcheck_curly);
    }

    #[test]
    fn test_spellcheck_invalid() {
        for text in [
            r#"{ "spellcheck.underline": "wavy" }"#,
            r#"{ "spellcheck.underline": "Curly" }"#,
            r#"{ "spellcheck.underline": "" }"#,
            r#"{ "spellcheck.underline": true }"#,
            r#"{ "spellcheck.underline": null }"#,
        ] {
            let res = load_spellcheck(text);
            assert!(
                matches!(res, Err(apperr::Error::SettingsInvalid("spellcheck.underline"))),
                "{text}"
            );
        }

        for text in [
            r#"{ "spellcheck.enabled": "false" }"#,
            r#"{ "spellcheck.enabled": 0 }"#,
            r#"{ "spellcheck.enabled": null }"#,
        ] {
            let res = load_spellcheck(text);
            assert!(
                matches!(res, Err(apperr::Error::SettingsInvalid("spellcheck.enabled"))),
                "{text}"
            );
        }
    }
}
