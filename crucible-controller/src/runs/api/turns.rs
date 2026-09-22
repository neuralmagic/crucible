use crate::api::dto::*;
use crate::dto::dto;

use crate::api::state::*;

use crate::wire_enum::parse_opt;

use axum::extract::{Path, Query, State};

use axum::response::{IntoResponse, Response};

use serde::Deserialize;

// --- work-pod turns (the `work_pods` dispatch ledger) ----------------

const TURNS_LIST_DEFAULT: i64 = 100;

const TURNS_LIST_MAX: i64 = 500;

dto! {
    /// One `work_pods` ledger row — everything an operator debugging a turn storm needs: what ran,
    /// for which issue, how it ended, why it failed, and when.
    pub struct WorkPodDto: From<r: crate::runs::workpod::WorkPodRow> {
        /// The pod's k8s object name (the row's primary key).
        pub pod_name: String,
        /// The work kind (grounded-rank | scope | run).
        pub kind: String,
        /// The issue the turn ran for, or null for kind-less maintenance work.
        pub issue_key: Option<String>,
        /// The lifecycle state (queued | running | succeeded | failed | collected | swept).
        pub state: String = r.state.as_str().to_string(),
        /// The ledger tag its cost books under.
        pub cost_tag: String,
        /// A result summary for a collected turn (verdict/report + cost), or null.
        pub result: Option<LongText> = r.result.map(LongText::full),
        /// The failure reason for a failed turn (timeout, no verdict, dispatch error), or null.
        /// Truncated by `GET /api/turns`, whole on `GET /api/turns/{pod_name}` — as is `result`.
        pub error: Option<LongText> = r.error.map(LongText::full),
        pub created_at: String,
        pub updated_at: String,
        /// When the pod went terminal, or null while queued/running.
        pub terminal_at: Option<String>,
    }
}

impl WorkPodDto {
    /// Truncate `result` and `error` for the list wire. A failed turn's `error` is a formatted
    /// anyhow chain wrapping pod log material, so 500 rows of it is a multi-hundred-KB payload
    /// feeding a single collapsed `<pre>`.
    fn truncated_for_list(mut self) -> Self {
        self.result = self.result.map(LongText::truncate);
        self.error = self.error.map(LongText::truncate);
        self
    }
}

/// The raw `GET /api/turns` query string: `state=`/`kind=` filters + `limit`.
#[derive(Debug, Deserialize, Default)]
pub(crate) struct TurnsQuery {
    state: Option<String>,
    kind: Option<String>,
    limit: Option<i64>,
}

impl TurnsQuery {
    /// Validate into the exact spellings the `work_pods` columns store, or a bad value's message
    /// (a 400, not a 500). Both vocabularies are closed enums server-side, so anything else is a
    /// caller typo — better rejected loudly than silently matching nothing.
    fn into_filters(self) -> Result<(Option<&'static str>, Option<&'static str>, i64), String> {
        let state = parse_opt::<crate::runs::workpod::WorkPodState>(self.state.as_deref())?
            .map(|s| s.as_str());
        let kind = match self.kind.as_deref().filter(|s| !s.is_empty()) {
            Some(s) => Some(
                crate::runs::workpod::WorkKind::parse_label(s)
                    .map_err(|e| e.to_string())?
                    .label_value(),
            ),
            None => None,
        };
        // clamp, not min: Postgres rejects a negative LIMIT outright.
        let limit = self
            .limit
            .unwrap_or(TURNS_LIST_DEFAULT)
            .clamp(0, TURNS_LIST_MAX);
        Ok((state, kind, limit))
    }
}

/// `GET /api/turns` — the work-pod dispatch ledger, newest first: every paid agent turn and loop
/// run the controller dispatched, with its terminal state and failure reason. The surface that
/// makes a 50-pod overnight failure storm visible instead of a silent `kubectl get pods` archaeology
/// dig.
#[utoipa::path(
    get,
    path = "/api/turns",
    params(
        ("state" = Option<String>, Query, description = "Filter by lifecycle state (queued|running|succeeded|failed|collected|swept)"),
        ("kind" = Option<String>, Query, description = "Filter by work kind (grounded-rank|scope|run)"),
        ("limit" = Option<i64>, Query, description = "Max rows to return (default 100, max 500)"),
    ),
    responses(
        (status = 200, description = "Work-pod rows, newest first (`result`/`error` are truncated; the full text is on `GET /api/turns/{pod_name}`)", body = Vec<WorkPodDto>),
        (status = 400, description = "Invalid query parameters", body = ErrorBody)
    )
)]
pub(crate) async fn list_turns(
    State(state): State<ApiState>,
    Query(q): Query<TurnsQuery>,
) -> Result<Response, AppError> {
    let (state_filter, kind_filter, limit) = match q.into_filters() {
        Ok(f) => f,
        Err(msg) => {
            return Ok(bad_request(msg));
        }
    };
    let rows =
        crate::runs::work_pods::list_work_pods(state.db.pool(), state_filter, kind_filter, limit)
            .await?;
    let dtos: Vec<WorkPodDto> = rows
        .into_iter()
        .map(|r| WorkPodDto::from(r).truncated_for_list())
        .collect();
    Ok(Json(dtos).into_response())
}

/// `GET /api/turns/{pod_name}` — one ledger row with `result` and `error` intact. The list
/// truncates both, so this is where an expanded turn row goes for the full failure chain.
#[utoipa::path(
    get,
    path = "/api/turns/{pod_name}",
    params(
        ("pod_name" = String, Path, description = "The work pod's k8s object name")
    ),
    responses(
        (status = 200, description = "The work-pod row, with full result/error text", body = WorkPodDto),
        (status = 404, description = "No such work pod", body = ErrorBody)
    )
)]
pub(crate) async fn get_turn(
    State(state): State<ApiState>,
    Path(pod_name): Path<String>,
) -> Result<Response, AppError> {
    match crate::runs::work_pods::get_work_pod(state.db.pool(), &pod_name).await? {
        Some(row) => Ok(Json(WorkPodDto::from(row)).into_response()),
        None => Ok(not_found(format!("turn not found: {pod_name}"))),
    }
}
