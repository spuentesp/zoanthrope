# zoanthrope — swarm coordination consultation

Consultation + debugging for a workspace coordinated by zoanthrope.
Read state; do not change it unless asked. State lives at
`.zoanthrope/state.json` (registry) and `.zoanthrope/locks/` (leases).

## Who holds what

- `zoanthrope status` — one screen: agents (`STALE` markers), claims
  (`path agent [intent] purpose`), the lease layer, the postboard,
  board epoch.
- `list_claims` (MCP) — every claim: `{path, agent, intent, claimed_at,
  purpose, held_by_stale}`. `who_am_i` — one agent's claims + posts.
- `list_active_agents` — everyone, with `stale` flags. Stale agents are
  listed, never dropped.

Claim semantics in one line: `edit` excludes everyone on the path,
`read` co-holds with `read` but not with `edit`, and an agent's
recorded intent is the strongest it holds (a later weaker claim never
downgrades it).

## The postboard

Posts are agents' public intentions — `list_posts` (filter `agent_id` /
`status`) or `status`'s postboard section. Statuses: `open` (announced)
→ `taken` (working) → `done`, or `dropped`. Newest-updated first. A
post with `files` says which claims to expect next.

## Reading a conflict response

A refused claim is not an error — `ok: false` plus:

```json
{"ok": false, "holder": "beta", "path": "a.rs", "claimed_at": …, "purpose": "…", "holder_stale": false, "intent": "edit"}
```

- `holder` — who is in the way; `intent` — what they are doing there.
- `holder_stale: true` — holder silent past the presence TTL. The claim
  **still blocks**; nothing is auto-released.
- `"lease": true` (plus `nonce`) — refused by the filesystem lease, not
  the registry: a lease-only writer holds the file (see
  `.zoanthrope/locks/`).
- Claims are all-or-nothing: one conflict ⇒ nothing granted.

## Debugging

**Stale agent holding claims.** Blocks on purpose. Either it comes back
(`zoanthrope rejoin --agent X` refreshes presence and re-asserts its
claims) or the holder runs `zoanthrope release`. No takeover tool — if
an operator must clear it, edit `state.json` and say so in the swarm.

**Orphaned claims** (registry says X, X is gone): same as stale. If
`status`'s lease section shows nothing for the path, the registry entry
is the only record.

**Orphaned leases** (sentinels, no registry claim): crashed holder left
them. They expire — older than the lease TTL (60s,
`ZOANTHROPE_LEASE_TTL_SECS`) they are cleaned on the next claim of that
path, or remove the `<path>.lock-*` file early.

**`claims_at_risk` after rejoin** — registry credits the path to the
caller but another agent holds a live lease. Rejoin reports, never
rewrites. Ask the holder to release, or wait for its lease to expire.

**State version error** — `state.json` is from another format version.
Move `.zoanthrope/` aside; agents re-claim. Never half-edit the field.
