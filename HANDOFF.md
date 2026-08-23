# Playbook lane: handoff

Date: 2026-08-22. Branch `playbook-lane`, pushed to `wseaton/playbook-lane`, 24 commits off
`main` (`7c2c1a5`). `cargo test --all` green (959 + 153 + 39 + 62 + 14), clippy silent,
`govctl check` clean.

**The suite being green means less than it looks.** An adversarial review reproduced 22 defects
the tests do not cover, two of them producing a wrong answer under a `verdict: valid` banner.
They are filed as WI-2026-08-22-006 through -009, and WI-001 is reopened. Read those before
trusting anything below.

## What a playbook is

A workflow that runs its graph **once** and produces **no score**. The lane for work whose value
is the work itself. No propose/apply/measure/decide, no frozen judge. In one sentence: the other
lane is a search, this one is a playbook.

## It works on a real paper

```sh
crucible plan run --manifest crucible.toml \
  --param paper_url=https://arxiv.org/abs/2503.01840 --max-cost 5 --max-time 20m
```

```
analyze  pass  cost=$0.8094  out={"algo_name":"eagle3","closest_model":"eagle3","confidence":"high"}
shape    pass  cost=$0.0000  out={"bytes":9025,"has_classification":true,"has_training":true}
verdict: valid
```

Real Claude, real arxiv fetch, real speculators checkout, 9KB spec captured to
`state/files/analyze/SPEC.md`, `task analyze` committed to git memory. The pack is in the
session scratchpad (`paper-playbook/`) and will not survive; it is three files and an adapted
`SKILL.md`, half an hour to rebuild. **The porting work is the interesting part**: the upstream
skill ends "Present to User, ask for approval", which a playbook turn has nobody to do. Cutting
that and appending an output contract is what each of the other five skills will need.

## Built

| | where |
| --- | --- |
| Lane admission, `type = "playbook"` | `manifest/workflow.rs`, `manifest/mod.rs` |
| Lane-scoped DSL namespace | `plan/starlark/globals.rs`, `idents.rs` |
| `params` + `param()`, JSON Schema, bound pre-evaluation | `plan/starlark/params.rs` |
| `skill()`, prompt assembled from a shipped SKILL.md | `plan/starlark.rs` (`skill_prompt`) |
| External-input marking | `plan/starlark.rs` (`render_prompt`), `values.rs` |
| `over =` fan-out, keyed by item | `plan/exec.rs`, `plan/ir.rs` |
| `emits_files`, captured through isolation | `plan/harness.rs` |
| Git memory, per task, on pass only | `plan/harness.rs` (`settled`) |
| Launcher + ceilings | `plan/cli.rs` |
| Bounded compilation | `plan/starlark.rs` |
| `tools/fake-agent.py`, model-less stand-in | `tools/` |

## Open defects, ranked

**WI-006, fan-out correctness. Two shapes are refused at compile time as of `b30949f`**, which
converts three wrong answers into two compile errors. The refusals lift when the executor can
run them: serialize the instances of a shared-workspace node, settle each before the next starts,
capture per instance. Measured before the refusal landed: four instances, every item attributed
to the wrong one, one that ran fine folded in as failed.

**WI-007, declared files.** A failed task's partial output stages into descendants (the JSON and
file channels disagree about the same task). One successful run makes a capturing task
permanently unrunnable, EACCES, because `fs::copy` propagates a staged input's 0444. A serial
producer's file never reaches an isolated consumer, 100%, because `inputs/` is in
`.git/info/exclude` and the worktree is populated from `git add -A`.

**WI-008, parameters.** Whether `--param` is honoured, validated, or silently dropped depends on
how the graph happens to be spelled. `materialize_manifest` deletes `file =` and freezes one
launcher's values into the tracked manifest; `scope` does it unattended with defaults.

**WI-009, marking.** The close marker is stripped with one non-overlapping `replace`, so a value
carrying a nested marker reassembles a working one and escapes the region. The open marker is
never stripped. The existing test feeds exactly one un-nested marker, which is why CI is green.

**WI-001, reopened.** `lambda` scores zero in the nesting scanner: `'lambda: ' * 4800` aborts the
process. `declared_params` parses on the caller's thread with no size cap, reachable in release.
The recursion that overflows is starlark's module-compile pass, not the parser, which the
original fix's reasoning had wrong.

**Accepted for now** (loud, deterministic, no state left behind): the scanner refuses some
ordinary flat literals; `emits_files = ["./A.md"]` compiles and can never capture; the cost
ceiling is checked once per fan-out wave rather than per instance; a non-finite `min` publishes
`"minimum": null`; CRLF frontmatter is shipped whole.

## Unbuilt from RFC-0002

Early completion, the playbook epilogue, resume folding, asks emission (the contract landed in
`crucible-contract/src/ask.rs`, the engine side did not), `C-PLAYBOOK-COMPOSITE`. Also
WI-003 (`isolated` becomes `workspace`) and the rest of WI-005 (per-task deadlines, terminating
an in-flight attempt, rejecting `iterations > 1`).

## Decisions made, do not relitigate

- **Ceilings are the launcher's.** A source may not declare one. Cost accounting is the
  orchestrator's: a task never reports what it spent, so `ShellRunner`'s `cost_usd: 0.0` is
  correct rather than a gap.
- **`max_fanout` is required with `over`**, capped by the engine at 256.
- **Only passing instances feed a join.**
- **External text is refused outside a prompt.** Nothing else can mark provenance.
- **The engine compiles the graph, not the controller.**
- **A skill is a naming and reuse construct** and must not widen what a task may reach. Whether
  it may *narrow* tool scope is a separate clause nobody has written.

## The controller

`~/git/agentic-epp-autoresearch` (the name has outlived its scope). Rust, in-cluster, Postgres,
bors-style queue; it launches work pods and injects their env. Consumes this repo as an exact git
pin in `core-pin.toml`, currently `7c2c1a5`, CI-asserted.

- **`InputKind` is the extension point** (`GitHub`/`Scenario`/`Jira`/`Unknown`). An ask is a
  fifth variant.
- **The queue already implements C-ASKS' orchestrator half.** Coalescing is dedupe, backoff is
  rate limiting, park-after-N is the blacklist.
- **`Unknown { tag }` degrades to inert**, so the engine can emit ask rows against the running
  controller before it understands them. Land in core, bump the pin, teach the controller.
- Tension: the queue's frozen rule is that a dequeued key carries no payload; C-ASKS says an ask
  carries its parameter values. Precedent is `Jira`, which fetches title and body once at adopt
  time, so params are stored on adopt and the key re-reads them.

## Non-obvious things that cost time

- **`starlark` turns on `serde_json/arbitrary_precision` for the whole binary.** Under that flag
  internally-tagged enums and `#[serde(flatten)]` structs cannot decode floats. Use
  `crucible_contract::json::from_str`.
- **`crucible` is bin-only.** `pub` does not exempt an item from `dead_code` and the guard is
  `-D warnings`, so nothing lands without a production caller. Three convenience wrappers are
  `#[cfg(test)]` for this.
- **A prompt starting with `-` was read as a flag** by the agent CLI. A SKILL.md opens with
  `---`. Fixed with an end-of-options marker; it was latent for the scored lane too.
- **`over` requires `isolated`, which forbids a `session`.** The kwarg-coverage test can no
  longer put every kwarg in one call; coverage is the union across a function's templates.
- **`govctl` refuses to delete a referenced clause**, and `clause new` appends to the section
  list while a sed rewrites the existing entries. Check for duplicates after a rename.
- **`otel::forwarding_mirrors_reparented_traces_and_holds_back_metrics` is flaky.** Three
  sightings, full-suite only, passes in isolation. Not ours.

## How the review was run

A `Workflow` with five hunters, one per mechanism, each given a specific case list rather than
"review this area"; each pipelined into its own skeptic at high effort defaulting to refuted; one
synthesizer. 11 agents, 1.09M tokens, 24 minutes. The schema forced a `confirmed` field
separating "ran it, got X" from "read it and reasoned", which is the field that made the report
worth acting on. Two free-running review agents before it produced nothing in 35 and 60 minutes.
