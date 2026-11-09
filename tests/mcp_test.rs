use serde_json::json;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use tempfile::tempdir;

#[test]
fn test_mcp_full_session_flow() {
    let dir = tempdir().unwrap();
    let file_a = dir.path().join("a.rs");
    let file_b = dir.path().join("b.rs");

    let mut fa = File::create(&file_a).unwrap();
    writeln!(
        fa,
        "pub fn beta() -> i32 {{\n    10\n}}\n\npub fn alpha() -> i32 {{\n    beta() + 32\n}}"
    )
    .unwrap();

    let mut fb = File::create(&file_b).unwrap();
    for i in 1..=100 {
        if i == 50 {
            writeln!(fb, "let val = alpha();").unwrap();
        } else {
            writeln!(fb, "// Line number {i}").unwrap();
        }
    }

    // Spawn loupe process with base dir
    let mut child = Command::new(env!("CARGO_BIN_EXE_loupe"))
        .arg("--dir")
        .arg(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("Failed to spawn loupe binary");

    let mut stdin = child.stdin.take().expect("Failed to open stdin");
    let stdout = child.stdout.take().expect("Failed to open stdout");
    let mut reader = BufReader::new(stdout);

    // 1. Initialize
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {}
    });
    writeln!(stdin, "{}", serde_json::to_string(&init_req).unwrap()).unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let init_resp: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(init_resp["id"], 1);
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "loupe");

    // 2. Tools list
    let list_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    writeln!(stdin, "{}", serde_json::to_string(&list_req).unwrap()).unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let list_resp: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(list_resp["id"], 2);
    let tools = list_resp["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|t| t["name"] == "read_files"));
    assert!(tools.iter().any(|t| t["name"] == "outline"));
    assert!(tools.iter().any(|t| t["name"] == "symbol"));
    assert!(tools.iter().any(|t| t["name"] == "edit_files"));

    // 3. Call read_files on both files in one batched call
    let call_req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "read_files",
            "arguments": {
                "files": [
                    { "path": file_a.to_str().unwrap() },
                    {
                        "path": file_b.to_str().unwrap(),
                        "ranges": [[10, 15], [90, 95]]
                    }
                ]
            }
        }
    });
    writeln!(stdin, "{}", serde_json::to_string(&call_req).unwrap()).unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let call_resp: serde_json::Value = serde_json::from_str(&line).unwrap();
    let text = call_resp["result"]["content"][0]["text"].as_str().unwrap();

    assert!(text.contains("pub fn alpha() -> i32"));
    assert!(text.contains("// Line number 10"));
    assert!(text.contains("// Line number 15"));
    assert!(text.contains("// Line number 90"));
    assert!(text.contains("lines omitted"));

    // 4. Call read_files again on file_a (whole file) -> should return deduplication stub!
    let call_req_dedup = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "read_files",
            "arguments": {
                "files": [
                    { "path": file_a.to_str().unwrap() }
                ]
            }
        }
    });
    writeln!(stdin, "{}", serde_json::to_string(&call_req_dedup).unwrap()).unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let dedup_resp: serde_json::Value = serde_json::from_str(&line).unwrap();
    let dedup_text = dedup_resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(dedup_text.contains("UNCHANGED in session"));

    // 5. Call outline on a.rs
    let outline_req = json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "outline",
            "arguments": {
                "path_or_glob": file_a.to_str().unwrap()
            }
        }
    });
    writeln!(stdin, "{}", serde_json::to_string(&outline_req).unwrap()).unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let outline_resp: serde_json::Value = serde_json::from_str(&line).unwrap();
    let outline_text = outline_resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(outline_text.contains("alpha"));
    assert!(outline_text.contains("beta"));
    assert!(outline_text.contains("fn"));

    // 6. Call symbol on alpha
    let symbol_req = json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": {
            "name": "symbol",
            "arguments": {
                "name": "alpha"
            }
        }
    });
    writeln!(stdin, "{}", serde_json::to_string(&symbol_req).unwrap()).unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let symbol_resp: serde_json::Value = serde_json::from_str(&line).unwrap();
    let symbol_text = symbol_resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(symbol_text.contains("=== Symbol: alpha"));
    assert!(symbol_text.contains("beta() + 32"));
    assert!(symbol_text.contains("Calls (callees):"));
    assert!(symbol_text.contains("beta"));
    assert!(symbol_text.contains("Referenced by (callers):"));
    assert!(symbol_text.contains("let val = alpha();"));

    // 7. Call edit_files via MCP to replace symbol 'beta' with AST syntax validation
    let edit_req = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "edit_files",
            "arguments": {
                "edits": [
                    {
                        "path": file_a.to_str().unwrap(),
                        "symbol": "beta",
                        "replacement": "pub fn beta() -> i32 {\n    999\n}"
                    }
                ],
                "validate_syntax": true
            }
        }
    });
    writeln!(stdin, "{}", serde_json::to_string(&edit_req).unwrap()).unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let edit_resp: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(edit_resp["result"]["isError"], false);
    let edit_text = edit_resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(edit_text.contains("Successfully updated 1 file(s)"));
    assert!(edit_text.contains("Syntax Validated"));

    // 8. Re-read file_a to verify updated content is returned
    let reread_req = json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "tools/call",
        "params": {
            "name": "read_files",
            "arguments": {
                "files": [{ "path": file_a.to_str().unwrap() }]
            }
        }
    });
    writeln!(stdin, "{}", serde_json::to_string(&reread_req).unwrap()).unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let reread_resp: serde_json::Value = serde_json::from_str(&line).unwrap();
    let reread_text = reread_resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(reread_text.contains("999"));

    // 9. Call edit_files with syntax error -> verify rejection and atomic rollback
    let bad_edit_req = json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/call",
        "params": {
            "name": "edit_files",
            "arguments": {
                "edits": [
                    {
                        "path": file_a.to_str().unwrap(),
                        "symbol": "beta",
                        "replacement": "pub fn beta() -> i32 {\n    let bad = ;\n}"
                    }
                ],
                "validate_syntax": true
            }
        }
    });
    writeln!(stdin, "{}", serde_json::to_string(&bad_edit_req).unwrap()).unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let bad_resp: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(bad_resp["result"]["isError"], true);
    let bad_text = bad_resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(bad_text.contains("Syntax error"));
    assert!(bad_text.contains("Atomic edit aborted"));

    drop(stdin);
    let _ = child.wait();
}
