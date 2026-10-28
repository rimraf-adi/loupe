//! The file graph: CSR adjacency, PageRank importance, `related` and `impact` queries.
//!
//! Nodes are snapshot files (sorted by path, so ids are deterministic). Each directed edge has a
//! kind and a *strength* in `(0, 1]`: 1.0 for a resolved import, a confidence for heuristic
//! kinds. Heuristic edges are never presented as certain; callers can filter by kind.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EdgeKind {
    /// `from` imports/includes/uses `to` (resolved from import statements).
    Imports,
    /// `from` mentions identifiers that `to` defines (heuristic name matching).
    Refs,
    /// `from` is a test of `to` (naming convention or imports).
    Tests,
    /// The two files changed together in git history.
    CoChange,
}

impl EdgeKind {
    pub const ALL: [EdgeKind; 4] = [
        EdgeKind::Imports,
        EdgeKind::Refs,
        EdgeKind::Tests,
        EdgeKind::CoChange,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Imports => "imports",
            EdgeKind::Refs => "refs",
            EdgeKind::Tests => "tests",
            EdgeKind::CoChange => "cochange",
        }
    }

    pub fn parse(s: &str) -> Option<EdgeKind> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// Contribution of this edge kind to PageRank importance.
    fn rank_factor(self) -> f32 {
        match self {
            EdgeKind::Imports => 1.0,
            EdgeKind::Refs => 0.3,
            EdgeKind::Tests => 0.1,
            EdgeKind::CoChange => 0.3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Edge {
    /// The other endpoint (target for out-edges, source for in-edges).
    pub node: u32,
    pub kind: EdgeKind,
    pub strength: f32,
}

#[derive(Clone, Debug, Default)]
pub struct GraphBuilder {
    n: usize,
    edges: Vec<(u32, u32, EdgeKind, f32)>,
}

impl GraphBuilder {
    pub fn new(n: usize) -> Self {
        GraphBuilder {
            n,
            edges: Vec::new(),
        }
    }

    /// Add a directed edge. Self-loops and out-of-range ids are ignored; strength is clamped to
    /// `(0, 1]`; duplicate `(from, to, kind)` edges keep the strongest.
    pub fn add(&mut self, from: u32, to: u32, kind: EdgeKind, strength: f32) {
        if from == to || from as usize >= self.n || to as usize >= self.n || strength <= 0.0 {
            return;
        }
        self.edges.push((from, to, kind, strength.min(1.0)));
    }

    pub fn finish(mut self, paths: Vec<String>) -> Graph {
        assert_eq!(paths.len(), self.n, "path count must match node count");
        // Sort, then collapse duplicates keeping the strongest.
        self.edges.sort_by(|a, b| {
            (a.0, a.1, a.2)
                .cmp(&(b.0, b.1, b.2))
                .then(b.3.total_cmp(&a.3))
        });
        self.edges
            .dedup_by(|b, a| (a.0, a.1, a.2) == (b.0, b.1, b.2));

        let (out_off, out) = csr(self.n, &self.edges, |&(f, t, k, s)| {
            (
                f,
                Edge {
                    node: t,
                    kind: k,
                    strength: s,
                },
            )
        });
        let (in_off, inn) = csr(self.n, &self.edges, |&(f, t, k, s)| {
            (
                t,
                Edge {
                    node: f,
                    kind: k,
                    strength: s,
                },
            )
        });
        let index = paths
            .iter()
            .enumerate()
            .map(|(i, p)| (p.clone(), i as u32))
            .collect();
        let mut g = Graph {
            paths,
            index,
            out_off,
            out,
            in_off,
            inn,
            rank: Vec::new(),
        };
        g.rank = g.pagerank(40, 0.85);
        g
    }
}

fn csr(
    n: usize,
    edges: &[(u32, u32, EdgeKind, f32)],
    key: impl Fn(&(u32, u32, EdgeKind, f32)) -> (u32, Edge),
) -> (Vec<u32>, Vec<Edge>) {
    let mut keyed: Vec<(u32, Edge)> = edges.iter().map(key).collect();
    keyed.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.node.cmp(&b.1.node))
            .then(a.1.kind.cmp(&b.1.kind))
    });
    let mut off = vec![0u32; n + 1];
    for (node, _) in &keyed {
        off[*node as usize + 1] += 1;
    }
    for i in 0..n {
        off[i + 1] += off[i];
    }
    (off, keyed.into_iter().map(|(_, e)| e).collect())
}

/// A file related to the query file, with a score in `(0, 1]` and the edge kinds that connect it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Related {
    pub path: String,
    pub score: f32,
    /// Hops from the query file along the best path found.
    pub hops: u8,
    /// Kinds of edges on that best path, in order from the query file.
    pub via: Vec<EdgeKind>,
}

/// A file that (transitively) depends on the query file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Impacted {
    pub path: String,
    /// 1 = directly imports/references/tests the query file.
    pub depth: u8,
    pub kind: EdgeKind,
}

#[derive(Clone, Debug)]
pub struct RelatedOptions {
    pub kinds: Vec<EdgeKind>,
    /// 1 or 2 are useful; deeper walks dilute quickly.
    pub hops: u8,
    pub limit: usize,
}

impl Default for RelatedOptions {
    fn default() -> Self {
        RelatedOptions {
            kinds: EdgeKind::ALL.to_vec(),
            hops: 2,
            limit: 20,
        }
    }
}

/// Score decay per extra hop.
const HOP_DECAY: f32 = 0.5;

#[derive(Clone, Debug)]
pub struct Graph {
    paths: Vec<String>,
    index: HashMap<String, u32>,
    out_off: Vec<u32>,
    out: Vec<Edge>,
    in_off: Vec<u32>,
    inn: Vec<Edge>,
    rank: Vec<f32>,
}

impl Graph {
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn node(&self, path: &str) -> Option<u32> {
        self.index.get(path).copied()
    }

    pub fn path(&self, node: u32) -> &str {
        &self.paths[node as usize]
    }

    pub fn out_edges(&self, node: u32) -> &[Edge] {
        &self.out[self.out_off[node as usize] as usize..self.out_off[node as usize + 1] as usize]
    }

    pub fn in_edges(&self, node: u32) -> &[Edge] {
        &self.inn[self.in_off[node as usize] as usize..self.in_off[node as usize + 1] as usize]
    }

    pub fn edge_count(&self, kind: EdgeKind) -> usize {
        self.out.iter().filter(|e| e.kind == kind).count()
    }

    /// PageRank importance (sums to 1). `None` for unknown paths.
    pub fn rank_of(&self, path: &str) -> Option<f32> {
        self.node(path).map(|n| self.rank[n as usize])
    }

    /// The `n` most important files, best first (ties by path).
    pub fn top_ranked(&self, n: usize) -> Vec<(&str, f32)> {
        let mut ids: Vec<u32> = (0..self.paths.len() as u32).collect();
        ids.sort_by(|&a, &b| {
            self.rank[b as usize]
                .total_cmp(&self.rank[a as usize])
                .then_with(|| self.paths[a as usize].cmp(&self.paths[b as usize]))
        });
        ids.into_iter()
            .take(n)
            .map(|i| (self.paths[i as usize].as_str(), self.rank[i as usize]))
            .collect()
    }

    /// Files most closely connected to `path`, best first.
    ///
    /// Scores propagate along edges: a neighbor scores `edge strength`, a neighbor-of-neighbor
    /// `strength * strength * 0.5`, and each file keeps the best score over all paths to it.
    /// Edges are followed in both directions (an importer is as related as an import).
    pub fn related(&self, path: &str, opts: &RelatedOptions) -> Vec<Related> {
        let Some(start) = self.node(path) else {
            return Vec::new();
        };
        struct Best {
            score: f32,
            hops: u8,
            via: Vec<EdgeKind>,
        }
        let mut best: HashMap<u32, Best> = HashMap::new();
        let mut frontier: Vec<(u32, f32, Vec<EdgeKind>)> = vec![(start, 1.0, Vec::new())];
        for hop in 1..=opts.hops.max(1) {
            let decay = HOP_DECAY.powi(i32::from(hop) - 1);
            let mut next: Vec<(u32, f32, Vec<EdgeKind>)> = Vec::new();
            for (node, score, via) in &frontier {
                let edges = self.out_edges(*node).iter().chain(self.in_edges(*node));
                for e in edges.filter(|e| opts.kinds.contains(&e.kind) && e.node != start) {
                    let s = score * e.strength * if hop == 1 { 1.0 } else { decay };
                    let mut v = via.clone();
                    v.push(e.kind);
                    if best.get(&e.node).is_none_or(|b| s > b.score) {
                        best.insert(
                            e.node,
                            Best {
                                score: s,
                                hops: hop,
                                via: v.clone(),
                            },
                        );
                        next.push((e.node, s, v));
                    }
                }
            }
            frontier = next;
        }
        let mut out: Vec<Related> = best
            .into_iter()
            .map(|(n, b)| Related {
                path: self.paths[n as usize].clone(),
                score: b.score,
                hops: b.hops,
                via: b.via,
            })
            .collect();
        out.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.path.cmp(&b.path))
        });
        out.truncate(opts.limit);
        out
    }

    /// Files that depend on `path`: reverse reachability over imports, references, and tests.
    /// Breadth-first, so `depth` is the shortest dependency distance. Sorted by depth, then
    /// importance, then path.
    pub fn impact(&self, path: &str, max_depth: u8, limit: usize) -> Vec<Impacted> {
        let Some(start) = self.node(path) else {
            return Vec::new();
        };
        let mut seen: HashMap<u32, (u8, EdgeKind)> = HashMap::new();
        let mut frontier = vec![start];
        for depth in 1..=max_depth.max(1) {
            let mut next = Vec::new();
            for node in frontier {
                for e in self.in_edges(node) {
                    let dependency =
                        matches!(e.kind, EdgeKind::Imports | EdgeKind::Refs | EdgeKind::Tests);
                    if dependency && e.node != start && !seen.contains_key(&e.node) {
                        seen.insert(e.node, (depth, e.kind));
                        next.push(e.node);
                    }
                }
            }
            frontier = next;
        }
        let mut out: Vec<(u32, u8, EdgeKind)> =
            seen.into_iter().map(|(n, (d, k))| (n, d, k)).collect();
        out.sort_by(|a, b| {
            a.1.cmp(&b.1)
                .then(self.rank[b.0 as usize].total_cmp(&self.rank[a.0 as usize]))
                .then_with(|| self.paths[a.0 as usize].cmp(&self.paths[b.0 as usize]))
        });
        out.into_iter()
            .take(limit)
            .map(|(n, depth, kind)| Impacted {
                path: self.paths[n as usize].clone(),
                depth,
                kind,
            })
            .collect()
    }

    /// Weighted PageRank by power iteration. Deterministic: fixed iteration count, fixed order.
    fn pagerank(&self, iterations: usize, damping: f32) -> Vec<f32> {
        let n = self.paths.len();
        if n == 0 {
            return Vec::new();
        }
        let weight = |e: &Edge| e.strength * e.kind.rank_factor();
        let out_weight: Vec<f32> = (0..n as u32)
            .map(|v| self.out_edges(v).iter().map(weight).sum())
            .collect();
        let uniform = 1.0 / n as f32;
        let mut rank = vec![uniform; n];
        for _ in 0..iterations {
            let dangling: f32 = (0..n)
                .filter(|&v| out_weight[v] == 0.0)
                .map(|v| rank[v])
                .sum();
            let base = (1.0 - damping) * uniform + damping * dangling * uniform;
            let mut next = vec![base; n];
            for v in 0..n as u32 {
                let ow = out_weight[v as usize];
                if ow == 0.0 {
                    continue;
                }
                let share = damping * rank[v as usize] / ow;
                for e in self.out_edges(v) {
                    next[e.node as usize] += share * weight(e);
                }
            }
            rank = next;
        }
        rank
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("f{i}.rs")).collect()
    }

    fn build(n: usize, edges: &[(u32, u32, EdgeKind, f32)]) -> Graph {
        let mut b = GraphBuilder::new(n);
        for &(f, t, k, s) in edges {
            b.add(f, t, k, s);
        }
        b.finish(names(n))
    }

    #[test]
    fn csr_adjacency_both_directions_and_dedup() {
        let g = build(
            4,
            &[
                (0, 1, EdgeKind::Imports, 1.0),
                (0, 1, EdgeKind::Imports, 0.4), // duplicate: strongest kept
                (0, 2, EdgeKind::Refs, 0.5),
                (2, 1, EdgeKind::Tests, 0.9),
                (1, 1, EdgeKind::Imports, 1.0), // self loop dropped
                (0, 9, EdgeKind::Imports, 1.0), // out of range dropped
                (3, 0, EdgeKind::Imports, 0.0), // zero strength dropped
            ],
        );
        assert_eq!(g.out_edges(0).len(), 2);
        assert_eq!(
            g.out_edges(0)[0],
            Edge {
                node: 1,
                kind: EdgeKind::Imports,
                strength: 1.0
            }
        );
        assert_eq!(g.in_edges(1).len(), 2);
        assert_eq!(g.out_edges(3).len(), 0);
        assert_eq!(g.edge_count(EdgeKind::Imports), 1);
        assert_eq!(g.node("f2.rs"), Some(2));
        assert_eq!(g.node("nope"), None);
    }

    #[test]
    fn pagerank_sums_to_one_and_favours_imported_files() {
        // Everyone imports f0; f0 imports nothing.
        let g = build(
            5,
            &[
                (1, 0, EdgeKind::Imports, 1.0),
                (2, 0, EdgeKind::Imports, 1.0),
                (3, 0, EdgeKind::Imports, 1.0),
                (4, 3, EdgeKind::Imports, 1.0),
            ],
        );
        let total: f32 = (0..5)
            .map(|i| g.rank_of(&format!("f{i}.rs")).unwrap())
            .sum();
        assert!((total - 1.0).abs() < 1e-4, "total {total}");
        let top = g.top_ranked(2);
        assert_eq!(top[0].0, "f0.rs");
        assert_eq!(top[1].0, "f3.rs");
        // Deterministic.
        let again = build(
            5,
            &[
                (1, 0, EdgeKind::Imports, 1.0),
                (2, 0, EdgeKind::Imports, 1.0),
                (3, 0, EdgeKind::Imports, 1.0),
                (4, 3, EdgeKind::Imports, 1.0),
            ],
        );
        assert_eq!(g.top_ranked(5), again.top_ranked(5));
    }

    #[test]
    fn empty_and_edgeless_graphs_work() {
        let g = build(0, &[]);
        assert!(g.is_empty() && g.top_ranked(3).is_empty());
        let g = build(3, &[]);
        let r = g.rank_of("f0.rs").unwrap();
        assert!((r - 1.0 / 3.0).abs() < 1e-5);
        assert!(g.related("f0.rs", &RelatedOptions::default()).is_empty());
        assert!(g.impact("f0.rs", 3, 10).is_empty());
    }

    #[test]
    fn related_scores_decay_and_follow_both_directions() {
        // 0 -> 1 -> 2 (imports), 3 co-changes with 0.
        let g = build(
            5,
            &[
                (0, 1, EdgeKind::Imports, 1.0),
                (1, 2, EdgeKind::Imports, 1.0),
                (3, 0, EdgeKind::CoChange, 0.6),
            ],
        );
        let r = g.related("f0.rs", &RelatedOptions::default());
        let by: HashMap<&str, &Related> = r.iter().map(|x| (x.path.as_str(), x)).collect();
        assert_eq!(by["f1.rs"].hops, 1);
        assert_eq!(by["f1.rs"].via, [EdgeKind::Imports]);
        assert!((by["f1.rs"].score - 1.0).abs() < 1e-6);
        assert_eq!(by["f2.rs"].hops, 2);
        assert!((by["f2.rs"].score - 0.5).abs() < 1e-6, "second hop decays");
        assert_eq!(
            by["f3.rs"].via,
            [EdgeKind::CoChange],
            "reverse direction followed"
        );
        assert!(!by.contains_key("f4.rs") && !by.contains_key("f0.rs"));
        assert_eq!(r[0].path, "f1.rs", "best first");

        let only_cochange = g.related(
            "f0.rs",
            &RelatedOptions {
                kinds: vec![EdgeKind::CoChange],
                ..RelatedOptions::default()
            },
        );
        assert_eq!(only_cochange.len(), 1);
        let one_hop = g.related(
            "f0.rs",
            &RelatedOptions {
                hops: 1,
                ..RelatedOptions::default()
            },
        );
        assert!(one_hop.iter().all(|x| x.hops == 1));
        let limited = g.related(
            "f0.rs",
            &RelatedOptions {
                limit: 1,
                ..RelatedOptions::default()
            },
        );
        assert_eq!(limited.len(), 1);
        assert!(g.related("missing", &RelatedOptions::default()).is_empty());
    }

    #[test]
    fn related_keeps_the_best_path_when_several_exist() {
        // Direct weak edge vs a strong two-hop path: 0 -(0.2)-> 2 and 0 -(1)-> 1 -(1)-> 2.
        let g = build(
            3,
            &[
                (0, 2, EdgeKind::Refs, 0.2),
                (0, 1, EdgeKind::Imports, 1.0),
                (1, 2, EdgeKind::Imports, 1.0),
            ],
        );
        let r = g.related("f0.rs", &RelatedOptions::default());
        let f2 = r.iter().find(|x| x.path == "f2.rs").unwrap();
        assert!(
            (f2.score - 0.5).abs() < 1e-6,
            "two-hop 0.5 beats direct 0.2: {f2:?}"
        );
        assert_eq!(f2.hops, 2);
    }

    #[test]
    fn impact_is_reverse_reachability_over_dependency_edges_only() {
        // 1 imports 0; 2 imports 1; 3 tests 0; 4 co-changes with 0 (not a dependency).
        let g = build(
            6,
            &[
                (1, 0, EdgeKind::Imports, 1.0),
                (2, 1, EdgeKind::Imports, 1.0),
                (3, 0, EdgeKind::Tests, 0.9),
                (4, 0, EdgeKind::CoChange, 1.0),
                (0, 5, EdgeKind::Imports, 1.0), // forward dependency: not impacted
            ],
        );
        let imp = g.impact("f0.rs", 3, 10);
        let got: Vec<(&str, u8)> = imp.iter().map(|i| (i.path.as_str(), i.depth)).collect();
        assert_eq!(got, [("f1.rs", 1), ("f3.rs", 1), ("f2.rs", 2)]);
        assert_eq!(g.impact("f0.rs", 1, 10).len(), 2, "depth limit");
        assert_eq!(g.impact("f0.rs", 3, 1).len(), 1, "result limit");
    }

    #[test]
    fn impact_terminates_on_cycles() {
        let g = build(
            3,
            &[
                (0, 1, EdgeKind::Imports, 1.0),
                (1, 2, EdgeKind::Imports, 1.0),
                (2, 0, EdgeKind::Imports, 1.0),
            ],
        );
        let imp = g.impact("f0.rs", 10, 10);
        assert_eq!(imp.len(), 2);
        let _ = g.related(
            "f0.rs",
            &RelatedOptions {
                hops: 10,
                ..RelatedOptions::default()
            },
        );
    }

    #[test]
    fn edge_kind_names_roundtrip() {
        for k in EdgeKind::ALL {
            assert_eq!(EdgeKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(EdgeKind::parse("nope"), None);
    }
}
