Make vLLM's fused MoE path faster on an NVIDIA A100 without changing its output, by hill
climbing: propose an edit, build it, measure it on a GPU, keep it only if it is faster, repeat.

You have two MCP servers. `buildit` builds container images from your workspace and pushes them.
`kueue` submits Kubernetes Jobs through Kueue in the one namespace this run may use. You have no
GPU, no kubectl and no cluster credentials; use only those tools and your shell. If either server
is missing, write `report.json` with an `error` saying so and stop. Do not look for credentials.

## What is measured

The seed task built vLLM at `vllm_ref` (below) and pushed it as `seed_image`. Your task inputs
carry it, and `seed_base`: the same image in the registry builds pull from. The image holds
`/bench/moe_bench.py`, which you cannot change. At Qwen3-30B-A3B's
shapes (128 experts, top 8, hidden 2048, intermediate 768, bf16) and token counts 1 to 8192, it
runs `fused_topk` then `fused_experts`, checks the output against a float32 reference, times both
replayed from a CUDA graph, and prints one line:

    hill-climb-result {"ok": true, "geomean_us": 123.4, "us": {"1": ..., "8192": ...}, ...}

Lower `geomean_us` is better. A candidate with `"ok": false` is wrong and never kept.

What you may edit, under `build/vllm/`: anything on that path, chiefly
`vllm/model_executor/layers/fused_moe/` (the Triton kernel in `fused_moe.py`, its launch
heuristics in `get_default_config`, tuned configs in `configs/`) and `csrc/moe/` (the CUDA
routing, alignment and sum kernels). There is no tuned config for these shapes on
`NVIDIA_A100-SXM4-80GB`; the file name is `E=128,N=768,device_name=NVIDIA_A100-SXM4-80GB.json`.
Python and JSON edits build in seconds; CUDA edits recompile and take minutes, so prefer the
former unless the profile says otherwise.

## Setup

    python3 hc.py checkout <vllm_ref>

Then call kueue `whoami` once and take a short tag: the first 6 hex characters of the SHA-256 of
its `run_id`. Every Job is named `hc-<tag>-<label>`, and every label is new.

## One candidate

`<label>` is a short lowercase word naming the change (`baseline` for the seed itself).

1. Edit files under `build/vllm/`, then `python3 hc.py stage <label> <seed_base>`. It prints
   `recipe` and `build`, the arguments for buildit `build`. For `recipe` `dockerfile`, add
   build_args `BASE`, `CUDA_ARCH_LIST`, `MAX_JOBS` and `SERVE_MODEL` from below, `timeout_s`
   3600, and these caches, unchanged:

       [{"name": "vllm-ccache", "path": "/root/.cache/ccache"},
        {"name": "vllm-deps", "path": "/src/vllm/.deps", "scope": "cluster",
         "key": ["vllm/CMakeLists.txt", "vllm/cmake/external_projects"]}]

2. Call buildit `build` with exactly those arguments. Once it returns, run `python3 hc.py reset`
   so the tree is the best kept one again; the build already has its copy.
3. Poll buildit `status` with `sleep 10` between polls until it ends. On failure read buildit
   `logs` and drop the candidate. On success call buildit `push` and keep the `image` it returns
   exactly. Then `python3 hc.py mark <label> pushed`.
4. Call kueue `workload_plan` with queue `QUEUE` (below) and this Job, then `workload_submit` with
   the same queue, manifest and `plan_hash`. Replace `NAME`, `IMAGE` and `LABEL`; change nothing
   else. Then `python3 hc.py mark <label> submitted`.

   ```json
   {"apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": "NAME"},
    "spec": {"backoffLimit": 0, "ttlSecondsAfterFinished": 7200, "activeDeadlineSeconds": 1800,
     "template": {"spec": {
      "restartPolicy": "Never",
      "nodeSelector": {"nvidia.com/gpu.product": "NVIDIA-A100-SXM4-80GB"},
      "volumes": [{"name": "shm", "emptyDir": {"medium": "Memory", "sizeLimit": "8Gi"}},
                  {"name": "tmp", "emptyDir": {}}],
      "containers": [{"name": "bench", "image": "IMAGE",
       "command": ["python3", "/bench/moe_bench.py"],
       "env": [{"name": "BENCH_LABEL", "value": "LABEL"}],
       "volumeMounts": [{"name": "shm", "mountPath": "/dev/shm"},
                        {"name": "tmp", "mountPath": "/tmp"}],
       "resources": {"requests": {"cpu": "8", "memory": "48Gi", "nvidia.com/gpu": "1"},
                     "limits": {"cpu": "8", "memory": "48Gi", "nvidia.com/gpu": "1"}}}]}}}}
   ```

5. Call kueue `workload_status` with kind `Job`, the name and `until` `finished` until it reports
   the Job finished; give up on one after 30 minutes. Call kueue `workload_logs` with kind `Job`,
   the name and grep `hill-climb-result`, and write the line it returns, unchanged, to
   `evidence/<label>.log`. Then `python3 hc.py mark <label> measured`.

## The climb

- Round 0: stage `baseline` with no edits by building `seed_image` directly: skip `stage` and
  `build`, use `seed_image` as the image in step 4, and mark `baseline staged` when you start.
- Rounds 1 to N (N is below): propose two or three different edits on top of the best kept tree,
  build all of them, then submit all their Jobs before waiting on any. Measure them all, then
  `python3 hc.py keep <label>` for the fastest correct one if it beats the best `geomean_us` so
  far by at least 1%. Stop early after a round in which nothing beat it.
- Read `fused_moe.py` and the configs for nearby shapes before the first edit. Tune from evidence:
  each result line has the per-token-count times, so say which counts a change is meant to move.

## Serving check

If something was kept, measure the best kept candidate and the seed once more under real serving:
submit the same Job as in step 4 for each, named `hc-<tag>-serve-best` and
`hc-<tag>-serve-seed`, with `"command": ["sh", "/bench/serve_bench.sh"]`, memory `96Gi`, and
`activeDeadlineSeconds` 2400. Grep their logs for `hill-climb-serve`, write the lines to
`evidence/serve-best.log` and `evidence/serve-seed.log`, and run `python3 hc.py mark serve-best
measured` and `python3 hc.py mark serve-seed measured`.

## The report

Write `report.json`:

```json
{"best": "<the kept label, or baseline>",
 "candidates": [{"label": "...", "round": 0, "recipe": "seed" | "layer" | "dockerfile",
                 "build_id": "<or null for baseline>", "image": "<the pushed image>",
                 "job": "<Job name>", "change": "<one line: what this edit did and why>"}],
 "summary": "<two or three sentences on what made it faster, and what did not>"}
```

List every candidate you built, including ones that failed to build or measured wrong. Then
write `PLAN_TASK_RESULT.json` holding only `{"report": "report.json"}`.
