# optimize-loop

An agent makes a slow program fast by measuring it on a cluster: it builds the program with the
run's `buildit`, runs a benchmark as Kueue Jobs with fixed CPU and memory through `crucible-kueue`,
reads the logs, edits, and goes again, trying several variants as parallel Jobs in at least one
round. A deterministic verifier then checks its report against evidence and passes the run only
above a speedup it computes itself.

| param | default | meaning |
| --- | --- | --- |
| `gpu` | `false` | `true` tunes a PyTorch attention kernel on a GPU flavor the agent picks with `hardware_describe` |
| `rounds` | `3` | optimization rounds after the baseline, 1 to 5 |
| `min_speedup` | `5` | the speedup over the baseline the verifier requires |

## The program

- `app/` (CPU): `main.go` is the harness, `topk.go` counts the top users of 200000 access records
  with a linear scan per record and a bubble sort. Correct, and about 100x slower than it needs to
  be. The harness seeds its input from the pod's hostname, runs `TopUsers` five times on a fresh
  copy, and prints one `optimize-loop-result {...}` line: `ops_per_s`, `p50_ms`, `total_ms`, the
  answer's `checksum`, and the SHA-256 of its own executable.
- `gpu/` (GPU): `bench.py` is the harness, `kernel.py` computes attention one query row at a
  time. The harness checks the kernel against a float64 reference and prints the reference's
  checksum, or `mismatch`.

The harnesses are frozen injects; the agent edits `topk.go` or `kernel.py`.

## The graph

```text
optimize (agent: build, run, measure, iterate; writes report.json)
  -> gate (command: report.json is well formed)
     -> collect (agent: copies crucible-kueue's replies into evidence/kueue.json)
        -> verify (evaluate: report.json against the evidence and the registry)
```

`collect` depends on `gate`, not on `optimize`, so nothing the optimizer wrote reaches its prompt.
It calls `workload_list`, which lists only this run's workloads, then `workload_status` and
`workload_logs` for each Job, and writes the replies as they came.

## What the verifier checks

`verify/verify.py` runs in the loop pod. Pack code there has no Kubernetes rights in the workload
namespace and no kueue token, so it cannot read Job logs itself; crucible-kueue's replies reach it
through `collect`. For each Job in the report it requires:

- the Job is in the evidence, belongs to this run (its `crucible.dev/run-id` is the loop pod's
  name) and succeeded;
- its log has exactly one result line, for the pod crucible-kueue says ran it, over the fixed
  workload, with the seed that pod's name gives;
- the report's checksum, `ops_per_s` and `p50_ms` match that line;
- its image is `<push repo>@sha256:...`, the push repository of the run's buildit grant, and the
  registry serves that digest; on CPU, the image's `/bench` hashes to the `exe_sha256` the Job
  printed, so the measurement came from that image;
- the log claims no more work time than the Job lasted.

Then it recomputes each CPU checksum from the pod name with its own reference implementation (GPU:
each must equal the baseline's), drops wrong answers, picks the fastest correct candidate itself,
and computes the speedup. The run fails when the report's best or speedup disagrees, when no
round ran two Jobs at once, when the best Job's wall time does not shrink with the time its log
claims to save, when the harness the agent built from differs from the pack's copy, or when the
speedup is under `min_speedup`.

It reads the registry with the loop pod's push authfile (`FORGE_AUTHFILE`), read only. What it
cannot rule out is a program built to lie about its own timings; the salted seed, the binary hash
and the wall-clock check make that cost more than the optimization.

## Run it

```sh
just optimize-loop-test                     # the verifier's tests and the pack's compile
just optimize-loop <context> <controller namespace> [gpu] [rounds] [min_speedup]
just optimize-loop-e2e-pirate <loop image> <kueue image> <controller image>
```

`just optimize-loop` launches the pack as a draft on a running controller whose runs are granted
`buildit` with a push repository and `crucible-kueue`, and waits for the verdict. Its Jobs request
1 CPU and 256Mi, limited alike, on `amd64` nodes. The GPU variant needs a queue with NVIDIA GPU
flavors and pulls a PyTorch CUDA base image; it has not been run live.
