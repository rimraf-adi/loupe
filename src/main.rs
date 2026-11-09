mod cache;
mod editor;
mod mcp;
mod outline;
mod parser;
mod reader;
mod symbol;

use mcp::McpServer;
use std::env;
use std::path::PathBuf;

fn print_usage() {
    eprintln!(
        "Loupe v{} - Fast code reader & atomic batch editor for AI coding agents\n\n\
        USAGE:\n  \
          loupe [OPTIONS]                 Run as stdio MCP server\n  \
          loupe read <files...>           CLI tool to read files with line numbers\n  \
          loupe outline <path_or_glob>    CLI tool to inspect symbol tree\n  \
          loupe symbol <name> [path]      CLI tool to inspect symbol body, callees & callers\n  \
          loupe edit <file> [OPTIONS]     CLI tool to edit file with AST syntax validation\n\n\
        OPTIONS:\n  \
          --dir <PATH>   Base directory for resolving relative paths\n  \
          --help         Print help\n  \
          --version      Print version\n",
        env!("CARGO_PKG_VERSION")
    );
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    let mut working_dir: Option<PathBuf> = None;
    let mut idx = 1;

    while idx < args.len() {
        match args[idx].as_str() {
            "--help" | "-h" => {
                print_usage();
                return Ok(());
            }
            "--version" | "-v" => {
                println!("loupe {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--dir" => {
                idx += 1;
                if idx < args.len() {
                    working_dir = Some(PathBuf::from(&args[idx]));
                }
            }
            "read" => {
                let files: Vec<reader::FileReadTarget> = args[idx + 1..]
                    .iter()
                    .map(|f| reader::FileReadTarget::Simple(PathBuf::from(f)))
                    .collect();
                let mut cache = cache::SessionCache::new();
                let res = reader::execute_read_files(
                    reader::ReadFilesArgs {
                        files,
                        max_tokens: None,
                    },
                    &mut cache,
                    working_dir.as_deref(),
                );
                print!("{}", res.text);
                return Ok(());
            }
            "outline" => {
                let target = args.get(idx + 1).cloned();
                let (text, _, _) = outline::execute_outline(
                    outline::OutlineArgs {
                        path_or_glob: target,
                        paths: None,
                        max_tokens: None,
                    },
                    working_dir.as_deref(),
                );
                print!("{}", text);
                return Ok(());
            }
            "symbol" => {
                let name = match args.get(idx + 1) {
                    Some(n) => n.clone(),
                    None => {
                        eprintln!("Error: symbol name required");
                        std::process::exit(1);
                    }
                };
                let path = args.get(idx + 2).cloned();
                let (text, _) = symbol::execute_symbol(
                    symbol::SymbolArgs {
                        name,
                        path,
                        max_tokens: None,
                    },
                    working_dir.as_deref(),
                );
                print!("{}", text);
                return Ok(());
            }
            "edit" => {
                let file_path = match args.get(idx + 1) {
                    Some(f) => PathBuf::from(f),
                    None => {
                        eprintln!("Error: target file path required for edit");
                        std::process::exit(1);
                    }
                };

                let mut symbol: Option<String> = None;
                let mut start_line: Option<usize> = None;
                let mut end_line: Option<usize> = None;
                let mut old_text: Option<String> = None;
                let mut replacement: Option<String> = None;
                let mut create = false;

                let mut sub_idx = idx + 2;
                while sub_idx < args.len() {
                    match args[sub_idx].as_str() {
                        "--symbol" => {
                            sub_idx += 1;
                            symbol = args.get(sub_idx).cloned();
                        }
                        "--start" => {
                            sub_idx += 1;
                            start_line = args.get(sub_idx).and_then(|s| s.parse().ok());
                        }
                        "--end" => {
                            sub_idx += 1;
                            end_line = args.get(sub_idx).and_then(|s| s.parse().ok());
                        }
                        "--old" => {
                            sub_idx += 1;
                            old_text = args.get(sub_idx).cloned();
                        }
                        "--replace" | "--new" => {
                            sub_idx += 1;
                            replacement = args.get(sub_idx).cloned();
                        }
                        "--create" => {
                            create = true;
                        }
                        _ => {}
                    }
                    sub_idx += 1;
                }

                let repl = replacement.unwrap_or_default();
                let mut cache = cache::SessionCache::new();
                let res = editor::execute_edit_files(
                    editor::EditFilesArgs {
                        edits: vec![editor::SingleEdit {
                            path: file_path,
                            start_line,
                            end_line,
                            old_text,
                            symbol,
                            replacement: repl,
                            create,
                        }],
                        validate_syntax: true,
                        dry_run: false,
                    },
                    &mut cache,
                    working_dir.as_deref(),
                );

                if res.success {
                    println!("{}", res.message);
                    for item in res.files_modified {
                        let val_status = item
                            .syntax_validated
                            .map(|v| format!(" [✓ Syntax Validated: {}]", v))
                            .unwrap_or_default();
                        println!(
                            "  - {}: {} ({} -> {} lines){}",
                            item.path, item.details, item.original_lines, item.new_lines, val_status
                        );
                    }
                    return Ok(());
                } else {
                    eprintln!("Error: {}", res.message);
                    std::process::exit(1);
                }
            }
            _ => {}
        }
        idx += 1;
    }

    let mut server = McpServer::new(working_dir);
    server.run_stdio()?;
    Ok(())
}
