# crucible

<div class="cru-hero">
  <span class="cru-mark">
    <img src="./img/crucible-mark.png" alt="The crucible mark: a vessel of molten metal." data-molten-fallback>
    <canvas data-molten width="480" height="480" aria-label="The crucible mark, molten: lava sloshing in the vessel. Click to slosh."></canvas>
  </span>
  <div>
    <p class="eyebrow">A workflow engine for agent work</p>
    <p class="headline">Write the graph. Crucible runs it and tells you whether it held.</p>
    <p class="lead">Agent turns, shell commands and checks, declared in Starlark, compiled to a static plan, and run in one Git workspace under a cost ceiling you set.</p>
    <div class="cru-actions">
      <a class="cru-button primary" href="./playbooks.html">Write a playbook →</a>
      <a class="cru-button" href="./controller-local.html">Run the control plane</a>
      <a class="cru-button" href="https://github.com/neuralmagic/crucible">GitHub</a>
    </div>
  </div>
</div>

A crucible workflow is a graph of tasks: an agent turn that drafts a fix, a command that runs
the tests, a reviewer that sends the draft back, a route that picks a branch. You write the
graph once. The engine owns everything after that: what runs next, what a failure means,
what gets retried, and what the run cost.

## Two lanes

<div class="cru-lanes">
  <div class="cru-lane">
    <h3>Playbooks</h3>
    <p class="tag">Run once, ship the result</p>
    <p>The graph runs to completion and hands back what it produced, with a verdict that says whether every required task held.</p>
    <ul>
      <li>Branch on typed answers with <code>route()</code></li>
      <li>Send a draft back to its author with <code>revise</code></li>
      <li>Fan out one isolated instance per item with <code>over</code></li>
    </ul>
    <p>Triage, reviews, release notes, anything with an end.</p>
  </div>
  <div class="cru-lane scored">
    <h3>Autoresearch</h3>
    <p class="tag">Loop until the number moves</p>
    <p>The graph runs inside a keep-or-discard loop. An agent proposes a change, a frozen judge measures it, and the engine keeps it only if it beats the best so far.</p>
    <ul>
      <li>Latency, throughput, a failing test suite</li>
      <li>Git is the memory: kept candidates are commits</li>
      <li>Wide rounds, portfolios, composite domains</li>
    </ul>
    <p>Anything you can score with one command.</p>
  </div>
</div>

Both lanes share the DSL, the executor and the control plane. A playbook is the graph; an
autoresearch run is the same graph with a loop around it.

## A playbook, end to end

A pack is a directory: a `crucible.toml`, a `workflow.star`, and whatever scripts the tasks
run. This one reads a ticket, routes it to one queue, and files it:

```python
read = command(name = "read", run = "./classify.sh", emits = ["bucket"])

gate = route(
    name = "gate",
    depends_on = [read],
    source = read,
    questions = {
        "bucket": choice(
            ask = "Which queue owns this ticket?",
            options = ["outage", "billing", "feature"],
        ),
    },
)

oncall = command(name = "oncall", run = "./file.sh oncall", depends_on = [gate],
                 when = gate.bucket, answers = "outage")
finance = command(name = "finance", run = "./file.sh finance", depends_on = [gate],
                  when = gate.bucket, answers = "billing")
backlog = command(name = "backlog", run = "./file.sh backlog", depends_on = [gate],
                  when = gate.bucket, otherwise = True)

filed = command(name = "filed", run = "./file.sh filed",
                depends_on = [oncall, finance, backlog], join = "passed")

workflow(type = "playbook", tasks = [read, gate, oncall, finance, backlog, filed], result = filed)
```

```text
$ crucible plan run --manifest crucible.toml --max-cost 1 --max-time 5m
  read                 pass       attempts=1 cost=$0.0000  out={"bucket":"billing"}
  gate                 pass       attempts=1 cost=$0.0000  out={"bucket":{"confidence":1.0,"label":"billing",...}}
  oncall               not_taken  attempts=0 cost=$0.0000  (gate.bucket resolved to billing)
  finance              pass       attempts=1 cost=$0.0000  out={}
  backlog              not_taken  attempts=0 cost=$0.0000  (gate.bucket resolved to billing)
  filed                pass       attempts=1 cost=$0.0000  out={}
plan v1: completed — spent $0.0000 of $1
verdict: valid
```

The branches nobody chose settle `not_taken`: they never dispatch and spend nothing. Swap
`source = read` for `min_confidence = 0.8` and a decision model answers the question instead,
with low-confidence answers recorded as `uncertain`, which `otherwise` catches.

<figure class="cru-figure">
  <img src="./img/controller-run.png" alt="A playbook run in the crucible control plane: the admitted task graph with a fanned-out build and measure, one failed instance, and the judge that kept the winner.">
  <figcaption>A playbook run in the control plane: a fanned-out build and measure, one failed instance, an advisory profile, and each task's cost.</figcaption>
</figure>

## What the engine holds for you

- **One workspace.** Tasks share a Git checkout. A task's declared files are staged for its
  dependents, and each passing task is a commit, so the run's history is `git log`.
- **Failures mean something.** Transport failures retry; measured failures never do. A task
  that cannot run on this substrate truncates the plan before anything spends. An advisory
  task can fail without invalidating the run.
- **Cost is an input.** A playbook does not start without `--max-cost` and `--max-time`, and
  spend is counted per attempt.
- **Agents run in a sandbox.** With the `openshell` backend each turn runs in a
  deny-by-default sandbox; the host keeps the credentials and the agent asks for what it
  needs over MCP.
- **The graph is static.** Starlark loops and functions unroll at compile time, so the plan
  you review is the plan that runs. Only `route`, `revise` and `over` decide anything at
  runtime, and each is bounded.

## Where to go next

<div class="cru-grid">
  <a class="cru-card" href="./playbooks.html">
    <h3>Your first playbook <span class="arrow">→</span></h3>
    <p>Write a pack, run it with no model, then point it at a real agent.</p>
  </a>
  <a class="cru-card" href="./playbook-patterns.html">
    <h3>Branching and review <span class="arrow">→</span></h3>
    <p>Routes, revise loops, fan-out, sessions, joins and advisory tasks.</p>
  </a>
  <a class="cru-card" href="./dsl-reference.html">
    <h3>DSL reference <span class="arrow">→</span></h3>
    <p>Every constructor and argument, generated from the compiler.</p>
  </a>
  <a class="cru-card" href="./how-it-works.html">
    <h3>The autoresearch loop <span class="arrow">→</span></h3>
    <p>Propose, apply, measure, keep or discard, against a frozen judge.</p>
  </a>
  <a class="cru-card" href="./controller-local.html">
    <h3>The control plane <span class="arrow">→</span></h3>
    <p>A UI, API and CLI for launching and watching runs. Starts on a laptop with one command.</p>
  </a>
  <a class="cru-card" href="./crucible-contract.html">
    <h3>The contract <span class="arrow">→</span></h3>
    <p>The frozen interface between the engine and a domain.</p>
  </a>
</div>

## The pieces

| Crate | What it is |
| --- | --- |
| `crucible/` | The engine: the Starlark compiler, the plan executor, the autoresearch loop, agent backends, and `crucible deploy`. |
| `crucible-controller/` | The control plane: a Postgres ledger, the reconciling daemon, the HTTP API and the embedded UI. |
| `crux/` | The CLI and MCP tools over the controller API. |
| `crucible-broker/` | The mediated broker: the host holds the keys, the sandboxed agent asks over MCP. |
| `forge/` | Image builds, registry and deployment support. |
