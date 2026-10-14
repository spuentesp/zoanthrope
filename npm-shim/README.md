# @spuentesp/zoanthrope (npm shim)

Thin Node launcher for the `zoanthrope` binary. On install it
downloads the platform binary from the matching GitHub release asset
(`zoanthrope-{target}.tar.gz`), verifies it against the release's
`SHA256SUMS`, and caches it per version. Node builtins only — no
runtime dependencies.

```
npx @spuentesp/zoanthrope mcp
zoanthrope claim --files a.rs --agent alpha
```

Environment knobs:

- `ZOANTHROPE_BIN` — use this binary path instead of downloading.
- `ZOANTHROPE_VERSION` — override the release version to download
  (defaults to this package's version).
- `ZOANTHROPE_CACHE_DIR` — override the binary cache directory.

Prebuilt targets: Linux and macOS (x86_64, aarch64) and Windows
(x86_64). Anything else
(or a failed download) gets a build-from-source message:
`cargo install --path .` from a clone of the repository.

What zoanthrope is, the tool surface, and the quickstart live in the
repository's top-level `README.md`.
