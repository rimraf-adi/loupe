//! Symbol queries over a snapshot: `find`, name-prefix completion, and `outline`.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use loupe_core::Result;
use loupe_overlay::Overlay;
use loupe_store::{SnapshotView, Store};
use serde::{Deserialize, Serialize};

use crate::extract::extract_symbols;
use crate::index::SymbolIndex;
use crate::registry::Registry;
use crate::symbol::{Symbol, SymbolKind, Visibility};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolHit {
    pub path: String,
    pub name: String,
    pub kind: SymbolKind,
    pub visibility: Visibility,
    pub signature: String,
    pub doc: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    /// Name of the enclosing definition, if any.
    pub parent: Option<String>,
    /// The file differs on disk from the snapshot this hit came from, so the signature and line
    /// numbers may be out of date. Never set for overlay-sourced hits.
    pub stale: bool,
}

/// Where an outline's symbols came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutlineSource {
    /// The segment's symbol index.
    Index,
    /// An unsaved overlay buffer, parsed on demand.
    Overlay,
    /// The file on disk, parsed on demand (not yet indexed).
    Disk,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outline {
    pub path: String,
    pub lang: Option<String>,
    pub symbols: Vec<Symbol>,
    pub source: OutlineSource,
    /// The file differs on disk from the snapshot the symbols were read from.
    pub stale: bool,
}

pub struct SymbolSearcher {
    root: PathBuf,
    view: SnapshotView,
    registry: &'static Registry,
}

impl std::fmt::Debug for SymbolSearcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SymbolSearcher")
            .field("view", &self.view)
            .finish()
    }
}

fn lang_of(path: &str, bytes: &[u8]) -> Option<&'static str> {
    loupe_walk::detect_language(path, &bytes[..bytes.len().min(256)])
}

fn hit(path: &str, s: &Symbol, all: &[Symbol]) -> SymbolHit {
    SymbolHit {
        path: path.to_string(),
        name: s.name.clone(),
        kind: s.kind,
        visibility: s.visibility,
        signature: s.signature.clone(),
        doc: s.doc.clone(),
        start_line: s.start_line,
        end_line: s.end_line,
        parent: s
            .parent
            .and_then(|p| all.get(p as usize))
            .map(|p| p.name.clone()),
        stale: false,
    }
}

impl SymbolSearcher {
    pub fn open(root: impl Into<PathBuf>, store: &Store) -> Result<SymbolSearcher> {
        Ok(SymbolSearcher {
            root: root.into(),
            view: SnapshotView::open(store)?,
            registry: Registry::global(),
        })
    }

    pub fn snapshot_id(&self) -> u64 {
        self.view.snapshot_id()
    }

    /// Whether `path` differs on disk from the snapshot (exact, by content hash). Files the
    /// overlay owns are never stale; unknown paths are not stale.
    pub fn is_stale(&self, path: &str, overlay: Option<&Overlay>) -> bool {
        if overlay.is_some_and(|o| o.get(path).is_some()) {
            return false;
        }
        self.view
            .snapshot()
            .get(path)
            .is_some_and(|meta| loupe_walk::is_stale(&self.root, path, meta))
    }

    /// The snapshot this searcher is pinned to.
    pub fn snapshot(&self) -> &loupe_core::Snapshot {
        self.view.snapshot()
    }

    /// Definitions named exactly `name`, optionally of one kind, sorted by path then line.
    ///
    /// Snapshot files whose blob is not indexed yet are not searched (the next update indexes
    /// them); overlay files always are.
    pub fn find(
        &self,
        name: &str,
        kind: Option<SymbolKind>,
        limit: usize,
        overlay: Option<&Overlay>,
    ) -> Result<Vec<SymbolHit>> {
        let owned = |p: &str| overlay.is_some_and(|o| o.get(p).is_some());
        let mut hits: Vec<SymbolHit> = Vec::new();

        for (seg_idx, seg) in self.view.segments().iter().enumerate() {
            let Some(idx) = SymbolIndex::from_segment(seg)? else {
                continue;
            };
            for r in idx.find(name)? {
                let Some(blob) = seg.blob(r.blob) else {
                    continue;
                };
                // A blob present in several segments is answered by its first one only.
                if self.view.home_of(&blob).map(|(s, _)| s) != Some(seg_idx) {
                    continue;
                }
                let paths = self.view.paths_of(&blob);
                if paths.is_empty() {
                    continue; // dead blob
                }
                let symbol = idx.symbol(r)?;
                if kind.is_some_and(|k| k != symbol.kind) {
                    continue;
                }
                let siblings = if symbol.parent.is_some() {
                    idx.symbols_of(r.blob)?
                } else {
                    Vec::new()
                };
                for path in paths.iter().filter(|p| !owned(p)) {
                    hits.push(hit(path, &symbol, &siblings));
                }
            }
        }

        if let Some(o) = overlay {
            for (path, bytes) in o.puts() {
                let symbols = extract_symbols(self.registry, lang_of(path, bytes), bytes);
                for s in symbols.iter().filter(|s| s.name == name) {
                    if kind.is_none_or(|k| k == s.kind) {
                        hits.push(hit(path, s, &symbols));
                    }
                }
            }
        }

        hits.sort_by(|a, b| (&a.path, a.start_line).cmp(&(&b.path, b.start_line)));
        hits.truncate(limit);
        // Exact freshness check, once per distinct file among the (few) returned hits.
        let mut memo: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
        for h in &mut hits {
            h.stale = *memo
                .entry(h.path.clone())
                .or_insert_with(|| self.is_stale(&h.path, overlay));
        }
        Ok(hits)
    }

    /// Definitions whose *name* contains at least `min_words` of `words` (case-insensitive
    /// substring), best first. Finds `validate_session_token` for the words "validate", "session",
    /// "token" without knowing the exact name. Returns `(hit, words matched)`.
    pub fn find_by_words(
        &self,
        words: &[String],
        min_words: usize,
        limit: usize,
    ) -> Result<Vec<(SymbolHit, usize)>> {
        if words.is_empty() || min_words == 0 {
            return Ok(Vec::new());
        }
        let lowered: Vec<String> = words.iter().map(|w| w.to_ascii_lowercase()).collect();
        let matches_in = |name: &[u8]| -> usize {
            let lower = name.to_ascii_lowercase();
            lowered
                .iter()
                .filter(|w| lower.windows(w.len()).any(|win| win == w.as_bytes()))
                .count()
        };
        let mut found: Vec<(SymbolHit, usize)> = Vec::new();
        for seg in self.view.segments() {
            let Some(idx) = SymbolIndex::from_segment(seg)? else {
                continue;
            };
            // Cap candidate names per segment so a single-word query cannot fan out unboundedly.
            for name in idx.names_where(|n| matches_in(n) >= min_words, limit * 4) {
                let n = matches_in(name.as_bytes());
                for hit in self.find(&name, None, 8, None)? {
                    found.push((hit, n));
                }
            }
        }
        found.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| a.0.path.cmp(&b.0.path))
                .then(a.0.start_line.cmp(&b.0.start_line))
        });
        found.truncate(limit);
        Ok(found)
    }

    /// Distinct symbol names starting with `prefix`, sorted, at most `limit`.
    pub fn names_with_prefix(
        &self,
        prefix: &str,
        limit: usize,
        overlay: Option<&Overlay>,
    ) -> Result<Vec<String>> {
        let mut names = BTreeSet::new();
        for seg in self.view.segments() {
            if let Some(idx) = SymbolIndex::from_segment(seg)? {
                names.extend(idx.names_with_prefix(prefix, limit));
            }
        }
        if let Some(o) = overlay {
            for (path, bytes) in o.puts() {
                names.extend(
                    extract_symbols(self.registry, lang_of(path, bytes), bytes)
                        .into_iter()
                        .map(|s| s.name)
                        .filter(|n| n.starts_with(prefix)),
                );
            }
        }
        Ok(names.into_iter().take(limit).collect())
    }

    /// Outline of one file, preferring (in order) the overlay, the symbol index, then parsing
    /// the file from disk. `None` if the path is unknown or removed.
    pub fn outline(&self, path: &str, overlay: Option<&Overlay>) -> Result<Option<Outline>> {
        if let Some(o) = overlay {
            match o.get(path) {
                Some(loupe_overlay::Entry::Put(bytes)) => {
                    let lang = lang_of(path, bytes);
                    return Ok(Some(Outline {
                        path: path.to_string(),
                        lang: lang.map(String::from),
                        symbols: extract_symbols(self.registry, lang, bytes),
                        source: OutlineSource::Overlay,
                        stale: false,
                    }));
                }
                Some(loupe_overlay::Entry::Removed) => return Ok(None),
                None => {}
            }
        }
        let Some(meta) = self.view.snapshot().get(path) else {
            return Ok(None);
        };
        if let Some((seg_idx, local)) = self.view.home_of(&meta.blob)
            && let Some(idx) = SymbolIndex::from_segment(&self.view.segments()[seg_idx])?
        {
            return Ok(Some(Outline {
                path: path.to_string(),
                lang: meta.lang.clone(),
                symbols: idx.symbols_of(local)?,
                source: OutlineSource::Index,
                stale: loupe_walk::is_stale(&self.root, path, meta),
            }));
        }
        // Not indexed (or indexed by a segment that predates symbols): parse what is on disk.
        let Ok(bytes) = fs::read(self.root.join(path)) else {
            return Ok(None);
        };
        Ok(Some(Outline {
            path: path.to_string(),
            lang: meta.lang.clone(),
            symbols: extract_symbols(self.registry, meta.lang.as_deref(), &bytes),
            source: OutlineSource::Disk,
            // Parsed from the file as it is now, so it cannot be out of date.
            stale: false,
        }))
    }
}
