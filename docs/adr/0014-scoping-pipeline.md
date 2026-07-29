# ADR 0014: Scoping as a governed pipeline — `crucible scope <issue>`

**Status:** Proposed; S0+S1 implemented (the `crucible scope` pipeline skeleton and the
`[judge.selftest]` primitive are live; S2, the propose turn, is in progress)
**Date:** 2026-07-02
**Related:** [ADR-0001](./0001-adaptive-harness.md) (the scoping phase this finally implements — "an
agent may *propose* the harness config"), [ADR-0007](./0007-isolation-preflight.md) (the preflight,
here a pipeline stage), [ADR-0008](./0008-domains-as-immutable-composes.md) (the freeze the pipeline
ends in), [ADR-0010](./0010-candidate-portfolios-and-search.md) (the wide leg that makes a freshly
scoped issue cheap to attack from several angles), the judge-tier issue ranking (onboarding triage),
`crucible check` + `crucible init` (the BYO on-ramp), `crucible::identity` (the freeze fingerprint).

## Context

The engine side is now rich — composites, mediated builds/deploys, profiling, portfolio search,
identity stamping — but every run still starts from a **hand-authored harness**: a human writes the
manifest, the gate, the world hooks, picks the workload and the baseline. That is roughly a day of
expert work per issue, and it has become the throughput bottleneck: the loop can explore five
approaches in parallel, and it still takes us a day to give it something to explore.

It is also the most *dangerous* manual step, not just the slowest. The two best incident stories in
this repo are both harness failures: #1109 burned a loop on a gate that could not isolate its target
(ADR-0007 exists because of it), and #1489's reward hack lived in a gate that under-counted the
PreRequest path. ADR-0001 designed the answer — scoping is **adaptive, once, then frozen**, with the
proposal automated and the output validated and human-approved — but only the freeze was ever built.
This ADR builds the rest.

## Decision

Introduce **`crucible scope <issue-url>`**: a five-stage pipeline that turns an upstream issue into a
validated, human-approved, frozen domain pack. Every stage exists to kill a bad harness as early and
as cheaply as possible.

1. **Ingest + triage.** `goal-from-issue` extracts the goal; the judge-tier ranking
   (T0/T1/T2/N) decides whether the issue is scopeable at all and biases the proposal (a T0 bug wants
   a test gate; a T1 perf issue wants a measured workload). An N-tier issue exits here with the reason.
2. **Propose.** One agent turn drafts the harness: the manifest, the gate script, the workload shape,
   the baseline choice, and the **negative controls** (a known-bad and a known-good configuration the
   gate must tell apart). This is ADR-0001's sanctioned adaptivity, in its sandbox: the proposer gets
   solution-space tools plus the issue, nothing else.
3. **Validate mechanically.** Three checks, all existing machinery, all unbypassable:
   - `crucible check` — the contract lint (manifest parses, files resolve, the measure contract emits
     `{valid, score}`, the gate is not agent-editable without a frozen inject);
   - the **gate self-test** (ADR-0001 safeguard 1) — the proposed known-bad must score strictly worse
     than the known-good, else the gate cannot discriminate and the proposal dies;
   - the **isolation preflight** (ADR-0007) — the target's own contribution to the gated metric must
     be material, measured from the target's instrumentation, else the issue is mis-scoped and the
     pipeline emits that finding instead of a harness.
   A failure at this stage kills the proposal *before any human reads it*.
4. **Approve.** The surviving domain pack opens as a **draft PR** (the ADR-0002 approval muscle): the
   manifest, gate, controls, and the validation evidence (self-test scores, preflight attribution) in
   one reviewable diff. A human signs off before the loop spends budget — ADR-0001's checkpoint,
   unchanged.
5. **Freeze.** On approval the manifest is pinned, the pack's `RunIdentity` digest is computed and
   recorded as its fingerprint, and the issue is runnable: `crucible run` (or a wide round, ADR-0010)
   against a world whose provenance is one content-addressed line.

## Why an agent proposing the judge stays honest

This is the reward-hacking surface ADR-0001 fenced, so the trust story must be explicit:

- **Proposer ≠ optimizer.** The scoping turn and the optimizing loop are different runs with no shared
  context; the scoping agent never runs against — and never sees the results of — its own gate.
- **Validation is mechanical.** The self-test and preflight are computed, not judged; a gate that
  cannot discriminate, or a target that is a sub-percent sliver of the metric, fails arithmetic, not
  taste.
- **A human approves the evaluation.** Nothing the proposer emits becomes a judge until a person has
  read the gate and its evidence — the same rule as every other judge change in the system.
- **The freeze is stamped.** The approved pack's identity digest makes post-approval drift detectable
  (ADR-0008); the gate that runs is provably the gate that was approved.

## What this is NOT

- **Not an adaptive loop.** The ADR-0001 wall stands: scoping adapts once, then freezes. The pipeline
  never runs concurrently with an optimizing loop on the same domain.
- **Not auto-run.** Stage 4 is mandatory. A wrong harness is wasted or misleading research; it keeps
  its human checkpoint.
- **Not a replacement for `author-m2-rig`.** v1 targets test-gate and local/GitWorld domains (the
  #1474/#1489 shape, and the BYO on-ramp). Composite GPU rigs need rig topology judgment that stays
  with a human until the ADR-0013 rig backends make "stand up the rig for the preflight" cheap.

## Consequences

- **Positive:** onboarding drops from a day of expert work to review-of-a-draft-PR; the two known
  harness failure modes (#1109 mis-scoping, #1489 gate blind spots) are caught by construction —
  stage 3 runs exactly the checks that would have caught each; every prior wave's machinery becomes
  load-bearing (check, identity, publish, the wide leg).
- **Negative / cost:** the gate self-test becomes a first-class engine contract (the manifest needs a
  place to declare the controls — likely `[judge.selftest]` with `good_cmd`/`bad_cmd`), which is new
  surface; scope turns cost real tokens on issues that may die in stage 3 (bounded by a per-scope
  budget cap); the preflight needs per-target attribution metrics, which not every issue has (absent
  instrumentation, stage 3 degrades to self-test + check and says so in the PR).

## Workstreams

| # | Workstream | Builds on |
| --- | --- | --- |
| S0 | `crucible scope` skeleton: pipeline runner chaining ingest → check → freeze on a hand-written pack (no propose turn yet) | `goal-from-issue`, `check`, `identity` |
| S1 | Gate self-test as an engine primitive (`[judge.selftest]`, negative controls, discrimination assert) | ADR-0001 safeguard 1 |
| S2 | The propose turn (method prompt + solution-space toolbox for harness drafting) | ADR-0001 scoping phase |
| S3 | Isolation preflight as a stage (attribution read from target instrumentation) | ADR-0007 |
| S4 | Draft-PR approval + freeze-on-approve (identity stamp, frozen pack published) | ADR-0002/0008, publish |

**Slicing:** S0 + S1 first (they harden *hand-written* harnesses immediately, before any agent
proposes one); S2 once the validation net is real; S3/S4 to close the loop.
