# Building vLLM PR branches on a cluster

Three tiers of the same workflow, from audited artifact to sub-minute kernel
iteration. All were exercised end to end against real vLLM pull requests on an
arm64 (Grace/GB200) Kubernetes cluster with a hard-tools crucible build: no
agents, no credentials, command-backend packs only.

| Tier | Tool | Round trip |
| --- | --- | --- |
| Cold build of a PR branch | `vllm-pr-pack` | ~35 min |
| Audited iteration on a compiled base | `vllm-iter-pack` | ~7 min |
| Warm compile pod for kernel edits | `compile-server` | ~9 s |

## vllm-pr-pack

Registers a playbook that fetches any vLLM ref — a branch, tag, sha, or
`refs/pull/N/head` from upstream or any fork — and builds it onto a pinned
multi-arch release image through the run's own buildit. The Dockerfile
compiles the checkout in place on top of the release image, so the CUDA
runtime and torch come from the release and only the checkout's kernels
compile. ccache and the FetchContent dependency tree persist on the
deployment's shared buildit cache, which is what makes later builds warm:
a rebuild of an unchanged tree measured 10.4 minutes against ~35 cold, and a
nearby tree (the PR's merge base) about the same.

Pair a PR with a release image close to its fork point. The API's `base.sha`
is the base branch at PR creation, which can be newer than the fork point and
carry changes the release image's torch cannot compile; resolve the true base
with the compare API (`compare/{base}...{head}` → `merge_base_commit.sha`)
and pick the newest release tag at or before it.

## vllm-iter-pack

The audited iteration loop. It diffs a ref against the commit a base image
was built from, then applies that diff on top of the compiled tree inside the
image and recompiles only what changed:

- `mode=build` (default): a buildah build whose `FROM` names the base, pulled
  over the pod network — works with any registry the builder pods can reach.
- `mode=layer`: buildit runs the script in the base image directly and
  appends one delta layer — the fastest, but the builder pod's node must be
  able to pull the base.
- `mode=image`: the whole tree as context, producing a runnable image layer
  on the base.

Three details the modes share, each learned from a failed run: the recompile
must set the same `CUDA_ARCH_LIST` and `MAX_JOBS` the base was built with or
setup.py invents architectures the base never compiled, some of which do not
build; the deps cache is keyed on files that must be present in the context,
so the iteration context carries the checkout's `CMakeLists.txt` and
`cmake/external_projects`; and the diff must exclude paths the base image
does not carry (benchmarks, tests, docs — anything the original build context
dropped), or `git apply` fails before any compile.

A `bench_cmd` parameter runs a shell command inside the build after the
compile; JSON written to `/buildit/facts.json` comes back on the run record.

## compile-server

A persistent, warm compile pod for the loop that does not need an artifact or
a run record: edit a `.cu` file locally, `vllm-iter.sh your.diff`, and the
patch applies to the compiled tree in the pod and ninja rebuilds only the
changed kernels. Measured: 24 ms revert, 6 ms apply, 6.7 s for one kernel and
two relinks — about 9 s end to end including the exec round trips. The
compile time of the changed file is the floor: a small kernel fits well under
30 s; CUTLASS-instantiation-heavy files take minutes each and no loop changes
that.

Operational notes, all from live failures:

- The pod needs `privileged: true` for buildah's overlay mounts. The mounts,
  not the storage, are the wall: without privilege, buildah falls back to
  fuse-overlayfs and dies on missing `/dev/fuse`.
- Do not use the `vfs` storage driver for a many-layer image: it stores every
  layer as a full directory copy, and a 14 GB image consumed 139 GB of node
  disk and put the node into disk pressure. Overlay stores the image once.
- Request `ephemeral-storage` (the builder jobs carry 100Gi) or the scheduler
  places the pod on a nearly-full node.
- The first `ninja` after the pod starts rebuilds more than the diff: layer
  extraction resets the build tree's mtimes. That first pass is the warm-up;
  every pass after it is incremental.
- A pod restart costs a re-pull of the base and that warm-up pass again.

## benchmark-loader

The nodes need not pull from the build registry at all. After a successful
build, the packs upload the compiled runtime tree — the vLLM source with its
`.so` extensions and the `vllm-rs` binary, without `.deps`, the build
directory, or the Rust target directory — as one digest-pinned blob to the
registry's blob API, and the run emits its URL alongside the image. A pod on
the stock release image, which every node can pull from its public registry,
fetches that blob over the pod network in an init container and runs the tree
in front of the entrypoint: the import resolves from the fetched tree and the
compiled extensions load against the same ABI they were built on. Scale the
pod spec to as many nodes as the benchmark needs — they all read the same
blob. The one contract: the pod's image must be the exact release image the
artifact was built on, which the run record names.

## Setup

1. Register both packs through the import flow (repo, branch, path
   `demo/vllm-pr-pack` and `demo/vllm-iter-pack`), then build a base:
   launch `vllm-pr-build` with `vllm_ref` set to your PR's merge base. The
   emitted image ref is the iteration base.
2. Iterate with `vllm-iter` launches, or drop to the compile server: set
   `BASE` in `compile-server/deployment.yaml` to the base image, apply it,
   wait for `WARM_OK`, and drive it with `compile-server/vllm-iter.sh`.

Registry hostnames are placeholders (`registry.example.com`); every image
reference the automation uses at runtime comes from the deployment's buildit
configuration or the pack parameters, so nothing here names a specific
cluster.
