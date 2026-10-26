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
    suggestions.truncate(MAX_SUGGESTIONS);

    // Keep the user's apostrophe style.
    if typographic {
        for s in &mut suggestions {
            *s = s.replace('\'', "\u{2019}");
        }
    }

    suggestions
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
}
