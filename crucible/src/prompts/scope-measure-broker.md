## Scope: this issue is broker-measured

The gate for this issue does NOT run on the machine that validates it. The score comes from GPU
hardware, and the only way to reach that hardware is the broker's code-gen MCP tools, which
submit queue-admitted GPU Jobs on your behalf. So the usual "GPU work means write `REJECTED.md`"
rule does not apply here: drafting a locally-runnable harness, or rejecting this issue because
you can't time it yourself, are both wrong answers. Draft the brokered gate.

You cannot execute the gate in this turn — no broker, no GPUs, no cluster. Ignore the later
instruction to run `measure_cmd` yourself; run the broker-free self-test control instead (below),
and read the `_scope_context/crucible-contract.md` measure contract carefully, since the contract
is all you get to check the gate against.

The goal text above carries the **codegen tool contract** for this issue: the frozen build,
benchmark, and profile commands, the GPU count, and the objective key/direction. Copy those
values into the manifest verbatim. Do not invent commands, images, or flags that aren't in it,
and do not drop the ones that are.

## What to draft

The manifest is `[repo]`, `[agent]` (with `[agent.broker]`), `[measure]`, `[judge]`,
`[judge.selftest]`. No `[world]` block: GitWorld's commit/reset is still the world, the GPU job is
a function of the built digest, not of live cluster state. A sketch — the repo, images, and
commands are fictional, only the shape matters:

```toml
[repo]
url = "https://example.invalid/acme/kernels.git"
ref = "main"

[agent]
backend = "local"
goal_file = "goal.md"

# The domain's broker binary, spawned as a run-lifetime child of the loop pod. `bin` is REQUIRED
# when enabled; `build = true` turns on the engine-side build/deploy tools the gate calls.
[agent.broker]
enabled = true
bin = "crucible-broker"
build = true

# The codegen tool contract, verbatim from the goal text. This IS the frozen command set: the
# broker validates it at startup and the GPU jobs run exactly these commands.
[measure]
gpus = 2
[measure.build]
base_image = "registry.example.invalid/acme/sandbox@sha256:deadbeef"
src_dir = "/workspace/kernels"
install_cmd = "pip install -e . --no-deps"
[measure.benchmark]
command = "bench --frozen-flags --output-json \"$OUT\""
objective = { key = "tpot_ms", direction = "lower" }
[measure.profile]
command = "capture --out \"$OUT\""
trace_ext = "json.gz"

[judge]
# tools/gate.sh talks MCP over http to the broker on loopback (the port from
# [agent.broker].bind), chains codegen_build -> codegen_benchmark (-> codegen_profile when the
# goal's objective needs the trace), and emits ONE contract JSON line.
measure_cmd = "./tools/gate.sh"
direction = "lower"
objective = "perf"
# The baseline reading costs a full GPU job for a number the tools already memoize per digest.
skip_baseline = true

[[workspace.inject]]
src = "tools/gate.sh"
dst = "tools/gate.sh"
frozen = true

[judge.selftest]
# Broker-free by contract: resolves the tool contract / builds the call ladder and prints it,
# without a single broker call. This is the only part of the gate host-side validation can
# honestly run, so it must pass with no broker and no GPUs reachable.
good_cmd = "./tools/gate.sh --selftest"
bad_cmd = "true"
runs = 3
```

**Broker-measured hard requirements** (a pack that skips any of these is not a valid proposal):

- **`[agent.broker]` with `enabled = true` and a `bin`.** The broker is how the gate reaches
  hardware; `bin` is required when enabled and validation fails without it. Set `build = true`
  when the gate calls `codegen_build` (it does).
- **`[measure]` carries the codegen tool contract from the goal text, unchanged.** It is the
  frozen command set the GPU jobs run (`gpus`, `build`, `benchmark`, `profile`). Rewriting a
  command, loosening a flag, or shrinking the workload is weakening the gate.
- **`measure_cmd` is a gate script that drives the broker's MCP tools** — `codegen_build` to turn
  the current workspace tree into a digest-pinned candidate, then `codegen_benchmark` (and
  `codegen_profile` only if the objective needs a trace) against that digest — and emits exactly
  one `{valid, score, solved, note, detail}` line. Measure the digest you just built, never a
  digest the agent handed you.
- **Fail closed on missing or truncated hardware evidence.** If a tool returns an error, a
  "not configured" reply, a truncated trace, a missing metrics key, or fewer readings than the
  contract asks for, emit `valid = false` with the reason in `note`. Never substitute a default
  score, a partial trace, or a cached number from a different digest for a measurement that did
  not happen. A gate that scores on truncated evidence is worse than a gate that errors.
- **Score from the tools' returned metrics only.** The objective key and direction come from the
  `[measure]` contract; do not score on anything the candidate code prints about itself.
- **`[judge.selftest]` is MANDATORY and MUST be broker-free.** It is the only part of this pack
  validation can run without GPUs, so it may not call the broker, reach the network, or need a
  digest. Make `good_cmd` exercise the gate's offline half — resolve the `[measure]` contract,
  build the call ladder it would issue, validate the JSON shape it would emit — and exit nonzero
  if any of that is wrong. `runs` must still be at least 3.
- **`skip_baseline = true`.** A baseline reading is another full GPU job for a number the
  digest-keyed tools already memoize; the loop does not need it.

## Author the work graph

Beside `crucible.toml`, write `workflow.star`: `propose(name = "propose", session = "solver")`,
then one `evaluate` task per oracle in the tools contract (dependencies mirroring the oracle
ladder; oracles past the correctness terminal get `required = False`), a `grade` folding their
evidence with the terminal oracle as `score`, and `decide`. The pipeline compiles it into the
manifest automatically; do not hand-write the `[workflow]` block.
