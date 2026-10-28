use std::fs;
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use loupe_api::{
    CompactPolicy, Graph, GraphOptions, Index, MapOptions, Overlay, RelatedOptions, SummaryCache,
    TextSearcher,
};
use loupe_core::{Config, Error, Result};
use loupe_map::{HeuristicSummarizer, MerkleTree};
use loupe_proto::{
    CompactSummary, MetricsInfo, Request, Response, StatusInfo, UpdateSummary, read_frame,
    validate_rel_path, write_frame,
};
use loupe_symbols::SymbolSearcher;
use loupe_walk::{is_indexable_content, is_secret_path};
use notify_debouncer_full::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};

#[derive(Clone, Debug)]
pub struct DaemonOptions {
    /// Re-scan the working tree this often even without watcher events (catches missed events).
    pub reconcile_every: Duration,
    /// Force-rehash every file this often, ignoring `(size, mtime)`.
    pub rehash_every: Duration,
    /// Watcher debounce window; also the floor on edit-to-visible latency.
    pub debounce: Duration,
    /// Watch the working tree. Without it only requests and timers change the index.
    pub watch: bool,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        DaemonOptions {
            reconcile_every: Duration::from_secs(30),
            rehash_every: Duration::from_secs(30 * 60),
            debounce: Duration::from_millis(30),
            watch: true,
        }
    }
}

enum Job {
    Update,
    Stop,
}

/// Compact automatically when an update leaves more live segments than this.
const AUTO_COMPACT_ABOVE: usize = 12;

struct Shared {
    config: Config,
    index: Mutex<Index>,
    searcher: ArcSwap<TextSearcher>,
    symbols: ArcSwap<SymbolSearcher>,
    /// Graph for one snapshot id, built on first use (it costs seconds on big repos).
    graph: Mutex<Option<(u64, Arc<Graph>)>>,
    /// Merkle tree of the snapshot, also cached per snapshot id.
    tree: Mutex<Option<(u64, Arc<MerkleTree>)>>,
    /// Module summaries keyed by directory hash; survives updates and restarts.
    summaries: SummaryCache,
    overlay: RwLock<Overlay>,
    ignore: RwLock<Gitignore>,
    updates: AtomicU64,
    compactions: AtomicU64,
    started: Instant,
    metrics: crate::metrics::Metrics,
    shutting_down: AtomicBool,
    jobs: Sender<Job>,
    socket: PathBuf,
}

/// A running daemon. Dropping it shuts it down.
pub struct DaemonHandle {
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
    worker: Option<JoinHandle<()>>,
    _debouncer: Option<Debouncer<RecommendedWatcher, RecommendedCache>>,
}

impl std::fmt::Debug for DaemonHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonHandle")
            .field("socket", &self.shared.socket)
            .finish()
    }
}

/// Take the writer lock, index once, bind the socket, and start serving in background threads.
///
/// Fails with [`Error::Locked`] if another process already writes this index.
pub fn start(config: Config, opts: DaemonOptions) -> Result<DaemonHandle> {
    let mut index = Index::open_writer(config.clone())?;
    index.update(false)?;
    let searcher = index.searcher()?;
    let symbols = index.symbol_searcher()?;
    let summaries = index.open_summary_cache();

    // We hold the exclusive writer lock, so any socket file left behind is from a dead daemon.
    let socket = crate::socket_path(&config.index_dir);
    let _ = fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).map_err(|e| Error::io(&socket, e))?;

    let (jobs, rx) = mpsc::channel();
    let shared = Arc::new(Shared {
        ignore: RwLock::new(build_ignore(&config.root)),
        config,
        index: Mutex::new(index),
        searcher: ArcSwap::from_pointee(searcher),
        symbols: ArcSwap::from_pointee(symbols),
        graph: Mutex::new(None),
        tree: Mutex::new(None),
        summaries,
        overlay: RwLock::new(Overlay::new()),
        updates: AtomicU64::new(0),
        compactions: AtomicU64::new(0),
        started: Instant::now(),
        metrics: crate::metrics::Metrics::default(),
        shutting_down: AtomicBool::new(false),
        jobs,
        socket,
    });

    let debouncer = if opts.watch {
        Some(start_watcher(&shared, opts.debounce)?)
    } else {
        None
    };

    let socket_for_err = shared.socket.clone();
    let worker = {
        let shared = shared.clone();
        let opts = opts.clone();
        thread::Builder::new()
            .name("loupe-worker".into())
            .spawn(move || worker_loop(&shared, &rx, &opts))
            .map_err(|e| Error::io(&socket_for_err, e))?
    };
    let accept = {
        let shared = shared.clone();
        thread::Builder::new()
            .name("loupe-accept".into())
            .spawn(move || accept_loop(&shared, &listener))
            .map_err(|e| Error::io(&socket_for_err, e))?
    };

    Ok(DaemonHandle {
        shared,
        accept: Some(accept),
        worker: Some(worker),
        _debouncer: debouncer,
    })
}

impl DaemonHandle {
    pub fn socket(&self) -> &Path {
        &self.shared.socket
    }

    /// Ask the daemon to stop. Returns immediately; use [`DaemonHandle::wait`] to join.
    pub fn shutdown(&self) {
        self.shared.shutdown();
    }

    /// Block until the daemon stops (a client sent `Shutdown`, or [`DaemonHandle::shutdown`]).
    pub fn wait(mut self) {
        self.join();
    }

    fn join(&mut self) {
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        self.shared.shutdown();
        self._debouncer.take();
        self.join();
    }
}

impl Shared {
    fn shutdown(&self) {
        if self.shutting_down.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.jobs.send(Job::Stop);
        // `accept` blocks; a throwaway connection wakes it so it can notice the flag.
        let _ = UnixStream::connect(&self.socket);
    }

    /// Re-scan, publish the new snapshot, and drop overlay entries the scan observed.
    fn run_update(&self, force_rehash: bool) -> Result<UpdateSummary> {
        let mut index = self.index.lock().unwrap();
        // Entries at or below this sequence predate the scan, so the scan sees their content.
        let seq = self.overlay.read().unwrap().seq();
        let report = index.update(force_rehash)?;
        if index.store().segment_ids().len() > AUTO_COMPACT_ABOVE {
            match index.compact(&CompactPolicy::default()) {
                Ok(c) if c.merged > 0 => {
                    self.compactions.fetch_add(1, Ordering::Relaxed);
                    if let Err(e) = index.gc() {
                        tracing::warn!(%e, "gc after auto-compaction failed");
                    }
                }
                Ok(_) => {}
                // Compaction is an optimisation; a failure must not fail the update.
                Err(e) => tracing::warn!(%e, "auto-compaction failed"),
            }
        }
        self.publish(&index)?;
        // Count before publishing, so any client that can see the new snapshot also sees the
        // update counted in `Status`.
        self.updates.fetch_add(1, Ordering::Relaxed);
        self.overlay.write().unwrap().drop_through(seq);
        *self.ignore.write().unwrap() = build_ignore(&self.config.root);
        Ok(UpdateSummary {
            snapshot_id: report.snapshot_id,
            files: report.files,
            added: report.diff.added.len(),
            modified: report.diff.modified.len(),
            removed: report.diff.removed.len(),
            unchanged: report.unchanged,
        })
    }

    /// Swap in searchers for the index's current snapshot.
    fn publish(&self, index: &Index) -> Result<()> {
        let searcher = index.searcher()?;
        let symbols = index.symbol_searcher()?;
        self.searcher.store(Arc::new(searcher));
        self.symbols.store(Arc::new(symbols));
        Ok(())
    }

    fn run_compact(&self) -> Result<CompactSummary> {
        let mut index = self.index.lock().unwrap();
        let report = index.compact(&CompactPolicy::default())?;
        let gc = index.gc()?;
        self.publish(&index)?;
        self.compactions.fetch_add(1, Ordering::Relaxed);
        Ok(CompactSummary {
            merged: report.merged,
            written: report.written,
            blobs_dropped_dead: report.blobs_dropped_dead,
            segments_removed: gc.segments_removed,
        })
    }

    fn metrics_info(&self) -> MetricsInfo {
        let searcher = self.searcher.load();
        let overlay = self.overlay.read().unwrap();
        MetricsInfo {
            uptime_secs: self.started.elapsed().as_secs(),
            requests: self.metrics.snapshot(),
            updates: self.updates.load(Ordering::Relaxed),
            compactions: self.compactions.load(Ordering::Relaxed),
            segments: searcher.segment_count(),
            index_bytes: dir_bytes(&self.config.index_dir),
            overlay_files: overlay.len(),
            overlay_bytes: overlay.bytes(),
        }
    }

    /// Fast path: mirror one changed path into the overlay. Returns whether it was relevant.
    fn sync_path(&self, abs: &Path) -> bool {
        let Ok(rel) = abs.strip_prefix(&self.config.root) else {
            return false;
        };
        if rel.as_os_str().is_empty()
            || abs.starts_with(&self.config.index_dir)
            || rel.components().any(|c| c.as_os_str() == ".git")
        {
            return false;
        }
        let Some(rel_str) = rel_to_string(rel) else {
            return false;
        };

        let admitted = match fs::metadata(abs) {
            Ok(md) if md.is_dir() => return true, // new files inside arrive as their own events
            Ok(md) if md.is_file() => {
                md.len() <= self.config.max_file_bytes
                    && (self.config.index_secrets || !is_secret_path(&rel_str))
                    && !self
                        .ignore
                        .read()
                        .unwrap()
                        .matched_path_or_any_parents(rel, false)
                        .is_ignore()
            }
            _ => false,
        };
        let content = admitted
            .then(|| fs::read(abs).ok())
            .flatten()
            .filter(|b| is_indexable_content(b));

        let mut overlay = self.overlay.write().unwrap();
        match content {
            Some(bytes) => {
                if let Err(e) = overlay.put(rel_str, bytes) {
                    // Over the overlay limit: the background update will pick the file up.
                    tracing::warn!(%e, "overlay put skipped");
                }
            }
            // Deleted, ignored, too large, or binary: the snapshot's copy (if any) must not show.
            None => {
                overlay.remove(rel_str);
            }
        }
        true
    }

    /// The graph for the snapshot queries currently see, building it if this snapshot has none.
    fn graph(&self) -> Result<Arc<Graph>> {
        let want = self.searcher.load().snapshot_id();
        let mut slot = self.graph.lock().unwrap();
        if let Some((id, g)) = slot.as_ref()
            && *id == want
        {
            return Ok(g.clone());
        }
        let built = Arc::new(self.index.lock().unwrap().graph(&GraphOptions::default())?);
        *slot = Some((want, built.clone()));
        Ok(built)
    }

    fn tree(&self, symbols: &SymbolSearcher) -> Arc<MerkleTree> {
        let want = symbols.snapshot_id();
        let mut slot = self.tree.lock().unwrap();
        if let Some((id, t)) = slot.as_ref()
            && *id == want
        {
            return t.clone();
        }
        let built = Arc::new(MerkleTree::build(symbols.snapshot()));
        *slot = Some((want, built.clone()));
        built
    }

    fn status(&self) -> StatusInfo {
        let searcher = self.searcher.load();
        let overlay = self.overlay.read().unwrap();
        StatusInfo {
            pid: std::process::id(),
            snapshot_id: searcher.snapshot_id(),
            files: searcher.file_count(),
            segments: searcher.segment_count(),
            overlay_files: overlay.len(),
            overlay_bytes: overlay.bytes(),
            updates: self.updates.load(Ordering::Relaxed),
        }
    }

    fn handle(&self, request: Request) -> Response {
        match self.dispatch(request) {
            Ok(r) => r,
            Err(e) => Response::Error(e.to_string()),
        }
    }

    fn dispatch(&self, request: Request) -> Result<Response> {
        Ok(match request {
            Request::Ping => Response::Pong,
            Request::Status => Response::Status(self.status()),
            Request::Search {
                pattern,
                word,
                opts,
            } => {
                let searcher = self.searcher.load_full();
                let overlay = self.overlay.read().unwrap().clone();
                Response::Search(if word {
                    searcher.search_word_with(&pattern, &opts, Some(&overlay))?
                } else {
                    searcher.search_with(&pattern, &opts, Some(&overlay))?
                })
            }
            Request::FindFiles { query, limit } => {
                let searcher = self.searcher.load_full();
                let overlay = self.overlay.read().unwrap().clone();
                Response::Fuzzy(searcher.find_files_with(&query, limit, Some(&overlay)))
            }
            Request::FindTokens { query, limit } => {
                let searcher = self.searcher.load_full();
                let overlay = self.overlay.read().unwrap().clone();
                Response::Fuzzy(searcher.find_tokens_with(&query, limit, Some(&overlay))?)
            }
            Request::FindSymbol { name, kind, limit } => {
                let symbols = self.symbols.load_full();
                let overlay = self.overlay.read().unwrap().clone();
                Response::Symbols(symbols.find(&name, kind, limit, Some(&overlay))?)
            }
            Request::Outline { path } => {
                validate_rel_path(&path).map_err(Error::InvalidArgument)?;
                let symbols = self.symbols.load_full();
                let overlay = self.overlay.read().unwrap().clone();
                Response::Outline(symbols.outline(&path, Some(&overlay))?)
            }
            Request::Related {
                path,
                kinds,
                hops,
                limit,
            } => {
                validate_rel_path(&path).map_err(Error::InvalidArgument)?;
                let kinds = if kinds.is_empty() {
                    loupe_graph::EdgeKind::ALL.to_vec()
                } else {
                    kinds
                };
                Response::Related(
                    self.graph()?
                        .related(&path, &RelatedOptions { kinds, hops, limit }),
                )
            }
            Request::Impact { path, depth, limit } => {
                validate_rel_path(&path).map_err(Error::InvalidArgument)?;
                Response::Impact(self.graph()?.impact(&path, depth, limit))
            }
            Request::RepoMap {
                budget,
                focus,
                name,
            } => {
                let graph = self.graph()?;
                let opts = MapOptions {
                    budget,
                    focus,
                    name,
                };
                // The pinned searcher and cached graph are reused: no index lock, no reopening.
                let symbols = self.symbols.load_full();
                let tree = self.tree(&symbols);
                Response::Map(loupe_api::repo_map(
                    &self.config.root,
                    &symbols,
                    Some(&tree),
                    &graph,
                    &self.summaries,
                    &HeuristicSummarizer,
                    &opts,
                )?)
            }
            Request::ModuleSummary { path } => {
                let dir = path.trim_matches('/');
                if !dir.is_empty() {
                    validate_rel_path(dir).map_err(Error::InvalidArgument)?;
                }
                let graph = self.graph()?;
                let symbols = self.symbols.load_full();
                let tree = self.tree(&symbols);
                Response::Module(loupe_api::module_summary(
                    &self.config.root,
                    &symbols,
                    Some(&tree),
                    &graph,
                    &self.summaries,
                    &HeuristicSummarizer,
                    dir,
                )?)
            }
            Request::TopRanked { limit } => Response::Ranked(
                self.graph()?
                    .top_ranked(limit)
                    .into_iter()
                    .map(|(p, r)| (p.to_string(), r))
                    .collect(),
            ),
            Request::Put { path, content } => {
                validate_rel_path(&path).map_err(Error::InvalidArgument)?;
                self.overlay.write().unwrap().put(path, content)?;
                Response::Ok
            }
            Request::Remove { path } => {
                validate_rel_path(&path).map_err(Error::InvalidArgument)?;
                self.overlay.write().unwrap().remove(path);
                Response::Ok
            }
            Request::Update { force_rehash } => Response::Updated(self.run_update(force_rehash)?),
            Request::Compact => Response::Compacted(self.run_compact()?),
            Request::Metrics => Response::Metrics(self.metrics_info()),
            Request::Shutdown => {
                self.shutdown();
                Response::Ok
            }
        })
    }
}

fn dir_bytes(p: &Path) -> u64 {
    fs::read_dir(p)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_bytes(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

fn rel_to_string(rel: &Path) -> Option<String> {
    let mut out = String::new();
    for comp in rel.components() {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(comp.as_os_str().to_str()?);
    }
    Some(out)
}

fn build_ignore(root: &Path) -> Gitignore {
    let mut b = GitignoreBuilder::new(root);
    for name in [".gitignore", ".ignore", loupe_core::config::IGNORE_FILE] {
        let p = root.join(name);
        if p.is_file() {
            let _ = b.add(p);
        }
    }
    b.build().unwrap_or_else(|_| Gitignore::empty())
}

fn start_watcher(
    shared: &Arc<Shared>,
    debounce: Duration,
) -> Result<Debouncer<RecommendedWatcher, RecommendedCache>> {
    let handler_shared = shared.clone();
    let mut debouncer = new_debouncer(debounce, None, move |result: DebounceEventResult| {
        let s = &handler_shared;
        let relevant = match result {
            Ok(events) => events
                .iter()
                .flat_map(|e| e.paths.iter())
                .fold(false, |any, p| s.sync_path(p) | any),
            // Event queue overflow or watcher error: re-scan to be safe.
            Err(errors) => {
                tracing::warn!(?errors, "watcher error; scheduling full update");
                true
            }
        };
        if relevant {
            let _ = s.jobs.send(Job::Update);
        }
    })
    .map_err(|e| Error::io(&shared.config.root, io::Error::other(e)))?;
    debouncer
        .watch(&shared.config.root, RecursiveMode::Recursive)
        .map_err(|e| Error::io(&shared.config.root, io::Error::other(e)))?;
    Ok(debouncer)
}

fn worker_loop(shared: &Shared, rx: &Receiver<Job>, opts: &DaemonOptions) {
    let mut last_update = Instant::now();
    let mut last_rehash = Instant::now();
    loop {
        let wait = opts.reconcile_every.saturating_sub(last_update.elapsed());
        match rx.recv_timeout(wait) {
            Ok(Job::Stop) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(Job::Update) => {
                // Coalesce a burst of requests into one scan.
                while let Ok(job) = rx.try_recv() {
                    if matches!(job, Job::Stop) {
                        return;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        let force = last_rehash.elapsed() >= opts.rehash_every;
        match shared.run_update(force) {
            Ok(_) => {}
            Err(e) => tracing::warn!(%e, "background update failed"),
        }
        last_update = Instant::now();
        if force {
            last_rehash = last_update;
        }
    }
}

fn accept_loop(shared: &Arc<Shared>, listener: &UnixListener) {
    for stream in listener.incoming() {
        if shared.shutting_down.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = stream else { continue };
        let shared = shared.clone();
        let _ = thread::Builder::new()
            .name("loupe-conn".into())
            .spawn(move || serve_connection(&shared, stream));
    }
    let _ = fs::remove_file(&shared.socket);
}

fn serve_connection(shared: &Shared, mut stream: UnixStream) {
    loop {
        let request: Request = match read_frame(&mut stream) {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(e) => {
                // A malformed frame leaves the stream unframed, so answer once and hang up.
                let _ = write_frame(&mut stream, &Response::Error(format!("bad request: {e}")));
                return;
            }
        };
        let shutting_down = matches!(request, Request::Shutdown);
        let (kind, started) = (request.kind(), Instant::now());
        let response = shared.handle(request);
        shared.metrics.record(
            kind,
            started.elapsed(),
            matches!(response, Response::Error(_)),
        );
        if write_frame(&mut stream, &response).is_err() || shutting_down {
            return;
        }
    }
}
