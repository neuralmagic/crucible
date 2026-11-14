# buildit

Build and push container images on a Kubernetes cluster, from a machine that can't build them
locally: an Apple Silicon mac where amd64 cross-builds segfault under QEMU, or an agent sandbox
with no builder. No daemons, no privileged pods, no in-cluster controllers.

| Mode | Command | Runs as | Survives a dropped connection |
| --- | --- | --- | --- |
| Pod (default) | `buildit build` | one pod, driven over the exec API | no |
| Job | `buildit build --mode job`, `buildit wait` | a Job, context in the registry | yes |
| MCP server | `buildit mcp` | one Kueue-admitted Job per build, run and push | yes |

```mermaid
sequenceDiagram
    participant L as laptop (buildit)
    participant K as Kubernetes API
    participant P as builder pod (unprivileged)
    participant R as registry
    L->>K: list nodes and pods, prefer idle nodes
    L->>K: create pod
    L->>P: exec: context tar.gz and registry auth on stdin
    L->>P: exec: build
    P->>R: push
    P-->>L: build log, digest
    L->>K: delete pod
```

## Install

Prebuilt binaries for Linux (x86_64, arm64) and macOS (arm64, x86_64) are on the
[releases page](https://github.com/wseaton/buildit/releases):

```sh
# pick your target: x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu,
#                   aarch64-apple-darwin, x86_64-apple-darwin
curl -fsSL https://github.com/wseaton/buildit/releases/latest/download/buildit-aarch64-apple-darwin.tar.gz \
  | tar -xz -C ~/.local/bin
```

Or from source:

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

| | |
| --- | --- |
| Output | The last stdout line is the digest-pinned ref (`repo@sha256:...`); the rest goes to stderr. |
| `--no-push` | Pod mode only. Prints nothing to stdout; ships registry credentials only if `~/.docker/config.json` has them. |
| Namespace | `default`, or `-n`, or `BUILDIT_NAMESPACE`. |
| Cluster | The current kubeconfig context, or `--kubecontext`. |
| Registry auth | The destination registry's inline token from `~/.docker/config.json`. `docker login <registry>` first; a macOS keychain credsStore won't work. |
| Context | Honors `.dockerignore`; always excludes top-level `.git/` and `target/`. |
| Logging | `RUST_LOG` (e.g. `RUST_LOG=debug`). |

## Backends

| Backend | How it builds | Notes |
| --- | --- | --- |
| `buildkit` (default) | rootless buildkitd via `buildctl-daemonless.sh`, uid 1000 | |
| `kaniko` | userspace layer unpack | Unmaintained upstream. No mount syscalls. |
| `buildah` | rootless, chroot isolation, vfs storage | Slower than buildkit on layer-heavy images. |

Builder pods are unprivileged and annotated
`container.apparmor.security.beta.kubernetes.io/builder: unconfined`. Ubuntu's default AppArmor
profile denies the mount syscalls rootless builders need, which shows up as
`remount /: permission denied` (buildah) or `failed to share mount point: /` (rootlesskit). Where
policy forbids the annotation, use kaniko.

## Scheduling

- buildit adds a *preferred* nodeAffinity for Ready, schedulable nodes with no GPU-requesting pods.
  If the lookup fails (RBAC) or no node is idle, the build schedules normally.
- `--schedule any` skips the lookup. So does `--node`, which pins with a nodeSelector.
- Builder pods are labeled `app=buildit`, end after 2h, and are deleted on completion, error, or
  ctrl-C. `buildit clean` deletes survivors.

## Detached builds (`--mode job` + `wait`)

```mermaid
sequenceDiagram
    participant L as laptop (buildit)
    participant R as registry
    participant K as Kubernetes API
    participant J as Job pod
    L->>R: push context as a one-layer image (skipped if unchanged)
    L->>K: create Job, then its auth Secret
    L-->>L: print the job name, exit
    K->>J: start
    J->>R: initContainer: crane export the context to an emptyDir
    J->>R: builder: build, push
    Note over J: digest written to /dev/termination-log
    L->>K: buildit wait: follow logs across retries
    K-->>L: digest-pinned ref
```

| | |
| --- | --- |
| Context image | `buildit-ctx-<sha256[..12]>` in the target repo. |
| Context expiry | On quay, labeled `quay.expires-after=2w` (`--context-label` overrides). Elsewhere, prune `buildit-ctx-*` with the registry's lifecycle policy. |
| Retries | `backoffLimit: 2`; each retry fetches the context again. |
| Cleanup | `ttlSecondsAfterFinished: 3600` removes the Job, its pods, and the Secret it owns. |
| `buildit wait <job>` | Reattaches at any point. The digest comes from the termination message, else a registry manifest HEAD. Killing `wait` kills nothing. |

## MCP server

`buildit mcp` serves six tools over stateless streamable HTTP, for agents in sandboxes that can't
build images. Every build, run and push is a Job that ends; nothing waits warm between builds.

```mermaid
flowchart TD
    AG["agent sandbox"] -- "MCP tools" --> MCP["buildit mcp"]
    MCP -- "Jobs" --> Q{{"Kueue queue"}}
    MCP <-- "contexts in, results out" --> CACHE[("buildit cache")]
    Q --> BJ["build Job"]
    Q --> RJ["run Job"]
    Q --> PJ["push Job"]
    BJ -- "image" --> REG[("cluster registry")]
    REG -- "image" --> RJ
    REG -- "image" --> PJ
    PJ -- "image" --> PUB[("publish repository")]
    BJ <-. "context in, results out" .-> CACHE
    RJ -. "results" .-> CACHE
```

| Tool | Does | Notes |
| --- | --- | --- |
| `build` | Starts a build of a workspace subdirectory. | Returns `build_id` at once; the build may wait in the queue. Optional `target` stage (or `build_target`). |
| `status` | Reports `running`, `succeeded`, `failed` or `cancelled`. | With the exit code, `timed_out`, and the end of the log. |
| `run` | Runs a command in a fresh pod of a succeeded build's image. | Copies `fetch_paths` to `.buildit/<build_id>/` in the sandbox. |
| `push` | Copies a succeeded build's image to the one repository the server publishes to. | |
| `logs` | Queries a build or run log by `grep` (a Rust regex, up to 20 lines of `context`), `from_line`/`to_line`, or `tail_lines` (default 100). | Numbered lines, capped at `max_bytes` (default 16 KiB, max 64 KiB). On overflow, grep and tail keep the end; a range keeps its start. |
| `clean` | Deletes a build's Jobs, or all of a sandbox's. | |

```mermaid
sequenceDiagram
    participant A as agent
    participant M as buildit mcp
    participant J as Jobs (Kueue)
    participant C as shared cache
    participant R as registry
    A->>M: build
    M->>C: context, by digest
    M->>J: build Job
    M-->>A: build_id
    Note over J: waits for queue quota
    J->>R: image, by digest
    J->>C: facts, digest, exit record
    loop until it ends
        A->>M: status
        M-->>A: running, succeeded, failed or cancelled
    end
    A->>M: run
    M->>J: run Job
    J->>C: fetched paths, exit record
    M-->>A: exit, log tail, fetched paths
    A->>M: push
    M->>J: push Job
    J->>R: crane copy to the publish repository
    M-->>A: image@sha256
    A->>M: clean
    M->>J: delete the build's Jobs
```

### Pods

```mermaid
flowchart LR
    CACHE[("shared cache")]
    REG[("registry")]
    subgraph POD["build pod"]
        F["init: fetch"]
        B["builder"]
        A["sidecar: archive"]
        W[/"workspace"/]
        O[/"buildit-out"/]
        F --> W --> B --> O --> A
    end
    CACHE -- "ctx/HEX" --> F
    B -- "push" --> REG
    A -- "builds/SANDBOX/ID/" --> CACHE
```

| Job | Init containers | Main container | Sidecar |
| --- | --- | --- | --- |
| build, buildah | `fetch` | `buildah bud`, `buildah push` | `archive` |
| build, BuildKit | `fetch`, `buildkit` | `buildkitd` and `buildctl build`, which pushes | `archive` |
| build with `base` | `fetch`, `prepare` | `script` in the base image | `archive`, after `crane append` |
| run | `fetch` | the command, in the built image | `archive` |
| push | | `crane copy` | |

| | |
| --- | --- |
| Admission | Kueue, against the quota of `--queue`. |
| Isolation | A user namespace (`--pod-users`), no service account token. `deploy/builder-admission.yaml` admits only these pods. |
| Lifetime | At most `--pod-deadline`; deleted `--job-ttl` seconds after the Job ends. |
| Resources | `--request`. Without requests a ClusterQueue counts nothing against its quota. |
| Concurrency | `--max-builds` running builds per sandbox. |
| Image | Pushed by digest to `--image-repo`, default the OpenShift internal registry (`image-registry.openshift-image-registry.svc:5000/<namespace>/buildit`), using the pod's service account token and the cluster's service CA. `--registry-auth insecure` for plain HTTP. A `FROM` pulls an earlier build's image the same way. |
| Archive | Uploads once the main container has exited, however it exited. Only the sidecar mounts the cache's write token. |
| Logs | Read from the pod while it lives, then kept in `--log-dir`, so `logs` and `status` outlive the Job and `clean`. |

### Builds from a base image

A build with `base` (`--base`) builds no Dockerfile. It runs `script` with `sh -e` in that image
(by digest; buildit resolves a tag), as the image's root in the pod's user namespace, with the
context at `path` and the build's args as environment. The result is `base` plus `path` as one new
layer, so a large base is neither unpacked nor pushed again. Caches mounted under `path` stay out
of the layer. `ccache: true` points ccache at the shared cache through `CCACHE_CONFIGPATH`.

### Shared cache

`buildit cache` is a blob store over HTTP: GET and HEAD are open, PUT and DELETE need the token in
`--token-file`, as a bearer token or a basic-auth password. `buildit mcp` needs it (`--cache-url`,
`--cache-token-file`).

| Key | Holds | Written by |
| --- | --- | --- |
| `ctx/<hex>` | a build context, by digest | the server |
| `builds/<sandbox>/<id>/` | `facts.tar`, `image`, `build.exit`, `run-<n>.tar`, `run-<n>.exit` | the archive sidecar |
| `dirs/<name>/<key>` | a cluster cache snapshot (zstd tarball) | builds that may write |
| `ccache/` | ccache remote storage | builds that may write |

```sh
buildit cache --dir /var/cache/buildit --token-file /etc/buildit-cache/token --max-gib 180
CCACHE_REMOTE_STORAGE="http://x:$TOKEN@buildit-cache:8850/ccache"    # writes
CCACHE_REMOTE_STORAGE="http://buildit-cache:8850/ccache|read-only"   # reads only
```

`--cache-writers` picks the builds that may write: `none`, `run` (the default: the run's own
builds on its socket) or `all`.

### Build caches

`caches: [{name, path, read_only}]` (`--cache NAME=PATH[:ro]`) mounts directories at `path` in
every `RUN` step. A build pod starts empty, so a cache outlives its build only with
`scope: "cluster"` (`NAME=PATH:cluster`), which snapshots it to the shared cache.

```mermaid
flowchart TD
    S["build starts"] --> K{"snapshot under<br/>this key?"}
    K -- yes --> RK["restore it"]
    K -- no --> N{"any snapshot<br/>under the name?"}
    N -- yes --> RN["restore the newest"]
    N -- no --> E["start empty"]
    RK --> B["build"]
    RN --> B
    E --> B
    B --> W{"succeeded, may write,<br/>key not saved yet?"}
    W -- yes --> SV["save under the key"]
    W -- no --> D["done"]
    SV --> D
```

- The snapshot key hashes the cache's name and the contents of the context paths in its `key`
  (`--cache-key NAME=PATH`), so it changes exactly when those sources do.
- `steps.log` stamps the two steps as `CACHE restore <name>` and `CACHE save <name>`.
- Every `RUN` step, and a `run` of the build (read-only), sees `/buildit`. A `facts.json` object
  left there comes back as `status.facts`. buildit writes `cache.json` there: the shared cache's
  URL and, for builds that may write, its token.
- `ccache: true` (`--ccache`) generates `/etc/ccache.conf` in every `RUN` step, with the shared
  cache as remote storage and results logged to `/buildit/ccache-stats.log`. It overrides the
  image's own file, is not part of the image, and needs ccache 4.4 or later.

### BuildKit (experimental)

BuildKit builds a Dockerfile as `docker build` does: a `COPY` whose wildcard matches nothing copies
nothing, where `buildah bud` fails.

| | |
| --- | --- |
| Per build | `builder: "buildkit"` (`--builder buildkit`). `build` and `status` report the builder used, so a caller can retry a buildah failure under BuildKit. |
| Server default | `--builder buildkit` (`MCP_BUILDER`), else `buildah`. The server warns at start. A build with `base` has no builder. |
| Binaries | From `docker.io/moby/buildkit:v0.35.0-rootless`, copied by an init container. |
| Storage | `--storage-driver vfs` is the `native` snapshotter, `overlay` is `overlayfs`. `--bud-arg` reaches buildah only. |
| Unchanged | The pod and its privileges, registry handling, `target`, build args, the timeout, cluster caches, `status`, `logs`. |

BuildKit has no `--volume`, so `buildkitd` runs with `noProcessSandbox` and a runtime wrapper that
drops each `RUN` step's hostname, binds the pod's `/dev/pts`, and mounts `/buildit`,
`/etc/ccache.conf` and the declared caches. The wrapper removes its mount points when the step
ends, so they are not in the image.

Known differences:

- The log is `buildctl --progress=plain`: lines are prefixed, `#9 0.156 hello`, and `steps.log`
  stamps lines like `#5 [build 2/3] RUN make` in place of `STEP` and `COMMIT`.
- A Dockerfile's own `RUN --mount=type=cache` ends with the pod; use a `scope: "cluster"` cache.
- BuildKit resolves every stage's base image before it builds any.

### Tasks

Clients that declare the `io.modelcontextprotocol/tasks` extension (protocol 2026-07-28) get a task
from `build` and `run` at once and poll `tasks/get`. Other clients get `build`'s immediate reply
and a `run` held open until it ends.

| | Build task | Run task |
| --- | --- | --- |
| Id | `build/<build_id>` | `run/<build_id>/<n>` |
| Completes with | the `status` reply | the `run` reply |

- `isError` is set unless the step succeeded, and on a failed fetch.
- The first `tasks/get` that sees a run finish copies its `fetch_paths` into the sandbox.
- `tasks/cancel` ends the step's Job; on a finished task it does nothing.
- Task state is read from the Jobs and the shared cache, so it survives server restarts.

### Callers

A bearer token names the caller. `MCP_TOKENS_FILE` holds `<token> <sandbox> [<workdir>]` lines
(workdir defaults to `BROKER_SANDBOX_WORKDIR`) and is re-read on every request. Builds are found by
Job labels, so a restarted server keeps serving them, and a sandbox sees only its own.

```sh
MCP_TOKENS_FILE=/run/crucible/buildit.tokens buildit mcp --cache-url http://buildit-cache:8850 \
  --cache-token-file /etc/buildit-cache/token
# e2e on a disposable kind cluster with a registry and a `buildit cache` in it
BUILDIT_E2E_KUBECONTEXT=kind-dev BUILDIT_E2E_IMAGE_REPO=<node-ip>:30500/buildit \
  BUILDIT_E2E_CACHE_URL=http://e2e-cache.default.svc:8850 BUILDIT_E2E_CACHE_TOKEN_FILE=token \
  BUILDIT_E2E_CACHE_FORWARD=127.0.0.1:8850 BUILDIT_E2E_POD_USERS=host cargo test -- --ignored
```

`e2e_buildkit_builds_what_docker_build_does_and_keeps_buildits_mounts` builds with BuildKit
whatever the other tests build with.

### The run's socket and the CLI client

`--socket <path> --socket-workdir <dir>` also serves the tools on a unix socket (mode 0600) to one
caller with no token: the run itself, as sandbox `loop-<hash of --run-id>` (`loop` without one).
No token may name that sandbox; policy, namespace, queue, push repository and limits are the same.
Contexts are directories of `--socket-workdir`, and `run` writes `fetch_paths` under it.

With `CRUCIBLE_BUILDIT_SOCKET` (or `--server <socket>`) set, `buildit build`, `status`, `run`,
`push`, `logs` and `clean` call that server and print one JSON object on stdout. Without it,
`build` and `clean` work on the cluster directly.

| Exit | When |
| --- | --- |
| 1 | The call fails, a build ends other than `succeeded`, or a `run`'s command exits nonzero or times out. |
| 2 | Usage error. |

```sh
export CRUCIBLE_BUILDIT_SOCKET=/run/state/buildit.sock
buildit build -c svc --target base     # blocks until the build ends; --no-wait returns at once
buildit run buildit-1a2b --fetch /out -- sh -c 'make test'
buildit push buildit-1a2b              # {"image": "repo@sha256:...", "context_digest": ...}
buildit clean                          # every build of this caller
```

### Context digest

Build replies, `status`, completed build tasks and `push` report `context_digest`: a sha256 over
one record per archive entry, sorted by path.

| Entry | Record |
| --- | --- |
| directory | `D NUL path NUL` |
| file | `F NUL path NUL mode NUL sha256(content) NUL` (mode as four octal digits) |
| symlink | `L NUL path NUL target NUL` |

Owners, times, directory and symlink modes and archive order do not count. The builder pod carries
the digest as its `buildit.dev/context-digest` annotation. `buildit context-digest <dir>` stages a
directory as a local workspace build does (`.dockerignore`, the excluded names, links that leave
the context dropped) and prints the same digest, so a caller can check what was built.

## Caching

Builder pods start cold. For `buildit build`:

```sh
# node-local NVMe (the CoreWeave play): pin + hostPath, fastest option
buildit build quay.io/acme/foo:tag --node gpu-node-7 --cache-hostpath /mnt/nvme/buildit-cache

# cluster storage: PVC, auto-created if missing (RWO: one build at a time)
buildit build quay.io/acme/foo:tag --cache-pvc buildit-cache --cache-size 50Gi
```

- The cache mounts at buildkit's store, buildah's containers storage, or kaniko's `--cache-dir`
  (kaniko also gets `--cache=true`, so RUN-layer cache goes to the registry).
- Rootless buildkit's snapshotter needs chown, so NFS-like storage classes (VAST, EFS) fail with
  `lchown ... operation not permitted`. Use `--cache-hostpath` or block storage for buildkit and
  buildah. kaniko's file cache works on anything.
- A hostPath cache exists only on its node, so keep `--node` with `--cache-hostpath`.
- The build workspace is always an `emptyDir`.

## How it compares

| Tool | Difference |
| --- | --- |
| `docker buildx` (kubernetes driver) | Keeps a buildkitd Deployment: a warm cache, but a daemon to manage, buildkit only, and the build dies with your connection. |
| Shipwright | An operator with CRDs and controllers. buildit needs only RBAC to create pods. |
| OpenShift binary builds (`oc start-build --from-dir`) | Nearly the same UX, OpenShift only. |
| Skaffold's kaniko builder | The same ephemeral pod, inside a dev-loop tool. |
| img, makisu, kim, kubectl-build | The same idea, all archived. |
| Depot.dev | The same thing as SaaS, on their hardware. |

## Agent skill

[`skills/buildit/`](skills/buildit/SKILL.md) is an
[Agent Skill](https://docs.claude.com/en/docs/agents-and-tools/agent-skills) that teaches a
skill-aware agent when to reach for buildit:

```sh
ln -s "$(pwd)/skills/buildit" ~/.claude/skills/buildit
```

## License

MIT
