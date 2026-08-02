# Getting started: zero to a running loop

This is the hands-on path from nothing to a crucible loop. It's deliberately
end-to-end, two acts:

1. **Run a loop locally** — the hello-world domain, no cluster, no LLM, runs in milliseconds.
2. **Onboard your own domain** — the manifest + the `measure` contract, the only two things you have to write.

For the *why* behind any of this, read [How it works](./how-it-works.md) (the diagram +
glossary) and [What crucible is](./crucible.md) first. The manifest and command shapes
below are the hands-on expansion of [the contract](./crucible-contract.md), which is the
normative reference. When this page and the contract disagree, the contract wins.

## Before you start

**Tooling** (from the [README](https://github.com/neuralmagic/crucible)): Rust + cargo, go,
`kubectl`/`oc`, `helm`/`helmfile`, [`just`](https://github.com/casey/just),
[`nu`](https://www.nushell.sh/) (nushell, the glue language), `podman`/`buildah`, `gitleaks`,
and the `claude` CLI. Act 1 only needs Rust, `just`, and `nu`.

**Credentials** (Act 2 with a real agent backend): Claude access for the `claude` CLI. The
`command` backend and `crucible init`/`check`/`scope` need no credentials at all.

**Build the binary.** Clone the repo (Act 1 uses the `examples/counter` files anyway) and
build from source:

```bash
git clone https://github.com/neuralmagic/crucible.git
cd crucible
cargo build --release -p crucible
install -m 755 target/release/crucible ~/.local/bin/   # or anywhere on PATH
```

Or grab a prebuilt binary from the
[releases page](https://github.com/neuralmagic/crucible/releases) when one is available for
your platform. With just the binary you can already onboard your own repo (Act 2's
`crucible init` → `crucible check` → `crucible scope` path needs nothing else).

One convention worth knowing before Act 2: a manifest refers to a pack's tools by **bare
name**. The counter example sidesteps this entirely (its `bump.nu` and `measure.nu` live in
the pack directory and are invoked by relative path); a pack that ships tool binaries is
responsible for putting them on `$PATH` before the loop runs.

---

## Act 1 — Run a loop locally

The fastest way to see the whole engine is `examples/counter`. The "repo under test" is a git
tree whose entire source is an integer in `value.txt`; the score *is* that integer; a
deterministic `command` backend (`bump.nu`) stands in for the LLM so it runs for free. It
exercises the real generic path (manifest → setup → propose → measure → keep/discard → git
memory) with no cluster and no model.

```bash
crucible --manifest examples/counter/crucible.toml --iterations 6
# or, from a source checkout: cargo run -p crucible -- --manifest examples/counter/crucible.toml --iterations 6
```

The manifest is the whole story (`examples/counter/crucible.toml`):

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

**What happens each iteration:** the engine runs `agent_cmd` to mutate the workspace, runs
`measure_cmd` to score it, and keeps the change (a git commit) iff it's strictly better per
`direction`, else rolls the workspace back. What you get out:

- `examples/counter/state/session.jsonl` — the append-only NDJSON event log (the source of truth for a run).
- The workspace git history — kept iterations are commits. **Git is the memory.**

That's the engine. Everything else is swapping the deterministic proposer for a real agent and
pointing `measure` at a real objective.

---

## Act 2 — Onboard your own domain

A domain is **a manifest plus a few executables**, not Rust. The litmus test for "framework,
not demo" is that a new domain runs with no engine changes. You write two things: a
`crucible.toml` and a `measure` command.

`crucible init` scaffolds both onto the current directory (a minimal manifest + a measure stub
that always reports the same score), so onboarding starts as editing two generated files rather
than writing them from scratch. Once you've pointed `[judge].measure_cmd` at the real gate,
`crucible check --manifest crucible.toml` validates the whole thing (every referenced file
resolves, `measure_cmd` runs once and prints the contract shape, and it warns if the gate sits
unprotected inside the workspace the agent edits) before you spend a real agent turn on it.
`crucible scope --pack <dir>` chains ingest (resolve the goal) → `check` → render
`WORKFLOW.png` → freeze (`SCOPE.md` with a `RunIdentity` digest) into one pipeline (ADR-0014 S0);
add `--issue owner/repo#N` to
ground the goal in a GitHub issue (native API fetch, honors `GITHUB_TOKEN`/`GH_TOKEN`).

### The manifest

Sections (full schema + per-field semantics in [the contract](./crucible-contract.md); the
parser rejects unknown keys, so typos fail loudly):

| Section | Purpose | Key fields |
| --- | --- | --- |
| `[repo]` | The code under test. | `url` or `path`, `ref` |
| `[workspace]` | Where it's checked out + fixtures. | `dir`, `setup_cmd`, `[[workspace.inject]]` (`src`/`dst`/`frozen`) |
| `[agent]` | Who proposes and how. | `model`, `backend`, `method_prompt`, `goal`/`goal_file`, `toolbox_dir`, `[agent.env]`, `[agent.openshell]`, `[agent.broker]` |
| `[judge]` | The frozen objective. | `measure_cmd`, `direction` (`lower`/`higher`), `[judge.selftest]` (`good_cmd`/`bad_cmd`) |
| `[world]` | Reversibility beyond git. | `apply_cmd`, `snapshot_cmd`, `restore_cmd` (omit all three → pure `GitWorld`) |
| `[deploy]` | Image rebuild targets (deploy domains). | `[deploy.buildah]` `registry`/`dockerfile`, `[deploy.env]` |
| `[search]` | Wide-round fan-out before the deep loop. | `wide`, `approaches` (required when `wide > 0`), `policy_k` |
| `[composite]` | Assemble N domains into one run. | `[composite].name`, `[[component]]` `domain`/`pr_repo` |

The single most important rule: **omit `[world]` entirely and you get `GitWorld`** (git is the
only thing that needs rolling back). Set any of the three `*_cmd`s and you get `CommandWorld`
(git plus your snapshot/restore, e.g. a live cluster).

### The `measure` contract

`measure_cmd` is the judge. Its **last stdout line beginning with `{`** is parsed as JSON:

```json
{ "valid": true, "score": 234.1, "solved": false, "note": "p99 234.1ms" }
```

- `valid` (required bool), `score` (number), `solved` (bool, default false), `note`/`detail` (optional).
- A **nonzero exit forces `valid=false`** regardless of what you printed.
- The engine injects `CRUCIBLE_BASELINE_SCORE`, `CRUCIBLE_BASELINE_TOTAL`, `CRUCIBLE_BEST_SCORE` so your gate can compare.
- Keep rule: keep iff `valid` and `score` strictly beats the best per `direction` (or `solved` is true, which forces a keep and ends the run, that's the bug-fix-gate case).

`measure_cmd` is any executable that prints one JSON object. A plain `sh` script is a
complete judge:

```sh
#!/bin/sh
p99=$(make bench | jq .p99_ms)
printf '{"valid": true, "score": %s, "note": "p99 %sms"}\n' "$p99" "$p99"
```

Any language works; the engine only reads stdout.

### Backends: who actually proposes

Set by `[agent].backend`:

- **`local`** (default) — spawns the `claude` CLI on this machine in the workspace. Needs the CLI + Claude creds.
- **`openshell`** — runs the turn inside a sandboxed pod (Landlock + deny-by-default egress). Needs `sandbox_image` + an `[agent.openshell].endpoints` egress allowlist. This is the in-cluster path.
- **`command`** — a fixed `sh -c` command as the "proposal," deterministic, no LLM, cost 0. The Act 1 counter case.

### Composite domains

A manifest with a top-level `[composite]` table assembles ≥2 existing component domains
(reused verbatim) into one run with a single combined gate. Picture a `server` domain and a
`router` domain: each keeps its own workspace and repo, but one `measure_cmd` scores the
assembled system, so a change to either component is judged by how the pair behaves
together. No engine changes; the engine just runs a vector of workspaces. Beyond the
manifest, the work of a composite is the shared plumbing: a combined gate that exercises
both components, and per-component deploy targets if the domains rebuild images.

---

## Where to go next

- [How it works](./how-it-works.md) — the full diagram + glossary.
- [What crucible is](./crucible.md) — the conceptual deep-dive and the contract table.
- [Implementation contract](./crucible-contract.md) — the normative manifest + command spec.
- [JIRA tools (mediated)](./jira-proxy.md) — grounding a goal in an issue.
