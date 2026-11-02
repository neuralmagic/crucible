# Example: build, run, measure, iterate

`examples/optimize-loop` is a playbook in which a sandboxed agent optimizes a program against
real measurements on a cluster, and a verifier checks the result against evidence rather than
the agent's word. It uses the two MCP servers a run can be granted with agent tokens (see
[Agent and people MCP architecture](../agent-mcp-architecture.md)):

- `buildit` builds the agent's workspace into an image in a builder pod and pushes it to the run's
  push repository by digest;
- `crucible-kueue` submits each benchmark as a Kueue Job with fixed CPU and memory, waits on it,
  and reads its logs.

The agent measures a baseline, changes the slow function, rebuilds and reruns for up to `rounds`
rounds, and runs two or three variants as parallel Jobs in at least one round. It reports each
round's Job name, image digest and numbers.

## How the result is checked

The loop pod, where the verifier runs, holds no kueue token and no rights in the workload
namespace. A second agent task, `collect`, reads crucible-kueue for every Job this run created
and writes the replies to a file; its prompt carries nothing the optimizer wrote. The `verify`
evaluate task then checks the report against those replies, against the registry (read with the
loop pod's push authfile), and against its own reference answer, computes the speedup itself, and
passes only above `min_speedup`. The pack's README lists every check.

## Where the numbers show up

`verify` prints its verdict as its result: `pass`, `speedup` (also its `score`), `baseline` and
`best` (each a Job, its image and the ops/s the evidence gave), `rounds` (each round's Jobs with
their measured ops/s and p50, or why one was rejected, and the round's best), and any `failures`.
A failing verdict keeps everything it measured. The controller stores it as the task's output on
the run's task results, so it outlives the loop pod:

- `crux graph <run>` prints it under the task table, and `crux graph <run> --json` returns each
  attempt's whole `output`;
- the run view's `verify` task panel shows it as the task's payload;
- `just optimize-loop` and `just optimize-loop-e2e` print it when the run ends, pass or fail.

## Launch it

```sh
just optimize-loop-test
just optimize-loop <kube context> <controller namespace>            # CPU, 3 rounds, 5x
just optimize-loop <kube context> <controller namespace> true 2 3   # GPU, 2 rounds, 3x
```

The controller's profile needs `[buildit]` with a `push_repo`, `[secrets] push_authfile`, and
`[kueue]` with that repository among its pull secrets. The GPU variant also needs a LocalQueue
with NVIDIA GPU flavors.
