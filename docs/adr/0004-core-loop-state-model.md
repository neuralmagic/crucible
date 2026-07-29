# ADR 0004: Core-loop state model — context struct + typed iteration, not whole-loop typestate

**Status:** Accepted (implemented 2026-06-25)
**Date:** 2026-06-25
**Context owner:** agentic-epp-autoresearch
**Related:** [ADR-0001](./0001-adaptive-harness.md) (freeze the judge), [ADR-0003](./0003-async-approval-waits.md)
(park/rescope/deny gates), the loop in `crucible/src/loop_driver.rs` (`run_loop`), and the
`run`/`loop_driver` split.

## Context

`run_loop` has crossed a complexity inflection. It now threads **~13 mutable bindings** across
iterations — `rows`, `spent`, `best_score`, `baseline_score`, `baseline_total`, `best_snap`,
`regime`, `fp`, `kept_shas`, `solved_any`, `escalated`, `pending_block`, `parked_total` — and exits
through **five `break`s** (stop, escalate, denied-escalate, budget, solved) plus two `continue`s
(parked-block, apply-failed). ADR-0003 added park / rescope / deny / provisioning gates on top of the
existing apply → measure → decide → keep/discard core. The next addition gets risky: the invariants
(re-baseline mutates a *coherent set* of fields together; you can't `measure` before `apply`, can't
`keep` without a `Reading`, can't measure on top of an escalation) live only in the reader's head.

A typestate pattern was proposed. This ADR decides **how much** typestate, and where — because the
naive answer (model every loop phase as a type) is the wrong shape for this loop.

## Decision

**Type the linear sub-protocol; keep a plain state machine with an explicit context for the cyclic
shell.** Concretely, three changes, in risk order:

1. **A `Run` context struct** bundling the per-run threaded state, with the segment-scoped fields
   factored into a `Segment` (because the re-scope mutates exactly those, together):

   ```rust
   struct Run {
       rows: Vec<Row>,
       spent: f64,
       kept_shas: Vec<String>,
       solved_any: bool,
       parked_total: Duration,         // ADR-0003 budget-pause accumulator
       pending_block: Option<PendingProvisioning>,  // ADR-0003
       segment: Segment,
   }
   /// Everything a re-scope replaces atomically (ADR-0001 safeguard 2 / ADR-0003).
   struct Segment {
       regime: String,
       fingerprint: String,
       baseline_score: f64,
       best_score: f64,
       baseline_total: u64,
       best_snap: Snapshot,
   }
   ```

   A re-scope becomes `run.segment = Segment::baseline(world, judge, regime)?` — one assignment that
   can't half-update the goalpost. This is the single biggest readability win and is independent of
   the rest.

2. **An explicit `LoopExit` enum** replacing the `escalated: Option<_>` flag + scattered `break`s, so
   there is one place that enumerates how a run can end, mapped to `Outcome`:

   ```rust
   enum LoopExit { Finished, Solved, Budget, Stopped, Escalated(Escalation) }
   ```

3. **Typestate the inner iteration** — the one genuinely linear protocol with real ordering
   invariants. The candidate moves Proposed → Applied → Measured, and only a `Measured` can be
   decided/kept (so "keep without a reading" and "measure before apply" stop compiling):

   ```rust
   struct Iteration<S> { it: u32, state: S }
   struct Proposed;                       // agent staged a candidate in the world
   struct Applied;                        // world.apply() succeeded
   struct Measured { reading: Reading, note: String, diff: String, diffstat: String }

   impl Iteration<Proposed> {
       fn apply(self, world: &dyn World) -> Result<Iteration<Applied>, ApplyFailed>;
   }
   impl Iteration<Applied> {
       fn measure(self, judge: &dyn Judge, ctx: &MeasureCtx, p: &Paths) -> Result<Iteration<Measured>>;
   }
   impl Iteration<Measured> {
       fn decide(self, judge: &dyn Judge, best: f64) -> (Row, Verdict);  // keep path holds the Reading by construction
   }
   ```

   The outer `for it in …` stays a loop over `&mut Run`; the gates (park / rescope / deny /
   escalation / provisioning) run before an iteration enters `Proposed`, and decide whether it does.

## What this is NOT

- **NOT whole-loop typestate** (`Loop<Parked>`, `Loop<Rescoping>`, … with `self`-consuming
  transitions). Typestate pays off when each state owns *distinct data + capabilities* (a connection,
  a builder, a resource protocol). This loop is the opposite: nearly all state is **shared and
  threaded through every phase**, so a per-phase type would carry one big shared context + a marker —
  the ceremony of typestate with none of the safety. The cyclic shell wants a state machine with an
  explicit context (#1/#2), not types-as-states.
- **NOT a behavior change.** This is a structural refactor: identical control flow, identical session
  log, identical exit codes. The existing tests (park outcomes, resume, fingerprint, exit codes) are
  the regression net and must stay green untouched.
- **NOT a new module boundary.** It stays in `loop_driver.rs` (+ maybe an `iteration.rs` sibling for
  the typestate); the `run`/`loop_driver` split is preserved.

## Consequences

- **Positive:** the re-scope can't half-update the goalpost (the `Segment` swap is atomic); the
  iteration protocol's ordering invariants are compile-enforced; `run_loop`'s signature and body
  shrink to "drive gates, then run a typed iteration, fold the result into `Run`"; the ways a run can
  end are enumerated in one enum.
- **Positive:** the next gate (e.g. the ADR-0003 capture-wait, or a new domain's phase) plugs into a
  named context + a typed iteration instead of adding a 14th threaded binding.
- **Negative / cost:** more types (the `Iteration<S>` zoo, the marker structs) and the `self`-consuming
  transitions are slightly more verbose than inline statements; worth it for the protocol, which is
  why we scope typestate to *only* the iteration.
- **Negative / cost:** a refactor of a core file — it must land as one reviewed change with the
  test suite green, not a drip of edits.

## Implementation notes

The change lands in three independently testable steps — the `Run` + `Segment` struct (threaded
bindings moved in, re-scope becomes one assignment), the `LoopExit` enum (replacing the `escalated`
flag + scattered `break`s, mapped to `Outcome`), then the `Iteration<S>` typestate (apply/measure/
decide extracted into the typed protocol) — each keeping `cargo test -p crucible` green (notably the
park/resume/exit-code tests).

- **`LoopExit::Escalated` is a unit variant, not `Escalated(Escalation)`.** The escalation is
  reported and the world rolled back *eagerly* at each break site, and the two sites report
  differently (the agent path calls `Reporter::escalation`; the denied-`block` path emits a `note`).
  Centralizing the report post-loop to consume a carried payload would change the session log, which
  this refactor must not do, so the carried value would be dead. The variant only marks the run
  "needs human" for the exit code; `Outcome.escalated = matches!(exit, LoopExit::Escalated)`.
- **`Segment::baseline(world, judge, goal, regime)` owns the fingerprint** (vs the ADR sketch's
  separate `fp` binding), so opening a segment is one call for both the initial baseline and a
  re-scope. The resume path constructs its `Segment` directly from the restored scores.
- **`Decided { row, verdict, reading }`** is what `Iteration::<Measured>::decide` returns (vs the
  sketch's `(Row, Verdict)`): the keep path needs the reading's score (→ `best_score`) and note (→
  snapshot label), so the typed step hands it back by construction.

## Open questions

1. **Where the gates live.** Do park / rescope / deny / escalation / provisioning stay inline in
   `run_loop`, or become a small `enum Gate { Park, Rescope, … }` step? Lean inline until a third gate
   makes the sequence unwieldy.
2. **`Snapshot` type.** `best_snap` is a `String` today (the world's opaque snapshot handle). The
   `Segment` struct is a good moment to newtype it (`struct Snapshot(String)`).
3. **Resume path.** `run_loop`'s resume branch reconstructs the same state; it must build a `Run` from
   `ResumeState` cleanly — confirm the `Segment` factoring doesn't complicate the replay.

## Principle, restated

Type the protocol, not the process. The iteration is a short linear contract — make its illegal
orderings unrepresentable. The loop is a long cyclic process over shared state — give that state a
name and its exits an enum, and leave the rest a plain, readable `for`.
