//! The teams surface (RFC-0003 C-TEAMS): create, read, rename, delete, manage members, and read
//! the audit trail. Every mutation is decided here against the caller's resolved memberships and
//! audited in the same transaction as its write.

use crate::api::dto::*;
use crate::api::state::*;
use crate::authz::action::{Action, ResourceType, Verb};
use crate::authz::bootstrap::members_json;
use crate::authz::decision::DenialBody;
use crate::authz::model::{
    Member, MemberKind, MemberRef, Membership, Principal, TeamRole, TeamSlug,
};
use crate::authz::policy::{self, Engine, SCHEMA_VERSION};
use crate::authz::store::{self, AuditEvent, MemberRow, OwnedResource, StoreError, TeamRow};
use crate::authz::{Caller, resolve};
use crate::dto::dto;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// How many audit rows one request returns.
const AUDIT_LIMIT: i64 = 200;

dto! {
    /// One member of a team.
    pub struct MemberDto: From<m: MemberRow> {
        pub kind: MemberKind = m.member.kind(),
        /// The login, full group path, nested team slug, or rule the kind names.
        pub member: String = m.member.stored(),
        pub role: TeamRole,
        pub since: String,
        pub added_by: Option<String>,
    }
}

/// A team with its members, the caller's own standing in it, and whether any membership currently
/// reaches a user.
#[derive(Debug, Serialize, ToSchema)]
pub struct TeamDto {
    pub slug: TeamSlug,
    pub display_name: String,
    pub created_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub members: Vec<MemberDto>,
    pub my_role: Option<TeamRole>,
    pub reachable: bool,
}

/// One team the caller is in, as `GET /api/whoami` reports it.
#[derive(Debug, Serialize, ToSchema)]
pub struct TeamMembershipDto {
    pub team: TeamSlug,
    #[serde(flatten)]
    pub membership: Membership,
}

dto! {
    /// One line of the authorization audit trail.
    pub struct AuthzAuditDto: From<a: store::AuditRow> {
        pub id: i64,
        pub at: String,
        pub actor: String,
        pub subject: Option<String>,
        pub auth_path: String,
        pub action: String,
        pub resource_type: String,
        pub resource_id: String,
        pub decision: String,
        pub rule: String,
        pub prior: Option<serde_json::Value>,
        pub result: Option<serde_json::Value>,
    }
}

/// A member as a request spells it.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct MemberBody {
    pub kind: MemberKind,
    pub member: String,
    pub role: TeamRole,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateTeamBody {
    pub slug: TeamSlug,
    pub display_name: String,
    /// Absent: the creator alone, at `owner`.
    pub members: Option<Vec<MemberBody>>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct RenameTeamBody {
    pub display_name: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PutMembersBody {
    pub members: Vec<MemberBody>,
}

/// Why a team could not be deleted: what it still owns.
#[derive(Debug, Serialize, ToSchema)]
pub struct TeamDeleteRefusal {
    pub error: String,
    /// The blocking resources the caller may read.
    pub blocking: Vec<OwnedResource>,
    /// How many more block it that the caller may not read.
    pub others: u64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListTeamsQuery {
    unreachable: Option<bool>,
}

fn internal(e: impl Into<anyhow::Error>) -> Response {
    AppError::from(e.into()).into_response()
}

/// The rule that lets a caller manage a team, or the refusal.
fn may_manage(caller: &Caller, slug: &TeamSlug) -> Result<&'static str, String> {
    if !caller.path.holds_roles() {
        return Err(
            "this session's group refresh was refused; sign in again to manage a team".into(),
        );
    }
    if caller.principals.team_role(slug) == Some(TeamRole::Owner) {
        Ok("team-owner-all")
    } else if caller.principals.is_platform_admin() {
        Ok("platform-admin-all")
    } else {
        Err(format!("managing team {slug} needs its owner role"))
    }
}

fn team_event(
    caller: &Caller,
    verb: Verb,
    slug: &TeamSlug,
    allowed: bool,
    rule: &str,
) -> AuditEvent {
    let action = Action {
        resource: ResourceType::Team,
        verb,
    };
    caller.audit_event(action, slug.to_string(), allowed, rule)
}

/// Record a refusal and answer it. A denied mutation whose audit row cannot be written is a 500,
/// not a 403: the refusal stands either way, but the trail must not silently miss it.
async fn denied(state: &ApiState, event: AuditEvent, msg: String) -> Response {
    let now = crate::clock::now_rfc3339();
    let mut conn = match state.db.pool().acquire().await {
        Ok(conn) => conn,
        Err(e) => return internal(e),
    };
    if let Err(e) = store::audit(&mut conn, &event, &now).await {
        return internal(e);
    }
    forbidden(msg)
}

/// Why a member list was refused.
#[derive(Debug, PartialEq, Eq)]
enum MemberRefusal {
    Unprocessable(String),
    Cycle(String),
}

impl IntoResponse for MemberRefusal {
    fn into_response(self) -> Response {
        match self {
            MemberRefusal::Unprocessable(msg) => unprocessable(msg),
            MemberRefusal::Cycle(msg) => conflict(msg),
        }
    }
}

/// Parse and check a submitted member list against the RFC's invariants: well-formed, no
/// duplicates, at least one user at `owner`, nested teams exist, and no cycle.
fn check_members(
    slug: &TeamSlug,
    submitted: &[MemberBody],
    all: &[MemberRow],
    teams: &[TeamRow],
) -> Result<Vec<Member>, MemberRefusal> {
    let mut members = Vec::with_capacity(submitted.len());
    for body in submitted {
        let member = MemberRef::parse(body.kind, &body.member)
            .map_err(|e| MemberRefusal::Unprocessable(e.to_string()))?;
        if members.iter().any(|m: &Member| m.member == member) {
            return Err(MemberRefusal::Unprocessable(format!(
                "{} {} is listed twice",
                body.kind.as_str(),
                member.stored()
            )));
        }
        if let MemberRef::Team(nested) = &member {
            if !teams.iter().any(|t| &t.slug == nested) {
                return Err(MemberRefusal::Unprocessable(format!("no team {nested}")));
            }
            if resolve::would_cycle(all, slug, nested) {
                return Err(MemberRefusal::Cycle(format!(
                    "listing team {nested} in team {slug} would close a membership cycle"
                )));
            }
        }
        members.push(Member {
            member,
            role: body.role,
        });
    }
    let has_user_owner = members
        .iter()
        .any(|m| m.role == TeamRole::Owner && matches!(m.member, MemberRef::User(_)));
    if !has_user_owner {
        return Err(MemberRefusal::Unprocessable(
            "a team needs at least one user member at the owner role".to_string(),
        ));
    }
    Ok(members)
}

async fn team_dto(state: &ApiState, caller: &Caller, team: TeamRow) -> anyhow::Result<TeamDto> {
    let pool = state.db.pool();
    let all = store::all_members(pool).await?;
    let users = store::known_users(pool).await?;
    Ok(assemble(caller, team, &all, &users))
}

fn assemble(
    caller: &Caller,
    team: TeamRow,
    all: &[MemberRow],
    users: &[store::KnownUser],
) -> TeamDto {
    let reachable = resolve::reachable(all, users, &team.slug);
    let members = all
        .iter()
        .filter(|m| m.team == team.slug)
        .cloned()
        .map(MemberDto::from)
        .collect();
    TeamDto {
        my_role: caller.principals.team_role(&team.slug),
        slug: team.slug,
        display_name: team.display_name,
        created_by: team.created_by,
        created_at: team.created_at,
        updated_at: team.updated_at,
        members,
        reachable,
    }
}

/// `GET /api/teams` — every team. `unreachable=true` narrows to teams none of whose memberships
/// reach a user, which only a platform administrator may ask.
#[utoipa::path(
    get,
    path = "/api/teams",
    params(("unreachable" = Option<bool>, Query, description = "Platform administrators only: teams that reach no user")),
    responses(
        (status = 200, description = "Teams", body = Vec<TeamDto>),
        (status = 403, description = "The unreachable listing needs platform administration", body = ErrorBody)
    )
)]
pub(crate) async fn list_teams(
    State(state): State<ApiState>,
    Query(q): Query<ListTeamsQuery>,
    caller: Caller,
) -> Result<Response, AppError> {
    let only_unreachable = q.unreachable.unwrap_or(false);
    if only_unreachable && !caller.principals.is_platform_admin() {
        return Ok(forbidden(
            "listing unreachable teams needs the platform administrators owner role",
        ));
    }
    let pool = state.db.pool();
    let teams = store::list_teams(pool).await?;
    let all = store::all_members(pool).await?;
    let users = store::known_users(pool).await?;
    let dtos: Vec<TeamDto> = teams
        .into_iter()
        .map(|t| assemble(&caller, t, &all, &users))
        .filter(|t| !only_unreachable || !t.reachable)
        .collect();
    Ok(Json(dtos).into_response())
}

/// `POST /api/teams` — create a team. The creator is listed at `owner` unless the request lists
/// members itself, in which case at least one user must be at `owner`.
#[utoipa::path(
    post,
    path = "/api/teams",
    request_body = CreateTeamBody,
    responses(
        (status = 201, description = "The team", body = TeamDto),
        (status = 403, description = "The caller has no principal that may own a team", body = ErrorBody),
        (status = 409, description = "The slug is taken or a member would close a cycle", body = ErrorBody),
        (status = 422, description = "A member was refused", body = ErrorBody)
    )
)]
pub(crate) async fn create_team(
    State(state): State<ApiState>,
    caller: Caller,
    Json(body): Json<CreateTeamBody>,
) -> Response {
    let Some(creator) = caller.actor().cloned() else {
        return forbidden("an anonymous caller has no principal to create a team with");
    };
    if !caller.path.may_own() || !caller.path.holds_roles() {
        return denied(
            &state,
            team_event(&caller, Verb::Create, &body.slug, false, "no-rule"),
            "this credential names nobody who may own a team".to_string(),
        )
        .await;
    }
    let display_name = body.display_name.trim().to_string();
    if display_name.is_empty() {
        return unprocessable("a team needs a display name");
    }
    let pool = state.db.pool();
    let (all, teams) = match tokio::try_join!(store::all_members(pool), store::list_teams(pool)) {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    let submitted = body.members.unwrap_or_else(|| {
        vec![MemberBody {
            kind: MemberKind::User,
            member: creator.name().to_string(),
            role: TeamRole::Owner,
        }]
    });
    let members = match check_members(&body.slug, &submitted, &all, &teams) {
        Ok(members) => members,
        Err(refusal) => return refusal.into_response(),
    };

    let now = crate::clock::now_rfc3339();
    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(e) => return internal(e),
    };
    let team =
        match store::insert_team(&mut tx, &body.slug, &display_name, Some(&creator), &now).await {
            Ok(team) => team,
            Err(StoreError::Duplicate { what }) => return conflict(what),
            Err(StoreError::Internal(e)) => return internal(e),
        };
    let rows =
        match store::replace_members(&mut tx, &body.slug, &members, Some(&creator), &now).await {
            Ok(rows) => rows,
            Err(e) => return internal(e),
        };
    let mut event = team_event(&caller, Verb::Create, &body.slug, true, "user-create-team");
    event.result = Some(serde_json::json!({
        "display_name": display_name,
        "members": members_json(&rows),
    }));
    if let Err(e) = store::audit(&mut tx, &event, &now).await {
        return internal(e);
    }
    if let Err(e) = tx.commit().await {
        return internal(e);
    }
    let caller = match caller.refreshed(pool, &state.roles).await {
        Ok(caller) => caller,
        Err(e) => return internal(e),
    };
    match team_dto(&state, &caller, team).await {
        Ok(dto) => (StatusCode::CREATED, Json(dto)).into_response(),
        Err(e) => internal(e),
    }
}

/// `GET /api/teams/{slug}` — one team with its members.
#[utoipa::path(
    get,
    path = "/api/teams/{slug}",
    params(("slug" = String, Path, description = "Team slug")),
    responses(
        (status = 200, description = "The team", body = TeamDto),
        (status = 404, description = "No team with that slug", body = ErrorBody)
    )
)]
pub(crate) async fn get_team(
    State(state): State<ApiState>,
    Path(slug): Path<String>,
    caller: Caller,
) -> Response {
    let Ok(slug) = TeamSlug::parse(&slug) else {
        return not_found(format!("no team {slug:?}"));
    };
    let team = match store::get_team(state.db.pool(), &slug).await {
        Ok(Some(team)) => team,
        Ok(None) => return not_found(format!("no team {slug}")),
        Err(e) => return internal(e),
    };
    match team_dto(&state, &caller, team).await {
        Ok(dto) => Json(dto).into_response(),
        Err(e) => internal(e),
    }
}

/// `PUT /api/teams/{slug}` — rename a team.
#[utoipa::path(
    put,
    path = "/api/teams/{slug}",
    params(("slug" = String, Path, description = "Team slug")),
    request_body = RenameTeamBody,
    responses(
        (status = 200, description = "The team", body = TeamDto),
        (status = 403, description = "Not an owner of the team", body = ErrorBody),
        (status = 404, description = "No team with that slug", body = ErrorBody),
        (status = 422, description = "An empty display name", body = ErrorBody)
    )
)]
pub(crate) async fn rename_team(
    State(state): State<ApiState>,
    Path(slug): Path<String>,
    caller: Caller,
    Json(body): Json<RenameTeamBody>,
) -> Response {
    let Ok(slug) = TeamSlug::parse(&slug) else {
        return not_found(format!("no team {slug:?}"));
    };
    let pool = state.db.pool();
    let team = match store::get_team(pool, &slug).await {
        Ok(Some(team)) => team,
        Ok(None) => return not_found(format!("no team {slug}")),
        Err(e) => return internal(e),
    };
    let rule = match may_manage(&caller, &slug) {
        Ok(rule) => rule,
        Err(msg) => {
            return denied(
                &state,
                team_event(&caller, Verb::Update, &slug, false, "no-rule"),
                msg,
            )
            .await;
        }
    };
    let display_name = body.display_name.trim().to_string();
    if display_name.is_empty() {
        return unprocessable("a team needs a display name");
    }
    let now = crate::clock::now_rfc3339();
    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(e) => return internal(e),
    };
    let renamed = match store::rename_team(&mut tx, &slug, &display_name, &now).await {
        Ok(Some(team)) => team,
        Ok(None) => return not_found(format!("no team {slug}")),
        Err(e) => return internal(e),
    };
    let mut event = team_event(&caller, Verb::Update, &slug, true, rule);
    event.prior = Some(serde_json::json!({"display_name": team.display_name}));
    event.result = Some(serde_json::json!({"display_name": display_name}));
    if let Err(e) = store::audit(&mut tx, &event, &now).await {
        return internal(e);
    }
    if let Err(e) = tx.commit().await {
        return internal(e);
    }
    match team_dto(&state, &caller, renamed).await {
        Ok(dto) => Json(dto).into_response(),
        Err(e) => internal(e),
    }
}

/// `DELETE /api/teams/{slug}` — delete a team that owns nothing.
#[utoipa::path(
    delete,
    path = "/api/teams/{slug}",
    params(("slug" = String, Path, description = "Team slug")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 403, description = "Not an owner of the team", body = ErrorBody),
        (status = 404, description = "No team with that slug", body = ErrorBody),
        (status = 409, description = "The team still owns resources", body = TeamDeleteRefusal)
    )
)]
pub(crate) async fn delete_team(
    State(state): State<ApiState>,
    Path(slug): Path<String>,
    caller: Caller,
) -> Response {
    let Ok(slug) = TeamSlug::parse(&slug) else {
        return not_found(format!("no team {slug:?}"));
    };
    let pool = state.db.pool();
    let team = match store::get_team(pool, &slug).await {
        Ok(Some(team)) => team,
        Ok(None) => return not_found(format!("no team {slug}")),
        Err(e) => return internal(e),
    };
    let rule = match may_manage(&caller, &slug) {
        Ok(rule) => rule,
        Err(msg) => {
            return denied(
                &state,
                team_event(&caller, Verb::Delete, &slug, false, "no-rule"),
                msg,
            )
            .await;
        }
    };
    let blocking = match store::owned_by(pool, &Principal::team(&slug)).await {
        Ok(blocking) => blocking,
        Err(e) => return internal(e),
    };
    if !blocking.is_empty() {
        let n = blocking.len();
        return (
            StatusCode::CONFLICT,
            Json(TeamDeleteRefusal {
                error: format!(
                    "team {slug} still owns {n} resource(s); transfer or delete them first"
                ),
                blocking,
                others: 0,
            }),
        )
            .into_response();
    }
    let now = crate::clock::now_rfc3339();
    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(e) => return internal(e),
    };
    let prior = match store::members_of(&mut *tx, &slug).await {
        Ok(rows) => rows,
        Err(e) => return internal(e),
    };
    match store::delete_team(&mut tx, &slug).await {
        Ok(true) => {}
        Ok(false) => return not_found(format!("no team {slug}")),
        Err(e) => return internal(e),
    }
    let mut event = team_event(&caller, Verb::Delete, &slug, true, rule);
    event.prior = Some(serde_json::json!({
        "display_name": team.display_name,
        "members": members_json(&prior),
    }));
    if let Err(e) = store::audit(&mut tx, &event, &now).await {
        return internal(e);
    }
    if let Err(e) = tx.commit().await {
        return internal(e);
    }
    StatusCode::NO_CONTENT.into_response()
}

/// `PUT /api/teams/{slug}/members` — replace the member set.
#[utoipa::path(
    put,
    path = "/api/teams/{slug}/members",
    params(("slug" = String, Path, description = "Team slug")),
    request_body = PutMembersBody,
    responses(
        (status = 200, description = "The team", body = TeamDto),
        (status = 403, description = "Not an owner of the team", body = ErrorBody),
        (status = 404, description = "No team with that slug", body = ErrorBody),
        (status = 409, description = "A nested team would close a cycle", body = ErrorBody),
        (status = 422, description = "A member was refused, or no user is left at owner", body = ErrorBody)
    )
)]
pub(crate) async fn put_members(
    State(state): State<ApiState>,
    Path(slug): Path<String>,
    caller: Caller,
    Json(body): Json<PutMembersBody>,
) -> Response {
    let Ok(slug) = TeamSlug::parse(&slug) else {
        return not_found(format!("no team {slug:?}"));
    };
    let pool = state.db.pool();
    let team = match store::get_team(pool, &slug).await {
        Ok(Some(team)) => team,
        Ok(None) => return not_found(format!("no team {slug}")),
        Err(e) => return internal(e),
    };
    let rule = match may_manage(&caller, &slug) {
        Ok(rule) => rule,
        Err(msg) => {
            return denied(
                &state,
                team_event(&caller, Verb::ManageMembers, &slug, false, "no-rule"),
                msg,
            )
            .await;
        }
    };
    let (all, teams) = match tokio::try_join!(store::all_members(pool), store::list_teams(pool)) {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    let others: Vec<MemberRow> = all.iter().filter(|m| m.team != slug).cloned().collect();
    let members = match check_members(&slug, &body.members, &others, &teams) {
        Ok(members) => members,
        Err(refusal) => return refusal.into_response(),
    };
    let now = crate::clock::now_rfc3339();
    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(e) => return internal(e),
    };
    let prior: Vec<MemberRow> = all.into_iter().filter(|m| m.team == slug).collect();
    let rows = match store::replace_members(&mut tx, &slug, &members, caller.actor(), &now).await {
        Ok(rows) => rows,
        Err(e) => return internal(e),
    };
    let mut event = team_event(&caller, Verb::ManageMembers, &slug, true, rule);
    event.prior = Some(members_json(&prior));
    event.result = Some(members_json(&rows));
    if let Err(e) = store::audit(&mut tx, &event, &now).await {
        return internal(e);
    }
    if let Err(e) = tx.commit().await {
        return internal(e);
    }
    match team_dto(&state, &caller, team).await {
        Ok(dto) => Json(dto).into_response(),
        Err(e) => internal(e),
    }
}

/// `GET /api/teams/{slug}/audit` — the team's authorization trail, newest first.
#[utoipa::path(
    get,
    path = "/api/teams/{slug}/audit",
    params(("slug" = String, Path, description = "Team slug")),
    responses(
        (status = 200, description = "Audit records", body = Vec<AuthzAuditDto>),
        (status = 403, description = "Not an owner of the team", body = ErrorBody),
        (status = 404, description = "No team with that slug", body = ErrorBody)
    )
)]
pub(crate) async fn team_audit(
    State(state): State<ApiState>,
    Path(slug): Path<String>,
    caller: Caller,
) -> Response {
    let Ok(slug) = TeamSlug::parse(&slug) else {
        return not_found(format!("no team {slug:?}"));
    };
    let pool = state.db.pool();
    match store::get_team(pool, &slug).await {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(format!("no team {slug}")),
        Err(e) => return internal(e),
    }
    if let Err(msg) = may_manage(&caller, &slug) {
        return forbidden(msg);
    }
    match store::audit_for(pool, "team", slug.as_str(), AUDIT_LIMIT).await {
        Ok(rows) => Json(
            rows.into_iter()
                .map(AuthzAuditDto::from)
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => internal(e),
    }
}

/// One action of the vocabulary.
#[derive(Debug, Serialize, ToSchema)]
pub struct ActionDto {
    pub action: String,
    pub resource: ResourceType,
    pub verb: Verb,
}

dto! {
    /// One stored policy set. `text` is the Cedar source.
    pub struct PolicySetDto: From<p: store::PolicySetRow> {
        pub digest: String,
        pub text: String,
        pub owner: String,
        pub schema_version: i32,
        pub created_by: Option<String>,
        pub created_at: String,
        pub activated_by: Option<String>,
        pub activated_at: Option<String>,
        pub active: bool,
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PolicySetBody {
    /// The Cedar policy text.
    pub text: String,
}

/// What an activation changed.
#[derive(Debug, Serialize, ToSchema)]
pub struct ActivationDto {
    pub prior: Option<String>,
    pub active: String,
}

/// `GET /api/authz/actions` — the action vocabulary (RFC-0003 C-ACTIONS).
#[utoipa::path(
    get,
    path = "/api/authz/actions",
    responses((status = 200, description = "Every action, in resource then verb order", body = Vec<ActionDto>))
)]
pub(crate) async fn list_actions() -> Json<Vec<ActionDto>> {
    Json(
        Action::all()
            .into_iter()
            .map(|a| ActionDto {
                action: a.to_string(),
                resource: a.resource,
                verb: a.verb,
            })
            .collect(),
    )
}

/// `GET /api/authz/schema` — the Cedar schema a policy set is validated against.
#[utoipa::path(
    get,
    path = "/api/authz/schema",
    responses((status = 200, description = "The Cedar schema", content_type = "text/plain", body = String))
)]
pub(crate) async fn get_schema() -> Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        policy::render_schema(),
    )
        .into_response()
}

/// `GET /api/authz/policy-sets` — every stored set, the active one first.
#[utoipa::path(
    get,
    path = "/api/authz/policy-sets",
    responses((status = 200, description = "Stored policy sets", body = Vec<PolicySetDto>))
)]
pub(crate) async fn list_policy_sets(
    State(state): State<ApiState>,
) -> Result<Json<Vec<PolicySetDto>>, AppError> {
    let rows = store::list_policy_sets(state.db.pool()).await?;
    Ok(Json(rows.into_iter().map(PolicySetDto::from).collect()))
}

/// `POST /api/authz/policy-sets` — validate and store a set without activating it.
#[utoipa::path(
    post,
    path = "/api/authz/policy-sets",
    request_body = PolicySetBody,
    responses(
        (status = 201, description = "The stored set", body = PolicySetDto),
        (status = 403, description = "Not a platform administrator", body = DenialBody),
        (status = 422, description = "The set does not validate; the failing policy is named", body = ErrorBody)
    )
)]
pub(crate) async fn create_policy_set(
    State(state): State<ApiState>,
    caller: Caller,
    Json(body): Json<PolicySetBody>,
) -> Response {
    let engine = match Engine::load(&body.text) {
        Ok(engine) => engine,
        Err(e) => return unprocessable(e.to_string()),
    };
    let now = crate::clock::now_rfc3339();
    let mut conn = match state.db.pool().acquire().await {
        Ok(conn) => conn,
        Err(e) => return internal(e),
    };
    let created_by = caller.actor().map(Principal::to_string);
    match store::insert_policy_set(
        &mut conn,
        engine.digest(),
        &body.text,
        SCHEMA_VERSION,
        created_by.as_deref(),
        &now,
    )
    .await
    {
        Ok(row) => (StatusCode::CREATED, Json(PolicySetDto::from(row))).into_response(),
        Err(e) => internal(e),
    }
}

/// `GET /api/authz/policy-sets/{digest}` — one stored set.
#[utoipa::path(
    get,
    path = "/api/authz/policy-sets/{digest}",
    params(("digest" = String, Path, description = "Content digest")),
    responses(
        (status = 200, description = "The set", body = PolicySetDto),
        (status = 404, description = "No set with that digest", body = ErrorBody)
    )
)]
pub(crate) async fn get_policy_set(
    State(state): State<ApiState>,
    Path(digest): Path<String>,
) -> Response {
    match store::get_policy_set(state.db.pool(), &digest).await {
        Ok(Some(row)) => Json(PolicySetDto::from(row)).into_response(),
        Ok(None) => not_found(format!("no policy set {digest}")),
        Err(e) => internal(e),
    }
}

/// `POST /api/authz/policy-sets/{digest}/activate` — make a stored set the one in force. Decided
/// by the set in force at that moment, audited with both digests, and re-validated on the way in.
#[utoipa::path(
    post,
    path = "/api/authz/policy-sets/{digest}/activate",
    params(("digest" = String, Path, description = "Content digest")),
    responses(
        (status = 200, description = "The prior and new digests", body = ActivationDto),
        (status = 403, description = "Not a platform administrator", body = DenialBody),
        (status = 404, description = "No set with that digest", body = ErrorBody),
        (status = 422, description = "The stored set no longer validates", body = ErrorBody)
    )
)]
pub(crate) async fn activate_policy_set(
    State(state): State<ApiState>,
    Path(digest): Path<String>,
    caller: Caller,
    axum::Extension(decision): axum::Extension<crate::authz::decision::Decision>,
) -> Response {
    let pool = state.db.pool();
    let row = match store::get_policy_set(pool, &digest).await {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(format!("no policy set {digest}")),
        Err(e) => return internal(e),
    };
    let engine = match Engine::load(&row.text) {
        Ok(engine) => engine,
        Err(e) => return unprocessable(e.to_string()),
    };
    let now = crate::clock::now_rfc3339();
    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(e) => return internal(e),
    };
    let by = caller.actor().map(Principal::to_string);
    let prior = match store::activate_policy_set(&mut tx, &digest, by.as_deref(), &now).await {
        Ok(Some(prior)) => prior,
        Ok(None) => return not_found(format!("no policy set {digest}")),
        Err(e) => return internal(e),
    };
    let action = Action {
        resource: ResourceType::PolicySet,
        verb: Verb::Activate,
    };
    let mut event = caller.audit_event(action, digest.clone(), true, decision.reason());
    event.prior = prior.as_ref().map(|d| serde_json::json!({"digest": d}));
    event.result = Some(serde_json::json!({"digest": digest}));
    if let Err(e) = store::audit(&mut tx, &event, &now).await {
        return internal(e);
    }
    if let Err(e) = tx.commit().await {
        return internal(e);
    }
    state.policy.swap(engine);
    Json(ActivationDto {
        prior,
        active: digest,
    })
    .into_response()
}

#[cfg(test)]
mod tests;
