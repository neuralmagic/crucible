//! MLflow exporter — the MLflow exporter, a controller background task over evidence that already
//! exists (the `otel-log` Tier 2 artifact in the drop-box + the folded `runs`/`candidates` rows).
//!
//! It runs POST-fold, never inline with a run or a fold. A failed export marks the bookkeeping row
//! ([`crate::runs::model::MlflowExportRow`]) and retries on the next sweep — the agentic-ci
//! `allow_failure: true` ethos. Pods never see MLflow credentials; the exporter lives ONLY here, is
//! a per-deployment opt-in (tracking URI + token + experiment mapping in config), and is entirely
//! OFF when [`MlflowConfig::from_env`] finds no tracking URI.
//!
//! Two halves, mirroring agentic-ci's `mlflow-push` + native tracking:
//!   * **Traces** — captured OTLP/JSON `/v1/traces` records re-encoded to OTLP protobuf
//!     ([`traces_to_protobuf`]) and POSTed to `<uri>/v1/traces` (protobuf, bearer,
//!     `x-mlflow-experiment-id`). Association is experiment-level only, so run/domain ride span
//!     resource attributes we stamp before the push, matched by tags on the MLflow run.
//!   * **Runs** — plain tracking REST: experiment get-by-name-else-create, run create, log-batch
//!     (params + per-turn metrics), update-to-close. Mapping: domain → experiment, crucible run →
//!     MLflow run, turn → metric step.

#![allow(clippy::disallowed_macros)]

use crate::client::Db;
use anyhow::{Context, Result, bail};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{
    AnyValue, KeyValue, any_value::Value as AnyValueEnum,
};
use opentelemetry_proto::tonic::resource::v1::Resource;
use prost::Message;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// How many runs one sweep drains at most, so a large backlog doesn't monopolize the task.
const SWEEP_BATCH: i64 = 50;

/// The per-deployment MLflow export config (MLflow exporter). Built from `CONTROLLER_MLFLOW_*`; absent
/// tracking URI ⇒ [`MlflowConfig::from_env`] returns `None` and the exporter never starts.
#[derive(Debug, Clone, PartialEq)]
pub struct MlflowConfig {
    /// The tracking server base (no trailing slash), e.g. `https://mlflow.internal`.
    tracking_uri: String,
    /// The bearer token, if the server needs one.
    token: Option<String>,
    /// Per-domain experiment-name overrides; a domain absent here falls back to
    /// [`MlflowConfig::default_experiment`] (or, if that's empty, the domain string itself).
    experiment_map: BTreeMap<String, String>,
    /// The experiment a domain with no explicit mapping lands in. Empty ⇒ use the domain as-is.
    default_experiment: String,
    /// How often the background task sweeps for un-exported runs.
    sweep_interval: Duration,
}

impl MlflowConfig {
    /// Read the config from the environment, or `None` when `CONTROLLER_MLFLOW_TRACKING_URI` is
    /// unset/blank — the OFF-when-absent contract. Env surface:
    ///   * `CONTROLLER_MLFLOW_TRACKING_URI` — required; the switch.
    ///   * `CONTROLLER_MLFLOW_TOKEN` — optional bearer.
    ///   * `CONTROLLER_MLFLOW_EXPERIMENT` — the default experiment name.
    ///   * `CONTROLLER_MLFLOW_EXPERIMENT_MAP` — `domain=exp,domain2=exp2` overrides.
    ///   * `CONTROLLER_MLFLOW_SWEEP_SECS` — sweep cadence (default 300s).
    pub fn from_env() -> Option<Self> {
        let tracking_uri = non_empty(std::env::var("CONTROLLER_MLFLOW_TRACKING_URI").ok())?;
        Some(Self {
            tracking_uri: tracking_uri.trim_end_matches('/').to_string(),
            token: non_empty(std::env::var("CONTROLLER_MLFLOW_TOKEN").ok()),
            experiment_map: parse_experiment_map(
                std::env::var("CONTROLLER_MLFLOW_EXPERIMENT_MAP")
                    .ok()
                    .as_deref()
                    .unwrap_or_default(),
            ),
            default_experiment: std::env::var("CONTROLLER_MLFLOW_EXPERIMENT")
                .ok()
                .unwrap_or_default(),
            sweep_interval: std::env::var("CONTROLLER_MLFLOW_SWEEP_SECS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|s| *s > 0)
                .map(Duration::from_secs)
                .unwrap_or_else(|| Duration::from_secs(300)),
        })
    }

    /// The experiment a run for `domain` lands in: an explicit mapping wins, else the configured
    /// default, else the domain string itself (so a bare tracking URI still partitions by domain).
    fn experiment_for(&self, domain: &str) -> String {
        if let Some(exp) = self.experiment_map.get(domain) {
            return exp.clone();
        }
        if !self.default_experiment.is_empty() {
            return self.default_experiment.clone();
        }
        domain.to_string()
    }
}

fn non_empty(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Parse a `domain=exp,domain2=exp2` mapping, skipping malformed entries.
fn parse_experiment_map(raw: &str) -> BTreeMap<String, String> {
    raw.split(',')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            let (k, v) = (k.trim(), v.trim());
            (!k.is_empty() && !v.is_empty()).then(|| (k.to_string(), v.to_string()))
        })
        .collect()
}

/// Re-encode one captured OTLP/JSON `/v1/traces` payload to the OTLP protobuf wire bytes MLflow's
/// `POST /v1/traces` ingests, stamping `resource_attrs` onto every `ResourceSpans.resource` first.
///
/// `opentelemetry-proto`'s `with-serde` deserializer decodes the hex `traceId`/`spanId`/
/// `parentSpanId` fields straight to bytes — the agentic-ci `_fixup_ids` step, done by the
/// deserializer, so no manual hex→base64 dance. The resource stamp is what carries run/domain
/// through to MLflow, whose trace association is experiment-level only.
fn traces_to_protobuf(payload: &Value, resource_attrs: &[(&str, &str)]) -> Result<Vec<u8>> {
    let mut req: ExportTraceServiceRequest =
        serde_json::from_value(payload.clone()).context("decoding OTLP/JSON traces")?;
    if !resource_attrs.is_empty() {
        let stamp: Vec<KeyValue> = resource_attrs
            .iter()
            .map(|(k, v)| KeyValue {
                key: (*k).to_string(),
                value: Some(AnyValue {
                    value: Some(AnyValueEnum::StringValue((*v).to_string())),
                }),
            })
            .collect();
        for rs in &mut req.resource_spans {
            rs.resource
                .get_or_insert_with(Resource::default)
                .attributes
                .extend(stamp.iter().cloned());
        }
    }
    Ok(req.encode_to_vec())
}

/// One captured OTLP export from the `otel-log` artifact: `{ts, path, payload}` (the harness
/// collector's jsonl line shape, `crucible-harness::otel`).
struct OtelRecord {
    path: String,
    payload: Value,
}

/// Read + decompress a pod's stored `otel-log` artifact (gzipped, per the Tier 2 caps) into its
/// export records. A missing artifact yields an empty vec — a run without captured telemetry still
/// exports its ledger metrics. Best-effort per line: a torn/unparseable line is skipped, never
/// fatal.
async fn read_otel_records(pool: &sqlx::PgPool, pod: &str) -> Result<Vec<OtelRecord>> {
    let owner = crate::runs::blob_store::ArtifactOwner::PodEvidence {
        pod: pod.to_string(),
    };
    let Some(payload) = crate::runs::blob_store::get_artifact(
        pool,
        &owner,
        crucible_contract::ArtifactKind::OtelLog.as_str(),
    )
    .await
    .with_context(|| format!("reading {pod}'s otel-log"))?
    else {
        return Ok(Vec::new());
    };
    let text = crate::runs::blob_store::gunzip_maybe(&payload.data)?;
    Ok(parse_otel_records(&text))
}

fn parse_otel_records(text: &str) -> Vec<OtelRecord> {
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .filter_map(|v| {
            Some(OtelRecord {
                path: v.get("path")?.as_str()?.to_string(),
                payload: v.get("payload")?.clone(),
            })
        })
        .collect()
}

/// The trace records among a set of captured exports (path contains `/v1/traces`) — the ones the
/// re-encode pushes.
fn trace_payloads(records: &[OtelRecord]) -> Vec<&Value> {
    records
        .iter()
        .filter(|r| r.path.contains("/v1/traces"))
        .map(|r| &r.payload)
        .collect()
}

/// Sum the `claude_code.token.usage` counter across the captured metric exports — the authoritative
/// total-token metric for the run. `None` when no token metric was captured (telemetry off).
fn total_tokens(records: &[OtelRecord]) -> Option<f64> {
    metric_points(records)
        .filter(|(name, _)| *name == "claude_code.token.usage")
        .filter_map(|(_, dp)| data_point_value(dp))
        .reduce(|a, b| a + b)
}

/// A first `model` attribute seen across the captured metric exports — the model param on the run.
fn model_name(records: &[OtelRecord]) -> Option<String> {
    metric_points(records).find_map(|(_, dp)| {
        array(dp, "attributes").iter().find_map(|attr| {
            if attr.get("key").and_then(Value::as_str) != Some("model") {
                return None;
            }
            attr.get("value")
                .and_then(|v| v.get("stringValue"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
    })
}

/// Every `(metric name, sum data point)` across the captured metric exports, in export order.
fn metric_points(records: &[OtelRecord]) -> impl Iterator<Item = (&str, &Value)> {
    records
        .iter()
        .filter(|r| r.path.contains("/v1/metrics"))
        .flat_map(|r| array(&r.payload, "resourceMetrics"))
        .flat_map(|rm| array(rm, "scopeMetrics"))
        .flat_map(|sm| array(sm, "metrics"))
        .flat_map(|m| {
            let name = m.get("name").and_then(Value::as_str).unwrap_or_default();
            array(m.get("sum").unwrap_or(&Value::Null), "dataPoints")
                .iter()
                .map(move |dp| (name, dp))
        })
}

fn array<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).map_or(&[], |a| a)
}

/// A metric data point's numeric value, `asInt` (OTLP encodes int64 as a JSON string) or `asDouble`.
fn data_point_value(dp: &Value) -> Option<f64> {
    if let Some(i) = dp.get("asInt") {
        return i
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .or_else(|| i.as_f64());
    }
    dp.get("asDouble").and_then(Value::as_f64)
}

/// One MLflow metric point: `turn → step` (a run's per-candidate gate scores log at `step = iter`;
/// run-level cost/tokens log at step 0).
#[derive(Debug, Clone, PartialEq)]
pub struct Metric {
    key: String,
    value: f64,
    step: i64,
}

/// The MLflow run status a crucible run closes as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Finished,
    Failed,
}

impl RunStatus {
    fn as_str(self) -> &'static str {
        match self {
            RunStatus::Finished => "FINISHED",
            RunStatus::Failed => "FAILED",
        }
    }

    /// Map a crucible run's `status` string onto the MLflow terminal status.
    fn from_run_status(status: &str) -> Self {
        let s = status.to_ascii_lowercase();
        if s.contains("fail") || s == "no-session" || s == "error" {
            RunStatus::Failed
        } else {
            RunStatus::Finished
        }
    }
}

/// The fully-assembled export for one crucible run — the pure input the REST push consumes, built
/// by [`assemble_run_export`] from the DB rows + the `otel-log` evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct RunExport {
    run_id: String,
    domain: String,
    experiment: String,
    /// `(key, value)` params: domain, model, run status, and best score.
    params: Vec<(String, String)>,
    /// gate score (run-level + per-candidate as steps), cost, tokens.
    metrics: Vec<Metric>,
    status: RunStatus,
    /// The re-encoded OTLP protobuf trace bodies (already resource-stamped), one per captured
    /// `/v1/traces` export.
    traces: Vec<Vec<u8>>,
}

/// Assemble the export for one run from the ledger rows + the `otel-log` evidence, or `None` when
/// the run row is gone (a rebuilt DB that dropped it). Pure w.r.t. the network — no MLflow call
/// here; that's [`export_one`].
async fn assemble_run_export(
    db: &Db,
    cfg: &MlflowConfig,
    run_id: &str,
) -> Result<Option<RunExport>> {
    let Some(run) = crate::runs::store::get_run(db.pool(), run_id).await? else {
        return Ok(None);
    };
    let (_issue, repo) = crate::runs::store::run_issue_repo(db.pool(), run_id).await?;
    let domain = repo.unwrap_or_else(|| "unknown".to_string());
    let experiment = cfg.experiment_for(&domain);

    // The captured telemetry, if this run's pod uploaded an otel-log to the drop-box.
    let records = match &run.pod {
        Some(pod) => read_otel_records(db.pool(), pod).await?,
        None => Vec::new(),
    };

    let run_attrs = [("crucible.run_id", run_id), ("crucible.domain", &domain)];
    let mut traces = Vec::new();
    for payload in trace_payloads(&records) {
        traces.push(traces_to_protobuf(payload, &run_attrs)?);
    }

    let mut params = vec![
        ("domain".to_string(), domain.clone()),
        ("status".to_string(), run.status.clone()),
    ];
    if let Some(model) = model_name(&records) {
        params.push(("model".to_string(), model));
    }
    if let Some(score) = run.best_score {
        params.push(("best_score".to_string(), score.to_string()));
    }

    // metrics: run-level gate score + cost + tokens at step 0, then per-candidate scores at their
    // iter as the step (turn → metric step).
    let mut metrics = Vec::new();
    if let Some(score) = run.best_score {
        metrics.push(Metric {
            key: "gate_score".to_string(),
            value: score,
            step: 0,
        });
    }
    if let Some(cost) = run.cost_usd {
        metrics.push(Metric {
            key: "cost_usd".to_string(),
            value: cost,
            step: 0,
        });
    }
    if let Some(tokens) = total_tokens(&records) {
        metrics.push(Metric {
            key: "tokens_total".to_string(),
            value: tokens,
            step: 0,
        });
    }
    for cand in crate::runs::store::list_candidates_for_run_by_iter(db.pool(), run_id).await? {
        if let (Some(score), Some(iter)) = (cand.score, cand.iter) {
            metrics.push(Metric {
                key: "gate_score".to_string(),
                value: score,
                step: iter,
            });
        }
    }

    Ok(Some(RunExport {
        run_id: run_id.to_string(),
        domain,
        experiment,
        params,
        metrics,
        status: RunStatus::from_run_status(&run.status),
        traces,
    }))
}

/// A thin MLflow tracking + OTLP client over `reqwest` (no maintained Rust MLflow client exists).
struct MlflowClient {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl MlflowClient {
    fn new(cfg: &MlflowConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building the MLflow http client")?;
        Ok(Self {
            http,
            base: cfg.tracking_uri.clone(),
            token: cfg.token.clone(),
        })
    }

    fn tracking_url(&self, method: &str) -> String {
        format!("{}/api/2.0/mlflow/{method}", self.base)
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => rb.bearer_auth(t),
            None => rb,
        }
    }

    /// The experiment id for `name`: get-by-name, else create. A create that races another
    /// exporter (`RESOURCE_ALREADY_EXISTS`) falls back to a re-get, so concurrent sweeps converge.
    async fn experiment_id(&self, name: &str) -> Result<String> {
        if let Some(id) = self.get_experiment(name).await? {
            return Ok(id);
        }
        match self.create_experiment(name).await {
            Ok(id) => Ok(id),
            Err(_) => self
                .get_experiment(name)
                .await?
                .context("experiment vanished between create and re-get"),
        }
    }

    async fn get_experiment(&self, name: &str) -> Result<Option<String>> {
        let encoded =
            percent_encoding::utf8_percent_encode(name, percent_encoding::NON_ALPHANUMERIC);
        let url = format!(
            "{}?experiment_name={encoded}",
            self.tracking_url("experiments/get-by-name")
        );
        let resp = self
            .auth(self.http.get(url))
            .send()
            .await
            .context("experiments/get-by-name")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body = ok_json(resp, "experiments/get-by-name").await?;
        Ok(body
            .get("experiment")
            .and_then(|e| e.get("experiment_id"))
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    async fn create_experiment(&self, name: &str) -> Result<String> {
        let resp = self
            .auth(
                self.http
                    .post(self.tracking_url("experiments/create"))
                    .json(&serde_json::json!({ "name": name })),
            )
            .send()
            .await
            .context("experiments/create")?;
        let body = ok_json(resp, "experiments/create").await?;
        body.get("experiment_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .context("experiments/create returned no experiment_id")
    }

    /// Create the MLflow run under `experiment_id`, tagged with the crucible run id + domain so the
    /// experiment-level trace association can be correlated back to this run. Returns its run id.
    async fn create_run(&self, experiment_id: &str, export: &RunExport) -> Result<String> {
        let body = serde_json::json!({
            "experiment_id": experiment_id,
            "run_name": export.run_id,
            "start_time": now_millis(),
            "tags": [
                {"key": "crucible.run_id", "value": export.run_id},
                {"key": "crucible.domain", "value": export.domain},
            ],
        });
        let resp = self
            .auth(self.http.post(self.tracking_url("runs/create")).json(&body))
            .send()
            .await
            .context("runs/create")?;
        let body = ok_json(resp, "runs/create").await?;
        body.get("run")
            .and_then(|r| r.get("info"))
            .and_then(|i| i.get("run_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .context("runs/create returned no run_id")
    }

    /// Log the run's params + metrics in one batch (`runs/log-batch`).
    async fn log_batch(
        &self,
        mlflow_run_id: &str,
        params: &[(String, String)],
        metrics: &[Metric],
    ) -> Result<()> {
        if params.is_empty() && metrics.is_empty() {
            return Ok(());
        }
        let ts = now_millis();
        let body = serde_json::json!({
            "run_id": mlflow_run_id,
            "params": params.iter().map(|(k, v)| serde_json::json!({"key": k, "value": v})).collect::<Vec<_>>(),
            "metrics": metrics.iter().map(|m| serde_json::json!({
                "key": m.key, "value": m.value, "timestamp": ts, "step": m.step,
            })).collect::<Vec<_>>(),
        });
        let resp = self
            .auth(
                self.http
                    .post(self.tracking_url("runs/log-batch"))
                    .json(&body),
            )
            .send()
            .await
            .context("runs/log-batch")?;
        ok_json(resp, "runs/log-batch").await?;
        Ok(())
    }

    /// Close the MLflow run with its terminal status (`runs/update`).
    async fn update_run(&self, mlflow_run_id: &str, status: RunStatus) -> Result<()> {
        let body = serde_json::json!({
            "run_id": mlflow_run_id,
            "status": status.as_str(),
            "end_time": now_millis(),
        });
        let resp = self
            .auth(self.http.post(self.tracking_url("runs/update")).json(&body))
            .send()
            .await
            .context("runs/update")?;
        ok_json(resp, "runs/update").await?;
        Ok(())
    }

    /// Push one re-encoded OTLP protobuf trace body to `<uri>/v1/traces` under `experiment_id`
    /// (protobuf content type, bearer, `x-mlflow-experiment-id` — the MLflow ≥ 3.6 OTLP contract).
    async fn push_traces(&self, experiment_id: &str, protobuf: Vec<u8>) -> Result<()> {
        let resp = self
            .auth(
                self.http
                    .post(format!("{}/v1/traces", self.base))
                    .header(reqwest::header::CONTENT_TYPE, "application/x-protobuf")
                    .header("x-mlflow-experiment-id", experiment_id)
                    .body(protobuf),
            )
            .send()
            .await
            .context("POST /v1/traces")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("POST /v1/traces failed: {status} {}", truncate(&text, 300));
        }
        Ok(())
    }
}

/// Decode a tracking-REST response, erroring with the server's body on a non-2xx.
async fn ok_json(resp: reqwest::Response, what: &str) -> Result<Value> {
    let status = resp.status();
    let text = resp.text().await.context(what.to_string())?;
    if !status.is_success() {
        bail!("{what} failed: {status} {}", truncate(&text, 300));
    }
    if text.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).with_context(|| format!("{what} response body"))
}

fn truncate(s: &str, max: usize) -> String {
    let capped: String = s.chars().take(max).collect();
    if capped.len() < s.len() {
        format!("{capped}…")
    } else {
        capped
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// Export one run to MLflow: create the experiment/run, log params + metrics, push its traces,
/// close the run. Returns `(mlflow_run_id, experiment_id)` on success for the bookkeeping row.
async fn export_one(
    db: &Db,
    cfg: &MlflowConfig,
    client: &MlflowClient,
    run_id: &str,
) -> Result<(String, String)> {
    let export = assemble_run_export(db, cfg, run_id)
        .await?
        .with_context(|| format!("run {run_id} vanished before export"))?;

    let experiment_id = client.experiment_id(&export.experiment).await?;
    let mlflow_run_id = client.create_run(&experiment_id, &export).await?;
    client
        .log_batch(&mlflow_run_id, &export.params, &export.metrics)
        .await?;
    for body in export.traces {
        client.push_traces(&experiment_id, body).await?;
    }
    client.update_run(&mlflow_run_id, export.status).await?;
    Ok((mlflow_run_id, experiment_id))
}

/// One sweep: export every terminal run the bookkeeping doesn't yet mark `exported`. Each run's
/// failure marks its row and is retried next sweep — one bad run never stalls the rest. Returns the
/// number newly exported.
async fn export_sweep(db: &Db, cfg: &MlflowConfig) -> Result<usize> {
    let client = MlflowClient::new(cfg)?;
    let run_ids =
        crate::runs::work_pods::runs_awaiting_mlflow_export(db.pool(), SWEEP_BATCH).await?;
    let mut exported = 0;
    for run_id in run_ids {
        crate::runs::work_pods::mark_mlflow_pending(db.pool(), &run_id).await?;
        match export_one(db, cfg, &client, &run_id).await {
            Ok((mlflow_run_id, experiment_id)) => {
                crate::runs::work_pods::mark_mlflow_exported(
                    db.pool(),
                    &run_id,
                    &mlflow_run_id,
                    &experiment_id,
                )
                .await?;
                exported += 1;
            }
            Err(e) => {
                let detail = format!("{e:#}");
                tracing::warn!(run_id = %run_id, error = %detail, "mlflow export failed (retrying next sweep)");
                crate::runs::work_pods::mark_mlflow_failed(db.pool(), &run_id, &detail).await?;
            }
        }
    }
    Ok(exported)
}

/// The resident export task: sweep on `cfg.sweep_interval` until `shutdown` fires. Spawned by the
/// binary ONLY when [`MlflowConfig::from_env`] returned a config, so an un-configured deployment
/// never runs this loop. A sweep error is logged, never fatal.
pub async fn export_loop(db: Db, cfg: MlflowConfig, shutdown: Arc<tokio::sync::Notify>) {
    let mut timer = tokio::time::interval(cfg.sweep_interval);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let signal = shutdown.notified();
    tokio::pin!(signal);
    signal.as_mut().enable();
    tracing::info!(uri = %cfg.tracking_uri, "mlflow exporter started");
    loop {
        tokio::select! {
            _ = timer.tick() => {
                match export_sweep(&db, &cfg).await {
                    Ok(n) if n > 0 => tracing::info!(exported = n, "mlflow export sweep"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = format!("{e:#}"), "mlflow export sweep failed (continuing)"),
                }
            }
            _ = signal.as_mut() => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn traces_payload() -> Value {
        json!({
            "resourceSpans": [{
                "resource": {"attributes": [
                    {"key": "service.name", "value": {"stringValue": "claude-code"}}
                ]},
                "scopeSpans": [{
                    "spans": [{
                        "traceId": "5b8efff798038103d269b633813fc60c",
                        "spanId": "eee19b7ec3c1b174",
                        "parentSpanId": "eee19b7ec3c1b173",
                        "name": "tool",
                        "kind": 1,
                        "startTimeUnixNano": "1544712660000000000",
                        "endTimeUnixNano": "1544712661000000000"
                    }]
                }]
            }]
        })
    }

    #[test]
    fn otlp_json_hex_ids_decode_to_bytes_and_resource_is_stamped() {
        let bytes = traces_to_protobuf(
            &traces_payload(),
            &[("crucible.run_id", "run-7"), ("crucible.domain", "o/r")],
        )
        .expect("re-encode");
        let back = ExportTraceServiceRequest::decode(bytes.as_slice()).expect("decode");
        let rs = &back.resource_spans[0];
        let span = &rs.scope_spans[0].spans[0];
        // hex → bytes, exactly like agentic-ci's _fixup_ids.
        assert_eq!(span.trace_id.len(), 16);
        assert_eq!(span.trace_id[0], 0x5b);
        assert_eq!(span.trace_id[15], 0x0c);
        assert_eq!(span.span_id.len(), 8);
        assert_eq!(span.span_id[0], 0xee);
        assert_eq!(span.parent_span_id.len(), 8);
        assert_eq!(span.name, "tool");
        // The run/domain resource stamp rode through (original service.name attr preserved).
        let attrs = &rs.resource.as_ref().expect("resource").attributes;
        let get = |k: &str| {
            attrs
                .iter()
                .find(|a| a.key == k)
                .and_then(|a| match &a.value {
                    Some(AnyValue {
                        value: Some(AnyValueEnum::StringValue(s)),
                    }) => Some(s.clone()),
                    _ => None,
                })
        };
        assert_eq!(get("crucible.run_id").as_deref(), Some("run-7"));
        assert_eq!(get("crucible.domain").as_deref(), Some("o/r"));
        assert_eq!(get("service.name").as_deref(), Some("claude-code"));
    }

    #[test]
    fn experiment_mapping_prefers_override_then_default_then_domain() {
        let mut map = BTreeMap::new();
        map.insert("owner/special".to_string(), "special-exp".to_string());
        let cfg = MlflowConfig {
            tracking_uri: "http://mlflow".to_string(),
            token: None,
            experiment_map: map,
            default_experiment: "crucible".to_string(),
            sweep_interval: Duration::from_secs(300),
        };
        assert_eq!(cfg.experiment_for("owner/special"), "special-exp");
        assert_eq!(cfg.experiment_for("owner/other"), "crucible");

        // With no default, the domain is its own experiment.
        let cfg2 = MlflowConfig {
            default_experiment: String::new(),
            ..cfg
        };
        assert_eq!(cfg2.experiment_for("owner/other"), "owner/other");
    }

    #[test]
    fn parse_experiment_map_skips_malformed() {
        let m = parse_experiment_map("a/b=exp1, c/d = exp2 ,junk,=x,y=");
        assert_eq!(m.get("a/b").map(String::as_str), Some("exp1"));
        assert_eq!(m.get("c/d").map(String::as_str), Some("exp2"));
        assert_eq!(m.len(), 2, "junk / empty-key / empty-value entries dropped");
    }

    #[test]
    fn tokens_and_model_from_metric_exports() {
        let records = vec![
            OtelRecord {
                path: "/v1/metrics".to_string(),
                payload: json!({"resourceMetrics": [{"scopeMetrics": [{"metrics": [{
                    "name": "claude_code.token.usage",
                    "sum": {"dataPoints": [
                        {"asInt": "100", "attributes": [{"key": "model", "value": {"stringValue": "claude-opus"}}]},
                        {"asInt": "20"}
                    ]}
                }]}]}]}),
            },
            OtelRecord {
                path: "/v1/traces".to_string(),
                payload: traces_payload(),
            },
        ];
        assert_eq!(total_tokens(&records), Some(120.0));
        assert_eq!(model_name(&records).as_deref(), Some("claude-opus"));
        assert_eq!(trace_payloads(&records).len(), 1);
    }

    #[test]
    fn total_tokens_is_none_without_telemetry() {
        let records = vec![OtelRecord {
            path: "/v1/traces".to_string(),
            payload: traces_payload(),
        }];
        assert_eq!(total_tokens(&records), None);
    }

    #[test]
    fn run_status_maps_failures() {
        assert_eq!(RunStatus::from_run_status("done"), RunStatus::Finished);
        assert_eq!(RunStatus::from_run_status("failed"), RunStatus::Failed);
        assert_eq!(RunStatus::from_run_status("no-session"), RunStatus::Failed);
    }

    // ---- e2e against a real local HTTP listener (no mocks) --------------------------------------

    /// Every request the fake MLflow server received: enough to assert the wire contract.
    #[derive(Clone)]
    struct CapturedReq {
        path: String,
        content_type: Option<String>,
        authorization: Option<String>,
        experiment_header: Option<String>,
        body: Vec<u8>,
    }

    type Captured = std::sync::Arc<std::sync::Mutex<Vec<CapturedReq>>>;

    fn record(cap: &Captured, path: &str, headers: &axum::http::HeaderMap, body: &[u8]) {
        let get = |h: &str| {
            headers
                .get(h)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        cap.lock().expect("lock").push(CapturedReq {
            path: path.to_string(),
            content_type: get("content-type"),
            authorization: get("authorization"),
            experiment_header: get("x-mlflow-experiment-id"),
            body: body.to_vec(),
        });
    }

    /// Spin up a minimal MLflow-shaped HTTP server on a random loopback port, capturing every
    /// request. Returns its base URL + the capture handle. A real listener — the HTTP path is
    /// exercised end to end, no mock transport.
    async fn fake_mlflow() -> (String, Captured) {
        use axum::extract::State;
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::{get, post};
        use axum::{Json, Router};

        let cap: Captured = Default::default();
        async fn get_by_name(
            State(cap): State<Captured>,
            headers: HeaderMap,
            body: axum::body::Bytes,
        ) -> (StatusCode, Json<Value>) {
            record(&cap, "experiments/get-by-name", &headers, &body);
            // Not found → the client takes the create path.
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error_code": "RESOURCE_DOES_NOT_EXIST"})),
            )
        }
        async fn create_exp(
            State(cap): State<Captured>,
            headers: HeaderMap,
            body: axum::body::Bytes,
        ) -> Json<Value> {
            record(&cap, "experiments/create", &headers, &body);
            Json(json!({"experiment_id": "exp-42"}))
        }
        async fn create_run(
            State(cap): State<Captured>,
            headers: HeaderMap,
            body: axum::body::Bytes,
        ) -> Json<Value> {
            record(&cap, "runs/create", &headers, &body);
            Json(json!({"run": {"info": {"run_id": "mlrun-99"}}}))
        }
        async fn log_batch(
            State(cap): State<Captured>,
            headers: HeaderMap,
            body: axum::body::Bytes,
        ) -> Json<Value> {
            record(&cap, "runs/log-batch", &headers, &body);
            Json(json!({}))
        }
        async fn update_run(
            State(cap): State<Captured>,
            headers: HeaderMap,
            body: axum::body::Bytes,
        ) -> Json<Value> {
            record(&cap, "runs/update", &headers, &body);
            Json(json!({}))
        }
        async fn traces(
            State(cap): State<Captured>,
            headers: HeaderMap,
            body: axum::body::Bytes,
        ) -> StatusCode {
            record(&cap, "/v1/traces", &headers, &body);
            StatusCode::OK
        }

        let app = Router::new()
            .route("/api/2.0/mlflow/experiments/get-by-name", get(get_by_name))
            .route("/api/2.0/mlflow/experiments/create", post(create_exp))
            .route("/api/2.0/mlflow/runs/create", post(create_run))
            .route("/api/2.0/mlflow/runs/log-batch", post(log_batch))
            .route("/api/2.0/mlflow/runs/update", post(update_run))
            .route("/v1/traces", post(traces))
            .with_state(cap.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), cap)
    }

    fn gzip(s: &str) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(s.as_bytes()).expect("gz");
        enc.finish().expect("finish")
    }

    async fn seed_run(pool: &sqlx::PgPool, run_id: &str, pod: &str) {
        sqlx::query("INSERT INTO issues (key, repo, status, priority, updated_at) VALUES ($1, $2, 'ranked', 0, '2026-07-07T00:00:00Z')")
            .bind("owner/repo#1").bind("owner/repo").execute(pool).await.expect("issue");
        sqlx::query("INSERT INTO scopes (id, issue) VALUES (1, $1)")
            .bind("owner/repo#1")
            .execute(pool)
            .await
            .expect("scope");
        sqlx::query("INSERT INTO runs (run_id, scope, status, pod, best_score, cost_usd) VALUES ($1, 1, 'done', $2, 0.87, 1.25)")
            .bind(run_id).bind(pod).execute(pool).await.expect("run");
        sqlx::query("INSERT INTO candidates (run_id, kind, lane, iter, score, decision) VALUES ($1, 'deep', 0, 2, 0.87, 'kept')")
            .bind(run_id).execute(pool).await.expect("candidate");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn export_sweep_pushes_to_a_real_listener_and_is_idempotent(pool: sqlx::PgPool) {
        let run_id = "run-2026-07-07-abc";
        let pod = "crucible-run-abc";
        seed_run(&pool, run_id, pod).await;

        // The run's otel-log evidence: one metrics export (tokens+model) + one traces export.
        let jsonl = format!(
            "{}\n{}\n",
            json!({"ts": 1, "path": "/v1/metrics", "payload": {"resourceMetrics": [{"scopeMetrics": [{"metrics": [{
                "name": "claude_code.token.usage",
                "sum": {"dataPoints": [{"asInt": "1500", "attributes": [{"key": "model", "value": {"stringValue": "claude-opus"}}]}]}
            }]}]}]}}),
            json!({"ts": 2, "path": "/v1/traces", "payload": traces_payload()}),
        );
        crate::runs::blob_store::put_artifact_bytes(
            &pool,
            &crate::runs::blob_store::ArtifactOwner::PodEvidence {
                pod: pod.to_string(),
            },
            "otel-log",
            u64::MAX,
            gzip(&jsonl),
        )
        .await
        .expect("store otel-log");

        let (base, cap) = fake_mlflow().await;
        let cfg = MlflowConfig {
            tracking_uri: base,
            token: Some("sekret".to_string()),
            experiment_map: BTreeMap::new(),
            default_experiment: "crucible".to_string(),
            sweep_interval: Duration::from_secs(300),
        };
        let db = Db::new(pool.clone());

        let n = export_sweep(&db, &cfg).await.expect("sweep");
        assert_eq!(n, 1, "the one terminal run exported");

        // Bookkeeping marked it exported with the server's ids.
        let row = crate::runs::work_pods::mlflow_export(db.pool(), run_id)
            .await
            .expect("q")
            .expect("row");
        assert_eq!(row.state, crate::runs::model::MlflowExportState::Exported);
        assert_eq!(row.mlflow_run_id.as_deref(), Some("mlrun-99"));
        assert_eq!(row.experiment_id.as_deref(), Some("exp-42"));

        let reqs = cap.lock().expect("lock").clone();
        let paths: Vec<&str> = reqs.iter().map(|r| r.path.as_str()).collect();
        assert!(
            paths.contains(&"experiments/create"),
            "created the experiment"
        );
        assert!(paths.contains(&"runs/create"));
        assert!(paths.contains(&"runs/log-batch"));
        assert!(paths.contains(&"runs/update"));

        // Every request carried the bearer.
        for r in &reqs {
            assert_eq!(
                r.authorization.as_deref(),
                Some("Bearer sekret"),
                "bearer on {}",
                r.path
            );
        }

        // The trace push: protobuf content type, experiment header, a body that decodes to the span
        // with the stamped resource attrs.
        let trace = reqs
            .iter()
            .find(|r| r.path == "/v1/traces")
            .expect("trace push");
        assert_eq!(
            trace.content_type.as_deref(),
            Some("application/x-protobuf")
        );
        assert_eq!(trace.experiment_header.as_deref(), Some("exp-42"));
        let decoded = ExportTraceServiceRequest::decode(trace.body.as_slice()).expect("protobuf");
        let attrs = &decoded.resource_spans[0]
            .resource
            .as_ref()
            .expect("res")
            .attributes;
        assert!(
            attrs.iter().any(|a| a.key == "crucible.run_id"),
            "run id stamped as a resource attr for experiment-level correlation"
        );

        // log-batch carried the params + metrics (gate score at step 0 AND per-candidate step 2).
        let batch = reqs
            .iter()
            .find(|r| r.path == "runs/log-batch")
            .expect("batch");
        let b: Value = serde_json::from_slice(&batch.body).expect("json");
        let metric_steps: Vec<i64> = b["metrics"]
            .as_array()
            .expect("metrics")
            .iter()
            .filter(|m| m["key"] == "gate_score")
            .filter_map(|m| m["step"].as_i64())
            .collect();
        assert!(
            metric_steps.contains(&0) && metric_steps.contains(&2),
            "turn→step mapping: {metric_steps:?}"
        );
        let has_tokens = b["metrics"]
            .as_array()
            .expect("m")
            .iter()
            .any(|m| m["key"] == "tokens_total" && m["value"] == 1500.0);
        assert!(has_tokens, "authoritative tokens from the otel-log");

        // Idempotency: a second sweep exports nothing and issues no new push.
        let before = cap.lock().expect("lock").len();
        let n2 = export_sweep(&db, &cfg).await.expect("sweep2");
        assert_eq!(n2, 0, "an already-exported run is skipped");
        assert_eq!(
            cap.lock().expect("lock").len(),
            before,
            "no new server calls"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn read_otel_records_decompresses_gzip_and_missing_is_empty(pool: sqlx::PgPool) {
        let jsonl = "{\"ts\":1,\"path\":\"/v1/traces\",\"payload\":{\"resourceSpans\":[]}}\n";
        crate::runs::blob_store::put_artifact_bytes(
            &pool,
            &crate::runs::blob_store::ArtifactOwner::PodEvidence {
                pod: "pod-otel".to_string(),
            },
            "otel-log",
            u64::MAX,
            gzip(jsonl),
        )
        .await
        .expect("store");

        let recs = read_otel_records(&pool, "pod-otel").await.expect("read");
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].path, "/v1/traces");

        assert!(
            read_otel_records(&pool, "no-such-pod")
                .await
                .expect("missing ok")
                .is_empty(),
            "a missing otel-log is an empty vec, never an error"
        );
    }
}
