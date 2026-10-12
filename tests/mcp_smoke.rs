//! End-to-end MCP smoke: spawn `zoanthrope mcp`, run the initialize
//! handshake, list tools, and do a claim_files grant + conflict round
//! trip over stdio. The classic pattern — keep it fast.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Client {
    fn spawn(workspace: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_zoanthrope"))
            .args(["mcp", "--workspace", workspace])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn zoanthrope mcp");
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = BufReader::new(child.stdout.take().expect("child stdout"));
        Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{line}").expect("write request");
        self.stdin.flush().expect("flush request");
        let response: Value = self.read_message();
        assert_eq!(response["id"], id, "response id matches request");
        response
    }

    fn read_message(&mut self) -> Value {
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line).expect("read response");
        assert!(n > 0, "server closed stdout");
        serde_json::from_str(line.trim()).expect("response is JSON")
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let response = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        assert_eq!(
            response["result"]["isError"], false,
            "tool call failed: {response}"
        );
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("text content");
        serde_json::from_str(text).expect("tool result is JSON")
    }

    fn shutdown(mut self) {
        drop(self.stdin);
        let _ = self.child.wait();
    }
}

fn temp_workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zoanthrope-mcp-smoke-{}-{}",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn mcp_smoke_initialize_tools_claim_and_conflict() {
    let ws = temp_workspace("smoke");
    let ws = ws.to_str().expect("utf-8 temp path");
    let mut client = Client::spawn(ws);

    // 1. Handshake.
    let init = client.request(
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "smoke", "version": "0"}
        }),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "zoanthrope");
    assert!(init["result"]["protocolVersion"].is_string());

    // 2. Tool listing includes the whole surface.
    let listed = client.request("tools/list", json!({}));
    let names: Vec<String> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    for expected in [
        "claim_files",
        "list_claims",
        "who_am_i",
        "list_active_agents",
        "register_agent",
        "rejoin_agent",
        "get_audit_log",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "missing {expected} in {names:?}"
        );
    }

    // 3. Register + claim a file.
    let reg = client.call_tool("register_agent", json!({"name": "alpha", "kind": "claude"}));
    assert_eq!(reg["ok"], true);
    let grant = client.call_tool(
        "claim_files",
        json!({"agent_id": "alpha", "files": [{"path": "a.rs", "purpose": "fix bug"}]}),
    );
    assert_eq!(grant["ok"], true);
    assert_eq!(grant["granted"][0]["path"], "a.rs");

    // 4. The second agent gets the clean conflict shape.
    let conflict = client.call_tool(
        "claim_files",
        json!({"agent_id": "beta", "files": ["a.rs"]}),
    );
    assert_eq!(conflict["ok"], false);
    assert_eq!(conflict["holder"], "alpha");
    assert_eq!(conflict["purpose"], "fix bug");
    assert_eq!(conflict["holder_stale"], false);
    assert_eq!(conflict["path"], "a.rs");
    assert_eq!(conflict["intent"], "edit");
    assert!(conflict["claimed_at"].is_number());

    // 5. The claim is visible in the registry and the audit ring.
    let claims = client.call_tool("list_claims", json!({}));
    assert_eq!(claims["claims"].as_array().unwrap().len(), 1);
    let audit = client.call_tool("get_audit_log", json!({"limit": 50}));
    assert!(audit["events"].as_array().unwrap().len() >= 2);

    // 6. Rejoin keeps the claim and republishes the board.
    let rejoined = client.call_tool("rejoin_agent", json!({"agent_id": "alpha"}));
    assert_eq!(rejoined["ok"], true);
    assert_eq!(rejoined["claims_kept"], json!(["a.rs"]));
    assert!(rejoined["view_epoch"].as_u64().unwrap() >= 1);

    // 7. Release frees the file; beta can take it.
    let released = client.call_tool(
        "claim_files",
        json!({"agent_id": "alpha", "mode": "release", "files": ["a.rs"]}),
    );
    assert_eq!(released["ok"], true);
    assert_eq!(released["released"], json!(["a.rs"]));
    let grant2 = client.call_tool(
        "claim_files",
        json!({"agent_id": "beta", "files": ["a.rs"]}),
    );
    assert_eq!(grant2["ok"], true);

    client.shutdown();
}
