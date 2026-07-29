# ADR 0017: Turn result contract — structured state back from turn pods

**Status:** Proposed (supersedes the 2026-07-06 stub; amends one ADR-0016 ruling, see §"Amendment";
refined 2026-07-07: harness crate + OTLP telemetry restoration + MLflow export workstream)
**Date:** 2026-07-06
**Related:** ADR-0015 (the WorkPod primitive whose results this carries),
ADR-0016 (the ledger the results land in, and the truth
model this must not break), the private control plane's workpod dispatcher (today's log-marker scrapers),
a run of internal incident PRs (the fragility class that motivated this),
[opendatahub-io/agentic-ci](https://github.com/opendatahub-io/agentic-ci) (the vendored harness
whose OTLP collector and library role this restores).
External prior art: Tekton results + TEP-0086/TEP-0127, Argo Workflows output offloading,
GitHub Actions runner auth, Buildkite job tokens, Temporal task tokens (condensed in §"Prior
art"; full cited notes in the appendix).

## Context

Turn pods (grounded rank, scope) and run pods return everything to the controller by printing
to stdout, scraped from the kube log after the pod goes terminal. Six marker literals, each
duplicated across `crucible/` and `crucible-controller/` with "keep in sync" comments as the
only contract:

| Payload | Marker | Emitted | Scraped | Cap |
| --- | --- | --- | --- | --- |
| Grounded-rank verdict | `CRUCIBLE_VERDICT:` | `rank_grounded.rs:435` | `workpod.rs:466` | none |
| Scope report | `CRUCIBLE_SCOPE_REPORT:` | `scope.rs:1592` | `workpod.rs:479` | none |
| Scope transcript | `CRUCIBLE_SCOPE_TRANSCRIPT:` | `scope.rs:1573` | `workpod.rs:570` | 8 MiB, gzip+base64 |
| Scope pack | `CRUCIBLE_SCOPE_PACK:` | `scope.rs:1583` | `workpod.rs:499` | 4 MiB, gzip+base64 |
| Live progress | `CRUCIBLE_SCOPE_PROGRESS:` | `scope.rs:53` | `turn_live.rs` (SSE) | feed-capped |
| Run session | `=== SESSION (rc=…) ===` | `render.rs:324` (shell) | `workpod.rs:600` | 64 MiB scrape |

One night of production use (2026-07-05) surfaced the fragility class four ways:

- the openshell event stream glues messages without newlines, breaking a last-line parse
  contract (a validated $8 pack died on the adversary verdict parse — fixed by a reverse-scan
  fallback in an internal PR);
- every payload fights the kubelet's ~10 MiB log-rotation budget, forcing per-payload byte caps
  and truncation policies — and rotation can still eat a marker line entirely, which no scan
  direction survives;
- binary artifacts ride logs as giant base64 lines — the least log-shaped data imaginable;
- markers duplicate literals across crates because the crate DAG forbids sharing (`crucible`
  and `crucible-controller` deliberately do not depend on each other).

Two adjacent gaps compound it: startup adoption re-ingests grounded-rank verdicts but not scope
reports (`reconcile_turn_on_startup` only calls `parse_verdict_logs`, workpod.rs:1506), so a
scope turn that finishes while the controller is down is lost; and turn pods are invisible to
the completion watch — they are polled in-band by `KubePodDispatcher::await_terminal()`
(workpod.rs:1376), which discards the pod status it already holds.

Markers were the right zero-infrastructure bootstrap. They are the wrong permanent contract for
control-flow-critical, sized, structured data.

### The harness gap

Crucible began as a consumer of the vendored `agentic-ci` harness and re-implemented its stream
decode + NDJSON event model natively when the vendored package was dropped. Two things fell on
the floor in that move:

- **The OTLP collector never made the jump.** agentic-ci ran a small OTLP http/json receiver
  next to the agent, injected `CLAUDE_CODE_ENABLE_TELEMETRY=1` + the `OTEL_*` exporter env into
  the sandbox, logged every export to a jsonl, derived a live token rate, and rolled it all up
  into the `otel_summary` NDJSON event at run end. Crucible kept the consumer half —
  `AgentEvent::OtelSummary` (event.rs:135), the TUI rendering, the prefer-OTEL-cost logic — but
  nothing produces the event anymore: cost is estimated from a hardcoded pricing table
  (agent.rs:283) that silently drifts with every model release, `Tokens.rate` is permanently
  `None` (stream_json.rs:214), and per-model splits, API request latency, and active time are
  simply gone from the evidence.
- **The harness is trapped in a binary.** Agent spawn, stream-json decode, the event pump, and
  session logging live inside `crucible`'s bin target (agent.rs, stream_json.rs, event.rs,
  session.rs). Nothing outside this repo can drive an agent run the way agentic-ci's library
  consumers could; there is a standing requirement to expose that capability as a crate again.

The contract work below forces the crate question anyway (the marker literals need a home
shared across a crate DAG that forbids `crucible` ↔ `crucible-controller` dependencies); this
ADR answers it once for both gaps.

## Prior art (survey, 2026-07-06)

The industry converged on exactly two mechanisms, and on abandoning everything else:

- **Tekton** returns task results via the container termination message and lives inside its
  limits: 4096 bytes per container, 12 KiB per pod divided equally among containers. Hitting
  that ceiling produced TEP-0086 (options: storage API service, sidecar, ConfigMaps, CRDs,
  PVCs, logs — logs rejected outright for having "no availability guarantee") and TEP-0127
  (sidecar-logs escape hatch: ~3 s pod overhead, controller needs log RBAC, still capped by
  the 1.5 MiB CRD ceiling). Lesson: termination message for small verdicts only, and don't
  build the large tier on logs or CRDs.
- **Argo Workflows** started with output parameters in pod annotations (256 KiB limit),
  migrated to a `WorkflowTaskResult` CRD, and still fights the 1 MiB etcd ceiling with
  compression and optional database offload. Artifacts (the large tier) go to an artifact
  repository, not through the Kubernetes API. Lesson: the Kubernetes control plane is not a
  result store; two tiers are load-bearing, not an optimization.
- **Kubernetes itself**: `terminationMessagePath` (default `/dev/termination-log`) is read by
  the kubelet into `containerStatuses[].state.terminated.message`, atomically, as part of pod
  status — no scraping, no rotation exposure. Truncation at 4096 bytes is *silent*.
  `FallbackToLogsOnError` returns at most 2048 bytes / 80 log lines and only on error exits —
  useful as a debugging aid, not a contract.
- **GitHub Actions / Buildkite** runners push results and artifacts to the control plane over
  authenticated HTTP with a **per-job token**: minted by the control plane at dispatch,
  scoped to exactly one job, lifetime = job timeout plus a small grace (GitHub: +10 minutes),
  never persisted, scrubbed from logs. This is the standard *shape* for our Tier 2 —
  credential scoped to one unit of work — for which Kubernetes has a native carrier (bound
  service-account tokens, below).
- **Temporal** completes activities by RPC with a single-use task token and is explicit that
  delivery is at-least-once: the completer must be idempotent, keyed on the stable task
  identity, not the token. Lesson: the controller deduplicates; a retried POST is normal, not
  an error.
- **Prow** uploads artifacts to GCS from a sidecar and signals completion with marker objects
  an external watcher polls — the pattern we'd converge on if we ever outgrow a single
  controller; noted, not adopted.

Every migration in the survey moved *away* from logs, annotations, and CRDs and *toward*
"tiny critical payload in pod status, everything else over authenticated push to a store."
That is the design below.

## Decision

A two-tier result contract, defined in one shared crate. Stdout demotes to a human-facing
live/debug channel: progress beats stay, payloads leave.

### Tier 1 — the verdict rides the termination message

The turn command (both `rank-grounded` and `scope`, behind the existing `--marker`-style flag,
renamed in spirit to "result mode") writes its result JSON to `/dev/termination-log` as its
final act. The controller reads it from
`pod.status.containerStatuses[].state.terminated.message` — atomically, from the same pod
status object every read point already holds. No scraping, no rotation exposure, no glue.

- **Budget:** turn pods are single-container, so the kubelet grants the full 4096 bytes. The
  engine self-caps the JSON at **3584 bytes** (headroom against the *silent* kubelet
  truncation) with honest field-level truncation of rationale text, the same discipline the
  transcript cap uses. A truncated verdict is still valid JSON; a kubelet-truncated one is
  garbage — the self-cap is what keeps parse failures structural rather than probabilistic.
- **Schema:** a versioned envelope in the contract crate:
  `{"v":1,"kind":"verdict"|"scope_report","payload":{…},"artifacts":[{"kind","digest","bytes","delivered"}],"usage":{…}}`.
  `usage` is the optional OTEL rollup (§"The OTLP collector comes back"):
  `{"cost_usd","total","models":[…],"api_requests","api_ms","active_seconds"}` — a few hundred
  bytes that put *authoritative* cost on the ledger row instead of a pricing-table estimate.
  Under self-cap pressure the per-model rows drop first; the totals stay.
  The `artifacts` manifest is the load-bearing novelty: Tier 1 is the authoritative *index* of
  every Tier 2 upload the pod attempted, with content digests. The controller trusts the
  termination message (authenticated by the kubelet), then checks the drop-box against the
  manifest — a missing or digest-mismatched artifact is detected without trusting the POST
  path at all.
- **`terminationMessagePolicy` stays `File`.** `FallbackToLogsOnError` truncates to 2048
  bytes/80 lines and only fires on error exits; it would hand us exactly the parse-a-maybe
  garbage contract this ADR exists to kill. During migration the marker scrape is the
  fallback instead (§Migration).
- **Read points, all three:** `await_terminal()` grows a return of the terminated container
  state (it polls `api.get(name)` already and throws the status away); the completion watch's
  `pod_completion_key()` path for run pods; and **startup adoption**, which reads the same pod
  status — closing the scope-turn adoption gap as a side effect of the mechanism rather than
  as a special case.

### Tier 2 — artifacts POST to the controller's evidence drop-box

Packs, transcripts, and run sessions are files, and they move as files: an authenticated
`POST /api/pods/{pod}/artifacts/{kind}` (kinds: `scope-pack`, `scope-transcript`,
`run-session`, `otel-log`) with the raw gzipped bytes as the body. Typed, validated, size-checked at the
door — no base64, no line discipline, no rotation budget.

- **Storage is the state volume, not the database.** The controller streams the body to
  `state/evidence/<pod>/<kind>` and records only pointers + digests in SQLite, preserving
  ADR-0016's rows-are-pointers rule. S3 can replace the directory later without touching the
  contract (the drop-box is an interface, not a place).
- **Auth: a pod-bound projected ServiceAccount token, validated via TokenReview.** The pod
  spec gains a `serviceAccountToken` projected volume with a dedicated audience
  (`crucible-ingest`) and a short TTL; `render_turn()` adds the volume and the
  `CRUCIBLE_INGEST_URL` env var (the default `automountServiceAccountToken: false` stays —
  this is one explicit, audience-locked file, not the kube-API passport). The ingest
  extractor sends the bearer to TokenReview and requires three things from the response: the
  audience matches, the service account is the turns' own, and the bound-pod claim
  (`authentication.kubernetes.io/pod-name`) equals the `{pod}` path segment. A turn is
  exactly one pod, so pod-binding *is* turn-scoping. Nothing is minted, stored, or expired by
  us: the kubelet rotates the token, the API server invalidates it when the pod dies, and a
  controller restart changes nothing about validation. The cost is a kube API call in the
  handler (cached briefly per token; the endpoint sees a handful of requests per turn, not a
  request stream), accepted because the controller is already a kube client everywhere else.
  Entirely separate from the human-facing oauth2-proxy/role stack.
- **Delivery is at-least-once; the controller deduplicates.** Uploads are content-addressed:
  a re-POST of a digest the drop-box already holds returns 200 and changes nothing. The engine
  retries a failed POST 3 times with backoff and then *keeps going* — it still writes Tier 1
  with that artifact's manifest entry marked `delivered:false`, so the failure lands loudly on
  the `work_pods` row instead of silently truncating like the log path does today. Oversize at
  the door is a 413 with the limit in the body; the caps move from log-shaped guesses to real
  HTTP limits (pack 16 MiB, transcript 32 MiB, session 128 MiB, otel-log 32 MiB, all
  compressed — generous because they no longer fight the kubelet for log budget).
- **Ordering:** artifacts POST first, termination message last. The Tier 1 manifest can then
  state the truth about every upload, and the controller's fold has everything it needs the
  moment the pod goes terminal.

### The fold stays pull-shaped (ADR-0016 amendment)

ADR-0016 ruled "a push endpoint is explicitly not v1 … never a correctness dependency." This
ADR amends that ruling narrowly rather than reversing it. The ingest endpoint is a **data-plane
evidence drop** — the network equivalent of the S3 bucket and the session log, just closer.
Pods still never touch the database; no DB write happens in the request handler beyond
recording the artifact pointer. The *fold* — outcomes into `scopes`/`runs`/`candidates`/
`ledger` rows — still happens in the controller's own reconcile transaction, triggered by the
pod going terminal, reading evidence (now: pod status + drop-box instead of scraped logs). The
database remains a rebuildable index; `crucible db rebuild` reads the evidence directory the
same way it reads session logs. What actually changed from ADR-0016's assumption: logs turned
out not to be a durable evidence store (the 2026-07-05 incidents, and Tekton's identical
finding), so the evidence needed a real drop point. Push is the delivery mechanism for
evidence, not the ingestion model.

### One contract crate

A new `crucible-contract` crate (no async, no kube, serde only) that `crucible`,
`crucible-controller`, and `crucible-broker` all depend on. It owns: the Tier 1 envelope and
payload types, the artifact-kind enum + per-kind size caps, the ingest request/response types,
the env-var names, the `managed-by` label, the `AgentEvent` NDJSON types (the session-log
format both the engine and the controller's SSE relay read), and — until the migration
completes — the six marker literals, deleting every "keep in sync" comment in the workspace.
The termination message and the ingest bodies are `serde` round-trips of the same types on
both sides; schema drift becomes a compile error instead of a 3 a.m. parse failure.

### The harness crate — the agentic-ci replacement

A second crate, `crucible-harness` (library; async), extracted from the `crucible` bin target:
agent spawn + env assembly, the stream-json decoder, the event pump + NDJSON session logging,
and the OTLP collector below. It depends on `crucible-contract` and nothing controller-shaped.
Consumers: the `crucible` binary (loop + turn commands), and external projects — the library
role the vendored agentic-ci package used to play, restored as a Rust crate.

The boundary rule between the two: **contract = wire types both sides serde-round-trip;
harness = everything that runs next to an agent.** The controller depends only on the
contract; the crate DAG rule (`crucible` ↮ `crucible-controller`) survives intact.

### The OTLP collector comes back, in-process

The harness embeds the collector agentic-ci ran as a subprocess, as an in-process task:

- **Receiver:** binds a configurable address, port 0 auto-assign; accepts `/v1/metrics`,
  `/v1/logs`, `/v1/traces` OTLP http/json POSTs; appends every export raw to `otel.jsonl` in
  the run dir (the `otel-log` Tier 2 artifact); answers `{"partialSuccess":{}}`.
- **Live:** a 60 s sliding-window token rate, fed to the event stream over a channel —
  `Tokens.rate` stops being permanently `None`. The rate/port *files* of the Python
  implementation were subprocess IPC; in-process they die.
- **Rollup:** at agent exit the collector builds the `otel_summary` event (authoritative
  cost, per-model input/output/cache tokens, `api_requests`, `api_ms`, `active_seconds`) —
  the event finally has a producer again — and the same rollup rides Tier 1 as the envelope's
  `usage` field.
- **Env injection:** the harness sets `CLAUDE_CODE_ENABLE_TELEMETRY=1`,
  `OTEL_{METRICS,LOGS,TRACES}_EXPORTER=otlp`, `OTEL_EXPORTER_OTLP_PROTOCOL=http/json`, the
  endpoint, `OTEL_METRIC_EXPORT_INTERVAL=10000`, plus full-fidelity spans exactly like
  agentic-ci did: `CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1` (tool nesting + subagent spans) and
  `OTEL_LOG_USER_PROMPTS=1` / `OTEL_LOG_TOOL_DETAILS=1` / `OTEL_LOG_TOOL_CONTENT=1` — on the
  spawned agent (local exec) or into the sandbox env (openshell), following agentic-ci's
  per-harness matrix (docs/otel-configuration.md upstream). Consequence, stated plainly: the
  `otel-log` evidence contains prompt text and tool input/output, so it is confidential the
  same way session transcripts already are; anywhere it is exported to (§MLflow) must be as
  trusted as the state volume.
- **Sandbox reach:** local agents export to `127.0.0.1`; an openshell sandbox reaches the
  collector exactly the way it reaches the broker — the collector binds `0.0.0.0` on the
  loop/turn pod and the sandbox gets a `host.containers.internal:<port>:full` egress rule
  (the proven 8849 pattern).
- **Degradation:** telemetry off or collector unreachable is graceful and identical to today —
  no `otel_summary`, no `usage`, pricing-table estimate as the cost fallback. The collector is
  evidence enrichment, never a run dependency.

### MLflow export — a controller background task over the evidence

The MLflow exporter is a private control-plane component: it lives in the controller
deployment and is a per-installation opt-in, no part of the public engine depends on it.

Capturing the raw OTLP jsonl as a first-class artifact is what makes an MLflow exporter a
bolt-on instead of a rework. agentic-ci proved the shape (`mlflow-push`: a decoupled,
allow-failure follow-up job that re-encodes the captured `/v1/traces` records OTLP-JSON →
protobuf and POSTs them to MLflow), and MLflow grew the native half since. Crucible runs map
onto the experiment-tracking ethos directly, so this is a workstream (R6), not a someday note.

- **Locus: the controller, only.** The exporter is a controller background task fed by
  evidence that already exists — the `otel-log` artifact in the drop-box and the folded
  ledger rows. It runs post-fold, never inline with a run or a fold, and failure marks the
  export row and retries later (the agentic-ci `allow_failure: true` ethos). Pods never see
  MLflow credentials; local `crucible --manifest` runs don't export.
- **Traces:** MLflow ≥ 3.6 ingests OTLP at `POST /v1/traces` — OTLP/HTTP + protobuf only
  (no gRPC), experiment selected by the `x-mlflow-experiment-id` header, bearer auth, gzip
  accepted from 3.7, SQL backend store required. Association is *experiment-level only*;
  there is no documented run-id header, so trace↔run correlation rides span/resource
  attributes we stamp ourselves (run id, turn, domain) plus matching tags on the MLflow run.
- **Runs:** plain tracking REST — `experiments/get-by-name`/`create`, `runs/create`,
  `runs/log-batch` (params: domain, model, effort, manifest knobs; metrics: gate score /
  cost / tokens, per-turn as steps), artifact upload via the proxied `mlflow-artifacts`
  route, `runs/update` to close. Mapping: **domain (or rig) → experiment, crucible run →
  MLflow run, turn → metric step.**
- **Dependencies:** no maintained Rust MLflow client exists; the exporter is a thin
  `reqwest` REST module plus `opentelemetry-proto` (serde feature) for the JSON→protobuf
  re-encode. Both already-idiomatic, well-maintained picks; nothing exotic.
- **Confidentiality:** with the content flags on (§env injection), exported traces carry
  prompt text and tool output — the MLflow instance must sit inside the same trust boundary
  as the state volume, and the exporter stays a per-deployment opt-in (tracking URI + token
  + experiment mapping in controller config, off when absent).

### What stays on stdout

Live progress (`CRUCIBLE_SCOPE_PROGRESS:` beats, human-readable narration) stays on stdout by
design: it is loss-tolerant, ordering-tolerant, and consumed live by the SSE relay
(`turn_live.rs`) — logs are exactly the right transport for it. The boundary rule: **stdout
carries what a human tails; the contract carries what the controller acts on.**

## Migration

Three phases, each shippable alone, old/new compatible in both directions throughout:

1. **Contract crate + Tier 1.** Engine writes the termination message *and* keeps emitting
   markers; controller prefers the termination message and falls back to the marker scrape
   (which also covers pods launched by an old engine image). Startup adoption switches to pod
   status, fixing the scope-turn gap. Ships alone with zero coordination risk.
2. **Tier 2.** Controller mints tokens and injects `CRUCIBLE_INGEST_*`; engine POSTs artifacts
   when the env vars are present and falls back to marker emission when they are absent (old
   controller + new image keeps working). Run-session delivery moves onto the same endpoint
   family, retiring the `=== SESSION ===` delimiter and the 64 MiB scrape.
3. **Demotion.** After both sides are rolled and a soak, the engine stops emitting payload
   markers (pack/transcript base64 lines and the report/verdict lines disappear from logs),
   and the controller's scrapers + duplicated literals are deleted. Progress beats remain.

The compatibility matrix is the fallback chain itself: new controller + old image → markers;
old controller + new image → markers (no ingest env, marker emission still on until phase 3);
new + new → the contract. Phase 3 is the only step that burns a bridge, and it is gated on the
fleet being past phases 1–2.

## Failure modes, named

- **Kubelet truncation:** prevented by the 3584-byte self-cap; a manifest digest that doesn't
  parse is a structural bug, not an operational one.
- **Pod dies before writing the message** (OOM, eviction, wrapper crash): phase=Failed with an
  empty message — the same information content as today's no-marker log, handled by the same
  failed-turn path, now distinguishable from "succeeded but unparseable."
- **Controller down during the turn:** auth state lives in the API server, not the controller,
  so nothing to recover; the pod's POSTs retry through the outage window, and whatever didn't
  land is recovered by adoption reading the termination message + manifest and flagging
  undelivered artifacts. A turn is never lost to a controller restart again.
- **Duplicate delivery** (pod retry, network retry): content-addressed dedup; idempotent 200.
- **Stolen token:** audience-locked to the ingest endpoint (useless against the kube API or
  any other service), pod-bound (only impersonates the one turn pod, and the API server kills
  it when that pod dies), and the endpoint is write-only — blast radius is "attacker can
  upload a wrong artifact whose digest won't match the kubelet-authenticated manifest."
- **Kube API unavailable at validation time:** TokenReview fails closed (503 to the pod, which
  retries on its backoff budget); the termination-message manifest keeps the miss loud.

## What this is NOT

- **Not a message bus, not steering.** One-shot result delivery at turn end. The ADR-0001 wall
  stands: nothing flows controller→pod mid-turn through this contract, and progress stays
  advisory.
- **Not a CRD, not annotations, not a sidecar.** The survey is unambiguous that all three are
  migrations waiting to happen (Argo's 256 KiB→1 MiB ladder, Tekton's TEP-0127 RBAC/overhead
  costs). We have a controller with a disk; we use it.
- **Not the inner loop's state.** A local `crucible --manifest` run still never needs a
  controller; result mode is opt-in exactly like `--marker` is today, and the session log +
  git remain a run's complete record (ADR-0016 unbroken).
- **Not human-facing API surface.** The ingest route lives outside the SPA/OpenAPI-typed
  surface's auth model, is write-only, and never appears in the UI's generated client.
- **Not the controller's observability stack.** The OTLP collector receives the *agent's*
  telemetry as run evidence; the controller's own prometheus `/metrics` + ServiceMonitor
  stack is a different concern and untouched by this ADR.

## Consequences

- **Positive:** the verdict path stops depending on log rotation, newline discipline, and
  base64-in-logs; scope turns survive controller downtime (adoption gap closed by
  construction); artifact caps become honest HTTP limits instead of log-budget gymnastics; six
  keep-in-sync literal pairs collapse into one typed crate; the failure story upgrades from
  "silently truncated" to "row flagged with exactly which artifact didn't land"; authoritative
  cost and per-model usage return to the ledger (the pricing-table estimate demotes to a
  fallback), and the harness becomes a consumable crate again instead of a bin-shaped fossil.
- **Negative / cost:** the controller gains a pod-facing HTTP surface (new attack surface,
  mitigated by write-only + audience/pod-bound tokens) and its ingest handler couples to the
  kube API (TokenReview, cached, failing closed);
  the state volume now holds evidence payloads, so disk sizing and retention become real
  (pointer-rows stay small; the *volume* doesn't); a three-phase migration must actually be
  driven to phase 3 or we run both contracts forever; `crucible-contract` and
  `crucible-harness` are two more crates whose semver discipline matters from day one, and the
  harness extraction is a large mechanical refactor of the engine's core.

## Workstreams

| # | Workstream | Builds on |
| --- | --- | --- |
| R0 | `crucible-contract` crate: envelope/payload/ingest types, kinds + caps, env names, marker literals moved | — |
| R1 | Tier 1: engine writes termination message; `await_terminal()` returns terminated state; controller prefers it with marker fallback; startup adoption reads pod status (scope gap closed) | R0 |
| R2 | Tier 2: projected-token volume + `CRUCIBLE_INGEST_URL` in `render_turn()`, TokenReview extractor, ingest route + drop-box, engine POST with retry + manifest | R0, R1 |
| R3 | Run-session delivery onto the ingest family; `=== SESSION ===` delimiter + 64 MiB scrape retired | R2 |
| R4 | Demotion: payload markers off, scrapers deleted, contract crate drops the marker literals | R1–R3 soaked |
| R5 | `crucible-harness` crate: extract spawn/decode/pump/session from the bin; OTLP collector + env injection + sandbox egress; `otel_summary` producer restored; `usage` in Tier 1; `otel-log` artifact kind | R0 |
| R6 | MLflow exporter in the controller: trace re-encode + `/v1/traces` push, run/param/metric mapping over tracking REST, per-deployment config + export bookkeeping | R2, R5 |

**Slicing:** R0+R1 ship together (pure win, no coordination). R2 behind them; R3 rides R2's
plumbing; R4 only after a fleet soak proves both directions of the compat matrix were honored.
R5 is independent of R2–R4 and can run in parallel once R0 lands: the extraction + collector
are engine-local, `usage` piggybacks on R1's envelope, and the `otel-log` upload piggybacks on
R2's endpoint (until R2, the otel jsonl just stays a run-dir file like every local artifact).
R6 is last by construction — it consumes what R2 delivers and R5 produces.

## Appendix: prior-art survey (full notes, 2026-07-06)

The condensed lessons live in §"Prior art"; these are the full survey notes with sources, kept
so the numbers and rejections above stay auditable.

### Tekton Pipelines

Task results ride the container termination message: **4096 bytes per container**, **12 KiB
per pod divided equally among all containers** (init containers included — 12 containers means
1024 bytes each). Hitting the ceiling produced two design cycles:

- **TEP-0086 "Changing the way result parameters are stored"** (status: Proposed) surveyed the
  alternatives: a dedicated storage API service (OIDC tokens from pods, backend-agnostic),
  a result sidecar uploading to external storage, ConfigMaps per TaskRun (rejected: 3+ API
  requests per TaskRun, still 1.5 MiB-limited), a custom CRD (same ceiling), PVCs (complexity/
  perf concerns), and **logs — rejected outright for having "no availability guarantee"**.
  <https://github.com/tektoncd/community/blob/main/teps/0086-changing-the-way-result-parameters-are-stored.md>
- **TEP-0127 "Larger Results via Sidecar Logs"** (status: Implemented) shipped the escape
  hatch: a Tekton-injected sidecar watches `/tekton/run`, waits for all steps, prints results
  to its stdout in a parsable pattern, and the controller reads the sidecar's logs. Costs:
  ~3 s extra pod startup, the controller needs pod-log RBAC, per-result size still gated by
  the `max-result-size` flag (default 4096 bytes), total still capped by the **1.5 MiB CRD
  limit** (TaskRun fails beyond it), and tasks assuming large results break on unconfigured
  installs. <https://github.com/tektoncd/community/blob/main/teps/0127-larger-results-via-sidecar-logs.md>

Motivating issues: <https://github.com/tektoncd/pipeline/issues/4012>,
<https://github.com/tektoncd/pipeline/issues/4060>; docs:
<https://tekton.dev/docs/pipelines/tasks/>.

### Argo Workflows

Output parameters are capped at **256 KiB** (the pod-annotation limit — parameters were
originally reported by the wait sidecar patching pod annotations)
(<https://argo-workflows.readthedocs.io/en/latest/walk-through/output-parameters/>); reporting
later moved to the internal **`WorkflowTaskResult` CRD** (higher capacity, needs garbage
collection)
(<https://github.com/argoproj/argo-workflows/blob/main/manifests/base/crds/full/argoproj.io_workflowtaskresults.yaml>).
Aggregate workflow status fights the **1 MiB etcd object limit** with, in order: node-status
compression (~20:1), opt-in SQL offload (`nodeStatusOffLoad: true`), and (v3.7+) container-args
offload to ConfigMaps past 128 KiB
(<https://argo-workflows.readthedocs.io/en/latest/offloading-large-workflows/>). The documented
recommendation for anything larger is **artifacts to an artifact repository (S3/GCS), never
parameters** — emit a small count parameter and index into the artifact
(<https://github.com/argoproj/argo-workflows/blob/main/examples/handle-large-output-results.yaml>).
The wait sidecar collects outputs and reports to the controller; it originally mounted
`docker.sock` (bypassing RBAC) and was migrated to the Kubernetes API via service account.

### Kubernetes primitives

`terminationMessagePath` (default `/dev/termination-log`) is read by the kubelet into
`containerStatuses[].state.terminated.message`, atomically, as part of pod status, immediately
on container termination. Limits: **4096 bytes per container, 12 KiB per pod total** (divided
equally), and truncation is **silent**. `terminationMessagePolicy: FallbackToLogsOnError`
substitutes the log tail only when the file is empty *and* the exit was an error, capped at
**2048 bytes or 80 lines, whichever is smaller**. The path is immutable after pod creation,
and the docs scope the mechanism to "brief final status, such as an assertion failure message".
<https://kubernetes.io/docs/tasks/debug/debug-application/determine-reason-pod-failure/>

### GitHub Actions runner

Per-job credential pattern: a job-scoped OAuth token is generated when the job is queued,
lives for the job duration (default 6 h timeout) **plus 10 minutes**, is held only in memory,
and is delivered inside a job message encrypted to the runner's RSA public key; each action
subprocess receives it as an env var registered as a secret (scrubbed from logs). Artifacts
upload through the authenticated API with that token.
<https://github.com/actions/runner/blob/main/docs/design/auth.md>,
<https://docs.github.com/actions/security-guides/automatic-token-authentication>

### Buildkite agent

Same shape: an internal per-job access token generated at job start, exposed as
`BUILDKITE_AGENT_ACCESS_TOKEN`, scoped to the single job, dead when the job finishes —
distinct from the long-lived cluster-wide agent registration token. Used for artifact upload,
annotations, metadata. <https://buildkite.com/docs/agent/v3/tokens>,
<https://buildkite.com/docs/apis/rest-api/artifacts>

### Temporal

Activity completion is an RPC carrying an opaque **task token**, single-use and scoped to one
execution attempt — invalidated when the attempt retries, which is why Temporal recommends
external services key off **Workflow Run ID + Activity ID**, never the token
(<https://docs.temporal.io/activities>). Semantics: workflows are exactly-once, **activities
are at-least-once** — the server does not deduplicate completions, so idempotency is the
completer's job, enforced at the receiving service
(<https://temporal.io/blog/idempotency-and-durable-execution>,
<https://community.temporal.io/t/is-a-system-generated-activity-id-suitable-to-use-as-an-idempotency-key/13181/2>).

### Prow, Airflow, Kueue (brief)

- **Prow**: pod utilities (`initupload`, `sidecar`) push artifacts to GCS under workload
  identity; `finished.json` marker objects signal completion and the `crier` watcher reports
  onward — sidecar-upload plus polled completion markers, the multi-controller shape we'd
  converge on past a single controller. <https://docs.prow.k8s.io/docs/spyglass/>
- **Airflow**: XCom results default into the metadata database, which bloats under artifact
  traffic; the documented fix is a custom XCom backend on object storage — the same
  pointers-not-payloads split as ADR-0016, by convention rather than contract.
  <https://airflow.apache.org/docs/apache-airflow/stable/core-concepts/xcoms.html>,
  <https://github.com/apache/airflow/issues/15761>
- **Kueue**: admission-controller model over Job status; no custom result-return path — not
  relevant to this contract.

### Synthesis carried into the decision

| Dimension | Value | Source |
| --- | --- | --- |
| Tier 1 ceiling | 4096 B/container (silent truncation), 12 KiB/pod | Kubernetes |
| Tier 1 self-cap | 3584 B (headroom under the silent cut) | this ADR, from the above |
| Fallback-to-logs cap | 2048 B / 80 lines, error exits only — unusable as contract | Kubernetes |
| Annotation/CRD result stores | 256 KiB / ~1 MiB ceilings + GC churn — rejected | Argo's migration ladder |
| Logs as result channel | "no availability guarantee" — rejected | Tekton TEP-0086, our #126 |
| Sidecar collection | ~3 s overhead + log RBAC + interop breakage — rejected | Tekton TEP-0127 |
| Per-task credential | scoped to one unit of work, lifetime = timeout + ~10 min grace | GH Actions, Buildkite |
| Delivery semantics | at-least-once; receiver deduplicates on stable task identity | Temporal |
