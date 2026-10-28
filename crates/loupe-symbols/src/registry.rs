//! Grammar registry: language id -> tree-sitter grammar + definition query.
//!
//! Definitions are found by data-driven query files (tags-style `.scm`), so supporting a
//! language means supplying a grammar and a query. The query uses these captures:
//!
//! - `@definition.<kind>` on the whole definition node (`function`, `method`, `struct`, `class`,
//!   `interface`, `trait`, `enum`, `impl`, `module`, `constant`, `variable`, `type`, `macro`)
//! - `@name` on the identifier (required)
//! - `@body` on the body, if any: the signature is the text before it
//! - `@doc` on an explicit documentation string (e.g. a Python docstring)
//!
//! Everything else (nesting, method promotion, leading doc comments) is language-agnostic.

use std::collections::HashMap;
use std::sync::OnceLock;

use tree_sitter::{Language, Query};

use crate::symbol::SymbolKind;

/// How to decide a symbol's visibility. A small closed set of conventions; `None` leaves it
/// `Unknown`, which is always acceptable for a new language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisibilityRule {
    None,
    /// Rust: signature starts with `pub`.
    RustPub,
    /// JS/TS: defined under an `export`; class members are public unless `private`/`protected`.
    ExportKeyword,
    /// Go: exported iff the name starts with an uppercase letter.
    GoCapital,
    /// Python: leading underscore (but not dunder) means private.
    PythonUnderscore,
    /// Java: `public` / `private` / `protected` modifiers, else package-private.
    JavaModifiers,
    /// C: `static` means file-private.
    CStatic,
}

#[derive(Debug)]
pub struct LanguageSpec {
    pub id: String,
    pub language: Language,
    /// Query source (`.scm` contents).
    pub query: String,
    pub visibility: VisibilityRule,
}

pub(crate) struct CompiledLanguage {
    pub language: Language,
    pub query: Query,
    pub visibility: VisibilityRule,
    pub name_capture: u32,
    pub body_capture: Option<u32>,
    pub doc_capture: Option<u32>,
    /// `(capture index, kind)` for every `@definition.*` capture.
    pub def_captures: Vec<(u32, SymbolKind)>,
}

pub struct Registry {
    languages: HashMap<String, CompiledLanguage>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut ids: Vec<&String> = self.languages.keys().collect();
        ids.sort();
        f.debug_struct("Registry").field("languages", &ids).finish()
    }
}

impl Registry {
    pub fn empty() -> Registry {
        Registry {
            languages: HashMap::new(),
        }
    }

    /// The shipped languages: Rust, Python, JavaScript, TypeScript (+ TSX), Go, C, Java.
    pub fn builtin() -> Registry {
        let mut r = Registry::empty();
        let specs = [
            (
                "rust",
                tree_sitter_rust::LANGUAGE.into(),
                include_str!("../queries/rust.scm"),
                VisibilityRule::RustPub,
            ),
            (
                "python",
                tree_sitter_python::LANGUAGE.into(),
                include_str!("../queries/python.scm"),
                VisibilityRule::PythonUnderscore,
            ),
            (
                "javascript",
                tree_sitter_javascript::LANGUAGE.into(),
                include_str!("../queries/javascript.scm"),
                VisibilityRule::ExportKeyword,
            ),
            (
                "typescript",
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                include_str!("../queries/typescript.scm"),
                VisibilityRule::ExportKeyword,
            ),
            (
                "tsx",
                tree_sitter_typescript::LANGUAGE_TSX.into(),
                include_str!("../queries/typescript.scm"),
                VisibilityRule::ExportKeyword,
            ),
            (
                "go",
                tree_sitter_go::LANGUAGE.into(),
                include_str!("../queries/go.scm"),
                VisibilityRule::GoCapital,
            ),
            (
                "c",
                tree_sitter_c::LANGUAGE.into(),
                include_str!("../queries/c.scm"),
                VisibilityRule::CStatic,
            ),
            (
                "java",
                tree_sitter_java::LANGUAGE.into(),
                include_str!("../queries/java.scm"),
                VisibilityRule::JavaModifiers,
            ),
        ];
        for (id, language, query, visibility) in specs {
            r.register(LanguageSpec {
                id: id.to_string(),
                language,
                query: query.to_string(),
                visibility,
            })
            .unwrap_or_else(|e| panic!("builtin query for {id} is invalid: {e}"));
        }
        r
    }

    /// Process-wide registry of the shipped languages, compiled once.
    pub fn global() -> &'static Registry {
        static GLOBAL: OnceLock<Registry> = OnceLock::new();
        GLOBAL.get_or_init(Registry::builtin)
    }

    /// Add (or replace) a language. Fails if the query does not compile or lacks `@name` /
    /// any `@definition.*` capture.
    pub fn register(&mut self, spec: LanguageSpec) -> Result<(), String> {
        let query = Query::new(&spec.language, &spec.query).map_err(|e| e.to_string())?;
        let names = query.capture_names();
        let index_of = |wanted: &str| names.iter().position(|n| *n == wanted).map(|i| i as u32);

        let name_capture = index_of("name").ok_or("query has no @name capture")?;
        let mut def_captures = Vec::new();
        for (i, n) in names.iter().enumerate() {
            if let Some(kind) = n.strip_prefix("definition.") {
                let kind = SymbolKind::parse(kind)
                    .ok_or_else(|| format!("unknown definition kind {kind:?}"))?;
                def_captures.push((i as u32, kind));
            }
        }
        if def_captures.is_empty() {
            return Err("query has no @definition.* capture".into());
        }
        self.languages.insert(
            spec.id.clone(),
            CompiledLanguage {
                language: spec.language,
                body_capture: index_of("body"),
                doc_capture: index_of("doc"),
                name_capture,
                def_captures,
                query,
                visibility: spec.visibility,
            },
        );
        Ok(())
    }

    pub fn has_grammar(&self, id: &str) -> bool {
        self.languages.contains_key(id)
    }

    pub fn language_ids(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self.languages.keys().map(String::as_str).collect();
        ids.sort_unstable();
        ids
    }

    pub(crate) fn get(&self, id: &str) -> Option<&CompiledLanguage> {
        self.languages.get(id)
    }
}
