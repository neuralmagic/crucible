# ADR 0008: Domains as immutable composes — the rpm-ostree model for the frozen world

**Status:** Accepted; partially implemented (the baked-workspace / no-runtime-clone discipline and per-domain images are in use; the compose-digest machinery and the compose reconciler are not built)
**Date:** 2026-06-26
**Related:** [ADR-0001](./0001-adaptive-harness.md) (freeze the judge AND the world — this is the world half
made concrete), [ADR-0005](./0005-engine-side-builds.md) (engine-side per-iteration rebuilds — this ADR
composes the *base* those builds layer onto, see Design details), [ADR-0006](./0006-profiling-over-mcp.md)
(`[[workspace.inject]]` — the frozen-judge reset, i.e. the ostree "reset /usr" primitive),
[ADR-0007](./0007-isolation-preflight.md) (judge-validation pre-flight), the onboarding
issue-ranking pipeline, the controller follow-up.

## Context

Launching the first two *local go-test* domains (#1489 T1 harness, #1474 T0 bug) surfaced a cluster of
failures that looked unrelated but share one root cause:

1. libgit2 has no TLS in the runtime image → `setup` can't clone.
2. image Go was 1.23 but upstream `go.mod` requires ≥ 1.25.11.
3. the #1489 harness was authored/validated against `main` HEAD but didn't match the **signatures** of
   the code the loop actually compiled (`getBlockHashes` shape, `PerPromptHashes`).
4. the engine skipped the clone because a **stale baked workspace** existed, so the loop ran against an
   old commit (go-redis v9.11.0 vs main's v9.20.0) — and the main-authored harness didn't compile against it.
5. the first measure does a cold `go mod download` at run time — slow, network-dependent, the cause of
   the fast "go test failed" baseline crashes.
6. the agent sandbox has no Go toolchain, so the agent bootstraps one mid-turn to test its own edits.

Every one is **runtime mutation of the environment drifting from what we authored against**. The judge
was frozen (ADR-0001), but the *world it runs in was not*: the loop clones a moving target (`[repo].ref
= "main"`), downloads deps on the fly, and we patch the running pod (emptyDir, configMap) to compensate.
A frozen judge measuring an unfrozen world measures different code on every run — which also makes
cross-run memory meaningless (two runs of "the same" domain compile different bytes).

## Decision

**Treat each autoresearch domain as an rpm-ostree-style immutable compose.** The world — upstream
**source@SHA** + the matched toolchain + pre-fetched dependencies + the frozen judge harness — is
composed *ahead of time* into a single content-addressed image. The loop runs that image; it does not
clone, fetch, or get patched at run time. To change the world you **re-compose and rebase** to a new
digest, atomically. The agent's edits are the *only* mutable layer.

| rpm-ostree | autoresearch domain |
| --- | --- |
| immutable `/usr`, content-addressed commit | the composed world: upstream@SHA + toolchain + vendored deps + frozen harness, one digest |
| atomic `rpm-ostree rebase` to a new commit | re-pin the loop to a new composed digest — never patch in place |
| writable `/etc` + `/var` overlay | the agent's edits — a copy-on-write layer over the immutable source |
| `ostree admin reset` | `measure` re-establishing the frozen judge (the `[[workspace.inject]]` re-copy, ADR-0006) |

**The key inversion:** a *baked* workspace is not a bug — it's the point. The #4 failure wasn't that the
engine skipped the clone; it was that the baked tree was a **stale accident** instead of the deliberate,
pinned, harness-matched compose. Skip-clone is *correct* against a frozen compose. So the fix isn't
"always clone fresh main" (that re-introduces drift) — it's "bake the **matched** commit and don't clone."

**Pin point:** the harness is validated against a specific upstream commit (the compose's source pin),
whose `go.mod` fixes the toolchain (e.g. `go 1.25.11`). That commit is the compose input for
#1489/#1474; the toolchain version is read from its `go.mod`, the deps are `go mod download`-ed into the
image at compose time.

**Why this kills the whole cluster:** with no runtime clone, #1 (TLS) is moot; the toolchain is in the
base, so #2 and #6 vanish; source and harness are composed together, so #3 and #4 can't drift; deps are
pre-fetched, so #5 never happens. Four mismatch failures and two capability gaps collapse into one
build-time discipline.

## Implementation sketch (a follow-up, not this ADR)

- A per-domain **compose** step (Containerfile / buildah): `FROM` the matched-toolchain base → clone
  `[repo]@SHA` → `go mod download` (or vendor) → inject the frozen judge → emit a content-addressed image.
  Validated at *build* time (it compiles + emits a valid baseline `JUDGE_CONTRACT`), which is exactly the
  ADR-0007 isolation/validation pre-flight moved left into the compose.
- Layering, ostree-style: a shared `repo@SHA + deps + toolchain` base across same-repo domains, plus a
  thin per-domain layer (harness + goal + manifest), so onboarding the next issue is a small top layer.
- The loop runs with `setup_cmd` as a no-op (workspace pre-populated by the compose); the agent's sandbox
  *is* the composed image, edits as its writable container layer; `measure` resets the frozen judge.
- A planned **compose reconciler** (distinct from ADR-0011's PR-feedback controller) gains a "compose +
  validate domain image" reconcile step that
  replaces the live `setup`/clone dance — `groomed → composed → validated → deployed`.

## Design details

### Scope — what this does and doesn't freeze

This ADR freezes the **build/world-definition** layer, and *only* that. Be precise about the boundary:

- **In scope (frozen by construction):** upstream source@SHA, the **toolchain** (hard-frozen — see below),
  the **baseline** dependency set, and the judge harness — every input that was previously resolved at run
  time. Two runs of the same `(compose input digest, overlay commit)` compile identical bytes. This closes
  the entire launch-failure cluster (#1–#6 in Context).
- **Out of scope (NOT frozen here):**
  - **Measurement-environment drift.** The compose freezes the EPP *build*, not the cluster it runs
    against. For a `go test` domain the judge runs *inside* the composed image, so the measured thing is
    fully inside the frozen boundary. For a perf domain (#1109 TTFT) the score depends on live model-server
    pods, GPU, Envoy, and network — none of which this ADR touches. Isolating the EPP's contribution from
    that confound is **ADR-0006 (profiling)**, not this ADR.
  - **Judge nondeterminism.** Identical bytes do not make a flaky test or a timing metric deterministic.
    "Reproducibility by construction" here means a reproducible *world*, not a reproducible *score*.
- **Discipline requirements (the freeze is only real if these hold):**
  - **Pin the toolchain base by digest, not tag.** `FROM golang:1.25.11` moves; the floor is frozen only
    because the base image *digest* is a compose input (see the digest section). The recipe must pin it.
  - **Dependencies are part of the overlay; the toolchain is not.** This is the one asymmetry that matters.
    A fix may legitimately need a **new dependency** — that's normal code work, so the sandbox keeps Go
    module-proxy egress (the #1489 manifest already allowlists `proxy.golang.org`/`sum.golang.org`) and the
    agent can `go get` mid-turn. The new dep lands in the **overlay** (the `go.mod`/`go.sum` delta on top of
    `repo@SHA`, fetched into the candidate build), captured by the overlay commit in the run identity — *no
    re-compose*. The baked deps are the *starting* set and a warm cache, not a ceiling. The **toolchain is
    hard-frozen**: the agent must not raise `go.mod`'s `go` directive past the baked version. The baked
    toolchain is authoritative — the engine pins it regardless of an agent edit to the `go` line — and a
    toolchain bump is a deliberate **re-compose**, never a turn. Rule of thumb: *new deps yes, new toolchain
    no.*

### The mutable overlay is the agent's git history, not a scratch container layer

The "copy-on-write overlay" in the rpm-ostree analogy is concrete: it's the agent's **git commits on top
of `repo@SHA`**, not a vague writable container layer. This is how the loop already persists edits — the
agent commits per turn (see #1109), so its work is a chain of commits whose parent is the pinned compose
SHA. That maps onto ostree exactly: the immutable compose is the content-addressed base commit; each turn
appends a commit; the branch ref *is* the overlay.

This framing buys three things the container-layer framing doesn't:

- **The overlay is durable and inspectable.** Edits survive the per-turn workdir copy in/out (the driver
  doesn't share the sandbox FS across turns); they're a real ref, diffable against the base, replayable,
  and the unit cross-run memory reasons about ("this commit on this base scored X").
- **Reset is `git`, not teardown.** Discarding a rejected turn is `reset --hard` to the base or the last
  kept commit — the immutable compose stays pristine because nothing ever wrote to it. The
  `[[workspace.inject]]` frozen-judge reset (ADR-0006) re-overlays the harness on top, the ostree
  "reset /usr" move.
- **"Same domain" has a precise identity.** A run is `(compose digest, base SHA, overlay commit)`. Two
  runs are comparable iff the first two match — which is the reproducibility guarantee that makes
  cross-run memory meaningful.

### Relationship to ADR-0005 (engine-side builds)

The two ADRs build at **different layers and cadences**, and compose cleanly:

| | ADR-0008 compose | ADR-0005 build |
| --- | --- | --- |
| **builds** | the immutable base world (`repo@SHA` + toolchain + deps + frozen harness) | the agent's candidate (its overlay commits materialized into a runnable EPP) |
| **cadence** | once per domain; re-compose to update | per iteration, mid-turn, with compile feedback |
| **ostree analogy** | the `/usr` commit | reading the `/etc`+`/var` overlay and producing a bootable result |
| **trigger** | controller reconcile (groomed → composed → validated → deployed) | agent `build_epp` over the broker, then `deploy_candidate` |

ADR-0008 does **not** replace ADR-0005 — it provides the base that 0005's per-iteration build layers onto.
`build_epp` reads the overlay (the agent's current branch ref) and builds it *against the composed base*.
That resolves two things ADR-0005 left open:

- **Workspace sync (0005's "the crux").** 0005 proposed the agent commit and push its branch to the loop
  pod as a git remote, and build from that ref. Under 0008 that's not a workaround, it's the model: the
  pushed ref *is* the CoW overlay, and the broker builds `base@SHA + overlay`. No new driver plumbing, no
  mid-turn FS checkpoint — the overlay was always git.
- **Build latency (0005 open question #1).** Because the compose bakes the toolchain and pre-fetches deps
  (a warm `GOMODCACHE`/`GOCACHE` in the base), the per-iteration build is an **incremental `go build`
  against a hot cache**, not a cold kaniko-from-scratch. The expensive, network-bound work (clone, dep
  download, toolchain install) happens once at compose time, not on every paid iteration. The isolated
  registry 0005 wants becomes the place re-composed base digests live.

In short: **0008 freezes the floor, 0005 builds on it.** The agent edits Go (overlay), `build_epp` compiles
overlay-on-base for feedback, `deploy_candidate`/`apply_cmd` ensures the measured EPP is that candidate, and
`measure` resets the frozen judge before scoring.

### What content-addresses a compose (the digest)

A compose has **two distinct addresses**, the way ostree separates a commit checksum from the rootfs it
produces:

- **Compose input digest** — the canonical identity. A hash over the *normalized inputs* that define the
  world, not over the resulting image bytes. This is what cross-run memory keys on and what the controller
  reconciles against.
- **Image digest** — the OCI manifest digest of the artifact that actually gets pulled and run. One input
  digest can map to several image digests over time (a base-image security rebuild changes the bytes but
  not the world); they are attested equivalent. **Never key memory on the image digest** — it drifts on
  build noise (timestamps, layer order, base rebuilds) and would falsely split runs of an identical world.

The input digest is a hash over:

1. **source** — `repo URL @ commit SHA`. The SHA already content-addresses the tree, so we hash the SHA,
   not a tree scan.
2. **toolchain** — the Go version string from `go.mod` (e.g. `go 1.25.11`) plus the toolchain base image's
   digest. This is the hard-frozen input: the agent cannot move it without a re-compose (see Scope).
3. **dependencies (baseline)** — the `go.sum` content hash *at the compose SHA*. `go.sum` is already a
   content-addressed lockfile over every module, so we hash *it*, never the baked `GOMODCACHE` bytes. This
   fixes the *starting* dep set only; an overlay that adds a dep extends `go.sum`, and that delta is carried
   by the overlay commit in the run identity — it does **not** mint a new base digest.
4. **frozen harness** — a git tree hash over the injected set (judge/harness sources + the goal/manifest +
   the expected `JUDGE_CONTRACT`).
5. **compose recipe** — a hash of the Containerfile / buildah steps, so changing the build logic itself
   invalidates the digest.

The elegant part: every input is *already* content-addressed (git SHA, `go.sum`, tree hashes), so the
compose digest is a cheap, deterministic **hash-of-hashes** — no need to checksum gigabytes of baked image.
A digest is only minted **after build-time validation passes** (compiles + emits a valid baseline
`JUDGE_CONTRACT`, ADR-0007), so a compose digest always denotes a *validated* world. The full run identity
from the overlay section is therefore `(compose input digest, overlay commit)` — the base SHA is already
folded into the input digest.

### When a re-compose triggers (auto-detect, deliberately apply)

The trigger is always one thing: **the desired compose input digest no longer equals the deployed one.**
The controller recomputes the digest from current pinned inputs each reconcile; a mismatch means the domain
has *drifted* and a re-compose is **proposed**. The sources of a mismatch:

- **upstream moved** — you want to pull a newer SHA. This is *not* auto-on-every-upstream-commit (that
  re-introduces the moving-target drift this ADR exists to kill); it's a deliberate pin bump that changes
  input (1).
- **harness changed** — the judge/measurement evolved (input 4). This is a **memory-epoch boundary**: the
  new world is not comparable to the old, so prior cross-run memory for the domain is retired, not carried.
- **toolchain or deps changed** — a `go.mod` Go-version bump or a `go.sum` change (inputs 2/3), e.g. the
  agent's own dependency edit.
- **recipe changed** — the compose logic itself (input 5).

The discipline is **detect continuously, apply deliberately.** The reconcile loop is Kubernetes-shaped:

```
observe inputs → compute desired digest
  → if desired == deployed: no-op
  → else: compose + validate candidate
      → on pass:  rebase the agent's overlay onto the new base, then atomic digest swap
      → on fail:  hold on the current digest, report the failing compose (never deploy unvalidated)
```

Two things stay gated rather than automatic, because they can't be silently correct: the **atomic swap**
(it changes which world the loop measures) and the **overlay rebase** (replaying the agent's commits onto a
new base SHA is a real `git rebase` that can conflict — surfaced for review, never force-resolved). So the
controller *detects* drift and *prepares* a validated candidate on its own, but promoting it is a deliberate
act — which is exactly the "re-pin to a new digest, never patch in place" rule from the Decision, now with a
concrete trigger and gate.

## Consequences

- **Positive:** the **build/world-definition** mismatch class is designed out, not patched around (cross-run
  memory finally compares identical bytes — of the *world*; see Scope for what reproducibility does and
  doesn't cover); `measure` is offline and fast (no per-run dep fetch); validation moves to build time (fail
  the compose, not the paid loop); it's the natural unit for the controller to manage and version.
- **Negative / cost:** a per-domain compose build and a re-compose to pick up upstream changes (a
  deliberate, atomic act — which is the point); larger images (deps baked in); the agent-edits-as-overlay
  relies on git history on top of `repo@SHA` (see Design details) rather than the immutable base ever being
  written, so the base stays pristine by construction. Pinning means the kept patch is against `SHA`, not live `main`, so upstreaming a
  win is a separate rebase — correct separation (the loop optimizes a frozen world; the PR rebases it).
- **Supersedes** the runtime patching (emptyDir over the stale workspace, configMap over the adapter)
  used to limp the first launch — those are the anti-pattern this ADR removes.
