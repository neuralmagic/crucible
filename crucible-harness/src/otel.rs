//! The in-process OTLP http/json collector: logs every export to a jsonl, derives a live 60 s
//! token rate, and rolls the capture up into the `otel_summary` event and the `usage` field.
//! Runs as an axum task on its own current-thread runtime so async stays confined here.
//! Degradation matches telemetry-off: the collector enriches evidence and is never a run
//! dependency.

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use crucible_contract::event::{AgentEvent, ModelUsage};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// The sliding window over which the live token rate is computed.
const WINDOW: Duration = Duration::from_secs(60);

/// Cap on a single OTLP export body so a runaway export can't OOM the collector; oversize is 413.
const MAX_EXPORT_BYTES: usize = 8 * 1024 * 1024;

/// Shared, lock-free reader of the collector's current token rate (tokens/sec). `0.0` (the
/// default) reads back as `None`, so a parser with no collector behind it emits `rate: None`.
#[derive(Clone, Default)]
pub struct RateHandle(Arc<AtomicU64>);

impl RateHandle {
    pub(crate) fn store(&self, rate: f64) {
        self.0.store(rate.to_bits(), Ordering::Relaxed);
    }

    /// The current rate in tokens/sec, or `None` when no rate has been established yet.
    pub(crate) fn get(&self) -> Option<f64> {
        let r = f64::from_bits(self.0.load(Ordering::Relaxed));
        (r > 0.0).then_some(r)
    }
}

/// Sliding-window rate estimator: samples `(monotonic_secs, cumulative_total)`, prunes anything
/// older than [`WINDOW`], and reports the slope across the surviving window. Time is passed in as
/// monotonic seconds so the window math is unit-testable without a clock.
struct RateWindow {
    samples: Vec<(f64, f64)>,
}

impl RateWindow {
    fn new() -> Self {
        RateWindow {
            samples: Vec::new(),
        }
    }

    /// Fold one export's cumulative token `total` in at `now_secs`; returns the current rate.
    /// A non-positive total is ignored (no sample recorded) and the rate holds.
    fn update(&mut self, now_secs: f64, total: f64) -> f64 {
        if total <= 0.0 {
            return self.rate();
        }
        self.samples.push((now_secs, total));
        let cutoff = now_secs - WINDOW.as_secs_f64();
        self.samples.retain(|(t, _)| *t >= cutoff);
        self.rate()
    }

    fn rate(&self) -> f64 {
        if self.samples.len() < 2 {
            return 0.0;
        }
        let (t0, v0) = self.samples[0];
        let (t1, v1) = self.samples[self.samples.len() - 1];
        let dt = t1 - t0;
        if dt > 0.0 { (v1 - v0) / dt } else { 0.0 }
    }
}

/// Server-side state shared across handlers.
struct CollectorState {
    log: Mutex<std::fs::File>,
    rate: RateHandle,
    window: Mutex<RateWindow>,
    start: Instant,
}

/// A running in-process OTLP collector. Owns a background thread with a current-thread tokio
/// runtime; [`OtelCollector::stop`] (or drop) shuts the server down gracefully and joins the
/// thread.
pub struct OtelCollector {
    port: u16,
    rate: RateHandle,
    log_path: PathBuf,
    shutdown: Option<oneshot::Sender<()>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl OtelCollector {
    /// Bind the collector to `bind_addr` on an OS-assigned port, writing every export raw to
    /// `log_path`. Returns once the port is known, or a bind error (caller treats it as
    /// telemetry-off). `bind_addr` is `127.0.0.1` for a local agent, or `0.0.0.0` when a
    /// sandboxed agent reaches the collector over `host.containers.internal`.
    pub fn start(log_path: PathBuf, bind_addr: &str) -> std::io::Result<OtelCollector> {
        // Fresh log per run.
        let _ = std::fs::remove_file(&log_path);
        let file = std::fs::File::options()
            .create(true)
            .append(true)
            .open(&log_path)?;

        let rate = RateHandle::default();
        let state = Arc::new(CollectorState {
            log: Mutex::new(file),
            rate: rate.clone(),
            window: Mutex::new(RateWindow::new()),
            start: Instant::now(),
        });

        let (port_tx, port_rx) = std::sync::mpsc::channel::<std::io::Result<u16>>();
        let (sd_tx, sd_rx) = oneshot::channel::<()>();
        let addr = format!("{bind_addr}:0");

        let handle = std::thread::Builder::new()
            .name("otel-collector".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = port_tx.send(Err(e));
                        return;
                    }
                };
                rt.block_on(async move {
                    let listener = match tokio::net::TcpListener::bind(&addr).await {
                        Ok(l) => l,
                        Err(e) => {
                            let _ = port_tx.send(Err(e));
                            return;
                        }
                    };
                    match listener.local_addr() {
                        Ok(a) => {
                            if port_tx.send(Ok(a.port())).is_err() {
                                return; // the caller stopped waiting; exit without serving
                            }
                        }
                        Err(e) => {
                            let _ = port_tx.send(Err(e));
                            return;
                        }
                    }
                    let app = Router::new()
                        .route("/v1/metrics", post(ingest))
                        .route("/v1/logs", post(ingest))
                        .route("/v1/traces", post(ingest))
                        .with_state(state);
                    let _ = axum::serve(listener, app)
                        .with_graceful_shutdown(async move {
                            let _ = sd_rx.await;
                        })
                        .await;
                });
            })?;

        match port_rx.recv() {
            Ok(Ok(port)) => Ok(OtelCollector {
                port,
                rate,
                log_path,
                shutdown: Some(sd_tx),
                handle: Some(handle),
            }),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                let _ = handle.join();
                Err(std::io::Error::other(
                    "otel collector thread exited before binding",
                ))
            }
        }
    }

    /// The OS-assigned port the collector is listening on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The shared rate handle to clone into a [`crate::stream_json::StreamJsonParser`].
    pub fn rate_handle(&self) -> RateHandle {
        self.rate.clone()
    }

    /// The `http://<host>:<port>` endpoint for a *local* agent (loopback).
    pub fn local_endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The `http://host.containers.internal:<port>` endpoint a sandboxed agent uses to reach a
    /// collector bound to `0.0.0.0` on the host pod.
    pub fn sandbox_endpoint(&self) -> String {
        format!("http://host.containers.internal:{}", self.port)
    }

    /// The sandbox egress allowlist entry that lets the sandbox reach the collector.
    pub fn sandbox_egress(&self) -> String {
        format!("host.containers.internal:{}:full", self.port)
    }

    /// Roll the captured jsonl up into the `otel_summary`; `None` when no export ever landed.
    /// The append handle is an unbuffered `std::fs::File`, so a fresh read sees every export.
    pub fn summary(&self) -> Option<OtelSummary> {
        build_summary(&self.log_path)
    }

    fn shutdown_and_join(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    /// Gracefully stop the server and join the collector thread.
    pub fn stop(mut self) {
        self.shutdown_and_join();
    }
}

impl Drop for OtelCollector {
    fn drop(&mut self) {
        self.shutdown_and_join();
    }
}

/// Ingest handler for all three OTLP routes: appends `{ts, path, payload}` to the jsonl, updates
/// the token rate for `/v1/metrics`, and answers `{"partialSuccess":{}}` (the OTLP success shape).
async fn ingest(State(state): State<Arc<CollectorState>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let body = req.into_body();
    let bytes = match axum::body::to_bytes(body, MAX_EXPORT_BYTES).await {
        Ok(b) => b,
        Err(_) => {
            return (StatusCode::PAYLOAD_TOO_LARGE, "payload too large").into_response();
        }
    };

    let payload: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({ "raw": String::from_utf8_lossy(&bytes) }));

    let record = json!({
        "ts": jiff::Timestamp::now().to_string(),
        "path": path,
        "payload": payload,
    });
    if let Ok(mut f) = state.log.lock() {
        // A write failure must never fail the export (evidence-only).
        let _ = writeln!(f, "{record}");
    }

    if path.contains("/v1/metrics") {
        let total = sum_token_usage(&payload);
        let now_secs = state.start.elapsed().as_secs_f64();
        if let Ok(mut w) = state.window.lock() {
            let rate = w.update(now_secs, total);
            state.rate.store(rate);
        }
    }

    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(r#"{"partialSuccess":{}}"#))
        .unwrap_or_else(|_| StatusCode::OK.into_response())
}

/// Sum every `claude_code.token.usage` data-point value in one metrics export (all models/types),
/// the cumulative total the rate window slopes over.
fn sum_token_usage(payload: &Value) -> f64 {
    let mut total = 0.0;
    for rm in array(payload, "resourceMetrics") {
        for sm in array(rm, "scopeMetrics") {
            for metric in array(sm, "metrics") {
                if metric.get("name").and_then(Value::as_str) != Some("claude_code.token.usage") {
                    continue;
                }
                for dp in data_points(metric) {
                    total += data_point_value(dp);
                }
            }
        }
    }
    total
}

/// The token/cost/latency/active-time rollup of a captured OTLP jsonl. Tokens are `u64` (the
/// contract shape); OTLP values accumulate as `f64` and round at the end.
#[derive(Debug, Clone, PartialEq)]
pub struct OtelSummary {
    pub cost_usd: f64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total: u64,
    pub models: Vec<ModelUsage>,
    pub api_requests: u32,
    pub api_ms: f64,
    pub active_seconds: f64,
}

impl OtelSummary {
    /// The `otel_summary` NDJSON event.
    pub fn to_event(&self) -> AgentEvent {
        AgentEvent::OtelSummary {
            cost_usd: self.cost_usd,
            total: self.total,
            models: self.models.clone(),
            api_requests: self.api_requests,
            api_ms: self.api_ms,
            active_seconds: self.active_seconds,
        }
    }
}

/// OTLP token-type names → the contract's snake_case [`ModelUsage`] fields.
const TOKEN_FIELDS: [(&str, TokenField); 4] = [
    ("input", TokenField::Input),
    ("output", TokenField::Output),
    ("cacheRead", TokenField::CacheRead),
    ("cacheCreation", TokenField::CacheWrite),
];

#[derive(Clone, Copy)]
enum TokenField {
    Input,
    Output,
    CacheRead,
    CacheWrite,
}

/// Read a captured OTLP jsonl and roll it up. `None` when the file is missing/empty/unreadable or
/// carries no records; the caller then leaves the estimate in place.
pub fn build_summary(log_path: &Path) -> Option<OtelSummary> {
    let text = std::fs::read_to_string(log_path).ok()?;
    let records: Vec<Value> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    if records.is_empty() {
        return None;
    }
    Some(summarize(&records))
}

/// The pure rollup over parsed records, split out so tests can drive it directly.
fn summarize(records: &[Value]) -> OtelSummary {
    // (model, token_type) -> tokens, model -> cost, active-time type -> seconds.
    let mut token_totals: BTreeMap<(String, String), f64> = BTreeMap::new();
    let mut cost_totals: BTreeMap<String, f64> = BTreeMap::new();
    let mut active_time: BTreeMap<String, f64> = BTreeMap::new();
    let mut api_requests: u32 = 0;
    let mut api_ms: f64 = 0.0;

    for rec in records {
        let path = rec.get("path").and_then(Value::as_str).unwrap_or("");
        let payload = rec.get("payload").cloned().unwrap_or(Value::Null);

        if path.contains("/v1/metrics") {
            for rm in array(&payload, "resourceMetrics") {
                for sm in array(rm, "scopeMetrics") {
                    for metric in array(sm, "metrics") {
                        let name = metric.get("name").and_then(Value::as_str).unwrap_or("");
                        for dp in data_points(metric) {
                            let attrs = attributes(dp.get("attributes"));
                            let value = data_point_value(dp);
                            match name {
                                "claude_code.token.usage" => {
                                    let model = attr_str(&attrs, "model");
                                    let ttype = attr_str(&attrs, "type");
                                    *token_totals.entry((model, ttype)).or_default() += value;
                                }
                                "claude_code.cost.usage" => {
                                    let model = attr_str(&attrs, "model");
                                    *cost_totals.entry(model).or_default() += value;
                                }
                                "claude_code.active_time.total" => {
                                    let ttype = attr_str(&attrs, "type");
                                    *active_time.entry(ttype).or_default() += value;
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        } else if path.contains("/v1/logs") {
            for rl in array(&payload, "resourceLogs") {
                for sl in array(rl, "scopeLogs") {
                    for lr in array(sl, "logRecords") {
                        let attrs = attributes(lr.get("attributes"));
                        if attr_str(&attrs, "event.name") == "claude_code.api_request" {
                            api_requests += 1;
                            api_ms += attrs.get("duration_ms").and_then(as_f64).unwrap_or(0.0);
                        }
                    }
                }
            }
        }
    }

    let models: Vec<String> = {
        let mut s: Vec<String> = token_totals.keys().map(|(m, _)| m.clone()).collect();
        s.sort();
        s.dedup();
        s
    };

    let mut model_rows = Vec::new();
    let (mut agg_in, mut agg_out, mut agg_cr, mut agg_cw) = (0.0, 0.0, 0.0, 0.0);
    for model in &models {
        let mut row = ModelUsage {
            model: model.clone(),
            cost_usd: *cost_totals.get(model).unwrap_or(&0.0),
            ..Default::default()
        };
        for (raw, field) in TOKEN_FIELDS {
            let v = *token_totals
                .get(&(model.clone(), raw.to_string()))
                .unwrap_or(&0.0);
            match field {
                TokenField::Input => {
                    row.input = round_u64(v);
                    agg_in += v;
                }
                TokenField::Output => {
                    row.output = round_u64(v);
                    agg_out += v;
                }
                TokenField::CacheRead => {
                    row.cache_read = round_u64(v);
                    agg_cr += v;
                }
                TokenField::CacheWrite => {
                    row.cache_write = round_u64(v);
                    agg_cw += v;
                }
            }
        }
        model_rows.push(row);
    }

    let cost_usd = cost_totals.values().sum();
    OtelSummary {
        cost_usd,
        input: round_u64(agg_in),
        output: round_u64(agg_out),
        cache_read: round_u64(agg_cr),
        cache_write: round_u64(agg_cw),
        total: round_u64(agg_in + agg_out + agg_cr + agg_cw),
        models: model_rows,
        api_requests,
        api_ms,
        active_seconds: active_time.values().sum(),
    }
}

/// The `OTEL_*` env every telemetry-on agent gets: all three OTLP signals over http/json to
/// `endpoint`, a 10 s metric interval, plus full-fidelity content flags. The content flags mean
/// the `otel-log` evidence carries prompt text and tool i/o, confidential like transcripts.
pub fn otel_env(endpoint: &str) -> Vec<(String, String)> {
    [
        ("CLAUDE_CODE_ENABLE_TELEMETRY", "1"),
        ("OTEL_METRICS_EXPORTER", "otlp"),
        ("OTEL_LOGS_EXPORTER", "otlp"),
        ("OTEL_TRACES_EXPORTER", "otlp"),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json"),
        ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint),
        ("OTEL_METRIC_EXPORT_INTERVAL", "10000"),
        ("CLAUDE_CODE_ENHANCED_TELEMETRY_BETA", "1"),
        ("OTEL_LOG_USER_PROMPTS", "1"),
        ("OTEL_LOG_TOOL_DETAILS", "1"),
        ("OTEL_LOG_TOOL_CONTENT", "1"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

// --- OTLP/JSON shape helpers -------------------------------------------------------------------

/// The `dataPoints` array of a metric, from whichever of `sum`/`gauge`/`histogram` carries it.
fn data_points(metric: &Value) -> Vec<&Value> {
    for key in ["sum", "gauge", "histogram"] {
        if let Some(dps) = metric.get(key).and_then(|d| d.get("dataPoints")) {
            return dps
                .as_array()
                .map(|a| a.iter().collect())
                .unwrap_or_default();
        }
    }
    Vec::new()
}

/// A data point's numeric value: `asDouble` or `asInt` (OTLP/JSON encodes int64 as a string, so
/// both a JSON number and a numeric string are accepted).
fn data_point_value(dp: &Value) -> f64 {
    dp.get("asDouble")
        .and_then(as_f64)
        .or_else(|| dp.get("asInt").and_then(as_f64))
        .unwrap_or(0.0)
}

/// Flatten an OTLP attribute list (`[{key, value:{stringValue|intValue|doubleValue}}]`) into a
/// plain map keyed by attribute name, values kept as raw JSON so numeric reads stay possible.
fn attributes(attrs: Option<&Value>) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    let Some(list) = attrs.and_then(Value::as_array) else {
        return out;
    };
    for a in list {
        let Some(key) = a.get("key").and_then(Value::as_str) else {
            continue;
        };
        let value = a.get("value").and_then(|v| {
            v.get("stringValue")
                .or_else(|| v.get("intValue"))
                .or_else(|| v.get("doubleValue"))
                .cloned()
        });
        if let Some(v) = value {
            out.insert(key.to_string(), v);
        }
    }
    out
}

/// A string-valued attribute, defaulting to `"unknown"` when absent.
fn attr_str(attrs: &BTreeMap<String, Value>, key: &str) -> String {
    match attrs.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => "unknown".to_string(),
    }
}

/// Coerce a JSON number or numeric string to `f64` (OTLP/JSON int64-as-string tolerance).
fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn array<'a>(v: &'a Value, key: &str) -> Vec<&'a Value> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

/// Round a non-negative f64 accumulator to `u64`, clamping at 0.
fn round_u64(v: f64) -> u64 {
    if v <= 0.0 { 0 } else { v.round() as u64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics_record(model: &str, ttype: &str, tokens: i64, cost: Option<f64>) -> Value {
        let mut metrics = vec![json!({
            "name": "claude_code.token.usage",
            "sum": {"dataPoints": [{
                "asInt": tokens,
                "attributes": [
                    {"key": "model", "value": {"stringValue": model}},
                    {"key": "type", "value": {"stringValue": ttype}},
                ],
            }]},
        })];
        if let Some(c) = cost {
            metrics.push(json!({
                "name": "claude_code.cost.usage",
                "sum": {"dataPoints": [{
                    "asDouble": c,
                    "attributes": [{"key": "model", "value": {"stringValue": model}}],
                }]},
            }));
        }
        json!({
            "path": "/v1/metrics",
            "payload": {"resourceMetrics": [{"scopeMetrics": [{"metrics": metrics}]}]},
        })
    }

    fn api_log_record(duration_ms: f64) -> Value {
        json!({
            "path": "/v1/logs",
            "payload": {"resourceLogs": [{"scopeLogs": [{"logRecords": [{
                "attributes": [
                    {"key": "event.name", "value": {"stringValue": "claude_code.api_request"}},
                    {"key": "duration_ms", "value": {"doubleValue": duration_ms}},
                ],
            }]}]}]},
        })
    }

    #[test]
    fn summarize_aggregates_by_model() {
        let records = vec![
            json!({
                "path": "/v1/metrics",
                "payload": {"resourceMetrics": [{"scopeMetrics": [{"metrics": [
                    {"name": "claude_code.token.usage", "sum": {"dataPoints": [
                        {"asInt": 1200, "attributes": [
                            {"key": "model", "value": {"stringValue": "claude-opus-4-6"}},
                            {"key": "type", "value": {"stringValue": "input"}},
                        ]},
                        {"asInt": 88000, "attributes": [
                            {"key": "model", "value": {"stringValue": "claude-opus-4-6"}},
                            {"key": "type", "value": {"stringValue": "cacheRead"}},
                        ]},
                    ]}},
                    {"name": "claude_code.cost.usage", "sum": {"dataPoints": [
                        {"asDouble": 0.84, "attributes": [
                            {"key": "model", "value": {"stringValue": "claude-opus-4-6"}},
                        ]},
                    ]}},
                ]}]}]},
            }),
            api_log_record(1900.0),
        ];
        let s = summarize(&records);
        assert_eq!(s.cost_usd, 0.84);
        assert_eq!(s.input, 1200);
        assert_eq!(s.cache_read, 88000);
        assert_eq!(s.total, 89200);
        assert_eq!(s.api_requests, 1);
        assert_eq!(s.api_ms, 1900.0);
        assert_eq!(s.models[0].model, "claude-opus-4-6");
        assert_eq!(s.models[0].cost_usd, 0.84);
    }

    #[test]
    fn otlp_int64_as_string_is_tolerated() {
        // Real OTLP/JSON encodes int64 as a *string*; the summary must still read it.
        let rec = json!({
            "path": "/v1/metrics",
            "payload": {"resourceMetrics": [{"scopeMetrics": [{"metrics": [
                {"name": "claude_code.token.usage", "sum": {"dataPoints": [
                    {"asInt": "4200", "attributes": [
                        {"key": "model", "value": {"stringValue": "m"}},
                        {"key": "type", "value": {"stringValue": "input"}},
                    ]},
                ]}},
            ]}]}]},
        });
        let s = summarize(&[rec]);
        assert_eq!(s.input, 4200);
        assert_eq!(s.total, 4200);
    }

    #[test]
    fn active_time_and_multi_model_split() {
        let records = vec![
            metrics_record("model-a", "input", 100, Some(0.10)),
            metrics_record("model-b", "output", 50, Some(0.05)),
            json!({
                "path": "/v1/metrics",
                "payload": {"resourceMetrics": [{"scopeMetrics": [{"metrics": [
                    {"name": "claude_code.active_time.total", "sum": {"dataPoints": [
                        {"asDouble": 42.0, "attributes": [
                            {"key": "type", "value": {"stringValue": "core"}},
                        ]},
                    ]}},
                ]}]}]},
            }),
        ];
        let s = summarize(&records);
        assert_eq!(s.models.len(), 2);
        assert!((s.cost_usd - 0.15).abs() < 1e-9);
        assert_eq!(s.active_seconds, 42.0);
        assert_eq!(s.input, 100);
        assert_eq!(s.output, 50);
    }

    #[test]
    fn build_summary_missing_file_returns_none() {
        let dir = std::env::temp_dir().join(format!("otel-test-{}", std::process::id()));
        assert!(build_summary(&dir.join("nope.jsonl")).is_none());
    }

    #[test]
    fn rate_window_slopes_over_samples() {
        let mut w = RateWindow::new();
        assert_eq!(w.update(0.0, 1000.0), 0.0);
        // 10s later, +5000 tokens -> 500 tok/s.
        let r = w.update(10.0, 6000.0);
        assert!((r - 500.0).abs() < 1e-9, "rate was {r}");
    }

    #[test]
    fn rate_window_prunes_stale_samples() {
        let mut w = RateWindow::new();
        w.update(0.0, 1000.0);
        w.update(10.0, 6000.0);
        // 120s in: earlier samples fall outside the 60s window; the rate resets to 0 until a
        // second fresh sample arrives.
        let r = w.update(120.0, 100_000.0);
        assert_eq!(r, 0.0, "single surviving sample => no slope");
        let r2 = w.update(130.0, 105_000.0);
        assert!((r2 - 500.0).abs() < 1e-9, "rate was {r2}");
    }

    #[test]
    fn rate_window_ignores_nonpositive_total() {
        let mut w = RateWindow::new();
        w.update(0.0, 1000.0);
        let held = w.update(5.0, 3500.0);
        assert!((held - 500.0).abs() < 1e-9);
        // A zero-total export doesn't record a sample and holds the rate.
        assert!((w.update(6.0, 0.0) - held).abs() < 1e-9);
    }

    #[test]
    fn rate_handle_zero_reads_as_none() {
        let h = RateHandle::default();
        assert!(h.get().is_none());
        h.store(318.4);
        assert_eq!(h.get(), Some(318.4));
    }

    #[test]
    fn otel_env_has_the_adr_matrix() {
        let env = otel_env("http://127.0.0.1:4318");
        let map: BTreeMap<_, _> = env.into_iter().collect();
        assert_eq!(map["CLAUDE_CODE_ENABLE_TELEMETRY"], "1");
        assert_eq!(map["OTEL_EXPORTER_OTLP_PROTOCOL"], "http/json");
        assert_eq!(map["OTEL_EXPORTER_OTLP_ENDPOINT"], "http://127.0.0.1:4318");
        assert_eq!(map["OTEL_METRIC_EXPORT_INTERVAL"], "10000");
        assert_eq!(map["CLAUDE_CODE_ENHANCED_TELEMETRY_BETA"], "1");
        assert_eq!(map["OTEL_LOG_TOOL_CONTENT"], "1");
        // All three signals point at otlp.
        for k in [
            "OTEL_METRICS_EXPORTER",
            "OTEL_LOGS_EXPORTER",
            "OTEL_TRACES_EXPORTER",
        ] {
            assert_eq!(map[k], "otlp");
        }
    }

    // End-to-end: real server, real HTTP POST, real file append, no mocks.
    #[test]
    fn collector_end_to_end_over_real_http() {
        let dir = std::env::temp_dir().join(format!("otel-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("otel.jsonl");
        let collector = OtelCollector::start(log.clone(), "127.0.0.1").expect("collector binds");
        let port = collector.port();
        assert_ne!(port, 0);

        let body = serde_json::to_vec(&json!({
            "resourceMetrics": [{"scopeMetrics": [{"metrics": [
                {"name": "claude_code.token.usage", "sum": {"dataPoints": [
                    {"asInt": 1200, "attributes": [
                        {"key": "model", "value": {"stringValue": "claude-opus-4-6"}},
                        {"key": "type", "value": {"stringValue": "input"}},
                    ]},
                ]}},
                {"name": "claude_code.cost.usage", "sum": {"dataPoints": [
                    {"asDouble": 0.42, "attributes": [
                        {"key": "model", "value": {"stringValue": "claude-opus-4-6"}},
                    ]},
                ]}},
            ]}]}],
        }))
        .unwrap();

        let resp = post_bytes(port, "/v1/metrics", &body);
        assert!(resp.contains("partialSuccess"), "otlp ack: {resp}");

        let summary = collector.summary().expect("summary after one export");
        assert_eq!(summary.input, 1200);
        assert_eq!(summary.cost_usd, 0.42);
        assert_eq!(summary.models[0].model, "claude-opus-4-6");

        collector.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collector_rejects_oversize_body() {
        let dir = std::env::temp_dir().join(format!("otel-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("otel.jsonl");
        let collector = OtelCollector::start(log, "127.0.0.1").expect("binds");
        let port = collector.port();
        let big = vec![b'x'; MAX_EXPORT_BYTES + 1];
        let resp = post_bytes(port, "/v1/metrics", &big);
        assert!(resp.contains("413"), "expected 413, got: {resp}");
        collector.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Minimal blocking HTTP/1.1 POST over a raw TCP socket, so the test needs no HTTP client dep.
    /// Returns the raw response (status line + headers + body) as a string.
    fn post_bytes(port: u16, path: &str, body: &[u8]) -> String {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        s.write_all(head.as_bytes()).unwrap();
        s.write_all(body).unwrap();
        s.flush().unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }
}
