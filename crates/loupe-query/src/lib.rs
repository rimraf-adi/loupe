//! Hybrid retrieval: one natural-language question in, a small token-budgeted answer out.
//!
//! The pipeline (docs/v1.md section 5.8):
//! 1. retrieve independently: IDF-weighted keyword search, symbol-name matches, semantic chunks;
//! 2. fuse the ranked file lists with Reciprocal Rank Fusion;
//! 3. expand along the file graph from the best hits (imports, tests, references, co-change);
//! 4. re-rank with a light importance (PageRank) and recency prior;
//! 5. pack evidence (symbol signatures, matching lines, best semantic chunks) into the budget.
//!
//! Each stage can be switched off, which is how the ablation eval measures what each one is
//! worth.

mod terms;

use std::collections::HashMap;
use std::time::Instant;

use loupe_core::{ApproxTokenCounter, Result, TokenCounter};
use loupe_embed::{SemanticOptions, SemanticSearcher, SemanticState};
use loupe_graph::{EdgeKind, Graph, RelatedOptions};
use loupe_overlay::Overlay;
use loupe_symbols::SymbolSearcher;
use loupe_text::{SearchOptions, TextSearcher};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

pub use terms::{Term, query_terms};

/// Everything a query reads. Searchers are pinned to one snapshot; build them once and reuse.
pub struct QueryContext<'a> {
    pub text: &'a TextSearcher,
    pub symbols: &'a SymbolSearcher,
    pub semantic: &'a SemanticSearcher,
    /// Needed only for graph expansion and the importance prior.
    pub graph: Option<&'a Graph>,
    pub overlay: Option<&'a Overlay>,
    pub counter: &'a dyn TokenCounter,
}

impl std::fmt::Debug for QueryContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryContext")
            .field("snapshot", &self.text.snapshot_id())
            .field("graph", &self.graph.is_some())
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueryOptions {
    pub budget_tokens: usize,
    pub max_files: usize,
    pub use_text: bool,
    pub use_symbols: bool,
    pub use_semantic: bool,
    /// Expand from the top hits along graph edges (off by default; see [`QueryOptions::default`]).
    pub use_graph: bool,
    /// Add the importance and recency priors.
    pub use_priors: bool,
    /// Weight of the semantic signal's reciprocal-rank contribution (text is 1.0).
    pub semantic_weight: f32,
    /// Weight of graph expansion gains.
    pub graph_weight: f32,
    /// Reciprocal Rank Fusion constant: smaller values favour each signal's top ranks more.
    pub rrf_k: f32,
}

/// Defaults chosen by a tune/held-out sweep on the localization eval (see docs/progress.md, M8):
/// a small RRF constant keeps strong lexical hits at the top, the weak semantic signal is
/// down-weighted, and graph expansion is opt-in because it helped only the tail on the harder repo
/// while hurting top ranks on small ones.
impl Default for QueryOptions {
    fn default() -> Self {
        QueryOptions {
            budget_tokens: 1500,
            max_files: 12,
            use_text: true,
            use_symbols: true,
            use_semantic: true,
            use_graph: false,
            use_priors: true,
            semantic_weight: 0.2,
            graph_weight: 0.3,
            rrf_k: 8.0,
        }
    }
}

/// Which retrieval stage contributed evidence for a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Signal {
    Text,
    Symbol,
    Semantic,
    /// Pulled in because it is connected to a strong hit (see `via`).
    Graph,
}

impl Signal {
    pub fn as_str(self) -> &'static str {
        match self {
            Signal::Text => "text",
            Signal::Symbol => "symbol",
            Signal::Semantic => "semantic",
            Signal::Graph => "graph",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub signal: Signal,
    /// 1-based, inclusive; 0 when the evidence is not tied to lines.
    pub start_line: u32,
    pub end_line: u32,
    /// A signature, a matching line, or a chunk snippet.
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueryHit {
    pub path: String,
    pub score: f32,
    pub signals: Vec<Signal>,
    /// For graph hits: the strong hit that led here and the edge kinds followed.
    pub via: Option<(String, Vec<EdgeKind>)>,
    pub evidence: Vec<Evidence>,
    /// The file differs on disk from the snapshot, so line numbers and snippets may be off.
    pub stale: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    pub snapshot_id: u64,
    /// A hit's file differs on disk from the snapshot, so its snippets may be out of date.
    pub stale: bool,
    pub semantic: SemanticState,
    /// The terms the query was reduced to for keyword and symbol retrieval.
    pub terms: Vec<String>,
    pub hits: Vec<QueryHit>,
    /// Tokens used by the rendered answer.
    pub tokens: usize,
    /// More files qualified than fit in the budget.
    pub truncated: bool,
    /// Milliseconds per stage.
    pub timings_ms: Vec<(String, u64)>,
}

/// Weights of each signal's reciprocal-rank contribution.
const W_TEXT: f32 = 1.0;
const W_SYMBOL: f32 = 1.2;
const GRAPH_SEEDS: usize = 5;
/// Importance prior: PageRank scaled so the average file is 1, capped, times this.
const PRIOR_RANK: f32 = 0.0015;
const PRIOR_RANK_CAP: f32 = 4.0;
/// Recency prior: files touched within this window get a small boost.
const RECENT_WINDOW_NS: f64 = 30.0 * 86_400.0 * 1e9;
const PRIOR_RECENT: f32 = 0.002;
const KEYWORD_LIMIT: usize = 8;

struct Ranked {
    /// File -> best-first rank list for one signal.
    order: Vec<String>,
    evidence: HashMap<String, Vec<Evidence>>,
}

fn lexical(ctx: &QueryContext<'_>, terms: &[Term]) -> Result<Ranked> {
    let n_files = ctx.text.file_count().max(1) as f32;
    let opts = SearchOptions {
        case_insensitive: true,
        literal: true,
        max_files: usize::MAX,
        max_matches_per_file: 1,
        context_lines: 0,
        timeout: Some(std::time::Duration::from_millis(400)),
    };
    // One search per term, in parallel; each yields the files containing it.
    let per_term: Vec<Result<loupe_text::SearchResult>> = terms
        .par_iter()
        .take(KEYWORD_LIMIT)
        .map(|t| ctx.text.search_with(&t.text, &opts, ctx.overlay))
        .collect();

    let mut score: HashMap<String, f32> = HashMap::new();
    let mut evidence: HashMap<String, Vec<Evidence>> = HashMap::new();
    for (term, result) in terms.iter().zip(per_term) {
        let r = result?;
        // Rare terms say more: IDF over files.
        let idf = (1.0 + n_files / (1.0 + r.total_files as f32)).ln() * term.weight;
        for hit in r.hits {
            *score.entry(hit.path.clone()).or_default() += idf;
            if let Some(s) = hit.snippets.into_iter().next() {
                let ev = evidence.entry(hit.path).or_default();
                if ev.len() < 2 {
                    ev.push(Evidence {
                        signal: Signal::Text,
                        start_line: s.start_line as u32,
                        end_line: s.end_line as u32,
                        text: s.text,
                    });
                }
            }
        }
    }
    Ok(rank_by_score(score, evidence))
}

fn rank_by_score(score: HashMap<String, f32>, evidence: HashMap<String, Vec<Evidence>>) -> Ranked {
    let mut v: Vec<(String, f32)> = score.into_iter().collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ranked {
        order: v.into_iter().map(|(p, _)| p).collect(),
        evidence,
    }
}

fn symbolic(ctx: &QueryContext<'_>, terms: &[Term]) -> Result<Ranked> {
    let mut score: HashMap<String, f32> = HashMap::new();
    let mut evidence: HashMap<String, Vec<Evidence>> = HashMap::new();
    for t in terms.iter().take(KEYWORD_LIMIT) {
        // Names are matched as typed and as written in the query (case matters for symbols).
        let mut spellings = vec![t.original.clone()];
        if t.text != t.original {
            spellings.push(t.text.clone());
        }
        for name in spellings {
            for hit in ctx.symbols.find(&name, None, 50, ctx.overlay)? {
                *score.entry(hit.path.clone()).or_default() += t.weight;
                let ev = evidence.entry(hit.path).or_default();
                if ev.len() < 3 {
                    ev.push(Evidence {
                        signal: Signal::Symbol,
                        start_line: hit.start_line,
                        end_line: hit.end_line,
                        text: hit.signature,
                    });
                }
            }
        }
    }
    // Definitions whose name contains several of the question's words ("validate session token"
    // -> `validate_session_token`): the definition itself, not just its callers.
    if terms.len() >= 2 {
        let words: Vec<String> = terms
            .iter()
            .take(KEYWORD_LIMIT)
            .map(|t| t.text.clone())
            .collect();
        for (hit, matched) in ctx.symbols.find_by_words(&words, 2, 20)? {
            *score.entry(hit.path.clone()).or_default() +=
                matched as f32 / words.len() as f32 * 2.0;
            let ev = evidence.entry(hit.path).or_default();
            if ev.len() < 3 {
                ev.push(Evidence {
                    signal: Signal::Symbol,
                    start_line: hit.start_line,
                    end_line: hit.end_line,
                    text: hit.signature,
                });
            }
        }
    }
    Ok(rank_by_score(score, evidence))
}

fn semantic(ctx: &QueryContext<'_>, query: &str) -> Result<(Ranked, SemanticState)> {
    let r = ctx.semantic.search(
        query,
        &SemanticOptions {
            k: 30,
            per_file: 1,
            snippet_lines: 4,
            ..SemanticOptions::default()
        },
        ctx.overlay,
    )?;
    let mut order = Vec::new();
    let mut evidence = HashMap::new();
    for h in r.hits {
        evidence.insert(
            h.path.clone(),
            vec![Evidence {
                signal: Signal::Semantic,
                start_line: h.start_line,
                end_line: h.end_line,
                text: format!(
                    "{}: {}",
                    h.breadcrumb,
                    h.snippet.lines().next().unwrap_or("")
                ),
            }],
        );
        order.push(h.path);
    }
    Ok((Ranked { order, evidence }, r.state))
}

/// Answer `query`.
pub fn query(ctx: &QueryContext<'_>, query: &str, opts: &QueryOptions) -> Result<QueryResult> {
    let mut timings: Vec<(String, u64)> = Vec::new();
    let mut time = |name: &str, since: Instant| {
        timings.push((name.to_string(), since.elapsed().as_millis() as u64))
    };
    let terms = query_terms(query);

    // 1. Independent retrieval.
    let t0 = Instant::now();
    let (lex, sem) = rayon::join(
        || {
            if opts.use_text {
                lexical(ctx, &terms).map(Some)
            } else {
                Ok(None)
            }
        },
        || {
            if opts.use_semantic {
                semantic(ctx, query).map(Some)
            } else {
                Ok(None)
            }
        },
    );
    let lex = lex?;
    let (sem, sem_state) = match sem? {
        Some((r, s)) => (Some(r), s),
        None => (
            None,
            if opts.use_semantic {
                SemanticState::Off
            } else {
                SemanticState::Full
            },
        ),
    };
    time("retrieve", t0);
    let t0 = Instant::now();
    let sym = if opts.use_symbols {
        Some(symbolic(ctx, &terms)?)
    } else {
        None
    };
    time("symbols", t0);

    // 2. Reciprocal Rank Fusion over files.
    let t0 = Instant::now();
    let mut fused: HashMap<String, f32> = HashMap::new();
    let mut signals: HashMap<String, Vec<Signal>> = HashMap::new();
    let mut evidence: HashMap<String, Vec<Evidence>> = HashMap::new();
    for (ranked, weight, signal) in [
        (lex.as_ref(), W_TEXT, Signal::Text),
        (sym.as_ref(), W_SYMBOL, Signal::Symbol),
        (sem.as_ref(), opts.semantic_weight, Signal::Semantic),
    ] {
        let Some(r) = ranked else { continue };
        for (rank, path) in r.order.iter().enumerate() {
            *fused.entry(path.clone()).or_default() += weight / (opts.rrf_k + rank as f32 + 1.0);
            signals.entry(path.clone()).or_default().push(signal);
            evidence
                .entry(path.clone())
                .or_default()
                .extend(r.evidence.get(path).cloned().unwrap_or_default());
        }
    }
    time("fuse", t0);

    // 3. Graph expansion from the strongest hits.
    let t0 = Instant::now();
    let mut via: HashMap<String, (String, Vec<EdgeKind>)> = HashMap::new();
    if opts.use_graph
        && let Some(graph) = ctx.graph
    {
        let mut seeds: Vec<(&String, &f32)> = fused.iter().collect();
        seeds.sort_by(|a, b| b.1.total_cmp(a.1).then_with(|| a.0.cmp(b.0)));
        let seeds: Vec<(String, f32)> = seeds
            .into_iter()
            .take(GRAPH_SEEDS)
            .map(|(p, s)| (p.clone(), *s))
            .collect();
        for (seed, seed_score) in seeds {
            let related = graph.related(
                &seed,
                &RelatedOptions {
                    kinds: vec![
                        EdgeKind::Imports,
                        EdgeKind::Tests,
                        EdgeKind::CoChange,
                        EdgeKind::Refs,
                    ],
                    hops: 1,
                    limit: 8,
                },
            );
            for r in related {
                let gain = seed_score * r.score * opts.graph_weight;
                *fused.entry(r.path.clone()).or_default() += gain;
                let sigs = signals.entry(r.path.clone()).or_default();
                if !sigs.contains(&Signal::Graph) {
                    sigs.push(Signal::Graph);
                }
                via.entry(r.path).or_insert((seed.clone(), r.via));
            }
        }
    }
    time("graph", t0);

    // 4. Priors.
    if opts.use_priors
        && let Some(graph) = ctx.graph
    {
        let n = graph.len().max(1) as f32;
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_nanos() as f64);
        for (path, score) in fused.iter_mut() {
            if let Some(rank) = graph.rank_of(path) {
                *score += PRIOR_RANK * (rank * n).min(PRIOR_RANK_CAP);
            }
            if let Some(meta) = ctx.text.file_meta(path) {
                let age = now_ns - meta.mtime_ns as f64;
                if (0.0..RECENT_WINDOW_NS).contains(&age) {
                    *score += PRIOR_RECENT * (1.0 - (age / RECENT_WINDOW_NS) as f32);
                }
            }
        }
    }

    // 5. Rank and pack.
    let t0 = Instant::now();
    let mut order: Vec<(String, f32)> = fused.into_iter().collect();
    order.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let total = order.len();

    let mut hits: Vec<QueryHit> = Vec::new();
    let mut used = 0usize;
    let header_cost = ctx.counter.count(&format!("query: {query}\n"));
    used += header_cost;
    for (path, score) in order.into_iter().take(opts.max_files) {
        let mut sigs = signals.remove(&path).unwrap_or_default();
        sigs.sort();
        sigs.dedup();
        let mut ev = evidence.remove(&path).unwrap_or_default();
        // Most informative first: definitions, then semantic chunks, then matching lines.
        ev.sort_by_key(|e| match e.signal {
            Signal::Symbol => 0,
            Signal::Semantic => 1,
            Signal::Text => 2,
            Signal::Graph => 3,
        });
        ev.dedup_by(|b, a| a.start_line == b.start_line && a.text == b.text);
        ev.truncate(3);
        let stale = ctx.text.is_stale(&path, ctx.overlay);
        let hit = QueryHit {
            via: via.remove(&path),
            stale,
            path,
            score,
            signals: sigs,
            evidence: ev,
        };
        let cost = ctx.counter.count(&render_hit(&hit));
        if !hits.is_empty() && used + cost > opts.budget_tokens {
            break;
        }
        used += cost;
        hits.push(hit);
    }
    let truncated = hits.len() < total.min(opts.max_files) || total > opts.max_files;
    // Staleness is judged per returned hit, whichever stage found it.
    let stale = hits.iter().any(|h| h.stale);
    time("pack", t0);

    Ok(QueryResult {
        snapshot_id: ctx.text.snapshot_id(),
        stale,
        semantic: sem_state,
        terms: terms.iter().map(|t| t.text.clone()).collect(),
        tokens: used,
        hits,
        truncated,
        timings_ms: timings,
    })
}

/// How one hit appears in the rendered answer.
pub fn render_hit(h: &QueryHit) -> String {
    let mut out = format!(
        "{}  [{}]",
        h.path,
        h.signals
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("+")
    );
    if let Some((seed, kinds)) = &h.via {
        let k: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
        out.push_str(&format!("  (via {seed}: {})", k.join(">")));
    }
    if h.stale {
        out.pop();
        out.push_str("  [stale]\n");
    } else {
        out.push('\n');
    }
    for e in &h.evidence {
        let loc = if e.start_line > 0 {
            format!("L{}", e.start_line)
        } else {
            String::new()
        };
        let first = e.text.lines().next().unwrap_or("");
        out.push_str(&format!("  {loc:<6} {}\n", first.trim()));
    }
    out
}

/// Render a whole result as text for an agent.
pub fn render(result: &QueryResult, query: &str) -> String {
    let mut out = format!("query: {query}\n");
    for h in &result.hits {
        out.push_str(&render_hit(h));
    }
    out.push_str(&format!(
        "[snapshot {:016x}{}{}{}; {} tokens]\n",
        result.snapshot_id,
        if result.stale { ", STALE hits" } else { "" },
        match result.semantic {
            SemanticState::Full => "",
            SemanticState::Partial => ", semantic partial",
            SemanticState::Off => ", semantic off",
        },
        if result.truncated { ", truncated" } else { "" },
        result.tokens
    ));
    out
}

/// Default counter, for callers that do not bring their own.
pub fn default_counter() -> ApproxTokenCounter {
    ApproxTokenCounter
}
