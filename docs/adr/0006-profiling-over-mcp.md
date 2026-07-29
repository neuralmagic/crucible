# ADR 0006: Profiler support over MCP — a generic capability, pprof for the EPP

**Status:** Accepted; implemented (generic profiler in `crucible-broker`; pprof backend for the EPP, torch-trace for vLLM)
**Date:** 2026-06-26
**Related:** [ADR-0002](./0002-mediated-provisioning-mcp.md) (the agent asks, the host holds the keys),
[ADR-0005](./0005-engine-side-builds.md) (engine-side build + the broker `measure` tool), the
`epp-mcp` broker, the #1109 TTFT domain (`crucible.ttft.toml`).

## Context

#1109 asks the agent to cut the EPP's contribution to TTFT. But the gate — end-to-end `bench --ttft`
through the live service — **cannot isolate the EPP**, and **cannot resolve small EPP wins**:

- The measured path is `client → envoy-proxy → (ext_proc) → epp → envoy → backend pool → first token`,
  all three in one pod behind `svc:80`. The `MOCK_PREFILL_TIME_PER_TOKEN_MS=0` scoping zeroes the
  backend *compute*, but the ~2.2 s TTFT still includes Envoy's 220 KB body handling, the data-plane
  transfer (220 KB × c=150 ≈ 33 MB/hop), backend *ingest* of that body, and queueing at c=150.
- Result: every micro-optimization the agent tried (alloc, JSON, GC, gRPC windows) came back "within
  noise." The agent was **guessing** at the hot path and optimizing a low-single-digit-% sliver that
  the c=150 measurement noise swamps. The TTFT gate is right for *scoring a candidate*; it is the wrong
  instrument for *finding* what to change.

A profile fixes both problems at once: `pprof` attributes CPU / allocations / contention to specific
EPP **functions and source lines** — isolated to EPP code by construction, and far more sensitive than
a noisy end-to-end delta. The EPP already ships pprof: `--enable-pprof` defaults to **true**
(`pkg/epp/server/options.go`), handlers are registered on the metrics server (`runner.go`,
`pkg/common/observability/profiling/pprof.go` → `net/http/pprof`), served on **`:9090`**. The catch:
that port runs controller-runtime **secure metrics**, so `/debug/pprof/` is token-gated (a bare GET
returns `Unauthorized`), and the sandboxed agent can reach neither the in-cluster ClusterIP (egress
wall) nor has `go tool pprof`.

## Decision

Add a **generic Profiler capability** to the broker — mediated "observe the candidate's internals" —
**with the concrete profiler as a domain-configured backend**, exactly the way `measure` shells the
domain's `BROKER_MEASURE_CMD`. The agent asks; the loop pod (cluster reach + token + tooling) captures
and analyzes and hands back text. The agent never touches the endpoint or knows the backend.

**Generic (broker core) — two tools that shell domain-provided commands:**

- **`profile{kind, seconds}`** — capture a profile of `kind` (backend-defined) over `seconds`. The
  broker shells `BROKER_PROFILE_CAPTURE_CMD` (env: `$KIND`, `$SECONDS`, `$OUT` path), stores the result
  on the loop pod under the build storage root, returns a handle + short summary.
- **`profile_query{query}`** — run `query` against the stored profile. The broker shells
  `BROKER_PROFILE_QUERY_CMD` (env: `$PROFILE`, `$QUERY`), returns text. The `query` vocabulary is
  backend-defined. Read-only by construction (the backend command is a fixed analyzer; the broker
  applies a deny-list so a query can't start a server or write files).

The broker knows nothing about pprof, Go, or the EPP — only "capture a profile, then query it,"
delegated to configured commands. This is the same split as `forge` (generic build engine) vs the EPP
build wiring, and `measure` (generic) vs `BROKER_MEASURE_CMD` (domain).

**Domain implementation (#1109 = pprof against the Go EPP):** the EPP domain sets the two commands —
`CAPTURE_CMD` = an authed GET of `epp:9090/debug/pprof/<kind>?seconds=$SECONDS`; `QUERY_CMD` =
`go tool pprof -<query> $PROFILE`. So for EPP, `kind` ∈ {`cpu`,`heap`,`mutex`,`block`} and `query` is
pprof's command surface: `top20`, `list HandleRequestBody` (per-line ns/alloc in the real source — the
killer), `peek json.Unmarshal`, `traces`, `tree`. Another domain (a Python service, a native binary)
swaps in py-spy / perf / eBPF via the same two env commands; the broker tools and the agent-facing
surface are unchanged.

The loop becomes **profile → read the hot lines → fix them → re-profile to confirm the hotspot shrank →
`measure` to score**. Profile to *find* (isolated, sensitive); the gate to *score*.

## Implementation

- **Broker tools** (`epp-mcp/src/profile.rs`, registered in `server.rs`, exported in
  `lib.rs`): generic `profile{kind,seconds}` + `profile_query{query,handle?}`, shelling
  `BROKER_PROFILE_CAPTURE_CMD` (env `$KIND/$SECONDS/$OUT`) / `BROKER_PROFILE_QUERY_CMD` (env
  `$PROFILE/$QUERY`). Captures stored under `<FORGE_STORAGE_ROOT>/profiles` with a `latest` marker;
  unconfigured → `disabled` (mirrors `measure`). Read-only deny-list on the query (no `$()`, no
  pipes/redirects, no `-http/-web/-output/-svg/...`), kind sanitized, handle path-traversal-guarded.
  No pprof/Go/EPP knowledge in the tool code. Unit-tested (deny-list, kind validation, traversal).
- **EPP/pprof backend (swappable domain wiring):** capture adapter `domains/<domain>/tools/epp-pprof.nu`
  (port-forwards :9090, mints the reader-SA token, authed-GETs `/debug/pprof/$KIND` → `$OUT`, same
  pattern as `epp-metrics.nu`); the two `BROKER_PROFILE_*` commands in the loop's deployment manifest
  (`epp-pprof ...` / `pprof -$QUERY $PROFILE`); the standalone `pprof` analyzer baked into
  `Containerfile.crucible` via a CGO-free Go stage.
- **RBAC (the access path — chose RBAC over an unauth :6060):** extended the `epp-metrics-reader`
  ClusterRole in `rig-up.rs` to add `/debug/pprof` + `/debug/pprof/*` nonResourceURLs, so the same
  reader token controller-runtime already authorizes for `/metrics` also covers pprof on that mux.
- **Advertised:** the `profile` skill (`domains/<domain>/skills/profile/`) + a "profile before you guess"
  note and the two tools in `prompts/research.md`.

Validated live:

- **Access works.** The reader token authorizes `/debug/pprof` end-to-end — `/metrics`,
  `/debug/pprof/`, and `/debug/pprof/profile?seconds=1` all return 200 once the `epp-metrics-reader`
  ClusterRole carries the pprof nonResourceURLs (this ADR) **and** the EPP service account holds
  `system:auth-delegator` (required for `:9090` auth to function at all — without it `/metrics` 500s too;
  `rig-up` creates the binding). controller-runtime authorizes `/debug/pprof` for a metrics-style
  nonResourceURL grant — same mux, per-path authz, so the URLs must be granted explicitly.
- **Capture under load works** and the signal is real. A 20 s CPU profile during a `measure`
  (TTFT 2275 ms, c=150) showed the EPP busy only ~24% of one core — so **most of the 2.2 s is NOT EPP
  CPU** (it's transfer/queue/envoy, exactly the isolation finding). Within the EPP's CPU the hot path
  is: `encoding/json` validation/parse of the 220 KB bodies (checkValid/unquoteBytes/appendString,
  ~15–20% cum), GC churn from those bodies (`gcDrain` 17% cum, `memclrNoHeapPointers` 9% flat,
  scanobject/memmove), and `prometheus/common/expfmt` text parsing (~15% cum — the EPP scraping
  backend metrics). None of this is the routing/scheduling logic the agent kept guessing at; the
  profile names the real cost. `top` / `peek` / `traces` / `tree` all work from the symbolized profile.

**`list=<Func>` per-line cost** is resolved by mapping the profile's source paths to the candidate's
staged tree. The EPP is built without `-trimpath` (`Dockerfile.epp` `WORKDIR /workspace`),
so the profile embeds `/workspace/pkg/epp/...` paths, while forge stages the candidate repo root at
`/var/lib/forge/ctx`. `BROKER_PROFILE_QUERY_CMD` now carries
`-trim_path=/workspace -source_path=/var/lib/forge/ctx`, so pprof strips the build prefix and finds the
file under the staged ctx: `list=<Func>` shows real per-line ns/alloc for the EPP's own `pkg/epp/...`
source once a `build_epp` has run this turn (ctx is the tree the deployed candidate was built from).
`top`/`peek`/`traces`/`tree` work without a build. **Deliberate non-goal:** `list` on stdlib/vendored
frames (`encoding/json`, `prometheus/common/expfmt`) doesn't resolve — only the EPP's own source is
staged, and the agent edits EPP code, not the stdlib, so the actionable surface is covered. Verified the
embedded paths against a live capture (`/workspace/pkg/epp/datalayer/collector.go`).

## Generic multi-target profiler and the vLLM backend

Three changes make the broker genuinely component-agnostic and ready for composite (multi-domain)
rigs, all in `crucible-broker/src/profile.rs` (the profiler lives in the extracted broker crate):

- **Capture extension externalized (`BROKER_PROFILE_EXT`).** The broker named handles `<kind>-<nonce>.pprof`
  — a pprof-ism in a supposedly generic core. The extension is a domain concern (the broker stores the
  bytes the capture writes and never interprets them); a format-sniffing analyzer wants the right name.
  Now domain-set, neutral default `prof`. The EPP sets `pprof`; the vLLM torch-trace backend sets `json.gz`.
- **Multiple profile targets (`BROKER_PROFILE_TARGETS`).** A composite deployment (vLLM +
  EPP) has more than one profileable component, each with its own image, way in, and profiler. Profiling
  is now keyed by a named **target**: `BROKER_PROFILE_TARGETS="vllm epp"` plus per-name
  `BROKER_PROFILE_<T>_CAPTURE_CMD` / `_QUERY_CMD` / `_EXT`. `profile`/`profile_query` take an optional
  `target` (omit it for a single-component rig; name it — e.g. `vllm` or `epp` — when there are several;
  an omitted target with several is an error that lists them). Captures nest under `profiles/<target>/`
  with a per-target `latest`. A single-component rig still just sets the un-prefixed `BROKER_PROFILE_*`
  (one implicit `default` target) — fully backward-compatible, no EPP-run change.
- **vLLM GPU-trace backend reconciled to the contract.** `domains/<domain>/tools/vllm-profile.nu` (capture)
  + `vllm-analyze.nu` (kernel→source correlation, inverting neuralmagic/ai_auto_perf_analysis) now honor
  the same `$KIND/$SECONDS/$OUT` // `$PROFILE/$QUERY` surface as `epp-pprof`, so the fold-in is pure env
  wiring — no broker core change. `$SECONDS` is unreferenced for vLLM (a GPU window is prompt-count
  bounded). The gate-workload question is settled serve-path: both gate and profiler drive the same
  frozen `VLLM_BENCH_*` workload via `vllm bench serve`. Live GPU validation requires the analysis-layer
  image; the contract is tested at its boundaries.

The agent-facing tools and the per-component domain wiring are unchanged in shape; the broker just routes
by target name. This is the same generic-core / domain-backend split as `forge` and `measure`.

## Consequences

- **Positive:** the agent optimizes the *measured* hot path instead of theorizing in the noise; the
  isolation/sensitivity problem the TTFT gate cannot solve is solved by direct observation; composes
  with `measure`/`build`/`deploy`/cap; generic, so it strengthens the case for the broker extraction.
- **Negative / cost:** a small RBAC grant, a binary in the loop image, two more broker tools, and a
  rebuild. Profiling adds minor overhead to the EPP during capture (CPU pprof is sampled — low). The
  `pprof` pass-through needs a sane command deny-list so the agent can't start servers or write files.
