//! Semantic search: embed the query, scan segment vectors, merge, and map chunks back to paths.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use loupe_core::{BlobId, Result};
use loupe_overlay::Overlay;
use loupe_store::{SnapshotView, Store};
use loupe_symbols::{Registry, extract_symbols};
use serde::{Deserialize, Serialize};

use crate::chunk::{ChunkOptions, chunk_file};
use crate::embedder::{Embedder, dot_i8, quantize};
use crate::store::{EmbedIndex, QuantQuery};

const QUERY_CACHE_CAP: usize = 64;
/// Candidates fetched per segment relative to `k`, so filtering (dead blobs, scope) still leaves
/// enough results.
const OVERFETCH: usize = 12;
const SNIPPET_LINE_BYTES: usize = 160;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SemanticOptions {
    pub k: usize,
    /// Only files under this path prefix.
    pub scope: Option<String>,
    /// At most this many chunks per file, so one big file cannot fill the list.
    pub per_file: usize,
    /// Lines of each chunk to include as the snippet.
    pub snippet_lines: usize,
}

impl Default for SemanticOptions {
    fn default() -> Self {
        SemanticOptions {
            k: 10,
            scope: None,
            per_file: 2,
            snippet_lines: 8,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SemanticHit {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    /// Cosine similarity in `[-1, 1]` (int8-quantised, so accurate to about 0.01).
    pub score: f32,
    /// `Parent > name` for definitions, `L10-49` for windows.
    pub breadcrumb: String,
    pub snippet: String,
    /// The file differs on disk from the snapshot this chunk was embedded from, so the lines and
    /// snippet may not match the current file. Never set for overlay-sourced hits.
    pub stale: bool,
}

/// How much of the index the answer could draw on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SemanticState {
    /// Every live segment has embeddings from the current model.
    Full,
    /// Some segments lack them (or were embedded by a different model); their files are not
    /// searchable semantically yet.
    Partial,
    /// No segment has usable embeddings.
    Off,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SemanticResult {
    pub snapshot_id: u64,
    pub state: SemanticState,
    pub hits: Vec<SemanticHit>,
    /// Chunks scored across all segments (and the overlay).
    pub chunks_scanned: usize,
}

pub struct SemanticSearcher {
    root: PathBuf,
    view: SnapshotView,
    embedder: Arc<dyn Embedder>,
    chunk_opts: ChunkOptions,
    queries: Mutex<HashMap<String, Arc<QuantQuery>>>,
}

impl std::fmt::Debug for SemanticSearcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SemanticSearcher")
            .field("view", &self.view)
            .field("model", &self.embedder.model_id())
            .finish()
    }
}

struct Candidate {
    score: f32,
    path: String,
    start_line: u32,
    end_line: u32,
    breadcrumb: String,
    /// Content for the snippet when it does not come from disk (overlay files).
    inline: Option<Arc<[u8]>>,
}

fn snippet(bytes: &[u8], start: u32, end: u32, max_lines: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .skip(start.saturating_sub(1) as usize)
        .take(((end + 1).saturating_sub(start) as usize).min(max_lines))
        .map(|l| {
            if l.len() <= SNIPPET_LINE_BYTES {
                return l.to_string();
            }
            let mut cut = SNIPPET_LINE_BYTES;
            while !l.is_char_boundary(cut) {
                cut -= 1;
            }
            format!("{}…", &l[..cut])
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl SemanticSearcher {
    pub fn open(
        root: impl Into<PathBuf>,
        store: &Store,
        embedder: Arc<dyn Embedder>,
        chunk_opts: ChunkOptions,
    ) -> Result<SemanticSearcher> {
        Ok(SemanticSearcher {
            root: root.into(),
            view: SnapshotView::open(store)?,
            embedder,
            chunk_opts,
            queries: Mutex::new(HashMap::new()),
        })
    }

    pub fn snapshot_id(&self) -> u64 {
        self.view.snapshot_id()
    }

    fn query_vector(&self, text: &str) -> Result<Arc<QuantQuery>> {
        if let Some(q) = self.queries.lock().unwrap().get(text) {
            return Ok(q.clone());
        }
        let (scale, values) = quantize(&self.embedder.embed_query(text)?);
        let q = Arc::new(QuantQuery { scale, values });
        let mut cache = self.queries.lock().unwrap();
        if cache.len() >= QUERY_CACHE_CAP {
            cache.clear();
        }
        cache.insert(text.to_string(), q.clone());
        Ok(q)
    }

    pub fn search(
        &self,
        query: &str,
        opts: &SemanticOptions,
        overlay: Option<&Overlay>,
    ) -> Result<SemanticResult> {
        let q = self.query_vector(query)?;
        let model = self.embedder.model_id();
        let owned = |p: &str| overlay.is_some_and(|o| o.get(p).is_some());
        let in_scope = |p: &str| opts.scope.as_deref().is_none_or(|s| p.starts_with(s));
        let want = (opts.k.max(1) * OVERFETCH).max(64);

        let mut usable = 0usize;
        let segments = self.view.segments();
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut scanned = 0usize;
        for (seg_idx, seg) in segments.iter().enumerate() {
            let Some(idx) = EmbedIndex::from_segment(seg)? else {
                continue;
            };
            if idx.model_id() != model || idx.dim() != self.embedder.dim() {
                continue;
            }
            usable += 1;
            scanned += idx.len();
            for (score, i) in idx.top(&q, want) {
                let meta = idx.chunk(i)?;
                let Some(blob) = seg.blob(meta.blob) else {
                    continue;
                };
                if self.view.home_of(&blob).map(|(s, _)| s) != Some(seg_idx) {
                    continue;
                }
                for path in self.view.paths_of(&blob) {
                    if owned(path) || !in_scope(path) {
                        continue;
                    }
                    candidates.push(Candidate {
                        score,
                        path: path.clone(),
                        start_line: meta.start_line,
                        end_line: meta.end_line,
                        breadcrumb: meta.breadcrumb.clone(),
                        inline: None,
                    });
                }
            }
        }

        if let Some(o) = overlay {
            for (path, bytes) in o.puts().into_iter().filter(|(p, _)| in_scope(p)) {
                let head = &bytes[..bytes.len().min(256)];
                let lang = loupe_walk::detect_language(path, head);
                let symbols = extract_symbols(Registry::global(), lang, bytes);
                let chunks = chunk_file(bytes, &symbols, &self.chunk_opts, &model);
                if chunks.is_empty() {
                    continue;
                }
                let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
                let vectors = self.embedder.embed_batch(&texts)?;
                scanned += chunks.len();
                for (c, v) in chunks.into_iter().zip(vectors) {
                    let (scale, values) = quantize(&v);
                    let score = dot_i8(&q.values, &values) as f32 * q.scale * scale;
                    candidates.push(Candidate {
                        score,
                        path: path.to_string(),
                        start_line: c.start_line,
                        end_line: c.end_line,
                        breadcrumb: c.breadcrumb,
                        inline: Some(bytes.clone()),
                    });
                }
            }
        }

        candidates.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.path.cmp(&b.path))
                .then(a.start_line.cmp(&b.start_line))
        });
        let mut per_file: HashMap<&str, usize> = HashMap::new();
        let mut hits = Vec::new();
        for c in &candidates {
            if hits.len() >= opts.k {
                break;
            }
            let n = per_file.entry(c.path.as_str()).or_default();
            if *n >= opts.per_file {
                continue;
            }
            *n += 1;
            // The snippet is read from the file as it is now; if that differs from what was
            // embedded, the hit says so (exact, by content hash).
            let (bytes, stale): (Vec<u8>, bool) = match &c.inline {
                Some(b) => (b.to_vec(), false),
                None => match fs::read(self.root.join(&c.path)) {
                    Ok(b) => {
                        let stale = self
                            .view
                            .snapshot()
                            .get(&c.path)
                            .is_some_and(|m| BlobId::of(&b) != m.blob);
                        (b, stale)
                    }
                    Err(_) => (Vec::new(), true),
                },
            };
            hits.push(SemanticHit {
                path: c.path.clone(),
                start_line: c.start_line,
                end_line: c.end_line,
                score: c.score,
                breadcrumb: c.breadcrumb.clone(),
                snippet: snippet(&bytes, c.start_line, c.end_line, opts.snippet_lines),
                stale,
            });
            if hits.len() >= opts.k {
                break;
            }
        }

        let state = match (usable, segments.len()) {
            (_, 0) => SemanticState::Full,
            (0, _) => SemanticState::Off,
            (u, n) if u == n => SemanticState::Full,
            _ => SemanticState::Partial,
        };
        Ok(SemanticResult {
            snapshot_id: self.view.snapshot_id(),
            state,
            hits,
            chunks_scanned: scanned,
        })
    }

    /// Count of local blobs that have embeddings, for diagnostics.
    pub fn indexed_chunks(&self) -> usize {
        self.view
            .segments()
            .iter()
            .filter_map(|s| EmbedIndex::from_segment(s).ok().flatten())
            .map(|i| i.len())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_takes_the_chunk_lines_and_truncates() {
        let text = "l1\nl2\nl3\nl4\nl5\n";
        assert_eq!(snippet(text.as_bytes(), 2, 4, 10), "l2\nl3\nl4");
        assert_eq!(
            snippet(text.as_bytes(), 2, 5, 2),
            "l2\nl3",
            "snippet_lines caps it"
        );
        assert_eq!(snippet(text.as_bytes(), 9, 12, 5), "", "past the end");
        let long = format!("{}\n", "é".repeat(300));
        let s = snippet(long.as_bytes(), 1, 1, 3);
        assert!(
            s.ends_with('…') && s.len() <= SNIPPET_LINE_BYTES + 3,
            "{}",
            s.len()
        );
    }
}
