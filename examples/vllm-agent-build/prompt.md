Build vLLM from source through the `buildit` MCP tools: once as checked out, then once after a
small edit to one CUDA kernel. Your task inputs carry `item`: the kernel to edit. Fetch each
build's ninja and ccache logs and report what each took.

If the buildit tools are not available to you, write `report.json` with an `error` saying so and
stop. Do not look for credentials, tokens or other ways in.

Every build uses the same arguments: context `build`, dockerfile `Dockerfile`, `ccache` true,
`timeout_s` 3600, build_args `BASE`, `CUDA_ARCH_LIST` and `MAX_JOBS` from below, and these
caches, unchanged:

    [{"name": "vllm-ccache", "path": "/root/.cache/ccache"},
     {"name": "vllm-deps", "path": "/src/vllm/.deps"}]

After calling `build`, poll `status` every 30 seconds until it is no longer running. If a build
fails, read the end of its log with `logs` and stop. When one succeeds, call `run` with its
build_id, cmd
`["sh", "-c", "mkdir -p /trace && cp /src/vllm/build/temp.*/.ninja_log /trace/ninja_log && cp /buildit/ccache-stats.log /buildit/steps.log /trace/"]`
and fetch_paths `["/trace"]`, then copy the three fetched files to `traces/<name>.ninja_log`,
`traces/<name>.ccache-stats.log` and `traces/<name>.steps.log`.

1. Check out vLLM at `vllm_ref` (below) into `build/vllm/`:
   `git init -q build/vllm && git -C build/vllm fetch -q --depth 1 https://github.com/vllm-project/vllm.git <vllm_ref> && git -C build/vllm checkout -q --detach FETCH_HEAD`
   Build it as `checkout`.
2. Append this line to `build/vllm/csrc/<item>_kernels.cu`, then build as `edit`:
   `namespace crucible_smoke { [[maybe_unused]] __attribute__((used)) inline const char marker[] = "crucible-smoke-<item>"; }`
3. Write `report.json`:
   `{"kernel": <item>, "builds": [{"name": "checkout" | "edit", "build_id": ..., "status": ..., "seconds": <from the build call to the last status>}]}`

Do not change any other file and do not call `clean`.
