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

`buildit mcp` serves `build`, `status`, `run`, `push`, `logs`, and `clean` as MCP tools over
stateless streamable http, for agents in sandboxes that can't build images. Each build, run and
push is a Kubernetes Job that ends: Kueue admits it against its queue's quota (`--queue`), its pod
runs in a user namespace (`--pod-users`), and Kubernetes deletes it `--job-ttl` seconds after it
ends. Nothing waits warm between builds.

```text
 shared cache                                       cluster registry
 ctx/<hex> ──fetch──▶ build Job: buildah bud ──push──▶ <repo>:<sandbox>-<id>
 builds/<sandbox>/<id>/ ◀─archive─┘                          │ kubelet pulls
   facts.tar image build.exit                                ▼
   run-<n>.tar run-<n>.exit ◀─archive──── run Job: cmd in the built image
```

`build` downloads a workspace subdirectory from the caller's sandbox, puts it in the shared cache
under its digest, and starts a build Job of it, optionally to a `target` stage (`build_target` is
accepted too). It returns the `build_id` at once; the build may wait in the queue. The build
pushes its image by digest to `--image-repo` (default the OpenShift internal registry,
`image-registry.openshift-image-registry.svc:5000/<namespace>/buildit`, which the pod reaches with
its service account token and the cluster's service CA; `--registry-auth insecure` for a registry
over plain HTTP); a Dockerfile's `FROM` pulls an earlier build's image from there the same
way. `status` reports `running`, `succeeded`, `failed` or `cancelled` with the exit
code, `timed_out`, and the end of the build log; poll it, then `run` a succeeded build and `clean`
it when done. `run` runs a command in a fresh pod of the built image and copies `fetch_paths` back
to `.buildit/<build_id>/` in the sandbox. `push` copies a succeeded build's image to the one
repository the server publishes to. `logs` queries a build or run log in the server, one of
`grep` (a Rust regex, with up to 20 lines of `context`), a `from_line`/`to_line` range, or the
last `tail_lines` (default 100). Output is numbered lines capped at `max_bytes` (default 16 KiB,
max 64 KiB); on overflow, grep and tail keep the end of the log and range keeps its start.
`clean` deletes a build's Jobs, or all of a sandbox's.

A build with `base` (`--base` on the CLI) builds no Dockerfile: it runs `script` with `sh -e` in
that image, as the image's root in the pod's user namespace, with the context placed at `path` and
the build's args as environment. The build is `base` (by digest; buildit resolves a tag) plus
`path` as one new layer, which the pod's archive sidecar appends with `crane append`, so a large
base is neither unpacked nor pushed again. Caches mounted under `path` stay out of the layer;
`ccache: true` points the script's ccache at the shared cache through `CCACHE_CONFIGPATH`.

Logs come from the pod while it lives. When a Job ends, the server keeps its log in `--log-dir`,
so `logs` and `status` outlive the Job and `clean`. A native sidecar in every build and run pod
uploads what the pod leaves (the build's facts and image digest, a run's fetched paths, each
step's exit record) to `builds/<sandbox>/<id>/` in the shared cache once the main container has
exited, however it exited. The sidecar alone mounts the cache's write token.

A build may declare caches, `caches: [{name, path, read_only}]` (`--cache NAME=PATH[:ro]` on the
CLI), directories mounted at `path` in every `RUN` step. A build pod starts empty, so a cache
outlives its build only with `scope: "cluster"` (`NAME=PATH:cluster`): it also lives in the shared
cache, as a zstd tarball under `dirs/<name>/<key>`. Its `key` lists context files and directories
(`--cache-key NAME=PATH`), and the snapshot's key is a hash of the name and their contents, so the
files that pin a cache's sources make a key that changes exactly when the sources do. Before
`bud`, a build restores the snapshot under its key, else the newest one under the name. After a
successful `bud`, a build that may write saves the cache under its key unless that key has a
snapshot already. Both steps appear in the build log and are stamped in `steps.log` as `CACHE
restore <name>` and `CACHE save <name>`. Every `RUN` step also sees `/buildit`, and so does a `run`
of the build, read-only. A `facts.json` object a build leaves there comes back as `status.facts`.
buildit writes `cache.json` there, naming the shared cache and, for the builds `--cache-writers`
names (`none`, `run`, the default, the run's own builds on its socket, or `all`), its write
token. A build without the token reads only.

A build with `ccache: true` (`--ccache` on the CLI) gets a generated `/etc/ccache.conf` in every
`RUN` step, so a Dockerfile that already compiles through ccache needs no change: ccache uses the
shared cache as remote storage (writing to it when the build may write) and logs each object's
result to `/buildit/ccache-stats.log`. The file overrides any `/etc/ccache.conf` the image has and
is not part of the image. It needs ccache 4.4 or later, which reads the `secondary_storage` key.

#### Building with BuildKit (experimental)

BuildKit builds a Dockerfile as `docker build` does: a `COPY` whose wildcard matches nothing
copies nothing, where `buildah bud` fails. A build asks for it with `builder: "buildkit"`
(`--builder buildkit` on the CLI), and `build` and `status` report the builder used, so a caller
whose build fails on a buildah difference can retry it under BuildKit. `--builder buildkit`
(`MCP_BUILDER`) on the server makes it the default for builds that name none; `buildah` is the
default otherwise, and a build with `base` has no builder. The server logs a warning at start
when its default is BuildKit, while this is experimental.

The pod and its privileges are the same. An init container copies BuildKit's static binaries from
`docker.io/moby/buildkit:v0.35.0-rootless`, and the `builder` container starts a `buildkitd` for
the one build and runs `buildctl build`, which pushes the image. Registry handling, `target`,
build args, the timeout, cluster caches, `status` and `logs` work as with buildah.
`--storage-driver vfs` is BuildKit's `native` snapshotter and `overlay` its `overlayfs`;
`--bud-arg` reaches buildah builds only.

`buildkitd` runs with `noProcessSandbox` and a wrapper around its runtime that drops each `RUN`
step's hostname, binds the pod's `/dev/pts`, and adds `/buildit`, `/etc/ccache.conf` and the
declared caches to the step's mounts, since BuildKit has no `--volume`; it removes the mount
points it made once the step ends, so they are not in the image. Known differences:

- The log is `buildctl --progress=plain`: output lines are prefixed, `#9 0.156 hello`, and
  `steps.log` stamps lines like `#5 [build 2/3] RUN make` in place of `STEP` and `COMMIT`.
- A Dockerfile's own `RUN --mount=type=cache` ends with the pod; use a `scope: "cluster"` cache.
- BuildKit resolves every stage's base image before it builds any.

`buildit cache` is that shared cache: blobs over HTTP, open to GET and HEAD, with PUT and DELETE
behind the token in `--token-file`, sent as a bearer token or a basic-auth password. `buildit mcp`
needs it and its token (`--cache-url`, `--cache-token-file`). ccache uses it as remote storage:

```sh
buildit cache --dir /var/cache/buildit --token-file /etc/buildit-cache/token --max-gib 180
CCACHE_REMOTE_STORAGE="http://x:$TOKEN@buildit-cache:8850/ccache"    # writes
CCACHE_REMOTE_STORAGE="http://buildit-cache:8850/ccache|read-only"   # reads only
```

Clients that declare the `io.modelcontextprotocol/tasks` extension (protocol 2026-07-28) get a
task back from `build` and `run` at once, `build/<build_id>` or `run/<build_id>/<n>`, and poll
`tasks/get`. A build task completes with the `status` reply, a run task with the reply `run`
returns; `isError` is set unless the step succeeded, and on a failed fetch. `fetch_paths` are
copied into the sandbox by the first `tasks/get` that sees the run finish. `tasks/cancel` ends the
step's Job and is a no-op on a finished task. Task state is read from the Jobs and the shared
cache, so it survives server restarts. Clients without the extension get `build`'s immediate reply
and a `run` held open until the run ends.

Callers are identified by the bearer token alone: `MCP_TOKENS_FILE` holds
`<token> <sandbox> [<workdir>]` lines (a missing workdir falls back to
`BROKER_SANDBOX_WORKDIR`) and is re-read on every request. Builds are found by Job labels, so a
restarted server keeps serving them, and a sandbox only sees its own. A sandbox may have
`--max-builds` builds running at once. Pods mount no service account token, run at most
`--pod-deadline`, and request what `--request` gives them, so a ClusterQueue counts nothing against
its quota for builds without requests. `deploy/builder-admission.yaml` admits only the pods these
Jobs run.

```sh
MCP_TOKENS_FILE=/run/crucible/buildit.tokens buildit mcp --cache-url http://buildit-cache:8850 \
  --cache-token-file /etc/buildit-cache/token
# OpenShift clusters whose CRI-O builder workload provides the user namespace
MCP_POD_USERS=openshift-builder buildit mcp --cache-url http://buildit-cache:8850 \
  --cache-token-file /etc/buildit-cache/token
# Restricted clusters can select Kaniko's userspace builder (no mount capabilities required)
MCP_BUILDER=kaniko buildit mcp --cache-url http://buildit-cache:8850 \
  --cache-token-file /etc/buildit-cache/token
# e2e on a disposable kind cluster with a registry and a `buildit cache` in it
BUILDIT_E2E_KUBECONTEXT=kind-dev BUILDIT_E2E_IMAGE_REPO=<node-ip>:30500/buildit \
  BUILDIT_E2E_CACHE_URL=http://e2e-cache.default.svc:8850 BUILDIT_E2E_CACHE_TOKEN_FILE=token \
  BUILDIT_E2E_CACHE_FORWARD=127.0.0.1:8850 BUILDIT_E2E_POD_USERS=host cargo test -- --ignored
```

`e2e_buildkit_builds_what_docker_build_does_and_keeps_buildits_mounts` builds with BuildKit
whatever the other tests build with.

### The run's socket and the CLI client

`--socket <path> --socket-workdir <dir>` also serves the tools on a unix socket
(mode 0600) to one caller with no token: the run itself, as sandbox
`loop-<hash of --run-id>` (`loop` without one). Its builds are kept apart from
every sandbox's by that name, under the same policy, namespace, queue, push
repository and limits, and no token may name it. Contexts are directories of
`--socket-workdir`, given relative or absolute; `run` writes `fetch_paths`
under it.

With `CRUCIBLE_BUILDIT_SOCKET` (or `--server <socket>`) set, `buildit build`,
`status`, `run`, `push`, `logs` and `clean` call that server over MCP and print
one JSON object on stdout. They exit 1 when the call fails, when a build ends
other than `succeeded`, or when a `run`'s command exits nonzero or times out;
usage errors exit 2. Without it, `buildit build` and `clean` work on the cluster
as before.

```sh
export CRUCIBLE_BUILDIT_SOCKET=/run/state/buildit.sock
buildit build -c svc --target base     # blocks until the build ends; --no-wait returns at once
buildit run buildit-1a2b --fetch /out -- sh -c 'make test'
buildit push buildit-1a2b              # {"image": "repo@sha256:...", "context_digest": ...}
buildit clean                          # every build of this caller
```

### Context digest

Build replies, `status`, completed build tasks and `push` report
`context_digest`, the sha256 of the context buildit uploaded. It hashes one
record per archive entry, sorted by path: `D NUL path NUL` for a directory,
`F NUL path NUL mode NUL sha256(content) NUL` for a file (mode as four octal
digits), `L NUL path NUL target NUL` for a symlink. Owners, times, directory and
symlink modes and archive order do not count. buildit keeps it as the builder
pod's `buildit.dev/context-digest` annotation. `buildit context-digest <dir>`
stages a directory as a local workspace build does (`.dockerignore`, the
excluded names, links that leave the context dropped) and prints the same
digest, so a caller can check that what was built is what it prepared.

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
