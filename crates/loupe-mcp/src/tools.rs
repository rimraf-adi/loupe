use std::time::Duration;

use loupe_api::{
    EdgeKind, GraphOptions, Index, MapOptions, OutlineOptions, QueryOptions, RelatedOptions,
    SearchOptions, SearchResult, SemanticOptions, SemanticState, Session, SummaryCache, SymbolKind,
    render_outline, render_query,
};
use loupe_core::{ApproxTokenCounter, Config, Error, TokenCounter};
use loupe_map::HeuristicSummarizer;
use serde_json::{Map, Value, json};

pub const TOOL_NAMES: [&str; 10] = [
    "repo_map",
    "module_summary",
    "outline",
    "query",
    "search_text",
    "search_semantic",
    "find_symbol",
    "refs",
    "related",
    "impact",
];

const DEFAULT_BUDGET: usize = 1500;
const MIN_BUDGET: usize = 50;

#[derive(Debug)]
pub enum ToolError {
    /// The arguments were wrong; the message tells the model how to fix them.
    Invalid(String),
    UnknownTool(String),
    /// The tool ran and failed in a way the model should read (e.g. nothing indexed yet).
    Failed(String),
    Internal(String),
}

impl From<Error> for ToolError {
    fn from(e: Error) -> Self {
        match e {
            Error::InvalidArgument(m) => ToolError::Invalid(m),
            other => ToolError::Failed(other.to_string()),
        }
    }
}

pub struct Tools {
    index: Index,
    session: Option<Session>,
    summaries: SummaryCache,
    repo_name: String,
}

impl std::fmt::Debug for Tools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tools").field("index", &self.index).finish()
    }
}

// ---- argument handling ---------------------------------------------------------------

struct Args<'a> {
    map: &'a Map<String, Value>,
}

impl<'a> Args<'a> {
    fn new(tool: &str, value: &'a Value, allowed: &[&str]) -> Result<Args<'a>, ToolError> {
        let map = match value {
            Value::Object(m) => m,
            Value::Null => {
                static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
                EMPTY.get_or_init(Map::new)
            }
            _ => {
                return Err(ToolError::Invalid(format!(
                    "{tool}: arguments must be an object"
                )));
            }
        };
        for key in map.keys() {
            if !allowed.contains(&key.as_str()) && key != "budget" {
                return Err(ToolError::Invalid(format!(
                    "{tool}: unknown argument {key:?}; valid arguments: {}, budget",
                    allowed.join(", ")
                )));
            }
        }
        Ok(Args { map })
    }

    fn str(&self, key: &str) -> Result<Option<&'a str>, ToolError> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(ToolError::Invalid(format!(
                "argument {key:?} must be a string"
            ))),
        }
    }

    fn required_str(&self, key: &str) -> Result<&'a str, ToolError> {
        self.str(key)?
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                ToolError::Invalid(format!(
                    "missing required argument {key:?} (a non-empty string)"
                ))
            })
    }

    fn usize(&self, key: &str, default: usize, max: usize) -> Result<usize, ToolError> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(default),
            Some(v) => v.as_u64().map(|n| (n as usize).min(max)).ok_or_else(|| {
                ToolError::Invalid(format!("argument {key:?} must be a non-negative integer"))
            }),
        }
    }

    fn bool(&self, key: &str) -> Result<bool, ToolError> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(ToolError::Invalid(format!(
                "argument {key:?} must be true or false"
            ))),
        }
    }

    fn strings(&self, key: &str) -> Result<Vec<String>, ToolError> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(a)) => a
                .iter()
                .map(|v| {
                    v.as_str().map(String::from).ok_or_else(|| {
                        ToolError::Invalid(format!("argument {key:?} must be a list of strings"))
                    })
                })
                .collect(),
            Some(_) => Err(ToolError::Invalid(format!(
                "argument {key:?} must be a list of strings"
            ))),
        }
    }

    fn budget(&self) -> Result<usize, ToolError> {
        Ok(self
            .usize("budget", DEFAULT_BUDGET, 100_000)?
            .max(MIN_BUDGET))
    }
}

/// Cut `text` at a line boundary so it fits `budget` tokens, saying so if it had to.
#[cfg(test)]
fn fit(text: &str, budget: usize) -> String {
    fit_for(text, budget, budget)
}

/// Like [`fit`], but the truncation note names `asked` (the caller's budget) while the cut uses
/// `budget` (what is left after reserving space for the footer).
fn fit_for(text: &str, budget: usize, asked: usize) -> String {
    let counter = ApproxTokenCounter;
    if counter.count(text) <= budget {
        return text.to_string();
    }
    let note = format!(
        "… truncated to fit the {asked}-token budget; narrow the query or raise `budget`\n"
    );
    let room = budget.saturating_sub(counter.count(&note));
    let mut out = String::new();
    let mut dropped = 0usize;
    for line in text.lines() {
        if dropped == 0 && counter.count(&out) + counter.count(line) < room {
            out.push_str(line);
            out.push('\n');
        } else {
            dropped += 1;
        }
    }
    out.push_str(&note);
    out
}

fn footer(snapshot: u64, stale: bool) -> String {
    format!(
        "[snapshot {snapshot:016x}{}]",
        if stale {
            ", STALE: some files changed on disk since indexing"
        } else {
            ""
        }
    )
}

fn finish(body: String, budget: usize, snapshot: u64, stale: bool) -> String {
    let foot = footer(snapshot, stale);
    let mut out = fit_for(
        &body,
        budget.saturating_sub(ApproxTokenCounter.count(&foot) + 1),
        budget,
    );
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&foot);
    out
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    let mut props = properties.as_object().cloned().unwrap_or_default();
    props.insert(
        "budget".into(),
        json!({"type": "integer", "minimum": MIN_BUDGET, "description": format!("Maximum tokens in the answer (default {DEFAULT_BUDGET}). Lower it to spend less context.")}),
    );
    json!({
        "name": name,
        "description": description,
        "inputSchema": {"type": "object", "properties": props, "required": required, "additionalProperties": false}
    })
}

fn search_result_text(r: &SearchResult) -> String {
    let mut out = String::new();
    for hit in &r.hits {
        out.push_str(&format!(
            "{} ({} match{}){}\n",
            hit.path,
            hit.match_count,
            if hit.match_count == 1 { "" } else { "es" },
            if hit.stale { " [stale]" } else { "" }
        ));
        for snip in &hit.snippets {
            for (i, line) in snip.text.lines().enumerate() {
                out.push_str(&format!("  {:>5}: {line}\n", snip.start_line + i));
            }
        }
    }
    out.push_str(&format!(
        "{} file(s), {} match(es){}{}\n",
        r.total_files,
        r.total_matches,
        if r.stats.truncated {
            "; more files matched than shown"
        } else {
            ""
        },
        if r.stats.timed_out {
            "; TIMED OUT, results are partial"
        } else {
            ""
        },
    ));
    out
}

impl Tools {
    pub fn open(config: Config) -> Result<Tools, Error> {
        let repo_name = config
            .root
            .file_name()
            .map_or_else(|| "repo".to_string(), |n| n.to_string_lossy().into_owned());
        let index = Index::open_reader(config)?;
        let summaries = index.open_summary_cache();
        Ok(Tools {
            index,
            session: None,
            summaries,
            repo_name,
        })
    }

    /// The tool list for `tools/list`.
    pub fn definitions() -> Vec<Value> {
        let path = |d: &str| json!({"type": "string", "description": d});
        vec![
            tool(
                "repo_map",
                "Orientation: a compact map of the whole repository (structure, entry points, the most important \
                 files with their key symbols, one-line summaries per directory), sized to a token budget. \
                 Call this FIRST in an unfamiliar repo. Use `focus` to bring a file, directory or keyword into view.",
                json!({"focus": {"type": "array", "items": {"type": "string"}, "description": "Paths, directories, or substrings to emphasise."}}),
                &[],
            ),
            tool(
                "module_summary",
                "A one-to-three line summary of a directory: what it is for, its key files, and what it depends on. \
                 Cheap; use it to decide where to look before opening anything. Omit `path` for the repo root.",
                json!({"path": path("Directory, relative to the repo root.")}),
                &[],
            ),
            tool(
                "outline",
                "A file's signatures and doc lines without bodies, with line numbers. Use this instead of reading a \
                 whole file when you only need to know what it defines.",
                json!({"path": path("File, relative to the repo root.")}),
                &["path"],
            ),
            tool(
                "query",
                "Answer a natural-language question about the code: combines keyword search, symbol lookup, and \
                 semantic search, ranks files, and returns the best files with the matching signatures and lines. \
                 Use this when you do not know the exact names. If you do know an identifier or string, search_text \
                 or find_symbol are more precise. Set `graph` to also pull in files connected by imports/tests.",
                json!({
                    "question": {"type": "string", "description": "What you are trying to find or understand."},
                    "max_files": {"type": "integer", "description": "At most this many files (default 12)."},
                    "graph": {"type": "boolean", "description": "Also include files connected to the best hits (default false)."}
                }),
                &["question"],
            ),
            tool(
                "search_text",
                "Exact text or regex search across the repo (like grep, but indexed and ranked by match count). \
                 Use for known strings, error messages, and patterns. `word` matches a whole identifier.",
                json!({
                    "pattern": {"type": "string", "description": "Regex, or literal text if `literal` is set."},
                    "literal": {"type": "boolean", "description": "Treat the pattern as plain text."},
                    "word": {"type": "boolean", "description": "Match a whole identifier (pattern must be an identifier)."},
                    "ignore_case": {"type": "boolean"},
                    "max_files": {"type": "integer", "description": "Default 20."},
                    "context": {"type": "integer", "description": "Context lines around each match (default 1)."}
                }),
                &["pattern"],
            ),
            tool(
                "search_semantic",
                "Search by meaning rather than exact words, over function/class-level chunks. Weaker than \
                 search_text when you know the words; useful for \"code that does X\" with unknown vocabulary. \
                 Prefer `query`, which combines this with exact signals.",
                json!({
                    "query": {"type": "string"},
                    "k": {"type": "integer", "description": "Results (default 10)."},
                    "scope": path("Only files under this path prefix.")
                }),
                &["query"],
            ),
            tool(
                "find_symbol",
                "Where a function, type, method, class, trait, ... is DEFINED, by exact name. Returns signatures \
                 with file and line.",
                json!({
                    "name": {"type": "string"},
                    "kind": {"type": "string", "enum": SymbolKind::ALL.map(|k| k.as_str()), "description": "Restrict to one kind."},
                    "limit": {"type": "integer", "description": "Default 20."}
                }),
                &["name"],
            ),
            tool(
                "refs",
                "Where an identifier is USED, plus where it is defined. Text-based (not compiler-accurate): it \
                 finds whole-word occurrences, so results can include unrelated symbols that share the name.",
                json!({"name": {"type": "string"}, "limit": {"type": "integer", "description": "Files to list (default 20)."}}),
                &["name"],
            ),
            tool(
                "related",
                "Files most connected to a given file through imports, tests, name references, and git co-change \
                 history. Answers \"what else would I need to look at or change with this?\"",
                json!({
                    "path": path("File, relative to the repo root."),
                    "kinds": {"type": "array", "items": {"type": "string", "enum": ["imports", "refs", "tests", "cochange"]}, "description": "Default: all."},
                    "hops": {"type": "integer", "description": "1 or 2 (default 2)."},
                    "limit": {"type": "integer", "description": "Default 15."}
                }),
                &["path"],
            ),
            tool(
                "impact",
                "Files that depend on a given file (transitively through imports, references, tests): what could \
                 break if you change it.",
                json!({
                    "path": path("File, relative to the repo root."),
                    "depth": {"type": "integer", "description": "Default 3."},
                    "limit": {"type": "integer", "description": "Default 30."}
                }),
                &["path"],
            ),
        ]
    }

    /// Follow commits made by a writer: reload the manifest and rebuild the pinned searchers if
    /// the snapshot moved.
    fn session(&mut self) -> Result<&Session, ToolError> {
        self.index.refresh().map_err(ToolError::from)?;
        if self.index.snapshot_id() == 0 {
            return Err(ToolError::Failed(
                "nothing is indexed yet; run `loupe index` (or `loupe daemon run`) for this repo first".into(),
            ));
        }
        let stale = self
            .session
            .as_ref()
            .is_none_or(|s| s.snapshot_id() != self.index.snapshot_id());
        if stale {
            self.session = Some(self.index.session(&GraphOptions::default())?);
        }
        Ok(self.session.as_ref().unwrap())
    }

    pub fn call(&mut self, name: &str, args: &Value) -> Result<String, ToolError> {
        if !TOOL_NAMES.contains(&name) {
            return Err(ToolError::UnknownTool(name.to_string()));
        }
        match name {
            "repo_map" => {
                let a = Args::new(name, args, &["focus"])?;
                let (budget, focus) = (a.budget()?, a.strings("focus")?);
                let repo_name = self.repo_name.clone();
                self.session()?;
                let session = self.session.as_ref().unwrap();
                let map = loupe_api::repo_map(
                    &self.index.config().root,
                    &session.symbols,
                    None,
                    &session.graph,
                    &self.summaries,
                    &HeuristicSummarizer,
                    &MapOptions {
                        budget: budget.saturating_sub(12).max(MIN_BUDGET),
                        focus,
                        name: repo_name,
                    },
                )?;
                Ok(finish(map.text, budget, session.snapshot_id(), false))
            }
            "module_summary" => {
                let a = Args::new(name, args, &["path"])?;
                let (budget, dir) = (a.budget()?, a.str("path")?.unwrap_or("").to_string());
                self.session()?;
                let session = self.session.as_ref().unwrap();
                let m = loupe_api::module_summary(
                    &self.index.config().root,
                    &session.symbols,
                    None,
                    &session.graph,
                    &self.summaries,
                    &HeuristicSummarizer,
                    &dir,
                )?;
                let mut body = format!("{}/  ({} files)\n{}\n", m.path, m.files, m.text);
                if !m.subdirs.is_empty() {
                    body.push_str(&format!("Subdirectories: {}\n", m.subdirs.join(", ")));
                }
                if !m.depends_on.is_empty() {
                    body.push_str(&format!("Depends on: {}\n", m.depends_on.join(", ")));
                }
                Ok(finish(body, budget, session.snapshot_id(), false))
            }
            "outline" => {
                let a = Args::new(name, args, &["path"])?;
                let (budget, path) = (a.budget()?, a.required_str("path")?.to_string());
                let session = self.session()?;
                let Some(o) = session.symbols.outline(&path, None)? else {
                    return Err(ToolError::Failed(format!(
                        "no such file in the index: {path}"
                    )));
                };
                let mut body = format!(
                    "{}  ({})\n",
                    o.path,
                    o.lang.as_deref().unwrap_or("unknown language")
                );
                if o.symbols.is_empty() {
                    body.push_str("(no symbols found; use search_text or read the file)\n");
                }
                body.push_str(&render_outline(&o.symbols, &OutlineOptions::default()));
                Ok(finish(body, budget, session.snapshot_id(), o.stale))
            }
            "query" => {
                let a = Args::new(name, args, &["question", "max_files", "graph"])?;
                let (budget, q) = (a.budget()?, a.required_str("question")?.to_string());
                let opts = QueryOptions {
                    budget_tokens: budget.saturating_sub(10).max(MIN_BUDGET),
                    max_files: a.usize("max_files", 12, 50)?,
                    use_graph: a.bool("graph")?,
                    ..QueryOptions::default()
                };
                let session = self.session()?;
                let r = session.query(&q, &opts, None)?;
                let body = if r.hits.is_empty() {
                    format!(
                        "No matches for {q:?}. Try search_text with an exact string, or repo_map to explore.\n"
                    )
                } else {
                    render_query(&r, &q)
                };
                Ok(finish(body, budget, r.snapshot_id, r.stale))
            }
            "search_text" => {
                let a = Args::new(
                    name,
                    args,
                    &[
                        "pattern",
                        "literal",
                        "word",
                        "ignore_case",
                        "max_files",
                        "context",
                    ],
                )?;
                let budget = a.budget()?;
                let pattern = a.required_str("pattern")?.to_string();
                let opts = SearchOptions {
                    case_insensitive: a.bool("ignore_case")?,
                    literal: a.bool("literal")?,
                    max_files: a.usize("max_files", 20, 200)?,
                    max_matches_per_file: 3,
                    context_lines: a.usize("context", 1, 5)?,
                    timeout: Some(Duration::from_secs(5)),
                };
                let word = a.bool("word")?;
                let session = self.session()?;
                let r = if word {
                    session.text.search_word(&pattern, &opts)?
                } else {
                    session.text.search(&pattern, &opts)?
                };
                Ok(finish(
                    search_result_text(&r),
                    budget,
                    r.snapshot_id,
                    r.stats.stale_files > 0,
                ))
            }
            "search_semantic" => {
                let a = Args::new(name, args, &["query", "k", "scope"])?;
                let (budget, q) = (a.budget()?, a.required_str("query")?.to_string());
                let opts = SemanticOptions {
                    k: a.usize("k", 10, 50)?,
                    scope: a.str("scope")?.map(String::from),
                    snippet_lines: 4,
                    ..SemanticOptions::default()
                };
                let session = self.session()?;
                let r = session.semantic.search(&q, &opts, None)?;
                let mut body = String::new();
                for h in &r.hits {
                    body.push_str(&format!(
                        "{:.2}  {}:{}-{}  {}{}\n",
                        h.score,
                        h.path,
                        h.start_line,
                        h.end_line,
                        h.breadcrumb,
                        if h.stale { "  [stale]" } else { "" }
                    ));
                    for line in h.snippet.lines().take(3) {
                        body.push_str(&format!("        {line}\n"));
                    }
                }
                match r.state {
                    SemanticState::Full => {}
                    SemanticState::Partial => body.push_str("(semantic index is partial: some files are not searchable by meaning yet)\n"),
                    SemanticState::Off => body.push_str("(no embeddings for the current model; run `loupe index`)\n"),
                }
                if r.hits.is_empty() {
                    body.push_str("No semantic matches.\n");
                }
                let stale = r.hits.iter().any(|h| h.stale);
                Ok(finish(body, budget, r.snapshot_id, stale))
            }
            "find_symbol" => {
                let a = Args::new(name, args, &["name", "kind", "limit"])?;
                let (budget, sym) = (a.budget()?, a.required_str("name")?.to_string());
                let kind = a
                    .str("kind")?
                    .map(|k| {
                        SymbolKind::parse(k)
                            .ok_or_else(|| ToolError::Invalid(format!("unknown kind {k:?}")))
                    })
                    .transpose()?;
                let limit = a.usize("limit", 20, 200)?;
                let session = self.session()?;
                let hits = session.symbols.find(&sym, kind, limit, None)?;
                let mut body = String::new();
                for h in &hits {
                    body.push_str(&format!(
                        "{}:{}  {}{}\n",
                        h.path,
                        h.start_line,
                        h.signature,
                        if h.stale { "  [stale]" } else { "" }
                    ));
                    if let Some(p) = &h.parent {
                        body.push_str(&format!("    in {p}\n"));
                    }
                    if let Some(d) = h.doc.as_deref().and_then(|d| d.lines().next()) {
                        body.push_str(&format!("    // {d}\n"));
                    }
                }
                if hits.is_empty() {
                    body.push_str(&format!("No definitions named {sym:?}. Try refs or search_text for usages, or query for a description.\n"));
                }
                let stale = hits.iter().any(|h| h.stale);
                Ok(finish(body, budget, session.snapshot_id(), stale))
            }
            "refs" => {
                let a = Args::new(name, args, &["name", "limit"])?;
                let (budget, sym) = (a.budget()?, a.required_str("name")?.to_string());
                let limit = a.usize("limit", 20, 200)?;
                let session = self.session()?;
                let defs = session.symbols.find(&sym, None, 20, None)?;
                let opts = SearchOptions {
                    max_files: limit,
                    max_matches_per_file: 3,
                    context_lines: 0,
                    timeout: Some(Duration::from_secs(5)),
                    ..SearchOptions::default()
                };
                let uses = session.text.search_word(&sym, &opts)?;
                let mut body = String::new();
                body.push_str("Defined at:\n");
                let defs_stale = defs.iter().any(|d| d.stale);
                for d in &defs {
                    body.push_str(&format!("  {}:{}  {}\n", d.path, d.start_line, d.signature));
                }
                if defs.is_empty() {
                    body.push_str("  (no definition found in the index)\n");
                }
                body.push_str("Used in (whole-word text matches, may include unrelated symbols of the same name):\n");
                for hit in &uses.hits {
                    let def_lines: Vec<u32> = defs
                        .iter()
                        .filter(|d| d.path == hit.path)
                        .map(|d| d.start_line)
                        .collect();
                    body.push_str(&format!(
                        "  {} ({} match{})\n",
                        hit.path,
                        hit.match_count,
                        if hit.match_count == 1 { "" } else { "es" }
                    ));
                    for s in &hit.snippets {
                        if def_lines.contains(&(s.start_line as u32)) {
                            continue; // that line is the definition itself
                        }
                        body.push_str(&format!(
                            "    {:>5}: {}\n",
                            s.start_line,
                            s.text.lines().next().unwrap_or("").trim()
                        ));
                    }
                }
                body.push_str(&format!(
                    "{} file(s){}\n",
                    uses.total_files,
                    if uses.stats.truncated {
                        "; more not shown"
                    } else {
                        ""
                    }
                ));
                Ok(finish(
                    body,
                    budget,
                    uses.snapshot_id,
                    uses.stats.stale_files > 0 || defs_stale,
                ))
            }
            "related" => {
                let a = Args::new(name, args, &["path", "kinds", "hops", "limit"])?;
                let (budget, path) = (a.budget()?, a.required_str("path")?.to_string());
                let kinds = a
                    .strings("kinds")?
                    .iter()
                    .map(|k| {
                        EdgeKind::parse(k).ok_or_else(|| {
                            ToolError::Invalid(format!(
                                "unknown edge kind {k:?}; use imports, refs, tests, cochange"
                            ))
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let opts = RelatedOptions {
                    kinds: if kinds.is_empty() {
                        EdgeKind::ALL.to_vec()
                    } else {
                        kinds
                    },
                    hops: a.usize("hops", 2, 3)? as u8,
                    limit: a.usize("limit", 15, 100)?,
                };
                let session = self.session()?;
                if session.graph.node(&path).is_none() {
                    return Err(ToolError::Failed(format!(
                        "no such file in the index: {path}"
                    )));
                }
                let related = session.graph.related(&path, &opts);
                let mut body = String::new();
                let mut any_stale = session.text.is_stale(&path, None);
                for r in &related {
                    let via: Vec<&str> = r.via.iter().map(|k| k.as_str()).collect();
                    let stale = session.text.is_stale(&r.path, None);
                    any_stale |= stale;
                    body.push_str(&format!(
                        "{:.2}  {}  [{}]{}\n",
                        r.score,
                        r.path,
                        via.join(" > "),
                        if stale { "  [stale]" } else { "" }
                    ));
                }
                if related.is_empty() {
                    body.push_str("Nothing related found (no edges for this file).\n");
                }
                Ok(finish(body, budget, session.snapshot_id(), any_stale))
            }
            "impact" => {
                let a = Args::new(name, args, &["path", "depth", "limit"])?;
                let (budget, path) = (a.budget()?, a.required_str("path")?.to_string());
                let (depth, limit) = (a.usize("depth", 3, 6)? as u8, a.usize("limit", 30, 200)?);
                let session = self.session()?;
                if session.graph.node(&path).is_none() {
                    return Err(ToolError::Failed(format!(
                        "no such file in the index: {path}"
                    )));
                }
                let impacted = session.graph.impact(&path, depth, limit);
                let mut body = String::new();
                let mut any_stale = session.text.is_stale(&path, None);
                for i in &impacted {
                    let stale = session.text.is_stale(&i.path, None);
                    any_stale |= stale;
                    body.push_str(&format!(
                        "depth {}  {}  ({}){}\n",
                        i.depth,
                        i.path,
                        i.kind.as_str(),
                        if stale { "  [stale]" } else { "" }
                    ));
                }
                if impacted.is_empty() {
                    body.push_str("Nothing depends on this file.\n");
                }
                Ok(finish(body, budget, session.snapshot_id(), any_stale))
            }
            other => Err(ToolError::Internal(format!(
                "tool {other} is listed but not implemented"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_truncates_on_line_boundaries_and_says_so() {
        let text: String = (0..200)
            .map(|i| format!("line number {i} with some padding words\n"))
            .collect();
        let out = fit(&text, 100);
        assert!(
            ApproxTokenCounter.count(&out) <= 100,
            "{}",
            ApproxTokenCounter.count(&out)
        );
        assert!(out.contains("truncated to fit the 100-token budget"));
        assert!(out.starts_with("line number 0 "));
        assert!(
            out.lines()
                .filter(|l| l.starts_with("line number"))
                .all(|l| l.ends_with("padding words")),
            "no half lines"
        );
        assert_eq!(fit("short\n", 100), "short\n");
    }

    #[test]
    fn every_tool_has_a_schema_description_and_budget() {
        let defs = Tools::definitions();
        let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(names, TOOL_NAMES);
        for d in &defs {
            let name = d["name"].as_str().unwrap();
            assert!(
                d["description"].as_str().unwrap().len() > 40,
                "{name} needs a real description"
            );
            let schema = &d["inputSchema"];
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["additionalProperties"], false);
            assert!(
                schema["properties"]["budget"]["type"] == "integer",
                "{name} takes a budget"
            );
            for r in schema["required"].as_array().unwrap() {
                assert!(
                    schema["properties"][r.as_str().unwrap()].is_object(),
                    "{name}: required {r} is not a property"
                );
            }
        }
    }

    #[test]
    fn argument_validation_messages_help_the_model_recover() {
        let v = json!({"pattern": "x", "paterrn": 1});
        let e = Args::new("search_text", &v, &["pattern"]).err().unwrap();
        assert!(
            matches!(&e, ToolError::Invalid(m) if m.contains("paterrn") && m.contains("pattern")),
            "{e:?}"
        );
        let v = json!({"limit": "ten"});
        let a = Args::new("t", &v, &["limit"]).unwrap();
        assert!(matches!(
            a.usize("limit", 5, 10),
            Err(ToolError::Invalid(_))
        ));
        assert_eq!(
            Args::new("t", &json!({"limit": 99}), &["limit"])
                .unwrap()
                .usize("limit", 5, 10)
                .unwrap(),
            10,
            "clamped to max"
        );
        assert!(matches!(
            Args::new("t", &json!({}), &[]).unwrap().required_str("x"),
            Err(ToolError::Invalid(_))
        ));
        assert!(matches!(
            Args::new("t", &json!([1]), &[]),
            Err(ToolError::Invalid(_))
        ));
        assert!(Args::new("t", &Value::Null, &[]).is_ok());
        assert_eq!(
            Args::new("t", &json!({"budget": 1}), &[])
                .unwrap()
                .budget()
                .unwrap(),
            MIN_BUDGET
        );
    }
}
