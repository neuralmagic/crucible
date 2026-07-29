# ADR 0005: Engine-side EPP builds, brokered to the agent over MCP

**Status:** Accepted; implemented (builds run buildah-unified on the loop pod — kaniko was dropped; open questions 1–2 were resolved by ADR-0008, see the annotations below)
**Date:** 2026-06-26
**Related:** [ADR-0001](./0001-adaptive-harness.md) (freeze the judge), [ADR-0002](./0002-mediated-provisioning-mcp.md)
(the agent asks, the host holds the keys), the `World::apply` trait, the `epp-mcp` broker,
`Containerfile.epp-sandbox`, the #1109 TTFT domain (`crucible.ttft.toml`).

## Context

#1109 is the **first domain that needs in-loop EPP *code* rebuilds**. The perf domain dodged this — it
is config-tuning (`apply` scorer weights onto the live EPP, no rebuild). When the #1109 loop ran, the
sandboxed agent could not make progress and doom-looped, because:

1. **It can't build.** `build-image` shells out to an out-of-sandbox remote-build script, which isn't in the
   sandbox; and a kaniko build needs build-namespace `kubectl`, a push credential for the candidate
   registry, and egress that the deny-by-default sandbox does not have. The agent correctly concluded it
   had no container builder available and gave up.
2. **It can't see the measurement.** `epp-sandbox:spike-1` predates `bench --ttft`, so the agent's
   `bench` is the old prefix-cache bench — it can't reproduce the judge's TTFT metric, so it
   reverse-engineers the binary (`strace`/`strings`) and spirals.

Putting build creds in the sandbox (the "provision the sandbox" option) enlarges the attack surface
and fights the `World` trait, which already says build+deploy is `apply`'s job (engine-side).

## Decision

**Move build+deploy off the agent to the loop pod (engine-side), and expose it to the agent as broker
(MCP) tools** — the ADR-0002 pattern, the agent *asks*, the loop pod *holds the keys*.

Why MCP rather than only `[world].apply_cmd`: a code loop needs **compile feedback within the turn**.
A purely post-turn `apply` build means a typo fails the whole iteration with no feedback. The agent
must trigger a build, get errors, and fix them before its turn ends. So:

- **Primary path — broker tools.** Add `build_epp` (and `deploy_candidate`, or a combined
  `build_and_deploy`) to `epp-mcp`, alongside `request_trace`. The agent calls them over the existing
  bridge (`host.containers.internal:8849`); the loop pod runs the container build (buildah today; the
  original design said kaniko) with its own push credential for the
  candidate registry, and returns a Resolution: `{ok, image_ref}` | `{compile_error, log}`. The
  agent iterates on errors, then deploys.
- **Backstop — `[world].apply_cmd`.** Keep an engine-side `apply_cmd` that ensures the latest built
  candidate is the deployed one before the judge measures (idempotent; a no-op if `deploy_candidate`
  already ran). The manifest already supports `[world].apply_cmd` and `CommandWorld::apply` runs it.
- **The agent's sandbox loses build creds entirely.** `build-epp`/`deploy-candidate` direct tools are
  dropped; the agent only edits Go + calls the broker.

## Components to build

1. **Loop-pod build capability** (in the crucible loop image): a self-contained buildah build + push
   (no dependency on an out-of-sandbox remote-build script) and a **scoped push robot credential for the
   candidate registry** (mounted secret).
2. **`epp-mcp` build tools** `build_epp` / `deploy_candidate`, mirroring `request_trace`'s
   admit/return shape. They run the loop-pod build and surface the compile log on failure.
3. **Workspace sync (the crux).** The loop pod must build the agent's *current* edits, which live in
   the sandbox mid-turn (the driver copies the workdir in/out per turn, it is not shared). Proposed:
   the agent commits its branch and pushes it to the loop pod's workspace as a git remote over the
   relay; the broker builds from that ref. (Reuses git-memory; no new driver plumbing. Alternative: a
   driver "mid-turn checkpoint" that syncs `/sandbox/epp` → loop workspace on the build call.)
4. **Rebuild `epp-sandbox`** with current epp-tools (so the agent's read-only `bench --ttft` matches
   the judge) + bake `bench.rs` source (or a TTFT measurement skill) so the agent understands the
   metric instead of reverse-engineering it. Drop the direct build/deploy tools.
5. **Manifest wiring:** broker build tools enabled in `[agent.broker]`; `[world].apply_cmd` as the
   ensure-deployed backstop; the candidate registry + egress allowlisted.

## Open questions

1. **Build latency.** *Resolved by ADR-0008:* the immutable compose bakes the toolchain and a warm
   `GOMODCACHE`/`GOCACHE` into the base, so the per-iteration build is an incremental build against a hot
   cache, not a cold from-scratch image build.
2. **Workspace sync mechanism.** *Resolved by ADR-0008:* the agent's branch push *is* the CoW overlay;
   the broker builds `base@SHA + overlay`.
3. **Cred scoping** — a push robot token limited to the candidate registry, not a broad registry cred.

## Consequences

- **Positive:** build creds never enter the sandbox (ADR-0001/0002 aligned); the agent gets real
  compile feedback; the loop pod is the one trusted place for push creds; `apply_cmd` guarantees the
  measured candidate is the built one; code-change domains become first-class, not just config tuning.
- **Negative / cost:** real infra (push cred, RBAC, a builder in the loop image — kaniko as
  designed, buildah as shipped — the workspace-sync mechanism, a sandbox rebuild) and
  per-iteration build latency.
