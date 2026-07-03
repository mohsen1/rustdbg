# Changelog

## Unreleased

- SKILL now leads with a **"When to reach for it (and when not)"** triage: read first;
  launch only for a *runtime* question in code too large to follow by eye; skip the
  debugger for small/localized bugs, **missing-output** bugs (nothing to trace), and
  blind fix-iteration; keep launches few. This is the lever against token waste — on the
  tsz benchmark it cut a missing-diagnostic case from **+192% tokens (17 exploratory
  launches) to +26% (1 launch)**, still fixed. General (not repo-specific).
- The debug adapter now **dies with the daemon on Linux** (`PR_SET_PDEATHSIG`), even on
  the daemon's own SIGKILL/OOM/crash where no cleanup code runs. Prevents a ~20 GB
  codelldb (its symbol footprint on a large repo) from orphaning into a memory leak that
  OOMs the next run. (macOS has no PDEATHSIG; there the daemon still reaps on
  relaunch/`rdbg down`.)

- `rdbg status` now reports the selected `adapter` (absolute path) — the live
  session's adapter, or the one `find_lldb_dap` would pick when no session is
  running yet. Lets you confirm which adapter is in play (bundled codelldb for
  richer eval, a PATH `lldb-dap`, or `xcrun`) without reading the daemon
  internals. Surfaced in `--json` and text output, and via MCP `debug_status`.
- `install.sh` now auto-installs **codelldb** per-platform (into
  `~/.local/share/rdbg/codelldb`, kept in its own dir so it finds its bundled liblldb),
  and rdbg prefers it — so `eval` handles **comparisons** (`a == b` → `true`),
  **arithmetic** (`a + b * 2` → `30`), and **tuple/field access** (`p.0`), which plain
  lldb-dap's C++ evaluator rejects. (Rust *method* calls like `v.len()` still can't be
  evaluated — lldb has no Rust codegen to run them; rdbg says so and points you to
  break inside instead.) Falls back to lldb-dap if codelldb can't be fetched; skip with
  `RDBG_NO_CODELLDB=1`, or point rdbg at an existing binary with `RDBG_CODELLDB=/path`.
  The failure hint is adapter-aware. (Eval was the top friction when agents debug real
  large-repo bugs: an agent burned ~10 calls fighting lldb-dap's evaluator, then fell
  back to `eprintln`.)
- One-shot panic triage: `rdbg debug --cargo <dir> [--bin|--test|--lib] --panic [-- ARGS]`
  (MCP `debug_panic`) builds, runs to the panic, and returns ONE bundle — the panic
  message, the first **user** stack frame (std/core panic machinery skipped) with its
  arguments and locals, and a short backtrace — instead of launch→bt→frame→vars. Says
  `no panic — program exited` plainly if it doesn't panic. Hunts the message before
  reading locals and never continues past `rust_panic`, so lldb's Rust String formatter
  can't wedge the adapter on a `--bin` panic.
- Predicate run-to: `rdbg continue --until '<path> <op> <value>'` (MCP
  `debug_continue` with `until`) keeps resuming past breakpoint stops and
  re-checks the condition at each stop — evaluated by rdbg itself via the
  variables tree, not by lldb, so it works where lldb conditional breakpoints
  don't bind or fire. Ops: `== != < <= > >=`; numeric comparison when both
  sides are numbers, string/bool equality otherwise. Returns the first stop
  where the condition holds (marked `>>> UNTIL: condition … held`), or reports
  that the program exited / the 10000-resume safety cap ran out without it
  holding. Needs at least one active breakpoint; plain `rdbg continue` is
  unchanged. One call instead of a continue/eval round-trip per iteration.
- rust-analyzer now starts **lazily** on the first navigation command (`where`/`def`/
  `hover`/`refs`) instead of eagerly on every session. Debug-only sessions — the common
  case — no longer pay its indexing cost, which on a large repo (~1.7M lines) is minutes
  of background CPU/RAM competing with the build and lldb. (Every WITH run in the tsz
  benchmark made 0 navigation calls yet paid the warm-up — a pure wall-time drain.)
- `eval` now redirects instead of leaking lldb's C++ error when handed a Rust
  expression it can't resolve (a comparison `==`, tuple `.0`, `->`, method call): it
  says eval takes variable PATHS, points to `rdbg vars`, and notes that `codelldb`
  adds comparison/arithmetic/field eval. `install.sh` now recommends codelldb. (On a real
  tsz bug, an agent burned ~10 calls fighting the C++ evaluator, then fell back to
  `eprintln` — the loop rdbg exists to replace.)
- When a run ends in program exit, `launch` / `continue` / `do` now report which
  breakpoints **did not fire** — distinguishing `NOT BOUND` (the name/path didn't
  resolve) from `bound, 0 hits` (the code never ran that line/fn on this input).
  Previously an unhit breakpoint just showed a silent `program exited`, which
  repeatedly led agents to assume a function "isn't called" (or the build is
  "optimized/inlined") and fall back to `eprintln`. Function breakpoints now track
  their bind status, and each breakpoint tracks a hit count.
- Global `--json` flag: pass it anywhere in the args and every command prints
  its result as one compact JSON line (the daemon's own response) instead of
  the human text rendering. `launch`/`trace` emit the stop/trace payload or the
  build error the same way. Schema documented in `docs/json-schema.md`.
- Outcome taxonomy: every response now carries a top-level `status` field
  classifying the result as exactly one of `ok | user_error | target_error |
  build_error | debug_adapter_error | timeout | no_session |
  no_new_information` (the last is reserved, not produced yet). `ok:bool`
  stays for back-compat. Derived in the daemon; client-side failures (cargo
  build/target errors, bad args, daemon not responding) use the same envelope.
- MCP: every `tools/call` result carries the daemon's `status` end-to-end in
  `_meta` as `rdbg/status`, so MCP callers can score outcomes without parsing
  the text content.

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
