You are optimizing `StreamJsonParser` in `crucible-harness/src/stream_json.rs`, the decoder that
turns Claude Code's `--output-format stream-json` NDJSON into `AgentEvent`s. The gate is
`./measure.sh`: it runs the crate's tests, then `cargo bench -p crucible-harness --bench stream_json`,
whose score is nanoseconds per input line over a fixed synthetic corpus (lower is better).

Rules of the gate, so you do not waste turns:

- The bench hashes every emitted event (serialized with serde). A candidate whose event stream
  differs from the baseline's is INVALID, not slow. Behaviour is fixed; only cost may move.
- `crucible-harness/benches/stream_json.rs` and `measure.sh` are frozen: edits to them are
  overwritten before every measure.
- Tests and `cargo clippy -p crucible-harness --all-targets -- -D warnings` must pass. You may add
  tests. Do not delete tests.
- Keep the public API (`StreamJsonParser::{default, with_meters, with_tool_io, push, flush}`).
- No `unwrap`/`expect`/indexing panics in non-test code.
- You may add a dependency (e.g. a faster JSON path) if it is well maintained; keep `Cargo.lock`
  consistent (`cargo update -p <crate>` not a bare `cargo update`).

Where the time goes today, roughly: every line is parsed into a `serde_json::Value` tree, even
the ones the parser then discards (`assistant` echoes, `rate_limit_event`); string fields are
copied out of the tree; text deltas are pushed through a `String` and re-scanned for newlines;
tool input JSON is accumulated then re-parsed. Profile before guessing:

    cargo bench -p crucible-harness --bench stream_json            # the score
    cargo test -p crucible-harness                                 # the gate's other half
    samply record ./target/release/deps/stream_json-*             # if samply is installed

{{GOAL}}

One focused change per turn. Read the score; a regression is rolled back for you.

Current status: {{STATUS}}
{{STEER}}
