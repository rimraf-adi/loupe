//! Assemble a [`Graph`] from a snapshot's per-blob facts: resolved imports, test links, name
//! references, and git co-change.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use loupe_core::{LocalBlobId, Result};
use loupe_store::SnapshotView;
use loupe_symbols::{SymbolIndex, SymbolKind};
use loupe_text::TextIndex;

use crate::cochange::{self, CoChangeOptions};
use crate::graph::{EdgeKind, Graph, GraphBuilder};
use crate::imports::{ImportIndex, PathIndex, resolve_all};

#[derive(Clone, Debug)]
pub struct GraphOptions {
    /// Heuristic name-reference edges (needs the text and symbol indexes).
    pub refs: bool,
    pub cochange: Option<CoChangeOptions>,
    /// Ignore identifiers shorter than this when matching references.
    pub min_name_len: usize,
    /// Skip names defined in more than this many files (too ambiguous to mean anything).
    pub max_definers: usize,
    /// Skip names that appear in more than this many files (too common to be informative).
    pub max_users: usize,
    /// Reference edges kept per referencing file.
    pub max_refs_per_file: usize,
}

impl Default for GraphOptions {
    fn default() -> Self {
        GraphOptions {
            refs: true,
            cochange: Some(CoChangeOptions::default()),
            min_name_len: 4,
            max_definers: 3,
            max_users: 300,
            max_refs_per_file: 30,
        }
    }
}

pub fn build_graph(root: &Path, view: &SnapshotView, opts: &GraphOptions) -> Result<Graph> {
    let snapshot = view.snapshot();
    let paths: Vec<String> = snapshot.files.keys().cloned().collect();
    let pidx = PathIndex::new(paths.iter().map(String::as_str));
    let mut builder = GraphBuilder::new(paths.len());

    let import_pairs = add_imports(view, &pidx, &mut builder)?;
    add_test_edges(&paths, &pidx, &import_pairs, &mut builder);
    if opts.refs {
        add_refs(view, &pidx, opts, &mut builder)?;
    }
    if let Some(co) = &opts.cochange {
        for (a, b, strength) in cochange::mine(root, |p| pidx.id_of(p).is_some(), co) {
            if let (Some(a), Some(b)) = (pidx.id_of(&a), pidx.id_of(&b)) {
                builder.add(a, b, EdgeKind::CoChange, strength);
            }
        }
    }
    Ok(builder.finish(paths))
}

/// Resolve each blob's stored imports for every path holding it; returns the `(from, to)` pairs.
fn add_imports(
    view: &SnapshotView,
    pidx: &PathIndex,
    builder: &mut GraphBuilder,
) -> Result<Vec<(u32, u32)>> {
    let mut pairs = Vec::new();
    for (seg_idx, seg) in view.segments().iter().enumerate() {
        let Some(imports) = ImportIndex::from_segment(seg)? else {
            continue;
        };
        for (local, blob) in seg.blobs().iter().enumerate() {
            if view.home_of(blob) != Some((seg_idx, LocalBlobId(local as u32))) {
                continue;
            }
            let raws = imports.imports_of(LocalBlobId(local as u32))?;
            if raws.is_empty() {
                continue;
            }
            for path in view.paths_of(blob) {
                let (Some(from), Some(meta)) = (pidx.id_of(path), view.snapshot().get(path)) else {
                    continue;
                };
                let Some(lang) = meta.lang.as_deref() else {
                    continue;
                };
                for to in resolve_all(pidx, lang, path, &raws) {
                    builder.add(from, to, EdgeKind::Imports, 1.0);
                    pairs.push((from, to));
                }
            }
        }
    }
    Ok(pairs)
}

// ---- tests edges ----------------------------------------------------------------------

fn is_test_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let in_test_dir = path
        .split('/')
        .rev()
        .skip(1)
        .any(|c| matches!(c, "test" | "tests" | "__tests__" | "spec" | "specs"));
    in_test_dir || test_stem(name).is_some()
}

/// The source stem a test file name refers to: `foo_test.go`, `test_foo.py`, `foo.test.ts`,
/// `foo.spec.js`, `FooTest.java`, `FooTests.cs`.
fn test_stem(name: &str) -> Option<String> {
    let stem = name.split('.').next().unwrap_or(name);
    let lower_name = name.to_ascii_lowercase();
    if let Some(s) = stem
        .strip_suffix("_test")
        .or_else(|| stem.strip_suffix("_spec"))
    {
        return Some(s.to_string());
    }
    if let Some(s) = stem.strip_prefix("test_") {
        return Some(s.to_string());
    }
    let parts: Vec<&str> = lower_name.split('.').collect();
    if parts.len() >= 3 && matches!(parts[parts.len() - 2], "test" | "spec") {
        return Some(stem.to_string());
    }
    for suffix in ["Tests", "Test"] {
        if let Some(s) = stem.strip_suffix(suffix)
            && !s.is_empty()
        {
            return Some(s.to_string());
        }
    }
    None
}

fn source_stem(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.split('.').next().unwrap_or(name).to_ascii_lowercase()
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.split('/')
        .zip(b.split('/'))
        .take_while(|(x, y)| x == y)
        .count()
}

fn add_test_edges(
    paths: &[String],
    pidx: &PathIndex,
    import_pairs: &[(u32, u32)],
    builder: &mut GraphBuilder,
) {
    let is_test: Vec<bool> = paths.iter().map(|p| is_test_path(p)).collect();
    let mut by_stem: HashMap<String, Vec<u32>> = HashMap::new();
    for (i, p) in paths.iter().enumerate() {
        if !is_test[i] {
            by_stem.entry(source_stem(p)).or_default().push(i as u32);
        }
    }

    // Naming convention: `foo_test.rs` tests `foo.rs`.
    for (i, p) in paths.iter().enumerate().filter(|(i, _)| is_test[*i]) {
        let name = p.rsplit('/').next().unwrap_or(p);
        let stem = test_stem(name)
            .unwrap_or_else(|| source_stem(p))
            .to_ascii_lowercase();
        let Some(candidates) = by_stem.get(&stem) else {
            continue;
        };
        let mut ranked: Vec<u32> = candidates.clone();
        ranked.sort_by_key(|&c| std::cmp::Reverse(common_prefix_len(pidx.path(c), p)));
        ranked.truncate(3);
        let strength = if candidates.len() == 1 { 0.9 } else { 0.6 };
        for c in ranked {
            builder.add(i as u32, c, EdgeKind::Tests, strength);
        }
    }

    // A test file importing a non-test file is testing it (or at least exercising it).
    let mut seen = HashSet::new();
    for &(from, to) in import_pairs {
        if is_test[from as usize] && !is_test[to as usize] && seen.insert((from, to)) {
            builder.add(from, to, EdgeKind::Tests, 0.7);
        }
    }
}

// ---- refs edges -----------------------------------------------------------------------

/// Heuristic references: file U mentions an identifier that file D defines, where the name is
/// distinctive (few definers, not ubiquitous). Confidence grows with the number of distinct
/// shared names and shrinks with how common each name is.
fn add_refs(
    view: &SnapshotView,
    pidx: &PathIndex,
    opts: &GraphOptions,
    builder: &mut GraphBuilder,
) -> Result<()> {
    let segments = view.segments();
    let text: Vec<TextIndex<'_>> = segments
        .iter()
        .map(TextIndex::from_segment)
        .collect::<Result<_>>()?;
    let symbols: Vec<Option<SymbolIndex<'_>>> = segments
        .iter()
        .map(SymbolIndex::from_segment)
        .collect::<Result<_>>()?;

    let ids_of_blob = |seg: usize, local: u32| -> Vec<u32> {
        segments[seg]
            .blob(LocalBlobId(local))
            .filter(|b| view.home_of(b).map(|(s, _)| s) == Some(seg))
            .map(|b| {
                view.paths_of(&b)
                    .iter()
                    .filter_map(|p| pidx.id_of(p))
                    .collect()
            })
            .unwrap_or_default()
    };

    let mut sums: HashMap<(u32, u32), f32> = HashMap::new();
    for (seg_idx, sym) in symbols.iter().enumerate() {
        let Some(sym) = sym else { continue };
        for name in sym.names() {
            if name.len() < opts.min_name_len {
                continue;
            }
            // Files defining the name as something worth referencing (not a bare method name).
            let mut definers: HashSet<u32> = HashSet::new();
            for r in sym.find(&name)? {
                let kind = sym.symbol(r)?.kind;
                if matches!(
                    kind,
                    SymbolKind::Method
                        | SymbolKind::Constant
                        | SymbolKind::Variable
                        | SymbolKind::Impl
                ) {
                    continue;
                }
                definers.extend(ids_of_blob(seg_idx, r.blob.0));
            }
            if definers.is_empty() || definers.len() > opts.max_definers {
                continue;
            }
            let mut users: HashSet<u32> = HashSet::new();
            for (t_idx, t) in text.iter().enumerate() {
                for local in t.token_postings(name.as_bytes())? {
                    users.extend(ids_of_blob(t_idx, local));
                }
                if users.len() > opts.max_users {
                    break;
                }
            }
            users.retain(|u| !definers.contains(u));
            if users.is_empty() || users.len() > opts.max_users {
                continue;
            }
            let weight = 1.0 / definers.len() as f32 / (2.0 + users.len() as f32).ln();
            for &u in &users {
                for &d in &definers {
                    *sums.entry((u, d)).or_default() += weight;
                }
            }
        }
    }

    // Saturating confidence, strongest `max_refs_per_file` per referencing file.
    let mut per_user: HashMap<u32, Vec<(u32, f32)>> = HashMap::new();
    for ((u, d), s) in sums {
        let strength = 1.0 - (-s).exp();
        if strength >= 0.15 {
            per_user.entry(u).or_default().push((d, strength));
        }
    }
    let mut users: Vec<u32> = per_user.keys().copied().collect();
    users.sort_unstable();
    for u in users {
        let list = per_user.get_mut(&u).unwrap();
        list.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        for &(d, strength) in list.iter().take(opts.max_refs_per_file) {
            builder.add(u, d, EdgeKind::Refs, strength);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_detection() {
        for p in [
            "src/foo_test.go",
            "test_foo.py",
            "app/foo.test.ts",
            "app/foo.spec.js",
            "src/FooTest.java",
            "src/FooTests.cs",
            "tests/anything.rs",
            "pkg/__tests__/x.js",
        ] {
            assert!(is_test_path(p), "{p}");
        }
        for p in [
            "src/foo.go",
            "src/latest.rs",
            "src/contest.py",
            "protest/x.rs",
            "src/attest.ts",
        ] {
            assert!(!is_test_path(p), "{p}");
        }
    }

    #[test]
    fn test_stems() {
        assert_eq!(test_stem("foo_test.go").as_deref(), Some("foo"));
        assert_eq!(test_stem("test_foo.py").as_deref(), Some("foo"));
        assert_eq!(test_stem("foo.test.ts").as_deref(), Some("foo"));
        assert_eq!(test_stem("foo.spec.js").as_deref(), Some("foo"));
        assert_eq!(test_stem("FooTest.java").as_deref(), Some("Foo"));
        assert_eq!(test_stem("Test.java"), None, "bare 'Test' names no source");
        assert_eq!(test_stem("foo.rs"), None);
    }
}
