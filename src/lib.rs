//! zoanthrope library: workspace state registry, filesystem lease
//! layer, MCP tool surface, and the stdio transport.
//!
//! The `zoanthrope` binary is a thin CLI over this; the real-concurrency
//! harnesses (`tests/clobber_chaos.rs`, `src/bin/zoo-bench.rs`) drive
//! the same [`tools::call_tool`] surface the MCP server serves.

pub mod fs_lease;
pub mod mcp;
pub mod state;
pub mod tools;
