# ADR 0009: Composite domains — combined multi-component autoresearch

**Status:** Accepted; implemented (composite manifests, the multi-workspace world, and the combined gate are live)
**Date:** 2026-06-27
**Related:** [ADR-0008](./0008-domains-as-immutable-composes.md) (each component is an immutable compose;
a composite is a *tuple* of composes plus the assembled rig), [ADR-0006](./0006-profiling-over-mcp.md)
(the multi-backend broker is what lets one run profile two runtimes — pprof for EPP, torch-trace for
vLLM), [ADR-0001](./0001-adaptive-harness.md) (freeze the judge AND the world — here extended to a
*cross-component* workload), [ADR-0002](./0002-mediated-provisioning-mcp.md) (the combined rig is heavier
to provision; the broker still mediates).

## Context

Every domain so far is single-component: one upstream repo, one judge, one world (EPP config, or vLLM
throughput). That shape can't express the features that actually matter most in llm-d, the **cross-cut**
ones, where a behavior is split across the router (EPP) and the engine (vLLM) and **neither side's gate
can see the payoff**. Two real, in-flight examples:

**1. LoRA-aware routing.** vLLM only emits `vllm:lora_requests_info`, which shows an adapter *only while
it has requests in flight* — an idle-but-loaded adapter vanishes from metrics, so routers (llm-d,
gateway-api-inference-extension) guess residency from request recency and can't tell a **warm** adapter
(in a GPU slot, ~0 load cost) from a **cold** one (needs a load, a TTFT spike, worst at low concurrency).
The vLLM half is prototyped as the upstream RFC
[vllm#45411](https://github.com/vllm-project/vllm/pull/45411) — adding a `LoRALoadEvent` engine
notification and `vllm:lora_adapter_loaded{adapter_name,level="gpu"|"cpu",pinned}` +
`vllm:num_{gpu,cpu}_loaded_lora_adapters` gauges, so a replica advertises its residency tiers before any
traffic. The EPP half (tracked under llm-d-router #1500/#709/#926) extends the existing `loraaffinity`
**scorer** + `loraspec` metric extractor to prefer GPU-slot > CPU-cache > cold endpoints. The win,
avoided cold-load TTFT, is **only measurable end-to-end**: across the EPP→vLLM path on a workload with
mixed adapter residency. vLLM's gate can't see routing; EPP's mock-rig gate can't see real adapter-load cost.

**2. P/D rollout protection.** In prefill/decode disaggregation the NIXL KV connector transfers KV from a
prefill engine to a decode engine, which is only valid if the two agree on their KV-transfer config
(`kv_cache_layout`, `tp_size`, `block_size`, dtype, model, NIXL backend/version — the
`NixlConnectorWorker`/handshake inputs). During a rolling update some endpoints carry the new config and
some the old; if the EPP pairs a prefill from one generation with a decode from another, the transfer
fails or corrupts. The EPP **has no way to block that pairing** today, because vLLM exposes no **P/D
config hash** for a routing **filter** to compare. The combined feature: vLLM exposes a fingerprint of
the KV-transfer-relevant config; the EPP adds a *filter* plugin that excludes any P/D pairing whose hashes
disagree (rollout protection). Its fitness, zero cross-incompatible transfers while preserving throughput
during a rollout, again exists only in the live EPP→vLLM-P/D system under a rollout event.

Both share a shape the engine can't currently host: **vLLM exposes a signal, the EPP routes/filters on
it, and the fitness is a property of the assembled system, not of either repo.** One is a soft *scorer*,
one is a hard *filter*; both need two repos edited together and measured as one. A single-component
manifest (`[repo]` is exactly one url|path, one `[judge]`, one world) cannot represent this.

## Decision

**Introduce composite domains: a domain that composes N component domains into one autoresearch run.** A
composite is config, not new per-feature engine code — it reuses the component domains (each an ADR-0008
immutable compose) and adds three things the engine must learn to do generically:

1. **A multi-workspace world** — the agent may edit *any* component's checkout in a turn; keep/discard and
   git memory span all components (a kept iteration is a *tuple* of overlay commits, one per component).
2. **A single combined gate** — one `measure_cmd` that stands up the *assembled* system (EPP routing to
   vLLM backends) and runs the frozen cross-cut workload, emitting the usual contract.
3. **The union of component surfaces** — both components' PATH tools, skills, and profiler backends are
   available in the one run (the agent profiles vLLM with torch-trace *and* the EPP with pprof — ADR-0006).

The engine stays **component-agnostic**: "combined" is a composite manifest plus a combined gate command,
not a `lora-routing` or `pd-rollout` special case baked into Rust. New cross-cut features are new
composite manifests + new gate wrappers, the same way new single domains are new manifests today.

```toml
# a composite domain's crucible.toml (this one assembled the vllm + epp packs)
[composite]
name = "lora-routing"

[[component]]
domain = "vllm"            # reuses the vllm domain pack (its compose, tools, profiler)
[[component]]
domain = "epp"             # reuses the epp domain pack

[judge]
# Drives the ASSEMBLED system: EPP in front of vLLM replicas with a mixed-adapter workload, scores
# end-to-end serving (e.g. TTFT for requests whose adapter is cold). Frozen workload = the adapter mix +
# concurrency + (for P/D) the rollout schedule (ADR-0001 extended to a cross-component evaluation).
measure_cmd = "lora-routing-bench --json"
direction   = "higher"

[world]
# Composite reversibility: each component's git overlay PLUS the live assembled rig (snapshot/restore
# the deployed EPP config + the vLLM replica set). One opaque token bundles both (ADR-0008 §overlay).
snapshot_cmd = "lora-rig-snapshot"
restore_cmd  = "lora-rig-restore"
```

## Implementation sketch (a follow-up, not this ADR)

- **Manifest:** a `[composite]` block with `[[component]]` entries referencing existing domain dirs (each
  contributing its `[repo]`/workspace, tools, skills, and profiler backend). A composite has no `[repo]`
  of its own; `[judge]`/`[world]` are the *combined* gate and reversibility. Single-component manifests
  are unchanged (a degenerate composite of one).
- **Engine:** generalize the World + run identity from one workspace to a **vector** of workspaces — the
  one real engine change. `Run`/`Segment`/`Snapshot` (ADR-0004) already model an iteration; extend the
  snapshot to a tuple of per-component overlay commits + the world token.
- **Gate:** the combined `measure_cmd` is a domain-owned wrapper (like `vllm-throughput`) that assembles
  the rig and runs the cross-cut workload. The engine treats it as the same opaque contract.
- **Compose:** each component is composed independently (ADR-0008); the composite pins a *tuple* of
  component digests. A re-compose of one component is a memory-epoch boundary for the composite.

## Design details

### Run identity is a tuple (cross-run memory still works)

ADR-0008 made a single run's identity `(compose input digest, overlay commit)`. A composite extends this
to a **vector**: `({(digest_i, overlay_i)} for each component, cross-cut workload hash)`. Two composite
runs are comparable iff every component's compose digest matches and the frozen cross-cut workload matches
— so cross-run memory ("this EPP-overlay + this vLLM-overlay on these bases scored X") stays meaningful,
the same guarantee ADR-0008 gives per component, lifted to the tuple. The agent's keep/discard commits
each component's overlay independently; a kept iteration is the tuple of commits that scored.

### Why a composite, not a merged repo

We do **not** vendor EPP into vLLM or vice versa. Each stays its own upstream `repo@SHA` with its own
toolchain (Go vs CUDA/Python), its own immutable compose, and its own profiler — because the kept patch
must upstream to *its* repo as a normal PR (ADR-0008's "the loop optimizes a frozen world; the PR rebases
it" applies per component). The composite is an *assembly* of frozen worlds, not a third repo. This is
also why the engine must stay component-agnostic: the unit of reuse is the existing domain pack.

### The combined gate is the only place the components meet

> **Amendment (2026-07-01).** ADR-0013 (proposed) moves *benchmark execution* into a pluggable rig
> backend, which produces a metrics bundle. Scoring stays here: the engine runs the domain's
> `measure_cmd` (now over that bundle), so `measure_cmd` remains the single engine↔domain contract.
> The backend runs the workload; it never computes the score.

Everything component-specific stays in its component (tools name-prefixed — `vllm-profile` vs the EPP's
`profile`; skills per-domain; profiler backends per-runtime). The **single integration contract** is the
combined `measure_cmd`: it is the one artifact that knows both components exist, because it assembles them
and runs the cross-cut workload. Keeping the meeting point to one command is what keeps the engine generic
— mirrors the single-domain rule that `measure_cmd` is the only engine↔domain contract.

### Merging surfaces without collisions (a real gotcha)

"Union of component surfaces" is not a free `cp`. Three things need care at assembly time:

- **Tools are safe; skills are not.** Component tools are already name-prefixed (`vllm-profile`,
  `vllm-throughput` vs the EPP's `profile`/`bench`), so PATH merges cleanly. But **skills are not
  prefixed**: two components `router` and `engine` both ship `skills/profile`, literally named
  `profile`, so merging both into one `.claude/skills` collides. The composite must **namespace skills by
  component** on assembly (e.g. `router-profile` / `engine-profile`), rather than each domain pre-prefixing
  (which would uglify the single-domain case). Namespacing is a merge-time concern, owned by the composite.
- **The method prompt is composite-level.** A component's `method_prompt` describes optimizing *that*
  component. A composite needs its own prompt that frames the cross-cut goal and *both* surfaces (when to
  edit the EPP scorer vs the vLLM signal it consumes). Components contribute their toolbox/skills; the
  composite owns the standing method.
- **Frozen-judge injects stay per-component** (`[[workspace.inject]]`, ADR-0006) — each lands in its own
  component workspace; the combined gate is the cross-cut judge on top.

### Where the combined gate runs (locus)

For a GPU composite the gate can't run in the agent's sandbox (no cluster reach, and measuring through a
port-forward confounds the result — the EPP lesson, ADR-0006). It runs **engine-side**, the same locus as
the EPP `measure`/`build` broker tools (ADR-0005/0006): the loop pod (or a broker-dispatched job) stands
up the assembled rig and runs the cross-cut workload in-cluster. The composite therefore *requires* the
broker on, with both runtimes' backends — which is why ADR-0006's multi-backend broker is load-bearing here.

### What the two drivers exercise (and why both)

- **LoRA-aware routing** is a *scorer* (soft preference) — the gate rewards routing warm adapters; a bad
  policy costs TTFT but never breaks correctness. The signal is vLLM residency gauges; the agent edits the
  EPP scorer (and possibly the vLLM eviction policy), profiles both, and the gate scores end-to-end TTFT.
- **P/D rollout protection** is a *filter* (hard constraint) — the gate must show **zero** cross-incompatible
  KV transfers under a rollout while preserving throughput; too strict strands capacity, too loose
  corrupts. The signal is a vLLM P/D config hash; the agent edits the EPP filter and the vLLM hash it
  exposes. The frozen workload includes a **rollout schedule** (the cross-cut analogue of a request mix).

Carrying both in the ADR keeps the design honest: the engine change (multi-workspace world + tuple
identity + combined gate) must serve a soft scorer *and* a hard filter, edits on the EPP side *and* the
vLLM side, without either feature leaking into engine code.

### Provisioning + cost (ADR-0002)

The combined rig is the heaviest yet: real vLLM replicas (GPU, and for P/D a prefill+decode pair with
NIXL) behind a live EPP, plus a rollout/adapter-mix harness. The broker (ADR-0002) still mediates the
expensive grants; the composite just needs *two* runtimes' worth of capacity and the multi-backend
profiler from ADR-0006. This cost is the reason composites are opt-in, not the default shape.

## Consequences

- **Positive:** the cross-cut feature class becomes optimizable at all — LoRA-aware routing and P/D
  rollout protection get a real fitness instead of hand-tuning; the win is measured where it actually
  exists (end-to-end), closing the neither-domain-can-see-it gap; the engine gains one general capability
  (multi-workspace runs) rather than per-feature code; component packs are reused verbatim, so a composite
  is mostly a manifest + a combined gate.
- **Negative / cost:** the heaviest rig to stand up and keep reversible (GPU P/D + EPP + a rollout/mix
  harness); multi-workspace git memory + tuple run-identity is new engine work (the one non-trivial change
  here); the combined gate is bespoke per cross-cut feature (authoring, not engine); a re-compose of any
  component retires the composite's cross-run memory epoch. Composites are opt-in for exactly these costs.
- **Forward pointer:** validates the component-agnostic discipline the recent work already follows —
  generic `crucible-broker` with injected per-domain backends, name-prefixed tools, per-domain skills,
  `measure_cmd` as the sole contract. Hold that line and a composite needs no engine special-casing beyond
  the multi-workspace world.
