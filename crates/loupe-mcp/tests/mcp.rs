use std::fs;

use loupe_api::Index;
use loupe_core::Config;
use loupe_mcp::{McpServer, TOOL_NAMES, Tools};
use serde_json::{Value, json};

struct Env {
    repo: tempfile::TempDir,
    idx: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        let e = Env {
            repo: tempfile::tempdir().unwrap(),
            idx: tempfile::tempdir().unwrap(),
        };
        e.write(
            "README.md",
            "# Shop\n\nAn online shop with sessions and invoices.\n",
        );
        e.write("auth/token.rs", "/// Validate a signed session token.\npub fn validate_session_token(raw: &str) -> bool {\n    verify_signature(raw)\n}\n\npub fn verify_signature(raw: &str) -> bool { true }\n");
        e.write("auth/login.rs", "use crate::auth::token::validate_session_token;\npub fn login(u: &str) { let _ = validate_session_token(u); }\n");
        e.write("billing/invoice.rs", "/// Compute an invoice total including tax.\npub fn invoice_total(items: &[u64]) -> u64 { 0 }\n");
        e
    }
    fn write(&self, rel: &str, content: &str) {
        let p = self.repo.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }
    fn config(&self) -> Config {
        let mut c = Config::new(self.repo.path().canonicalize().unwrap());
        c.index_dir = self.idx.path().to_path_buf();
        c
    }
    fn index(&self) {
        let mut idx = Index::open_writer(self.config()).unwrap();
        idx.update(false).unwrap();
    }
    fn server(&self) -> McpServer {
        McpServer::new(Tools::open(self.config()).unwrap())
    }
}

fn rpc(s: &mut McpServer, id: u64, method: &str, params: Value) -> Value {
    let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    let line = s.handle_line(&req.to_string()).expect("a response");
    serde_json::from_str(&line).unwrap()
}

fn call(s: &mut McpServer, tool: &str, args: Value) -> (bool, String) {
    let r = rpc(s, 1, "tools/call", json!({"name": tool, "arguments": args}));
    assert!(r.get("error").is_none(), "protocol error: {r}");
    let res = &r["result"];
    (
        res["isError"].as_bool().unwrap(),
        res["content"][0]["text"].as_str().unwrap().to_string(),
    )
}

#[test]
fn handshake_and_tool_listing() {
    let env = Env::new();
    env.index();
    let mut s = env.server();
    let init = rpc(
        &mut s,
        1,
        "initialize",
        json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "loupe");
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert!(
        init["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("repo_map")
    );
    assert!(
        s.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_none(),
        "notifications get no reply"
    );
    assert_eq!(rpc(&mut s, 2, "ping", Value::Null)["result"], json!({}));

    let list = rpc(&mut s, 3, "tools/list", Value::Null);
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, TOOL_NAMES);
    // A client asking for a different revision is told which one we speak.
    let other = rpc(
        &mut s,
        4,
        "initialize",
        json!({"protocolVersion": "1999-01-01"}),
    );
    assert_eq!(other["result"]["protocolVersion"], "2024-11-05");
}

#[test]
fn malformed_input_gets_proper_json_rpc_errors() {
    let env = Env::new();
    env.index();
    let mut s = env.server();
    let parse: Value = serde_json::from_str(&s.handle_line("{not json").unwrap()).unwrap();
    assert_eq!(parse["error"]["code"], -32700);
    assert_eq!(parse["id"], Value::Null);
    let nf = rpc(&mut s, 5, "no/such/method", Value::Null);
    assert_eq!(nf["error"]["code"], -32601);
    let unknown = rpc(
        &mut s,
        6,
        "tools/call",
        json!({"name": "teleport", "arguments": {}}),
    );
    assert_eq!(unknown["error"]["code"], -32602);
    assert!(
        unknown["error"]["message"]
            .as_str()
            .unwrap()
            .contains("teleport")
    );
    let noname = rpc(&mut s, 7, "tools/call", json!({}));
    assert_eq!(noname["error"]["code"], -32602);
    let notobj: Value = serde_json::from_str(&s.handle_line("[1,2]").unwrap()).unwrap();
    assert_eq!(notobj["error"]["code"], -32600);
    assert!(s.handle_line("   ").is_none());
    // The server keeps working after every kind of bad input.
    assert_eq!(rpc(&mut s, 8, "ping", Value::Null)["result"], json!({}));
}

#[test]
fn bad_arguments_are_reported_to_the_model_not_as_protocol_errors() {
    let env = Env::new();
    env.index();
    let mut s = env.server();
    let (err, text) = call(&mut s, "search_text", json!({"patern": "x"}));
    assert!(
        err && text.contains("patern") && text.contains("valid arguments"),
        "{text}"
    );
    let (err, text) = call(&mut s, "outline", json!({}));
    assert!(err && text.contains("path"), "{text}");
    let (err, text) = call(
        &mut s,
        "find_symbol",
        json!({"name": "x", "kind": "banana"}),
    );
    assert!(err && text.contains("banana"), "{text}");
    let (err, text) = call(&mut s, "search_text", json!({"pattern": "("}));
    assert!(err && text.contains("regex"), "{text}");
    let (err, text) = call(&mut s, "outline", json!({"path": "nope.rs"}));
    assert!(err && text.contains("nope.rs"), "{text}");
    let (err, _) = call(&mut s, "search_text", json!({"pattern": 5}));
    assert!(err);
}

#[test]
fn each_tool_answers_and_stamps_the_snapshot() {
    let env = Env::new();
    env.index();
    let mut s = env.server();

    let (err, map) = call(&mut s, "repo_map", json!({"budget": 600}));
    assert!(!err);
    assert!(
        map.contains("Summary: An online shop with sessions and invoices."),
        "{map}"
    );
    assert!(map.contains("auth/") && map.contains("[snapshot "), "{map}");

    let (_, m) = call(&mut s, "module_summary", json!({"path": "auth"}));
    assert!(m.contains("auth/") && m.contains("2 files"), "{m}");
    assert!(
        call(&mut s, "module_summary", json!({}))
            .1
            .contains("(4 files)")
    );

    let (_, o) = call(&mut s, "outline", json!({"path": "auth/token.rs"}));
    assert!(o.contains("pub fn validate_session_token(raw: &str) -> bool  L2-4  // Validate a signed session token."), "{o}");

    let (_, q) = call(
        &mut s,
        "query",
        json!({"question": "where do we validate the session token"}),
    );
    assert!(
        q.lines().nth(1).unwrap().starts_with("auth/token.rs"),
        "{q}"
    );

    let (_, t) = call(
        &mut s,
        "search_text",
        json!({"pattern": "verify_signature", "literal": true}),
    );
    assert!(
        t.contains("auth/token.rs (2 matches)") && t.contains("3:"),
        "{t}"
    );
    let (_, w) = call(
        &mut s,
        "search_text",
        json!({"pattern": "login", "word": true}),
    );
    assert!(w.contains("auth/login.rs"), "{w}");

    let (_, sem) = call(
        &mut s,
        "search_semantic",
        json!({"query": "compute the invoice total with tax", "k": 3}),
    );
    assert!(sem.contains("billing/invoice.rs"), "{sem}");

    let (_, f) = call(
        &mut s,
        "find_symbol",
        json!({"name": "invoice_total", "kind": "function"}),
    );
    assert!(
        f.contains("billing/invoice.rs:2  pub fn invoice_total"),
        "{f}"
    );
    assert!(
        call(&mut s, "find_symbol", json!({"name": "ghost"}))
            .1
            .contains("No definitions named")
    );

    let (_, r) = call(&mut s, "refs", json!({"name": "validate_session_token"}));
    assert!(
        r.contains("Defined at:") && r.contains("auth/token.rs:2") && r.contains("auth/login.rs"),
        "{r}"
    );

    let (_, rel) = call(
        &mut s,
        "related",
        json!({"path": "auth/token.rs", "kinds": ["imports"]}),
    );
    assert!(
        rel.contains("auth/login.rs") && rel.contains("[imports]"),
        "{rel}"
    );
    let (_, imp) = call(&mut s, "impact", json!({"path": "auth/token.rs"}));
    assert!(imp.contains("depth 1  auth/login.rs"), "{imp}");
    assert!(
        call(&mut s, "impact", json!({"path": "billing/invoice.rs"}))
            .1
            .contains("Nothing depends")
    );

    for text in [&map, &o, &t, &f, &rel] {
        assert!(
            text.trim_end().ends_with(']') && text.contains("[snapshot 0000"),
            "{text}"
        );
    }
}

#[test]
fn budgets_are_honoured_by_every_tool_that_can_run_long() {
    let env = Env::new();
    for i in 0..60 {
        env.write(&format!("gen/file{i}.rs"), &format!("/// Helper number {i} for the widget system.\npub fn widget_helper_{i}() {{ let widget = {i}; }}\n"));
    }
    env.index();
    let mut s = env.server();
    for (tool, args) in [
        (
            "search_text",
            json!({"pattern": "widget", "max_files": 100}),
        ),
        ("repo_map", json!({"focus": ["gen"]})),
        (
            "query",
            json!({"question": "widget helper", "max_files": 50}),
        ),
        ("find_symbol", json!({"name": "widget_helper_1"})),
    ] {
        let mut small = args.clone();
        small["budget"] = json!(120);
        let (err, text) = call(&mut s, tool, small);
        assert!(!err, "{tool}: {text}");
        let tokens = text.len().div_ceil(4);
        assert!(
            tokens <= 140,
            "{tool} used {tokens} tokens for a 120-token budget:\n{text}"
        );
    }
    let (_, big) = call(
        &mut s,
        "search_text",
        json!({"pattern": "widget", "max_files": 100, "budget": 20000}),
    );
    let (_, small) = call(
        &mut s,
        "search_text",
        json!({"pattern": "widget", "max_files": 100, "budget": 150}),
    );
    assert!(
        small.len() < big.len() && small.contains("truncated to fit the 150-token budget"),
        "{small}"
    );
}

#[test]
fn the_server_follows_new_commits_and_reports_an_unindexed_repo() {
    let env = Env::new();
    // Nothing indexed yet: a clear, actionable message, and the server stays up.
    let mut s = env.server();
    let (err, text) = call(&mut s, "repo_map", json!({}));
    assert!(err && text.contains("nothing is indexed yet"), "{text}");
    env.index();
    let (err, text) = call(&mut s, "find_symbol", json!({"name": "invoice_total"}));
    assert!(
        !err && text.contains("billing/invoice.rs"),
        "the same server sees the first commit: {text}"
    );

    // A later commit by another process is picked up on the next call.
    env.write("billing/refund.rs", "pub fn issue_refund(order: u64) {}\n");
    env.index();
    let (_, text) = call(&mut s, "find_symbol", json!({"name": "issue_refund"}));
    assert!(text.contains("billing/refund.rs"), "{text}");
}

#[test]
fn stale_files_are_flagged_in_the_footer() {
    let env = Env::new();
    env.index();
    let mut s = env.server();
    std::thread::sleep(std::time::Duration::from_millis(20));
    env.write("billing/invoice.rs", "/// Compute an invoice total including tax (changed).\npub fn invoice_total(items: &[u64]) -> u64 { 7 }\n");
    let (_, t) = call(
        &mut s,
        "search_text",
        json!({"pattern": "invoice_total", "literal": true}),
    );
    assert!(t.contains("[stale]") && t.contains("STALE"), "{t}");
}
