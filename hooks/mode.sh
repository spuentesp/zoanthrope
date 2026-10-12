#!/usr/bin/env bash
# zoanthrope runtime mode — shared by the guard hooks (source me).
#
# Advisory vs blocking is a runtime mode, not an installation choice:
# one wiring serves both. The guard resolves the mode per tool call:
#
#   advisory  claim, print the claim/conflict JSON to stderr, always
#             exit 0 — the tool call is never denied (pre-edit.sh
#             behavior)
#   blocking  a conflict prints the deny JSON and exits 2 with a
#             reason naming the holder (the experiment default)
#
# Precedence, highest wins:
#   1. $ZOANTHROPE_MODE           per-agent env override — a swarm can
#                                 block its bots and leave one actor
#                                 advisory
#   2. .zoanthrope/config.json    {"mode": "advisory"} at the workspace
#                                 root (git toplevel found by walking
#                                 up, like the CLI; the file is
#                                 optional and only a "mode" key is
#                                 read)
#   3. default                    blocking
#
# An invalid value never crashes a hook: one warning line on stderr,
# then blocking.
#
# Usage:  . mode.sh; zoo_resolve_mode [ROOT]
# Prints "advisory" or "blocking" on stdout, warnings on stderr,
# exits 0 always. ROOT (else $ZOANTHROPE_WORKSPACE, else the git
# toplevel walk from $HOOK_CWD/$PWD) is where the config is looked up.

zoo_mode_warn() {
    printf 'zoanthrope: invalid mode %s — falling back to blocking\n' "$1" >&2
}

# Print the raw "mode" value from config file $1. Exit 1 when the file
# is not a JSON object (the caller warns); print nothing when the
# optional file carries no "mode" key.
zoo_mode_config_value() {
    python3 -c '
import json, sys
try:
    with open(sys.argv[1]) as f:
        d = json.load(f)
except Exception:
    sys.exit(1)
if not isinstance(d, dict):
    sys.exit(1)
v = d.get("mode")
if isinstance(v, str):
    sys.stdout.write(v)
elif v is not None:
    sys.stdout.write(str(v))
' "$1" 2>/dev/null
}

# Workspace root: explicit arg > $ZOANTHROPE_WORKSPACE > walk up from
# $HOOK_CWD/$PWD looking for .git (the CLI's discover_root) > start dir.
zoo_mode_root() {
    if [ -n "${1:-}" ]; then
        printf '%s\n' "$1"
        return 0
    fi
    if [ -n "${ZOANTHROPE_WORKSPACE:-}" ]; then
        printf '%s\n' "$ZOANTHROPE_WORKSPACE"
        return 0
    fi
    d="${HOOK_CWD:-$PWD}"
    start="$d"
    while [ -n "$d" ]; do
        if [ -e "$d/.git" ]; then
            printf '%s\n' "$d"
            return 0
        fi
        parent="${d%/*}"
        if [ -z "$parent" ] || [ "$parent" = "$d" ]; then
            break
        fi
        d="$parent"
    done
    printf '%s\n' "$start"
}

zoo_resolve_mode() {
    raw=""
    if [ -n "${ZOANTHROPE_MODE:-}" ]; then
        # env wins: once set it is authoritative — no config consult
        raw="$ZOANTHROPE_MODE"
    else
        cfg="$(zoo_mode_root "${1:-}")/.zoanthrope/config.json"
        if [ -f "$cfg" ]; then
            if ! raw="$(zoo_mode_config_value "$cfg")"; then
                zoo_mode_warn "in $cfg"
                printf 'blocking\n'
                return 0
            fi
        fi
    fi

    if [ -z "$raw" ]; then
        printf 'blocking\n'
        return 0
    fi

    case "$(printf '%s' "$raw" | tr 'A-Z' 'a-z')" in
        advisory) printf 'advisory\n' ;;
        blocking) printf 'blocking\n' ;;
        *)
            zoo_mode_warn "\"$raw\""
            printf 'blocking\n'
            ;;
    esac
}
