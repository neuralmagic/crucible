# ADR 0026: The no-judge task lane

**Status:** Implemented
**Date:** 2026-08-17
**Related:** [ADR-0001](./0001-adaptive-harness.md) (the frozen-judge wall this lane deliberately
does not weaken), [ADR-0004](./0004-core-loop-state-model.md) (the row/session shapes the lane
reuses), [ADR-0023](./0023-recovery-classification.md) (resume, which is row-shaped and therefore
works unchanged)

## Context

Crucible had no lane for unsupervised agent chores with no objective: "consolidate the open
dependabot PRs", "fix flaky tests nightly". `[judge]` was required at the type level, so the
degenerate case wasn't expressible, and teams reached for weekend-scale external runners that hand
the agent credentials and keep no durable state. The gap is real and adjacent: everything such a
task needs (sandbox, broker mediation, session log, publish-on-keep, resume) already exists; only
the mandatory scoring stood in the way.

## Decision

**Absent `[judge]` = the task lane.** A manifest with no `[judge]` table builds the engine's
`TaskJudge` (`crucible/src/task_judge.rs`), mirroring the existing absent-`[world]`-means-`GitWorld`
fallback:

- `measure` emits `Reading { valid: true, score: None, solved: false }` — no command runs, no
  number is fabricated.
- `decide` keeps unconditionally and never solves; the run always exits via `Finished` (or
  budget/stop/escalation, as ever).
- `improved` is unconditionally true, so a completed task run exits 0.
- `objective()` is `"task"`, which is the discriminator on the wire: `Start.gate == "task"`.

Rows stay the ordinary `decision: "keep"` shape with `score: null`, so every keep consumer
(draft-PR publish, S3 record, resume fold, kept-best restore, flow rendering) works unchanged.
`skip_baseline` is forced on: the iter-0 row is `baseline-skipped` and the segment baseline stays
at the non-finite sentinel, the pre-existing skip-baseline convention.

Composites still require `[judge]`: a composite exists to combine scored components. The scope
pipeline's gaming review still rejects a proposed pack with no judge. The deploy renderer omits
`BROKER_MEASURE_CMD` entirely for a task manifest, so the broker's `measure` tool answers with its
"measurement not configured" error instead of running an empty command.

`crucible check` on a task manifest skips the measure probe, the editable-gate lint, and the
selftest block (including the missing-selftest warning), and instead requires a non-empty goal and
prints a loud notice: "task mode: no [judge] — every completed turn is kept and published
unscored". The engine prints the same line at run start.

## Alternatives rejected

- **`[judge] mode = "none"`**: safer against accidentally deleting `[judge]` from a scored
  manifest (absence would then be a parse error), but it adds a concept and makes every `JudgeCfg`
  field conditionally required. Mitigations chosen instead: the check/run-start notices, and the
  fact that a frozen pack losing its `[judge]` changes the run identity digest, which resume
  already warns about loudly. If accidental omission bites in practice, this marker is the escape
  hatch.
- **`[agent] mode = "task"`**: conflates proposal-policy config with run semantics and still
  forces the judge optional.
- **A separate one-shot subcommand**: re-implements session Start/Row/Summary/Shutdown, publish,
  and resume, all of which the loop already provides.

## Consequences

- The trust boundary is untouched: the agent still holds no privilege, and task output is kept
  commits published as a draft PR. Privileged write actions (merging a PR, closing an issue)
  remain broker-tool material, added case by case with server-side admission.
- Exit 0 does not mean "the chore succeeded" — it means the run completed. A task run whose every
  turn errored (all rows discarded on apply failure) still exits 0. Consumers must inspect rows or
  PR presence; a later refinement may downgrade the exit when no keep row exists.
- `Summary.best_score`/`Segment.baseline_score` carry the non-finite sentinel and serialize to
  `null`; readers must not hard-require a finite summary score (pre-existing for skip-baseline
  codegen runs).
- Downstream dashboards that read `decision == "keep"` as "beat the objective" must branch on
  `gate == "task"`.

## Future work

- An agent-declared "done early" marker (the fixed-N iteration budget is the only terminator
  today), fitting the existing `drain_turn_markers` shape.
- Broker write tools (`merge_pr` and friends) for task flavors that need more than a draft PR.
