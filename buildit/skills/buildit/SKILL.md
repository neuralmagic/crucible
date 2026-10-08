---
name: buildit
description: Build and push a container image remotely on a Kubernetes cluster using the buildit CLI (single unprivileged builder pod, no daemons or controllers). Use when a Dockerfile needs an image for an architecture the local machine can't build natively (e.g. amd64 images from Apple Silicon, where QEMU cross-builds are slow or crash), or when no local container builder is available. Reaches for the cluster as a remote builder.
---

# Build container images remotely with buildit

Build on the cluster instead of cross-building locally. QEMU emulation of
heavy builds (notably Rust) is slow and unreliable; a cluster node of the
target architecture builds natively.

`buildit` creates an unprivileged builder pod, streams the build context over
the Kubernetes exec API, builds + pushes, deletes the pod, and prints the
digest-pinned ref as the last stdout line.

## Prerequisites

- `buildit` on PATH (`cargo install --git https://github.com/wseaton/buildit`).
- A kubeconfig context pointing at the build cluster (current context is used
  unless `--kubecontext` is passed) with permission to create pods and exec
  into them in the target namespace.
- Registry credentials for the destination exist inline in
  `~/.docker/config.json` under `.auths["<registry>"].auth`. If missing, run
  `docker login <registry>` (a macOS keychain `credsStore` won't write an
  inline token).
- Namespace defaults to `default`; override with `-n` or `BUILDIT_NAMESPACE`.

## Usage

```bash
buildit build registry.example.com/team/foo:tag        # context=., Dockerfile
buildit build registry.example.com/team/foo:tag \
  -f Dockerfile.prod -c ./svc -n builds --build-arg FOO=bar
buildit clean -n builds                                # sweep leftover pods/jobs
```

For long builds or flaky connections, detach: the build runs as a Kubernetes
Job (context pushed to the registry, survives client disconnect, retries
safely) and you reattach whenever:

```bash
JOB=$(buildit build registry.example.com/team/foo:tag -n builds --mode job | tail -1)
buildit wait $JOB -n builds        # follows logs, prints the digest-pinned ref
```

Everything logs to stderr (tracing, `RUST_LOG` to adjust); the last stdout
line is `repo@sha256:...`, so `IMG=$(buildit build ... | tail -1)` is
scriptable. `.dockerignore` is honored; top-level `.git/` and `target/` are
always excluded.

Builds can take several minutes (base image pull + full compile); the
builder's output streams live to the terminal.

## Options worth knowing

- `--mode pod|job` (default pod): pod = interactive exec-driven build;
  job = detached Kubernetes Job, reattach with `buildit wait <job>`.
- `--output apply|render` (default apply): render prints the manifests as
  YAML to stdout without touching the cluster or registry.
- `--schedule idle|any` (default idle): idle prefers nodes with no GPU
  workloads (best effort).
- `--request KEY=QTY` / `--limit KEY=QTY` (repeatable): builder container
  resources, e.g. `--request cpu=4 --request memory=8Gi --limit memory=16Gi`.
- `--cache-pvc NAME` / `--cache-hostpath PATH`: persistent build cache
  (PVC auto-created; hostPath is node-local NVMe, pair with `--node`).
  NFS-backed PVCs don't work with rootless buildkit/buildah (chown); kaniko
  is fine on anything.
- `--node NAME`: pin the builder to a node (disables the idle scout).
- `--label KEY=VALUE` (repeatable): labels on the built image.
- `--context-label KEY=VALUE` (repeatable, job mode): labels on the pushed
  context image. Defaults to `quay.expires-after=2w` on quay registries so
  `buildit-ctx-*` tags self-prune; passing any --context-label overrides.

## Backends

`--backend buildkit` (default) | `kaniko` | `buildah`. All unprivileged.
kaniko is unmaintained upstream, kept as fallback. buildah runs rootless with
chroot isolation + vfs storage (slower on layer-heavy images).

Builder pods get a best-effort nodeAffinity for nodes with no GPU-requesting
pods, so builds stay off active GPU workloads (`--schedule any` to skip).

## Redeploy after a successful push

Prefer the digest-pinned ref buildit prints — it sidesteps stale-tag caching
entirely:

```bash
IMG=$(buildit build registry.example.com/team/foo:tag -n builds | tail -1)
kubectl -n <app-ns> set image deploy/<name> <container>=$IMG
kubectl -n <app-ns> rollout status deploy/<name>
```

## Gotchas

- **AppArmor, not cluster policy, usually blocks rootless builders.** On
  Ubuntu nodes the default profile denies mount syscalls inside user
  namespaces (`remount /: permission denied`, `failed to share mount point:
  /`). buildit's pod specs carry
  `container.apparmor.security.beta.kubernetes.io/builder: unconfined`, which
  unblocks it. If cluster policy forbids that annotation, use
  `--backend kaniko` — it never touches mount syscalls.
- **Overwriting an existing tag** may serve a node-cached layer on redeploy;
  deploy by digest (above) or use a fresh tag per build.
- **Don't trust a 0 exit from a piped `docker build ... | tail`** locally —
  the exit is the tail's. buildit reads the real exec status frame instead.
