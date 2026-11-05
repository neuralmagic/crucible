# vllm-agent-build

A `seed` command builds vLLM at `vllm_ref` once through the run's own `buildit`. Agents then fan
out over CUDA kernels, one per kernel in `kernels`, at most eight at a time. Each builds the same
ref as checked out, then edits its kernel and builds again, three times. The `trace` task renders
every build into `traces/index.html`, which the run page shows under its captured files.

## The deployment

A run gets `buildit` when it is dispatched to one of the profile's `[buildit] clusters`. Three
settings decide how fast its vLLM builds are:

```toml
[buildit]
clusters  = ["spoke-a"]
namespace = "crucible-builds"

[buildit.cache]                       # ccache objects shared across pods and runs
cluster      = "spoke-a"
token_secret = "buildit-cache-token"

[buildit.cluster_builders.spoke-a]    # layers stored once instead of copied per layer
storage_driver = "overlay"
```

`[buildit.cache]` needs its token Secret in the loop namespace and `crucible deploy
--buildit-cache` applied on the cluster (see `docs/controller-deploy.md`). `overlay` needs a kernel
that allows overlay mounts in a user namespace (5.13 and later). Launch on a cluster the cache
serves: a run elsewhere builds without it.

With `writers = "run"`, the default, only the run's own builds write the shared cache: the ones
`seed` starts on the run's socket. Agents' builds only read it. So the seed fills the cache from a
known ref and a known Dockerfile, every agent's `checkout` build is served from it, and an agent's
`edit` compiles its one changed object and stores nothing.

## The pack

- `build/Dockerfile` is an ordinary vLLM source build: CUDA dev packages and the build
  requirements on a vLLM release image, then the `build/vllm/` checkout compiled in place with
  `setup.py build_ext --inplace`. Nothing in it knows about buildit or caching. `build/` is the
  build context: buildit ships a subdirectory of the workspace, never the workspace root.
- `seed.py` reads its parameters from `$CRUCIBLE_INPUTS_FILE`. It checks vLLM out into
  `build/vllm/` and stops if a name in `kernels` has no `csrc/<name>_kernels.cu`. It then runs
  `buildit build` and `buildit run` on the socket with the same arguments the agents pass, and
  copies the build's logs to `traces/seed.*`. Its last stdout line emits `kernels`, the list
  `edit` maps over.
- Every build names the same buildit caches: ccache's directory and vLLM's FetchContent sources
  (`.deps`). buildit keeps them in the caller's builder pod between builds. `.deps` is also a
  cluster cache, keyed by `CMakeLists.txt` and `cmake/external_projects`, where vLLM pins every
  dependency: the seed saves it, and each agent's new pod restores it instead of cloning about
  4 GB from GitHub. The CMake build dir is not cached: configure writes generated kernels such as
  Marlin's `kernel_selector.h` into the source tree, which every build replaces, and a cached
  build dir records them as already generated.
- Every build also sets `ccache: true`. buildit then mounts a generated `/etc/ccache.conf` into
  every `RUN` step, with the shared cache as ccache's remote storage, and logs each object to
  `/buildit/ccache-stats.log`.
- `edit` is `isolated`, so each instance checks vLLM out in a worktree of its own. Its
  `required = False` lets the sweep finish when one kernel fails. After each build an instance
  copies the ninja log, that ccache log and the time buildit stamped on each Dockerfile step out of
  the build with buildit's `run`, and declares them as its files.
- `trace/render.py` renders the seed from `inputs/seed/` and each instance from
  `inputs/edit[<kernel>]/`. It writes one table of every build's time and ccache results, with
  each build's trace page opening in place, since a join task cannot declare a file per instance.
- The checkout's `.git` stays in the build context, since `setup.py` takes vLLM's version from it.
  `.gitignore` keeps the checkout out of the run's workspace history.
- The sandbox needs `git` and egress to github.com to check vLLM out:
  `[agent.openshell] binaries = ["/usr/bin/git", "/usr/lib/git-core/git-remote-https"]`.

`trace/buildtrace.py` and `trace/trace.html` are copies of the ones in
`examples/vllm-build-bench/bench`; a test keeps them identical.
