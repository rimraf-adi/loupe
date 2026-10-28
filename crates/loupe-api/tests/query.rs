//! The hybrid `query` over a real index.

use std::fs;

use loupe_api::{GraphOptions, Index, QueryOptions, Signal, render_query};
use loupe_core::Config;

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
        p.write("auth/token.rs", "/// Validate a signed session token.\npub fn validate_session_token(raw: &str) -> Claims {\n    verify_signature(raw)\n}\n\npub fn verify_signature(raw: &str) -> bool { true }\n");
        p.write("auth/login.rs", "use crate::auth::token::validate_session_token;\npub fn login(user: &str) { let _ = validate_session_token(user); }\n");
        p.write("auth/login_test.rs", "fn t() { login(\"a\"); }\n");
        p.write("billing/invoice.rs", "/// Compute the total of an invoice including tax.\npub fn invoice_total(items: &[Item]) -> u64 { 0 }\n");
        p.write(
            "render/mesh.rs",
            "pub fn render_triangle_mesh(v: &[Vertex]) {}\n",
        );
        p.write(
            "README.md",
            "# Shop\n\nAn online shop with sessions and invoices.\n",
        );
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

fn graph_opts() -> GraphOptions {
    GraphOptions {
        cochange: None,
        ..GraphOptions::default()
    }
}

#[test]
fn a_question_finds_the_right_file_with_evidence_from_several_signals() {
    let p = Project::new();
    let idx = p.index();
    let s = idx.session(&graph_opts()).unwrap();
    let r = s
        .query(
            "where do we validate the session token signature",
            &QueryOptions::default(),
            None,
        )
        .unwrap();
    assert_eq!(r.hits[0].path, "auth/token.rs", "{}", render_query(&r, "q"));
    let top = &r.hits[0];
    assert!(
        top.signals.contains(&Signal::Text) && top.signals.contains(&Signal::Semantic),
        "{:?}",
        top.signals
    );
    assert!(
        top.evidence
            .iter()
            .any(|e| e.text.contains("validate_session_token")),
        "{:?}",
        top.evidence
    );
    assert!(r.terms.contains(&"session".to_string()) && r.terms.contains(&"token".to_string()));
    assert!(r.snapshot_id > 0 && !r.stale);
    assert!(
        r.timings_ms.iter().any(|(n, _)| n == "retrieve")
            && r.timings_ms.iter().any(|(n, _)| n == "pack")
    );
    let text = render_query(&r, "q");
    assert!(
        text.contains("auth/token.rs") && text.contains("[snapshot "),
        "{text}"
    );
}

#[test]
fn identifiers_in_the_question_hit_symbol_definitions_directly() {
    let p = Project::new();
    let s = p.index().session(&graph_opts()).unwrap();
    let r = s
        .query("what does invoice_total do", &QueryOptions::default(), None)
        .unwrap();
    assert_eq!(r.hits[0].path, "billing/invoice.rs");
    assert!(
        r.hits[0].signals.contains(&Signal::Symbol),
        "{:?}",
        r.hits[0].signals
    );
    let ev = r.hits[0]
        .evidence
        .iter()
        .find(|e| e.signal == Signal::Symbol)
        .unwrap();
    assert_eq!(ev.text, "pub fn invoice_total(items: &[Item]) -> u64");
    assert_eq!(ev.start_line, 2);
}

#[test]
fn each_stage_can_be_switched_off() {
    let p = Project::new();
    let s = p.index().session(&graph_opts()).unwrap();
    let q = "validate session token";
    let only_text = s
        .query(
            q,
            &QueryOptions {
                use_semantic: false,
                use_symbols: false,
                ..QueryOptions::default()
            },
            None,
        )
        .unwrap();
    assert!(
        only_text
            .hits
            .iter()
            .all(|h| h.signals.iter().all(|s| *s == Signal::Text))
    );
    let only_sem = s
        .query(
            q,
            &QueryOptions {
                use_text: false,
                use_symbols: false,
                ..QueryOptions::default()
            },
            None,
        )
        .unwrap();
    assert!(
        only_sem
            .hits
            .iter()
            .all(|h| h.signals.iter().all(|s| *s == Signal::Semantic))
    );
    let nothing = s
        .query(
            q,
            &QueryOptions {
                use_text: false,
                use_symbols: false,
                use_semantic: false,
                ..QueryOptions::default()
            },
            None,
        )
        .unwrap();
    assert!(nothing.hits.is_empty());
}

#[test]
fn graph_expansion_is_opt_in_and_explains_itself() {
    let p = Project::new();
    let s = p.index().session(&graph_opts()).unwrap();
    let q = "verify_signature";
    let plain = s
        .query(
            q,
            &QueryOptions {
                use_semantic: false,
                ..QueryOptions::default()
            },
            None,
        )
        .unwrap();
    assert!(
        plain
            .hits
            .iter()
            .all(|h| !h.signals.contains(&Signal::Graph))
    );
    assert!(
        plain.hits.iter().all(|h| h.path != "auth/login.rs"),
        "login.rs never mentions the term"
    );

    let wide = s
        .query(
            q,
            &QueryOptions {
                use_semantic: false,
                use_graph: true,
                ..QueryOptions::default()
            },
            None,
        )
        .unwrap();
    let login = wide
        .hits
        .iter()
        .find(|h| h.path == "auth/login.rs")
        .expect("pulled in via the import graph");
    assert!(login.signals.contains(&Signal::Graph));
    let (seed, kinds) = login.via.as_ref().unwrap();
    assert_eq!(seed, "auth/token.rs");
    assert!(!kinds.is_empty());
    assert!(render_query(&wide, q).contains("(via auth/token.rs:"));
}

#[test]
fn the_budget_and_file_limits_are_respected() {
    let p = Project::new();
    let s = p.index().session(&graph_opts()).unwrap();
    let q = "session token invoice render mesh signature total tax";
    let big = s
        .query(
            q,
            &QueryOptions {
                budget_tokens: 100_000,
                ..QueryOptions::default()
            },
            None,
        )
        .unwrap();
    assert!(big.hits.len() >= 3);
    let small = s
        .query(
            q,
            &QueryOptions {
                budget_tokens: 60,
                ..QueryOptions::default()
            },
            None,
        )
        .unwrap();
    assert!(
        small.tokens <= 60 || small.hits.len() == 1,
        "{} tokens, {} hits",
        small.tokens,
        small.hits.len()
    );
    assert!(small.hits.len() < big.hits.len());
    assert!(small.truncated);
    assert!(
        !small.hits.is_empty(),
        "at least one result even under a tiny budget"
    );
    let capped = s
        .query(
            q,
            &QueryOptions {
                max_files: 2,
                budget_tokens: 100_000,
                ..QueryOptions::default()
            },
            None,
        )
        .unwrap();
    assert_eq!(capped.hits.len(), 2);
}

#[test]
fn overlay_edits_are_visible_to_every_stage() {
    let p = Project::new();
    let mut idx = p.index();
    idx.put(
        "scratch/new.rs",
        b"/// Compress a buffer using zstd.\npub fn compress_buffer(d: &[u8]) {}\n".to_vec(),
    )
    .unwrap();
    let s = idx.session(&graph_opts()).unwrap();
    let r = s
        .query(
            "compress buffer zstd",
            &QueryOptions::default(),
            Some(idx.overlay()),
        )
        .unwrap();
    assert_eq!(
        r.hits[0].path,
        "scratch/new.rs",
        "{}",
        render_query(&r, "q")
    );
    let gone = s
        .query("compress buffer zstd", &QueryOptions::default(), None)
        .unwrap();
    assert!(gone.hits.iter().all(|h| h.path != "scratch/new.rs"));
}

#[test]
fn stale_files_are_flagged() {
    let p = Project::new();
    let s = p.index().session(&graph_opts()).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    p.write("billing/invoice.rs", "/// Compute the total of an invoice including tax (edited).\npub fn invoice_total(items: &[Item]) -> u64 { 1 }\n");
    let r = s
        .query("invoice total tax", &QueryOptions::default(), None)
        .unwrap();
    assert!(
        r.stale,
        "the text stage reads the changed file and must say so"
    );
    assert!(render_query(&r, "q").contains("STALE"));
}

#[test]
fn empty_and_stopword_queries_return_nothing_or_semantic_noise_only() {
    let p = Project::new();
    let s = p.index().session(&graph_opts()).unwrap();
    let r = s.query("", &QueryOptions::default(), None).unwrap();
    assert!(r.hits.is_empty() || r.hits.iter().all(|h| h.signals == [Signal::Semantic]));
    let r = s
        .query("how do the of and", &QueryOptions::default(), None)
        .unwrap();
    assert!(r.terms.is_empty());
    assert!(
        r.hits
            .iter()
            .all(|h| !h.signals.contains(&Signal::Text) && !h.signals.contains(&Signal::Symbol))
    );
}
