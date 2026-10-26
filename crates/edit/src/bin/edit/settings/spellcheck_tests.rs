// Licensed under the MIT License.
// Fork-only: spellcheck (not part of upstream microsoft/edit).

//! Tests for the `spellcheck.*` settings.

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
    // The underline is "auto" by default: curly unless the terminal can't draw it.
    let s = Settings::new();
    assert!(s.spellcheck_enabled);
    assert_eq!(s.spellcheck_curly, None);

    let s = load_spellcheck("{}").unwrap();
    assert!(s.spellcheck_enabled);
    assert_eq!(s.spellcheck_curly, None);

    let s = load_spellcheck(r#"{ "files.associations": {}, "spellcheck": false }"#).unwrap();
    assert!(s.spellcheck_enabled);
    assert_eq!(s.spellcheck_curly, None);
}

#[test]
fn test_spellcheck_values() {
    let s = load_spellcheck(r#"{ "spellcheck.enabled": false }"#).unwrap();
    assert!(!s.spellcheck_enabled);
    assert_eq!(s.spellcheck_curly, None);

    let s = load_spellcheck(r#"{ "spellcheck.underline": "plain" }"#).unwrap();
    assert!(s.spellcheck_enabled);
    assert_eq!(s.spellcheck_curly, Some(false));

    let s = load_spellcheck(r#"{ "spellcheck.underline": "auto" }"#).unwrap();
    assert_eq!(s.spellcheck_curly, None);

    let s = load_spellcheck(
        r#"{
            // Comments and trailing commas are allowed.
            "spellcheck.enabled": true,
            "spellcheck.underline": "curly",
        }"#,
    )
    .unwrap();
    assert!(s.spellcheck_enabled);
    assert_eq!(s.spellcheck_curly, Some(true));
}

#[test]
fn test_spellcheck_invalid() {
    for text in [
        r#"{ "spellcheck.underline": "wavy" }"#,
        r#"{ "spellcheck.underline": "Curly" }"#,
        r#"{ "spellcheck.underline": "Auto" }"#,
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
        assert!(matches!(res, Err(apperr::Error::SettingsInvalid("spellcheck.enabled"))), "{text}");
    }
}
