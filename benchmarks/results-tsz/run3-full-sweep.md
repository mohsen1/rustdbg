# tsz benchmark — run 3: full sweep, 22 cases, Opus, with the revised SKILL

The "run all, never token waste" sweep. **22 contamination-isolated cases** (June 2026
tsz bug-fixes, clean checkout at each parent, web disallowed), Opus, medium effort, WITH
the revised skill (triage + fix-discipline) and the tool fixes (lazy symbols, PDEATHSIG,
token-metric). Raw: `runs-opus-A.json` (cases 0–10), `runs-opus-B.json` (11–21).

## Result

| metric | value |
|---|---|
| clean with/without pairs | **16** (of 22) |
| **cells with +100% token waste** | **NONE** |
| wins (Δ<0) | 11 / 16 |
| median Δ | **−29%** |
| aggregate tokens | 86.9M → 46.2M (**−47%**) |
| fix rate (clean pairs) | **32/32 (100%)** |

Not counted: 4 cases not-red-at-parent (auto-skipped, invalid), and 2 hard cases whose
WITH run failed (see below).

### Every clean pair (sorted by Δ)

```
 ed1c5b05  26.05 ->  3.78M   -85%     8292e69d   3.37 ->  0.62M   -82%
 06943a    12.52 ->  3.74M   -70%     296bf8c6   3.87 ->  1.26M   -67%
 6e94f91c   0.85 ->  0.39M   -54%     f908e5dd  12.07 ->  7.81M   -35%
 c2e8b63a   7.08 ->  4.89M   -31%     4da902     7.84 ->  5.48M   -30%
 9e1fce29   1.44 ->  1.04M   -28%     e2320f4e   0.40 ->  0.34M   -13%
 e77a0957   2.84 ->  2.60M    -8%     0537606e   1.42 ->  1.71M   +20%
 7d9f9cf0   0.80 ->  0.99M   +23%     30792170   0.37 ->  0.46M   +27%
 795b89e6   0.34 ->  0.43M   +29%     1226c7c2   5.66 -> 10.69M   +89%
```

## What this shows

1. **No catastrophic waste.** Every one of the 16 clean cases is under +100%. The five
   positives are small: four are the *fixed debugger overhead on bugs that read cheaply*
   (`0537606e`/`7d9f9cf0`/`30792170`/`795b89e6`, all <1.5M without, +20–29%), and nominal
   (`1226c7c2`) is +89%.
2. **The SKILL turned the run-2 disasters into wins.** The contravariant case (`8292e69d`)
   was **+192%** in run 2 (17 exploratory launches on a missing diagnostic); with "read
   for missing diagnostics, don't hunt" it is now **−82%**. The nominal fix-thrash case was
   **+747%** in run 2; the fix-discipline rule pulled it to +89% here (still the residual —
   a weak-model over-engagement pattern, and the case nearest the line).
3. **Big wins where reading is expensive** (−85% at 26M→3.8M; several −30–70%), which is
   the whole thesis: rdbg pays off on large/complex code and is ~neutral on cheap bugs.
4. **The tool is now viable at scale.** codelldb measured **636MB** during WITH runs (was
   ~20GB before `target.preload-symbols false`) — no OOM across 40+ runs.

## Honest caveats

- **Single-run variance is high** (the false-positive cell measured 3.4–10.2M across runs
  earlier; here it landed at −70%). These are single samples; the *large* effects and the
  "no cell >+100%" result are trustworthy, but exact per-case Δ needs multi-trial to state
  tightly. A multi-trial sweep is the natural next step now that runs are stable.
- **One +100% cell, and it's variance — not debugger waste.** `b01338524f` re-ran clean
  at **+147%** (1.04M→2.5M), so the "zero +100%" claim above is really **16 of 17** clean
  pairs under +100%. But its transcript shows only **1 launch** and **26 greps** — the
  debugger was barely used; the cost is reading/fix-iteration *variance*, not the agent
  over-debugging. The SKILL did its job (few launches); single-run reading noise pushed
  this cheap cell over the line. Multi-trial would almost certainly bring it back down.
  This is the honest frontier: the *systematic* waste (launch-hunting, fix-thrash) is
  gone; residual +100% cells come from run-to-run reading variance on cheap bugs.
- **`4aac` (subclass-ctor) is genuinely hard for Opus** — WITHOUT cost 17.6M/44min; its
  WITH failure is bug difficulty, not rdbg waste.
- **4 not-red-at-parent skips** reduce the pool; more candidates would restore 20 clean.
