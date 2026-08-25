# selfhost — crucible optimizing crucible

The candidate is this repo. A real agent edits `crucible-harness/src/stream_json.rs` (the
agent-stream decoder every loop runs) and the gate measures it with
`crucible-harness/benches/stream_json.rs`: ns per input line over a deterministic synthetic
`stream-json` corpus (~145k lines, thinking/text deltas, chunked tool inputs, tool-result echoes,
retries, raw non-JSON lines). Lower wins.

Why this surface: pure CPU, sub-5s measure, ±0.5% run-to-run noise, and a leaf crate so the
agent's edits can't wander into engine plumbing.

## The gate can't be gamed

- The bench hashes every emitted `AgentEvent`; a changed stream reports `valid: false`.
- The bench and `measure.sh` are `frozen = true` injects, re-copied before every measure.
- `measure.sh` runs the crate's tests first; a red test is `valid: false`.
- `[judge.selftest]` proves the gate discriminates: `bad_cmd` decodes every line twice.

Re-bake the hash after an intentional event-shape change:

    cargo bench -p crucible-harness --bench stream_json -q -- --print-hash

## Run on a laptop

    crucible check --manifest examples/selfhost/crucible.toml
    crucible --manifest examples/selfhost/crucible.toml --agent-backend local --iterations 8

Needs `cargo` + `jq` + `claude` on PATH: `--agent-backend local` runs the agent here instead of
in the pack's sandbox image, and the measure runs here either way. The workspace is a fresh
clone of `[repo] url` at `git_ref`, so local commits must be pushed to be under test.
`just bench-stream` runs the bench by itself.

First proven run (2026-08-25, local backend, 3 iterations): 891 -> 224.7 -> 200.5 -> 176.6
ns/line, every iteration kept, $7.89, 32 min.

## Run through the controller

The playbook registry only takes `type = "playbook"` packs, so a scored loop enters through the
scenario path: adopt a scenario whose **authoritative** brief names this pack, the scope pod
copies it out of the checkout and validates it (`crucible check` + the gaming review), and the
run pod loops it with the controller's `run_iterations` / `run_max_cost` knobs.

    curl -sS -X POST "$CONTROLLER/api/scenarios" \
        -H "authorization: Bearer $TOKEN" -H "content-type: application/json" \
        -d @examples/selfhost/scenario.json

The run pod is the controller's pinned `crucible-loop` image (it ships rustup; crates.io is
reachable from the run namespace), the agent turn runs in this pack's `sandbox_image`, and a
private sandbox repo needs the deploy profile's `[secrets].pull_authfile` to cover it.

## Run in-cluster

Two images, both Rust-capable, because the agent turn and the measure run in different pods:

| Pod | Image | Recipe |
| --- | --- | --- |
| agent sandbox (OpenShell) | `ghcr.io/neuralmagic/crucible-sandbox-rust` | `Containerfile.sandbox-rust` |
| loop pod (engine + measure) | `ghcr.io/neuralmagic/crucible-rust` | `Containerfile.runtime-rust` |

Both pre-fetch the workspace's `Cargo.lock` deps (plus criterion/divan) so the first
`cargo bench` in a fresh pod compiles offline. Set `backend = "openshell"`, `sandbox_image`,
and point the loop pod at the `crucible-rust` image in the deploy profile.

Dev builds (private quay repos, need a pull secret in the namespace):

    quay.io/wseaton/crucible-sandbox-rust@sha256:031c9dc4f513b5ac6517ec617825d39ca7ff4d79e0f8bc60f9211edd4d529b95
    quay.io/wseaton/crucible-rust@sha256:59dd35b0b636f2df70ec2ca513745946cb31cc282b000ca62ac3ee6f53232366

How to build these (and images for any other domain): `docs/domain-images.md`.
