#!/usr/bin/env bash
# Claude Code PreToolUse hook for Edit/Write/MultiEdit.
# LEGACY ADVISORY ALIAS: kept for existing wirings. The supported path
# is one wiring of claim-guard.sh, which resolves the runtime mode
# (env > .zoanthrope/config.json > blocking) — advisory mode behaves
# exactly like this script. Reads the file path from stdin (Claude Code
# passes JSON) and claims it via `zoanthrope claim`. Always exits 0 —
# failure must NEVER block the agent. Stderr is for diagnostics.
#
# Identity resolution order:
#   1. $ZOANTHROPE_AGENT_NAME (explicit override)
#   2. Generic agent env vars any framework may set:
#      CLAUDE_AGENT_NAME, MCP_CLIENT_NAME, AGENT_NAME
#   3. Fallback: "<kind>-<ppid>-<hostname-short>" (stable for the parent)
#
# The claim output is the same JSON body the MCP tool returns — the
# conflict shape names the holder if the file is taken.

set +e  # disable errexit for the rest of the script — must exit 0
trap 'exit 0' ERR  # any error → silent exit 0

FILE_PATH="${1:-}"
if [ -z "$FILE_PATH" ]; then
    STDIN_JSON="$(cat 2>/dev/null || true)"
    if [ -n "$STDIN_JSON" ]; then
        FILE_PATH="$(printf '%s' "$STDIN_JSON" | sed -n 's/.*"file_path"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)"
    fi
fi

if [ -z "$FILE_PATH" ]; then exit 0; fi

# Identity resolution.
if [ -n "$ZOANTHROPE_AGENT_NAME" ]; then
    AGENT_NAME="$ZOANTHROPE_AGENT_NAME"
elif [ -n "$CLAUDE_AGENT_NAME" ]; then
    AGENT_NAME="claude-code-$CLAUDE_AGENT_NAME"
elif [ -n "$MCP_CLIENT_NAME" ]; then
    AGENT_NAME="$MCP_CLIENT_NAME"
elif [ -n "$AGENT_NAME" ]; then
    AGENT_NAME="$AGENT_NAME"
else
    SHORT_HOST=$(hostname -s 2>/dev/null || echo "host")
    AGENT_NAME="claude-code-${PPID:-?}-${SHORT_HOST}"
fi

if ! command -v zoanthrope >/dev/null 2>&1; then
    echo "zoanthrope not on PATH; skipping claim" >&2
    exit 0
fi

# One CLI call, the JSON body on stdout (grant or the conflict shape).
# Printed to stderr as the hook's diagnostic; exit code is ignored on
# purpose — a conflict must inform, never block.
zoanthrope claim \
    --files "$FILE_PATH" \
    --agent "$AGENT_NAME" \
    --intent edit >&2
exit 0
