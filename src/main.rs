mod cache;
mod mcp;
mod reader;

use mcp::McpServer;
use std::env;
use std::path::PathBuf;

fn print_usage() {
    eprintln!(
        "Loupe v{} - Fast code reader MCP server for AI coding agents\n\n\
        USAGE:\n  \
          loupe [OPTIONS]\n  \
          loupe read <files...>\n\n\
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
                // CLI convenience mode for testing
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
            _ => {}
        }
        idx += 1;
    }

    let mut server = McpServer::new(working_dir);
    server.run_stdio()?;
    Ok(())
}
