use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::Context;
use loupe_api::{Index, QueryOptions, SearchOptions, SemanticOptions, Session, SymbolKind};
use loupe_core::{ApproxTokenCounter, TokenCounter};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub query: String,
    /// Repo-relative paths the change touched (still present at HEAD).
    pub truth: Vec<String>,
}

/// Something that answers "which files matter for this query?" with a ranked list.
pub trait Arm {
    fn name(&self) -> &str;
    fn run(&self, query: &str, k: usize) -> anyhow::Result<Vec<String>>;
}

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "from", "that", "this", "when", "into", "make", "fix", "fixes",
    "fixed", "add", "adds", "added", "use", "uses", "using", "update", "updates", "remove",
    "removes", "support", "instead", "than", "have", "has", "should", "does", "not", "are", "was",
    "now", "also", "some", "more", "bump", "release",
];
const MAX_KEYWORDS: usize = 8;

/// Search keywords from free text: lowercase identifier-ish words of 4+ characters, minus
/// stopwords, deduped in order of appearance, capped.
pub fn keywords(query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in query.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let w = word.to_lowercase();
        if w.chars().count() >= 4 && !STOPWORDS.contains(&w.as_str()) && !out.contains(&w) {
            out.push(w);
        }
        if out.len() == MAX_KEYWORDS {
            break;
        }
    }
    out
}

/// Fraction of `truth` found in the first `k` entries of `ranked`.
pub fn recall_at_k(truth: &[String], ranked: &[String], k: usize) -> f64 {
    if truth.is_empty() {
        return 0.0;
    }
    let top: BTreeSet<&String> = ranked.iter().take(k).collect();
    truth.iter().filter(|t| top.contains(t)).count() as f64 / truth.len() as f64
}

/// Rank files by how many distinct keywords they matched (most first, ties by path).
pub fn rank_by_keyword_coverage(per_keyword: &[BTreeSet<String>]) -> Vec<String> {
    let mut score: BTreeMap<&String, usize> = BTreeMap::new();
    for files in per_keyword {
        for f in files {
            *score.entry(f).or_default() += 1;
        }
    }
    let mut ranked: Vec<(&String, usize)> = score.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    ranked.into_iter().map(|(p, _)| p.clone()).collect()
}

#[derive(Debug)]
pub struct ArmReport {
    pub name: String,
    pub tasks: usize,
    /// `(k, mean recall@k)`.
    pub recall: Vec<(usize, f64)>,
    /// Mean tokens (approximate) of the top-`max(k)` paths, a floor for what an agent would read.
    pub mean_tokens: f64,
    pub p50: Duration,
    pub p95: Duration,
}

pub fn evaluate(arm: &dyn Arm, tasks: &[Task], ks: &[usize]) -> anyhow::Result<ArmReport> {
    let kmax = ks.iter().copied().max().unwrap_or(10);
    let counter = ApproxTokenCounter;
    let mut sums = vec![0.0; ks.len()];
    let mut tokens = 0usize;
    let mut times = Vec::with_capacity(tasks.len());
    for task in tasks {
        let started = Instant::now();
        let ranked = arm
            .run(&task.query, kmax)
            .with_context(|| format!("arm {} on task {}", arm.name(), task.id))?;
        times.push(started.elapsed());
        for (sum, &k) in sums.iter_mut().zip(ks) {
            *sum += recall_at_k(&task.truth, &ranked, k);
        }
        tokens += counter.count(
            &ranked
                .iter()
                .take(kmax)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    times.sort();
    let n = tasks.len().max(1);
    let pct = |p: f64| {
        times
            .get(((times.len() as f64 * p) as usize).min(times.len().saturating_sub(1)))
            .copied()
            .unwrap_or_default()
    };
    Ok(ArmReport {
        name: arm.name().to_string(),
        tasks: tasks.len(),
        recall: ks
            .iter()
            .copied()
            .zip(sums.into_iter().map(|s| s / n as f64))
            .collect(),
        mean_tokens: tokens as f64 / n as f64,
        p50: pct(0.5),
        p95: pct(0.95),
    })
}

/// Baseline: shell out to ripgrep once per keyword.
#[derive(Debug)]
pub struct RipgrepArm {
    pub root: PathBuf,
}

impl RipgrepArm {
    /// Flags matching the indexer's file selection so the arms compare like with like. `--hidden`
    /// alone would also search `.git/` (whose logs contain commit subjects, i.e. the queries),
    /// so it is excluded explicitly, as the indexer does.
    pub const SELECTION_FLAGS: [&'static str; 4] = [
        "--hidden",
        "--glob=!.git",
        "--no-require-git",
        "--max-filesize=1M",
    ];

    pub fn available() -> bool {
        Command::new("rg")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }
}

impl Arm for RipgrepArm {
    fn name(&self) -> &str {
        "ripgrep"
    }

    fn run(&self, query: &str, k: usize) -> anyhow::Result<Vec<String>> {
        let mut per_keyword = Vec::new();
        for kw in keywords(query) {
            let out = Command::new("rg")
                .args(Self::SELECTION_FLAGS)
                .args(["-l", "-i", "-F", "--no-messages", "-e", &kw])
                .arg(&self.root)
                .output()
                .context("running rg")?;
            per_keyword.push(relative_paths(&out.stdout, &self.root));
        }
        Ok(rank_by_keyword_coverage(&per_keyword)
            .into_iter()
            .take(k)
            .collect())
    }
}

fn relative_paths(stdout: &[u8], root: &Path) -> BTreeSet<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|l| Path::new(l).strip_prefix(root).ok())
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect()
}

/// The same keyword-coverage strategy, answered from the loupe text index.
#[derive(Debug)]
pub struct TextIndexArm {
    pub index: Index,
}

impl Arm for TextIndexArm {
    fn name(&self) -> &str {
        "text-index"
    }

    fn run(&self, query: &str, k: usize) -> anyhow::Result<Vec<String>> {
        let searcher = self.index.searcher()?;
        let opts = SearchOptions {
            case_insensitive: true,
            literal: true,
            max_files: usize::MAX,
            max_matches_per_file: 1,
            context_lines: 0,
            timeout: None,
        };
        let mut per_keyword = Vec::new();
        for kw in keywords(query) {
            let r = searcher.search(&kw, &opts)?;
            per_keyword.push(r.hits.into_iter().map(|h| h.path).collect());
        }
        Ok(rank_by_keyword_coverage(&per_keyword)
            .into_iter()
            .take(k)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn keywords_drop_stopwords_short_words_and_dupes() {
        assert_eq!(
            keywords("Fix the parser: handle Parser errors in tokenize_input (#123)"),
            ["parser", "handle", "errors", "tokenize_input"]
        );
        assert!(keywords("fix it").is_empty());
        assert_eq!(
            keywords("alpha beta gamma delta epsilon zeta theta iota kappa lambda").len(),
            MAX_KEYWORDS
        );
    }

    #[test]
    fn recall() {
        let truth = s(&["a", "b", "c", "d"]);
        let ranked = s(&["x", "a", "y", "b", "c"]);
        assert_eq!(recall_at_k(&truth, &ranked, 1), 0.0);
        assert_eq!(recall_at_k(&truth, &ranked, 2), 0.25);
        assert_eq!(recall_at_k(&truth, &ranked, 5), 0.75);
        assert_eq!(recall_at_k(&[], &ranked, 5), 0.0);
    }

    #[test]
    fn coverage_ranking_prefers_files_matching_more_keywords() {
        let set = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<BTreeSet<_>>();
        let ranked =
            rank_by_keyword_coverage(&[set(&["a", "b", "c"]), set(&["b", "c"]), set(&["c"])]);
        assert_eq!(ranked, ["c", "b", "a"]);
        let tie = rank_by_keyword_coverage(&[set(&["z", "m"])]);
        assert_eq!(tie, ["m", "z"], "ties break by path");
    }

    struct Fixed(Vec<String>);
    impl Arm for Fixed {
        fn name(&self) -> &str {
            "fixed"
        }
        fn run(&self, _: &str, k: usize) -> anyhow::Result<Vec<String>> {
            Ok(self.0.iter().take(k).cloned().collect())
        }
    }

    #[test]
    fn evaluate_aggregates_per_task() {
        let tasks = vec![
            Task {
                id: "1".into(),
                query: "q".into(),
                truth: s(&["a", "b"]),
            },
            Task {
                id: "2".into(),
                query: "q".into(),
                truth: s(&["x"]),
            },
        ];
        let r = evaluate(&Fixed(s(&["a", "b", "c"])), &tasks, &[1, 3]).unwrap();
        assert_eq!(r.tasks, 2);
        assert_eq!(r.recall, [(1, 0.25), (3, 0.5)]);
        assert!(r.mean_tokens > 0.0);
    }

    #[test]
    fn arms_find_the_changed_files_on_a_toy_repo() {
        let dir = tempfile::tempdir().unwrap();
        let idx = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("parser.rs"), "fn tokenize_input() {}\n").unwrap();
        std::fs::write(root.join("lexer.rs"), "fn tokenize_input() { parser }\n").unwrap();
        std::fs::write(root.join("other.rs"), "fn unrelated() {}\n").unwrap();
        // Git metadata echoes commit subjects; neither arm may return it.
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(
            root.join(".git/HEAD_LOG"),
            "Rework tokenize_input and parser handling\n",
        )
        .unwrap();
        let mut config = loupe_core::Config::new(&root);
        config.index_dir = idx.path().to_path_buf();
        let mut index = Index::open_writer(config).unwrap();
        index.update(false).unwrap();

        let arm = TextIndexArm { index };
        let ranked = arm
            .run("Rework tokenize_input and parser handling", 5)
            .unwrap();
        // lexer.rs matches both keywords, parser.rs matches both ("parser" in name is not content).
        assert_eq!(ranked.first().map(String::as_str), Some("lexer.rs"));
        assert!(!ranked.contains(&"other.rs".to_string()));
        assert!(ranked.iter().all(|p| !p.starts_with(".git")), "{ranked:?}");

        if RipgrepArm::available() {
            let rg = RipgrepArm { root: root.clone() };
            assert_eq!(
                rg.run("Rework tokenize_input and parser handling", 5)
                    .unwrap(),
                ranked
            );
        }
    }
}

/// Semantic arm: the best chunk per file from the embedding index, files ranked by that score.
#[derive(Debug)]
pub struct SemanticArm {
    pub index: Index,
}

impl Arm for SemanticArm {
    fn name(&self) -> &str {
        "semantic"
    }

    fn run(&self, query: &str, k: usize) -> anyhow::Result<Vec<String>> {
        let r = self.index.search_semantic(
            query,
            &SemanticOptions {
                k,
                per_file: 1,
                snippet_lines: 0,
                ..SemanticOptions::default()
            },
        )?;
        Ok(r.hits.into_iter().map(|h| h.path).collect())
    }
}

/// First sentence of a doc comment, if it reads like a description (enough words, not a
/// directive or example).
fn doc_sentence(doc: &str) -> Option<String> {
    let flat = doc.split_whitespace().collect::<Vec<_>>().join(" ");
    let end = flat
        .char_indices()
        .find(|&(i, c)| c == '.' && flat[i + 1..].chars().next().is_none_or(char::is_whitespace))
        .map_or(flat.len(), |(i, _)| i);
    let sentence = flat[..end].trim().to_string();
    let words = sentence.split_whitespace().count();
    let prose = sentence.chars().filter(|c| c.is_alphabetic()).count() * 10 >= sentence.len() * 7;
    ((6..=40).contains(&words) && prose && !sentence.starts_with(['#', '`', '<', '[', '/']))
        .then_some(sentence)
}

/// Doc-to-definition tasks: query = the first sentence of a definition's doc comment, truth = the
/// file that defines it. Deterministic: every `stride`-th qualifying definition in path order.
/// Meaningful only when the index was built *without* doc text in its chunks, otherwise the query
/// is a substring of its own answer.
pub fn doc_tasks(index: &Index, limit: usize) -> anyhow::Result<Vec<Task>> {
    let snapshot = index.store().load_snapshot()?;
    let symbols = index.symbol_searcher()?;
    let mut all = Vec::new();
    for (path, meta) in &snapshot.files {
        if meta.lang.is_none() {
            continue;
        }
        let Some(outline) = symbols.outline(path, None)? else {
            continue;
        };
        for s in outline.symbols {
            if !matches!(
                s.kind,
                SymbolKind::Function
                    | SymbolKind::Method
                    | SymbolKind::Struct
                    | SymbolKind::Class
                    | SymbolKind::Trait
                    | SymbolKind::Enum
                    | SymbolKind::Interface
            ) {
                continue;
            }
            if let Some(q) = s.doc.as_deref().and_then(doc_sentence) {
                all.push((q, path.clone(), s.name));
            }
        }
    }
    // Skip queries that several files answer equally (same sentence defined in many files).
    let mut count: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (q, _, _) in &all {
        *count.entry(q.as_str()).or_default() += 1;
    }
    let unique: Vec<&(String, String, String)> = all
        .iter()
        .filter(|(q, _, _)| count[q.as_str()] == 1)
        .collect();
    let stride = (unique.len() / limit.max(1)).max(1);
    Ok(unique
        .into_iter()
        .step_by(stride)
        .take(limit)
        .enumerate()
        .map(|(i, (q, path, name))| Task {
            id: format!("{i}:{name}"),
            query: q.clone(),
            truth: vec![path.clone()],
        })
        .collect())
}

#[cfg(test)]
mod doc_task_tests {
    use super::doc_sentence;

    #[test]
    fn doc_sentences_must_read_like_prose() {
        assert_eq!(
            doc_sentence("Parse the configuration file from disk. Returns an error on failure.")
                .as_deref(),
            Some("Parse the configuration file from disk")
        );
        assert_eq!(doc_sentence("Too short."), None);
        assert_eq!(
            doc_sentence("# Heading: an example of the thing we do here today"),
            None
        );
        assert_eq!(
            doc_sentence("`foo(bar, baz, qux)` calls the function with many words inside"),
            None
        );
        assert_eq!(
            doc_sentence(&"word ".repeat(60)),
            None,
            "too long to be a one-line description"
        );
    }
}

/// The hybrid `query` pipeline, with stages switchable for ablation.
pub struct QueryArm {
    label: String,
    session: Session,
    opts: QueryOptions,
}

impl std::fmt::Debug for QueryArm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryArm")
            .field("label", &self.label)
            .finish()
    }
}

impl QueryArm {
    pub fn new(label: &str, index: &Index, opts: QueryOptions) -> anyhow::Result<QueryArm> {
        // Co-change edges are mined from the same git history that supplies the ground truth, so a
        // task's own commit would link its answer files together. Leave them out of the eval.
        let graph = loupe_api::GraphOptions {
            cochange: None,
            ..loupe_api::GraphOptions::default()
        };
        Ok(QueryArm {
            label: label.to_string(),
            session: index.session(&graph)?,
            opts,
        })
    }
}

impl Arm for QueryArm {
    fn name(&self) -> &str {
        &self.label
    }

    fn run(&self, query: &str, k: usize) -> anyhow::Result<Vec<String>> {
        let opts = QueryOptions {
            max_files: k,
            budget_tokens: usize::MAX / 4,
            ..self.opts.clone()
        };
        Ok(self
            .session
            .query(query, &opts, None)?
            .hits
            .into_iter()
            .map(|h| h.path)
            .collect())
    }
}
