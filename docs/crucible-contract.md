# Crucible implementation contract (frozen interface)

This is the spec for the interface between the engine (`crucible`) and a
domain. The engine implements it, a domain satisfies it, and a minimal fake domain
(`examples/counter/`) tests it end-to-end with no EPP and no cluster.

Concept: [What crucible is](./crucible.md).
Trust line (engine hands the agent a World, never a Judge): [ADR 0001](./adr/0001-adaptive-harness.md).

> **Contract status: NORMATIVE.** Words like *must* / *exactly one* are binding. The engine
> and every domain (EPP included) conform to this; tests assert it.

---

## 1. The manifest (`crucible.toml`)

The engine reads exactly one manifest per run. Default path `./crucible.toml` (override
`--manifest <path>`). The **manifest directory** = `dirname(manifest)`; it anchors all
config-relative paths below.

```toml
[repo]
# Exactly one of url|path. The upstream the agent edits a checkout of.
url  = "https://github.com/owner/name.git"   # OR
path = "."                                    # local path, relative to manifest dir
ref  = "main"                                 # optional; default: clone default branch

[workspace]
dir       = "workspace"                       # checkout dir, relative to manifest dir. default "workspace"
setup_cmd = "git clone ... && git checkout"   # optional; default: engine git clone+checkout of [repo]

# Frozen-judge / fixture injection (optional, repeatable). After setup, the engine copies each
# `src` (relative to the manifest dir, a baked artifact outside the clone target, so the agent has
# no pre-clone copy) to `dst` (relative to the workspace). A `frozen` inject (default true) is ALSO
# re-copied before every scored measure, so a candidate can't edit the gate (a T1 scoring harness, a
# seeded regression test) to game it; set `frozen = false` for a one-time fixture the agent may then
# edit. This is the generic alternative to hand-chaining an `install` into `setup_cmd`.
[[workspace.inject]]
src    = "judges/1489/judge_harness_test.go"  # baked judge, manifest-relative
dst    = "pkg/.../judge_harness_test.go"      # into the clone, workspace-relative
frozen = true                                 # re-copied before each measure (default true)

[agent]
model         = "claude-opus-4-6"             # default engine constant
method_prompt = "method.md"                   # manifest-relative file; template, {{GOAL}}/{{STATUS}}/{{STEER}}
goal          = "raise the score"             # OR goal_file = "goals/x.md" (manifest-relative)
toolbox_dir   = "commands"                    # optional; copied into <workspace>/.claude/skills
backend       = "local"                       # local | openshell | command   (see §6)
sandbox_image = "ghcr.io/<org>/<domain>-sandbox:<tag>"  # openshell backend only
agent_cmd     = "..."                          # command backend only (§6)
[agent.env]                                   # injected into the agent process (creds, Vertex, etc.)
ANTHROPIC_VERTEX_PROJECT_ID = "my-gcp-project"

[judge]
measure_cmd = "./measure.sh"                  # REQUIRED. §3. Any executable, any language.
direction   = "lower"                         # lower | higher. REQUIRED.
objective   = "score"                         # display label (the old `gate` name). default "score"

[judge.selftest]                              # optional (ADR-0014 S1). Runs in `crucible check`, never in a loop iteration.
good_cmd = "..."                              # stages a known-good config in the workspace
bad_cmd  = "..."                              # stages a known-bad config in the workspace
runs     = 1                                  # measurements averaged per control. default 1

[world]
# All three omitted  →  GitWorld (tree-only reversibility; the 80% case).
apply_cmd    = "..."                          # optional. §3.
snapshot_cmd = "..."                          # optional. §3.
restore_cmd  = "..."                          # optional. §3.

[search]                                      # optional (ADR-0010). Absent or wide=0 → pure-deep, the default.
wide       = 0                                # u32. N parallel propose turns before the deep loop. default 0
approaches = ["...", "..."]                   # REQUIRED when wide > 0: one distinct approach string per candidate slot
policy     = "top-k"                          # only v1 policy. default "top-k"
policy_k   = 1                                # how many wide-round winners seed the deep loop. default 1, must be in 1..=wide
```

Rules:
- **Required:** `[repo]` (url xor path), `[judge].measure_cmd`, `[judge].direction`.
- `[world]` with no commands ⇒ **GitWorld** (§5). Any command given ⇒ **CommandWorld** (§5),
  which still owns git memory and layers the given commands on top.
- `[judge.selftest]`, if present, requires both `good_cmd` and `bad_cmd` (a self-test that only
  stages one side isn't a control); `runs` must be `>= 1`.
- `[search]`, if present with `wide > 0`, requires `approaches.len() >= wide` (hard error,
  diversity is engineered, not auto-generated) and `policy_k` in `1..=wide`.
- Unknown keys are an error (typo protection), not silently ignored.
- **Frozen loading (`load_frozen`).** When the manifest file lives *inside* the workspace it
  targets (the BYO on-ramp: `crucible init` scaffolds `[repo] path = "."`), the engine parses it
  from the workspace's pristine base commit, not the current working tree (so an in-flight agent
  edit to `crucible.toml` can't retarget its own gate mid-run). Before any base commit exists (the
  very first run), it hard-warns and trusts the working tree for that run only; later runs freeze
  to the base commit. A manifest that lives outside the workspace (any out-of-workspace pack) is
  unaffected.

### 1.1 The gate self-test (`crucible check`, ADR-0014 S1)

`[judge.selftest]` declares two controls the gate must tell apart before it's trusted. `crucible
check` (§9) runs it pre-loop, never inside a loop iteration:

1. snapshot the pristine workspace,
2. restore to pristine, stage `good_cmd`, measure `runs` times through the domain's own `Judge`,
   restore to pristine again,
3. same for `bad_cmd`,
4. **pass** iff both controls' readings are all `valid` and `good`'s mean score is *strictly*
   better than `bad`'s per `[judge].direction`.

The workspace is restored to pristine on every exit path (pass, fail, or error). A manifest with
no `[judge.selftest]` isn't an error, `crucible check` warns instead, since the gate hasn't been
proven to discriminate.

### 1.2 Wide-round search (`[search]`, ADR-0010)

`[search].wide > 0` (or `--wide N` on the CLI, which overrides the manifest) fans out `N`
independent PROPOSE turns in per-candidate git worktrees under the state dir before the deep
loop starts, one turn per `approaches` entry biased into its prompt. Each candidate's diff is
applied (cherry-picked) into the shared main workspace and measured serially there (measurement
never runs concurrently, only proposal does). The scored set is ranked by `[search].policy` (v1:
`"top-k"`); the `policy_k` (or `--wide-keep K`) winner(s) seed the deep loop, which then runs as
normal. Session rows from the wide round carry an additive `phase: "wide"` field (§7) so a
consumer can tell a wide-round row from a deep-loop row without a wire-shape change.

### 1.3 Scope-authored workflows (`workflow.star`)

A scoped pack may include `workflow.star` beside `crucible.toml`. It is authoring syntax, not a
runtime interpreter: scope compiles it to the existing `[[workflow.task]]` manifest IR before
validation and again before freeze. The generated TOML is the runtime authority. This keeps plan
authorship readable without moving the frozen judge or execution semantics into Starlark.

Topology is authorable; authority is not. A workflow declares `type = "autoresearch"` or
`type = "custom"`, and an engine or outer orchestrator admits it only when it advertises the
matching workflow capability. Each `engine` task also requires its own capability. Crucible's loop
advertises `workflow.autoresearch` plus propose/apply/measure/grade/decide, while its generic plan runner
does not. It adds `agent.session.persist` only when the selected backend and harness can honor an
opaque continuation. Serializing an engine task never grants access to the `World` or frozen
`Judge`.

An autoresearch workflow is checked by semantics rather than reserved task names. Its selected
result must be a `decide` task sourced from a frozen `measure` or authored `grade`, with `apply`
and `propose` ancestors.
Tasks may be inserted anywhere, operations may be renamed, and multiple candidate or measurement
branches may exist. A custom workflow has no autoresearch-shape requirement; universal DAG,
source-typing, and operation-capability rules still apply.

The basic autoresearch flow is explicit and editable:

```python
candidate = propose(name = "invent", session = "solver")

critics = [
    agent(
        name = "correctness",
        prompt = prompt_file("prompts/correctness.md"),
        model = "claude-opus-4-6",
        effort = "high",
        isolated = True,
        depends_on = [candidate],
    ),
    agent(
        name = "novelty",
        prompt = prompt_file("prompts/novelty.md"),
        required = False,
        isolated = True,
        depends_on = [candidate],
    ),
]

synthesize = agent(
    name = "synthesize",
    prompt = prompt_file("prompts/synthesize.md"),
    session = "solver",
    depends_on = deps(critics),
    join = "passed",
)
smoke = command(name = "smoke", run = "./smoke.sh", depends_on = [synthesize])
live = apply(name = "deploy-preview", depends_on = [smoke])
score = measure(name = "benchmark", depends_on = [live])
decision = decide(name = "keep-if-better", measurement = score)

workflow(
    type = "autoresearch",
    tasks = [candidate] + critics + [synthesize, smoke, live, score, decision],
    result = decision,
)
```

`default_autoresearch(extra_tasks)` is the compatibility convenience. It expands to the same
ordinary propose/apply/measure/decide tasks, attaching unconnected extras after proposal and before
apply. A missing `workflow.star` retains the historical default loop behavior. The old positional
`workflow(tasks)` form remains accepted as a splice adapter while packs migrate.

Measurement can remain the historical opaque `measure()` call, or be authored as a visible DAG:

```python
live = apply(name = "deploy", depends_on = [candidate])
correctness = evaluate(
    name = "correctness",
    run = "./correctness.sh",
    depends_on = [live],
    isolated = True,
)
latency = evaluate(
    name = "latency",
    run = "./latency.sh",
    depends_on = [correctness],
    threshold = 12.5,
    direction = "lower",
    isolated = True,
)
racecheck = evaluate(
    name = "racecheck",
    run = "./racecheck.sh",
    depends_on = [correctness],
    required = False,
    isolated = True,
)
measurement = grade(
    name = "grade",
    evidence = [correctness, latency, racecheck],
    score = latency,
)
decision = decide(name = "choose", measurement = measurement)
```

Dependencies define measurement rungs; ready isolated siblings run concurrently. `evaluate()`
expects a JSON object on its last stdout line. `pass = false` vetoes a result, malformed `pass`
fails closed, and paired `threshold`/`direction` fields grade numeric `score`. Without a threshold,
omitted `pass` means success. `grade()` selects a passing score evaluator and folds passing
evidence by default; set `join = "all"` for a strict join. The legacy `measure()` path and default
workflow are unchanged.

`session = "solver"` binds agent-producing tasks to a durable logical conversation. The checkout
may roll back after a discarded candidate while the solver session continues forward and retains
what it learned. Tasks sharing a session must be dependency-ordered and cannot be isolated;
parallel critics should stay fresh or use distinct sessions. Admission requires
`agent.session.persist`. A missing `session` preserves the historical fresh-turn behavior.

Crucible's private ledger contains only the logical name, an opaque harness cursor, and a
completed-turn count. It never copies that cursor or Claude's native transcript into
`session.jsonl`; the existing live harness event policy, including streamed thinking events, is
unchanged. Claude Code implements native start/resume for both the local and Vertex/OpenShell paths. Because OpenShell sandboxes are
per-turn-fresh, Crucible saves Claude's native transcript as mode-0600 private engine state and
restores it at the exact pinned config path in the next sandbox; that file is never included in the
published run record. Hermes fails closed for a persistent binding until it has an equivalent
continuation store; it never silently degrades the binding to a fresh prompt.

The first turn receives the complete method and goal prompt. A resumed proposer receives only the
new authoritative delta: current regime, current-best status, and new steering. Its retained
session already holds the stable instructions and hypotheses, while the current checkout and
`RESULTS.md` remain authoritative after any world rollback.

A custom orchestrator can admit a graph with no research lifecycle at all—for example, a creative
studio that fans out three treatments, curates them, and publishes a contact sheet:

```python
treatments = [
    agent(name = "surreal", prompt = prompt_file("prompts/surreal.md"), isolated = True),
    agent(name = "minimal", prompt = prompt_file("prompts/minimal.md"), isolated = True),
    agent(name = "documentary", prompt = prompt_file("prompts/documentary.md"), isolated = True),
]
curate = agent(
    name = "curate",
    prompt = prompt_file("prompts/curate.md"),
    depends_on = deps(treatments),
    join = "passed",
)
publish = command(name = "contact-sheet", run = "./render.sh", depends_on = [curate])
workflow(type = "custom", tasks = treatments + [curate, publish], result = publish)
```

That graph requires `workflow.custom`; it does not pretend to satisfy autoresearch just because it
uses agents and commands.

This is a declarative Starlark subset: assignments, strings, numbers, booleans, lists, list
concatenation, and direct calls to the functions below. Control flow, function definitions,
comprehensions, mutation, arbitrary operators, and `load()` are rejected. Task values are opaque;
they can be referenced directly in `depends_on`, `measurement`, and `result` without repeating
names.

- `propose(...)`, `apply(...)`, `measure(...)`, `grade(...)`, and `decide(...)` create
  capability-owned engine tasks. `decide(measurement = score)` selects its measurement.
- `agent(...)` creates an agent task. `isolated = True` gives it a disposable worktree, ideal for
  concurrent read-only critics; leave it false for a synthesizer whose edits must survive.
  `session = "name"` opts into an engine-managed durable conversation.
- `command(...)` creates a deterministic shell task in the candidate workspace.
- `evaluate(...)` creates a typed measurement command with optional threshold grading.
- `top_k(...)` creates a reducer for wider authored graphs.
- `deps(tasks)` returns constructor task names, avoiding repeated string wiring.
- `prompt_file(path)` reads a regular UTF-8 file below the pack directory and embeds its contents
  in the generated manifest. Absolute paths, `..`, symlinks, non-files, and oversized inputs are
  rejected.
- `workflow(type = ..., tasks = ..., result = ...)` is the explicit final expression.
- `default_autoresearch(extra_tasks)` expands the historical loop into fully visible nodes.

Agent tasks receive upstream results in their prompt and write one JSON object to
`PLAN_TASK_RESULT.json`. Required failures discard the candidate; advisory tasks use
`required = False`. `join = "passed"` waits for all dependencies, then receives their non-empty set
of successful results. No passing input blocks the task.

For local review, `crucible plan compile-workflow --file workflow.star` prints stable canonical
JSON. Add `--manifest crucible.toml` to also replace the generated `[workflow]` block. Compilation
applies source, task-count, evaluation-step, and prompt-size ceilings. The compiler exposes no
filesystem API except `prompt_file`, and no process, environment, network, clock, or randomness API.
Scope validation renders the admitted graph to `WORKFLOW.png` for the scope PR, grouping
`evaluate` and `grade` as Measurement.

---

## 2. Path resolution (portable, never `CARGO_MANIFEST_DIR`)

| Thing | Resolves to |
| --- | --- |
| config: `method_prompt`, `goal_file`, `toolbox_dir` | **manifest-relative** |
| agent workspace (the measured checkout) | `manifest_dir / [workspace].dir` |
| runtime state (`session.jsonl`, `control.json`) | `--state-dir`, default `manifest_dir/state` |
| `STEER.md` | `--steer`, default `manifest_dir/STEER.md` |
| `ESCALATION.json` (agent's harness-blocker marker, ADR-0001) | `<workspace>/ESCALATION.json`: written by `escalate`, consumed by the engine post-turn |

The binary's own install location is **never** used to resolve anything. A target repo is
self-describing: drop a `crucible.toml` at its root and run `crucible` inside it.

---

## 3. The command protocol

Every command (`measure_cmd`, `apply_cmd`, `snapshot_cmd`, `restore_cmd`, `setup_cmd`) is a
**string executed via `sh -c "<cmd>"`**, with:

- **cwd** = the agent workspace (`manifest_dir/[workspace].dir`).
- **PATH inherited**, so a command may be a bare installed tool (`bench`) or a workspace-relative
  script (`./measure.sh`).
- **env** = the engine's env + `[agent].env` (where relevant) + the injected variables below.

### `measure_cmd` (the Judge): REQUIRED
- **Injected env:** `CRUCIBLE_BASELINE_SCORE`, `CRUCIBLE_BASELINE_TOTAL`, `CRUCIBLE_BEST_SCORE`
  (absent on the baseline measurement; present thereafter). Values are the engine's current
  numbers as decimal strings.
- **stdout:** the engine reads **the last line that starts with `{`** and parses it as:
  ```json
  { "valid": true, "score": 12.5, "solved": false, "note": "p99=12.5ms", "detail": { } }
  ```
  - `valid` (bool, REQUIRED): false ⇒ unscoreable candidate, always discarded.
  - `score` (number|null): the fitness. `null`/absent ⇒ treated as invalid.
  - `solved` (bool, optional, default false): the win condition was met (terminates the loop).
  - `note` (string, optional): one-line human summary.
  - `detail` (object, optional): free-form; surfaced in the row + session log. The domain
    stashes anything extra here (e.g. EPP `cache_hit_rate`; the test gate's `total`).
- **exit code:** nonzero ⇒ the reading is forced `valid:false` regardless of stdout.

### `apply_cmd` (make the candidate live): optional
- Run after the agent turn, before `measure`, when present. Nonzero exit ⇒ the iteration is
  treated as an invalid candidate (discard). No stdout contract.
- **Omit for code/agent-deploys domains** (EPP: the agent deploys via skills during its turn;
  the counter: the edit *is* the candidate). Present for engine-driven build+push+set-image.

### 3.1 Build mode: how an edit becomes the thing measured

Between "the agent edited a file" and "`measure_cmd` read a score" sits a step whose shape you must
choose when you design a pack. It differs **per component**, it is the dominant term in
per-iteration wall-clock, and picking it wrong fails silently. Full rationale in
[ADR-0020](./adr/0020-candidate-build-modes.md).

| Mode | When it applies | What `apply_cmd` does | Cost |
| --- | --- | --- | --- |
| **no artifact** | the gate compiles + runs the workspace in place | nothing (omit `apply_cmd`) | seconds |
| **no rebuild** | the agent changes *config* of a live rig | push the config, wait for rollout | a rollout |
| **derive-layer** | the changed sources are **interpreted** (Python) | append them onto a pinned base as a real OCI layer | ~8s |
| **image** | the changed sources are **compiled** (Go, Rust, C++) | a real container build; the compile happens here | minutes |

Rules that are easy to get wrong, and that `crucible check` should catch before you spend a turn:

- **`derive-layer` requires that the base image and the push target share a registry.** The layer
  mounts server-side only when they match; across registries the same operation streams every base
  blob (8 seconds becomes 20+ minutes) and nothing warns you.
- **`derive-layer` cannot carry compiled sources.** Appending a `.go` file to an image changes
  nothing that runs. The loop measures the base image forever and reports every candidate as a
  no-op, the failure mode ADR-0007 exists to catch.
- **A compile failure must be distinguishable from a bad score.** In `image` mode the build is the
  loop's fastest feedback signal: a compile error is returned to the agent as a *free retry* with no
  candidate spent. An `apply_cmd` that collapses "did not compile" into "scored badly" throws that
  away.
- **A Containerfile on the measured path is part of the judge, not part of the solution.** If the
  build recipe lives in the agent's workspace, it must be a `frozen = true` inject (§1), re-copied
  before every scored measure. Otherwise the agent can edit the recipe that builds the artifact it
  is scored on (vendor a prebuilt binary, neuter the compile), which is the ADR-0001 trust line,
  broken.

### `snapshot_cmd` / `restore_cmd` (domain reversibility): optional, must come as a pair
- `snapshot_cmd`: **stdout last line = one opaque token** (any non-empty string; base64 it if
  it contains newlines). Nonzero exit ⇒ snapshot failed (engine aborts the keep).
- `restore_cmd`: receives the token in env **`CRUCIBLE_TOKEN`**. Rolls external state back to
  that token. Nonzero exit ⇒ restore failed (engine surfaces it). (Env, not argv, so a
  multi-line base64 payload round-trips without shell-quoting.)
- The engine treats the token as **opaque** and never parses it. (§5 explains how the engine
  frames its own git ref alongside this token.)

### `setup_cmd` (prepare the workspace): optional
- **The one cwd exception:** runs with **cwd = manifest dir** (the workspace does not exist
  yet, it's what setup creates). Every other command runs with cwd = workspace.
- Default when omitted: engine does `git clone [repo] <workspace> && git checkout [ref]`. The
  workspace must end up a git repo (GitWorld commits into it); for a non-git `[repo].path`,
  give a `setup_cmd` that copies the tree in and `git init`s it (see `examples/counter/`).

---

## 4. The decide rule (universal, no per-domain code)

Given a `Reading { valid, score, solved, note, detail }`, the current `best_score`, and the
manifest `direction`:

```
keep   = valid && score.is_some() && (better(score, best_score, direction) || solved)
solved = reading.solved
better(s, b, lower)  = s < b
better(s, b, higher) = s > b
```

- **`solved` implies `keep`.** A win is the whole point, so a candidate the measure command
  declares `solved` is kept (and terminates the loop) *even if its score doesn't strictly beat
  best*. This is load-bearing for any domain whose win lands at an equal score: EPP's test gate
  wins with a green suite (0 failures == the baseline's 0) plus a new regression test. Without
  `|| solved` that win would be discarded and the loop would never finish. `solved` never
  rescues an invalid reading.
- The **first valid reading** sets the baseline (`best_score`, and `baseline_total` =
  `detail.total` if present) and is always kept.
- The loop terminates when a kept iteration is `solved`, or budget/iterations exhausted, or
  stop/escalate.
- **No domain Rust decides anything.** A win condition more complex than "better score" (e.g.
  EPP's "green AND a new regression test") is computed *inside the measure command*, which
  reads `CRUCIBLE_BASELINE_TOTAL` and emits `solved`.

---

## 5. World = reversibility (engine never names git)

`World::Snapshot = String`, opaque to the engine (`run_loop` only round-trips it back to
`restore`).

- **GitWorld** (default): `snapshot()` = stage+commit the workspace, return the commit SHA;
  `restore(sha)` = `git reset --hard <sha>` + `git clean` (excluding `.claude/`, `RESULTS.md`).
  The kept-commit chain **is** the memory. Works for any git repo, zero domain code.
- **CommandWorld** (any `[world]` command given): always owns git memory as above, **and**
  layers the domain commands. The snapshot token it stores is the composite
  `"<git-sha>\t<domain-token>"`:
  - keep: commit (git half) → run `snapshot_cmd`, capture its token (domain half) → join.
  - discard: split → `git reset --hard <sha>` + clean → run `restore_cmd` with
    `CRUCIBLE_TOKEN=<domain-token>`.
  - The engine exposes `last_commit_sha()` (the git half) for `kept_shas`/publish; the domain
    half is never inspected.

The engine's loop body calls **only** `world.snapshot()` / `world.restore(&snap)`. It contains
no `git`/`vcs::` calls and no `kubectl`.

---

## 6. Agent transport (`AgentSource`)

The engine renders a prompt (`method_prompt` with `{{GOAL}}`/`{{STATUS}}`/`{{STEER}}` filled),
hands it + the workspace to an agent that edits the workspace, and never hands it the Judge
(ADR-0001). Backends, selected by `[agent].backend`:

- **`local`**: direct `claude --output-format stream-json` with `[agent].env`
  (real Claude/Vertex turn).
- **`openshell`**: sandboxed pod turn driven by the in-Rust OpenShell driver
  (`backend = "openshell"`, `sandbox_image`).
- **`command`**: run `[agent].agent_cmd` via `sh -c` in the workspace as the proposal. A
  *deterministic, free* proposer (no LLM). This is a real transport, not a mock: it makes the
  minimal example a fast, deterministic e2e (e.g. `agent_cmd = "./bump.nu"` increments a
  counter). Use it to test the engine's loop/protocol without burning tokens.

### One vs two execution environments (the only backend fact a domain must know)

**The engine and its `measure`/`apply`/`snapshot`/`restore`/`setup` commands always run where
`crucible` runs.** Only the *agent turn* is sandboxed under `openshell`. So:

- **`local` / `command`**: one environment. Agent and engine share the host PATH + filesystem;
  the agent edits the workspace in place. Provide one toolbox on PATH. Nothing else.
- **`openshell`**: two environments. (1) the engine/loop-image PATH runs the contract commands;
  (2) a **separate sandbox image** runs the agent's skills. The OpenShell driver uploads the
  workspace into the sandbox and syncs edits back, so anything the agent needs to reach the
  outside (kubeconfig, tokens) must be **relayed via `[[agent.relay]]` or provided in
  `[agent].env`** (the sandbox is network/fs-isolated). A domain therefore ships *two*
  tool surfaces under openshell: contract commands on the loop PATH, agent skills baked into
  `sandbox_image`.

The manifest `[agent]` block (`backend`, `sandbox_image`, `env`, `relay`, `openshell`) *selects and
configures* the backend; flipping `local`↔`openshell` is config, not code. What the manifest
cannot do is *build* the sandbox image or inject creds for you, that's the inherent cost of
sandboxing, called out here so it's a known rule, not a surprise. Reversibility commands
(`snapshot`/`restore`) run engine-side, so they reach the live system directly regardless of
backend; only the agent is boxed.

### 6.1 Sandbox egress (`[agent.openshell]`)

The sandbox is deny-by-default. Two lists open it back up, and a flag decides whether they
extend the engine's built-ins or replace them:

```toml
[agent.openshell]
endpoints = ["api.example.com:443:full"]  # host:port:access[:proto[:enforcement]]
binaries  = ["/usr/local/bin/claude"]     # only these may open a socket
inherit_defaults = true                   # default
```

With `inherit_defaults = true` (the default) the lists are appended, de-duplicated, to the
built-ins: the public forges, PyPI, Vertex, Anthropic, and the agent CLIs. Appending can never
*remove* a built-in.

Set `inherit_defaults = false` and the resolved allowlist is exactly what the manifest names,
binaries included. This is the only way to subtract a default, and it is required for two cases:

- **Air-gapped or private-registry runs**, where the public internet is not reachable (or not
  permitted) and the agent must talk to an in-cluster model endpoint and registry instead.
- **Contamination control.** An agent scored on an upstream issue can, with the default
  allowlist, read the upstream *fix* for that issue on `github.com`. A measurement that must
  not be polluted by the open web has to drop that endpoint, and dropping it means opting out.

The broker endpoint is **auto-appended** by the engine when `[agent.broker].enabled` is true
(ADR-0019 P2). The engine first resolves the broker URL (an explicit `[agent.broker].url`
override, or derived from the active compute driver's hostname), then derives the egress
`host:port:full` entry from that URL's authority. When no explicit port is present, the scheme
default applies (`http` = 80, `https` = 443). Because both are derived from the same resolved
URL, the allowlist entry and the address the sandbox contacts cannot disagree. The broker
endpoint is appended regardless of `inherit_defaults`, because the broker is engine plumbing
the domain opted into, not a built-in the domain can subtract. A broker-less opt-out with both
lists empty is still a legal total air-gap: nothing resolves, and no binary may open a socket.

---

## 7. Session wire format (compatibility)

The NDJSON session log (`state/session.jsonl`) keeps its existing event kinds
(`start`/`phase`/`row`/`budget`/`summary`/`finished`) and **field names unchanged**. In
particular the objective label is still written under the JSON key **`gate`** (now carrying a
free-text label like `"score"`/`"bench"`, not the deleted enum). This keeps `--resume`, the
remote viewer, and already-published S3 runs loading. Do **not** rename the wire key.

A `row` event's wire record (`RowWire`) carries an additive, optional `phase` field
(`"wide"` for a wide-round candidate row, absent for a deep-loop row). It's `skip_serializing_if
= "Option::is_none"`, so a deep-only run's wire bytes are unchanged from before wide rounds
existed.

Additive event kinds beyond the compat set include:

- **`identity`**: the run's `RunIdentity` (below), emitted once at setup and again on
  `--resume` (the freshly recomputed identity). A mismatch against the original run's identity is
  a hard-warning `note` event, never an abort.
- **`shutdown`**: `{ outcome, reason }`, emitted **exactly once**, as the **last** line of every
  run (after `finished`/`summary`). `outcome` is one of `finished`/`solved`/`budget`/`stopped`/
  `escalated`/`error`. Session-log consumers key a run's terminal state off this line; a dead
  stream with **no** `shutdown` line means the pod likely died mid-run, not a clean exit.
- **`agent_session`**: `{ session, action, turn }`, emitted before a persistent agent turn so a
  viewer can draw continuation lanes and distinguish `started` from `resumed`. It deliberately
  contains neither the provider cursor nor native transcript content.

**`RunIdentity`** (`crucible/src/identity.rs`) is the comparability key: two runs' scores are
comparable only if it matches. It's a hash-of-hashes (`v1:<hex>`) over, per component (one
unnamed entry for a single-domain run, one per `[[component]]` for a composite): `repo`
(`[repo].url`/`path`) and the workspace's pristine base commit SHA; plus, once per run: the
frozen manifest text's hash, a hash over every `[[workspace.inject]]`'s source content plus
destination path, `[judge].measure_cmd`, and `[judge].direction`. It's computed once at run
setup and doesn't change within a run (a re-scope moves the loop's own `Segment` fingerprint,
a different hash over goal/objective/regime; the two are deliberately independent).

---

## 8. What a domain author writes (the whole surface)

1. a `crucible.toml`,
2. a `measure` command emitting `{valid, score, solved?}` (any language),
3. optionally `apply`/`snapshot`/`restore` commands,
4. a method prompt + goal,
5. agent creds in `[agent].env`.

Everything else (loop, budget, keep/discard, all reporters + remote viewer, steer/stop/resume,
session log, escalation, git memory) is the engine's, for free. The litmus test
(`examples/counter/`) exercises items 1, 2, 4 with GitWorld and the `command` backend, no Rust.

---

## 9. CLI surface (non-normative pointer)

The manifest/protocol above is the contract; these subcommands are mechanical consumers of it,
noted here tersely so this doc stays the map of what's authoritative:

- **`crucible init [--dir <path>]`**: scaffolds a minimal `crucible.toml` + a measure stub that
  always reports the same score. Refuses to overwrite existing files.
- **`crucible check --manifest <path>`**: validates a manifest with no agent turn (parses it,
  unknown keys are a parse error, resolves every referenced file, runs `measure_cmd` once to
  prove the measure contract, runs `[judge.selftest]` if declared (§1.1), and warns if the gate
  is reachable by the agent's own edits: a `measure_cmd` token pointing inside the workspace with
  no matching frozen inject). Exit nonzero with findings on failure; warnings never fail it.
- **`crucible scope --pack <dir> [--issue owner/repo#N | --goal-file <f>] [--force] [--json]`**
  (ADR-0014 S0): pipeline over a hand-written domain pack. **ingest** the goal (from `--issue`,
  fetched natively from the GitHub REST API (honors `GITHUB_TOKEN`/`GH_TOKEN` and
  `GITHUB_API_URL`), or `--goal-file`, or the pack's own `[agent].goal`/`goal_file`),
  then **validate** (`crucible check`, as a library call), then **freeze** (writes `SCOPE.md` in
  the pack dir with the goal, the check outcome, and the pack's `RunIdentity` digest). Stops at
  the first failing stage. `S2` (propose)/`S3` (preflight)/`S4` (approval) are listed in
  `SCOPE.md` as pending, see [ADR 0014](./adr/0014-scoping-pipeline.md). A `--goal-file`'s text is
  goal framing under the same de-prescription rules as a `--issue`'s title/body: the problem for
  the pack-designing agent to solve, never a solution. §8's `_controls`/self-test strip at
  freeze applies identically regardless of which arm sourced the goal.
- **`crucible ps [--namespace <ns>] [--json]`**: lists loop pods across the cluster, selecting on
  the `app.kubernetes.io/managed-by=crucible` label every rendered loop pod carries. `ITER` ships
  as `-` (reserved, see `ps.rs`'s module doc for why it isn't wired up yet).
- **`crucible deploy render|apply --manifest <path> --profile <path> [--iterations N] [--no-pin]`**:
  renders (or renders-then-applies) the loop pod + a cross-namespace RoleBinding from the
  manifest + a deploy profile, image tags resolved to `@sha256:…` digests. Works for a composite
  manifest **or** a plain single-domain one (the latter needs its own `[deploy]` block naming the
  build/deploy target, a single domain is a degenerate composite of one).
- **`crucible watch-pr --pr <url> [--pr <url> ...] (--control-addr <host:port> | --reseed <path>)
  [--once] [--poll-secs N] [--bot-user <login>] [--allow-user <login> ...]`**: watches one or
  more draft PRs' review comments (`--pr` repeatable: a kept composite candidate opens one linked
  PR per component fork) and either steers a live run over its control bridge or appends to a
  reseed file the next run's first turn reads. `--once` fetches and exits instead of polling.
