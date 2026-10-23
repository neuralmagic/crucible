# Your first playbook

A playbook is a graph that runs once. This page builds a playbook from an empty directory.
An agent writes a haiku, and a command counts its lines. If the count is wrong, the command
sends the draft back for a second round. The first run uses a stand-in for the agent and
costs nothing. The second run uses a real agent.

You need a `crucible` binary on `PATH` ([installation](https://github.com/neuralmagic/crucible#installation)).
Every build includes the playbook lane. Only the scored loop needs `--features autoresearch`.

## The pack

A pack is a directory that holds all the files a run needs:

```text
haiku/
├── crucible.toml    the manifest: the agent, the workspace, which graph to run
├── workflow.star    the graph
├── count.sh         the reviewer
└── stand-in.sh      a fake agent, for runs without a model
```

<ol class="cru-steps">
<li>

**The manifest.** `crucible.toml` sets the backend for agent turns and the initial content
of the workspace. Without `[repo]`, the workspace starts empty and holds only the files that
`inject` lists.

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

`backend = "command"` runs `agent_cmd` instead of a model. The rest of the pack sees no
difference, so you can test the pack in CI.

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

`emits` declares the output fields. A passing `poem` must return a `lines` field. If the
field is missing, `poem` fails, so the error appears at its source and not in a later task.
`emits_files` names the file that the engine stages for `check`. `revise = poem` makes
`check` a reviewer. If `check` fails, `poem` runs again with the verdict. There are at most
two rounds.

</li>
<li>

**The scripts.** A command task passes if it exits with status zero. Its last stdout line is
its JSON output. This is the reviewer:

```sh
#!/bin/sh
n=$(grep -c . HAIKU.md)
printf '{"lines": %s}\n' "$n"
[ "$n" -eq 3 ]
```

The stand-in writes four lines in the first round. When its prompt contains the reviewer's
verdict under `revision`, it writes three lines:

```sh
#!/bin/sh
case "$CRUCIBLE_PROMPT" in
*'"revision"'*) printf 'molten in the dark\nthe crucible holds its breath\npour, and it is steel\n' > HAIKU.md ;;
*) printf 'molten in the dark\nthe crucible holds its breath\nand then some more\npour, and it is steel\n' > HAIKU.md ;;
esac
echo "{\"lines\": $(grep -c . HAIKU.md)}" > PLAN_TASK_RESULT.json
```

An agent turn reports its result in `PLAN_TASK_RESULT.json` in the workspace root. The
engine adds this instruction to every agent prompt. Name the `emits` fields in the prompt, as
`poem` does with `lines`.

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

`--max-cost` and `--max-time` are required. A pack cannot set its own limits. The person or
system that launches the pack sets them. Once spend reaches `--max-cost`, no further agent task,
model-decided route, or fan-out lane starts. At most eight lanes of one fan-out run at once, so
the overrun is at most what the lanes already running go on to spend. Commands still run after
the cost ceiling, so the run reports the work it paid for. `--max-time` stops everything.

`crucible check` validates the manifest, resolves every file that it references, and lists the
egress and credentials that the pack would get. It does not run a task.

The verdict is `valid` because every required task passed. Use the verdict in scripts.
`plan run` exits with status 3 for any other verdict, and with status 1 if it cannot run the
plan.

## What the run left behind

The run writes two directories next to the manifest. `workspace/` is a Git repository, and
each passing task makes a commit:

```text
$ git -C workspace log --format=%s
task poem
task poem
autoresearch: baseline workspace
```

There are two `task poem` commits. The first draft passed as a task because it wrote its
result. The review failed it, and the second round replaced it. `plan run` prints the final
state of each task. `state/session.jsonl` records every round:

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

If a task makes something outside the run, declare the field that holds its URL as `link`.
Declare a list of URLs as `links`:

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

Only http and https URLs are valid. Any other value fails the task. The engine reads the
host and path of each URL to name it (`github`, a pull request, `#123`). The run and task
views show a provider mark that links to the URL.

## If the run is interrupted

`plan run --resume` continues the run in `state/`:

```sh
crucible plan run --manifest crucible.toml --max-cost 1 --max-time 5m --resume
```

A task that the session log already settled does not run again. This includes single items
of a fan-out and single rounds of a revise loop. The workspace goes back to the commit where
the run started. A later task sees the files that earlier tasks declared in `emits_files`,
not other files they left in the tree. Spend and elapsed time from before the interruption
count against the limits. Elapsed time includes the time when the run was down. A resume
fails if the pack, a parameter, or `--max-cost` is different from the start of the run. If
the run already shut down, a resume only exits with its verdict again.

A pod that the control plane dispatches resumes automatically. If the engine stops, the
container restarts and resumes, up to three times. A pod that you render by hand does the
same if its profile sets `[cluster] state_pvc`.

## Rerun from a task

If a late task fails, fix it and rerun from that task. You do not have to run the whole
graph again. Move the state of the finished run to a different directory, and give that
directory to `--rerun-of`:

```sh
mv state parent
crucible plan run --manifest crucible.toml --max-cost 1 --max-time 5m \
  --rerun-of parent --from check
```

`check` and all tasks downstream of it run again. Each task that did not pass in the parent
run also runs again. The rerun reuses all other tasks. Their output and declared files go to
their dependents as before, and the rerun replays their commits onto the commit where the
parent started. The session log marks each reused row with `reused_from`, which names the run
that settled it. The spend of reused rows does not count against the new limits.

You can edit the tasks that you rerun. If you edit a task that the rerun would reuse, the
rerun fails and names the task. This prevents a stale result from being reused. Add the task
to `--from` to run it again. This check compares only what the graph declares. It does not
compare the scripts in the pack. If you edit only `check.sh`, no task runs again because of
that edit.

In the control plane, the page of a finished launch has a **Rerun** button. It shows which
tasks the rerun reuses and the expected cost. From the CLI, launch the draft or playbook
again and name the run to rerun:

```sh
crux draft-launch haiku --max-cost 1 --max-time 5m --rerun-of <run id> --from check
```

A draft rerun runs the newest save. A pod rerun gets the parent's evidence from the
controller at `[cluster].ingest_url`. The profile of a pod executor must set this value.

## Put a real agent on it

Remove `agent_cmd` and select a backend:

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

The harness is `claude`, `codex`, `opencode`, `pi`, or `hermes`. The `--harness` and
`--model` options of `plan run` override the manifest for one run. A task can set its own
values with `agent(..., harness = "codex", model = "...")`. A real run costs money.
`--max-cost` limits that cost.

With `openshell`, a task can run in its own sandbox. Declare the sandbox in the manifest
and name it on the task:

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

The turn starts from that image. It adds those endpoints to the endpoints of
`[agent.openshell]`. From the secrets, relay files, and `[mcp]` servers that the pack
declares, the turn gets only the ones that the sandbox lists. The turn can reach the broker
only if `broker = true`. Every turn still gets the model credentials of the deployment. A
task without `sandbox` runs in `sandbox_image` and gets every declared secret and relay.
Tasks that share a session must use the same sandbox.

A sandbox limits what the engine provisions for a task. It does not limit what a task reads
from upstream tasks. If a task had a secret, its output, files, and workspace changes still
go to the tasks after it. The capability disclosure lists each sandbox. At launch, the
controller checks each sandbox image against the catalog.

## MCP servers

With `openshell`, a pack can start MCP servers on the loop pod. Use them for tools that need
credentials that the sandbox must not have:

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

A turn can reach only the servers that its scope names. The scope is `[agent].mcp`, or the
`mcp` list of the named sandbox that the turn runs in. The default is no servers. The harness
configuration of the turn lists only those servers.

Each server runs as a separate process on a separate port. Ports start at 8850 and follow key
order. A server gets only these environment variables: `PATH`, `HOME`, its `env` and `inherit`
names, `MCP_NAME`, `MCP_BIND`, `MCP_TOKENS_FILE`, and `MCP_TOOLS`. Every turn gets a new token
for each server. The engine writes the token to `MCP_TOKENS_FILE` with the sandbox and workdir
of the turn ([format](crucible-contract.md#62-mcp-token-file-mcp_tokens_file)). The engine
revokes the token when it deletes the sandbox. The server identifies the caller from the token
alone.

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

Parameters go into prompts and skill arguments. They do not go into a command line. If you
put a parameter into `run`, the compiler rejects the pack:

```text
argument "run" carries a value supplied from outside the pack. A prompt marks such a span so
an agent can tell it from an instruction; nothing else can, so do not build it into "run". A
command or evaluate task reads it as data from the "params" entry of the inputs JSON in $CRUCIBLE_INPUTS_FILE.
```

A command or evaluate task reads each declared parameter from the `params` entry of the
inputs JSON. `CRUCIBLE_INPUTS_FILE` names that file. Each parameter has its declared name and
type, and the file also holds the outputs of the task's dependencies. If the source has no
`params` block, the entry is an empty object. Agent tasks do not get this entry. They get
parameter values only through the prompt.

```python
fetch = command(
    name = "fetch",
    run = "python3 -c 'import json, os; p = json.load(open(os.environ[\"CRUCIBLE_INPUTS_FILE\"]))[\"params\"]; print(json.dumps({\"topic\": p[\"topic\"]}))'",
)
```

`params` is a reserved name. A task cannot name a dependency `params`.

The control plane validates a launch against the same schema.

## History

A playbook that runs on a schedule can read the results of its last runs. The pack names one
task as its record task. An agent, skill, command, or evaluate task can request up to 30
earlier runs:

```python
triage = agent(
    name = "triage",
    prompt = "Find what is broken. Check the run history first: skip what earlier runs could not fix.",
    history = 5,
    emits = ["broken", "tried"],
)
workflow(type = "playbook", tasks = [triage], history_record = triage)
```

Each run records only the status of the record task and the fields that it declares in
`emits`. A later run of the same standing launch gets those records under `history`, oldest
first. The list includes failed runs and runs that timed out:

```json
{"records": [{"run": "...", "started_at": "...", "ended_at": "...", "outcome": "finished",
              "verdict": "valid", "revision": "...", "link": "...",
              "entry": {"task": "triage", "status": "pass", "output": {"broken": 2, "tried": ["..."]}}}],
 "dropped": 0}
```

A command reads the history from the inputs file that `CRUCIBLE_INPUTS_FILE` names. An agent
sees it in its prompt, marked as external input, because an earlier agent wrote part of it.
If the records are larger than the size limit that the operator sets, the engine removes the
oldest full records and counts them in `dropped`. `crucible check` prints the limit.

A manual launch is not part of a series, so it gets an empty list. A local `plan run` also
gets an empty list, unless `CRUCIBLE_HISTORY` is set. To record a result when the main graph
fails, make the record task an epilogue task. The record shape of a pack can change between
revisions, so a reader must accept older entries. Each record contains the `revision` that
wrote it.

## Launch it from the control plane

The [control plane](./controller-local.md) has a registry of playbooks, a draft studio to
edit them, and a ledger of all launches. Start a controller and point `crux` at it. Then:

```sh
crux draft-create haiku --description "a haiku, reviewed"
crux draft-push haiku ./haiku --base-version 1
crux draft-launch haiku --max-cost 1 --max-time 5m
```

`draft-push` saves the directory as the next version of the draft and compiles it. When the
draft is ready, `crux draft-graduate` opens a PR that exports it to a repository.
`crux playbook-import owner/repo --path haiku` proposes to register it from that repository.
After registration, `crux launch` starts the playbook with parameters, and
`crux playbook-runs` lists its runs.

### Let your own agent write it

If an agent writes the pack, give it the co-draft skill before it starts. The controller
generates the skill at `<controller>/api/playbooks/drafts/skill`, and the skill contains the
controller URL. The skill tells the agent how to use the draft tools, how to use
`base_version` when it saves, and when to stop. The **Agent skill** button on the drafts page
downloads the skill, and `crux draft-create` prints the link. Install the skill and register
the MCP endpoint of the controller:

```sh
mkdir -p ~/.claude/skills/crucible-co-draft
curl -fsSL -o ~/.claude/skills/crucible-co-draft/SKILL.md \
  -H "Authorization: Bearer $CONTROLLER_API_TOKEN" \
  "$CONTROLLER_URL/api/playbooks/drafts/skill"
claude mcp add --transport http crucible "$CONTROLLER_URL/mcp" \
  --header "Authorization: Bearer $CONTROLLER_API_TOKEN"
```

Then ask the agent for a pack. The agent saves into a draft, and you can see it compile in
the studio. When the diagnostics are clean, the agent gives you the studio link.

<div class="cru-callout">
  <p><strong>Next:</strong> <a href="./playbook-patterns.html">Branching and review</a> describes routes, fan-out, sessions, and advisory tasks. <code>examples/playbook</code>, <code>examples/route</code>, <code>examples/revise-loop</code>, and <code>examples/triage</code> are complete packs that you can copy.</p>
</div>
