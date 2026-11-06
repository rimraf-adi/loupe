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
    /// `hashing` (default) or a fetched model id (needs `--features real-embedder`).
    #[arg(long, global = true, default_value = "hashing")]
    embedder: String,
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
    let embedder = pick_embedder(&cli.embedder)?;
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
            // Fusion sweep: RRF constant x semantic weight, graph off (it is opt-in).
            let mut variants: Vec<(String, loupe_api::QueryOptions)> = vec![
                ("defaults".to_string(), full.clone()),
                (
                    "text+symbols".to_string(),
                    base(&|o| {
                        o.use_semantic = false;
                        o.use_graph = false;
                        o.use_priors = false;
                    }),
                ),
            ];
            for rrf in [8.0f32, 15.0, 30.0, 60.0] {
                for sem in [0.2f32, 0.4, 0.7, 1.0, 1.5] {
                    variants.push((
                        format!("g rrf{rrf}/sem{sem}"),
                        base(&|o| {
                            o.rrf_k = rrf;
                            o.semantic_weight = sem;
                            o.use_graph = false;
                        }),
                    ));
                }
            }
            for (label, opts) in variants {
                arms.push(Box::new(QueryArm::new(&label, &idx, opts)?));
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

fn pick_embedder(id: &str) -> anyhow::Result<std::sync::Arc<dyn loupe_api::Embedder>> {
    if id == "hashing" {
        return Ok(std::sync::Arc::new(loupe_api::HashingEmbedder::default()));
    }
    #[cfg(feature = "real-embedder")]
    {
        let spec = loupe_model::lookup(id).ok_or_else(|| anyhow::anyhow!("unknown model {id}"))?;
        let root = loupe_model::models_dir();
        match loupe_model::status(spec, &root) {
            loupe_model::ModelStatus::Ready(dir) => Ok(std::sync::Arc::new(
                loupe_model::BertEmbedder::load(spec, &dir)?,
            )),
            _ => anyhow::bail!("model {id} not fetched; run `loupe model fetch {id}`"),
        }
    }
    #[cfg(not(feature = "real-embedder"))]
    anyhow::bail!("embedder {id} needs --features real-embedder")
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
