#!/usr/bin/env bash
# zoanthrope PostToolUse watch hook (matcher: ALL tools).
#
# Independent post-hoc detector: fingerprints a set of workspace files
# after every tool call and reports two things to the audit log:
#
#   * a `watch` line per tool call with the files whose content changed
#     (the clobber forensics channel);
#   * an `unclaimed-mutation` line whenever a tracked file changed
#     outside any claim window — i.e. the mutation was not preceded by
#     a claim grant for the file (any agent) whose grant timestamp
#     PREDATES the mutation. That is the audit trail for Bash commands
#     the guard could not parse: the change is never blocked, but it is
#     never silent either.
#
# Attribution is order-sensitive, not a bare lookback: a claim granted
# AFTER the write lands credits nothing — the grant must fall within
# ZOANTHROPE_WATCH_LOOKBACK seconds AND not be later than the new
# content's mtime (the moment the mutation landed). A claim taken
# before the write (the normal guard flow) still credits it. The line
# is a report, not a verdict.
#
# Watch set: ZOANTHROPE_WATCH_FILES when set (comma-separated paths).
# When it is unset, the default is the workspace's `git ls-files` —
# the files the repo actually tracks. If git is unavailable, the
# default is empty (nothing watched), as before.
#
# Env:
#   ZOANTHROPE_WATCH_FILES   comma-separated paths to watch
#                            (unset = git ls-files of the workspace)
#   ZOANTHROPE_WATCH_STATE   digest state file
#                            (default: $ZOO_LOG.watch-state.json)
#   ZOANTHROPE_WATCH_LOOKBACK  claim lookback seconds (default 60)
#   ZOANTHROPE_AGENT / ZOANTHROPE_BIN / ZOO_LOG  as in claim-guard.sh

set +e
INPUT="$(cat 2>/dev/null || true)"

export ZOO_LOG="${ZOO_LOG:-${ZOANTHROPE_LOG:-}}"
export ZOO_WATCH_STATE="${ZOANTHROPE_WATCH_STATE:-}"
export ZOO_WATCH_LOOKBACK="${ZOANTHROPE_WATCH_LOOKBACK:-60}"
export ZOO_WATCH_AGENT="${ZOANTHROPE_AGENT:-${ZOANTHROPE_AGENT_NAME:-unknown}}"
WS_ROOT="${ZOANTHROPE_WORKSPACE:-}"
if [ -z "$WS_ROOT" ]; then
    WS_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
fi
export ZOO_WATCH_ROOT="$WS_ROOT"

printf '%s' "$INPUT" | python3 -c '
import hashlib, json, os, subprocess, sys, time

try:
    d = json.loads(sys.stdin.read() or "{}")
except Exception:
    d = {}
tool = d.get("tool_name") or "?"
log = os.environ.get("ZOO_LOG")
state_path = os.environ.get("ZOO_WATCH_STATE") or (
    (log + ".watch-state.json") if log else None
)
root = os.environ.get("ZOO_WATCH_ROOT") or "."
agent = os.environ.get("ZOO_WATCH_AGENT", "unknown")
lookback = float(os.environ.get("ZOO_WATCH_LOOKBACK", "60") or 60)
now = time.time()

# Watch set: explicit ZOANTHROPE_WATCH_FILES when set (even empty =
# watch nothing); when unset, default to the workspace"'"'"'s tracked files.
raw = os.environ.get("ZOANTHROPE_WATCH_FILES")
if raw is None:
    try:
        out = subprocess.check_output(
            ["git", "-C", root, "ls-files", "-z"], stderr=subprocess.DEVNULL
        )
        files = [
            os.path.join(root, p)
            for p in out.decode("utf-8", "replace").split("\0")
            if p
        ]
    except Exception:
        files = []  # no git, no default set — nothing watched
else:
    files = [p for p in raw.split(",") if p]


def snap():
    out = {}
    for p in files:
        try:
            st = os.stat(p)
            with open(p, "rb") as f:
                h = hashlib.sha256(f.read()).hexdigest()
            out[p] = {"mtime": st.st_mtime, "sha256": h}
        except OSError:
            out[p] = {"mtime": 0, "sha256": ""}
    return out


cur = snap()
prev = {}
if state_path and os.path.exists(state_path):
    try:
        with open(state_path) as f:
            prev = json.load(f)
    except Exception:
        prev = {}
changed = {p: cur[p] for p in cur if p not in prev or prev[p] != cur[p]}
if state_path:
    try:
        with open(state_path, "w") as f:
            json.dump(cur, f)
    except OSError:
        pass


def log_spellings(path):
    """A grant may log the canonical workspace-relative key (Rust side)
    while the watch set holds absolute paths — match both."""
    try:
        rel = os.path.relpath(path, root)
    except ValueError:
        return (path,)
    if rel.startswith(".."):
        return (path,)
    return (path, rel.replace(os.sep, "/"))


def recent_grant(path, mtime):
    """A claim window covers the change only when some grant for the
    file is recent AND precedes the mutation (its timestamp is not
    later than the new content'"'"'s mtime). A post-hoc claim — granted
    after the write already landed — credits nothing."""
    if not log or not os.path.exists(log):
        return False
    spellings = log_spellings(path)
    try:
        with open(log) as f:
            for line in f:
                try:
                    e = json.loads(line)
                except Exception:
                    continue
                # any-agent claim window covers the change (report only
                # when the change sat outside every claim window)
                if not (e.get("hook") == "pre" and e.get("result") == "grant"):
                    continue
                if e.get("file") not in spellings:
                    continue
                claim = e.get("claim") or {}
                ts = float(claim.get("claimed_at") or e.get("ts") or 0)
                if ts <= mtime and now - ts <= lookback:
                    return True
    except OSError:
        return False
    return False


entry = {"ts": now, "hook": "watch", "tool": tool, "changed": changed}
if log:
    with open(log, "a") as f:
        f.write(json.dumps(entry) + "\n")
    for path in changed:
        if not recent_grant(path, cur[path]["mtime"]):
            unclaimed = {
                "ts": now,
                "hook": "watch",
                "event": "unclaimed-mutation",
                "agent": agent,
                "tool": tool,
                "file": path,
            }
            f_path = log
            with open(f_path, "a") as f:
                f.write(json.dumps(unclaimed) + "\n")
' 2>/dev/null
exit 0
