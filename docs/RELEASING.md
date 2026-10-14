# Releasing zoanthrope

Everything here exists so a release is cut by **pushing a tag**, nothing
else. No local build, no manual upload, no long-lived tokens.

## Tag convention

- Tags are git tags: `vX.Y.Z` (e.g. `v0.1.0`).
- Pre-releases: `vX.Y.Z-rcN` (e.g. `v0.1.0-rc1`). They publish to npm
  under the `next` dist-tag; stables take `latest`.
- **Tag only after CI is green on `main`** for the exact commit being
  tagged. `release.yml` enforces this (it refuses to build a tag whose
  commit has no successful completed `push` CI run on `main`).
- Before tagging, the version must match in **all** of:
  - `Cargo.toml` (`version`)
  - `npm-shim/package.json` (`version`)
  - `server.json` (`version` and every `packages[].version`)
  - `CHANGELOG.md` (an entry for the version)

  `release.yml`'s `version-check` job fails the release on drift; it
  never rewrites the files silently.

## Cut a release

```sh
# 1. on main, with green CI for HEAD
git switch main && git pull

# 2. bump versions + CHANGELOG (chore commit), push, wait for CI
#    (skip if the tree already carries the release version)

# 3. tag and push the tag — that is the whole trigger
git tag v0.1.0
git push origin v0.1.0
```

## What the workflows do

### `.github/workflows/ci.yml` (push to `main` + PRs)

| Job | What it runs |
|---|---|
| `lint` | `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` |
| `test` | `cargo test` (the full suite) |
| `tlc` | `cd formal && ./check.sh` — TLC over every spec in `formal/MANIFEST` (the **formal gate**) |
| `chaos-smoke` | `clobber_chaos_smoke` (2×8, both modes) |
| `chaos-full` | full chaos run — `workflow_dispatch` only, inputs `workers` / `iters` / `mode` |

A `concurrency` group cancels superseded runs per ref.

**Branch protection**: mark `lint`, `test`, `tlc`, and `chaos-smoke` as
required status checks on `main`. `tlc` is the formal gate — do not
release without it. (Configure at
`https://github.com/spuentesp/zoanthrope/settings/branches`.)

**TLC jar**: `formal/check.sh` needs `TLA2TOOLS_JAR` pointing at
`tla2tools.jar` (v1.8.0). The jar is **not vendored** in the repo; the
`tlc` CI job downloads the official
`https://github.com/tlaplus/tlaplus/releases/download/v1.8.0/tla2tools.jar`
and pins it by SHA256
(`7beec0f04818732a62fa193731711a99aa4f11279499b2360a7d156c519ea78d`)
before running the checker. Locally:

```sh
TLA2TOOLS_JAR=/path/to/tla2tools.jar formal/check.sh
```

### `.github/workflows/release.yml` (tag `v*`)

| Job | What it does |
|---|---|
| `ci-status-check` | refuses to release a commit without a green completed CI run on `main` |
| `version-check` | tag vs `Cargo.toml` / `npm-shim/package.json` / `server.json` |
| `build` (×5) | release binaries for Linux/macOS × x86_64/aarch64 plus Windows x86_64 → `zoanthrope-{target}.tar.gz` + per-artifact `.sha256` |
| `release` | aggregate `SHA256SUMS`, compose notes from `docs/release-notes-template.md`, `gh`-release the assets (created only after **all** builds pass) |
| `publish-npm` | `npm publish --provenance --access public` via OIDC (no `NPM_TOKEN`) |
| `publish-npm-retry` | `workflow_dispatch` escape hatch: re-publish npm for an existing tag without rebuilding |

Asset-name contract (the npm shim downloads exactly this):
`zoanthrope-{target}.tar.gz` with the platform binary at the archive
root — bare `zoanthrope` on Linux/macOS, `zoanthrope.exe` on Windows —
checksums in `SHA256SUMS`.

## npm trusted publishing (OIDC) — one-time owner configuration

The publish job authenticates with npm Trusted Publishing. There is no
`NPM_TOKEN` secret to create, rotate, or leak. Configure at
`https://www.npmjs.com/package/@spuentesp/zoanthrope/access`:

| Field | Value |
|---|---|
| Provider | **GitHub Actions** |
| Repository | **`spuentesp/zoanthrope`** |
| Workflow filename | **`release.yml`** *(basename only)* |
| Environment | *(blank)* |
| Permissions | **`npm publish`** *(not `npm stage publish`)* |

The job's `id-token: write` permission is what makes the OIDC exchange
possible; do not remove it. The npm client must be ≥ 11.5.1 for OIDC —
the workflow upgrades to npm 12 first (older clients silently fall back
to a credential-less publish and the registry 404s the PUT).

The package name is `@spuentesp/zoanthrope`; the shim's `postinstall`
downloads the platform binary from this repo's GitHub release, which is
why `publish-npm` runs **after** the release assets are up.

## Register in the MCP registry

`server.json` at the repo root is ready for
`registry.modelcontextprotocol.io`. It validates against the published
schema `https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json`
(registry name form: `io.github.spuentesp/zoanthrope`; the registry
caps `description` at 100 characters).

To submit (owner action, after the npm package exists):

1. Install the publisher CLI (release binaries are published at
   `github.com/modelcontextprotocol/registry/releases`):

   ```sh
   # NOTE: get mcp-publisher ONLY from these GitHub releases or Homebrew.
   # The npm package named "mcp-publisher" is a different, unrelated tool
   # (the name is squatted) — never `npm i -g mcp-publisher`.
   curl -fsSL -o mcp-publisher.tar.gz \
     "https://github.com/modelcontextprotocol/registry/releases/latest/download/mcp-publisher_linux_amd64.tar.gz"
   tar xzf mcp-publisher.tar.gz mcp-publisher
   ```

   (darwin_amd64 / darwin_arm64 / linux_arm64 variants exist; or
   `go install github.com/modelcontextprotocol/registry/cmd/mcp-publisher@latest`.)
2. `mcp-publisher validate` in the repo root (checks `server.json` against the schema).
3. `mcp-publisher login` — GitHub auth for the `io.github.spuentesp`
   namespace (proving you control `github.com/spuentesp`).
4. `mcp-publisher publish` — publishes this `server.json`.

Alternatively submit via PR to `github.com/modelcontextprotocol/registry`
(see their `docs/reference/server-json/`), or the form at
`modelcontextprotocol.io`. Ownership check for npm packages: the
registry matches `mcpName` in `npm-shim/package.json`
(`io.github.spuentesp/zoanthrope`) against `server.json`'s `name` —
both are already set. Keep `server.json`'s version fields in sync when
bumping versions (the `version-check` job enforces it).

## Release checklist

1. **CI green** on the `main` commit (`lint`, `test`, `tlc`, `chaos-smoke`).
2. **Tag**: `git tag vX.Y.Z && git push origin vX.Y.Z`.
3. **Release assets**: `release.yml` builds the five tarballs,
   `SHA256SUMS`, and the GitHub Release.
4. **npm**: `publish-npm` publishes `@spuentesp/zoanthrope` (verify at
   `https://www.npmjs.com/package/@spuentesp/zoanthrope`; a broken
   publish can be retried with the `publish-npm-retry` dispatch).
5. **Registry**: `mcp-publisher publish` with the repo's `server.json`.
