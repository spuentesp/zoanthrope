//! Runtime mode of the guard: advisory vs blocking is resolved per
//! tool call, never an installation choice — one wiring of
//! `claim-guard.sh` serves both. Precedence, highest wins:
//! `ZOANTHROPE_MODE` env > `.zoanthrope/config.json` `"mode"` at the
//! workspace root (git toplevel walk, like the CLI) > default
//! blocking. An invalid value falls back to blocking with exactly one
//! warning line and never crashes the hook.

#![cfg(unix)]

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn zoo_bin() -> &'static str {
    env!("CARGO_BIN_EXE_zoanthrope")
}

fn hooks_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("hooks")
}

fn guard(name: &str) -> PathBuf {
    hooks_dir().join("claude-code").join(name)
}

/// Source `hooks/mode.sh` and print what `zoo_resolve_mode` resolves.
/// `root` is the optional ROOT argument (None → helper walks up for
/// the git toplevel itself); `cwd` is the process working directory.
fn resolve_mode(
    env_mode: Option<&str>,
    root: Option<&Path>,
    cwd: Option<&Path>,
) -> (String, String, i32) {
    let script = r#". "$1"
if [ -n "$2" ]; then zoo_resolve_mode "$2"; else zoo_resolve_mode; fi"#;
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(script)
        .arg("mode-test")
        .arg(hooks_dir().join("mode.sh"))
        .arg(
            root.map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        )
        .env_remove("ZOANTHROPE_MODE")
        .env_remove("ZOANTHROPE_WORKSPACE")
        .env_remove("HOOK_CWD");
    if let Some(m) = env_mode {
        cmd.env("ZOANTHROPE_MODE", m);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd.output().expect("run mode resolver");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// Run `claim-guard.sh` with base env plus `extra_envs` (mode control).
fn run_guard(
    ws: &Path,
    agent: &str,
    stdin_json: &str,
    extra_envs: &[(&str, &str)],
) -> (String, String, i32) {
    let log = ws.join("mode-hook.log");
    let mut cmd = Command::new("bash");
    cmd.arg(guard("claim-guard.sh"))
        .env("ZOANTHROPE_BIN", zoo_bin())
        .env("ZOANTHROPE_WORKSPACE", ws)
        .env("ZOANTHROPE_AGENT", agent)
        .env("ZOO_LOG", log)
        .env_remove("ZOANTHROPE_MODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra_envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn claim-guard");
    child
        .stdin
        .take()
        .expect("guard stdin")
        .write_all(stdin_json.as_bytes())
        .expect("feed guard stdin");
    let out = child.wait_with_output().expect("guard output");
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

fn edit_input(cwd: &Path, file_path: &str) -> String {
    serde_json::json!({
        "tool_name": "Edit",
        "tool_input": {"file_path": file_path, "old_string": "x", "new_string": "y"},
        "cwd": cwd.to_string_lossy(),
    })
    .to_string()
}

fn bash_input(cwd: &Path, command: &str) -> String {
    serde_json::json!({
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "cwd": cwd.to_string_lossy(),
    })
    .to_string()
}

fn scratch(name: &str) -> PathBuf {
    let ws = std::env::temp_dir().join(format!("zoo-mode-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&ws);
    fs::create_dir_all(ws.join("shared")).expect("scratch workspace");
    fs::write(ws.join("shared/a.txt"), "seed\n").expect("seed file");
    ws
}

fn write_config(ws: &Path, body: &str) {
    fs::create_dir_all(ws.join(".zoanthrope")).expect("state dir");
    fs::write(ws.join(".zoanthrope/config.json"), body).expect("config.json");
}

#[test]
fn default_mode_is_blocking() {
    let ws = scratch("default");
    let (out, err, code) = resolve_mode(None, Some(&ws), None);
    assert_eq!(code, 0, "resolver must never fail");
    assert_eq!(out.trim(), "blocking", "blocking is the default");
    assert!(err.is_empty(), "no warning expected: {err}");
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn config_overrides_default() {
    let ws = scratch("config");
    write_config(&ws, r#"{"mode": "advisory"}"#);
    let (out, err, code) = resolve_mode(None, Some(&ws), None);
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "advisory");
    assert!(err.is_empty(), "no warning expected: {err}");
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn env_overrides_config() {
    let ws = scratch("env-wins");
    write_config(&ws, r#"{"mode": "advisory"}"#);
    let (out, err, _) = resolve_mode(Some("blocking"), Some(&ws), None);
    assert_eq!(out.trim(), "blocking", "env beats an advisory config");
    assert!(err.is_empty(), "no warning expected: {err}");

    write_config(&ws, r#"{"mode": "blocking"}"#);
    let (out, _, _) = resolve_mode(Some("advisory"), Some(&ws), None);
    assert_eq!(out.trim(), "advisory", "env beats a blocking config");
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn invalid_env_falls_back_to_blocking_with_one_warning() {
    let ws = scratch("bad-env");
    write_config(&ws, r#"{"mode": "advisory"}"#);
    let (out, err, code) = resolve_mode(Some("banana"), Some(&ws), None);
    assert_eq!(code, 0, "invalid mode must never fail the hook");
    assert_eq!(out.trim(), "blocking", "invalid env → blocking");
    assert!(
        err.contains("invalid mode") && err.contains("banana"),
        "must warn on stderr: {err}"
    );
    assert_eq!(err.lines().count(), 1, "exactly one warning line: {err}");
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn invalid_config_value_falls_back_to_blocking_with_one_warning() {
    let ws = scratch("bad-config");
    write_config(&ws, r#"{"mode": 5}"#);
    let (out, err, code) = resolve_mode(None, Some(&ws), None);
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "blocking");
    assert!(err.contains("invalid mode"), "must warn: {err}");
    assert_eq!(err.lines().count(), 1, "exactly one warning line: {err}");

    // unparsable config is equally survivable
    write_config(&ws, "{oops");
    let (out, err, code) = resolve_mode(None, Some(&ws), None);
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "blocking");
    assert!(err.contains("invalid mode"), "must warn: {err}");
    assert_eq!(err.lines().count(), 1, "exactly one warning line: {err}");

    // a config without a "mode" key is just the default — no warning
    write_config(&ws, r#"{"other": 1}"#);
    let (out, err, _) = resolve_mode(None, Some(&ws), None);
    assert_eq!(out.trim(), "blocking");
    assert!(err.is_empty(), "optional key must stay silent: {err}");
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn config_root_found_by_git_toplevel_walk() {
    let ws = scratch("walk");
    write_config(&ws, r#"{"mode": "advisory"}"#);
    fs::create_dir_all(ws.join(".git")).expect("git toplevel marker");
    let nested = ws.join("a/b");
    fs::create_dir_all(&nested).expect("nested cwd");
    // no explicit ROOT: the helper walks up from cwd like the CLI
    let (out, err, code) = resolve_mode(None, None, Some(&nested));
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "advisory");
    assert!(err.is_empty(), "no warning expected: {err}");
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn advisory_mode_never_returns_nonzero_on_conflict() {
    let ws = scratch("advisory");
    let file = ws.join("shared/a.txt");
    assert!(claim_cli(&ws, "beta", file.to_str().unwrap()));

    // env-driven advisory on the Edit vector
    let (stdout, stderr, code) = run_guard(
        &ws,
        "alpha",
        &edit_input(&ws, "shared/a.txt"),
        &[("ZOANTHROPE_MODE", "advisory")],
    );
    assert_eq!(
        code, 0,
        "advisory must never deny: stdout={stdout} stderr={stderr}"
    );
    assert!(
        !stdout.contains("permissionDecision") && !stdout.contains("deny"),
        "no deny JSON on stdout: {stdout}"
    );
    assert!(
        stderr.contains("beta"),
        "conflict JSON must reach stderr: {stderr}"
    );

    // config-driven advisory on the Bash-mutation vector
    write_config(&ws, r#"{"mode": "advisory"}"#);
    let cmd = "cat > shared/a.txt <<'EOF'\nclobber\nEOF";
    let (stdout, stderr, code) = run_guard(&ws, "alpha", &bash_input(&ws, cmd), &[]);
    assert_eq!(
        code, 0,
        "advisory must never deny: stdout={stdout} stderr={stderr}"
    );
    assert!(
        !stdout.contains("permissionDecision") && !stdout.contains("deny"),
        "no deny JSON on stdout: {stdout}"
    );
    assert!(
        stderr.contains("beta"),
        "conflict JSON must reach stderr: {stderr}"
    );

    // the conflict was reported but not dropped from the audit trail
    let log_body = fs::read_to_string(ws.join("mode-hook.log")).unwrap_or_default();
    assert!(
        log_body.contains("conflict"),
        "advisory conflicts must stay audited: {log_body}"
    );
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn advisory_mode_still_claims_the_file() {
    let ws = scratch("advisory-claim");
    write_config(&ws, r#"{"mode": "advisory"}"#);
    let file = ws.join("shared/a.txt");
    let (stdout, stderr, code) = run_guard(&ws, "alpha", &edit_input(&ws, "shared/a.txt"), &[]);
    assert_eq!(
        code, 0,
        "advisory grant path: stdout={stdout} stderr={stderr}"
    );
    assert!(stderr.contains("granted"), "claim JSON on stderr: {stderr}");
    assert!(
        !claim_cli(&ws, "beta", file.to_str().unwrap()),
        "advisory mode must still take the claim"
    );
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn advisory_mode_warns_on_stale_claim_but_never_denies() {
    // Advisory stays non-blocking on staleness, but the warning is
    // printed to stderr — the hole is visible even when nothing denies.
    let ws = scratch("stale-advisory");
    let file = ws.join("shared/a.txt");
    assert!(claim_cli(&ws, "alpha", file.to_str().unwrap()));
    fs::write(&file, "seed\nbeta-new-line\n").expect("out-of-band append");
    let (stdout, stderr, code) = run_guard(
        &ws,
        "alpha",
        &edit_input(&ws, "shared/a.txt"),
        &[("ZOANTHROPE_MODE", "advisory")],
    );
    assert_eq!(code, 0, "advisory must never deny: stdout={stdout}");
    assert!(
        !stdout.contains("permissionDecision") && !stdout.contains("deny"),
        "no deny JSON on stdout: {stdout}"
    );
    assert!(
        stderr.contains("stale-claim") && stderr.contains("re-read"),
        "staleness warning must reach stderr: {stderr}"
    );
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn blocking_unchanged_env_wins_over_advisory_config() {
    let ws = scratch("blocking");
    write_config(&ws, r#"{"mode": "advisory"}"#);
    assert!(claim_cli(
        &ws,
        "beta",
        ws.join("shared/a.txt").to_str().unwrap()
    ));
    let (stdout, stderr, code) = run_guard(
        &ws,
        "alpha",
        &edit_input(&ws, "shared/a.txt"),
        &[("ZOANTHROPE_MODE", "blocking")],
    );
    assert_eq!(code, 2, "blocking deny is unchanged: stdout={stdout}");
    assert!(
        stdout.contains("permissionDecision") && stdout.contains("deny"),
        "deny JSON missing: {stdout}"
    );
    assert!(stderr.contains("beta"), "must name the holder: {stderr}");
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn invalid_mode_warns_and_still_blocks_through_the_guard() {
    let ws = scratch("bad-guard");
    write_config(&ws, r#"{"mode": "advisory"}"#);
    assert!(claim_cli(
        &ws,
        "beta",
        ws.join("shared/a.txt").to_str().unwrap()
    ));
    let (stdout, stderr, code) = run_guard(
        &ws,
        "alpha",
        &edit_input(&ws, "shared/a.txt"),
        &[("ZOANTHROPE_MODE", "banana")],
    );
    assert_eq!(code, 2, "invalid mode falls back to blocking: {stdout}");
    assert!(
        stderr.contains("invalid mode"),
        "warning on stderr: {stderr}"
    );
    assert!(
        stdout.contains("deny"),
        "blocking behavior unchanged: {stdout}"
    );
    let _ = fs::remove_dir_all(&ws);
}
