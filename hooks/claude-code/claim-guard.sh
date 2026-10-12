#!/usr/bin/env bash
# zoanthrope PreToolUse guard — runtime mode: advisory | blocking.
#
# Covers Edit/Write/MultiEdit (tool_input.file_path) AND Bash commands
# that mutate files (tool_input.command, parsed by
# `zoanthrope mutation-files` — redirects, sed -i, tee, cp/mv targets,
# python/perl write sinks; see src/bash_parse.rs for the limits).
#
# Flow per tool call: claim EVERY workspace file the call would mutate
# (one all-or-nothing `zoanthrope claim`) — either mode claims — then
# the resolved mode decides:
#   * advisory → exit 0 always: the claim/conflict JSON goes to stderr
#                (pre-edit.sh behavior), the tool call is never denied;
#   * blocking → grant: exit 0; foreign claim: DENY: exit 2 + a
#                PreToolUse JSON permissionDecision naming the holder.
# Claims are released by release-guard.sh on PostToolUse.
#
# Mode (resolved per call by hooks/mode.sh, highest wins):
#   1. ZOANTHROPE_MODE env (advisory | blocking) — per-agent override;
#   2. .zoanthrope/config.json {"mode": "..."} at the workspace root;
#   3. default: blocking.
# An invalid value never crashes the hook: one warning on stderr, then
# blocking. release-guard.sh / watch.sh never deny, so mode does not
# apply to them.
#
# A Bash command whose mutations cannot be parsed claims nothing here;
# watch.sh fingerprints the change afterwards and reports an
# `unclaimed-mutation` audit line. That is the trail for unparseable
# commands — the guard never blocks what it cannot see.
#
# Env:
#   ZOANTHROPE_WORKSPACE  workspace root (default: git toplevel / pwd)
#   ZOANTHROPE_BIN        zoanthrope binary (default: on PATH)
#   ZOANTHROPE_AGENT      claim identity (also ZOANTHROPE_AGENT_NAME)
#   ZOANTHROPE_MODE       runtime mode override (advisory | blocking)
#   ZOO_LOG / ZOANTHROPE_LOG   optional JSONL audit log

set +e
# Shared mode resolution (env > .zoanthrope/config.json > blocking).
# A missing helper must never crash the guard: with set +e the source
# error is absorbed and the unresolved mode falls through to blocking.
. "$(dirname "${BASH_SOURCE[0]:-$0}")/../mode.sh"
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

RESULT="$("$ZOO" claim --files "$COMMA" --agent "$AGENT" --intent edit \
    --workspace "$WS_ROOT" 2>/dev/null)"
OK="$(printf '%s' "$RESULT" | python3 -c '
import json, sys
try:
    print("true" if json.load(sys.stdin).get("ok") else "false")
except Exception:
    print("false")' 2>/dev/null)"

# Per-file audit line (the shape the A/B metrics reader consumes).
printf '%s' "$RESULT" | ZOO_GUARD_TS="$TS" ZOO_GUARD_AGENT="$AGENT" \
    ZOO_GUARD_TOOL="$LOG_TOOL" python3 -c '
import json, os, sys
try:
    claim = json.load(sys.stdin)
except Exception:
    claim = None
log = os.environ.get("ZOO_LOG") or os.environ.get("ZOANTHROPE_LOG")
if not log:
    sys.exit(0)
ok = (claim or {}).get("ok") is True
rows = (
    [(g.get("path"), g) for g in (claim.get("granted") or [])]
    if ok
    else [((claim or {}).get("path"), claim)]
)
for path, body in rows:
    if not path:
        continue
    entry = {
        "ts": float(os.environ.get("ZOO_GUARD_TS", "0")),
        "hook": "pre",
        "agent": os.environ.get("ZOO_GUARD_AGENT", "unknown"),
        "tool": os.environ.get("ZOO_GUARD_TOOL", ""),
        "file": path,
        "result": "grant" if ok else "conflict",
        "claim": body,
    }
    with open(log, "a") as f:
        f.write(json.dumps(entry) + "\n")
' 2>/dev/null

# Mode decides AFTER the claim: advisory and blocking claim identically.
MODE="$(zoo_resolve_mode "$WS_ROOT")"
if [ "$MODE" = "advisory" ]; then
    # Advisory = pre-edit.sh behavior: report the claim or conflict JSON
    # on stderr and let the tool call through — never deny, exit 0.
    printf '%s\n' "$RESULT" >&2
    exit 0
fi

if [ "$OK" = "true" ]; then
    exit 0
fi

DENY_JSON="$(printf '%s' "$RESULT" | python3 -c '
import json, sys
try:
    claim = json.load(sys.stdin)
except Exception:
    claim = {}
path = claim.get("path", "?")
holder = claim.get("holder", "unknown")
reason = ("zoanthrope: '\''%s'\'' is claimed by '\''%s'\'' — edit denied. "
          "Wait, then retry (max 3 retries).") % (path, holder)
print(json.dumps({"hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "deny",
    "permissionDecisionReason": reason}}))
' 2>/dev/null)"
if [ -z "$DENY_JSON" ]; then
    DENY_JSON='{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"zoanthrope: claim failed — edit denied."}}'
fi
printf '%s\n' "$DENY_JSON"
printf '%s\n' "$DENY_JSON" >&2
exit 2
