use crate::parser::SupportedLanguage;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use tree_sitter::Node;

const DEFAULT_MAX_TOKENS: usize = 12_000;
const CHARS_PER_TOKEN: usize = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolOutline {
    pub name: String,
    pub kind: String,
    pub signature: String,
    pub start_line: usize,
    pub end_line: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<SymbolOutline>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OutlineArgs {
    /// Path to a file or a glob pattern (e.g. "src/**/*.rs" or "src/main.rs")
    pub path_or_glob: Option<String>,
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    #[serde(default)]
    pub max_tokens: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct FileOutline {
    pub path: String,
    pub total_lines: usize,
    pub symbols: Vec<SymbolOutline>,
}

pub fn extract_outline(source: &str, lang: SupportedLanguage) -> Vec<SymbolOutline> {
    let mut parser = match lang.make_parser() {
        Some(p) => p,
        None => return Vec::new(),
    };

    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let root = tree.root_node();
    let source_bytes = source.as_bytes();
    extract_symbols_from_node(root, source_bytes, lang)
}

fn extract_symbols_from_node(
    node: Node,
    source: &[u8],
    lang: SupportedLanguage,
) -> Vec<SymbolOutline> {
    let mut symbols = Vec::new();
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        match lang {
            SupportedLanguage::Rust => {
                if let Some(sym) = extract_rust_symbol(child, source) {
                    symbols.push(sym);
                } else {
                    // Recurse into mod blocks or sub-blocks if needed
                    if child.kind() == "mod_item" {
                        if let Some(body) = child.child_by_field_name("body") {
                            let sub_syms = extract_symbols_from_node(body, source, lang);
                            let mod_name = child
                                .child_by_field_name("name")
                                .and_then(|n| n.utf8_text(source).ok())
                                .unwrap_or("mod");
                            symbols.push(SymbolOutline {
                                name: mod_name.to_string(),
                                kind: "mod".to_string(),
                                signature: format!("mod {}", mod_name),
                                start_line: child.start_position().row + 1,
                                end_line: child.end_position().row + 1,
                                children: sub_syms,
                            });
                        }
                    }
                }
            }
            SupportedLanguage::Python => {
                if let Some(sym) = extract_python_symbol(child, source) {
                    symbols.push(sym);
                }
            }
            SupportedLanguage::TypeScript | SupportedLanguage::Tsx => {
                if let Some(sym) = extract_ts_symbol(child, source, lang) {
                    symbols.push(sym);
                }
            }
        }
    }

    symbols
}

fn extract_rust_symbol(node: Node, source: &[u8]) -> Option<SymbolOutline> {
    let kind = node.kind();
    let start_line = node.start_position().row + 1;
    let end_line = node.end_position().row + 1;

    match kind {
        "function_item" => {
            let name_node = node.child_by_field_name("name")?;
            let name = name_node.utf8_text(source).ok()?.to_string();
            let sig = extract_signature_until(node, source, &["{", ";"]);
            Some(SymbolOutline {
                name,
                kind: "fn".to_string(),
                signature: sig,
                start_line,
                end_line,
                children: vec![],
            })
        }
        "struct_item" => {
            let name_node = node.child_by_field_name("name")?;
            let name = name_node.utf8_text(source).ok()?.to_string();
            let sig = extract_signature_until(node, source, &["{", ";"]);
            Some(SymbolOutline {
                name,
                kind: "struct".to_string(),
                signature: sig,
                start_line,
                end_line,
                children: vec![],
            })
        }
        "enum_item" => {
            let name_node = node.child_by_field_name("name")?;
            let name = name_node.utf8_text(source).ok()?.to_string();
            let sig = extract_signature_until(node, source, &["{"]);
            Some(SymbolOutline {
                name,
                kind: "enum".to_string(),
                signature: sig,
                start_line,
                end_line,
                children: vec![],
            })
        }
        "trait_item" => {
            let name_node = node.child_by_field_name("name")?;
            let name = name_node.utf8_text(source).ok()?.to_string();
            let sig = extract_signature_until(node, source, &["{"]);
            let mut children = vec![];
            if let Some(body) = node.child_by_field_name("body") {
                children = extract_symbols_from_node(body, source, SupportedLanguage::Rust);
            }
            Some(SymbolOutline {
                name,
                kind: "trait".to_string(),
                signature: sig,
                start_line,
                end_line,
                children,
            })
        }
        "impl_item" => {
            let sig = extract_signature_until(node, source, &["{"]);
            let mut children = vec![];
            if let Some(body) = node.child_by_field_name("body") {
                children = extract_symbols_from_node(body, source, SupportedLanguage::Rust);
            }
            Some(SymbolOutline {
                name: sig.clone(),
                kind: "impl".to_string(),
                signature: sig,
                start_line,
                end_line,
                children,
            })
        }
        "type_item" => {
            let name_node = node.child_by_field_name("name")?;
            let name = name_node.utf8_text(source).ok()?.to_string();
            let sig = extract_signature_until(node, source, &[";"]);
            Some(SymbolOutline {
                name,
                kind: "type".to_string(),
                signature: sig,
                start_line,
                end_line,
                children: vec![],
            })
        }
        _ => None,
    }
}

fn extract_python_symbol(node: Node, source: &[u8]) -> Option<SymbolOutline> {
    let kind = node.kind();
    let start_line = node.start_position().row + 1;
    let end_line = node.end_position().row + 1;

    match kind {
        "function_definition" => {
            let name_node = node.child_by_field_name("name")?;
            let name = name_node.utf8_text(source).ok()?.to_string();
            let sig = extract_signature_until(node, source, &[":"]);
            Some(SymbolOutline {
                name,
                kind: "def".to_string(),
                signature: sig,
                start_line,
                end_line,
                children: vec![],
            })
        }
        "class_definition" => {
            let name_node = node.child_by_field_name("name")?;
            let name = name_node.utf8_text(source).ok()?.to_string();
            let sig = extract_signature_until(node, source, &[":"]);
            let mut children = vec![];
            if let Some(body) = node.child_by_field_name("body") {
                children = extract_symbols_from_node(body, source, SupportedLanguage::Python);
            }
            Some(SymbolOutline {
                name,
                kind: "class".to_string(),
                signature: sig,
                start_line,
                end_line,
                children,
            })
        }
        _ => None,
    }
}

fn extract_ts_symbol(node: Node, source: &[u8], lang: SupportedLanguage) -> Option<SymbolOutline> {
    let kind = node.kind();
    let start_line = node.start_position().row + 1;
    let end_line = node.end_position().row + 1;

    // Handle export wrapper e.g. `export function ...`
    if kind == "export_statement" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(mut sym) = extract_ts_symbol(child, source, lang) {
                sym.signature = format!("export {}", sym.signature);
                return Some(sym);
            }
        }
        return None;
    }

    match kind {
        "function_declaration" | "method_definition" => {
            let name_node = node.child_by_field_name("name")?;
            let name = name_node.utf8_text(source).ok()?.to_string();
            let sig = extract_signature_until(node, source, &["{", ";"]);
            Some(SymbolOutline {
                name,
                kind: "function".to_string(),
                signature: sig,
                start_line,
                end_line,
                children: vec![],
            })
        }
        "class_declaration" => {
            let name_node = node.child_by_field_name("name")?;
            let name = name_node.utf8_text(source).ok()?.to_string();
            let sig = extract_signature_until(node, source, &["{"]);
            let mut children = vec![];
            if let Some(body) = node.child_by_field_name("body") {
                children = extract_symbols_from_node(body, source, lang);
            }
            Some(SymbolOutline {
                name,
                kind: "class".to_string(),
                signature: sig,
                start_line,
                end_line,
                children,
            })
        }
        "interface_declaration" => {
            let name_node = node.child_by_field_name("name")?;
            let name = name_node.utf8_text(source).ok()?.to_string();
            let sig = extract_signature_until(node, source, &["{"]);
            Some(SymbolOutline {
                name,
                kind: "interface".to_string(),
                signature: sig,
                start_line,
                end_line,
                children: vec![],
            })
        }
        "type_alias_declaration" => {
            let name_node = node.child_by_field_name("name")?;
            let name = name_node.utf8_text(source).ok()?.to_string();
            let sig = extract_signature_until(node, source, &[";"]);
            Some(SymbolOutline {
                name,
                kind: "type".to_string(),
                signature: sig,
                start_line,
                end_line,
                children: vec![],
            })
        }
        _ => None,
    }
}

fn extract_signature_until(node: Node, source: &[u8], terminators: &[&str]) -> String {
    let raw = match node.utf8_text(source) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };

    let mut end_pos = raw.len();
    for term in terminators {
        if let Some(idx) = raw.find(term) {
            if idx < end_pos {
                end_pos = idx;
            }
        }
    }

    let sig = raw[..end_pos].trim();
    // Compact internal whitespace
    sig.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn format_outline(
    outlines: &[FileOutline],
    max_tokens: Option<usize>,
) -> (String, bool) {
    let budget_tokens = max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    let mut remaining_chars = budget_tokens.saturating_mul(CHARS_PER_TOKEN);
    let mut output = String::new();
    let mut truncated = false;

    for file in outlines {
        if remaining_chars == 0 {
            output.push_str("\n[Token budget limit reached. Omitted remaining file outlines.]\n");
            truncated = true;
            break;
        }

        let header = format!(
            "=== Outline: {} ({} symbols, {} lines) ===\n",
            file.path,
            count_symbols(&file.symbols),
            file.total_lines
        );

        if remaining_chars < header.len() {
            output.push_str("\n[Token budget limit reached.]\n");
            truncated = true;
            break;
        }

        remaining_chars -= header.len();
        output.push_str(&header);

        for sym in &file.symbols {
            format_symbol_tree(sym, 0, &mut output, &mut remaining_chars, &mut truncated);
            if remaining_chars == 0 {
                break;
            }
        }
        output.push('\n');
    }

    (output, truncated)
}

fn count_symbols(symbols: &[SymbolOutline]) -> usize {
    let mut c = symbols.len();
    for s in symbols {
        c += count_symbols(&s.children);
    }
    c
}

fn format_symbol_tree(
    sym: &SymbolOutline,
    indent_level: usize,
    output: &mut String,
    remaining_chars: &mut usize,
    truncated: &mut bool,
) {
    let indent = "  ".repeat(indent_level);
    let line_range = format!("L{}-L{}", sym.start_line, sym.end_line);
    let line = format!(
        "{}  {:<10} {:<6} {}\n",
        indent, line_range, sym.kind, sym.signature
    );

    if *remaining_chars < line.len() {
        output.push_str("  [... additional symbols omitted due to budget ...]\n");
        *truncated = true;
        *remaining_chars = 0;
        return;
    }

    *remaining_chars -= line.len();
    output.push_str(&line);

    for child in &sym.children {
        format_symbol_tree(child, indent_level + 1, output, remaining_chars, truncated);
        if *remaining_chars == 0 {
            break;
        }
    }
}

pub fn execute_outline(
    args: OutlineArgs,
    working_dir: Option<&Path>,
) -> (String, usize, bool) {
    let mut paths_to_check = Vec::new();

    if let Some(target) = args.path_or_glob {
        paths_to_check.push(target);
    }
    if let Some(extra_paths) = args.paths {
        paths_to_check.extend(extra_paths);
    }

    if paths_to_check.is_empty() {
        paths_to_check.push("src/**/*.rs".to_string());
    }

    let base_dir = working_dir.unwrap_or_else(|| Path::new("."));
    let mut resolved_files = Vec::new();

    for item in &paths_to_check {
        if item.contains('*') || item.contains('?') {
            // Glob pattern
            let full_pattern = if Path::new(item).is_absolute() {
                item.clone()
            } else {
                base_dir.join(item).to_string_lossy().to_string()
            };

            if let Ok(paths) = glob::glob(&full_pattern) {
                for entry in paths.flatten() {
                    if entry.is_file() {
                        resolved_files.push(entry);
                    }
                }
            }
        } else {
            let p = Path::new(item);
            let effective = if p.is_absolute() {
                p.to_path_buf()
            } else {
                base_dir.join(p)
            };
            if effective.is_file() {
                resolved_files.push(effective);
            }
        }
    }

    resolved_files.sort();
    resolved_files.dedup();

    if resolved_files.is_empty() {
        return (
            format!(
                "No matching files found for: {}\n",
                paths_to_check.join(", ")
            ),
            0,
            false,
        );
    }

    let mut outlines = Vec::new();
    for file_path in resolved_files {
        let lang = match SupportedLanguage::from_path(&file_path) {
            Some(l) => l,
            None => continue,
        };

        let content = match fs::read_to_string(&file_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let total_lines = content.lines().count();
        let symbols = extract_outline(&content, lang);
        let display_path = file_path
            .strip_prefix(base_dir)
            .unwrap_or(&file_path)
            .to_string_lossy()
            .to_string();

        outlines.push(FileOutline {
            path: display_path,
            total_lines,
            symbols,
        });
    }

    let (text, truncated) = format_outline(&outlines, args.max_tokens);
    let total_symbols = outlines.iter().map(|o| count_symbols(&o.symbols)).sum();
    (text, total_symbols, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rust_outline() {
        let code = r#"
pub struct User {
    pub id: u64,
}

impl User {
    pub fn new(id: u64) -> Self {
        Self { id }
    }
}

pub enum Role {
    Admin,
    Member,
}

pub fn calculate_score(user: &User) -> u32 {
    100
}
"#;
        let syms = extract_outline(code, SupportedLanguage::Rust);
        assert_eq!(syms.len(), 4);
        assert_eq!(syms[0].name, "User");
        assert_eq!(syms[0].kind, "struct");
        assert_eq!(syms[1].kind, "impl");
        assert_eq!(syms[1].children.len(), 1);
        assert_eq!(syms[1].children[0].name, "new");
        assert_eq!(syms[2].name, "Role");
        assert_eq!(syms[2].kind, "enum");
        assert_eq!(syms[3].name, "calculate_score");
        assert_eq!(syms[3].kind, "fn");
    }

    #[test]
    fn test_python_outline() {
        let code = r#"
class Database:
    def connect(self):
        pass

def query_all():
    return []
"#;
        let syms = extract_outline(code, SupportedLanguage::Python);
        assert_eq!(syms.len(), 2);
        assert_eq!(syms[0].name, "Database");
        assert_eq!(syms[0].kind, "class");
        assert_eq!(syms[0].children.len(), 1);
        assert_eq!(syms[0].children[0].name, "connect");
        assert_eq!(syms[1].name, "query_all");
    }

    #[test]
    fn test_ts_outline() {
        let code = r#"
export interface Item {
    id: string;
}

export class Service {
    run(): void {}
}

export function helper(): boolean {
    return true;
}
"#;
        let syms = extract_outline(code, SupportedLanguage::TypeScript);
        assert_eq!(syms.len(), 3);
        assert_eq!(syms[0].name, "Item");
        assert_eq!(syms[0].kind, "interface");
        assert_eq!(syms[1].name, "Service");
        assert_eq!(syms[1].kind, "class");
        assert_eq!(syms[2].name, "helper");
        assert_eq!(syms[2].kind, "function");
    }
}
