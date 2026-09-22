use crate::api::dto::*;
use crate::api::state::*;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};

// --- connected-cluster utilization -------------------------------------------

#[utoipa::path(
    get,
    path = "/api/clusters",
    responses(
        (status = 200, description = "Per-cluster GPU utilization + Kueue pressure for every \
         connected cluster (hub + spokes). Snapshots come from a short TTL cache; a cluster \
         that fails or times out reports reachable=false with its last good numbers and their \
         age, never a request error.", body = Vec<crate::runs::cluster_stats::ClusterSnapshot>)
    )
)]
pub(crate) async fn list_clusters(State(state): State<ApiState>) -> Response {
    let names = state.clusters.names();
    Json(state.cluster_stats.snapshots(&names).await).into_response()
}

// --- eligible dispatch targets ----------------------------------------------

/// The dispatch targets one caller may choose from, for a launch form to render.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct DispatchTargetsDto {
    /// Default first, then the rest in connection order. Empty means this caller can dispatch
    /// nowhere, and the launch endpoints refuse with the reason.
    pub targets: Vec<crate::runs::dispatch_target::EligibleTarget>,
    /// Why the set is empty, when it is; null otherwise.
    pub refusal: Option<String>,
}

/// Assemble the target context for one caller. Reads the caller's own registered kubeconfigs, so
/// the set is never wider than the principals on the request.
pub(crate) async fn target_context<'a>(
    state: &'a ApiState,
    caller: &crate::authz::Caller,
    rules: &'a (dyn Fn(&crate::authz::decision::Resource) -> bool + Sync),
    connected: &'a mut Vec<String>,
    personal: &'a mut Vec<crate::runs::dispatch_target::PersonalTarget>,
) -> Result<crate::runs::dispatch_target::TargetContext<'a>, AppError> {
    *connected = state.clusters.names();
    *personal =
        crate::runs::dispatch_target::personal_targets(state.db.pool(), &caller.principals).await?;
    Ok(crate::runs::dispatch_target::TargetContext {
        connected,
        default: &state.default_cluster,
        policy: &state.cluster_policy,
        rules,
        personal,
        capability: state.dispatch,
    })
}

/// The per-target dispatch decision for `caller`, as [`target_context`] takes it.
pub(crate) fn dispatch_rules<'a>(
    state: &'a ApiState,
    caller: &'a crate::authz::Caller,
) -> impl Fn(&crate::authz::decision::Resource) -> bool + Sync + 'a {
    move |resource| {
        crate::authz::owner::may(
            state,
            caller,
            crate::authz::action::Verb::Dispatch,
            resource,
        )
    }
}

#[utoipa::path(
    get,
    path = "/api/dispatch-targets",
    params(
        ("playbook" = Option<String>, Query, description = "Registry id of the pack the targets \
         are for; its declared agent substrate narrows the set. Omitted = the engine's default \
         backend.")
    ),
    responses(
        (status = 200, description = "Every cluster this caller may dispatch onto, default first. \
         A target the caller holds no principal for is absent, and a personal target belonging to \
         somebody else is absent from every other caller\'s set.", body = DispatchTargetsDto),
        (status = 404, description = "The named playbook is not registered", body = ErrorBody)
    )
)]
pub(crate) async fn list_dispatch_targets(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
    Query(q): Query<DispatchTargetsQuery>,
) -> Result<Response, AppError> {
    // The pack's substrate, when the form names one: a pack the deployment cannot dispatch has no
    // eligible target, and saying so on the form beats refusing at the POST.
    let agent = match q.playbook.as_deref() {
        Some(id) => match crate::playbooks::registry::get(state.db.pool(), id).await? {
            Some(playbook) => playbook.agent.unwrap_or_else(default_agent),
            None => return Ok(not_found(format!("playbook not found: {id}"))),
        },
        None => default_agent(),
    };
    let (mut connected, mut personal) = (Vec::new(), Vec::new());
    let rules = dispatch_rules(&state, &caller);
    let ctx = target_context(&state, &caller, &rules, &mut connected, &mut personal).await?;
    let targets = crate::runs::dispatch_target::eligible(&ctx, &caller.principals, &agent);
    let refusal = targets.is_empty().then(|| {
        match crate::runs::dispatch_target::resolve(&ctx, &caller.principals, &agent, None) {
            Ok(_) => "no dispatch target is available".to_string(),
            Err(e) => e.to_string(),
        }
    });
    Ok(Json(DispatchTargetsDto { targets, refusal }).into_response())
}

#[derive(Debug, serde::Deserialize, utoipa::IntoParams)]
pub(crate) struct DispatchTargetsQuery {
    pub playbook: Option<String>,
}

/// The substrate assumed when no pack is named: the engine\'s own default backend and no image,
/// which is what an autoresearch scenario dispatches with.
fn default_agent() -> crate::playbooks::dispatch::PackAgent {
    crate::playbooks::dispatch::PackAgent::new("local".to_string(), None)
}
