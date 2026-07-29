# The OpenShell fork

Crucible pins `openshell-core` to a fork — [`wseaton/OpenShell`](https://github.com/wseaton/OpenShell),
branch `crucible/grpc-base` — instead of upstream [`nvidia/OpenShell`](https://github.com/nvidia/OpenShell).
This page is the running ledger of *why*, so nobody has to re-derive it from git archaeology.

## Why a fork exists at all

The branch policy is **trivially rebasable**: `crucible/grpc-base` is upstream `main` plus a small
stack of crucible-needed commits, rebased forward rather than diverging. The module docs in
`crucible/src/openshell/mod.rs` state the consequence: because the fork is trivially rebasable,
version-locking crucible to `openshell-core` "is no longer the risk it was when this shelled the
CLI" — the control-plane boundary is the gateway's native `openshell.v1` gRPC API, and the pinned
rev is the exact rev the shipped gateway binary is built from.

Two distinct gaps keep the fork alive:

1. **Fork-only commits** — features upstream doesn't have yet (the ledger below).
2. **Upstream-main commits ahead of the last release** — the pin rides upstream `main`, not the
   release tag, because crucible needs post-release work (see "Why the pin is ahead of v0.0.81").

## Pin mechanics (where the rev lives)

One rev, four consumers, all derived from `Cargo.lock`:

| Mechanism | Where | What it does |
| --- | --- | --- |
| Git dependency | `crucible/Cargo.toml` (`openshell-core = { git = "https://github.com/wseaton/OpenShell.git", rev = "…" }`) | The source of truth; `Cargo.lock` records the resolved 40-char rev |
| `cargo xtask openshell-rev` | `xtask/src/main.rs` | Canonical extraction of the rev from `Cargo.lock`; CI workflows and `just openshell-rev` shell out to it |
| `CRUCIBLE_OPENSHELL_REV` | `crucible/build.rs` → `EXPECTED_GATEWAY_REV` in `crucible/src/openshell/grpc.rs` | Embeds the rev in the binary; the runtime version gate warns on a `+g<sha>` mismatch and hard-fails below `MIN_GATEWAY_VERSION` |
| Gateway image build | `.github/workflows/openshell-gateway.yml` | Builds the gateway + supervisor images from the exact pinned rev, tagged `sha-<rev>`; loop images COPY the binaries out, and `docker.yml` fails fast if the sha-tagged image is missing |

## The divergence ledger

Fork-only commits on `crucible/grpc-base`, relative to upstream `main` (merge-base `40194f9`,
checked 2026-07-13):

| sha | What | Why crucible needs it | Upstreamable? |
| --- | --- | --- | --- |
| `bc8342bb` | `feat(providers): add aws credential provider via web identity` — gateway runs STS `AssumeRoleWithWebIdentity` off a rotating token file; a loopback container-credentials emulator in the sandbox netns serves the short-lived creds via `AWS_CONTAINER_CREDENTIALS_FULL_URI`, so no static AWS keys ever touch the sandbox filesystem | Sandboxed agents need S3 (artifacts, run evidence) without static keys; mirrors the existing `google-cloud` provider | Yes — designed as a general provider; no upstream PR filed yet |

That's the whole fork today: one commit. Earlier gRPC-boundary work that lived on this branch has
already landed upstream (which is why the branch is named `crucible/grpc-base` but no gRPC commits
remain fork-only).

## Why the pin is ahead of v0.0.81

The pin is 11 commits ahead of the last upstream release tag (`v0.0.81`); 10 of those are upstream
`main` commits, not fork work. The ones crucible actually depends on:

- `8eacb47` `feat(kubernetes): add sidecar supervisor topology (#2076)` — the supervisor runs as a
  sidecar container in the sandbox pod, which is where crucible's client-cert trust model lives
  (see below).
- `1070213` `fix(core): pin supervisor image tag to gateway version for all drivers (#2070)` —
  keeps the `sha-<rev>` gateway/supervisor image pair coherent.
- `614c8c1` `feat(kubernetes): support PVC subPath driver config (#2034)` — workspace layout under
  the k8s driver.
- `40194f9` `fix(network): fail closed when credential placeholders cannot be rewritten (#2162)` —
  credential-safety fix on the egress path.

This is why `MIN_GATEWAY_VERSION` in `grpc.rs` is `0.0.82`: the RPC surface crucible calls does not
exist in any released gateway yet.

## Pending upstream contributions

Candidates to shrink the gap to zero:

- **The AWS web-identity provider** (`bc8342bb`, the only fork commit). Upstreaming it makes the
  fork pure `upstream main @ <rev>` and the "fork" becomes just a pin.
- **Upload/download RPCs.** File transfer still goes through the `openshell` CLI (SSH-tar over the
  gateway's `CreateSshSession` relay) because no RPC covers it — one of the two CLI remnants named
  in `crucible/src/openshell/mod.rs`. A native upload/download RPC would let crucible drop the CLI
  from the turn path entirely.
- **mTLS user auth under the kubernetes driver.** The gateway hard-rejects
  `--enable-mtls-auth` with the kubernetes compute driver (`openshell-server/src/cli.rs`:
  "mTLS user authentication is not supported with the Kubernetes compute driver"), so an earlier
  fork PR had to render
  `allow_unauthenticated_users = true` for k8s-driver gateways (`crucible/src/openshell/gateway.rs`).
  Trust model while the escape hatch exists: transport mTLS with a per-pod CA still gates every
  connection (`require_client_auth`), and the client cert lives only in crucible's turn pod and the
  supervisor sidecar container — never the agent container — so possession of the cert is the
  authorization. **Acceptance criterion for the upstream change: crucible deletes the
  `allow_unauthenticated_users` escape hatch.**

## Maintenance rule

Every pin bump and every new fork commit adds or updates a ledger row above, **in the same PR that
moves `Cargo.lock`**. A rev in the lockfile that this page cannot explain is a bug.

## How to bump the pin

1. **Rebase the fork branch**: rebase `crucible/grpc-base` onto upstream `main` (it must stay
   trivially rebasable — if a commit stops rebasing cleanly, that is the signal to upstream it or
   drop it), push to `wseaton/OpenShell`.
2. **Update the dependency**: bump `rev` in `crucible/Cargo.toml` and let `Cargo.lock` re-resolve.
3. **Images build themselves**: `openshell-gateway.yml` triggers on `Cargo.lock` changes and builds
   `openshell-gateway:sha-<rev>` + `openshell-supervisor:sha-<rev>`. Note the first-run ordering:
   `docker.yml` fails fast until the gateway image exists, then retries clean.
4. **Version stamp needs upstream tags**: the gateway stamps its version via
   `git describe --tags --long`, and the fork's own tags are stale — the workflow fetches
   `refs/tags/v*` from `nvidia/OpenShell` before building. Nothing to do manually, but if the
   stamp ever reads `0.0.0`, this is where to look.
5. **Update this ledger** (see the rule above), and bump `MIN_GATEWAY_VERSION` in
   `crucible/src/openshell/grpc.rs` if the bump starts using RPCs older gateways lack.
