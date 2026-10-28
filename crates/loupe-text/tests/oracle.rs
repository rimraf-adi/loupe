//! Correctness oracle: the trigram index must never lose a file the real regex matches.
//!
//! Soundness is the property that matters (`brute-force matches ⊆ index candidates`); the final
//! result of "candidates, then verify" then equals brute force by construction. These tests
//! hammer that property in memory with random regexes and corpora.

use std::collections::BTreeSet;

use loupe_core::LocalBlobId;
use loupe_text::{BuiltIndex, Query, TextIndexBuilder, extract_terms, plan};
use proptest::prelude::*;
use regex::bytes::RegexBuilder;

fn index_docs(docs: &[Vec<u8>]) -> BuiltIndex {
    let mut b = TextIndexBuilder::new();
    for (i, d) in docs.iter().enumerate() {
        b.add(LocalBlobId(i as u32), extract_terms(d)).unwrap();
    }
    b.finish().unwrap()
}

/// Docs the planner + index would hand to the verifier.
fn candidates(built: &BuiltIndex, n_docs: usize, pattern: &str, ci: bool) -> BTreeSet<usize> {
    let query = plan(pattern, ci).unwrap();
    match built.view().unwrap().candidates(&query).unwrap() {
        None => (0..n_docs).collect(),
        Some(ids) => ids.into_iter().map(|i| i as usize).collect(),
    }
}

/// Docs containing a non-empty regex match (the searcher ignores empty matches).
fn brute(docs: &[Vec<u8>], pattern: &str, ci: bool) -> BTreeSet<usize> {
    let re = RegexBuilder::new(pattern)
        .case_insensitive(ci)
        .multi_line(true)
        .build()
        .unwrap();
    docs.iter()
        .enumerate()
        .filter(|(_, d)| re.find_iter(d).any(|m| !m.is_empty()))
        .map(|(i, _)| i)
        .collect()
}

fn assert_sound(docs: &[Vec<u8>], pattern: &str, ci: bool) {
    let built = index_docs(docs);
    let truth = brute(docs, pattern, ci);
    let cands = candidates(&built, docs.len(), pattern, ci);
    assert!(
        truth.is_subset(&cands),
        "pattern {pattern:?} (ci={ci}) lost files {:?}\n  plan: {}\n  docs: {:?}",
        truth.difference(&cands).collect::<Vec<_>>(),
        plan(pattern, ci).unwrap().describe(),
        docs.iter()
            .map(|d| String::from_utf8_lossy(d).into_owned())
            .collect::<Vec<_>>(),
    );
}

fn regex_strategy() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![
        "[a-cA-C]{1,5}".prop_map(String::from),
        Just("[ab]".to_string()),
        Just("[a-c]".to_string()),
        Just(".".to_string()),
        Just(r"\w".to_string()),
        Just(r"\s".to_string()),
        Just("[^a]".to_string()),
        Just("^".to_string()),
        Just("$".to_string()),
        Just(r"\b".to_string()),
        "[a-c]{1,4}".prop_map(|s| format!("(?i:{s})")),
    ];
    leaf.prop_recursive(4, 24, 3, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 2..4).prop_map(|v| v.concat()),
            proptest::collection::vec(inner.clone(), 2..4)
                .prop_map(|v| format!("(?:{})", v.join("|"))),
            inner.clone().prop_map(|s| format!("(?:{s})*")),
            inner.clone().prop_map(|s| format!("(?:{s})+")),
            inner.clone().prop_map(|s| format!("(?:{s})?")),
            inner.clone().prop_map(|s| format!("(?:{s}){{2}}")),
            inner.prop_map(|s| format!("(?:{s}){{1,3}}")),
        ]
    })
}

fn docs_strategy() -> impl Strategy<Value = Vec<Vec<u8>>> {
    proptest::collection::vec("[a-cA-C \n]{0,40}".prop_map(String::into_bytes), 1..8)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2000, ..ProptestConfig::default() })]

    #[test]
    fn index_never_loses_a_match(
        pattern in regex_strategy(),
        docs in docs_strategy(),
        ci in any::<bool>(),
    ) {
        assert_sound(&docs, &pattern, ci);
    }

    /// Any substring of a document, as a literal, must find that document.
    #[test]
    fn literal_substrings_are_found(
        docs in proptest::collection::vec("[a-cA-C \n]{3,40}".prop_map(String::into_bytes), 1..6),
        pick in any::<prop::sample::Index>(),
        from in any::<prop::sample::Index>(),
        len in 1usize..8,
        ci in any::<bool>(),
    ) {
        let doc = &docs[pick.index(docs.len())];
        let start = from.index(doc.len());
        let end = (start + len).min(doc.len());
        let lit = regex::escape(&String::from_utf8_lossy(&doc[start..end]));
        // Only literals that regex-match non-trivially are interesting.
        let truth = brute(&docs, &lit, ci);
        let cands = candidates(&index_docs(&docs), docs.len(), &lit, ci);
        prop_assert!(truth.is_subset(&cands), "literal {lit:?} lost files");
    }

    /// Multi-byte text, which exercises byte-level trigrams and non-ASCII class expansion.
    #[test]
    fn unicode_patterns_are_sound(
        docs in proptest::collection::vec("[aé字K\u{212A} \n]{0,30}".prop_map(String::into_bytes), 1..6),
        pat in "[aé字kK\u{212A}]{1,4}",
        ci in any::<bool>(),
    ) {
        assert_sound(&docs, &regex::escape(&pat), ci);
    }
}

#[test]
fn kelvin_sign_matches_k_case_insensitively() {
    // (?i)k also matches U+212A KELVIN SIGN, whose bytes are not ASCII-folded in the index.
    let docs: Vec<Vec<u8>> = vec![
        "KEL".into(),
        "kel".into(),
        "\u{212A}el".into(),
        "nothing".into(),
    ];
    assert_sound(&docs, "kel", true);
    let cands = candidates(&index_docs(&docs), docs.len(), "kel", true);
    assert!(
        cands.contains(&2),
        "kelvin-sign doc must be a candidate: {cands:?}"
    );
}

#[test]
fn newline_in_pattern_is_sound() {
    let docs: Vec<Vec<u8>> = vec!["foo\nbar".into(), "foo bar".into(), "bar\nfoo".into()];
    for p in [r"foo\nbar", r"o\nb", r"foo\n", r"\nbar"] {
        assert_sound(&docs, p, false);
    }
}

#[test]
fn selective_patterns_really_prune() {
    // Soundness alone is satisfied by returning everything, so also check pruning happens.
    let mut docs: Vec<Vec<u8>> = (0..200)
        .map(|i| format!("filler text number {i}").into_bytes())
        .collect();
    docs.push(b"contains the unusual_identifier here".to_vec());
    docs.push(b"also unusual_identifier twice unusual_identifier".to_vec());
    let built = index_docs(&docs);

    let c = candidates(&built, docs.len(), "unusual_identifier", false);
    assert_eq!(c, BTreeSet::from([200, 201]));

    let c = candidates(&built, docs.len(), "unusual_.*tifier", false);
    assert_eq!(c, BTreeSet::from([200, 201]));

    let c = candidates(&built, docs.len(), "(?i)UNUSUAL_IDENTIFIER", false);
    assert_eq!(c, BTreeSet::from([200, 201]));

    // No usable literal: everything is a candidate and the planner says so.
    assert_eq!(plan(r"\w+", false).unwrap(), Query::All);
    assert_eq!(
        candidates(&built, docs.len(), r"\w+", false).len(),
        docs.len()
    );
}
