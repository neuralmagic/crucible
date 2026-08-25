# Images for a new domain

A crucible run uses two pods, and a domain that needs a toolchain has to put it in both:

| Pod | What runs there | Base image | Derive with |
| --- | --- | --- | --- |
| loop pod | the engine, `setup_cmd`, `measure_cmd`, `[judge.selftest]`, git memory | `ghcr.io/neuralmagic/crucible` (`Containerfile.runtime`) | `Containerfile.runtime-<domain>` |
| agent sandbox | the agent turn (`claude`/`codex` + whatever the agent shells) | `quay.io/aipcc/agentic-ci/claude-sandbox` | `Containerfile.sandbox-<domain>` |

The runtime image is domain-neutral on purpose: it ships `gcc`, `openssl-devel`, `python3`,
`git`, `jq`, and nothing else a measure might want. The sandbox ships the agent CLI and
python. Everything a domain's `measure_cmd` or agent turn needs beyond that is the domain's
image to add. `examples/selfhost` (Rust) is the worked example: `Containerfile.runtime-rust`
and `Containerfile.sandbox-rust`.

## What the two images must satisfy

**Loop pod (`Containerfile.runtime-<domain>`)**

- `FROM ghcr.io/neuralmagic/crucible:<tag>`; keep `WORKDIR /opt/crucible` and do not replace
  `/usr/local/bin/{crucible,openshell,openshell-gateway}`.
- Install the toolchain somewhere world-readable (`/usr/local/...`), not under `/root`: the pod
  runs as an arbitrary uid on OpenShift.
- Pre-fetch what `measure_cmd` builds against. The loop pod has egress (crates.io answers 200
  from `crucible-system` on waldorf, checked 2026-08-25), but a cold dependency fetch inside the
  first measure is slow and is the first thing to flake.
- `dnf` is available (full UBI10).

**Agent sandbox (`Containerfile.sandbox-<domain>`)**

- `FROM quay.io/aipcc/agentic-ci/claude-sandbox:<tag>`. It is a *stripped* UBI10: `rpm` but no
  `dnf`, no compiler, uid `sandbox` (998), `HOME=/sandbox`, `claude` at `/usr/local/bin/claude`.
- To add RPMs, install them into a rootfs on a full UBI10 stage of the same release and overlay
  it:

      FROM registry.access.redhat.com/ubi10/ubi:10.2 AS pkgs
      RUN dnf install -y --nodocs --installroot /mnt/rootfs --releasever 10 \
              --setopt install_weak_deps=false gcc glibc-devel binutils ... \
          && dnf clean all --installroot /mnt/rootfs
      FROM quay.io/aipcc/agentic-ci/claude-sandbox:0.3.11
      COPY --from=pkgs /mnt/rootfs/ /

  `perf` is not in the UBI repos; sampling profilers that carry their own sampler (`samply`)
  work, `cargo flamegraph` does not.
- Toolchain under `/usr/local` with `chmod -R a+rwX` on its caches, and `USER sandbox` at the
  end. Any `ENV PATH` you set must include the toolchain's bin dir; the agent inherits it.
- Pre-fetch dependencies here too, and more aggressively: the sandbox is deny-by-default egress,
  so a `cargo add`/`pip install` the agent does mid-turn only works if the registry is on the
  manifest's `[agent.openshell].endpoints` allowlist *and* the binary doing the fetch is in
  `[agent.openshell].binaries` (e.g. `/usr/local/cargo/bin/cargo`). Anything already in the
  image's cache sidesteps both.
- Add the sandbox base to `tools/base-image-allowlist.txt` (the build-graph lint fails on an
  unknown base).

## Build

Both images are amd64 and heavy (compiler + dep fetch). Build on the cluster, not under QEMU:

    buildit build quay.io/<you>/crucible-sandbox-<domain>:v1 -n weaton-dev \
        --kubecontext coreweave-waldorf --mode job -f Containerfile.sandbox-<domain> \
        --request cpu=8 --request memory=16Gi
    buildit wait <job> -n weaton-dev --kubecontext coreweave-waldorf   # last line: digest-pinned ref

`.dockerignore` keeps `target/`, `.claude/`, and example workspaces out of the context.
Pin `CRUCIBLE_TAG` for the runtime derivative (`--build-arg CRUCIBLE_TAG=<tag>`), or the
loop pod's engine can drift from the controller's.

A fresh quay repo is private: the namespace needs an `imagePullSecrets` entry (a
`kubernetes.io/dockerconfigjson` secret from `~/.docker/config.json`) before the pod can pull,
or the pod sits in `ImagePullBackOff` with a `401`.

## Verify before pointing a manifest at it

Run each image as a uid it will not expect and check the toolchain, the cache, and an offline
build:

    kubectl run smoke --restart=Never --image=<ref> \
        --overrides='{"spec":{"imagePullSecrets":[{"name":"quay-pull"}],"securityContext":{"runAsUser":12345,"runAsGroup":0}}}' \
        --command -- sh -c 'id; cargo --version; cd /tmp && cargo new -q t && cd t && cargo build -q --offline && ./target/debug/t'

Then `crucible check --manifest <pack>/crucible.toml` locally (it runs `measure_cmd` and the
gate self-test), set `backend = "openshell"` + `sandbox_image`, and deploy.
