# Control flow as data, and the wire fields that would carry it: handoff

Date: 2026-09-05. Six PRs landed in two days; the next piece is a contract bump, so read
"Closing the boundary" before touching `crucible-contract`. The 2026-08-25 handoff this replaces
is in git history; every arc in it shipped.

## What landed (all on main, oldest first)

| PR | Head | What |
|---|---|---|
| #108 | `4e44425` | `cargo xtask modgraph` (item-level module graph, `--check` fails on cycles, in CI); main.rs split into `args`, `cli`, `process`; 14 module cycles broken by moving items; run.rs split into `cli/{run,setup,workspace}`; the bin's flat modules regrouped into `agent/`, `runloop/`, `control/`, `report/`, `scope/`, `cli/`. The reporter is handed to a run-scoped `LoopTaskRunner` (no `Arc<Mutex<R>>`). |
| #109 | `45e0cb5` | `runloop::machine`: the loop's states, events, and transition table; the driver advances it at every gate; `LoopPhase` (control-plane status) is a projection of it. `docs/loop-states.md` rendered from the table. |
| #110 | `a0a48ae` | `plan::machine`: per-task and per-plan tables the executor asserts at every decision; `BlockedReason` typed (its `Display` is the unchanged note text); `execute` returns `Result`. `crucible::diagram` holds the cursor, table checks, and dot/mermaid renderers both machines use. `docs/plan-states.md`. |
| #111 | `a65c107` | Incidental complexity the tables exposed: one attempt-settling path in the executor, a three-variant `Halt`, one test env lock, one `manifest_dir`, `Attempt::failed`/`transport`. |
| #112 | `e04cda6` | Work items WI-2026-09-05-001 and -002 (gov/work). |
| #113 | `f2d540c` | WI-001 done: `--graph-loop` and the `Iteration<Proposed, Applied, Measured>` typestate are gone; every iteration runs as an executor plan; ADR-0004 carries the addendum. Plus the broker's env-mutating tests on one lock. |

The lib/bin split stays (RFC-0004: the controller links the lib). `crucible::plan`, `manifest`,
`deploy`, `flow`, `exposure`, `errors` paths are an API; bin-side modules move freely.

## In flight right now

- **Core pin bump.** The controller repo (`crucible-domains`) follows this repo's main
  automatically: `.github/workflows/core-pin-bump.yml` polls every 15 minutes and opens a queued
  PR per new head. Bumps to `45e0cb5`, `a0a48ae`, `e04cda6` merged on 2026-09-05. **PR #622
  (bump to `f2d540c`) was open with CI running at 09:20 PT.** Check with
  `gh -R neuralmagic/crucible-domains pr view 622`. Nothing manual is needed; if it fails, the
  failure is the controller compiling against the new lib, and that is the thing to read.
- **fips-watch run after the bump.** Will wants a controller workflow exercised on the new
  engine. Launch with the crucible MCP: `crucible_launch id=fips-watch max_cost=5 max_time=30m`
  (params empty; `downstream_repo` defaults to `opendatahub-io/modelexpress`). The last manual run
  was `playbook:fips-watch:01a04841-…`, $2.00, dispatched as a pod on cluster `waldorf`. To prove
  the new engine ran it, read the loop image off the pod while it runs:
  `oc --context coreweave-waldorf -n crucible-system get pod <pod> -o jsonpath='{.spec.containers[*].image}'`
  and compare to the `crucible-loop` image crucible-domains' docker.yml published after #622 merged.
  The controller itself serves from the spoke prod cluster; that `oc` context needs a fresh login.

## Closing the boundary: the next PR pair

Typed state now stops at the process boundary. The session log still carries a blocked task's
reason as prose and ignores most of the loop's phases. Two additive fields close it.

### Facts that constrain the design (verified 2026-09-05)

- The controller ingests session events into its own `Ev` enum (`crucible-controller/src/ingest.rs`,
  `#[serde(tag = "kind")]`): row, summary, budget, identity, pr_links, plan_admitted,
  task_result, shutdown; everything else is `Other`. Task results land in `run_task_results`
  (status, note, cost, secs). `failure_cause()` takes the first non-empty note of a
  fail/transport/truncated task; blocked tasks never contribute.
- "run errored: {reason}: {cause}" (`model.rs` `ParkReason::RunErrored`) is the shutdown reason
  string plus that note.
- `SessionEvent::Phase { phase, iter }` already exists on the wire with three values
  (preflight, baseline, iteration), emitted through `Reporter::phase`. The controller drops it.
- The heartbeat is deliberately not a session event (OTLP span + pod log only). Do not carry
  phase on it.
- `report.json` (`crucible_contract::report::TaskReport`: name, status, cost) is consumed by the
  broker's Slack card, not the controller.
- **Contract versions match only by equality** (RFC-0004 C-CONTRACT-VERSION). `CONTRACT_VERSION`
  is `1.2.0` and has never been bumped since it was introduced. The auto pin-bump moves
  `crucible-contract` and `crucible` together, so a controller compiled after the bump carries the
  new version; the runtime image is labeled with it (`Containerfile.runtime` asserts the label
  equals `crucible --contract-version`). Between the image publish and the controller rollout
  (imagepatcher moves the deployment to `latest`), dispatch to the new image is refused and
  ledgered by design. This will be the first exercise of that path.

### Engine PR (this repo)

1. `crucible-contract`: add
   `TaskBlocked { reason: BlockedReasonKind, task: Option<String> }` with
   `BlockedReasonKind` = required_task_failed | budget_ceiling | wall_clock_ceiling |
   dependency_did_not_pass | staging_refused (snake_case). Add it as
   `#[serde(default, skip_serializing_if = "Option::is_none")] blocked: Option<TaskBlocked>` to
   `SessionEvent::TaskResult` and to `report::TaskReport`. Extend the documented `phase`
   vocabulary to the `LoopPhase` tokens: starting, preflight, baseline, wide, iteration, paused,
   parked, distressed, escalated, epilogue, finished. Bump `CONTRACT_VERSION` to `1.3.0`.
2. `plan::exec::TaskResult` gains `blocked: Option<BlockedReason>`; `TaskResult::blocked(&reason)`
   fills it. `BlockedReason` gets a `wire()` returning the contract type. The note stays
   byte-identical (tests pin it).
3. `plan::events::task_result_event` and `plan::cli`'s `report.tasks.push` copy `blocked`.
4. `runloop::machine::Machine::advance` reports the phase through the reporter on every change,
   not only preflight/baseline/iteration. Replace the reporter's `Phase` enum with
   `(LoopPhase, iter)` or map `LoopPhase` into `SessionPhase`; `report/session.rs` owns the
   mapping. The control-plane `set_phase` stays as is.
5. Tests: a session-log round trip for a blocked task shows both the note and the typed reason;
   the loop tests assert the phase sequence for a parked run; `scripts/state-docs.sh --check` and
   `cargo xtask modgraph --check` stay green.
6. Ship, then watch the pin bump: confirm the controller's compile against 1.3.0 passes CI and
   that its config endpoint shows the dispatch-image mismatch clearing after rollout.

### Controller PR (crucible-domains)

1. `Ev::TaskResult` gains `blocked: Option<TaskBlocked>` (default). Migration: nullable
   `blocked_reason` and `blocked_by` on `run_task_results`.
2. `failure_cause()` and `RunErrored` derive from the typed reason when present: "blocked:
   required task brief failed" from data, not substring matching.
3. Handle `Ev::Phase` instead of dropping it: keep the latest phase on the run row and show it in
   the runs table and run page, so a parked or distressed run reads that way while it happens.
4. The broker's Slack card can group non-passing tasks by `blocked.reason`.

## Also queued

- **WI-2026-09-05-002**, collapse the claude/codex/hermes harness backends onto one spec plus the
  methods that genuinely differ. Scope and criteria are in gov/work; measure first (the
  per-backend tests are the regression net).
- Transition coverage: nothing yet records which table edges the suites exercise.

## Gotchas learned this week

- `govctl work move <id> done` runs the project's default guards, including
  `cargo test --workspace`; a flaky test anywhere in the workspace blocks the close. Notes take
  `--add`, description takes `--set --stdin`, and an ADR may not mention a work item id.
- `cargo fmt` reshapes text between reading and patching it; anchor edits on the formatted text.
- libtest runs tests in name order; a test that mutates process-global state (pids, environ)
  changes who it races when it is renamed or moved. Every such test now takes
  `crucible::test_support::env_lock()` (engine) or `crate::test_support::env_lock()` (broker).
- `execute` is fallible; the executor's and harness's test modules wrap it once (`fn execute`
  shadowing the import) rather than touching every call.
- The docs SVGs are rendered locally with graphviz and committed; CI checks the `.dot` and the
  pages, not the SVG. `just state-docs` regenerates all three.
