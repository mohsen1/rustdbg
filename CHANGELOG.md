# Changelog

## 0.3.0

- `--lib` for `rdbg launch` / `rdbg trace` (MCP `debug_launch` / `debug_trace`
  accept `lib: true`) — build and debug the library's own unit-test binary
  (inline `#[cfg(test)] mod tests`), with the test name after `--` as the
  filter: `rdbg launch --cargo . --lib --break src/lib.rs:42 -- my_test`.
- Forgiving `--test <name>`: when `<name>` is not an integration-test target
  (no `tests/<name>.rs`) but matches a `#[test]` in the library's unit tests,
  rdbg falls back to `--lib` with `<name>` as the test filter; otherwise the
  error spells out the correct `--lib` invocation instead of cargo's bare
  "no test target named".
- Actionable launch errors: unknown args, missing `--break`, missing
  `--cargo`/`--bin-path`, and a nonexistent `--bin-path` all say the correct
  invocation; a failed cargo build returns the rendered compiler diagnostics.
  Build failures no longer `exit(2)` out of the MCP server.
- `rdbg do '<cmd>; <cmd>; ...'` / MCP `debug_do` — run several subcommands in one
  call. Each is labeled with its command; the batch stops at the first error or
  program exit. One call instead of a fixed break/inspect/continue recipe.
- Delta stops — each stop now also lists just the top-frame locals that changed
  since the previous stop (`~ sum: u32 = 6 (was 3)`, `+ new`, `(+N unchanged)`),
  not the full dump. `rdbg vars --full` (MCP `debug_locals` `full:true`) forces
  the complete deep dump; `rdbg vars` is unchanged.

## 0.2.0

- `rdbg trace` / MCP `debug_trace` — run through breakpoint hits without stopping
  and return a compact table (one call instead of break/inspect/continue per
  hit). `--capture <paths>` evaluates variable paths at each hit; `--max` caps it.
- `rdbg eval <path>...` takes multiple paths in one call.
- `rdbg set <path> = <value> --then continue|step` — change a value and resume,
  to test a fix live.
- `eval` (and `trace` capture) resolve `&reference` paths via the variables tree,
  so `it.qty` on a `&Item` works instead of erroring.


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
