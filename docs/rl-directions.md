# RL / training directions from the rdbg debugging study

Where our findings sit in the literature, and the ideas they open. (Full search +
synthesis in `benchmarks/adopt/rl-literature.json`.) Citations marked ⚠ are
2026-dated arXiv IDs from an automated search and should be verified before use.

## How our findings land
- **Adoption (0% neutral / ~100% forceful): corroborated, not novel.** Debug2Fix
  (Microsoft, 2026 ⚠) independently found ~9% adoption on direct debug-tool exposure
  (degrading Sonnet −14.8%) and ~99% when forced — a near-exact match. debug-gym
  (MSR, arXiv 2503.21557) blames low uptake on scarcity of debugging traces in
  pretraining and names "RL/SFT an info-seeking model on debugging traces" an *open
  problem*.
- **Outcome (reading matches the debugger at ~half the tokens): a sharp counterpoint.**
  The optimistic headlines (Debug2Fix +13–22%) are vs static/print baselines with
  forced debug-first. Ours is the isolating comparison they skip — debugger vs plain
  reading at matched difficulty and tokens. R2E-Gym (arXiv 2504.07164) independently
  shows execution-free reading rivals execution on outcome.
- **Confabulation measurement: appears novel.** ChatDBG argues grounding "reduces
  speculative reasoning" but never measures it; CodeHalu/CRUXEval score static
  generations / value-prediction. No one measured the rate an *agent* asserts
  unobserved runtime facts mid-trajectory — and our nuance (it barely drops with
  debugger access, 2.0→1.8) is a novel negative.
- **The unclaimed gap:** (a) trajectories curated on *grounding density* and stripped
  of tool-friction as SFT/PRM data (all pipelines filter on outcome); (b) an
  *observe-before-assert* reward for an agent (distinct from rewarding correct value
  *prediction* — a lucky-right value is still ungrounded); (c) debugger-in-the-loop RL
  with *weight updates* (debug-gym proposes it; InspectCoder is inference-time only;
  LeDex is self-debug, no runtime debugger).

## Ideas (ranked; most are testable offline on our archived runs)
1. **Confabulation-rate eval** — grade every runtime-factual claim as observed /
   confabulated-correct / confabulated-wrong vs an rdbg oracle; abstention-aware
   (TruthRL). pass@k is blind to it; underpins everything below. *Low effort, no GPUs.*
2. **Observe-before-assert grounding reward** — RL term rewarding claims that were
   tool-observed, penalizing confabulated I/O; reward the *act* of grounding, not a
   lucky-correct guess. The debug-gym open problem. Ports Stop-Rewarding-Hallucinated-
   Steps / TruthRL / Proof-of-Use into interactive debugging.
3. **Friction-filtered grounding-distilled SFT** — rewrite traces to (observation →
   inference → edit), drop the ~13× tool-friction, SFT on those. No prior pipeline
   curates on grounding + strips friction.
4. **Reward disambiguating, not redundant, observations** — info-gain over the agent's
   own hypothesis distribution; attacks the "post-hoc verification in 80%" pathology
   and guards idea 2 against farming trivial groundings.
5. **Learned "when to debug" gating** — predict per-bug whether runtime grounding is
   decisive; pay the debugger cost only on that ~20% subset. Converts net-negative to
   net-positive; the selective policy the field's all-or-nothing work lacks.
6. **Grounding-aware trajectory verifier** for best-of-n — featurize grounding density
   + confab flags; capture the debugger's richness as deploy-cheap verifier signal.
