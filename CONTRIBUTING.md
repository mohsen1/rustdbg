# Contributing

`rdbg` is a single Rust binary. `serde_json` is the only runtime dependency.

- Build and test: `cargo build --release` and `cargo test`.
- Layout under `src/`: `dap.rs` (Debug Adapter Protocol client), `lsp.rs`
  (rust-analyzer client), `session.rs` (the debug session and breakpoint model),
  `daemon.rs` (per-project socket server), `client.rs` (the `rdbg` CLI),
  `mcp.rs` (the MCP server), `render.rs` and `util.rs` (helpers).
- One binary, three roles picked from the first argument: the CLI (default),
  the daemon (`__daemon`), and the MCP server (`mcp`).
- `docker/` builds a Debian image and runs a debug session, for reproducing the
  Linux setup.

Open an issue before a large change.
