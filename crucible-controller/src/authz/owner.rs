//! Owner selection on a creation surface (RFC-0003 C-OWNERSHIP) and the decision a handler takes
//! on a resource it resolved.

use crate::api::state::{ApiState, ErrorBody, Json};
use crate::authz::Caller;
use crate::authz::action::{Action, ResourceType, Verb};
use crate::authz::decision::{Decision, DenialBody, Resource, Subject};
use crate::authz::model::Principal;
use crate::authz::store;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// Why a requested owner was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum OwnerRefusal {
    Anonymous,
    CannotOwn,
    Malformed(String),
    NotActedAs { asked: String, held: Vec<String> },
}

impl IntoResponse for OwnerRefusal {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            OwnerRefusal::Anonymous => (
                StatusCode::FORBIDDEN,
                "an anonymous caller has no principal to own a resource".to_string(),
            ),
            OwnerRefusal::CannotOwn => (
                StatusCode::FORBIDDEN,
                "this credential names nobody who may own a resource".to_string(),
            ),
            OwnerRefusal::Malformed(e) => (StatusCode::UNPROCESSABLE_ENTITY, e),
            OwnerRefusal::NotActedAs { asked, held } => (
                StatusCode::FORBIDDEN,
                format!(
                    "{asked} is not a principal you act as; you act as {}",
                    held.join(", ")
                ),
            ),
        };
        (status, Json(ErrorBody::new(msg))).into_response()
    }
}

/// The owner a creation asks for: the caller's user principal unless the request names another
/// principal the caller acts as. `self` and `user:self` mean the caller.
pub fn resolve_owner(caller: &Caller, asked: Option<&str>) -> Result<Principal, OwnerRefusal> {
    if !caller.path.may_own() {
        return Err(OwnerRefusal::CannotOwn);
    }
    let Some(user) = caller.principals.user() else {
        return Err(OwnerRefusal::Anonymous);
    };
    let asked = asked.map(str::trim).filter(|a| !a.is_empty());
    let owner = match asked {
        None | Some("self") | Some("user:self") => user.clone(),
        Some(raw) => Principal::parse(raw).map_err(|e| OwnerRefusal::Malformed(e.to_string()))?,
    };
    if !owner.may_own() {
        return Err(OwnerRefusal::Malformed(format!(
            "{owner} cannot own a resource"
        )));
    }
    if !caller.principals.covers(&owner) {
        return Err(OwnerRefusal::NotActedAs {
            asked: owner.to_string(),
            held: caller
                .principals
                .all()
                .into_iter()
                .map(|p| p.to_string())
                .collect(),
        });
    }
    Ok(owner)
}

/// A refused decision, answered as the structured denial, or a refusal whose audit row could not
/// be written, answered as the failure it is.
#[derive(Debug)]
pub enum Denied {
    Refused {
        who: String,
        action: Action,
        decision: Decision,
    },
    Unrecorded(anyhow::Error),
}

impl IntoResponse for Denied {
    fn into_response(self) -> Response {
        match self {
            Denied::Refused {
                who,
                action,
                decision,
            } => denial(&who, action, &decision),
            Denied::Unrecorded(e) => crate::api::state::AppError::from(e).into_response(),
        }
    }
}

/// The decision on a resource the handler resolved, under the policy set in force. A denied
/// mutation is written to the audit trail before it is answered, and a trail that cannot be
/// written fails the request (RFC-0003 C-AUDIT).
pub async fn decide(
    state: &ApiState,
    caller: &Caller,
    action: Action,
    resource: &Resource,
) -> Result<Decision, Denied> {
    let now = jiff::Timestamp::now().as_second();
    let decision = match Subject::of(&caller.principals, caller.proves_groups()) {
        Some(subject) => state
            .policy
            .current()
            .authorize(&subject, action, resource, now),
        None => Decision {
            allowed: false,
            rules: Vec::new(),
        },
    };
    if decision.allowed {
        return Ok(decision);
    }
    let who = caller.actor_label();
    if action.verb != Verb::Read {
        let event = caller.audit_event(action, resource.id.clone(), false, decision.reason());
        let now = crate::clock::now_rfc3339();
        let written = async {
            let mut conn = state.db.pool().acquire().await?;
            store::audit(&mut conn, &event, &now).await
        }
        .await;
        if let Err(e) = written {
            return Err(Denied::Unrecorded(e));
        }
    }
    Err(Denied::Refused {
        who,
        action,
        decision,
    })
}

/// `create` on a root resource, decided against the intended owner.
pub async fn decide_create(
    state: &ApiState,
    caller: &Caller,
    rtype: ResourceType,
    owner: &Principal,
) -> Result<Decision, Denied> {
    let action = Action {
        resource: rtype,
        verb: Verb::Create,
    };
    decide(
        state,
        caller,
        action,
        &Resource::new(rtype, "new", owner.clone()),
    )
    .await
}

/// The owner a creation asks for, resolved against the principals the caller acts as and decided
/// as `create` on `rtype` against that owner.
#[allow(clippy::result_large_err)]
pub async fn owner_for_create(
    state: &ApiState,
    caller: &Caller,
    asked: Option<&str>,
    rtype: ResourceType,
) -> Result<Principal, Response> {
    let owner = resolve_owner(caller, asked).map_err(IntoResponse::into_response)?;
    decide_create(state, caller, rtype, &owner)
        .await
        .map_err(IntoResponse::into_response)?;
    Ok(owner)
}

/// Why a request on a resolved resource was not served: the caller may not read it, so it does
/// not exist for them (RFC-0003 C-READ-SCOPING), or they may read it and the action was denied.
#[derive(Debug)]
pub enum Refused {
    NotFound(String),
    Denied(Denied),
}

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        match self {
            Refused::NotFound(msg) => crate::api::dto::not_found(msg),
            Refused::Denied(denied) => denied.into_response(),
        }
    }
}

/// Decide `verb` on a resource the handler looked up, with the share the caller holds on it. A
/// caller without `read` gets the not-found answer whatever the verb; one with `read` gets the
/// structured denial when `verb` is refused.
pub async fn decide_on(
    state: &ApiState,
    caller: &Caller,
    resource: &Resource,
    verb: Verb,
) -> Result<Decision, Refused> {
    let shared =
        crate::authz::granted::attach(state.db.pool(), &caller.principals, resource.clone())
            .await
            .map_err(|e| Refused::Denied(Denied::Unrecorded(e)))?;
    let resource = &shared;
    let read = Action {
        resource: resource.rtype,
        verb: Verb::Read,
    };
    let readable = decide(state, caller, read, resource).await;
    if readable.is_err() {
        if verb != Verb::Read {
            let action = Action {
                resource: resource.rtype,
                verb,
            };
            if let Err(Denied::Unrecorded(e)) = decide(state, caller, action, resource).await {
                return Err(Refused::Denied(Denied::Unrecorded(e)));
            }
        }
        return Err(Refused::NotFound(format!(
            "no {} {:?}",
            resource.rtype.as_str(),
            resource.id
        )));
    }
    if verb == Verb::Read {
        return readable.map_err(Refused::Denied);
    }
    let action = Action {
        resource: resource.rtype,
        verb,
    };
    decide(state, caller, action, resource)
        .await
        .map_err(Refused::Denied)
}

/// The decision on `verb` without its audit row: what a list or an eligible-set derivation asks
/// per item. A refusal that answers a request goes through [`decide`] instead.
pub fn may(state: &ApiState, caller: &Caller, verb: Verb, resource: &Resource) -> bool {
    let Some(subject) = Subject::of(&caller.principals, caller.proves_groups()) else {
        return false;
    };
    let action = Action {
        resource: resource.rtype,
        verb,
    };
    let now = jiff::Timestamp::now().as_second();
    state
        .policy
        .current()
        .authorize(&subject, action, resource, now)
        .allowed
}

/// The verbs `caller` may perform on `resource`, in vocabulary order, without audit rows: what a
/// page enables its controls from. `resource` carries the share the caller holds on it.
pub fn actions(state: &ApiState, caller: &Caller, resource: &Resource) -> Vec<Verb> {
    resource
        .rtype
        .verbs()
        .into_iter()
        .filter(|verb| may(state, caller, *verb, resource))
        .collect()
}

/// [`actions`] on one resource, with the caller's share on it looked up first.
pub async fn actions_on(
    state: &ApiState,
    caller: &Caller,
    resource: Resource,
) -> anyhow::Result<Vec<Verb>> {
    let resource =
        crate::authz::granted::attach(state.db.pool(), &caller.principals, resource).await?;
    Ok(actions(state, caller, &resource))
}

/// The rows `caller` may read, each with the verbs they may perform on it.
pub async fn readable_with_actions<T>(
    state: &ApiState,
    caller: &Caller,
    rtype: ResourceType,
    rows: Vec<T>,
    resource: impl Fn(&T) -> Resource,
) -> anyhow::Result<Vec<(T, Vec<Verb>)>> {
    let mut held = crate::authz::granted::held(state.db.pool(), &caller.principals, rtype).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let mut resource = resource(&row);
            resource.share = held.remove(&resource.id);
            let verbs = actions(state, caller, &resource);
            verbs.contains(&Verb::Read).then_some((row, verbs))
        })
        .collect())
}

/// `rows` narrowed to the ones the caller may read, in their order (RFC-0003 C-READ-SCOPING),
/// shares included. A withheld row is not audited and the withheld count is not reported anywhere.
pub async fn readable<T>(
    state: &ApiState,
    caller: &Caller,
    rtype: ResourceType,
    rows: Vec<T>,
    resource: impl Fn(&T) -> Resource,
) -> anyhow::Result<Vec<T>> {
    let rows = readable_with_actions(state, caller, rtype, rows, resource).await?;
    Ok(rows.into_iter().map(|(row, _)| row).collect())
}

/// The row a handler looked up by `id`, decided for `verb` against its owner. A missing row and a
/// row the caller may not read are the same not-found answer.
pub async fn decide_row<T>(
    state: &ApiState,
    caller: &Caller,
    rtype: ResourceType,
    id: &str,
    verb: Verb,
    row: Option<T>,
    owner: impl Fn(&T) -> Principal,
) -> Result<T, Refused> {
    let Some(row) = row else {
        return Err(Refused::NotFound(format!("no {} {id:?}", rtype.as_str())));
    };
    let resource = Resource::new(rtype, id, owner(&row));
    decide_on(state, caller, &resource, verb).await?;
    Ok(row)
}

pub(crate) fn denial(who: &str, action: Action, decision: &Decision) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(DenialBody {
            error: format!("{who} may not {action} ({})", decision.reason()),
            rule: decision.reason(),
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::model::{Membership, Principals, TeamRole, TeamSlug, Via};
    use crate::identity::auth::AuthPath;
    use std::collections::{BTreeMap, BTreeSet};

    fn caller(login: Option<&str>, groups: &[&str], teams: &[&str], path: AuthPath) -> Caller {
        let groups: Vec<String> = groups.iter().map(|g| g.to_string()).collect();
        let teams = teams
            .iter()
            .map(|t| {
                (
                    TeamSlug::parse(t).expect("slug"),
                    Membership {
                        role: TeamRole::Member,
                        via: BTreeSet::from([Via::Direct {
                            role: TeamRole::Member,
                        }]),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        Caller {
            principals: Principals::new(login, &groups).with_teams(teams),
            path,
        }
    }

    #[test]
    fn the_owner_defaults_to_self_and_must_be_acted_as() {
        let alice = caller(Some("alice"), &["/groups/x"], &["llm-d"], AuthPath::Session);
        assert_eq!(
            resolve_owner(&alice, None).expect("self"),
            Principal::User("alice".into())
        );
        assert_eq!(
            resolve_owner(&alice, Some("user:self")).expect("self"),
            Principal::User("alice".into())
        );
        assert_eq!(
            resolve_owner(&alice, Some("team:llm-d")).expect("team"),
            Principal::parse("team:llm-d").expect("parses")
        );
        assert_eq!(
            resolve_owner(&alice, Some("group:/groups/x")).expect("group"),
            Principal::parse("group:/groups/x").expect("parses")
        );
        assert!(matches!(
            resolve_owner(&alice, Some("team:other")),
            Err(OwnerRefusal::NotActedAs { .. })
        ));
        assert!(matches!(
            resolve_owner(&alice, Some("user:bob")),
            Err(OwnerRefusal::NotActedAs { .. })
        ));
        assert!(matches!(
            resolve_owner(&alice, Some("nope")),
            Err(OwnerRefusal::Malformed(_))
        ));
        assert!(matches!(
            resolve_owner(&alice, Some("run:r1")),
            Err(OwnerRefusal::Malformed(_))
        ));
    }

    #[test]
    fn a_credential_that_may_not_own_and_an_anonymous_caller_are_refused() {
        let anon = caller(None, &[], &[], AuthPath::Session);
        assert_eq!(resolve_owner(&anon, None), Err(OwnerRefusal::Anonymous));
        let unknown = caller(Some("x"), &[], &[], AuthPath::UnknownClusterToken);
        assert_eq!(resolve_owner(&unknown, None), Err(OwnerRefusal::CannotOwn));
    }
}
