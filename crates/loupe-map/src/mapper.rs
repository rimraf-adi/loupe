//! The mapper: cached module summaries and the token-budgeted repo map.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};
use std::path::Path;

use loupe_core::{Error, Result, Snapshot, TokenCounter};
use loupe_graph::{EdgeKind, Graph, RelatedOptions};
use loupe_symbols::{SymbolKind, SymbolSearcher, Visibility};

use crate::cache::SummaryCache;
use crate::merkle::MerkleTree;
use crate::summarize::{FileInfo, ModuleInput, Summarizer, extract_purpose};

/// Cache level for module summaries.
const LEVEL_MODULE: u8 = 1;
/// Version of how [`ModuleInput`]s are assembled from a directory (which docs count, which
/// symbols are listed, ...). It is part of the cache key alongside the summarizer's own version:
/// change it whenever input assembly changes, or stale summaries will be served.
const INPUT_VERSION: u32 = 3;
const MAX_SYMBOLS_PER_FILE: usize = 8;
const SYMBOLS_ON_MAP_LINE: usize = 4;
const MAX_DOC_FILE_BYTES: usize = 64 * 1024;

/// A top-level definition, as much as the map needs to know about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub visibility: Visibility,
}

/// Where the mapper gets top-level symbols.
pub trait SymbolSource {
    fn top_level_symbols(&self, path: &str) -> Vec<TopSymbol>;
}

/// Where the mapper reads README / module-doc files from.
pub trait FileSource {
    fn read(&self, path: &str, max_bytes: usize) -> Option<Vec<u8>>;
}

impl SymbolSource for SymbolSearcher {
    fn top_level_symbols(&self, path: &str) -> Vec<TopSymbol> {
        let Ok(Some(outline)) = self.outline(path, None) else {
            return Vec::new();
        };
        outline
            .symbols
            .into_iter()
            .filter(|s| s.parent.is_none())
            .map(|s| TopSymbol {
                name: s.name,
                kind: s.kind,
                visibility: s.visibility,
            })
            .collect()
    }
}

/// Reads files relative to a repo root.
#[derive(Debug)]
pub struct DiskFiles<'a>(pub &'a Path);

impl FileSource for DiskFiles<'_> {
    fn read(&self, path: &str, max_bytes: usize) -> Option<Vec<u8>> {
        use std::io::Read;
        let mut buf = Vec::new();
        std::fs::File::open(self.0.join(path))
            .ok()?
            .take(max_bytes as u64)
            .read_to_end(&mut buf)
            .ok()?;
        Some(buf)
    }
}

pub struct MapContext<'a> {
    pub snapshot: &'a Snapshot,
    pub symbols: &'a dyn SymbolSource,
    pub files: &'a dyn FileSource,
    pub graph: &'a Graph,
    pub cache: &'a SummaryCache,
    pub summarizer: &'a dyn Summarizer,
    pub counter: &'a dyn TokenCounter,
}

impl std::fmt::Debug for MapContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MapContext")
            .field("files", &self.snapshot.len())
            .field("summarizer", &self.summarizer.version())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct MapOptions {
    /// Token budget for the whole map.
    pub budget: usize,
    /// Paths (files or directories), or substrings, to bring into focus: they and the files
    /// closest to them in the graph are shown first.
    pub focus: Vec<String>,
    /// Name shown in the header.
    pub name: String,
}

impl Default for MapOptions {
    fn default() -> Self {
        MapOptions {
            budget: 2000,
            focus: Vec::new(),
            name: "repo".to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RepoMap {
    pub text: String,
    /// Tokens of `text` by the context's counter.
    pub tokens: usize,
    pub shown_files: usize,
    pub shown_dirs: usize,
    pub total_files: usize,
    pub total_dirs: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModuleSummary {
    pub path: String,
    /// One to three lines.
    pub text: String,
    pub files: usize,
    pub subdirs: Vec<String>,
    /// Directories outside this one that its files import from, most-imported first.
    pub depends_on: Vec<String>,
    /// Hex of the directory's Merkle hash (the cache key).
    pub hash: String,
}

/// How many summaries a pass computed versus reused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SummarizeStats {
    pub computed: usize,
    pub cached: usize,
}

pub struct RepoMapper<'a> {
    ctx: MapContext<'a>,
    tree: std::borrow::Cow<'a, MerkleTree>,
    version: String,
    computed: Cell<usize>,
    cached: Cell<usize>,
}

impl std::fmt::Debug for RepoMapper<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoMapper")
            .field("dirs", &self.tree.len())
            .field("version", &self.version)
            .finish()
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn hex(h: &[u8; 32]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

fn indent(depth: usize) -> String {
    "  ".repeat(depth)
}

/// How strongly a file speaks for its directory (lower is better): a README, then the package
/// manifest, then the module's entry file. `None` for files that say nothing about the directory.
fn doc_rank(name: &str) -> Option<u8> {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("readme") {
        Some(0)
    } else if matches!(
        lower.as_str(),
        "cargo.toml" | "package.json" | "pyproject.toml"
    ) {
        Some(1)
    } else if matches!(
        lower.as_str(),
        "lib.rs" | "mod.rs" | "__init__.py" | "doc.go"
    ) {
        Some(2)
    } else if matches!(
        lower.as_str(),
        "index.ts" | "index.tsx" | "index.js" | "index.jsx" | "index.mjs"
    ) {
        // Only JS/TS barrel files: `index.rs` is just a module that happens to be named index.
        Some(3)
    } else {
        None
    }
}

impl<'a> RepoMapper<'a> {
    /// Builds the Merkle tree from the snapshot. Callers that map the same snapshot repeatedly
    /// should build it once and use [`RepoMapper::with_tree`].
    pub fn new(ctx: MapContext<'a>) -> Self {
        let tree = std::borrow::Cow::Owned(MerkleTree::build(ctx.snapshot));
        Self::assemble(ctx, tree)
    }

    /// Use a prebuilt tree, which must be [`MerkleTree::build`] of `ctx.snapshot`.
    pub fn with_tree(ctx: MapContext<'a>, tree: &'a MerkleTree) -> Self {
        Self::assemble(ctx, std::borrow::Cow::Borrowed(tree))
    }

    fn assemble(ctx: MapContext<'a>, tree: std::borrow::Cow<'a, MerkleTree>) -> Self {
        let version = format!("{}/in{INPUT_VERSION}", ctx.summarizer.version());
        RepoMapper {
            ctx,
            tree,
            version,
            computed: Cell::new(0),
            cached: Cell::new(0),
        }
    }

    pub fn merkle(&self) -> &MerkleTree {
        &self.tree
    }

    /// Reset and return the compute/cache counters (for tests and diagnostics).
    pub fn take_stats(&self) -> SummarizeStats {
        SummarizeStats {
            computed: self.computed.take(),
            cached: self.cached.take(),
        }
    }

    /// Summarize every directory bottom-up. `computed` is how many directories needed a fresh
    /// summary; `cached` is every other directory, whose summary was already stored under its
    /// current hash.
    pub fn summarize_all(&self) -> Result<SummarizeStats> {
        self.take_stats();
        let mut dirs: Vec<&str> = self.tree.dir_paths().collect();
        dirs.sort_by_key(|d| {
            std::cmp::Reverse(d.matches('/').count() + usize::from(!d.is_empty()))
        });
        let total = dirs.len();
        for d in dirs {
            self.summary_text(d)?;
        }
        let computed = self.take_stats().computed;
        Ok(SummarizeStats {
            computed,
            cached: total - computed,
        })
    }

    fn summary_text(&self, dir: &str) -> Result<String> {
        let node = self.tree.node(dir).ok_or_else(|| {
            Error::InvalidArgument(format!("no such directory in the snapshot: {dir:?}"))
        })?;
        if let Some(text) = self.ctx.cache.get(LEVEL_MODULE, &node.hash, &self.version) {
            self.cached.set(self.cached.get() + 1);
            return Ok(text);
        }

        let mut subdirs = Vec::new();
        for name in &node.dirs {
            let text = self.summary_text(&join(dir, name))?;
            subdirs.push((name.clone(), text.lines().next().unwrap_or("").to_string()));
        }
        let mut files = Vec::new();
        let mut purpose: Option<(u8, String)> = None;
        for name in &node.files {
            let path = join(dir, name);
            let mut top = self.ctx.symbols.top_level_symbols(&path);
            let symbol_count = top.len();
            // Real definitions first, public before private; `mod x;` declarations last. The sort
            // is stable, so source order is kept within each group.
            top.sort_by_key(|s| {
                (
                    s.kind == SymbolKind::Module,
                    s.visibility != Visibility::Public,
                )
            });
            files.push(FileInfo {
                name: name.clone(),
                lang: self.ctx.snapshot.get(&path).and_then(|m| m.lang.clone()),
                symbols: top
                    .into_iter()
                    .take(MAX_SYMBOLS_PER_FILE)
                    .map(|s| s.name)
                    .collect(),
                symbol_count,
            });
            if let Some(rank) = doc_rank(name)
                && purpose.as_ref().is_none_or(|(best, _)| rank < *best)
                && let Some(p) = self
                    .ctx
                    .files
                    .read(&path, MAX_DOC_FILE_BYTES)
                    .and_then(|b| extract_purpose(name, &b))
            {
                purpose = Some((rank, p));
            }
        }
        let input = ModuleInput {
            path: dir.to_string(),
            files,
            subdirs,
            purpose: purpose.map(|(_, p)| p),
        };
        let text = self.ctx.summarizer.summarize(&input);
        self.ctx
            .cache
            .put(LEVEL_MODULE, &node.hash, &self.version, &text);
        self.computed.set(self.computed.get() + 1);
        Ok(text)
    }

    /// One to three lines about a directory, plus what it depends on.
    pub fn module_summary(&self, dir: &str) -> Result<ModuleSummary> {
        let dir = dir.trim_matches('/');
        let text = self.summary_text(dir)?;
        let node = self.tree.node(dir).unwrap();
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        let files = self
            .ctx
            .snapshot
            .files
            .range(prefix.clone()..)
            .take_while(|(p, _)| p.starts_with(&prefix))
            .count();
        Ok(ModuleSummary {
            path: dir.to_string(),
            text,
            files,
            subdirs: node.dirs.clone(),
            depends_on: self.depends_on(&prefix),
            hash: hex(&node.hash),
        })
    }

    /// Directories outside `prefix` that files under it import from, most-imported first.
    fn depends_on(&self, prefix: &str) -> Vec<String> {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for (path, _) in self
            .ctx
            .snapshot
            .files
            .range(prefix.to_string()..)
            .take_while(|(p, _)| p.starts_with(prefix))
        {
            let Some(node) = self.ctx.graph.node(path) else {
                continue;
            };
            for e in self
                .ctx
                .graph
                .out_edges(node)
                .iter()
                .filter(|e| e.kind == EdgeKind::Imports)
            {
                let target = self.ctx.graph.path(e.node);
                if !target.starts_with(prefix) || prefix.is_empty() {
                    if prefix.is_empty() {
                        continue; // everything is inside the root
                    }
                    let dir = target.rsplit_once('/').map_or("", |(d, _)| d);
                    *counts.entry(dir.to_string()).or_default() += 1;
                }
            }
        }
        let mut v: Vec<(String, usize)> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.into_iter()
            .take(5)
            .map(|(d, _)| {
                if d.is_empty() {
                    "(root)".to_string()
                } else {
                    d
                }
            })
            .collect()
    }

    // ---- repo map ---------------------------------------------------------------------

    /// Render the map: header, entry points, and as much of the directory tree as fits the
    /// budget, choosing files and directories by graph importance and focus.
    pub fn repo_map(&self, opts: &MapOptions) -> Result<RepoMap> {
        let snapshot = self.ctx.snapshot;
        let total_files = snapshot.len();
        let n = total_files.max(1) as f32;

        // Scores: PageRank scaled so the average file is ~1, plus focus boosts.
        let mut file_score: HashMap<&str, f32> = HashMap::new();
        for path in snapshot.files.keys() {
            let mut base = self.ctx.graph.rank_of(path).unwrap_or(1.0 / n) * n;
            // Config, data, and docs rarely answer "where is the code for X?".
            let lang = snapshot.get(path).and_then(|m| m.lang.as_deref());
            if lang.is_none_or(|l| {
                matches!(
                    l,
                    "toml"
                        | "yaml"
                        | "json"
                        | "markdown"
                        | "html"
                        | "css"
                        | "sql"
                        | "make"
                        | "dockerfile"
                        | "cmake"
                )
            }) {
                base *= 0.5;
            }
            file_score.insert(path.as_str(), base);
        }
        self.apply_focus(opts, &mut file_score);

        // Directory score = best file in its subtree.
        let mut dir_score: HashMap<String, f32> = HashMap::new();
        for (path, score) in &file_score {
            let mut d = path.rsplit_once('/').map_or("", |(d, _)| d);
            loop {
                let e = dir_score.entry(d.to_string()).or_insert(0.0);
                if *score > *e {
                    *e = *score;
                }
                if d.is_empty() {
                    break;
                }
                d = d.rsplit_once('/').map_or("", |(p, _)| p);
            }
        }

        let header = self.header(opts);
        let entry = self.entry_points(&file_score);
        let mut fixed = header.clone();
        let root_summary = self.summary_text("")?;
        if let Some(first) = root_summary.lines().next().filter(|l| !l.is_empty()) {
            fixed.push_str(&format!("Summary: {first}\n"));
        }
        if !entry.is_empty() {
            fixed.push_str(&format!("Entry points: {}\n", entry.join(", ")));
        }
        let fixed_cost = self.ctx.counter.count(&fixed);
        // Leave slack for the "… +N more" lines that are added after admission.
        let mut remaining = (opts.budget as f64 * 0.9) as usize;
        remaining = remaining.saturating_sub(fixed_cost);

        // Greedy admission by score, only attaching to already-admitted directories.
        #[derive(PartialEq)]
        struct Cand {
            score: f32,
            depth: usize,
            path: String,
            is_dir: bool,
        }
        impl Eq for Cand {}
        impl Ord for Cand {
            fn cmp(&self, o: &Self) -> std::cmp::Ordering {
                self.score
                    .total_cmp(&o.score)
                    .then(o.depth.cmp(&self.depth))
                    .then_with(|| o.path.cmp(&self.path))
            }
        }
        impl PartialOrd for Cand {
            fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(o))
            }
        }

        let mut admitted_dirs: BTreeSet<String> = BTreeSet::new();
        let mut admitted_files: BTreeSet<String> = BTreeSet::new();
        let mut lines: HashMap<String, String> = HashMap::new(); // rendered line per admitted node
        let mut heap: BinaryHeap<Cand> = BinaryHeap::new();

        let push_children = |heap: &mut BinaryHeap<Cand>, dir: &str| {
            let (subdirs, files) = self.tree.children_paths(dir);
            for d in subdirs {
                heap.push(Cand {
                    score: dir_score.get(&d).copied().unwrap_or(0.0),
                    depth: d.matches('/').count() + 1,
                    path: d,
                    is_dir: true,
                });
            }
            for f in files {
                heap.push(Cand {
                    score: file_score.get(f.as_str()).copied().unwrap_or(0.0),
                    depth: f.matches('/').count() + 1,
                    path: f,
                    is_dir: false,
                });
            }
        };

        // The root is always shown (its line is the header's tree anchor, so it costs nothing).
        admitted_dirs.insert(String::new());
        push_children(&mut heap, "");
        // Once something important is turned away for cost, only comparably important items may
        // still use the leftover budget; otherwise cheap low-value lines (lockfiles, dotfiles)
        // would crowd out the directories we could not afford.
        let mut reject_floor: Option<f32> = None;
        while let Some(c) = heap.pop() {
            if reject_floor.is_some_and(|floor| c.score < floor * 0.6) {
                break; // candidates pop in descending score order
            }
            let line = if c.is_dir {
                self.dir_line(&c.path)?
            } else {
                self.file_line(&c.path)
            };
            let cost = self.ctx.counter.count(&line) + 1;
            if cost > remaining {
                reject_floor.get_or_insert(c.score);
                continue; // a comparably important, cheaper candidate may still fit
            }
            remaining -= cost;
            lines.insert(c.path.clone(), line);
            if c.is_dir {
                admitted_dirs.insert(c.path.clone());
                push_children(&mut heap, &c.path);
            } else {
                admitted_files.insert(c.path.clone());
            }
        }

        // Render in tree order.
        let mut body = String::new();
        self.render_dir("", 0, &admitted_dirs, &admitted_files, &lines, &mut body);
        let mut text = format!("{fixed}{body}");

        // Safety net: the slack should make this a no-op, but never exceed the budget.
        let mut dropped_any = false;
        while self.ctx.counter.count(&text) > opts.budget && !admitted_files.is_empty() {
            let victim = admitted_files
                .iter()
                .min_by(|a, b| {
                    file_score[a.as_str()]
                        .total_cmp(&file_score[b.as_str()])
                        .then_with(|| b.cmp(a))
                })
                .cloned()
                .unwrap();
            admitted_files.remove(&victim);
            body.clear();
            self.render_dir("", 0, &admitted_dirs, &admitted_files, &lines, &mut body);
            text = format!("{fixed}{body}");
            dropped_any = true;
        }
        let _ = dropped_any;

        Ok(RepoMap {
            tokens: self.ctx.counter.count(&text),
            text,
            shown_files: admitted_files.len(),
            shown_dirs: admitted_dirs.len().saturating_sub(1),
            total_files,
            total_dirs: self.tree.len().saturating_sub(1),
        })
    }

    fn apply_focus(&self, opts: &MapOptions, score: &mut HashMap<&str, f32>) {
        for focus in &opts.focus {
            let focus = focus.trim_matches('/');
            if focus.is_empty() {
                continue;
            }
            let lower = focus.to_ascii_lowercase();
            let dir_prefix = format!("{focus}/");
            let paths: Vec<&str> = score.keys().copied().collect();
            for p in &paths {
                if *p == focus || p.starts_with(&dir_prefix) {
                    *score.get_mut(p).unwrap() += 50.0;
                } else if p.to_ascii_lowercase().contains(&lower) {
                    *score.get_mut(p).unwrap() += 10.0;
                }
            }
            // Files the graph connects to the focus.
            let seeds: Vec<&str> = paths.iter().copied().filter(|p| *p == focus).collect();
            for seed in seeds {
                for r in self.ctx.graph.related(
                    seed,
                    &RelatedOptions {
                        limit: 40,
                        ..RelatedOptions::default()
                    },
                ) {
                    if let Some(s) = score.get_mut(r.path.as_str()) {
                        *s += 20.0 * r.score;
                    }
                }
            }
        }
    }

    fn header(&self, opts: &MapOptions) -> String {
        let mut by_lang: BTreeMap<&str, usize> = BTreeMap::new();
        for meta in self.ctx.snapshot.files.values() {
            *by_lang
                .entry(meta.lang.as_deref().unwrap_or("other"))
                .or_default() += 1;
        }
        let total = self.ctx.snapshot.len().max(1);
        let mut langs: Vec<(&str, usize)> = by_lang.into_iter().collect();
        langs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let langs: Vec<String> = langs
            .iter()
            .take(4)
            .map(|(l, c)| format!("{l} {}%", (c * 100 + total / 2) / total))
            .collect();
        format!(
            "# {} — {} files, {} dirs · {}\n",
            opts.name,
            self.ctx.snapshot.len(),
            self.tree.len().saturating_sub(1),
            langs.join(", ")
        )
    }

    /// Conventional program entry points, best-ranked first.
    fn entry_points(&self, score: &HashMap<&str, f32>) -> Vec<String> {
        let mut found: Vec<(&str, f32)> = self
            .ctx
            .snapshot
            .files
            .keys()
            .filter(|p| {
                let name = p.rsplit('/').next().unwrap_or(p);
                matches!(
                    name,
                    "main.rs"
                        | "main.go"
                        | "main.py"
                        | "__main__.py"
                        | "main.c"
                        | "Main.java"
                        | "main.ts"
                        | "main.js"
                        | "index.ts"
                        | "index.js"
                        | "app.py"
                        | "manage.py"
                        | "server.ts"
                        | "server.js"
                ) && p.matches('/').count() <= 3
            })
            .map(|p| (p.as_str(), score.get(p.as_str()).copied().unwrap_or(0.0)))
            .collect();
        found.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        found
            .into_iter()
            .take(6)
            .map(|(p, _)| p.to_string())
            .collect()
    }

    fn dir_line(&self, dir: &str) -> Result<String> {
        let summary = self.summary_text(dir)?;
        let first = summary.lines().next().unwrap_or("");
        let files_in = {
            let prefix = format!("{dir}/");
            self.ctx
                .snapshot
                .files
                .range(prefix.clone()..)
                .take_while(|(p, _)| p.starts_with(&prefix))
                .count()
        };
        let name = dir.rsplit('/').next().unwrap_or(dir);
        let depth = dir.matches('/').count();
        let noun = if files_in == 1 { "file" } else { "files" };
        Ok(format!(
            "{}{name}/ — {first}  [{files_in} {noun}]",
            indent(depth + 1)
        ))
    }

    fn file_line(&self, path: &str) -> String {
        let (dir_depth, name) = match path.rsplit_once('/') {
            Some((d, n)) => (d.matches('/').count() + 1, n),
            None => (0, path),
        };
        let mut top = self.ctx.symbols.top_level_symbols(path);
        top.sort_by_key(|s| {
            (
                s.kind == SymbolKind::Module,
                s.visibility != Visibility::Public,
                matches!(s.kind, SymbolKind::Constant | SymbolKind::Variable),
            )
        });
        let names: Vec<String> = top
            .iter()
            .filter(|s| !matches!(s.kind, SymbolKind::Impl | SymbolKind::Module))
            .take(SYMBOLS_ON_MAP_LINE)
            .map(|s| s.name.clone())
            .collect();
        let more = top.len().saturating_sub(names.len());
        let tail = if names.is_empty() {
            String::new()
        } else {
            format!(
                " — {}{}",
                names.join(", "),
                if more > 0 {
                    format!(" (+{more})")
                } else {
                    String::new()
                }
            )
        };
        format!("{}{name}{tail}", indent(dir_depth + 1))
    }

    fn render_dir(
        &self,
        dir: &str,
        _depth: usize,
        dirs: &BTreeSet<String>,
        files: &BTreeSet<String>,
        lines: &HashMap<String, String>,
        out: &mut String,
    ) {
        let (subdirs, file_paths) = self.tree.children_paths(dir);
        let depth = if dir.is_empty() {
            0
        } else {
            dir.matches('/').count() + 1
        };
        let mut hidden_files = 0usize;
        for f in &file_paths {
            if files.contains(f) {
                out.push_str(&lines[f]);
                out.push('\n');
            } else {
                hidden_files += 1;
            }
        }
        for d in &subdirs {
            if dirs.contains(d) {
                out.push_str(&lines[d]);
                out.push('\n');
                self.render_dir(d, depth + 1, dirs, files, lines, out);
            }
        }
        let hidden_dirs = subdirs.iter().filter(|d| !dirs.contains(*d)).count();
        if hidden_files + hidden_dirs > 0 {
            let mut parts = Vec::new();
            if hidden_files > 0 {
                parts.push(format!(
                    "{hidden_files} file{}",
                    if hidden_files == 1 { "" } else { "s" }
                ));
            }
            if hidden_dirs > 0 {
                parts.push(format!(
                    "{hidden_dirs} dir{}",
                    if hidden_dirs == 1 { "" } else { "s" }
                ));
            }
            out.push_str(&format!("{}… +{}\n", indent(depth + 1), parts.join(", ")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::summarize::{CallbackSummarizer, HeuristicSummarizer};
    use loupe_core::{ApproxTokenCounter, BlobId, FileMeta};
    use loupe_graph::GraphBuilder;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct FakeSymbols(HashMap<String, Vec<TopSymbol>>);
    impl SymbolSource for FakeSymbols {
        fn top_level_symbols(&self, path: &str) -> Vec<TopSymbol> {
            self.0.get(path).cloned().unwrap_or_default()
        }
    }

    #[derive(Default)]
    struct FakeFiles(HashMap<String, String>);
    impl FileSource for FakeFiles {
        fn read(&self, path: &str, _max: usize) -> Option<Vec<u8>> {
            self.0.get(path).map(|s| s.as_bytes().to_vec())
        }
    }

    fn sym(name: &str) -> TopSymbol {
        TopSymbol {
            name: name.into(),
            kind: SymbolKind::Function,
            visibility: Visibility::Public,
        }
    }

    struct World {
        snapshot: Snapshot,
        symbols: FakeSymbols,
        files: FakeFiles,
        graph: Graph,
    }

    /// `files`: (path, content marker). `imports`: (from, to) pairs among those paths.
    fn world(files: &[(&str, &str)], imports: &[(&str, &str)]) -> World {
        let mut snapshot = Snapshot::default();
        let mut symbols = FakeSymbols::default();
        for (p, c) in files {
            snapshot.files.insert(
                (*p).to_string(),
                FileMeta {
                    size: c.len() as u64,
                    mtime_ns: 0,
                    blob: BlobId::of(c.as_bytes()),
                    lang: Some("rust".into()),
                },
            );
            let stem = p.rsplit('/').next().unwrap().trim_end_matches(".rs");
            symbols.0.insert(
                (*p).to_string(),
                vec![sym(&format!("{stem}_fn")), sym(&format!("{stem}_other"))],
            );
        }
        let paths: Vec<String> = snapshot.files.keys().cloned().collect();
        let id = |p: &str| paths.iter().position(|x| x == p).unwrap() as u32;
        let mut b = GraphBuilder::new(paths.len());
        for (from, to) in imports {
            b.add(id(from), id(to), EdgeKind::Imports, 1.0);
        }
        World {
            snapshot,
            symbols,
            files: FakeFiles::default(),
            graph: b.finish(paths),
        }
    }

    fn mapper<'a>(w: &'a World, cache: &'a SummaryCache, s: &'a dyn Summarizer) -> RepoMapper<'a> {
        static COUNTER: ApproxTokenCounter = ApproxTokenCounter;
        RepoMapper::new(MapContext {
            snapshot: &w.snapshot,
            symbols: &w.symbols,
            files: &w.files,
            graph: &w.graph,
            cache,
            summarizer: s,
            counter: &COUNTER,
        })
    }

    const TREE: &[(&str, &str)] = &[
        ("README.md", "r"),
        ("src/main.rs", "m"),
        ("src/core.rs", "c"),
        ("src/net/tcp.rs", "t"),
        ("src/net/udp.rs", "u"),
        ("src/util/fmt.rs", "f"),
        ("docs/guide.md", "g"),
        ("tests/it.rs", "i"),
    ];

    fn tree_world() -> World {
        world(
            TREE,
            &[
                ("src/main.rs", "src/core.rs"),
                ("src/net/tcp.rs", "src/core.rs"),
                ("src/net/udp.rs", "src/core.rs"),
                ("src/util/fmt.rs", "src/core.rs"),
                ("tests/it.rs", "src/core.rs"),
                ("src/main.rs", "src/net/tcp.rs"),
            ],
        )
    }

    #[test]
    fn single_file_change_recomputes_only_its_ancestor_path() {
        let w1 = tree_world();
        let cache = SummaryCache::in_memory();
        let h = HeuristicSummarizer;
        let m1 = mapper(&w1, &cache, &h);
        let first = m1.summarize_all().unwrap();
        assert_eq!(
            first.computed,
            m1.merkle().len(),
            "cold: every directory computed once"
        );
        assert_eq!(first.cached, 0);
        assert_eq!(
            m1.summarize_all().unwrap(),
            SummarizeStats {
                computed: 0,
                cached: m1.merkle().len()
            }
        );

        // Edit src/net/tcp.rs: its ancestors are src/net, src, and the root.
        let mut files: Vec<(&str, &str)> = TREE.to_vec();
        files[3] = ("src/net/tcp.rs", "CHANGED");
        let w2 = world(&files, &[]);
        let m2 = mapper(&w2, &cache, &h);
        let again = m2.summarize_all().unwrap();
        assert_eq!(again.computed, 3, "src/net, src, and the root");
        assert_eq!(again.cached, m2.merkle().len() - 3);
    }

    #[test]
    fn summarizer_callback_runs_only_for_changed_directories() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let c2 = calls.clone();
        let llm = CallbackSummarizer::new("v1", move |i| {
            c2.fetch_add(1, Ordering::SeqCst);
            Some(format!(
                "LLM summary of {}",
                if i.path.is_empty() { "root" } else { &i.path }
            ))
        });
        let w = tree_world();
        let cache = SummaryCache::in_memory();
        let m = mapper(&w, &cache, &llm);
        m.summarize_all().unwrap();
        let dirs = m.merkle().len();
        assert_eq!(calls.load(Ordering::SeqCst), dirs);
        m.summarize_all().unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            dirs,
            "second pass is all cache hits"
        );
        assert!(
            m.module_summary("src/net")
                .unwrap()
                .text
                .starts_with("LLM summary of src/net")
        );

        // A new prompt version must not reuse summaries from the old one.
        let llm2 = CallbackSummarizer::new("v2", |_| Some("new prompt".into()));
        let m2 = mapper(&w, &cache, &llm2);
        assert_eq!(
            m2.module_summary("src").unwrap().text.lines().next(),
            Some("new prompt")
        );
    }

    #[test]
    fn module_summary_reports_purpose_files_and_dependencies() {
        let mut w = tree_world();
        w.files.0.insert(
            "src/net/README.md".into(),
            "# Net\n\nTransport protocols for the daemon. More.\n".into(),
        );
        w.snapshot.files.insert(
            "src/net/README.md".into(),
            FileMeta {
                size: 1,
                mtime_ns: 0,
                blob: BlobId::of(b"readme"),
                lang: Some("markdown".into()),
            },
        );
        // Rebuild the graph for the new path set.
        let paths: Vec<String> = w.snapshot.files.keys().cloned().collect();
        let id = |p: &str| paths.iter().position(|x| x == p).unwrap() as u32;
        let mut b = GraphBuilder::new(paths.len());
        b.add(
            id("src/net/tcp.rs"),
            id("src/core.rs"),
            EdgeKind::Imports,
            1.0,
        );
        b.add(
            id("src/net/udp.rs"),
            id("src/core.rs"),
            EdgeKind::Imports,
            1.0,
        );
        b.add(
            id("src/net/udp.rs"),
            id("src/util/fmt.rs"),
            EdgeKind::Imports,
            1.0,
        );
        w.graph = b.finish(paths);

        let cache = SummaryCache::in_memory();
        let h = HeuristicSummarizer;
        let m = mapper(&w, &cache, &h);
        let s = m.module_summary("src/net/").unwrap();
        assert_eq!(s.path, "src/net");
        assert_eq!(
            s.text.lines().next(),
            Some("Transport protocols for the daemon.")
        );
        assert!(s.text.contains("Key files: tcp.rs, udp.rs"), "{}", s.text);
        assert_eq!(s.files, 3);
        assert_eq!(
            s.depends_on,
            ["src", "src/util"],
            "core.rs imported twice, fmt.rs once"
        );
        assert_eq!(s.hash.len(), 64);
        assert!(m.module_summary("").unwrap().depends_on.is_empty());
        assert_eq!(
            m.module_summary("").unwrap().subdirs,
            ["docs", "src", "tests"]
        );
        assert!(matches!(
            m.module_summary("no/such/dir"),
            Err(Error::InvalidArgument(_))
        ));
    }

    #[test]
    fn repo_map_is_deterministic_and_has_a_header_and_entry_points() {
        let w = tree_world();
        let h = HeuristicSummarizer;
        let opts = MapOptions {
            budget: 400,
            name: "demo".into(),
            ..MapOptions::default()
        };
        let a = mapper(&w, &SummaryCache::in_memory(), &h)
            .repo_map(&opts)
            .unwrap();
        let b = mapper(&w, &SummaryCache::in_memory(), &h)
            .repo_map(&opts)
            .unwrap();
        assert_eq!(a, b, "fresh mappers and caches give identical output");
        let cache = SummaryCache::in_memory();
        let m = mapper(&w, &cache, &h);
        assert_eq!(
            m.repo_map(&opts).unwrap(),
            m.repo_map(&opts).unwrap(),
            "cache hits give identical output"
        );
        assert!(
            a.text.starts_with("# demo — 8 files, 5 dirs · rust"),
            "{}",
            a.text
        );
        assert!(a.text.contains("Entry points: src/main.rs"), "{}", a.text);
        assert_eq!(a.total_files, 8);
    }

    #[test]
    fn budget_is_respected_and_more_budget_shows_more() {
        let w = tree_world();
        let h = HeuristicSummarizer;
        let cache = SummaryCache::in_memory();
        let m = mapper(&w, &cache, &h);
        let mut last_files = 0;
        for budget in [60, 90, 120, 200, 400, 1000, 4000] {
            let r = m
                .repo_map(&MapOptions {
                    budget,
                    ..MapOptions::default()
                })
                .unwrap();
            assert!(
                r.tokens <= budget,
                "budget {budget}: {} tokens\n{}",
                r.tokens,
                r.text
            );
            assert!(
                r.shown_files >= last_files,
                "budget {budget} showed fewer files than a smaller one"
            );
            last_files = r.shown_files;
        }
        let full = m
            .repo_map(&MapOptions {
                budget: 100_000,
                ..MapOptions::default()
            })
            .unwrap();
        assert_eq!(full.shown_files, 8, "everything fits");
        assert_eq!(full.shown_dirs, 5);
        assert!(
            !full.text.contains('…') || !full.text.contains("+"),
            "no elision when all shown: {}",
            full.text
        );
    }

    #[test]
    fn important_files_appear_before_unimportant_ones() {
        let w = tree_world();
        let h = HeuristicSummarizer;
        let cache = SummaryCache::in_memory();
        let m = mapper(&w, &cache, &h);
        // core.rs is imported by five files, so it must be the first file admitted.
        let tiny = m
            .repo_map(&MapOptions {
                budget: 110,
                ..MapOptions::default()
            })
            .unwrap();
        assert!(tiny.text.contains("core.rs"), "{}", tiny.text);
        assert!(
            !tiny.text.contains("guide.md"),
            "low-rank docs are the first to go: {}",
            tiny.text
        );
        assert!(tiny.text.contains('…'), "elision is marked: {}", tiny.text);
    }

    #[test]
    fn focus_pulls_a_low_ranked_file_into_a_small_map() {
        let w = tree_world();
        let h = HeuristicSummarizer;
        let cache = SummaryCache::in_memory();
        let m = mapper(&w, &cache, &h);
        let opts = |focus: &[&str]| MapOptions {
            budget: 110,
            focus: focus.iter().map(|s| s.to_string()).collect(),
            ..MapOptions::default()
        };
        assert!(!m.repo_map(&opts(&[])).unwrap().text.contains("guide.md"));
        let focused = m.repo_map(&opts(&["docs/guide.md"])).unwrap();
        assert!(focused.text.contains("guide.md"), "{}", focused.text);
        // A directory focus works, and so does a substring.
        assert!(
            m.repo_map(&opts(&["docs"]))
                .unwrap()
                .text
                .contains("guide.md")
        );
        assert!(
            m.repo_map(&opts(&["guide"]))
                .unwrap()
                .text
                .contains("guide.md")
        );
        // Focus on a file also lifts its graph neighbours.
        let near = m.repo_map(&opts(&["src/net/udp.rs"])).unwrap();
        assert!(
            near.text.contains("udp.rs") && near.text.contains("core.rs"),
            "{}",
            near.text
        );
    }

    #[test]
    fn empty_and_flat_repos_render() {
        let h = HeuristicSummarizer;
        let empty = world(&[], &[]);
        let cache = SummaryCache::in_memory();
        let m = mapper(&empty, &cache, &h);
        let r = m.repo_map(&MapOptions::default()).unwrap();
        assert!(r.text.starts_with("# repo — 0 files"));
        assert_eq!((r.shown_files, r.total_files), (0, 0));

        let flat = world(&[("a.rs", "a"), ("b.rs", "b")], &[]);
        let r = mapper(&flat, &SummaryCache::in_memory(), &h)
            .repo_map(&MapOptions::default())
            .unwrap();
        assert!(
            r.text.contains("a.rs — a_fn, a_other") && r.text.contains("b.rs"),
            "{}",
            r.text
        );
    }

    #[test]
    fn a_budget_smaller_than_the_header_still_returns_the_header() {
        let w = tree_world();
        let h = HeuristicSummarizer;
        let r = mapper(&w, &SummaryCache::in_memory(), &h)
            .repo_map(&MapOptions {
                budget: 1,
                ..MapOptions::default()
            })
            .unwrap();
        assert!(r.text.starts_with("# repo"));
        assert_eq!(r.shown_files, 0);
    }
}
