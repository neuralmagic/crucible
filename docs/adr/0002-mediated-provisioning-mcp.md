# ADR 0002: Mediated provisioning — the agent asks, a server-side MCP admits

**Status:** Accepted; implemented (the broker now lives in the extracted `crucible-broker` crate; the draft-PR approval path was proven end-to-end against a fork)
**Date:** 2026-06-25
**Context owner:** agentic-epp-autoresearch
**Related:** [ADR-0001](./0001-adaptive-harness.md) (freeze the judge), the `request-trace`
gap, the control bridge (`crucible/src/control.rs`), the `escalate` hatch (`tools/escalate.nu`
+ `crucible/src/escalation.rs`), the `session` MCP server.

## Context

ADR-0001 drew the trust wall: the agent may change its **solution**, never its **evaluation**.
But a blocked agent has a legitimate need the loop can't yet serve cleanly — it discovers it
must measure a regime it has no calibrated trace for (a new model, a higher request
concurrency, a novel workload), and the only honest GPU-free answer is *capture/calibrate a
new trace*. Today that means either:

- the agent reaches the cluster directly (it holds a rig `edit` kubeconfig) and could, in
  principle, submit a GPU capture Job or dispatch CI itself, or
- it writes `ESCALATION.json` and the whole run halts for a human.

The first is unacceptable: **we will not give the sandboxed agent direct authority to spend
GPU, submit a Kueue Workload, or dispatch a GitHub Actions workflow.** Those are privileged,
shared-blast-radius, money-spending operations. The second is too coarse for a need that is
often *grantable* (the trace just needs capturing) — it ends the run and waits on a human even
when policy could have admitted it.

The missing piece is a **mediation layer**: a place that holds the privilege and the admission
logic, exposes only a narrow "ask" to the agent, and decides whether/how to grant. An MCP
server already sits on the loop pod (wrapping the read-only `session` surface). This ADR makes
that server the **provisioning admission broker** too.

## Decision

**The agent asks; a server-side MCP on the loop pod admits. Provisioning privilege and the
admission logic live in the MCP, never in the sandbox.** Concretely:

1. **Privilege separation by identity.**
   - *Solution-space* cluster ops the agent already does (`apply` EPP config, `bench`,
     `inspect-rig`) stay direct, under the agent's scoped rig credential (ADR-0001's solution
     column). Unchanged.
   - *Provisioning* ops (submit a GPU capture Job / Kueue Workload, dispatch a GHA workflow,
     anything that spends GPU or mutates the evaluation) are **removed from the agent's reach**.
     The agent's credential must not carry these rights (RBAC: the agent SA can `edit` the rig
     namespace but cannot create Workloads/Jobs in the capture namespace; no forge token in the
     sandbox). Only the MCP's own identity can, and it gates every such call.

2. **The MCP is the only privileged endpoint the sandbox can reach.** OpenShell egress is
   deny-by-default; we allowlist exactly the loop pod's MCP endpoint and nothing else
   provisioning-capable. The agent speaks a small typed tool surface (below); it never sees a
   kubeconfig for the capture namespace, a Kueue API, or a forge token. Same mediated-capability
   pattern OpenShell already uses to serve the Vertex token through the metadata emulator without
   ever handing claude the token.

3. **Requests are classified by whether they touch the judge** (this is the integrity-critical
   axis, not the transport):
   - **In-scope** (a trace *missing* for the already-frozen regime, a pure cache fill): the MCP
     resolves it and returns `MOCK_*` knobs synchronously; the turn continues. No judge change,
     no re-baseline.
   - **Judge-changing** (a new concurrency / workload / regime the frozen baseline was never
     measured at): the MCP **must not** silently continue — that would make every later "keep"
     incomparable (the ADR-0001 reward-hack surface). It triggers a governed **re-scope**:
     re-derive the baseline at the new regime, bump the harness fingerprint, run the gate
     self-test, and only then start a new comparable segment. Here the coarseness is protective:
     the agent never measures its own change against a goalpost it just moved inside one turn.

4. **Admission gating is server-side and opportunistic:**
   on a cache MISS the MCP runs the `gpu-check` headroom logic (allocatable − Σrequested, with a
   reserve + a concurrent-capture cap); if there is headroom it auto-submits the capture under
   Kueue; else it defers/queues and the loop continues on fallback. The agent sees only a
   `trace_id` + a pollable handle.

5. **Human-in-the-loop is pluggable, and headless-first.** When a grant genuinely needs a human
   (beyond budget, a judge-changing re-scope under a stricter policy, a starving queue), the MCP
   raises the ask through one of two interchangeable approval backends behind a single request
   object — and keeps the request **async** (the loop records the ask and continues / parks; the
   human approves a batch out-of-band):
   - **Forge-native (the headless default) — a draft PR.** The MCP (which holds a repo-scoped
     forge token server-side — the agent does not) pushes an `agentic/<run_id>`-prefixed branch
     and opens a **draft PR** describing the request: params, `trace_id`, GPU estimate, the
     rendered capture manifest. Approval is a maintainer **slash-command** comment
     (`/approve-capture <trace_id>`) or marking the PR ready/merging — gated by CODEOWNERS +
     branch protection, audited by the forge itself. The MCP polls for the signal, then proceeds.
     A draft PR (over a bare issue) gives diff-review of the manifest, CODEOWNERS, and doubles as
     the **state-persistence** record (the run's branch outlives the pod). It builds on the
     existing `publish.rs` branch push.
   - **Control-channel (attended).** The MCP surfaces the same request as an event over the
     control bridge we built; an operator watching `crucible view --pod` approves with a keystroke
     (the steer/stop path). Immediate, for attended runs.
   The kept result records which path approved it (provenance, like the steer audit log), so a
   forge-approved capture is as traceable as an operator-approved one.

6. **Degraded mode is escalate-halt, never auto-admit.** When a judge-changing request arrives in
   a pure-headless run with no approval surface reachable (no forge token, no attended operator),
   the MCP does **not** silently auto-admit a goalpost move. It falls back to the `escalate` hatch:
   record the ask, restore the world, halt for a human. Unattended autonomy never extends to
   changing the evaluation without a recorded human (or policy) yes. In-scope cache fills still
   return synchronously in degraded mode — only judge-changing grants halt.

## What this is NOT

- **Not** giving the agent a Kueue client, a forge token, or GPU-namespace RBAC. The agent's
  blast radius stays exactly the rig solution surface.
- **Not** a replacement for the frozen judge. A judge-changing grant *re-scopes* (new baseline +
  fingerprint); it never edits the gate in place.
- **Not** retiring escalation. `ESCALATION.json` (`tools/escalate.nu` → the engine hook) remains
  the honest valve for the *unservable* cases no tool can grant — a dead signal (`tokenload`=0),
  a missing rig capability, "infeasible" — and the degraded path when the MCP is unreachable
  (offline / pure-headless-no-forge). Mediated provisioning handles the *grantable* asks;
  escalation handles the *un-grantable* ones.

## MCP tool surface (initial)

One server, narrow tools, all privileged work behind them:

| Tool | Returns | Privilege held server-side |
| --- | --- | --- |
| `request_trace{model, concurrency, prefixes, max_tokens, long_tokens, long_frac}` | `hit{knobs, trace_id}` \| `needs_capture{trace_id, plan, est_gpus}` \| `submitted{handle}` \| `pending_approval{ref}` | cache resolve (local `traces/` + S3), `gpu-check` admission, Kueue submit |
| `poll_trace{handle}` | `queued` \| `running` \| `ready{knobs}` \| `denied{reason}` | watch the capture Job / approval signal |
| `request_regime{...}` | `rescope_started{fingerprint}` \| `pending_approval{ref}` | re-baseline + fingerprint bump (judge-changing) |
| `session_status` (read-only) | the `session` JSON snapshot | none (wraps the existing read surface) |

`request_trace` content-hashes its params into a `trace_id` shared with the EPP CI cache,
so a trace captured by the loop warms CI and vice-versa.

## Architecture sketch

```
  sandbox (egress: ONLY the MCP endpoint allowlisted)
    claude ──MCP──> loop-pod MCP server  (trusted identity: rig + capture RBAC + forge token)
                          │
                          ├─ resolve cache (traces/ + S3 by trace_id)  ── HIT ─> knobs (sync)
                          ├─ admission: gpu-check headroom + caps  ── ok ─> Kueue submit
                          ├─ judge-changing? ─> re-scope (baseline + fingerprint, ADR-0001)
                          └─ needs human? ─> approval backend:
                                 ├─ forge: open issue/PR, poll for /approve-capture (headless)
                                 └─ control bridge: surface event, operator keystroke (attended)
```

## Consequences

- **Positive:** the agent literally cannot provision — no GPU spend, no Kueue, no CI dispatch
  from the sandbox — yet it is no longer dead-ended on a grantable need. The choke point is one
  auditable server-side process.
- **Positive:** admission + approval are policy, not code scattered across tools; headless runs
  get a real (forge-native, async, CODEOWNERS-gated) approval path, attended runs keep the
  keystroke path, both record provenance.
- **Positive:** reuses what exists — the control bridge, `gpu-check`, the `request-trace` cache
  design, the `autoresearch/<run_id>` branch push, the `session` MCP.
- **Negative / cost:** a real MCP server on the loop pod (lifecycle, the loop pod's identity now
  also carries capture/forge rights — concentrate and guard it), MCP plumbing into the sandbox
  claude config, and an egress allowlist entry for the MCP endpoint. RBAC must be tightened so
  the *agent* identity loses any latent provisioning rights.
- **Negative / cost:** the judge-changing re-scope (auto re-baseline + fingerprint + gate
  self-test) is real engine work, not just a knob hand-back.

## Testing & rollout

The approval + state-persistence path is validated against a throwaway fork before targeting any
real repo; the forge token is held server-side by the MCP, never relayed into the sandbox.

- The loop's `[repo].url` points at the fork; the MCP opens its draft PRs there.
- Branches are `agentic/<run_id>`-prefixed, so the agent's pushes/approval PRs are self-labelling
  and the fork's history *is* the durable run record (state persistence survives pod teardown).
- The rig forwards a **repo-scoped fine-grained PAT** (scoped to the fork only) into the loop pod —
  mounted server-side and held by the MCP exactly like the kubeconfig / AWS token, **never**
  relayed into the sandbox. The agent gets no forge credential.

This validates the full headless approval loop (open draft PR → `/approve-capture` → proceed) and
state persistence before anything touches a real repo.

## Design notes

- **Re-scope unit:** a judge-changing grant starts a **new fingerprinted segment** within the run
  (not a new `run_id`), so one session log tells the whole story across the boundary.
- **Forge surface:** a **draft PR** (not a bare issue) — diff-review + CODEOWNERS + it doubles as
  the state record.
- **Degraded mode:** **escalate-halt** (never auto-admit a judge change unattended) — decision 6.
- **MCP transport** is HTTP/SSE at the loop pod IP:port, allowlisted in the OpenShell egress policy
  as `full` for just that host:port, with the sandbox claude MCP config seeded (env / `.mcp.json`).
- **Agent RBAC** keeps `edit` on the rig namespace only, with no path to Workload/Job creation in a
  capture namespace; `gpu-check`'s admission logic lives in a shared lib the MCP calls.

## Principle, restated

The agent reaches the cluster only through doors the host holds the keys to. Solution-space
doors are already open to it; every provisioning door is locked, and the MCP is the only one who
can open one — after admission, and (when it must) after a human says yes through a channel that
fits a headless run.
