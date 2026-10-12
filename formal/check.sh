#!/usr/bin/env bash
# Run every spec in formal/MANIFEST and check the outcome matches its
# `expect` column: `pass` rows must be TLC-clean, `fail` rows must
# reproduce their documented counterexample. Any other outcome fails
# the run.
#
#   TLA2TOOLS_JAR=/path/to/tla2tools.jar ./check.sh            # all
#   TLA2TOOLS_JAR=/path/to/tla2tools.jar ./check.sh FsLease    # filter
#
# `TLA2TOOLS_JAR` must point at the TLA+ tools jar (v1.8.0); the jar is
# deliberately not vendored here.
set -uo pipefail
FORMAL="$(cd "$(dirname "$0")" && pwd)"
TLC="${TLA2TOOLS_JAR:-}"
if [[ -z "$TLC" || ! -f "$TLC" ]]; then
    echo "set TLA2TOOLS_JAR to a tla2tools.jar (TLA+ tools v1.8.0)" >&2
    exit 2
fi
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
filter="${1:-}"
bad=0 ran=0
while IFS='|' read -r cfg module expect _; do
    cfg="$(echo "$cfg" | xargs)"
    module="$(echo "$module" | xargs)"
    expect="$(echo "$expect" | xargs)"
    [[ -z "$cfg" || "$cfg" == \#* ]] && continue
    [[ -n "$filter" && "$cfg" != *"$filter"* ]] && continue
    ran=$((ran + 1))
    ( cd "$WORK" && java -jar "$TLC" -deadlock -metadir "$WORK/st_$cfg" \
        -config "$FORMAL/$cfg.cfg" "$FORMAL/$module.tla" >"$WORK/$cfg.out" 2>&1 )
    rc=$?
    case "$expect:$rc" in
        pass:0) printf 'ok    %-34s (pass)\n' "$cfg" ;;
        fail:12) printf 'ok    %-34s (counterexample, as documented)\n' "$cfg" ;;
        *) printf 'FAIL  %-34s expected %s, TLC exit %s\n' "$cfg" "$expect" "$rc"
           tail -n 8 "$WORK/$cfg.out"
           bad=$((bad + 1)) ;;
    esac
done < "$FORMAL/MANIFEST"
echo "$ran specs checked, $bad unexpected"
[[ $bad -eq 0 ]]
