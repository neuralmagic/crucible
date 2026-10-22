# The OpenShell fork

Crucible pins `openshell-core` and `openshell-policy` to a fork,
[`wseaton/OpenShell`](https://github.com/wseaton/OpenShell), branch `crucible/v0.1.2`, instead of
upstream [`nvidia/OpenShell`](https://github.com/nvidia/OpenShell). This page records the
reasons for the fork, so that you do not have to find them in the Git history.

## Reason for the fork

The branch must **rebase without conflicts**. `crucible/v0.1.2` is the upstream release tag
`v0.1.2` with a small stack of commits that Crucible needs. The module docs in
`crucible/src/openshell/mod.rs` give the result: the control-plane boundary is the native
`openshell.v1` gRPC API of the gateway, and the shipped gateway, supervisor, and sandbox
runtime binaries are built from the exact pinned rev.

The pin follows a release tag, not upstream `main`. Upstream v0.1.2 has everything that
Crucible needs from upstream, so the only difference is the fork-only stack below.

## Pin mechanics

There is one rev, and five consumers use it. All of them get it from `Cargo.lock`:

| Mechanism | Where | What it does |
| --- | --- | --- |
| Git dependency | root `Cargo.toml` (`openshell-core` and `openshell-policy`, `rev = "…"`) | The source of truth. `Cargo.lock` records the resolved 40-character rev. |
| `cargo xtask openshell-rev` | `xtask/src/main.rs` | Reads the rev from `Cargo.lock`. CI workflows and `just openshell-rev` call it. |
| `CRUCIBLE_OPENSHELL_REV` | `crucible/build.rs` → `EXPECTED_GATEWAY_REV` in `crucible/src/openshell/grpc.rs` | Puts the rev in the binary. The runtime version check warns on a `+g<sha>` mismatch and fails below `MIN_GATEWAY_VERSION`. |
| Image builds | `.github/workflows/openshell-gateway.yml` | Builds the `openshell-gateway`, `openshell-supervisor`, and `openshell-sandbox` images from the exact pinned rev, with the tag `sha-<rev>`. Loop images copy the gateway binaries from them. `docker.yml` fails immediately if one of the three is missing. |
| Sandbox runtime default | `Cluster::sandbox_runtime_image` in `crucible/src/deploy/profile.rs` | A deploy profile without `sandbox_runtime_image` gets `ghcr.io/neuralmagic/openshell-sandbox:sha-<rev>` for the compiled rev. |

## The divergence ledger

Fork-only commits on `crucible/v0.1.2`, relative to upstream `v0.1.2` (`6648bd0c`), checked
2026-10-21:

| sha | What | Why crucible needs it | Upstreamable? |
| --- | --- | --- | --- |
| `3441f91a` | `feat(kubernetes): answer driver-supplied static hosts in the supervisor`: the kubernetes driver accepts `pod.host_aliases` (hostname to IP) in a sandbox's `driver_config`, validates it, and the network supervisor answers those names before its resolver, with every policy and destination check still applied | Deployments reach hosts with no public DNS record (`[cluster].host_aliases`); stock v0.1.2 rejects the key, and sandbox egress is resolved by the supervisor, not by pod `hostAliases` | Yes, upstream PR pending |
| `46dcbe04` | `fix(providers): restore supervisor-backed GCP metadata discovery`, a cherry-pick of upstream `f7273e48` (#3973): the seccomp network broker relays connects to `127.0.0.1:8174` to the supervisor, which serves the GCE metadata emulator again | v0.1.2 still injects `GCE_METADATA_HOST=127.0.0.1:8174` for `google-cloud` providers but dropped the emulator in the sandbox split, so Vertex-authenticated turns fail with `ECONNREFUSED` refreshing the token | Already upstream; drop when rebasing onto a tag that contains `f7273e48` |
| `aeeecb7d` | `Pass StaticHosts to the metadata proxy tests`: the cherry-picked tests call two functions that `3441f91a` gave an extra argument | Keeps the fork's test suite compiling | No, fork-only; dropped once either commit above is |
| `50978123` | `Move noyalib to 0.0.58`: clears RUSTSEC-2026-0333; `openshell-policy-schema` now rejects explicit nulls itself, since 0.0.58 deserializes `null` into a struct | `cargo audit` and `cargo deny` fail on noyalib 0.0.28 | Yes |

Dropped at this pin, relative to the previous `crucible/grpc-base` stack:

- **`fix(driver-kubernetes): pin ndots=1 on sandbox pods`.** Not kept. If sandbox DNS lookups
  fail with the separate supervisor pod, it returns as a new ledger row.
- **`feat(providers): add aws credential provider via web identity`** (`bc8342bb`). In v0.1.2,
  the `aws_sts_assume_role` refresh gets credentials from the ambient AWS identity of the
  gateway. Crucible now runs the gateway child as the sandbox read role through the projected
  sts-audience token (`AWS_ROLE_ARN` and `AWS_WEB_IDENTITY_TOKEN_FILE`, IMDS disabled). The
  `aws-s3` provider assumes that same role again, so the trust policy of the role must admit
  the role itself. The loop process keeps the publish role. The gateway does not inherit
  it.

## Changes in v0.1.2 that Crucible adapted to

- Every public request has a `workspace_scope`. `crucible_broker::workspace::scoped` sets the
  `default` workspace on each request. A test that scans the source fails on any request
  without it.
- Provider profiles are import-only. The gateway has no compiled profiles. Crucible ships the
  `google-cloud`, `aws-s3`, and `crucible-broker` profiles from
  `crucible/src/openshell/provider.rs` and imports them before it creates providers.
- `gateway.toml` uses schema version 2 (singular `compute_driver`). The kubernetes driver runs
  the supervisor as a separate pod, so `grpc_endpoint` is the IP of the loop pod (with a
  matching IP SAN in the server certificate). `sandbox_runtime_image` is a new image.
  `allow_driver_config = true` lets the driver configuration of each Crucible sandbox through.
- The sandbox RBAC adds the supervisor pod lifecycle, Services, bootstrap Secrets,
  NetworkPolicies, and admission metadata reads, as in the upstream helm chart.

`MIN_GATEWAY_VERSION` in `grpc.rs` is `0.1.2`. The v0.1.2 wire changes (name-addressed RPCs,
Timestamp and enum fields) do not work with older gateways.

## Pending upstream contributions

Changes that would remove the fork:

- **Static hosts** (`3441f91a`). When upstream merges it, and the next release tag has the GCP
  metadata fix and noyalib 0.0.53 or later, the fork is only a release pin.
- **Upload and download RPCs.** File transfer still uses the `openshell` CLI (SSH-tar over the
  `CreateSshSession` relay of the gateway), because no RPC supports it. This is one of the two
  remaining CLI uses that `crucible/src/openshell/mod.rs` names. With a native upload and
  download RPC, Crucible could remove the CLI from the turn path.
- **mTLS user auth with the kubernetes driver.** The gateway rejects mTLS user authentication
  with the kubernetes compute driver, so Crucible renders `allow_unauthenticated_users = true`
  for gateways with the k8s driver (`crucible/src/openshell/gateway.rs`). The trust model while
  this setting exists: transport mTLS with a CA for each pod still checks every connection
  (`require_client_auth`). The client certificate is only in the Crucible turn pod and the
  supervisor pod, never in the agent container, so possession of the certificate is the
  authorization. **Acceptance criterion for the upstream change: Crucible removes the
  `allow_unauthenticated_users` setting.**

## Maintenance rule

Each pin bump and each new fork commit adds or updates a ledger row above, **in the same PR
that changes `Cargo.lock`**. If this page cannot explain a rev in the lockfile, that is a
bug.

## How to bump the pin

1. **Rebase the fork branch.** Rebase the fork-only stack onto the new upstream tag as a new
   `crucible/<tag>` branch. It must rebase without conflicts. If a commit has conflicts, send
   it upstream or remove it. Push to `wseaton/OpenShell`.
2. **Update the dependency.** Change `rev` for `openshell-core` and `openshell-policy` in the
   root `Cargo.toml`, and let `Cargo.lock` resolve again.
3. **Images build automatically.** A change to `Cargo.lock` starts `openshell-gateway.yml`,
   which builds `openshell-gateway:sha-<rev>`, `openshell-supervisor:sha-<rev>`, and
   `openshell-sandbox:sha-<rev>`. On the first run, `docker.yml` fails until all three images
   exist. Then a retry passes.
4. **The version stamp needs upstream tags.** The gateway gets its version from
   `git describe --tags --long`, and the tags of the fork are old, so the workflow fetches the
   numeric `v*` release tags from `nvidia/OpenShell` before it builds. You do not have to do
   anything, but if the stamp is `0.0.0`, examine this step.
5. **Check the schemas again.** Derive `UPSTREAM_KUBERNETES_COMPUTE_CONFIG_FIELDS` in
   `crucible/src/openshell/gateway.rs` again from the `KubernetesComputeConfig` of the driver,
   and derive the sandbox RBAC from the upstream helm chart.
6. **Update this ledger** (see the rule above). Increase `MIN_GATEWAY_VERSION` in
   `crucible/src/openshell/grpc.rs` if the new pin uses RPCs that older gateways do not
   have.
