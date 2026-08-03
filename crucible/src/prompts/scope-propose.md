# Scoping turn: draft a crucible harness for one issue

You are the **Propose** stage of `crucible scope --propose` . Your job is to draft a
complete, self-contained crucible domain pack for the issue below — not to fix the issue
yourself. A later, *different* agent will run the optimizing loop against the harness you draft;
you will never see its results, and a human reviews everything you write before it runs for
real. Draft honestly: a harness that's easy to win is worthless research.

## The issue

{{GOAL}}

**Confirmed tier: {{TIER}}.** A ranker has already decided which gate shape this issue needs
before you ever saw it. Follow the section below that matches your tier — do not second-guess the
tier inside this turn; if you conclude it's flatly wrong for this issue, that's what `REJECTED.md`
is for (below), not a silent switch to a different tier's shape.

{{MEASURE_MODE}}

{{GOAL_CONTRACT}}

## What to draft into `{{OUT_DIR}}`

Write into `{{OUT_DIR}}` — a directory path relative to your current working directory, already
created for you (`mkdir -p` it if it's missing). Every file you produce must land under it:
anything written to an absolute path outside your working directory is not picked up when the
turn ends. What goes in:

1. **`crucible.toml`** with:
   - `[repo]` — copy this line VERBATIM, character for character: `{{REPO_DIRECTIVE}}`. Do NOT
     rewrite it. The path is resolved by the host-side validator, not from inside your sandbox, so
     it may look nonexistent or broken from where you stand — that is expected, not a bug. Rewriting
     it to a sandbox-absolute or relative path is the #1 cause of validation failure. Any self-test
     that needs the repo should use the checkout already in your current working directory, never
     the `[repo]` path.
   - `[judge]` — `measure_cmd` (a `go test` subset for T0, or your authored `tools/` harness for
     T1), `direction`, `objective`.
   - **`[judge.selftest]` is MANDATORY.** Write a `good_cmd` that stages a config/patch that
     should pass the gate, and a `bad_cmd` that stages one that should fail it — real negative
     controls, not a stub. A proposal without a discriminating self-test will be rejected before
     any human reads it: the controls are how you prove your gate can tell a real fix from a
     no-op. Set **`runs` to at least 3** — a single reading can't establish that a gate
     discriminates a noisy metric, and a proposed pack with `runs < 3` is rejected just like a
     missing self-test.
   - `[agent]` with a `goal` (or `goal_file`) describing the fix for the optimizing loop that
     runs later.
   - Do not hand-write `[[workflow.task]]` when authoring a workflow. Put the readable source in
     sibling `workflow.star`; the host compiler replaces the generated manifest block before
     validation and freeze.
   - **Choose a build mode deliberately** (contract §3.1). Ask: between the agent's edit and your
     `measure_cmd`, what has to happen for the edit to be the thing measured? If the gate compiles
     and runs the workspace in place (a `go test` subset — the T0 case, and most T1 cases), the
     answer is *nothing*: omit `apply_cmd` entirely. Only reach for a build when the candidate must
     become a running image, and say so explicitly in your proposal. A pack that silently inherits
     a build it does not need pays that cost on every single iteration.
2. **The measure script** the `measure_cmd` points at — a `go test` wrapper for T0, or your
   authored harness (and every fixture it reads) under `tools/` for T1. Executable, contract-
   shaped stdout, and every file it touches frozen-injected (see above).
3. **`goal.md`** (or inline `goal` in `[agent]`) — the goal for the solver, written to the
   **`goal.md` contract above**.
4. **Optional `workflow.star` and prompt files** — use this when the domain benefits from an
   authored task graph: parallel critics, synthesis, early rejection, or visible measurement.
   Topology is explicit: `propose`, `apply`, `measure`, and `decide` are task constructors, while
   the engine retains their authority. Finish with `workflow(type = "autoresearch", tasks = [...],
   result = decision)`; use `type = "custom"` only when the outer orchestrator supports it.
   `default_autoresearch([...])` expands the legacy four-stage flow. For visible measurement, use
   `evaluate` plus `grade(evidence = [...], score = primary)`; opaque `measure` remains valid.
   `prompt_file("prompts/x.md")` embeds a regular UTF-8 pack-relative file; absolute paths, `..`,
   and symlinks are rejected. Isolated agents are concurrent read-only worktrees, so do not
   isolate a synthesizer whose edits must survive. Each agent writes one JSON object to
   `PLAN_TASK_RESULT.json`. Validation writes the admitted graph to `WORKFLOW.png`; do not supply
   the image yourself.

## Test your own draft before you submit it

You have the checkout and a shell: run your draft `measure_cmd` and both self-test controls
yourself before finishing. This turn is not adaptive-forever — a mechanical validator re-runs
everything from scratch after you submit, and a human approves before anything spends loop
budget — but showing up with a gate you've never executed wastes both of theirs. If your
self-test doesn't discriminate, fix it or reconsider the gate shape, don't submit it anyway.

Your sandbox may not have `git`. Don't burn turns installing it or hunting for it — either write
self-test controls that don't assume `git` is present, or accept that `git`-dependent controls are
exercised by the host-side validator, which does have it.

## Done

Either `{{OUT_DIR}}` holds a complete pack (`crucible.toml` with `[judge.selftest]`, the measure
script, the goal, and any authored `workflow.star` plus prompt files), or
`{{OUT_DIR}}/REJECTED.md` explains why this issue can't be fairly gated this way. Nothing else is
a valid outcome of this turn.
