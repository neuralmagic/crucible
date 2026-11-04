# vllm-agent-build

An agent checks out vLLM, builds it through the run's `buildit` from a plain Dockerfile, edits
`csrc/layernorm_kernels.cu`, and builds again. The `trace` task then renders each build as an html
trace, which the run page shows under its captured files.

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
that allows overlay mounts in a user namespace (5.13 and later).

## The pack

- `build/Dockerfile` is an ordinary vLLM source build: CUDA dev packages and the build
  requirements on a vLLM release image, then the agent's `build/vllm/` checkout compiled in place
  with `setup.py build_ext --inplace`. Nothing in it knows about buildit or caching. `build/` is the
  build context: buildit ships a subdirectory of the workspace, never the workspace root.
- The agent builds it once as checked out, then edits one CUDA kernel at a time and builds again,
  three times. Every `build` call names the same buildit caches: ccache's directory and vLLM's
  FetchContent sources (`.deps`). buildit keeps them in the sandbox's builder pod between builds,
  so each build configures afresh and ccache serves every object an edit did not touch. The
  CMake build dir is not cached: configure writes generated kernels such as Marlin's
  `kernel_selector.h` into the source tree, which every build replaces, and a cached build dir
  records them as already generated.
- The calls also set `ccache: true`. buildit then mounts a generated `/etc/ccache.conf` into every
  `RUN` step: the shared cache as ccache's remote storage, so a first build in a new pod starts
  from objects other runs compiled, and a per-object log at `/buildit/ccache-stats.log`.
- After each build the agent copies the ninja log out of the build dir, and that ccache log and the
  time buildit stamped on each Dockerfile step out of `/buildit`, with buildit's `run`, which sees
  them read-only, and declares them as its files.
  `trace/render.py` turns each pair into `traces/<build>.html` and writes `traces/index.html`
  comparing the builds. The run page shows them under its captured files.
- The checkout's `.git` stays in the build context, since `setup.py` takes vLLM's version from it.
  `.gitignore` keeps the checkout out of the run's workspace history.
- The sandbox needs `git` and egress to github.com to check vLLM out:
  `[agent.openshell] binaries = ["/usr/bin/git", "/usr/lib/git-core/git-remote-https"]`.

`trace/buildtrace.py` and `trace/trace.html` are copies of the ones in
`examples/vllm-build-bench/bench`; a test keeps them identical.
