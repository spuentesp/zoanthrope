//! Claim under contention answers grant or conflict — never plumbing.
//!
//! The contract: `claim_files` under pure contention either GRANTS or
//! returns the conflict shape naming the holder. A lock-acquisition
//! error (`could not lock: timed out`) is an infrastructure failure
//! leaking through the claim surface, and fails this test. Two shapes
//! of the same hammer: threads sharing one process, and concurrent
//! CLI processes on one state file.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;

use serde_json::{json, Value};

fn temp_workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zoanthrope-claim-contention-{}-{}",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..4 {
        std::fs::write(dir.join(format!("f{i}.rs")), format!("fn f{i}() {{}}\n")).unwrap();
    }
    dir
}

/// Grant or the conflict shape (holder named). Anything else — an
/// `error` body without a holder — is the infrastructure leak this
/// test exists to forbid.
fn assert_grant_or_conflict(v: &Value, ctx: &str) {
    assert!(v["ok"].is_boolean(), "{ctx}: not a claim body: {v}");
    if v["ok"] == true {
        assert!(v["granted"].is_array(), "{ctx}: grant body: {v}");
    } else {
        assert!(
            v["holder"].is_string(),
            "{ctx}: conflict must name the holder: {v}"
        );
        assert!(
            v["error"].is_null(),
            "{ctx}: infra error leaked through claim: {v}"
        );
    }
}

#[test]
fn threaded_claims_under_contention_grant_or_conflict() {
    let root = temp_workspace("threads");
    const AGENTS: usize = 8;
    const ITERS: usize = 20;
    let handles: Vec<_> = (0..AGENTS)
        .map(|a| {
            let root = root.clone();
            thread::spawn(move || {
                for it in 0..ITERS {
                    let agent = format!("agent-{a}");
                    let file = format!("f{}.rs", (a + it) % 4);
                    let v = zoanthrope::tools::call_tool(
                        &root,
                        "claim_files",
                        &json!({
                            "agent_id": agent,
                            "intent": "edit",
                            "purpose": format!("it={it}"),
                            "files": [{"path": file, "purpose": format!("it={it}")}],
                        }),
                    )
                    .unwrap_or_else(|e| panic!("claim_files errored under contention: {e}"));
                    assert_grant_or_conflict(&v, "claim");
                    let r = zoanthrope::tools::call_tool(
                        &root,
                        "claim_files",
                        &json!({
                            "agent_id": agent,
                            "mode": "release",
                            "files": [format!("f{}.rs", (a + it) % 4)],
                        }),
                    )
                    .unwrap_or_else(|e| panic!("release errored under contention: {e}"));
                    assert_eq!(r["ok"], true, "release body: {r}");
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("worker panicked");
    }
}

#[test]
fn concurrent_cli_claims_grant_or_conflict() {
    let root = temp_workspace("cli");
    const AGENTS: usize = 12;
    let handles: Vec<_> = (0..AGENTS)
        .map(|a| {
            let root = root.clone();
            thread::spawn(move || {
                let agent = format!("agent-{a}");
                for it in 0..8 {
                    let file = format!("f{}.rs", (a + it) % 4);
                    let out = Command::new(env!("CARGO_BIN_EXE_zoanthrope"))
                        .args(["claim", "--files", &file, "--agent", &agent, "--workspace"])
                        .arg(&root)
                        .stdin(Stdio::null())
                        .output()
                        .expect("zoanthrope claim");
                    let v: Value = serde_json::from_slice(&out.stdout)
                        .unwrap_or_else(|e| panic!("claim non-JSON: {e}: {out:?}"));
                    assert_grant_or_conflict(&v, "cli claim");
                    let _ = Command::new(env!("CARGO_BIN_EXE_zoanthrope"))
                        .args([
                            "release",
                            "--files",
                            &file,
                            "--agent",
                            &agent,
                            "--workspace",
                        ])
                        .arg(&root)
                        .stdin(Stdio::null())
                        .output()
                        .expect("zoanthrope release");
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("worker panicked");
    }
}
