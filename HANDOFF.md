# Linked boundary, the Vertex move, and the self-host demo: handoff

Date: 2026-08-25. Three arcs ran together and are entangled, so read the demo section last: it
is blocked on the other two.

## What this was

A self-hosted demo loop (crucible optimizing its own agent-stream decoder) turned into two
deeper things. Adopting it on prod failed on a flag the controller had been sending to an engine
that never accepted it, which is the argv boundary failing exactly as designed, so RFC-0004 now
governs that boundary and the engine half has shipped. Separately, the move to the
`crucible-729940` Vertex project collided with a live Google outage and then with a credential
binding bug that only a sandboxed turn could reveal.

## The demo pack

`examples/selfhost` is on main. It scores `StreamJsonParser` in ns per input line over a
deterministic synthetic corpus. The bench hashes every emitted event, so a candidate that drops
or reshapes events reports invalid rather than fast; the bench and `measure.sh` are frozen
injects, the gate also runs the crate's tests and clippy with warnings denied, and the self-test
proves discrimination by decoding every line twice.

Proven locally on 2026-08-25 with the `local` backend: three iterations, three keeps,
891 to 176.6 ns per line, $7.89, run ended `finished`. It has never completed on prod.

`examples/selfhost/scenario.json` is the adoption body. It carries no `git_ref` on purpose: see
the traps below.

## Engine (this repo), all merged to main

| What | Commit |
| --- | --- |
| The self-host pack, the bench, the Rust images, `docs/domain-images.md` | `ab48d5e` (#70) |
| Bind the regional Vertex hosts | `7a97f2b` (#73) |
| Split crucible into a library and a thin binary; contract version; `--repo-ref` | `7b59a44` (#72) |
| Keep the AWS SDK out of the linked library; trace the renders | `71a2918` (#74) |

The runtime image built from `71a2918` is
`ghcr.io/neuralmagic/crucible@sha256:cff37efd7abdbfb7431d1fe526b2acb37bef1c2591b52282efb3639e64300216`
and carries `io.crucible.contract-version=1.0.0`. Its build fails if that label disagrees with
the binary it ships.

RFC-0004 is on main in `spec`. It has not been advanced; whether the engine half shipping is
enough to seal the candidate is a call for whoever picks this up.

Open here: **#75** (this handoff and the work items), #65 and #51 from earlier arcs.

## Controller (`~/git/agentic-epp-autoresearch`)

**#459 is the gate.** Branch `linked-boundary`, pinned to engine `71a2918`. It replaces every
render subprocess with a linked call, records the contract version, compares it against every
configured dispatch image, and refuses a launch against a mismatched image with a ledger entry
naming both versions and a `ContractRejected` park. 1210 tests plus 16 e2e green against a live
database, clippy clean, both guardrails green. It has been rebased twice, and main is moving
under it roughly hourly, so expect a third.

Also open: **#462** (work items), #461, #426, #408, #372.

Merged today: #451 (Vertex project flip), #456 (us-east5 pin), #457 (the revert of #456).

## Prod state right now

MPP `crucible--runtime-ext`, Argo `Application/crucible-controller`, sync is manual.

- Vertex project `crucible-729940`, service account `crucible-agent@crucible-729940`, secret
  `crucible-vertex-adc` rolled on both MPP and waldorf.
- `CLOUD_ML_REGION` is back to `global` after the revert. This is a stopgap.
- Loop image `ghcr.io/neuralmagic/crucible-loop:main@sha256:5aca6eed...`, which predates every
  engine change above.
- `run_iterations=10`, `run_max_cost=40` in the overrides ConfigMap. **Every Argo sync resets
  these to null.** Re-run `scripts/roll-vertex-project.fish knobs` after each sync.
- Four adopted scenarios are parked, all at $0: `01a03a0f` and `01a03ac3` on the `--repo-ref`
  failure, `01a03ad1` and `01a03b22` on the credential binding.

## Three traps, each of which cost an hour

**A region other than `global` breaks every sandboxed turn** until a loop image carries `7a97f2b`.
The gateway resolves a region to its own apex domain (`us-east5-aiplatform.googleapis.com`), the
engine bound the credential only to `aiplatform.googleapis.com` and its dot-subdomains, and a
hyphen-prefixed apex domain is not a dot-subdomain. The credential goes unbound, the metadata
emulator answers 503, and the agent exits 1 with nothing on stderr. A direct `rawPredict` probe
proves nothing about this: it bypasses the gateway. Probe with a real turn in a pod.

**The `global` endpoint is degraded.** It returned NOT_FOUND for enabled models on 30 to 70
percent of calls through the evening. Claude Code probes availability before a turn and falls
back to `claude-sonnet-4-5`, which this project is not entitled to, so a blip is fatal to the
turn. Enabled: `claude-opus-4-6`, `claude-opus-4-6[1m]`, `claude-sonnet-5`,
`claude-haiku-4-5@20251001`. Not enabled: `claude-sonnet-4-5` (worth asking for, it makes the
fallback survivable) and `claude-opus-4-5@20251101` (the undated id is the one that exists).

**A scenario adopted with a `git_ref` fails at argument parsing** on any loop image older than
`7b59a44`. The controller has sent `--repo-ref` since 2026-08-02 and no engine accepted it.
Adopt without one until prod carries the new image.

## Live bugs found by the audit, not yet fixed

An audit of the controller after linking found 21 sites rebuilding engine meaning from text.
Two are wrong today:

- `validate_params` builds every jsonschema instance as a string, so a pack declaring an
  integer, number, boolean, or array parameter cannot be launched through the controller, even
  though the engine's binder accepts the same text.
- `task_evidence::session_result` never reads a `task_result`'s status and serves whichever line
  came last, so a failing attempt after a passing one reports as passing.

Tracked as WI-2026-08-25-002 in the controller repo, gated on WI-2026-08-25-005 here.

## Work items

Here: 001 launch an existing pack without a scope turn (queue), 002 `--repo-ref` (active, one
criterion left: prod on a loop image built from the pin), 003 linked renders (done), 004
reachable-model catalog (queue), 005 expose the types the controller must not mirror (queue),
006 carry a brokered measure through the turn render (queue).

Controller: 001 link the renders and check the contract version (active, #459), 002 decode
engine outcomes from the linked types (queue), 003 re-pin the Vertex region (queue).

006 is worth understanding: the controller has also been sending `--broker-measure`, which no
engine ever accepted, so brokered scope turns have been failing silently. Linking turned that
into a typed refusal at dispatch. The engine needs the field before the refusal can go away.

## To finish the demo, in order

1. Merge #459 once `build-and-test` clears, rebasing again if main moved.
2. Wait for the `crucible-loop` image built from that merge; take its digest.
3. One controller values PR: bump the turn profile's `loop` pin to that digest **and** re-pin
   `CLOUD_ML_REGION` and `vertex.region` to `us-east5` (both, or the region change is half
   applied). That closes WI-003 there and the last criterion of 002 here.
4. `scripts/roll-vertex-project.fish sync`, then `restart`, then `knobs`.
5. Port-forward 8899, then `scripts/roll-vertex-project.fish adopt`. It strips `git_ref` for
   you. Watch the scope turn through `/api/issues/{key}/scope-transcript`, which is the only
   place the sandbox's own stderr surfaces.

`scripts/roll-vertex-project.fish` takes `sa`, `secrets`, `sync`, `restart`, `knobs`,
`park <key>`, and `adopt`.

## Smaller things

- Local `#[sqlx::test]` runs need `DATABASE_URL=postgres://postgres:ci@localhost:55432/crucible`.
  Without it they fail in a way that reads exactly like a session-ingest regression. It cost an
  hour before the tests turned out to be fine.
- Linker errors saying nothing useful were a full disk. Each worktree carries its own `target`,
  and they run 8 to 83 GB. `.claude/worktrees` under this repo held another 25 GB.
- Prod's vault-sync CronJob was crash-looping on a rustls provider panic. Fixed on controller
  main by `1410728`, unrelated to this work.
