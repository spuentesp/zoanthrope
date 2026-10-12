# zoanthrope {{VERSION}}

Tag: `{{TAG}}`. Built by `.github/workflows/release.yml` from the
tagged commit after green CI on `main`.

## Install

```
npm install -g @spuentesp/zoanthrope
npx @spuentesp/zoanthrope mcp
```

Or grab a binary from the assets below (`zoanthrope-{target}.tar.gz`,
verified against `SHA256SUMS`).

## Assets

- `zoanthrope-x86_64-unknown-linux-gnu.tar.gz`
- `zoanthrope-aarch64-unknown-linux-gnu.tar.gz`
- `zoanthrope-x86_64-apple-darwin.tar.gz`
- `zoanthrope-aarch64-apple-darwin.tar.gz`
- `SHA256SUMS` (checksums the shim verifies downloads against)
- `server.json` (MCP registry metadata)

## Release checklist

1. CI green on the tagged commit (incl. the TLC formal gate).
2. GitHub Release assets uploaded (this release).
3. npm package published via OIDC trusted publishing.
4. MCP registry entry published (see `docs/RELEASING.md`).

## npm trusted-publisher configuration (one-time, owner)

At `https://www.npmjs.com/package/@spuentesp/zoanthrope/access`:

| Field | Value |
|---|---|
| Provider | GitHub Actions |
| Repository | spuentesp/zoanthrope |
| Workflow filename | `release.yml` *(basename only)* |
| Environment | *(blank)* |
| Permissions | `npm publish` |

No `NPM_TOKEN` secret exists or is needed — the publish job authenticates
via GitHub Actions OIDC (`id-token: write`) with `--provenance`.
