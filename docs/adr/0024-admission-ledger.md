# ADR 0024: Admission ledger for external inputs

**Status:** Implemented
**Date:** 2026-08-04
**Related:** [ADR-0003](./0003-async-approval-waits.md) (the approval path whose crash window this
closes), [ADR-0023](./0023-recovery-classification.md) (the resume gate this plugs into),
ADR-0016 (session.jsonl, which this deliberately does NOT extend).

## Context

Every external input into a run was volatile, unrecorded, or both:

- **Steer**: the bridge appended raw text to `STEER.md` and the loop consumed it by
  read-then-blank. A pod that died after the blank but before the turn finished lost the steer
  with no record it had ever existed. A redelivered PR comment (bridge reconnect, a second
  `watch-pr --once`) steered twice: the watcher's dedupe was an in-memory `HashSet` that died
  with its process.
- **Approve / deny / rescope**: in-memory `Mutex` slots. A second `rescope` before the drain
  silently overwrote the first. A double `approve` got `"no pending approval"` instead of
  converging. A pod death while parked lost the approval entirely — the provisioning marker was
  already consumed, so nothing could rebuild it.
- **set-budget / pause**: in-memory levels; a resumed run silently reverted to `--max-cost` and
  un-paused itself.
- No command carried an idempotency key, so every redelivery was a fresh, unrecorded mutation.

## Decision

A second append-only NDJSON file, `state/admissions.jsonl`, with a two-state machine per
idempotency key: exactly one `admitted`, then at most one `settled` (first terminal outcome
wins). Adapted from flue's `AgentSubmissionStore` (idempotent admission keyed by submission id,
durable admission *before* effect, first-terminal-state-wins settlement), in crucible's
single-process NDJSON idiom.

Three rules:

- **Admission precedes effect.** Nothing mutates `ControlState`, the steer queue, or the STOP
  flag until the `admitted` line is fsync'd. The one exception is `stop`/`abort`: refusing to
  stop a running loop over a disk error is worse than a missing record, so they apply anyway and
  reply `"unrecorded":true`.
- **Idempotency converges.** Same key + same payload returns the original admission (`dup:true`,
  plus the settled outcome if it has one) and writes nothing; same key + different payload is a
  conflict, refused with nothing written.
- **The ledger is authoritative for operator inputs; the session log for loop wait-state.**
  On resume the ledger is read first, and a re-scope it holds under the key derived from the
  parked ask suppresses ADR-0023's re-park (that grant already landed; parking would idle on an
  approval that already happened). The decision lives in one function, `recovery::resume_approval`,
  next to `plan_recovery`.

Supporting calls:

- **Derived grant keys.** An `approve` converts into a re-scope admitted under
  `rescope-from:approve:<trace_id>` — derived from the *ask*, not from the approving command. Two
  operators approving the same ask converge on one grant, and a resume can recognize the grant
  that belongs to the approval its log left dangling. This is what closes the
  admit-to-convert crash window that was unrecoverable before.
- **Settlement is keyed to turn completion, not to the drain.** A steer batch settles only after
  a turn actually started; a turn that died in transport leaves the batch owed, so the re-run of
  that iteration re-delivers it. `applied` for a steer means *delivered into a prompt*, not
  heeded, and a steer carried by an iteration that was discarded is still applied.
- **The low-level file mechanics live in `forge::ndjson`** (flock-guarded append with optional
  fsync, torn-tail-tolerant fold, quarantine of an unreadable file, and a heal of a half-written
  last line so the next record isn't glued onto it). The broker's step ledger reuses it; the
  domain semantics stay with each owner.
- **No session-log mirror.** Projecting admissions into `session.jsonl` was cut: the ledger is
  already declared authoritative, so a mirror is pure visibility and its divergence risk (a
  crash between the two writes) buys nothing today. The loop still notes an injected steer, and
  bridge replies carry the key, `dup`, and the settled outcome.

## Boundaries

Iteration accounting, transport retry, the kept-tree restore, grading, decide semantics, and
publish are untouched. The steer settle sits at the existing `IterStep` join and reads only
"did a turn start"; `set-budget` still flows through the existing `live_max_cost` slot and
`over_budget` is unchanged.

## Consequences

The bridge no longer writes `STEER.md`, so tooling that read that file as an observability
window now sees only the file channel (`watch-pr --reseed`, a manual `echo >>`); the ledger is
where a steer's fate is recorded. Two durable logs can disagree after a crash between them,
which is why one of them is declared authoritative rather than both being merged. Each admission
costs an `fsync`, negligible at human input rates and unrate-limited exactly as before. The file
is never compacted, the same unbounded-growth property `session.jsonl` already has. `STEER.md`
blocks are still admitted under generated keys, so a reseed file appended twice with the same
comment steers twice; keying the file channel by the marker's `id=` is the follow-up that closes
it (the live bridge path, which is where redelivery actually happens, is keyed today).
