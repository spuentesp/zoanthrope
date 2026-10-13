//! CLI round-trip for caller stamps: `zoanthrope claim --stamp name=value`
//! records the pin; `status` and `who_am_i` hand it back. The flag is
//! repeatable — every stamp flows into the same claim.

use std::path::PathBuf;
use std::process::Command;

use serde_json::json;

fn temp_workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zoanthrope-cli-stamps-{}-{}",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn claim_stamp_flag_round_trips_into_status_and_who_am_i() {
    let ws = temp_workspace("roundtrip");
    std::fs::write(ws.join("f.txt"), "seed\n").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_zoanthrope"))
        .args([
            "claim",
            "--files",
            "f.txt",
            "--agent",
            "alpha",
            "--stamp",
            "symbol:parse_token=deadbeef",
            "--stamp",
            "graph:ast=cafe",
            "--workspace",
        ])
        .arg(&ws)
        .output()
        .expect("zoanthrope claim --stamp");
    assert!(
        out.status.success(),
        "claim exits 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let grant: serde_json::Value = serde_json::from_slice(&out.stdout).expect("claim JSON");
    assert_eq!(grant["ok"], true);
    assert_eq!(
        grant["granted"][0]["caller_stamps"],
        json!([
            {"name": "symbol:parse_token", "value": "deadbeef"},
            {"name": "graph:ast", "value": "cafe"},
        ])
    );

    // `status` shows the pin.
    let status = Command::new(env!("CARGO_BIN_EXE_zoanthrope"))
        .args(["status", "--workspace"])
        .arg(&ws)
        .output()
        .expect("zoanthrope status");
    let status = String::from_utf8(status.stdout).expect("utf8 status");
    assert!(
        status.contains("symbol:parse_token=deadbeef"),
        "status shows the stamp:\n{status}"
    );

    // `who_am_i` (the MCP tool surface over the same state) shows it too.
    let who = zoanthrope::tools::call_tool(&ws, "who_am_i", &json!({"agent_id": "alpha"}))
        .expect("who_am_i");
    assert_eq!(
        who["claims"][0]["caller_stamps"][0]["name"],
        "symbol:parse_token"
    );
    assert_eq!(who["claims"][0]["caller_stamps"][0]["value"], "deadbeef");
    assert_eq!(
        who["claims"][0]["caller_stamps"].as_array().unwrap().len(),
        2
    );

    let _ = std::fs::remove_dir_all(&ws);
}
