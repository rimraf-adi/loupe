//! A scripted "agent" talks to the real `loupe mcp` binary over stdio and completes a code
//! localization task using tool calls only (no file reads, no grep): orient with `repo_map`,
//! ask `query`, zoom with `outline`, confirm with `refs`.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

struct Agent {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Agent {
    fn start(root: &std::path::Path, index_dir: &std::path::Path) -> Agent {
        let mut child = Command::new(env!("CARGO_BIN_EXE_loupe"))
            .args(["mcp", "-C"])
            .arg(root)
            .arg("--index-dir")
            .arg(index_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Agent {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn send(&mut self, v: Value) {
        writeln!(self.stdin, "{v}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let resp: Value =
            serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad response {line:?}: {e}"));
        assert_eq!(resp["id"], id);
        resp
    }

    fn tool(&mut self, name: &str, args: Value) -> String {
        let r = self.request("tools/call", json!({"name": name, "arguments": args}));
        assert!(r["error"].is_null(), "{r}");
        assert_eq!(
            r["result"]["isError"], false,
            "{}",
            r["result"]["content"][0]["text"]
        );
        r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write(root: &std::path::Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

#[test]
fn an_agent_can_localize_a_change_using_tool_calls_only() {
    let repo = tempfile::tempdir().unwrap();
    let idx = tempfile::tempdir().unwrap();
    let root = repo.path().canonicalize().unwrap();
    write(
        &root,
        "README.md",
        "# Gateway\n\nAn HTTP gateway that authenticates requests and forwards them.\n",
    );
    write(
        &root,
        "src/main.rs",
        "mod auth;\nmod proxy;\nfn main() { proxy::run(); }\n",
    );
    write(
        &root,
        "src/auth/mod.rs",
        "//! Request authentication.\npub mod session;\n",
    );
    write(
        &root,
        "src/auth/session.rs",
        "/// Check that a session cookie has not expired.\npub fn session_is_expired(cookie: &Cookie, now: u64) -> bool {\n    cookie.issued_at + SESSION_TTL < now\n}\n\npub const SESSION_TTL: u64 = 3600;\n",
    );
    write(
        &root,
        "src/proxy/mod.rs",
        "use crate::auth::session::session_is_expired;\npub fn run() {}\npub fn handle(cookie: &Cookie) -> bool {\n    !session_is_expired(cookie, 0)\n}\n",
    );
    write(
        &root,
        "src/metrics.rs",
        "pub fn record_latency_histogram(ms: u64) {}\n",
    );

    // Index it (a separate writer process, as in real use).
    let mut config = loupe_core::Config::new(&root);
    config.index_dir = idx.path().to_path_buf();
    loupe_api::Index::open_writer(config)
        .unwrap()
        .update(false)
        .unwrap();

    let mut agent = Agent::start(&root, idx.path());

    // Handshake, as an MCP client does.
    let init = agent.request("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "script", "version": "0"}}));
    assert_eq!(init["result"]["serverInfo"]["name"], "loupe");
    agent.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let tools = agent.request("tools/list", Value::Null);
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for needed in [
        "repo_map",
        "query",
        "outline",
        "refs",
        "search_text",
        "find_symbol",
    ] {
        assert!(names.contains(&needed), "{needed} missing from {names:?}");
    }

    // Task: "session expiry is computed wrongly; find where, and what else is affected."
    // 1. Orient.
    let map = agent.tool("repo_map", json!({"budget": 500}));
    assert!(
        map.contains("authenticates requests"),
        "the map says what the repo is: {map}"
    );
    assert!(map.contains("auth/"), "{map}");

    // 2. Ask in natural language.
    let found = agent.tool(
        "query",
        json!({"question": "where do we check whether a session has expired"}),
    );
    let top = found.lines().nth(1).unwrap_or("");
    assert!(
        top.starts_with("src/auth/session.rs"),
        "the definition ranks first:\n{found}"
    );
    assert!(found.contains("session_is_expired"), "{found}");

    // 3. Zoom into the file without reading it whole.
    let outline = agent.tool("outline", json!({"path": "src/auth/session.rs"}));
    assert!(
        outline.contains("pub fn session_is_expired(cookie: &Cookie, now: u64) -> bool  L2-4"),
        "{outline}"
    );
    assert!(outline.contains("pub const SESSION_TTL"), "{outline}");

    // 4. Who else is affected?
    let refs = agent.tool("refs", json!({"name": "session_is_expired"}));
    assert!(
        refs.contains("src/proxy/mod.rs"),
        "the caller shows up: {refs}"
    );
    let impact = agent.tool("impact", json!({"path": "src/auth/session.rs"}));
    assert!(impact.contains("src/proxy/mod.rs"), "{impact}");

    // Every answer is small and stamped.
    for text in [&map, &found, &outline, &refs, &impact] {
        assert!(text.contains("[snapshot "), "{text}");
        assert!(
            text.len() / 4 < 600,
            "answers stay small: {} tokens",
            text.len() / 4
        );
    }

    // Bad input is survivable: the agent reads the message and retries.
    let bad = agent.request(
        "tools/call",
        json!({"name": "outline", "arguments": {"file": "x"}}),
    );
    assert_eq!(bad["result"]["isError"], true);
    assert!(
        bad["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("valid arguments")
    );
    let retry = agent.tool("outline", json!({"path": "src/metrics.rs"}));
    assert!(retry.contains("record_latency_histogram"));
}
