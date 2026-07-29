//! The engine context: one multi-thread tokio runtime, created once at engine start, whose
//! [`Handle`] every async call site reaches; plus the engine's own OTLP span exporter.
//!
//! [`EngineCtx::new`] publishes the runtime's `Handle` into a module-level `OnceLock` read back
//! with [`handle`]; `EngineCtx` still OWNS the runtime and the OTLP guard, so the runtime shuts
//! down and spans flush when the owning frame unwinds (the `Handle` clone does not keep it
//! alive). Set-once, read-only, a deliberate exception to the no-globals rule.
//!
//! Telemetry: when `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` / `OTEL_EXPORTER_OTLP_ENDPOINT` is set,
//! a `tracing-opentelemetry` layer exports crucible's own spans (`service.name=crucible`) with
//! W3C propagation; no endpoint = no layer installed at all, zero overhead, and deliberately no
//! stderr fmt layer either (the engine narrates through the sink). Unrelated to
//! [`crate::agent`]'s in-process `OtelCollector`, which RECEIVES the child agent's OTLP for cost
//! accounting, the collector's child-directed `OTEL_*` env must never touch the engine process,
//! and this exporter must never inherit it.

use anyhow::{Context, Result};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::runtime::{Handle, Runtime};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// The `service.name` every exported span carries.
const SERVICE_NAME: &str = "crucible";

/// The published engine runtime handle. Set once by [`EngineCtx::new`]; read by [`handle`]. A
/// `Handle` is a cheap clone that does not keep the runtime alive, the owning [`EngineCtx`] does.
static HANDLE: OnceLock<Handle> = OnceLock::new();

/// The installed tracer provider, published for [`flush`] (a best-effort flush before a
/// `std::process::exit` that would skip [`EngineCtx`]'s `Drop`). `None` when no OTLP endpoint is set.
static PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

/// The opt-in GenAI content-logs provider (see [`install_logs`]); `None` on the default path.
static LOGS_PROVIDER: OnceLock<SdkLoggerProvider> = OnceLock::new();

/// One engine-lifetime multi-thread tokio runtime plus the engine's OTLP span guard. Construct once
/// per process on the arm that needs async (the loop path, `scope`, `rank-grounded`, and the swept
/// controller/`fetch` calls); construction publishes the handle for [`handle`]. The autopilot
/// daemon is deliberately *not* built through this, it keeps its own runtime and its own
/// (`crucible-controller`) telemetry.
pub(crate) struct EngineCtx {
    // Field order is drop order: the telemetry guard drops first (flushing spans while the
    // runtime is still alive to drive the batch processor), then `_runtime`. Held, never read.
    _telemetry: EngineTelemetry,
    _runtime: Runtime,
}

impl EngineCtx {
    /// Build the engine runtime, publish its handle, and install the OTLP layer (when configured).
    /// `enable_all` turns on the I/O and time drivers the tonic channel and RPC timeouts need.
    pub(crate) fn new() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("building the engine tokio runtime")?;
        let _ = HANDLE.set(runtime.handle().clone());
        // Install within the runtime context so the OTLP batch span processor's background task
        // attaches to THIS runtime (the reason the exporter needs an engine-lifetime runtime).
        let telemetry = {
            let _guard = runtime.enter();
            EngineTelemetry::install()
        };
        Ok(Self {
            _telemetry: telemetry,
            _runtime: runtime,
        })
    }
}

/// The published engine runtime handle, for async call sites downstream of [`EngineCtx::new`]
/// (the openshell turn boundary, the swept controller/S3 calls). An error (never a panic) when
/// no `EngineCtx` has been constructed yet, so a caller can surface it cleanly.
pub(crate) fn handle() -> Result<&'static Handle> {
    HANDLE.get().context(
        "the engine runtime is not initialized (EngineCtx::new was not called before an async \
         call site) — this is an engine wiring bug",
    )
}

/// Best-effort span flush, for the run paths that end in `std::process::exit` (which skips
/// [`EngineCtx`]'s `Drop`). Bounded on a scratch thread so a hung collector cannot wedge exit;
/// a no-op when no exporter is installed.
pub(crate) fn flush() {
    let tracer = PROVIDER.get().cloned();
    let logs = LOGS_PROVIDER.get().cloned();
    if tracer.is_none() && logs.is_none() {
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Some(p) = tracer {
            let _ = p.force_flush();
        }
        if let Some(p) = logs {
            let _ = p.force_flush();
        }
        let _ = tx.send(());
    });
    let _ = rx.recv_timeout(Duration::from_secs(3));
}

/// The W3C env vars the controller's dispatches (loop run, scope turn, rank turn) inject; the
/// engine adopts them as its trace parent. Distinct from the `OTEL_*` exporter config, so they
/// never collide with it.
const TRACEPARENT_ENV: &str = "TRACEPARENT";
const TRACESTATE_ENV: &str = "TRACESTATE";

/// The controller-injected W3C parent from the process env, or `None` when `TRACEPARENT` is absent
/// (the normal local/uninstrumented invocation). A present-but-unparseable value is a
/// warn-and-ignore (never fail the work); the adopting span just roots itself.
fn dispatch_parent() -> Option<opentelemetry::Context> {
    let traceparent = std::env::var(TRACEPARENT_ENV).ok();
    let present = traceparent
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let parent = extract_parent(traceparent, std::env::var(TRACESTATE_ENV).ok());
    if parent.is_none() && present {
        tracing::warn!("TRACEPARENT is set but unparseable; the span roots itself");
    }
    parent
}

/// The long-lived `run` root span for a loop pod dispatched by the controller, parented to that
/// dispatch's span so Tempo shows one tree (controller → run → turn → RPCs). Returns `None`, and the
/// openshell turn spans root themselves independently, unless BOTH the engine's OTLP exporter is
/// installed AND the controller injected a parseable `TRACEPARENT`. Enter the returned span for the
/// life of the loop: the `openshell_turn` spans, created on the same thread, then nest under it
/// (wide-round turns run on their own threads and root themselves, no thread-local to inherit).
///
/// CONSUMER kind pairs with the controller dispatch's PRODUCER span, the async producer/consumer
/// edge Tempo's service-graph processor draws the controller → crucible link from.
pub(crate) fn run_span(workspace: &str, run_id: &str) -> Option<tracing::Span> {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    // Exporter off (the default) means the tracing macros are no-ops, nothing to parent.
    PROVIDER.get()?;
    let parent = dispatch_parent()?;
    let span = tracing::info_span!(
        "run",
        otel.kind = "consumer",
        workspace = workspace,
        run_id = run_id,
    );
    // Best-effort: a failed link just leaves the run span self-rooted, never fails the run.
    if let Err(e) = span.set_parent(parent) {
        tracing::warn!(error = %e, "linking the run span to the controller dispatch failed");
    }
    Some(span)
}

/// Which controller-dispatched agent turn is adopting the dispatch as its trace parent.
pub(crate) enum TurnSpanKind {
    /// `crucible scope --propose`, pairs with the controller's `dispatch_scope` PRODUCER.
    Scope,
    /// `crucible rank-grounded`, pairs with the controller's `dispatch_grounded_rank` PRODUCER.
    RankGrounded,
}

/// The CONSUMER root span for one controller-dispatched agent turn (scope-propose / grounded-rank),
/// parented to the controller's PRODUCER dispatch span exactly like [`run_span`], so the turn's
/// `openshell_turn` span nests under the dispatch instead of floating as an orphaned trace. Same
/// contract as [`run_span`]: `None` unless BOTH the OTLP exporter is installed AND a parseable
/// `TRACEPARENT` was injected; enter it on the thread that runs the turn, and close it before any
/// `process::exit` tail (which skips drops) so the OTLP layer can batch it.
pub(crate) fn turn_span(kind: TurnSpanKind, issue: Option<&str>) -> Option<tracing::Span> {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    // Exporter off (the default) means the tracing macros are no-ops, nothing to parent.
    PROVIDER.get()?;
    let parent = dispatch_parent()?;
    let issue = issue.unwrap_or("");
    let span = match kind {
        TurnSpanKind::Scope => {
            tracing::info_span!("scope", otel.kind = "consumer", issue = issue)
        }
        TurnSpanKind::RankGrounded => {
            tracing::info_span!("rank_grounded", otel.kind = "consumer", issue = issue)
        }
    };
    // Best-effort: a failed link just leaves the turn span self-rooted, never fails the turn.
    if let Err(e) = span.set_parent(parent) {
        tracing::warn!(error = %e, "linking the turn span to the controller dispatch failed");
    }
    Some(span)
}

/// Graft one child span per agent tool call under `parent` (the turn root span), reconstructed
/// from the parsed session transcript so a single Tempo waterfall shows turn → tool calls. Spans
/// carry the transcript's real start/end times (built straight on the SDK tracer, not the `tracing`
/// macros, which would stamp "now"), so the batch exporter ships them under the SAME trace as the
/// turn root. A no-op when the exporter is off or `parent` isn't a valid recording span, the
/// enrichment is telemetry, never a turn dependency.
///
/// `turn_end` closes any call the transcript left open (no `tool_result` before the turn ended);
/// that span is also marked errored, since an unfinished tool is a turn that was cut short.
pub(crate) fn synthesize_tool_spans(
    parent: &tracing::Span,
    invocations: &[crate::turn_trace::ToolInvocation],
    turn_end: std::time::SystemTime,
) {
    use opentelemetry::KeyValue;
    use opentelemetry::trace::{Span as _, TraceContextExt as _, Tracer as _};
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let Some(provider) = PROVIDER.get() else {
        return;
    };
    let cx = parent.context();
    if !cx.span().span_context().is_valid() {
        return;
    }
    let tracer = provider.tracer(SERVICE_NAME);
    for inv in invocations {
        let mut attrs = vec![KeyValue::new("tool.name", inv.name.clone())];
        if !inv.summary.is_empty() {
            attrs.push(KeyValue::new("tool.input", inv.summary.clone()));
        }
        let unfinished = inv.end.is_none();
        let errored = inv.error || unfinished;
        attrs.push(KeyValue::new("tool.error", errored));
        // Child timestamps come from the SANDBOX clock; `turn_end` is engine-clock. Clamp so skew
        // between the two can never mint a negative-duration span.
        let end = inv.end.unwrap_or(turn_end).max(inv.start);

        let mut builder = tracer
            .span_builder(inv.name.clone())
            .with_kind(opentelemetry::trace::SpanKind::Internal)
            .with_start_time(inv.start)
            .with_attributes(attrs);
        if errored {
            let msg = if unfinished {
                "no tool_result before turn end"
            } else {
                "tool reported an error"
            };
            builder = builder.with_status(opentelemetry::trace::Status::error(msg));
        }
        // `build_with_context` starts the span at `with_start_time`; end it at the result time.
        let mut span = tracer.build_with_context(builder, &cx);
        span.end_with_timestamp(end);
    }
}

/// What the engine exports for a turn, resolved from the installed OTLP providers. Spans and content
/// logs install independently (spans on a traces endpoint; content logs on the opt-in flag plus a
/// logs endpoint), so content-only is reachable via a logs-specific endpoint override, not linear.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TurnExport {
    Off,
    Spans,
    Content,
    SpansAndContent,
}

impl TurnExport {
    pub(crate) fn resolve() -> Self {
        match (PROVIDER.get().is_some(), LOGS_PROVIDER.get().is_some()) {
            (false, false) => Self::Off,
            (true, false) => Self::Spans,
            (false, true) => Self::Content,
            (true, true) => Self::SpansAndContent,
        }
    }

    /// Any exporter is listening, the gate before pulling the transcript back at all.
    pub(crate) fn emits_anything(self) -> bool {
        self != Self::Off
    }

    /// Spans installed: synthesize tool spans and write the per-turn traceparent.
    pub(crate) fn spans(self) -> bool {
        matches!(self, Self::Spans | Self::SpansAndContent)
    }

    /// Content-logs provider installed: emit the conversation records.
    pub(crate) fn content(self) -> bool {
        matches!(self, Self::Content | Self::SpansAndContent)
    }
}

/// Whether conversation-CONTENT export is requested (`CRUCIBLE_TURN_TRACE_CONTENT` truthy). Opt-in
/// because it reverses the deliberate span content strip (full prompts / completions / tool i/o).
fn content_logs_enabled() -> bool {
    matches!(
        std::env::var("CRUCIBLE_TURN_TRACE_CONTENT").ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("on")
    )
}

/// Emit the turn's conversation as OpenTelemetry GenAI log records correlated to `parent` by trace +
/// span id, so a logs backend can join the conversation to its trace. Best-effort enrichment, never
/// a turn dependency.
pub(crate) fn emit_conversation_logs(
    parent: &tracing::Span,
    records: &[crate::turn_trace::GenAiRecord],
) {
    use opentelemetry::logs::LoggerProvider as _;
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let Some(provider) = LOGS_PROVIDER.get() else {
        return;
    };
    if records.is_empty() {
        return;
    }
    let sc = parent.context().span().span_context().clone();
    if !sc.is_valid() {
        return;
    }
    let logger = provider.logger(SERVICE_NAME);
    emit_records(&logger, &sc, records);
}

/// The pure emission over a logger and a span context, split out so a test can drive it against an
/// in-memory exporter (real SDK path, no mock logger). Each record becomes one log record correlated
/// to `sc`; the transcript's last assistant turn is the `gen_ai.choice` completion.
fn emit_records<L>(
    logger: &L,
    sc: &opentelemetry::trace::SpanContext,
    records: &[crate::turn_trace::GenAiRecord],
) where
    L: opentelemetry::logs::Logger,
{
    use opentelemetry::logs::{LogRecord as _, Severity};

    let last_assistant = records
        .iter()
        .rposition(|r| matches!(r, crate::turn_trace::GenAiRecord::Assistant { .. }));
    for (i, rec) in records.iter().enumerate() {
        let mut lr = logger.create_log_record();
        lr.set_severity_number(Severity::Info);
        lr.set_trace_context(sc.trace_id(), sc.span_id(), Some(sc.trace_flags()));
        // Anthropic Claude is the only agent model behind these transcripts.
        lr.add_attribute("gen_ai.system", "anthropic");
        map_record(&mut lr, rec, Some(i) == last_assistant);
        logger.emit(lr);
    }
}

/// Set the gen_ai event name, body, and per-kind attributes on one log record. `is_final_assistant`
/// promotes an assistant record from `gen_ai.assistant.message` to the `gen_ai.choice` completion.
fn map_record<R>(lr: &mut R, rec: &crate::turn_trace::GenAiRecord, is_final_assistant: bool)
where
    R: opentelemetry::logs::LogRecord,
{
    use crate::turn_trace::GenAiRecord;
    use opentelemetry::logs::AnyValue;

    match rec {
        GenAiRecord::System { content } => {
            lr.set_event_name("gen_ai.system.message");
            lr.set_body(AnyValue::from(content.clone()));
        }
        GenAiRecord::User { content } => {
            lr.set_event_name("gen_ai.user.message");
            lr.set_body(AnyValue::from(content.clone()));
        }
        GenAiRecord::Tool {
            id,
            content,
            is_error,
        } => {
            lr.set_event_name("gen_ai.tool.message");
            lr.add_attribute("gen_ai.tool.call.id", id.clone());
            if *is_error {
                lr.add_attribute("error.type", "tool_error");
            }
            lr.set_body(AnyValue::from(content.clone()));
        }
        GenAiRecord::Assistant {
            text,
            reasoning,
            tool_calls,
            model,
        } => {
            if let Some(m) = model {
                lr.add_attribute("gen_ai.request.model", m.clone());
            }
            let message = assistant_message_body(text.as_deref(), reasoning.as_deref(), tool_calls);
            if is_final_assistant {
                lr.set_event_name("gen_ai.choice");
                lr.set_body(choice_body(message, !tool_calls.is_empty()));
            } else {
                lr.set_event_name("gen_ai.assistant.message");
                lr.set_body(message);
            }
        }
    }
}

/// The gen_ai assistant message body: `content` (text), `reasoning`, and `tool_calls` (each with
/// `id`/`name`/`arguments`), omitting absent fields.
fn assistant_message_body(
    text: Option<&str>,
    reasoning: Option<&str>,
    tool_calls: &[crate::turn_trace::ToolCall],
) -> opentelemetry::logs::AnyValue {
    use opentelemetry::logs::AnyValue;

    let mut fields: Vec<(&'static str, AnyValue)> = Vec::new();
    if let Some(t) = text {
        fields.push(("content", AnyValue::from(t.to_string())));
    }
    if let Some(r) = reasoning {
        fields.push(("reasoning", AnyValue::from(r.to_string())));
    }
    if !tool_calls.is_empty() {
        let calls: AnyValue = tool_calls
            .iter()
            .map(|c| {
                AnyValue::from_iter([
                    ("id", AnyValue::from(c.id.clone())),
                    ("name", AnyValue::from(c.name.clone())),
                    ("arguments", AnyValue::from(c.arguments.clone())),
                ])
            })
            .collect();
        fields.push(("tool_calls", calls));
    }
    AnyValue::from_iter(fields)
}

/// The gen_ai.choice body wrapping the completion `message` with `index` and a `finish_reason`
/// (`tool_calls` when the turn ended on a tool call, else `stop`).
fn choice_body(
    message: opentelemetry::logs::AnyValue,
    has_tool_calls: bool,
) -> opentelemetry::logs::AnyValue {
    use opentelemetry::logs::AnyValue;

    let finish = if has_tool_calls { "tool_calls" } else { "stop" };
    AnyValue::from_iter([
        ("index", AnyValue::Int(0)),
        ("finish_reason", AnyValue::from(finish)),
        ("message", message),
    ])
}

/// Serialize the engine's current recording span to a W3C `(traceparent, tracestate)` pair. The
/// long-lived provisioning broker outlives a turn, so its per-turn parent can't ride a static pod
/// env (that would collapse every turn onto the first); instead the engine writes this value to the
/// per-turn file channel the broker reads fresh per call. `None` when tracing isn't recording, no
/// OTLP layer, or no valid active span. The inject side of [`extract_parent`].
pub(crate) fn current_trace_env() -> Option<(String, Option<String>)> {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    trace_env_from_context(&tracing::Span::current().context())
}

/// The pure half of [`current_trace_env`]: inject `cx` through the W3C propagator, returning the
/// `traceparent` (and a non-empty `tracestate`) only when `cx` carries a valid span context.
fn trace_env_from_context(cx: &opentelemetry::Context) -> Option<(String, Option<String>)> {
    use opentelemetry::propagation::TextMapPropagator as _;
    use opentelemetry::trace::TraceContextExt as _;

    if !cx.span().span_context().is_valid() {
        return None;
    }
    let mut carrier = HashMap::new();
    TraceContextPropagator::new().inject_context(cx, &mut carrier);
    let traceparent = carrier.remove("traceparent")?;
    let tracestate = carrier.remove("tracestate").filter(|s| !s.is_empty());
    Some((traceparent, tracestate))
}

/// Extract the controller's remote parent context from the W3C carrier values, or `None` when
/// `traceparent` is absent/blank or serializes to an invalid (all-zeros) span context. A local
/// `TraceContextPropagator` mirrors the controller's injection side.
fn extract_parent(
    traceparent: Option<String>,
    tracestate: Option<String>,
) -> Option<opentelemetry::Context> {
    use opentelemetry::propagation::TextMapPropagator as _;
    use opentelemetry::trace::TraceContextExt as _;

    let traceparent = traceparent.filter(|s| !s.trim().is_empty())?;
    let mut carrier = HashMap::new();
    carrier.insert("traceparent".to_string(), traceparent);
    if let Some(ts) = tracestate.filter(|s| !s.trim().is_empty()) {
        carrier.insert("tracestate".to_string(), ts);
    }
    let cx = TraceContextPropagator::new().extract(&carrier);
    cx.span().span_context().is_valid().then_some(cx)
}

/// The engine's OTLP guards: the tracer provider when a traces endpoint was configured, and the
/// GenAI content-logs provider when the content flag + an endpoint are set. Either may be `None`
/// (the default). Dropping them shuts the providers down (a full flush), bounded so a dead collector
/// cannot hang exit.
struct EngineTelemetry {
    provider: Option<SdkTracerProvider>,
    logs_provider: Option<SdkLoggerProvider>,
}

impl EngineTelemetry {
    /// Install the trace layer and the (opt-in) content-logs provider, each independent and each
    /// degrading to off on any failure. Must run inside a tokio runtime context (the exporters build
    /// tonic channels and spawn batch processors).
    fn install() -> Self {
        Self {
            provider: install_traces(),
            logs_provider: install_logs(),
        }
    }
}

/// Install the global tracing subscriber with only the OTLP span layer, when a traces endpoint is
/// set. A build/registration failure degrades to telemetry-off, never fatal. Returns the provider
/// (also published to [`PROVIDER`]) or `None`.
fn install_traces() -> Option<SdkTracerProvider> {
    let endpoint = resolve_endpoint(
        std::env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").ok(),
        std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok(),
    )?;

    match build_provider(&endpoint) {
        Ok(provider) => {
            let tracer = provider.tracer(SERVICE_NAME);
            let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
            let filter =
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
            // No stderr fmt layer on purpose: the engine narrates via the sink/eprintln!, so a
            // fmt layer would duplicate output. Spans go only to the OTLP layer.
            let installed = tracing_subscriber::registry()
                .with(filter)
                .with(otel_layer)
                .try_init()
                .is_ok();
            if !installed {
                return None;
            }
            // W3C context for the gateway RPC interceptor to inject.
            opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
            let _ = PROVIDER.set(provider.clone());
            eprintln!("[crucible] OTLP span export enabled -> {endpoint}");
            Some(provider)
        }
        Err(e) => {
            eprintln!(
                "[crucible] OTEL endpoint set but the OTLP exporter failed to build; spans \
                 off: {e:#}"
            );
            None
        }
    }
}

/// Install the GenAI content-logs provider when `CRUCIBLE_TURN_TRACE_CONTENT` is on AND an OTLP logs
/// endpoint is set (the logs-specific override wins, else the base). Off by default = nothing
/// installed = [`emit_conversation_logs`] is a no-op. Publishes to [`LOGS_PROVIDER`]; returns it or
/// `None`. A build failure degrades to content-off, never fatal.
fn install_logs() -> Option<SdkLoggerProvider> {
    if !content_logs_enabled() {
        return None;
    }
    let endpoint = resolve_logs_endpoint(
        std::env::var("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT").ok(),
        std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok(),
    )?;
    match build_logs_provider(&endpoint) {
        Ok(provider) => {
            let _ = LOGS_PROVIDER.set(provider.clone());
            eprintln!("[crucible] OTLP GenAI content-log export enabled -> {endpoint}");
            Some(provider)
        }
        Err(e) => {
            eprintln!(
                "[crucible] CRUCIBLE_TURN_TRACE_CONTENT set but the OTLP log exporter failed to \
                 build; content logs off: {e:#}"
            );
            None
        }
    }
}

impl Drop for EngineTelemetry {
    fn drop(&mut self) {
        // Bounded shutdown so a hung collector can't wedge engine exit: shut both providers down on
        // a scratch thread and join with a short deadline.
        let provider = self.provider.take();
        let logs_provider = self.logs_provider.take();
        if provider.is_none() && logs_provider.is_none() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Some(p) = provider {
                let _ = p.shutdown();
            }
            if let Some(p) = logs_provider {
                let _ = p.shutdown();
            }
            let _ = tx.send(());
        });
        let _ = rx.recv_timeout(Duration::from_secs(3));
    }
}

/// Resolve the OTLP endpoint: the traces-specific env wins over the base one, and an empty/
/// whitespace value is treated as unset. Pure, so the set/unset table is unit-testable.
fn resolve_endpoint(traces: Option<String>, base: Option<String>) -> Option<String> {
    [traces, base]
        .into_iter()
        .flatten()
        .find(|s| !s.trim().is_empty())
}

/// Resolve the OTLP/HTTP logs endpoint (Loki's OTLP ingest is HTTP-only). Per the OTLP/HTTP spec the
/// signal-specific `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` is the full URL, used verbatim; the base
/// `OTEL_EXPORTER_OTLP_ENDPOINT` gets `/v1/logs` appended (unlike gRPC, which never appends).
/// Empty/whitespace reads as unset. Pure, so the append table is unit-testable.
fn resolve_logs_endpoint(logs: Option<String>, base: Option<String>) -> Option<String> {
    let clean = |s: Option<String>| s.filter(|v| !v.trim().is_empty());
    if let Some(full) = clean(logs) {
        return Some(full);
    }
    let base = clean(base)?;
    Some(format!("{}/v1/logs", base.trim_end_matches('/')))
}

/// Build a batch-exporting tracer provider aimed at `endpoint` (OTLP over grpc/tonic).
fn build_provider(endpoint: &str) -> Result<SdkTracerProvider> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .context("building the OTLP gRPC span exporter")?;
    let resource = Resource::builder().with_service_name(SERVICE_NAME).build();
    Ok(SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build())
}

/// Build a batch-exporting logs provider aimed at `endpoint` (OTLP over HTTP/protobuf → Loki), the
/// content-log sibling of [`build_provider`]. The endpoint's path is already resolved, so it is used
/// verbatim.
fn build_logs_provider(endpoint: &str) -> Result<SdkLoggerProvider> {
    let exporter = opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .build()
        .context("building the OTLP HTTP log exporter")?;
    let resource = Resource::builder().with_service_name(SERVICE_NAME).build();
    Ok(SdkLoggerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_unset_is_none() {
        assert_eq!(resolve_endpoint(None, None), None);
        // Empty / whitespace values read as unset (never an empty endpoint).
        assert_eq!(resolve_endpoint(Some("".into()), Some("   ".into())), None);
    }

    #[test]
    fn traces_specific_endpoint_wins_over_base() {
        assert_eq!(
            resolve_endpoint(
                Some("http://tempo:4317/traces".into()),
                Some("http://tempo:4317".into())
            ),
            Some("http://tempo:4317/traces".into())
        );
    }

    #[test]
    fn logs_endpoint_appends_v1_logs_only_to_the_base() {
        // Signal-specific: full URL, verbatim (no path appended).
        assert_eq!(
            resolve_logs_endpoint(
                Some("http://loki:3100/otlp/v1/logs".into()),
                Some("http://collector:4318".into())
            ),
            Some("http://loki:3100/otlp/v1/logs".into())
        );
        // Base only: append `/v1/logs`, collapsing a trailing slash.
        assert_eq!(
            resolve_logs_endpoint(None, Some("http://collector:4318".into())),
            Some("http://collector:4318/v1/logs".into())
        );
        assert_eq!(
            resolve_logs_endpoint(None, Some("http://collector:4318/".into())),
            Some("http://collector:4318/v1/logs".into())
        );
        // Empty / whitespace reads as unset.
        assert_eq!(resolve_logs_endpoint(Some("  ".into()), None), None);
    }

    #[test]
    fn extract_parent_round_trips_the_controller_traceparent() {
        use opentelemetry::trace::TraceContextExt as _;
        // The exact W3C string the controller's `apply_trace_env` injects for a known trace/span.
        let traceparent = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
        let cx = extract_parent(Some(traceparent.to_string()), None)
            .expect("a well-formed traceparent extracts a valid parent");
        let sc = cx.span().span_context().clone();
        assert!(sc.is_valid());
        // The engine's parent must carry the controller's trace-id and span-id verbatim.
        assert_eq!(
            format!("{:032x}", u128::from_be_bytes(sc.trace_id().to_bytes())),
            "0af7651916cd43dd8448eb211c80319c"
        );
        assert_eq!(
            format!("{:016x}", u64::from_be_bytes(sc.span_id().to_bytes())),
            "b7ad6b7169203331"
        );
    }

    #[test]
    fn extract_parent_ignores_absent_or_garbage_traceparent() {
        // Absent: the normal local run (turn spans root themselves).
        assert!(extract_parent(None, None).is_none());
        // Blank/whitespace reads as absent.
        assert!(extract_parent(Some("   ".into()), None).is_none());
        // Garbage / unparseable: warn-and-ignore, never a bogus parent.
        assert!(extract_parent(Some("not-a-traceparent".into()), None).is_none());
        // A well-formed but all-zeros context is invalid and must never root a parent.
        assert!(
            extract_parent(
                Some("00-00000000000000000000000000000000-0000000000000000-00".into()),
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn trace_env_formats_and_round_trips_through_extract() {
        use opentelemetry::trace::{
            SpanContext, SpanId, TraceContextExt as _, TraceFlags, TraceId, TraceState,
        };
        // A context carrying a known, valid span, the shape a recording turn span has.
        let sc = SpanContext::new(
            TraceId::from_hex("0af7651916cd43dd8448eb211c80319c").expect("trace id"),
            SpanId::from_hex("b7ad6b7169203331").expect("span id"),
            TraceFlags::SAMPLED,
            true,
            TraceState::default(),
        );
        let cx = opentelemetry::Context::new().with_remote_span_context(sc);

        let (traceparent, tracestate) =
            trace_env_from_context(&cx).expect("a valid context yields a traceparent");
        assert_eq!(
            traceparent,
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
        );
        assert_eq!(tracestate, None, "empty tracestate is dropped");

        // The minted value the broker reads back must extract to the same trace/span.
        let back = extract_parent(Some(traceparent), tracestate)
            .expect("the minted traceparent extracts a valid parent");
        assert_eq!(
            back.span().span_context().trace_id(),
            cx.span().span_context().trace_id()
        );
    }

    #[test]
    fn trace_env_from_invalid_context_is_none() {
        // No active span: never mint an all-zeros traceparent into the per-turn channel.
        assert!(trace_env_from_context(&opentelemetry::Context::new()).is_none());
    }

    use crate::turn_trace::{GenAiRecord, ToolCall};
    use opentelemetry::logs::{AnyValue, LoggerProvider as _};
    use opentelemetry::trace::{SpanContext, SpanId, TraceFlags, TraceId, TraceState};
    use opentelemetry_sdk::logs::{InMemoryLogExporter, SdkLoggerProvider, SimpleLogProcessor};

    fn sample_context() -> SpanContext {
        SpanContext::new(
            TraceId::from_hex("0af7651916cd43dd8448eb211c80319c").expect("trace id"),
            SpanId::from_hex("b7ad6b7169203331").expect("span id"),
            TraceFlags::SAMPLED,
            true,
            TraceState::default(),
        )
    }

    fn any_str(v: Option<&AnyValue>) -> Option<String> {
        match v {
            Some(AnyValue::String(s)) => Some(s.to_string()),
            _ => None,
        }
    }

    fn map_field<'a>(body: Option<&'a AnyValue>, key: &str) -> Option<&'a AnyValue> {
        match body {
            Some(AnyValue::Map(m)) => m.get(&opentelemetry::Key::from(key.to_string())),
            _ => None,
        }
    }

    /// The full mapping through the REAL SDK export path (in-memory exporter, simple processor,
    /// no runtime, no mock): the four record kinds get the right event names, the last assistant is
    /// the `gen_ai.choice` completion, and every record correlates to the turn span.
    #[test]
    fn emit_records_maps_kinds_and_correlates() {
        let exporter = InMemoryLogExporter::default();
        let provider = SdkLoggerProvider::builder()
            .with_log_processor(SimpleLogProcessor::new(exporter.clone()))
            .build();
        let logger = provider.logger(SERVICE_NAME);
        let sc = sample_context();

        let records = vec![
            GenAiRecord::User {
                content: "build it".into(),
            },
            // A non-final assistant turn (has a following tool result) → assistant.message.
            GenAiRecord::Assistant {
                text: Some("running".into()),
                reasoning: Some("think".into()),
                tool_calls: vec![ToolCall {
                    id: "t1".into(),
                    name: "Bash".into(),
                    arguments: r#"{"command":"make"}"#.into(),
                }],
                model: Some("claude-opus-4-8".into()),
            },
            GenAiRecord::Tool {
                id: "t1".into(),
                content: "ok".into(),
                is_error: false,
            },
            // The final assistant turn → gen_ai.choice completion.
            GenAiRecord::Assistant {
                text: Some("done".into()),
                reasoning: None,
                tool_calls: vec![],
                model: None,
            },
        ];
        emit_records(&logger, &sc, &records);
        provider.force_flush().expect("flush");

        let logs = exporter.get_emitted_logs().expect("emitted logs");
        assert_eq!(logs.len(), 4);
        let names: Vec<Option<&'static str>> = logs.iter().map(|l| l.record.event_name()).collect();
        assert_eq!(
            names,
            vec![
                Some("gen_ai.user.message"),
                Some("gen_ai.assistant.message"),
                Some("gen_ai.tool.message"),
                Some("gen_ai.choice"),
            ]
        );

        // Every record carries the turn's trace + span id.
        for l in &logs {
            let tc = l.record.trace_context().expect("trace context set");
            assert_eq!(tc.trace_id, sc.trace_id());
            assert_eq!(tc.span_id, sc.span_id());
        }

        // The user body is the full prompt string.
        assert_eq!(any_str(logs[0].record.body()), Some("build it".into()));

        // The assistant message body carries text, reasoning, and the tool call's full args.
        let asst = logs[1].record.body();
        assert_eq!(any_str(map_field(asst, "content")), Some("running".into()));
        assert_eq!(any_str(map_field(asst, "reasoning")), Some("think".into()));
        match map_field(asst, "tool_calls") {
            Some(AnyValue::ListAny(calls)) => {
                assert_eq!(calls.len(), 1);
                let call = Some(&calls[0]);
                assert_eq!(any_str(map_field(call, "name")), Some("Bash".into()));
                assert_eq!(
                    any_str(map_field(call, "arguments")),
                    Some(r#"{"command":"make"}"#.into())
                );
            }
            other => panic!("expected tool_calls list, got {other:?}"),
        }

        // The choice body wraps the completion message with a finish_reason.
        let choice = logs[3].record.body();
        assert_eq!(
            any_str(map_field(choice, "finish_reason")),
            Some("stop".into())
        );
        assert_eq!(
            any_str(map_field(map_field(choice, "message"), "content")),
            Some("done".into())
        );
    }

    /// The content flag is off unless explicitly set truthy, the gate that keeps the logs provider
    /// (and thus all content emission) uninstalled on the default path.
    #[test]
    fn content_logs_enabled_reads_the_flag() {
        // The var is unset in the test process, so the default reads false.
        assert!(std::env::var("CRUCIBLE_TURN_TRACE_CONTENT").is_err());
        assert!(!content_logs_enabled());
    }

    #[test]
    fn turn_export_query_methods() {
        assert!(!TurnExport::Off.emits_anything());
        for e in [
            TurnExport::Spans,
            TurnExport::Content,
            TurnExport::SpansAndContent,
        ] {
            assert!(e.emits_anything());
        }
        assert!(TurnExport::Spans.spans() && !TurnExport::Spans.content());
        assert!(TurnExport::Content.content() && !TurnExport::Content.spans());
        assert!(TurnExport::SpansAndContent.spans() && TurnExport::SpansAndContent.content());
    }

    #[test]
    fn base_endpoint_used_when_no_traces_specific() {
        assert_eq!(
            resolve_endpoint(None, Some("http://tempo:4317".into())),
            Some("http://tempo:4317".into())
        );
        // An empty traces-specific value falls through to the base.
        assert_eq!(
            resolve_endpoint(Some("  ".into()), Some("http://tempo:4317".into())),
            Some("http://tempo:4317".into())
        );
    }
}
