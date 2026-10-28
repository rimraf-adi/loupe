//! Warm query latency vs ripgrep on a real corpus.
//!
//! Set `LOUPE_BENCH_CORPUS` to a directory to benchmark (e.g. `~/.cargo/registry/src`, or a
//! checkout fetched by `scripts/fetch-corpora.sh`). Without it the bench prints a note and
//! exits, so `cargo bench -- --test` stays fast in CI.
//!
//! Both sides select files the same way (hidden files included, `.gitignore` honored without a
//! `.git` dir, 1 MiB cap) and report files-with-matches, so result counts should be identical;
//! the bench prints both and aborts the comparison if they differ.
//!
//! Caveat: loupe runs in-process while ripgrep pays process start-up (a few ms) per query. That
//! favors loupe on fast queries; the heavier queries show the index's real effect.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use loupe_api::{Index, SearchOptions};
use loupe_core::Config;

struct Query {
    name: &'static str,
    pattern: &'static str,
    literal: bool,
    ignore_case: bool,
}

const QUERIES: &[Query] = &[
    Query {
        name: "rare_literal",
        pattern: "poll_ready",
        literal: true,
        ignore_case: false,
    },
    Query {
        name: "very_rare_literal",
        pattern: "ClientCertificateType",
        literal: true,
        ignore_case: false,
    },
    Query {
        name: "common_literal",
        pattern: "fn new",
        literal: true,
        ignore_case: false,
    },
    Query {
        name: "regex_with_literals",
        pattern: r"impl<.*> Future for",
        literal: false,
        ignore_case: false,
    },
    Query {
        name: "case_insensitive",
        pattern: "tokio",
        literal: true,
        ignore_case: true,
    },
    Query {
        name: "alternation",
        pattern: "TcpStream|UdpSocket",
        literal: false,
        ignore_case: false,
    },
    Query {
        name: "no_literal_full_scan",
        pattern: r"\b[a-z]{30,}\b",
        literal: false,
        ignore_case: false,
    },
];

fn rg_count(root: &PathBuf, q: &Query) -> usize {
    let mut cmd = Command::new("rg");
    cmd.args([
        "--hidden",
        "--glob=!.git",
        "--no-require-git",
        "--max-filesize=1M",
        "-l",
        "--no-messages",
    ]);
    if q.literal {
        cmd.arg("-F");
    }
    if q.ignore_case {
        cmd.arg("-i");
    }
    let out = cmd
        .args(["-e", q.pattern])
        .arg(root)
        .output()
        .expect("running rg");
    String::from_utf8_lossy(&out.stdout).lines().count()
}

fn bench_search(c: &mut Criterion) {
    let Some(root) = std::env::var_os("LOUPE_BENCH_CORPUS").map(PathBuf::from) else {
        eprintln!("LOUPE_BENCH_CORPUS not set; skipping search benchmark");
        return;
    };
    let root = root.canonicalize().expect("corpus path");
    let have_rg = Command::new("rg")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());

    let index_dir = tempfile::tempdir().unwrap();
    let mut config = Config::new(&root);
    config.index_dir = index_dir.path().to_path_buf();
    let mut index = Index::open_writer(config).unwrap();
    let started = Instant::now();
    let report = index.update(false).unwrap();
    eprintln!(
        "cold index: {} files in {:.2?}, index dir {} bytes",
        report.files,
        started.elapsed(),
        dir_size(index_dir.path())
    );
    let searcher = index.searcher().unwrap();

    let mut group = c.benchmark_group("search");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(3));

    for q in QUERIES {
        let opts = SearchOptions {
            case_insensitive: q.ignore_case,
            literal: q.literal,
            max_files: usize::MAX,
            max_matches_per_file: 1,
            context_lines: 0,
            timeout: None,
        };
        let ours = searcher.search(q.pattern, &opts).unwrap();
        if have_rg {
            let theirs = rg_count(&root, q);
            eprintln!("{:<22} loupe={} rg={}", q.name, ours.total_files, theirs);
            assert_eq!(
                ours.total_files, theirs,
                "result sets differ for {}",
                q.name
            );
        } else {
            eprintln!(
                "{:<22} loupe={} (rg not installed)",
                q.name, ours.total_files
            );
        }

        group.bench_function(format!("{}/loupe", q.name), |b| {
            b.iter(|| searcher.search(q.pattern, &opts).unwrap().total_files)
        });
        if have_rg {
            group.bench_function(format!("{}/rg", q.name), |b| b.iter(|| rg_count(&root, q)));
        }
    }
    group.finish();
}

fn dir_size(p: &std::path::Path) -> u64 {
    std::fs::read_dir(p)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

criterion_group!(benches, bench_search);
criterion_main!(benches);
