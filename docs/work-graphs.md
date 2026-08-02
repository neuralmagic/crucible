# Work graphs

A **plan** is a versioned DAG of **tasks** that a deterministic executor runs. Tasks are agent
turns, plan-authored commands, or engine-owned reducers. The executor owns advancement: a task never
decides what runs next.

Today, the engine supplies a default loop graph and a wide-tournament template, and a human or
pack can supply TOML or JSON through the `plan` CLI. Workflow admission separates authorable
topology from authority: an orchestrator must advertise the workflow type and engine operations
it can safely execute.

## Running a plan

```sh
# compile and print it, without executing
crucible plan show --file plan.toml
crucible plan show --file plan.toml --mermaid      # flowchart source
crucible plan show --file plan.toml --render       # PNG, inline if the terminal supports it

# execute: command tasks run as subprocesses, agent tasks through the real harness
crucible plan run --file plan.toml --manifest crucible.toml

# execute without a manifest: agent tasks run a stand-in command instead
crucible plan run --file plan.toml --agent-cmd ./role.sh
```

`--cap <name>` (repeatable) declares what the substrate can do; see *needs* below.

`plan run` exits nonzero when the plan does not reach a valid verdict.

## File format

```toml
version = 1                 # the only format version accepted today
# reason = "..."            # reserved for a future replan protocol

[budget]
usd = 5.0                   # required, positive; execution fails closed on overrun

[[task]]
name = "propose"            # unique within the plan
kind = "agent"
prompt = "..."
model = "claude-opus-4-6"   # optional per-task overrides of the manifest's [agent]
harness = "claude"
effort = "high"
session = "solver"         # optional durable logical conversation

[[task]]
name = "measure"
kind = "command"
command = "./bench.sh"
depends_on = ["propose"]
needs = "gpu"               # default "any"
required = true             # default true
isolation = "worktree"      # optional
join = "all"                # default "all"

[[task]]
name = "pick"
kind = "top_k"
k = 1
direction = "lower"         # or "higher"
depends_on = ["measure"]
```

### Task kinds

| Kind | What it runs |
| --- | --- |
| `agent` | One agent turn. `harness` / `model` / `effort` override the manifest's `[agent]` defaults per task; `session` opts into durable continuation. |
| `command` | A plan-authored command, with the same output contract as `measure_cmd`. |
| `top_k` | Engine-owned reducer: keep the `k` best inputs by their `score` field. Needs at least one dependency. |
| `engine` | Capability-owned operation (`propose`, `apply`, `measure`, `decide`, or `measure_diff`). Only an admitting orchestrator can execute it. |

Serialization is not authority. A workflow may author and sequence engine nodes, but admission
requires matching capabilities such as `workflow.autoresearch`, `engine.apply`, and
`engine.measure`. The generic plan runner rejects them because it owns neither a `World` nor a
frozen `Judge`.

A command string is not a trust boundary by itself. If it invokes a script that must remain
trusted after an agent task edits the workspace, declare that script as a frozen
`[[workspace.inject]]` in the manifest. The plan runner restores frozen injects in the task's
actual workspace before every task, including isolated worktrees.

A logical `session` is serial state, so every pair of tasks sharing one must have a dependency
path between them. A session task cannot use disposable worktree isolation. The public ledger keeps
only an opaque harness cursor and completed-turn count; private reasoning is never copied into the
plan or public session log. Claude Code's native transcript remains in its private local store or a
mode-0600 engine store restored into each fresh OpenShell sandbox. Omit the field for the historical
fresh-turn behavior.

### Task output

A task's output is JSON and becomes its dependents' input.

- `command`: the last non-empty stdout line. Nonzero exit is a measured failure; a spawn
  failure is a transport failure. Upstream outputs arrive as `CRUCIBLE_INPUTS` (a JSON object
  keyed by task name), plus `CRUCIBLE_TASK`.
- `agent` under `--manifest`: the turn writes a single JSON object to `PLAN_TASK_RESULT.json`
  in the workspace root. A missing file after a normal turn is a measured failure; an explicit
  spawn, harness, or stream error is a transport failure and follows the retry policy.
- `agent` under `--agent-cmd`: the stand-in receives `CRUCIBLE_PROMPT`, `CRUCIBLE_HARNESS`,
  `CRUCIBLE_MODEL`, `CRUCIBLE_EFFORT`, and returns JSON on its last stdout line.

`top_k` reads a finite numeric `score` from each input, so an upstream task that wants to rank
must emit one.

## Execution semantics

**Readiness.** A task dispatches when its dependencies are terminal and its join is satisfied.
Dispatch order is declaration-stable, so the event stream is deterministic.

**Truncation is fail-closed.** If a `required` task can never run on this substrate (its
`needs` is not in the declared caps, or a dependency cannot run), the whole plan is truncated
and *nothing* is dispatched. A truncated DAG cannot produce an honest pass. An advisory task
in the same position is skipped, along with its dependents, and validity is unaffected.

**Failure.** A `required` task that fails short-circuits the plan; everything undispatched is
blocked. An advisory failure blocks only its dependents.

**Retry is not recheck.** Transport failures retry, bounded (2 by default). A measured failure
never reruns: a task that failed, failed.

**Budget.** Cost is known only after an attempt completes, so an in-flight attempt may report a
total above `budget.usd`. Any overrun invalidates the plan and blocks all further dispatch and
retries. Reaching the budget exactly is valid only when no further retry or task is needed.

### `needs`

The substrate capability a task requires. `"any"` runs everywhere. Anything else must be
declared with `--cap`, otherwise the task is unrunnable, and `plan show` reports the truncation
verdict before you spend anything.

### `isolation`

`isolation = "worktree"` gives the task a private clone of the workspace, including its
uncommitted state. Two effects:

- Tasks isolated this way and ready at the same time run **concurrently**. Without isolation
  they would collide on the single `PLAN_TASK_RESULT.json` in the shared workspace.
- The task's edits are **discarded**. What leaves is its declared output, so this is for
  review and analysis work, not for a task whose diff has to survive.

A runner that cannot isolate refuses the task rather than silently running it in the shared
workspace.

### `join`

`join = "all"` (default) requires every dependency to pass. `join = "passed"` dispatches once
every dependency is terminal and folds only the ones that passed. Use it for a reducer over a
lossy fan-out, or for a gate over reviewers where one being advisory must not stop the run.

## The loop as a plan

`--graph-loop` runs each loop iteration as a capability-admitted `autoresearch` workflow. With no
authored workflow, the default expands to ordinary tasks:

```
propose (engine) -> apply -> measure -> decide
```

Task names and intervening topology are author-defined. An `autoresearch` result must be a decision
fed by a frozen measurement with apply and proposal ancestors. A `custom` workflow has no such
shape requirement; the outer orchestrator only admits it when it advertises `workflow.custom`.

```toml
[workflow]
type = "autoresearch"
result = "keep-if-better"

[[workflow.task]]
name = "invent"
kind = "engine"
op = "propose"
session = "solver"

[[workflow.task]]
name = "review"
kind = "command"
command = "./review.sh"
depends_on = ["invent"]

[[workflow.task]]
name = "deploy-preview"
kind = "engine"
op = "apply"
depends_on = ["review"]

[[workflow.task]]
name = "benchmark"
kind = "engine"
op = "measure"
depends_on = ["deploy-preview"]

[[workflow.task]]
name = "keep-if-better"
kind = "engine"
op = "decide"
source = "benchmark"
depends_on = ["benchmark"]
```

The corresponding admission needs `workflow.autoresearch`, `engine.propose`, `engine.apply`,
`engine.measure`, `engine.decide`, and—because it binds `solver`—`agent.session.persist`. A custom orchestrator can instead admit `type = "custom"`
and any subset of operations it implements. Task-level `needs` still controls where an admitted
task can run; workflow capabilities control what the orchestrator is authorized to mean.

Same decisions and same session events as the default path, plus additive `plan_admitted` and
`task_result` lines. Cross-round state, keep/discard, and every between-round control (parking,
steering, re-scoping, budget) stay with the driver. The wide round runs as a template compiled
from `[search]` on both paths.

The templates carry no budget of their own: the run budget is the driver's, checked between
rounds, so a turn that overruns the cap is still measured and decided.

Default off while it soaks.

## Worked example

`examples/adversarial-review` puts a review task between a code node and the gate below it, in
single-reviewer and two-reviewer panel shapes. The panel runs isolated reviewers concurrently
and joins them on a policy gate: correctness blocks, copy-edit is advisory. It runs free
against a stand-in manifest or against real models with the live one.
