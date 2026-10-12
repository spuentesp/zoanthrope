//! The PostToolUse watch hook: attribution order and the default watch
//! set.
//!
//! Two reviewed defects live here. (1) The 60s lookback credited a
//! mutation to *any* claim in the window regardless of order, so a
//! claim taken AFTER an unclaimed write silenced the
//! `unclaimed-mutation` alert. Attribution must require the claim grant
//! to precede the mutation. (2) The watch set was explicit-only; when
//! `ZOANTHROPE_WATCH_FILES` is unset it now defaults to the workspace's
//! `git ls-files`.

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

fn run_watch(
    ws: &Path,
    agent: &str,
    stdin_json: &str,
    log: &Path,
    watch_files: Option<&str>,
) -> (String, String, i32) {
    let mut cmd = Command::new("bash");
    cmd.arg(hook("watch.sh"))
        .env("ZOANTHROPE_BIN", zoo_bin())
        .env("ZOANTHROPE_WORKSPACE", ws)
        .env("ZOANTHROPE_AGENT", agent)
        .env("ZOO_LOG", log)
        .env("ZOANTHROPE_WATCH_STATE", ws.join("watch-state.json"))
        .env_remove("ZOANTHROPE_WATCH_FILES")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(files) = watch_files {
        cmd.env("ZOANTHROPE_WATCH_FILES", files);
    }
    let mut child = cmd.spawn().expect("spawn watch");
    child
        .stdin
        .take()
        .expect("watch stdin")
        .write_all(stdin_json.as_bytes())
        .expect("feed watch stdin");
    let out = child.wait_with_output().expect("watch output");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// Run `claim-guard.sh` (Edit on `file_path`) — the real pre-hook that
/// writes the grant line the watch attribute against.
fn guard_claim(ws: &Path, agent: &str, file_path: &str, log: &Path) -> i32 {
    let stdin = serde_json::json!({
        "tool_name": "Edit",
        "tool_input": {"file_path": file_path, "old_string": "x", "new_string": "y"},
        "cwd": ws.to_string_lossy(),
    })
    .to_string();
    let mut child = Command::new("bash")
        .arg(hook("claim-guard.sh"))
        .env("ZOANTHROPE_BIN", zoo_bin())
        .env("ZOANTHROPE_WORKSPACE", ws)
        .env("ZOANTHROPE_AGENT", agent)
        .env("ZOO_LOG", log)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn claim-guard");
    child
        .stdin
        .take()
        .expect("guard stdin")
        .write_all(stdin.as_bytes())
        .expect("feed guard stdin");
    let out = child.wait_with_output().expect("guard output");
    out.status.code().unwrap_or(-1)
}

fn bash_input(ws: &Path) -> String {
    serde_json::json!({
        "tool_name": "Bash",
        "tool_input": {"command": "echo hi"},
        "cwd": ws.to_string_lossy(),
    })
    .to_string()
}

fn unclaimed_count(log: &Path) -> usize {
    fs::read_to_string(log)
        .unwrap_or_default()
        .matches("unclaimed-mutation")
        .count()
}

fn scratch(name: &str) -> PathBuf {
    let ws = std::env::temp_dir().join(format!("zoo-watch-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&ws);
    fs::create_dir_all(&ws).expect("scratch workspace");
    fs::write(ws.join("f.txt"), "seed\n").expect("seed file");
    ws
}

/// Two quiet baseline runs: the first watch ever reports every watched
/// file as "changed" (no prior snapshot), so settle before acting.
fn baseline(ws: &Path, log: &Path, watch_files: Option<&str>) -> usize {
    run_watch(ws, "alpha", &bash_input(ws), log, watch_files);
    run_watch(ws, "alpha", &bash_input(ws), log, watch_files);
    unclaimed_count(log)
}

#[test]
fn post_hoc_claim_does_not_silence_the_alert() {
    // Repro shape: write f with NO claim, then claim f (anyone) within
    // the lookback. The claim came AFTER the mutation — it must not
    // credit the write; `unclaimed-mutation` must still fire.
    let ws = scratch("order");
    let log = ws.join("hook.log");
    let f = ws.join("f.txt");
    let base = baseline(&ws, &log, Some(f.to_str().unwrap()));

    // the unclaimed write (out-of-band, no claim)
    fs::write(&f, "unclaimed write\n").expect("unclaimed write");
    let mtime = fs::metadata(&f).unwrap().modified().unwrap();

    // ...then a claim, within the 60s lookback but after the fact
    let code = guard_claim(&ws, "alpha", "f.txt", &log);
    assert_eq!(code, 0, "the post-hoc claim is granted (that is the point)");
    assert!(
        unclaimed_count(&log) == base,
        "the grant itself writes no watch lines"
    );
    // A second grant record with the absolute spelling (hooks and tools
    // disagree on spelling) — also granted after the mutation. This is
    // the review's repro: any claim inside the lookback window used to
    // credit the write regardless of order.
    {
        use std::io::Write as _;
        let mtime_unix = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let late_ts = mtime_unix + 0.5;
        let mut flog = fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .expect("append grant line");
        writeln!(
            flog,
            "{}",
            serde_json::json!({
                "ts": late_ts,
                "hook": "pre",
                "agent": "beta",
                "tool": "Edit",
                "file": f.to_str().unwrap(),
                "result": "grant",
                "claim": {"claimed_at": late_ts, "path": f.to_str().unwrap()},
            })
        )
        .unwrap();
    }

    run_watch(
        &ws,
        "alpha",
        &bash_input(&ws),
        &log,
        Some(f.to_str().unwrap()),
    );
    assert_eq!(
        unclaimed_count(&log),
        base + 1,
        "a claim granted AFTER the mutation must not silence the alert"
    );
    // the test's own ordering premise: the grant really did come later
    let log_body = fs::read_to_string(&log).unwrap();
    let grant_ts: f64 = log_body
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| e["hook"] == "pre" && e["result"] == "grant")
        .filter_map(|e| e["ts"].as_f64())
        .next()
        .expect("grant line");
    let mtime_unix = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    assert!(
        grant_ts > mtime_unix,
        "repro premise: claim lands after the write ({grant_ts} vs {mtime_unix})"
    );
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn claim_before_write_credits_the_mutation() {
    // The control that keeps the fix honest: the normal claimed-write
    // flow (guard claims, then the tool writes) must NOT alert.
    let ws = scratch("credit");
    let log = ws.join("hook.log");
    let f = ws.join("f.txt");
    let base = baseline(&ws, &log, Some(f.to_str().unwrap()));

    // PreToolUse claims first ...
    let code = guard_claim(&ws, "alpha", "f.txt", &log);
    assert_eq!(code, 0);
    // ... then the tool write lands (mtime after the grant).
    fs::write(&f, "claimed write\n").expect("claimed write");

    run_watch(
        &ws,
        "alpha",
        &bash_input(&ws),
        &log,
        Some(f.to_str().unwrap()),
    );
    assert_eq!(
        unclaimed_count(&log),
        base,
        "a claim granted BEFORE the write credits it — no alert"
    );
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn default_watch_set_is_git_ls_files() {
    // With ZOANTHROPE_WATCH_FILES unset, the watch set is the
    // workspace's tracked files (git ls-files) — and untracked files
    // stay out of it (fallback when git is unavailable: nothing).
    let ws = scratch("gitset");
    let log = ws.join("hook.log");
    let tracked = ws.join("tracked.txt");
    let untracked = ws.join("untracked.txt");
    fs::write(&tracked, "tracked seed\n").unwrap();
    fs::write(&untracked, "untracked seed\n").unwrap();
    for args in [vec!["init", "-q"], vec!["add", "tracked.txt"]] {
        let st = Command::new("git")
            .args(&args)
            .current_dir(&ws)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("git available in tests");
        assert!(st.success(), "git {args:?} must succeed");
    }

    let base = baseline(&ws, &log, None);
    // out-of-band writes, no claims
    fs::write(&tracked, "tracked change\n").unwrap();
    fs::write(&untracked, "untracked change\n").unwrap();
    run_watch(&ws, "alpha", &bash_input(&ws), &log, None);

    assert!(
        unclaimed_count(&log) > base,
        "the tracked file's unclaimed change must be reported"
    );
    let log_body = fs::read_to_string(&log).unwrap();
    assert!(
        log_body.contains("unclaimed-mutation") && log_body.contains("tracked.txt"),
        "the alert names the tracked file: {log_body}"
    );
    // untracked.txt is not in the default watch set at all
    let watch_lines: Vec<serde_json::Value> = log_body
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| e["hook"] == "watch" && e.get("changed").is_some())
        .collect();
    let mentions_untracked = watch_lines.iter().any(|e| {
        e["changed"]
            .as_object()
            .map(|m| m.keys().any(|k| k.contains("untracked")))
            .unwrap_or(false)
    });
    assert!(
        !mentions_untracked,
        "untracked files are outside the default watch set: {watch_lines:?}"
    );
    let _ = fs::remove_dir_all(&ws);
}
