//! fzf-style fuzzy find over file paths and indexed identifiers.
//!
//! Pure subsequence matching with fzf-like scoring (`nucleo-matcher`), over names already in
//! memory (the snapshot's paths, the token index). It needs no index of its own. Fuzzy matching
//! inside file contents is deliberately not offered: trigrams need contiguous literals.

use std::collections::BTreeSet;

use loupe_core::Result;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};

use crate::index::TextIndex;
use crate::search::TextSearcher;
use crate::terms::extract_tokens;
use loupe_overlay::Overlay;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FuzzyHit {
    pub text: String,
    /// Higher is better; 0 for an empty query.
    pub score: u32,
}

/// Rank `items` against `query` (smart case: case-insensitive unless the query has capitals).
/// Ties keep input order, so sorted input gives deterministic output. An empty query returns
/// everything, which makes `find --all | fzf` work.
fn rank<'a>(
    items: impl IntoIterator<Item = &'a str>,
    query: &str,
    limit: usize,
    paths: bool,
) -> Vec<FuzzyHit> {
    let config = if paths {
        Config::DEFAULT.match_paths()
    } else {
        Config::DEFAULT
    };
    let mut matcher = Matcher::new(config);
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    pattern
        .match_list(items, &mut matcher)
        .into_iter()
        .take(limit)
        .map(|(text, score)| FuzzyHit {
            text: text.to_string(),
            score,
        })
        .collect()
}

impl TextSearcher {
    /// Fuzzy-match file paths in the current snapshot.
    pub fn find_files(&self, query: &str, limit: usize) -> Vec<FuzzyHit> {
        self.find_files_with(query, limit, None)
    }

    /// Like [`TextSearcher::find_files`], with overlay additions included and removals hidden.
    pub fn find_files_with(
        &self,
        query: &str,
        limit: usize,
        overlay: Option<&Overlay>,
    ) -> Vec<FuzzyHit> {
        let Some(o) = overlay else {
            return rank(self.paths(), query, limit, true);
        };
        let mut all: BTreeSet<&str> = self.paths().filter(|p| !o.is_removed(p)).collect();
        all.extend(o.puts().into_iter().map(|(p, _)| p));
        rank(all, query, limit, true)
    }

    /// Fuzzy-match indexed identifiers (the token index): a typo-tolerant, abbreviation-friendly
    /// symbol-name lookup.
    pub fn find_tokens(&self, query: &str, limit: usize) -> Result<Vec<FuzzyHit>> {
        self.find_tokens_with(query, limit, None)
    }

    /// Like [`TextSearcher::find_tokens`], also offering identifiers from overlay files.
    pub fn find_tokens_with(
        &self,
        query: &str,
        limit: usize,
        overlay: Option<&Overlay>,
    ) -> Result<Vec<FuzzyHit>> {
        let mut all = BTreeSet::new();
        for seg in self.segments() {
            all.extend(TextIndex::from_segment(seg)?.tokens());
        }
        if let Some(o) = overlay {
            for (_, bytes) in o.puts() {
                all.extend(
                    extract_tokens(bytes)
                        .into_iter()
                        .map(|t| String::from_utf8_lossy(&t).into_owned()),
                );
            }
        }
        Ok(rank(all.iter().map(String::as_str), query, limit, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(h: &[FuzzyHit]) -> Vec<&str> {
        h.iter().map(|x| x.text.as_str()).collect()
    }

    const PATHS: [&str; 5] = [
        "docs/notes.txt",
        "src/main.rs",
        "src/server.rs",
        "src/util/server_helpers.rs",
        "tests/server_test.rs",
    ];

    #[test]
    fn subsequence_match_ranks_best_first() {
        let r = rank(PATHS, "srvr", 10, true);
        assert!(!r.is_empty());
        assert_eq!(r[0].text, "src/server.rs", "{r:?}");
        assert!(r.iter().all(|h| h.text.contains('s')));
        assert!(!texts(&r).contains(&"src/main.rs"));
    }

    #[test]
    fn smart_case() {
        let lower = rank(["Server", "server"], "server", 10, false);
        assert_eq!(lower.len(), 2);
        let upper = rank(["Server", "server"], "Server", 10, false);
        assert_eq!(texts(&upper), ["Server"]);
    }

    #[test]
    fn empty_query_returns_everything_in_order_up_to_limit() {
        let r = rank(PATHS, "", 3, true);
        assert_eq!(texts(&r), PATHS[..3]);
        assert!(r.iter().all(|h| h.score == 0));
    }

    #[test]
    fn no_match_is_empty() {
        assert!(rank(PATHS, "zzzqqq", 10, true).is_empty());
    }

    #[test]
    fn path_mode_prefers_filename_matches() {
        let r = rank(
            ["a/b/server/mod.rs", "src/server.rs"],
            "server.rs",
            10,
            true,
        );
        assert_eq!(r[0].text, "src/server.rs");
    }
}
