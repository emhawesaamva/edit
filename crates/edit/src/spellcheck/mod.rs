// Licensed under the MIT License.
// Fork-only: spellcheck (not part of upstream microsoft/edit).

//! Spell checking, based on the `spellbook` crate and a bundled en_US Hunspell dictionary.
//!
//! The dictionary is parsed on a background thread (see [`start_loading`]).
//! Until it's ready, [`dictionary()`] returns `None` and spell checking silently does nothing.
//!
//! The pieces:
//! * [`tokenizer`] splits a line segment into candidate words.
//! * [`scope`] decides which parts of a line get checked, based on the language and highlight kinds.
//! * `crate::buffer::spelling` applies all of that to a `TextBuffer` (rendering, suggestions).

mod dictionary;
pub mod scope;
pub mod tokenizer;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(test)]
pub use dictionary::dictionary_blocking;
pub use dictionary::{
    DictionaryStatus, dictionary, dictionary_status, poll_timeout, start_loading,
};
pub use spellbook::Dictionary;

use crate::framebuffer::Attributes;

/// Words longer than this (in bytes) are neither checked nor offered suggestions.
pub const MAX_WORD_LEN: usize = 48;
const MAX_SUGGESTIONS: usize = 8;
const WORD_CACHE_CAPACITY: usize = 16 * 1024;

static ENABLED: AtomicBool = AtomicBool::new(false);
static CURLY: AtomicBool = AtomicBool::new(true);

thread_local! {
    static WORD_CACHE: RefCell<HashMap<Box<str>, bool>> = RefCell::new(HashMap::new());
}

/// Applies the user's settings. Starts loading the dictionary if spell checking is enabled.
pub fn configure(enabled: bool, curly: bool) {
    CURLY.store(curly, Ordering::Relaxed);
    set_enabled(enabled);
}

/// Whether spell checking is globally enabled (View > Spell Check).
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Globally enables or disables spell checking. Enabling it starts loading the dictionary.
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
    if enabled {
        start_loading();
    }
}

/// The framebuffer attribute used to mark misspelled words.
pub fn underline_attr() -> Attributes {
    if CURLY.load(Ordering::Relaxed) { Attributes::Undercurl } else { Attributes::Underlined }
}

/// Checks a single word. Results are cached, because the same words get checked on every frame.
pub fn is_word_correct(dict: &Dictionary, word: &str) -> bool {
    // The dictionary only knows the ASCII apostrophe.
    let normalized;
    let word = if word.contains('\u{2019}') {
        normalized = word.replace('\u{2019}', "'");
        normalized.as_str()
    } else {
        word
    };

    WORD_CACHE.with_borrow_mut(|cache| {
        if let Some(&correct) = cache.get(word) {
            return correct;
        }
        if cache.len() >= WORD_CACHE_CAPACITY {
            cache.clear();
        }
        let correct = dict.check(word);
        cache.insert(word.into(), correct);
        correct
    })
}

/// Returns up to `MAX_SUGGESTIONS` (8) replacements for `word`. This can be slow, so call it once per request.
pub fn suggest(dict: &Dictionary, word: &str) -> Vec<String> {
    if word.is_empty() || word.len() > MAX_WORD_LEN {
        return Vec::new();
    }

    let typographic = word.contains('\u{2019}');
    let word = word.replace('\u{2019}', "'");

    let mut suggestions = Vec::new();
    dict.suggest(&word, &mut suggestions);

    // For a capitalized word like "Teh", spellbook first suggests for "Teh" ("Eh", "Th", ...)
    // and appends the suggestions for "teh" ("The", ...) after them, so the truncation below
    // would drop the latter. Interleave the two, so that the best of each come first.
    if is_capitalized(&word) {
        let mut lower = Vec::new();
        dict.suggest(&word.to_lowercase(), &mut lower);
        let lower: Vec<String> = lower.iter().map(|s| capitalize(s)).collect();
        suggestions = interleave(&suggestions, &lower);
    }

    suggestions.truncate(MAX_SUGGESTIONS);

    // Keep the user's apostrophe style.
    if typographic {
        for s in &mut suggestions {
            *s = s.replace('\'', "\u{2019}");
        }
    }

    suggestions
}

/// Whether the word starts with an uppercase letter and has no other uppercase letters ("Teh").
fn is_capitalized(word: &str) -> bool {
    let mut chars = word.chars();
    chars.next().is_some_and(char::is_uppercase) && !chars.any(char::is_uppercase)
}

/// Uppercases the first character, like spellbook does for the suggestions of capitalized words.
fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Alternates between `all` and `preferred`, skipping duplicates. Only items in `all` are returned,
/// so this merely reorders `all`, such that the first items of `preferred` move up.
fn interleave(all: &[String], preferred: &[String]) -> Vec<String> {
    let mut res: Vec<String> = Vec::with_capacity(all.len());
    let mut a = all.iter();
    let mut b = preferred.iter().filter(|s| all.contains(s));

    loop {
        let (x, y) = (a.next(), b.next());
        if x.is_none() && y.is_none() {
            break;
        }
        for s in [x, y].into_iter().flatten() {
            if !res.contains(s) {
                res.push(s.clone());
            }
        }
    }

    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_word_correct() {
        let dict = dictionary_blocking().unwrap();
        for word in ["hello", "Hello", "don't", "don\u{2019}t", "dog's", "naive", "cafe"] {
            assert!(is_word_correct(dict, word), "{word}");
        }
        for word in ["teh", "recieve", "helo"] {
            assert!(!is_word_correct(dict, word), "{word}");
        }
    }

    #[test]
    fn test_suggest() {
        let dict = dictionary_blocking().unwrap();
        assert!(suggest(dict, "teh").iter().any(|s| s == "the"));
        assert!(suggest(dict, "recieve").iter().any(|s| s == "receive"));
        assert!(suggest(dict, "").is_empty());
        assert!(suggest(dict, &"a".repeat(MAX_WORD_LEN + 1)).is_empty());
        assert!(suggest(dict, "dont\u{2019}").iter().all(|s| !s.contains('\'')));
    }

    #[test]
    fn test_suggest_restores_typographic_apostrophe() {
        let dict = dictionary_blocking().unwrap();
        let typographic = suggest(dict, "does\u{2019}nt");
        assert!(typographic.iter().any(|s| s == "doesn\u{2019}t"), "{typographic:?}");
        assert!(typographic.iter().all(|s| !s.contains('\'')), "{typographic:?}");
        // ASCII stays ASCII.
        let ascii = suggest(dict, "doesnt");
        assert!(ascii.iter().any(|s| s == "doesn't"), "{ascii:?}");
        assert!(ascii.iter().all(|s| !s.contains('\u{2019}')), "{ascii:?}");
    }

    // Regression: spellbook lists the suggestions for "teh" after the ones for "Teh",
    // past `MAX_SUGGESTIONS`, so a capitalized "Teh" never offered "The".
    #[test]
    fn test_suggest_capitalized() {
        let dict = dictionary_blocking().unwrap();

        let teh = suggest(dict, "Teh");
        assert!(teh.len() <= MAX_SUGGESTIONS, "{teh:?}");
        assert!(teh.iter().take(2).any(|s| s == "The"), "{teh:?}");
        assert!(teh.iter().all(|s| s.starts_with(char::is_uppercase)), "{teh:?}");
        let dont = suggest(dict, "Dont");
        assert!(dont.iter().any(|s| s == "Don't"), "{dont:?}");
        let dont = suggest(dict, "Dont\u{2019}");
        assert!(dont.iter().all(|s| !s.contains('\'')), "{dont:?}");

        // spellbook's first choice stays first, e.g. for proper nouns.
        let mut first = Vec::new();
        for word in ["Teh", "Amercia", "Germnay", "Pariss", "Thier"] {
            dict.suggest(word, &mut first);
            assert_eq!(suggest(dict, word).first(), first.first(), "{word}");
        }
        assert_eq!(suggest(dict, "Amercia").first().map(String::as_str), Some("America"));

        // Lowercase words are unaffected.
        assert_eq!(suggest(dict, "teh").first().map(String::as_str), Some("the"));
    }

    #[test]
    fn test_interleave() {
        let v = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let all = v(&["Eh", "Th", "Te", "Tet", "The", "Tech"]);

        assert_eq!(
            interleave(&all, &v(&["The", "Eh", "Tech"])),
            v(&["Eh", "The", "Th", "Te", "Tech", "Tet"])
        );
        // Items that aren't in `all` are dropped, and duplicates are skipped.
        assert_eq!(
            interleave(&all, &v(&["Zzz", "The", "The", "Eh"])),
            v(&["Eh", "The", "Th", "Te", "Tet", "Tech"])
        );
        assert_eq!(interleave(&all, &[]), all);
        assert_eq!(interleave(&[], &v(&["The"])), Vec::<String>::new());
    }

    #[test]
    fn test_is_capitalized() {
        for word in ["Teh", "The", "Éa", "O'neil", "Ab"] {
            assert!(is_capitalized(word), "{word}");
        }
        for word in ["teh", "TEh", "O'Neil", "McDonald", "", "'Tis", "éA"] {
            assert!(!is_capitalized(word), "{word}");
        }
        assert_eq!(capitalize("the"), "The");
        assert_eq!(capitalize("éa"), "Éa");
        assert_eq!(capitalize(""), "");
    }

    #[test]
    fn test_word_cache() {
        let dict = dictionary_blocking().unwrap();
        let cached = |word: &str| WORD_CACHE.with_borrow(|c| c.get(word).copied());

        // Typographic apostrophes share the entry of the ASCII spelling.
        assert!(is_word_correct(dict, "don\u{2019}t"));
        assert_eq!(cached("don't"), Some(true));
        assert_eq!(cached("don\u{2019}t"), None);

        // The cache never grows past its capacity, and stays correct after it's been cleared.
        let mut word = String::new();
        for i in 0..WORD_CACHE_CAPACITY + 100 {
            word.clear();
            word.push_str("zq");
            let mut n = i;
            while n > 0 {
                word.push((b'a' + (n % 26) as u8) as char);
                n /= 26;
            }
            assert!(!is_word_correct(dict, &word), "{word}");
            assert!(WORD_CACHE.with_borrow(|c| c.len()) <= WORD_CACHE_CAPACITY);
        }
        assert!(WORD_CACHE.with_borrow(|c| c.len()) < WORD_CACHE_CAPACITY);
        for _ in 0..2 {
            assert!(is_word_correct(dict, "hello"));
            assert!(!is_word_correct(dict, "teh"));
        }
        assert_eq!(cached("hello"), Some(true));
        assert_eq!(cached("teh"), Some(false));
    }
}
