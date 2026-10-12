# zoanthrope

> The Tyranid creature that relays the Hive Mind's will to the swarm —
> zoanthrope is the coordination relay: who works where, what was
> promised, what happened.

**A swarm coordination MCP server.**
zoanthrope answers one question — *who is working on what?* — for a
workspace of collaborating agents. File claims with read/edit intents,
agent presence, a public postboard of intentions, per-path filesystem
leases against clobbering, a rejoin path for agents that went away and
came back, and a capped audit ring. It keeps a team of agents out of
each other's way and leaves a trail of who promised what.

## What it is

- a small MCP server (`zoanthrope mcp`) over stdio JSON-RPC
- a hook-facing CLI (`zoanthrope claim` / `release` / `rejoin` / `status`)
- a state registry at `.zoanthrope/state.json` plus filesystem leases at
  `.zoanthrope/locks/` — zero daemons, nothing to install and keep alive

## Tool surface

All tools take and return JSON objects. Success bodies carry
`"ok": true`; a refused claim is a *successful* call whose body has
`"ok": false` plus the conflict shape.

| Tool | Arguments | Returns |
|---|---|---|
| `claim_files` | `agent_id`, `mode` (`claim` \| `release` \| `list`, default `claim`), `files` (paths or `{path, purpose?}`), `purpose?`, `intent?` (`edit` default \| `read`) | claim: `{ok, granted: [{path, claimed_at, purpose, intent}]}` or the conflict shape; release: `{ok, released: [path]}`; list: `{ok, claims}` |
| `list_claims` | `agent_id?` | `{ok, claims: [{path, agent, intent, claimed_at, purpose, held_by_stale}]}` |
| `who_am_i` | `agent_id` | `{ok, agent_id, name, kind, last_seen, stale, claims, posts}` |
| `list_active_agents` | `include_stale?` | `{ok, agents: [{agent_id, name, kind, last_seen, stale, claims_count}]}` |
| `register_agent` | `name`, `kind?` | `{ok, agent_id, name, kind, last_seen}` |
| `post_intention` | `agent_id`, `title`, `note`, `files?` | `{ok, post_id, post: {post_id, agent_id, title, files, note, status, created_at, updated_at}}` |
| `list_posts` | `agent_id?`, `status?` | `{ok, posts: [...]}` newest-updated first |
| `update_post` | `agent_id`, `post_id`, `status?` (`open`\|`taken`\|`done`\|`dropped`), `title?`, `note?`, `files?` | `{ok, post_id, post}` |
| `remove_post` | `agent_id`, `post_id` | `{ok, post_id, removed: true}` |
| `rejoin_agent` | `agent_id`, `files?` | `{ok, agent_id, claims_kept, claims_at_risk, view_epoch, board}` |
| `get_audit_log` | `limit?` (default 200), `since_unix?` | `{ok, events: [{ts, agent, action, path?, purpose?}]}` |

### The conflict shape

The heart of the product: a claim that clashes with another agent's
recorded intent is refused *all-or-nothing* and says exactly who is in
the way:

```json
{"ok": false, "holder": "beta", "path": "a.rs", "claimed_at": 1791780567.1, "purpose": "fix the parser", "holder_stale": false, "intent": "edit"}
```

### Claim semantics

A claim carries an intent (`read` or `edit`):

- an **edit** claim conflicts with anything another agent holds on the
  path — exactly one writer at a time;
- a **read** claim conflicts with another agent's **edit**, but readers
  co-hold happily with each other;
- an agent's recorded intent on a path is the **strongest** it holds —
  a later weaker claim never hides a stronger one. Only release (then
  re-claim) downgrades.

Claims are all-or-nothing per call: one conflict in a batch means
nothing is granted. A holder silent past the presence TTL is `stale`,
its claims are `held_by_stale`, and they still **block** — claims are
never auto-released; forcing one open is a policy this product did not
make.

A claim also binds the file's **content**: at claim (or re-claim) time
it records a content fingerprint — `mtime+size` always, plus `sha256`
for files ≤ 256 KiB, so same-size rewrites on coarse filesystems are
still caught. Every guarded write (the hook family) checks the file
against the claim's fingerprint first: if the file changed under the
claim — an out-of-band append, a generator run — the write is refused
with a `stale-claim` denial (holder is self) telling the agent to
re-read and re-claim. Re-claiming re-binds the fingerprint to the
current content; `release` drops it. No deliberate takeover exists: a
claim lock is a content promise, not just a writer lock.

### Filesystem leases (anti-clobbering)

A granted **edit** claim also takes a filesystem lease under
`.zoanthrope/locks/`: a nonce-named sentinel file per path, acquired
under a short `O_EXCL` guard (so concurrent acquirers cannot both pass
the holder scan), released by move-aside-and-verify (so a delayed
release can never delete a replacement holder's file). A live foreign
lease refuses a claim even when the registry is free — a lease-only
writer (an editor's save, a stray process) still gets the file to
itself. Read claims take no lease.

Leases carry a heartbeat mtime; one older than 60s
(`ZOANTHROPE_LEASE_TTL_SECS` overrides) is stale and may be taken
over, again move-aside-and-verify. `status` prints the lease layer so
orphaned sentinels are easy to spot.

### Postboard

Posts are the agents' public intentions — one line a human can read
while debugging a swarm. `post_intention` opens one (`status: open`);
`update_post` moves it through `taken` → `done`, or `dropped` if the
plan changed. Posts do not replace each other; an agent may carry
several. Only the posting agent may edit or remove its post.

### Rejoin

An agent that went away and came back (restart, sleep, disconnect)
calls `rejoin_agent`. It refreshes presence, re-asserts the agent's own
claims (filesystem lease refreshed or re-taken), republishes the board,
and reports — never rewrites. A claim the registry still credits to the
caller but where another agent now holds a live lease comes back as
`claims_at_risk`: reported, not stolen back, not released. Rejoin never
breaks claims, own or foreign.

## Quickstart

```console
$ cargo build --release
$ ./target/release/zoanthrope mcp
```

Then, over stdio (one JSON-RPC message per line):

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"demo","version":"0"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"claim_files","arguments":{"agent_id":"alpha","files":[{"path":"src/a.rs","purpose":"fix the parser"}]}}}
```

Point another agent at the same file and its call returns the conflict
shape above with `"holder": "alpha"`.

Pre-edit hooks use the CLI mirror instead of MCP (see `hooks/`):

```console
$ zoanthrope claim --files a.rs,b.rs --agent alpha --purpose "fix bug" --intent edit
$ zoanthrope release --files a.rs,b.rs --agent alpha
$ zoanthrope rejoin --agent alpha
$ zoanthrope status [--workspace DIR]
```

Exit codes: `0` granted/ok, `1` conflict (or `ok: false`), `2` usage or
IO error. `--workspace` is optional everywhere; without it the root is
found by walking up from the current directory for `.git`.

### Wire the hooks (Claude Code)

The guard family claims every file a tool call would mutate and, in
blocking mode, turns conflicts into tool-call denials. This is the
exact wiring that ran the recorded collision experiments — copy it
into your workspace's `.claude/settings.json` (adjust the paths to
where you keep the repo):

```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Edit|Write|MultiEdit|Bash",
        "hooks": [{ "type": "command", "command": "/path/to/zoanthrope/hooks/claude-code/claim-guard.sh" }] }
    ],
    "PostToolUse": [
      { "matcher": "Edit|Write|MultiEdit|Bash",
        "hooks": [{ "type": "command", "command": "/path/to/zoanthrope/hooks/claude-code/release-guard.sh" }] },
      { "matcher": ".*",
        "hooks": [{ "type": "command", "command": "/path/to/zoanthrope/hooks/claude-code/watch.sh" }] }
    ]
  }
}
```

Each agent needs its identity in the environment (`ZOANTHROPE_AGENT=alpha`,
…) — see `hooks/README.md` for the identity resolution order and how
to port the hooks to other agent frameworks.

**One wiring, both modes.** Advisory vs blocking is a runtime mode,
not a different hook: this wiring serves both. `claim-guard.sh`
resolves the mode per tool call, highest priority wins:

1. `ZOANTHROPE_MODE` env var (`advisory` | `blocking`) — the
   per-agent override, so a swarm can block its bots and leave one
   advisory actor;
2. `.zoanthrope/config.json` at the workspace root (found by walking
   up to the git toplevel, like the CLI; the file is optional):

   ```json
   {"mode": "advisory"}
   ```

3. default: `blocking` (the experiment-proven behavior).

In `advisory` mode the guard claims the file, prints the
claim/conflict JSON to stderr, and always exits 0 — it never emits a
`permissionDecision: deny`. In `blocking` mode a conflict denies the
tool call: exit 2, reason naming the holder (unchanged). An invalid
mode value falls back to `blocking` with one warning line on stderr —
a hook never crashes. `pre-edit.sh` remains as the legacy advisory
alias; `release-guard.sh` and `watch.sh` never deny, so the mode does
not apply to them.

## Instructions for agents

Paste this into your swarm's prompt or `AGENTS.md` — it is the protocol
the collision experiments validated:

```text
A coordination service (zoanthrope) is active in this workspace.
- Before touching shared files, post your intention (`post_intention` /
  `zoanthrope claim --files … --agent <you> --purpose "…"`).
- An edit may be DENIED with a reason naming the holder. That means
  another agent owns the file right now: wait briefly, then retry
  (up to 3 times). If still denied, work on something else and record
  it — never work around the lock (no out-of-band writes).
- Prefer append/insert on shared files; if you must rewrite a file
  whole, read it back after writing and merge anything that appeared.
- Post what you plan to do on the postboard before you start, and
  update the post when done (`update_post`).
- If you return after being away, call `zoanthrope rejoin --agent <you>`
  once — it re-asserts your claims and tells you what changed.
```

For consultation and debugging (who holds what, stale agents, orphaned
claims), give your agents `skills/zoanthrope/SKILL.md`.

## State

Everything lives in `.zoanthrope/state.json` at the workspace root:
claims, agents, posts, the published board (who holds what, stamped
with a generation `epoch`), and an append-only audit ring capped at
1000 events. Writes are atomic (temp file + rename — the rename is the
generation publish) and serialized by a lock file. A state file from a
different format version is refused with instructions; move
`.zoanthrope/` aside to start fresh.

Presence TTL defaults to 15 minutes; `ZOANTHROPE_TTL_SECS` overrides.

## Performance

*Decide in nanoseconds, persist in microseconds, scale to 128
concurrent agents without losing a write.* The guarantee covers write
paths that go through the guarded hooks (`Edit`/`Write`/`MultiEdit`
and recognized `Bash` mutations); unparseable commands are audited as
`unclaimed-mutation` rather than blocked, and generator scripts
(`node script.js`) are not automatically covered unless they write
through recognized idioms — the audit trail remains the safety net.
All numbers measured on this machine; each family reproduces with the
command named.

**Latency** — `cargo run --release --bin zoo-bench`:

| Path | p50 | p99 |
|---|---|---|
| claim+release pair, in memory | **0.30 µs** | 0.92 µs |
| claim+release pair, `state.json` on disk | **387 µs** | 695 µs |
| contended-8 on hot files (after the backoff fix) | 355 µs | **14.5 ms** |
| `list_posts` over 1000 posts | 3.86 ms | — |

Persistence is the cost, the decision is not. The contended-8 p95 is
7.3 ms; under the earlier fixed 20 ms poll the same tail reached
101 ms, and lock acquisition now backs off exponentially to bring it
down. Postboard creation runs at 614 posts/s.

**Clobber chaos** — `cargo test --release --test clobber_chaos --
--ignored` (scale with `ZOO_CHAOS_WORKERS`):

| Workers | Iterations | Violations | Throughput | Conflicts resolved |
|---|---|---|---|---|
| 16 | 9,600 | **0** | — | — |
| 64 | 12,800 | **0** | 588 ops/s | 9,809 |
| 128 | 19,200 | **0** | 591 ops/s | 16,205 |

Prevention holds as the swarm grows. One documented limit, plainly:
the post-hoc stomp *detector* samples faster than it can observe when
writes exceed ~10⁵/s — prevention (the lock) is the guarantee; the
audit trail is best-effort at machine-gun rates.

**The A/B with real agents** — 3 concurrent agents on shared files,
locked arm vs an unguarded control (full protocol and per-run data in
`experiments/zoo-ab-2026-10-12.md`): the locked arm finished with
**0 lost work**, 3 denials that named the holder, all recovered on
retry, at equal wall time (53.1 s vs 52.9 s). Scope, framed as
coverage: tool-mediated edits (`Edit`/`Write`) ride the host harness's
own CAS/OCC — verified **0 lost work in 24 agent runs** — so the lock
is engineered for the vector that escapes those tools: **Bash
mutations, codemods, generators, scripts**. On that vector the
round-3 table reads 0 vs 1 lost, with +6 in-flight clobbers
self-healed in the control.

## Formal guarantees

The coordination core is checked against TLA+ models in `formal/`:

- **`PresenceClaims`** — claim arbitration: no conflicting holders,
  read co-holding, and strongest-intent recording;
- **`FsLease` / `FsLeaseGuard`** — filesystem lease exclusion, the
  anti-clobbering core (move-aside-and-verify release, `O_EXCL`
  acquire guard);
- **`RejoinProtocol`** — returning agents re-assert without breaking
  claims, own or foreign.

Verified with TLC via `formal/check.sh`: **11/11 configurations** — 4
invariant configurations green, and 7 negative-control configurations
reproduce their counterexamples (the specs demonstrably catch the
failure modes they claim to: broken arbitration, unguarded leases,
leaky rejoin). One command to re-verify:

```console
$ cd formal && ./check.sh
```

(`TLA2TOOLS_JAR` points at a `tla2tools.jar`, TLA+ tools v1.8.0; the
spec→code mapping lives in `formal/README.md`.)

## Development

```console
$ cargo test
$ cargo fmt --check
$ cargo clippy --all-targets -- -D warnings
```

`tests/mcp_smoke.rs` spawns `zoanthrope mcp` and runs the full
initialize → tools/list → claim → conflict → postboard round trip →
rejoin → release over stdio. `npm-shim/` is the `@spuentesp/zoanthrope`
node launcher (thin, no download machinery — it forwards to a binary on
`PATH` or `ZOANTHROPE_BIN`).

## License

MIT — see `LICENSE`.
