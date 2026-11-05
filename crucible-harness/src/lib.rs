//! The crucible agent harness: everything that runs next to an agent.
//!
//! - [`stream_json`] decodes Claude Code's `--output-format stream-json` into
//!   [`crucible_contract::event::AgentEvent`] NDJSON.
//! - [`codex_json`] decodes `codex exec --json` into the same events.
//! - [`opencode_json`] decodes `opencode run --format json` into the same events.
//! - [`pi_json`] decodes `pi --mode json` into the same events.
//! - [`otel`] is the in-process OTLP http/json collector: telemetry capture, live token rate,
//!   and the `otel_summary` / `usage` rollup.
//! - [`redact`] masks the secret values a turn handed its agent.

pub mod codex_json;
pub mod line;
pub mod opencode_json;
pub mod otel;
pub mod pi_json;
pub mod redact;
pub mod stream_json;
pub mod tool_summary;
