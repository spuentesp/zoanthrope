# Hooks

Pre-edit hooks claim the file an agent is about to write, so the
swarm sees the intention before the edit lands. Advisory vs blocking
is a **runtime mode**, not an installation choice: one wiring of
`claude-code/claim-guard.sh` serves both.

## Modes: advisory vs blocking

`claim-guard.sh` resolves the mode per tool call (shared helper
`hooks/mode.sh`), highest priority wins:

1. `$ZOANTHROPE_MODE` env var (`advisory` | `blocking`) — the
   per-agent override: a swarm can block its bots and leave one
   advisory actor;
2. `.zoanthrope/config.json` at the workspace root, found by walking
   up to the git toplevel like the CLI (the file is optional, only a
   `mode` key is read):

   ```json
   {"mode": "advisory"}
   ```

3. default: `blocking` (the experiment-proven behavior).

In `advisory` mode the guard claims the file, prints the
claim/conflict JSON to stderr, and always exits 0 — never a
`permissionDecision: deny`. In `blocking` mode a conflict denies the
tool call (deny JSON, exit 2, reason naming the holder). An invalid
value falls back to `blocking` with one warning line on stderr — a
hook never crashes. `release-guard.sh` and `watch.sh` never deny, so
the mode does not apply to them (releases still run, so advisory
claims still close their windows).

`claude-code/pre-edit.sh` is the **legacy advisory alias**: kept for
existing wirings, always advisory, exits 0 unconditionally. New
wiring uses `claim-guard.sh` and picks the mode at runtime.

## Guard family (Claude Code)

`claude-code/claim-guard.sh` + `release-guard.sh` + `watch.sh` are the
guard family: the same claim flow in both modes, and in blocking mode
(the default) a conflict **denies** the tool call (PreToolUse
`permissionDecision: deny`, exit 2, reason naming the holder) instead
of only reporting it. Wire them in `.claude/settings.json` with
`PreToolUse` / `PostToolUse` matchers for `Edit|Write|MultiEdit`
**and** `Bash`, and the `.*` matcher for the watch hook — the wiring
is the same for both modes.

The guard covers the whole clobber vector, not just `Edit`/`Write`:

* **`Edit`/`Write`/`MultiEdit`** — `tool_input.file_path` is claimed
  and released in both modes (the claim flow is `pre-edit.sh`'s;
  only the mode decides whether a conflict reports or denies).
* **`Bash`** — `tool_input.command` is parsed by
  `zoanthrope mutation-files` into the workspace files the command
  would **mutate**, and those are claimed in one all-or-nothing call
  before the command runs. Recognized idioms (heuristic allowlist,
  see `src/bash_parse.rs`):
  - redirections `> FILE`, `>> FILE`, `&> FILE` (heredoc/here-string
    payloads are not scanned for further redirects);
  - `sed -i` / `sed -iSUFFIX` / `--in-place` file operands;
  - `tee` output files;
  - `cp`/`mv`/`rename` destinations (last operand, or `-t DIR`);
  - `python -c` / `perl -e` (and heredoc scripts like
    `python3 - <<'PY'`) write sinks: `open(path, "w"-ish)`, perl
    three-arg and glued-mode `open`, `Path(…).write_text/write_bytes`;
  - nested `sh -c '…'` / `bash -c '…'` scripts, recursively.

  Deliberate limits: no `$VAR` expansion, no `eval`/command
  substitution (paths must be literal), no `dd of=`, `truncate`,
  `install`, `rsync`, editors, computed paths, or perl `open` with a
  variable path. When a mutation cannot be parsed the command is
  **not** blocked — `watch.sh` fingerprints tracked files after every
  tool call and writes an **`unclaimed-mutation`** audit line for any
  change that was not preceded by any claim grant for the file (lookback
  `ZOANTHROPE_WATCH_LOOKBACK`, default 60 s). Unparseable writes are
  therefore always visible in the audit trail, never silently lost.

Environment (all guard hooks): `ZOANTHROPE_WORKSPACE` (root; default
git toplevel), `ZOANTHROPE_BIN` (default `zoanthrope` on PATH),
`ZOANTHROPE_AGENT` (identity), `ZOANTHROPE_MODE` (runtime mode
override), `ZOO_LOG`/`ZOANTHROPE_LOG` (JSONL audit log).
`watch.sh` additionally takes `ZOANTHROPE_WATCH_FILES`
(comma-separated absolute paths) and `ZOANTHROPE_WATCH_STATE`.

## The pattern

Every agent framework's hook does the same four things:

1. read the target file path from `$1` or from the framework's stdin
   JSON (`"file_path"` field);
2. resolve the agent name (explicit override, then framework env vars,
   then a stable parent-derived fallback);
3. run `zoanthrope claim --files "$FILE_PATH" --agent "$AGENT_NAME" --intent edit`;
4. exit 0 unconditionally.

The claim CLI prints exactly the JSON the `claim_files` MCP tool
returns — on success the grant shape, on refusal the conflict shape
with `holder` / `intent` / `claimed_at` — so a hook stays a one-liner
and the agent can read who is in the way:

```json
{"caller_stamps":[],"claimed_at":1791780567.1,"holder":"beta","holder_stale":false,"intent":"edit","ok":false,"path":"src/a.rs","purpose":"fix bug"}
```

## Reference hook

[`claude-code/pre-edit.sh`](claude-code/pre-edit.sh) is the reference
claim hook (Claude Code `PreToolUse` for Edit/Write/MultiEdit) and now
serves as the **legacy advisory alias**: still valid, always exits 0 —
but new wiring should use `claim-guard.sh`, which runs the same claim
flow and honors the runtime mode (so `advisory` behaves exactly like
this script). To port `pre-edit.sh` to another agent, change two
strings — the comment header and the fallback identity prefix:

```console
$ sed -e 's/claude-code/codex/g' hooks/claude-code/pre-edit.sh > hooks/codex/pre-edit.sh
$ chmod +x hooks/codex/pre-edit.sh
```

If the framework pipes a different stdin envelope, adjust the one
`sed` that extracts `"file_path"` (or pass the path as `$1` if the
framework supports it). Everything else — identity resolution, the
fail-open discipline, the claim call — stays identical.

## Identity

| Env var | Effect |
|---|---|
| `ZOANTHROPE_AGENT_NAME` | explicit agent name (wins) |
| `CLAUDE_AGENT_NAME` | becomes `claude-code-$CLAUDE_AGENT_NAME` |
| `MCP_CLIENT_NAME` / `AGENT_NAME` | used as-is |
| (none) | `<kind>-<ppid>-<hostname-short>` |

Release hooks are the same shape with `zoanthrope release --files …`.
Agents that go away and come back should call `zoanthrope rejoin
--agent …` once after startup — it re-asserts their claims and reports
anything that changed while they were gone.
