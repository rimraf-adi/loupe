//! Trigram + token index, regex planner, and verifier (milestone M2).
//!
//! Search is three steps: [`plan`] turns a regex into a trigram [`Query`] that every match must
//! satisfy; the per-segment [`TextIndex`] answers it with candidate blobs; [`TextSearcher`]
//! maps blobs to paths and verifies candidates by running the real regex over the file.

mod build;
mod fuzzy;
mod index;
mod plan;
mod postings;
mod search;
mod terms;

pub use build::{BuildStats, write_text_index};
pub use fuzzy::FuzzyHit;
pub use index::{BuiltIndex, POST_FILE, TOK_FILE, TRI_FILE, TextIndex, TextIndexBuilder};
pub use loupe_overlay::Overlay;
pub use plan::{Query, plan};
pub use search::{FileHit, SearchOptions, SearchResult, SearchStats, Snippet, TextSearcher};
pub use terms::{FileTerms, extract_terms, extract_tokens, extract_trigrams};
