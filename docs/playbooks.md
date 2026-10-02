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
appends that instruction to every agent prompt; name the fields `emits` promises in the prompt
itself, as `poem` does with `lines`.

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
against: `plan run` exits 3 on any other verdict, and 1 when it could not run the plan at all.

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

## Report a pull request or a pushed branch

A task that produces something outside the run declares the field holding its url as `link`,
or a list of them as `links`:

```python
deliver = skill(
    name = "deliver",
    skill = "open-pr",
    depends_on = [check],
    emits = {"pushed": "links", "pr": "link"},
)
```

```json
{"pushed": ["https://github.com/neuralmagic/crucible/tree/haiku"], "pr": "https://github.com/neuralmagic/crucible/pull/123"}
```

Only http(s) urls pass; anything else fails the task where it happened. The engine reads the
host and path of each one to name it (`github`, a pull request, `#123`), and the run and task
views render a provider mark linking out to it.

## If the run is interrupted

`plan run --resume` continues the run in `state/`:

```sh
crucible plan run --manifest crucible.toml --max-cost 1 --max-time 5m --resume
```

Tasks the session log already settled do not run again, including single items of a fan-out
and single rounds of a revise loop. The workspace goes back to the commit the run started
from, so a later task sees what earlier tasks declared in `emits_files`, not what they left in
the tree. Spend and elapsed time from before the interruption count against the ceilings, and
elapsed time includes the time the run was down. A resume refuses a pack, parameter, or
`--max-cost` that differs from the run's start, and a run that already shut down only
exits with its verdict again.

A pod the control plane dispatches does this itself: when the engine dies, the container
restarts and resumes, up to three times. A hand-rendered pod does the same when its profile sets
`[cluster] state_pvc`.

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

Under `openshell`, a task can also run in its own sandbox. Declare it in the manifest and name
it on the task:

```toml
[agent.sandbox.go]
image = "ghcr.io/acme/sandbox-go@sha256:..."
secrets = []                                  # [[secret]] names with an env projection
relays = []                                   # [[agent.relay]] destinations
broker = false                                # reach the [agent.broker]
mcp = []                                      # [mcp] servers this sandbox reaches
endpoints = ["proxy.golang.org:443:read-only"]
```

```python
analyze = agent(name = "analyze", prompt = "...", sandbox = "go")
```

The turn starts from that image and adds those endpoints to `[agent.openshell]`'s. Of the pack's
declared secrets, relay files, and `[mcp]` servers it receives only the ones listed, and it
reaches the broker only when `broker = true`. The deployment's own model credentials still reach every turn. A task
without `sandbox` runs in `sandbox_image` with every declared secret and relay. Tasks that
share a session must share a sandbox. A sandbox limits what the engine provisions, not what a task
reads from upstream: output, files, and workspace changes from a task that held a secret still
reach the tasks after it. The capability disclosure lists each sandbox, and the controller
checks each sandbox image against the catalog at launch.

## MCP servers

Under `openshell`, a pack can start MCP servers on the loop pod for tools that hold credentials
the sandbox must not:

```toml
[mcp.buildit]
bin = "/usr/local/bin/buildit"
args = ["mcp"]
env = { BUILDIT_NAMESPACE = "builds" }        # set on the server
inherit = ["KUBERNETES_SERVICE_HOST", "KUBERNETES_SERVICE_PORT"]  # copied from the loop pod
tools = ["build", "run", "logs"]              # passed as MCP_TOOLS

[agent]
mcp = ["buildit"]                             # turns without a named sandbox
```

A turn reaches only the servers its scope names: `[agent].mcp`, or the `mcp` of the named sandbox
it runs in. The default is none, and the turn's harness config lists exactly those servers.
Each server runs as its own process on its own port, from 8850 in key order, with only `PATH`,
`HOME`, its `env` and `inherit` names, and `MCP_NAME`, `MCP_BIND`, `MCP_TOKENS_FILE` and
`MCP_TOOLS`. Every turn gets a fresh token per server, written to `MCP_TOKENS_FILE` with the
turn's sandbox and workdir ([format](crucible-contract.md#62-mcp-token-file-mcp_tokens_file)) and
revoked when the sandbox is deleted, so the server knows who is calling from the token alone.

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
an agent can tell it from an instruction; nothing else can, so do not build it into "run". A
command or evaluate task reads it as data from the "params" entry of the inputs JSON in $CRUCIBLE_INPUTS_FILE.
```

A command or evaluate task reads every declared parameter from the `params` entry of the inputs
JSON (the file `CRUCIBLE_INPUTS_FILE` names), under its name and in its declared type, beside its dependencies'
outputs. A source with no `params` block gives it an empty object. Agent tasks get no such
entry; their values reach them only through the prompt.

```python
fetch = command(
    name = "fetch",
    run = "python3 -c 'import json, os; p = json.load(open(os.environ[\"CRUCIBLE_INPUTS_FILE\"]))[\"params\"]; print(json.dumps({\"topic\": p[\"topic\"]}))'",
)
```

`params` is reserved, so no task may name a dependency `params`.

The same schema is what the control plane validates a launch against.

## History

A playbook that runs on a schedule can read what its last few runs found. The pack names one
task as its record, and any agent, skill, command, or evaluate task asks for up to 30 earlier
runs:

```python
triage = agent(
    name = "triage",
    prompt = "Find what is broken. Check the run history first: skip what earlier runs could not fix.",
    history = 5,
    emits = ["broken", "tried"],
)
workflow(type = "playbook", tasks = [triage], history_record = triage)
```

Each run records the record task's status and the fields it declares in `emits`, and nothing
else. A later run of the same standing launch receives those records under `history`, oldest
first, failed and timed-out runs included:

```json
{"records": [{"run": "...", "started_at": "...", "ended_at": "...", "outcome": "finished",
              "verdict": "valid", "revision": "...", "link": "...",
              "entry": {"task": "triage", "status": "pass", "output": {"broken": 2, "tried": ["..."]}}}],
 "dropped": 0}
```

A command reads it from the inputs file `CRUCIBLE_INPUTS_FILE` names. An agent sees it in its prompt, marked as external
input, because an earlier agent wrote part of it. When the records exceed the operator's size
limit, the oldest are dropped whole and counted in `dropped`; `crucible check` prints the limit.
A manual launch belongs to no series and gets an empty list, which is also what a local
`plan run` gets unless `CRUCIBLE_HISTORY` is set. Make the record task an epilogue task to record
even when the main graph fails. A pack's record shape can change between revisions, so a reader
should tolerate older entries; each record carries the `revision` that wrote it.

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
