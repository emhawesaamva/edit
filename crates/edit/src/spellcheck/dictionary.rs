// Licensed under the MIT License.
// Fork-only: spellcheck (not part of upstream microsoft/edit).

//! Loads the bundled dictionary on a background thread.
//!
//! Parsing takes tens to hundreds of milliseconds, which we don't want to block the first frame.
//! The main loop has no timer, so [`poll_timeout`] tells it to wake up periodically while loading
//! and once more when the dictionary becomes available, so that the underlines show up.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use spellbook::Dictionary;

const AFF: &str = include_str!("../../../../assets/dictionaries/en_US/index.aff");
const DIC: &str = include_str!("../../../../assets/dictionaries/en_US/index.dic");

/// How often the main loop should wake up while the dictionary is loading.
const LOADING_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// `None` inside the `OnceLock` means that loading failed.
static DICTIONARY: OnceLock<Option<Dictionary>> = OnceLock::new();
static LOADING_STARTED: AtomicBool = AtomicBool::new(false);
static READY_SEEN: AtomicBool = AtomicBool::new(false);

// The dictionary is parsed on one thread and used on another.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Dictionary>();
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DictionaryStatus {
    NotStarted,
    Loading,
    Ready,
    Unavailable,
}

/// Starts parsing the dictionary on a background thread. Safe to call repeatedly.
pub fn start_loading() {
    if LOADING_STARTED.swap(true, Ordering::Relaxed) {
        return;
    }

    let spawned =
        thread::Builder::new().name("spellcheck".into()).spawn(|| _ = DICTIONARY.get_or_init(load));
    if spawned.is_err() {
        _ = DICTIONARY.get_or_init(|| None);
    }
}

fn load() -> Option<Dictionary> {
    Dictionary::new(AFF, DIC).ok()
}

/// Returns the dictionary if it has finished loading. Never blocks.
pub fn dictionary() -> Option<&'static Dictionary> {
    DICTIONARY.get().and_then(Option::as_ref)
}

pub fn dictionary_status() -> DictionaryStatus {
    match DICTIONARY.get() {
        Some(Some(_)) => DictionaryStatus::Ready,
        Some(None) => DictionaryStatus::Unavailable,
        None if LOADING_STARTED.load(Ordering::Relaxed) => DictionaryStatus::Loading,
        None => DictionaryStatus::NotStarted,
    }
}

/// The longest the main loop may wait for input before rendering another frame.
pub fn poll_timeout() -> Duration {
    if !LOADING_STARTED.load(Ordering::Relaxed) {
        return Duration::MAX;
    }
    match DICTIONARY.get() {
        None => LOADING_POLL_INTERVAL,
        // One immediate frame once loading has finished, so that the underlines appear.
        Some(_) if !READY_SEEN.swap(true, Ordering::Relaxed) => Duration::ZERO,
        Some(_) => Duration::MAX,
    }
}

/// Loads the dictionary on the calling thread. For tests only.
#[cfg(test)]
pub fn dictionary_blocking() -> Option<&'static Dictionary> {
    LOADING_STARTED.store(true, Ordering::Relaxed);
    DICTIONARY.get_or_init(load).as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bundled_dictionary_loads() {
        let dict = dictionary_blocking().expect("bundled dictionary must parse");
        assert!(dict.check("hello"));
        assert!(!dict.check("teh"));
        assert_eq!(dictionary_status(), DictionaryStatus::Ready);
        assert!(dictionary().is_some());
    }

    #[test]
    fn test_poll_timeout_after_loading() {
        dictionary_blocking().unwrap();

        // Only main.rs calls `poll_timeout()`, so this is the first call since loading finished:
        // One immediate frame, so that the underlines appear, then back to waiting for input.
        assert_eq!(poll_timeout(), Duration::ZERO);
        assert_eq!(poll_timeout(), Duration::MAX);
        assert_eq!(poll_timeout(), Duration::MAX);

        // Loading again does nothing.
        start_loading();
        assert_eq!(dictionary_status(), DictionaryStatus::Ready);
        assert_eq!(poll_timeout(), Duration::MAX);
    }

    /// Run with `cargo test --release -p edit load_time -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn load_time() {
        let beg = std::time::Instant::now();
        let dict = load().unwrap();
        let load = beg.elapsed();

        let beg = std::time::Instant::now();
        for _ in 0..1000 {
            std::hint::black_box(dict.check(std::hint::black_box("recieve")));
        }
        let check = beg.elapsed() / 1000;

        let beg = std::time::Instant::now();
        let mut out = Vec::new();
        dict.suggest("recieve", &mut out);
        let suggest = beg.elapsed();

        // `spellcheck::suggest()` asks twice for capitalized words.
        let beg = std::time::Instant::now();
        std::hint::black_box(super::super::suggest(&dict, "teh"));
        let suggest_lower = beg.elapsed();
        let beg = std::time::Instant::now();
        std::hint::black_box(super::super::suggest(&dict, "Teh"));
        let suggest_capitalized = beg.elapsed();

        println!(
            "load: {load:?}, check: {check:?}, suggest: {suggest:?}, \
             spellcheck::suggest(\"teh\"): {suggest_lower:?}, spellcheck::suggest(\"Teh\"): {suggest_capitalized:?}"
        );
    }
}
