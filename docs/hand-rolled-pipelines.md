# Hand-rolled pipelines

How to drive the broker's code-gen GPU tools directly, without the controller, a scenario, or
the loop. Use this to validate a new domain's tool contract, smoke-test a substrate, or debug.
The production path is a scenario through the loop, and everything below is what the loop
normally does for you.

## Overview

An agent (or a shell with curl) talks MCP-over-HTTP to a broker. The broker exposes four
digest-keyed tools: `codegen_build` turns the current workspace tree into a pinned candidate
image; `codegen_benchmark`, `codegen_lm_eval`, and `codegen_profile` run frozen commands
against a digest as queue-admitted GPU Jobs and return metrics or trace handles. The agent has
no cluster credentials and does not write job specs. It can only vary the kwargs the config
declares (`mutable_kwargs`). A measurement maps to exactly one source state: the tools reject
digests the broker didn't build and refuse to build a tree they can't hash.

## Topology

Three pods/volumes, one queue:

```
┌────────────────┐   MCP/HTTP    ┌────────────────┐   batch/v1 Jobs   ┌──────────────┐
│ driver pod     │──────────────▶│ broker pod     │──────────────────▶│ GPU job      │
│ (your pipeline │               │ (crucible-     │   (Kueue queue)   │ (frozen cmd, │
│  or a shell)   │               │  broker)       │                   │  digest img) │
└──────┬─────────┘               └──────┬─────────┘                   └──────┬───────┘
       │        workspace PVC (RWX)     │            model-cache PVC (RO)   │
       └───────────────/workspace───────┘            artifacts PVC (profile only)
```

- **Workspace PVC** (RWX): the candidate source tree. The driver edits it; the broker hashes and
  builds it. Measure jobs do not mount it: a measurement is a function of the digest alone.
- **Model-cache PVC** (optional, RO on jobs): prewarmed weights, so serve start is a disk load.
  Only for domains that serve a model; unset means the jobs get no weights mount.
- **Artifacts PVC** (optional, writable, profile jobs only): where trace files land; the broker
  collects them into its log store. Without it, traces use a size-limited inline path that
  errors on oversized traces instead of truncating them.
- **Kueue**: jobs carry the `kueue.x-k8s.io/queue-name` label. The ClusterQueue quota decides
  when jobs run. Size the quota to the workload's hardware profile.

## Broker configuration

Substrate env (where things run):

```
BROKER_CODEGEN=1
BROKER_CODEGEN_NAMESPACE=<jobs namespace>
BROKER_SANDBOX_WORKDIR=<sandbox workspace path>   # required, the live-sandbox tree the broker pulls
BROKER_CODEGEN_QUEUE=<localqueue>              # default crucible-measure
BROKER_CODEGEN_MODEL_PVC=<claim>               # optional; unset skips the weights mount, set = RO at /models
BROKER_CODEGEN_ARTIFACTS_PVC=<claim>           # optional, profile trace transport
BROKER_CODEGEN_SANDBOX_WORKDIR=/workspace/<repo>  # fallback tree when no live sandbox
BROKER_CODEGEN_MAX_GPUS=<substrate ceiling>
BROKER_CODEGEN_MAX_CALLS_PER_TURN / _MAX_GPU_MINUTES_PER_TURN   # budgets, 0 = off
FORGE_REGISTRY=<candidate push repo>  FORGE_AUTHFILE=<push authfile path>
```

Tool contract (what is frozen, what the agent may vary): merged JSON, manifest defaults
overlaid by per-scenario values:

```json
BROKER_CODEGEN_TOOLS_DEFAULTS = {
  "gpus": 2,
  "build":     { "base_image": "registry.example.com/org/sandbox@sha256:...",
                 "src_dir": "/workspace/vllm",
                 "install_cmd": "VLLM_USE_PRECOMPILED=1 pip install -e . --user --no-deps",
                 "copy_chown": "1000:1000",
                 "mutable_kwargs": { "mode": ["derive"] } },
  "benchmark": { "command": "bench --frozen-flags ... --output-json \"$OUT\"",
                 "output_len": 1024, "num_prompts": 4,
                 "objective": { "key": "tpot_ms", "direction": "lower" },
                 "mutable_kwargs": { "toggles": {"MY_FEATURE_FLAG": ["0","1"]}, "reps": {} } },
  "lm_eval":   { "command": "eval --frozen ... ",
                 "objective": { "key": "score", "direction": "higher" },
                 "mutable_kwargs": { "limit": {} } },
  "profile":   { "command": "capture --out \"$OUT\"", "trace_ext": "json.gz" }
}
```

Each command writes its result to `$OUT` (provided by the job wrapper). If a tool's section is
omitted, calling that tool returns "not configured": omission is how a tool is mapped to
some deployments and not others. Undeclared kwargs and out-of-domain values are rejected.

## Driving it by hand

Streamable-HTTP MCP is three curls. Take the session id header from `initialize`; each result
arrives in a `data:` SSE frame:

```sh
curl -s -X POST $BROKER/mcp -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"me","version":"0"}}}'
# ... then notifications/initialized, then:
#   tools/call codegen_build      {"mode":"derive"}          -> {tree_hash, digest, cached}
#   tools/call codegen_benchmark  {"digest": "..."}          -> {metrics, objective, logs, cached}
#   tools/call codegen_lm_eval    {"digest": "...", "limit": 200}
#   tools/call codegen_profile    {"digest": "..."}          -> {trace, logs}
#   tools/call fetch_log / fetch_trace {"handle": "..."}
```

From an agent, the same tools appear as `mcp__crucible__codegen_*` by pointing `mcp_servers` at
the broker URL. A domain's gate typically wraps the build → benchmark → lm_eval chain into the
judge `{valid, score, detail}` contract as a single command, along these lines:

```python
# measure: the pack's gate script, invoked as [judge].measure_cmd
build = mcp("codegen_build", mode="derive")            # {tree_hash, digest, cached}
bench = mcp("codegen_benchmark", digest=build.digest)  # {metrics, objective, ...}
evals = mcp("codegen_lm_eval", digest=build.digest, limit=200)
valid = bench.ok and evals.score >= ACCURACY_FLOOR     # accuracy floor guards the perf metric
print(json.dumps({"valid": valid,
                  "score": bench.metrics["tpot_ms"],
                  "detail": {"digest": build.digest, "lm_eval": evals.score}}))
```

## Failure modes and fixes

- **Run the broker as PID 1.** Not `server & sleep N`: when the sleep exits, the pod completes
  and the broker disappears mid-run.
- **Service selectors must match only the broker pod.** A label shared with the driver pod
  routes some requests to a pod with no listener.
- **Git ownership.** The broker hashes a PVC workspace owned by another uid; git refuses with
  "dubious ownership" and builds fail. Add `git config --global --add safe.directory '*'` to
  the broker's entry command.
- **Set `VLLM_USE_PRECOMPILED=1` and a small `MAX_JOBS` in driver pods.** Otherwise a
  `pip install -e .` in the driver starts a parallel kernel compile and the pod OOMs. The
  broker build already checks compilation; the driver doesn't need to.
- **Give agent-driver pods enough memory.** Long-lived agent sessions grow; a multi-hour
  multi-phase run needed a 32Gi limit.
- **Broker restarts lose provenance and memos** (in-memory). After a restart, run
  `codegen_build` again before measuring: an unchanged tree and config produce the same
  candidate.
- **Unprivileged buildah needs `STORAGE_DRIVER=vfs`** (no /dev/fuse), and vfs storage is large
  (roughly 10x image size per build). It is pruned after every build, but budget node disk for
  one build in flight. The layer-capture primitive replaces this path.
- **Run long drivers as the pod's main process**, logging to the shared PVC. `kubectl exec`
  sessions die on output floods or disconnects and kill their children. Resume from step
  artifacts rather than streaming through exec.
- **`kubectl cp` of a directory** copies its contents when the destination exists. Check where
  files actually landed.
- **Absolute paths in analysis artifacts.** A config generated before a path rewrite bakes the
  old paths. Rewrite every artifact text file (metadata files included) before generating
  configs from them.
