//! Repo map and module summaries over a real index.

use std::fs;

use loupe_api::{CallbackSummarizer, GraphOptions, Index, MapOptions, SummaryCache};
use loupe_core::Config;
use loupe_map::HeuristicSummarizer;

struct Project {
    repo: tempfile::TempDir,
    idx: tempfile::TempDir,
}

impl Project {
    fn new() -> Project {
        let p = Project {
            repo: tempfile::tempdir().unwrap(),
            idx: tempfile::tempdir().unwrap(),
        };
        p.write(
            "README.md",
            "# Demo\n\nA small service for tracking widgets. Built for tests.\n",
        );
        p.write("src/main.rs", "mod store;\nfn main() { store::open(); }\n");
        p.write(
            "src/store.rs",
            "//! Persistent widget storage.\npub struct Store;\npub fn open() -> Store { Store }\n",
        );
        p.write("src/net/mod.rs", "//! Network transports.\npub mod tcp;\n");
        p.write(
            "src/net/tcp.rs",
            "use crate::store::Store;\npub struct TcpConn;\npub fn connect() {}\n",
        );
        p.write("docs/guide.md", "# Guide\n\nHow to use the thing.\n");
        p.write("web/app.ts", "export function render() {}\n");
        p
    }
    fn write(&self, rel: &str, content: &str) {
        let path = self.repo.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    fn index(&self) -> Index {
        let mut c = Config::new(self.repo.path().canonicalize().unwrap());
        c.index_dir = self.idx.path().to_path_buf();
        let mut idx = Index::open_writer(c).unwrap();
        idx.update(false).unwrap();
        idx
    }
}

fn opts(budget: usize) -> MapOptions {
    MapOptions {
        budget,
        name: "demo".into(),
        ..MapOptions::default()
    }
}

#[test]
fn the_map_describes_the_repo_from_its_own_docs_and_symbols() {
    let p = Project::new();
    let idx = p.index();
    let graph = idx
        .graph(&GraphOptions {
            cochange: None,
            ..GraphOptions::default()
        })
        .unwrap();
    let cache = SummaryCache::in_memory();
    let map = idx
        .repo_map_with(
            &idx.symbol_searcher().unwrap(),
            &graph,
            &cache,
            &HeuristicSummarizer,
            &opts(5000),
        )
        .unwrap();

    assert!(
        map.text.starts_with("# demo — 7 files, 4 dirs"),
        "{}",
        map.text
    );
    assert!(
        map.text.contains("Entry points: src/main.rs"),
        "{}",
        map.text
    );
    assert!(
        map.text
            .contains("Summary: A small service for tracking widgets."),
        "root purpose comes from the README: {}",
        map.text
    );
    assert!(
        map.text.contains("src/ — 2 files (rust 2); defines"),
        "src has no lib.rs/mod.rs, so its line is generated: {}",
        map.text
    );
    assert!(
        map.text.contains("docs/ — 1 file (markdown 1)  [1 file]"),
        "singular nouns: {}",
        map.text
    );
    assert!(
        map.text.contains("net/ — Network transports."),
        "{}",
        map.text
    );
    assert!(
        map.text.contains("main.rs — main (+1)"),
        "definitions listed; the `mod store;` declaration is only counted: {}",
        map.text
    );
    assert!(
        map.text.contains("store.rs — Store, open"),
        "top-level symbols listed: {}",
        map.text
    );
    assert!(
        map.text.contains("tcp.rs — TcpConn, connect"),
        "{}",
        map.text
    );
    assert_eq!((map.shown_files, map.total_files), (7, 7));
    assert!(map.tokens <= 5000);
}

#[test]
fn module_summaries_report_purpose_and_what_the_module_depends_on() {
    let p = Project::new();
    let idx = p.index();
    let graph = idx
        .graph(&GraphOptions {
            cochange: None,
            ..GraphOptions::default()
        })
        .unwrap();
    let cache = SummaryCache::in_memory();
    let m = idx
        .module_summary_with(
            &idx.symbol_searcher().unwrap(),
            &graph,
            &cache,
            &HeuristicSummarizer,
            "src/net",
        )
        .unwrap();
    assert_eq!(m.text.lines().next(), Some("Network transports."));
    assert_eq!(m.files, 2);
    assert_eq!(m.depends_on, ["src"], "tcp.rs imports src/store.rs");
    let root = idx
        .module_summary_with(
            &idx.symbol_searcher().unwrap(),
            &graph,
            &cache,
            &HeuristicSummarizer,
            "",
        )
        .unwrap();
    assert_eq!(
        root.text.lines().next(),
        Some("A small service for tracking widgets.")
    );
    assert!(
        idx.module_summary_with(
            &idx.symbol_searcher().unwrap(),
            &graph,
            &cache,
            &HeuristicSummarizer,
            "nope"
        )
        .is_err()
    );
}

#[test]
fn an_edit_recomputes_only_the_ancestor_directories() {
    let p = Project::new();
    let mut idx = p.index();
    let g = GraphOptions {
        cochange: None,
        ..GraphOptions::default()
    };
    let cache = SummaryCache::in_memory();
    let h = HeuristicSummarizer;

    let graph = idx.graph(&g).unwrap();
    idx.repo_map_with(
        &idx.symbol_searcher().unwrap(),
        &graph,
        &cache,
        &h,
        &opts(5000),
    )
    .unwrap();
    let cold = cache.misses();
    assert_eq!(cold, 5, "root, src, src/net, docs, web: one summary each");

    idx.repo_map_with(
        &idx.symbol_searcher().unwrap(),
        &graph,
        &cache,
        &h,
        &opts(5000),
    )
    .unwrap();
    assert_eq!(cache.misses(), cold, "a repeat is all cache hits");

    // Edit one deep file; unrelated directories must stay cached.
    p.write("src/net/tcp.rs", "use crate::store::Store;\npub struct TcpConn;\npub fn connect() {}\npub fn disconnect() {}\n");
    idx.update(false).unwrap();
    let graph = idx.graph(&g).unwrap();
    idx.repo_map_with(
        &idx.symbol_searcher().unwrap(),
        &graph,
        &cache,
        &h,
        &opts(5000),
    )
    .unwrap();
    assert_eq!(
        cache.misses() - cold,
        3,
        "only src/net, src, and the root were recomputed"
    );
}

#[test]
fn summaries_persist_across_processes_through_the_redb_cache() {
    let p = Project::new();
    let idx = p.index();
    let graph = idx
        .graph(&GraphOptions {
            cochange: None,
            ..GraphOptions::default()
        })
        .unwrap();
    {
        let cache = idx.open_summary_cache();
        assert!(cache.is_persistent());
        idx.repo_map_with(
            &idx.symbol_searcher().unwrap(),
            &graph,
            &cache,
            &HeuristicSummarizer,
            &opts(5000),
        )
        .unwrap();
        assert_eq!(cache.misses(), 5);
    }
    let cache = idx.open_summary_cache();
    idx.repo_map_with(
        &idx.symbol_searcher().unwrap(),
        &graph,
        &cache,
        &HeuristicSummarizer,
        &opts(5000),
    )
    .unwrap();
    assert_eq!(
        cache.misses(),
        0,
        "a new handle (a new process) finds everything cached"
    );
}

#[test]
fn llm_summaries_are_called_once_per_changed_directory_only() {
    let p = Project::new();
    let mut idx = p.index();
    let g = GraphOptions {
        cochange: None,
        ..GraphOptions::default()
    };
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = calls.clone();
    let llm = CallbackSummarizer::new("t1", move |i| {
        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Some(format!(
            "LLM: {}",
            if i.path.is_empty() { "root" } else { &i.path }
        ))
    });
    let cache = SummaryCache::in_memory();
    let graph = idx.graph(&g).unwrap();
    let map = idx
        .repo_map_with(
            &idx.symbol_searcher().unwrap(),
            &graph,
            &cache,
            &llm,
            &opts(5000),
        )
        .unwrap();
    assert!(map.text.contains("src/ — LLM: src"), "{}", map.text);
    let first = calls.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(first, 5);

    p.write(
        "web/app.ts",
        "export function render() {}\nexport function mount() {}\n",
    );
    idx.update(false).unwrap();
    let graph = idx.graph(&g).unwrap();
    idx.repo_map_with(
        &idx.symbol_searcher().unwrap(),
        &graph,
        &cache,
        &llm,
        &opts(5000),
    )
    .unwrap();
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst) - first,
        2,
        "web and the root"
    );
}

#[test]
fn focus_and_budget_shape_the_map() {
    let p = Project::new();
    let idx = p.index();
    let graph = idx
        .graph(&GraphOptions {
            cochange: None,
            ..GraphOptions::default()
        })
        .unwrap();
    let cache = SummaryCache::in_memory();
    let h = HeuristicSummarizer;
    let small = idx
        .repo_map_with(
            &idx.symbol_searcher().unwrap(),
            &graph,
            &cache,
            &h,
            &opts(70),
        )
        .unwrap();
    assert!(
        small.tokens <= 70,
        "{} tokens\n{}",
        small.tokens,
        small.text
    );
    assert!(small.shown_files < 7);
    let focused = idx
        .repo_map_with(
            &idx.symbol_searcher().unwrap(),
            &graph,
            &cache,
            &h,
            &MapOptions {
                focus: vec!["web".into()],
                ..opts(70)
            },
        )
        .unwrap();
    assert!(focused.text.contains("app.ts"), "{}", focused.text);
}
