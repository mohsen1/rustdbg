# Changelog

## 0.1.0

- `rdbg` CLI and `rdbg mcp` MCP server (24 tools) over one per-project daemon
  that holds a paused `lldb-dap` session and a warm `rust-analyzer`.
- Breakpoints: line, function, conditional, hit-count, logpoint, panic, and
  watchpoint, with list / remove / enable / disable.
- Run control: continue, step over/in/out/instruction, run-to-line, pause,
  restart.
- Inspect and change state: readable Rust locals, variable-path eval, set
  variable, watch expressions, backtrace, source listing.
- Navigation via rust-analyzer: where / definition / hover / references.
- Threads and stack-frame selection.
- Prebuilt binaries for macOS (arm64, x86_64) and Linux (x86_64 musl, arm64).
