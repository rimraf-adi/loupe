//! `eval mine <repo> --out tasks.json` and `eval run <repo> tasks.json --index-dir DIR`.

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use loupe_api::Index;
use loupe_bench::eval::{
    Arm, ArmReport, QueryArm, RipgrepArm, SemanticArm, Task, TextIndexArm, doc_tasks, evaluate,
};
use loupe_bench::mine::{MineOptions, mine};
use loupe_core::Config;

#[derive(Parser)]
#[command(about = "Task-level localization eval")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Mine tasks (commit subject -> changed files) from a repo's git history.
    Mine {
        repo: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 200)]
        max_tasks: usize,
    },
    /// Doc-to-definition eval: queries are doc-comment sentences, truth is the defining file.
    /// Builds a throwaway index whose chunks carry no doc text (so queries do not leak answers)
    /// and compares the lexical keyword arm with the semantic arm.
    Semantic {
        repo: PathBuf,
        /// Index directory (created; must not be in use).
        #[arg(long)]
        index_dir: PathBuf,
        #[arg(long, default_value_t = 300)]
        limit: usize,
    },
    /// Run every available arm over a task file and print recall, tokens, and latency.
    Run {
        repo: PathBuf,
        tasks: PathBuf,
        /// Where the index lives (created/updated first).
        #[arg(long)]
        index_dir: PathBuf,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let embedder: std::sync::Arc<dyn loupe_api::Embedder> =
        std::sync::Arc::new(loupe_api::HashingEmbedder::default());
    loupe_api::set_default_embedder(embedder.clone());
    match cli.command {
        Cmd::Mine {
            repo,
            out,
            max_tasks,
        } => {
            let tasks = mine(
                &repo,
                &MineOptions {
                    max_tasks,
                    ..MineOptions::default()
                },
            )?;
            std::fs::write(&out, serde_json::to_vec_pretty(&tasks)?)?;
            println!("mined {} tasks -> {}", tasks.len(), out.display());
        }
        Cmd::Semantic {
            repo,
            index_dir,
            limit,
        } => {
            let root = repo.canonicalize()?;
            let mut config = Config::new(&root);
            config.index_dir = index_dir.clone();
            let chunking = loupe_api::ChunkOptions {
                include_docs: false,
                ..Default::default()
            };
            let mut index = Index::open_writer(config.clone())?
                .with_embedding(embedder.clone(), chunking.clone());
            let started = std::time::Instant::now();
            let rep = index.update(false)?;
            eprintln!(
                "indexed {} files, embedded {} chunks in {:.1?}",
                rep.files,
                rep.chunks_embedded,
                started.elapsed()
            );
            let tasks = doc_tasks(&index, limit)?;
            drop(index);
            let reader = |_: ()| -> anyhow::Result<Index> {
                Ok(Index::open_reader(config.clone())?
                    .with_embedding(embedder.clone(), chunking.clone()))
            };
            println!(
                "{} doc-to-definition tasks on {}",
                tasks.len(),
                root.display()
            );
            let arms: Vec<Box<dyn Arm>> = vec![
                Box::new(TextIndexArm { index: reader(())? }),
                Box::new(SemanticArm { index: reader(())? }),
            ];
            for arm in &arms {
                print_report(&evaluate(arm.as_ref(), &tasks, &[1, 5, 10, 20])?);
            }
        }
        Cmd::Run {
            repo,
            tasks,
            index_dir,
            limit,
        } => {
            let mut tasks: Vec<Task> =
                serde_json::from_slice(&std::fs::read(&tasks).context("reading tasks")?)?;
            tasks.truncate(limit);
            let root = repo.canonicalize()?;
            let mut config = Config::new(&root);
            config.index_dir = index_dir;
            {
                let mut index = Index::open_writer(config.clone())?;
                index.update(false)?;
            }

            let mut arms: Vec<Box<dyn Arm>> = vec![
                Box::new(TextIndexArm {
                    index: Index::open_reader(config.clone())?,
                }),
                Box::new(SemanticArm {
                    index: Index::open_reader(config.clone())?,
                }),
            ];
            // The hybrid pipeline and its ablations, through the same code the `query` tool uses.
            let idx = Index::open_reader(config)?;
            let full = loupe_api::QueryOptions::default();
            let base = |f: &dyn Fn(&mut loupe_api::QueryOptions)| {
                let mut o = full.clone();
                o.semantic_weight = 0.2;
                o.graph_weight = 0.3;
                f(&mut o);
                o
            };
            let variants = [
                ("sem0.2+gr0.3", base(&|_| {})),
                ("rrf30", base(&|o| o.rrf_k = 30.0)),
                ("rrf15", base(&|o| o.rrf_k = 15.0)),
                ("rrf8", base(&|o| o.rrf_k = 8.0)),
                (
                    "rrf8+sem0.4",
                    base(&|o| {
                        o.rrf_k = 8.0;
                        o.semantic_weight = 0.4;
                    }),
                ),
                (
                    "rrf8+nograph",
                    base(&|o| {
                        o.rrf_k = 8.0;
                        o.use_graph = false;
                    }),
                ),
                (
                    "rrf15+nograph",
                    base(&|o| {
                        o.rrf_k = 15.0;
                        o.use_graph = false;
                    }),
                ),
                (
                    "rrf8+gr0.6",
                    base(&|o| {
                        o.rrf_k = 8.0;
                        o.graph_weight = 0.6;
                    }),
                ),
                (
                    "text+symbols",
                    base(&|o| {
                        o.use_semantic = false;
                        o.use_graph = false;
                        o.use_priors = false;
                    }),
                ),
            ];
            for (label, opts) in variants {
                arms.push(Box::new(QueryArm::new(label, &idx, opts)?));
            }
            if RipgrepArm::available() {
                arms.insert(0, Box::new(RipgrepArm { root }));
            } else {
                eprintln!("ripgrep not found; skipping that arm");
            }
            let ks = [5, 10, 20];
            println!("{} tasks", tasks.len());
            for arm in &arms {
                print_report(&evaluate(arm.as_ref(), &tasks, &ks)?);
            }
        }
    }
    Ok(())
}

fn print_report(r: &ArmReport) {
    let recall: Vec<String> = r
        .recall
        .iter()
        .map(|(k, v)| format!("R@{k} {v:.3}"))
        .collect();
    println!(
        "{:<12} {}  tokens {:.0}  p50 {:.1?}  p95 {:.1?}",
        r.name,
        recall.join("  "),
        r.mean_tokens,
        r.p50,
        r.p95
    );
}
