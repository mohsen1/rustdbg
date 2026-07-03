---
name: rust-debugger
description: Debug a Rust program or failing test with rdbg — set breakpoints (line, function, conditional, hit-count, panic, or watchpoint), run and step, read locals as real Rust values (Vec/String/struct/enum), change a variable mid-run, and jump to definition/hover/references via rust-analyzer. Reach for it on a *runtime* question in large or complex code — a wrong computed value, an unexpected branch/type/state, or a panic — where reading and grep have stalled and you need to see actual runtime state instead of adding println!/dbg!. Read first for small, localized, or missing-output bugs a quick read already pins down; the debugger earns its cost mainly where the code is too large or the flow too tangled to trace by eye.
---

# rust-debugger

Debug from the command line with `rdbg`. It holds one paused process per project,
so breakpoints and state carry across calls. The target must be built with debug
info (the default `cargo build`). Run `rdbg` with no arguments for the full list.

Requires `rdbg` on `PATH` (`curl -fsSL https://azimi.me/rust-debugger-skill/install.sh | sh`),
plus `rust-analyzer` and `lldb-dap`.

## When to reach for it (and when not)

Read first. The debugger earns its cost only on a question you can't answer by
reading — and on a large repo every `launch` rebuilds, so a wasted debugging detour
is expensive. Decide *before* you launch:

**Reach for rdbg** when you have a **runtime question at a place you can name**:
- a value is **wrong** and you need the real inputs/flow that produced it;
- an **unexpected branch, type, or state** at runtime that reading can't pin down;
- a **panic** — `rdbg debug --panic` lands on the culprit frame in one call;
- you want to **test a fix live** with `set --then continue` before editing + rebuilding.
The biggest wins: wrong or extra output you can break at and trace *backward* to the
deciding code, in a codebase too large to follow by eye.

**Don't launch — just read/grep — when**:
- the failing test plus a quick read already point at the fix (small, localized bugs):
  debugging only adds fixed build + session overhead;
- the output is **missing** — nothing is emitted to break on, so finding the *absent*
  check is a reading task; the debugger can't trace code that never ran;
- you're **iterating on a candidate fix**: re-run the narrowed test, or validate the
  hypothesis with `set --then continue` — don't re-`launch` after every edit.

**Stay cheap.** Keep launches few: one session with several breakpoints, or one
`trace`, beats re-launching (each rebuilds). If 2–3 probes haven't localized it, stop
and go back to reading — you're probably at the wrong layer, and more debugging will
only burn tokens.

## Start a session

```
rdbg where parse_config                            # find where to break
rdbg launch --cargo . --bin app --break src/config.rs:88 -- --threads 4
rdbg launch --cargo . --lib --break src/lib.rs:42 -- my_test        # a #[test] in the library
rdbg launch --cargo . --test mytest --break tests/mytest.rs:12 -- some_case  # tests/mytest.rs
rdbg launch --bin-path target/debug/app --break src/main.rs:11   # skip the build
```

Pick the target by where the test lives: `--lib` for a `#[test]` inside the
library (`#[cfg(test)] mod tests` in `src/` — the common case), `--test <name>`
only for an integration test file `tests/<name>.rs`. In both, the words after
`--` are the test-name filter, so exactly the test you name runs.

Add `--panic` to also stop where any panic is raised, or `--break-fn <name>`.

To watch a value evolve without stepping, `trace` instead of `launch` — it runs
through every hit and returns a table in one call:

```
rdbg trace --cargo . --bin app --break src/x.rs:42 --capture i,sum --max 30
rdbg trace --cargo . --lib --break src/lib.rs:42 --capture a,b -- my_test
```

## Breakpoints

Set or change these any time, including while paused.

```
rdbg break src/x.rs:42                # line
rdbg break src/x.rs:42 --if "i == 5"  # conditional (simple comparisons)
rdbg break src/x.rs:42 --hit 3        # on the 3rd hit
rdbg break src/x.rs:42 --log "i={i}"  # logpoint (print, don't stop)
rdbg break --fn my_crate::do_thing    # entering a function
rdbg break --panic                    # where a Rust panic is raised
rdbg watch cfg.threads                # when a value changes
rdbg breaks                           # list with ids; break-rm/break-on/break-off <id>
```

## Run and step

```
rdbg continue
rdbg continue --until 'sum >= 100'    # keep resuming until a condition holds
rdbg step over | in | out | insn
rdbg until src/x.rs:99                 # run to a line
rdbg pause                            # interrupt a running program
rdbg restart
```

`continue --until '<path> <op> <value>'` (ops `== != < <= > >=`) re-checks the
condition at each breakpoint stop itself — one call instead of a
continue/eval loop per iteration, and it works where lldb conditional
breakpoints don't fire. Needs an active breakpoint to stop at; ends at the
first stop where the condition holds, or reports that the program exited.

## Read and change state

```
rdbg vars                             # locals with real Rust values
rdbg eval items[0].qty sum            # one or more variable paths (not method calls)
rdbg set cfg.threads = 8 --then continue   # change a value and resume
rdbg set cfg.threads = 8              # change a value
rdbg watch-expr add total             # re-shown at every stop
rdbg bt                               # backtrace
rdbg list                             # source around the current line
rdbg state                            # stop + locals + watches together
```

## Threads and frames

```
rdbg threads
rdbg thread <id>
rdbg frame <n> | up | down            # vars/eval follow the selected frame
```

## Navigate

```
rdbg where <Name>
rdbg def | hover | refs <file> <line> <col>
```

`rdbg stop` ends the session; `rdbg down` stops the daemon.

## Common loops

- **Wrong value.** Break where it is computed, `vars` and `eval` to see the real
  inputs, `step` to watch it go wrong, `set` to test a fix without recompiling.
- **Value goes wrong at some iteration.** Break in the loop, then
  `continue --until 'sum > 100'` to jump straight to the first stop where the
  condition holds instead of continue/eval-ing by hand.
- **Panic.** `rdbg debug --cargo . --lib --panic -- <test>` (or `--bin`/`--test`)
  runs to the panic and returns the message, the first *user* frame with its
  arguments and locals, and a backtrace in one call. (Or `launch … --panic`, then
  `bt`/`up` to your frame, if you want to keep poking around after.)
- **Unexpected mutation.** `watch <var>`, then `continue` to stop the moment it
  changes.
- **Failing test.** `--lib … -- <test_name>` for a `#[test]` in the library,
  `--test <name> … -- <test_name>` for `tests/<name>.rs`; break at the assertion
  or inside the code under test.

## Notes

- `eval`, `set`, and conditions take variable paths and simple comparisons, not
  arbitrary Rust expressions. `codelldb` on `PATH` lifts this.
- Debug the debug build; a `--release` binary has little to inspect.
- One paused process per project; `rdbg down` (or 30 minutes idle) releases it.
