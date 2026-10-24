# buildit

Build and push container images remotely on a Kubernetes cluster, from a
machine that can't build them locally. Born on an Apple Silicon mac where
heavy amd64 cross-builds segfault under QEMU emulation; the cluster's amd64
nodes build natively instead.

No daemons, no privileged pods, no in-cluster controllers: `buildit` creates
a single unprivileged builder pod, streams your build context to it over the
Kubernetes exec API, builds + pushes, prints the digest, and deletes the pod.

```
┌─────────────┐  tar.gz over exec stdin   ┌──────────────────────┐
│ your laptop │ ────────────────────────► │ builder pod (uid1000)│──► registry
│  (buildit)  │ ◄──────────────────────── │ buildkit/kaniko/     │
└─────────────┘   build logs + digest     │ buildah, no privilege│
                                          └──────────────────────┘
```

## Install

Prebuilt binaries for Linux (x86_64, arm64) and macOS (arm64, x86_64) are on
the [releases page](https://github.com/wseaton/buildit/releases):

```sh
# pick your target: x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu,
#                   aarch64-apple-darwin, x86_64-apple-darwin
curl -fsSL https://github.com/wseaton/buildit/releases/latest/download/buildit-aarch64-apple-darwin.tar.gz \
  | tar -xz -C ~/.local/bin
```

Or build from source:

```sh
cargo install --git https://github.com/wseaton/buildit
```

## Usage

```sh
buildit build quay.io/acme/foo:tag                 # context=., Dockerfile, current kubecontext
buildit build quay.io/acme/foo:tag \
  -f Dockerfile.prod -c ./svc -n builds \
  --backend buildah --build-arg FOO=bar
JOB=$(buildit build quay.io/acme/foo:tag --mode job | tail -1)   # fire and forget
buildit wait $JOB                                  # reattach anytime, prints the digest
buildit build quay.io/acme/foo:tag \
  --request cpu=4 --request memory=8Gi --limit memory=16Gi \
  --label team=infra                               # builder resources + image labels
buildit build quay.io/acme/foo:tag --output render # print manifests, touch nothing
buildit build localhost/foo:dev --target base --no-push   # build one stage, push nothing
buildit clean                                      # delete leftover builder pods/jobs
```

The last stdout line is the digest-pinned ref (`repo@sha256:...`), everything
else goes to stderr, so `$(buildit build ... | tail -1)` is scriptable.
`--no-push` (pod mode only) prints nothing to stdout and ships registry
credentials only if `~/.docker/config.json` has them.
Logging is `tracing`-based; set `RUST_LOG` (e.g. `RUST_LOG=debug` for kube
client internals) to adjust verbosity.

The namespace defaults to `default`; override per-invocation with `-n` or
persistently with `BUILDIT_NAMESPACE`. `--kubecontext` selects a kubeconfig
context (defaults to the current one).

Registry auth comes from `~/.docker/config.json`; only the destination
registry's inline token is shipped to the pod (`docker login <registry>` first
if it's missing; a macOS keychain credsStore won't work).

`.dockerignore` is honored when the context is tarred; top-level `.git/` and
`target/` are always excluded.

## Backends

All run in unprivileged pods. Each pod carries a
`container.apparmor.security.beta.kubernetes.io/builder: unconfined`
annotation: on Ubuntu nodes the default AppArmor profile denies the mount
syscalls rootless user-namespace builders need, which presents as
`remount /: permission denied` (buildah) or `failed to share mount point: /`
(rootlesskit). If your cluster policy forbids the annotation, kaniko still
works — it never touches mount syscalls.

- `buildkit` (default): rootless buildkitd via `buildctl-daemonless.sh`, uid 1000
- `kaniko`: userspace layer unpack; unmaintained upstream but a solid fallback
- `buildah`: rootless, chroot isolation + vfs storage, needs a minimal
  `storage.conf` because the stable image's config points at a rootful store
  (vfs is slower than buildkit on layer-heavy images)

## Scheduling

Before creating the builder pod, buildit lists nodes and pods and adds a
*preferred* nodeAffinity for Ready, schedulable nodes with zero
GPU-requesting pods, keeping builds off nodes with active GPU workloads.
Best effort only: if the scout fails (RBAC) or no node is idle, the build
schedules normally. `--schedule any` skips the scout.

Builder pods are labeled `app=buildit`, self-terminate after 2h
(`activeDeadlineSeconds`), and are deleted on completion, error, or ctrl-C;
`buildit clean` sweeps any survivors.

## Detached builds (`--mode job` + `wait`)

The default mode is interactive: context streamed over the exec API, build
driven by execs, pod deleted when done. Fast and zero-footprint, but the
build's fate is tied to your connection.

`--mode job` trades a little registry storage for durability. The context is
pushed to the registry as a single-layer OCI image (via
[`oci-client`](https://github.com/oras-project/rust-oci-client)), tagged
`buildit-ctx-<sha256[..12]>` in the target repo — content-addressed, so an
unchanged tree skips the upload. The build then runs as a Kubernetes **Job**:
an initContainer (`crane export`) stages the context into an emptyDir, the
builder runs as the container's actual command, and the digest is written to
`/dev/termination-log` so it survives in pod status. Registry auth rides in a
short-lived Secret owner-ref'd to the Job.

Because the context lives in the registry, everything Jobs are good at works
for real: `backoffLimit: 2` retries rebuild from a re-fetched context,
`ttlSecondsAfterFinished: 3600` garbage-collects the Job, its pods, and (via
the ownerRef) the Secret an hour after finishing.

`buildit build ... --mode job` prints the job name and exits; `buildit wait
<job>` reattaches at any point — it follows builder logs across retries,
then prints the digest-pinned ref (read from the termination message, with a
registry manifest HEAD as fallback). Killing `wait` kills nothing.

Context images are labeled for self-pruning where the registry supports it:
on quay registries buildit defaults to `quay.expires-after=2w` (override or
extend with `--context-label`). Elsewhere the `buildit-ctx-*` tags are tiny
and content-addressed; prune with your registry's lifecycle policy.

## MCP server

`buildit mcp` serves `build`, `status`, `run`, `logs`, and `clean` as MCP
tools over stateless streamable http, for agents in sandboxes that can't build
images. `build` downloads a workspace subdirectory from the caller's sandbox,
starts a buildah build of it in a builder pod, optionally to a `target` stage
(`build_target` is accepted too), without pushing, and returns its `build_id`
while the build runs on in the pod. `status` reports `running`, `succeeded`,
`failed` or `cancelled` with the exit code, `timed_out`, and the end of the build log; poll
it, then `run` a succeeded build and `clean` it when done. The build's
`timeout_s` is capped to end before the pod deadline. A pod that has ended
still reports its build: the last recorded exit, else `failed` (`timed_out`
when the pod hit its deadline), with the build log read from the container
log. `run`
runs a command in a fresh container of the image and copies `fetch_paths`
back to `.buildit/<build_id>/` in the sandbox. `logs` reads the newest 4 MiB
of a build or run log from the builder pod and queries it in the server, one
of `grep` (a Rust regex, with up to 20 lines of `context`), a
`from_line`/`to_line` range, or the last `tail_lines` (default 100). Output is
numbered lines capped at `max_bytes` (default 16 KiB, max 64 KiB); on
overflow, grep and tail keep the end of the log and range keeps its start.
`clean` deletes builder pods.

Build and run steps run detached in the builder pod. A step's exit code,
cancellation and fetched tarball are kept in files next to its log, so any
server process can report on it. Clients that declare the
`io.modelcontextprotocol/tasks` extension (protocol 2026-07-28) get a task
back from `build` and `run` as soon as the step starts, `build/<build_id>` or
`run/<build_id>/<n>`, and poll `tasks/get`. A build task completes with the
`status` reply, a run task with the reply `run` returns; `isError` is set
unless the step succeeded, and on a failed fetch. `fetch_paths` are copied
into the sandbox by the first `tasks/get` that sees the run finish.
`tasks/cancel` kills the step (a cancelled run also removes its container) and
is a no-op on a finished task. Task state lives in the pod, so it survives
server restarts; a build task outlives its pod's end like `status` does, a
run task goes away with the pod. Clients without the extension get `build`'s
immediate reply and a `run` held open until the run ends.

Callers are identified by the bearer token alone: `MCP_TOKENS_FILE` holds
`<token> <sandbox> [<workdir>]` lines (a missing workdir falls back to
`BROKER_SANDBOX_WORKDIR`) and is re-read on every request. Builds are found by
pod labels, so a restarted server keeps serving them, and a sandbox only sees
its own. Builder pods mount no service account token and end at
`--pod-deadline`; a sandbox may hold `--max-builds` live ones.

```sh
MCP_TOKENS_FILE=/run/crucible/buildit.tokens buildit mcp
BUILDIT_E2E_KUBECONTEXT=kind-dev cargo test -- --ignored   # e2e on a disposable cluster
```

## How it compares

- **`docker buildx` (kubernetes driver)** — the closest relative. It keeps a
  persistent buildkitd Deployment in your cluster, which buys a warm build
  cache but costs you a daemon to manage; it's buildkit-only, and the build
  dies with your connection. buildit's pods are ephemeral, backends are
  pluggable, and `--mode job` survives disconnects.
- **Shipwright** — k8s-native builds with pluggable strategies, but it's an
  operator: CRDs, controllers, cluster-admin buy-in. buildit needs nothing
  installed in the cluster, just RBAC to create pods.
- **OpenShift binary builds** (`oc start-build --from-dir`) — nearly the same
  UX, OpenShift-only.
- **Skaffold's kaniko builder** — same ephemeral-pod mechanism, but embedded
  in a dev-loop tool rather than a standalone CLI.
- **img, makisu, kim, kubectl-build** — the same idea, all archived.
- **Depot.dev** — this experience as SaaS, on their hardware instead of your
  cluster.

The trade for ephemeral builders is a cold cache per build; `--cache-pvc`
(below) claws that back with a persistent volume.

## Caching

Ephemeral builders start cold. Two ways to fix that:

```sh
# node-local NVMe (the CoreWeave play): pin + hostPath, fastest option
buildit build quay.io/acme/foo:tag --node gpu-node-7 --cache-hostpath /mnt/nvme/buildit-cache

# cluster storage: PVC, auto-created if missing (RWO: one build at a time)
buildit build quay.io/acme/foo:tag --cache-pvc buildit-cache --cache-size 50Gi
```

The cache mounts at each backend's natural location: buildkit's store,
buildah's containers storage, kaniko's `--cache-dir` (kaniko also gets
`--cache=true`, so RUN-layer cache goes to the registry). In testing, a
25-second RUN step went from 25s to `CACHED` with a node-local hostPath.

Compatibility note: rootless buildkit's snapshotter needs chown support from
the filesystem, so NFS-flavored storage classes (VAST, EFS, etc.) fail with
`lchown ... operation not permitted` — use `--cache-hostpath` or a
block-backed storage class for buildkit/buildah. kaniko's file cache works on
anything. The build *workspace* always stays on an `emptyDir`, which is
node-local disk (NVMe on CoreWeave) — only the cache needs a home.

`--node` pins via nodeSelector and disables the idle-node scout; hostPath
caches only exist on their node, so keep the pair together.

## Agent skill

[`skills/buildit/`](skills/buildit/SKILL.md) ships an
[Agent Skill](https://docs.claude.com/en/docs/agents-and-tools/agent-skills)
that teaches Claude Code (or any skill-aware agent) when and how to reach for
buildit — e.g. when a Dockerfile needs an amd64 image and the local machine
is Apple Silicon. Install it by copying or symlinking:

```sh
ln -s "$(pwd)/skills/buildit" ~/.claude/skills/buildit
```

## License

MIT
