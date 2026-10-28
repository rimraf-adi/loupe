//! Compaction, segment-size bounding, and GC.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::Duration;

use loupe_api::{CompactPolicy, GraphOptions, Index, QueryOptions, SearchOptions};
use loupe_core::Config;

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
    fn config(&self) -> Config {
        let mut c = Config::new(self.repo.path().canonicalize().unwrap());
        c.index_dir = self.idx.path().to_path_buf();
        c
    }
    fn write(&self, rel: &str, content: &str) {
        let p = self.repo.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }
    fn writer(&self) -> Index {
        let mut idx = Index::open_writer(self.config()).unwrap();
        idx.set_retire_grace(Duration::ZERO);
        idx
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

fn opts() -> SearchOptions {
    SearchOptions {
        literal: true,
        max_files: 1000,
        timeout: None,
        context_lines: 0,
        ..SearchOptions::default()
    }
}

fn files_matching(idx: &Index, word: &str) -> BTreeSet<String> {
    idx.search_text(word, &opts())
        .unwrap()
        .hits
        .into_iter()
        .map(|h| h.path)
        .collect()
}

fn symbol_paths(idx: &Index, name: &str) -> BTreeSet<String> {
    idx.find_symbol(name, None, 100)
        .unwrap()
        .into_iter()
        .map(|h| h.path)
        .collect()
}

fn age_files(root: &Path) {
    let old = std::time::SystemTime::now() - Duration::from_secs(3600);
    fn walk(p: &Path, old: std::time::SystemTime) {
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
    walk(root, old);
}

#[test]
fn an_update_larger_than_the_segment_budget_is_split_without_changing_results() {
    let env = Env::new();
    for i in 0..40 {
        env.write(
            &format!("src/m{i}.rs"),
            &format!(
                "pub fn split_target_{i}() {{ let padding = \"{}\"; }}\n",
                "x".repeat(200)
            ),
        );
    }
    let mut small = env.config();
    small.max_segment_bytes = 3_000; // roughly 12 files per segment
    let mut idx = Index::open_writer(small).unwrap();
    let rep = idx.update(false).unwrap();
    assert!(
        rep.segments_written >= 3,
        "{} segments",
        rep.segments_written
    );
    assert_eq!(idx.store().segment_ids().len(), rep.segments_written);
    for i in [0, 17, 39] {
        assert_eq!(
            symbol_paths(&idx, &format!("split_target_{i}")),
            BTreeSet::from([format!("src/m{i}.rs")])
        );
    }
    assert_eq!(files_matching(&idx, "split_target_").len(), 40);
    let q = idx
        .session(&GraphOptions {
            cochange: None,
            ..GraphOptions::default()
        })
        .unwrap();
    assert!(
        !q.query("split target 17", &QueryOptions::default(), None)
            .unwrap()
            .hits
            .is_empty()
    );

    // The same repo indexed with one big segment answers identically.
    drop(idx);
    let env2 = Env::new();
    for i in 0..40 {
        env2.write(
            &format!("src/m{i}.rs"),
            &format!(
                "pub fn split_target_{i}() {{ let padding = \"{}\"; }}\n",
                "x".repeat(200)
            ),
        );
    }
    let mut one = env2.writer();
    assert_eq!(one.update(false).unwrap().segments_written, 1);
    assert_eq!(
        files_matching(&one, "split_target_"),
        (0..40).map(|i| format!("src/m{i}.rs")).collect()
    );
}

#[test]
fn compaction_merges_many_small_segments_and_answers_exactly_the_same() {
    let env = Env::new();
    let mut idx = env.writer();
    for round in 0..14 {
        env.write(
            &format!("src/r{round}.rs"),
            &format!("pub fn round_fn_{round}() {{ /* merge_marker */ }}\n"),
        );
        idx.update(false).unwrap();
    }
    assert!(idx.store().segment_ids().len() >= 14);
    let before_text = files_matching(&idx, "merge_marker");
    let before_syms: Vec<BTreeSet<String>> = (0..14)
        .map(|i| symbol_paths(&idx, &format!("round_fn_{i}")))
        .collect();
    let before_sem = idx
        .search_semantic("round fn merge marker", &Default::default())
        .unwrap()
        .hits
        .len();
    let before_bytes = dir_bytes(&env.idx.path().join("segments"));

    let rep = idx
        .compact(&CompactPolicy {
            min_dead_ratio: 0.5,
            max_segments: 4,
        })
        .unwrap();
    assert!(rep.merged >= 10, "{rep:?}");
    assert_eq!(
        rep.blobs_dropped_dead, 0,
        "nothing was dead, only fragmented"
    );
    assert_eq!(rep.blobs_skipped_changed, 0);
    let segs = idx.store().segment_ids().len();
    assert!(segs <= 5, "{segs} segments after compaction");
    assert!(
        idx.gc().unwrap().segments_removed >= rep.merged,
        "retired segments are collected"
    );
    assert!(idx.store().verify().is_empty());

    assert_eq!(files_matching(&idx, "merge_marker"), before_text);
    for i in 0..14 {
        assert_eq!(
            symbol_paths(&idx, &format!("round_fn_{i}")),
            before_syms[i as usize]
        );
    }
    assert_eq!(
        idx.search_semantic("round fn merge marker", &Default::default())
            .unwrap()
            .hits
            .len(),
        before_sem
    );
    assert!(
        dir_bytes(&env.idx.path().join("segments")) < before_bytes,
        "fewer segments means less per-segment overhead"
    );
    // And a normal update afterwards finds nothing left to do.
    assert!(idx.update(false).unwrap().unchanged);
}

#[test]
fn compaction_reclaims_dead_blobs_left_by_edits_and_deletions() {
    let env = Env::new();
    let mut idx = env.writer();
    for i in 0..30 {
        env.write(
            &format!("src/f{i}.rs"),
            &format!(
                "pub fn gen_one_{i}() {{ let v = \"{}\"; }}\n",
                "a".repeat(300)
            ),
        );
    }
    idx.update(false).unwrap();
    // Rewrite every file (all first-generation blobs die), then delete a third of them.
    for i in 0..30 {
        env.write(
            &format!("src/f{i}.rs"),
            &format!(
                "pub fn gen_two_{i}() {{ let v = \"{}\"; }}\n",
                "b".repeat(300)
            ),
        );
    }
    idx.update(false).unwrap();
    for i in 0..10 {
        fs::remove_file(env.repo.path().join(format!("src/f{i}.rs"))).unwrap();
    }
    idx.update(false).unwrap();
    let segs_before = idx.store().segment_ids().len();
    let bytes_before = dir_bytes(&env.idx.path().join("segments"));
    // Two generations of blobs were indexed; the delete-only update wrote no segment.
    assert_eq!(segs_before, 2);
    assert_eq!(
        symbol_paths(&idx, "gen_one_5").len(),
        0,
        "dead blobs never surface in results"
    );

    let rep = idx
        .compact(&CompactPolicy {
            min_dead_ratio: 0.5,
            max_segments: 100,
        })
        .unwrap();
    assert!(rep.blobs_dropped_dead >= 30, "{rep:?}");
    assert!(rep.merged >= 1);
    idx.gc().unwrap();
    let bytes_after = dir_bytes(&env.idx.path().join("segments"));
    assert!(
        bytes_after < bytes_before,
        "segments shrank from {bytes_before} to {bytes_after} bytes"
    );
    assert_eq!(files_matching(&idx, "gen_two_").len(), 20);
    assert_eq!(files_matching(&idx, "gen_one_").len(), 0);
    assert!(idx.store().verify().is_empty());
}

#[test]
fn a_file_that_changed_before_compaction_is_never_answered_from_a_wrong_index() {
    let env = Env::new();
    let mut idx = env.writer();
    for i in 0..6 {
        env.write(
            &format!("src/s{i}.rs"),
            &format!("pub fn fragile_{i}() {{ let tag = \"original\"; }}\n"),
        );
        idx.update(false).unwrap();
    }
    // One file changes on disk, but the index has not seen it yet.
    std::thread::sleep(Duration::from_millis(20));
    env.write(
        "src/s2.rs",
        "pub fn fragile_2() { let tag = \"edited!!\"; }\n",
    );

    let rep = idx
        .compact(&CompactPolicy {
            min_dead_ratio: 0.5,
            max_segments: 2,
        })
        .unwrap();
    assert_eq!(
        rep.blobs_skipped_changed, 1,
        "the changed file is not re-indexed under its old identity: {rep:?}"
    );
    // Right now: text search still finds s2 (unindexed blobs are always scanned) with its true
    // content, flagged stale; nothing returns the old "original" text for it.
    let r = idx.search_text("edited!!", &opts()).unwrap();
    assert_eq!(r.hits.len(), 1);
    assert!(r.hits[0].stale);
    assert!(
        idx.search_text("original", &opts())
            .unwrap()
            .hits
            .iter()
            .all(|h| h.path != "src/s2.rs")
    );
    // The next update indexes the new content and clears the flag.
    idx.update(false).unwrap();
    assert!(!idx.search_text("edited!!", &opts()).unwrap().hits[0].stale);
    assert_eq!(
        symbol_paths(&idx, "fragile_2"),
        BTreeSet::from(["src/s2.rs".to_string()])
    );
}

#[test]
fn readers_pinned_to_an_old_snapshot_survive_compaction_and_gc_within_the_grace() {
    let env = Env::new();
    let mut idx = Index::open_writer(env.config()).unwrap(); // default 30 s grace
    for i in 0..8 {
        env.write(
            &format!("src/p{i}.rs"),
            &format!("pub fn pinned_{i}() {{}}\n"),
        );
        idx.update(false).unwrap();
    }
    let session = idx
        .session(&GraphOptions {
            cochange: None,
            ..GraphOptions::default()
        })
        .unwrap();
    let before = session.text.search("pinned_", &opts()).unwrap().total_files;

    idx.compact(&CompactPolicy {
        min_dead_ratio: 0.5,
        max_segments: 2,
    })
    .unwrap();
    idx.gc().unwrap();
    // The pinned session keeps working (its segments are mmapped and still on disk)...
    assert_eq!(
        session.text.search("pinned_", &opts()).unwrap().total_files,
        before
    );
    // ...and a fresh reader sees the compacted index.
    let fresh = Index::open_reader(env.config()).unwrap();
    assert_eq!(
        fresh.search_text("pinned_", &opts()).unwrap().total_files,
        before
    );
    assert!(fresh.store().segment_ids().len() <= 3);
}

#[test]
fn nothing_to_compact_is_a_no_op() {
    let env = Env::new();
    env.write("src/a.rs", "pub fn only_one() {}\n");
    age_files(env.repo.path());
    let mut idx = env.writer();
    idx.update(false).unwrap();
    let gen_before = idx.snapshot_id();
    let rep = idx.compact(&CompactPolicy::default()).unwrap();
    assert_eq!(rep, Default::default());
    assert_eq!(
        idx.snapshot_id(),
        gen_before,
        "no new generation was committed"
    );
}
