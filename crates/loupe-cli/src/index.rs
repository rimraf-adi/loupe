//! `loupe index` / `loupe status` / `loupe search`.

use std::time::Instant;

use anyhow::Context;
use loupe_api::{
    EdgeKind, GraphOptions, Index, MapOptions, OutlineOptions, RelatedOptions, SearchOptions,
    SearchResult, SemanticOptions, SemanticState, SymbolKind, render_outline,
};
use loupe_core::Config;
use loupe_daemon::Client;
use loupe_proto::{Request, Response};

pub fn index(config: &Config, force_rehash: bool) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut index = Index::open_writer(config.clone())
        .with_context(|| format!("opening index at {}", config.index_dir.display()))?;
    let report = index.update(force_rehash)?;
    let (walk, diff) = (&report.walk, &report.diff);

    println!(
        "snapshot {:016x}: {} files  (+{} ~{} -{})",
        report.snapshot_id,
        report.files,
        diff.added.len(),
        diff.modified.len(),
        diff.removed.len()
    );
    println!(
        "  hashed {}, reused {}; skipped: {} large, {} binary, {} minified, {} secret, {} symlink; {} errors",
        walk.hashed,
        walk.reused,
        walk.skipped_large,
        walk.skipped_binary,
        walk.skipped_minified,
        walk.skipped_secret,
        walk.skipped_symlink,
        walk.errors
    );
    match report.new_segment {
        Some((id, n)) => println!("  new segment {:016x}: {n} blobs indexed", id.0),
        None if report.unchanged => println!("  up to date, nothing written"),
        None => println!("  no new blobs, snapshot updated"),
    }
    if report.skipped_blobs > 0 {
        println!(
            "  {} blob(s) changed while indexing; they will be picked up on the next run",
            report.skipped_blobs
        );
    }
    println!("  {:.1?}", started.elapsed());
    Ok(())
}

pub fn status(config: &Config) -> anyhow::Result<()> {
    let index = Index::open_reader(config.clone())
        .with_context(|| format!("opening index at {}", config.index_dir.display()))?;
    let store = index.store();
    let snapshot = store.load_snapshot()?;
    println!("index:    {}", config.index_dir.display());
    println!(
        "snapshot: {:016x} ({} files)",
        store.snapshot_id(),
        snapshot.len()
    );
    println!("segments: {}", store.segment_ids().len());
    let bad = store.verify();
    if bad.is_empty() {
        println!("verify:   ok");
        Ok(())
    } else {
        for (id, err) in &bad {
            println!("verify:   segment {:016x}: {err}", id.0);
        }
        anyhow::bail!("{} corrupt segment(s)", bad.len())
    }
}

pub struct SearchArgs {
    pub pattern: String,
    pub word: bool,
    pub opts: SearchOptions,
    pub explain: bool,
    pub files_only: bool,
    pub no_daemon: bool,
}

/// A running daemon for this index, unless the user opted out.
fn daemon_client(config: &Config, no_daemon: bool) -> Option<Client> {
    if no_daemon {
        return None;
    }
    Client::connect(&config.index_dir).ok()
}

pub fn search(config: &Config, args: SearchArgs) -> anyhow::Result<()> {
    let started = Instant::now();
    let (result, via, file_count) = match daemon_client(config, args.no_daemon) {
        Some(mut client) => {
            let response = client.call(&Request::Search {
                pattern: args.pattern.clone(),
                word: args.word,
                opts: args.opts.clone(),
            })?;
            let Response::Search(result) = response else {
                anyhow::bail!("unexpected daemon response: {response:?}");
            };
            let files = match client.call(&Request::Status)? {
                Response::Status(s) => s.files,
                _ => 0,
            };
            (result, "daemon", files)
        }
        None => {
            let index = Index::open_reader(config.clone())
                .with_context(|| format!("opening index at {}", config.index_dir.display()))?;
            if index.snapshot_id() == 0 {
                anyhow::bail!("nothing indexed yet; run `loupe index` first");
            }
            let searcher = index.searcher()?;
            let result = if args.word {
                searcher.search_word(&args.pattern, &args.opts)?
            } else {
                searcher.search(&args.pattern, &args.opts)?
            };
            let files = searcher.file_count();
            (result, "direct", files)
        }
    };
    let elapsed = started.elapsed();

    print_result(&result, &args);
    let s = &result.stats;
    println!(
        "\n{} file(s), {} match(es) in {:.1?}  [snapshot {:016x}, {via}{}{}{}{}]",
        result.total_files,
        result.total_matches,
        elapsed,
        result.snapshot_id,
        if s.truncated { ", truncated" } else { "" },
        if s.timed_out { ", TIMED OUT" } else { "" },
        if s.stale_files > 0 {
            ", STALE files present"
        } else {
            ""
        },
        if s.overlay_files > 0 {
            ", overlay in use"
        } else {
            ""
        },
    );
    if args.explain {
        println!(
            "plan: {}\n  candidates {} of {} files{}; scanned {}; unreadable {}; unindexed {}; overlay {}",
            s.plan,
            s.candidates,
            file_count,
            if s.full_scan { " (full scan)" } else { "" },
            s.scanned,
            s.unreadable,
            s.unindexed_files,
            s.overlay_files
        );
    }
    Ok(())
}

fn print_result(result: &SearchResult, args: &SearchArgs) {
    for hit in &result.hits {
        let stale = if hit.stale { "  [stale]" } else { "" };
        if args.files_only {
            println!("{}{stale}", hit.path);
            continue;
        }
        println!(
            "{} ({} match{}){stale}",
            hit.path,
            hit.match_count,
            if hit.match_count == 1 { "" } else { "es" }
        );
        for snippet in &hit.snippets {
            for (i, line) in snippet.text.lines().enumerate() {
                println!("  {:>5}: {line}", snippet.start_line + i);
            }
            println!("  ·····");
        }
    }
}

pub fn find(
    config: &Config,
    query: &str,
    tokens: bool,
    limit: usize,
    scores: bool,
    no_daemon: bool,
) -> anyhow::Result<()> {
    let hits = match daemon_client(config, no_daemon) {
        Some(mut client) => {
            let request = if tokens {
                Request::FindTokens {
                    query: query.to_string(),
                    limit,
                }
            } else {
                Request::FindFiles {
                    query: query.to_string(),
                    limit,
                }
            };
            match client.call(&request)? {
                Response::Fuzzy(h) => h,
                other => anyhow::bail!("unexpected daemon response: {other:?}"),
            }
        }
        None => {
            let index = Index::open_reader(config.clone())
                .with_context(|| format!("opening index at {}", config.index_dir.display()))?;
            if index.snapshot_id() == 0 {
                anyhow::bail!("nothing indexed yet; run `loupe index` first");
            }
            let searcher = index.searcher()?;
            if tokens {
                searcher.find_tokens(query, limit)?
            } else {
                searcher.find_files(query, limit)
            }
        }
    };
    for h in hits {
        if scores {
            println!("{:>5}  {}", h.score, h.text);
        } else {
            println!("{}", h.text);
        }
    }
    Ok(())
}

pub fn daemon_run(config: &Config, opts: loupe_daemon::DaemonOptions) -> anyhow::Result<()> {
    let handle = loupe_daemon::start(config.clone(), opts)
        .with_context(|| format!("starting daemon for {}", config.index_dir.display()))?;
    println!(
        "loupe daemon serving {} on {} (pid {})",
        config.root.display(),
        handle.socket().display(),
        std::process::id()
    );
    handle.wait();
    println!("daemon stopped");
    Ok(())
}

pub fn daemon_stop(config: &Config) -> anyhow::Result<()> {
    let mut client =
        Client::connect(&config.index_dir).context("no daemon is running for this index")?;
    client.call(&Request::Shutdown)?;
    println!("daemon stopping");
    Ok(())
}

pub fn daemon_status(config: &Config) -> anyhow::Result<()> {
    let mut client =
        Client::connect(&config.index_dir).context("no daemon is running for this index")?;
    let Response::Status(s) = client.call(&Request::Status)? else {
        anyhow::bail!("unexpected daemon response");
    };
    println!("daemon pid {}", s.pid);
    println!(
        "snapshot: {:016x} ({} files, {} segments)",
        s.snapshot_id, s.files, s.segments
    );
    println!(
        "overlay:  {} files, {} bytes",
        s.overlay_files, s.overlay_bytes
    );
    println!("updates:  {}", s.updates);
    Ok(())
}

pub fn compact(config: &Config) -> anyhow::Result<()> {
    if let Ok(mut client) = Client::connect(&config.index_dir) {
        let Response::Compacted(c) = client.call(&Request::Compact)? else {
            anyhow::bail!("unexpected daemon response");
        };
        println!(
            "daemon compacted: merged {} segments into {}, dropped {} dead blobs, removed {} files",
            c.merged, c.written, c.blobs_dropped_dead, c.segments_removed
        );
        return Ok(());
    }
    let mut index = Index::open_writer(config.clone())
        .with_context(|| format!("opening index at {}", config.index_dir.display()))?;
    let before = index.store().segment_ids().len();
    let r = index.compact(&loupe_api::CompactPolicy::default())?;
    let g = index.gc()?;
    println!(
        "segments {} -> {}: merged {}, wrote {}, dropped {} dead blobs ({} kept, {} skipped as changed)",
        before,
        index.store().segment_ids().len(),
        r.merged,
        r.written,
        r.blobs_dropped_dead,
        r.blobs_kept,
        r.blobs_skipped_changed
    );
    println!(
        "gc: removed {} segments now, {} still inside the retire grace (run `loupe gc` later)",
        g.segments_removed, g.segments_retained
    );
    Ok(())
}

pub fn gc(config: &Config) -> anyhow::Result<()> {
    let index = Index::open_writer(config.clone())
        .with_context(|| format!("opening index at {}", config.index_dir.display()))?;
    let g = index.gc()?;
    println!(
        "removed {} segments, {} temp files, {} old snapshots; {} segments retained by grace",
        g.segments_removed, g.tmp_removed, g.snapshots_removed, g.segments_retained
    );
    Ok(())
}

pub fn daemon_metrics(config: &Config) -> anyhow::Result<()> {
    let mut client =
        Client::connect(&config.index_dir).context("no daemon is running for this index")?;
    let Response::Metrics(m) = client.call(&Request::Metrics)? else {
        anyhow::bail!("unexpected daemon response");
    };
    println!(
        "uptime {}s, {} updates, {} compactions, {} segments, index {:.1} MiB, overlay {} files / {} bytes",
        m.uptime_secs,
        m.updates,
        m.compactions,
        m.segments,
        m.index_bytes as f64 / (1024.0 * 1024.0),
        m.overlay_files,
        m.overlay_bytes
    );
    println!(
        "{:<14} {:>8} {:>6} {:>9} {:>9} {:>9} {:>9}",
        "request", "count", "errs", "p50 us", "p95 us", "p99 us", "max us"
    );
    for r in &m.requests {
        println!(
            "{:<14} {:>8} {:>6} {:>9} {:>9} {:>9} {:>9}",
            r.kind, r.count, r.errors, r.p50_us, r.p95_us, r.p99_us, r.max_us
        );
    }
    Ok(())
}

pub fn symbol(
    config: &Config,
    name: &str,
    kind: Option<&str>,
    limit: usize,
    no_daemon: bool,
) -> anyhow::Result<()> {
    let kind = kind
        .map(|k| SymbolKind::parse(k).with_context(|| format!("unknown kind {k:?}")))
        .transpose()?;
    let hits = match daemon_client(config, no_daemon) {
        Some(mut client) => match client.call(&Request::FindSymbol {
            name: name.to_string(),
            kind,
            limit,
        })? {
            Response::Symbols(h) => h,
            other => anyhow::bail!("unexpected daemon response: {other:?}"),
        },
        None => open_indexed(config)?.find_symbol(name, kind, limit)?,
    };
    if hits.is_empty() {
        println!("no definitions named {name:?}");
    }
    for h in hits {
        println!("{}:{}  {}", h.path, h.start_line, h.signature);
        if let Some(parent) = &h.parent {
            println!("    in {parent}");
        }
        if let Some(doc) = h.doc.as_deref().and_then(|d| d.lines().next()) {
            println!("    // {doc}");
        }
    }
    Ok(())
}

pub fn outline(config: &Config, path: &str, plain: bool, no_daemon: bool) -> anyhow::Result<()> {
    let outline = match daemon_client(config, no_daemon) {
        Some(mut client) => match client.call(&Request::Outline {
            path: path.to_string(),
        })? {
            Response::Outline(o) => o,
            other => anyhow::bail!("unexpected daemon response: {other:?}"),
        },
        None => open_indexed(config)?.outline(path)?,
    };
    let Some(outline) = outline else {
        anyhow::bail!("no such file in the index: {path}");
    };
    println!(
        "{}  ({})",
        outline.path,
        outline.lang.as_deref().unwrap_or("unknown language")
    );
    if outline.symbols.is_empty() {
        println!("  (no symbols found)");
    }
    let opts = OutlineOptions {
        docs: !plain,
        lines: !plain,
    };
    print!("{}", render_outline(&outline.symbols, &opts));
    Ok(())
}

fn open_indexed(config: &Config) -> anyhow::Result<Index> {
    let index = Index::open_reader(config.clone())
        .with_context(|| format!("opening index at {}", config.index_dir.display()))?;
    if index.snapshot_id() == 0 {
        anyhow::bail!("nothing indexed yet; run `loupe index` first");
    }
    Ok(index)
}

fn parse_kinds(kinds: &[String]) -> anyhow::Result<Vec<EdgeKind>> {
    kinds
        .iter()
        .map(|k| EdgeKind::parse(k).with_context(|| format!("unknown edge kind {k:?}")))
        .collect()
}

fn via(kinds: &[EdgeKind]) -> String {
    kinds
        .iter()
        .map(|k| k.as_str())
        .collect::<Vec<_>>()
        .join(" > ")
}

pub fn related(
    config: &Config,
    path: &str,
    kinds: &[String],
    hops: u8,
    limit: usize,
    no_daemon: bool,
) -> anyhow::Result<()> {
    let kinds = parse_kinds(kinds)?;
    let started = Instant::now();
    let related = match daemon_client(config, no_daemon) {
        Some(mut client) => match client.call(&Request::Related {
            path: path.to_string(),
            kinds,
            hops,
            limit,
        })? {
            Response::Related(r) => r,
            other => anyhow::bail!("unexpected daemon response: {other:?}"),
        },
        None => {
            let index = open_indexed(config)?;
            let kinds = if kinds.is_empty() {
                EdgeKind::ALL.to_vec()
            } else {
                kinds
            };
            index
                .graph(&GraphOptions::default())?
                .related(path, &RelatedOptions { kinds, hops, limit })
        }
    };
    if related.is_empty() {
        println!("nothing related to {path:?} (unknown file, or no edges)");
    }
    for r in &related {
        println!("{:>5.2}  {}  [{}]", r.score, r.path, via(&r.via));
    }
    println!("\n{} file(s) in {:.1?}", related.len(), started.elapsed());
    Ok(())
}

pub fn impact(
    config: &Config,
    path: &str,
    depth: u8,
    limit: usize,
    no_daemon: bool,
) -> anyhow::Result<()> {
    let impacted = match daemon_client(config, no_daemon) {
        Some(mut client) => match client.call(&Request::Impact {
            path: path.to_string(),
            depth,
            limit,
        })? {
            Response::Impact(i) => i,
            other => anyhow::bail!("unexpected daemon response: {other:?}"),
        },
        None => open_indexed(config)?
            .graph(&GraphOptions::default())?
            .impact(path, depth, limit),
    };
    if impacted.is_empty() {
        println!("nothing depends on {path:?}");
    }
    for i in &impacted {
        println!("depth {}  {}  ({})", i.depth, i.path, i.kind.as_str());
    }
    Ok(())
}

pub fn rank(config: &Config, limit: usize, no_daemon: bool) -> anyhow::Result<()> {
    let ranked: Vec<(String, f32)> = match daemon_client(config, no_daemon) {
        Some(mut client) => match client.call(&Request::TopRanked { limit })? {
            Response::Ranked(r) => r,
            other => anyhow::bail!("unexpected daemon response: {other:?}"),
        },
        None => open_indexed(config)?
            .graph(&GraphOptions::default())?
            .top_ranked(limit)
            .into_iter()
            .map(|(p, r)| (p.to_string(), r))
            .collect(),
    };
    for (path, r) in ranked {
        println!("{:>8.5}  {path}", r);
    }
    Ok(())
}

fn repo_name(config: &Config) -> String {
    config
        .root
        .file_name()
        .map_or_else(|| "repo".to_string(), |n| n.to_string_lossy().into_owned())
}

pub fn repo_map(
    config: &Config,
    budget: usize,
    focus: Vec<String>,
    no_daemon: bool,
) -> anyhow::Result<()> {
    let name = repo_name(config);
    let map = match daemon_client(config, no_daemon) {
        Some(mut client) => match client.call(&Request::RepoMap {
            budget,
            focus,
            name,
        })? {
            Response::Map(m) => m,
            other => anyhow::bail!("unexpected daemon response: {other:?}"),
        },
        None => open_indexed(config)?.repo_map(&MapOptions {
            budget,
            focus,
            name,
        })?,
    };
    print!("{}", map.text);
    eprintln!(
        "[{} tokens; {}/{} files and {}/{} dirs shown]",
        map.tokens, map.shown_files, map.total_files, map.shown_dirs, map.total_dirs
    );
    Ok(())
}

pub fn module(config: &Config, path: &str, no_daemon: bool) -> anyhow::Result<()> {
    let m = match daemon_client(config, no_daemon) {
        Some(mut client) => match client.call(&Request::ModuleSummary {
            path: path.to_string(),
        })? {
            Response::Module(m) => m,
            other => anyhow::bail!("unexpected daemon response: {other:?}"),
        },
        None => open_indexed(config)?.module_summary(path)?,
    };
    println!("{}/  ({} files)", m.path, m.files);
    println!("{}", m.text);
    if !m.depends_on.is_empty() {
        println!("Depends on: {}", m.depends_on.join(", "));
    }
    Ok(())
}

pub fn sem(
    config: &Config,
    query: &str,
    limit: usize,
    scope: Option<String>,
    per_file: usize,
) -> anyhow::Result<()> {
    let index = open_indexed(config)?;
    let searcher = index.semantic_searcher()?;
    let started = Instant::now();
    let r = searcher.search(
        query,
        &SemanticOptions {
            k: limit,
            scope,
            per_file,
            ..SemanticOptions::default()
        },
        None,
    )?;
    let elapsed = started.elapsed();
    for h in &r.hits {
        println!(
            "{:>5.2}  {}:{}-{}  {}",
            h.score, h.path, h.start_line, h.end_line, h.breadcrumb
        );
        for line in h.snippet.lines().take(3) {
            println!("         {line}");
        }
    }
    let state = match r.state {
        SemanticState::Full => "",
        SemanticState::Partial => ", PARTIAL: some segments lack embeddings",
        SemanticState::Off => ", OFF: no embeddings for the current model; run `loupe index`",
    };
    println!(
        "\n{} hit(s) over {} chunks in {:.1?}  [snapshot {:016x}{state}]",
        r.hits.len(),
        r.chunks_scanned,
        elapsed,
        r.snapshot_id
    );
    Ok(())
}

pub fn query(
    config: &Config,
    question: &str,
    budget: usize,
    max_files: usize,
    graph: bool,
) -> anyhow::Result<()> {
    let index = open_indexed(config)?;
    let started = Instant::now();
    let session = index.session(&GraphOptions::default())?;
    let built = started.elapsed();
    let opts = loupe_api::QueryOptions {
        budget_tokens: budget,
        max_files,
        use_graph: graph,
        ..Default::default()
    };
    let t = Instant::now();
    let r = session.query(question, &opts, None)?;
    print!("{}", loupe_api::render_query(&r, question));
    let stages: Vec<String> = r
        .timings_ms
        .iter()
        .map(|(n, ms)| format!("{n} {ms}ms"))
        .collect();
    eprintln!(
        "[query {:.1?} ({}); searchers+graph built in {built:.1?}]",
        t.elapsed(),
        stages.join(", ")
    );
    Ok(())
}
