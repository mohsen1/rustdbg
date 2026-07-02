# What the benchmarks actually showed

Two questions: does rdbg help an agent, and will an agent use it?

## Tier 1 — small planted-bug crates
With rdbg available, agents dip into it and it modestly helps on bugs that need a
runtime value (see `bench.py` results). On bugs you can spot by reading, it's a wash.

## Tier 2 — real fixed bugs in tsz (~1.7M lines)
Every case the mining surfaced was a diagnostic/display bug, and across every run
the agent used rdbg **0 times** — it grepped the emit code and fixed it. The
fingerprint-trace *works* (break `--fn push_diagnostic`, `eval diag.code`, `bt`
walks back to the deciding function), but with a neutral prompt the agent never
reached for it, even when the exact recipe was in the prompt.

## The adoption experiment (the answer)
Same failing test, Claude Code / Opus / medium effort, 10 runs:

| | strong CLAUDE.md | control |
|---|---|---|
| used rdbg | 5/5 (100%) | 0/5 (0%) |
| mean rdbg calls | 7.6 | 0 |
| mean tokens | 386k | 135k |
| mean wall | ~67s | ~24s |
| passed | 5/5 | 5/5 |

**Prompting fully controls adoption (0% → 100%).** The Read/Grep/Run bias is a
default, not a constraint — a forceful CLAUDE.md that mandates the debugger and
discourages the grep loop flips it completely.

**But adoption ≠ benefit.** On this readable bug, forcing rdbg cost ~2.85x tokens
and ~2.8x wall for zero correctness gain. The debugger is overhead when reading
already works.

## Takeaway
Don't mandate rdbg blanket — that's ~3x waste on easy bugs. Trigger it
selectively: when the bug is runtime-opaque or the agent is stuck in a
non-converging read loop. rdbg's value is real but conditional on the bug
actually needing runtime state; a passive "skill available" note yields 0 use, so
adoption needs an opinionated skill/hook, tuned to fire when it will pay off.
