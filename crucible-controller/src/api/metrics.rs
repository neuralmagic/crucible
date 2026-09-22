//! The unauthenticated `/metrics` route, served outside the bearer guard.

/// The `/metrics` handler: render the registry (refreshing DB-mirrored gauges first) as the
/// Prometheus text format. Served unauthenticated so the shared kube-prometheus-stack can scrape it
/// — [`crate::serve`] merges this router in *after* the bearer-guard layer, exempting it (the same
/// way a health endpoint is). A `Db` built without metrics (never in production) answers 503.
async fn metrics_handler(
    axum::extract::State(state): axum::extract::State<crate::api::state::ApiState>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(m) = state.db.metrics() else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "metrics not initialized\n",
        )
            .into_response();
    };
    let ceiling = state.caps.as_ref().map(|c| c.daily_cost_ceiling);
    match gauge_state(&state.db, ceiling)
        .await
        .and_then(|gauges| m.gather(gauges))
    {
        Ok(body) => (
            [(
                axum::http::header::CONTENT_TYPE,
                crate::metrics::TEXT_CONTENT_TYPE,
            )],
            body,
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("gathering metrics failed: {e:#}\n"),
        )
            .into_response(),
    }
}

/// The DB-mirrored gauges as of now: today's spend against `ceiling`, work pods by kind and state,
/// pending approvals, and builds by backend and state.
pub(crate) async fn gauge_state(
    db: &crate::client::Db,
    ceiling: Option<f64>,
) -> anyhow::Result<crate::metrics::GaugeState> {
    let today = crate::clock::today_utc();
    let daily_spend_usd = crate::ledger::ledger_day_total(db.pool(), &today).await?;
    let rows = crate::runs::work_pods::work_pods_in_states(
        db.pool(),
        &[
            crate::runs::workpod::WorkPodState::Queued,
            crate::runs::workpod::WorkPodState::Running,
            crate::runs::workpod::WorkPodState::Succeeded,
            crate::runs::workpod::WorkPodState::Failed,
            crate::runs::workpod::WorkPodState::Collected,
            crate::runs::workpod::WorkPodState::Swept,
        ],
    )
    .await?;
    let mut workpods: std::collections::BTreeMap<(String, &'static str), i64> =
        std::collections::BTreeMap::new();
    for row in rows {
        *workpods.entry((row.kind, row.state.as_str())).or_insert(0) += 1;
    }
    let approvals_pending = i64::try_from(
        crate::issues::store::awaiting_approval_scopes(db.pool())
            .await?
            .len(),
    )
    .unwrap_or(i64::MAX);
    let build_rows = crate::builds::store::builds_in_states(
        db.pool(),
        &[
            crate::builds::model::BuildState::Pending,
            crate::builds::model::BuildState::Dispatched,
            crate::builds::model::BuildState::Succeeded,
            crate::builds::model::BuildState::Failed,
            crate::builds::model::BuildState::TimedOut,
        ],
    )
    .await?;
    let mut builds: std::collections::BTreeMap<(&'static str, &'static str), i64> =
        std::collections::BTreeMap::new();
    for row in build_rows {
        *builds
            .entry((row.backend.as_str(), row.state.as_str()))
            .or_insert(0) += 1;
    }
    Ok(crate::metrics::GaugeState {
        daily_spend_usd,
        daily_ceiling_usd: ceiling,
        workpods: workpods.into_iter().map(|((k, s), n)| (k, s, n)).collect(),
        approvals_pending,
        builds: builds.into_iter().map(|((b, s), n)| (b, s, n)).collect(),
    })
}

/// The unauthenticated `/metrics` router, merged onto the served app outside the bearer guard.
pub(crate) fn router(state: crate::api::state::ApiState) -> axum::Router {
    axum::Router::new()
        .route("/metrics", axum::routing::get(metrics_handler))
        .with_state(state)
}
