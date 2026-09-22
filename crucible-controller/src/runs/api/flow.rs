#![allow(clippy::disallowed_macros)]

use crate::api::dto::*;
use crate::api::state::*;
use crate::runs::artifacts::{FetchError, fetch_artifact, prefix_of};
use crate::runs::flow_enriched::{cache_path, datadog_env_ready, render_flow};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

// --- span-enriched flow report ------------------------------------------------

/// The `Cache-Control` for a rendered flow page: both inputs (session log + trace) are immutable,
/// so the render is too.
const IMMUTABLE_CACHE: &str = "public, max-age=31536000, immutable";

/// The trace-id query. Hex/alphanumeric only — the same `plain_token` rule the engine's CLI
/// enforces, checked here first so a bad id is a 400, not a failed Datadog search.
#[derive(Debug, Deserialize)]
pub(crate) struct FlowEnrichedQuery {
    trace_id: String,
}

/// `GET /api/runs/{run_id}/flow-enriched?trace_id=<id>` — regenerate the run's flow.html
/// server-side WITH Datadog span timings, cache it on the scratch volume, serve it sandboxed.
/// The run's trace id is NOT recorded at ingest today, so the caller supplies it explicitly.
/// TODO(core): log the OTLP trace id into session.jsonl at run start so this param can default.
#[utoipa::path(
    get,
    path = "/api/runs/{run_id}/flow-enriched",
    params(
        ("run_id" = String, Path, description = "Run identifier"),
        ("trace_id" = String, Query, description = "Datadog trace id (plain alphanumeric token)"),
    ),
    responses(
        (status = 200, description = "Span-enriched flow.html (sandboxed, immutable cache)", content_type = "text/html"),
        (status = 304, description = "Not modified (If-None-Match matched)"),
        (status = 400, description = "trace_id missing or not a plain alphanumeric token", body = ErrorBody),
        (status = 404, description = "Run not found, run has no session evidence, or session.jsonl missing", body = ErrorBody),
        (status = 502, description = "Session fetch or Datadog span fetch failed", body = ErrorBody),
        (status = 503, description = "DD_API_KEY / DD_APP_KEY not configured on this deploy", body = ErrorBody),
    )
)]
pub(crate) async fn get_run_flow_enriched(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
    Query(q): Query<FlowEnrichedQuery>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    let record = |outcome: &str| {
        if let Some(m) = state.db.metrics() {
            m.record_flow_enriched(outcome);
        }
    };

    if !crate::runs::flow_enriched::valid_trace_id(&q.trace_id) {
        record("bad_trace");
        return Ok(bad_request("trace_id must be a plain alphanumeric token"));
    }
    let trace_id = q.trace_id;

    let etag = format!("\"{run_id}/flow-enriched/{trace_id}\"");
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    if if_none_match.is_some_and(|inm| inm == etag) {
        record("not_modified");
        return Ok(StatusCode::NOT_MODIFIED.into_response());
    }

    // Cache before the creds pre-flight: an already-rendered page keeps working even if the DD
    // keys rotate out of the deploy.
    let cached = cache_path(&state.scratch_dir, &run_id, &trace_id);
    if let Ok(bytes) = tokio::fs::read(&cached).await {
        record("cache_hit");
        return Ok(flow_response(bytes, &etag));
    }

    let dd = match datadog_env_ready() {
        Ok(dd) => dd,
        Err(var) => {
            record("no_creds");
            return Ok(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("{var} is not configured on this deploy"),
            ));
        }
    };

    let Some(run) = crate::runs::store::get_run(state.db.pool(), &run_id).await? else {
        record("no_evidence");
        return Ok(not_found(format!("run not found: {run_id}")));
    };
    let Some(uri) = run.session_uri.as_deref() else {
        record("no_evidence");
        return Ok(not_found(format!("run {run_id} has no session evidence")));
    };
    let Some(prefix) = prefix_of(uri) else {
        record("no_evidence");
        return Ok(not_found(format!(
            "run {run_id} session_uri has no resolvable prefix"
        )));
    };

    // `_guard` keeps an S3 temp download alive through the render below.
    let (session_path, _guard) =
        match fetch_artifact(state.db.pool(), prefix, "session.jsonl").await {
            Ok(f) => f,
            Err(FetchError::NotFound) => {
                record("no_evidence");
                return Ok(not_found(format!(
                    "artifact not found: {run_id}/session.jsonl"
                )));
            }
            Err(FetchError::Fetch(msg)) => {
                record("fetch_error");
                tracing::warn!(run_id, error = %msg, "flow-enriched: session fetch failed");
                return Ok(error_response(
                    StatusCode::BAD_GATEWAY,
                    "failed to fetch session log",
                ));
            }
        };

    let cache_dir = cached
        .parent()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("flow cache path {} has no parent", cached.display()))?;
    tokio::fs::create_dir_all(&cache_dir).await?;
    // Render into a temp file in the cache dir, then atomically rename in. A failed render drops
    // the temp file; racing duplicate requests are benign (same bytes).
    let tmp = tempfile::Builder::new()
        .suffix(".html")
        .tempfile_in(&cache_dir)?
        .into_temp_path();
    // DD error bodies can echo the request; log the detail, never reflect it to the client.
    if let Err(detail) = render_flow(&dd, &session_path, &trace_id, &tmp).await {
        record("render_error");
        tracing::warn!(run_id, detail, "flow-enriched: render failed");
        return Ok(error_response(
            StatusCode::BAD_GATEWAY,
            "flow render failed",
        ));
    }
    let bytes = tokio::fs::read(&tmp).await?;
    tmp.persist(&cached)
        .map_err(|e| anyhow::anyhow!("publishing flow cache entry: {e}"))?;
    // Best-effort: the cache would otherwise grow forever.
    if let Err(e) = crate::runs::flow_enriched::prune_cache(&state.scratch_dir, &cached).await {
        tracing::warn!(error = %e, "flow-cache prune failed");
    }

    record("ok");
    Ok(flow_response(bytes, &etag))
}

fn error_response(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(ErrorBody::new(msg))).into_response()
}

/// The rendered page, served with the same sandbox posture as the `flow.html` artifact proxy:
/// opaque-origin via CSP `sandbox allow-scripts`, immutable cache, stable ETag.
fn flow_response(bytes: Vec<u8>, etag: &str) -> Response {
    let mut resp = bytes.into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(IMMUTABLE_CACHE),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox allow-scripts"),
    );
    if let Ok(v) = HeaderValue::from_str(etag) {
        h.insert(header::ETAG, v);
    }
    resp
}
