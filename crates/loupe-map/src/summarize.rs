//! Module summaries: what a directory is for, in one to three lines.
//!
//! [`HeuristicSummarizer`] is the deterministic default (README / module doc comments, then
//! top-level symbols and names). [`CallbackSummarizer`] lets the harness plug in an LLM; because
//! summaries are cached by directory hash, the callback only runs for directories that changed.

/// What a summarizer sees about one file in the directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileInfo {
    pub name: String,
    pub lang: Option<String>,
    /// Top-level symbol names, public first, in source order within each group (capped).
    pub symbols: Vec<String>,
    /// Number of top-level symbols in the file (before capping).
    pub symbol_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleInput {
    /// Directory path, `""` for the repo root.
    pub path: String,
    pub files: Vec<FileInfo>,
    /// `(name, first line of that subdirectory's summary)`, sorted by name.
    pub subdirs: Vec<(String, String)>,
    /// A purpose sentence taken from a README or module doc comment in this directory.
    pub purpose: Option<String>,
}

pub trait Summarizer: Send + Sync {
    /// Part of the cache key: change it whenever the output format or prompt changes.
    fn version(&self) -> String;
    fn summarize(&self, input: &ModuleInput) -> String;
}

const PURPOSE_MAX: usize = 160;

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// First sentence of `text`, whitespace-collapsed and clipped.
fn first_sentence(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let end = collapsed
        .char_indices()
        .find(|&(i, c)| {
            matches!(c, '.' | '!' | '?')
                && collapsed[i + c.len_utf8()..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace)
        })
        .map_or(collapsed.len(), |(i, c)| i + c.len_utf8());
    clip(&collapsed[..end], PURPOSE_MAX)
}

/// Pull a purpose sentence out of a README or a module's leading doc comment.
///
/// `name` is the file name. Returns `None` if nothing usable is found.
pub fn extract_purpose(name: &str, content: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(&content[..content.len().min(16 * 1024)]);
    let lower = name.to_ascii_lowercase();
    let doc = if lower.starts_with("readme") {
        readme_purpose(&text)
    } else if matches!(lower.as_str(), "cargo.toml" | "pyproject.toml") {
        manifest_description(&text, r#"(?m)^\s*description\s*=\s*"((?:[^"\\]|\\.)*)""#)
    } else if lower == "package.json" {
        manifest_description(&text, r#""description"\s*:\s*"((?:[^"\\]|\\.)*)""#)
    } else if lower.ends_with(".py") {
        python_docstring(&text)
    } else if lower.ends_with(".rs") {
        leading_comment(&text, &["//!"])
    } else {
        leading_comment(&text, &["///", "//", "#"]).or_else(|| block_comment(&text))
    }?;
    let s = first_sentence(&doc);
    (s.chars().filter(|c| c.is_alphabetic()).count() >= 8).then_some(s)
}

/// The `description` field of a package manifest: the author's own one-line statement of purpose.
fn manifest_description(text: &str, pattern: &str) -> Option<String> {
    let re = regex::Regex::new(pattern).ok()?;
    let raw = re.captures(text)?.get(1)?.as_str();
    Some(raw.replace("\\\"", "\"").replace("\\\\", "\\"))
}

fn readme_purpose(text: &str) -> Option<String> {
    let mut para = Vec::new();
    for line in text.lines().map(str::trim) {
        let skip = line.is_empty()
            || line.starts_with('#')
            || line.starts_with("![")
            || line.starts_with("[![")
            || line.starts_with("<")
            || line.starts_with("---")
            || line.starts_with("===")
            || line.starts_with("```");
        if skip {
            if !para.is_empty() {
                break;
            }
            continue;
        }
        para.push(line);
    }
    (!para.is_empty()).then(|| para.join(" "))
}

fn leading_comment(text: &str, markers: &[&str]) -> Option<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("#!") && out.is_empty() {
            continue; // shebang
        }
        let Some(marker) = markers.iter().find(|m| t.starts_with(**m)) else {
            if out.is_empty() && t.is_empty() {
                continue;
            }
            break;
        };
        out.push(t[marker.len()..].trim().to_string());
    }
    let joined = out.join(" ");
    (!joined.trim().is_empty()).then_some(joined)
}

fn block_comment(text: &str) -> Option<String> {
    let t = text.trim_start();
    let body = t.strip_prefix("/**").or_else(|| t.strip_prefix("/*"))?;
    let body = &body[..body.find("*/")?];
    let joined = body
        .lines()
        .map(|l| l.trim().trim_start_matches('*').trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!joined.is_empty()).then_some(joined)
}

fn python_docstring(text: &str) -> Option<String> {
    let mut rest = text.trim_start();
    // Skip a shebang and leading comment lines.
    while rest.starts_with('#') {
        rest = rest.split_once('\n')?.1.trim_start();
    }
    for q in ["\"\"\"", "'''"] {
        if let Some(inner) = rest.strip_prefix(q) {
            return Some(inner[..inner.find(q)?].trim().to_string());
        }
    }
    None
}

/// Deterministic summaries from documentation, names, and top-level symbols.
#[derive(Clone, Copy, Debug, Default)]
pub struct HeuristicSummarizer;

impl Summarizer for HeuristicSummarizer {
    fn version(&self) -> String {
        "heuristic-1".to_string()
    }

    fn summarize(&self, input: &ModuleInput) -> String {
        let mut lines: Vec<String> = Vec::new();

        // Line 1: what it is.
        lines.push(match &input.purpose {
            Some(p) => p.clone(),
            None => generated_purpose(input),
        });

        // Line 2: key files (most top-level symbols first).
        let mut files: Vec<&FileInfo> = input.files.iter().filter(|f| f.symbol_count > 0).collect();
        files.sort_by(|a, b| {
            b.symbol_count
                .cmp(&a.symbol_count)
                .then_with(|| a.name.cmp(&b.name))
        });
        if !files.is_empty() {
            let shown: Vec<&str> = files.iter().take(5).map(|f| f.name.as_str()).collect();
            let more = files.len().saturating_sub(shown.len());
            let extra = if more > 0 {
                format!(" (+{more})")
            } else {
                String::new()
            };
            lines.push(format!("Key files: {}{extra}", shown.join(", ")));
        }

        // Line 3: subdirectories with their own first lines.
        if !input.subdirs.is_empty() {
            let shown: Vec<String> = input
                .subdirs
                .iter()
                .take(4)
                .map(|(name, first)| {
                    if first.is_empty() {
                        format!("{name}/")
                    } else {
                        format!("{name}/ ({})", clip(first, 48))
                    }
                })
                .collect();
            let more = input.subdirs.len().saturating_sub(shown.len());
            let extra = if more > 0 {
                format!(" (+{more})")
            } else {
                String::new()
            };
            lines.push(format!("Subdirs: {}{extra}", shown.join("; ")));
        }
        lines.join("\n")
    }
}

fn generated_purpose(input: &ModuleInput) -> String {
    let n = input.files.len();
    if n == 0 {
        let names: Vec<&str> = input
            .subdirs
            .iter()
            .take(5)
            .map(|(n, _)| n.as_str())
            .collect();
        return if names.is_empty() {
            "Empty directory.".to_string()
        } else {
            format!("Groups {}.", names.join(", "))
        };
    }
    // Dominant language(s) by file count.
    let mut by_lang: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for f in &input.files {
        *by_lang
            .entry(f.lang.as_deref().unwrap_or("other"))
            .or_default() += 1;
    }
    let mut langs: Vec<(&str, usize)> = by_lang.into_iter().collect();
    langs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let langs: Vec<String> = langs
        .iter()
        .take(2)
        .map(|(l, c)| format!("{l} {c}"))
        .collect();

    let mut names: Vec<&str> = Vec::new();
    for f in &input.files {
        for s in &f.symbols {
            if !names.contains(&s.as_str()) {
                names.push(s);
            }
        }
    }
    let defines = if names.is_empty() {
        String::new()
    } else {
        let shown: Vec<&str> = names.iter().take(5).copied().collect();
        format!("; defines {}", shown.join(", "))
    };
    format!(
        "{n} file{} ({}){defines}",
        if n == 1 { "" } else { "s" },
        langs.join(", ")
    )
}

type SummaryCallback = Box<dyn Fn(&ModuleInput) -> Option<String> + Send + Sync>;

/// Summaries from a callback (typically an LLM call), with the heuristic as fallback.
///
/// `version` must change whenever the prompt or model does, so cached summaries from an older
/// prompt are not reused.
pub struct CallbackSummarizer {
    version: String,
    callback: SummaryCallback,
}

impl std::fmt::Debug for CallbackSummarizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallbackSummarizer")
            .field("version", &self.version)
            .finish()
    }
}

impl CallbackSummarizer {
    pub fn new(
        version: impl Into<String>,
        callback: impl Fn(&ModuleInput) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        CallbackSummarizer {
            version: version.into(),
            callback: Box::new(callback),
        }
    }
}

impl Summarizer for CallbackSummarizer {
    fn version(&self) -> String {
        format!("callback:{}", self.version)
    }

    fn summarize(&self, input: &ModuleInput) -> String {
        (self.callback)(input)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| HeuristicSummarizer.summarize(input))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, lang: &str, symbols: &[&str], count: usize) -> FileInfo {
        FileInfo {
            name: name.into(),
            lang: Some(lang.into()),
            symbols: symbols.iter().map(|s| s.to_string()).collect(),
            symbol_count: count,
        }
    }

    #[test]
    fn purpose_from_readme_skips_headings_badges_and_blank_lines() {
        let readme = b"# Project\n\n[![CI](x)](y)\n![logo](z)\n\nA fast widget factory for busy teams. It does more.\nSecond line of the paragraph.\n\nNext paragraph.\n";
        assert_eq!(
            extract_purpose("README.md", readme).as_deref(),
            Some("A fast widget factory for busy teams.")
        );
        assert_eq!(extract_purpose("readme.txt", b"# Only a heading\n"), None);
    }

    #[test]
    fn purpose_from_rust_python_js_docs() {
        assert_eq!(
            extract_purpose(
                "lib.rs",
                b"//! Parses configuration files. Details follow.\n//! More.\n\nuse x;\n"
            )
            .as_deref(),
            Some("Parses configuration files.")
        );
        assert_eq!(extract_purpose("lib.rs", b"use x;\n//! too late\n"), None);
        assert_eq!(
            extract_purpose("__init__.py", b"#!/usr/bin/env python\n\"\"\"Networking helpers for the app.\n\nLong text.\"\"\"\nimport os\n").as_deref(),
            Some("Networking helpers for the app.")
        );
        assert_eq!(
            extract_purpose(
                "index.ts",
                b"/**\n * Public entry point of the client library.\n */\nexport * from './a';\n"
            )
            .as_deref(),
            Some("Public entry point of the client library.")
        );
        assert_eq!(
            extract_purpose(
                "main.go",
                b"// Command server runs the HTTP API.\npackage main\n"
            )
            .as_deref(),
            Some("Command server runs the HTTP API.")
        );
    }

    #[test]
    fn purpose_from_package_manifests() {
        assert_eq!(
            extract_purpose("Cargo.toml", b"[package]\nname = \"x\"\ndescription = \"Public facade the harness depends on\"\nversion = \"1\"\n").as_deref(),
            Some("Public facade the harness depends on")
        );
        assert_eq!(
            extract_purpose("package.json", b"{\n  \"name\": \"x\",\n  \"description\": \"A \\\"quoted\\\" widget library for teams\"\n}\n").as_deref(),
            Some("A \"quoted\" widget library for teams")
        );
        assert_eq!(
            extract_purpose(
                "pyproject.toml",
                b"[project]\ndescription = \"Tools for parsing logs quickly\"\n"
            )
            .as_deref(),
            Some("Tools for parsing logs quickly")
        );
        assert_eq!(
            extract_purpose("Cargo.toml", b"[package]\nname = \"x\"\n"),
            None
        );
    }

    #[test]
    fn trivial_or_license_noise_is_not_a_purpose() {
        assert_eq!(
            extract_purpose("a.rs", b"//! TODO\n"),
            None,
            "too short to say anything"
        );
        assert_eq!(extract_purpose("a.rs", b""), None);
        assert_eq!(extract_purpose("a.bin", &[0xff, 0xfe, 0x00]), None);
    }

    #[test]
    fn first_sentence_handles_abbreviations_and_length() {
        assert_eq!(
            first_sentence("Uses v1.2 of the API. Next."),
            "Uses v1.2 of the API."
        );
        assert_eq!(first_sentence("No terminator here"), "No terminator here");
        let long = "word ".repeat(100);
        let s = first_sentence(&long);
        assert!(s.chars().count() <= PURPOSE_MAX && s.ends_with('…'), "{s}");
    }

    #[test]
    fn heuristic_uses_the_purpose_then_key_files_then_subdirs() {
        let input = ModuleInput {
            path: "src/net".into(),
            files: vec![
                file("tcp.rs", "rust", &["TcpConn", "connect"], 2),
                file("mod.rs", "rust", &[], 0),
                file("udp.rs", "rust", &["Udp"], 5),
            ],
            subdirs: vec![
                ("tls".into(), "TLS handshakes.".into()),
                ("quic".into(), String::new()),
            ],
            purpose: Some("Network transports.".into()),
        };
        let s = HeuristicSummarizer.summarize(&input);
        assert_eq!(
            s,
            "Network transports.\nKey files: udp.rs, tcp.rs\nSubdirs: tls/ (TLS handshakes.); quic/"
        );
        assert_eq!(s, HeuristicSummarizer.summarize(&input), "deterministic");
    }

    #[test]
    fn heuristic_without_docs_describes_languages_and_symbols() {
        let input = ModuleInput {
            path: "x".into(),
            files: vec![
                file("a.py", "python", &["Foo", "bar"], 2),
                file("b.py", "python", &["Foo", "baz"], 2),
                file("c.js", "javascript", &[], 0),
            ],
            subdirs: vec![],
            purpose: None,
        };
        assert_eq!(
            HeuristicSummarizer
                .summarize(&input)
                .lines()
                .next()
                .unwrap(),
            "3 files (python 2, javascript 1); defines Foo, bar, baz"
        );
        let empty = ModuleInput {
            path: "e".into(),
            files: vec![],
            subdirs: vec![("a".into(), String::new()), ("b".into(), String::new())],
            purpose: None,
        };
        assert_eq!(
            HeuristicSummarizer
                .summarize(&empty)
                .lines()
                .next()
                .unwrap(),
            "Groups a, b."
        );
        let nothing = ModuleInput {
            path: "n".into(),
            files: vec![],
            subdirs: vec![],
            purpose: None,
        };
        assert_eq!(HeuristicSummarizer.summarize(&nothing), "Empty directory.");
        assert!(HeuristicSummarizer.summarize(&input).lines().count() <= 3);
    }

    #[test]
    fn callback_summarizer_falls_back_and_versions_the_cache() {
        let input = ModuleInput {
            path: "d".into(),
            files: vec![file("a.rs", "rust", &["A"], 1)],
            subdirs: vec![],
            purpose: None,
        };
        let llm = CallbackSummarizer::new("prompt-7", |i| Some(format!("  LLM says {}  ", i.path)));
        assert_eq!(llm.summarize(&input), "LLM says d");
        assert_eq!(llm.version(), "callback:prompt-7");
        assert_ne!(llm.version(), HeuristicSummarizer.version());
        let flaky = CallbackSummarizer::new("v", |_| None);
        assert_eq!(
            flaky.summarize(&input),
            HeuristicSummarizer.summarize(&input)
        );
        let blank = CallbackSummarizer::new("v", |_| Some("   ".into()));
        assert_eq!(
            blank.summarize(&input),
            HeuristicSummarizer.summarize(&input)
        );
    }
}
