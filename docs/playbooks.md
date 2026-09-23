# Your first playbook

A playbook is a graph that runs once. This page builds one from an empty directory: an agent
writes a haiku, a command counts its lines, and a failing count sends the draft back for a
second try. It runs first with a stand-in for the agent, so it costs nothing, and then with a
real one.

You need a `crucible` binary on `PATH` ([installation](https://github.com/neuralmagic/crucible#installation)).
The playbook lane is in every build; `--features autoresearch` is only for the scored loop.

## The pack

A pack is a directory. Everything a run needs lives in it:

```text
haiku/
├── crucible.toml    the manifest: the agent, the workspace, which graph to run
├── workflow.star    the graph
├── count.sh         the reviewer
└── stand-in.sh      a fake agent, for runs without a model
```

<ol class="cru-steps">
<li>

**The manifest.** `crucible.toml` says who does the agent turns and what the workspace
starts with. With no `[repo]`, the workspace starts empty and holds only what `inject` lists.

```toml
[workspace]
inject = ["count.sh", "stand-in.sh"]

[agent]
backend = "command"
agent_cmd = "./stand-in.sh"
goal = "Write a haiku about molten metal."

[workflow]
type = "playbook"
file = "workflow.star"
```

`backend = "command"` runs `agent_cmd` in place of a model. The rest of the pack cannot tell
the difference, which is what makes a pack testable in CI.

</li>
<li>

**The graph.** `workflow.star` declares the tasks and the edges between them.

```python
poem = agent(
    name = "poem",
    prompt = "Write a haiku about molten metal to HAIKU.md. Report its line count as `lines`.",
    emits = ["lines"],
    emits_files = ["HAIKU.md"],
)

check = command(
    name = "check",
    run = "./count.sh",
    depends_on = [poem],
    revise = poem,
    max_rounds = 2,
)

workflow(type = "playbook", tasks = [poem, check])
```

`emits` is a promise: a passing `poem` must return a `lines` field, or it fails where it
happened instead of confusing whatever reads it. `emits_files` names the file that `check`
gets staged. `revise = poem` makes `check` a reviewer: when it fails, `poem` runs again with
the verdict in hand, up to two rounds in total.

</li>
<li>

**The scripts.** A command task passes by exiting zero, and its last stdout line is its JSON
output. The reviewer:

```sh
#!/bin/sh
n=$(grep -c . HAIKU.md)
printf '{"lines": %s}\n' "$n"
[ "$n" -eq 3 ]
```

The stand-in writes four lines on the first round, then three once its prompt carries the
reviewer's verdict under `revision`:

```sh
#!/bin/sh
case "$CRUCIBLE_PROMPT" in
*'"revision"'*) printf 'molten in the dark\nthe crucible holds its breath\npour, and it is steel\n' > HAIKU.md ;;
*) printf 'molten in the dark\nthe crucible holds its breath\nand then some more\npour, and it is steel\n' > HAIKU.md ;;
esac
echo "{\"lines\": $(grep -c . HAIKU.md)}" > PLAN_TASK_RESULT.json
```

An agent turn reports by writing `PLAN_TASK_RESULT.json` in the workspace root. The engine
appends that instruction to every agent prompt, but not the fields: ask for what `emits`
promises in the prompt itself, as `poem` does with `lines`.

</li>
</ol>

## Run it

```sh
chmod +x count.sh stand-in.sh
crucible check --manifest crucible.toml
crucible plan run --manifest crucible.toml --max-cost 1 --max-time 5m
```

```text
  poem                 pass       attempts=1 cost=$0.0000  out={"lines":3}
  check                pass       attempts=1 cost=$0.0000  out={"lines":3}
plan v1: completed — spent $0.0000 of $1
verdict: valid
```

`--max-cost` and `--max-time` are required. A pack cannot set its own limits; whoever
launches it does. `crucible check` validates the manifest, resolves every file it references
and lists the egress and credentials the pack would get, without running anything.

The verdict is `valid` because every required task passed. It is the thing to script
against: `plan run` exits nonzero on any other verdict.

## What the run left behind

The run writes two directories next to the manifest. `workspace/` is a Git repository, and
each passing task is a commit:

```text
$ git -C workspace log --format=%s
task poem
task poem
autoresearch: baseline workspace
```

Two `task poem` commits: the first draft passed as a task (it wrote its result), the review
failed it, and the second round replaced it. The rows `plan run` prints are each task's
final state; `state/session.jsonl` records every round:

```text
$ jq -r 'select(.task != null and .status != null) | "\(.task) \(.status)"' state/session.jsonl
poem[round-1] pass
check[round-1] fail
poem[round-2] pass
check[round-2] pass
poem pass
check pass
```

## Put a real agent on it

Drop `agent_cmd` and pick a backend:

```toml
[agent]
backend = "local"
harness = "claude"
goal = "Write a haiku about molten metal."
```

| Backend | Where the turn runs |
| --- | --- |
| `command` | `agent_cmd`, no model. For tests. |
| `local` | The harness CLI on this machine, under your login. |
| `openshell` | An OpenShell sandbox from `sandbox_image`, with deny-by-default egress. |

The harness is `claude`, `codex`, `opencode`, `pi` or `hermes`. `--harness` and `--model` on
`plan run` override the manifest for one run, and any task can pin its own with
`agent(..., harness = "codex", model = "...")`. A real run spends money, which is what
`--max-cost` is for.

## Parameters

A pack that takes input declares a `params` block as the first statement of `workflow.star`:

```python
params = {
    "topic": {"type": "string", "default": "molten metal", "doc": "what the haiku is about"},
}

poem = agent(
    name = "poem",
    prompt = "Write a haiku about " + param("topic") + " to HAIKU.md. Report its line count as `lines`.",
    emits = ["lines"],
    emits_files = ["HAIKU.md"],
)
```

```sh
crucible plan params --file workflow.star           # the JSON Schema, without running anything
crucible plan run --manifest crucible.toml --param topic=slag --max-cost 1 --max-time 5m
```

Parameters reach prompts and skill arguments, never a command line. Build one into `run` and
the compiler refuses the pack:

```text
argument "run" carries a value supplied from outside the pack. A prompt marks such a span so
an agent can tell it from an instruction; nothing else can, so pass it to the task as a file
or an environment variable instead of building it into "run".
```

The same schema is what the control plane validates a launch against.

## Launch it from the control plane

The [control plane](./controller-local.md) keeps a registry of playbooks, a draft studio for
editing them, and a ledger of every launch. With a controller running and `crux` pointed at
it:

```sh
crux draft-create haiku --description "a haiku, reviewed"
crux draft-push haiku ./haiku --base 1
crux draft-launch haiku --max-cost 1 --max-time 5m
```

`draft-push` saves the directory as the draft's next version and compiles it. When the draft
is ready, `crux draft-graduate` opens the PR that exports it to a repository, and
`crux playbook-import owner/repo --path haiku` proposes registering it from there. Once
registered, `crux launch` starts it with parameters, and `crux playbook-runs` lists what ran.

<div class="cru-callout">
  <p><strong>Next:</strong> <a href="./playbook-patterns.html">Branching and review</a> covers routes, fan-out, sessions and advisory tasks. <code>examples/playbook</code>, <code>examples/route</code>, <code>examples/revise-loop</code> and <code>examples/triage</code> are complete packs to copy from.</p>
</div>
