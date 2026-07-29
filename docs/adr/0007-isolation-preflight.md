# ADR 0007: Verify the gate isolates the target before spending a loop — the metric that misframed #1109

**Status:** Accepted
**Date:** 2026-06-26
**Related:** [ADR-0001](./0001-adaptive-harness.md) (automate the setup, freeze the judge — world-scoping is setup),
[ADR-0006](./0006-profiling-over-mcp.md) (the Profiler — found the EPP CPU is incidental), the
onboarding issue-ranking pipeline (ranks issues by *measurability*), the #1109 TTFT domain.

## Context

#1109 ("long-context requests see >1s TTFT overhead in the EPP request path") ran as a loop and
produced two valid-but-flat candidates (collector caching, body re-chunking; both within c=150
noise). ADR-0006's profiler showed the EPP is ~24% of one core busy with GC incidental. We then ran
the cheap, decisive check: the EPP's **own** instrumentation, delta'd around a 600-request measure.

| EPP metric (per request, fresh 600-req measure) | value | share of TTFT |
| --- | --- | --- |
| `llm_d_router_epp_scheduler_e2e_duration_seconds` (the routing decision) | **0.018 ms** | ~0.0008% |
| `inference_extension_plugin_duration_seconds` (all ~16 plugins summed) | **~0.03 ms** | ~0.001% |
| `llm_d_router_epp_request_duration_seconds` (handler attached for the whole request) | **2262.8 ms** | ≈100% |

The EPP's actual *work* is **~20–30 microseconds per request**. The 2.26 s is the ext_proc handler
sitting **blocked**, waiting on Envoy's 220 KB body buffering + the data-plane transfer + the mock
backend + c=150 queueing — none of which the EPP controls.

**How #1109 got misframed.** `llm_d_router_epp_request_duration_seconds` ≈ 2.26 s ≈ the end-to-end
TTFT, because that metric measures the handler's wall-clock *attachment* time (mostly blocked-wait),
not EPP compute. Reading it as "the EPP request path has >1s overhead" is the trap. The compute metric
(`scheduler_e2e` = 17 µs) tells the truth: a 17 µs decision wrapped in 2.26 s of waiting on everything
else. There is no >1s EPP overhead to remove.

**Root cause is world-scoping (ADR-0001's setup column), not the EPP.** The 220 KB-body mock scoping
was chosen to *stress* the EPP path, but it drowns the EPP in Envoy transfer cost — the scoping
defeats the isolation it was meant to create. The gate is end-to-end and cannot attribute a result to
the EPP, so every EPP-side candidate ties in the noise by construction.

## Decision

**Onboarding must verify the gate actually isolates the target — and that the target's contribution is
material — BEFORE spending a loop.** Measurability (can a frozen judge decide success?) is necessary
but not sufficient; the issue-ranking pipeline ranks for it but #1109 passed measurability while being
unwinnable. Add an **isolation pre-flight** to the judge-validation gate:

1. **Attribute the gate.** Before the first scored loop iteration, measure the *target's own*
   contribution to the gated metric using the target's instrumentation (here: the EPP's
   `scheduler_e2e` / `plugin_duration` vs the end-to-end `request_duration`), or a direct
   isolated harness. If the target is a sub-percent sliver of the gated number, the loop cannot win —
   **escalate the issue as mis-scoped, don't run it.**
2. **Distinguish work from wait.** A `*_request_duration` that tracks the end-to-end number is a
   handler-attachment time, not compute. Prefer a compute/decision metric (or a profile, ADR-0006)
   for attribution; never infer "the component is slow" from a duration that includes downstream wait.
3. **Re-scope or escalate.** A real EPP-TTFT experiment needs a world where the EPP's contribution is
   material: an EPP-heavy workload (large prefix trees, many pods, expensive scoring) or smaller
   bodies so transfer isn't the whole budget. Absent that, the correct output is an escalation.

#1109 is therefore **escalated** (no EPP-side win exists as gated), and the planned from-scratch
ext_proc isolation harness is **shelved for #1109** — the pre-flight already answered what it would
measure (~30 µs). The harness stays a valid general instrument for an issue where the EPP request path
genuinely is the cost.

## Consequences

- **Positive:** a ~10-minute instrumented pre-flight kills an unwinnable run before it spends a paid
  loop; the autoresearch stack surfaces a mis-scoped problem instead of grinding (the system working);
  the check is generic (any target with self-instrumentation) and slots into the judge-validation gate
  the controller needs; it converts "the loop keeps tying in noise" from a mystery into a one-number
  verdict.
- **Negative / cost:** another gate in onboarding (the pipeline must learn to read a target's
  attribution metric, or stand up the isolated harness, before greenlighting); a per-target choice of
  "which metric attributes the gate," which isn't always obvious.
- **Carries forward:** this is the empirical case for the world-scoping discipline — the judge can be
  perfectly frozen and still measure the wrong thing if the world doesn't isolate the target.
