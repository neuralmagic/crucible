//! `crucible flow`: post-hoc run explainability. Folds a finished run's `session.jsonl`
//! (required) plus a Datadog APM span export (optional, from a `--spans` file or fetched
//! live with `--dd-trace`, see `flow_dd`) into a small renderer-agnostic
//! flow model, then emits it as JSON, Graphviz dot, or mermaid — the output format is
//! picked by the `--out` extension.
//!
//! The JSON IR is the contract: an HTML (or any other) renderer needs nothing beyond
//! `flow.json`, so extraction and emission never mix. The session log alone covers
//! decisions, scores, edits, rungs, budget and publish; the span export adds wall-clock
//! (iteration windows, turn durations, the agent's per-tool-call timeline, rung
//! durations). Wide-round and infra rows are out of scope: this is the deep-loop
//! overview a human is walked through.

pub mod html;
pub mod model;

use crate::flow::model::{FlowError, build_model, emit_dot, emit_mermaid};
use anyhow::Result;

/// The inputs of one flow render: the run's session log and, when the caller has one, the
/// Datadog span export for the same run (spans API v2 objects, a JSON array or `{"data": [...]}`).
pub struct FlowInput {
    pub session_log: String,
    pub spans_json: Option<String>,
}

/// The output document format. The CLI picks it from the `--out` extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowFormat {
    /// The IR (`flow.json`).
    Json,
    /// Graphviz dot.
    Dot,
    /// Mermaid.
    Mermaid,
    /// The self-contained explainer page.
    Html,
}

impl FlowFormat {
    /// The format an output path's extension selects.
    pub fn from_extension(ext: &str) -> Result<Self, FlowError> {
        match ext {
            "json" => Ok(FlowFormat::Json),
            "dot" => Ok(FlowFormat::Dot),
            "mmd" => Ok(FlowFormat::Mermaid),
            "html" => Ok(FlowFormat::Html),
            other => Err(FlowError::UnknownFormat { ext: other.into() }),
        }
    }
}

/// Render the flow document: the library form of `crucible flow`, which reads `--session` and
/// `--spans` (or fetches `--dd-trace`) into a [`FlowInput`] and writes the result to `--out`.
/// Pure: nothing beyond `input` is read.
#[tracing::instrument(skip_all, fields(format = ?format), err)]
pub fn render(input: &FlowInput, format: FlowFormat) -> Result<String> {
    let model = build_model(&input.session_log, input.spans_json.as_deref())?;
    Ok(match format {
        FlowFormat::Json => {
            let mut s = serde_json::to_string_pretty(&model).map_err(FlowError::Model)?;
            s.push('\n');
            s
        }
        FlowFormat::Dot => emit_dot(&model),
        FlowFormat::Mermaid => emit_mermaid(&model),
        FlowFormat::Html => crate::flow::html::emit_html(&model),
    })
}

/// Fold a session log into the rendered report pair (`flow.json`, `flow.html`) with no span
/// export — publish runs on the loop pod, which has no Datadog creds, so the page uses its
/// no-spans fallback.
pub fn render_report(session_log: &str) -> Result<(String, String)> {
    let model = build_model(session_log, None)?;
    let mut json = serde_json::to_string_pretty(&model).map_err(FlowError::Model)?;
    json.push('\n');
    Ok((json, crate::flow::html::emit_html(&model)))
}
