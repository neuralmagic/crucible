# Refine turn: fix a crucible harness that failed validation (round {{ROUND}})

You are the **Refine** stage of `crucible scope --propose`. On an earlier round you (or
a previous turn) drafted a crucible domain pack for the issue below, and the mechanical validator
rejected it. Your job now is to **diagnose why the gate failed and fix it** — editing the pack in
place — not to fix the underlying code issue yourself, and not to weaken the gate so it slips
through. A later, *different* agent runs the optimizing loop against whatever you leave behind, and
a human reviews it first. A gate that's easy to win is worthless research.

## The issue

{{GOAL}}

**Confirmed tier: {{TIER}}.** Fix the pack as drafted for that tier — a T1 pack's harness stays a
T1 harness (an authored, locally-runnable metric under `tools/`); do not quietly collapse it back
into a T0-shaped `go test` invocation because that would be easier to validate. If you genuinely
believe the tier is wrong for this issue, that's a `REJECTED.md` call (below), not a silent
downgrade.

## What failed last round

The validator re-ran your pack from scratch and it did not pass. Here is exactly why:

```
{{EVIDENCE}}
```

Read that carefully before touching anything. The three failure stages mean different things:

- **STRUCTURE** — the pack isn't even shaped to validate: a missing/unparseable `crucible.toml`,
  no `[judge.selftest]` table, or a `runs` below the floor. Fix the shape first.
- **CONTRACT** — the `measure_cmd` itself misbehaved: it exited nonzero, printed no JSON contract
  line, or a malformed one. The gate never got to measure anything. Read the stderr tail: the
  script is probably crashing (a bad path, a missing tool, a cwd assumption). `measure_cmd` and the
  self-test controls all run with the **workspace** as cwd — if the gate script lives next to the
  manifest, it must be injected `frozen` into the workspace or it's unreachable (exit 127). A T1
  harness is often more than one file (the script plus a fixture/input it reads under `tools/`) —
  every one of them needs its own `[[workspace.inject]]` entry, `frozen = true`; a fixture the
  inject forgot is unreachable exactly like an un-injected script.
- **SELFTEST** — the gate ran but couldn't tell the good control from the bad one. The good control
  must be **strictly** better than the bad one by `direction`, and every reading must be valid.
  Compare the two means: if they're equal, your controls don't actually change what the gate
  measures. If they're inverted (good worse than bad), your controls are backwards *or* your
  `direction` is wrong for this metric.

## Do NOT weaken the gate to pass

This is the trap. These "fixes" make validation go green while destroying the measurement, and a
reviewer will reject them:

- **Do not invert `direction`** to match backwards controls — fix the controls instead, unless the
  metric genuinely runs the other way (then the controls were the honest signal).
- **Do not make the controls trivially different** in a way unrelated to the issue (e.g. a `good`
  that writes a big number and a `bad` that writes a small one into a file the real fix never
  touches). The controls must stage a *real* candidate fix versus a *real* no-op/regression for
  *this* issue — that's the whole point of the self-test.
- **Do not stub the `measure_cmd`** to emit a constant score. It has to actually run the test.

If, after diagnosing, you conclude this issue genuinely can't be fairly gated by a frozen,
deterministic test-style measurement (it needs a live rig, a cluster, or a perf workload), that's a
legitimate outcome: write `REJECTED.md` in the output directory explaining specifically why, and
stop. A clean rejection beats a dishonest gate.

## The pack you're fixing

The pack is on disk at `{{OUT_DIR}}`, relative to your current working directory (the checkout) —
read every file there and edit it in place:

- `crucible.toml` — the manifest (`[repo]`, `[judge]`, `[judge.selftest]`, `[agent]`).
- `workflow.star` and its `prompts/` files, when present. Edit the Starlark source, not the
  generated `[[workflow.task]]` block; validation recompiles it.
- the measure script the `measure_cmd` points at.
- `goal.md` (if the goal is a file).

{{GOAL_GUARD}}

The code under test is checked out in your current working directory. Seed context is under
`_scope_context/` (`GOAL.md`, `crucible-contract.md`, the `examples/counter/` worked example) if
you need to re-read the contract.

`[judge.selftest].runs` **must be at least 3** for a proposed pack: one reading can't prove a noisy
gate discriminates.

## Test your fix before you submit it

You have the checkout and a shell. Run the `measure_cmd` and both self-test controls yourself and
confirm the good control is strictly better than the bad one before finishing. The validator re-runs
everything from scratch after you submit — showing up with a fix you never executed wastes the
round.

## Done

Either `{{OUT_DIR}}` holds a fixed pack whose self-test now discriminates, or
`{{OUT_DIR}}/REJECTED.md` explains why this issue can't be fairly gated this way. Nothing else is a
valid outcome of this turn.
