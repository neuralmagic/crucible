# cascade

The litmus domain for one-shot, measurement-free workflows: a graph that runs once, keeps no
score, and either reaches a valid verdict or does not.

```text
                                  ┌─> audit-headings  (isolated) ─┐
  draft ──> shape ──> polish ─────┼─> audit-bullets   (isolated) ─┼─> roundup
 (agent)  (command)  (agent)      └─> audit-freshness (advisory) ─┘   join = "passed"
    └────── session "scribe" ──────┘
```

Nothing here is proposed, applied, measured, or decided. `draft` writes release notes from an
inbox, `shape` refuses to let a miscounted draft reach the second turn, `polish` titles the
file in the *same* agent conversation, three auditors read the result in private clones, and
`roundup` folds whoever reported. The deterministic `command` backend (`role.sh`) stands in
for every agent turn, so the whole graph runs in under two seconds with no model and no cost.

## Run

```sh
crucible plan run --file examples/cascade/plan.toml \
                  --manifest examples/cascade/crucible.toml
```

Expected, exactly:

```text
[audit-freshness] inbox/.last-release is missing: cannot date the entries
  draft                pass       attempts=1 cost=$0.0000  out={"entries":3}
  shape                pass       attempts=1 cost=$0.0000  out={"bullets":3,"declared":3,"matches":true}
  polish               pass       attempts=1 cost=$0.0000  out={"continued":true,"session":"scribe","titled":true}
  audit-headings       pass       attempts=1 cost=$0.0000  out={"findings":[],"topic":"headings"}
  audit-bullets        pass       attempts=1 cost=$0.0000  out={"findings":[],"topic":"bullets"}
  audit-freshness      fail       attempts=1 cost=$0.0000  (turn ended without writing PLAN_TASK_RESULT.json — nothing to grade)
  roundup              pass       attempts=1 cost=$0.0000  out={"findings":[],"reporting":["audit-bullets","audit-headings"],"silent":["audit-freshness"]}
plan v1: completed — spent $0.0000 of $1
verdict: valid
```

Exit 0. Re-running without deleting `workspace/` and `state/` prints the same rows: every
stand-in rewrites what it owns.

The graph in its authoring syntax, and its checked-in compiler golden:

```sh
crucible plan compile-workflow --file examples/cascade/workflow.star
crucible plan show --file examples/cascade/plan.toml --mermaid
```

`--agent-cmd ./role.sh` is the other stand-in spelling, and it cannot run this pack: that
runner has no workspace to clone, so it refuses the three isolated auditors, and no session
ledger, so `polish` reports `continued: false`. Isolation and durable sessions need
`--manifest`, whose `[agent]` still resolves to `role.sh` and still costs nothing.

## Files

| File | Purpose |
| --- | --- |
| `crucible.toml` | judge-free manifest, `command` backend via `role.sh`, no cost |
| `plan.toml` | the runnable graph, task for task the same as `workflow.star` |
| `workflow.star` | the same graph in the authoring syntax, fan-out built by a `def` and a `for` |
| `expected-workflow.json` | canonical compiler golden |
| `prompts/*.md` | turn prompts embedded with `prompt_file(...)` |
| `inbox/*.md` | three one-line change entries, the drafter's input |
| `role.sh` | stand-in drafter / polisher / auditor, branching on `CRUCIBLE_PROMPT` |
| `shape.sh` | the gate between the two scribe turns |
| `roundup.sh` | the lossy join over the auditors |

## What each piece is here to show

- **A session across tasks.** `draft` and `polish` both declare `session = "scribe"`, with a
  command task between them. The engine hands the second turn the same conversation, which
  the stand-in proves by comparing `CRUCIBLE_AGENT_SESSION_ID` against the one `draft`
  recorded: `continued: true`. Sessions must be dependency-ordered, and cannot be isolated.
- **A command task gating agents.** `shape` reads `draft`'s declared `entries` out of
  `CRUCIBLE_INPUTS` and counts the bullets on disk. A nonzero exit is a measured failure, and
  `polish` sits downstream, so a miscounted draft never buys a second turn.
- **Isolated peers.** The three auditors are `isolation = "worktree"`, so the executor
  dispatches them as one concurrent batch, each against a private clone carrying the shared
  workspace's uncommitted state. An isolated task's edits are discarded; only its result
  leaves, which is why they are read-only reviewers.
- **`join = "passed"`.** `roundup` waits for all three auditors to reach a terminal state and
  receives only the ones that passed. Under the default `join = "all"` a single advisory
  failure would block it.
- **An advisory task.** `audit-freshness` is `required = false` and fails on purpose: it wants
  a `inbox/.last-release` cut line this pack never ships, so it writes no
  `PLAN_TASK_RESULT.json` and the turn has nothing to grade. It blocks nothing downstream of
  itself and does not touch the verdict. Make it required and the run exits nonzero.
- **Declared outputs.** `draft` promises `entries`, each auditor promises `findings`. A
  passing attempt whose JSON omits a promised field is a failure, which is what lets `shape`
  and `roundup` read those fields without defending against their absence.
- **The new authoring grammar.** `workflow.star` builds the fan-out with a `def` macro over a
  literal `(topic, blocking)` list, which is the whole reason the compiler took `def`, `for`,
  and tuple unpacking. Adding a fourth auditor is one line.

## Why it runs on the plan runner

The cascade lane does not exist yet. A judge-free manifest rejects a `[workflow]` block today,
so `crucible.toml` carries none and `plan.toml` is the executable form. When the lane lands,
`workflow.star` becomes the authority: `type = "custom"` becomes `type = "cascade"`, the block
materializes into the manifest, and `plan.toml` goes away. A test compiles `workflow.star`,
checks it against the golden, and asserts its tasks equal `plan.toml`'s, so the two forms
cannot drift in the meantime.

## Limits

- `plan run` gives no per-task wall time or token count; `cost_usd` is all it accounts.
- A run's verdict is the plan runner's: every required task passed and the graph completed.
  The cascade lane's own vocabulary — early completion, epilogue tasks, the shutdown outcome —
  has nowhere to land here yet.
- The stand-in auditors are regex-grade. They demonstrate the graph, not review quality.
