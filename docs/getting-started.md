# Getting started: zero to a running loop

This page shows how to start an autoresearch loop from nothing. For a graph that runs one
time, start with [Your first playbook](./playbooks.md). This page has two parts:

1. **Run a loop locally.** The example domain needs no cluster and no LLM, and it runs in
   milliseconds.
2. **Onboard your own domain.** You write two things: the manifest and the `measure`
   command.

[How the loop works](./how-it-works.md) (the diagram and glossary) and
[The autoresearch loop, in one read](./crucible.md) explain the design. The manifest and
command formats on this page expand on [the contract](./crucible-contract.md), which is the
normative reference. If this page and the contract are different, the contract is correct.

## Before you start

**Tools.** To build, you need Rust and Cargo. Part 1 also needs
[`nu`](https://www.nushell.sh/) (nushell, the glue language). Development and deployment use
more tools: Go, `kubectl` or `oc`, `helm` and `helmfile`,
[`just`](https://github.com/casey/just), `podman` and `buildah`, `gitleaks`, and the
`claude` CLI.

**Credentials.** Part 2 with a real agent backend needs Claude access for the `claude` CLI.
The `command` backend and `crucible init`, `check`, and `scope` need no credentials.

**Build the binary.** Clone the repository (part 1 uses the `examples/counter` files) and
build from source:

```bash
git clone https://github.com/neuralmagic/crucible.git
cd crucible
cargo build --release -p crucible --features autoresearch
install -m 755 target/release/crucible ~/.local/bin/   # or anywhere on PATH
```

You can also download a prebuilt binary from the
[releases page](https://github.com/neuralmagic/crucible/releases), if there is one for your
platform. The binary alone is sufficient to onboard your own repository with the
`crucible init` → `crucible check` → `crucible scope` steps of part 2.

Before part 2, note this convention: a manifest refers to the tools of a pack by **bare
name**. The counter example does not use this convention, because `bump.nu` and `measure.nu`
are in the pack directory and the manifest calls them by relative path. A pack that ships
tool binaries must put them on `$PATH` before the loop runs.

---

## Part 1: run a loop locally

`examples/counter` is the fastest way to see the full engine. The repository under test is a
Git tree with one integer in `value.txt`. The score is that integer. A deterministic
`command` backend (`bump.nu`) replaces the LLM, so the run costs nothing. It uses the real
generic path (manifest → setup → propose → measure → keep/discard → Git history) without a
cluster or a model.

```bash
crucible --manifest examples/counter/crucible.toml --iterations 6
# or, from a source checkout: cargo run -p crucible --features autoresearch -- --manifest examples/counter/crucible.toml --iterations 6
```

The manifest (`examples/counter/crucible.toml`) defines the full run:

```toml
[repo]
path = "."

[workspace]
dir = "workspace"
setup_cmd = "mkdir -p workspace && cp value.txt measure.nu bump.nu workspace/ && git -C workspace init -q && git -C workspace add -A && git -C workspace -c user.email=crucible@local -c user.name=crucible -c commit.gpgsign=false commit -qm baseline"

[agent]
backend   = "command"
agent_cmd = "./bump.nu"   # deterministic proposer: value.txt += 1
goal      = "Raise the integer in value.txt as high as you can (target: 5)."

[judge]
measure_cmd = "./measure.nu"
direction   = "higher"
objective   = "value"

# No [world] block -> GitWorld: kept iterations are commits, discards reset --hard + clean.
```

**Each iteration:** the engine runs `agent_cmd` to change the workspace and runs
`measure_cmd` to score it. It keeps the change as a Git commit only if the score is strictly
better in the `direction`. If not, it rolls the workspace back. The run writes:

- `examples/counter/state/session.jsonl`: the append-only NDJSON event log. It is the source
  of truth for the run.
- The Git history of the workspace. Each kept iteration is a commit.

To use the engine on real work, replace the deterministic proposer with a real agent and
point `measure` at a real objective.

---

## Part 2: onboard your own domain

A domain is **a manifest and some executables**. It is not Rust. A new domain runs without
engine changes. You write two things: a `crucible.toml` and a `measure` command.

`crucible init` creates both in the current directory: a minimal manifest and a measure
script that always reports the same score. You then edit the two generated files. After you
point `[judge].measure_cmd` at the real gate, run `crucible check --manifest crucible.toml`.
It validates the configuration before you spend an agent turn:

- every referenced file resolves;
- `measure_cmd` runs one time and prints the contract format;
- it warns if the gate is unprotected in the workspace that the agent edits.

`crucible scope --pack <dir>` runs one pipeline (ADR-0014 S0): it resolves the goal, runs
`check`, renders `WORKFLOW.png`, and freezes `SCOPE.md` with a `RunIdentity` digest. Add
`--issue owner/repo#N` to take the goal from a GitHub issue. It fetches the issue through the
GitHub API and uses `GITHUB_TOKEN` or `GH_TOKEN`.

### The manifest

[The contract](./crucible-contract.md) gives the full schema and the meaning of each field.
The parser rejects unknown keys, so a typo causes an error.

| Section | Purpose | Key fields |
| --- | --- | --- |
| `[repo]` | The code under test. | `url` or `path`, `ref` |
| `[workspace]` | The checkout location and fixtures. | `dir`, `setup_cmd`, `[[workspace.inject]]` (`src`/`dst`/`frozen`) |
| `[agent]` | The proposer and its settings. | `model`, `backend`, `method_prompt`, `goal`/`goal_file`, `toolbox_dir`, `[agent.env]`, `[agent.openshell]`, `[agent.broker]` |
| `[judge]` | The frozen objective. | `measure_cmd`, `direction` (`lower`/`higher`), `[judge.selftest]` (`good_cmd`/`bad_cmd`) |
| `[world]` | Restore of state outside Git. | `apply_cmd`, `snapshot_cmd`, `restore_cmd` (omit all three for `GitWorld`) |
| `[deploy]` | Image rebuild targets (deploy domains). | `[deploy.buildah]` `registry`/`dockerfile`, `[deploy.env]` |
| `[search]` | Wide round before the deep loop. | `wide`, `approaches` (required if `wide > 0`), `policy_k` |
| `[composite]` | Combine N domains into one run. | `[composite].name`, `[[component]]` `domain`/`pr_repo` |

The most important rule: **if you omit `[world]`, you get `GitWorld`**, and Git is the only
state that the engine rolls back. If you set any of the three `*_cmd` keys, you get
`CommandWorld`: Git and your snapshot and restore commands, for example for a live cluster.

### The `measure` contract

`measure_cmd` is the judge. The engine parses the **last stdout line that starts with `{`**
as JSON:

```json
{ "valid": true, "score": 234.1, "solved": false, "note": "p99 234.1ms" }
```

- `valid` (required bool), `score` (number), `solved` (bool, default false), `note` and
  `detail` (optional).
- A **nonzero exit status sets `valid=false`**, whatever the command printed.
- The engine sets `CRUCIBLE_BASELINE_SCORE`, `CRUCIBLE_BASELINE_TOTAL`, and
  `CRUCIBLE_BEST_SCORE`, so your gate can compare.
- Keep rule: the engine keeps a candidate only if it is `valid` and its `score` is strictly
  better than the best in the `direction`. If `solved` is true, the engine keeps the
  candidate and ends the run. Use `solved` for a bug-fix gate.

`measure_cmd` is any executable that prints one JSON object. A plain `sh` script is a
complete judge:

```sh
#!/bin/sh
p99=$(make bench | jq .p99_ms)
printf '{"valid": true, "score": %s, "note": "p99 %sms"}\n' "$p99" "$p99"
```

You can use any language. The engine reads only stdout.

### Backends

`[agent].backend` selects the proposer:

- **`local`** (default): runs the selected harness (default `claude`) on this machine in the
  workspace. Needs the harness CLI and its credentials.
- **`openshell`**: runs the turn in a sandboxed pod (Landlock and deny-by-default egress).
  Needs `sandbox_image` and an egress allowlist in `[agent.openshell].endpoints`. Use this
  backend in a cluster.
- **`command`**: runs a fixed `sh -c` command as the proposal. It is deterministic, uses no
  LLM, and costs nothing. The counter example in part 1 uses it.

### Composite domains

A manifest with a top-level `[composite]` table combines two or more existing component
domains, without changes to them, into one run with one combined gate. For example, a
`server` domain and a `router` domain each keep their own workspace and repository. One
`measure_cmd` scores the combined system, so the gate judges a change to either component by
the behavior of the pair. The engine does not change. It runs a list of workspaces. In
addition to the manifest, a composite needs a combined gate that tests both components, and
deploy targets for each component if the domains rebuild images.

---

## Where to go next

- [How the loop works](./how-it-works.md): the full diagram and glossary.
- [The loop in one read](./crucible.md): the concepts and the contract table.
- [Implementation contract](./crucible-contract.md): the normative manifest and command
  specification.
- [JIRA tools (mediated)](./jira-proxy.md): take a goal from an issue.
