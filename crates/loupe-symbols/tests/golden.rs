//! Golden tests: extraction output per language, reviewed by eye and pinned with `insta`.
//!
//! To accept intentional changes: `INSTA_UPDATE=always cargo test -p loupe-symbols --test golden`
//! then read the diff of `tests/snapshots/`.

use loupe_symbols::{
    LanguageSpec, OutlineOptions, Registry, Symbol, SymbolKind, VisibilityRule, extract_symbols,
    render_outline,
};

/// One line per symbol with everything the index stores, so regressions in any field show up.
fn detailed(symbols: &[Symbol]) -> String {
    let mut out = String::new();
    for s in symbols {
        let parent = s
            .parent
            .map_or("-".to_string(), |p| symbols[p as usize].name.clone());
        out.push_str(&format!(
            "{:<9} {:<9} {:<18} parent={:<10} L{}-{}  | {}",
            s.kind.as_str(),
            s.visibility.as_str(),
            s.name,
            parent,
            s.start_line,
            s.end_line,
            s.signature
        ));
        if let Some(doc) = &s.doc {
            out.push_str(&format!("  /// {}", doc.replace('\n', " / ")));
        }
        out.push('\n');
    }
    out
}

fn run(lang: &str, file: &str) -> Vec<Symbol> {
    let path = format!("{}/tests/fixtures/{file}", env!("CARGO_MANIFEST_DIR"));
    let source = std::fs::read(&path).unwrap();
    extract_symbols(Registry::global(), Some(lang), &source)
}

macro_rules! golden {
    ($test:ident, $lang:expr, $file:expr) => {
        #[test]
        fn $test() {
            let symbols = run($lang, $file);
            assert!(!symbols.is_empty(), "no symbols extracted from {}", $file);
            insta::assert_snapshot!(concat!(stringify!($test), "_symbols"), detailed(&symbols));
            insta::assert_snapshot!(
                concat!(stringify!($test), "_outline"),
                render_outline(&symbols, &OutlineOptions::default())
            );
        }
    };
}

golden!(rust, "rust", "sample.rs");
golden!(python, "python", "sample.py");
golden!(typescript, "typescript", "sample.ts");
golden!(javascript, "javascript", "sample.js");
golden!(go, "go", "sample.go");
golden!(c, "c", "sample.c");
golden!(java, "java", "Sample.java");

#[test]
fn language_without_a_grammar_uses_the_generic_fallback() {
    let symbols = run("kotlin", "sample.kt");
    insta::assert_snapshot!("kotlin_fallback", detailed(&symbols));
    assert!(
        symbols
            .iter()
            .all(|s| s.parent.is_none() && s.doc.is_none())
    );
}

#[test]
fn unknown_language_and_empty_input_yield_nothing_and_never_fail() {
    let r = Registry::global();
    assert!(extract_symbols(r, None, b"fn main() {}").is_empty());
    assert!(extract_symbols(r, Some("rust"), b"").is_empty());
    assert!(extract_symbols(r, Some("kotlin"), b"").is_empty());
    // Garbage and binary-ish input must not panic.
    for lang in ["rust", "python", "typescript", "go", "c", "java", "kotlin"] {
        let _ = extract_symbols(r, Some(lang), b"\x00\xff\xfe{{{ ((( fn class def \xc3\x28");
        let _ = extract_symbols(r, Some(lang), &b"fn ".repeat(10_000));
    }
}

#[test]
fn syntax_errors_still_yield_the_symbols_that_parse() {
    let src = b"pub fn good() {}\n\nfn broken( {\n\npub fn also_good() {}\n";
    let names: Vec<String> = extract_symbols(Registry::global(), Some("rust"), src)
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert!(names.contains(&"good".to_string()), "{names:?}");
}

#[test]
fn adding_a_language_needs_only_a_grammar_and_a_query() {
    // Register Rust again under a new id with a tiny query: no Rust code in the crate changes.
    let mut registry = Registry::empty();
    registry
        .register(LanguageSpec {
            id: "mini".into(),
            language: tree_sitter_rust::LANGUAGE.into(),
            query: "(function_item name: (identifier) @name) @definition.function".into(),
            visibility: VisibilityRule::None,
        })
        .unwrap();
    assert!(registry.has_grammar("mini") && !registry.has_grammar("rust"));
    let symbols = extract_symbols(
        &registry,
        Some("mini"),
        b"fn a() {}\nstruct S;\nfn b() {}\n",
    );
    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["a", "b"], "only what the query asks for");
    assert!(symbols.iter().all(|s| s.kind == SymbolKind::Function));
}

#[test]
fn invalid_queries_are_rejected_at_registration() {
    let mut registry = Registry::empty();
    let spec = |query: &str| LanguageSpec {
        id: "bad".into(),
        language: tree_sitter_rust::LANGUAGE.into(),
        query: query.into(),
        visibility: VisibilityRule::None,
    };
    assert!(
        registry
            .register(spec("(not_a_node) @definition.function"))
            .is_err()
    );
    assert!(
        registry
            .register(spec("(function_item) @definition.function"))
            .is_err(),
        "no @name"
    );
    assert!(
        registry
            .register(spec("(function_item name: (identifier) @name) @oops"))
            .is_err(),
        "no definition capture"
    );
    assert!(
        registry
            .register(spec(
                "(function_item name: (identifier) @name) @definition.banana"
            ))
            .is_err(),
        "unknown kind"
    );
    assert!(!registry.has_grammar("bad"));
}

#[test]
fn builtin_registry_lists_the_shipped_languages() {
    assert_eq!(
        Registry::global().language_ids(),
        [
            "c",
            "go",
            "java",
            "javascript",
            "python",
            "rust",
            "tsx",
            "typescript"
        ]
    );
}

#[test]
fn symbols_are_in_source_order_with_valid_parents_and_ranges() {
    for (lang, file) in [
        ("rust", "sample.rs"),
        ("python", "sample.py"),
        ("typescript", "sample.ts"),
        ("go", "sample.go"),
        ("c", "sample.c"),
        ("java", "Sample.java"),
    ] {
        let path = format!("{}/tests/fixtures/{file}", env!("CARGO_MANIFEST_DIR"));
        let source = std::fs::read(&path).unwrap();
        let symbols = extract_symbols(Registry::global(), Some(lang), &source);
        for (i, s) in symbols.iter().enumerate() {
            assert!(
                s.start_byte <= s.end_byte && (s.end_byte as usize) <= source.len(),
                "{file}: {s:?}"
            );
            assert!(s.start_line <= s.end_line);
            if let Some(p) = s.parent {
                let parent = &symbols[p as usize];
                assert!((p as usize) < i, "{file}: parent must precede child");
                assert!(
                    parent.start_byte <= s.start_byte && s.end_byte <= parent.end_byte,
                    "{file}: {s:?} not inside {parent:?}"
                );
            }
            if i > 0 {
                assert!(
                    symbols[i - 1].start_byte <= s.start_byte,
                    "{file}: out of order"
                );
            }
        }
    }
}
