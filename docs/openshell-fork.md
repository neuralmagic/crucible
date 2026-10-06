# The OpenShell fork

Crucible pins `openshell-core` and `openshell-policy` to a fork,
[`wseaton/OpenShell`](https://github.com/wseaton/OpenShell), branch `crucible/v0.1.2`, instead of
upstream [`nvidia/OpenShell`](https://github.com/nvidia/OpenShell). This page is the running ledger
of *why*, so nobody has to re-derive it from git archaeology.

## Why a fork exists at all

The branch policy is **trivially rebasable**: `crucible/v0.1.2` is the upstream release tag
`v0.1.2` plus a small stack of crucible-needed commits. The module docs in
`crucible/src/openshell/mod.rs` state the consequence: the control-plane boundary is the gateway's
native `openshell.v1` gRPC API, and the pinned rev is the exact rev the shipped gateway,
supervisor, and sandbox runtime binaries are built from.

The pin rides a release tag, not upstream `main`: everything crucible needs from upstream is in
v0.1.2, so the only gap is the fork-only stack below.

## Pin mechanics (where the rev lives)

One rev, five consumers, all derived from `Cargo.lock`:

| Mechanism | Where | What it does |
| --- | --- | --- |
| Git dependency | root `Cargo.toml` (`openshell-core` and `openshell-policy`, `rev = "…"`) | The source of truth; `Cargo.lock` records the resolved 40-char rev |
| `cargo xtask openshell-rev` | `xtask/src/main.rs` | Canonical extraction of the rev from `Cargo.lock`; CI workflows and `just openshell-rev` shell out to it |
| `CRUCIBLE_OPENSHELL_REV` | `crucible/build.rs` → `EXPECTED_GATEWAY_REV` in `crucible/src/openshell/grpc.rs` | Embeds the rev in the binary; the runtime version gate warns on a `+g<sha>` mismatch and hard-fails below `MIN_GATEWAY_VERSION` |
| Image builds | `.github/workflows/openshell-gateway.yml` | Builds the `openshell-gateway`, `openshell-supervisor`, and `openshell-sandbox` images from the exact pinned rev, tagged `sha-<rev>`; loop images COPY the gateway binaries out, and `docker.yml` fails fast if any of the three is missing |
| Sandbox runtime default | `Cluster::sandbox_runtime_image` in `crucible/src/deploy/profile.rs` | A deploy profile without `sandbox_runtime_image` gets `ghcr.io/neuralmagic/openshell-sandbox:sha-<rev>` for the compiled-in rev |

## The divergence ledger

Fork-only commits on `crucible/v0.1.2`, relative to upstream `v0.1.2` (`6648bd0c`), checked
2026-10-05:

| sha | What | Why crucible needs it | Upstreamable? |
| --- | --- | --- | --- |
| `3441f91a` | `feat(kubernetes): answer driver-supplied static hosts in the supervisor`: the kubernetes driver accepts `pod.host_aliases` (hostname to IP) in a sandbox's `driver_config`, validates it, and the network supervisor answers those names before its resolver, with every policy and destination check still applied | Deployments reach hosts with no public DNS record (`[cluster].host_aliases`); stock v0.1.2 rejects the key, and sandbox egress is resolved by the supervisor, not by pod `hostAliases` | Yes, upstream PR pending |

Dropped at this pin, relative to the previous `crucible/grpc-base` stack:

- **`fix(driver-kubernetes): pin ndots=1 on sandbox pods`.** Not carried forward. If sandbox DNS
  lookups regress under the split supervisor pod, it comes back as a new ledger row.
- **`feat(providers): add aws credential provider via web identity`** (`bc8342bb`). v0.1.2's
  `aws_sts_assume_role` refresh mints from the gateway's ambient AWS identity. Crucible now runs the
  gateway child as the sandbox read role through the projected sts-audience token
  (`AWS_ROLE_ARN` + `AWS_WEB_IDENTITY_TOKEN_FILE`, IMDS disabled), and the `aws-s3` provider
  assumes that same role again, so the role's trust policy must admit itself. The loop process
  keeps the publish role; the gateway never inherits it.

## What changed with v0.1.2 that crucible adapted to

- Every public request carries a `workspace_scope`; `crucible_broker::workspace::scoped` sets the
  `default` workspace on each one, and a source-scanning test fails on any request built without it.
- Provider profiles are import-only: no profile is compiled into the gateway. Crucible ships
  `google-cloud`, `aws-s3`, and `crucible-broker` profiles from `crucible/src/openshell/provider.rs`
  and imports them before creating providers.
- `gateway.toml` is schema version 2 (singular `compute_driver`). The kubernetes driver runs the
  supervisor as its own pod, so `grpc_endpoint` is the loop pod's IP (with a matching server-cert
  IP SAN), `sandbox_runtime_image` is a new image, and `allow_driver_config = true` lets crucible's
  per-sandbox driver config through.
- The sandbox RBAC grows the supervisor pod lifecycle, Services, bootstrap Secrets,
  NetworkPolicies, and admission metadata reads, matching upstream's helm chart.

`MIN_GATEWAY_VERSION` in `grpc.rs` is `0.1.2`: the v0.1.2 wire changes (name-addressed RPCs,
Timestamp and enum fields) break against every older gateway.

## Pending upstream contributions

Candidates to shrink the gap to zero:

- **Static hosts** (`3441f91a`, the only fork commit). Once it lands upstream the fork is a bare
  release pin.
- **Upload/download RPCs.** File transfer still goes through the `openshell` CLI (SSH-tar over the
  gateway's `CreateSshSession` relay) because no RPC covers it, one of the two CLI remnants named
  in `crucible/src/openshell/mod.rs`. A native upload/download RPC would let crucible drop the CLI
  from the turn path entirely.
- **mTLS user auth under the kubernetes driver.** The gateway rejects mTLS user authentication
  with the kubernetes compute driver, so crucible renders `allow_unauthenticated_users = true` for
  k8s-driver gateways (`crucible/src/openshell/gateway.rs`). Trust model while the escape hatch
  exists: transport mTLS with a per-pod CA still gates every connection (`require_client_auth`),
  and the client cert lives only in crucible's turn pod and the supervisor pod, never the agent
  container, so possession of the cert is the authorization. **Acceptance criterion for the
  upstream change: crucible deletes the `allow_unauthenticated_users` escape hatch.**

## Maintenance rule

Every pin bump and every new fork commit adds or updates a ledger row above, **in the same PR that
moves `Cargo.lock`**. A rev in the lockfile that this page cannot explain is a bug.

## How to bump the pin

1. **Rebase the fork branch**: rebase the fork-only stack onto the new upstream tag as a new
   `crucible/<tag>` branch (it must stay trivially rebasable; if a commit stops rebasing cleanly,
   that is the signal to upstream it or drop it), push to `wseaton/OpenShell`.
2. **Update the dependency**: bump `rev` for both `openshell-core` and `openshell-policy` in the
   root `Cargo.toml` and let `Cargo.lock` re-resolve.
3. **Images build themselves**: `openshell-gateway.yml` triggers on `Cargo.lock` changes and builds
   `openshell-gateway:sha-<rev>`, `openshell-supervisor:sha-<rev>`, and
   `openshell-sandbox:sha-<rev>`. Note the first-run ordering: `docker.yml` fails fast until all
   three exist, then retries clean.
4. **Version stamp needs upstream tags**: the gateway stamps its version via
   `git describe --tags --long`, and the fork's own tags are stale, so the workflow fetches the
   numeric `v*` release tags from `nvidia/OpenShell` before building. Nothing to do manually, but
   if the stamp ever reads `0.0.0`, this is where to look.
5. **Re-check the schemas**: re-derive `UPSTREAM_KUBERNETES_COMPUTE_CONFIG_FIELDS` in
   `crucible/src/openshell/gateway.rs` from the driver's `KubernetesComputeConfig`, and the sandbox
   RBAC from upstream's helm chart.
6. **Update this ledger** (see the rule above), and bump `MIN_GATEWAY_VERSION` in
   `crucible/src/openshell/grpc.rs` if the bump starts using RPCs older gateways lack.
