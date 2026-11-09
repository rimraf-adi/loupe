use crate::cache::SessionCache;
use crate::outline::{extract_outline, SymbolOutline};
use crate::parser::SupportedLanguage;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SingleEdit {
    pub path: PathBuf,
    #[serde(default)]
    pub start_line: Option<usize>,
    #[serde(default)]
    pub end_line: Option<usize>,
    #[serde(default)]
    pub old_text: Option<String>,
    #[serde(default)]
    pub symbol: Option<String>,
    pub replacement: String,
    #[serde(default)]
    pub create: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EditFilesArgs {
    pub edits: Vec<SingleEdit>,
    #[serde(default = "default_true")]
    pub validate_syntax: bool,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EditResultItem {
    pub path: String,
    pub original_lines: usize,
    pub new_lines: usize,
    pub syntax_validated: Option<String>,
    pub details: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EditFilesResult {
    pub success: bool,
    pub message: String,
    pub files_modified: Vec<EditResultItem>,
}

pub fn execute_edit_files(
    args: EditFilesArgs,
    cache: &mut SessionCache,
    working_dir: Option<&Path>,
) -> EditFilesResult {
    let base_dir = working_dir.unwrap_or_else(|| Path::new("."));

    if args.edits.is_empty() {
        return EditFilesResult {
            success: false,
            message: "No edits provided".to_string(),
            files_modified: Vec::new(),
        };
    }

    // Group edits by file
    let mut file_edits_map: HashMap<PathBuf, Vec<SingleEdit>> = HashMap::new();
    for edit in args.edits {
        let abs_path = if edit.path.is_absolute() {
            edit.path.clone()
        } else {
            base_dir.join(&edit.path)
        };
        file_edits_map.entry(abs_path).or_default().push(edit);
    }

    // In-memory buffer of proposed file contents
    let mut modified_contents: HashMap<PathBuf, (String, usize, String)> = HashMap::new();
    // Path -> (new_content, original_lines, details)

    for (file_path, edits) in &file_edits_map {
        let (original_content, original_lines) = if file_path.exists() {
            match fs::read_to_string(file_path) {
                Ok(c) => {
                    let lines = c.lines().count();
                    (c, lines)
                }
                Err(e) => {
                    return EditFilesResult {
                        success: false,
                        message: format!("Failed to read {}: {}", file_path.display(), e),
                        files_modified: Vec::new(),
                    };
                }
            }
        } else {
            let allow_create = edits.iter().any(|e| e.create);
            if !allow_create {
                return EditFilesResult {
                    success: false,
                    message: format!(
                        "File {} does not exist and 'create' was not set to true",
                        file_path.display()
                    ),
                    files_modified: Vec::new(),
                };
            }
            (String::new(), 0)
        };

        let mut current_content = original_content;
        let mut edit_details = Vec::new();

        // Sort edits targeting line ranges in descending order of start_line
        // so that lower edits don't shift line numbers for higher edits
        let mut sorted_edits = edits.clone();
        sorted_edits.sort_by(|a, b| {
            let a_start = a.start_line.unwrap_or(0);
            let b_start = b.start_line.unwrap_or(0);
            b_start.cmp(&a_start)
        });

        for edit in &sorted_edits {
            if edit.create && !file_path.exists() && current_content.is_empty() {
                current_content = edit.replacement.clone();
                edit_details.push(format!("Created file ({} lines)", current_content.lines().count()));
                continue;
            }

            if let Some(ref sym_name) = edit.symbol {
                match apply_symbol_replacement(&current_content, file_path, sym_name, &edit.replacement) {
                    Ok(new_text) => {
                        current_content = new_text;
                        edit_details.push(format!("Replaced symbol '{}'", sym_name));
                    }
                    Err(e) => {
                        return EditFilesResult {
                            success: false,
                            message: format!("Failed in {}: {}", file_path.display(), e),
                            files_modified: Vec::new(),
                        };
                    }
                }
            } else if let (Some(start), Some(end)) = (edit.start_line, edit.end_line) {
                match replace_line_range(&current_content, start, end, &edit.replacement) {
                    Ok(new_text) => {
                        current_content = new_text;
                        edit_details.push(format!("Replaced lines {}-{}", start, end));
                    }
                    Err(e) => {
                        return EditFilesResult {
                            success: false,
                            message: format!("Failed in {}: {}", file_path.display(), e),
                            files_modified: Vec::new(),
                        };
                    }
                }
            } else if let Some(ref old_str) = edit.old_text {
                match replace_text(&current_content, old_str, &edit.replacement) {
                    Ok(new_text) => {
                        current_content = new_text;
                        edit_details.push("Replaced text match".to_string());
                    }
                    Err(e) => {
                        return EditFilesResult {
                            success: false,
                            message: format!("Failed in {}: {}", file_path.display(), e),
                            files_modified: Vec::new(),
                        };
                    }
                }
            } else if edit.create {
                current_content = edit.replacement.clone();
                edit_details.push("Overwrote full file".to_string());
            } else {
                return EditFilesResult {
                    success: false,
                    message: format!(
                        "Edit for {} must specify one of 'symbol', ('start_line' + 'end_line'), 'old_text', or 'create'",
                        file_path.display()
                    ),
                    files_modified: Vec::new(),
                };
            }
        }

        modified_contents.insert(
            file_path.clone(),
            (current_content, original_lines, edit_details.join(", ")),
        );
    }

    // Syntax validation in-memory BEFORE touching disk
    let mut validation_info = HashMap::new();
    if args.validate_syntax {
        for (file_path, (content, _, _)) in &modified_contents {
            if let Some(lang) = SupportedLanguage::from_path(file_path) {
                if let Err(err) = validate_file_syntax(content, lang) {
                    return EditFilesResult {
                        success: false,
                        message: format!(
                            "Validation Error in {}: {}. Atomic edit aborted; no files written.",
                            file_path.display(),
                            err
                        ),
                        files_modified: Vec::new(),
                    };
                }
                validation_info.insert(
                    file_path.clone(),
                    format!("{:?}", lang),
                );
            }
        }
    }

    // Dry-run mode: return preview without writing
    if args.dry_run {
        let mut results = Vec::new();
        for (path, (content, orig_lines, details)) in modified_contents {
            let rel_path = path
                .strip_prefix(base_dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            let new_lines = content.lines().count();
            results.push(EditResultItem {
                path: rel_path,
                original_lines: orig_lines,
                new_lines,
                syntax_validated: validation_info.get(&path).cloned(),
                details,
            });
        }
        return EditFilesResult {
            success: true,
            message: format!("[DRY RUN] Would update {} file(s)", results.len()),
            files_modified: results,
        };
    }

    // Write all files to disk atomically
    let mut results = Vec::new();
    for (path, (content, orig_lines, details)) in modified_contents {
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                if let Err(e) = fs::create_dir_all(parent) {
                    return EditFilesResult {
                        success: false,
                        message: format!("Failed to create parent directory for {}: {}", path.display(), e),
                        files_modified: Vec::new(),
                    };
                }
            }
        }

        if let Err(e) = fs::write(&path, &content) {
            return EditFilesResult {
                success: false,
                message: format!("Failed to write {}: {}", path.display(), e),
                files_modified: Vec::new(),
            };
        }

        // Invalidate or update cache entry for this file
        cache.invalidate(&path);

        let rel_path = path
            .strip_prefix(base_dir)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        let new_lines = content.lines().count();

        results.push(EditResultItem {
            path: rel_path,
            original_lines: orig_lines,
            new_lines,
            syntax_validated: validation_info.get(&path).cloned(),
            details,
        });
    }

    EditFilesResult {
        success: true,
        message: format!("Successfully updated {} file(s)", results.len()),
        files_modified: results,
    }
}

pub fn replace_line_range(
    content: &str,
    start_line: usize,
    end_line: usize,
    replacement: &str,
) -> Result<String> {
    if start_line == 0 {
        return Err(anyhow!("start_line must be >= 1"));
    }
    if start_line > end_line {
        return Err(anyhow!(
            "start_line ({}) cannot be greater than end_line ({})",
            start_line,
            end_line
        ));
    }

    let has_trailing_newline = content.ends_with('\n');
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    if start_line > total_lines + 1 {
        return Err(anyhow!(
            "start_line {} exceeds total lines in file ({})",
            start_line,
            total_lines
        ));
    }

    let mut out: Vec<&str> = Vec::new();

    // 1. Keep lines before start_line
    for i in 1..start_line {
        if i <= total_lines {
            out.push(lines[i - 1]);
        }
    }

    // 2. Insert replacement
    let repl_trimmed = replacement
        .strip_suffix("\r\n")
        .or_else(|| replacement.strip_suffix('\n'))
        .unwrap_or(replacement);

    if !repl_trimmed.is_empty() {
        for l in repl_trimmed.lines() {
            out.push(l);
        }
    }

    // 3. Keep lines after end_line
    if end_line < total_lines {
        for i in (end_line + 1)..=total_lines {
            out.push(lines[i - 1]);
        }
    }

    let mut result = out.join("\n");
    if has_trailing_newline || replacement.ends_with('\n') {
        result.push('\n');
    }

    Ok(result)
}

pub fn replace_text(content: &str, old_text: &str, new_text: &str) -> Result<String> {
    if !content.contains(old_text) {
        return Err(anyhow!("Target old_text not found in file content"));
    }
    let count = content.matches(old_text).count();
    if count > 1 {
        return Err(anyhow!(
            "Target old_text occurs {} times in file. Please specify a line range or provide unique surrounding lines.",
            count
        ));
    }
    Ok(content.replacen(old_text, new_text, 1))
}

fn apply_symbol_replacement(
    content: &str,
    file_path: &Path,
    sym_name: &str,
    replacement: &str,
) -> Result<String> {
    let lang = SupportedLanguage::from_path(file_path).ok_or_else(|| {
        anyhow!(
            "Language for {} not supported for symbol-targeted editing",
            file_path.display()
        )
    })?;

    let outlines = extract_outline(content, lang);
    let range = find_symbol_range(&outlines, sym_name).ok_or_else(|| {
        anyhow!(
            "Symbol '{}' not found in {}",
            sym_name,
            file_path.display()
        )
    })?;

    replace_line_range(content, range.0, range.1, replacement)
}

fn find_symbol_range(symbols: &[SymbolOutline], target_name: &str) -> Option<(usize, usize)> {
    for sym in symbols {
        if sym.name == target_name {
            return Some((sym.start_line, sym.end_line));
        }
        if let Some(res) = find_symbol_range(&sym.children, target_name) {
            return Some(res);
        }
    }
    None
}

pub fn validate_file_syntax(content: &str, lang: SupportedLanguage) -> Result<()> {
    let mut parser = match lang.make_parser() {
        Some(p) => p,
        None => return Ok(()),
    };

    let tree = match parser.parse(content, None) {
        Some(t) => t,
        None => return Err(anyhow!("Failed to parse source file")),
    };

    let root = tree.root_node();
    if root.has_error() {
        if let Some((line, col)) = find_first_error(&root) {
            return Err(anyhow!("Syntax error at line {}, column {}", line, col));
        }
        return Err(anyhow!("Syntax error detected in file"));
    }

    Ok(())
}

fn find_first_error(node: &tree_sitter::Node) -> Option<(usize, usize)> {
    if node.is_error() || node.is_missing() {
        return Some((
            node.start_position().row + 1,
            node.start_position().column + 1,
        ));
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.has_error() {
            if let Some(err) = find_first_error(&child) {
                return Some(err);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_replace_line_range() {
        let code = "fn one() {}\nfn two() {}\nfn three() {}\n";
        let replaced = replace_line_range(code, 2, 2, "fn new_two() {}").unwrap();
        assert_eq!(replaced, "fn one() {}\nfn new_two() {}\nfn three() {}\n");
    }

    #[test]
    fn test_replace_text() {
        let code = "let a = 1;\nlet b = 2;\n";
        let replaced = replace_text(code, "let b = 2;", "let b = 42;").unwrap();
        assert_eq!(replaced, "let a = 1;\nlet b = 42;\n");
    }

    #[test]
    fn test_symbol_replacement() {
        let code = "fn alpha() -> i32 {\n    1\n}\n\nfn beta() -> i32 {\n    2\n}\n";
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        fs::write(&file, code).unwrap();

        let mut cache = SessionCache::new();
        let args = EditFilesArgs {
            edits: vec![SingleEdit {
                path: PathBuf::from("test.rs"),
                start_line: None,
                end_line: None,
                old_text: None,
                symbol: Some("alpha".to_string()),
                replacement: "fn alpha() -> i32 {\n    999\n}".to_string(),
                create: false,
            }],
            validate_syntax: true,
            dry_run: false,
        };

        let res = execute_edit_files(args, &mut cache, Some(dir.path()));
        assert!(res.success);
        let updated = fs::read_to_string(&file).unwrap();
        assert!(updated.contains("999"));
        assert!(updated.contains("fn beta() -> i32"));
    }

    #[test]
    fn test_syntax_validation_rejects_bad_code() {
        let code = "fn alpha() {\n    let x = 1;\n}\n";
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        fs::write(&file, code).unwrap();

        let mut cache = SessionCache::new();
        let args = EditFilesArgs {
            edits: vec![SingleEdit {
                path: PathBuf::from("test.rs"),
                start_line: Some(2),
                end_line: Some(2),
                old_text: None,
                symbol: None,
                replacement: "    let x = ;".to_string(), // syntax error!
                create: false,
            }],
            validate_syntax: true,
            dry_run: false,
        };

        let res = execute_edit_files(args, &mut cache, Some(dir.path()));
        assert!(!res.success);
        assert!(res.message.contains("Syntax error"));

        // Verify disk file was NOT modified!
        let untouched = fs::read_to_string(&file).unwrap();
        assert_eq!(untouched, code);
    }
}
