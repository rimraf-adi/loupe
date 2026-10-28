mod index;

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use loupe_api::SearchOptions;
use loupe_core::Config;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "loupe", version, about = "Fast queryable codebase index")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct RepoArgs {
    /// Repository root.
    #[arg(long, short = 'C', default_value = ".")]
    root: PathBuf,
    /// Index directory (default: <root>/.loupe).
    #[arg(long)]
    index_dir: Option<PathBuf>,
    /// Also index files that look like secrets (.env, private keys, ...).
    #[arg(long)]
    index_secrets: bool,
    /// Source megabytes indexed per segment; bounds peak memory while indexing (default 64).
    #[arg(long)]
    max_segment_mb: Option<u64>,
}

impl RepoArgs {
    fn config(&self) -> anyhow::Result<Config> {
        let mut config = Config::new(self.root.canonicalize()?);
        if let Some(dir) = &self.index_dir {
            config.index_dir = dir.clone();
        }
        config.index_secrets = self.index_secrets;
        if let Some(mb) = self.max_segment_mb {
            config.max_segment_bytes = mb.max(1) * 1024 * 1024;
        }
        Ok(config)
    }
}

#[derive(Subcommand)]
enum Command {
    /// Print the resolved configuration for a repo root.
    Config(RepoArgs),
    /// Walk the repo and update the index. A rerun on an unchanged repo reads nothing.
    Index {
        #[command(flatten)]
        repo: RepoArgs,
        /// Re-hash every file instead of trusting (size, mtime).
        #[arg(long)]
        force_rehash: bool,
    },
    /// Show the current snapshot and verify every live segment's checksums.
    Status(RepoArgs),
    /// Merge fragmented segments and drop dead data, then collect retired segments. Asks a
    /// running daemon to do it when there is one (it holds the writer lock).
    Compact(RepoArgs),
    /// Delete leftover temp files and unreferenced segments (retired ones after their grace).
    Gc(RepoArgs),
    /// Fuzzy-find file paths (or identifiers with --tokens), fzf-style. Plain output, one per
    /// line, so it pipes into fzf: `loupe find --all | fzf`.
    Find {
        /// Fuzzy query (omit with --all).
        #[arg(default_value = "")]
        query: String,
        #[command(flatten)]
        repo: RepoArgs,
        /// Match indexed identifiers instead of file paths.
        #[arg(long)]
        tokens: bool,
        /// List everything (empty query), unlimited.
        #[arg(long)]
        all: bool,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        /// Show scores.
        #[arg(long)]
        scores: bool,
        /// Ignore a running daemon and read the on-disk index directly.
        #[arg(long)]
        no_daemon: bool,
    },
    /// Ask a question in plain words: keyword + symbol + semantic retrieval, fused and packed
    /// into a token budget.
    Query {
        question: String,
        #[command(flatten)]
        repo: RepoArgs,
        #[arg(short = 'b', long, default_value_t = 1500)]
        budget: usize,
        #[arg(short = 'n', long, default_value_t = 12)]
        max_files: usize,
        /// Also pull in files connected to the best hits (imports, tests, references, co-change).
        #[arg(long)]
        graph: bool,
    },
    /// Serve the index as MCP tools over stdio (for any MCP-capable agent). Reads the on-disk
    /// index and follows new commits; run `loupe daemon run` or `loupe index` to keep it fresh.
    Mcp(RepoArgs),
    /// Natural-language search over definition-level chunks (embedding similarity).
    Sem {
        query: String,
        #[command(flatten)]
        repo: RepoArgs,
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
        /// Only files under this path prefix.
        #[arg(long)]
        scope: Option<String>,
        /// Chunks per file at most.
        #[arg(long, default_value_t = 2)]
        per_file: usize,
    },
    /// Find definitions by exact name (functions, structs, classes, ...).
    Symbol {
        name: String,
        #[command(flatten)]
        repo: RepoArgs,
        /// Only this kind: function, method, struct, class, interface, trait, enum, impl,
        /// module, constant, variable, type, macro.
        #[arg(short = 'k', long)]
        kind: Option<String>,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        /// Ignore a running daemon and read the on-disk index directly.
        #[arg(long)]
        no_daemon: bool,
    },
    /// A token-budgeted map of the repo: structure, entry points, and the most important files.
    Map {
        #[command(flatten)]
        repo: RepoArgs,
        /// Token budget for the whole map.
        #[arg(short = 'b', long, default_value_t = 2000)]
        budget: usize,
        /// Files, directories, or substrings to bring into focus (repeatable).
        #[arg(short = 'f', long)]
        focus: Vec<String>,
        #[arg(long)]
        no_daemon: bool,
    },
    /// One to three lines about a directory (what it is for, key files, what it depends on).
    Module {
        /// Directory path; empty for the repo root.
        #[arg(default_value = "")]
        path: String,
        #[command(flatten)]
        repo: RepoArgs,
        #[arg(long)]
        no_daemon: bool,
    },
    /// Files most connected to a file: imports, tests, name references, git co-change.
    Related {
        path: String,
        #[command(flatten)]
        repo: RepoArgs,
        /// Edge kinds to follow (comma-separated): imports, refs, tests, cochange. Default: all.
        #[arg(long, value_delimiter = ',')]
        kinds: Vec<String>,
        #[arg(long, default_value_t = 2)]
        hops: u8,
        #[arg(short = 'n', long, default_value_t = 15)]
        limit: usize,
        /// Ignore a running daemon and build the graph from the on-disk index.
        #[arg(long)]
        no_daemon: bool,
    },
    /// Files that depend on a file (reverse imports/references/tests, transitively).
    Impact {
        path: String,
        #[command(flatten)]
        repo: RepoArgs,
        #[arg(long, default_value_t = 3)]
        depth: u8,
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        no_daemon: bool,
    },
    /// The most important files by PageRank over the graph.
    Rank {
        #[command(flatten)]
        repo: RepoArgs,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        no_daemon: bool,
    },
    /// Print a file's signature outline (no bodies).
    Outline {
        path: String,
        #[command(flatten)]
        repo: RepoArgs,
        /// Omit line ranges and doc lines.
        #[arg(long)]
        plain: bool,
        /// Ignore a running daemon and read the on-disk index directly.
        #[arg(long)]
        no_daemon: bool,
    },
    /// Run or control the background daemon (warm index + file watcher).
    Daemon {
        #[command(subcommand)]
        action: DaemonCmd,
    },
    /// Search the indexed repo (regex by default).
    Search {
        /// Regex (or literal with -F, or identifier with -w).
        pattern: String,
        #[command(flatten)]
        repo: RepoArgs,
        /// Treat the pattern as a literal string.
        #[arg(short = 'F', long)]
        fixed: bool,
        /// Match a whole identifier, using the token index.
        #[arg(short = 'w', long, conflicts_with = "fixed")]
        word: bool,
        /// Case-insensitive.
        #[arg(short = 'i', long)]
        ignore_case: bool,
        /// Lines of context around each match.
        #[arg(short = 'c', long, default_value_t = 1)]
        context: usize,
        /// Maximum files to show.
        #[arg(short = 'n', long, default_value_t = 20)]
        max_files: usize,
        /// Maximum snippets per file.
        #[arg(long, default_value_t = 3)]
        max_per_file: usize,
        /// Print only matching paths.
        #[arg(short = 'l', long)]
        files_only: bool,
        /// Give up verifying after this many seconds (0 = no limit).
        #[arg(long, default_value_t = 10)]
        timeout: u64,
        /// Show the trigram plan and candidate counts.
        #[arg(long)]
        explain: bool,
        /// Ignore a running daemon and search the on-disk index directly.
        #[arg(long)]
        no_daemon: bool,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Run in the foreground until stopped. Takes the writer lock and keeps the index fresh.
    Run {
        #[command(flatten)]
        repo: RepoArgs,
        /// Do not watch the filesystem (only requests and the reconcile timer update the index).
        #[arg(long)]
        no_watch: bool,
        /// Seconds between safety re-scans.
        #[arg(long, default_value_t = 30)]
        reconcile_secs: u64,
    },
    /// Ask a running daemon to stop.
    Stop(RepoArgs),
    /// Show a running daemon's state.
    Status(RepoArgs),
    /// Show a running daemon's request counts and latency percentiles.
    Metrics(RepoArgs),
}

fn init_tracing() {
    let filter = EnvFilter::try_from_env("LOUPE_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

fn main() -> anyhow::Result<()> {
    init_tracing();
    match Cli::parse().command {
        Command::Config(repo) => {
            let config = repo.config()?;
            tracing::debug!(?config, "resolved config");
            println!("{config:#?}");
        }
        Command::Index { repo, force_rehash } => index::index(&repo.config()?, force_rehash)?,
        Command::Status(repo) => index::status(&repo.config()?)?,
        Command::Compact(repo) => index::compact(&repo.config()?)?,
        Command::Gc(repo) => index::gc(&repo.config()?)?,
        Command::Find {
            query,
            repo,
            tokens,
            all,
            limit,
            scores,
            no_daemon,
        } => index::find(
            &repo.config()?,
            &query,
            tokens,
            if all { usize::MAX } else { limit },
            scores,
            no_daemon,
        )?,
        Command::Query {
            question,
            repo,
            budget,
            max_files,
            graph,
        } => index::query(&repo.config()?, &question, budget, max_files, graph)?,
        Command::Mcp(repo) => {
            let tools = loupe_mcp::Tools::open(repo.config()?)?;
            let stdin = std::io::stdin();
            loupe_mcp::serve(
                loupe_mcp::McpServer::new(tools),
                stdin.lock(),
                std::io::stdout(),
            )?;
        }
        Command::Sem {
            query,
            repo,
            limit,
            scope,
            per_file,
        } => index::sem(&repo.config()?, &query, limit, scope, per_file)?,
        Command::Symbol {
            name,
            repo,
            kind,
            limit,
            no_daemon,
        } => index::symbol(&repo.config()?, &name, kind.as_deref(), limit, no_daemon)?,
        Command::Outline {
            path,
            repo,
            plain,
            no_daemon,
        } => index::outline(&repo.config()?, &path, plain, no_daemon)?,
        Command::Map {
            repo,
            budget,
            focus,
            no_daemon,
        } => index::repo_map(&repo.config()?, budget, focus, no_daemon)?,
        Command::Module {
            path,
            repo,
            no_daemon,
        } => index::module(&repo.config()?, &path, no_daemon)?,
        Command::Related {
            path,
            repo,
            kinds,
            hops,
            limit,
            no_daemon,
        } => index::related(&repo.config()?, &path, &kinds, hops, limit, no_daemon)?,
        Command::Impact {
            path,
            repo,
            depth,
            limit,
            no_daemon,
        } => index::impact(&repo.config()?, &path, depth, limit, no_daemon)?,
        Command::Rank {
            repo,
            limit,
            no_daemon,
        } => index::rank(&repo.config()?, limit, no_daemon)?,
        Command::Daemon { action } => match action {
            DaemonCmd::Run {
                repo,
                no_watch,
                reconcile_secs,
            } => index::daemon_run(
                &repo.config()?,
                loupe_daemon::DaemonOptions {
                    watch: !no_watch,
                    reconcile_every: Duration::from_secs(reconcile_secs.max(1)),
                    ..Default::default()
                },
            )?,
            DaemonCmd::Stop(repo) => index::daemon_stop(&repo.config()?)?,
            DaemonCmd::Status(repo) => index::daemon_status(&repo.config()?)?,
            DaemonCmd::Metrics(repo) => index::daemon_metrics(&repo.config()?)?,
        },
        Command::Search {
            pattern,
            repo,
            fixed,
            word,
            ignore_case,
            context,
            max_files,
            max_per_file,
            files_only,
            timeout,
            explain,
            no_daemon,
        } => {
            let opts = SearchOptions {
                case_insensitive: ignore_case,
                literal: fixed,
                max_files,
                max_matches_per_file: max_per_file,
                context_lines: context,
                timeout: (timeout > 0).then(|| Duration::from_secs(timeout)),
            };
            index::search(
                &repo.config()?,
                index::SearchArgs {
                    pattern,
                    word,
                    opts,
                    explain,
                    files_only,
                    no_daemon,
                },
            )?
        }
    }
    Ok(())
}
