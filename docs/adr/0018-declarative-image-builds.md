# ADR 0018: Declarative image builds — build backends + the `building` state

**Status:** Proposed
**Date:** 2026-07-08
**Related:** [ADR-0005](./0005-engine-side-builds.md) (forge, the in-loop candidate builder this
extends), [ADR-0008](./0008-domains-as-immutable-composes.md) (the composed-world images these
builds produce), [ADR-0012](./0012-rendered-deployments.md) (digest pinning; rendered pods consume
`image@digest`), ADR-0013 (the pluggable-backend
precedent this copies), ADR-0015/ADR-0016
(the controller + ledger the `building` state lands in).

## Context

Four tiers of image production exist today, and only the first two are automated:

1. **CI-built**: the engine/loop image (`Containerfile.crucible`, `docker.yml`) and the ADR-0008
   world-base image, whose CI workflow ends with *printed instructions telling a
   human to repin* `WORLD_BASE` in `Containerfile.crucible`.
2. **Hand-built**: every sandbox and loop-openshell image. The Containerfiles embed the procedure
   ("on the arm64 laptop run the build remotely on the cluster"): mutable `:dev`/`:m1` tags, digests
   hand-plumbed into manifests, deploy profiles, and gitops values.
3. **In-loop candidate builds**: forge — buildah `build_and_push` for real Dockerfile builds, and
   the native-OCI `derive_layer` (oci-client: deterministic overlay layer, server-side blob
   mount, manifest rewrite) for base+edited-files candidates. This tier works.
4. **Version-knob choreography**: `VLLM_VERSION`, `REPO_SHA`, `WORLD_BASE` are ARGs kept in sync
   by convention and comments.

Onboarding a new composite therefore means handwriting 2–3 Containerfiles, hand-running remote
builds, and hand-plumbing digests — and the outer loop has no answer to "an approved pack implies
a rebuilt world image" (the WorkKind::Build gap). Two prior calls shape the fix:

- **On-cluster builds** borrow buildit's detached rootless-buildah Job pattern: owner-ref'd
  registry secret, digest-file output, ttl-reaped, idle-node targeting. The controller must not
  stream build contexts; detached Jobs only.
- **GitHub Actions dispatch** is the second backend. Other teams already build images in Actions;
  for the ADR-0013 self-serve story a team without cluster build capacity brings a repo with a
  build workflow and crucible dispatches it. (The earlier "GH runners are very weak" ruling was
  about candidate *test runs*, which stay on-cluster; plain image builds are what those runners
  are fine at.)

## Decision

### 1. One `BuildBackend` contract, two implementations

`Cluster` (detached rootless-buildah Job, folded into forge next to the existing buildah/oci
paths — we do not ship a separate buildit binary in the controller image) and `GithubActions`
(`workflow_dispatch` + poll). A backend's only obligations: run the build, push to the tag it was
given, report success/failure. **Crucible resolves the digest itself** via the existing
`forge::oci::pin_digest` against the pushed tag — the registry is the single source of truth for
both backends, retries are idempotent (a re-dispatch of an already-built context is a no-op
push), and no output ever has to travel back through Job logs or Actions artifacts.

### 2. Named builds, declared in the domain manifest

```toml
[build.backend-sandbox]
backend = "cluster"
image   = "ghcr.io/example-org/backend-sandbox"
timeout = "30m"
[build.backend-sandbox.cluster]
containerfile = "domains/backend/Containerfile.sandbox"
context       = "."                 # or an OCI-artifact ref (buildit's ctx_ref trick)
platform      = "linux/amd64"
[build.backend-sandbox.watch]
paths = ["domains/backend/Containerfile.sandbox", "domains/backend/tools/**"]

[build.sandbox]
backend = "github-actions"
image   = "ghcr.io/example-org/fullstack-sandbox"
needs   = ["backend-sandbox"]       # ordering + digest availability
[build.sandbox.github]
repo     = "neuralmagic/crucible"   # profile default; must pass the allowed-orgs whitelist
workflow = "crucible-build.yml"     # must exist on the dispatched ref; we never generate it
ref      = "main"
[build.sandbox.github.inputs]       # forwarded after template expansion, nothing else
containerfile = "domains/fullstack/Containerfile.sandbox"
base-image    = "{{ builds.backend-sandbox.digest_ref }}"
image         = "{{ image }}:{{ tag }}"
```

Load-bearing pieces:

- **`needs`** is a plain dependency list (reconcile won't dispatch a build until its deps have
  digests), and **`{{ builds.<name>.digest_ref }}`** hands the pinned output of a dependency to a
  downstream build. Together they retire the manual world-base repin dance mechanically.
- **`watch.paths` defines "build-needed"**: hash the watched paths at the pinned ref → the
  context digest; rebuild when it differs from the last recorded build for this image. The same
  predicate drives both backends. No `[build]` block ⇒ today's behavior exactly (statically
  configured image, never rebuilt) — the schema is purely additive. An *empty* `watch.paths` inside
  a build block is the opposite: nothing to compare ⇒ **always rebuild** (a force signal that never
  equals a recorded digest), not a constant "nothing changed".
- **The template vocabulary is closed**: `sha`, `tag`, `image`, `correlation_id`,
  `builds.<name>.digest_ref`. Nothing else expands. Anything richer turns the config into a
  programming language and — because the manifest is agent-influenced content post-pack-approval —
  a string-injection path into someone's CI. For the same reason there is **no env/secret
  pass-through**: credentials live on the backend side (the workflow repo's own secrets, the
  cluster Job's mounted authfile), never in our config.

### 3. The workflow-file contract (versioned), with declared escape hatches

A conforming workflow (`crucible-build.yml`, contract v1) honors exactly two behaviors:

1. echo the `correlation_id` input into the run name — `workflow_dispatch` returns 204 with no
   run id, so the run name is how we find our dispatch;
2. push the built image to the `image` input's tag.

Arbitrary pre-existing workflows that can't be modified declare how we adapt instead:

```toml
[build.sandbox.github.outputs]
digest      = "registry-tag"    # default; or { from = "artifact", name = "digest", path = "digest.txt" }
correlation = "run-name"        # default; or "dispatch-window" (serialize dispatches per workflow)
```

**Introspection is validation, not magic**: at manifest load / admin time, fetch the workflow
YAML and parse `on.workflow_dispatch.inputs` — fail fast when the declared mapping misses a
required input or names one that doesn't exist, warn on type mismatches. Introspection checks the
wiring a human declared; it never invents mappings. (Later it can power the admin UI: discovered
inputs rendered next to the mapping.)

### 4. The controller blocks measurement on an explicit `building` state

Remote builds take single-digit minutes, so this is a reconcile state, not an inline preflight
wait: a build-needed row enters `building` with the dispatched Job/run and expected tag recorded;
reconcile polls the backend the way it already watches work pods; success pins the digest onto
the row and only then does the run become dispatchable (rendered pods consume `image@digest`,
ADR-0012). Failure parks the row with a build-log pointer as evidence; a timeout cap keeps a
wedged run from holding a rig slot; controller restart re-adopts in-flight builds by listing
Jobs/runs, never from memory. Budgeting: per-kind concurrency cap, no $ ledger — builds are
compute, not LLM spend.

### 5. `crucible build <name>` — the same path, human-invoked

The CLI subcommand dispatches a named build (either backend), waits, and prints the digest-pinned
ref. It replaces today's hand-run remote cluster build and is the exact code path the
controller dispatches later — one implementation, two callers.

### 6. Self-test on the self-hosting domain

The self-hosting domain (the engine-develops-itself pack, the cheapest full integration test we own)
gets a `[build.loop]` block dispatching this repo's own Actions build of
`Containerfile.crucible`. That proves the GithubActions backend end-to-end — dispatch,
correlation, poll, tag push, `pin_digest` — against CI we fully control, before any external
team's workflow is in the loop. The discrimination check mirrors ADR-0014: known-good = dispatch
at HEAD and assert the resolved digest matches the pushed tag; known-bad = a workflow ref whose
build must fail, asserting the row parks with the log pointer.

## Alternatives considered

- **Generate workflow files on the fly** — rejected: repos own their build environment; we
  version a contract instead, like the turn-result envelope (ADR-0017).
- **Digest reporting via artifacts/job outputs as the default** — rejected: the registry already
  knows; kept only as a declared escape hatch for unmodifiable workflows.
- **Inline preflight blocking instead of a `building` state** — rejected: minutes-long remote
  builds would wedge reconcile; explicit state is also what restart re-adoption needs.
- **Ship buildit as a CLI inside the controller image** — rejected: forge already has kube-rs,
  buildah args, and authfile handling; folding the detached-Job mode in avoids a second binary
  with its own release pipeline.
- **Auto-map introspected inputs by name** — rejected: silent mis-wiring into someone's CI;
  introspection validates declared mappings only.

## Migration

- **M0** — forge grows the detached-Job cluster backend + `crucible build` (CLI-only).
- **M1** — controller `building` state + build dispatch/adoption in reconcile; approved packs
  with a `[build]` block flow through it.
- **M2** — GithubActions backend + workflow introspection validation; `crucible-build.yml`
  contract v1 lands in this repo.
- **M3** — dogfood: the self-hosting domain's `[build.loop]` self-test; convert the hand-built sandbox
  and loop-openshell images to declared builds; retire the world-base manual repin.

## Consequences

Onboarding a domain's images becomes a manifest edit plus (optionally) a workflow file; digests
flow mechanically from build to consumer; the measurement rig can never run on a stale or
half-built image; and the hand-run build skill becomes a fallback rather than the procedure. The
cost: a new reconcile state, a versioned workflow contract to maintain, and GH-side coupling
(App installation needs `actions:write`) for the remote backend.
