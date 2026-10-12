//! Minimal MCP stdio transport: JSON-RPC 2.0 over newline-delimited
//! stdio. Handles `initialize`, `notifications/initialized`, `ping`,
//! `tools/list`, and `tools/call` — the handshake shape of the
//! standard MCP protocol, nothing more.

use std::io::{BufRead, Write};
use std::path::Path;

use serde_json::{json, Value};

use crate::tools;

const PROTOCOL_VERSION: &str = "2024-11-05";

/// Serve MCP over stdin/stdout until EOF. Each line is one JSON-RPC
/// message (requests and notifications alike).
pub fn run_stdio(root: &Path) -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_line(root, &line) {
            let mut raw = serde_json::to_string(&response)
                .unwrap_or_else(|e| format!(r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32603,"message":"serialize: {e}"}}}}"#));
            raw.push('\n');
            stdout.write_all(raw.as_bytes())?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// One line in, at most one response out (notifications produce none).
fn handle_line(root: &Path, line: &str) -> Option<Value> {
    let request: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {"code": -32700, "message": format!("parse error: {e}")},
            }))
        }
    };
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    // No id means a notification: it gets no response at all.
    let id = id?;
    let mut response = json!({"jsonrpc": "2.0", "id": id});

    match method {
        "initialize" => {
            response["result"] = json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {
                    "name": "zoanthrope",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            });
        }
        "notifications/initialized" | "initialized" => return None,
        "ping" => {
            response["result"] = json!({});
        }
        "tools/list" => {
            response["result"] = json!({"tools": tools::tool_defs()});
        }
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            response["result"] = match tools::call_tool(root, name, &arguments) {
                Ok(value) => json!({
                    "content": [{"type": "text", "text": value.to_string()}],
                    "isError": false,
                }),
                Err(e) => json!({
                    "content": [{"type": "text", "text": json!({"ok": false, "error": e}).to_string()}],
                    "isError": true,
                }),
            };
        }
        other => {
            response["error"] = json!({
                "code": -32601,
                "message": format!("method not found: {other}"),
            });
        }
    }
    Some(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zoanthrope-mcp-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn call(name: &str, line: &str) -> Value {
        let root = temp_root(name);
        handle_line(&root, line).expect("request must produce a response")
    }

    #[test]
    fn initialize_handshake_shape() {
        let resp = call(
            "init",
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        );
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(resp["result"]["serverInfo"]["name"], "zoanthrope");
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn notifications_get_no_response() {
        let root = temp_root("notif");
        assert!(handle_line(
            &root,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
        )
        .is_none());
        assert!(handle_line(&root, r#"{"jsonrpc":"2.0","method":"whatever"}"#).is_none());
    }

    #[test]
    fn tools_list_and_call_round_trip() {
        let root = temp_root("roundtrip");
        let listed = handle_line(
            &root,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
        )
        .unwrap();
        let tools = listed["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in tools::TOOL_NAMES {
            assert!(names.contains(expected), "missing tool {expected}");
        }

        let called = handle_line(
            &root,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"register_agent","arguments":{"name":"alpha","kind":"claude"}}}"#,
        )
        .unwrap();
        assert_eq!(called["result"]["isError"], false);
        let body: Value =
            serde_json::from_str(called["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["name"], "alpha");
    }

    #[test]
    fn errors_are_json_rpc_errors() {
        let resp = call("errors", "not json at all");
        assert_eq!(resp["error"]["code"], -32700);
        let resp = call(
            "errors",
            r#"{"jsonrpc":"2.0","id":9,"method":"no/such","params":{}}"#,
        );
        assert_eq!(resp["error"]["code"], -32601);
    }
}
