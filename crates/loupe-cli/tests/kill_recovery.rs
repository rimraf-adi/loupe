//! Crash recovery with real processes.
//!
//! A `loupe index` process is killed (a) at every durable step, via the debug-build crash points
//! in `loupe-store`, and (b) at random moments with SIGKILL. After each death the index must be
//! intact (readable, every live segment verified, snapshot either the old one or the new one,
//! searchable), a writer must be able to take over, and the next normal run must converge to
//! exactly what a clean index of the current files would contain.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

struct Env {
    repo: tempfile::TempDir,
    idx: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        let e = Env {
            repo: tempfile::tempdir().unwrap(),
            idx: tempfile::tempdir().unwrap(),
        };
        for i in 0..24 {
            e.write(
                &format!("src/m{i}.rs"),
                &format!(
                    "// marker_{}\npub fn func_{i}() {{ let v = {i}; }}\n",
                    i % 5
                ),
            );
        }
        e
    }

    fn root(&self) -> PathBuf {
        self.repo.path().canonicalize().unwrap()
    }

    fn write(&self, rel: &str, content: &str) {
        let p = self.repo.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn loupe(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_loupe"));
        c.args(args)
            .arg("-C")
            .arg(self.root())
            .arg("--index-dir")
            .arg(self.idx.path())
            .env("LOUPE_LOG", "error")
            .stderr(Stdio::piped())
            .stdout(Stdio::piped());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.loupe(args).output().unwrap()
    }

    /// `(snapshot id, live segment count)` from `loupe status`, asserting verification is clean.
    fn status(&self) -> (u64, usize) {
        let out = self.run(&["status"]);
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(
            out.status.success(),
            "status failed after the crash:\n{text}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(text.contains("verify:   ok"), "{text}");
        let snap = text.lines().find(|l| l.starts_with("snapshot:")).unwrap();
        let id = u64::from_str_radix(snap.split_whitespace().nth(1).unwrap(), 16).unwrap();
        let segs: usize = text
            .lines()
            .find(|l| l.starts_with("segments:"))
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        (id, segs)
    }

    fn matching_files(&self, marker: &str) -> BTreeSet<String> {
        let out = self.run(&["search", marker, "-F", "-l", "-n", "1000", "--no-daemon"]);
        assert!(
            out.status.success(),
            "search failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.ends_with(".rs") || l.contains(".rs "))
            .map(|l| l.split_whitespace().next().unwrap().to_string())
            .collect()
    }

    fn brute_force(&self, marker: &str) -> BTreeSet<String> {
        fn walk(dir: &Path, base: &Path, out: &mut BTreeSet<String>, marker: &str) {
            for e in fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, base, out, marker);
                } else if fs::read_to_string(&p).is_ok_and(|c| c.contains(marker)) {
                    out.insert(
                        p.strip_prefix(base)
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                }
            }
        }
        let mut out = BTreeSet::new();
        walk(&self.root(), &self.root(), &mut out, marker);
        out
    }

    fn segment_dirs(&self) -> usize {
        fs::read_dir(self.idx.path().join("segments"))
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().len() == 16)
            .count()
    }

    fn leftovers(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for dir in ["", "segments", "snapshots"] {
            for e in fs::read_dir(self.idx.path().join(dir)).unwrap() {
                let name = e.unwrap().file_name().to_string_lossy().into_owned();
                if name.starts_with(".tmp") || name.ends_with(".tmp") {
                    v.push(format!("{dir}/{name}"));
                }
            }
        }
        v
    }

    /// Change the repo so the next `index` has real work: new blobs to index.
    fn churn(&self, round: usize) {
        self.write(
            &format!("src/new_{round}.rs"),
            &format!("// marker_{}\npub fn added_{round}() {{}}\n", round % 5),
        );
        self.write(
            "src/m0.rs",
            &format!(
                "// marker_{}\npub fn func_0() {{ let v = {round}; }}\n",
                round % 5
            ),
        );
    }

    /// After a crash: intact, takeable by a writer, and converging.
    fn assert_recovers(&self, before: (u64, usize), context: &str) {
        // 1. Readable and fully verified, and still one of the two legitimate states.
        let after = self.status();
        assert!(
            after.0 == before.0 || after.0 == before.0 + 1,
            "{context}: snapshot {} is neither {} nor {}",
            after.0,
            before.0,
            before.0 + 1
        );
        // 2. Searchable in whatever state it is in.
        for m in ["marker_0", "marker_3"] {
            let _ = self.matching_files(m);
        }
        // 3. A new writer can take over (the dead process's lock is gone) and converges.
        let out = self.run(&["index"]);
        assert!(
            out.status.success(),
            "{context}: recovery run failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let (id, segs) = self.status();
        assert!(id >= after.0);
        for m in [
            "marker_0", "marker_1", "marker_2", "marker_3", "marker_4", "added_",
        ] {
            assert_eq!(
                self.matching_files(m),
                self.brute_force(m),
                "{context}: results for {m:?} after recovery"
            );
        }
        // 4. Recovery cleaned up: no temp files, no orphan segment directories.
        assert!(
            self.leftovers().is_empty(),
            "{context}: leftovers {:?}",
            self.leftovers()
        );
        assert_eq!(
            self.segment_dirs(),
            segs,
            "{context}: orphan segment directories"
        );
        // 5. And the run after that has nothing to do.
        let out = self.run(&["index"]);
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("up to date"),
            "{context}: not stable:\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

#[test]
fn a_process_dying_at_every_durable_step_leaves_a_recoverable_index() {
    let points = [
        "segment.payload_written@1",
        "segment.payload_written@4",
        "segment.payload_written@8",
        "segment.before_meta@1",
        "segment.before_rename@1",
        "segment.after_rename@1",
        "snapshot.before_rename@1",
        "commit.after_snapshot@1",
        "manifest.before_rename@1",
        "commit.after_manifest@1",
    ];
    for (round, point) in points.iter().enumerate() {
        let env = Env::new();
        let first = env.run(&["index"]);
        assert!(
            first.status.success(),
            "{}",
            String::from_utf8_lossy(&first.stderr)
        );
        let before = env.status();

        env.churn(round);
        let mut cmd = env.loupe(&["index"]);
        cmd.env("LOUPE_CRASH", point);
        let out = cmd.output().unwrap();
        assert!(
            !out.status.success(),
            "{point}: the process was supposed to die there but exited normally"
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(&format!(
                "crash injected at {}",
                point.split('@').next().unwrap()
            )),
            "{point}: died for another reason:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );

        env.assert_recovers(before, point);
    }
}

#[test]
fn random_sigkills_during_indexing_never_corrupt_the_index() {
    let env = Env::new();
    assert!(env.run(&["index"]).status.success());
    let mut rng = 0x2545_f491_4f6c_dd1du64;
    let mut killed_mid_run = 0;
    for round in 0..40usize {
        env.churn(round);
        let before = env.status();
        let mut child = env.loupe(&["index"]).spawn().unwrap();
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        std::thread::sleep(Duration::from_micros(2_000 + rng % 70_000));
        let finished = child.try_wait().unwrap().is_some();
        if !finished {
            child.kill().unwrap(); // SIGKILL
            killed_mid_run += 1;
        }
        let _ = child.wait_with_output();

        let after = env.status();
        assert!(
            after.0 == before.0 || after.0 == before.0 + 1,
            "round {round}: snapshot {} after {}",
            after.0,
            before.0
        );
        if round % 8 == 7 {
            env.assert_recovers(after, &format!("sigkill round {round}"));
        }
    }
    assert!(
        killed_mid_run >= 5,
        "only {killed_mid_run} of 40 runs were actually killed mid-run; the test is too gentle"
    );
    env.assert_recovers(env.status(), "final");
}

#[test]
fn a_second_writer_cannot_start_while_one_is_running_and_a_dead_one_releases_the_lock() {
    let env = Env::new();
    assert!(env.run(&["index"]).status.success());
    // A daemon holds the writer lock.
    let mut daemon = env.loupe(&["daemon", "run", "--no-watch"]).spawn().unwrap();
    for _ in 0..100 {
        if env.run(&["daemon", "status"]).status.success() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let blocked = env.run(&["index"]);
    assert!(
        !blocked.status.success(),
        "a second writer must be refused while the daemon runs"
    );
    assert!(
        String::from_utf8_lossy(&blocked.stderr)
            .to_lowercase()
            .contains("lock"),
        "{}",
        String::from_utf8_lossy(&blocked.stderr)
    );
    // Reads are unaffected.
    assert!(env.run(&["status"]).status.success());

    // SIGKILL the daemon: the OS drops its lock and a new writer proceeds.
    daemon.kill().unwrap();
    let _ = daemon.wait();
    env.churn(99);
    let out = env.run(&["index"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    env.assert_recovers(env.status(), "after killing the lock holder");
}

#[test]
fn compact_gc_and_daemon_metrics_commands_work() {
    let env = Env::new();
    // Several small updates leave several segments to merge.
    for round in 0..6 {
        env.churn(round);
        assert!(env.run(&["index"]).status.success());
    }
    let (_, segs_before) = env.status();
    assert!(segs_before >= 6, "{segs_before} segments");
    let out = env.run(&["compact"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (_, segs_after) = env.status();
    assert!(segs_after < segs_before, "{segs_before} -> {segs_after}");
    assert!(env.run(&["gc"]).status.success());
    for m in ["marker_0", "marker_3", "added_"] {
        assert_eq!(env.matching_files(m), env.brute_force(m), "{m}");
    }

    // With a daemon running, compact and metrics go through it.
    let mut daemon = env.loupe(&["daemon", "run", "--no-watch"]).spawn().unwrap();
    for _ in 0..100 {
        if env.run(&["daemon", "status"]).status.success() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = env.run(&["compact"]);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success() && text.contains("daemon compacted"),
        "{text}"
    );
    let out = env.run(&["daemon", "metrics"]);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        text.contains("compactions") && text.contains("compact"),
        "{text}"
    );
    let _ = env.run(&["daemon", "stop"]);
    let _ = daemon.wait();
}
