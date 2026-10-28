//! Co-change mining: files that change together in git history are related, whether or not any
//! import connects them.
//!
//! Uses the `git` CLI read-only (`git log --name-only`). A missing `git`, a non-repository, or an
//! empty history simply yields no edges; indexing never depends on it.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct CoChangeOptions {
    /// How many of the most recent commits to read.
    pub max_commits: usize,
    /// Commits touching more files than this are skipped (formatting, vendoring, renames).
    pub max_files_per_commit: usize,
    /// A commit's weight halves every this many days.
    pub half_life_days: f64,
    /// Neighbours kept per file.
    pub top_k: usize,
}

impl Default for CoChangeOptions {
    fn default() -> Self {
        CoChangeOptions {
            max_commits: 3000,
            max_files_per_commit: 40,
            half_life_days: 365.0,
            top_k: 15,
        }
    }
}

/// Directed co-change edge `(from, to, strength)`, strength in `(0, 1]`. Every kept pair is
/// emitted in both directions with the same strength.
pub type CoChangeEdge = (String, String, f32);

/// Mine co-change edges among `known` files. Paths are relative to `root`.
pub fn mine<F: Fn(&str) -> bool>(
    root: &Path,
    is_known: F,
    opts: &CoChangeOptions,
) -> Vec<CoChangeEdge> {
    let output = Command::new("git")
        .current_dir(root)
        .args([
            "log",
            "--no-merges",
            "--relative",
            "--name-only",
            "--pretty=format:%x1e%ct",
        ])
        .arg(format!("-n{}", opts.max_commits))
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as f64;
    let log = String::from_utf8_lossy(&output.stdout);

    let mut pairs: HashMap<(&str, &str), f64> = HashMap::new();
    for record in log.split('\x1e').filter(|r| !r.trim().is_empty()) {
        let mut lines = record.lines();
        let Some(ts) = lines.next().and_then(|l| l.trim().parse::<f64>().ok()) else {
            continue;
        };
        let all: Vec<&str> = lines.map(str::trim).filter(|l| !l.is_empty()).collect();
        if all.len() > opts.max_files_per_commit {
            continue;
        }
        let mut files: Vec<&str> = all.into_iter().filter(|f| is_known(f)).collect();
        files.sort_unstable();
        files.dedup();
        if files.len() < 2 {
            continue;
        }
        let age_days = ((now - ts) / 86_400.0).max(0.0);
        let weight = 0.5f64.powf(age_days / opts.half_life_days) / (files.len() - 1) as f64;
        for (i, a) in files.iter().enumerate() {
            for b in &files[i + 1..] {
                *pairs.entry((a, b)).or_default() += weight;
            }
        }
    }

    // Strongest partner weight per file, used to normalise.
    let mut max_w: HashMap<&str, f64> = HashMap::new();
    let mut neighbours: HashMap<&str, Vec<(&str, f64)>> = HashMap::new();
    for (&(a, b), &w) in &pairs {
        for (x, y) in [(a, b), (b, a)] {
            let m = max_w.entry(x).or_insert(0.0);
            *m = m.max(w);
            neighbours.entry(x).or_default().push((y, w));
        }
    }

    // Keep a pair if either file ranks the other in its top_k, and give it ONE strength: the
    // relation is symmetric, so normalising per source file would make a file's only partner look
    // maximal from its side while looking weaker from the partner's side. Dividing by the larger
    // of the two files' strongest partners keeps both directions equal.
    let mut kept: Vec<(&str, &str, f64)> = Vec::new();
    for (src, list) in neighbours.iter_mut() {
        list.sort_by(|x, y| y.1.total_cmp(&x.1).then(x.0.cmp(y.0)));
        list.truncate(opts.top_k);
        for &(dst, w) in list.iter() {
            kept.push((
                if src < &dst { src } else { dst },
                if src < &dst { dst } else { src },
                w,
            ));
        }
    }
    kept.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    kept.dedup_by(|b, a| (a.0, a.1) == (b.0, b.1));
    let mut out = Vec::with_capacity(kept.len() * 2);
    for (a, b, w) in kept {
        let strength = (w / max_w[a].max(max_w[b])) as f32;
        out.push((a.to_string(), b.to_string(), strength));
        out.push((b.to_string(), a.to_string(), strength));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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

    fn commit(dir: &Path, files: &[(&str, &str)], msg: &str) {
        for (p, c) in files {
            let path = dir.join(p);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, c).unwrap();
        }
        assert!(git(dir, &["add", "-A"]));
        assert!(git(dir, &["commit", "-q", "-m", msg]));
    }

    #[test]
    fn pairs_that_change_together_are_linked_and_normalised() {
        let dir = tempfile::tempdir().unwrap();
        if !git(dir.path(), &["init", "-q"]) {
            eprintln!("git unavailable; skipping");
            return;
        }
        let d = dir.path();
        commit(d, &[("a.rs", "1"), ("b.rs", "1"), ("c.rs", "1")], "init");
        commit(d, &[("a.rs", "2"), ("b.rs", "2")], "ab 1");
        commit(d, &[("a.rs", "3"), ("b.rs", "3")], "ab 2");
        commit(d, &[("a.rs", "4"), ("c.rs", "4")], "ac");
        commit(d, &[("solo.rs", "1")], "solo");

        let edges = mine(d, |_| true, &CoChangeOptions::default());
        let get = |a: &str, b: &str| edges.iter().find(|(x, y, _)| x == a && y == b).map(|e| e.2);
        assert_eq!(get("a.rs", "b.rs"), Some(1.0), "b is a's strongest partner");
        assert!(get("a.rs", "c.rs").unwrap() < 1.0);
        for (x, y) in [("a.rs", "b.rs"), ("a.rs", "c.rs"), ("b.rs", "c.rs")] {
            assert_eq!(get(x, y), get(y, x), "co-change is symmetric: {x} <-> {y}");
        }
        assert!(get("b.rs", "a.rs").is_some() && get("b.rs", "c.rs").is_some());
        assert!(
            edges
                .iter()
                .all(|(a, b, _)| a != "solo.rs" && b != "solo.rs")
        );
        assert!(edges.iter().all(|(_, _, s)| *s > 0.0 && *s <= 1.0));

        // Only known files participate.
        let known = mine(d, |f| f != "c.rs", &CoChangeOptions::default());
        assert!(known.iter().all(|(a, b, _)| a != "c.rs" && b != "c.rs"));
    }

    #[test]
    fn a_files_only_partner_is_not_inflated_to_the_maximum() {
        // x changes with y in two commits, y also changes with z in four: from x's side y is its
        // only partner, but the pair is weaker than y-z and must read that way from both sides.
        let dir = tempfile::tempdir().unwrap();
        if !git(dir.path(), &["init", "-q"]) {
            return;
        }
        let d = dir.path();
        for i in 0..2 {
            commit(
                d,
                &[("x.rs", &format!("{i}")), ("y.rs", &format!("{i}"))],
                "xy",
            );
        }
        for i in 0..4 {
            commit(
                d,
                &[("y.rs", &format!("z{i}")), ("z.rs", &format!("{i}"))],
                "yz",
            );
        }
        let edges = mine(d, |_| true, &CoChangeOptions::default());
        let get = |a: &str, b: &str| {
            edges
                .iter()
                .find(|(x, y, _)| x == a && y == b)
                .map(|e| e.2)
                .unwrap()
        };
        assert_eq!(get("y.rs", "z.rs"), 1.0);
        let xy = get("x.rs", "y.rs");
        assert!(
            (xy - 0.5).abs() < 0.05,
            "x-y should be about half of y-z (2 vs 4 commits), got {xy}"
        );
        assert_eq!(xy, get("y.rs", "x.rs"));
    }

    #[test]
    fn huge_commits_are_ignored_and_top_k_caps_neighbours() {
        let dir = tempfile::tempdir().unwrap();
        if !git(dir.path(), &["init", "-q"]) {
            return;
        }
        let d = dir.path();
        let many: Vec<(String, &str)> = (0..10).map(|i| (format!("f{i}.rs"), "x")).collect();
        let refs: Vec<(&str, &str)> = many.iter().map(|(p, c)| (p.as_str(), *c)).collect();
        commit(d, &refs, "vendor drop");
        let opts = CoChangeOptions {
            max_files_per_commit: 5,
            ..CoChangeOptions::default()
        };
        assert!(
            mine(d, |_| true, &opts).is_empty(),
            "10-file commit exceeds the cap of 5"
        );

        // A pair is kept if either file ranks the other in its top 3, so one file can end up with
        // more than 3 neighbours, but the total stays bounded by files x top_k (x2 directions).
        let opts = |top_k| CoChangeOptions {
            top_k,
            ..CoChangeOptions::default()
        };
        let uncapped = mine(d, |_| true, &opts(100));
        let capped = mine(d, |_| true, &opts(3));
        assert_eq!(uncapped.len(), 10 * 9, "all 45 pairs, both directions");
        assert!(capped.len() <= 10 * 3 * 2, "{} edges", capped.len());
        assert!(capped.len() < uncapped.len());
    }

    #[test]
    fn not_a_repository_or_no_commits_yields_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(mine(dir.path(), |_| true, &CoChangeOptions::default()).is_empty());
        if git(dir.path(), &["init", "-q"]) {
            assert!(mine(dir.path(), |_| true, &CoChangeOptions::default()).is_empty());
        }
    }
}
