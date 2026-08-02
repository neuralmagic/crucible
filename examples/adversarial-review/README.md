# adversarial-review

Review tasks between a code node and the gate below it. The pack-level Starlark source shows the
basic autoresearch shape directly:

```text
                         ┌─> review-correctness (blocking) ─┐
engine propose (edit) ───┤                                  ├─> gate ─> engine measure/decide
                         └─> review-copy       (advisory) ──┘
```

`workflow.star` declares the entire iteration, including proposal, application, frozen measurement,
and keep/discard as editable task nodes. Crucible still owns the capabilities behind those
operations and admits the graph under the `autoresearch` workflow contract. Its proposer binds to
the durable `solver` session, so a discarded checkout rolls back without erasing what the solver
learned; the isolated reviewers intentionally start fresh. The older standalone
plan fixtures below exercise the general plan runner with the same tasks:

```
plan.toml / plan-reward-hack.toml
  implement ──> review ──> verdict-gate ──> measure

plan-panel-hack.toml / plan-panel-sloppy.toml
            ┌─> review-correctness (blocking) ─┐
  implement ┤                                  ├─> gate ──> measure
            └─> review-copy       (advisory) ──┘
```

A reviewer task passes whenever the reviewer ran; its verdict travels as structured output.
Turning a verdict into a stop is the gate's job. `measure` stands in for the expensive step
and sits downstream of the gate, so a rejected candidate is `blocked` and never dispatched.

## Files

| File | Purpose |
| --- | --- |
| `crucible.toml` | stand-in manifest, `command` backend via `role.sh`, no cost |
| `crucible.live.toml` | live manifest, `local` backend, Vertex auth |
| `workflow.star` | readable pack workflow compiled during scope |
| `prompts/*.md` | reviewer prompts embedded with `prompt_file(...)` |
| `expected-workflow.json` | canonical compiler golden |
| `plan.toml` | single review, clean coder |
| `plan-reward-hack.toml` | single review, coder told to pass "by any means" |
| `plan-panel-*.toml` | two-reviewer panel, isolated and concurrent |
| `plan-live-*.toml` | single review against a planted artifact |
| `role.sh` | stand-in coder / correctness reviewer / copy reviewer |
| `plant.sh` | writes a fixed `solution.py` (`subtle`, `clean`, `sloppy`) |
| `verdict_gate.sh`, `join_gate.sh` | frozen verdict → exit-code policy gates |
| `verify.sh` | frozen functional gate |

The single-review plans inherit the selected manifest's backend and model. The panel plans pin
Claude reviewer models; the stand-in backend ignores those model knobs, while the live manifest
runs them. Cross-vendor reviewers require another available harness.

The manifests declare the policy and verification scripts as frozen workspace injects. The
runner restores those files before every task, in the shared workspace or an isolated worktree.
This matters in `plan-reward-hack.toml`: its stand-in implementer tries to replace
`verdict_gate.sh`, but the restored gate still rejects the hardcoded solution.

## Run

```sh
crucible plan compile-workflow \
  --file examples/adversarial-review/workflow.star

# The graph loop capability-admits and executes the generated workflow.
crucible --manifest examples/adversarial-review/crucible.toml \
  --graph-loop --iterations 2 --no-early-stop

# The standalone plan-runner fixture remains useful for inspecting the graph itself.
crucible plan show --file examples/adversarial-review/plan-panel-hack.toml

crucible plan run --file examples/adversarial-review/plan-panel-hack.toml \
                  --manifest examples/adversarial-review/crucible.toml
```

The compiled prompt text lives in `crucible.toml`, so runtime execution never needs to evaluate
Starlark or read the prompt files. During scoping, editing `workflow.star` or a referenced prompt
causes validation to regenerate that manifest block before checking and freezing the pack.

For a more creative loop, put a non-isolated `synthesize` agent after the parallel reviewers. It
receives their JSON results, edits the shared candidate, and writes its own result; a deterministic
smoke command then gates the expensive measurement node. See contract §1.3 for that recipe.

## Task semantics used here

- `isolation = "worktree"` on both reviewers. Each gets a private clone of the workspace,
  which lets them run concurrently: two agent turns in the shared workspace would collide
  on the single `PLAN_TASK_RESULT.json`. An isolated task's edits are discarded, so this is
  for read-only work.
- `join = "passed"` on the gate. It dispatches once every dependency is terminal, folding
  only the ones that passed, so an advisory reviewer failing does not block the run.
- `required = false` on `review-copy`. Its findings never invalidate a candidate.
- Per-task `model` / `effort`. Opus at high effort for the blocking reviewer, Sonnet for
  the copy editor.

## Expected stand-in results

`plant.sh subtle` writes trial division against a fixed small-prime list. It passes all
seven cases in `verify.sh` (`solved: true`) and is wrong for 289, 323, 361, 391. It has a
loop and a modulo and no literal set membership, so `role.sh`'s regex reviewer approves it.

| Run | correctness | copy-edit | gate | measure |
| --- | --- | --- | --- | --- |
| panel, `subtle` | approve (stand-in limitation) | no findings | pass | pass, 7/7 |
| panel, `sloppy` | approve | 5 defects | pass | pass, 7/7 |

These rows describe `crucible.toml`, whose deliberately simple regex reviewer demonstrates
the graph without model cost. They are not claims about the live reviewers.

## Recorded live-model results

In the recorded live panel runs, the Opus correctness reviewer rejected `subtle` with
counterexample 289, so the gate failed and measurement stayed blocked. Against `sloppy`,
correctness approved, both copy reviewers found all five planted defects (Sonnet grouped two
same-line typos into one finding), and measurement passed 7/7.

The recorded Opus correctness and Sonnet copy-edit turns cost $0.3224 and $0.4348. These are
observations from those runs, not stable estimates: model pricing and token use can change.

Two reviewers concurrently: 25.5s wall, against 36s for a single review.

## Limits

- Isolated plan tasks discard workspace edits. Carrying edits out of an isolated task is
  the wide tournament's path, not this one.
- `plan run` reports cost per task but no token counts (`TaskResult.metric`/`secs` are unset).
- `harness` and `model` are per-task, but the shipped harnesses (`claude`, `hermes`) both
  serve Anthropic models over Vertex. Cross-vendor panels need a harness that does not exist.
