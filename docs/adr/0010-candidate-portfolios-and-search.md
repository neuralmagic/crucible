# ADR 0010: Candidate portfolios — explore/exploit search over reviewable candidates

**Status:** Superseded by [ADR 0028](./0028-retire-the-wide-tournament.md) (2026-09-04). Was implemented (v1, 2026-07-02: parallel propose turns + serialized measurement + top-k ranking behind `--wide N` / `[search]`; later re-implemented as a [work-graph](../work-graphs.md) template compiled from `[search]`). The wide round, `[search]`, and `--wide` no longer exist; breadth is authored as a fan-out in the work graph.
**Date:** 2026-06-28
**Related:** [ADR-0004](./0004-core-loop-state-model.md) (the core loop is a *sequential* refinement of one
line; this generalizes it to a population), [ADR-0009](./0009-composite-domains.md) (a composite candidate
is a tuple of per-component edits; the portfolio is a set of such tuples), [ADR-0001](./0001-adaptive-harness.md)
(the frozen judge is what makes a portfolio *rankable* instead of a taste test), [ADR-0002](./0002-mediated-provisioning-mcp.md)
/ [ADR-0003](./0003-async-approval-waits.md) (a candidate surfaces as a reviewable draft PR; a composite
candidate as a *set of linked* PRs), [ADR-0005](./0005-engine-side-builds.md) (deploy-to-rank: a candidate
that changes the rig can only be scored once the broker builds+deploys it).

## Context

The loop today (ADR-0004) is a single line of descent: iteration N refines iteration N−1, `--iterations N`
turns deep. That is pure **exploit** — it makes *one* approach better. Running the composite P/D
rollout-protection goal one-shot (`--iterations 1`, openshell backend) showed two things at once:

1. **One-shotting the right *shape* works.** Cold, in one turn, claude found that
   `compute_nixl_compatibility_hash()` already exists in vLLM, exposed a P/D config-hash, and added an EPP
   filter pairing only matching prefill↔decode — the correct cross-cut architecture, reasoned from scratch.
2. **A single shot is one sample from a wide space.** The same goal has *architecturally distinct*
   solutions with very different blast radius, and the model "knows" several of them. Betting the whole run
   on the first one it picks throws away the others — and "which is best" is not an eyeball question.

The competing approaches for *this* goal, concretely (each a legitimate, different candidate):

| # | Approach | vLLM change | Character |
| - | -------- | ----------- | --------- |
| 1 | metrics-scrape: vLLM exposes the hash as a Prometheus metric, EPP reads+filters | yes (metric) | proactive, idiomatic |
| 2 | pod-label: stamp the hash as a k8s label, EPP filters on the label | small | proactive, no scrape |
| 3 | EPP-only: derive compatibility from attrs the EPP already sees (model + kv-transfer args) | none | proactive, lowest blast radius |
| 4 | sidecar handshake: push the check into the routing sidecar | small | proactive, earlier reject |
| 5 | generation-affinity: pair deterministically by a gen label | none | crude; arguably moves the goalpost |
| 6 | reactive dead-peer: on a distinct handshake-failure status, mark that pair "dead" + evict (TTL) | tiny (status code) | reactive, generalizes to *any* incompatibility |

These are not refinements of each other — they are different *families*. And the frozen gate
(`pd-rollout-bench`) can actually **rank** them, including behaviorally: a proactive filter jumps to ~100%
success immediately; the reactive dead-peer *ramps* as it learns, so its score depends on how long the
fixed workload runs. That difference is *measured*, not argued.

## Decision

Generalize the loop from a single line of descent to a **candidate portfolio** with two explicitly
different search modes, ranked by the frozen judge, and surfaced as reviewable PRs.

> **Design review amendments (2026-07-01, ahead of implementation).** The approved v1 design pins:
> propose turns run in **parallel** (thread-per-candidate, worktree-per-candidate under the run's
> state dir) while **measurement serializes** on the shared rig by staging each candidate's patch
> through the main workspace (no World/Judge trait changes); the wide→deep hand-off is a pluggable
> **`SearchPolicy`** (`[search].policy`, v1 ships `top-k` only; round-robin / successive-halving are
> future impls — top-k-after-one-measurement picks winners from a single noisy reading, which those
> policies fix by re-measuring survivors each round); session-log rows gain an additive
> `phase: "wide"` field; and `[search].approaches` is **required** when `wide > 0` — no
> auto-generated fallback, per §3's engineered-diversity rule.

**1. Two investigation strategies, not one knob.** Iteration count is not the axis; *strategy* is:

- **Wide / explore.** N independent candidates in parallel, each a *different angle* (method-prompt biased
  to a distinct approach from the table above), each with **minimal context** — "implement a minimal version
  of approach X." Each runs in its own sandbox, so the fan-out costs ~one turn of wall-time. Goal: discover
  which *families* clear the gate at all.
- **Deep / exploit.** Take the gate's top-k and iterate them sequentially with **full context** — the prior
  diff, its measured score, and *what specifically failed* — "make this actually solve it." This is ADR-0004's
  loop, unchanged, applied to a seeded starting point.

The method prompt differs by mode; the difference *is* the strategy.

**2. Explore → exploit (a tournament with a refinement final).** The default run is: a wide round → rank by
the gate → a deep round on the winner(s). The wide round **seeds** the deep round. Today's engine only does
the deep half; this ADR adds the wide half and the hand-off. (A pure-wide or pure-deep run remains valid for
the degenerate cases.)

**3. The judge is the ranker (ADR-0001).** A portfolio is only better than guessing because the frozen gate
gives an objective, reproducible score per candidate. No candidate is "picked" by review aesthetics; review
chooses *among gate-ranked* options.

**4. Candidates are reviewable artifacts (ADR-0002/0003), composite-aware (ADR-0009).** Each candidate is
published as a draft PR per edited component, **cross-linked** (the P/D candidate → one PR on the vLLM fork +
one on the EPP fork). The PR is based on the *exact sha the agent edited against* (reachable in the fork's
object network), so it is immune to the fork having diverged from upstream. A portfolio is therefore a set of
PR-sets a human can diff side-by-side while the gate orders them.

**5. Deploy-to-rank is the gate to real scores (ADR-0005).** A candidate that changes the rig (vLLM + EPP) is
only scored after the multi-backend broker builds+deploys it. Until then a candidate is *captured and
reviewable* (its diff + PRs) but scores at baseline. Ranking a portfolio by true fitness requires the deploy
path; reviewing it does not.

## Consequences

- The loop gains a population and a `(wide, deep)` schedule; ADR-0004's `Segment`/`Iteration` typestate
  becomes the *deep* leg, and a new fan-out drives the *wide* leg over the same diff-capture + PR-publish
  plumbing (`World::staged_diff`, the native `publish.rs` single- and multi-fork draft-PR path).
- Cost scales with breadth: N wide candidates = N sandboxes (parallel) + N deploys to rank. Breadth is a
  budget knob, not free.
- Diversity must be *engineered* (distinct method prompts / forced approaches); N identical turns are not a
  portfolio. Prompt-diverse fan-out beats one-turn-self-enumerate because the latter shares the model's first
  framing.
- "Solved" is still the gate's call, now over the best of a population rather than the end of a single line.

## Alternatives considered

- **Single-line refinement only (status quo).** Simplest, but bets the run on the first approach and has no
  way to compare families — exactly what the one-shot experiment exposed.
- **One turn self-enumerates and implements K approaches.** Cheaper than fan-out, but the K attempts share one
  context and one framing, so they cluster; weaker diversity for the same review/rank cost.
- **Human picks the approach up front.** Throws away the gate's whole reason to exist (objective ranking) and
  the model's ability to surface approaches a human wouldn't enumerate (e.g. #6 reactive dead-peer).
