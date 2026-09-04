# ADR 0028: Retire the wide tournament

**Status:** Accepted; implemented (2026-09-04). Governance source: `gov/adr/ADR-0024`.
**Date:** 2026-09-04
**Related:** [ADR-0010](./0010-candidate-portfolios-and-search.md) (superseded), [ADR-0004](./0004-core-loop-state-model.md)
(the deep loop that remains), [work graphs](../work-graphs.md) (where breadth now lives)

## Context

ADR-0010 added a wide round before the deep loop: N parallel propose turns in per-candidate
worktrees, serial diff scoring on the shared deployment, a top-k fold, and the winner's diff
seeding the deep loop. It shipped as a work-graph template with its own manifest table
(`[search]`), two CLI flags (`--wide`, `--wide-keep`), a row phase (`"wide"`), a crash class
(`died_in_wide_round`), and a second row-accounting filter in the resume fold.

No domain pack sets `[search]`, and the loop is being refactored into one event-sourced state
machine. Every piece the wide round adds is a second copy of something the deep loop already
has: a second runner over `Row`, a second phase to classify, a second filter in every fold, and
a third `TaskKind` dispatch site. Each would have to be carried through the refactor and taught
to the approval-gate work that follows it.

## Decision

Delete the wide tournament: the runner, the template, the `[search]` table and its validation,
the `--wide` and `--wide-keep` flags, and the `engine.measure_diff` operation. The
`died_in_wide_round` arm goes with them; a plan admitted before any iteration phase now
classifies as an open plan like any other.

The wire stays readable. `phase: "wide"` remains a legal row value and `died_in_wide_round`
remains a decodable recovery-class token, because logs written before this decision carry
them; readers keep such rows out of the deep loop's baseline and best, exactly as the resume
fold did. RFC-0001 C-SEARCH is deprecated and the `[search]` rule leaves C-MANIFEST.

Breadth, when a run wants it, is authored inside the work graph: isolated plan tasks fanned out
over a list and a `top_k` reducer express the same search without a separate pre-loop stage.

## Consequences

- One runner over `Row`, one phase vocabulary, one row filter. The event-sourced refactor and
  the approval gates have one fewer shape to carry.
- A run that wants breadth authors it in `workflow.star`; no shipped template does that yet.
- Old session logs with `"wide"` rows render those rows as legacy context rather than as a
  named stage.
