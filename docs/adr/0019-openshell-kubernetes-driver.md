# ADR 0019: The loop pod stops being a container host — OpenShell's Kubernetes driver

**Status:** Proposed
**Date:** 2026-07-08
**Related:** [ADR-0002](./0002-mediated-provisioning-mcp.md) (the broker the sandbox reaches — note the
concrete reach *hostname* is not in that ADR; it lives in `manifest.rs`'s `BrokerCfg` docs and
[the contract](../crucible-contract.md) §6.1, which is what this ADR amends),
[ADR-0005](./0005-engine-side-builds.md) (buildah in the loop image),
[ADR-0012](./0012-rendered-deployments.md) (the rendered loop pod, which is what changes),
[ADR-0018](./0018-declarative-image-builds.md) (moves candidate builds off the loop pod; this ADR
depends on it to finish the job).

## Context

The `openshell` backend runs the agent's sandbox as a **container nested inside the loop pod**. Podman
is the compute driver. That single choice is upstream of most of the accidental complexity in the deploy
path:

- The loop pod runs `privileged: true` (`crucible/src/deploy/render.rs:376`, with no comment explaining
  why), and so does every one-shot turn pod (`render.rs:1211`). Note that the test named
  `turn_pod_renders_the_reduced_privileged_shape` means reduced *machinery*, not reduced privilege.
  De-privileging is therefore two call sites, not one.
- The loop image installs `podman`, `buildah`, and `fuse-overlayfs` (`Containerfile.crucible:100`).
- `crucible/src/openshell/gateway.rs` must scrub `KUBERNETES_SERVICE_HOST`/`PORT` before launching the
  gateway, because otherwise it detects the in-cluster signal and demands a Kubernetes driver config
  that conflicts with the podman driver. This is upstream-patch 1.1, and the module docs call it
  "load-bearing." **We suppress the k8s driver on purpose.**
- The nested podman has no `imagePullSecret`, so a private sandbox image needs its own credential path
  (`pull_authfile` → `REGISTRY_AUTH_FILE`). The kubelet solves this problem for free, for every other
  pod in the cluster.

Crucible touches podman in exactly one place: `gateway.rs:120` starts `podman system service` to expose
a rootless API socket for the gateway. Nothing else in the engine shells out to it.

OpenShell ships `crates/openshell-driver-kubernetes`, which runs the sandbox as a **sibling pod**. It is
linked into `openshell-server` in-process (`openshell-server/Cargo.toml:21`), and it is present in the
exact fork commit our gateway binary is built from (`wseaton/OpenShell@f25ab2e4`) — so this is a
configuration and plumbing change, not a fork rebase. Per its README the `openshell-sandbox` supervisor
still owns "agent isolation, credential injection, policy polling," so the deny-by-default egress
allowlist that our contamination guard depends on ([ADR-0001](./0001-adaptive-harness.md),
`openshell/policy.rs`) survives the switch unchanged.

## Decision

**Make the compute driver a configuration knob, and default in-cluster deployments to `kubernetes`.**
The sandbox becomes a sibling pod scheduled by Kubernetes. The loop pod stops being a container host.

Podman stays a supported driver. It is what `--agent-backend=openshell` uses on a laptop, and other
agent infrastructure runs on EC2 where there is no Kubernetes API to talk to. Deleting it would trade
one lock-in for another. `gateway_toml()` currently hardcodes `compute_drivers = ["podman"]`; that
becomes a selected value with `kubernetes` chosen by `crucible deploy render`.

### What the switch buys

| Today (podman driver) | With the kubernetes driver |
| --- | --- |
| `privileged: true` on the loop pod | Capabilities on the *sandbox* pod; loop pod needs none (see caveat) |
| `podman` + `fuse-overlayfs` in the loop image | Neither |
| `KUBERNETES_SERVICE_*` unconditionally scrubbed (`gateway.rs:153`) | Scrub becomes **driver-conditional** — the Kubernetes driver *needs* those vars for `kube::Config::infer()` |
| `podman system service` booted (`gateway.rs:120`) | Skipped entirely; nothing local to boot |
| `pull_authfile` + `REGISTRY_AUTH_FILE` for the nested pull | An ordinary `imagePullSecrets` on the sandbox pod (`image_pull_secrets` in the driver config) |
| Sandbox resources/GPU are podman's problem | Kubernetes schedules, limits, and admits the sandbox |

### The five things that actually have to change

**1. The broker reach contract (the load-bearing break).** Today the sandbox reaches the broker at
`host.containers.internal:8849` over the pod-internal podman bridge. The Kubernetes driver injects
`hostAliases` for **`host.docker.internal` and `host.openshell.internal`** only
(`driver-kubernetes/src/driver.rs:1538`) — `host.containers.internal` does not exist. Every domain
manifest hardcodes the old name in `[agent.broker].url` and in `[agent.openshell].endpoints`. The alias
is only injected when `host_gateway_ip` is set, and it is a static config string, so crucible must fill
it with the loop pod's own IP at render time via the downward API (`status.podIP`).

The broker's default URL and the default egress endpoint become driver-dependent. The engine resolves
the host and templates it into both `.mcp.json` and the policy allowlist, so a domain manifest never
names the transport again. Recon (2026-07-08) settled the shape:

- `ComputeDriver::broker_host()` returns `host.containers.internal` (podman) or `host.openshell.internal`
  (kubernetes — the alias the driver actually injects). Hostname only; the port stays where it already
  lives, in `bind` and the URL builder.
- `BrokerCfg.url` becomes `Option<String>` with `#[serde(default)]`: `None` means engine-resolved, `Some`
  is an explicit override the engine honors. **Not** the "compare the parsed value against the old
  default to guess whether the author set it" heuristic — that silently steals a URL from anyone who
  writes the default out longhand. `deny_unknown_fields` is no obstacle: it rejects *unknown* keys, not
  *omitted* ones. `DeployProfile`'s `Secrets` already pairs `deny_unknown_fields` with omitted
  `Option` fields, and the single-domain render test parses a profile with no `[secrets]` block at all.
- The engine **auto-appends** the broker endpoint to the resolved allowlist **when, and only when,
  `[agent.broker].enabled`**. Not unconditionally. This keeps `inherit_defaults = false` honest: the
  allowlist stays fully determined by the manifest, and a broker-less domain that opts out still
  resolves to nothing (the `opting_out_with_an_empty_table_denies_everything` test must keep passing).
  It also makes the old footgun impossible — the URL and the allowlist entry can no longer disagree,
  because one derives from the other.
- Verified: **no shipped manifest sets `[agent.broker].url`**, and ADR-0002 never names the hostname, so
  the blast radius is the four domain manifests that hand-list the endpoint, plus doc comments.

P2 must land this doc change too: [the contract](../crucible-contract.md) §6.1 currently states that the
broker endpoint "is **not** a built-in, so a domain that enables `[agent.broker]` already lists it." True
today, false the moment the engine appends it. Update §6.1 in the same PR, not before.

**2. The NetworkPolicy stops being a formality.** `render.rs:823` documents that the sandbox reaches the
broker "over the pod-INTERNAL podman bridge — traffic a NetworkPolicy never sees." Once the sandbox is a
sibling pod, gateway traffic (`:17670`) and broker traffic (`:8849`) become real cluster networking, and
the loop pod's deny-all-ingress policy will drop it. The netpol must allow ingress from sandbox pods on
exactly those two ports, and nothing else. **This is a security-relevant change**: the broker, which
holds every credential, goes from unreachable-by-construction to reachable-by-policy. It must be
reviewed as such, not waved through as plumbing.

**3. A new cluster-wide prerequisite.** The driver creates `agents.x-k8s.io/v1alpha1` `Sandbox` objects
(`driver.rs:82-84`), not pods. That CRD and its controller come from
[kubernetes-sigs/agent-sandbox](https://github.com/kubernetes-sigs/agent-sandbox). This is the one step
that needs cluster-admin and is not reversible by editing a manifest.

Pin the controller release at install time (do not track the `latest` URL OpenShell's chart README
suggests), so the cluster cannot drift under you on someone else's release. It registers no admission
webhook, so a controller outage cannot wedge unrelated workloads on a shared cluster. One finding worth
carrying in the ADR:

- **A deprecation fuse.** The CRD serves `v1beta1` (storage) *and* `v1alpha1` (deprecated, `served=true`).
  Our pinned driver hardcodes `v1alpha1` (`driver.rs:83`), so it works today and emits a deprecation
  warning. The first `agent-sandbox` release that stops serving `v1alpha1` breaks the driver. Pin the
  controller version, and treat "OpenShell moves to `v1beta1`" as a prerequisite for any upgrade.

**4. RBAC.** From OpenShell's chart, the gateway's ServiceAccount needs a namespaced Role over
`agents.x-k8s.io` `sandboxes` + `sandboxes/status` (create/delete/get/list/patch/update/watch), `events`
(get/list/watch), and `pods` (get); plus a **ClusterRole** granting `authentication.k8s.io`
`tokenreviews` (create — for the `IssueSandboxToken` bootstrap) and `nodes` (get/list/watch). The
cluster-scoped half is new; today's loop SA is namespaced. `crucible deploy render` emits the RBAC, so
this landed in the rendered RBAC output.

**5. Supervisor delivery.** `supervisor_sideload_method` defaults to `image-volume`, which needs the
Kubernetes `ImageVolume` feature gate (beta ≥1.33, GA ≥1.36). Default to `init-container`, which the
driver documents as working on all versions, and revisit once the target cluster reaches 1.36.

### What this does *not* buy, yet

Removing podman does **not** by itself remove `privileged: true`. Rootless buildah in the loop pod is
almost certainly the other reason for it, and buildah is there for ADR-0005 in-loop candidate builds.
Dropping the privilege escalation needs **both** this ADR and ADR-0018's `Cluster` build backend, which
moves candidate builds out to detached rootless-buildah Jobs. That is the actual prize, and it is why
this ADR sequences *after* ADR-0018. Claiming de-privileging before then would be a lie.

## Migration

Each phase is independently landable and independently revertible. No phase is a big-bang cutover.

- **P0 — Prerequisite (cluster-admin, out-of-band).** Install the `agent-sandbox` controller,
  version-pinned. Nothing in this repo changes.
- **P1 — Driver becomes a knob.** `gateway_toml()` takes a `ComputeDriver` enum instead of hardcoding
  `["podman"]`; `[openshell.drivers.kubernetes]` is emitted when selected. Default stays `podman`, so
  this is a pure no-op refactor with unit tests on the rendered TOML.
- **P2 — Transport-agnostic broker reach.** The engine resolves the broker host from the active driver
  and templates it into `.mcp.json` and the policy allowlist. Domain manifests stop naming
  `host.containers.internal`. Ships under the podman driver first, where it must be a no-op — that is
  the test.
- **P3 — Render the Kubernetes shape.** `crucible deploy render` gains: `host_gateway_ip` from the
  downward API, the sandbox `imagePullSecrets`, the two RBAC objects, and the NetworkPolicy ingress
  rules. Behind a profile flag (`[cluster].sandbox_driver = "kubernetes"`), default off.
- **P4 — First real run.** Flip one domain (the one with the cheapest gate) on the target cluster. Verify: the
  sandbox lands as a sibling pod, the agent reaches the broker, `openshell policy update` still denies
  by default, and the gate score matches a podman-driver baseline. **The measurement must not move.**
- **P5 — De-privilege.** After ADR-0018's `Cluster` backend lands, drop `podman`, `buildah`, and
  `fuse-overlayfs` from the loop image, and `privileged: true` from the pod. Retire `pull_authfile` and
  `REGISTRY_AUTH_FILE`.

## Alternatives considered

- **Keep the podman driver.** Zero work, and it keeps laptop/EC2 parity in one code path. Rejected as
  the *default* because it forces a privileged pod, a bespoke credential path, and an upstream patch
  that exists only to hide Kubernetes from a program running in Kubernetes. Kept as a *supported*
  driver for exactly the laptop/EC2 reasons.
- **The `docker` or `vm` drivers.** `docker` has the same nesting problem with a worse security story.
  `vm` is heavier than the isolation we need, given the supervisor already enforces egress.
- **Run the sandbox as a bare Pod we render ourselves**, skipping the CRD. Rejected: it forks the
  supervisor bootstrap (`IssueSandboxToken`, TLS material, relay) that the driver already implements,
  and we would own it forever.

## Consequences

- **Positive:** the loop pod stops being a container host; the sandbox gets ordinary Kubernetes
  scheduling, limits, GPU admission, and image pull; one upstream patch and one credential path retire;
  the path to `privileged: false` opens.
- **Negative / cost:** a cluster-scoped CRD + controller + ClusterRole become prerequisites (a real
  onboarding tax for a new cluster, and it needs cluster-admin); the broker becomes network-reachable
  and must be protected by policy rather than by topology; the workspace moves onto a PVC that upstream
  itself calls "a stopgap persistence model," so the workspace-sync path needs re-verification.
- **Risk:** the driver's README labels its `driver_config` schema an "RFC 0005 POC." We are adopting a
  component that upstream has not frozen. P1–P3 are cheap and revertible precisely because P4 might
  reveal that it is not ready.

**Gateway user auth under this driver (added 2026-07-13, PR #249).** The gateway hard-rejects mTLS
user authentication with the Kubernetes compute driver (`openshell-server/src/cli.rs` — podman gets it
implicitly, k8s is told to bring OIDC or a fronting proxy), while auto-enabling its authenticator chain
from certgen's JWT bundle — so crucible's own bearer-less RPCs die `UNAUTHENTICATED` at
`CreateProvider`. The rendered k8s-driver gateway config therefore sets
`[openshell.gateway.auth] allow_unauthenticated_users = true` (`crucible/src/openshell/gateway.rs`);
podman rendering is untouched. This is an escape hatch, not an auth downgrade: the socket stays gated
by transport mTLS with a per-pod CA (`require_client_auth`), and the client cert exists only in
crucible's turn pod and the supervisor sidecar container — never the agent container — so possession
of the cert is the authorization, exactly what `mtls_auth` grants under podman. The hatch holds until
upstream accepts mTLS user auth under the kubernetes driver; the acceptance criterion for that change
is that crucible deletes this line. See [the fork ledger](../openshell-fork.md).

## Open questions

**Resolved 2026-07-08 by source recon** (kept here because the answers are load-bearing):

- *Does the tar-upload workspace path survive the PVC-backed `/sandbox`?* **Yes, unchanged.**
  `openshell/sandbox.rs` only builds `openshell sandbox upload/download` argv. The CLI transfers over
  the gateway's gRPC `CreateSshSession` relay (`openshell-cli/src/ssh.rs:94`, `:751`, `:976`), which
  reaches the sandbox through the supervisor. There is no bind-mount or podman reference in that path,
  and the podman driver has no upload path of its own — the relay is shared by both drivers.
- *Does anything besides `gateway.rs:120` assume a local podman?* Only the socket-path helper
  (`gateway.rs:72-77`), which is reachable solely from the podman boot path. No `fuse-overlayfs` or
  podman invocation exists anywhere else in the engine; no domain hook script references either.
  `crucible view` is unaffected (local files or `kubectl exec`).
- *Does the "container runtime reaps detached daemons" teardown story break?* **No, and it was the wrong
  thing to worry about.** That claim (`gateway.rs:15-17`) is about the gateway and podman daemons dying
  with the loop pod, not about the sandbox. Sandbox teardown is already explicit and driver-agnostic:
  `run.rs:187-190` calls `openshell sandbox delete` through gRPC, which under the Kubernetes driver
  deletes the `Sandbox` object.

**Still open:**

1. Does the sandbox pod need `enable_user_namespaces` / `app_armor_profile: Unconfined` on the target
   cluster's nodes? The chart defaults to `Unconfined` because RuntimeDefault can block the supervisor's netns
   setup — a claim worth confirming rather than inheriting.
