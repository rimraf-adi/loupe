//! Public facade: open an index, update it from the working tree, search it.
//!
//! This is the surface the Rust harness embeds. The CLI, benches, and (later) the daemon and MCP
//! server all go through it. M3 extends it with the watcher, overlay, and daemon client.

mod build;
mod session;

use std::collections::HashSet;
use std::sync::Arc;

use build::{EmbedCtx, Item, REQUIRED_SEGMENT_FILES, build_segment};
use loupe_core::ApproxTokenCounter;
use loupe_core::{BlobId, Config, Result, SegmentId};
use loupe_embed::{EmbedIndex, SemanticSearcher};
use loupe_graph::build_graph;
use loupe_map::{DiskFiles, HeuristicSummarizer, MapContext, RepoMapper};
use loupe_store::SnapshotView;
use loupe_store::Store;
use loupe_symbols::SymbolSearcher;
use loupe_walk::{Diff, WalkOptions, WalkStats, scan};

pub use loupe_core::{self as core, Error};
pub use loupe_embed::{
    ChunkOptions, Embedder, HashingEmbedder, SemanticHit, SemanticOptions, SemanticResult,
    SemanticState, VectorCache,
};
pub use loupe_graph::{
    CoChangeOptions, Edge, EdgeKind, Graph, GraphOptions, Impacted, Related, RelatedOptions,
};
pub use loupe_map::{
    CallbackSummarizer, MapOptions, MerkleTree, ModuleInput, ModuleSummary, RepoMap, Summarizer,
    SummaryCache,
};
pub use loupe_query::{
    Evidence, QueryHit, QueryOptions, QueryResult, Signal, render as render_query,
};
pub use loupe_store::GcReport;
pub use loupe_symbols::{
    Outline, OutlineOptions, OutlineSource, Symbol, SymbolHit, SymbolKind, Visibility,
    render_outline,
};
pub use loupe_text::{
    FileHit, FuzzyHit, Overlay, SearchOptions, SearchResult, SearchStats, Snippet, TextSearcher,
};
pub use session::Session;

/// An opened index: one writer (can [`update`](Index::update)) or a read-only reader.
pub struct Index {
    config: Config,
    store: Store,
    overlay: Overlay,
    embedder: Arc<dyn Embedder>,
    chunking: ChunkOptions,
    vectors: VectorCache,
}

impl std::fmt::Debug for Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index")
            .field("root", &self.config.root)
            .field("snapshot_id", &self.store.snapshot_id())
            .field("model", &self.embedder.model_id())
            .finish()
    }
}

#[derive(Default)]
struct Built {
    /// New segments with the number of blobs each indexed.
    segments: Vec<(SegmentId, usize)>,
    skipped: usize,
    chunks_embedded: usize,
    chunks_reused: usize,
}

/// When [`Index::compact`] merges segments.
#[derive(Clone, Debug)]
pub struct CompactPolicy {
    /// Merge any segment at least this fraction of whose blobs are dead.
    pub min_dead_ratio: f64,
    /// Merge the smallest segments until at most this many remain.
    pub max_segments: usize,
}

impl Default for CompactPolicy {
    fn default() -> Self {
        CompactPolicy {
            min_dead_ratio: 0.5,
            max_segments: 8,
        }
    }
}

/// What a compaction did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompactReport {
    /// Segments removed from the live set.
    pub merged: usize,
    /// Segments written to replace them.
    pub written: usize,
    pub blobs_kept: usize,
    /// Dead blobs left behind (no path holds them any more).
    pub blobs_dropped_dead: usize,
    /// Live blobs not re-indexed because their file changed on disk meanwhile.
    pub blobs_skipped_changed: usize,
}

/// What an [`Index::update`] did.
#[derive(Clone, Debug)]
pub struct UpdateReport {
    pub snapshot_id: u64,
    pub files: usize,
    pub diff: Diff,
    pub walk: WalkStats,
    /// New segment and the number of blobs it indexed, if one was written.
    pub new_segment: Option<(SegmentId, usize)>,
    /// Blobs left unindexed because the file changed or vanished mid-update.
    pub skipped_blobs: usize,
    /// Live segments removed because they were unreadable, built by an older version, or
    /// embedded by a different model; their blobs were re-indexed.
    pub dropped_segments: usize,
    /// Segments the update wrote (more than one when the work exceeded `max_segment_bytes`).
    pub segments_written: usize,
    /// Chunks that needed a fresh embedding in this update.
    pub chunks_embedded: usize,
    /// Chunks whose vector was reused from the cache (unchanged text, moved or renamed files).
    pub chunks_reused: usize,
    /// Nothing changed, so nothing was written.
    pub unchanged: bool,
}

static DEFAULT_EMBEDDER: std::sync::OnceLock<Arc<dyn Embedder>> = std::sync::OnceLock::new();

/// Make every [`Index`] opened from now on in this process use `embedder` (call once, early, e.g.
/// from `main`). Returns false if a default was already set. Without it, indexes use the built-in
/// hashing embedder.
pub fn set_default_embedder(embedder: Arc<dyn Embedder>) -> bool {
    DEFAULT_EMBEDDER.set(embedder).is_ok()
}

impl Index {
    /// Open for writing (takes the writer lock; creates the index directory if needed).
    pub fn open_writer(config: Config) -> Result<Index> {
        let store = Store::open_writer(&config.index_dir)?;
        Ok(Index::assemble(config, store))
    }

    /// Open read-only. The index directory must exist.
    pub fn open_reader(config: Config) -> Result<Index> {
        let store = Store::open_reader(&config.index_dir)?;
        Ok(Index::assemble(config, store))
    }

    fn assemble(config: Config, store: Store) -> Index {
        let embedder: Arc<dyn Embedder> = DEFAULT_EMBEDDER
            .get()
            .cloned()
            .unwrap_or_else(|| Arc::new(HashingEmbedder::default()));
        let vectors = VectorCache::open(&config.index_dir.join("cache").join("embeddings.redb"));
        Index {
            config,
            store,
            overlay: Overlay::new(),
            embedder,
            chunking: ChunkOptions::default(),
            vectors,
        }
    }

    /// Use a different embedder or chunking. Segments embedded by another model are dropped and
    /// rebuilt on the next [`Index::update`]; the vector cache is keyed by model, so nothing
    /// stale is reused.
    pub fn with_embedding(mut self, embedder: Arc<dyn Embedder>, chunking: ChunkOptions) -> Index {
        self.embedder = embedder;
        self.chunking = chunking;
        self
    }

    pub fn embedder(&self) -> &Arc<dyn Embedder> {
        &self.embedder
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn overlay(&self) -> &Overlay {
        &self.overlay
    }

    /// Record an unsaved or just-changed file. Queries see it immediately; the next
    /// [`Index::update`] that observes the filesystem after this call absorbs it.
    pub fn put(
        &mut self,
        path: impl Into<String>,
        content: impl Into<std::sync::Arc<[u8]>>,
    ) -> Result<()> {
        self.overlay.put(path, content).map(|_| ())
    }

    /// Record that a file was deleted.
    pub fn remove(&mut self, path: impl Into<String>) {
        self.overlay.remove(path);
    }

    /// Id of the snapshot queries currently run against (0 = nothing indexed yet).
    pub fn snapshot_id(&self) -> u64 {
        self.store.snapshot_id()
    }

    /// Follow commits made by a writer in another process. Readers only.
    pub fn refresh(&mut self) -> Result<()> {
        self.store.refresh()
    }

    /// Bring the index in line with the working tree.
    ///
    /// Walks the repo (re-reading only files whose size/mtime changed), then indexes every blob
    /// in the new snapshot that no live segment holds yet. Selecting by "not indexed" rather than
    /// by "in the diff" means a blob skipped earlier (file edited mid-update) is retried, and an
    /// unchanged repo writes nothing.
    pub fn update(&mut self, force_rehash: bool) -> Result<UpdateReport> {
        let overlay_seq = self.overlay.seq();
        let prev = (self.store.snapshot_id() > 0)
            .then(|| self.store.load_snapshot())
            .transpose()?;
        let result = scan(&self.config, prev.as_ref(), &WalkOptions { force_rehash })?;

        // Keep only segments that open cleanly and have every file the current code needs. A
        // corrupt segment, or one built by an older version (e.g. before symbols existed), is
        // dropped from the live set; its blobs then count as unindexed and are rebuilt below.
        let mut kept = Vec::new();
        let mut dropped_segments = 0;
        let mut indexed = HashSet::<BlobId>::new();
        for id in self.store.segment_ids() {
            match self.store.open_segment(id) {
                Ok(seg) => match REQUIRED_SEGMENT_FILES
                    .iter()
                    .find(|f| seg.file(f).is_none())
                {
                    None if self.embedded_by_other_model(&seg) => {
                        tracing::info!(segment = id.0, "re-embedding segment: model changed");
                        dropped_segments += 1;
                    }
                    None => {
                        indexed.extend(seg.blobs().iter().copied());
                        kept.push(id);
                    }
                    Some(missing_file) => {
                        tracing::info!(segment = id.0, missing_file, "rebuilding outdated segment");
                        dropped_segments += 1;
                    }
                },
                Err(err) => {
                    tracing::warn!(segment = id.0, %err, "dropping unreadable segment; it will be rebuilt");
                    dropped_segments += 1;
                }
            }
        }
        let mut seen = HashSet::new();
        let missing: Vec<Item> = result
            .snapshot
            .files
            .iter()
            .filter(|(_, m)| !indexed.contains(&m.blob) && seen.insert(m.blob))
            .map(|(path, m)| Item {
                blob: m.blob,
                path: path.clone(),
                lang: m.lang.clone(),
                size: m.size,
            })
            .collect();

        let unchanged = prev
            .as_ref()
            .is_some_and(|p| p.files == result.snapshot.files)
            && missing.is_empty()
            && dropped_segments == 0;
        let mut new_segment = None;
        let mut skipped_blobs = 0;
        let (mut chunks_embedded, mut chunks_reused) = (0, 0);
        let mut segments_written = 0;
        if !unchanged {
            let mut segments = kept;
            if !missing.is_empty() {
                let built = self.build_items(&missing)?;
                chunks_embedded = built.chunks_embedded;
                chunks_reused = built.chunks_reused;
                skipped_blobs = built.skipped;
                segments_written = built.segments.len();
                if let Some(&(first, _)) = built.segments.first() {
                    new_segment = Some((first, built.segments.iter().map(|(_, n)| n).sum()));
                }
                segments.extend(built.segments.iter().map(|(id, _)| *id));
            }
            self.store.commit(&result.snapshot, &segments)?;
        }
        // The scan started after `overlay_seq`, so it saw everything the overlay held then.
        self.overlay.drop_through(overlay_seq);

        Ok(UpdateReport {
            snapshot_id: self.store.snapshot_id(),
            files: result.snapshot.len(),
            diff: result.diff,
            walk: result.stats,
            new_segment,
            skipped_blobs,
            dropped_segments,
            chunks_embedded,
            chunks_reused,
            segments_written,
            unchanged,
        })
    }

    /// Index `items` into one or more segments, each covering at most
    /// `config.max_segment_bytes` of source so memory stays bounded. Segments are written but not
    /// yet live; the caller commits them.
    fn build_items(&mut self, items: &[Item]) -> Result<Built> {
        let limit = self.config.max_segment_bytes.max(1);
        let mut groups: Vec<&[Item]> = Vec::new();
        let (mut start, mut bytes) = (0usize, 0u64);
        for (i, item) in items.iter().enumerate() {
            if bytes > 0 && bytes + item.size.max(1) > limit {
                groups.push(&items[start..i]);
                (start, bytes) = (i, 0);
            }
            bytes += item.size.max(1);
        }
        if start < items.len() {
            groups.push(&items[start..]);
        }

        let mut built = Built::default();
        for group in groups {
            let mut writer = self.store.new_segment()?;
            let embed = EmbedCtx {
                embedder: self.embedder.as_ref(),
                cache: &self.vectors,
                chunking: &self.chunking,
            };
            let stats = build_segment(&mut writer, &self.config.root, group, &embed)?;
            built.skipped += stats.skipped_changed + stats.skipped_unreadable;
            built.chunks_embedded += stats.chunks_embedded;
            built.chunks_reused += stats.chunks_reused;
            if stats.indexed > 0 {
                built.segments.push((writer.finish()?, stats.indexed));
            }
        }
        Ok(built)
    }

    /// Merge segments to keep their number and dead weight down.
    ///
    /// A segment is a candidate if at least `policy.min_dead_ratio` of its blobs are no longer
    /// live (no path in the snapshot holds them), and, when more than `policy.max_segments`
    /// segments exist, the smallest ones are merged until the count fits. Candidates' live blobs
    /// are re-indexed from the working tree into new segments (embeddings come from the vector
    /// cache, so this is mostly parsing). A live blob whose file has since changed is not
    /// re-indexed; its path is picked up by the next [`Index::update`] and is reported `stale`
    /// until then. Retired segments are deleted by [`Index::gc`] after the reader grace period.
    pub fn compact(&mut self, policy: &CompactPolicy) -> Result<CompactReport> {
        let snapshot = self.store.load_snapshot()?;
        let live_blobs = snapshot.live_blobs();

        struct Info {
            id: SegmentId,
            total: usize,
            live: usize,
        }
        let mut infos = Vec::new();
        for id in self.store.segment_ids() {
            let seg = self.store.open_segment(id)?;
            let live = seg
                .blobs()
                .iter()
                .filter(|b| live_blobs.contains(*b))
                .count();
            infos.push(Info {
                id,
                total: seg.blobs().len(),
                live,
            });
        }
        let mut chosen: Vec<SegmentId> = infos
            .iter()
            .filter(|i| {
                i.total > 0 && (1.0 - i.live as f64 / i.total as f64) >= policy.min_dead_ratio
            })
            .map(|i| i.id)
            .collect();
        let remaining = infos.len() - chosen.len();
        if remaining > policy.max_segments {
            // Too many segments: merge the smallest of the rest.
            let mut rest: Vec<&Info> = infos.iter().filter(|i| !chosen.contains(&i.id)).collect();
            rest.sort_by_key(|i| i.live);
            let excess = remaining - policy.max_segments + 1; // merging k segments into one removes k - 1
            chosen.extend(rest.into_iter().take(excess.max(2)).map(|i| i.id));
        }
        if chosen.len() < 2
            && !chosen
                .iter()
                .any(|id| infos.iter().any(|i| i.id == *id && i.live < i.total))
        {
            return Ok(CompactReport::default());
        }

        // Live blobs of the chosen segments, one representative path each.
        let mut items: Vec<Item> = Vec::new();
        let mut seen = HashSet::new();
        let mut dead = 0usize;
        let mut paths_by_blob: std::collections::HashMap<BlobId, (&String, &loupe_core::FileMeta)> =
            std::collections::HashMap::new();
        for (path, meta) in &snapshot.files {
            paths_by_blob.entry(meta.blob).or_insert((path, meta));
        }
        for id in &chosen {
            let seg = self.store.open_segment(*id)?;
            for blob in seg.blobs() {
                match paths_by_blob.get(blob) {
                    Some((path, meta)) if seen.insert(*blob) => items.push(Item {
                        blob: *blob,
                        path: (*path).clone(),
                        lang: meta.lang.clone(),
                        size: meta.size,
                    }),
                    Some(_) => {}
                    None => dead += 1,
                }
            }
        }

        let built = self.build_items(&items)?;
        let mut segments: Vec<SegmentId> = self
            .store
            .segment_ids()
            .into_iter()
            .filter(|id| !chosen.contains(id))
            .collect();
        segments.extend(built.segments.iter().map(|(id, _)| *id));
        self.store.commit(&snapshot, &segments)?;
        Ok(CompactReport {
            merged: chosen.len(),
            written: built.segments.len(),
            blobs_kept: built.segments.iter().map(|(_, n)| n).sum(),
            blobs_dropped_dead: dead,
            blobs_skipped_changed: built.skipped,
        })
    }

    /// Delete files no live state needs: temp files, crash orphans, and retired segments past the
    /// reader grace period. Runs automatically when a writer opens the index.
    pub fn gc(&self) -> Result<loupe_store::GcReport> {
        self.store.gc()
    }

    /// How long retired segments keep their files (default 30 s).
    pub fn set_retire_grace(&mut self, grace: std::time::Duration) {
        self.store.set_retire_grace(grace);
    }

    fn embedded_by_other_model(&self, seg: &loupe_store::Segment) -> bool {
        match EmbedIndex::from_segment(seg) {
            Ok(Some(idx)) => {
                idx.model_id() != self.embedder.model_id() || idx.dim() != self.embedder.dim()
            }
            _ => true, // unreadable embedding files: rebuild the segment
        }
    }

    /// Pin text, symbol and semantic searchers plus the graph to the current snapshot.
    pub fn session(&self, graph: &GraphOptions) -> Result<Session> {
        Ok(Session {
            text: self.searcher()?,
            symbols: self.symbol_searcher()?,
            semantic: self.semantic_searcher()?,
            graph: self.graph(graph)?,
        })
    }

    /// A semantic searcher pinned to the current snapshot.
    pub fn semantic_searcher(&self) -> Result<SemanticSearcher> {
        SemanticSearcher::open(
            &self.config.root,
            &self.store,
            self.embedder.clone(),
            self.chunking.clone(),
        )
    }

    /// Natural-language search over definition-level chunks (overlay included).
    pub fn search_semantic(&self, query: &str, opts: &SemanticOptions) -> Result<SemanticResult> {
        self.semantic_searcher()?
            .search(query, opts, Some(&self.overlay))
    }

    /// A searcher pinned to the current snapshot. Cheap enough to build per query batch; hold
    /// one across queries to avoid re-reading segment metadata.
    pub fn searcher(&self) -> Result<TextSearcher> {
        TextSearcher::open(&self.config.root, &self.store)
    }

    /// Build the file graph (imports, test links, name references, git co-change) for the
    /// current snapshot. Cost grows with repo size; hold on to the result across queries.
    pub fn graph(&self, opts: &GraphOptions) -> Result<Graph> {
        let view = SnapshotView::open(&self.store)?;
        build_graph(&self.config.root, &view, opts)
    }

    /// The persistent summary cache (`<index>/cache/summaries.redb`), or an in-memory one if the
    /// file is held by another process. Share one across calls so summaries are reused.
    pub fn open_summary_cache(&self) -> SummaryCache {
        SummaryCache::open(&self.config.index_dir.join("cache").join("summaries.redb"))
    }

    /// A token-budgeted map of the repo: header, entry points, and the most important files and
    /// directories with one-line summaries. Deterministic for a given snapshot and options.
    ///
    /// `symbols` and `graph` must come from the same snapshot; reuse them across calls (building
    /// a searcher re-verifies segment checksums, which is the dominant cost of a cold call).
    pub fn repo_map_with(
        &self,
        symbols: &SymbolSearcher,
        graph: &Graph,
        cache: &SummaryCache,
        summarizer: &dyn Summarizer,
        opts: &MapOptions,
    ) -> Result<RepoMap> {
        repo_map(
            &self.config.root,
            symbols,
            None,
            graph,
            cache,
            summarizer,
            opts,
        )
    }

    /// One to three lines about a directory, plus what it depends on.
    pub fn module_summary_with(
        &self,
        symbols: &SymbolSearcher,
        graph: &Graph,
        cache: &SummaryCache,
        summarizer: &dyn Summarizer,
        dir: &str,
    ) -> Result<ModuleSummary> {
        module_summary(
            &self.config.root,
            symbols,
            None,
            graph,
            cache,
            summarizer,
            dir,
        )
    }

    /// Convenience: builds the searcher and graph and uses the persistent cache and the
    /// heuristic summarizer.
    pub fn repo_map(&self, opts: &MapOptions) -> Result<RepoMap> {
        let graph = self.graph(&GraphOptions::default())?;
        self.repo_map_with(
            &self.symbol_searcher()?,
            &graph,
            &self.open_summary_cache(),
            &HeuristicSummarizer,
            opts,
        )
    }

    /// Convenience counterpart of [`Index::module_summary_with`].
    pub fn module_summary(&self, dir: &str) -> Result<ModuleSummary> {
        let graph = self.graph(&GraphOptions::default())?;
        self.module_summary_with(
            &self.symbol_searcher()?,
            &graph,
            &self.open_summary_cache(),
            &HeuristicSummarizer,
            dir,
        )
    }

    /// A symbol searcher pinned to the current snapshot.
    pub fn symbol_searcher(&self) -> Result<SymbolSearcher> {
        SymbolSearcher::open(&self.config.root, &self.store)
    }

    /// Definitions named exactly `name` (optionally of one kind), overlay included.
    pub fn find_symbol(
        &self,
        name: &str,
        kind: Option<SymbolKind>,
        limit: usize,
    ) -> Result<Vec<SymbolHit>> {
        self.symbol_searcher()?
            .find(name, kind, limit, Some(&self.overlay))
    }

    /// Signature outline of one file (no bodies); `None` if the file is unknown or removed.
    pub fn outline(&self, path: &str) -> Result<Option<Outline>> {
        self.symbol_searcher()?.outline(path, Some(&self.overlay))
    }

    pub fn search_text(&self, pattern: &str, opts: &SearchOptions) -> Result<SearchResult> {
        self.searcher()?
            .search_with(pattern, opts, Some(&self.overlay))
    }

    pub fn search_word(&self, word: &str, opts: &SearchOptions) -> Result<SearchResult> {
        self.searcher()?
            .search_word_with(word, opts, Some(&self.overlay))
    }
}

fn with_mapper<T>(
    root: &std::path::Path,
    symbols: &SymbolSearcher,
    tree: Option<&MerkleTree>,
    graph: &Graph,
    cache: &SummaryCache,
    summarizer: &dyn Summarizer,
    f: impl FnOnce(&RepoMapper<'_>) -> Result<T>,
) -> Result<T> {
    let files = DiskFiles(root);
    let ctx = MapContext {
        snapshot: symbols.snapshot(),
        symbols,
        files: &files,
        graph,
        cache,
        summarizer,
        counter: &ApproxTokenCounter,
    };
    match tree {
        Some(t) => f(&RepoMapper::with_tree(ctx, t)),
        None => f(&RepoMapper::new(ctx)),
    }
}

/// [`Index::repo_map_with`] without an [`Index`], for callers (the daemon) that already hold a
/// pinned searcher and graph and do not want to take the index lock.
pub fn repo_map(
    root: &std::path::Path,
    symbols: &SymbolSearcher,
    tree: Option<&MerkleTree>,
    graph: &Graph,
    cache: &SummaryCache,
    summarizer: &dyn Summarizer,
    opts: &MapOptions,
) -> Result<RepoMap> {
    with_mapper(root, symbols, tree, graph, cache, summarizer, |m| {
        m.repo_map(opts)
    })
}

/// [`Index::module_summary_with`] without an [`Index`].
pub fn module_summary(
    root: &std::path::Path,
    symbols: &SymbolSearcher,
    tree: Option<&MerkleTree>,
    graph: &Graph,
    cache: &SummaryCache,
    summarizer: &dyn Summarizer,
    dir: &str,
) -> Result<ModuleSummary> {
    with_mapper(root, symbols, tree, graph, cache, summarizer, |m| {
        m.module_summary(dir)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Repo {
        dir: tempfile::TempDir,
        idx: tempfile::TempDir,
    }

    impl Repo {
        fn new() -> Self {
            Repo {
                dir: tempfile::tempdir().unwrap(),
                idx: tempfile::tempdir().unwrap(),
            }
        }
        fn config(&self) -> Config {
            let mut c = Config::new(self.dir.path().canonicalize().unwrap());
            c.index_dir = self.idx.path().to_path_buf();
            c
        }
        fn write(&self, rel: &str, content: &str) {
            let p = self.dir.path().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, content).unwrap();
        }
        fn age_all(&self) {
            let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
            fn walk(p: &std::path::Path, old: std::time::SystemTime) {
                for e in fs::read_dir(p).unwrap() {
                    let p = e.unwrap().path();
                    if p.is_dir() {
                        walk(&p, old);
                    } else {
                        fs::File::options()
                            .write(true)
                            .open(&p)
                            .unwrap()
                            .set_modified(old)
                            .unwrap();
                    }
                }
            }
            walk(self.dir.path(), old);
        }
    }

    fn all() -> SearchOptions {
        SearchOptions {
            max_files: usize::MAX,
            timeout: None,
            ..SearchOptions::default()
        }
    }

    #[test]
    fn update_then_search() {
        let r = Repo::new();
        r.write("a.rs", "fn alpha() {}\n");
        r.write("b.rs", "fn beta() {}\n");
        let mut idx = Index::open_writer(r.config()).unwrap();
        let rep = idx.update(false).unwrap();
        assert_eq!(rep.files, 2);
        assert_eq!(rep.new_segment.map(|(_, n)| n), Some(2));
        assert_eq!(idx.search_text("alpha", &all()).unwrap().total_files, 1);
        assert_eq!(idx.search_word("beta", &all()).unwrap().total_files, 1);
    }

    #[test]
    fn unchanged_update_writes_nothing() {
        let r = Repo::new();
        r.write("a.rs", "fn alpha() {}\n");
        r.age_all();
        let mut idx = Index::open_writer(r.config()).unwrap();
        idx.update(false).unwrap();
        let id = idx.snapshot_id();
        let rep = idx.update(false).unwrap();
        assert!(rep.unchanged);
        assert_eq!(rep.walk.hashed, 0);
        assert_eq!(idx.snapshot_id(), id, "no new generation");
    }

    #[test]
    fn incremental_update_indexes_only_new_blobs() {
        let r = Repo::new();
        r.write("a.rs", "fn alpha() {}\n");
        r.write("b.rs", "fn beta() {}\n");
        r.age_all();
        let mut idx = Index::open_writer(r.config()).unwrap();
        idx.update(false).unwrap();

        r.write("c.rs", "fn gamma() {}\n");
        r.write("b.rs", "fn beta_two() {}\n");
        let rep = idx.update(false).unwrap();
        assert_eq!(
            rep.new_segment.map(|(_, n)| n),
            Some(2),
            "only c.rs and new b.rs"
        );
        assert_eq!(idx.store().segment_ids().len(), 2);
        assert_eq!(idx.search_text("gamma", &all()).unwrap().total_files, 1);
        assert_eq!(idx.search_text("beta_two", &all()).unwrap().total_files, 1);
        assert_eq!(
            idx.search_text("fn beta\\(", &all()).unwrap().total_files,
            0,
            "old b.rs content is gone"
        );
    }

    #[test]
    fn renaming_a_file_reindexes_nothing() {
        let r = Repo::new();
        r.write("old.rs", "fn unique_content() {}\n");
        r.age_all();
        let mut idx = Index::open_writer(r.config()).unwrap();
        idx.update(false).unwrap();
        fs::rename(r.dir.path().join("old.rs"), r.dir.path().join("new.rs")).unwrap();
        let rep = idx.update(false).unwrap();
        assert!(rep.new_segment.is_none(), "same blob, already indexed");
        let hits = idx.search_text("unique_content", &all()).unwrap();
        assert_eq!(hits.hits.len(), 1);
        assert_eq!(hits.hits[0].path, "new.rs");
    }

    #[test]
    fn second_writer_is_refused_but_reader_works() {
        let r = Repo::new();
        r.write("a.rs", "fn alpha() {}\n");
        let mut w = Index::open_writer(r.config()).unwrap();
        w.update(false).unwrap();
        assert!(matches!(
            Index::open_writer(r.config()),
            Err(Error::Locked(_))
        ));
        let mut reader = Index::open_reader(r.config()).unwrap();
        assert_eq!(reader.search_text("alpha", &all()).unwrap().total_files, 1);

        r.write("b.rs", "fn beta() {}\n");
        w.update(false).unwrap();
        assert_eq!(reader.search_text("beta", &all()).unwrap().total_files, 0);
        reader.refresh().unwrap();
        assert_eq!(reader.search_text("beta", &all()).unwrap().total_files, 1);
    }

    #[test]
    fn overlay_put_remove_and_absorption() {
        let r = Repo::new();
        r.write("a.rs", "fn on_disk() {}\n");
        r.age_all();
        let mut idx = Index::open_writer(r.config()).unwrap();
        idx.update(false).unwrap();

        idx.put("a.rs", b"fn in_buffer() {}\n".to_vec()).unwrap();
        assert_eq!(idx.search_text("in_buffer", &all()).unwrap().total_files, 1);
        assert_eq!(idx.search_text("on_disk", &all()).unwrap().total_files, 0);
        idx.remove("a.rs");
        assert_eq!(idx.search_text("in_buffer", &all()).unwrap().total_files, 0);

        idx.put("a.rs", b"fn in_buffer() {}\n".to_vec()).unwrap();
        idx.update(false).unwrap(); // the scan observed the filesystem after the put
        assert!(idx.overlay().is_empty(), "overlay absorbed by the update");
        assert_eq!(idx.search_text("on_disk", &all()).unwrap().total_files, 1);
    }

    const RUST_SRC: &str = "/// Greets.\npub fn greet(name: &str) -> String {\n    format!(\"hi {name}\")\n}\n\npub struct Greeter;\n\nimpl Greeter {\n    pub fn new() -> Self { Greeter }\n}\n";

    #[test]
    fn symbols_are_indexed_found_and_outlined() {
        let r = Repo::new();
        r.write("src/lib.rs", RUST_SRC);
        r.write("notes.txt", "fn not_code() {}\n"); // no language: no symbols
        let mut idx = Index::open_writer(r.config()).unwrap();
        idx.update(false).unwrap();

        let hits = idx.find_symbol("greet", None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "src/lib.rs");
        assert_eq!(hits[0].kind, SymbolKind::Function);
        assert_eq!(hits[0].doc.as_deref(), Some("Greets."));
        assert_eq!(hits[0].signature, "pub fn greet(name: &str) -> String");

        let new = idx.find_symbol("new", None, 10).unwrap();
        assert_eq!(
            (new[0].kind, new[0].parent.as_deref()),
            (SymbolKind::Method, Some("Greeter"))
        );
        assert!(
            idx.find_symbol("new", Some(SymbolKind::Struct), 10)
                .unwrap()
                .is_empty()
        );
        assert!(idx.find_symbol("not_code", None, 10).unwrap().is_empty());

        let outline = idx.outline("src/lib.rs").unwrap().unwrap();
        assert_eq!(outline.lang.as_deref(), Some("rust"));
        assert_eq!(outline.source, loupe_symbols::OutlineSource::Index);
        let text = render_outline(&outline.symbols, &OutlineOptions::default());
        assert!(
            text.contains("pub fn greet(name: &str) -> String  L2-4  // Greets."),
            "{text}"
        );
        assert!(
            text.contains("  pub fn new() -> Self"),
            "methods are indented: {text}"
        );
        assert!(idx.outline("missing.rs").unwrap().is_none());
    }

    #[test]
    fn symbols_follow_the_overlay() {
        let r = Repo::new();
        r.write("src/lib.rs", RUST_SRC);
        r.age_all();
        let mut idx = Index::open_writer(r.config()).unwrap();
        idx.update(false).unwrap();

        idx.put("src/lib.rs", b"pub fn only_in_buffer() {}\n".to_vec())
            .unwrap();
        assert!(
            idx.find_symbol("greet", None, 10).unwrap().is_empty(),
            "snapshot version is hidden"
        );
        assert_eq!(
            idx.find_symbol("only_in_buffer", None, 10).unwrap().len(),
            1
        );
        let o = idx.outline("src/lib.rs").unwrap().unwrap();
        assert_eq!(o.source, loupe_symbols::OutlineSource::Overlay);
        assert_eq!(o.symbols.len(), 1);

        idx.put("src/new.rs", b"pub struct Fresh;\n".to_vec())
            .unwrap();
        assert_eq!(
            idx.find_symbol("Fresh", None, 10).unwrap()[0].path,
            "src/new.rs"
        );
        idx.remove("src/lib.rs");
        assert!(idx.outline("src/lib.rs").unwrap().is_none());
    }

    #[test]
    fn outdated_and_corrupt_segments_are_rebuilt_automatically() {
        let r = Repo::new();
        r.write("src/lib.rs", RUST_SRC);
        r.age_all();
        let config = r.config();

        // A segment as an older version built it: text index only, no symbol files.
        {
            let mut store = loupe_store::Store::open_writer(&config.index_dir).unwrap();
            let result = scan(&config, None, &WalkOptions::default()).unwrap();
            let items: Vec<(BlobId, String)> = result
                .snapshot
                .files
                .iter()
                .map(|(p, m)| (m.blob, p.clone()))
                .collect();
            let mut w = store.new_segment().unwrap();
            loupe_text::write_text_index(&mut w, &config.root, &items).unwrap();
            let id = w.finish().unwrap();
            store.commit(&result.snapshot, &[id]).unwrap();
        }
        let mut idx = Index::open_writer(config.clone()).unwrap();
        assert!(
            idx.find_symbol("greet", None, 10).unwrap().is_empty(),
            "old segment has no symbols"
        );
        let rep = idx.update(false).unwrap();
        assert_eq!(rep.dropped_segments, 1);
        assert_eq!(rep.new_segment.map(|(_, n)| n), Some(1), "blobs re-indexed");
        assert!(!rep.unchanged);
        assert_eq!(idx.find_symbol("greet", None, 10).unwrap().len(), 1);
        assert_eq!(
            idx.search_text("pub fn greet", &all()).unwrap().total_files,
            1
        );
        assert!(idx.update(false).unwrap().unchanged, "stable afterwards");

        // Corrupt the live segment's text index: the next update drops and rebuilds it.
        let seg_id = idx.store().segment_ids()[0];
        let victim = config
            .index_dir
            .join("segments")
            .join(format!("{:016x}", seg_id.0))
            .join(loupe_text::POST_FILE);
        let mut bytes = std::fs::read(&victim).unwrap();
        bytes[0] ^= 0xff;
        std::fs::write(&victim, bytes).unwrap();
        assert!(
            idx.searcher().is_err(),
            "searching a corrupt index fails loudly"
        );
        let rep = idx.update(false).unwrap();
        assert_eq!(rep.dropped_segments, 1);
        assert_eq!(
            idx.search_text("pub fn greet", &all()).unwrap().total_files,
            1
        );
        assert_eq!(idx.find_symbol("greet", None, 10).unwrap().len(), 1);
    }

    #[test]
    fn symbols_survive_incremental_updates_and_renames() {
        let r = Repo::new();
        r.write("a.rs", "pub fn alpha() {}\n");
        r.age_all();
        let mut idx = Index::open_writer(r.config()).unwrap();
        idx.update(false).unwrap();
        r.write("b.rs", "pub fn beta() {}\n");
        idx.update(false).unwrap();
        assert_eq!(idx.find_symbol("alpha", None, 10).unwrap().len(), 1);
        assert_eq!(idx.find_symbol("beta", None, 10).unwrap().len(), 1);

        std::fs::rename(r.dir.path().join("a.rs"), r.dir.path().join("renamed.rs")).unwrap();
        idx.update(false).unwrap();
        let hits = idx.find_symbol("alpha", None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "renamed.rs");
    }
}
