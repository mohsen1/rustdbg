# tsz benchmark — with vs without rdbg

Does giving a coding agent `rdbg` help it fix real bugs in a large codebase, without
wasting tokens when it isn't needed? This measures fix rate and tokens on real merged
bug-fixes in `tsz` (a ~1.7M-line TypeScript type-checker in Rust), running the agent
twice per bug: once plain, once with `rdbg`.

Harness: [`bench_tsz.py`](../bench_tsz.py). Detailed writeups:
[`run2-opus-vs-sonnet.md`](run2-opus-vs-sonnet.md) (2 models, 6 cases) and
[`run3-full-sweep.md`](run3-full-sweep.md) (22 cases + multi-trial — the current result).

## Method (contamination-isolated)

- **Cases** are merged `fix(checker/solver)` PRs from **June 2026** — months past the
  model's training cutoff — each adding a regression test, spanning diagnostic shapes
  (wrong value, false-positive, missing, nominal-relation, subclass-ctor, …).
- Each case is a **clean single-commit checkout at the fix's parent** with only the
  regression test overlaid: `git log`/`show`/`blame` reveal nothing about the fix, the
  fix commit isn't in the object store, and **`WebSearch`/`WebFetch` are disallowed** —
  the agent cannot look up the answer.
- tsz's own `.claude` is **stripped** so both conditions are a plain agent; the WITH
  condition adds only `rdbg` + a short tsz pointer (the general skill carries the rest).
- Fix = the crate's regression test passes (`cargo nextest` exit 0). Each run is isolated
  in a capped disk image; the WITH condition uses codelldb.

## Current headline (run 3 — 22 cases, Opus)

| metric | value |
|---|---|
| clean with/without pairs | 16 (of 22; 4 not-red skips, 2 hard cases) |
| aggregate tokens | 86.9M → 46.2M (**−47%**) |
| median Δ | **−29%** |
| fix rate | **32/32 (100%)** |
| cells with *systematic* +100% waste | **none** |

The debugger wins big where the bug is expensive to read (−85% at 26M→3.8M, and a spread
of −30% to −82%) and is at most a small fixed overhead on cheap bugs.

## The finding that made it a product: a triage skill

Earlier runs exposed *token waste* — the agent debugging when it shouldn't (17 launches
hunting a missing diagnostic → **+192%**; 18 edits churning a fix → **+747%**). The fix
was **not tsz-specific**: the SKILL now leads with a triage ("read first; launch only for
a runtime question in code too large to read by eye; skip cheap/missing-output bugs; keep
launches few") and a fix-discipline rule ("fix once, don't churn; validate live with
`set`"). With it:

| case | before (run 2) | after (run 3, single) | multi-trial median |
|---|---|---|---|
| contravariant (missing diag) | +192% | −82% | — |
| nominal (fix-thrash) | +747% | +89% | **−39%** |
| `b01338524f` (cheap) | — | +147% | **−5%** |

The apparent +100% single-run cells (`nominal`, `b01338524f`) are **variance, not waste**:
multi-trialing them (4× per condition) gives median deltas of −39% and −5%. And rdbg
*narrows* variance (nominal WITH spans 4.2–8.4M vs WITHOUT 4.2–16.2M) — grounding makes
the agent more consistent, not just cheaper. So the skill's typical behavior never wastes.

## Why it works (unchanged thesis, now validated at scale)

- **rdbg's value scales with how expensive the bug is to read**, times the model's
  reading tax. Big wins where reading thrashes; ~neutral where it's cheap. On the same
  contravariant bug, Opus (strong reader) got +192% overhead in run 2 while Sonnet
  (thrashes unaided) got −92% — same bug, opposite by model.
- **Bug-type fit.** Wrong/extra diagnostics → break on the emit sink and backtrace to the
  deciding code (big wins). *Missing* diagnostics have no emit to trace → the skill now
  says *read*, don't hunt (which flipped contravariant from +192% to a win).

## Tool fixes this depended on

- **Silent breakpoints** — `rdbg` now reports on exit which breakpoints did *not* fire
  (`NOT BOUND` vs `bound, 0 hits`), so the agent re-targets instead of falling back to
  `eprintln`.
- **codelldb footprint** — `target.preload-symbols false` cut the adapter from ~20GB to
  **636MB** on tsz (measured live); `PR_SET_PDEATHSIG` reaps it if the daemon is killed.
  Without these the WITH condition OOM-spiralled on the big repo.
- **Token metric** — a `claude -p` run can emit >1 `result` event; the harness now takes
  the largest (main run), not the last.

## Caveats (honest)

- **Single-run variance is high** (the false-positive cell measured 3.4M–10.2M across
  runs). The large effects and "no systematic waste" hold at n=1, but tight per-case
  numbers need the multi-trial median — which is the correct measure here.
- **16 clean of 22**: 4 cases were not-red-at-parent (invalid, skipped) and 2 are hard
  for Opus regardless (`4aac` cost 17.6M unaided). A fully rigorous claim wants ~20 clean
  cases × N trials with CIs.

## Reproduce

```sh
# mine cases -> cases-tsz.json (see bench_tsz.py header), then per slot + capped image:
python3 bench_tsz.py --slot A --image /Volumes/tszA --cases 0,1,2,3 --model opus
# multi-trial one case (median beats single-run noise); run from benchmarks/:
python3 multitrial.py <case_idx> 4 /Volumes/tszMT
```
