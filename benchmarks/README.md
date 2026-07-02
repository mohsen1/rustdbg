# Benchmarks

Do coding agents fix bugs faster / cheaper when they can reach for `rdbg`? Two
tiers: small planted-bug crates, and real fixed bugs in a large repo.

Runs make real API calls and cost money. The `with` condition needs `rdbg` on
PATH (`curl -fsSL https://azimi.me/rust-debugger-skill/install.sh | sh`) plus
`rust-analyzer` and `lldb-dap`.

## Tier 1 — micro (`bench.py`)

Each task is a small Rust crate with a planted bug and a failing test. The
harness runs an agent headless on the same prompt, once **without** rdbg and once
**with** it, and records wall time, tokens, and whether `cargo test` passes.

```sh
python3 bench.py                                   # all tasks, both agents
python3 bench.py --agents claude --tasks accumulator --repeat 3
```

## Tier 2 — larger repo, real bugs (`bench_repo.py`)

SWE-bench style, on [tsz](https://github.com/tsz-org/tsz) (a ~500k-line Rust
TypeScript type-checker). Each case is a merged bug-fix commit that shipped a
regression test. The harness resets a dedicated worktree to the fix's **parent**
commit, overlays just the regression test, confirms it is red, then runs the
agent to re-derive the fix — the merged commit is the ground truth.

```sh
git -C ~/code/tsz worktree add --detach ~/code/tsz-bench origin/main
cd ~/code/tsz-bench && cargo build --bin tsz          # warm the cache once
python3 bench_repo.py --agents claude --cases 10
```

Cases are mined from git history (behavioral diagnostic / display / narrowing
fixes with a separable regression test and a small non-test diff) and stored in
`results-repo/cases.json`. This is where a debugger earns its keep: the build is
expensive, so replacing rebuild-and-print cycles with one paused inspection
matters more than on toy crates.

## Tasks

- `accumulator` — a data pipeline returns the wrong number (a filter keeps the
  wrong elements). The failure doesn't point at the line; inspecting the
  intermediate value localizes it.
- `panic_index` — an off-by-one index panics on valid input. A panic breakpoint
  lands on the frame with the bad index.

Add a task by dropping a crate under `tasks/<name>/` with a failing test and a
`PROMPT.md`.

## Output

`results/runs.json` plus a printed table of per-run and with-vs-without means
(pass rate, wall seconds, tokens, cost).

## Results (claude, one run each)

| task | bug | tokens without → with | Δ | solved |
|---|---|---|---|---|
| accumulator | wrong filter predicate | 176,998 → 153,945 | −13% | both |
| recursion | wrong base case | 172,199 → 149,685 | −13% | both |
| panic_index | off-by-one index | 175,051 → 179,161 | +2% | both |
| overflow | u8 truncation | 150,314 → 153,101 | +2% | both |
| bracket_depth | wrong transition | 177,014 → 181,432 | +2% | both |

Mean: 170,315 → 163,465 tokens (−4%); 44.9s → 47.8s wall; 5/5 solved in both
conditions.

## Reading the results

Token cost is the primary signal — every extra rebuild-and-print cycle is a
model turn. The benefit concentrates on bugs where reading the code isn't enough
(accumulator, recursion); bugs you can spot by eye break even, because the fixed
cost (one build + analyzer warmup) isn't recovered on crates this small. Wall
time is build-dominated and roughly even here. These are single runs — noisy;
raise `--repeat` and add harder, more realistic bugs to sharpen the signal.
