# Changelog

All notable changes to zoanthrope. Versions follow `vX.Y.Z` git tags
(see `docs/RELEASING.md`).

## 0.1.2 — 2026-10-12

- File identity: one canonical claim key — absolute/relative/`..` spellings of the same file now resolve to a single claim (a lock whose key can duplicate is a broken lock).
- Claims bind content: a `ContentStamp` (mtime+size, sha256 for small files) is recorded at claim time; guarded writes against a changed file are denied as `stale-claim` — re-read, re-claim. Preventive locking now also prevents stale overwrites, not just concurrent ones.
- Watcher attribution requires the claim to precede the mutation (post-hoc claims no longer silence `unclaimed-mutation`); default watch set is the git-tracked file list.

## 0.1.1 — 2026-10-12

- npm shim: `bin` entry repaired (npm requires `./`-prefixed bin paths; 0.1.0 shipped without the `zoanthrope` command linked). 0.1.1 supersedes it.

## 0.1.0 — 2026-10-12


First release.

### Added

- **Claims** — file claims with `read`/`edit` intents over a workspace;
  all-or-nothing conflict refusal that names the holder, their intent,
  and whether they look stale (`claim_files`, `list_claims`).
- **Presence** — agent registration, liveness, and the rejoin path for
  agents that went away and came back (`register_agent`, `who_am_i`,
  `list_active_agents`, `rejoin_agent`).
- **Postboard** — a public board of intentions: post, list, update,
  remove (`post_intention`, `list_posts`, `update_post`,
  `remove_post`).
- **Filesystem leases** — per-path `create_new` leases with an O_EXCL
  acquire guard, keeping concurrent agents from clobbering each other.
- **Bash-aware claim guards** — heuristic parse of shell commands into
  the workspace files they would mutate (`mutation-files`), plus
  advisory and blocking hook modes.
- **MCP server + hook CLI** — `zoanthrope mcp` (stdio JSON-RPC) and
  `zoanthrope claim` / `release` / `rejoin` / `status`; state registry
  at `.zoanthrope/state.json`, capped audit ring (`get_audit_log`).
- **Formal specs** — TLA+ models for claim arbitration, leases, and
  the rejoin protocol (`formal/`), with `formal/check.sh` driving TLC
  over `formal/MANIFEST` — including the documented negative controls
  and residual risks.
- **Chaos harness** — `clobber_chaos` double-write detector with a fast
  smoke lane (2×8, both modes) and a full run (`zoo-bench` alongside).
- **Release machinery** — CI (fmt/clippy/tests, TLC formal gate, chaos
  smoke), tag-driven release workflow with SHA256-checksummed binaries,
  npm-installable shim (`@spuentesp/zoanthrope`), and MCP registry
  metadata (`server.json`).

### Verified

- 76 tests green; TLC: 11 specs checked, 0 unexpected
  (proofs plus documented counterexamples).
- Chaos smoke: zero undetected double-writes in both modes; full chaos
  run available on demand.
