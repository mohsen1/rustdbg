# tsz fix-rate benchmark — with vs without rdbg

Does giving a coding agent `rdbg` help it fix real bugs in a large codebase? This
measures fix rate, tokens, and wall time on real merged bug-fixes in
[tsz](../../../tsz) (a ~1.7M-line TypeScript type-checker in Rust), with the agent
run twice per bug: once plain, once with `rdbg` + a fingerprint-trace recipe.

Harness: [`bench_tsz.py`](../bench_tsz.py). Raw per-run data:
[`runs-final.json`](runs-final.json). Cases: [`cases-tsz.json`](cases-tsz.json).

## Method (contamination-isolated)

- **Cases** are merged `fix(checker/solver)` PRs from **June 2026** — months past the
  model's training cutoff — each adding a regression test. Chosen to span diagnostic
  shapes: a wrong displayed value, a false-positive (extra) diagnostic, and a missing
  diagnostic.
- Each case is a **clean single-commit checkout at the fix's parent** with only the
  regression test overlaid: `git log`/`show`/`blame` reveal nothing about the fix, the
  fix commit isn't in the object store, and **`WebSearch`/`WebFetch` are disallowed** —
  so the agent cannot look up the answer.
- tsz's own `.claude` (its `tsz-tracing`/`tsz-emit` skills) is **stripped** so both
  conditions are a plain agent; the WITH condition adds only `rdbg`.
- Agent: **Opus, medium effort**. Fix = the crate's regression test passes
  (`cargo nextest` exit 0). Each run is isolated in a capped disk image.

## Results (Opus, 3 cases)

| case | bug type | without rdbg | with rdbg | Δ tokens | fix |
|---|---|---|---|---|---|
| 4da902 | wrong display value | 8.83M / 811s | **4.47M / 498s** | **−49%** | ✓ / ✓ |
| 06943a | false-positive (extra) | 22.9M / 1928s | **6.92M / 866s** | **−70%** | ✓ / ✓ |
| 8292e69 | missing diagnostic | 5.51M / 769s | 10.5M / 1427s | **+91%** | ✓ / ✓ |
| **total** | | 37.2M | 21.9M | **−41%** | **3/3 both** |

## Verdict (honest, not a blanket win)

1. **Fix rate is identical (3/3 both).** `rdbg` does not change *whether* Opus fixes
   these — only the cost.
2. **`rdbg`'s value scales with how expensive the bug is to *read*.** On the two
   hard-to-localize bugs (reading thrashed to 8.8M and 22.9M tokens), tracing to the
   emit site cut cost **49% and 70%**. On the one bug reading localized cheaply (5.5M),
   `rdbg` was net **overhead (+91%)**.
3. **Bug type matters.** Wrong/extra diagnostics suit `rdbg` — break on the emit sink
   and backtrace to the deciding code. A *missing* diagnostic has no emit to trace, so
   the agent traces solver internals the long way; `rdbg` helps less and can cost more.

## The tool fix this depended on

Before a fix to `rdbg`, the *same* case-0 showed WITH costing **more** (9.19M vs 8.45M):
`--break-fn push_diagnostic` exited **silently** for diagnostics that don't route
through that sink, so the agent assumed the function "isn't called" and fell back to
`eprintln` — the manual loop `rdbg` exists to replace. `rdbg` now reports, on program
exit, which breakpoints **did not fire** (`NOT BOUND` vs `bound, 0 hits`). The winning
transcripts show the agent reading `push_diagnostic — bound, 0 hits`, re-targeting to
`emit_render_request`/other sites, hitting the real one, and fixing at half the cost.
The −49%/−70% wins exist only because of that fix.

## Caveats

- **n = 3.** The pattern (big wins where reading thrashes, overhead where it doesn't)
  matches the larger `rlenv` dataset, but three cases is three cases.
- **Opus is the strongest reader**, so this is conservative — the wins came exactly on
  the bugs where even Opus thrashed; a weaker (e.g. Sonnet-tier) agent pays a larger
  reading tax and would likely benefit on more of them.

## Reproduce

```sh
# mine cases -> cases-tsz.json (see bench_tsz.py header), then, per slot + capped image:
python3 bench_tsz.py --slot A --image /Volumes/tszA --cases 0,1,2
```
