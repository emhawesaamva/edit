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
        // The bundled dictionary has no accented words, only "cafe" and "naive".
        // Accept "café" and "naïve" too, at the cost of also accepting misplaced accents ("cafè").
        let correct = dict.check(word) || (!word.is_ascii() && dict.check(&fold_diacritics(word)));
        cache.insert(word.into(), correct);
        correct
    })
}

/// The base letter of each character in U+00C0..=U+017F (Latin-1 Supplement and Latin Extended-A).
/// `.` keeps the character: the æ and œ ligatures are expanded by [`fold_diacritics`] instead,
/// and ð, þ, ß, ı, ĳ, ĸ, ŉ, ŋ and ſ are letters in their own right, not accented ones.
const FOLD_TABLE: &[u8; 192] = b"\
    AAAAAA.CEEEEIIII.NOOOOO.OUUUUY..aaaaaa.ceeeeiiii.nooooo.ouuuuy.y\
    AaAaAaCcCcCcCcDdDdEeEeEeEeEeGgGgGgGgHhHhIiIiIiIiI...JjKk.LlLlLlL\
    lLlNnNnNn...OoOoOo..RrRrRrSsSsSsSsTtTtTtUuUuUuUuUuUuWwYyYZzZzZz.";

/// Strips the accents from Latin letters ("café" -> "cafe"), both precomposed ones
/// and combining marks (U+0300-036F), and expands æ and œ ("archæology" -> "archaeology").
fn fold_diacritics(word: &str) -> String {
    let mut folded = String::with_capacity(word.len());
    for c in word.chars() {
        match c {
            '\u{300}'..='\u{36f}' => {}
            // Title case, because all-caps words aren't checked ("Œdipus" -> "Oedipus").
            'Æ' => folded.push_str("Ae"),
            'æ' => folded.push_str("ae"),
            'Œ' => folded.push_str("Oe"),
            'œ' => folded.push_str("oe"),
            _ => match (c as usize).checked_sub(0xc0).and_then(|i| FOLD_TABLE.get(i)) {
                Some(&base) if base != b'.' => folded.push(base as char),
                _ => folded.push(c),
            },
        }
    }
    folded
}

/// Returns up to `MAX_SUGGESTIONS` (8) replacements for `word`. This can be slow, so call it once per request.
pub fn suggest(dict: &Dictionary, word: &str) -> Vec<String> {
    if word.is_empty() || word.len() > MAX_WORD_LEN {
        return Vec::new();
    }

    let typographic = word.contains('\u{2019}');
    let word = word.replace('\u{2019}', "'");
    // The dictionary has no accented words, so the suggestions never have accents anyway.
    // Without them, spellbook starts closer to the intended word ("naïeve" -> "naive", not "nave").
    let word = if word.is_ascii() { word } else { fold_diacritics(&word) };

    let mut suggestions = Vec::new();
    dict.suggest(&word, &mut suggestions);

    // For a capitalized word like "Teh", spellbook first lists its suggestions for "Teh" as typed
    // and then those for "teh". The former are mostly edits of the capital letter ("Eh"),
    // splits after capitalized abbreviations ("Ac cussed" for "Accussed") and proper nouns.
    // The latter are spellbook's own ranking for the common word ("The").
    // So alternate between the two, starting with the lowercase ranking, but keep spellbook's
    // first choice first if it's a proper noun ("Amercia" -> "America"; see `keeps_first_place`).
    // Measured on real misspellings (Wikipedia's list, Birkbeck, aspell), this beats spellbook's
    // order for capitalized common words and matches it for proper nouns.
    if is_capitalized(&word) {
        let mut lower = Vec::new();
        dict.suggest(&word.to_lowercase(), &mut lower);
        let lower: Vec<String> = lower.iter().map(|s| capitalize(s)).collect();
        let first = suggestions
            .first()
            .filter(|s| keeps_first_place(dict, &word, s, lower.first()))
            .cloned();
        suggestions = interleave(&lower, &suggestions);
        if let Some(first) = first
            && let Some(i) = suggestions.iter().position(|s| *s == first)
        {
            suggestions[..=i].rotate_right(1);
        }
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

/// Whether spellbook's `first` suggestion for the capitalized `word` stays first:
/// * it's a proper noun ("America": the dictionary doesn't know "america"),
/// * it starts with the same letter as `word`,
/// * and the best suggestion for the lowercase word isn't simply two swapped letters,
///   unless `first` is too ("Htis" -> "This", not "Hts"; but "Amercia" -> "America").
fn keeps_first_place(
    dict: &Dictionary,
    word: &str,
    first: &str,
    lower_first: Option<&String>,
) -> bool {
    !first.contains([' ', '-'])
        && word.chars().next() == first.chars().next()
        && !dict.check(&first.to_lowercase())
        && !lower_first.is_some_and(|l| {
            l != first && is_adjacent_swap(word, l) && !is_adjacent_swap(word, first)
        })
}

/// Whether `b` is `a` with two adjacent letters swapped ("Teh" -> "The"). Ignores case.
fn is_adjacent_swap(a: &str, b: &str) -> bool {
    let a: Vec<char> = a.chars().flat_map(char::to_lowercase).collect();
    let b: Vec<char> = b.chars().flat_map(char::to_lowercase).collect();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = (0..a.len()).filter(|&i| a[i] != b[i]);
    match (diff.next(), diff.next(), diff.next()) {
        (Some(i), Some(j), None) => j == i + 1 && a[i] == b[j] && a[j] == b[i],
        _ => false,
    }
}

/// Alternates between `preferred` and `all`, starting with `preferred`, skipping duplicates.
/// Only items in `all` are returned, so this merely reorders `all`, such that the first items
/// of `preferred` move up.
fn interleave(preferred: &[String], all: &[String]) -> Vec<String> {
    let mut res: Vec<String> = Vec::with_capacity(all.len());
    let mut a = preferred.iter().filter(|s| all.contains(s));
    let mut b = all.iter();

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

    // Regression: the dictionary only has "cafe" and "naive", so every accented word was flagged.
    #[test]
    fn test_is_word_correct_accented() {
        let dict = dictionary_blocking().unwrap();
        for word in [
            "café",
            "cafe\u{301}", // decomposed
            "naïve",
            "Naïve",
            "résumé",
            "clichés",
            "façade",
            "fiancée",
            "coöperate",
            "José",
            "Zoë\u{2019}s",
            "archæology",
            "Œdipus",
            "Łódź",
            // The price: misplaced accents are accepted, too.
            "cafè",
        ] {
            assert!(is_word_correct(dict, word), "{word}");
        }
        // ß and þ aren't accented letters, so these aren't "Strasse" and "the".
        for word in ["teh", "tèh", "cafée", "Straße", "þe"] {
            assert!(!is_word_correct(dict, word), "{word}");
        }

        // The result is cached under the word as written.
        let cached = |word: &str| WORD_CACHE.with_borrow(|c| c.get(word).copied());
        assert!(is_word_correct(dict, "entrée"));
        assert_eq!(cached("entrée"), Some(true));
        assert!(!is_word_correct(dict, "entréee"));
        assert_eq!(cached("entréee"), Some(false));
    }

    #[test]
    fn test_fold_diacritics() {
        assert_eq!(fold_diacritics("ÀÁÂÃÄÅÇÈÉÊËÌÍÎÏÑÒÓÔÕÖØÙÚÛÜÝ"), "AAAAAACEEEEIIIINOOOOOOUUUUY");
        assert_eq!(fold_diacritics("àáâãäåçèéêëìíîïñòóôõöøùúûüýÿ"), "aaaaaaceeeeiiiinoooooouuuuyy");
        assert_eq!(
            fold_diacritics("ĀăĆčĎđĘěĞħİĵĶĹłŃňŐŘşŠťŦŨůŴŷŸŹž"),
            "AaCcDdEeGhIjKLlNnORsStTUuWyYZz"
        );
        assert_eq!(fold_diacritics("ÆæŒœ"), "AeaeOeoe");
        // Combining marks are dropped.
        assert_eq!(fold_diacritics("e\u{301}a\u{308}\u{300}n\u{36f}"), "ean");
        // Everything else stays: letters of their own, non-letters, other blocks and scripts, ASCII.
        for s in ["ÐðÞþßıĲĳĸŉŊŋſ", "×÷¿", "ƀạλ", "Hello, world!", ""] {
            assert_eq!(fold_diacritics(s), s);
        }
    }

    #[test]
    fn test_suggest_accented() {
        let dict = dictionary_blocking().unwrap();
        let first = |word: &str| suggest(dict, word).into_iter().next();

        // Regression: spellbook got the accented word, which is further away from any
        // dictionary word ("naïeve" -> "nave", "résumme" -> "Summers", "tèh" -> "Th").
        assert_eq!(first("naïeve").as_deref(), Some("naive"));
        assert_eq!(first("résumme").as_deref(), Some("resume"));
        assert_eq!(first("Résumme").as_deref(), Some("Resume"));
        assert_eq!(first("tèh").as_deref(), Some("the"));
        assert_eq!(first("cafe\u{301}e").as_deref(), Some("cafe"));
        assert!(suggest(dict, "caffé").iter().any(|s| s == "cafe"));

        // The apostrophe style is kept, and nothing breaks on letters that aren't folded.
        let cafes = suggest(dict, "cafée\u{2019}s");
        assert!(cafes.iter().any(|s| s == "cafe\u{2019}s"), "{cafes:?}");
        assert!(cafes.iter().all(|s| !s.contains('\'')), "{cafes:?}");
        for word in ["Straße", "þe", "ÐÐ", "Éa", "é", "\u{301}", "\u{301}\u{301}"] {
            assert!(suggest(dict, word).len() <= MAX_SUGGESTIONS, "{word}");
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
        assert!(teh.iter().all(|s| s.starts_with(char::is_uppercase)), "{teh:?}");
        let dont = suggest(dict, "Dont");
        assert!(dont.iter().any(|s| s == "Don't"), "{dont:?}");
        let dont = suggest(dict, "Dont\u{2019}");
        assert!(dont.iter().all(|s| !s.contains('\'')), "{dont:?}");

        // Regression: spellbook's first choice for the word as typed came first, so a
        // sentence-initial "Teh" offered "Eh" (and "Accross" "Ac cross") before the common word.
        // Its ranking for the lowercase word comes first now...
        let first = |word: &str| suggest(dict, word).into_iter().next();
        for (word, want) in [
            ("Teh", "The"),
            ("Thier", "Their"),
            ("Accross", "Across"),
            ("Aquired", "Acquired"),
            ("Agian", "Again"),
            // A proper noun doesn't beat two swapped letters ("Hts", "Verizon").
            ("Htis", "This"),
            ("Verison", "Version"),
        ] {
            assert_eq!(first(word).as_deref(), Some(want), "{word}");
        }
        // ...except that a proper noun spellbook ranks first stays first.
        for (word, want) in [
            ("Amercia", "America"),
            ("Febuary", "February"),
            ("Guiness", "Guinness"),
            ("Lybia", "Libya"),
            ("Pariss", "Paris"),
        ] {
            assert_eq!(first(word).as_deref(), Some(want), "{word}");
        }

        // This only reorders spellbook's suggestions.
        let mut native = Vec::new();
        for word in ["Teh", "Accussed", "Amercia", "Thier"] {
            dict.suggest(word, &mut native);
            let ours = suggest(dict, word);
            assert!(ours.iter().all(|s| native.contains(s)), "{word}: {ours:?} vs {native:?}");
        }

        // Lowercase words keep spellbook's order.
        for word in ["teh", "thier", "agian", "amercia"] {
            dict.suggest(word, &mut native);
            native.truncate(MAX_SUGGESTIONS);
            assert_eq!(suggest(dict, word), native, "{word}");
        }
    }

    #[test]
    fn test_interleave() {
        let v = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let all = v(&["Eh", "Th", "Te", "Tet", "The", "Tech"]);

        assert_eq!(
            interleave(&v(&["The", "Eh", "Tech"]), &all),
            v(&["The", "Eh", "Th", "Tech", "Te", "Tet"])
        );
        // Items that aren't in `all` are dropped, and duplicates are skipped.
        assert_eq!(
            interleave(&v(&["Zzz", "The", "The", "Eh"]), &all),
            v(&["The", "Eh", "Th", "Te", "Tet", "Tech"])
        );
        assert_eq!(interleave(&[], &all), all);
        assert_eq!(interleave(&v(&["The"]), &[]), Vec::<String>::new());
    }

    #[test]
    fn test_is_adjacent_swap() {
        for (a, b) in [("Teh", "The"), ("Ehllo", "Hello"), ("Htis", "This"), ("ab", "BA")] {
            assert!(is_adjacent_swap(a, b), "{a} {b}");
        }
        for (a, b) in [
            ("Teh", "Teh"),
            ("Teh", "Tea"),
            ("Lybia", "Libya"),
            ("Teh", "Thee"),
            ("abc", "cba"),
            ("abcd", "badc"),
            ("", ""),
        ] {
            assert!(!is_adjacent_swap(a, b), "{a} {b}");
        }
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
