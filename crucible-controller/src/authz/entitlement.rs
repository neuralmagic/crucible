//! Lanes a caller is entitled to see and use. A lane is one resource owned by its team, so the
//! policy decides the entitlement like any other read: the team's members hold it, platform
//! administrators hold everything, and a policy set may grant it to anyone else.

use crate::api::state::ApiState;
use crate::authz::Caller;
use crate::authz::action::ResourceType;
use crate::authz::decision::Resource;
use crate::authz::model::{Principal, TeamSlug};

/// A lane the caller may use, as `/api/whoami` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Entitlement {
    Autoresearch,
}

/// The autoresearch lane, owned by the `autoresearch` team.
pub fn autoresearch_lane() -> Resource {
    Resource::new(
        ResourceType::Autoresearch,
        "autoresearch",
        Principal::team(&TeamSlug::autoresearch()),
    )
}

/// Whether the deployment runs the autoresearch lane and `caller` may read it. A controller with
/// its guard off has no one to tell apart, so its loopback caller holds every lane.
#[cfg(feature = "autoresearch")]
pub fn autoresearch(state: &ApiState, caller: &Caller) -> bool {
    state.autoresearch
        && (caller.path == crate::identity::auth::AuthPath::Open
            || crate::authz::owner::may(
                state,
                caller,
                crate::authz::action::Verb::Read,
                &autoresearch_lane(),
            ))
}

/// A build without the autoresearch lane entitles nobody to it.
#[cfg(not(feature = "autoresearch"))]
pub fn autoresearch(_state: &ApiState, _caller: &Caller) -> bool {
    false
}

/// Every lane `caller` is entitled to.
pub fn of(state: &ApiState, caller: &Caller) -> Vec<Entitlement> {
    let mut held = Vec::new();
    if autoresearch(state, caller) {
        held.push(Entitlement::Autoresearch);
    }
    held
}
