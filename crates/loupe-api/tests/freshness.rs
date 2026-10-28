//! Freshness: no result about a file is ever returned from out-of-date index data without the
//! `stale` flag, and no fresh result is wrongly flagged.
//!
//! The model-based test applies random file operations (create, edit, delete, rename, and an
//! adversarial same-size/same-mtime edit) interleaved with *partial* re-indexing, so the index is
//! often behind the disk, then checks every flag of every result type against ground truth
//! computed independently from file contents.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use loupe_api::{GraphOptions, Index, QueryOptions, SearchOptions, SemanticOptions, Session};
use loupe_core::{BlobId, Config};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

const WORDS: [&str; 8] = [
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
];

fn content(rng: &mut Rng, id: usize) -> String {
    let w1 = WORDS[rng.below(WORDS.len())];
    let w2 = WORDS[rng.below(WORDS.len())];
    format!(
        "/// Handles {w1} and {w2} requests.\npub fn handler_{id}_{w1}() {{ let marker_{w2} = {id}; }}\n"
    )
}

struct Env {
    repo: tempfile::TempDir,
    idx: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        Env {
            repo: tempfile::tempdir().unwrap(),
            idx: tempfile::tempdir().unwrap(),
        }
    }
    fn root(&self) -> std::path::PathBuf {
        self.repo.path().canonicalize().unwrap()
    }
    fn config(&self) -> Config {
        let mut c = Config::new(self.root());
        c.index_dir = self.idx.path().to_path_buf();
        c
    }
    fn write(&self, rel: &str, content: &str) {
        let p = self.repo.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }
}

/// Ground truth, computed without any of the code under test: does the file on disk differ from
/// the blob the snapshot recorded?
fn truth_stale(session: &Session, root: &Path, path: &str) -> bool {
    match session.text.file_meta(path) {
        Some(meta) => match fs::read(root.join(path)) {
            Ok(bytes) => BlobId::of(&bytes) != meta.blob,
            Err(_) => true,
        },
        None => false,
    }
}

fn graph() -> GraphOptions {
    GraphOptions {
        cochange: None,
        ..GraphOptions::default()
    }
}

fn check_all_results(session: &Session, root: &Path, rng: &mut Rng, files: &[String]) {
    // Text search: flags match ground truth, and fresh hits quote lines that exist on disk now.
    let opts = SearchOptions {
        max_files: 100,
        timeout: None,
        context_lines: 0,
        ..SearchOptions::default()
    };
    for w in WORDS {
        let r = session
            .text
            .search(
                w,
                &SearchOptions {
                    literal: true,
                    case_insensitive: true,
                    ..opts.clone()
                },
            )
            .unwrap();
        for hit in &r.hits {
            assert_eq!(
                hit.stale,
                truth_stale(session, root, &hit.path),
                "text hit {} for {w:?}",
                hit.path
            );
            let disk = fs::read_to_string(root.join(&hit.path)).unwrap();
            for snip in &hit.snippets {
                for line in snip.text.lines() {
                    assert!(
                        disk.contains(line),
                        "text hit quotes a line not on disk: {line:?} in {}",
                        hit.path
                    );
                }
            }
        }
    }
    // Symbols: find by names that exist (or existed) plus an outline of a random file.
    for f in files {
        let id: String = f.chars().filter(char::is_ascii_digit).collect();
        for w in WORDS {
            let name = format!("handler_{id}_{w}");
            for h in session.symbols.find(&name, None, 10, None).unwrap() {
                assert_eq!(
                    h.stale,
                    truth_stale(session, root, &h.path),
                    "symbol hit {} {name}",
                    h.path
                );
            }
        }
    }
    if !files.is_empty() {
        let f = &files[rng.below(files.len())];
        if let Some(o) = session.symbols.outline(f, None).unwrap() {
            // Outlines served from the index carry the flag; ones parsed from disk are fresh by construction.
            if o.source == loupe_api::OutlineSource::Index {
                assert_eq!(o.stale, truth_stale(session, root, f), "outline of {f}");
            } else {
                assert!(!o.stale);
            }
        }
    }
    // Semantic and hybrid.
    let w = WORDS[rng.below(WORDS.len())];
    let sem = session
        .semantic
        .search(
            &format!("handles {w} requests"),
            &SemanticOptions {
                k: 20,
                per_file: 2,
                ..SemanticOptions::default()
            },
            None,
        )
        .unwrap();
    for h in &sem.hits {
        assert_eq!(
            h.stale,
            truth_stale(session, root, &h.path),
            "semantic hit {}",
            h.path
        );
    }
    let q = session
        .query(
            &format!("handler {w} requests"),
            &QueryOptions {
                max_files: 20,
                budget_tokens: 100_000,
                ..QueryOptions::default()
            },
            None,
        )
        .unwrap();
    for h in &q.hits {
        assert_eq!(
            h.stale,
            truth_stale(session, root, &h.path),
            "query hit {}",
            h.path
        );
    }
    assert_eq!(q.stale, q.hits.iter().any(|h| h.stale));
}

#[test]
fn no_result_is_ever_stale_without_the_flag_under_random_edits_and_partial_updates() {
    for seed in 1..=14u64 {
        let env = Env::new();
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut files: Vec<String> = Vec::new();
        let mut next_id = 0usize;
        for _ in 0..4 {
            let name = format!("src/f{next_id}.rs");
            env.write(&name, &content(&mut rng, next_id));
            files.push(name);
            next_id += 1;
        }
        let mut idx = Index::open_writer(env.config()).unwrap();
        idx.update(false).unwrap();

        for step in 0..10 {
            match rng.below(6) {
                0 => {
                    let name = format!("src/f{next_id}.rs");
                    env.write(&name, &content(&mut rng, next_id));
                    files.push(name);
                    next_id += 1;
                }
                1 if !files.is_empty() => {
                    let f = files[rng.below(files.len())].clone();
                    env.write(&f, &content(&mut rng, next_id));
                    next_id += 1;
                }
                2 if files.len() > 1 => {
                    let f = files.remove(rng.below(files.len()));
                    fs::remove_file(env.repo.path().join(&f)).unwrap();
                }
                3 if !files.is_empty() => {
                    let i = rng.below(files.len());
                    let to = format!("src/moved{next_id}.rs");
                    next_id += 1;
                    fs::rename(env.repo.path().join(&files[i]), env.repo.path().join(&to)).unwrap();
                    files[i] = to;
                }
                4 if !files.is_empty() => {
                    // Adversarial: different content, identical size AND identical mtime. A
                    // stat-based check cannot see this; only a content hash can.
                    let f = files[rng.below(files.len())].clone();
                    let path = env.repo.path().join(&f);
                    let old = fs::read_to_string(&path).unwrap();
                    let mtime = fs::metadata(&path).unwrap().modified().unwrap();
                    let swapped = old.replacen("Handles", "Hunted ", 1);
                    assert_eq!(swapped.len(), old.len());
                    fs::write(&path, swapped).unwrap();
                    fs::File::options()
                        .write(true)
                        .open(&path)
                        .unwrap()
                        .set_modified(mtime)
                        .unwrap();
                }
                _ => {}
            }
            // Often skip the update, so the index lags the disk.
            if rng.chance(40) {
                idx.update(false).unwrap();
            }
            let session = idx.session(&graph()).unwrap();
            check_all_results(&session, &env.root(), &mut rng, &files);
            let _ = step;
        }
    }
}

#[test]
fn a_same_size_same_mtime_edit_is_caught_by_every_result_type() {
    let env = Env::new();
    let original = "/// Handles alpha requests.\npub fn handler_0_alpha() { let x = 1; }\n";
    env.write("src/a.rs", original);
    let mut idx = Index::open_writer(env.config()).unwrap();
    idx.update(false).unwrap();
    let path = env.repo.path().join("src/a.rs");
    let mtime = fs::metadata(&path).unwrap().modified().unwrap();

    // Same length, different bytes, original mtime restored.
    let edited = original.replace("let x = 1;", "let x = 2;");
    assert_eq!(edited.len(), original.len());
    fs::write(&path, &edited).unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    let md = fs::metadata(&path).unwrap();
    assert_eq!(
        (md.len(), md.modified().unwrap()),
        (original.len() as u64, mtime),
        "stat is identical"
    );

    let s = idx.session(&graph()).unwrap();
    let opts = SearchOptions {
        literal: true,
        max_files: 5,
        timeout: None,
        ..SearchOptions::default()
    };
    assert!(s.text.search("handler_0_alpha", &opts).unwrap().hits[0].stale);
    assert!(s.symbols.find("handler_0_alpha", None, 5, None).unwrap()[0].stale);
    assert!(s.symbols.outline("src/a.rs", None).unwrap().unwrap().stale);
    assert!(
        s.semantic
            .search("handles alpha requests", &SemanticOptions::default(), None)
            .unwrap()
            .hits[0]
            .stale
    );
    let q = s
        .query("handler alpha requests", &QueryOptions::default(), None)
        .unwrap();
    assert!(q.hits[0].stale && q.stale);

    // An update absorbs it; nothing is flagged afterwards.
    idx.update(true).unwrap();
    let s = idx.session(&graph()).unwrap();
    assert!(!s.symbols.find("handler_0_alpha", None, 5, None).unwrap()[0].stale);
    assert!(!s.text.search("handler_0_alpha", &opts).unwrap().hits[0].stale);
    assert!(
        !s.query("handler alpha requests", &QueryOptions::default(), None)
            .unwrap()
            .stale
    );
}

#[test]
fn deleted_files_are_stale_and_the_overlay_is_never_stale() {
    let env = Env::new();
    env.write("src/a.rs", "pub fn gone_soon() {}\n");
    env.write("src/b.rs", "pub fn stays_put() {}\n");
    let mut idx = Index::open_writer(env.config()).unwrap();
    idx.update(false).unwrap();
    fs::remove_file(env.repo.path().join("src/a.rs")).unwrap();

    let s = idx.session(&graph()).unwrap();
    assert!(
        s.symbols.find("gone_soon", None, 5, None).unwrap()[0].stale,
        "the file is gone"
    );
    assert!(!s.symbols.find("stays_put", None, 5, None).unwrap()[0].stale);
    assert!(s.text.is_stale("src/a.rs", None));
    assert!(!s.text.is_stale("src/b.rs", None));
    assert!(
        !s.text.is_stale("never/existed.rs", None),
        "unknown paths have nothing to be stale against"
    );

    // An unsaved buffer is the freshest truth, whatever the snapshot or the disk says.
    idx.put(
        "src/b.rs",
        b"pub fn stays_put() { /* edited in memory */ }\n".to_vec(),
    )
    .unwrap();
    let s = idx.session(&graph()).unwrap();
    let overlay = Some(idx.overlay());
    assert!(!s.text.is_stale("src/b.rs", overlay));
    let hits = s.symbols.find("stays_put", None, 5, overlay).unwrap();
    assert!(!hits[0].stale);
    let q = s
        .query("stays put", &QueryOptions::default(), overlay)
        .unwrap();
    let b = q
        .hits
        .iter()
        .find(|h| h.path == "src/b.rs")
        .expect("src/b.rs is found");
    assert!(!b.stale, "the overlay owns src/b.rs");
    // The deleted file may also come back from the index; it must be flagged, not hidden.
    assert!(
        q.hits
            .iter()
            .filter(|h| h.path == "src/a.rs")
            .all(|h| h.stale)
    );
    // ...even though, with no overlay, the same file would be fine too (disk == snapshot).
    assert!(!s.text.is_stale("src/b.rs", None));
}

#[test]
fn graph_tool_results_can_be_checked_for_staleness_by_path() {
    let env = Env::new();
    env.write("a.py", "import b\n");
    env.write("b.py", "x = 1\n");
    let mut idx = Index::open_writer(env.config()).unwrap();
    idx.update(false).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    env.write("a.py", "import b\ny = 2\n");
    let s = idx.session(&graph()).unwrap();
    let imp = s.graph.impact("b.py", 2, 10);
    assert_eq!(imp.len(), 1);
    assert!(
        s.text.is_stale(&imp[0].path, None),
        "a.py changed after indexing"
    );
    assert!(!s.text.is_stale("b.py", None));
    let _ = SystemTime::now();
}
