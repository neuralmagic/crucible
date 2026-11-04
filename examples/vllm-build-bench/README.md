# vllm-build-bench

Times vLLM candidate builds through the run's `buildit`. No agent: seven command tasks replay a
fixed trace of source edits against one vLLM commit, one build each, then `report` writes
`report.json` and prints the summary.

| step | edits | mode |
| --- | --- | --- |
| `clean` | none | derive |
| `leaf` | `csrc/layernorm_kernels.cu` | full |
| `leaf_py` | `leaf` + `vllm/logger.py` | full, same compiled sources as `leaf` |
| `revert` | none | derive, same compiled sources as `clean` |
| `leaf_again` | `leaf` | full, same tree as `leaf` |
| `header` | `csrc/dispatch_utils.h` (28 TUs) | full |
| `cmake` | `CMakeLists.txt` | full |

A step builds `derive` when its compiled sources (vLLM's `WHEEL_BLOCKING_PATHS` and
`WHEEL_WARNING_PATHS`) match the clean tree, `full` otherwise. Each C++ edit adds a variable whose
string the compiled extension keeps; the build greps the extensions and edited files for markers,
and a step fails unless they are exactly its edits.

## The build

`bench/Containerfile` rebuilds the vLLM sandbox base on `base_image` (CUDA dev packages, vLLM at
`vllm_ref` installed editable from its precompiled wheel, and
[vllm-envs](https://github.com/neuralmagic/vllm-envs)), copies the step's tree over it, and runs
`bench/informer.py build`, a build informer on vllm-envs:

- `informer.py caches` names the caches a build declares; the replay passes them to buildit as
  `--cache NAME=PATH`, so the Containerfile mounts nothing;
- vllm-envs keys the tree's compiled outputs (compiled sources, toolchain, Python ABI, GPU target,
  build env) and clones its pinned external sources once per pin, passed as `*_SRC_DIR`;
- a build whose key is in the build store restores those outputs and compiles nothing;
- otherwise `setup.py build_ext --inplace` runs through ccache in a CMake build dir that, with
  FetchContent sources and the kernels CMake generates into `csrc`, persists between builds; a
  manifest of source hashes touches what changed, so ninja follows content, not clocks;
- the informer writes what the caches held to `/buildit/facts.json`, which buildit returns as the
  build's `facts`.

Declared caches last as long as the builder pod. buildit runs a sandbox's next build in its idle
pod, so from the second step on the base step is a layer cache hit and the caches are warm. When
buildit names a shared cache in `/buildit/cache.json`, ccache and the build store also read from
it, and write to it when the build holds its token: a build in another pod, or another run,
starts with the objects and outputs others stored (`store` is then `remote-hit`, and
`ccache_remote_hits` counts objects ccache fetched).

Each record splits the build's wall time:

| field | span |
| --- | --- |
| `before_base_s` | submit, builder pod start, context upload, base image pull |
| `base_s` | the sandbox base, which a prebuilt base image already holds; 0 when cached |
| `copy_s` | committing the base layer, copying the tree |
| `setup_s` | everything before the compile but the base step |
| `compile_s` | the extension build, or restoring it from the build store |
| `after_compile_s` | committing the image, status poll |
| `candidate_s` | `wall_s - base_s`: the build on a prebuilt base |
| `push_s` | `buildit push`, when `push` is set |
| `arch`, `cpus` | the builder pod's machine and visible CPUs |
| `base_cached` | the base step came from buildah's layer cache |

and carries the informer's facts:

| fact | meaning |
| --- | --- |
| `store` | `hit`, `remote-hit` (from the shared cache), `miss` (compiled and stored) or `off` (derive) |
| `shared` | the shared cache: `read-write`, `read-only` or `off` |
| `published` | after a miss: `stored` in the shared cache, `read-only`, or why the upload failed |
| `key` | vllm-envs' build key, with the base image folded in |
| `ccache_hits`, `ccache_misses` | ccache's counts for the compile |
| `ccache_remote_hits` | hits served from the shared cache |
| `configure_s` | CMake's configure time, from its log |
| `touched` | sources whose content changed since the last compile in the kept build dir |
| `outputs` | files stored under `key` |
| `src_dirs` | external projects built from vllm-envs' pinned sources |
| `build_s` | the informer's own wall time |

`before_base_s` and `after_compile_s` subtract builder-pod timestamps from loop-pod ones; with a
cached base step, `before_base_s` and `copy_s` are null.

## Params

| param | default |
| --- | --- |
| `base_image` | `vllm/vllm-openai:v0.32.0` by digest |
| `vllm_ref` | `5df63a72`, the commit `base_image` was built from |
| `arch_list` | `10.0` |
| `max_jobs` | `16` |
| `push` | `true` |
| `timeout_s` | `3600`, buildit's cap |

## Running it

The run needs a buildit grant, with a push repository when `push` is set, whose builder pods are
amd64, sized for `max_jobs` nvcc jobs, and reach docker.io, github.com, pypi.org, wheels.vllm.ai,
and the Ubuntu and NVIDIA apt repositories.

```sh
crux draft-create vllm-build-bench --description "time vLLM candidate builds"
crux draft-push vllm-build-bench examples/vllm-build-bench --base-version <version>
crux draft-launch vllm-build-bench --max-cost 1 --max-time 720m
```

`just vllm-build-bench-test` runs the replay and the informer against real git checkouts and a
C++ compiler, and compiles the pack.
