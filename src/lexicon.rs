//! Korean linguistic data (negation markers, particles and endings, stopwords, antonyms, ID column names...).
//!
//! The word lists live in `locales/lexicon.ko.json` and build.rs turns them into constants here, so the Rust
//! sources stay English while the linter and the profiler keep understanding Korean prompts and data.

include!(concat!(env!("OUT_DIR"), "/lexicon_ko.rs"));
