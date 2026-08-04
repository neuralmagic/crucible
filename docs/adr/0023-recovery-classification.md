# ADR 0023: Recovery classification for `--resume`

**Status:** Implemented
**Date:** 2026-08-04
**Related:** [ADR-0003](./0003-async-approval-waits.md) (the approval waits whose loss this
fixes), [ADR-0004](./0004-core-loop-state-model.md) (the loop state the classifier reconstructs),
ADR-0016 (session.jsonl as the source of truth the classifier reads).

## Context

Resume was a pure counter fold: replay `state/session.jsonl` into rows, spent, best score, and
`next_iter`, then re-enter the loop. Everything the tail of the log says about HOW the run died
was discarded:

- `shutdown` is documented as the last line of every clean exit, and its absence is already the
  viewer's "pod died mid-run" signal, but resume never read it. A resumed *solved* or
  *escalated* run with iterations remaining re-entered the loop, because the nothing-to-do guard
  was pure iteration/budget arithmetic.
- A dangling `agent_start` (no `agent_done`) is a died-mid-turn fingerprint with rich evidence
  between the brackets (token/cost events, the last error, the durable session and turn
  number). All invisible.
- A pod that died while parked on a block-mode approval resumed with the approval silently
  dropped: `pending_block` is in-memory only, `PROVISIONING_PENDING.json` is consume-on-read
  and long gone by park time, and `set_pending_regime` was never re-registered, so an operator
  `approve` resolved nothing.

## Decision

One streaming pass over the log (`crucible/src/recovery.rs`) produces both the existing resume
counters (`ResumeFold`, extracted from the old `load_resume_state` loop body) and a typed
`Classification` of the tail. `plan_recovery` maps the classification to a `RecoveryPlan`
(`NoOp` / `Refuse` / `Continue`), the single gate the resume path in `run.rs` goes through.

Key calls:

- **Classification is derived from the log tail, never from marker files.** Markers
  (`ESCALATION.json`, `PROVISIONING_PENDING.json`) are consume-on-read and gone by the time the
  loop acts on them; the log is the only durable record. This adapts the shape of flue's
  recovery pass (converge durable records, classify each interrupted unit into
  settle/retry/continue/repair), not its mechanism.
- **New wire events close the observability gap**: `approval_wait`/`approval_resolved` bracket
  every approval, and `recovery` records the classification once per resume. A stop-while-parked
  deliberately does NOT emit `approval_resolved`: a stop doesn't resolve the ask, and the
  still-open bracket is what makes the resumed run re-park. The flip side: an operator who
  stopped a run precisely to abandon the ask must `deny` it, or the resume re-parks.
- **Policy deltas from the old arithmetic guard**: a `shutdown` of `finished`/`solved` is a
  clean no-op even with iterations remaining; `escalated` refuses to resume (exit code 2's
  meaning survives a resume; the message names the escape hatch); `budget` honors a raised cap.
  The arithmetic guard itself survives verbatim as the belt-and-suspenders no-op for torn tails.
- **`died_in_plan_task` is coarse by construction.** The graph runner batches `task_result`
  emission until the executor returns, so a mid-plan death leaves `plan_admitted` with zero
  per-task rows; the declared-vs-resulted gap is all the classifier can report. Un-batching
  those events is a possible future change owned by the graph runner.

## Boundaries

The classifier reports facts and stops. It never touches `next_iter` derivation, transport
retry/backoff (`is_transport_turn_error` is not called from recovery), the kept-tree restore
(`restore_kept_best` owns putting the tree back), or grading. `TurnEvidence` (event count, last
cost, last error, dangling session cursor) is the designed input for iteration-accounting work,
not a policy.

## Consequences

Old logs lack the approval bracket, so `died_awaiting_approval` is undetectable for pre-change
runs; they degrade to `died_between_iterations`, which is exactly the old behavior. Unknown
`shutdown.outcome` / `approval_wait.mode` tokens degrade (`Other` / `continue`) rather than
error, so a newer writer never bricks an older resumer. The classifier encodes the emission
grammar (AgentStart/Done bracketing, batched task results, shutdown-last); its tests live next
to the scanner as the tripwire for reporter changes that would skew it.
