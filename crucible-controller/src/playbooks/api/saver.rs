use crate::api::state::*;

/// Who a launch runs as, and where. The same resolution for every launch endpoint: the dispatch
/// target against the caller's reach, the agent pin, and, for a standing surface that fires with
/// no session, the owner principal and group snapshot that save is the only authorization of.
#[derive(Debug)]
pub(crate) struct Saver {
    pub actor: Option<String>,
    /// `user:{login}` when the surface snapshots an owner; `None` for a session launch.
    pub owner_principal: Option<String>,
    /// The caller's groups, as the launch (or the snapshot) carries them.
    pub groups: serde_json::Value,
    pub dispatch_target: String,
    pub provider: Option<String>,
    pub model: Option<String>,
}

/// Who a standing surface owns the row to. `Session` is a launch under the caller's session;
/// `Snapshot` owns a new row to the principal the request names (the caller by default) and
/// snapshots the caller's groups; `Keep` leaves an existing row's owner and snapshot as they are
/// unless the caller is that owner, whose fresh claims replace the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ownership<'a> {
    Session,
    Snapshot {
        asked: Option<&'a str>,
    },
    Keep {
        principal: Option<&'a str>,
        groups: Option<&'a serde_json::Value>,
    },
}

/// What the body asked for.
pub(crate) struct SaverRequest<'a> {
    pub agent: Option<crate::playbooks::dispatch::PackAgent>,
    pub dispatch_target: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub model: Option<&'a str>,
}

fn cannot_own() -> ApiError {
    ApiError::Forbidden(
        "this credential stands for no confirmed sign-in (a downgraded or viewed-as session), \
         so it can own nothing that fires without a session"
            .to_string(),
    )
}

/// Resolve the saver. A snapshot surface refuses a credential that may not own, since the row
/// would own to nobody; every surface resolves the target and the pin against the caller.
pub(crate) async fn resolve_saver(
    state: &ApiState,
    caller: &crate::authz::Caller,
    groups: &crate::identity::auth::Groups,
    ownership: Ownership<'_>,
    req: SaverRequest<'_>,
) -> ApiResult<Saver> {
    let owner_principal = match ownership {
        Ownership::Session => None,
        Ownership::Snapshot { asked } => {
            if !caller.path.may_own() {
                return Err(cannot_own());
            }
            let owner = crate::authz::owner::resolve_owner(caller, asked)?;
            Some(owner.to_string())
        }
        Ownership::Keep { principal, .. } => principal.map(str::to_string),
    };
    let snapshot = match ownership {
        Ownership::Keep {
            principal,
            groups: kept,
        } if principal
            .and_then(|p| crate::authz::model::Principal::parse(p).ok())
            .as_ref()
            != caller.actor() =>
        {
            kept.cloned()
                .unwrap_or_else(|| serde_json::Value::Array(Vec::new()))
        }
        _ => serde_json::Value::from(groups.0.clone()),
    };
    let dispatch_target =
        resolve_dispatch_target(state, caller, req.agent, req.dispatch_target).await?;
    let pin = match crate::playbooks::api::providers::require_agent_pin(
        state,
        caller,
        req.provider,
        req.model,
    )
    .await
    {
        Ok(Ok(pin)) => pin,
        Ok(Err(refusal)) => return Err(ApiError::invalid_fields(vec![refusal.field_error()])),
        Err(e) => return Err(e.into()),
    };
    let actor = caller.actor_principals().login().map(str::to_string);
    Ok(Saver {
        owner_principal,
        actor,
        groups: snapshot,
        dispatch_target,
        provider: pin.provider,
        model: pin.model,
    })
}

impl Saver {
    /// The dispatch pin a launch this saver mints is written with.
    pub(crate) fn pin(&self) -> crate::launches::model::DispatchPin<'_> {
        crate::launches::model::DispatchPin {
            target: Some(&self.dispatch_target),
            provider: self.provider.as_deref(),
            model: self.model.as_deref(),
        }
    }
}

/// Resolve what a launch asked to dispatch onto against what its caller may reach. A refusal is a
/// 403 naming the eligible set, not a silent fall back to the default: dispatching somebody's run
/// onto a cluster they did not choose is worse than making them choose again.
pub(crate) async fn resolve_dispatch_target(
    state: &ApiState,
    caller: &crate::authz::Caller,
    agent: Option<crate::playbooks::dispatch::PackAgent>,
    requested: Option<&str>,
) -> ApiResult<String> {
    // A pack with no recorded substrate reads as the engine default; the startup backfill fills in
    // what it can, and refusing here for want of a stamp would be a lie about the pack.
    let agent = agent.unwrap_or(crate::playbooks::dispatch::PackAgent::new(
        "local".to_string(),
        None,
    ));
    let (mut connected, mut personal) = (Vec::new(), Vec::new());
    let rules = crate::runs::api::cluster_view::dispatch_rules(state, caller);
    let ctx = crate::runs::api::cluster_view::target_context(
        state,
        caller,
        &rules,
        &mut connected,
        &mut personal,
    )
    .await?;
    let refusal =
        match crate::runs::dispatch_target::resolve(&ctx, &caller.principals, &agent, requested) {
            Ok(target) => return Ok(target),
            Err(refusal) => refusal,
        };
    if let crate::runs::dispatch_target::TargetRefusal::NotAuthorized { name, .. } = &refusal {
        let owner = personal
            .iter()
            .find(|t| &t.name == name)
            .map(|t| t.owner.clone());
        let resource = crate::runs::dispatch_target::target_resource(name, owner.as_ref());
        let action = crate::authz::action::Action {
            resource: crate::authz::action::ResourceType::DispatchTarget,
            verb: crate::authz::action::Verb::Dispatch,
        };
        crate::authz::owner::decide(state, caller, action, &resource).await?;
    }
    Err(refusal.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Db;
    use crate::identity::auth::{AuthPath, Groups};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use sqlx::PgPool;

    fn request<'a>() -> SaverRequest<'a> {
        SaverRequest {
            agent: None,
            dispatch_target: None,
            provider: None,
            model: None,
        }
    }

    async fn resolve(
        pool: PgPool,
        path: AuthPath,
        ownership: Ownership<'_>,
    ) -> Result<Saver, StatusCode> {
        let state = ApiState::test(Db::new(pool));
        let groups = Groups(vec!["/groups/team-x".to_string()]);
        let caller = crate::authz::resolve_caller(
            state.db.pool(),
            &state.roles,
            Some("alice"),
            &groups.0,
            path,
        )
        .await
        .expect("resolves");
        resolve_saver(&state, &caller, &groups, ownership, request())
            .await
            .map_err(|e| e.into_response().status())
    }

    /// A surface that fires later with no session owns to the saver; a credential that may not own
    /// cannot own it, and the refusal is the same on every such surface.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_snapshot_surface_refuses_a_credential_that_may_not_own(pool: PgPool) {
        for path in [AuthPath::DowngradedSession, AuthPath::Impersonated] {
            let refused = resolve(pool.clone(), path, Ownership::Snapshot { asked: None })
                .await
                .expect_err("cannot own");
            assert_eq!(refused, StatusCode::FORBIDDEN, "{path:?}");
        }
    }

    /// The same credential launches fine under its session: an immediate launch owns nothing.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_session_surface_takes_any_credential_and_snapshots_no_owner(pool: PgPool) {
        let saver = resolve(pool, AuthPath::DowngradedSession, Ownership::Session)
            .await
            .expect("resolved");
        assert_eq!(saver.actor.as_deref(), Some("alice"));
        assert_eq!(saver.owner_principal, None);
        assert_eq!(saver.groups, serde_json::json!(["/groups/team-x"]));
    }

    /// A snapshot surface records the principal spelling the standing rows store.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_snapshot_surface_owns_to_the_saver(pool: PgPool) {
        let saver = resolve(pool, AuthPath::Session, Ownership::Snapshot { asked: None })
            .await
            .expect("resolved");
        assert_eq!(saver.owner_principal.as_deref(), Some("user:alice"));
        assert_eq!(saver.groups, serde_json::json!(["/groups/team-x"]));
    }

    /// An edit by someone other than the owner leaves the row owned and snapshotted as it was; an
    /// edit by the owner refreshes the snapshot from their claims.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn keeping_an_owner_preserves_the_snapshot_unless_the_owner_edits(pool: PgPool) {
        let kept = serde_json::json!(["/groups/other"]);
        let saver = resolve(
            pool.clone(),
            AuthPath::Session,
            Ownership::Keep {
                principal: Some("user:bob"),
                groups: Some(&kept),
            },
        )
        .await
        .expect("resolved");
        assert_eq!(saver.owner_principal.as_deref(), Some("user:bob"));
        assert_eq!(saver.groups, kept);
        let saver = resolve(
            pool,
            AuthPath::Session,
            Ownership::Keep {
                principal: Some("user:alice"),
                groups: Some(&kept),
            },
        )
        .await
        .expect("resolved");
        assert_eq!(saver.owner_principal.as_deref(), Some("user:alice"));
        assert_eq!(saver.groups, serde_json::json!(["/groups/team-x"]));
    }
}
