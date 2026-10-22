//! What one agent turn hands back, and the stream pump that folds its events into that.

use crate::agent::event::{AgentEvent, RawStream, Tokens, cost_of};
use crate::agent::harness::StreamDecoder;
use crate::agent::tool_chain::{StopReason, ToolChain};
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
    /// A `[[agent.tool_plugins]]` guard stopped the turn.
    #[error(transparent)]
    Stopped(StopReason),
}

impl TurnFailure {
    /// What the failure counts as on a task result.
    pub fn transport_cause(&self) -> TransportCause {
        match self {
            TurnFailure::Spawn(_) => TransportCause::Agent,
            TurnFailure::Orchestration { cause, .. } => *cause,
            TurnFailure::DeadlineExceeded(_) => TransportCause::Agent,
            TurnFailure::Stopped(_) => TransportCause::Agent,
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

/// What a drained agent stream amounted to: its cost signals, and the tool chain stop that cut it
/// short.
pub(crate) struct PumpEnd {
    pub(crate) cost: f64,
    pub(crate) best_tokens: Option<Tokens>,
    pub(crate) stopped: Option<StopReason>,
}

/// The decoder-driving core of an agent stdout pump: one [`StreamDecoder`] plus the
/// turn's running (max authoritative cost, largest token sample). Pure and sync, no I/O. Each
/// complete stdout line is [`push`](StreamPump::push)ed in; the local-child path feeds it off a
/// `BufReader` ([`pump_stream`]), the openshell exec path feeds it lines straight off the
/// gRPC stream. Splitting the loop from the byte source is what lets the async exec path reuse the
/// exact same accounting + sink dispatch from any line source (BufReader or gRPC stream).
pub(crate) struct StreamPump {
    decoder: Box<dyn StreamDecoder>,
    chain_decoder: Option<Box<dyn StreamDecoder>>,
    cost: f64,
    best_tokens: Option<Tokens>,
    chain: ToolChain,
}

impl StreamPump {
    /// A fresh pump over the harness's `decoder` (see `Backend::decoder`), its events run through
    /// `chain`. `chain_decoder` decodes the same lines with full tool IO for the chain when
    /// `decoder` is compact; `None` hands the chain `decoder`'s own events.
    pub(crate) fn new(
        decoder: Box<dyn StreamDecoder>,
        chain_decoder: Option<Box<dyn StreamDecoder>>,
        chain: ToolChain,
    ) -> Self {
        Self {
            decoder,
            chain_decoder,
            cost: 0.0,
            best_tokens: None,
            chain,
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
        let events = self.decoder.push(line);
        for ev in &events {
            account(ev, &mut self.cost, &mut self.best_tokens);
            if json {
                sink(line, RawStream::Stdout, Some(ev));
            } else if let Some(human) = human_line(ev) {
                sink(&human, RawStream::Stdout, Some(ev));
            }
        }
        let chain_events = match &mut self.chain_decoder {
            Some(decoder) => decoder.push(line),
            None => events,
        };
        for ev in &chain_events {
            if let Some(reason) = self.chain.observe(ev) {
                emit(&stop_event(reason), sink);
            }
        }
    }

    /// The tool chain stop this stream has hit, if any; the caller ends the agent on it.
    pub(crate) fn stopped(&self) -> Option<&StopReason> {
        self.chain.stopped()
    }

    /// The turn's max authoritative cost, largest token sample, and chain stop once the stream
    /// ends, after the chain's end-of-turn reports reach `sink`.
    pub(crate) fn finish(
        self,
        sink: &mut impl FnMut(&str, RawStream, Option<&AgentEvent>),
    ) -> PumpEnd {
        report_chain(&self.chain, sink);
        PumpEnd {
            cost: self.cost,
            best_tokens: self.best_tokens,
            stopped: self.chain.into_stopped(),
        }
    }
}

/// Send each of `chain`'s end-of-turn reports to `sink` as a `Log` event.
pub(crate) fn report_chain(
    chain: &ToolChain,
    sink: &mut impl FnMut(&str, RawStream, Option<&AgentEvent>),
) {
    for (label, value) in chain.reports() {
        tracing::info!(label, report = %value, "tool chain report");
        emit(
            &AgentEvent::Log {
                level: "info".into(),
                label: label.into(),
                value: Some(value.to_string()),
            },
            sink,
        );
    }
}

fn emit(ev: &AgentEvent, sink: &mut impl FnMut(&str, RawStream, Option<&AgentEvent>)) {
    let human = human_line(ev).unwrap_or_default();
    sink(&human, RawStream::Stderr, Some(ev));
}

/// The run-log event for a tool chain stop.
pub(crate) fn stop_event(reason: &StopReason) -> AgentEvent {
    AgentEvent::Error {
        error_type: "tool_plugin".into(),
        message: reason.to_string(),
    }
}

/// The turn a chain stop leaves, or a completed one.
pub(crate) fn ended_turn(cost: f64, stopped: Option<StopReason>) -> TurnOutcome {
    match stopped {
        Some(reason) => TurnOutcome::failed(cost, TurnFailure::Stopped(reason)),
        None => TurnOutcome::completed(cost),
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
        AgentEvent::Log {
            label,
            value: Some(value),
            ..
        } if label.starts_with("tool_") => Some(format!("\u{1f9e9} {label}: {value}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::tool_chain::{StopReason, ToolChain, Wedged};
    use crate::agent::turn::{PumpEnd, StreamPump};
    use crate::manifest::ToolPluginSpec;
    use crucible_contract::event::{AgentEvent, RawStream};
    use crucible_harness::StreamJsonParser;
    use std::num::NonZeroUsize;

    /// One Claude `stream-json` tool call, and its result when `result` is given, as the CLI emits them.
    fn claude_call(id: usize, command: &str, result: Option<(&str, bool)>) -> Vec<String> {
        let input = serde_json::json!({ "command": command }).to_string();
        let mut lines = vec![
            serde_json::json!({"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","id":format!("toolu_{id}"),"name":"Bash"}}}).to_string(),
            serde_json::json!({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":input}}}).to_string(),
            serde_json::json!({"type":"stream_event","event":{"type":"content_block_stop"}}).to_string(),
        ];
        if let Some((text, is_error)) = result {
            lines.push(serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":format!("toolu_{id}"),"is_error":is_error,"content":text}]}}).to_string());
        }
        lines
    }

    fn limit(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    /// Drives `lines` through the real Claude decoder and a pump, stopping at the first wedge.
    fn pump(lines: &[String], tool_io: bool, n: usize) -> (PumpEnd, usize, Vec<AgentEvent>) {
        let decoder = Box::new(StreamJsonParser::default().with_tool_io(tool_io));
        let mut pump = StreamPump::new(decoder, None, guard(n));
        let mut events = Vec::new();
        let mut read = 0;
        for line in lines {
            pump.push(
                line,
                true,
                &mut |_l: &str, _s: RawStream, ev: Option<&AgentEvent>| events.extend(ev.cloned()),
            );
            read += 1;
            if pump.stopped().is_some() {
                break;
            }
        }
        let end = pump.finish(&mut |_l: &str, _s: RawStream, ev: Option<&AgentEvent>| {
            events.extend(ev.cloned())
        });
        (end, read, events)
    }

    fn guard(n: usize) -> ToolChain {
        ToolChain::new(&[ToolPluginSpec::RepeatGuard { limit: limit(n) }])
    }

    fn wedged(end: PumpEnd) -> Option<Wedged> {
        end.stopped.map(|StopReason::Wedged(wedged)| wedged)
    }

    #[test]
    fn the_chain_reads_full_results_while_the_run_log_stays_compact() {
        let polls: Vec<String> = (0..40)
            .flat_map(|i| {
                let status = format!("{}% built", i * 2);
                claude_call(i, "buildit status b-1", Some((status.as_str(), false)))
            })
            .collect();
        let run = |lines: &[String]| {
            let mut pump = StreamPump::new(
                Box::new(StreamJsonParser::default()),
                Some(Box::new(StreamJsonParser::default().with_tool_io(true))),
                ToolChain::new(&crate::manifest::default_chain()),
            );
            let mut logged = Vec::new();
            let mut sink =
                |_l: &str, _s: RawStream, ev: Option<&AgentEvent>| logged.extend(ev.cloned());
            for line in lines {
                pump.push(line, true, &mut sink);
            }
            (pump.finish(&mut sink), logged)
        };
        let (end, logged) = run(&polls);
        assert!(
            end.stopped.is_none(),
            "a poll whose answer changes is progress"
        );
        assert!(
            logged.iter().all(|ev| !matches!(
                ev,
                AgentEvent::Tool {
                    result: Some(_),
                    ..
                }
            )),
            "the compact log carries no tool results"
        );
        let same: Vec<String> = (0..40)
            .flat_map(|i| claude_call(i, "buildit status b-1", Some(("0% built", false))))
            .collect();
        assert!(matches!(run(&same).0.stopped, Some(StopReason::Wedged(_))));
    }

    #[test]
    fn the_same_command_issued_limit_times_in_a_row_wedges_the_turn() {
        let command = "go list -deps ./cmd/server 2>&1 | tail -2";
        let lines: Vec<String> = (0..40)
            .flat_map(|i| claude_call(i, command, None))
            .collect();
        let (end, read, events) = pump(&lines, false, 25);
        assert_eq!(
            wedged(end),
            Some(Wedged {
                calls: vec![format!("Bash `$ {command}`")],
                repeats: 25,
            })
        );
        assert_eq!(read, 25 * 3, "the pump stops at the 25th call");
        assert!(events.iter().any(|ev| matches!(
            ev,
            AgentEvent::Error { error_type, message }
                if error_type == "tool_plugin" && message.contains("repeated 25 times in a row")
        )));
    }

    #[test]
    fn one_call_short_of_the_limit_is_not_a_wedge() {
        let lines: Vec<String> = (0..24).flat_map(|i| claude_call(i, "make", None)).collect();
        assert_eq!(wedged(pump(&lines, false, 25).0), None);
    }

    #[test]
    fn a_different_call_restarts_the_count() {
        let mut lines: Vec<String> = (0..20).flat_map(|i| claude_call(i, "make", None)).collect();
        lines.extend(claude_call(20, "make test", None));
        lines.extend((21..41).flat_map(|i| claude_call(i, "make", None)));
        assert_eq!(wedged(pump(&lines, false, 25).0), None);
    }

    #[test]
    fn a_command_that_fails_the_same_way_every_time_wedges_through_its_failure_lines() {
        let lines: Vec<String> = (0..30)
            .flat_map(|i| claude_call(i, "./run.sh", Some(("permission denied", true))))
            .collect();
        let (end, _, _) = pump(&lines, false, 25);
        let wedged = wedged(end).unwrap();
        assert_eq!(
            wedged.calls,
            ["Bash `$ ./run.sh`"],
            "the call, not its failure line, is reported"
        );
    }

    #[test]
    fn polling_whose_answer_changes_is_progress_under_full_tool_io() {
        let lines: Vec<String> = (0..60)
            .flat_map(|i| {
                let status = format!("build {}% done", i * 100 / 60);
                claude_call(i, "buildit status b-1", Some((status.as_str(), false)))
            })
            .collect();
        assert_eq!(wedged(pump(&lines, true, 25).0), None);
    }

    #[test]
    fn polling_that_gets_the_same_answer_every_time_is_a_wedge_under_full_tool_io() {
        let lines: Vec<String> = (0..30)
            .flat_map(|i| claude_call(i, "cat /tmp/deps.json", Some(("{}", false))))
            .collect();
        assert!(wedged(pump(&lines, true, 25).0).is_some());
    }

    #[test]
    fn a_wedge_is_reported_once() {
        let lines: Vec<String> = (0..80).flat_map(|i| claude_call(i, "ls", None)).collect();
        let decoder = Box::new(StreamJsonParser::default());
        let mut pump = StreamPump::new(decoder, None, guard(3));
        let mut errors = 0;
        for line in &lines {
            pump.push(
                line,
                true,
                &mut |_l: &str, _s: RawStream, ev: Option<&AgentEvent>| {
                    errors += usize::from(matches!(ev, Some(AgentEvent::Error { .. })));
                },
            );
        }
        assert_eq!(errors, 1);
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
            failed: false,
        };
        let line = crate::agent::turn::human_line(&ev).unwrap();
        assert_eq!(
            line,
            "\u{1f527} mcp__buildit__run failed: push to https://***@quay.io/ refused, GH_TOKEN=***"
        );
    }
}
