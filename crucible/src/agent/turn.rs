//! What one agent turn hands back, and the stream pump that folds its events into that.

use crate::agent::event::{AgentEvent, RawStream, Tokens, cost_of};
use crate::agent::harness::StreamDecoder;
use crate::args::Args;

/// How a turn ended when it did not complete: the agent never produced output because the
/// transport itself failed. Distinct from a turn that ran and answered badly.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TurnFailure {
    /// The local transport could not be launched (bad argv, missing binary, harness error).
    #[error("agent spawn failed: {0}")]
    Spawn(String),
    /// The openshell driver's multi-step flow failed (gateway, provider, sandbox, exec, transfer).
    #[error("openshell orchestration failed: {0}")]
    Orchestration(String),
}

/// One turn's result: what it cost, and whether it ran at all. `failure` is `Some` when the turn
/// never completed; `cost_usd` still carries whatever the turn spent before it broke, so a partial
/// turn is not silently free.
#[derive(Clone, Debug, PartialEq)]
pub struct TurnOutcome {
    pub cost_usd: f64,
    pub failure: Option<TurnFailure>,
}

impl TurnOutcome {
    /// A turn that ran to completion at `cost_usd`. Says nothing about the quality of its output.
    pub fn completed(cost_usd: f64) -> Self {
        Self {
            cost_usd,
            failure: None,
        }
    }

    /// A turn that broke at `failure` after spending `cost_usd`.
    pub fn failed(cost_usd: f64, failure: TurnFailure) -> Self {
        Self {
            cost_usd,
            failure: Some(failure),
        }
    }

    pub fn failure(&self) -> Option<&TurnFailure> {
        self.failure.as_ref()
    }
}

/// Whether the in-process OTLP collector should run for this turn. Opt-in ("result bundling": "result
/// mode is opt-in exactly like `--marker`"), keyed on `CRUCIBLE_OTEL` being truthy in the
/// manifest's `[agent].env` or the process environment. Off by default keeps a local run's behavior
/// byte-identical to today (pricing-table estimate). Promoting this to a first-class manifest field
/// is a follow-up.
pub(crate) fn otel_enabled(args: &Args) -> bool {
    let truthy = |v: &str| matches!(v.trim(), "1" | "true" | "yes" | "on");
    if let Some((_, v)) = args.env.iter().find(|(k, _)| k == "CRUCIBLE_OTEL") {
        return truthy(v);
    }
    std::env::var("CRUCIBLE_OTEL")
        .map(|v| truthy(&v))
        .unwrap_or(false)
}

/// The upstream OTLP/HTTP receiver the collector mirrors this turn's agent telemetry to, named by
/// `CRUCIBLE_OTEL_FORWARD` in the manifest's `[agent].env` or the process environment. Unset keeps
/// the collector a terminal sink (capture stays in `otel.jsonl`).
///
/// Spans are re-parented onto the turn span before they leave, so the agent's `llm_request` spans
/// nest under the run instead of forming an orphan trace: the agent's exporter cannot adopt a
/// parent itself, since the OTel JS SDK does not read `TRACEPARENT` from the environment.
pub(crate) fn otel_forward(args: &Args) -> Option<crucible_harness::OtelForward> {
    let endpoint = match args.env.iter().find(|(k, _)| k == "CRUCIBLE_OTEL_FORWARD") {
        Some((_, v)) => v.clone(),
        None => std::env::var("CRUCIBLE_OTEL_FORWARD").ok()?,
    };
    if endpoint.trim().is_empty() {
        return None;
    }
    let tp = crate::agent::engine::current_trace_env().map(|(tp, _)| tp);
    Some(crucible_harness::OtelForward::new(endpoint, tp.as_deref()))
}

/// Whether session-log tool events carry full inputs and result excerpts
/// (`CRUCIBLE_SESSION_TOOL_IO=full` in the manifest `[agent].env` or the process
/// env). Off by default: the compact name+summary form keeps the log small, but
/// made a run's edits unreconstructable without diffing the PR — this flag exists
/// for runs someone will want to review.
pub(crate) fn tool_io_full(args: &Args) -> bool {
    let full = |v: &str| v.trim().eq_ignore_ascii_case("full");
    if let Some((_, v)) = args
        .env
        .iter()
        .find(|(k, _)| k == "CRUCIBLE_SESSION_TOOL_IO")
    {
        return full(v);
    }
    std::env::var("CRUCIBLE_SESSION_TOOL_IO")
        .map(|v| full(&v))
        .unwrap_or(false)
}

/// The decoder-driving core of an agent stdout pump: one [`StreamDecoder`] plus the
/// turn's running (max authoritative cost, largest token sample). Pure and sync, no I/O. Each
/// complete stdout line is [`push`](StreamPump::push)ed in; the local-child path feeds it off a
/// `BufReader` ([`pump_stream`]), the openshell exec path feeds it lines straight off the
/// gRPC stream. Splitting the loop from the byte source is what lets the async exec path reuse the
/// exact same accounting + sink dispatch from any line source (BufReader or gRPC stream).
pub(crate) struct StreamPump {
    decoder: StreamDecoder,
    cost: f64,
    best_tokens: Option<Tokens>,
}

impl StreamPump {
    /// A fresh pump over the harness's `decoder` (see [`crate::manifest::Harness::decoder`]).
    pub(crate) fn new(decoder: StreamDecoder) -> Self {
        Self {
            decoder,
            cost: 0.0,
            best_tokens: None,
        }
    }

    /// Feed one complete stdout line: decode it into [`AgentEvent`]s, fold each into the
    /// running totals, and drive `sink`. `json` matches the front-end mode, `true` consumers read
    /// the event, `false` (console) prints a human line.
    pub(crate) fn push(
        &mut self,
        line: &str,
        json: bool,
        sink: &mut impl FnMut(&str, RawStream, Option<&AgentEvent>),
    ) {
        for ev in self.decoder.push(line) {
            account(&ev, &mut self.cost, &mut self.best_tokens);
            if json {
                sink(line, RawStream::Stdout, Some(&ev));
            } else if let Some(human) = human_line(&ev) {
                sink(&human, RawStream::Stdout, Some(&ev));
            }
        }
    }

    /// The turn's (max authoritative cost, largest token sample) once the stream ends.
    pub(crate) fn finish(self) -> (f64, Option<Tokens>) {
        (self.cost, self.best_tokens)
    }
}

/// Fold one event into the turn's running totals: the highest authoritative cost seen,
/// and the largest token sample (the estimate fallback when no cost is reported).
pub(crate) fn account(ev: &AgentEvent, cost: &mut f64, best_tokens: &mut Option<Tokens>) {
    if let Some(c) = cost_of(ev) {
        *cost = cost.max(c);
    }
    if let AgentEvent::Tokens(t) = ev
        && best_tokens.as_ref().is_none_or(|b| t.total >= b.total)
    {
        *best_tokens = Some(t.clone());
    }
}

/// Render an event as a human-readable line for the headless console. `None` for events that stay quiet
/// in a log (init/result/lifecycle); token/tool/text/thinking/retry/error show.
pub(crate) fn human_line(ev: &AgentEvent) -> Option<String> {
    match ev {
        AgentEvent::Text { delta } => Some(delta.clone()),
        AgentEvent::Thinking { delta } => Some(format!("\u{1f9e0} {delta}")),
        AgentEvent::Tool {
            name,
            summary,
            subagent,
            ..
        } => {
            let icon = if *subagent { "\u{1f916}" } else { "\u{1f527}" };
            Some(format!("{icon} {name} {summary}").trim_end().to_string())
        }
        AgentEvent::Tokens(t) => Some(format!(
            "\u{1f4ca} TOKENS in={} out={} cache_r={} cache_w={} total={}",
            t.input, t.output, t.cache_read, t.cache_write, t.total
        )),
        AgentEvent::Retry {
            attempt,
            max,
            error,
        } => Some(format!("\u{1f504} Retry {attempt}/{max} {error}")),
        AgentEvent::Error {
            error_type,
            message,
        } => Some(format!("\u{274c} Error: {error_type}: {message}")),
        _ => None,
    }
}
