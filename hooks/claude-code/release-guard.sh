#!/usr/bin/env bash
# zoanthrope PostToolUse release — counterpart of claim-guard.sh.
#
# Releases the claims taken for the finished tool call and records a
# post-edit digest per file. Handles Edit/Write/MultiEdit (file_path)
# and Bash mutations (command re-parsed with the same
# `zoanthrope mutation-files` heuristic — so a granted Bash mutation
# releases exactly what it claimed).
#
# Env: same as claim-guard.sh.

set +e
INPUT="$(cat 2>/dev/null || true)"

eval "$(printf '%s' "$INPUT" | python3 -c '
import json, sys, shlex
fp = cwd = tool = cmd = ""
try:
    d = json.loads(sys.stdin.read() or "{}")
except Exception:
    d = {}
ti = d.get("tool_input") or {}
if isinstance(ti, dict) and isinstance(ti.get("file_path"), str):
    fp = ti["file_path"]
elif isinstance(d.get("file_path"), str):
    fp = d["file_path"]
if isinstance(ti, dict) and isinstance(ti.get("command"), str):
    cmd = ti["command"]
if isinstance(d.get("cwd"), str):
    cwd = d["cwd"]
if isinstance(d.get("tool_name"), str):
    tool = d["tool_name"]
print("FILE_PATH=" + shlex.quote(fp))
print("COMMAND=" + shlex.quote(cmd))
print("HOOK_CWD=" + shlex.quote(cwd))
print("TOOL_NAME=" + shlex.quote(tool))
')" 2>/dev/null

WS_ROOT="${ZOANTHROPE_WORKSPACE:-}"
if [ -z "$WS_ROOT" ]; then
    WS_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
fi
ZOO="${ZOANTHROPE_BIN:-zoanthrope}"
AGENT="${ZOANTHROPE_AGENT:-${ZOANTHROPE_AGENT_NAME:-unknown}}"
export ZOO_LOG="${ZOO_LOG:-${ZOANTHROPE_LOG:-}}"
TS="$(date +%s.%N)"

if [ "$TOOL_NAME" = "Bash" ]; then
    COMMA="$("$ZOO" mutation-files --command "$COMMAND" --cwd "${HOOK_CWD:-$PWD}" \
        --workspace "$WS_ROOT" 2>/dev/null \
        | python3 -c '
import json, sys
try:
    files = json.load(sys.stdin)
except Exception:
    files = []
out = []
for p in files:
    if p and p not in out:
        out.append(p)
print(",".join(out))' 2>/dev/null)"
    LOG_TOOL="Bash"
else
    if [ -z "$FILE_PATH" ]; then exit 0; fi
    case "$FILE_PATH" in
        /*) ;;
        *) FILE_PATH="${HOOK_CWD:-$PWD}/$FILE_PATH" ;;
    esac
    FILE_PATH="$(realpath -m "$FILE_PATH" 2>/dev/null || echo "$FILE_PATH")"
    COMMA="$FILE_PATH"
    LOG_TOOL="$TOOL_NAME"
fi

if [ -z "$COMMA" ]; then exit 0; fi

RESULT="$("$ZOO" release --files "$COMMA" --agent "$AGENT" \
    --workspace "$WS_ROOT" 2>/dev/null)"
export ZOO_POST_RELEASE="${RESULT:-null}"

# Per-file post line with digest (same shape the A/B metrics reader
# consumes for claim windows).
printf '%s\n' "$COMMA" | ZOO_POST_TS="$TS" ZOO_POST_AGENT="$AGENT" \
    ZOO_POST_TOOL="$LOG_TOOL" python3 -c '
import hashlib, json, os, sys
try:
    claim = json.loads(os.environ.get("ZOO_POST_RELEASE", "null"))
except Exception:
    claim = None
log = os.environ.get("ZOO_LOG") or os.environ.get("ZOANTHROPE_LOG")
for path in sys.stdin.read().split(","):
    path = path.strip()
    if not path:
        continue
    try:
        st = os.stat(path)
        mtime = st.st_mtime
    except OSError:
        mtime = 0
    try:
        with open(path, "rb") as f:
            sha = hashlib.sha256(f.read()).hexdigest()
    except OSError:
        sha = ""
    if log:
        entry = {
            "ts": float(os.environ.get("ZOO_POST_TS", "0")),
            "hook": "post",
            "agent": os.environ.get("ZOO_POST_AGENT", "unknown"),
            "tool": os.environ.get("ZOO_POST_TOOL", ""),
            "file": path,
            "mtime": mtime,
            "sha256": sha,
            "release": claim,
        }
        with open(log, "a") as f:
            f.write(json.dumps(entry) + "\n")
' 2>/dev/null
exit 0
