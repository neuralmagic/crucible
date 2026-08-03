## Scope: v1 targets T0 and T1

This pipeline handles two gate shapes:

- **T0 (existing-tests)** — a bug fixable and verifiable by a frozen, deterministic `go
  test`-style command (or an equivalent deterministic measurement) already latent in the repo.
  GitWorld, no rig, no cluster, no perf workload.
- **T1 (new-metric-harness)** — no existing test can gate this issue, but a measurable quantity
  (latency, memory, throughput, an operation count, a wall-clock cost over a fixed workload) can,
  and you can build the harness that measures it yourself, locally, deterministically. Still
  GitWorld, still no rig, still no cluster — the difference from T0 is *you author the
  measurement* instead of pointing at one that already exists.

If the issue in front of you needs a measured perf gate against a **live rig or cluster**
(T2/T3 — a deployed service, GPU hardware, a multi-node/multi-component setup), that is still
**out of scope for this pipeline**. Do not force it. Instead, write `REJECTED.md` in the output
directory explaining specifically why (what the issue needs that this pipeline can't gate), and
stop — a clean rejection is a correct, useful answer, for T2/T3 exactly as it always was.

## What to draft

The code under test is checked out in your current working directory. Read it before proposing
anything. Seed context is under `_scope_context/`:

- `_scope_context/GOAL.md` — the issue text (same as above).
- `_scope_context/crucible-contract.md` — the full engine/domain contract: manifest shape,
  command protocol, `measure_cmd`'s `{valid, score, solved, note, detail}` output, `[world]`.
- `_scope_context/examples/counter/` — the minimal worked example of a complete pack
  (`crucible.toml` + `measure.nu`): read it to see the shape, not to copy its domain logic. It's a
  T0-shaped example (its gate is a bespoke script, not `go test`, but the manifest shape — no
  authored `tools/` harness, no reps-inside-the-tool — is T0's).

And here is a sketch of a **T0** pack — a `go test`-backed `measure_cmd`, GitWorld, no `[world]`
block. The repo and paths are fictional; only the shape matters:

```toml
# Bug-fix gate for widgetlib issue #7 ("Reticulate() drops the last spline").
[repo]
url = "https://example.invalid/acme/widgetlib.git"
ref = "main"

[agent]
backend = "local"
goal_file = "goal.md"

[judge]
# measure.sh runs `go test ./pkg/reticulator/...` and emits the contract JSON:
# score = failing-test count, lower wins; solved = green AND a new regression test exists.
# measure_cmd (and the selftest commands) run with the WORKSPACE as cwd — the gate script
# must be staged there, so inject it frozen (below); a bare ./measure.sh next to the
# manifest is unreachable and fails validation with exit 127.
measure_cmd = "./measure.sh"
direction = "lower"
objective = "test"

# Stage the gate into the workspace, re-copied before every measurement so it can't be gamed.
[[workspace.inject]]
src = "measure.sh"
dst = "measure.sh"
frozen = true

[judge.selftest]
# good: apply a known-correct reference patch, then commit — the gate must go green.
good_cmd = "git apply _controls/reference-fix.patch && git add -A && git -c user.email=scope@local -c user.name=scope commit -qm good-control"
# bad: stay on the buggy baseline (a no-op) — the gate must stay red.
bad_cmd = "true"
# runs MUST be at least 3 for a proposed pack: one reading can't prove a noisy gate discriminates.
runs = 3

# No [world] block: a pure in-tree fix gated by `go test` only — GitWorld's commit/reset
# reversibility is the whole world.
```

If you're drafting **T1**, the manifest shape is identical — `[repo]`, `[judge]`,
`[judge.selftest]`, `[agent]`, no `[world]` — except `measure_cmd` points at a harness *you write*
instead of an existing test runner, and that harness lives under a `tools/` subdirectory of the
pack (the same convention this repo's own domains use for their measure adapters — see any
`domains/*/tools/` if you want prior art, though those are hand-maintained rigs, not what you're
drafting). A sketch, same fictional repo, a metric this time ("Reticulate() allocates one buffer
per point instead of reusing one — issue asks for O(1) allocations"):

```toml
[judge]
# tools/bench.sh runs a fixed workload (a pinned 10k-point input, tools/fixture.json) 5 times
# INSIDE the script and emits the contract JSON with score = median wall-clock ms. The engine
# calls measure_cmd exactly once per iteration — repetition to de-noise a wall-clock reading
# happens inside the tool, never by the engine re-invoking it.
measure_cmd = "./tools/bench.sh"
direction = "lower"
objective = "perf"

# Every file the harness reads must be frozen-injected — the script AND the fixture, both
# unreachable from a bare workspace clone (they live in the pack dir, not the checkout).
[[workspace.inject]]
src = "tools/bench.sh"
dst = "tools/bench.sh"
frozen = true
[[workspace.inject]]
src = "tools/fixture.json"
dst = "tools/fixture.json"
frozen = true

[judge.selftest]
# good: apply the real fix (reuses a pooled buffer) — median ms must drop.
good_cmd = "git apply _controls/reference-fix.patch && git add -A && git -c user.email=scope@local -c user.name=scope commit -qm good-control"
# bad: the buggy baseline, unchanged — median ms must stay high.
bad_cmd = "true"
runs = 3
```

**T1 hard requirements** (a harness that skips any of these is not a valid T1 proposal):

- **Deterministic, fixed workload.** Pin every seed and every input the harness runs against
  (a fixed dataset, a fixed random seed, a fixed input size) — the same run must produce the same
  answer modulo the metric's own noise floor, never a workload that regenerates itself randomly
  per invocation.
- **Repetitions live inside the tool, not the engine.** The engine calls `measure_cmd` exactly
  once per measured iteration. If the metric is noisy enough to need N readings averaged (or a
  min-of-N), that averaging happens *inside your script*, which still emits exactly one contract
  JSON line per invocation. Don't rely on the pipeline calling your script multiple times — it
  won't outside of `[judge.selftest].runs`, which is a different knob (see below).
- **Score from observed behavior only.** The harness must derive the score by watching the code
  run from the outside (timing it, counting operations it performs, checking its output) —
  never by trusting a number the code under test prints, returns, or writes about its own
  performance. A self-reported number is not a measurement; see the gaming-review attack list
  this pack will face after it validates.
- **Selftest controls stage a real fix vs. a real regression for *this* issue** — same
  requirement as T0, just against your harness instead of `go test`. `good_cmd` applies (or
  simulates) the actual improvement the issue asks for; `bad_cmd` stays on the buggy/slow
  baseline. Degenerate controls (writing a big number vs. a small one into a file the real fix
  never touches) fail review even if the mechanical validator lets them through.
- **`runs` ≥ 3, same floor as T0.** This is `[judge.selftest].runs` — how many times the
  validator re-runs `good_cmd`/`bad_cmd` to prove the gate discriminates the noisy metric, not
  how many reps your harness does internally per call (that's the point above). Both matter,
  they're not the same knob.
