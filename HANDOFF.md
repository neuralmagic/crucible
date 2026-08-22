# Playbook lane: handoff

Date: 2026-08-22. Branch is still `cascade-dsl` (the rename touched the text, not
the ref). 20 commits off `main` (`7c2c1a5`). `cargo test --all` green
(954 + 153 + 39 + 62 + 14), clippy silent, `govctl check` clean. Nothing pushed.

## What a playbook is

A workflow that runs its graph **once** and produces **no score**. The lane for
work whose value is the work itself. No propose/apply/measure/decide, no frozen
judge. The contrast in one sentence: the other lane is a search, this one is a
playbook.

It was called `cascade` until today. That named the wrong property (every workflow
here is a DAG) and competed with a name ADR-0021 had already given it, "the task
lane". Three uses of "cascade" survive in the tree and mean Kubernetes cascading
deletion; leave them.

## It works. Run it

```sh
crucible plan run --manifest examples/playbook/crucible.toml \
                  --max-cost 1 --max-time 5m
```

A pack fanning out over runtime-discovered items, keyed by item:

```
discover  pass  out={"files":["legacy.py","main.py","notes.md","README.md"]}
review    fail  out={"instances":4,"passed":3,"failed":1,...}  (1 of 4 failed: legacy.py)
roundup   pass  out={"reviewed":3,"skipped":1,"total_issues":4}
plan v1: completed — verdict: valid                                    exit 0
```

Drop another file in the workspace and re-run: five instances, no code or graph
change.

## Built and tested

| | where |
| --- | --- |
| Lane admission, `type = "playbook"` | `manifest/workflow.rs`, `manifest/mod.rs` |
| Lane-scoped DSL namespace (6 constructors, not 13) | `plan/starlark/globals.rs`, `idents.rs` |
| `params` + `param()`, JSON Schema, bound pre-evaluation | `plan/starlark/params.rs` |
| External-input marking in prompts | `plan/starlark.rs` (`take_prompt`), `values.rs` |
| `over =` fan-out, keyed by item | `plan/exec.rs`, `plan/ir.rs` |
| `emits_files`, captured through isolation, staged from every ancestor | `plan/harness.rs` |
| Git memory, per task, on pass only | `plan/harness.rs` (`settled`) |
| Launcher + ceilings (`--max-cost`, `--max-time`) | `plan/cli.rs` |
| Bounded compilation (no pack can abort the engine) | `plan/starlark.rs` |
| `tools/fake-agent.py`, a deterministic stand-in | `tools/` |

## The four things most likely to be got wrong later

**Capture happens at attempt time, not at settle time.** An isolated task's
worktree is `remove_dir_all`'d the instant `run_in` returns, so a `TaskRunner` hook
called by the executor is too late: there is nothing left to copy. This was built
the wrong way round first.

**Instances are keyed by item, never by position.** Airflow keys mapped tasks on
`map_index`, which is why clearing one after its input shifted reprocesses the
wrong thing. `audit[gamma]` names the same work on every run that finds gamma,
which is what will make resume's folded results matchable.

**Per-task git memory is playbook-only.** The scored loop owns the same repository
for keep/discard of whole candidates and must not find per-task commits inside an
iteration. The runner takes the behaviour from the manifest's declared lane.

**A join looks *through* a mapped node.** A node has one status, which cannot say
"two of three survived", and that is exactly what `join = "passed"` needs.
`FanoutSummary` rides on the result. The first design claimed the existing
vocabulary sufficed; the end-to-end test disproved it.

## Open work, in the order I would take it

**`skill()` is next, and it needs a clause first.** No clause defines what a skill
task *is*, which is why it was deliberately kept out of C-WORKFLOW's constructor
list. The decision: is a skill a prompt-file indirection (read `SKILL.md`, use it
as the prompt), or does it carry tool and permission scope? The speculators pack's
`skill(name=, skill=, args={...})` suggests the latter. An hour or a day depending
which.

**Task output is not marked as external.** `param()` produces marked text; a value
from a task's JSON output or a fan-out `over` item does not. That is precisely the
discovery-to-implement path: when `discover` finds a URL and hands it to a mapped
`implement`, the URL reaches the prompt unmarked. The machinery exists
(`values::ExternalText`); the open question is where the line goes, because marking
*all* task output would mark most of every prompt and train readers to ignore the
marks.

**WI-2026-08-22-005, the rest of it.** Ceiling refusal and `plan_admitted` are
done. Outstanding: per-task deadlines (`plan/harness.rs` spawns with no timeout of
any kind), terminating an attempt already in flight, rejecting `iterations > 1`,
and deleting `examples/playbook/plan.toml`.

**WI-2026-08-22-003, `isolated` becomes `workspace`.** `isolated = True` is the
word an author writes when they mean "runs in parallel". Needs per-task
result-file namespacing first. 78 sites, reaches the scored lane.

**Unbuilt from RFC-0002:** early completion, the epilogue for a playbook, resume
folding, asks emission (the contract landed in `crucible-contract/src/ask.rs`, the
engine side did not), and `C-PLAYBOOK-COMPOSITE`.

## Decisions made, do not relitigate

- **Ceilings are the launcher's.** A source may not declare one; the engine
  refuses to dispatch without both. Cost accounting is the orchestrator's too: a
  task never reports what it spent, so `ShellRunner`'s `cost_usd: 0.0` is correct
  rather than a gap.
- **`max_fanout` is required alongside `over`** and capped by the engine at 256.
- **Only passing instances feed a join.** A null under a failed key would read as
  "ran, found nothing", which is a different claim.
- **External text is refused outside a prompt.** Nothing else can mark provenance,
  and a shell command built from outside text is an injection.
- **The engine compiles the graph, not the controller.** Confirmed against the
  controller's actual shape.
- **`errand` and `routine` were considered and rejected** in favour of `playbook`.

## The controller

`~/git/agentic-epp-autoresearch` (the name has outlived its scope). Rust,
in-cluster at `crucible-controller.autoresearch.svc:8080`, Postgres, a bors-style
queue; it launches work pods and injects their env. It consumes this repo as an
**exact git pin** in `core-pin.toml`, currently `7c2c1a5`, and CI asserts every git
dependency resolves to it.

Three findings that shape the asks work:

- **`InputKind` is the extension point** (`GitHub`/`Scenario`/`Jira`/`Unknown`).
  An ask is a fifth variant.
- **The queue already implements C-ASKS' orchestrator half.** Coalescing is
  dedupe, exponential backoff is rate limiting, park-after-N is the blacklist.
- **`Unknown { tag }` degrades to inert**, so the engine can emit ask rows against
  the running controller *before* it understands them. No flag day: land in core,
  bump the pin, teach the controller.

One tension to resolve when writing it. The queue's frozen rule is that a dequeued
key carries no payload, because the worker re-reads state; C-ASKS says an ask
carries its parameter values. The precedent is there: `Jira` fetches title and body
once at adopt time, so params are stored on adopt and the key re-reads them.

## Non-obvious things that cost time

- **`starlark` turns on `serde_json/arbitrary_precision` for the whole binary.**
  Under that flag serde's internally-tagged enums and `#[serde(flatten)]` structs
  cannot decode floats. Fixed via `crucible_contract::json::from_str`.
- **`crucible` is bin-only.** No `[lib]`, so `pub` does not exempt an item from
  `dead_code`, and the clippy guard runs `-D warnings`. Nothing lands without a
  production caller; three convenience wrappers are `#[cfg(test)]` for exactly this.
- **Compilation runs on a 256MB thread.** Dropping a value the evaluator built
  recurses once per level and cannot be refused in advance; `MAX_EVAL_TICKS` bounds
  the achievable depth and the stack is sized to that bound. Parse depth *is*
  refused in advance, because a 256KB source can nest deeper than any stack covers.
- **The parse-depth measure counts operators, not only brackets.** `not not not x`
  and `1+1+1+…` overflow with no bracket in sight. A bracket-only check catches
  four of seven shapes and looks finished.
- **`govctl` refuses to delete a referenced clause.** The rename needed the ADR and
  work-item references repointed first, and `clause new` appends to the section
  list while a sed rewrites the existing entries, so check for duplicates.
- **`otel::forwarding_mirrors_reparented_traces_and_holds_back_metrics` is flaky.**
  Three sightings, full-suite runs only, passes in isolation every time. Not ours.
