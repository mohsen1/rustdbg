# Benchmarks

Does a coding agent fix bugs faster and cheaper when it can reach for `rdbg`?

## tsz fix-rate benchmark (`bench_tsz.py`)

Real merged bug-fixes in `tsz` (a ~1.7M-line Rust type-checker), run twice per bug —
once plain, once with `rdbg` — measuring **fix rate, tokens, and wall time**. Cases are
chosen post-training-cutoff and each is a clean single-commit checkout at the fix's
parent with only the regression test overlaid, so the agent cannot look the answer up
(no future history, web tools disallowed). See
[`results-tsz/README.md`](results-tsz/README.md) for the method, results, and honest
caveats.

Headline: **−41% tokens overall, 3/3 fix rate both ways** — `rdbg`'s value scales with
how expensive the bug is to *read* (−49%/−70% on hard-to-localize bugs; net overhead on
a bug that reads cheaply).

Runs make real API calls and cost money. The `with` condition needs `rdbg` on PATH
(`curl -fsSL https://azimi.me/rust-debugger-skill/install.sh | sh`) plus `rust-analyzer`
and a debug adapter (`install.sh` sets up codelldb).

```sh
# mine cases -> results-tsz/cases-tsz.json (see bench_tsz.py header), then per slot + capped image:
python3 bench_tsz.py --slot A --image /Volumes/tszA --cases 0,1,2
```
