# `rdbg`

**Debug a Rust program from a coding agent — set breakpoints, run, and read the
actual values of variables instead of guessing from `println!`.** A single
binary that wraps `rust-analyzer` and `lldb-dap`, usable as a CLI skill or an MCP
server for Claude Code and Codex.

Website: <https://azimi.me/rust-debugger-skill/>

```sh
curl -fsSL https://azimi.me/rust-debugger-skill/install.sh | sh
```

```console
$ rdbg launch --cargo examples/demo --bin demo --break src/main.rs:12
>>> STOP [breakpoint] demo::total  main.rs:12  (thread 1)
   ->    12 |         sum += it.qty;
$ rdbg vars
  items: &[Item]
    [0]: Item
      name: String = "apple"
      qty: unsigned int = 3
  sum: unsigned int = 0
$ rdbg eval items[0].qty      # unsigned int = 3
$ rdbg set sum = 100          # change a variable, keep running
$ rdbg step over
```

Watch a value evolve across a loop in **one** call instead of stepping:

```console
$ rdbg trace --cargo . --bin demo --break src/main.rs:10 --capture sum,it.qty
trace: 3 hit(s)
 #1   demo::total  main.rs:10   sum=0  it.qty=3
 #2   demo::total  main.rs:10   sum=3  it.qty=0
 #3   demo::total  main.rs:10   sum=3  it.qty=7
```

By default:

- Breakpoints can be line, function, conditional (`--if`), hit-count (`--hit`),
  logpoint (`--log`), panic, or watchpoint (break when a value changes).
- Locals print as real Rust values — `Vec`, `String`, structs, and enums render
  readably, not as raw pointers.
- One paused process per project is held open between calls, so state survives
  across commands; the daemon shuts down after 30 minutes idle.
- `rust-analyzer` navigation (`where` / `def` / `hover` / `refs`) works
  alongside the live session.

## Why?

An agent editing Rust can read the source but not the run. It sees that
`parse_config` exists; it can't see that `threads` came back `0`. The usual
workaround is to add `println!`, rebuild, read the log, and delete it — a slow
loop that only shows what you thought to print.

`rdbg` gives the agent the other half: break where a value is computed, read the
real inputs, step to watch it go wrong, and change a variable in place to test a
fix. Break on a panic to land on the frame that raised it, with its arguments.
Watch a variable to stop the instant it changes.

## Install

```sh
curl -fsSL https://azimi.me/rust-debugger-skill/install.sh | sh
```

Or with Cargo:

```sh
cargo install --git https://github.com/mohsen1/rust-debugger-skill
```

It needs two things on `PATH`:

- `rust-analyzer` — `rustup component add rust-analyzer`
- `lldb-dap` — from the Xcode command line tools on macOS, or `apt install lldb`
  / `brew install llvm` on Linux. `codelldb`, if present, is preferred and adds
  full Rust expression evaluation.

Build the program you want to debug with debug info (the default `cargo build`).

## Use it as a skill

`rdbg` is the whole interface. Drop [`skill/rust-debugger`](skill/rust-debugger/SKILL.md)
into `.claude/skills/` (or `.agents/skills/` for Codex) and the agent drives the
CLI directly. Run `rdbg` with no arguments for the full command list.

```sh
rdbg where parse_config                              # find where to break
rdbg launch --cargo . --bin app --break src/x.rs:88  # build and run to it
rdbg launch --cargo . --lib --break src/lib.rs:42 -- my_test   # a #[test] in the library
rdbg vars ; rdbg eval cfg.threads sum ; rdbg step over
rdbg set cfg.threads = 4 --then continue             # test a fix live
rdbg trace --cargo . --bin app --break src/x.rs:88 --capture cfg.threads
rdbg break --panic                                   # stop where a panic fires
rdbg watch cfg.threads                               # stop when it changes
```

## Use it as an MCP server

The same binary runs an MCP server (`rdbg mcp`) exposing 24 tools —
`debug_launch`, `debug_step`, `debug_locals`, `debug_eval`, `debug_set`,
`debug_where`, and the rest.

Claude Code — `.mcp.json` in your project, or `claude mcp add rustdbg -- rdbg mcp`:

```json
{ "mcpServers": { "rustdbg": { "command": "rdbg", "args": ["mcp"] } } }
```

Codex — `~/.codex/config.toml`:

```toml
[mcp_servers.rustdbg]
command = "rdbg"
args = ["mcp"]
```

The server picks up the project from the directory it starts in.

## Machine-readable output

Pass `--json` (anywhere in the args) and every command prints its result as one
compact JSON line with a `status` field classifying the outcome — `ok`,
`user_error`, `target_error`, `build_error`, `debug_adapter_error`, `timeout`,
`no_session`, or `no_new_information` (reserved) — so results can be scored
automatically (evals, RL environments, scripts):

```sh
$ rdbg --json launch --cargo . --lib --break src/lib.rs:37 -- my_test
{"ok":true,"status":"ok","stop":{...}}
$ rdbg --json launch --cargo . --test nope -- x
{"ok":false,"status":"target_error","error":"no integration test target 'nope' — ..."}
```

MCP tool results carry the same `status` in `_meta` (`rdbg/status`). The full
per-command schema is in [docs/json-schema.md](docs/json-schema.md).

## How it works

A per-project daemon holds one paused `lldb-dap` session (with the Rust value
formatters loaded) and a warm `rust-analyzer`, and serves commands over a Unix
socket. The CLI and the MCP server are both thin clients of that daemon, so a
breakpoint set in one call is still there in the next and the program stays
paused between an agent's tool calls. State lives in `.rdbg/` — add it to
`.gitignore`.

## Limitations

- `eval`, `set`, and breakpoint conditions take variable paths and simple
  primitive comparisons, not arbitrary Rust expressions. `codelldb` on `PATH`
  lifts this.
- There is no set-next-statement or reverse debugging; `restart` relaunches the
  program.
- Threads stop together. On macOS the worker-thread list at a breakpoint can be
  partial; the stopped thread is always usable.
- Rust value rendering is best on recent `lldb` / `codelldb`; older `lldb`
  (14) shows some containers as raw fields.

## Roadmap

Every command is one turn for an agent, so the theme is collapsing round-trips.
Shipped so far: `trace`, multi-path `eval`, `set --then`, **`rdbg do`** (several
subcommands in one call), **delta stops** (each stop shows only the locals
that *changed* — `~ sum: u32 = 6 (was 3)`), and **predicate run-to**
(`rdbg continue --until 'sum > 100'` — rdbg re-checks the condition itself at
each breakpoint stop, so it works past lldb's condition limits). Next:

- **One-shot panic triage** — `rdbg debug --test t --panic` returns the panic
  message, the first user frame with its arguments, and locals in one bundle.

## Build

```sh
cargo build --release      # target/release/rdbg
cargo test
```

`docker/` has a Debian image that builds `rdbg` and runs a debug session, for
reproducing the Linux setup.

## License

MIT or Apache-2.0, at your option.
