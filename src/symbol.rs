use crate::outline::extract_outline;
use crate::parser::SupportedLanguage;
use ignore::WalkBuilder;
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use tree_sitter::Node;

const DEFAULT_MAX_TOKENS: usize = 12_000;
const CHARS_PER_TOKEN: usize = 4;

#[derive(Debug, Clone, Deserialize)]
pub struct SymbolArgs {
    pub name: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<usize>,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct SymbolMatch {
    pub file_path: PathBuf,
    pub name: String,
    pub kind: String,
    pub signature: String,
    pub start_line: usize,
    pub end_line: usize,
    pub body: String,
    pub callees: Vec<CalleeInfo>,
    pub callers: Vec<CallerRef>,
}

#[derive(Debug)]
pub struct CalleeInfo {
    pub name: String,
    pub signature: Option<String>,
    pub location: Option<String>,
}

#[derive(Debug)]
pub struct CallerRef {
    pub file_path: String,
    pub line: usize,
    pub snippet: String,
}

pub fn execute_symbol(
    args: SymbolArgs,
    working_dir: Option<&Path>,
) -> (String, bool) {
    let base_dir = working_dir.unwrap_or_else(|| Path::new("."));
    let target_name = args.name.trim();

    // 1. Locate candidate files
    let candidate_files = match &args.path {
        Some(p) => {
            let path = Path::new(p);
            let effective = if path.is_absolute() {
                path.to_path_buf()
            } else {
                base_dir.join(path)
            };
            if effective.is_file() {
                vec![effective]
            } else {
                vec![]
            }
        }
        None => discover_source_files(base_dir),
    };

    if candidate_files.is_empty() {
        return (
            format!("No source files found to search for symbol '{}'\n", target_name),
            false,
        );
    }

    // 2. Search for symbol definition
    let mut matches = Vec::new();

    // Fast-path index of all outline symbols across candidates for callee resolution
    let mut project_symbol_sigs: BTreeMap<String, (String, String, usize)> = BTreeMap::new();
    // name -> (file_path, signature, line)

    for file_path in &candidate_files {
        let lang = match SupportedLanguage::from_path(file_path) {
            Some(l) => l,
            None => continue,
        };

        let content = match fs::read_to_string(file_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let outlines = extract_outline(&content, lang);
        let rel_path = file_path
            .strip_prefix(base_dir)
            .unwrap_or(file_path)
            .to_string_lossy()
            .to_string();

        for sym in &outlines {
            record_symbol_signatures(sym, &rel_path, &mut project_symbol_sigs);
        }

        // Check if target symbol is in this file
        if let Some(m) = find_symbol_in_source(
            file_path,
            &rel_path,
            &content,
            lang,
            target_name,
        ) {
            matches.push(m);
        }
    }

    if matches.is_empty() {
        return (
            format!(
                "Symbol '{}' not found in {} searched files.\n",
                target_name,
                candidate_files.len()
            ),
            false,
        );
    }

    // 3. Resolve callee signatures and callers
    for m in &mut matches {
        for callee in &mut m.callees {
            if let Some((loc_file, sig, line)) = project_symbol_sigs.get(&callee.name) {
                callee.signature = Some(sig.clone());
                callee.location = Some(format!("{}:L{}", loc_file, line));
            }
        }

        // Find callers
        m.callers = find_callers_in_project(&candidate_files, base_dir, &m.name, &m.file_path, m.start_line);
    }

    // 4. Format output with budget
    format_symbol_results(&matches, args.max_tokens)
}

fn record_symbol_signatures(
    sym: &crate::outline::SymbolOutline,
    rel_path: &str,
    map: &mut BTreeMap<String, (String, String, usize)>,
) {
    if !sym.name.is_empty() {
        map.insert(sym.name.clone(), (rel_path.to_string(), sym.signature.clone(), sym.start_line));
    }
    for child in &sym.children {
        record_symbol_signatures(child, rel_path, map);
    }
}

fn discover_source_files(base_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let walker = WalkBuilder::new(base_dir)
        .standard_filters(true)
        .hidden(true)
        .build();

    for entry in walker.flatten() {
        if entry.file_type().map_or(false, |ft| ft.is_file()) {
            let path = entry.path();
            if SupportedLanguage::from_path(path).is_some() {
                // Ignore build artifacts directories if not ignored by standard_filters
                let path_str = path.to_string_lossy();
                if !path_str.contains("/target/")
                    && !path_str.contains("/node_modules/")
                    && !path_str.contains("/.git/")
                {
                    files.push(path.to_path_buf());
                }
            }
        }
    }
    files
}

fn find_symbol_in_source(
    file_path: &Path,
    rel_path: &str,
    source: &str,
    lang: SupportedLanguage,
    target_name: &str,
) -> Option<SymbolMatch> {
    let mut parser = lang.make_parser()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    let source_bytes = source.as_bytes();

    let node = find_node_matching_symbol(root, source_bytes, lang, target_name)?;
    let start_line = node.start_position().row + 1;
    let end_line = node.end_position().row + 1;

    let body_lines: Vec<&str> = source.lines().collect();
    let line_width = format!("{}", end_line).len().max(3);
    let mut formatted_body = String::new();

    for idx in start_line..=end_line {
        if let Some(line) = body_lines.get(idx - 1) {
            formatted_body.push_str(&format!(
                "{:>width$} | {}\n",
                idx,
                line,
                width = line_width
            ));
        }
    }

    let callees = extract_callees_from_node(node, source_bytes);

    Some(SymbolMatch {
        file_path: file_path.to_path_buf(),
        name: target_name.to_string(),
        kind: node.kind().to_string(),
        signature: format!("{}:L{}-L{}", rel_path, start_line, end_line),
        start_line,
        end_line,
        body: formatted_body,
        callees,
        callers: vec![],
    })
}

fn find_node_matching_symbol<'a>(
    node: Node<'a>,
    source: &'a [u8],
    lang: SupportedLanguage,
    target_name: &str,
) -> Option<Node<'a>> {
    let kind = node.kind();
    let is_definition = match lang {
        SupportedLanguage::Rust => matches!(
            kind,
            "function_item" | "struct_item" | "enum_item" | "trait_item" | "type_item"
        ),
        SupportedLanguage::Python => {
            matches!(kind, "function_definition" | "class_definition")
        }
        SupportedLanguage::TypeScript | SupportedLanguage::Tsx => matches!(
            kind,
            "function_declaration"
                | "method_definition"
                | "class_declaration"
                | "interface_declaration"
                | "type_alias_declaration"
        ),
    };

    if is_definition {
        if let Some(name_node) = node.child_by_field_name("name") {
            if let Ok(text) = name_node.utf8_text(source) {
                if text == target_name {
                    return Some(node);
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = find_node_matching_symbol(child, source, lang, target_name) {
            return Some(found);
        }
    }

    None
}

fn extract_callees_from_node(node: Node, source: &[u8]) -> Vec<CalleeInfo> {
    let mut seen = HashSet::new();
    let mut callees = Vec::new();
    let mut stack = vec![node];

    while let Some(curr) = stack.pop() {
        let kind = curr.kind();
        if kind == "call_expression" || kind == "call" {
            let func_node = curr.child_by_field_name("function").or_else(|| curr.child(0));
            if let Some(fn_node) = func_node {
                // If method call e.g. foo.bar(), extract "bar" or "foo.bar"
                let name = match fn_node.kind() {
                    "field_expression" | "attribute" | "member_expression" => {
                        fn_node
                            .child_by_field_name("field")
                            .or_else(|| fn_node.child_by_field_name("property"))
                            .or_else(|| fn_node.child_by_field_name("attribute"))
                            .and_then(|n| n.utf8_text(source).ok())
                            .unwrap_or("")
                    }
                    _ => fn_node.utf8_text(source).unwrap_or(""),
                };

                let trimmed = name.trim();
                if !trimmed.is_empty()
                    && !seen.contains(trimmed)
                    && trimmed.chars().all(|c| c.is_alphanumeric() || c == '_')
                {
                    seen.insert(trimmed.to_string());
                    callees.push(CalleeInfo {
                        name: trimmed.to_string(),
                        signature: None,
                        location: None,
                    });
                }
            }
        }

        let mut cursor = curr.walk();
        for child in curr.children(&mut cursor) {
            stack.push(child);
        }
    }

    callees.sort_by(|a, b| a.name.cmp(&b.name));
    callees
}

fn find_callers_in_project(
    candidates: &[PathBuf],
    base_dir: &Path,
    symbol_name: &str,
    def_file: &Path,
    def_line: usize,
) -> Vec<CallerRef> {
    let mut callers = Vec::new();
    let call_pattern = format!("{}(", symbol_name);

    for file_path in candidates {
        let is_same_file = file_path == def_file;
        let content = match fs::read_to_string(file_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let rel_path = file_path
            .strip_prefix(base_dir)
            .unwrap_or(file_path)
            .to_string_lossy()
            .to_string();

        for (idx, line) in content.lines().enumerate() {
            let line_num = idx + 1;
            if is_same_file && line_num == def_line {
                continue; // Skip the definition line itself
            }

            if line.contains(&call_pattern) || line.contains(symbol_name) {
                let trimmed = line.trim();
                // Filter out comments / imports if trivial
                if !trimmed.starts_with("//") && !trimmed.starts_with('#') {
                    callers.push(CallerRef {
                        file_path: rel_path.clone(),
                        line: line_num,
                        snippet: trimmed.to_string(),
                    });
                    if callers.len() >= 25 {
                        return callers;
                    }
                }
            }
        }
    }

    callers
}

fn format_symbol_results(
    matches: &[SymbolMatch],
    max_tokens: Option<usize>,
) -> (String, bool) {
    let budget_tokens = max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    let mut remaining_chars = budget_tokens.saturating_mul(CHARS_PER_TOKEN);
    let mut output = String::new();
    let mut truncated = false;

    for m in matches {
        let header = format!(
            "=== Symbol: {} ({}, L{}-L{}) ===\n",
            m.name, m.file_path.display(), m.start_line, m.end_line
        );
        if remaining_chars < header.len() {
            truncated = true;
            break;
        }
        remaining_chars -= header.len();
        output.push_str(&header);

        // Body
        output.push_str(&m.body);
        output.push('\n');

        // Callees
        if !m.callees.is_empty() {
            let mut callee_section = String::from("Calls (callees):\n");
            for c in &m.callees {
                if let (Some(sig), Some(loc)) = (&c.signature, &c.location) {
                    callee_section.push_str(&format!("  - {} ({})\n", sig, loc));
                } else {
                    callee_section.push_str(&format!("  - {} (external/unindexed)\n", c.name));
                }
            }
            callee_section.push('\n');
            output.push_str(&callee_section);
        }

        // Callers
        if !m.callers.is_empty() {
            let mut caller_section = String::from("Referenced by (callers):\n");
            for r in &m.callers {
                caller_section.push_str(&format!(
                    "  - {}:L{}: {}\n",
                    r.file_path, r.line, r.snippet
                ));
            }
            caller_section.push('\n');
            output.push_str(&caller_section);
        }
    }

    (output, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use std::fs::File;
    use std::io::Write;

    #[test]
    fn test_execute_symbol() {
        let dir = tempdir().unwrap();
        let main_path = dir.path().join("main.rs");
        let helper_path = dir.path().join("helper.rs");

        let mut f_helper = File::create(&helper_path).unwrap();
        writeln!(f_helper, "pub fn calculate(val: i32) -> i32 {{\n    val * 2\n}}").unwrap();

        let mut f_main = File::create(&main_path).unwrap();
        writeln!(
            f_main,
            "mod helper;\n\npub fn process() {{\n    let x = calculate(10);\n}}\n\nfn main() {{\n    process();\n}}"
        )
        .unwrap();

        let args = SymbolArgs {
            name: "process".to_string(),
            path: None,
            max_tokens: Some(5000),
        };

        let (text, truncated) = execute_symbol(args, Some(dir.path()));
        assert!(!truncated);
        assert!(text.contains("=== Symbol: process"));
        assert!(text.contains("let x = calculate(10);"));
        assert!(text.contains("calculate"));
        assert!(text.contains("Referenced by"));
        assert!(text.contains("process();"));
    }
}
