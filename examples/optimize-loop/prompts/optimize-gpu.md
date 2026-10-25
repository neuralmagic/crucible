You are optimizing a slow PyTorch attention kernel by measuring it on a GPU in a Kubernetes
cluster. You have two MCP servers. `buildit` builds container images from your workspace and
pushes them. `kueue` submits Kubernetes workloads through Kueue in the one namespace this run may
use. You have no kubectl, no registry credentials, no cluster credentials and no GPU in your own
shell; use only their tools and your shell.

## The program

`gpu/` in your workspace holds the program:

- `gpu/bench.py` is the benchmark harness. Do not edit it. It builds fixed float32 inputs of
  shape (2, 8, 512, 64) on the GPU, runs `kernel.attention(q, k, v)` five times, compares each
  output with a float64 reference it computes itself, and prints one line starting
  `optimize-loop-result ` followed by JSON with `ops_per_s`, `p50_ms`, `total_ms` and
  `checksum`. The checksum is the same for every correct kernel; a kernel off by more than
  2e-3 anywhere prints `mismatch`.
- `gpu/kernel.py` holds `attention`. It is correct and slow. This is the file you improve. It
  must return float32 or wider with the input's shape. Lower-precision math that misses the
  tolerance counts as wrong.
- `gpu/Containerfile` builds it on a PyTorch CUDA image. Do not edit it.

## Pick the hardware

Call kueue `whoami`, `queues_list` and `hardware_describe` once, at the start. Pick a LocalQueue
whose flavors offer NVIDIA GPUs, and one GPU product. When `hardware_describe` shows the flavor
spans several GPU products, pin the product with a `nodeSelector` on its
`nvidia.com/gpu.product` label in every Job, so each round runs on the same kind of GPU. Say
which product you picked in the report summary.

## Each measurement

1. Compute a short tag for this run in the shell: the first 6 hex characters of the SHA-256 of
   the `run_id` whoami returns. Every Job you create is named `ol-<tag>-r<round>-<label>`, with
   `<label>` a short lowercase word naming the change, and every name is new.
2. Build: call buildit `build` with the context directory (`gpu`, or a variant directory) and
   dockerfile `Containerfile`. Poll buildit `status` until it succeeds or fails, running
   `sleep 15` between polls. On failure read buildit `logs` and fix the code.
3. Call buildit `push` with the build_id. Keep the `image` it returns exactly as returned.
4. Call kueue `workload_plan` with the queue and this batch/v1 Job, then `workload_submit` with
   the same queue, manifest and `plan_hash`. Replace `NAME`, `IMAGE` and `LABEL`, and add the
   GPU product `nodeSelector` if you need one. Change nothing else.

   ```json
   {
     "apiVersion": "batch/v1",
     "kind": "Job",
     "metadata": {"name": "NAME"},
     "spec": {
       "backoffLimit": 0,
       "ttlSecondsAfterFinished": 7200,
       "template": {"spec": {
         "restartPolicy": "Never",
         "nodeSelector": {"kubernetes.io/arch": "amd64"},
         "containers": [{
           "name": "bench",
           "image": "IMAGE",
           "env": [{"name": "BENCH_VARIANT", "value": "LABEL"}],
           "resources": {
             "requests": {"cpu": "4", "memory": "16Gi", "nvidia.com/gpu": "1"},
             "limits": {"cpu": "4", "memory": "16Gi", "nvidia.com/gpu": "1"}
           }
         }]
       }}
     }
   }
   ```

5. Call kueue `workload_status` with kind `Job`, the name, and `until` `finished`, again until
   it reports the Job finished. A Job may queue for GPU quota; give up on one after 30 minutes.
6. Call kueue `workload_logs` with kind `Job`, the name, and grep `optimize-loop-result`.
7. Call buildit `clean` with the build_id once the Job has finished.

## The loop

- Round 0: measure `gpu/` unchanged. That is the baseline.
- Rounds 1 to N, where N is the round limit given at the end of this prompt: change
  `attention`, measure again. Stop early once a round brings no further gain.
- In at least one round, try two or three different ideas at the same time: copy `gpu/` to
  `variants/<label>/` for each (copy `bench.py` and `Containerfile` unchanged), edit only each
  copy's `kernel.py`, build and push all of them, then submit all their Jobs before you wait on
  any of them.
- After the last round, leave the fastest correct `attention` in `gpu/kernel.py`.

## The report

Write `report.json` in the workspace root:

```json
{
  "jobs": [
    {"round": 0, "job": "<Job name>", "image": "<repository@sha256:...>",
     "checksum": "<from its log>", "ops_per_s": <from its log>, "p50_ms": <from its log>,
     "change": "<one line: what this variant changed>"}
  ],
  "best_job": "<the name of the fastest Job whose checksum is the baseline's>",
  "speedup": <its ops_per_s divided by the baseline's>,
  "summary": "<the GPU product, and two or three sentences on what made it faster>"
}
```

List every Job you submitted that finished, the baseline as round 0, with numbers copied from
its log line. A separate verifier reads each Job's log and each image from the registry itself
and fails the run on any number that does not match, so report what the logs say.

Then write `PLAN_TASK_RESULT.json` holding only `{"report": "report.json"}`.
