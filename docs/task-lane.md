# Tasks: general-purpose orchestration

Crucible is an optimization loop, but not every job has an objective. The **task lane** runs
the same loop — sandbox, broker mediation, session log, publish, resume — for work that just
needs doing: consolidate the open dependabot PRs, fix flaky tests nightly, regenerate docs,
triage an issue backlog. One manifest, no Rust, no gate script.

Opt in by omitting `[judge]`. That's the whole switch ([ADR-0026](./adr/0026-no-judge-task-lane.md)).

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

Each iteration is one agent turn. Every completed turn is kept and committed to git memory;
there is no baseline, no score, no discard. The run exits 0 when the iterations are spent.

## What you get for free

- **Reversible memory**: each turn is a commit; an interrupted run resumes with `--resume`
  from the session log, rolling back any half-finished turn to the last kept state.
- **The deliverable**: kept commits pushed as a draft PR (`[publish].pr_repo`), with the run
  log and per-turn diffs recorded to S3 when a results bucket is configured.
- **The trust model**: with `backend = "openshell"` the agent runs under Landlock with a
  deny-by-default egress allowlist, and holds no credentials. Anything privileged goes
  through the broker's tools, never the agent's environment. Merging the PR stays with you.
- **Forensics**: `state/session.jsonl` is the source of truth; `crucible flow --session
  state/session.jsonl --out flow.html` renders a self-contained explainer of what the agent
  did each turn.
- **Steering**: `STEER.md`, the control bridge, distress paging, and budget caps
  (`--max-cost`, `--max-time`) all work exactly as in scored runs.

## Semantics to know

| Question | Answer |
| --- | --- |
| How does it end? | The iteration budget (or `--max-cost`/`--max-time`/stop). `solved` never fires; `--no-early-stop` is a no-op. |
| What does exit 0 mean? | The run completed. It does not certify the chore succeeded — read the rows or the PR. |
| What's on the wire? | `Start.gate == "task"` is the discriminator; an iter-0 `baseline-skipped` row, then `keep` rows with `score: null`. |
| Can a composite be a task? | No. Composites exist to combine scored components; `[judge]` stays required there. |
| What else is off the table? | `[search]`, `[workflow]`, and `[preflight]` — all three need scores, so a task manifest rejects them at load. `objective = "task"` is reserved on scored judges. |
| Scheduled runs? | Render the pod with `crucible deploy render` (a playbook launch adds `--playbook --max-time <dur>`) and drive it from any scheduler (a Kubernetes CronJob works today); native controller scheduling is planned. |

## The runnable reference

`examples/task/` is the litmus domain: the deterministic `command` backend stands in for an
LLM, so the full path (manifest → turns → keeps → session log → resume) runs in milliseconds:

```sh
crucible --manifest examples/task/crucible.toml --iterations 3
```

Use its manifest as the template for anything that authors task manifests programmatically.
