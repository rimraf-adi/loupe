//! Regex -> trigram query planner.
//!
//! Produces a boolean [`Query`] over trigrams that is a *necessary* condition for a match: every
//! file the regex matches satisfies it, so the index can only over-approximate, never miss.
//! The verifier then runs the real regex over the candidates.
//!
//! Method (after Russ Cox's codesearch): walk the regex HIR tracking, per node, either
//! an *exact set* of byte strings the node can match (when small), or a trigram query that any
//! match must satisfy. Concatenation multiplies exact sets while they stay small and flushes
//! them into a query otherwise.

use std::collections::BTreeSet;

use loupe_core::{Error, Result};
use regex_syntax::hir::{Class, Hir, HirKind};

use crate::terms::{fold, trigram_key};

/// Largest exact set carried through a concatenation before it is flushed into a query.
const EXACT_LIMIT: usize = 64;
/// Longest exact string kept; longer products are flushed.
const EXACT_MAX_LEN: usize = 256;
/// Character classes with more members than this are treated as "any character".
const CLASS_LIMIT: u64 = 10;
/// Trigrams required per string; more are sampled evenly (still a necessary condition).
const MAX_TRIGRAMS_PER_STRING: usize = 24;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Query {
    /// No constraint: every file is a candidate.
    All,
    /// The regex cannot match anything.
    Never,
    /// File contains this (folded) trigram.
    Tri(u32),
    And(Vec<Query>),
    Or(Vec<Query>),
}

impl Query {
    pub fn and(parts: Vec<Query>) -> Query {
        let mut flat = BTreeSet::new();
        for p in parts {
            match p {
                Query::All => {}
                Query::Never => return Query::Never,
                Query::And(inner) => flat.extend(inner),
                other => {
                    flat.insert(other);
                }
            }
        }
        match flat.len() {
            0 => Query::All,
            1 => flat.into_iter().next().unwrap(),
            _ => Query::And(flat.into_iter().collect()),
        }
    }

    pub fn or(parts: Vec<Query>) -> Query {
        let mut flat = BTreeSet::new();
        for p in parts {
            match p {
                Query::Never => {}
                Query::All => return Query::All,
                Query::Or(inner) => flat.extend(inner),
                other => {
                    flat.insert(other);
                }
            }
        }
        match flat.len() {
            0 => Query::Never,
            1 => flat.into_iter().next().unwrap(),
            _ => Query::Or(flat.into_iter().collect()),
        }
    }

    /// Human-readable form for `--explain`, e.g. `(hel & ell & llo) | wor`.
    pub fn describe(&self) -> String {
        match self {
            Query::All => "ALL".into(),
            Query::Never => "NEVER".into(),
            Query::Tri(t) => {
                let b = [(t >> 16) as u8, (t >> 8) as u8, *t as u8];
                format!("{:?}", String::from_utf8_lossy(&b))
            }
            Query::And(v) => format!(
                "({})",
                v.iter()
                    .map(Query::describe)
                    .collect::<Vec<_>>()
                    .join(" & ")
            ),
            Query::Or(v) => format!(
                "({})",
                v.iter()
                    .map(Query::describe)
                    .collect::<Vec<_>>()
                    .join(" | ")
            ),
        }
    }
}

/// Plan a pattern with the same flags the verifier compiles it with.
pub fn plan(pattern: &str, case_insensitive: bool) -> Result<Query> {
    let hir = regex_syntax::ParserBuilder::new()
        .case_insensitive(case_insensitive)
        .multi_line(true)
        .utf8(false)
        .build()
        .parse(pattern)
        .map_err(|e| Error::InvalidArgument(format!("invalid regex: {e}")))?;
    let info = walk(&hir);
    Ok(match info.exact {
        Some(set) => exact_to_query(&set),
        None => info.query,
    })
}

type Exact = BTreeSet<Vec<u8>>;

struct Info {
    /// When `Some`, the node matches exactly these strings and `query` is unused.
    exact: Option<Exact>,
    query: Query,
}

impl Info {
    fn exact(set: Exact) -> Info {
        Info {
            exact: Some(set),
            query: Query::All,
        }
    }
    fn query(query: Query) -> Info {
        Info { exact: None, query }
    }
    fn any() -> Info {
        Info::query(Query::All)
    }
    fn into_query(self) -> Query {
        match self.exact {
            Some(set) => exact_to_query(&set),
            None => self.query,
        }
    }
}

fn empty_string() -> Exact {
    BTreeSet::from([Vec::new()])
}

fn fold_bytes(b: &[u8]) -> Vec<u8> {
    b.iter().map(|&c| fold(c)).collect()
}

fn walk(hir: &Hir) -> Info {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => Info::exact(empty_string()),
        HirKind::Literal(lit) => Info::exact(BTreeSet::from([fold_bytes(&lit.0)])),
        HirKind::Class(class) => class_info(class),
        HirKind::Capture(cap) => walk(&cap.sub),
        HirKind::Repetition(rep) => {
            if rep.min == 0 {
                Info::any()
            } else if rep.min == 1 && rep.max == Some(1) {
                walk(&rep.sub)
            } else {
                // One copy of the sub-pattern must appear.
                Info::query(walk(&rep.sub).into_query())
            }
        }
        HirKind::Concat(subs) => concat(subs.iter().map(walk).collect()),
        HirKind::Alternation(subs) => {
            let infos: Vec<Info> = subs.iter().map(walk).collect();
            if infos.iter().all(|i| i.exact.is_some()) {
                let union: Exact = infos
                    .iter()
                    .flat_map(|i| i.exact.as_ref().unwrap().iter().cloned())
                    .collect();
                if union.len() <= EXACT_LIMIT {
                    return Info::exact(union);
                }
            }
            Info::query(Query::or(infos.into_iter().map(Info::into_query).collect()))
        }
    }
}

fn class_info(class: &Class) -> Info {
    let mut members: Exact = BTreeSet::new();
    match class {
        Class::Unicode(c) => {
            let total: u64 = c
                .ranges()
                .iter()
                .map(|r| u64::from(r.end() as u32 - r.start() as u32) + 1)
                .sum();
            if total > CLASS_LIMIT {
                return Info::any();
            }
            for r in c.ranges() {
                for ch in r.start()..=r.end() {
                    let mut buf = [0u8; 4];
                    members.insert(fold_bytes(ch.encode_utf8(&mut buf).as_bytes()));
                }
            }
        }
        Class::Bytes(c) => {
            let total: u64 = c
                .ranges()
                .iter()
                .map(|r| u64::from(r.end() - r.start()) + 1)
                .sum();
            if total > CLASS_LIMIT {
                return Info::any();
            }
            for r in c.ranges() {
                for b in r.start()..=r.end() {
                    members.insert(vec![fold(b)]);
                }
            }
        }
    }
    // An empty class matches nothing; `exact_to_query` turns the empty set into `Never`.
    Info::exact(members)
}

fn concat(children: Vec<Info>) -> Info {
    let mut cur = empty_string();
    let mut parts: Vec<Query> = Vec::new();
    let mut flushed = false;

    for child in children {
        let Some(set) = child.exact else {
            parts.push(exact_to_query(&cur));
            parts.push(child.query);
            cur = empty_string();
            flushed = true;
            continue;
        };
        let product_len = cur.len().saturating_mul(set.len());
        let longest = cur.iter().map(Vec::len).max().unwrap_or(0)
            + set.iter().map(Vec::len).max().unwrap_or(0);
        if product_len <= EXACT_LIMIT && longest <= EXACT_MAX_LEN {
            cur = cur
                .iter()
                .flat_map(|a| {
                    set.iter().map(move |b| {
                        let mut s = a.clone();
                        s.extend_from_slice(b);
                        s
                    })
                })
                .collect();
        } else {
            parts.push(exact_to_query(&cur));
            cur = set;
            flushed = true;
        }
    }

    if !flushed {
        return Info::exact(cur);
    }
    parts.push(exact_to_query(&cur));
    Info::query(Query::and(parts))
}

/// A match is one of `set`, so the file contains all trigrams of at least one string.
fn exact_to_query(set: &Exact) -> Query {
    let alternatives = set.iter().map(|s| string_to_query(s)).collect();
    Query::or(alternatives)
}

fn string_to_query(s: &[u8]) -> Query {
    // Windows containing a newline are not indexed, so they cannot be required.
    let mut tris: Vec<u32> = s
        .windows(3)
        .filter(|w| !w.contains(&b'\n'))
        .map(|w| trigram_key(w[0], w[1], w[2]))
        .collect();
    tris.sort_unstable();
    tris.dedup();
    if tris.len() > MAX_TRIGRAMS_PER_STRING {
        let step = tris.len() as f64 / MAX_TRIGRAMS_PER_STRING as f64;
        tris = (0..MAX_TRIGRAMS_PER_STRING)
            .map(|i| tris[(i as f64 * step) as usize])
            .collect();
    }
    Query::and(tris.into_iter().map(Query::Tri).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(pattern: &str) -> String {
        plan(pattern, false).unwrap().describe()
    }

    #[test]
    fn literal_becomes_trigram_and() {
        assert_eq!(d("hello"), r#"("ell" & "hel" & "llo")"#);
    }

    #[test]
    fn short_or_unconstrained_is_all() {
        assert_eq!(d("ab"), "ALL");
        assert_eq!(d(""), "ALL");
        assert_eq!(d("a+"), "ALL");
        assert_eq!(d(".*"), "ALL");
        assert_eq!(d(r"\w+"), "ALL");
    }

    #[test]
    fn alternation_is_or() {
        assert_eq!(d("abc|xyz"), r#"("abc" | "xyz")"#);
        // One short branch means that branch could match anything.
        assert_eq!(d("abc|x"), "ALL");
    }

    #[test]
    fn literals_on_both_sides_of_a_wildcard_are_both_required() {
        assert_eq!(d("foo.*bar"), r#"("bar" & "foo")"#);
        assert_eq!(d(r"foo\d+bar"), r#"("bar" & "foo")"#);
    }

    #[test]
    fn small_classes_expand_and_large_ones_break() {
        // 2 x 2 expansions: abcX / abcY forms share trigrams.
        assert_eq!(
            d("ab[cd]ef"),
            r#"(("abc" & "bce" & "cef") | ("abd" & "bde" & "def"))"#
        );
        assert_eq!(d("foo[a-z]bar"), r#"("bar" & "foo")"#);
    }

    #[test]
    fn case_insensitive_folds_to_one_string() {
        assert_eq!(
            plan("HeLLo", true).unwrap().describe(),
            r#"("ell" & "hel" & "llo")"#
        );
        // Case-sensitive planning folds too, since the index is folded.
        assert_eq!(d("HeLLo"), r#"("ell" & "hel" & "llo")"#);
    }

    #[test]
    fn repetition_keeps_one_copy() {
        assert_eq!(d("(abc)+"), r#""abc""#);
        assert_eq!(d("(abc)*"), "ALL");
        assert_eq!(d("(abc)?x"), "ALL");
    }

    #[test]
    fn anchors_and_boundaries_are_transparent() {
        assert_eq!(d(r"^\bfoobar\b$"), r#"("bar" & "foo" & "oba" & "oob")"#);
    }

    #[test]
    fn newline_trigrams_are_never_required() {
        assert_eq!(d("ab\ncd"), "ALL");
        assert_eq!(d("abc\ndef"), r#"("abc" & "def")"#);
    }

    #[test]
    fn empty_class_never_matches() {
        assert_eq!(d(r"[^\x00-\x{10FFFF}]"), "NEVER");
        assert_eq!(d(r"abc[^\x00-\x{10FFFF}]"), "NEVER");
    }

    #[test]
    fn long_literals_cap_the_trigram_count() {
        let q = plan(&"abcdefghijklmnopqrstuvwxyz".repeat(5), false).unwrap();
        let Query::And(v) = q else {
            panic!("expected And, got {q:?}")
        };
        assert!(v.len() <= MAX_TRIGRAMS_PER_STRING);
    }

    #[test]
    fn invalid_regex_is_an_error() {
        assert!(matches!(plan("(", false), Err(Error::InvalidArgument(_))));
    }

    #[test]
    fn and_or_simplify() {
        assert_eq!(Query::and(vec![]), Query::All);
        assert_eq!(Query::or(vec![]), Query::Never);
        assert_eq!(Query::and(vec![Query::All, Query::Tri(1)]), Query::Tri(1));
        assert_eq!(Query::or(vec![Query::Never, Query::Tri(1)]), Query::Tri(1));
        assert_eq!(Query::and(vec![Query::Tri(1), Query::Never]), Query::Never);
        assert_eq!(Query::or(vec![Query::Tri(1), Query::All]), Query::All);
        assert_eq!(
            Query::and(vec![
                Query::Tri(2),
                Query::And(vec![Query::Tri(1), Query::Tri(2)])
            ]),
            Query::And(vec![Query::Tri(1), Query::Tri(2)])
        );
    }
}
