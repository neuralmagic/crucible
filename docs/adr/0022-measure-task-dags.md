# ADR 0022: Measure task DAGs — the engine walks the ladder

**Status:** Partially implemented
**Date:** 2026-07-18
**Related:** [ADR-0001](./0001-adaptive-harness.md) (the frozen judge this walker becomes part of),
[ADR-0005](./0005-engine-side-builds.md) (the brokered codegen tools the walker dispatches),
ADR-0016 (session.jsonl as the source of truth the DB
re-indexes — the property the per-task cache leans on),
[ADR-0017](./0017-turn-result-contract.md) (the event contract per-task results extend),
[ADR-0020](./0020-candidate-build-modes.md) (how a candidate becomes the digest these tasks measure).
Supersedes the walker half of the kernel domain's gate; subsumes #8 (named-job registry); completes
#291 (regrade). Tracking issue: #292.

## Context

A GPU-measured code domain grades a candidate through a sequence of oracles: a CPU reference
self-check, a single-GPU numerical diff, an ncu tensor-pipe capture, a fused multi-GPU equality
test, a compute-sanitizer racecheck. Today that sequence lives inside an opaque domain gate script
(call it `kernel_gate.py`, a GPU kernel domain's rung-walker): the engine runs one `measure_cmd`, the script walks
the ladder by calling broker MCP tools itself, and eight hundred lines later the engine receives a
single `{valid, score, detail}` blob. The individual oracles already run as separate Kueue GPU
jobs — the *walk* is the only part that is a black box.

The kernel domain's bring-up paid for that black box three times in two days:

- **No per-step resume.** Run 13 proved steps 1–5 on a cached digest, then died at step 6. The
  rerun repaid all five proven steps. Regrade (#291) should mean "re-run the failed step on the
  cached digest", keyed on (digest, step) — but no such key exists anywhere.
- **Silent retries.** Step 6 hit its 90-minute job deadline five consecutive times. Kubernetes
  retried behind everyone's back; the engine saw nothing until a verdict arrived ~8 hours late.
  There was no engine-visible per-step state to even ask about.
- **Policy in per-domain Python.** Transport-error-versus-measured-failure classification,
  advisory-past-terminal semantics, substrate capability filtering, fail-closed hardware
  truncation — all generic judge policy, all hand-implemented (and hand-patched, twice) in one
  domain's gate script. The next codegen domain would copy-paste the lot.

And the sequence is not actually a ladder. The ncu capture and the racecheck both depend only on
the single-GPU diff; on two GPUs they could run concurrently. A ladder is a small DAG that nobody
wrote down as one.

## Decision

Implemented (2026-08-02): typed `evaluate` tasks and engine-owned `grade` run in the main work
graph. Dependencies define rungs, isolated evaluators use existing parallel batches, and `grade`
feeds `decide`. Legacy `measure_cmd` remains supported. Broker templates, caching, and targeted
regrade remain open.

The manifest declares the DAG as data. The engine walks it. The domain keeps exactly one thing:
the frozen per-task command baked into the sandbox image.

### Vocabulary: the unit is a **task**

Declared `[[measure.task]]`, identified by name, connected by `depends_on` edges. Not "rung" (a
ladder word — the whole point is that this is not a ladder), and not stage/step/node/check/job,
which all name live things elsewhere in the engine (the scope pipeline's `Stage`, the journey's
`JourneyStep`, the SPA's journey nodes, `crucible check`, the broker's Kueue jobs). No new code or
prose says "rung"; existing occurrences are cleaned up by whichever change rewrites their file.

### The manifest schema

`MeasureCfg` (`crucible/src/manifest/measure.rs`) stops being an opaque pass-through table and
becomes typed, because the engine now consumes half of it. The fixed `benchmark`/`lm_eval`/
`profile` tool trio is replaced by **named job templates** — this is the #8 registry, and it kills
both the case-select env-toggle overload and the dummy `[measure.lm_eval]` block the kernel domain
carries only because the broker demands a command:

```toml
[measure]
gpus = 1
score_task = "calc-diff"     # optional; default = last-passing required task in topo order

[measure.build]              # unchanged — the broker build contract (ADR-0005/0020)
base_image = "..."

[measure.job.oracle]         # named job templates replace the benchmark/lm_eval/profile trio
command = 'python $REFERENCE_DIR/run_task.py --task "$CRUCIBLE_TASK" --out "$OUT"'
kind    = "metrics"          # or "trace" (+ trace_ext); optional per-job gpus override

[[measure.task]]
name       = "calc-diff"     # identity; injected as $CRUCIBLE_TASK, frozen, agent-invisible
job        = "oracle"
depends_on = ["refcheck"]    # default []
needs      = "fp8-tc"        # substrate capability, default "any"
threshold  = 0.001
direction  = "lower"         # an explicit `pass` bool in $OUT wins over threshold grading
required   = true            # default true
```

Validated at parse time: unique names, edges resolve, acyclic, referenced jobs exist. Exactly one
of `[judge].measure_cmd` or `[[measure.task]]` — declaring both is an error, declaring neither
where a judge is needed stays an error. Domains without tasks (the vLLM brief's single-shot gate)
are untouched; the legacy trio keys still project to the broker.

Substrate capabilities are **profile-side facts**: the deploy profile's `[measure]` block gains
`caps = ["any", "fp8-tc"]`, projected as `CRUCIBLE_MEASURE_CAPS` into the loop pod. This replaces
the capability map hardcoded in the Python gate and the substrate env hack. Fail-closed
survives the move: tasks declared but caps unset means the walker refuses to measure.

The broker never sees the DAG. `BROKER_CODEGEN_TOOLS_DEFAULTS` grows a `job` map of named
templates; the `[[measure.task]]` array is stripped before projection. One new broker tool,
`codegen_task {digest, task, job}`, dispatches a template with the frozen `CRUCIBLE_TASK` env; the
memo key gains the task name, which also fixes the collision where two profile captures with
identical fixed toggles shared a cache entry.

An escape hatch for exotic scoring exists but is deliberately narrow: an optional
`[measure].grade_cmd` receives the assembled per-task results as JSON on stdin and emits the
standard `{valid, score, ...}` line, overriding the engine's default fold. Grading rules
(threshold, direction, pass-key) are data and stay in the manifest; there are no per-task grader
commands. The kernel domain needs neither; its gate script is deleted, not shrunk.

### Validity: `required`, not "terminal"

"Terminal rung" and "advisory past the terminal" are ladder concepts; with parallel branches
"past" has no meaning. The DAG-native contract:

- A task is **runnable** iff its `needs` is in the substrate caps and every transitive dependency
  is runnable.
- **valid = every `required` task is runnable and passed.**
- **Hardware truncation is computed before dispatching anything**: a required task filtered out by
  the substrate means `valid:false` with an explicit note and zero GPU spend. A truncated DAG can
  never produce an honest pass, so it fails fast instead of measuring toward a foregone verdict.
- Advisory (`required = false`) tasks: unrunnable means skipped; a failure is recorded and never
  gates validity, but the failed task's own dependents are blocked — nothing runs on top of a
  failure.
- Short-circuit: a required task's measured failure (or transport-retry exhaustion) fails the
  reading immediately; undispatched tasks are marked blocked, in-flight ones complete and cache.

The old per-profile terminal difference (one numeric format gates on the fused test, another on
the single-GPU diff) becomes plain data: each manifest marks different tasks required.

### The walker

A new `TaskDagJudge` behind the existing `Judge` trait, selected by `build_judge` when the
manifest declares tasks; `CommandJudge` is untouched and the loop driver does not change. It calls
the broker's codegen tools over the same MCP streamable-HTTP surface the Python gate used — the
broker stays a dumb, trusted executor where one tool call is one Kueue job is one memo entry.

The walk is a ready-set loop over the DAG. **v1 pins `max_inflight = 1`**: the kernel domain's DAG is
nearly a chain, the single-GPU queue serializes anyway, and in-flight-sibling cancellation is
machinery no domain needs yet. The structure is parallel-ready; lifting the cap is a constant, not
a redesign.

Retry policy splits on the classification the broker already puts on the wire:
`CodegenReply::JobFailed` is a *measured* failure and is never retried — a kernel that failed
calc_diff failed. `CodegenReply::Error` and HTTP failures are *transport* and get a bounded
auto-retry (default 2), every attempt an engine-visible event. The forge GPU job pins
`backoffLimit: 0` so Kubernetes never again retries where the engine can't see; the 90-minute
deadline itself stays a broker/forge-owned substrate fact.

### Cache and regrade

The durable (digest, task) record is the session log, per ADR-0016: every attempt appends a
`SessionEvent::TaskResult`, and on resume or regrade the engine folds the log into a
`(digest, task) → result` map, skipping recorded passes and re-running recorded transport
failures. No new persistence machinery; the run-13 scar (die at task 6, repay 1–5) closes by
construction. The broker's in-memory memo remains the fast path within a broker lifetime.

`crucible regrade --digest <D> [--task <name>]` (#291) builds a plan restricted to the target's
transitive closure, serves dependencies from cache, force-reruns the target, and re-folds the
verdict. So regrade-after-broker-restart works without a rebuild, the broker's `is_built`
provenance check re-verifies the candidate image in the registry instead of only trusting its
in-memory set.

### Telemetry

`SessionEvent::TaskResult { iter, digest, task, job, attempt, status, metric, note, secs }` with
`status ∈ pass | fail | transport | skipped | blocked | truncated` — additive, no wire-version
bump. The controller folds it into a `task_results` table (child of candidates, rebuildable like
everything else), the SPA gets a per-candidate task grid with live progress from the session tail,
and the candidate row keeps an assembled `detail.tasks[]` so the existing single-row view degrades
gracefully. The walker opens one OTLP span per task attempt under the per-turn traceparent; the
broker's existing per-tool spans nest beneath it unchanged. "Task 6 running, attempt 2" is visible
within seconds instead of eight hours after the fact.

## Consequences

- Generic judge policy (classification, retry, truncation, short-circuit, caching) is written
  once, in Rust, tested in the engine — the next codegen domain declares ~40 lines of TOML and
  bakes one frozen command, instead of copy-pasting an 800-line walker.
- #8 and #291 stop being separate work: the task→job mapping is the registry, and per-task cache
  keys make regrade incremental.
- The gate-script tier shrinks: the kernel domain's `kernel_gate.py` and its tests are deleted, their
  pure semantics ported into the walker's unit tests; `run_rung.py` becomes `run_task.py`
  switching on `$CRUCIBLE_TASK`. `crucible check` prints the resolved runnable set and the
  truncation verdict, replacing the gate's `--selftest`.
- The broker gains a tool and a config key but no orchestration; its trust posture is unchanged.
- Single-shot domains keep `measure_cmd` forever — the DAG path is opt-in per manifest.
- Follow-ups deliberately deferred: `max_inflight > 1` (needs sibling-cancellation semantics),
  richer score composition (the answer is `grade_cmd`, not a bigger TOML grading language), and
  baseline-differential grading (count only *new* racecheck hazards vs the base SHA) as a task
  attribute.

## Alternatives considered

**Broker-side orchestration** — the engine calls one `codegen_walk` tool and the broker runs the
DAG. Rejected: it structurally recreates the one-long-opaque-call problem (per-task progress again
trapped behind a single tool call), and it moves judge policy into the component whose job is to
be a dumb executor. The transport/measured split the walker needs is already visible on the reply
wire; the broker has nothing to add but opacity.

**Keep the gate script as a thin grader** — engine walks, then shells out to domain Python for the
verdict fold. Rejected: the fold (required-set over task results) is five lines of generic policy;
leaving it in per-domain scripts preserves exactly the copy-paste channel this ADR exists to
close. `grade_cmd` remains for domains that genuinely need custom scoring, as an explicit opt-in
rather than the default architecture.

**A generic engine DAG framework** — model tasks on a general workflow engine (or grow `WorkKind`
into one). Rejected as scope creep: this is a judge implementation detail behind an existing
trait, not a new orchestration primitive. If a second DAG consumer appears, extract then.
