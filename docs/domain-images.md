# Images for a new domain

A Crucible run uses two pods. A domain that needs a toolchain must install it in both:

| Pod | What runs there | Base image | Derive with |
| --- | --- | --- | --- |
| loop pod | the engine, `setup_cmd`, `measure_cmd`, `[judge.selftest]`, git memory | `ghcr.io/neuralmagic/crucible` (`Containerfile.runtime`) | `Containerfile.runtime-<domain>` |
| agent sandbox | the agent turn (`claude`/`codex`/`opencode`/`pi` and the commands that the agent runs) | `quay.io/aipcc/agentic-ci/claude-sandbox` | `Containerfile.sandbox-<domain>` |

The runtime image does not depend on a domain. It contains `gcc`, `openssl-devel`,
`python3`, `git`, and `jq`, and no other measurement tools. The sandbox contains the agent
CLI and Python. The domain image must add all other tools that `measure_cmd` or the agent
turn needs. `examples/selfhost` (Rust) is the example: `Containerfile.runtime-rust` and
`Containerfile.sandbox-rust`.

## What the two images must satisfy

**Loop pod (`Containerfile.runtime-<domain>`)**

- Use `FROM ghcr.io/neuralmagic/crucible:<tag>`. Keep `WORKDIR /opt/crucible`. Do not replace
  `/usr/local/bin/{crucible,openshell,openshell-gateway}`.
- Install the toolchain in a world-readable location (`/usr/local/...`), not under `/root`. On
  OpenShift, the pod runs as an arbitrary uid.
- Pre-fetch the dependencies that `measure_cmd` builds against. The loop pod has egress
  (crates.io answered 200 from `crucible-system` on waldorf on 2026-08-25), but a cold
  dependency fetch in the first measure is slow and often fails first.
- `dnf` is available (full UBI10).

**Agent sandbox (`Containerfile.sandbox-<domain>`)**

- Use `FROM quay.io/aipcc/agentic-ci/claude-sandbox:<tag>`. It is a *minimal* UBI10: it has
  `rpm` but no `dnf` and no compiler. The uid is `sandbox` (998), `HOME=/sandbox`, and
  `claude` is at `/usr/local/bin/claude`.
- To add RPMs, install them into a rootfs in a full UBI10 stage of the same release, and copy
  that rootfs over the image:

      FROM registry.access.redhat.com/ubi10/ubi:10.2 AS pkgs
      RUN dnf install -y --nodocs --installroot /mnt/rootfs --releasever 10 \
              --setopt install_weak_deps=false gcc glibc-devel binutils ... \
          && dnf clean all --installroot /mnt/rootfs
      FROM quay.io/aipcc/agentic-ci/claude-sandbox:0.3.11
      COPY --from=pkgs /mnt/rootfs/ /

  `perf` is not in the UBI repositories. Sampling profilers with their own sampler
  (`samply`) work. `cargo flamegraph` does not work.
- Install the toolchain under `/usr/local`, run `chmod -R a+rwX` on its caches, and put
  `USER sandbox` at the end. If you set `ENV PATH`, include the bin directory of the
  toolchain. The agent inherits it.
- Pre-fetch more dependencies here than in the loop pod. The sandbox has deny-by-default
  egress. A `cargo add` or `pip install` by the agent during a turn works only if the registry
  is on the `[agent.openshell].endpoints` allowlist of the manifest *and* the binary that
  fetches is in `[agent.openshell].binaries` (for example, `/usr/local/cargo/bin/cargo`).
  Files that are already in the image cache need neither.
- Add the sandbox base to `tools/base-image-allowlist.txt`. The build-graph lint fails on an
  unknown base.

## Build

Both images are amd64 and large (compiler and dependency fetch). Build them on the cluster, not under QEMU:

    buildit build quay.io/<you>/crucible-sandbox-<domain>:v1 -n weaton-dev \
        --kubecontext coreweave-waldorf --mode job -f Containerfile.sandbox-<domain> \
        --request cpu=8 --request memory=16Gi
    buildit wait <job> -n weaton-dev --kubecontext coreweave-waldorf   # last line: digest-pinned ref

`.dockerignore` keeps `target/`, `.claude/`, and the example workspaces out of the build
context. Pin `CRUCIBLE_TAG` for the runtime image (`--build-arg CRUCIBLE_TAG=<tag>`). If you
do not, the engine version in the loop pod can be different from the controller version.

A new quay repository is private. The namespace needs an `imagePullSecrets` entry (a
`kubernetes.io/dockerconfigjson` secret from `~/.docker/config.json`) before the pod can pull
the image. Without it, the pod stays in `ImagePullBackOff` with a `401`.

## Verify the images

Before a manifest uses an image, run the image as an unexpected uid. Check the toolchain, the
cache, and an offline build:

    kubectl run smoke --restart=Never --image=<ref> \
        --overrides='{"spec":{"imagePullSecrets":[{"name":"quay-pull"}],"securityContext":{"runAsUser":12345,"runAsGroup":0}}}' \
        --command -- sh -c 'id; cargo --version; cd /tmp && cargo new -q t && cd t && cargo build -q --offline && ./target/debug/t'

Then run `crucible check --manifest <pack>/crucible.toml` locally. It runs `measure_cmd` and
the gate self-test. Set `backend = "openshell"` and `sandbox_image`, and deploy.
