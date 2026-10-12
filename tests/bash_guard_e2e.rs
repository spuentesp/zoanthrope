//! End-to-end: the Bash-mutation guard path, driven through the real
//! hook scripts and the real `zoanthrope` binary.
//!
//! The scenario: `Bash` tool calls whose commands mutate workspace
//! files (`cat > file`, the codemod/generator clobber vector) must flow
//! through the SAME claim arbitration as Edit/Write — a foreign claim
//! denies the call with a reason naming the holder — and a change that
//! slips past the parser must surface as an `unclaimed-mutation` audit
//! line from the watch hook.

#![cfg(unix)]

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn zoo_bin() -> &'static str {
    env!("CARGO_BIN_EXE_zoanthrope")
}

fn hook(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("hooks/claude-code")
        .join(name)
}

fn run_hook(
    name: &str,
    ws: &Path,
    agent: &str,
    stdin_json: &str,
    log: &Path,
) -> (String, String, i32) {
    let mut child = Command::new("bash")
        .arg(hook(name))
        .env("ZOANTHROPE_BIN", zoo_bin())
        .env("ZOANTHROPE_WORKSPACE", ws)
        .env("ZOANTHROPE_AGENT", agent)
        .env("ZOO_LOG", log)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook");
    child
        .stdin
        .take()
        .expect("hook stdin")
        .write_all(stdin_json.as_bytes())
        .expect("feed hook stdin");
    let out = child.wait_with_output().expect("hook output");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

fn claim_cli(ws: &Path, agent: &str, files: &str) -> bool {
    let out = Command::new(zoo_bin())
        .args(["claim", "--files", files, "--agent", agent, "--workspace"])
        .arg(ws)
        .output()
        .expect("zoanthrope claim");
    out.status.success()
}

fn bash_input(cwd: &Path, command: &str) -> String {
    serde_json::json!({
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "cwd": cwd.to_string_lossy(),
    })
    .to_string()
}

fn edit_input(cwd: &Path, file_path: &str) -> String {
    serde_json::json!({
        "tool_name": "Edit",
        "tool_input": {"file_path": file_path, "old_string": "x", "new_string": "y"},
        "cwd": cwd.to_string_lossy(),
    })
    .to_string()
}

fn scratch(name: &str) -> PathBuf {
    let ws = std::env::temp_dir().join(format!("zoo-guard-e2e-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&ws);
    fs::create_dir_all(ws.join("shared")).expect("scratch workspace");
    fs::write(ws.join("shared/config.yaml"), "version = 2\n").expect("seed file");
    fs::write(ws.join("shared/types.ts"), "export const REGISTRY = [];\n").expect("seed file");
    ws
}

#[test]
fn bash_redirect_is_denied_by_foreign_claim() {
    let ws = scratch("deny");
    let log = ws.join("hook.log");
    // beta holds the hot file
    assert!(
        claim_cli(&ws, "beta", ws.join("shared/config.yaml").to_str().unwrap()),
        "beta's claim must be granted"
    );
    // alpha tries to clobber it with a shell redirect — the classic
    // lost-update vector that bypasses Edit/Write OCC
    let cmd = "cat > shared/config.yaml <<'EOF'\n[alpha]\nkey_one = \"alpha-value-1\"\nEOF";
    let (stdout, stderr, code) =
        run_hook("claim-guard.sh", &ws, "alpha", &bash_input(&ws, cmd), &log);
    assert_eq!(
        code, 2,
        "guard must deny (exit 2); stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains("\"permissionDecision\": \"deny\"")
            || stdout.contains("\"permissionDecision\":\"deny\""),
        "deny JSON missing: {stdout}"
    );
    assert!(
        stdout.contains("beta") && stderr.contains("beta"),
        "denial must name the holder 'beta': stdout={stdout} stderr={stderr}"
    );
    // the denial is audited as a conflict, not a silent drop
    let log_body = fs::read_to_string(&log).expect("hook log");
    assert!(
        log_body.contains("\"result\": \"conflict\"")
            || log_body.contains("\"result\":\"conflict\""),
        "conflict not logged: {log_body}"
    );

    // and the file really was not touched
    assert_eq!(
        fs::read_to_string(ws.join("shared/config.yaml")).unwrap(),
        "version = 2\n"
    );
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn bash_redirect_grants_releases_and_recovers() {
    let ws = scratch("grant");
    let log = ws.join("hook.log");
    let cmd = "cat > shared/types.ts <<'EOF'\nexport const REGISTRY = [{ name: \"alpha\" }];\nEOF";
    let (stdout, _stderr, code) =
        run_hook("claim-guard.sh", &ws, "alpha", &bash_input(&ws, cmd), &log);
    assert_eq!(code, 0, "free file must grant: {stdout}");
    // the claim is live now — a foreign agent is denied
    assert!(
        !claim_cli(&ws, "beta", ws.join("shared/types.ts").to_str().unwrap()),
        "beta must not steal alpha's live claim"
    );
    // PostToolUse releases it again
    let (_o, _e, pcode) = run_hook(
        "release-guard.sh",
        &ws,
        "alpha",
        &bash_input(&ws, cmd),
        &log,
    );
    assert_eq!(pcode, 0);
    assert!(
        claim_cli(&ws, "beta", ws.join("shared/types.ts").to_str().unwrap()),
        "file must be free after release"
    );
    let log_body = fs::read_to_string(&log).expect("hook log");
    assert!(
        log_body.contains("\"result\": \"grant\"") || log_body.contains("\"result\":\"grant\"")
    );
    assert!(log_body.contains("\"hook\": \"post\"") || log_body.contains("\"hook\":\"post\""));
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn edit_write_path_still_claims_and_denies() {
    // Byte-compatible regression net for the Edit/Write claim flow.
    let ws = scratch("edit");
    let log = ws.join("hook.log");
    assert!(claim_cli(
        &ws,
        "beta",
        ws.join("shared/config.yaml").to_str().unwrap()
    ));
    let (_o, stderr, code) = run_hook(
        "claim-guard.sh",
        &ws,
        "alpha",
        &edit_input(&ws, "shared/config.yaml"),
        &log,
    );
    assert_eq!(code, 2, "Edit on a foreign claim must deny");
    assert!(stderr.contains("beta"), "must name holder: {stderr}");
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn mutation_files_cli_handles_relative_workspace() {
    let ws = scratch("rel");
    let parent = ws.parent().unwrap().to_path_buf();
    let rel = ws.file_name().unwrap().to_string_lossy().into_owned();
    let out = Command::new(zoo_bin())
        .args([
            "mutation-files",
            "--command",
            "cat > shared/config.yaml <<'EOF'\nx\nEOF",
            "--cwd",
            &rel,
            "--workspace",
            &rel,
        ])
        .current_dir(&parent)
        .output()
        .expect("mutation-files with relative paths");
    assert!(
        out.status.success(),
        "CLI must accept relative cwd/workspace"
    );
    let files: Vec<String> = serde_json::from_slice(&out.stdout).expect("JSON array");
    assert_eq!(
        files,
        vec![ws.join("shared/config.yaml").to_string_lossy().into_owned()],
        "relative workspace must still resolve the target"
    );
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn stale_held_claim_denies_the_guard_write() {
    // P1: a claim binds to content. An out-of-band append under a held
    // claim must make the next guarded write stale — denied with
    // `stale-claim` (holder is self) instead of clobbering — and the
    // recovery (re-read, re-claim) re-binds the fingerprint.
    let ws = scratch("stale");
    let log = ws.join("hook.log");
    let f = ws.join("shared/config.yaml");
    // alpha claims the file and reads it at t0
    assert!(
        claim_cli(&ws, "alpha", f.to_str().unwrap()),
        "alpha's claim must be granted"
    );
    // out-of-band process appends — beta's addition lands under the claim
    fs::write(&f, "version = 2\nbeta-new-line\n").expect("out-of-band append");
    // a write through the guard with stale content must be DENIED
    let (stdout, stderr, code) = run_hook(
        "claim-guard.sh",
        &ws,
        "alpha",
        &edit_input(&ws, "shared/config.yaml"),
        &log,
    );
    assert_eq!(
        code, 2,
        "stale write must deny (exit 2); stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains("stale-claim") && stderr.contains("stale-claim"),
        "denial must say stale-claim: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stderr.contains("re-read") && stderr.contains("re-claim"),
        "the message must name the recovery: {stderr}"
    );
    // the file was not touched — beta's line survives
    assert_eq!(
        fs::read_to_string(&f).unwrap(),
        "version = 2\nbeta-new-line\n",
        "stale content must never clobber"
    );
    // recovery protocol: re-read (simulated by the fresh read above),
    // then re-claim — the re-claim re-binds the fingerprint
    assert!(
        claim_cli(&ws, "alpha", f.to_str().unwrap()),
        "plain re-claim refreshes the fingerprint"
    );
    let (stdout, _stderr, code) = run_hook(
        "claim-guard.sh",
        &ws,
        "alpha",
        &edit_input(&ws, "shared/config.yaml"),
        &log,
    );
    assert_eq!(code, 0, "guarded write after re-claim must pass: {stdout}");
    // the refusal is audited as its own class, not a silent drop
    let log_body = fs::read_to_string(&log).expect("hook log");
    assert!(
        log_body.contains("stale-claim"),
        "stale-claim must be audited: {log_body}"
    );
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn unparseable_mutation_is_reported_unclaimed() {
    let ws = scratch("watch");
    let log = ws.join("hook.log");
    let tracked = ws.join("shared/config.yaml");
    // a mutation idiom the parser does not know (here: `dd of=…`) —
    // the guard must NOT block (it claims nothing), and the watch hook
    // must report the change as an unclaimed mutation
    let cmd = "dd of=shared/config.yaml status=none";
    let (_o, _e, code) = run_hook("claim-guard.sh", &ws, "alpha", &bash_input(&ws, cmd), &log);
    assert_eq!(code, 0, "unparseable command must pass through the guard");
    fs::write(&tracked, "version = 3\n").expect("simulate the unparseable write");
    let watch_state = ws.join("watch-state.json");
    let (wo, we, wcode) = {
        let mut child = Command::new("bash")
            .arg(hook("watch.sh"))
            .env("ZOANTHROPE_BIN", zoo_bin())
            .env("ZOANTHROPE_WORKSPACE", &ws)
            .env("ZOANTHROPE_AGENT", "alpha")
            .env("ZOO_LOG", &log)
            .env("ZOANTHROPE_WATCH_FILES", tracked.to_str().unwrap())
            .env("ZOANTHROPE_WATCH_STATE", watch_state.to_str().unwrap())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn watch");
        child
            .stdin
            .take()
            .expect("watch stdin")
            .write_all(bash_input(&ws, cmd).as_bytes())
            .expect("feed watch stdin");
        let out = child.wait_with_output().expect("watch output");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.code().unwrap_or(-1),
        )
    };
    assert_eq!(wcode, 0, "watch must never fail: {wo} {we}");
    let log_body = fs::read_to_string(&log).expect("hook log");
    assert!(
        log_body.contains("unclaimed-mutation"),
        "watch must report an unclaimed-mutation line: {log_body}"
    );
    let _ = fs::remove_dir_all(&ws);
}
