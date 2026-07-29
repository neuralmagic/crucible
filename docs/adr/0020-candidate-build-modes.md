# ADR 0020: Candidate build modes — how a proposal becomes a measured artifact

**Status:** Proposed
**Date:** 2026-07-09
**Related:** [ADR-0001](./0001-adaptive-harness.md) (the frozen judge, which the build recipe is part of),
[ADR-0005](./0005-engine-side-builds.md) (engine-side candidate builds, brokered),
[ADR-0008](./0008-domains-as-immutable-composes.md) (the pinned base a candidate derives onto),
[ADR-0009](./0009-composite-domains.md) (a composite mixes modes per component),
[ADR-0018](./0018-declarative-image-builds.md) (the `BuildBackend` contract — for *infrastructure*
images; this ADR is the tier it deliberately left alone),
[ADR-0019](./0019-openshell-kubernetes-driver.md) (whose P5 de-privileging depends on this).

## Context

Between "the agent edited a file" and "the judge read a score" sits a step nobody has written down: the
edit has to become the thing that is actually measured. How that happens differs per component, is
currently expressed as bash inside per-domain apply hooks, and is the single biggest driver of
per-iteration wall-clock. It is load-bearing and undocumented.

Three modes exist today.

**1. No artifact.** The gate compiles and runs in place, engine-side, on the loop pod. The bug-fix
domains do this: `crucible.test.toml` → `measure-test`, `crucible.1489.toml` → `measure-1489`, both
`go test` against the agent's workspace. `examples/counter` doesn't even compile. There is no image, no
registry, no deploy. Cost: seconds.

**2. No rebuild.** The gate measures a live rig whose *configuration* the agent changed. The EPP perf
domain (`crucible.toml` → `bench`) is config tuning: scorer weights are applied onto the running EPP.
ADR-0005 records this as the reason the perf domain "dodged" the build problem entirely. Cost: a rollout.

**3. Build and roll.** The candidate must become a running image before `measure_cmd` executes. This
splits by **language, not by cleverness**, and a composite runs both halves at once
(the composite's apply hook, `fullstack-lora-apply.sh`):

- **`derive-layer` (interpreted).** vLLM's changed `.py` files are appended onto a pinned base as a real
  OCI layer via `forge-derive-layer` (`forge/src/oci.rs`) — no buildah, no compile, no runtime configmap
  overlay. It takes ~8 seconds **only because the base is a same-repo mirror tag**, so the registry
  mounts the base blobs server-side. `oci.rs:10` is explicit: "server-side MOUNT when registries match
  (instant), else stream pull→push". Point the base at the upstream Docker Hub tag and 8 seconds becomes
  20+ minutes, silently.
- **`image` (compiled).** EPP is Go, so it cannot skip the compile. `forge build-candidate` runs a real
  `buildah bud` against `Dockerfile.epp`. The compile lives inside the image build, inside `apply_cmd`,
  inside the gate's critical path, on every candidate.

Two existing properties of mode 3 are assets and must survive any redesign:

- **A compile error is free.** `build-candidate` exits nonzero on a compile error
  (`BuildOutcome::CompileError`, `forge/src/lib.rs:79`); the apply hook maps that to exit 3
  (`fullstack-lora-apply.sh`); the broker returns `CompileError { log }` and the agent retries **without
  spending a candidate**. The compile is not only a tax, it is the fastest feedback signal in the loop. A build mode
  that cannot distinguish "your code does not compile" from "your candidate scored badly" is a
  regression, not a refactor.
- **An unchanged component does not rebuild.** `build-candidate --skip-if-exists` keys the tag on a diff
  hash and does a manifest `HEAD` against the registry; a hit reuses the digest-pinned ref with no build
  (`forge/src/bin/build-candidate.rs:104`). `ensure-deployed` is the `apply_cmd` backstop that guarantees
  the measured candidate is the built one.

**What ADR-0018 does and does not cover.** Its Context enumerates four tiers of image production and says
of tier 3, in-loop candidate builds, "This tier works." `[build.<name>]` targets the *infrastructure*
images — sandboxes, the loop image, the world base. The per-iteration candidate build was left in forge,
as shell, per domain. That is the gap.

## Decision

**Name the modes, declare them per component, and let the engine enforce their preconditions.**

### 1. `mode` is a typed, per-component manifest field

A composite genuinely mixes modes, so this belongs on the component, not the domain.

```toml
[component.vllm.build]
mode    = "derive-layer"
base    = "registry.example.com/team/vllm-candidate:v0.23.0"   # MUST share a registry with `target`
target  = "registry.example.com/team/vllm-candidate"
paths   = ["vllm/"]                             # the subtree whose edits become the layer

[component.epp.build]
mode          = "image"
backend       = "cluster"                       # ADR-0018's BuildBackend
containerfile = "Dockerfile.epp"                # MUST be a frozen inject (see §4)
target        = "registry.example.com/team/epp-candidate"
```

`mode = "none"` (the default) covers modes 1 and 2 above: no artifact, no rebuild. A domain that omits
`[component.<n>.build]` behaves exactly as it does today.

### 2. `crucible check` enforces the preconditions, before a turn is spent

The failure modes here are silent and expensive, which is precisely what `crucible check` exists to catch
(ADR-0014):

- `derive-layer` where `base` and `target` are on **different registries** → hard error, naming the 8s vs
  20min consequence. This is the single sharpest edge in the current design and today nothing warns.
- `derive-layer` whose `paths` include compiled sources → hard error. Appending a `.go` file to an image
  changes nothing that runs; the loop would measure the base image forever and report the change as a
  no-op. (This is the same class of bug as ADR-0007's isolation pre-flight.)
- `image` whose `containerfile` is not a `frozen = true` inject → hard error. See §4.

### 3. Caching: cache the *compiler*, not the layers

For `image` mode the per-iteration cost is dominated by the compile, and **layer caching does not help**:
the source layer changes by definition every turn, so every cache below the `COPY` is invalidated. What
makes a compiled candidate fast is a warm **compiler** cache (Go build cache, `sccache`) on a persistent
volume attached to the builder. That is a different mechanism from anything ADR-0018's backends provide,
and it is the thing worth building.

`--skip-if-exists` remains the zeroth-order cache and is strictly better than any of this: an unchanged
component performs no build at all.

### 4. The build recipe is part of the judge

**`Dockerfile.epp` currently lives inside the agent's workspace** (the EPP checkout under the
composite's workspace), the build context is the agent's checkout
(`build-candidate --context epp`), and the lora-routing composite manifest (`crucible.lora-routing.toml`)
declares **zero `[[workspace.inject]]` blocks**. The agent can edit the recipe that builds the artifact it
is scored on: vendor a prebuilt binary, neuter the compile, `COPY` around whatever the gate assumes. This
is the same shape as the #1489 reward hack, and making build modes declarative *widens* the surface unless
we close it.

Per ADR-0001 the agent may change its solution, never its evaluation. A Containerfile on the measured path
is part of the evaluation. It must be a `frozen = true` inject, re-copied before every scored measure,
exactly like a judge harness.

### 5. The pack-designing agent must be told, and the contract is the only channel

`crucible scope` writes `docs/crucible-contract.md` **verbatim** into the pack-designing agent's context
directory (`crucible/src/scope.rs:303` embeds it, `:1393` writes it out). That agent authors the
`crucible.toml` and the measure script for a new issue. Today `crucible/src/prompts/scope-propose.md`
says nothing about builds at all, so the agent picks a mode by accident — or, more often, copies whichever
apply hook it saw last.

An ADR the agent never reads changes nothing. **The modes therefore live in the contract (§3.1), not only
here.** This ADR is the rationale; the contract is the interface. Anyone tempted to "clean up" the
duplication should move the rationale, never the rules.

### 6. Where builds run

Per-iteration candidate builds stay **on-cluster** (ADR-0018's `Cluster` backend). `GithubActions` stays
for infrastructure images. Dispatch latency, queue wait, and a cold runner would land on *every iteration*,
and a runner cannot hold the warm compiler cache §3 depends on. ADR-0018 already drew this line for
candidate *test runs*; the same reasoning applies to candidate *builds*.

This is also what unblocks [ADR-0019](./0019-openshell-kubernetes-driver.md) P5: moving the candidate build
off the loop pod is what removes `buildah` from the loop image. Together with the Kubernetes compute driver
removing `podman`, that is what finally makes `privileged: false` possible.

## Alternatives considered

- **Leave it as bash in apply hooks.** It works, and it is invisible. Onboarding a composite means
  copy-pasting `fullstack-lora-apply.sh` and hoping the base tag is on the right registry. Rejected: the
  preconditions are unenforced and the failure modes are silent.
- **One mode for everything (always a full rebuild).** Uniform and slow. It would take vLLM's 8-second
  derive to a multi-minute buildah build for zero benefit, since nothing in a Python candidate compiles.
- **Always derive-layer.** Impossible for compiled components; a Go binary is not the sum of its `.go`
  files.
- **Full rebuild in GitHub Actions per iteration, relying on layer cache.** The tempting one. Rejected in
  §3/§6: the source layer invalidates the cache every turn, and dispatch latency is per-iteration.

## Consequences

- **Positive:** the mode a domain uses becomes readable, checkable, and reviewable rather than implied by
  which shell script it copied. The two silent 20-minute/no-op failure modes become `crucible check`
  errors. The reward-hacking hole in the build recipe closes. ADR-0019 P5 gains its precondition.
- **Negative / cost:** a new manifest surface and a `BuildBackend` extension; the warm-cache volume is real
  infrastructure; freezing the Containerfile means a domain author can no longer iterate on it inside a run
  (which is the point, and is the same cost ADR-0001 already accepted for the judge).
- **Risk:** `paths`-based derive is a heuristic for "did anything compiled change." A component that is
  mostly Python with a compiled extension will fool it. `crucible check` can only catch the declared shape,
  not a `.so` built at import time.

## Open questions

1. What marks a candidate build-needed at all? Today it is "the diff is non-empty for this component's
   subtree." That is a per-component `paths` glob, and it is the same predicate ADR-0018 needs for its
   `[build.<name>.watch]` block. One predicate, two consumers — worth unifying.
2. Does the warm compiler cache belong to the builder Job (a PVC per component) or to a long-lived builder
   Deployment? The Job model is simpler and matches ADR-0018; a PVC that outlives the Job is the minimum.
3. Do we need a `mode = "config"` distinct from `"none"` to name mode 2 (config tuning with a rollout) so
   `apply_cmd` stops being the only place that knowledge lives?
