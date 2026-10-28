//! Mine localization tasks from git history: multi-file commits whose subject becomes the query.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, bail};

use crate::eval::{Task, keywords};

#[derive(Clone, Debug)]
pub struct MineOptions {
    pub max_tasks: usize,
    /// Commits touching fewer surviving files are skipped (single-file fixes are not interesting).
    pub min_files: usize,
    /// Commits touching more files are skipped (renames, vendoring, mass reformatting).
    pub max_files: usize,
    /// How many commits to look back over.
    pub scan_commits: usize,
}

impl Default for MineOptions {
    fn default() -> Self {
        MineOptions {
            max_tasks: 200,
            min_files: 2,
            max_files: 12,
            scan_commits: 5000,
        }
    }
}

pub fn mine(repo: &Path, opts: &MineOptions) -> anyhow::Result<Vec<Task>> {
    // Record separator \x1e between commits, unit separator \x1f between hash and subject.
    let out = Command::new("git")
        .current_dir(repo)
        .args([
            "log",
            "--no-merges",
            "--name-only",
            "--pretty=format:%x1e%H%x1f%s",
        ])
        .arg(format!("-n{}", opts.scan_commits))
        .output()
        .context("running git log")?;
    if !out.status.success() {
        bail!(
            "git log failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let log = String::from_utf8_lossy(&out.stdout);

    let mut tasks = Vec::new();
    for record in log.split('\x1e').filter(|r| !r.trim().is_empty()) {
        let mut lines = record.lines();
        let Some((hash, subject)) = lines.next().and_then(|h| h.split_once('\x1f')) else {
            continue;
        };
        let touched: Vec<String> = lines
            .map(str::trim)
            .filter(|l| !l.is_empty())
            // Ground truth must be findable in the current tree.
            .filter(|l| repo.join(l).is_file())
            .map(String::from)
            .collect();
        if touched.len() < opts.min_files || touched.len() > opts.max_files {
            continue;
        }
        // A query with no usable keywords cannot be answered by any arm.
        if keywords(subject).len() < 2 {
            continue;
        }
        tasks.push(Task {
            id: hash[..hash.len().min(12)].to_string(),
            query: subject.to_string(),
            truth: touched,
        });
        if tasks.len() == opts.max_tasks {
            break;
        }
    }
    Ok(tasks)
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

    #[test]
    fn mines_multi_file_commits_only() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        if !git(d, &["init", "-q"]) {
            eprintln!("git unavailable; skipping");
            return;
        }
        let commit = |files: &[(&str, &str)], msg: &str| {
            for (p, c) in files {
                fs::write(d.join(p), c).unwrap();
            }
            assert!(git(d, &["add", "-A"]));
            assert!(git(d, &["commit", "-q", "-m", msg]));
        };
        commit(
            &[("a.rs", "1"), ("b.rs", "1"), ("c.rs", "1")],
            "Initial scaffolding for parser module",
        );
        commit(&[("a.rs", "2")], "Tweak single file only here");
        commit(
            &[("a.rs", "3"), ("b.rs", "3")],
            "Handle tokenizer overflow in parser",
        );
        commit(&[("a.rs", "4"), ("b.rs", "4")], "ok");
        fs::remove_file(d.join("c.rs")).unwrap();
        assert!(git(d, &["add", "-A"]));
        assert!(git(
            d,
            &["commit", "-q", "-m", "Remove obsolete scaffolding helpers"]
        ));

        let tasks = mine(d, &MineOptions::default()).unwrap();
        let queries: Vec<&str> = tasks.iter().map(|t| t.query.as_str()).collect();
        // Newest first; single-file, keyword-poor, and (after c.rs vanished) one-file commits are out.
        assert_eq!(
            queries,
            [
                "Handle tokenizer overflow in parser",
                "Initial scaffolding for parser module"
            ]
        );
        assert_eq!(tasks[0].truth.len(), 2);
        assert_eq!(
            tasks[1].truth,
            ["a.rs", "b.rs"],
            "deleted c.rs is dropped from ground truth"
        );
    }

    #[test]
    fn not_a_repo_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(mine(dir.path(), &MineOptions::default()).is_err());
    }
}
