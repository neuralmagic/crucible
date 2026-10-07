use crate::api::dto::*;
use crate::api::state::*;
use crate::model::SortDir;
use crate::runs::model::TaskName;
use crate::runs::model::{RunKindFilter, RunQuery, RunRow, RunSort};
use crate::wire_enum::{parse_opt, parse_or_default};
use anyhow::Context;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

const RUNS_LIST_DEFAULT: i64 = 50;

const RUNS_LIST_MAX: i64 = 500;

/// One leaderboard row: a run joined to the issue key + repo it ran for, with the creation stamp
/// derived from the run id. The parity target for the SSG's `index.html` leaderboard table.
#[derive(Debug, Serialize, ToSchema)]
pub struct RunRowDto {
    pub run_id: String,
    /// The name people read the run by (`benevolent-monkey`); null for a run from before names.
    #[schema(value_type = Option<String>)]
    pub name: Option<crate::runs::names::RunName>,
    pub issue_key: Option<String>,
    pub repo: Option<String>,
    pub status: String,
    pub best_score: Option<f64>,
    pub cost_usd: Option<f64>,
    /// The kept-candidate PR url for this run, or null when the run kept no PR — the runs
    /// leaderboard renders it as an out-link to the opened draft.
    pub pr_url: Option<String>,
    /// RFC3339 UTC creation time parsed off the run id's leading stamp; null for a non-stamped id.
    pub created: Option<String>,
    /// Every measured candidate score in iteration order. Empty when the run has measured nothing
    /// yet. The first element is the run's baseline, so the list page charts a run without a
    /// per-run request.
    pub score_series: Vec<f64>,
    /// Task attempts that ended `transport`: lost to infrastructure, not to a verdict.
    pub transport_losses: i64,
}

impl RunRowDto {
    pub(crate) fn from_parts(r: RunRow, score_series: Vec<f64>) -> Self {
        RunRowDto {
            transport_losses: r.transport_losses,
            run_id: r.run_id,
            name: r.name,
            issue_key: r.issue_key,
            repo: r.repo,
            status: r.status,
            best_score: r.best_score,
            cost_usd: r.cost_usd,
            pr_url: r.pr_url,
            created: r.created,
            score_series,
        }
    }
}

/// The raw `GET /api/runs` query string: filters, sorting, and paging.
#[derive(Debug, Deserialize, Default)]
pub(crate) struct RunsQuery {
    status: Option<String>,
    kind: Option<String>,
    repo: Option<String>,
    dispatch_target: Option<String>,
    sort: Option<String>,
    dir: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

impl RunsQuery {
    /// Parse into the strong [`RunQuery`], or a bad value's message (a 400, not a 500). `status` is a
    /// free string here (the `runs.status` column holds engine outcomes — finished/incomplete/
    /// running/… — not the issue-lifecycle [`Status`] enum), so it's matched as-is, never parsed.
    pub(crate) fn into_model(self) -> Result<RunQuery, String> {
        let sort = parse_or_default::<RunSort>(self.sort.as_deref())?;
        let dir = parse_or_default::<SortDir>(self.dir.as_deref())?;
        // clamp, not min: Postgres rejects a negative LIMIT outright; offset floors at 0.
        let limit = self
            .limit
            .unwrap_or(RUNS_LIST_DEFAULT)
            .clamp(0, RUNS_LIST_MAX);
        let offset = self.offset.unwrap_or(0).max(0);
        let kind = parse_opt::<RunKindFilter>(self.kind.as_deref())?;
        Ok(RunQuery {
            status: self.status.filter(|s| !s.is_empty()),
            kind,
            repo: self.repo.filter(|r| !r.is_empty()),
            dispatch_target: self.dispatch_target.filter(|t| !t.is_empty()),
            sort,
            dir,
            limit,
            offset,
        })
    }
}

#[utoipa::path(
    get,
    path = "/api/runs/{run_id}",
    params(
        ("run_id" = String, Path, description = "Run identifier")
    ),
    responses(
        (status = 200, description = "Run metadata and candidates", body = RunDetail),
        (status = 404, description = "Run not found", body = ErrorBody)
    )
)]
pub(crate) async fn get_run(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> Result<Response, AppError> {
    let Some(run) = crate::runs::store::get_run(state.db.pool(), &run_id).await? else {
        return Ok(not_found(format!("run not found: {run_id}")));
    };
    let (issue_key, repo) = crate::runs::store::run_issue_repo(state.db.pool(), &run_id).await?;
    let agent = match issue_key.as_deref() {
        Some(key) => crate::issues::store::get_issue(state.db.pool(), key)
            .await?
            .as_ref()
            .map(AgentPinDto::of)
            .unwrap_or_default(),
        None => AgentPinDto::default(),
    };
    let candidates = crate::runs::store::list_candidates_for_run(state.db.pool(), &run_id).await?;
    let transport_losses =
        crate::runs::task_results::count_transport_losses(state.db.pool(), &run_id).await?;
    let detail = RunDetail {
        run: RunDto::from_parts(run, issue_key, repo, agent, transport_losses),
        candidates: candidates.into_iter().map(CandidateDto::from).collect(),
    };
    Ok(Json(detail).into_response())
}

/// `GET /api/runs/{run_id}/graph` — the run's newest admitted work graph plus every task attempt
/// folded against it. A 404 covers both an unknown run and a run that never admitted a plan (every
/// run logged before the work-graph executor existed), which is what lets the SPA hide the panel
/// without a separate capability probe.
#[utoipa::path(
    get,
    path = "/api/runs/{run_id}/graph",
    params(
        ("run_id" = String, Path, description = "Run identifier")
    ),
    responses(
        (status = 200, description = "The admitted task graph and each task's per-iteration results", body = RunGraphDto),
        (status = 404, description = "Run not found, or the run admitted no plan", body = ErrorBody)
    )
)]
pub(crate) async fn get_run_graph(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> Result<Response, AppError> {
    let Some(plan) = crate::runs::task_results::latest_run_plan(state.db.pool(), &run_id).await?
    else {
        return Ok(not_found(format!("no task graph for run: {run_id}")));
    };
    let tasks: Vec<crucible_contract::session::PlanTaskWire> =
        serde_json::from_str(&plan.graph_json)
            .with_context(|| format!("decoding the stored task graph for run {run_id}"))?;
    let results = crate::runs::task_results::list_task_results(state.db.pool(), &run_id).await?;
    let tasks: Vec<PlanTaskDto> = tasks.into_iter().map(PlanTaskDto::from).collect();
    // `None` here is a revision that stored no exposure, which the wire has to keep distinct from
    // a pack that declares no outputs.
    let (issue_key, _) = crate::runs::store::run_issue_repo(state.db.pool(), &run_id).await?;
    let exposure = match issue_key.as_deref() {
        Some(key) => {
            match crate::launches::store::exposure_for_issue(state.db.pool(), key).await? {
                Ok(exposure) => exposure,
                Err(refused) => {
                    tracing::warn!(run_id, error = %refused, "run graph served without outputs");
                    None
                }
            }
        }
        None => None,
    };
    let names: Vec<TaskName> = tasks
        .iter()
        .map(|t| TaskName::from(t.name.as_str()))
        .collect();
    let outputs = exposure.map(|e| {
        e.outputs
            .iter()
            .map(|o| GraphOutputDto::from((o, names.as_slice())))
            .collect()
    });
    let read = tasks.iter().any(|t| !t.over.is_empty() || t.kind == ROUTE);
    let session = match read {
        true => {
            crate::runs::task_evidence::run_session(state.db.pool(), &state.scratch_dir, &run_id)
                .await?
        }
        false => None,
    };
    let fanout = mapped_widths(session.as_deref(), &tasks);
    let decisions = route_decisions(session.as_deref(), &tasks);
    Ok(Json(RunGraphDto {
        plan_version: plan.plan_version,
        tasks,
        results: results.into_iter().map(TaskResultDto::from).collect(),
        outputs,
        fanout,
        decisions,
    })
    .into_response())
}

/// How wide the session says each mapped task was spread. A run with no stored session reports
/// none: the width is unknown, not zero.
fn mapped_widths(session: Option<&str>, tasks: &[PlanTaskDto]) -> Vec<FanOutCountDto> {
    let Some(session) = session else {
        return Vec::new();
    };
    tasks
        .iter()
        .filter(|t| !t.over.is_empty())
        .filter_map(|t| {
            let items = crate::runs::task_evidence::fanout_width(session, &t.name, &t.over)?;
            let not_taken = crate::runs::task_evidence::session_result(session, &t.name)
                .and_then(|folded| folded.payload?.get("not_taken")?.as_i64())
                .unwrap_or(0);
            Some(FanOutCountDto {
                task: t.name.clone(),
                items: i64::try_from(items).unwrap_or(i64::MAX),
                not_taken,
            })
        })
        .collect()
}

const ROUTE: &str = "route";

/// The mapped node a `node[key]` instance name belongs to. A declared name may not hold a bracket.
fn instance_node(task: &str) -> Option<&str> {
    let (node, rest) = task.split_once('[')?;
    (!node.is_empty() && rest.ends_with(']')).then_some(node)
}

/// What each route decided, by the last output the session recorded for each route task and each
/// instance of a mapped route. A mapped route's node tallies its instances' labels; its own folded
/// output is a count, not a decision.
fn route_decisions(session: Option<&str>, tasks: &[PlanTaskDto]) -> Vec<RouteDecisionDto> {
    use std::collections::BTreeMap;
    let Some(session) = session else {
        return Vec::new();
    };
    let routes: BTreeMap<&str, bool> = tasks
        .iter()
        .filter(|t| t.kind == ROUTE)
        .map(|t| (t.name.as_str(), !t.over.is_empty()))
        .collect();
    if routes.is_empty() {
        return Vec::new();
    }
    let mut last: BTreeMap<String, Option<serde_json::Value>> = BTreeMap::new();
    for line in session.lines() {
        let Ok(serde_json::Value::Object(event)) = serde_json::from_str(line) else {
            continue;
        };
        if event.get("kind").and_then(serde_json::Value::as_str) != Some("task_result") {
            continue;
        }
        let Some(task) = event.get("task").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let decides = match instance_node(task) {
            Some(node) => routes.get(node) == Some(&true),
            None => routes.get(task) == Some(&false),
        };
        if decides {
            last.insert(task.to_string(), event.get("output").cloned());
        }
    }
    let mut tallies: BTreeMap<(String, String), BTreeMap<String, i64>> = BTreeMap::new();
    for (task, output) in last {
        let Some(decision) = output.and_then(|output| {
            serde_json::from_value::<crucible_contract::decision::Decision>(output).ok()
        }) else {
            continue;
        };
        let node = instance_node(&task).map(str::to_string);
        for (question, answer) in decision.0 {
            for owner in [Some(task.clone()), node.clone()].into_iter().flatten() {
                *tallies
                    .entry((owner, question.to_string()))
                    .or_default()
                    .entry(answer.label.to_string())
                    .or_default() += 1;
            }
        }
    }
    tallies
        .into_iter()
        .map(|((task, question), labels)| RouteDecisionDto {
            task,
            question,
            labels: labels
                .into_iter()
                .map(|(label, count)| LabelCountDto { label, count })
                .collect(),
        })
        .collect()
}

/// `?from_seq=N` resume point for `GET /api/runs/{run_id}/live`. The `Last-Event-ID` header (set by
/// a reconnecting `EventSource`) wins over this when present.
#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct LiveQuery {
    from_seq: Option<u64>,
}

/// `GET /api/runs/{run_id}/live` — a read-only SSE relay of a *running* run's live session events,
/// streamed by dialing the loop pod's control bridge. Event types: `session` (raw session.jsonl
/// line), `status` (a snapshot), `phase` and `log` (spoke runs), and a terminal `end` (reason).
/// Every frame carries an `id` of `<session-seq>.<log-ordinal>`. An unknown run is a 404; a known
/// non-running run gets an immediate `end{run-not-running}`, so the UI can tell them apart. Resume
/// via `?from_seq=N` or the `Last-Event-ID` header (the header wins).
#[utoipa::path(
    get,
    path = "/api/runs/{run_id}/live",
    params(
        ("run_id" = String, Path, description = "Run identifier"),
        ("from_seq" = Option<u64>, Query, description = "Resume after this seq (Last-Event-ID header wins)")
    ),
    responses(
        (status = 200, description = "SSE stream of live session events (session/status/end)", content_type = "text/event-stream"),
        (status = 404, description = "Run not found", body = ErrorBody)
    )
)]
pub(crate) async fn live_run(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
    Query(q): Query<LiveQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    // The Last-Event-ID header (a reconnecting EventSource sends the last id it saw) wins over the
    // query param, so a resume "just works" without the client re-deriving from_seq.
    let resume = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<crate::runs::live::StreamCursor>().ok())
        .or_else(|| {
            q.from_seq
                .map(crate::runs::live::StreamCursor::from_session)
        })
        .unwrap_or_default();
    crate::runs::live::live_response(&state, &run_id, resume).await
}

/// `GET /api/turns/{pod_name}/live` — a read-only SSE relay of a *running* turn pod's lifecycle +
/// logs (`crate::runs::turn_live`). Event types: `phase` (pod phase, on connect and change), `progress`
/// (a `CRUCIBLE_SCOPE_PROGRESS` round-boundary beat, JSON), `log` (a capped raw log line), and a
/// terminal `end` (turn-not-running / pod-gone / completed: <phase> / timeout / error: …). An
/// unknown pod name is a 404; a known non-running turn gets an immediate `end{turn-not-running}`.
#[utoipa::path(
    get,
    path = "/api/turns/{pod_name}/live",
    params(
        ("pod_name" = String, Path, description = "The turn pod's k8s object name (the work-pod ledger key)")
    ),
    responses(
        (status = 200, description = "SSE stream of live turn events (phase/progress/log/end)", content_type = "text/event-stream"),
        (status = 404, description = "Turn not found", body = ErrorBody)
    )
)]
#[cfg(feature = "autoresearch")]
pub(crate) async fn live_turn(
    State(state): State<ApiState>,
    Path(pod_name): Path<String>,
) -> Response {
    crate::runs::turn_live::live_response(&state, &pod_name).await
}

/// `GET /api/runs` — the runs leaderboard: filter by `status`/`repo`/`dispatch_target`, sort by
/// `best_score`/`cost`/`created` (+ `dir`), page with `limit`/`offset`.
#[utoipa::path(
    get,
    path = "/api/runs",
    params(
        ("status" = Option<String>, Query, description = "Filter by run status (finished|incomplete|running|…)"),
        ("repo" = Option<String>, Query, description = "Filter by repository (owner/repo)"),
        ("dispatch_target" = Option<String>, Query, description = "Filter by the exact cluster target recorded on the run"),
        ("sort" = Option<String>, Query, description = "Sort key (best_score|cost|created); default created"),
        ("dir" = Option<String>, Query, description = "Sort direction (asc|desc); default desc"),
        ("limit" = Option<i64>, Query, description = "Max runs to return (default 50, max 500)"),
        ("offset" = Option<i64>, Query, description = "Rows to skip for paging (default 0)"),
    ),
    responses(
        (status = 200, description = "Runs leaderboard rows", body = Vec<RunRowDto>),
        (status = 400, description = "Invalid query parameters", body = ErrorBody)
    )
)]
pub(crate) async fn list_runs(
    State(state): State<ApiState>,
    Query(q): Query<RunsQuery>,
) -> Result<Response, AppError> {
    let query = match q.into_model() {
        Ok(q) => q,
        Err(msg) => {
            return Ok(bad_request(msg));
        }
    };
    let rows = crate::runs::store::list_runs_page(state.db.pool(), &query).await?;
    let run_ids: Vec<String> = rows.iter().map(|r| r.run_id.clone()).collect();
    let mut series = crate::runs::store::score_series_for_runs(state.db.pool(), &run_ids).await?;
    let dtos: Vec<RunRowDto> = rows
        .into_iter()
        .map(|r| {
            let s = series.remove(&r.run_id).unwrap_or_default();
            RunRowDto::from_parts(r, s)
        })
        .collect();
    Ok(Json(dtos).into_response())
}

/// `GET /api/runs/{run_id}/iterations` — the run's per-candidate rows (iter, lane, kind, score,
/// decision, pr_url) ordered by iteration/lane index: the data the SSG's per-run decision table and
/// score curve render from. A 404 for an unknown run.
#[utoipa::path(
    get,
    path = "/api/runs/{run_id}/iterations",
    params(
        ("run_id" = String, Path, description = "Run identifier")
    ),
    responses(
        (status = 200, description = "The run's candidates, ordered by iteration/lane", body = Vec<CandidateDto>),
        (status = 404, description = "Run not found", body = ErrorBody)
    )
)]
pub(crate) async fn get_run_iterations(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> Result<Response, AppError> {
    if crate::runs::store::get_run(state.db.pool(), &run_id)
        .await?
        .is_none()
    {
        return Ok(not_found(format!("run not found: {run_id}")));
    }
    let candidates =
        crate::runs::store::list_candidates_for_run_by_iter(state.db.pool(), &run_id).await?;
    let dtos: Vec<CandidateDto> = candidates.into_iter().map(CandidateDto::from).collect();
    Ok(Json(dtos).into_response())
}

/// `GET /api/runs/{run_id}/artifacts` — the run's artifact manifest, distinct from the
/// `/artifacts/{path}` proxy below (axum routes the tail-less literal ahead of the catch-all).
/// Every listed path is servable via the proxy. A local `session_uri` prefix is walked for real
/// files with sizes; an `s3://` prefix gets the derived list (the standard names plus
/// `diffs/iter-<N>.patch` per recorded deep iteration) with null sizes — the controller has no S3
/// client by design. A run with no session evidence gets an empty manifest, not a 404.
#[utoipa::path(
    get,
    path = "/api/runs/{run_id}/artifacts",
    params(
        ("run_id" = String, Path, description = "Run identifier")
    ),
    responses(
        (status = 200, description = "The run's servable artifacts (empty when the run recorded no session evidence)", body = crate::runs::artifacts::ArtifactManifest),
        (status = 404, description = "Run not found", body = ErrorBody)
    )
)]
pub(crate) async fn list_run_artifacts(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> Result<Response, AppError> {
    let Some(run) = crate::runs::store::get_run(state.db.pool(), &run_id).await? else {
        return Ok(not_found(format!("run not found: {run_id}")));
    };
    let Some(prefix) = run
        .session_uri
        .as_deref()
        .and_then(crate::runs::artifacts::prefix_of)
    else {
        return Ok(
            Json(crate::runs::artifacts::ArtifactManifest { entries: vec![] }).into_response(),
        );
    };
    let candidates = crate::runs::store::list_candidates_for_run(state.db.pool(), &run_id).await?;
    let mut iters: Vec<i64> = candidates.iter().filter_map(|c| c.iter).collect();
    iters.sort_unstable();
    iters.dedup();
    Ok(Json(crate::runs::artifacts::build_manifest(prefix, &iters).await).into_response())
}

/// `GET /api/runs/{run_id}/artifacts/{path}` — the lazy artifact proxy. `path` is one of the
/// whitelisted artifacts (`session.jsonl`, `RESULTS.md`, `summary.json`, `flow.json`, `flow.html`,
/// `diffs/<file>`, or any manifest-listable relative path of 1-3 safe non-dot segments); anything
/// else is a 400. Resolves the object under the run's `session_uri` prefix and streams it back with
/// an immutable cache. Routed as a catch-all (see [`router`]); documented here.
#[utoipa::path(
    get,
    path = "/api/runs/{run_id}/artifacts/{path}",
    params(
        ("run_id" = String, Path, description = "Run identifier"),
        ("path" = String, Path, description = "Whitelisted artifact path: session.jsonl | RESULTS.md | summary.json | flow.json | flow.html | diffs/<file> | any manifest-listed relative path (1-3 safe non-dot segments)")
    ),
    responses(
        (status = 200, description = "The artifact bytes (streamed, immutable cache)"),
        (status = 304, description = "Not modified (If-None-Match matched)"),
        (status = 400, description = "Path is not a whitelisted artifact"),
        (status = 404, description = "Run or artifact not found", body = ErrorBody)
    )
)]
pub(crate) async fn get_artifact(
    State(state): State<ApiState>,
    Path((run_id, path)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    let run = match crate::runs::store::get_run(state.db.pool(), &run_id).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            if let Some(m) = state.db.metrics() {
                m.record_artifact_request("run_not_found");
            }
            return not_found(format!("run not found: {run_id}"));
        }
        Err(e) => return AppError(e).into_response(),
    };
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    crate::runs::artifacts::serve_artifact(
        state.db.metrics(),
        state.db.pool(),
        run.session_uri.as_deref(),
        &run_id,
        &path,
        if_none_match,
    )
    .await
}

/// Build a parquet download response: octet-typed body + an attachment `Content-Disposition`.
fn parquet_response(bytes: Vec<u8>, filename: &str) -> Response {
    let mut resp = bytes.into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.apache.parquet"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    resp
}

/// `GET /api/export/runs.parquet` — the runs analytics table, generated on demand from the ledger
/// (replacing the SSG's `runs.parquet` sidecar so DuckDB reads it off the API).
#[utoipa::path(
    get,
    path = "/api/export/runs.parquet",
    responses(
        (status = 200, description = "runs.parquet (Apache Parquet)", content_type = "application/vnd.apache.parquet")
    )
)]
pub(crate) async fn export_runs(State(state): State<ApiState>) -> Result<Response, AppError> {
    let rows = crate::runs::store::export_run_rows(state.db.pool()).await?;
    let bytes = crate::runs::export::write_run_parquet(&rows)?;
    if let Some(m) = state.db.metrics() {
        m.record_export("runs");
    }
    Ok(parquet_response(bytes, "runs.parquet"))
}

/// `GET /api/export/iterations.parquet` — the per-iteration analytics table (replacing the SSG's
/// `iterations.parquet` sidecar).
#[utoipa::path(
    get,
    path = "/api/export/iterations.parquet",
    responses(
        (status = 200, description = "iterations.parquet (Apache Parquet)", content_type = "application/vnd.apache.parquet")
    )
)]
pub(crate) async fn export_iterations(State(state): State<ApiState>) -> Result<Response, AppError> {
    let rows = crate::runs::store::export_iteration_rows(state.db.pool()).await?;
    let bytes = crate::runs::export::write_iter_parquet(&rows)?;
    if let Some(m) = state.db.metrics() {
        m.record_export("iterations");
    }
    Ok(parquet_response(bytes, "iterations.parquet"))
}

#[cfg(test)]
mod tests {
    use crate::runs::api::runs::*;

    /// A plan task as the admitted plan stores it, with only the fields the fan-out count reads.
    fn task(name: &str, over: &str) -> PlanTaskDto {
        PlanTaskDto {
            name: name.to_string(),
            kind: "agent".to_string(),
            depends_on: Vec::new(),
            session: String::new(),
            needs: "all".to_string(),
            required: true,
            over: over.to_string(),
            max_fanout: 0,
            when: String::new(),
            keyed: Vec::new(),
        }
    }

    fn route(name: &str, over: &str) -> PlanTaskDto {
        PlanTaskDto {
            kind: ROUTE.to_string(),
            ..task(name, over)
        }
    }

    fn decided(task: &str, answers: &[(&str, &str)]) -> String {
        let output: serde_json::Map<String, serde_json::Value> = answers
            .iter()
            .map(|(question, label)| {
                (
                    question.to_string(),
                    serde_json::json!({"label": label, "confidence": 0.9,
                                       "probabilities": {label.to_string(): 0.9}}),
                )
            })
            .collect();
        format!(
            "{}\n",
            serde_json::json!({"v":1,"kind":"task_result","task":task,"status":"pass",
                               "attempts":1,"output":output})
        )
    }

    fn label_counts(labels: &[(&str, i64)]) -> Vec<LabelCountDto> {
        labels
            .iter()
            .map(|(label, count)| LabelCountDto {
                label: label.to_string(),
                count: *count,
            })
            .collect()
    }

    #[test]
    fn a_mapped_route_tallies_its_instances_and_each_instance_reports_its_own_label() {
        let session = [
            decided("gate", &[("ship", "yes")]),
            decided("triage[a]", &[("tier", "high")]),
            decided("triage[b]", &[("tier", "low")]),
            decided("triage[c]", &[("tier", "low")]),
            "{\"kind\":\"task_result\",\"task\":\"triage[d]\",\"status\":\"not_taken\",\"output\":null}\n".to_string(),
            "{\"kind\":\"task_result\",\"task\":\"triage\",\"status\":\"pass\",\"output\":{\"instances\":4,\"passed\":3,\"failed\":0,\"not_taken\":1}}\n".to_string(),
        ]
        .concat();
        let tasks = [
            route("gate", ""),
            route("triage", "scan.items"),
            task("scan", ""),
        ];
        let decisions = route_decisions(Some(&session), &tasks);
        let find = |task: &str| {
            decisions
                .iter()
                .find(|d| d.task == task)
                .unwrap_or_else(|| panic!("{task} decided: {decisions:?}"))
        };
        assert_eq!(find("gate").question, "ship");
        assert_eq!(find("gate").labels, label_counts(&[("yes", 1)]));
        assert_eq!(find("triage").question, "tier");
        assert_eq!(
            find("triage").labels,
            label_counts(&[("high", 1), ("low", 2)])
        );
        assert_eq!(find("triage[b]").labels, label_counts(&[("low", 1)]));
        assert!(
            decisions.iter().all(|d| d.task != "triage[d]"),
            "an instance not taken decided nothing"
        );
        assert_eq!(decisions.len(), 5, "{decisions:?}");
    }

    #[test]
    fn a_retried_route_reports_its_last_decision() {
        let session = [
            decided("gate", &[("ship", "no")]),
            decided("gate", &[("ship", "yes")]),
        ]
        .concat();
        let decisions = route_decisions(Some(&session), &[route("gate", "")]);
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].labels, label_counts(&[("yes", 1)]));
    }

    #[test]
    fn a_settled_fan_out_reports_the_instances_its_fold_counts_not_taken() {
        let session = "{\"kind\":\"task_result\",\"task\":\"fix\",\"status\":\"pass\",\"output\":{\"instances\":5,\"passed\":2,\"failed\":0,\"not_taken\":3}}\n";
        let widths = mapped_widths(Some(session), &[task("fix", "scan.items")]);
        assert_eq!(widths.len(), 1);
        assert_eq!(widths[0].items, 5);
        assert_eq!(widths[0].not_taken, 3);
        assert_eq!(
            mapped_widths(Some(SESSION), &[task("triage", "scan.issues")])[0].not_taken,
            0,
            "a fold that predates the count reads as zero"
        );
    }

    /// A triage run's session: `triage` folded over three issues, and `audit` is still fanning
    /// out over a field `scan` never emitted.
    const SESSION: &str = concat!(
        "{\"v\":1,\"kind\":\"plan_admitted\",\"plan_version\":1}\n",
        "{\"v\":1,\"kind\":\"task_result\",\"task\":\"scan\",\"status\":\"pass\",\"attempts\":1,\"output\":{\"issues\":[\"a\",\"b\",\"c\"]}}\n",
        "{\"v\":1,\"kind\":\"task_result\",\"task\":\"triage[a]\",\"status\":\"pass\",\"attempts\":1,\"output\":null}\n",
        "{\"v\":1,\"kind\":\"task_result\",\"task\":\"triage\",\"status\":\"pass\",\"attempts\":1,\"output\":{\"instances\":3,\"passed\":3,\"failed\":0}}\n",
    );

    #[test]
    fn only_mapped_tasks_the_session_can_count_are_reported() {
        let tasks = [
            task("scan", ""),
            task("triage", "scan.issues"),
            task("audit", "scan.missing"),
            task("report", ""),
        ];
        let widths = mapped_widths(Some(SESSION), &tasks);
        assert_eq!(widths.len(), 1, "{widths:?}");
        assert_eq!(widths[0].task, "triage");
        assert_eq!(widths[0].items, 3);
    }

    #[test]
    fn a_run_with_no_session_reports_no_widths() {
        let tasks = [task("triage", "scan.issues")];
        assert!(mapped_widths(None, &tasks).is_empty());
    }
}
