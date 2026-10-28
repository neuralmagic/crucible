//! What one agent turn hands back, and the stream pump that folds its events into that.

use crate::agent::event::{AgentEvent, Pricing, RawStream, Tokens, cost_of};
use crate::agent::harness::StreamDecoder;
use crate::args::Args;
use crucible_contract::TransportCause;

/// How a turn ended when it did not complete: the agent never produced output because the
/// transport itself failed. Distinct from a turn that ran and answered badly.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TurnFailure {
    /// The local transport could not be launched (bad argv, missing binary, harness error).
    #[error("agent spawn failed: {0}")]
    Spawn(String),
    /// The openshell driver's multi-step flow failed; `cause` says at which step.
    #[error("openshell orchestration failed: {message}")]
    Orchestration {
        cause: TransportCause,
        message: String,
    },
    /// The turn was killed at its deadline. The agent ran; it ran out of time.
    #[error("{}", .0.note())]
    DeadlineExceeded(crucible::deadline::Deadline),
}

impl TurnFailure {
    /// What the failure counts as on a task result.
    pub fn transport_cause(&self) -> TransportCause {
        match self {
            TurnFailure::Spawn(_) => TransportCause::Agent,
            TurnFailure::Orchestration { cause, .. } => *cause,
            TurnFailure::DeadlineExceeded(_) => TransportCause::Agent,
        }
    }
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
/// turn's running [`TurnCost`]. Pure and sync, no I/O. Each
/// complete stdout line is [`push`](StreamPump::push)ed in; the local-child path feeds it off a
/// `BufReader` ([`pump_stream`]), the openshell exec path feeds it lines straight off the
/// gRPC stream. Splitting the loop from the byte source is what lets the async exec path reuse the
/// exact same accounting + sink dispatch from any line source (BufReader or gRPC stream).
pub(crate) struct StreamPump {
    decoder: Box<dyn StreamDecoder>,
    cost: TurnCost,
}

impl StreamPump {
    /// A fresh pump over the harness's `decoder` (see `Backend::decoder`).
    pub(crate) fn new(decoder: Box<dyn StreamDecoder>, pricing: Pricing) -> Self {
        Self {
            decoder,
            cost: TurnCost::new(pricing),
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
            self.cost.observe(&ev);
            if json {
                sink(line, RawStream::Stdout, Some(&ev));
            } else if let Some(human) = human_line(&ev) {
                sink(&human, RawStream::Stdout, Some(&ev));
            }
        }
    }

    pub(crate) fn finish(self) -> TurnCost {
        self.cost
    }
}

/// One turn's running cost: the highest cost its harness reported and its largest token sample,
/// settled under the turn's [`Pricing`].
#[derive(Debug, Clone)]
pub(crate) struct TurnCost {
    pricing: Pricing,
    reported: f64,
    tokens: Option<Tokens>,
}

impl TurnCost {
    pub(crate) fn new(pricing: Pricing) -> Self {
        Self {
            pricing,
            reported: 0.0,
            tokens: None,
        }
    }

    pub(crate) fn observe(&mut self, ev: &AgentEvent) {
        if let Some(c) = cost_of(ev) {
            self.report(c);
        }
        if let AgentEvent::Tokens(t) = ev
            && self.tokens.as_ref().is_none_or(|b| t.total >= b.total)
        {
            self.tokens = Some(t.clone());
        }
    }

    /// A cost the harness reported outside its event stream.
    pub(crate) fn report(&mut self, usd: f64) {
        self.reported = self.reported.max(usd);
    }

    pub(crate) fn total(&self) -> f64 {
        self.pricing.settle(self.reported, self.tokens.as_ref())
    }
}

/// Render observable activity and failed results for the headless console.
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
            let summary = if crate::turn_trace::redact_enabled() {
                crate::turn_trace::redact(summary)
            } else {
                summary.clone()
            };
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
        AgentEvent::Result {
            is_error: true,
            subtype,
            error,
            ..
        } => Some(format!(
            "\u{274c} Error: {subtype}: {}",
            error.as_deref().unwrap_or("agent reported a failed turn"),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::event::{Pricing, Tokens};
    use crate::agent::turn::TurnCost;
    use crucible_contract::event::AgentEvent;
    use crucible_contract::inference::{TokenPrice, UsdPerMtok};

    #[test]
    fn a_failed_result_is_visible_without_a_separate_error_event() {
        let event = AgentEvent::Result {
            subtype: "error_during_execution".into(),
            is_error: true,
            turns: 0,
            cost_usd: 0.0,
            error: Some("Prompt too long".into()),
        };
        assert!(
            crate::agent::turn::human_line(&event)
                .unwrap()
                .contains("Prompt too long")
        );
        assert!(crate::agent::turn::human_line(&result(0.0)).is_none());
    }

    fn result(cost_usd: f64) -> AgentEvent {
        AgentEvent::Result {
            subtype: "success".into(),
            is_error: false,
            turns: 4,
            cost_usd,
            error: None,
        }
    }

    fn tokens(input: u64, output: u64, cache_read: u64, cache_write: u64) -> AgentEvent {
        AgentEvent::Tokens(Tokens {
            input,
            output,
            cache_read,
            cache_write,
            total: input + output + cache_read + cache_write,
            rate: None,
            cost_usd: None,
        })
    }

    fn settle(pricing: Pricing, events: &[AgentEvent]) -> f64 {
        let mut cost = TurnCost::new(pricing);
        for ev in events {
            cost.observe(ev);
        }
        cost.total()
    }

    #[test]
    fn a_self_hosted_turn_reporting_25_dollars_settles_at_zero() {
        let events = [tokens(3_000_000, 400_000, 9_000_000, 200_000), result(25.0)];
        let pricing = Pricing::resolve("zai-org/glm-5", None);
        assert_eq!(settle(pricing, &events), 0.0);
        let mut backfilled = TurnCost::new(pricing);
        backfilled.report(25.0);
        assert_eq!(backfilled.total(), 0.0);
    }

    #[test]
    fn a_vendor_turn_keeps_the_cost_its_harness_reported() {
        let events = [tokens(3_000_000, 400_000, 9_000_000, 200_000), result(25.0)];
        assert_eq!(
            settle(Pricing::resolve("claude-opus-4-6", None), &events),
            25.0
        );
        let estimated = settle(
            Pricing::resolve("claude-opus-4-6", None),
            &[tokens(1_000_000, 0, 0, 0)],
        );
        assert!((estimated - 5.0).abs() < 1e-9, "{estimated}");
    }

    #[test]
    fn a_priced_binding_settles_at_its_largest_token_sample_times_its_price() {
        let usd = |v| UsdPerMtok::new(v).unwrap();
        let price = TokenPrice {
            input: usd(1.0),
            output: usd(4.0),
            cache_read: usd(0.1),
            cache_write: usd(2.0),
        };
        let events = [
            tokens(1_000_000, 100_000, 0, 0),
            tokens(3_000_000, 400_000, 9_000_000, 200_000),
            result(25.0),
        ];
        let want = 3.0 * 1.0 + 0.4 * 4.0 + 9.0 * 0.1 + 0.2 * 2.0;
        for model in ["zai-org/glm-5", "claude-opus-4-6"] {
            let got = settle(Pricing::resolve(model, Some(&price)), &events);
            assert!((got - want).abs() < 1e-9, "{model}: got {got}, want {want}");
        }
    }

    #[test]
    fn tool_lines_scrub_credentials_from_the_summary() {
        let ev = AgentEvent::Tool {
            name: "mcp__buildit__run".to_string(),
            summary: "failed: push to https://bot:hunter2@quay.io refused, GH_TOKEN=abc123"
                .to_string(),
            subagent: false,
            input: None,
            result: None,
        };
        let line = crate::agent::turn::human_line(&ev).unwrap();
        assert_eq!(
            line,
            "\u{1f527} mcp__buildit__run failed: push to https://***@quay.io/ refused, GH_TOKEN=***"
        );
    }
}
