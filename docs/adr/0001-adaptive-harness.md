# ADR 0001: Adaptive harness — adapt the setup, freeze the judge

**Status:** Accepted; implemented (the setup/solution tool split, fingerprinted segments, and the `escalate` hatch are live in the engine)
**Date:** 2026-06-23
**Context owner:** agentic-epp-autoresearch

## Context

The loop's integrity rests on one thing: the gate (fitness) must be **trustworthy and
outside the agent's reach**. A kept "win" is only as honest as the harness that judged it.

We've found it valuable to *tailor the harness to each issue* — e.g. for a heterogeneous
routing workload we hand-shaped the gate, chose a baseline, and picked the perf gate.
Automating that tailoring ("an adaptive harness") is attractive: the
harness would meet each issue where it is. But adaptivity aimed at the wrong layer turns the
loop into a **reward-hacking machine** — if the harness can reshape itself toward the thing
being optimized, the agent (or the adaptation itself) will make the workload easy, move the
baseline, or pick a metric it already passes.

## Decision

**Adapt the setup. Freeze the judge. The agent may change its solution; it must never change
its evaluation.** Concretely, split the system into two phases with a hard wall between them:

### 1. Scoping (adaptive, once, then frozen)

Analyze the issue and decide the **workload, baseline, and gate**. This is the part worth
automating (an agent may *propose* the harness config), but
its output is:

- **pinned** — written to an immutable harness manifest,
- **content-hashed** — a fingerprint of (workload + gate + baseline),
- **validated** — passes the gate self-test (below) before it's trusted,
- **human-approved** — a person signs off before the loop spends budget.

### 2. The loop (immutable gate)

The optimizing agent runs against the frozen harness. It gets **only solution-space tools**
and a **read-only** gate. It cannot touch the workload, the baseline, or the metric. The
harness manifest is mounted read-only, outside the agent's editable workspace.

## Tool classification (capability boundary)

Every tool is one of two kinds. The optimizing agent's toolbox contains **only the second
column**. The first is used during scoping and then frozen.

| Evaluation / setup (frozen, NOT in the loop agent's toolbox) | Solution (agent may use) |
| --- | --- |
| `rig-config` (workload/`MOCK_*`/replicas/model) | `apply` (EPP plugin/scorer config) |
| `order-capture` (latency calibration, GPU) | `build-image` + `deploy` (candidate EPP) |
| `BENCH_*` workload shape, `--long-frac`, seeds | `gotest` (verification, read-only) |
| the baseline config + its measured score | `bench` / `inspect-rig` / `epp-metrics` **read-only** |

The loop agent's toolbox must exclude `rig-config` and `order-capture`, since they can change
`MOCK_*` (the latency model) and the workload — i.e. the evaluation surface. They are exposed
only in scoping, never in the loop driver.

## Safeguards (make adaptivity safe rather than scary)

1. **Gate self-test (negative controls).** Before a gate is trusted, prove it
   *discriminates*: a known-bad config must score worse and a known-good better (e.g.
   prefix-affinity p99 292ms vs prefix-blind 506ms). Automate it as a precondition —
   a gate that can't tell good from bad is the real danger, not the agent.
2. **Harness fingerprint on every result.** Stamp each kept result with the manifest hash so
   wins are reproducible and scope drift is detectable at a glance.
3. **Fixed baseline.** Derive the baseline once during scoping; never re-derive it inside the
   loop (a moving baseline makes "improvement" meaningless).
4. **Multi-seed gating.** Average a few seeds so a lucky single run isn't kept (esp. for
   noisy dynamics like load-scorer herding).
5. **Cost/blast-radius gating.** Anything in the setup column that spends GPU or mutates
   shared infra (e.g. `order-capture`) keeps an explicit human-gated `--execute`, as today.

## Escape hatch: the agent may declare the judge inadequate (not change it)

Freezing the judge only stays honest if the agent has a sanctioned way to say *"this judge
cannot evaluate what I need."* Without it, a blocked agent either thrashes out discarded
changes or tries to game the gate. So the complement to "freeze the judge" is a structured
**escalation**:

- The agent calls `escalate --category {harness-limitation|infeasible|needs-info} --reason …
  --evidence …` (writes `ESCALATION.json`). The loop detects it, **stops, restores the rig,
  and surfaces the report for human review.**
- This is distinct from a discard ("this change didn't help") — escalation means *no* change
  here can be fairly evaluated, so the harness itself needs a human fix.
- Anti-abuse: the tool requires a substantive reason and asks for evidence (e.g. `inspect-rig`
  output across configs); the escalation goes to a human, it is not a silent success. It is
  the agent's only sanctioned move against the evaluation — it can flag it, never edit it.

This is the path when, for example, `tokenload` reads zero in-flight load everywhere (the
producer signal is dead, so the gate cannot see length-aware routing): rather than discard
forever, the agent escalates "the gate can't differentiate length-aware routing" with the
`inspect-rig` evidence, and a human fixes the producer wiring or the workload.

## Consequences

- **Positive:** we keep the appealing part — the harness meets each issue where it is — while
  the optimization loop stays trustworthy and its results stay comparable and reproducible.
- **Positive:** clear capability boundary; the loop agent literally cannot reward-hack its
  evaluation because it has no tool to reach it.
- **Negative / cost:** scoping becomes a real, gated step (validation + approval), not a
  side-effect of running the loop. That's deliberate — a wrong harness is wasted or
  misleading research, so it deserves a checkpoint.
- The loop's tool wiring splits the toolbox into setup vs solution, emits a harness manifest +
  fingerprint, and runs the gate self-test as a scoping precondition. None of this requires an
  adaptive *loop* — only an adaptive *scoping* step.

## Principle, restated

In ML you adapt the experimental setup once, carefully, and then you never let the model edit
the test set. Same here: automate the setup, never the judge.
