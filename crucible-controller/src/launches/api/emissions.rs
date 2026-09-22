//! `POST /api/emissions/run` — file the experiment review trail for an external run: one the
//! controller did NOT dispatch (a harness-launched loop pod whose PRs came from publish-on-keep).
//! Same engine, same ledger as the completion-edge hook, so replays are idempotent and the
//! tracker creds stay controller-side — the loop reports results, it never holds a token.

use crate::api::dto::{bad_gateway, unavailable, unprocessable};
use crate::api::state::ErrorBody;
use crate::api::{dto::require_non_empty, state::ApiState};
use axum::extract::{Json, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct EmitRunBody {
    /// The key the trail is ledgered under (a stored `issues.key` when one exists, but any stable
    /// experiment key works — an `issues` row is not required).
    issue_key: String,
    /// Human title for the emitted summaries; blank falls back to `issue_key`.
    #[serde(default)]
    title: String,
    /// The frozen pack digest the run executed — the other half of the experiment identity.
    pack_digest: String,
    run_id: String,
    prs: Vec<EmitPr>,
    justification: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct EmitPr {
    url: String,
    repo: String,
    /// Component name for a composite run; blank for single-repo.
    #[serde(default)]
    component: String,
    /// The candidate head branch the PR was opened from.
    #[serde(default)]
    branch: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct EmitAck {
    /// Tracker issues newly filed (the experiment container counts once, on first emission).
    issues_created: usize,
    /// Issues patched in place (a replay updates rather than skips).
    issues_updated: usize,
}

/// Admin-gated: an external emission is a human vouching that a run's PRs deserve a review trail,
/// so it's ledgered with a justification like every other human-authorized write.
#[utoipa::path(
    post,
    path = "/api/emissions/run",
    request_body = EmitRunBody,
    responses(
        (status = 201, description = "Review trail filed (idempotent — replays create nothing new)", body = EmitAck),
        (status = 403, description = "Caller is not in the admin whitelist", body = ErrorBody),
        (status = 422, description = "issue_key/pack_digest/run_id/justification must be non-empty; prs must be non-empty with non-blank url+repo", body = ErrorBody),
        (status = 502, description = "The tracker rejected a create call", body = ErrorBody),
        (status = 503, description = "Emission is not configured on this controller", body = ErrorBody)
    )
)]
pub(crate) async fn emit_run(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
    Json(body): Json<EmitRunBody>,
) -> Response {
    let Some(ctx) = state.emission.clone() else {
        return unavailable(
            "emission is not configured (set JIRA_EMISSION_PROJECT, JIRA_EMISSION_EPIC_TYPE_ID, JIRA_EMISSION_TASK_TYPE_ID + Jira creds)",
        );
    };
    if let Some(msg) = require_non_empty(&[
        ("issue_key", &body.issue_key),
        ("pack_digest", &body.pack_digest),
        ("run_id", &body.run_id),
        ("justification", &body.justification),
    ]) {
        return unprocessable(msg);
    }
    if body.prs.is_empty() {
        return unprocessable("prs must be non-empty");
    }
    for pr in &body.prs {
        if let Some(msg) = require_non_empty(&[("prs[].url", &pr.url), ("prs[].repo", &pr.repo)]) {
            return unprocessable(msg);
        }
    }

    let pr_links: Vec<crate::runs::ingest::PrLink> = body
        .prs
        .iter()
        .map(|pr| crate::runs::ingest::PrLink {
            url: pr.url.clone(),
            repo: pr.repo.clone(),
            name: pr.component.clone(),
            branch: pr.branch.clone(),
        })
        .collect();
    let title = if body.title.trim().is_empty() {
        body.issue_key.clone()
    } else {
        body.title.trim().to_string()
    };
    let exp = crate::launches::emission::ExperimentRef {
        issue_key: body.issue_key.trim(),
        issue_title: &title,
        pack_digest: body.pack_digest.trim(),
    };

    let outcome = match crate::launches::emission::emit_pr_reviews(
        &state.db,
        &ctx,
        &exp,
        body.run_id.trim(),
        &pr_links,
    )
    .await
    {
        Ok(n) => n,
        Err(e) => {
            return bad_gateway(format!("filing the review trail: {e:#}"));
        }
    };

    let actor = identity.as_deref().unwrap_or("unknown");
    state
        .audit(
            crate::event_log::Event::now(
                body.issue_key.trim(),
                "emit",
                "emit",
                Some(&format!(
                    "external run {} emission: {} created, {} patched: {}",
                    body.run_id.trim(),
                    outcome.created,
                    outcome.updated,
                    body.justification.trim()
                )),
                None,
            )
            .by(Some(actor)),
            "emit_run",
        )
        .await;

    (
        StatusCode::CREATED,
        Json(EmitAck {
            issues_created: outcome.created,
            issues_updated: outcome.updated,
        }),
    )
        .into_response()
}
