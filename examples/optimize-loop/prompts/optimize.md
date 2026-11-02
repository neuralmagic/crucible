You are optimizing a slow Go program by measuring it on a Kubernetes cluster. You have two MCP
servers. `buildit` builds container images from your workspace and pushes them. `kueue` submits
Kubernetes workloads through Kueue in the one namespace this run may use. You have no kubectl,
no registry credentials and no cluster credentials; use only their tools and your shell.

## The program

`app/` in your workspace holds the program:

- `app/main.go` is the benchmark harness. Do not edit it. It generates 200000 access records
  from a seed derived from the pod's hostname, calls `TopUsers` on a fresh copy five times, and
  prints one line starting `optimize-loop-result ` followed by JSON with `ops_per_s`, `p50_ms`,
  `total_ms`, `checksum` (a hash of the answer) and `exe_sha256`.
- `app/topk.go` holds `TopUsers`. It is correct and slow. This is the file you improve. Keep its
  answer identical: the same users, counts, bytes and order, ties included.
- `app/Containerfile` and `app/go.mod` build it. Do not edit them.

You may run `go test`, `go vet` or the program itself in your shell to check an idea, but only
numbers from Kueue Jobs count.

## Each measurement

1. Call kueue `whoami` and `queues_list`, once, at the start. Use the LocalQueue they report.
   Compute a short tag for this run in the shell: the first 6 hex characters of the SHA-256 of
   the `run_id` whoami returns (`printf %s "$RUN_ID" | sha256sum | cut -c1-6`). Every Job you
   create is named `ol-<tag>-r<round>-<label>`, with `<label>` a short lowercase word naming the
   change, and every name is new.
2. Build: call buildit `build` with the context directory (`app`, or a variant directory, see
   below) and dockerfile `Containerfile`. Poll buildit `status` for its build_id until it
   succeeds or fails, running `sleep 10` in the shell between polls. On failure read buildit
   `logs`, fix the Go code, and build again.
3. Call buildit `push` with the build_id. Keep the `image` it returns, a
   `repository@sha256:...` reference, exactly as returned.
4. Call kueue `workload_plan` with the queue and this batch/v1 Job, then `workload_submit` with
   the same queue, the same manifest and the plan's `plan_hash`. Replace `NAME`, `IMAGE` and
   `LABEL`. Do not change anything else: the CPU and memory requests and limits are fixed so
   every round is measured on the same budget.

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
             "requests": {"cpu": "1", "memory": "256Mi"},
             "limits": {"cpu": "1", "memory": "256Mi"}
           }
         }]
       }}
     }
   }
   ```

5. Call kueue `workload_status` with kind `Job`, the name, and `until` `finished`, again until
   it reports the Job finished. Give up on a Job after 15 minutes.
6. Call kueue `workload_logs` with kind `Job`, the name, and grep `optimize-loop-result`. The
   JSON after the prefix is the measurement. A Job that failed, or printed no such line, measured
   nothing.
7. Call buildit `clean` with the build_id once the Job has finished.

## The loop

- Round 0: measure `app/` unchanged. That is the baseline.
- Rounds 1 to N, where N is the round limit given at the end of this prompt: read the last
  measurements, change `TopUsers`, measure again. Stop early once a round brings no further
  gain.
- In at least one round, try two or three different ideas at the same time: copy `app/` to
  `variants/<label>/` for each (copy `main.go`, `go.mod` and `Containerfile` unchanged), edit only
  each copy's `topk.go`, build and push all of them, then submit all their Jobs before you wait
  on any of them, so they run in parallel.
- After the last round, leave the fastest correct `TopUsers` in `app/topk.go`.

The seed comes from the pod's hostname, so every Job prints a different checksum and the
verifier recomputes each one from its pod's name. Check a change before you build it: run the
original and the changed program in your shell (`go run .` in each directory, with
`BENCH_RECORDS=20000` for speed); on the same host they read the same seed, so their checksums
must be equal. A variant with a wrong answer is not counted, whatever its speed.

## The report

Write `report.json` in the workspace root:

```json
{
  "jobs": [
    {"round": 0, "job": "<Job name>", "image": "<repository@sha256:...>",
     "checksum": "<from its log>", "ops_per_s": <from its log>, "p50_ms": <from its log>,
     "change": "<one line: what this variant changed>"}
  ],
  "best_job": "<the name of the fastest correct Job>",
  "speedup": <its ops_per_s divided by the baseline's>,
  "summary": "<two or three sentences on what made it faster>"
}
```

List every Job you submitted that finished, the baseline as round 0, with numbers copied from
its log line. A separate verifier reads each Job's log and each image from the registry itself
and fails the run on any number that does not match, so report what the logs say.

Then write `PLAN_TASK_RESULT.json` holding only `{"report": "report.json"}`.
