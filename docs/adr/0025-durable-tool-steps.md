# ADR 0025: Durable tool steps for broker builds and measures

**Status:** Implemented
**Date:** 2026-08-04
**Related:** [ADR-0005](./0005-engine-side-builds.md) (`build_epp`), [ADR-0022](./0022-measure-task-dags.md)
(the run-13 scar, and the cache paragraph this amends), [ADR-0024](./0024-admission-ledger.md)
(the `forge::ndjson` mechanics both ledgers share)

## Context

Every completed piece of expensive broker work lived in memory and died with the pod:

- The codegen memo is a `Mutex<HashMap>` and the `built` provenance set another `Mutex<HashSet>`.
  A restarted broker rebuilt an image whose digest was already in the registry, and then `is_built`
  refused the digest it had been handed ("not produced by codegen_build in this broker's lifetime"),
  forcing a rebuild before any measure could run.
- `build_epp` had no memo at all: every call re-synced the sandbox and re-ran buildah, even for a
  byte-identical tree.
- Kueue measures were memoized in that same map, so a finished 90-minute GPU oracle was repaid in
  full after a restart. ADR-0022 records the scar: run 13 proved steps 1–5 on a cached digest, died
  at step 6, and the rerun repaid all five.

## Decision

A **durable step**: a unit of work whose completed result is an immutable value, recorded in an
append-only NDJSON ledger (`<storage>/steps.jsonl`) keyed by the content of its inputs. The
semantics are flue's `step.do`, kept verbatim:

- **At-least-once-executed.** A crash between the work finishing and the record landing re-executes
  it; two racing callers may both run the same step. Bodies must be safe to repeat (builds push
  immutable tags, measures resubmit jobs).
- **Exactly-once-recorded.** One fsync'd line lands before the caller consumes the value; a later
  call with the same identity replays it without running the body.
- **Only settled facts are recorded.** `Ok` is recorded, `Err` (transport) is not. A deterministic
  measured failure IS a fact of the identity and is recorded — a compile error replays instead of
  rebuilding a known-broken tree. A possibly-flaky failure is not: `JobFailed` and `TimedOut` stay
  uncached, exactly as the in-memory memo already had it.
- **Eligibility: values, never state.** A step must never record a claim about mutable external
  state. This is why `deploy_candidate` and the composite apply are NOT steps: "X is deployed" is
  cluster state, not a value, and skipping a re-deploy after a pod death would assert something
  nobody checked. Deploys always re-execute.

Identity is `(scope, step)` where `step` embeds the full content key:

| Step | Name |
| --- | --- |
| `build_epp` | `build-epp:<sandbox git tree hash>:<build-config fingerprint>` |
| codegen build | the existing `build` \| `<tree>` \| `<mode>` \| `<cfg hash>` |
| benchmark / lm_eval / profile | the existing `<kind>` \| `<digest>` \| `<sorted kwargs>` |

`scope` is the constant `"broker"`. Deliberately not the turn token: the driver writes a fresh one
per turn *sandbox*, so scoping by it would throw away exactly the replay across pod death this
exists for. Content keys make the wide scope safe — a changed tree, config, digest, or kwarg is a
different step, and the registry holds the artifact either way.

The ledger is the durable tier *under* the existing memos, not a replacement: lookup order is memo,
then ledger, then do the work. A ledger hit rehydrates the memo, and a recorded build also restores
the digest's provenance, which is what unblocks measure-after-restart. Budget accounting only runs
on real executions, so a replay costs zero GPU-minutes.

**The ledger is a cache, never authoritative.** An unreadable file is quarantined, a torn tail is
skipped, a failed append is logged and the real value returned. Every degradation lands on
"execute the work", never on a failed call.

### Amendment to ADR-0022

ADR-0022's *Cache and regrade* section proposed folding `session.jsonl` into a `(digest, task)`
map as the durable record. For the broker half — build and measure replay, and the `is_built`
provenance gate it names — this ledger supersedes that: `session.jsonl` is the run's report,
single-writer with a "Shutdown is always last" invariant, and the broker is a separate process that
cannot append to it. The engine-side per-task fold for the measure DAG remains open and unbuilt.

## Boundaries

Engine-side replay was cut from this change: a plan task's input fingerprint does not capture
workspace state, so whether a replayed evaluate result is valid depends on which tree a resume
restored. That question belongs to the resume/retry work, and this ledger deliberately does not
touch `crucible/src/plan`, `loop_graph.rs`, or `loop_driver.rs`. The broker keys are content
digests of immutable artifacts and have no such dependency.

## Consequences

Replay is only as durable as the directory under the ledger. The rendered loop pod backs
`FORGE_STORAGE_ROOT` with an `emptyDir` (per-run fresh buildah storage), so today replay covers a
broker restart within a live pod, not pod death. Extending it across pod death is one env var:
point `BROKER_STEP_LEDGER_DIR` at the run-state PVC (`<domain-dir>/state`, where `session.jsonl`
and `admissions.jsonl` already live). The renderer does not set it yet; that is the follow-up that
closes the run-13 scar end to end.

A replayed reply carries `cached: true`, including `build_epp`'s (a new optional field, absent when
false, so an ordinary reply's JSON is unchanged). A replayed build-log handle may dangle after a
pod death if the log store was not on a durable volume; `fetch_log` already answers readably for a
missing handle.

A pod death *mid-job* orphans the Kueue Job (it is garbage-collected with the pod), and the retry
resubmits: at-least-once, accepted for v1. Adopting a still-running Job through a deterministic
name is deferred — the ledger only skips *completed* steps. Also deferred: re-verifying a
ledger-recorded digest against the registry before trusting it in `is_built`, and compaction (the
file is append-only, at a couple of hundred bytes per record).

A cached compile error could in principle mask a fix that lives outside both the tree and the build
config; the config fingerprint covers the base image and the install command, so the exposure is
narrow.
