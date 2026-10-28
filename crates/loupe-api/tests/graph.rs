//! Graph integration: a small multi-language project with git history, indexed for real.

use std::fs;
use std::path::Path;
use std::process::Command;

use loupe_api::{EdgeKind, Graph, GraphOptions, Index, RelatedOptions};
use loupe_core::Config;

fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

struct Project {
    repo: tempfile::TempDir,
    idx: tempfile::TempDir,
    git: bool,
}

impl Project {
    fn new() -> Project {
        let p = Project {
            repo: tempfile::tempdir().unwrap(),
            idx: tempfile::tempdir().unwrap(),
            git: false,
        };
        let git_ok = git(p.repo.path(), &["init", "-q"]);
        Project { git: git_ok, ..p }
    }
    fn write(&self, rel: &str, content: &str) {
        let p = self.repo.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }
    fn commit(&self, msg: &str) {
        if self.git {
            assert!(git(self.repo.path(), &["add", "-A"]));
            assert!(git(self.repo.path(), &["commit", "-q", "-m", msg]));
        }
    }
    fn index(&self) -> Index {
        let mut c = Config::new(self.repo.path().canonicalize().unwrap());
        c.index_dir = self.idx.path().to_path_buf();
        let mut idx = Index::open_writer(c).unwrap();
        idx.update(false).unwrap();
        idx
    }
}

fn has_edge(g: &Graph, from: &str, to: &str, kind: EdgeKind) -> bool {
    let f = g.node(from).unwrap_or_else(|| panic!("no node {from}"));
    g.out_edges(f)
        .iter()
        .any(|e| e.kind == kind && g.path(e.node) == to)
}

fn project() -> Project {
    let p = Project::new();
    // Rust workspace.
    p.write("crates/app/src/main.rs", "mod store;\nmod net;\nuse crate::store::Store;\nuse core::ids::Id;\nfn main() { let _ = Store; }\n");
    p.write(
        "crates/app/src/store.rs",
        "pub struct Store;\npub struct UniqueWidgetFactory;\n",
    );
    p.write("crates/app/src/net/mod.rs", "pub mod tcp;\n");
    p.write(
        "crates/app/src/net/tcp.rs",
        "use super::Thing;\nuse crate::store::Store;\n",
    );
    p.write(
        "crates/app/tests/store_test.rs",
        "// exercises storage\nfn t() {}\n",
    );
    p.write(
        "crates/app/src/consumer.rs",
        "// builds a UniqueWidgetFactory without importing it\nfn c() {}\n",
    );
    p.write("crates/core/src/lib.rs", "pub mod ids;\n");
    p.write("crates/core/src/ids.rs", "pub struct Id;\n");
    // TypeScript.
    p.write("web/src/app.ts", "import { helper } from './util';\nimport React from 'react';\nexport const a = helper();\n");
    p.write(
        "web/src/util.ts",
        "export function helper() { return 1; }\n",
    );
    p.write("web/src/util.test.ts", "import { helper } from './util';\n");
    // Python.
    p.write("py/app/main.py", "from app.util import work\nimport os\n");
    p.write("py/app/util.py", "def work(): pass\n");
    p.commit("initial");
    // Co-change history: store.rs and main.rs always change together, util.ts alone.
    for i in 0..4 {
        p.write(
            "crates/app/src/store.rs",
            &format!("pub struct Store;\npub struct UniqueWidgetFactory;\n// rev {i}\n"),
        );
        p.write("crates/app/src/main.rs", &format!("mod store;\nmod net;\nuse crate::store::Store;\nuse core::ids::Id;\nfn main() {{ let _ = Store; }} // rev {i}\n"));
        p.commit(&format!("change both {i}"));
    }
    p.write(
        "web/src/util.ts",
        "export function helper() { return 2; }\n",
    );
    p.commit("util alone");
    p
}

#[test]
fn every_edge_kind_is_built_from_a_real_index() {
    let p = project();
    let idx = p.index();
    let g = idx.graph(&GraphOptions::default()).unwrap();
    assert!(g.len() >= 13);

    // Imports (resolved to files): Rust mods/uses, workspace crate, TS relative, Python absolute.
    for (from, to) in [
        ("crates/app/src/main.rs", "crates/app/src/store.rs"),
        ("crates/app/src/main.rs", "crates/app/src/net/mod.rs"),
        ("crates/app/src/main.rs", "crates/core/src/ids.rs"),
        ("crates/app/src/net/mod.rs", "crates/app/src/net/tcp.rs"),
        ("crates/app/src/net/tcp.rs", "crates/app/src/store.rs"),
        ("crates/app/src/net/tcp.rs", "crates/app/src/net/mod.rs"),
        ("crates/core/src/lib.rs", "crates/core/src/ids.rs"),
        ("web/src/app.ts", "web/src/util.ts"),
        ("py/app/main.py", "py/app/util.py"),
    ] {
        assert!(
            has_edge(&g, from, to, EdgeKind::Imports),
            "missing import edge {from} -> {to}"
        );
    }
    assert!(
        !g.out_edges(g.node("web/src/app.ts").unwrap())
            .iter()
            .any(|e| g.path(e.node).contains("react"))
    );

    // Tests: by naming convention and by importing the code under test.
    assert!(has_edge(
        &g,
        "crates/app/tests/store_test.rs",
        "crates/app/src/store.rs",
        EdgeKind::Tests
    ));
    assert!(has_edge(
        &g,
        "web/src/util.test.ts",
        "web/src/util.ts",
        EdgeKind::Tests
    ));

    // Refs: consumer.rs mentions an identifier only store.rs defines, with no import.
    assert!(has_edge(
        &g,
        "crates/app/src/consumer.rs",
        "crates/app/src/store.rs",
        EdgeKind::Refs
    ));
    assert!(!has_edge(
        &g,
        "crates/app/src/consumer.rs",
        "crates/app/src/main.rs",
        EdgeKind::Refs
    ));

    // Co-change (needs git): store.rs <-> main.rs changed together every time.
    if p.git {
        assert!(has_edge(
            &g,
            "crates/app/src/store.rs",
            "crates/app/src/main.rs",
            EdgeKind::CoChange
        ));
        assert!(has_edge(
            &g,
            "crates/app/src/main.rs",
            "crates/app/src/store.rs",
            EdgeKind::CoChange
        ));
    }
}

#[test]
fn related_and_impact_answer_real_questions() {
    let p = project();
    let g = p.index().graph(&GraphOptions::default()).unwrap();

    let rel = g.related(
        "crates/app/src/store.rs",
        &RelatedOptions {
            limit: 10,
            ..RelatedOptions::default()
        },
    );
    let paths: Vec<&str> = rel.iter().map(|r| r.path.as_str()).collect();
    assert!(
        paths.contains(&"crates/app/src/main.rs"),
        "importer is related: {paths:?}"
    );
    assert!(
        paths.contains(&"crates/app/tests/store_test.rs"),
        "its test is related: {paths:?}"
    );
    assert!(
        paths.contains(&"crates/app/src/consumer.rs"),
        "a referencing file is related: {paths:?}"
    );
    assert!(
        rel.windows(2).all(|w| w[0].score >= w[1].score),
        "sorted best first"
    );

    let imp = g.impact("crates/core/src/ids.rs", 3, 20);
    let impacted: Vec<(&str, u8)> = imp.iter().map(|i| (i.path.as_str(), i.depth)).collect();
    assert!(
        impacted.contains(&("crates/core/src/lib.rs", 1)),
        "{impacted:?}"
    );
    assert!(
        impacted.contains(&("crates/app/src/main.rs", 1)),
        "{impacted:?}"
    );

    let only_tests = g.related(
        "web/src/util.ts",
        &RelatedOptions {
            kinds: vec![EdgeKind::Tests],
            hops: 1,
            limit: 10,
        },
    );
    assert_eq!(only_tests.len(), 1);
    assert_eq!(only_tests[0].path, "web/src/util.test.ts");
}

#[test]
fn pagerank_ranks_widely_imported_files_highest() {
    let p = project();
    let g = p
        .index()
        .graph(&GraphOptions {
            cochange: None,
            ..GraphOptions::default()
        })
        .unwrap();
    let top: Vec<&str> = g.top_ranked(4).into_iter().map(|(p, _)| p).collect();
    // store.rs is imported by main.rs and tcp.rs and referenced by consumer.rs.
    assert!(top.contains(&"crates/app/src/store.rs"), "{top:?}");
    let total: f32 = g.top_ranked(g.len()).iter().map(|(_, r)| r).sum();
    assert!((total - 1.0).abs() < 1e-3);
    // Deterministic across builds.
    let again = p
        .index()
        .graph(&GraphOptions {
            cochange: None,
            ..GraphOptions::default()
        })
        .unwrap();
    assert_eq!(g.top_ranked(g.len()), again.top_ranked(again.len()));
}

#[test]
fn graph_survives_edits_and_works_without_git_or_refs() {
    let p = Project::new();
    p.write("a.py", "import b\n");
    p.write("b.py", "x = 1\n");
    let mut idx = p.index();
    let opts = GraphOptions {
        refs: false,
        cochange: None,
        ..GraphOptions::default()
    };
    let g = idx.graph(&opts).unwrap();
    assert!(has_edge(&g, "a.py", "b.py", EdgeKind::Imports));
    assert_eq!(
        g.edge_count(EdgeKind::Refs) + g.edge_count(EdgeKind::CoChange),
        0
    );

    p.write("c.py", "import a\n");
    idx.update(false).unwrap();
    let g = idx.graph(&opts).unwrap();
    assert!(has_edge(&g, "c.py", "a.py", EdgeKind::Imports));
    assert!(
        has_edge(&g, "a.py", "b.py", EdgeKind::Imports),
        "older segment's imports still resolve"
    );
}
