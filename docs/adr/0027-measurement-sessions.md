# ADR 0027: Measurement sessions — one warm engine, many observations

**Status:** Proposed
**Date:** 2026-08-14
**Related:** [ADR-0005](./0005-engine-side-builds.md) (the brokered codegen tools this changes the
shape of), [ADR-0006](./0006-profiling-over-mcp.md) (the profile capture a session absorbs),
[ADR-0008](./0008-domains-as-immutable-composes.md) (the frozen world a session must not escape),
[ADR-0022](./0022-measure-task-dags.md) (the ladder whose rungs become session points),
[ADR-0025](./0025-durable-tool-steps.md) (the value/state boundary this extends).

## Context

Every brokered measure call is one Kueue Job that boots the engine from cold, measures, and dies.
For a small kernel oracle that is the right trade. For a served model it is most of the run.

Measured on the GLM-5.2 NVFP4 revalidation run (`20260812T150558Z`), reading the nanosecond epochs
embedded in the broker's log handles:

| gap | min |
| --- | --- |
| iter1 correctness → latency | 25.8 |
| iter1 latency → mechanism | 24.4 |
| iter2 correctness → latency | 32.2 |
| iter2 latency → mechanism | 24.0 |

Roughly 25 minutes per rung, of which the domain manifest accounts ~6 minutes of page-cache
prefetch and ~11 minutes of shard load plus compile. Under nine minutes of each rung is
measurement. Four GPU jobs per iteration pay that toll four times to measure one digest — the A/B
patched leg is already free because `measure_key(digest, "benchmark", env)` hashes identically to
the latency rung, but correctness, latency, the A/B disabled leg, and mechanism each boot their own
engine.

The workload's own runtime already supports the alternative. At the pinned vLLM SHA the HTTP
frontend exposes `/reset_prefix_cache`, `/reset_mm_cache`, `/reset_encoder_cache`, `/sleep`,
`/wake_up`, `/is_sleeping`, `/start_profile`, and `/stop_profile`, and `rust/src/bench/src/sweep.rs`
already runs N benchmark points against one live server, resetting the prefix cache between each.
The capability is not the blocker. The blocker is that the broker has no model for work that
outlives a single tool call, and no answer to the question that makes such work trustworthy.

## The question this ADR exists to answer

Not "can we keep the engine warm" — obviously we can. The question is **when do two measurements
taken through one process count as two independent observations**, and how does a result carry
enough evidence for a reader to believe it.

Get this wrong and the rig gets substantially faster and quietly less trustworthy, which is the
worst trade available to a system whose only product is numbers people act on.

## Decision

A **measurement session** is a broker-owned engine process that serves an ordered sequence of
measurement points against one candidate digest under one process configuration.

### 1. The independence contract

A domain that wants sessions declares a reset protocol. Two points in a session count as
independent observations only if the protocol ran clean between them.

- **Declared resets.** The manifest names the state cleared between points. The broker executes
  them and records the outcome in the point's result. For vLLM the starting set is prefix cache,
  multimodal cache, encoder cache.
- **Declared residue.** The manifest also names the state that is *not* cleared and is therefore
  accepted drift — allocator fragmentation, autotuner and CUDA-graph caches, device clock and
  thermal history. This list is the honest part of the contract. A domain that cannot enumerate its
  residue has not earned a session.
- **Bounded drift.** Residue that grows without bound requires a cap: a maximum point count per
  session, after which the broker recycles the process. Absent a measured bound, the cap is
  declared, not inferred.
- **Fixed order.** Results from a session are comparable only if measurement order is fixed or
  proven order-insensitive. The manifest fixes the order; the result records the point's ordinal.
  Reordering points is a contract change, not a scheduling detail.
- **A failed reset is not a measurement.** If the protocol errors, the point is `transport` and is
  never recorded as a settled fact. Silence here is how a dirty session poisons a ledger forever.

### 2. Sessions are state; points are values

ADR-0025 draws the line: a durable step records a value, never a claim about mutable external
state, which is why `deploy_candidate` is not a step. A live engine process is exactly such state,
so:

- **The session handle is never a durable step.** It is not resumable, not memoizable, and not
  replayable. A broker restart abandons the session and re-boots.
- **Points remain durable steps**, but their identity grows to include the reset-protocol digest
  and the point's ordinal. Without both, a number produced by a dirty or reordered session replays
  as a settled fact for the life of the ledger — the failure mode ADR-0025's eligibility rule
  exists to prevent, arriving through a door it did not cover.

### 3. What stays a one-shot job

Anything that changes process-level configuration cannot share a session:

- **Any A/B leg driven by an import-time toggle.** `VLLM_GLM_TOGGLE` selects kernels at import;
  the patched and disabled legs are two processes, always. Two sessions per iteration is the floor,
  and stating it here is meant to stop a later reader from trying to optimise it away.
- **Builds**, which produce the digest a session measures.

### 4. Lifecycle

Start job → explicit readiness gate (a probe, never a sleep) → warm-up points whose results are
discarded by declaration → N measured points with resets between → teardown. The session's expected
span must fit `BROKER_CODEGEN_DEADLINE_SECONDS` (today 5400s); the broker refuses to start a session
it cannot finish rather than discovering the deadline mid-ladder.

### 5. Domains that declare nothing keep today's behaviour

No session protocol means one job per call, unchanged. This is not a migration everyone takes.

## Alternatives

**Fat single job** — one `[measure]` command that serves, runs every point against itself, and
emits a multi-metric JSON the gate splits into rungs. Requires no broker change at all. Rejected as
the end state: it loses per-point isolation, retry, and memo granularity, and one boot plus
correctness plus latency plus profile sits uncomfortably close to the 90-minute deadline. **Accepted
as the prototype** — it validates the independence contract on real hardware for the price of a
manifest edit, and that contract is the part worth being sure about before it is built into the
broker.

**A standing server outside the run.** Rejected outright: ADR-0008's frozen world requires the
measured engine to be the candidate's own build. A shared rig measures something nobody committed.

## Consequences

- **Comparability breaks at the switch.** TPOT from a served benchmark is not TPOT from
  `vllm bench throughput --async-engine`. The gate scores against each run's own preflight baseline,
  so runs stay internally valid, but no session-era number may be compared against a pre-session
  record. A re-baseline against the same frozen seed is part of adopting this, not a follow-up.
- **The mechanism rung stops being a job.** `/start_profile` and `/stop_profile` make it a point
  inside the session, which also moves the trace from `nsys-rep` to a torch-profiler trace. That
  trade buys operator attribution, which is what the mechanism rung was always actually asking for.
- **Expected saving**, on the shape measured above: two boots per iteration instead of four, around
  fifty minutes per iteration, with the mechanism collapse on top.
- **A new way to be wrong.** Before this, a bad number came from a bad candidate or a bad rig. After
  this, it can come from a dirty session. The reset record in each point's result is what makes that
  diagnosable rather than mysterious.
