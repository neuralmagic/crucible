# ADR 0012: Crucible-rendered deployments — generate the loop/broker/rig manifests from the manifest + a deploy profile

**Status:** Accepted (slice 1 implemented)
**Date:** 2026-06-28
**Related:** [ADR-0002](./0002-mediated-provisioning-mcp.md) (the broker whose env the pod hand-duplicates),
[ADR-0005](./0005-engine-side-builds.md) (the `FORGE_*` build/deploy config the pod hand-duplicates),
[ADR-0008](./0008-domains-as-immutable-composes.md) (immutable composes — the rendered deploy must pin
*digests*, not tags), [ADR-0009](./0009-composite-domains.md) (the composite loop pod that drove this),
ADR-0011 (the controller runs "deploy" as a step — it should
call a renderer, not hand-edit yaml).

## Context

A crucible run is a pod (the loop), usually a child broker process, the live rig it measures, and the RBAC
+ secrets + volumes that wire those together. Today all of that is **hand-written, per-domain YAML**. Bringing
a composite rig online requires hand-editing, in lockstep:

- `loop-pod-openshell.yaml` — ~250 lines: the wrapper script, ~20 env vars (`FORGE_*`, `VLLM_*`,
  `BROKER_COMPOSITE`, `RIG_NAMESPACE`, `KUBECONFIG`, Vertex creds), six volumes/mounts (kube token, kubeconfig,
  push authfile, forge storage, prior-candidate, quay auth), `serviceAccountName`, a digest-pinned image.
- `loop-rbac.yaml` — a cross-namespace RoleBinding.
- `modelservers.yaml` — the rig.
- ad-hoc configmaps (prior-candidate, loggers).

Three structural problems:

1. **The pod env duplicates the manifest.** `FORGE_REGISTRY`/`FORGE_DOCKERFILE`/`FORGE_DEPLOY_*`,
   `VLLM_BASE_REF`, `BROKER_COMPOSITE`, the broker bind/name — these are facts crucible already knows from
   `crucible.pd-rollout.toml` (`[agent.broker]`, the components, the deploy targets). They are copied into
   the pod spec by hand and kept in sync by hand. The manifest is the source of truth; the pod is a stale
   shadow of it.

2. **Manual digest pinning is a footgun, not a convenience.** Every image (`loop-openshell`, the sandbox)
   is pinned to a `sha256:…` with a comment "re-pin after each rebuild — a mutable tag serves the stale
   cached layer." Re-pinning a milestone tag by hand after a rebuild is error-prone; forget it and the
   node silently runs the old binary. The digest is mechanically derivable from the tag at deploy time.

3. **The pod spec is ~90% identical across domains.** The EPP loop pod and the composite loop pod differ
   only in domain-specific env + which broker binary + which manifest. The shared structure (openshell
   privileged pod, projected kube token, IRSA, forge storage, broker child) is copy-pasted and diverges.

This is the same theme as ADR-0011: the *engine* is clean; the
*deployment* of it is manual toil that should be the engine's job.

## Decision

**Crucible renders its own deployment.** A `crucible deploy render` (emit YAML) / `crucible deploy apply`
command generates the loop pod + broker wiring + RBAC + rig references from two inputs:

- **the domain manifest** (already the source of truth: components, `[agent.broker]`, `[agent.env]`, build
  /deploy targets), and
- **a thin deploy profile** — the *only* hand-written part: the environment-specific facts crucible can't
  know (cluster/namespace, secret names, creds source, GPU/resources, the backend = openshell vs local, the
  results bucket / IRSA role). One small file per cluster, not per run.

What the renderer owns:

- **Project manifest config into pod env**, instead of hand-duplicating it. The `[agent.broker]` + build
  /deploy targets become the broker child's `FORGE_*`/`BROKER_*` env; the components become `VLLM_*`/the
  apply-hook env. Change the manifest, re-render — no second copy to forget.
- **Resolve + pin image digests at render time.** The profile names a tag (or `:latest` of a build); the
  renderer looks up the digest and emits the `@sha256:…` pin. The stale-layer footgun disappears; the
  pin is always correct because it's computed, not typed.
- **Share a template per backend pattern.** One openshell-loop template (privileged pod, projected token,
  IRSA, forge storage, broker child) parameterized by manifest + profile; domains stop copy-pasting it.
- **Emit the RBAC + secret *references*** the run needs (the SA → `edit` binding in the rig namespace, the
  push-authfile mount), as part of the same render — not a separate hand-authored file that drifts.

What it deliberately does **not** own: the *contents* of secrets (it references them; a human/External
Secrets provisions them), and the rig's domain YAML where the rig is a fixed artifact (ADR-0001 frozen
workload) — the renderer references the rig, it doesn't invent model-server topology.

```text
  crucible.pd-rollout.toml ─┐
                            ├─ crucible deploy render ──► loop-pod.yaml + rbac.yaml (+ apply)
  deploy-profile (cluster) ─┘        │
                                     ├─ env projected from the manifest (no hand-duplication)
                                     ├─ image refs resolved to @sha256 (no manual re-pin)
                                     └─ one shared openshell-loop template across domains
```

## Consequences

- **One source of truth.** The manifest already defines the run; the deploy stops being a second,
  hand-synced copy of it. "Change the gate / the broker / a component" is a manifest edit + re-render, not a
  manifest edit + a yaml safari.
- **Reproducible + footgun-free deploys.** Digests are computed; a re-render after a rebuild is always
  correct. The "forgot to re-pin, ran the stale binary" class of bug is gone.
- **Composes with the controller (ADR-0011).** The autoresearch controller's "deploy" step becomes
  `crucible deploy apply` — it never hand-writes pod specs. And with ADR-0008, the rendered deploy pins the
  *composed* image digests, so the immutable-compose guarantee reaches all the way to the running pod.
- **Lowers the bar for a new domain.** Onboarding a domain (the vLLM/EPP/composite progression) stops
  requiring a 250-line pod spec; it's a manifest + a profile entry.
- **Open questions.**
  - *Render engine.* A native Rust renderer in crucible (typed, knows the manifest) vs emitting kustomize
    /helm (gitops-native, but another layer). Lean native: crucible already parses the manifest and resolves
    images; a typed render keeps the pod spec honest to the config types.
  - *How much k8s to model.* Just the loop pod + RBAC + broker wiring (the run), or the rig too? Start with
    the run (the part that's pure duplication); leave the frozen rig as a referenced artifact.
    *Resolved by ADR-0013 (proposed):* the rig half is deferred to a pluggable external renderer
    (`[rig].backend` / RigBackend); this renderer keeps owning the run.
  - *Profile granularity.* Per-cluster vs per-domain-per-cluster. Start per-cluster with
    per-domain overrides only where a domain genuinely needs more (GPU, extra secrets).
  - *Apply vs emit.* Emit YAML (review/gitops) by default; `apply` as a convenience that shells `kubectl`.
    The renderer stays declarative; applying is a thin wrapper, same split as ADR-0005's build-vs-deploy.
  - *Secrets.* Reference-only (the renderer never holds a credential); document the expected secret names in
    the profile so a missing secret is a clear render-time error, not a `CreateContainerConfigError` later.

## Implementation (slice 1)

`crucible deploy render|apply` (`crucible/src/deploy/`) renders a composite's loop `Pod` + cross-namespace
`RoleBinding` from the manifest + a per-cluster profile, as real `k8s-openapi` objects serialized to YAML.
Decisions taken, resolving the open questions above:

- **Native Rust renderer**, real `k8s-openapi` types (the spec stays honest to the API). Emit YAML by
  default; `apply` pipes it through `kubectl apply -f -`.
- **Build/deploy targets are typed manifest config** (`[deploy]`): the generic forge build contract
  (`buildah` → `FORGE_*`, `deploy_name` → `FORGE_DEPLOY_NAME`) is typed; a per-component `env` map carries
  the domain hook's own env names verbatim. On a composite, `[deploy.<component>]` overrides the base.
- **Perfectly generic engine.** The renderer names *only* env it itself consumes (`BROKER_*`, `FORGE_*`,
  `OPENSHELL_SUPERVISOR_IMAGE`, mount-path consts). Every domain/vendor name (`EPP_*`, `VLLM_*`,
  `GCLOUD_CREDENTIALS`, `BENCH_*`) lives in the profile's generic `env`/`secret_env` maps or `[deploy].env`.
  The nested podman was originally logged into a single registry parsed from the sandbox image ref
  (`forge::oci::registry_of`); the wrapper now sets `REGISTRY_AUTH_FILE` at the mounted authfile instead,
  which podman honors ahead of its own lookup and which covers every registry a run pulls from.
- **Digests resolved in-process** via `oci-client` (`forge::oci::pin_digest`, no `crane`/`skopeo`), using the
  operator's existing registry login. Verified: the milestone tag resolves to the exact `@sha256:…` the loop
  pod previously hand-pinned.
- **Profile** (`profile.<cluster>.toml`) is the only hand-written part: namespaces, SA, secret names,
  resources, the loop image *tag* (not digest), and the generic hook/gate env.

Scope held to **the run** (loop pod + RBAC). Single-domain (non-composite) render landed 2026-07-02
(a plain manifest renders as a degenerate composite of one; it needs its own `[deploy]` block).
Still deferred: a `local`-backend template, and modeling the rig (since answered by ADR-0013).

### kube-rs migration — the engine no longer *depends* on `kubectl`

The goal here is that the engine and the `[world]` hooks never *depend* on shelling `kubectl` — not to
strip the binary. `kubectl` stays in the loop images as an operator/agent debugging affordance (too handy
to drop); the engine just doesn't call it. `forge::kube` (typed `kube-rs` 4.0 + k8s-openapi 0.28) owns
the client, patches, rollout-wait, pod exec/exec-streaming, GPU headroom, and dynamic server-side apply,
behind a blocking facade (a `block_on` that uses `block_in_place` under the broker's runtime, else a private
one). Migrated off `kubectl`:

- `forge::deploy` / `current_image` → typed patch + get (the jsonpath is gone); `tag_of` → `oci-client`
  `Reference`; rollout timeouts parse via **jiff**.
- the broker GPU admission (`gpu_check`) → typed `Node`/`Pod` sums + `apply_yaml` (no more `serde_json::Value`
  walking).
- `crucible deploy apply` → server-side `apply_yaml`.
- the pd-rollout `[world]` hooks (apply/snapshot/restore) → a generic **`forge-kube`** CLI
  (`set-image`/`set-pull-secret`/`set-rolling-update`/`rollout-restart`/`rollout-status`/`current-image`/
  `pod-name`/`exec`/`apply`); the domain orchestration stays in the hook.
- the remote viewer (`crucible view --pod`) → a streaming exec (`tail -F`) + exec-with-stdin for steer/stop.

Both loop images ship `forge-kube` (the hooks' typed kube path) *and* keep `kubectl` (the debugging
affordance). The EPP rig's engine-critical `[world]` hooks (rig-snapshot/restore/down) are migrated to
`forge-kube`; the EPP *debug/profiling* tools (inspect-rig, epp-metrics, epp-pprof) deliberately stay on
`kubectl` — they're interactive debugging, and `epp-pprof` is better migrated alongside ADR-0006.
