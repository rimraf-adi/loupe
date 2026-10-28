//! End-to-end: files on disk -> walker -> store segments -> `TextSearcher`.

use std::collections::BTreeSet;
use std::fs;
use std::time::Duration;

use loupe_core::{BlobId, Config, Snapshot};
use loupe_store::Store;
use loupe_text::{SearchOptions, TextSearcher, write_text_index};
use loupe_walk::{WalkOptions, scan};
use regex::bytes::RegexBuilder;

struct Fixture {
    repo: tempfile::TempDir,
    _index: tempfile::TempDir,
    store: Store,
    config: Config,
}

impl Fixture {
    fn new() -> Fixture {
        let repo = tempfile::tempdir().unwrap();
        let index = tempfile::tempdir().unwrap();
        let config = Config::new(repo.path().canonicalize().unwrap());
        let store = Store::open_writer(index.path()).unwrap();
        Fixture {
            repo,
            _index: index,
            store,
            config,
        }
    }

    fn write(&self, rel: &str, content: &str) {
        let p = self.repo.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    /// Scan the repo and index every blob that is not indexed yet into a new segment.
    fn index(&mut self, write_segment: bool) -> Snapshot {
        let result = scan(&self.config, None, &WalkOptions::default()).unwrap();
        let mut indexed = std::collections::HashSet::new();
        for id in self.store.segment_ids() {
            indexed.extend(self.store.open_segment(id).unwrap().blobs().iter().copied());
        }
        let mut seen = std::collections::HashSet::new();
        let mut items: Vec<(BlobId, String)> = Vec::new();
        for (path, meta) in &result.snapshot.files {
            if !indexed.contains(&meta.blob) && seen.insert(meta.blob) {
                items.push((meta.blob, path.clone()));
            }
        }
        let mut segments = self.store.segment_ids();
        if write_segment && !items.is_empty() {
            let mut w = self.store.new_segment().unwrap();
            write_text_index(&mut w, &self.config.root, &items).unwrap();
            segments.push(w.finish().unwrap());
        }
        self.store.commit(&result.snapshot, &segments).unwrap();
        result.snapshot
    }

    fn searcher(&self) -> TextSearcher {
        TextSearcher::open(&self.config.root, &self.store).unwrap()
    }

    fn brute(&self, snap: &Snapshot, pattern: &str, ci: bool) -> BTreeSet<String> {
        let re = RegexBuilder::new(pattern)
            .case_insensitive(ci)
            .multi_line(true)
            .build()
            .unwrap();
        snap.files
            .keys()
            .filter(|p| {
                let bytes = fs::read(self.config.root.join(p)).unwrap();
                re.find_iter(&bytes).any(|m| !m.is_empty())
            })
            .cloned()
            .collect()
    }
}

fn all(max: usize) -> SearchOptions {
    SearchOptions {
        max_files: max,
        timeout: None,
        ..SearchOptions::default()
    }
}

fn paths(r: &loupe_text::SearchResult) -> BTreeSet<String> {
    r.hits.iter().map(|h| h.path.clone()).collect()
}

const FILES: &[(&str, &str)] = &[
    (
        "src/main.rs",
        "fn main() {\n    let server = Server::new();\n    server.run();\n}\n",
    ),
    (
        "src/server.rs",
        "pub struct Server;\nimpl Server {\n    pub fn new() -> Self { Server }\n    pub fn run(&self) {}\n}\n",
    ),
    (
        "src/util.rs",
        "pub fn helper() -> u32 { 42 }\n// TODO: remove helper\n",
    ),
    (
        "README.md",
        "# Demo\nRun the Server with `cargo run`.\nTODO list below.\n",
    ),
    ("docs/notes.txt", "nothing relevant here\n"),
    (
        "copy/util_copy.rs",
        "pub fn helper() -> u32 { 42 }\n// TODO: remove helper\n",
    ),
];

fn fixture_with_files() -> (Fixture, Snapshot) {
    let mut fx = Fixture::new();
    for (p, c) in FILES {
        fx.write(p, c);
    }
    let snap = fx.index(true);
    (fx, snap)
}

#[test]
fn matches_brute_force_on_a_variety_of_patterns() {
    let (fx, snap) = fixture_with_files();
    let s = fx.searcher();
    let cases: &[(&str, bool)] = &[
        ("Server", false),
        ("server", true),
        ("server", false),
        (r"fn \w+\(", false),
        (r"Server::new|Server;", false),
        ("TODO", false),
        ("todo", true),
        (r"pub fn (new|run|helper)", false),
        (r"helper.*42", false),
        (r"^// TODO", false),
        ("absent_string_xyz", false),
        (r"\w+", false), // no literals: full scan
    ];
    for &(pat, ci) in cases {
        let opts = SearchOptions {
            case_insensitive: ci,
            ..all(usize::MAX)
        };
        let got = paths(&s.search(pat, &opts).unwrap());
        assert_eq!(got, fx.brute(&snap, pat, ci), "pattern {pat:?} ci={ci}");
    }
}

#[test]
fn reports_lines_context_and_counts() {
    let (fx, _) = fixture_with_files();
    let r = fx
        .searcher()
        .search(
            "server",
            &SearchOptions {
                literal: true,
                context_lines: 0,
                ..all(10)
            },
        )
        .unwrap();
    // Case-sensitive "server": main.rs has `let server =` and `server.run()`.
    let hit = r.hits.iter().find(|h| h.path == "src/main.rs").unwrap();
    assert_eq!(hit.match_count, 2);
    assert_eq!(hit.snippets.len(), 1, "adjacent lines merge");
    assert_eq!(
        (hit.snippets[0].start_line, hit.snippets[0].end_line),
        (2, 3)
    );
    assert!(hit.snippets[0].text.contains("server.run();"));
    assert!(!hit.stale);
}

#[test]
fn identical_files_at_two_paths_are_both_returned() {
    let (fx, _) = fixture_with_files();
    let r = fx.searcher().search("remove helper", &all(10)).unwrap();
    assert_eq!(
        paths(&r),
        BTreeSet::from(["src/util.rs".to_string(), "copy/util_copy.rs".to_string()])
    );
}

#[test]
fn ranks_by_match_count_then_path_and_truncates() {
    let mut fx = Fixture::new();
    fx.write("a.txt", "needle\n");
    fx.write("b.txt", "needle needle needle\n");
    fx.write("c.txt", "needle\nneedle\n");
    fx.write("d.txt", "needle\n");
    fx.index(true);
    let r = fx.searcher().search("needle", &all(3)).unwrap();
    let order: Vec<&str> = r.hits.iter().map(|h| h.path.as_str()).collect();
    assert_eq!(order, ["b.txt", "c.txt", "a.txt"]);
    assert_eq!((r.total_files, r.total_matches), (4, 7));
    assert!(r.stats.truncated);
    assert_eq!(r.hits[0].match_count, 3);
}

#[test]
fn index_prunes_candidates() {
    let (fx, _) = fixture_with_files();
    let r = fx.searcher().search("Server::new", &all(10)).unwrap();
    assert!(!r.stats.full_scan);
    assert!(
        r.stats.candidates < FILES.len(),
        "candidates: {}",
        r.stats.candidates
    );
    assert_eq!(paths(&r), BTreeSet::from(["src/main.rs".to_string()]));

    let r = fx.searcher().search(r"\w+", &all(10)).unwrap();
    assert!(r.stats.full_scan);
    assert_eq!(r.stats.candidates, FILES.len());
}

#[test]
fn edited_file_is_flagged_stale() {
    let (fx, _) = fixture_with_files();
    // Same-size edit would be invisible to size checks; mtime catches it.
    std::thread::sleep(Duration::from_millis(20));
    fx.write(
        "src/util.rs",
        "pub fn helper() -> u32 { 43 }\n// TODO: remove helper\n",
    );
    let r = fx.searcher().search("remove helper", &all(10)).unwrap();
    let util = r.hits.iter().find(|h| h.path == "src/util.rs").unwrap();
    assert!(util.stale);
    assert!(
        !r.hits
            .iter()
            .find(|h| h.path == "copy/util_copy.rs")
            .unwrap()
            .stale
    );
    assert_eq!(r.stats.stale_files, 1);
}

#[test]
fn deleted_file_is_counted_not_fatal() {
    let (fx, _) = fixture_with_files();
    fs::remove_file(fx.config.root.join("src/util.rs")).unwrap();
    let r = fx.searcher().search("remove helper", &all(10)).unwrap();
    assert_eq!(r.stats.unreadable, 1);
    assert_eq!(paths(&r), BTreeSet::from(["copy/util_copy.rs".to_string()]));
}

#[test]
fn unindexed_files_are_still_searched() {
    let mut fx = Fixture::new();
    for (p, c) in FILES {
        fx.write(p, c);
    }
    let snap = fx.index(false); // snapshot committed, but no segment holds any blob
    let s = fx.searcher();
    let r = s.search("Server::new", &all(10)).unwrap();
    assert_eq!(r.stats.unindexed_files, FILES.len());
    assert_eq!(paths(&r), fx.brute(&snap, "Server::new", false));
    assert!(!paths(&r).is_empty());
}

#[test]
fn searches_across_multiple_segments() {
    let mut fx = Fixture::new();
    fx.write("one.txt", "alpha marker\n");
    fx.index(true);
    fx.write("two.txt", "beta marker\n");
    let snap = fx.index(true);
    assert_eq!(fx.store.segment_ids().len(), 2);
    let r = fx.searcher().search("marker", &all(10)).unwrap();
    assert_eq!(paths(&r), fx.brute(&snap, "marker", false));
    assert_eq!(r.total_files, 2);
    assert_eq!(
        fx.searcher().search("beta", &all(10)).unwrap().total_files,
        1
    );
}

#[test]
fn files_removed_from_the_snapshot_disappear_from_results() {
    let mut fx = Fixture::new();
    fx.write("keep.txt", "shared phrase here\n");
    fx.write("gone.txt", "unique_to_gone phrase\n");
    fx.index(true);
    fs::remove_file(fx.config.root.join("gone.txt")).unwrap();
    fx.index(true); // new snapshot without gone.txt; its blob stays in the old segment
    let r = fx.searcher().search("unique_to_gone", &all(10)).unwrap();
    assert_eq!(r.total_files, 0, "dead blobs must not surface");
    assert_eq!(r.stats.candidates, 0);
    assert_eq!(
        fx.searcher()
            .search("phrase", &all(10))
            .unwrap()
            .total_files,
        1
    );
}

#[test]
fn word_search_uses_the_token_index() {
    let (fx, _) = fixture_with_files();
    let s = fx.searcher();
    let r = s.search_word("helper", &all(10)).unwrap();
    assert_eq!(
        paths(&r),
        BTreeSet::from(["src/util.rs".to_string(), "copy/util_copy.rs".to_string()])
    );
    assert!(r.stats.plan.starts_with("token"));
    assert!(r.stats.candidates <= 2);

    // Whole-word: "Server" must not match "Servers", and is case-sensitive.
    assert_eq!(s.search_word("Serve", &all(10)).unwrap().total_files, 0);
    assert_eq!(s.search_word("server", &all(10)).unwrap().total_files, 1); // main.rs variable only

    // Words outside the token index range fall back to a regex search and still work.
    assert!(s.search_word("x", &all(10)).is_ok());

    assert!(s.search_word("not valid", &all(10)).is_err());
    assert!(s.search_word("9lives", &all(10)).is_err());
    assert!(s.search_word("", &all(10)).is_err());
}

#[test]
fn token_prefix_completion() {
    let (fx, _) = fixture_with_files();
    let s = fx.searcher();
    assert_eq!(s.tokens_with_prefix("Serv", 10).unwrap(), ["Server"]);
    let helpers = s.tokens_with_prefix("hel", 10).unwrap();
    assert_eq!(helpers, ["helper"]);
    assert!(s.tokens_with_prefix("zzz", 10).unwrap().is_empty());
    assert_eq!(s.tokens_with_prefix("", 3).unwrap().len(), 3);
}

#[test]
fn full_scan_respects_the_timeout() {
    let (fx, _) = fixture_with_files();
    let opts = SearchOptions {
        timeout: Some(Duration::ZERO),
        ..all(10)
    };
    let r = fx.searcher().search(r"\w+", &opts).unwrap();
    assert!(r.stats.timed_out);
    assert_eq!(r.stats.scanned, 0);
    assert!(r.hits.is_empty());
}

#[test]
fn invalid_patterns_are_errors_not_panics() {
    let (fx, _) = fixture_with_files();
    let s = fx.searcher();
    assert!(s.search("(", &all(10)).is_err());
    assert!(s.search("[a-", &all(10)).is_err());
    // The same string is fine as a literal.
    assert!(
        s.search(
            "(",
            &SearchOptions {
                literal: true,
                ..all(10)
            }
        )
        .is_ok()
    );
}

#[test]
fn empty_index_searches_cleanly() {
    let fx = Fixture::new();
    let s = fx.searcher();
    let r = s.search("anything", &all(10)).unwrap();
    assert_eq!(r.total_files, 0);
    assert_eq!(s.snapshot_id(), 0);
}

#[test]
fn searcher_snapshot_is_stable_across_later_commits() {
    let mut fx = Fixture::new();
    fx.write("a.txt", "first marker\n");
    fx.index(true);
    let before = fx.searcher();
    fx.write("b.txt", "second marker\n");
    fx.index(true);
    assert_eq!(before.search("marker", &all(10)).unwrap().total_files, 1);
    assert_eq!(
        fx.searcher()
            .search("marker", &all(10))
            .unwrap()
            .total_files,
        2
    );
}

#[test]
fn fuzzy_find_files_and_tokens() {
    let (fx, _) = fixture_with_files();
    let s = fx.searcher();
    let files: Vec<String> = s
        .find_files("srvr", 5)
        .into_iter()
        .map(|h| h.text)
        .collect();
    assert_eq!(
        files.first().map(String::as_str),
        Some("src/server.rs"),
        "{files:?}"
    );
    assert!(s.find_files("zzzqq", 5).is_empty());
    assert_eq!(s.find_files("", usize::MAX).len(), FILES.len());

    let toks: Vec<String> = s
        .find_tokens("hlp", 5)
        .unwrap()
        .into_iter()
        .map(|h| h.text)
        .collect();
    assert!(toks.contains(&"helper".to_string()), "{toks:?}");
    let toks: Vec<String> = s
        .find_tokens("Srv", 5)
        .unwrap()
        .into_iter()
        .map(|h| h.text)
        .collect();
    assert_eq!(toks.first().map(String::as_str), Some("Server"), "{toks:?}");
}

mod overlay {
    use super::*;
    use loupe_text::Overlay;

    fn put(o: &mut Overlay, path: &str, content: &str) {
        o.put(path, content.as_bytes().to_vec()).unwrap();
    }

    #[test]
    fn overlay_content_replaces_the_snapshot_version() {
        let (fx, _) = fixture_with_files();
        let s = fx.searcher();
        let mut o = Overlay::new();
        // src/util.rs on disk says "helper"; the unsaved buffer no longer does.
        put(&mut o, "src/util.rs", "pub fn renamed() {}\n");
        let r = s.search_with("helper", &all(10), Some(&o)).unwrap();
        assert_eq!(paths(&r), BTreeSet::from(["copy/util_copy.rs".to_string()]));
        assert_eq!(r.stats.overlay_files, 1);

        let r = s.search_with("renamed", &all(10), Some(&o)).unwrap();
        assert_eq!(paths(&r), BTreeSet::from(["src/util.rs".to_string()]));
        assert!(!r.hits[0].stale, "overlay content is the freshest truth");
    }

    #[test]
    fn overlay_adds_new_files_and_hides_removed_ones() {
        let (fx, _) = fixture_with_files();
        let s = fx.searcher();
        let mut o = Overlay::new();
        put(&mut o, "src/brand_new.rs", "fn brand_new_function() {}\n");
        o.remove("README.md");
        let r = s
            .search_with("brand_new_function", &all(10), Some(&o))
            .unwrap();
        assert_eq!(paths(&r), BTreeSet::from(["src/brand_new.rs".to_string()]));
        assert_eq!(
            s.search_with("TODO list", &all(10), Some(&o))
                .unwrap()
                .total_files,
            0
        );
        assert_eq!(
            s.search("TODO list", &all(10)).unwrap().total_files,
            1,
            "snapshot itself is unchanged"
        );

        let w = s
            .search_word_with("brand_new_function", &all(10), Some(&o))
            .unwrap();
        assert_eq!(w.total_files, 1);
    }

    #[test]
    fn overlay_works_with_full_scan_plans_and_empty_overlay() {
        let (fx, snap) = fixture_with_files();
        let s = fx.searcher();
        let mut o = Overlay::new();
        let base = s.search_with(r"\w+", &all(100), Some(&o)).unwrap();
        assert_eq!(
            base.total_files,
            snap.len(),
            "empty overlay changes nothing"
        );
        put(&mut o, "x.txt", "word\n");
        let r = s.search_with(r"\w+", &all(100), Some(&o)).unwrap();
        assert_eq!(r.total_files, snap.len() + 1);
    }

    #[test]
    fn fuzzy_find_sees_overlay() {
        let (fx, _) = fixture_with_files();
        let s = fx.searcher();
        let mut o = Overlay::new();
        put(&mut o, "src/zebra_stripes.rs", "fn zebra_crossing() {}\n");
        o.remove("src/main.rs");
        let f: Vec<String> = s
            .find_files_with("zebra", 5, Some(&o))
            .into_iter()
            .map(|h| h.text)
            .collect();
        assert_eq!(f, ["src/zebra_stripes.rs"]);
        assert!(
            s.find_files_with("main", 5, Some(&o))
                .iter()
                .all(|h| h.text != "src/main.rs")
        );
        let t: Vec<String> = s
            .find_tokens_with("zebcross", 3, Some(&o))
            .unwrap()
            .into_iter()
            .map(|h| h.text)
            .collect();
        assert_eq!(t, ["zebra_crossing"]);
    }
}
