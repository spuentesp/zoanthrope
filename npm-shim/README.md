# @spuentesp/zoanthrope (npm shim)

Thin Node launcher for the `zoanthrope` binary. It downloads nothing:
set `ZOANTHROPE_BIN` to the binary's path, or keep `zoanthrope` on your
`PATH` (e.g. `cargo install --path .` from the repo root).

```
npx @spuentesp/zoanthrope mcp
zoanthrope claim --files a.rs --agent alpha
```

What zoanthrope is (and is not), the tool surface, and the quickstart
live in the repository's top-level `README.md`.
