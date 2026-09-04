# Architecture decision records

The ADRs capture the load-bearing design calls behind crucible: why the loop is
shaped the way it is, what was traded off, and what the alternatives were. They are
ordered, append-only, and meant to be read when you want the *why* rather than the *how*.

Expand this section in the sidebar to browse the full list.

| ADR | Decision | Status |
| --- | --- | --- |
| [0001](./0001-adaptive-harness.md) | Adaptive harness | Partially implemented |
| [0002](./0002-mediated-provisioning-mcp.md) | Mediated provisioning (MCP) | Implemented |
| [0003](./0003-async-approval-waits.md) | Async approval waits | Implemented |
| [0004](./0004-core-loop-state-model.md) | Core-loop state model | Implemented |
| [0005](./0005-engine-side-builds.md) | Engine-side builds (MCP) | Implemented |
| [0006](./0006-profiling-over-mcp.md) | Profiler support over MCP | Implemented |
| [0007](./0007-isolation-preflight.md) | Isolation pre-flight (the metric that misframed #1109) | Accepted (process) |
| [0008](./0008-domains-as-immutable-composes.md) | Domains as immutable composes (the rpm-ostree model) | Partially implemented |
| [0009](./0009-composite-domains.md) | Composite domains (combined multi-component autoresearch) | Implemented |
| [0010](./0010-candidate-portfolios-and-search.md) | Candidate portfolios — explore/exploit search | Superseded by 0028 |
| [0012](./0012-rendered-deployments.md) | Crucible-rendered deployments, generating the loop/broker/deployment manifests | Implemented |
| [0014](./0014-scoping-pipeline.md) | Scoping as a governed pipeline, `crucible scope <issue>` | Partially implemented |
| [0017](./0017-turn-result-contract.md) | Turn result contract, structured state back from turn pods | Implemented |
| [0018](./0018-declarative-image-builds.md) | Declarative image builds, build backends + the `building` state | Implemented |
| [0019](./0019-openshell-kubernetes-driver.md) | The loop pod stops being a container host, OpenShell's Kubernetes driver | Partially implemented |
| [0020](./0020-candidate-build-modes.md) | Candidate build modes, how a proposal becomes a measured artifact | Proposed |
| [0022](./0022-measure-task-dags.md) | Measure task DAGs, the engine walks the ladder | Proposed |
| [0023](./0023-recovery-classification.md) | Recovery classification for `--resume` | Implemented |
| [0024](./0024-admission-ledger.md) | Admission ledger for external inputs | Implemented |
| [0025](./0025-durable-tool-steps.md) | Durable tool steps for broker builds and measures | Implemented |
| [0026](./0026-no-judge-task-lane.md) | The no-judge task lane | Implemented |
| [0027](./0027-measurement-sessions.md) | Measurement sessions, one warm engine and many observations | Proposed |
| [0028](./0028-retire-the-wide-tournament.md) | Retire the wide tournament | Implemented |
| [0029](./0029-approval-gates.md) | Plan-authored approval gates on an event-sourced loop | Implemented |
| [0030](./0030-container-isolation-for-tasks.md) | Container isolation for deterministic tasks | Proposed |
