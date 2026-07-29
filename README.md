# crucible

A **goal-driven, gated, keep/discard autoresearch loop** for letting an agent improve a
codebase against a frozen objective. Each iteration the agent reads the history, forms one
hypothesis, changes the world, and is gated on a single measurement: keep if it strictly
beats the best so far, else revert. **Git is the memory.** The judge is frozen, the agent
can change its solution but never its evaluation ([ADR-0001](docs/adr/0001-adaptive-harness.md)).

The headline artifact is the **loop engine**: the Run/Segment/Iteration state machine that
drives propose → apply → measure → keep/discard, `crucible deploy render|apply` for
projecting digest-pinned loop pods from a manifest, the OpenShell Kubernetes driver for
sandboxed agent turns, the mediated MCP broker (the host holds the keys, the agent can only
ask), and `forge` for engine-side image builds. The engine is domain-neutral: a *domain* is
a `crucible.toml` manifest plus a few executables, in any language. Full docs live in the
**[book](https://neuralmagic.github.io/crucible/)** (`just book-serve` locally); the
normative reference is [the contract](docs/crucible-contract.md); the *why* is in the
[ADRs](docs/adr/index.md).

## Install

Build from source (stable Rust toolchain):

```bash
git clone https://github.com/neuralmagic/crucible.git && cd crucible
cargo build --release -p crucible
install -m 755 target/release/crucible ~/.local/bin/   # or anywhere on PATH
```

Or grab a prebuilt binary from the
[releases page](https://github.com/neuralmagic/crucible/releases) when one is available
for your platform.

### Tooling

- A stable [Rust toolchain](https://rustup.rs) to build the engine.
- [nushell](https://www.nushell.sh) to run the counter example's scripts
  (`bump.nu` / `measure.nu`) and the `tools/*.nu` control-plane glue.

## Quick start (no cluster, no LLM, milliseconds)

```bash
git clone https://github.com/neuralmagic/crucible.git && cd crucible
crucible --manifest examples/counter/crucible.toml --iterations 6
```

`examples/counter` is the whole engine in miniature: the "repo" is an integer in
`value.txt`, the score *is* that integer, and a deterministic `command` backend stands in
for the LLM. It exercises the real path (manifest → setup → propose → measure →
keep/discard → git memory) for free.

## Quick start: your own repo

The binary alone is enough to onboard a repo, no clone of this one needed:

```bash
cd your-repo
crucible init                                  # scaffold crucible.toml + a measure stub
crucible check --manifest crucible.toml        # validate the contract, no agent turn spent
crucible scope --pack . --issue owner/repo#N   # optional: ground the goal in a GitHub issue
                                               # (native API fetch, honors GITHUB_TOKEN/GH_TOKEN)
git add crucible.toml crucible-measure.sh && git commit -m "add crucible pack"
crucible --manifest crucible.toml --iterations 6
```

Commit the generated `crucible.toml` and `crucible-measure.sh` before running: the loop
clones your repo into `workspace/`, so uncommitted files never make it into the run. If a
first run fails early it can leave an empty `workspace/` behind; delete it before
retrying.

## The contract, in one breath

A domain's `crucible.toml` names the code under test (`[repo]`), who proposes
(`[agent]`: backend `local` = the `claude` CLI, `openshell` = a sandboxed in-cluster pod,
`command` = deterministic), the frozen objective (`[judge].measure_cmd`, whose last JSON
stdout line is `{"valid": true, "score": 234.1, ...}`), and reversibility (`[world]`,
omit it entirely for pure git rollback). Two optional tables sharpen the search and the
trust story: `[judge.selftest]` declares negative controls (`good_cmd`/`bad_cmd`) that
`crucible check` uses to prove the gate can discriminate
([ADR-0014](docs/adr/0014-scoping-pipeline.md)), and `[search]` configures a wide/explore
round: N approach-biased candidates proposed in parallel, ranked by the gate, the winner
seeding the normal deep loop ([ADR-0010](docs/adr/0010-candidate-portfolios-and-search.md)).
A top-level `[composite]` table assembles N component domains into one run with a single
combined gate ([ADR-0009](docs/adr/0009-composite-domains.md)).

## Authoring a domain

`examples/counter` is the complete template. Its manifest points the engine at a tree, a
deterministic proposer, and a measure command:

```toml
[repo]
path = "."

[agent]
backend   = "command"       # deterministic proposer; swap for "local" or "openshell"
agent_cmd = "./bump.nu"     # the "agent": value.txt += 1
goal      = "Raise the integer in value.txt as high as you can (target: 5)."

[judge]
measure_cmd = "./measure.nu"
direction   = "higher"
```

`measure.nu` is the entire judge, in any language you like:

```nu
#!/usr/bin/env nu
let v = (open value.txt | str trim | into int)
{valid: true, score: $v, solved: ($v >= 5), note: $"value=($v)"} | to json -r
```

A real domain replaces the counter's pieces one at a time: the tree becomes your repo, the
proposer becomes an LLM backend, and the measure command becomes your benchmark or test
gate. Organizations keep their domain packs (manifests, prompts, goals, judges, tools) in
their own repositories; the engine loads any pack via `--manifest`, and nothing about a
pack requires changes to this repo.

## The command surface

| Command | What it does |
| --- | --- |
| `crucible --manifest <m> --iterations N` | run the loop; add `--wide N` for an explore round first |
| `crucible init` / `crucible check` | scaffold a manifest + measure stub in your own repo; validate the contract, run the gate self-test |
| `crucible scope --pack <dir> [--issue owner/repo#N]` | the scoping pipeline: ingest → validate → freeze report with the run-identity digest |
| `crucible deploy render\|apply` | render the loop pod + RBAC from manifest + cluster profile (single-domain or composite), digests pinned |
| `crucible ps [--namespace] [--json]` | list every crucible loop pod in the cluster |
| `crucible watch-pr --pr <url>... (--control-addr \| --reseed <f>)` | feed authorized PR review comments into a live run or the next one |
| `crucible build <name>` | dispatch a named `[build.<name>]` from the manifest and print the digest-pinned ref ([ADR-0018](docs/adr/0018-declarative-image-builds.md)) |

An optional private control plane can sit above the engine: a controller that discovers
candidate issues, scopes them into packs, launches runs, and records outcomes. It speaks
the public `crucible-contract` ingest API over HTTP; loop pods report to it but never link
it, so the engine runs identically with or without one.

## Layout

```
crucible/          the domain-neutral engine (loop driver, manifest, deploy render, TUI)
crucible-contract/ the wire contract: turn results, ingest types shared across binaries
crucible-vcs/      git-as-memory plumbing
crucible-harness/  OTLP turn-result emission
crucible-broker/   generic MCP broker: the host-side privileged interface the sandboxed
                   agent can only ask (build/deploy/measure/profile/trace)
forge/             build/deploy/kube plumbing the engine and broker share
tools/*.nu         control-plane glue on PATH (steer, stop, escalate, session)
examples/counter   the canonical litmus domain (no cluster, no LLM)
docs/              the mdbook: concepts, contract, ADRs
```

## Running for real

- **Local loop:** point `crucible --manifest` at any pack; the `local` backend drives the
  `claude` CLI directly.
- **In-cluster loop:** the openshell backend runs the agent in a sandboxed pod; the loop
  pod + RBAC are rendered from the manifest + a per-cluster profile via
  `crucible deploy render|apply` ([ADR-0012](docs/adr/0012-rendered-deployments.md)).
- **Steering:** `steer` nudges a live run; Ctrl+C stops cleanly, restores the world, and
  summarizes.

## Development

```bash
cargo build --release -p crucible
just lint              # fmt + clippy
cargo test -p crucible # engine tests (manifest canaries, loop driver, deploy render)
```

CI runs everything under `.github/workflows`; it behaves the same locally.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
