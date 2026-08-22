# Playbook lane: handoff

Date: 2026-08-22. Branch `playbook-dsl`, rebased onto `main` (`7c2c1a5`), rebase
verified: 41 files, identical logical diff. Nothing committed since the rebase;
the governance edits below are in the working tree.

## What a playbook is

A workflow that runs its graph **once** and produces **no score**. The lane for
work whose value is the work itself: skill pipelines, scheduled chores,
discovery sweeps. No propose/apply/measure/decide, no frozen judge.

Fan-out over items discovered at run time is **one run per item**, never graph
expansion. The graph stays static and renderable before any spend. A run emits
"asks"; a receiving orchestrator admits them.

## State

The design is finalized. `govctl check` passes clean.

| Artifact | Status |
| --- | --- |
| RFC-0001 "Crucible implementation contract" | **0.2.0**, normative, phase `spec` (reopened by the bump) |
| RFC-0002 "Playbook workflows" | **normative**, 0.1.0, 11 clauses |
| ADR-0021, ADR-0009 | reconciled, both now ref RFC-0002 |
| DSL implementation | committed on the branch, full suite green |
| Litmus pack `examples/playbook/` | runs free on the command backend |
| WI-2026-08-22-001 / -002 | queued, the DSL compiler gaps |

## What landed in RFC-0001 0.2.0

C-WORKFLOW, rewritten:

- grammar admits dictionaries, conditionals, comprehensions, iteration,
  user-defined functions, `load()`, and starlark's pure standard library; any
  builtin reaching the filesystem, a process, the network, the clock, or
  randomness must be absent
- compilation must be bounded in depth and size and must not abort the process
  on an author-supplied source; bounds are checked before the work they bound
- `load()` confinement covers links of any kind; module count and total size are
  counted on admission, not on return
- `type = "playbook"` admitted, delegating to RFC-0002
- a task's declared files continue through the graph alongside its JSON output,
  isolation notwithstanding
- a required task may not depend on an advisory one through all-join edges
  (`join = "passed"` is the exemption)
- `param` joins the enumerated constructors
- a named-argument diagnostic must locate the argument; suggestions draw on the
  source's own bindings

C-WIRE: shutdown `outcome` gains `complete` (a task said there was no work left;
the unscored counterpart of `solved`).

## What changed in RFC-0002 before finalizing

- C-SCOPE dropped the contingency paragraph; RFC-0001 0.2.0 admits both
  obligations it was waiting on
- new **C-CASCADE-SURFACE** (normative): what a playbook author writes, and the
  explicit refusal to condition any obligation on a measure command, judge,
  baseline, apply/snapshot/restore, or result task
- new **C-CASCADE-SHAPE** (informative): the graph expresses ordering and
  gating, never iteration; a repair loop is one task, a discovered item set is
  asks
- C-CASCADE-LANE now names `complete` instead of promising a value C-WIRE lacked
- C-TASK-FILES confinement extended to hard links

## Deliberately not done

- **`skill()` is not in C-WORKFLOW's constructor list.** The amendment draft
  listed it, but no clause defines what a skill task is, and the enumerated list
  is a permission list. `param` went in because C-CASCADE-PARAMS requires it.
  Decide whether `skill` gets a clause or stays out.
- **RFC-0002's implementation is not decomposed into work items.** Only the two
  DSL-compiler WIs are filed. The lane itself (admit `type = "playbook"`, params
  and `param()`, `emits_files`, epilogue, early completion, verdict and exit
  code, resume folding, ceilings, ask emission) still needs scoping by `gov`.
- **`examples/playbook` still says `type = "custom"`** and its manifest carries no
  `[workflow]` block. C-CASCADE-SURFACE's litmus obligation is not yet met.
- **`govctl render` writes `docs/rfc/*.md`**, which this repo has never tracked
  (`docs/` is the mdbook source with its own hand-maintained naming). The
  rendered output was removed. Decide whether to adopt it into `docs/SUMMARY.md`
  or point `docs_output` somewhere else.

## Non-obvious things that cost time

- **`starlark` turns on `serde_json/arbitrary_precision` for the whole binary.**
  Cargo features unify across the graph, and under that flag serde's
  internally-tagged enums and `#[serde(flatten)]` structs cannot decode floats
  (`invalid type: map, expected f64`). `SessionEvent` is tagged and carries
  floats; `Envelope` flattens. Unfixed, the swap silently breaks `--resume`, the
  viewer, S3 records and controller ingest with no compile error. Fixed via
  `crucible_contract::json::from_str` (a `Value` round-trip).
- **`load()` needs no separate identity hashing.** C-WORKFLOW already requires
  compiling to manifest IR before freeze with the generated TOML as runtime
  authority, and C-WIRE hashes the frozen manifest text. Loaded content bakes in.
- The DSL is Rust-2018 module style: `starlark.rs` and `starlark/` coexist
  deliberately, so `mod tests` never moved and "zero test edits" stays checkable
  with `git diff --stat`.
- The earlier handoff pointed at "ADR-0026" for the composite-requires-judge
  rationale. There is no ADR-0026. It is **ADR-0021**, and the conflict was
  sharper than a missing link: its decision text refused `[workflow]` in a
  judgeless manifest and required `[judge]` of composites, both of which
  RFC-0002 reopens. ADR-0009 carries the combined-gate rationale.

## Decisions already made, do not relitigate

- **Budget lives with the controller.** A playbook source may not declare a
  ceiling at all; the engine refuses to dispatch without one supplied by the
  launcher.
- **Composites are permitted without `[judge]`** for playbooks.
- **Failure semantics are the author's** via `required`, and a required task may
  not depend through all-join edges on an advisory one (now universal, in
  `plan/ir.rs` and in C-WORKFLOW).
- **GPU is reached by delegation, not placement.** `needs` names a brokered
  capability; a playbook runs on one substrate for its whole life.
- **Gate label stays `"task"`.** No third label; consumers needing the graph read
  the admitted-plan event.
- **Dedupe/blacklist state lives in the controller DB**, keyed by the ask key.

## Out of scope, tracked

The broker needs a `file_issue` tool (publish opens PRs, the broker has only
`draft_pr`). The controller side (cron rows, deterministic admission filter,
blacklist predicate) wants its own RFC; RFC-0002 deliberately stops at C-ASKS.

## Reference

- Domain: `vllm-project/speculators`. Autopilot skills on the fork
  `orestis-z/speculators@4da14f6`. The designed pack (`discover.star`,
  `paper.star`, `papers.star`, `lib/speculators.star`) is a design artifact, not
  runnable, and is the source of the `param()`/`skill()` surface.
- Old scratchpad (session-local, will not last):
  `/private/tmp/claude-501/-Users-weaton-git-crucible/703e6b48-c6ae-40b0-9d53-5f6819d93515/scratchpad`
