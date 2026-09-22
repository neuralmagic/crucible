use crate::api::dto::*;
use crate::api::state::*;
use axum::extract::State;
use serde::Serialize;
use utoipa::ToSchema;

// --- GET handlers ------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct Health {
    status: &'static str,
}

#[utoipa::path(
    get,
    path = "/healthz",
    responses(
        (status = 200, description = "Service is healthy", body = Health)
    )
)]
pub(crate) async fn healthz() -> Json<Health> {
    Json(Health { status: "ok" })
}

/// Which build is answering.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct Version {
    /// The commit this binary was built from, twelve hex characters, or `unknown` for a build with
    /// no git context to read.
    git_sha: &'static str,
    /// The controller crate's version.
    version: &'static str,
}

/// `GET /api/version` — the commit this controller is running.
///
/// A merged change and a running one are different facts, and until this existed the difference
/// was only visible by reading the Deployment's image digest from inside the cluster. Anything
/// that can reach the API can now tell them apart.
#[utoipa::path(
    get,
    path = "/api/version",
    responses((status = 200, description = "The running build", body = Version))
)]
pub(crate) async fn version() -> Json<Version> {
    Json(Version {
        git_sha: env!("CRUCIBLE_GIT_SHA"),
        version: env!("CARGO_PKG_VERSION"),
    })
}

/// The caller's identity as the guard proved it; null when unauthenticated (local dev, loopback).
/// `role` is the two-tier role (`admin` implies operator). `admin` mirrors `role == "admin"` and is
/// kept for one release as a back-compat alias — the SPA reads `role` now, but external callers may
/// still read the old bool.
///
/// `groups` is what the issuer (or, in proxy mode, the SSO edge) asserted for this caller,
/// normalized. It is empty on the machine paths, and empty on a human path when the IdP emits no
/// `groups` claim — which is the difference between `auth.operatorGroups` granting write access and
/// granting nothing, so it is reported rather than left to be inferred from a role.
///
/// `mode` is which identity model this deployment runs, so the SPA knows whether its sign-in and
/// sign-out URLs are the sidecar's or the controller's. `downgraded` says a live session's group
/// refresh was refused, so the SPA can tell a viewer who lost their role from one who never had it.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct Whoami {
    user: Option<String>,
    admin: bool,
    role: crate::identity::auth::Role,
    groups: Vec<String>,
    mode: crate::identity::auth::AuthMode,
    downgraded: bool,
    /// Whether the presenting credential proves groups (RFC-0003 C-CREDENTIAL-PARITY). False on
    /// a static token, a cluster token, or a downgraded session, whatever `groups` holds.
    proves_groups: bool,
    /// The teams the caller reaches, with the role held and how.
    teams: Vec<crate::authz::api::TeamMembershipDto>,
}

#[utoipa::path(
    get,
    path = "/api/whoami",
    responses(
        (status = 200, description = "The authenticated user, role, and back-compat admin flag, or null outside the proxy", body = Whoami)
    )
)]
pub(crate) async fn whoami(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    groups: crate::identity::auth::Groups,
    auth_path: crate::identity::auth::AuthPath,
    caller: crate::authz::Caller,
) -> Json<Whoami> {
    let role = if auth_path.holds_roles() {
        state.roles.role(&identity, &groups)
    } else {
        crate::identity::auth::Role::Viewer
    };
    let teams = caller
        .principals
        .teams()
        .iter()
        .map(|(team, membership)| crate::authz::api::TeamMembershipDto {
            team: team.clone(),
            membership: membership.clone(),
        })
        .collect();
    Json(Whoami {
        user: identity.0,
        admin: role == crate::identity::auth::Role::Admin,
        role,
        groups: groups.0,
        mode: state.auth_mode,
        downgraded: !auth_path.holds_roles(),
        proves_groups: auth_path.carries_groups(),
        teams,
    })
}

/// The access boundary the `/admin` page renders: the admin/operator login whitelists and the
/// allowed-org list `POST /api/repos` enforces. A read-only projection of the deploy-pinned
/// [`crate::identity::auth::Roles`] + [`RepoWhitelist`] config — never runtime-mutable, so there's no PUT
/// twin (unlike the Lane O2 config knobs). Unguarded like the other `/api/*` reads.
#[derive(Debug, Serialize, ToSchema)]
pub struct AccessDto {
    /// GitHub logins in the admin whitelist (`CONTROLLER_ADMINS`); empty means locked-closed.
    pub admins: Vec<String>,
    /// GitHub logins in the operator whitelist (`CONTROLLER_OPERATORS`).
    pub operators: Vec<String>,
    /// Orgs `POST /api/repos` accepts (`CONTROLLER_ALLOWED_ORGS`); empty locks new-repo adds closed.
    pub allowed_orgs: Vec<String>,
}

#[utoipa::path(
    get,
    path = "/api/access",
    responses(
        (status = 200, description = "The admin/operator login whitelists and the repo-add allowed-org list", body = AccessDto)
    )
)]
pub(crate) async fn get_access(State(state): State<ApiState>) -> Json<AccessDto> {
    Json(AccessDto {
        admins: state.roles.admins().to_vec(),
        operators: state.roles.operators().to_vec(),
        allowed_orgs: state.repo_whitelist.allowed_orgs().to_vec(),
    })
}

#[utoipa::path(
    get,
    path = "/api/overview",
    responses(
        (status = 200, description = "Dashboard overview with status/tier counts and caps", body = Overview)
    )
)]
pub(crate) async fn overview(State(state): State<ApiState>) -> Result<Json<Overview>, AppError> {
    let dto = overview_dto(&state.db, state.caps.as_ref()).await?;
    Ok(Json(dto))
}

#[utoipa::path(
    get,
    path = "/api/ledger/summary",
    responses(
        (status = 200, description = "Daily cost summary for the last 30 days", body = LedgerSummaryDto)
    )
)]
pub(crate) async fn ledger_summary(
    State(state): State<ApiState>,
) -> Result<Json<LedgerSummaryDto>, AppError> {
    Ok(Json(ledger_summary_dto(&state.db).await?))
}

/// Today's ledgered spend broken down by cost tag, against the daily ceiling — the dashboard's
/// answer to "what is the money going to?" now that grounded turns cost real dollars per turn.
#[derive(Debug, Serialize, ToSchema)]
pub struct LedgerByTagDto {
    /// The UTC day the breakdown covers (`YYYY-MM-DD`, always today).
    pub day: String,
    /// The daemon's daily cost ceiling, or null when the API runs without caps.
    pub ceiling: Option<f64>,
    pub tags: Vec<LedgerTagDto>,
}

/// One cost tag's summed spend for the day (biggest spender first).
#[derive(Debug, Serialize, ToSchema)]
pub struct LedgerTagDto {
    /// The ledger `kind` the cost booked under (rank-grounded | scope | run | …).
    pub tag: String,
    pub total_usd: f64,
}

#[utoipa::path(
    get,
    path = "/api/ledger/by-tag",
    responses(
        (status = 200, description = "Today's cost grouped by ledger tag, with the daily ceiling", body = LedgerByTagDto)
    )
)]
pub(crate) async fn ledger_by_tag(
    State(state): State<ApiState>,
) -> Result<Json<LedgerByTagDto>, AppError> {
    let day = crate::clock::today_utc();
    let tags = crate::ledger::ledger_day_by_tag(state.db.pool(), &day).await?;
    Ok(Json(LedgerByTagDto {
        day,
        ceiling: state.caps.as_ref().map(|c| c.daily_cost_ceiling),
        tags: tags
            .into_iter()
            .map(|t| LedgerTagDto {
                tag: t.tag,
                total_usd: t.total_usd,
            })
            .collect(),
    }))
}
