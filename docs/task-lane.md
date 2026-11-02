# Tasks: the no-judge lane

The autoresearch loop needs an objective, but some jobs have no objective. The **task lane**
runs the same loop (sandbox, broker, session log, publish, resume) for work without a score:
consolidate the open dependabot PRs, fix flaky tests every night, regenerate docs, triage an
issue backlog. You need one manifest. You do not need Rust or a gate script.

To use the task lane, omit `[judge]`. No other setting is necessary
([ADR-0026](./adr/0026-no-judge-task-lane.md)).

## The minimal manifest

```toml
[repo]
url = "https://github.com/you/repo"

[agent]
backend = "local"        # or "openshell" for the sandboxed path
model   = "claude-opus-4-6"
goal    = """
Consolidate the open dependabot PRs into a single branch.
Resolve conflicts, run the test suite, and write a summary to RESULTS.md.
"""

[publish]
pr_repo = "you/repo-fork"   # kept commits ship as one draft PR
```

Run it:

```sh
crucible check --manifest task.toml     # validates; prints the task-mode notice
crucible --manifest task.toml --iterations 3
```

Each iteration is one agent turn. The engine keeps every completed turn and commits it to
Git. There is no baseline, no score, and no discard. The run exits with status 0 when no
iterations remain.

## What the engine supplies

- **History**: each turn is a commit. An interrupted run continues from the session log with
  `--resume`. The engine rolls back an incomplete turn to the last kept state.
- **The deliverable**: the engine pushes kept commits as a draft PR (`[publish].pr_repo`). If
  a results bucket is configured, it also records the run log and the diff of each turn to
  S3.
- **The trust model**: with `backend = "openshell"`, the agent runs under Landlock with a
  deny-by-default egress allowlist, and it has no credentials. All privileged operations go
  through the broker tools, not the agent environment. You merge the PR.
- **Forensics**: `state/session.jsonl` is the source of truth.
  `crucible flow --session state/session.jsonl --out flow.html` renders a self-contained page
  that shows what the agent did in each turn.
- **Steering**: `STEER.md`, the control bridge, distress paging, and the limits
  (`--max-cost`, `--max-time`) work as in scored runs.

## Semantics to know

| Question | Answer |
| --- | --- |
| How does the run end? | At the iteration limit, or at `--max-cost`, `--max-time`, or stop. `solved` does not occur. `--no-early-stop` has no effect. |
| What does exit status 0 mean? | The run completed. It does not mean that the task succeeded. Read the rows or the PR. |
| What is on the wire? | `Start.gate == "task"` is the discriminator. There is an iteration-0 `baseline-skipped` row, then `keep` rows with `score: null`. |
| Can a composite be a task? | No. A composite combines scored components, so it needs `[judge]`. |
| Which sections are not allowed? | `[search]`, `[workflow]`, and `[preflight]`. All three need scores, so a task manifest rejects them at load. `objective = "task"` is reserved for scored judges. |
| Scheduled runs? | The controller schedules only playbooks. For a task run, render the pod with `crucible deploy render` and start it from a scheduler, for example a Kubernetes CronJob. |

## The runnable reference

`examples/task/` is the reference domain. The deterministic `command` backend replaces the
LLM, so the full path (manifest → turns → keeps → session log → resume) runs in
milliseconds:

```sh
crucible --manifest examples/task/crucible.toml --iterations 3
```

Use its manifest as the template for tools that generate task manifests.
