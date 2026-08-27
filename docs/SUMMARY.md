# Summary

[Introduction](./introduction.md)

# Concepts

- [How it works](./how-it-works.md)
- [What crucible is](./crucible.md)

# Getting started

- [Zero to a running loop](./getting-started.md)
- [Tasks: general-purpose orchestration](./task-lane.md)
- [Images for a new domain](./domain-images.md)

# Reference

- [Implementation contract](./crucible-contract.md)
- [Work graphs](./work-graphs.md)
- [Workflow DSL reference](./dsl-reference.md)
- [Hand-rolled codegen pipelines](./hand-rolled-pipelines.md)
- [The codex harness](./codex-harness.md)
- [The OpenShell fork](./openshell-fork.md)
- [JIRA tools (mediated)](./jira-proxy.md)

# Specifications

- [RFC-0001: Implementation contract](./rfc/RFC-0001.md)
- [RFC-0002: Playbook workflows](./rfc/RFC-0002.md)
- [RFC-0004: Linked controller/engine boundary](./rfc/RFC-0004.md)

# Architecture decisions

- [Architecture decision records](./adr/index.md)
  - [ADR 0001: Adaptive harness](./adr/0001-adaptive-harness.md)
  - [ADR 0002: Mediated provisioning (MCP)](./adr/0002-mediated-provisioning-mcp.md)
  - [ADR 0003: Async approval waits](./adr/0003-async-approval-waits.md)
  - [ADR 0004: Core-loop state model](./adr/0004-core-loop-state-model.md)
  - [ADR 0005: Engine-side builds (MCP)](./adr/0005-engine-side-builds.md)
  - [ADR 0006: Profiler support over MCP](./adr/0006-profiling-over-mcp.md)
  - [ADR 0007: Isolation pre-flight (the metric that misframed #1109)](./adr/0007-isolation-preflight.md)
  - [ADR 0008: Domains as immutable composes (the rpm-ostree model)](./adr/0008-domains-as-immutable-composes.md)
  - [ADR 0009: Composite domains (combined multi-component autoresearch)](./adr/0009-composite-domains.md)
  - [ADR 0010: Candidate portfolios — explore/exploit search](./adr/0010-candidate-portfolios-and-search.md)
  - [ADR 0012: Crucible-rendered deployments — generate the loop/broker manifests](./adr/0012-rendered-deployments.md)
  - [ADR 0014: Scoping as a governed pipeline — `crucible scope <issue>`](./adr/0014-scoping-pipeline.md)
  - [ADR 0017: Turn result contract — structured state back from turn pods](./adr/0017-turn-result-contract.md)
  - [ADR 0018: Declarative image builds — build backends + the `building` state](./adr/0018-declarative-image-builds.md)
  - [ADR 0019: The loop pod stops being a container host — OpenShell's Kubernetes driver](./adr/0019-openshell-kubernetes-driver.md)
  - [ADR 0020: Candidate build modes — how a proposal becomes a measured artifact](./adr/0020-candidate-build-modes.md)
  - [ADR 0022: Measure task DAGs — the engine walks the ladder](./adr/0022-measure-task-dags.md)
  - [ADR 0023: Recovery classification for `--resume`](./adr/0023-recovery-classification.md)
  - [ADR 0024: Admission ledger for external inputs](./adr/0024-admission-ledger.md)
  - [ADR 0025: Durable tool steps for broker builds and measures](./adr/0025-durable-tool-steps.md)
  - [ADR 0026: The no-judge task lane](./adr/0026-no-judge-task-lane.md)
