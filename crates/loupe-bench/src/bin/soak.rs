//! Soak test: a daemon with its file watcher runs against a repo that is edited, created, deleted
//! and renamed continuously, while searches check what the index says.
//!
//! Every mutation must become visible (and every deletion invisible) within a deadline, and
//! periodically the index is quiesced and compared with a brute-force scan of the working tree.
//! At the end it reports memory, segment count and index size so growth shows up.
//!
//! This is a *short* soak (default 60 s). It exercises the same code paths a long one would, but it
//! cannot show what only hours of uptime would; run it with `--secs 14400` for that.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use clap::Parser;
use loupe_api::SearchOptions;
use loupe_core::Config;
use loupe_daemon::{Client, DaemonOptions};
use loupe_proto::{Request, Response};

#[derive(Parser)]
struct Args {
    /// How long to run, in seconds.
    #[arg(long, default_value_t = 60)]
    secs: u64,
    /// Files in the initial repo.
    #[arg(long, default_value_t = 300)]
    files: usize,
    /// Seconds a change may take to become visible before it counts as a failure.
    #[arg(long, default_value_t = 10)]
    deadline_secs: u64,
    #[arg(long, default_value_t = 1)]
    seed: u64,
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn opts() -> SearchOptions {
    SearchOptions {
        literal: true,
        max_files: 10_000,
        timeout: None,
        context_lines: 0,
        ..SearchOptions::default()
    }
}

fn search(client: &mut Client, token: &str) -> anyhow::Result<BTreeSet<String>> {
    match client.call(&Request::Search {
        pattern: token.to_string(),
        word: false,
        opts: opts(),
    })? {
        Response::Search(r) => Ok(r.hits.into_iter().map(|h| h.path).collect()),
        other => anyhow::bail!("unexpected response {other:?}"),
    }
}

fn brute(root: &Path, token: &str) -> BTreeSet<String> {
    fn walk(dir: &Path, root: &Path, token: &str, out: &mut BTreeSet<String>) {
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, root, token, out);
            } else if fs::read_to_string(&p).is_ok_and(|c| c.contains(token)) {
                out.insert(
                    p.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(root, root, token, &mut out);
    out
}

fn rss_mb() -> f64 {
    Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<f64>()
                .ok()
        })
        .map_or(0.0, |kb| kb / 1024.0)
}

fn open_fds() -> usize {
    fs::read_dir("/dev/fd").map_or(0, |d| d.count())
}

fn write(root: &Path, rel: &str, token: &str, filler: usize) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    let body = format!(
        "// {token}\npub fn f_{}() {{ let pad = \"{}\"; }}\n",
        token.replace('-', "_"),
        "x".repeat(filler)
    );
    fs::write(p, body).unwrap();
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let repo = tempfile::tempdir()?;
    let idx = tempfile::tempdir()?;
    let root: PathBuf = repo.path().canonicalize()?;
    let mut rng = Rng(args.seed.max(1) * 0x9e37_79b9_7f4a_7c15);

    // path -> the unique token its current content carries.
    let mut model: BTreeMap<String, String> = BTreeMap::new();
    let mut gen_counter = 0u64;
    let mut fresh = |rng: &mut Rng| {
        gen_counter += 1;
        format!("soaktok-{gen_counter}-{:x}", rng.next() & 0xffff)
    };
    for i in 0..args.files {
        let rel = format!("src/d{}/f{i}.rs", i % 10);
        let t = fresh(&mut rng);
        write(&root, &rel, &t, 100 + rng.below(400));
        model.insert(rel, t);
    }

    let mut config = Config::new(root.clone());
    config.index_dir = idx.path().to_path_buf();
    let daemon = loupe_daemon::start(
        config.clone(),
        DaemonOptions {
            reconcile_every: Duration::from_secs(2),
            rehash_every: Duration::from_secs(20),
            debounce: Duration::from_millis(100),
            watch: true,
            min_update_interval: Duration::from_millis(200),
        },
    )?;
    let mut client = Client::connect(&config.index_dir)?;

    let deadline = Duration::from_secs(args.deadline_secs);
    let (mut mutations, mut audits, mut failures) = (0u64, 0u64, 0u64);
    let mut latencies: Vec<f64> = Vec::new();
    let mut fail = |msg: String| {
        eprintln!("FAIL: {msg}");
        failures += 1;
    };
    let rss_start = rss_mb();
    let fds_start = open_fds();
    let mut rss_samples = vec![rss_start];
    let begin = Instant::now();
    let mut next_audit = Instant::now() + Duration::from_secs(5);

    while begin.elapsed() < Duration::from_secs(args.secs) {
        // One mutation, then wait until the index agrees.
        let keys: Vec<String> = model.keys().cloned().collect();
        let (expect_present, expect_gone): (Option<(String, String)>, Option<String>) =
            match rng.below(10) {
                0..=4 => {
                    // edit in place
                    let rel = keys[rng.below(keys.len())].clone();
                    let old = model[&rel].clone();
                    let t = fresh(&mut rng);
                    write(&root, &rel, &t, 100 + rng.below(400));
                    model.insert(rel.clone(), t.clone());
                    (Some((rel, t)), Some(old))
                }
                5..=6 => {
                    let rel = format!("src/new/n{}.rs", rng.next() % 100_000);
                    let t = fresh(&mut rng);
                    write(&root, &rel, &t, 200);
                    if let Some(old) = model.insert(rel.clone(), t.clone()) {
                        (Some((rel, t)), Some(old))
                    } else {
                        (Some((rel, t)), None)
                    }
                }
                7..=8 if keys.len() > 20 => {
                    let rel = keys[rng.below(keys.len())].clone();
                    let old = model.remove(&rel).unwrap();
                    fs::remove_file(root.join(&rel))?;
                    (None, Some(old))
                }
                _ if keys.len() > 20 => {
                    // rename: same content, new path
                    let rel = keys[rng.below(keys.len())].clone();
                    let t = model.remove(&rel).unwrap();
                    let to = format!("src/moved/m{}.rs", rng.next() % 100_000);
                    fs::create_dir_all(root.join(&to).parent().unwrap())?;
                    fs::rename(root.join(&rel), root.join(&to))?;
                    model.insert(to.clone(), t.clone());
                    (Some((to, t)), None)
                }
                _ => (None, None),
            };
        mutations += 1;

        let t0 = Instant::now();
        loop {
            let present_ok = match &expect_present {
                Some((rel, t)) => search(&mut client, t)? == BTreeSet::from([rel.clone()]),
                None => true,
            };
            let gone_ok = match &expect_gone {
                Some(t) => {
                    let got = search(&mut client, t)?;
                    // A rename keeps its token, so that token legitimately still exists.
                    expect_present.as_ref().is_some_and(|(_, nt)| nt == t) || got.is_empty()
                }
                None => true,
            };
            if present_ok && gone_ok {
                latencies.push(t0.elapsed().as_secs_f64() * 1000.0);
                break;
            }
            if t0.elapsed() > deadline {
                fail(format!(
                    "change not reflected after {deadline:?}: present={expect_present:?} gone={expect_gone:?}"
                ));
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        if Instant::now() >= next_audit {
            next_audit = Instant::now() + Duration::from_secs(10);
            // Quiesce, then compare everything with the disk.
            let _ = client.call(&Request::Update { force_rehash: true })?;
            audits += 1;
            let mut sample: Vec<&String> = model.values().collect();
            let step = (sample.len() / 40).max(1);
            sample = sample.into_iter().step_by(step).collect();
            for t in sample {
                let (got, want) = (search(&mut client, t)?, brute(&root, t));
                if got != want {
                    fail(format!("audit: {t}: index {got:?} vs disk {want:?}"));
                }
            }
            for common in ["pub fn f_soaktok", "soaktok-1-"] {
                let (got, want) = (search(&mut client, common)?, brute(&root, common));
                if got != want {
                    fail(format!(
                        "audit: {common:?}: {} vs {} files",
                        got.len(),
                        want.len()
                    ));
                }
            }
            rss_samples.push(rss_mb());
        }
    }

    // Final state.
    let _ = client.call(&Request::Update { force_rehash: true })?;
    let Response::Metrics(m) = client.call(&Request::Metrics)? else {
        anyhow::bail!("no metrics");
    };
    let (rss_end, fds_end) = (rss_mb(), open_fds());
    latencies.sort_by(f64::total_cmp);
    let pct = |q: f64| latencies[((latencies.len() as f64 * q) as usize).min(latencies.len() - 1)];
    println!(
        "soak: {:.0}s, {mutations} mutations, {audits} audits, {failures} failures",
        begin.elapsed().as_secs_f64()
    );
    if !latencies.is_empty() {
        println!(
            "edit->searchable latency ms: p50 {:.0}  p95 {:.0}  max {:.0}",
            pct(0.5),
            pct(0.95),
            latencies[latencies.len() - 1]
        );
    }
    println!(
        "rss MB: start {rss_start:.0}  end {rss_end:.0}  peak sample {:.0}",
        rss_samples.iter().copied().fold(0.0, f64::max)
    );
    println!("open fds: start {fds_start}  end {fds_end}");
    println!(
        "daemon: {} updates, {} compactions, {} segments, index {:.1} MiB, overlay {} files",
        m.updates,
        m.compactions,
        m.segments,
        m.index_bytes as f64 / 1048576.0,
        m.overlay_files
    );
    drop(daemon);
    if failures > 0 {
        std::process::exit(1);
    }
    Ok(())
}
