#!/usr/bin/env bash
# zoanthrope PostToolUse watch hook (matcher: ALL tools).
#
# Independent post-hoc detector: fingerprints a fixed set of workspace
# files after every tool call and reports two things to the audit log:
#
#   * a `watch` line per tool call with the files whose content changed
#     (the clobber forensics channel);
#   * an `unclaimed-mutation` line whenever a tracked file changed
#     outside any claim window — i.e. the mutation was not preceded by
#     a claim grant for the file (any agent) within the last
#     ZOANTHROPE_WATCH_LOOKBACK seconds, default 60). That is the audit
#     trail for Bash commands the guard could not parse: the change is
#     never blocked, but it is never silent either.
#
# "Claim window" here is approximated from the audit log's pre-grant
# timestamps (watch runs at PostToolUse, after release-guard.sh may
# already have closed the window). A missed parse is therefore always
# visible; a change made long after someone else's claim can look
# unclaimed — the line is a report, not a verdict.
#
# Env:
#   ZOANTHROPE_WATCH_FILES   comma-separated absolute paths to watch
#   ZOANTHROPE_WATCH_STATE   digest state file
#                            (default: $ZOO_LOG.watch-state.json)
#   ZOANTHROPE_WATCH_LOOKBACK  claim lookback seconds (default 60)
#   ZOANTHROPE_AGENT / ZOANTHROPE_BIN / ZOO_LOG  as in claim-guard.sh

set +e
INPUT="$(cat 2>/dev/null || true)"

export ZOO_LOG="${ZOO_LOG:-${ZOANTHROPE_LOG:-}}"
export ZOO_WATCH_FILES="${ZOANTHROPE_WATCH_FILES:-}"
export ZOO_WATCH_STATE="${ZOANTHROPE_WATCH_STATE:-}"
export ZOO_WATCH_LOOKBACK="${ZOANTHROPE_WATCH_LOOKBACK:-60}"
export ZOO_WATCH_AGENT="${ZOANTHROPE_AGENT:-${ZOANTHROPE_AGENT_NAME:-unknown}}"

printf '%s' "$INPUT" | python3 -c '
import hashlib, json, os, sys, time

try:
    d = json.loads(sys.stdin.read() or "{}")
except Exception:
    d = {}
tool = d.get("tool_name") or "?"
log = os.environ.get("ZOO_LOG")
state_path = os.environ.get("ZOO_WATCH_STATE") or (
    (log + ".watch-state.json") if log else None
)
files = [p for p in os.environ.get("ZOO_WATCH_FILES", "").split(",") if p]
agent = os.environ.get("ZOO_WATCH_AGENT", "unknown")
lookback = float(os.environ.get("ZOO_WATCH_LOOKBACK", "60") or 60)
now = time.time()


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


def recent_grant(path):
    if not log or not os.path.exists(log):
        return False
    try:
        with open(log) as f:
            for line in f:
                try:
                    e = json.loads(line)
                except Exception:
                    continue
                # any-agent claim window covers the change (report only
                # when the change sat outside every claim window)
                if (
                    e.get("hook") == "pre"
                    and e.get("result") == "grant"
                    and e.get("file") == path
                    and now - float(e.get("ts") or 0) <= lookback
                ):
                    return True
    except OSError:
        return False
    return False


entry = {"ts": now, "hook": "watch", "tool": tool, "changed": changed}
if log:
    with open(log, "a") as f:
        f.write(json.dumps(entry) + "\n")
    for path in changed:
        if not recent_grant(path):
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
