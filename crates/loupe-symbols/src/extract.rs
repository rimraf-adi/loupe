//! Symbol extraction: tree-sitter queries when a grammar is registered, a generic
//! declaration-line heuristic otherwise.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::OnceLock;

use regex::bytes::Regex;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, QueryCursor};

use crate::registry::{CompiledLanguage, Registry, VisibilityRule};
use crate::symbol::{Symbol, SymbolKind, Visibility};

const MAX_SIGNATURE_BYTES: usize = 240;
const MAX_DOC_BYTES: usize = 600;
/// Files larger than this are not parsed (symbol offsets are `u32`, and giant generated files
/// are not worth the time).
const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;

/// Extract definitions from `source`.
///
/// `lang` is a language id from `loupe_walk::detect_language`. Registered languages go through
/// tree-sitter; other detected languages get the generic fallback; `None` yields nothing.
pub fn extract_symbols(registry: &Registry, lang: Option<&str>, source: &[u8]) -> Vec<Symbol> {
    let Some(lang) = lang else {
        return Vec::new();
    };
    if source.len() > MAX_SOURCE_BYTES {
        return Vec::new();
    }
    match registry.get(lang) {
        Some(spec) => extract_tree_sitter(spec, source),
        None => extract_fallback(source),
    }
}

struct Raw<'t> {
    node: Node<'t>,
    kind: SymbolKind,
    pattern: usize,
    name: (usize, usize),
    body_start: Option<usize>,
    doc: Option<String>,
}

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new(Parser::new());
}

fn extract_tree_sitter(spec: &CompiledLanguage, source: &[u8]) -> Vec<Symbol> {
    let tree = PARSER.with(|p| {
        let mut parser = p.borrow_mut();
        parser.set_language(&spec.language).ok()?;
        parser.parse(source, None)
    });
    let Some(tree) = tree else {
        return Vec::new();
    };

    // One entry per definition node: several patterns may match the same node (e.g. a plain
    // and a docstring variant). The lowest pattern index decides the kind and name.
    let has_exports = source.windows(7).any(|w| w == b"export ");
    let mut raws: HashMap<usize, Raw<'_>> = HashMap::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&spec.query, tree.root_node(), source);
    while let Some(m) = matches.next() {
        let mut def: Option<(Node<'_>, SymbolKind)> = None;
        let (mut name, mut body, mut doc) = (None, None, None);
        for cap in m.captures {
            if let Some(&(_, kind)) = spec.def_captures.iter().find(|(i, _)| *i == cap.index) {
                def = Some((cap.node, kind));
            } else if cap.index == spec.name_capture {
                name = Some(cap.node);
            } else if Some(cap.index) == spec.body_capture {
                body = Some(cap.node);
            } else if Some(cap.index) == spec.doc_capture {
                doc = Some(cap.node);
            }
        }
        let (Some((node, kind)), Some(name)) = (def, name) else {
            continue;
        };
        let doc = doc
            .and_then(|d| d.utf8_text(source).ok())
            .map(strip_string_quotes);
        match raws.get_mut(&node.id()) {
            Some(existing) => {
                if m.pattern_index < existing.pattern {
                    existing.kind = kind;
                    existing.pattern = m.pattern_index;
                    existing.name = (name.start_byte(), name.end_byte());
                }
                existing.body_start = existing.body_start.or(body.map(|b| b.start_byte()));
                existing.doc = existing.doc.take().or(doc);
            }
            None => {
                raws.insert(
                    node.id(),
                    Raw {
                        node,
                        kind,
                        pattern: m.pattern_index,
                        name: (name.start_byte(), name.end_byte()),
                        body_start: body.map(|b| b.start_byte()),
                        doc,
                    },
                );
            }
        }
    }

    let mut ordered: Vec<Raw<'_>> = raws.into_values().collect();
    // Outer definitions before the ones they contain.
    ordered.sort_by(|a, b| {
        a.node
            .start_byte()
            .cmp(&b.node.start_byte())
            .then(b.node.end_byte().cmp(&a.node.end_byte()))
    });

    struct Open {
        end: usize,
        out: Option<u32>,
        in_function: bool,
    }
    let mut stack: Vec<Open> = Vec::new();
    let mut out: Vec<Symbol> = Vec::new();

    for raw in ordered {
        let (start, end) = (raw.node.start_byte(), raw.node.end_byte());
        while stack.last().is_some_and(|o| o.end <= start) {
            stack.pop();
        }
        let parent = stack.iter().rev().find_map(|o| o.out);
        let in_function = stack.last().is_some_and(|o| o.in_function);
        let is_function = matches!(raw.kind, SymbolKind::Function | SymbolKind::Method);

        // Locals are not part of a file's API surface: variables/constants inside functions, and
        // methods of object literals or anonymous classes inside functions.
        let parent_is_fn = parent.is_some_and(|p| {
            matches!(
                out[p as usize].kind,
                SymbolKind::Function | SymbolKind::Method
            )
        });
        let local = (in_function
            && matches!(raw.kind, SymbolKind::Constant | SymbolKind::Variable))
            || (raw.kind == SymbolKind::Method && parent_is_fn);
        if local {
            stack.push(Open {
                end,
                out: None,
                in_function: true,
            });
            continue;
        }

        let name = String::from_utf8_lossy(&source[raw.name.0..raw.name.1]).into_owned();
        let mut kind = raw.kind;
        if kind == SymbolKind::Function
            && parent.is_some_and(|p| out[p as usize].kind.is_container())
        {
            kind = SymbolKind::Method;
        }
        let mut signature = signature_of(source, start, end, raw.body_start);
        if let Some(keyword) = declaration_keyword(raw.node, source) {
            signature = format!("{keyword} {signature}");
        }
        let visibility = visibility_of(
            spec.visibility,
            kind,
            &name,
            &signature,
            raw.node,
            parent.map(|p| &out[p as usize]),
            has_exports,
        );
        let doc = raw.doc.or_else(|| leading_doc(raw.node, source));

        let idx = out.len() as u32;
        out.push(Symbol {
            name,
            kind,
            visibility,
            parent,
            start_line: raw.node.start_position().row as u32 + 1,
            end_line: raw.node.end_position().row as u32 + 1,
            start_byte: start as u32,
            end_byte: end as u32,
            signature,
            doc,
        });
        stack.push(Open {
            end,
            out: Some(idx),
            in_function: in_function || is_function,
        });
    }
    out
}

fn signature_of(source: &[u8], start: usize, end: usize, body_start: Option<usize>) -> String {
    let cut = match body_start {
        Some(b) if b >= start && b <= end => b,
        _ => source[start..end]
            .iter()
            .position(|&c| c == b'{' || c == b'\n')
            .map_or(end, |p| start + p),
    };
    let text = String::from_utf8_lossy(&source[start..cut]);
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_end_matches('{').trim_end();
    truncate(trimmed, MAX_SIGNATURE_BYTES)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

/// `type` / `const` / `var` for a spec nested in a declaration (Go's `type_spec` inside
/// `type_declaration`), whose own text lacks the keyword.
fn declaration_keyword(node: Node<'_>, source: &[u8]) -> Option<String> {
    let parent = node.parent()?;
    if !parent.kind().ends_with("_declaration") || parent.start_byte() >= node.start_byte() {
        return None;
    }
    let first = parent.child(0)?;
    if first.is_named() {
        return None;
    }
    let text = first.utf8_text(source).ok()?;
    (!text.is_empty() && text.bytes().all(|b| b.is_ascii_alphabetic())).then(|| text.to_string())
}

fn visibility_of(
    rule: VisibilityRule,
    kind: SymbolKind,
    name: &str,
    signature: &str,
    node: Node<'_>,
    parent: Option<&Symbol>,
    has_exports: bool,
) -> Visibility {
    let has_word = |w: &str| {
        signature
            .split_whitespace()
            .any(|t| t == w || t.starts_with(&format!("{w}(")))
    };
    match rule {
        VisibilityRule::None => Visibility::Unknown,
        VisibilityRule::RustPub => {
            if kind == SymbolKind::Impl {
                Visibility::Unknown // impl blocks carry no visibility of their own
            } else if signature.starts_with("pub")
                // Trait members are as visible as the trait itself.
                || parent.is_some_and(|p| {
                    p.kind == SymbolKind::Trait
                        || (p.kind == SymbolKind::Impl && p.signature.contains(" for "))
                })
            {
                Visibility::Public
            } else {
                Visibility::Private
            }
        }
        VisibilityRule::ExportKeyword => {
            if node
                .parent()
                .is_some_and(|p| p.kind() == "export_statement")
            {
                Visibility::Public
            } else if kind == SymbolKind::Method {
                if has_word("private") {
                    Visibility::Private
                } else if has_word("protected") {
                    Visibility::Protected
                } else {
                    Visibility::Public
                }
            } else if has_exports {
                Visibility::Private
            } else {
                Visibility::Unknown // CommonJS-style file: nothing says what is private
            }
        }
        VisibilityRule::GoCapital => {
            if name.chars().next().is_some_and(char::is_uppercase) {
                Visibility::Public
            } else {
                Visibility::Private
            }
        }
        VisibilityRule::PythonUnderscore => {
            if name.starts_with('_') && !(name.starts_with("__") && name.ends_with("__")) {
                Visibility::Private
            } else {
                Visibility::Public
            }
        }
        VisibilityRule::JavaModifiers => {
            if has_word("public") || parent.is_some_and(|p| p.kind == SymbolKind::Interface) {
                Visibility::Public
            } else if has_word("private") {
                Visibility::Private
            } else if has_word("protected") {
                Visibility::Protected
            } else {
                Visibility::Package
            }
        }
        VisibilityRule::CStatic => {
            if has_word("static") {
                Visibility::Private
            } else {
                Visibility::Public
            }
        }
    }
}

/// Contiguous comment lines directly above a definition, skipping attributes/decorators.
fn leading_doc(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut first = node;
    while let Some(p) = first.parent() {
        if matches!(p.kind(), "export_statement" | "decorated_definition") {
            first = p;
        } else {
            break;
        }
    }
    let mut lines: Vec<String> = Vec::new();
    let mut expected_row = first.start_position().row;
    let mut cur = first;
    while let Some(prev) = cur.prev_sibling() {
        let kind = prev.kind();
        let adjacent = prev.end_position().row + 1 >= expected_row;
        if kind.contains("comment") && adjacent {
            if let Ok(text) = prev.utf8_text(source) {
                lines.push(clean_comment(text));
            }
        } else if !(adjacent && (kind.contains("attribute") || kind.contains("decorator"))) {
            break;
        }
        expected_row = prev.start_position().row;
        cur = prev;
    }
    lines.reverse();
    let doc = lines.join("\n").trim().to_string();
    (!doc.is_empty()).then(|| truncate(&doc, MAX_DOC_BYTES))
}

fn clean_comment(text: &str) -> String {
    text.lines()
        .map(|l| {
            let l = l.trim();
            let l = l
                .trim_start_matches("///")
                .trim_start_matches("//!")
                .trim_start_matches("//")
                .trim_start_matches("/**")
                .trim_start_matches("/*")
                .trim_end_matches("*/")
                .trim_start_matches('*')
                .trim_start_matches('#');
            l.trim().to_string()
        })
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_string_quotes(raw: &str) -> String {
    let t = raw.trim();
    let t = t.trim_start_matches(|c: char| c.is_ascii_alphabetic()); // r"", f"", b""
    for q in ["\"\"\"", "'''", "\"", "'"] {
        if let Some(inner) = t.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return inner.trim().to_string();
        }
    }
    t.to_string()
}

// ---- generic fallback ----------------------------------------------------------------

fn fallback_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?m)^[ \t]*(?:(?:pub|public|private|protected|internal|static|export|default|async|final|abstract|open|override|inline|extern|suspend|data|sealed|unsafe)[ \t]+)*(fn|def|function|func|fun|sub|class|struct|interface|trait|enum|impl|object|module|namespace|type|const|val)[ \t]+(?:(?:val|var)[ \t]+)?([A-Za-z_][A-Za-z0-9_]*)",
        )
        .expect("fallback regex")
    })
}

/// Declaration-line heuristic for languages without a grammar. Finds `keyword Name` at the start
/// of a line; reports one-line symbols with no nesting, docs, or visibility.
fn extract_fallback(source: &[u8]) -> Vec<Symbol> {
    let mut out = Vec::new();
    let mut line = 1u32;
    let mut counted_to = 0usize;
    for caps in fallback_regex().captures_iter(source) {
        let whole = caps.get(0).unwrap();
        let keyword = caps.get(1).unwrap().as_bytes();
        let name = caps.get(2).unwrap();
        let kind = match keyword {
            b"fn" | b"def" | b"function" | b"func" | b"fun" | b"sub" => SymbolKind::Function,
            b"class" | b"object" => SymbolKind::Class,
            b"struct" => SymbolKind::Struct,
            b"interface" => SymbolKind::Interface,
            b"trait" => SymbolKind::Trait,
            b"enum" => SymbolKind::Enum,
            b"impl" => SymbolKind::Impl,
            b"module" | b"namespace" => SymbolKind::Module,
            b"type" => SymbolKind::Type,
            _ => SymbolKind::Constant,
        };
        // Constants only count at column 0; indented ones are locals.
        if kind == SymbolKind::Constant && matches!(source.get(whole.start()), Some(b' ' | b'\t')) {
            continue;
        }
        line += source[counted_to..whole.start()]
            .iter()
            .filter(|&&b| b == b'\n')
            .count() as u32;
        counted_to = whole.start();
        let line_end = source[whole.start()..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(source.len(), |p| whole.start() + p);
        let sig_start = whole.start()
            + source[whole.start()..line_end]
                .iter()
                .take_while(|b| b.is_ascii_whitespace())
                .count();
        out.push(Symbol {
            name: String::from_utf8_lossy(name.as_bytes()).into_owned(),
            kind,
            visibility: Visibility::Unknown,
            parent: None,
            start_line: line,
            end_line: line,
            start_byte: whole.start() as u32,
            end_byte: line_end as u32,
            signature: signature_of(source, sig_start, line_end, None),
            doc: None,
        });
    }
    out
}
