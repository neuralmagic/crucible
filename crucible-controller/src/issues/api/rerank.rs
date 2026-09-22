use crate::api::dto::*;
use crate::api::state::*;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// --- force re-rank --------------------------------------------------------------

/// The `POST /api/issues/rerank` body, tagged on `scope`: `{"scope":"all"}`,
/// `{"scope":"tier","tier":"T2"}`, or `{"scope":"unranked"}`. The tier spelling is the DB/ranker
/// vocabulary (`T0|T1|T2|T3|N`); anything else is a 400.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum RerankFilter {
    All,
    Tier { tier: String },
    Unranked,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct RerankAck {
    key: String,
    actor: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct BulkRerankAck {
    /// How many issues will rank fresh on the next reconcile sweep.
    affected: u64,
    scope: String,
    actor: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/issues/{key}/rerank",
    params(
        ("key" = String, Path, description = "Issue key (percent-encoded)")
    ),
    responses(
        (status = 200, description = "Rank cache cleared; the ranker re-runs on the next reconcile sweep (not inline in this request). The standing tier keeps gating until the fresh verdict lands.", body = RerankAck),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "Issue not found", body = ErrorBody)
    )
)]
pub(crate) async fn rerank_issue(
    State(state): State<ApiState>,
    Path(key): Path<String>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
) -> Result<Response, AppError> {
    let Some(issue) = crate::issues::store::get_issue(state.db.pool(), &key).await? else {
        return Ok(not_found(format!("issue not found: {key}")));
    };
    crate::issues::store::clear_rank(state.db.pool(), &key).await?;
    let status = issue.status.as_str();
    state
        .db
        .events()
        .append(
            &crate::event_log::Event::now(
                &key,
                status,
                status,
                Some("force re-rank: rank cache cleared, re-ranks next sweep"),
                None,
            )
            .by(identity.as_deref()),
        )
        .await?;
    Ok(Json(RerankAck {
        key,
        actor: identity.0,
    })
    .into_response())
}

#[utoipa::path(
    post,
    path = "/api/issues/rerank",
    request_body = RerankFilter,
    responses(
        (status = 200, description = "Rank caches cleared for every `new` issue in scope; the ranker re-runs per issue on upcoming reconcile sweeps (not inline in this request). Standing tiers keep gating until fresh verdicts land.", body = BulkRerankAck),
        (status = 400, description = "Unknown tier in the filter", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody)
    )
)]
pub(crate) async fn rerank_issues(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
    Json(filter): Json<RerankFilter>,
) -> Result<Response, AppError> {
    let scope = match filter {
        RerankFilter::All => crate::issues::model::RerankScope::All,
        RerankFilter::Unranked => crate::issues::model::RerankScope::Unranked,
        RerankFilter::Tier { tier } => match crucible_contract::Tier::parse(&tier) {
            Ok(t) => crate::issues::model::RerankScope::Tier(t),
            Err(e) => {
                return Ok(bad_request(format!("{e:#}")));
            }
        },
    };
    let affected = crate::issues::store::clear_rank_bulk(state.db.pool(), scope).await?;
    // One summary event on the synthetic `rerank` key (the `autopilot` key's precedent) — a line
    // per issue for a thousand-row sweep would drown the feed.
    state
        .db
        .events()
        .append(
            &crate::event_log::Event::now(
                "rerank",
                "requested",
                "cleared",
                Some(&format!(
                    "bulk force re-rank ({}): {affected} issue(s) re-rank next sweep",
                    scope.describe()
                )),
                None,
            )
            .by(identity.as_deref()),
        )
        .await?;
    Ok(Json(BulkRerankAck {
        affected,
        scope: scope.describe(),
        actor: identity.0,
    })
    .into_response())
}
