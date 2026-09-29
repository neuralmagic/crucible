//! Lanes a caller is entitled to see and use. Each lane is one platform resource and holding it is
//! one action, `<lane>:access`, so who holds a lane is whatever the active policy set says: the
//! shipped default grants platform administrators and operators, and a rule over `group:`,
//! `user:` or `team:` tags grants anyone else.

use crate::api::state::ApiState;
use crate::authz::Caller;
use crate::authz::action::ResourceType;
use crate::authz::decision::Resource;

/// A lane the caller may use, as `/api/whoami` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Entitlement {
    Autoresearch,
}

/// The autoresearch lane, as the policy decides on it.
pub fn autoresearch_lane() -> Resource {
    Resource::platform(ResourceType::Autoresearch, "autoresearch")
}

/// Whether the deployment runs the autoresearch lane and `caller` may access it. A controller with
/// its guard off has no one to tell apart, so its loopback caller holds every lane unless it acts
/// as a team, which answers for the team.
#[cfg(feature = "autoresearch")]
pub fn autoresearch(state: &ApiState, caller: &Caller) -> bool {
    state.autoresearch
        && ((caller.path == crate::identity::auth::AuthPath::Open && caller.acting_as.is_none())
            || crate::authz::owner::may(
                state,
                caller,
                crate::authz::action::Verb::Access,
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
