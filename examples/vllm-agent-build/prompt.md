Build vLLM from source through the `buildit` MCP tools: once as checked out, then three times,
each after a small edit to a different CUDA kernel, the way a kernel author iterates. Fetch each
build's ninja and ccache logs and report what each took.

If the buildit tools are not available to you, write `report.json` with an `error` saying so and
stop. Do not look for credentials, tokens or other ways in.

Every build uses the same arguments: context `.`, dockerfile `build/Dockerfile`, `ccache` true,
`timeout_s` 3600, build_args `BASE`, `TORCH_CUDA_ARCH_LIST` and `MAX_JOBS` from below, and these
caches, unchanged:

    [{"name": "vllm-ccache", "path": "/root/.cache/ccache"},
     {"name": "vllm-deps", "path": "/src/vllm/.deps"},
     {"name": "vllm-build", "path": "/src/vllm/build"}]

After calling `build`, poll `status` every 30 seconds until it is no longer running. If a build
fails, read the end of its log with `logs` and stop. When one succeeds, call `run` with its
build_id, cmd
`["sh", "-c", "mkdir -p /trace && cp /src/vllm/build/temp.*/.ninja_log /trace/ninja_log && cp /buildit/ccache-stats.log /trace/ccache-stats.log"]`
and fetch_paths `["/trace"]`, then copy the two fetched files to `traces/<name>.ninja_log` and
`traces/<name>.ccache-stats.log`.

1. Check out vLLM at `vllm_ref` (below) into `vllm/`:
   `git init -q vllm && git -C vllm fetch -q --depth 1 https://github.com/vllm-project/vllm.git <vllm_ref> && git -C vllm checkout -q --detach FETCH_HEAD`
   Build it as `checkout`.
2. For N in 1, 2, 3, append this line to the file below, then build as `editN`:
   `namespace crucible_smoke { [[maybe_unused]] __attribute__((used)) inline const char marker_N[] = "crucible-smoke-N"; }`
   - N=1: `vllm/csrc/layernorm_kernels.cu`
   - N=2: `vllm/csrc/activation_kernels.cu`
   - N=3: `vllm/csrc/layernorm_kernels.cu` again
3. Write `report.json`:
   `{"builds": [{"name": "checkout" | "edit1" | "edit2" | "edit3", "build_id": ..., "status": ..., "seconds": <from the build call to the last status>}]}`

Do not change any other file and do not call `clean`.
