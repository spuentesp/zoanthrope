# Formal specs for the coordination core

TLA+ models that pin the invariants of zoanthrope's coordination
semantics. Each spec is small enough for exhaustive TLC. They are kept
byte-identical to the models the coordination core was designed
against; the mapping below says where each guarantee lives in this
crate.

## Tooling

TLC (the model checker) lives outside this repo — point
`TLA2TOOLS_JAR` at a `tla2tools.jar` (TLA+ tools v1.8.0; Java 25+
works). `check.sh` runs every row of [`MANIFEST`](MANIFEST) against its
expected outcome and is green when all match:

```console
$ TLA2TOOLS_JAR=/path/to/tla2tools.jar ./formal/check.sh
$ TLA2TOOLS_JAR=/path/to/tla2tools.jar ./formal/check.sh FsLease   # filter
```

`expect=pass` rows must be TLC-clean (exit 0). `expect=fail` rows are
negative controls and documented residuals: each reproduces a
counterexample (exit 12) for a design this crate deliberately does
not implement. The suite is green when every cfg lands on its
documented outcome — the pass rows are the proof that the migrated
system satisfies its invariants.

## Spec → Rust mapping

| Spec | Guarantee | Rust home |
|------|-----------|-----------|
| `PresenceClaims.tla` | **NoConflictingHolders**: no two agents hold conflicting claims on a file. `Grantable`: an edit conflicts with anything, a read conflicts with an edit. Recorded intent is the **strongest** held (`Strongest = TRUE`). | `src/state.rs` — `ClaimIntent`, `intents_conflict`, `State::check_claim` (Grantable), `State::claim` / `apply_claim` (strongest rule on re-claim). One scope only (the path itself; the spec's `file`/`sym` scopes collapse — symbol scopes are code knowledge). |
| `FsLease.tla` | **MutualExclusion**: at most one agent believes it holds the per-path lease. Fixed-path `create_new` acquire; release and steal are move-aside-and-verify, so a delayed release can never delete a replacement holder's file. | `src/fs_lease.rs` — `acquire` (nonce-named sentinel + `create_new`), `release` (rename-and-verify), `take_over_stale`, `refresh`. Adopted mode is the fixed-path exclusive lease (`FsLease_Excl.cfg`); the directory-scan mode (`FsLease.cfg`) is the pre-fix negative control. |
| `FsLeaseGuard.tla` | The scan-then-create acquire is serialized per path by an `O_EXCL` guard file: concurrent acquirers cannot both pass the scan. | `src/fs_lease.rs` — `AcquireGuard` (`create_new` on `<sanitized>.guard`, `GUARD_TTL` takeover by atomic rename). `FsLeaseGuard_Unguarded.cfg` is the pre-fix negative control; `FsLeaseGuard_Stall.cfg` is the frozen-holder residual (accepted). |
| `RejoinProtocol.tla` | **Convergence**: quiescent ⇒ published board equals what the records compute. **NoLostUpdate**: a writer landing mid-rebuild is never lost (clear-before-read). **I7 reader consistency**: a reader sees one generation — board and epoch publish together. | `src/state.rs` — `DirtyFlag::run_if_dirty` (clear-before-read = variant (b)), `State::publish_board` / `rejoin_board`, `Board` + `board.epoch`, `State::rejoin_report` + `RejoinReport`. `RejoinProtocol.cfg` (clear-after-read) is the pre-fix negative control; `RejoinProtocol_VariantB.cfg` is the adopted order and passes. |

## Which cfg is which

| Cfg | Outcome | Why it is here |
|-----|---------|----------------|
| `PresenceClaims` | pass | the arbitration the registry implements |
| `PresenceClaims_LastWins` | fail | negative control: "last claim wins" recording breaks exclusivity |
| `FsLease_Excl` | pass | the lease acquire/release the crate implements |
| `FsLease` | fail | negative control: bare scan-then-create |
| `FsLease_ExclExpiry` | fail | residual: expiry + steal would need fencing tokens; release/steal here are rename-and-verify and the registry claim (no TTL) is authoritative |
| `FsLeaseGuard` | pass | the acquire guard the crate implements |
| `FsLeaseGuard_Unguarded` | fail | negative control: unguarded acquire |
| `FsLeaseGuard_Stall` | fail | residual: a guard holder frozen past `GUARD_TTL` |
| `RejoinProtocol_VariantB` | pass | the rejoin order the crate implements (clear-before-read) |
| `RejoinProtocol` | fail | negative control: clear-after-read loses a writer's mark |
| `RejoinProtocolMidRejoinInvariant` | fail | documents that NoLostUpdate is transiently false mid-rejoin under clear-before-read — Convergence is the owed invariant |

## Divergence notes

- The specs model a long-lived process with lockless writers. This
  crate is zero-daemon: state writers serialize on the `.lock` file and
  every save is an atomic rename, so the lost-mark and two-views
  windows are closed by construction. The clear-before-read +
  epoch-paired publish are still implemented and tested
  (`state::tests::rejoin_lost_mark_guard` and friends), so the
  semantics survive any future long-lived transport.
- `PresenceClaims` has two scopes per file (file-level and
  symbol-level). zoanthrope has one: the file path. The strongest-rule
  still matters — `state::tests::strongest_intent_beats_the_last_claim`
  is the executable form of `PresenceClaims_LastWins.cfg`.
- `FsLease` models a single fixed path; the implementation also keeps
  per-acquire nonces in the filename (nothing can collide with another
  holder's file) and a heartbeat mtime (the TTL window). The
  rename-and-verify release is the spec's Excl-mode release, applied
  owner-only.
