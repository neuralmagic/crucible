//! Launch admission: the one place a launch request is checked against the pack it targets, for
//! every endpoint that launches or stores a launch.

use crate::api::dto::*;
use crate::api::state::*;
use crate::playbooks::registry::{FieldError, PlaybookRow};
use axum::response::{IntoResponse, Response};

/// The newest save of a draft, compiled: what a draft launch or a draft-head schedule targets.
pub(crate) struct DraftHead {
    pub draft: crate::playbooks::drafts::DraftRow,
    pub version: i64,
    pub schema: serde_json::Value,
    pub schema_digest: String,
    pub agent: Option<crate::playbooks::dispatch::PackAgent>,
}

impl DraftHead {
    /// `latest` as a launch target. `None` when that save never compiled, so it has no form.
    pub(crate) fn compiled(
        draft: crate::playbooks::drafts::DraftRow,
        latest: crate::playbooks::drafts::LatestVersion,
    ) -> Option<Self> {
        let (Some(schema), Some(schema_digest)) = (latest.params_schema, latest.schema_digest)
        else {
            return None;
        };
        Some(DraftHead {
            draft,
            version: latest.version,
            schema,
            schema_digest,
            agent: latest.agent,
        })
    }
}

/// What a launch runs: a registered pack at its revision, or a draft at a saved version.
#[derive(Clone, Copy)]
pub(crate) enum LaunchTarget<'a> {
    Registered(&'a PlaybookRow),
    Draft(&'a DraftHead),
}

impl<'a> LaunchTarget<'a> {
    pub(crate) fn id(self) -> &'a str {
        match self {
            Self::Registered(pack) => &pack.id,
            Self::Draft(head) => &head.draft.id,
        }
    }

    pub(crate) fn owner(self) -> &'a crate::authz::model::Principal {
        match self {
            Self::Registered(pack) => &pack.owner,
            Self::Draft(head) => &head.draft.owner,
        }
    }

    pub(crate) fn schema_digest(self) -> &'a str {
        match self {
            Self::Registered(pack) => &pack.schema_digest,
            Self::Draft(head) => &head.schema_digest,
        }
    }

    pub(crate) fn agent(self) -> Option<&'a crate::playbooks::dispatch::PackAgent> {
        match self {
            Self::Registered(pack) => pack.agent.as_ref(),
            Self::Draft(head) => head.agent.as_ref(),
        }
    }

    /// The git repo the pack comes from or graduates to, `None` when it has neither.
    pub(crate) fn repo(self) -> Option<&'a str> {
        match self {
            Self::Registered(pack) => pack.source.repo(),
            Self::Draft(head) => head.draft.graduation_repo.as_deref(),
        }
    }

    /// What a launch row records as its repo.
    pub(crate) fn launch_repo(self) -> &'a str {
        self.repo()
            .unwrap_or(crate::playbooks::registry::DRAFT_LAUNCH_REPO)
    }

    /// What a standing launch of this target fires.
    pub(crate) fn standing(self) -> crate::launches::standing::StandingTarget<'a> {
        match self {
            Self::Registered(pack) => {
                crate::launches::standing::StandingTarget::Adopted(pack.revision())
            }
            Self::Draft(_) => crate::launches::standing::StandingTarget::DraftHead,
        }
    }

    /// The draft version a launch of this target is authorized against.
    pub(crate) fn draft_version(self) -> Option<i64> {
        match self {
            Self::Registered(_) => None,
            Self::Draft(head) => Some(head.version),
        }
    }

    /// 409 when the form was rendered against a schema this target no longer serves.
    #[allow(clippy::result_large_err)]
    fn check_schema_digest(self, asked: Option<&str>) -> Result<(), Response> {
        let current = self.schema_digest();
        match asked {
            Some(asked) if asked != current => {
                let kind = match self {
                    Self::Registered(_) => "playbook",
                    Self::Draft(_) => "draft",
                };
                Err(conflict(format!(
                    "{kind} {} now serves schema {current}, not {asked}; reload the form",
                    self.id()
                )))
            }
            _ => Ok(()),
        }
    }

    #[allow(clippy::result_large_err)]
    async fn schema(self, pool: &sqlx::PgPool) -> Result<serde_json::Value, Response> {
        match self {
            Self::Registered(pack) => {
                match crate::playbooks::registry::schema(pool, &pack.id).await {
                    Ok(Some(schema)) => Ok(schema),
                    Ok(None) => Err(not_found(format!("no playbook {:?}", pack.id))),
                    Err(e) => Err(AppError::from(e).into_response()),
                }
            }
            Self::Draft(head) => Ok(head.schema.clone()),
        }
    }

    #[allow(clippy::result_large_err)]
    async fn tree(
        self,
        pool: &sqlx::PgPool,
    ) -> Result<crucible_contract::pack_tree::PackTree, Response> {
        let (tree, missing) = match self {
            Self::Registered(pack) => (
                crate::playbooks::registry::pack(pool, &pack.id).await,
                format!("no playbook {:?}", pack.id),
            ),
            Self::Draft(head) => (
                crate::playbooks::drafts::version_tree(pool, &head.draft.id, Some(head.version))
                    .await
                    .map(|found| found.map(|(_, tree)| tree)),
                format!("no draft {:?}", head.draft.id),
            ),
        };
        match tree {
            Ok(Some(tree)) => Ok(tree),
            Ok(None) => Err(not_found(missing)),
            Err(e) => Err(unreadable_tree(e)),
        }
    }
}

/// Something a launch can target.
pub(crate) trait Admissible {
    fn target(&self) -> LaunchTarget<'_>;
}

impl Admissible for PlaybookRow {
    fn target(&self) -> LaunchTarget<'_> {
        LaunchTarget::Registered(self)
    }
}

impl Admissible for DraftHead {
    fn target(&self) -> LaunchTarget<'_> {
        LaunchTarget::Draft(self)
    }
}

/// An owned [`LaunchTarget`], for a surface that accepts either.
#[allow(clippy::large_enum_variant)]
pub(crate) enum LaunchPack {
    Registered(PlaybookRow),
    Draft(DraftHead),
}

impl Admissible for LaunchPack {
    fn target(&self) -> LaunchTarget<'_> {
        match self {
            Self::Registered(pack) => LaunchTarget::Registered(pack),
            Self::Draft(head) => LaunchTarget::Draft(head),
        }
    }
}

/// What a launch request asks for.
#[derive(Clone, Copy)]
pub(crate) struct LaunchRequest<'a> {
    pub params: &'a std::collections::BTreeMap<String, String>,
    pub max_cost: f64,
    pub max_time: &'a str,
    /// The schema digest the form was rendered against.
    pub schema_digest: Option<&'a str>,
}

/// What a launch is authorized to run: the pack it targets, the values the pack's schema
/// accepted, and the ceilings the admin caps allowed.
pub(crate) struct AuthorizedLaunch<P> {
    pub pack: P,
    /// The validated `{name: value}` object as stored.
    pub params: serde_json::Value,
    pub max_cost: f64,
    pub max_time: crate::model::MaxTime,
}

/// What a pack whose manifest never parsed is refused with. A launch would fail at the engine's
/// first read of it, so it is refused here instead.
pub(crate) const UNREADABLE_MANIFEST: &str =
    "the pack's crucible.toml did not parse, so what substrate its agent needs is unknown";

/// A launch this deployment has no substrate for. Refused here, at launch, rather than discovered
/// at the bottom of a failed reconcile: the pack is fine, this deployment is what cannot run it.
pub(crate) fn undispatchable(refusal: String) -> Response {
    refused(
        "this deployment cannot dispatch the pack's agent backend",
        vec![crate::playbooks::registry::FieldError {
            field: String::new(),
            message: refusal,
        }],
    )
}

/// Admit `request` against `pack`: the schema it was rendered against, whether this deployment
/// can dispatch the pack's agent, the ceilings, the values, and the graph those values compile.
/// `Err` is the response to answer with.
#[allow(clippy::result_large_err)]
pub(crate) async fn admit<P: Admissible>(
    state: &ApiState,
    pack: P,
    request: LaunchRequest<'_>,
) -> Result<AuthorizedLaunch<P>, Response> {
    let target = pack.target();
    let (schema, max_time) = admit_pack(state, target, request).await?;
    let params = crate::playbooks::registry::validate_params(&schema, request.params)
        .map_err(invalid_fields)?;
    check_plan(target.tree(state.db.pool()).await?, &params, &max_time).await?;
    Ok(AuthorizedLaunch {
        pack,
        params,
        max_cost: request.max_cost,
        max_time,
    })
}

/// [`admit`] up to the values, for a standing launch whose values are only complete at each
/// firing: the schema to validate the fixed values against, and the bounded `max_time`.
#[allow(clippy::result_large_err)]
pub(crate) async fn admit_pack(
    state: &ApiState,
    target: LaunchTarget<'_>,
    request: LaunchRequest<'_>,
) -> Result<(serde_json::Value, crate::model::MaxTime), Response> {
    target.check_schema_digest(request.schema_digest)?;
    if let Some(refusal) = match target.agent() {
        Some(agent) => state.dispatch.refusal(agent),
        None => Some(UNREADABLE_MANIFEST.to_string()),
    } {
        return Err(undispatchable(refusal));
    }
    let max_time = check_ceilings(&state.playbook_caps, request.max_cost, request.max_time)
        .map_err(|field| invalid_fields(vec![field]))?;
    let schema = target.schema(state.db.pool()).await?;
    Ok((schema, max_time))
}

/// Check the launcher's ceilings against the admin caps. Ceilings are the launcher's — the pack
/// declares none — so this is the only bound on them.
pub(crate) fn check_ceilings(
    caps: &crate::config::PlaybookCaps,
    max_cost: f64,
    max_time: &str,
) -> Result<crate::model::MaxTime, FieldError> {
    let refuse = |field: &str, message: String| FieldError {
        field: field.to_string(),
        message,
    };
    if !max_cost.is_finite() || max_cost <= 0.0 {
        return Err(refuse(
            "max_cost",
            "max_cost must be a positive number of USD".to_string(),
        ));
    }
    if max_cost > caps.max_cost {
        return Err(refuse(
            "max_cost",
            format!(
                "max_cost {max_cost} is above this controller's cap of {}",
                caps.max_cost
            ),
        ));
    }
    let max_time = crate::model::MaxTime::parse(max_time).map_err(|m| refuse("max_time", m))?;
    if max_time.secs() > caps.max_time.secs() {
        return Err(refuse(
            "max_time",
            format!(
                "max_time {max_time} is above this controller's cap of {}",
                caps.max_time
            ),
        ));
    }
    Ok(max_time)
}

/// Refuse a launch of `tree` the engine would refuse at startup: compiled with the launch's
/// validated values, its graph does not compile or declares a task timeout past `max_time`.
#[allow(clippy::result_large_err)]
async fn check_plan(
    tree: crucible_contract::pack_tree::PackTree,
    params: &serde_json::Value,
    max_time: &crate::model::MaxTime,
) -> Result<(), Response> {
    let params = crate::launches::store::stored_params(params);
    match crate::playbooks::registry::check_tree_runnable(tree, params, max_time.clone()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(field)) => Err(invalid_fields(vec![field])),
        Err(e) => Err(AppError::from(e).into_response()),
    }
}

/// The response for a pack tree a launch could not read: a 422 naming why when the stored bytes
/// cannot be a tree, a 500 otherwise.
fn unreadable_tree(e: anyhow::Error) -> Response {
    if e.downcast_ref::<crate::playbooks::pack_trees::Unconvertible>()
        .is_some()
    {
        return invalid_fields(vec![FieldError {
            field: String::new(),
            message: format!("{e:#}"),
        }]);
    }
    AppError::from(e).into_response()
}

/// Decide `launch` on a pack against its owner, for every surface that fires it: the form POST,
/// one-shots, watches and schedules.
#[allow(clippy::result_large_err)]
pub(crate) async fn decide_launch(
    state: &ApiState,
    caller: &crate::authz::Caller,
    target: LaunchTarget<'_>,
) -> Result<(), Response> {
    crate::authz::owner::decide_on(
        state,
        caller,
        &crate::authz::decision::Resource::new(
            crate::authz::action::ResourceType::Playbook,
            target.id(),
            target.owner().clone(),
        ),
        crate::authz::action::Verb::Launch,
    )
    .await
    .map(|_| ())
    .map_err(IntoResponse::into_response)
}
