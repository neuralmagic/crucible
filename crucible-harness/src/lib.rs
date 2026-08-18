//! The crucible agent harness: everything that runs next to an agent.
//!
//! - [`stream_json`] decodes Claude Code's `--output-format stream-json` into
//!   [`crucible_contract::event::AgentEvent`] NDJSON.
//! - [`otel`] is the in-process OTLP http/json collector: telemetry capture, live token rate,
//!   and the `otel_summary` / `usage` rollup.

pub mod otel;
pub mod stream_json;

pub use otel::{
    CostHandle, LiveMeters, OtelCollector, OtelForward, OtelSummary, RateHandle, build_summary,
    otel_env,
};
pub use stream_json::StreamJsonParser;
