# Reading a run

A run leaves a graph, a log, a transcript and the files its tasks declared. Every surface reads
the same record, so use whichever is closest: the desktop app for browsing, `crux` for scripting
and for anything you want in a terminal.

## Where to look

| Question | Desktop app | `crux` |
| --- | --- | --- |
| Is it running? What has it cost? | Sidebar, run header | `crux playbook-runs` |
| Which task failed? | Tasks and Graph panes | `crux graph <run_id>` |
| What did it print? | Log pane, by task | `crux run-log <run_id> -f` |
| What did the agent do? | Transcript pane | `crux run-session <run_id>` |
| Which files did it keep? | Files pane | `crux run-files <run_id>`, `crux run-file <run_id> <task>/<path>` |
| What was this launched with? | Launch pane | `crux playbook-runs --json` |

`crux graph` also takes `--mermaid` for a diagram you can paste into a review. `crux run-file`
takes `--out` for a file that is not text.

## Read the failure, not the log

1. **Find the first red task.** A task that never ran is one whose dependency did not pass, so the
   interesting failure is the earliest one, not the loudest.
2. **Read that task's lines only.** Each line in `run-log` carries a `[<task>]` prefix, and the log
   of a parallel graph interleaves tasks.
3. **Check the ordinary causes in this order:**
   - a declared output the task never wrote (an agent that did not write its result document, a
     command that printed something after its JSON line);
   - a declared file that is missing, or under a name the task spelled differently;
   - an input that was not injected, so the script fails to open it;
   - a task limit longer than the launch's `--max-time`, which is refused before any task runs;
   - a provider whose harness does not match the sandbox image, which fails in the first turn.

Two things that look like failures and are not. A command prints nothing until it finishes, so
minutes of silence during a build is normal. And the pod's `verdict:` line can arrive before the
controller has ingested the run, so `graph` and `run-files` may answer empty or 404 for a run that
just ended; `run-log` is the view that exists throughout.

## Rerun from the task that failed

Fix the pack, push it, then rerun from the failed task. Earlier results are reused when their
compiled definitions did not change, which is what keeps a cheap fix cheap.

```sh
crux draft-launch my-pack --rerun-of <run_id> --from check \
  --max-cost 5 --max-time 1h
```

In the app, the task card's **Rerun from here** lists what will be reused and what will run again,
and lets you change the target, model and ceilings first. A rerun starts from the commit its
parent started from, not from the current head, so a rerun reproduces the run it came from.

## Stop the spend

```sh
crux cancel <launch key>    # as playbook-runs lists it
```

Cancel parks the launch and deletes its running pods; the ceilings stop a run on their own, but a
run that is merely slow will sit inside them until the wall clock runs out. Cancelling is the only
thing that stops a run early.
