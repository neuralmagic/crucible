//! Route bindings (RFC-0003 C-ENFORCEMENT): every served route names one action and where its
//! resource is resolved, and a request to a route with no binding is denied for every caller.
//!
//! `Resolver::Platform` routes act on the deployment itself; the middleware decides them here,
//! beside the guard the handler still holds, so a request passes only when both allow.
//! `Resolver::Route` routes act on a resource the handler looks up (an owner, a run tree); the
//! handler decides once ownership lands on that type, and until then keeps its guard.

use crate::api::state::{ApiState, ErrorBody, Json};
use crate::authz::Caller;
use crate::authz::action::{Action, ResourceType, Verb};
use crate::authz::decision::Resource;
use axum::extract::{FromRequestParts, MatchedPath, Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Where a route's resource comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolver {
    /// The deployment, owned by the platform administrators team; decided by the middleware.
    Platform,
    /// A resource the handler resolves and decides on.
    Route,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub method: &'static str,
    pub path: &'static str,
    pub action: Action,
    pub resolver: Resolver,
}

const fn bind(
    method: &'static str,
    path: &'static str,
    resource: ResourceType,
    verb: Verb,
    resolver: Resolver,
) -> Binding {
    Binding {
        method,
        path,
        action: Action { resource, verb },
        resolver,
    }
}

use Resolver::{Platform, Route};
use ResourceType as R;
use Verb as V;

/// The binding table, one entry per served method and path.
pub static BINDINGS: &[Binding] = &[
    bind("GET", "/healthz", R::Platform, V::Read, Route),
    bind("GET", "/api/openapi.json", R::Platform, V::Read, Route),
    bind("GET", "/api/version", R::Platform, V::Read, Route),
    bind("GET", "/api/whoami", R::Platform, V::Read, Route),
    bind("GET", "/api/access", R::Platform, V::Read, Route),
    bind("GET", "/api/overview", R::Platform, V::Read, Route),
    bind("GET", "/api/funnel", R::Platform, V::Read, Route),
    bind("GET", "/api/ledger/summary", R::Platform, V::Read, Route),
    bind("GET", "/api/ledger/by-tag", R::Platform, V::Read, Route),
    bind("GET", "/api/events", R::Platform, V::Read, Route),
    bind("GET", "/api/events/stream", R::Platform, V::Read, Route),
    bind("GET", "/api/config", R::Platform, V::Read, Route),
    bind(
        "GET",
        "/api/config/broker-contracts",
        R::Platform,
        V::Read,
        Route,
    ),
    bind(
        "GET",
        "/api/config/playbook-caps",
        R::Platform,
        V::Read,
        Route,
    ),
    bind("GET", "/api/config/providers", R::Platform, V::Read, Route),
    bind(
        "PUT",
        "/api/config/overrides",
        R::Platform,
        V::Update,
        Platform,
    ),
    bind(
        "PUT",
        "/api/config/dispatch-defaults",
        R::Platform,
        V::Update,
        Platform,
    ),
    bind(
        "DELETE",
        "/api/config/dispatch-defaults",
        R::Platform,
        V::Update,
        Platform,
    ),
    bind("POST", "/api/reconcile", R::Platform, V::Update, Platform),
    bind(
        "POST",
        "/api/emissions/run",
        R::Platform,
        V::Update,
        Platform,
    ),
    bind("GET", "/api/images", R::Platform, V::Read, Route),
    bind("POST", "/api/images/rank", R::Platform, V::Read, Route),
    bind(
        "POST",
        "/api/images/refresh",
        R::Platform,
        V::Update,
        Platform,
    ),
    bind("GET", "/api/clusters", R::Platform, V::Read, Route),
    bind(
        "GET",
        "/api/dispatch-targets",
        R::DispatchTarget,
        V::Read,
        Route,
    ),
    bind("GET", "/api/export/runs.parquet", R::Run, V::Read, Route),
    bind(
        "GET",
        "/api/export/iterations.parquet",
        R::Run,
        V::Read,
        Route,
    ),
    bind("GET", "/api/authz/actions", R::Platform, V::Read, Route),
    bind("GET", "/api/authz/schema", R::Platform, V::Read, Route),
    bind(
        "GET",
        "/api/authz/policy-sets",
        R::PolicySet,
        V::Read,
        Route,
    ),
    bind(
        "POST",
        "/api/authz/policy-sets",
        R::PolicySet,
        V::Create,
        Platform,
    ),
    bind(
        "GET",
        "/api/authz/policy-sets/{digest}",
        R::PolicySet,
        V::Read,
        Route,
    ),
    bind(
        "POST",
        "/api/authz/policy-sets/{digest}/activate",
        R::PolicySet,
        V::Activate,
        Platform,
    ),
    bind(
        "POST",
        "/api/issues/{key}/park",
        R::Issue,
        V::Update,
        Platform,
    ),
    bind(
        "POST",
        "/api/issues/{key}/unpark",
        R::Issue,
        V::Update,
        Platform,
    ),
    bind("GET", "/api/approvals", R::Issue, V::Read, Route),
    bind("GET", "/api/runs", R::Run, V::Read, Route),
    bind("GET", "/api/runs/{run_id}", R::Run, V::Read, Route),
    bind(
        "GET",
        "/api/runs/{run_id}/iterations",
        R::Run,
        V::Read,
        Route,
    ),
    bind("GET", "/api/runs/{run_id}/graph", R::Run, V::Read, Route),
    bind(
        "GET",
        "/api/runs/{run_id}/tasks/{task}/evidence",
        R::Run,
        V::Read,
        Route,
    ),
    bind("GET", "/api/runs/{run_id}/log", R::Run, V::Read, Route),
    bind("GET", "/api/runs/{run_id}/files", R::Run, V::Read, Route),
    bind(
        "GET",
        "/api/runs/{run_id}/files/{*key}",
        R::Run,
        V::Read,
        Route,
    ),
    bind(
        "GET",
        "/api/runs/{run_id}/artifacts",
        R::Run,
        V::Read,
        Route,
    ),
    bind(
        "GET",
        "/api/runs/{run_id}/artifacts/{*path}",
        R::Run,
        V::Read,
        Route,
    ),
    bind(
        "GET",
        "/api/runs/{run_id}/flow-enriched",
        R::Run,
        V::Read,
        Route,
    ),
    bind("GET", "/api/runs/{run_id}/live", R::Run, V::Read, Route),
    bind(
        "PUT",
        "/api/runs/{run_id}/session",
        R::Run,
        V::Update,
        Route,
    ),
    bind("GET", "/api/playbooks", R::Playbook, V::Read, Route),
    bind("POST", "/api/playbooks", R::Playbook, V::Create, Route),
    bind("GET", "/api/playbooks/{id}", R::Playbook, V::Read, Route),
    bind(
        "GET",
        "/api/playbooks/{id}/schema",
        R::Playbook,
        V::Read,
        Route,
    ),
    bind(
        "POST",
        "/api/playbooks/{id}/launch",
        R::Playbook,
        V::Launch,
        Route,
    ),
    bind(
        "PUT",
        "/api/playbooks/{id}/owner",
        R::Playbook,
        V::Transfer,
        Route,
    ),
    bind(
        "GET",
        "/api/playbooks/{id}/shares",
        R::Playbook,
        V::Read,
        Route,
    ),
    bind(
        "PUT",
        "/api/playbooks/{id}/shares/{grantee}",
        R::Playbook,
        V::Share,
        Route,
    ),
    bind(
        "DELETE",
        "/api/playbooks/{id}/shares/{grantee}",
        R::Playbook,
        V::Share,
        Route,
    ),
    bind(
        "GET",
        "/api/playbook-drafts/{id}/shares",
        R::PlaybookDraft,
        V::Read,
        Route,
    ),
    bind(
        "PUT",
        "/api/playbook-drafts/{id}/shares/{grantee}",
        R::PlaybookDraft,
        V::Share,
        Route,
    ),
    bind(
        "DELETE",
        "/api/playbook-drafts/{id}/shares/{grantee}",
        R::PlaybookDraft,
        V::Share,
        Route,
    ),
    bind(
        "GET",
        "/api/playbooks/imports/{id}/shares",
        R::PackImport,
        V::Read,
        Route,
    ),
    bind(
        "PUT",
        "/api/playbooks/imports/{id}/shares/{grantee}",
        R::PackImport,
        V::Share,
        Route,
    ),
    bind(
        "DELETE",
        "/api/playbooks/imports/{id}/shares/{grantee}",
        R::PackImport,
        V::Share,
        Route,
    ),
    bind(
        "GET",
        "/api/providers/{id}/shares",
        R::ModelProvider,
        V::Read,
        Route,
    ),
    bind(
        "PUT",
        "/api/providers/{id}/shares/{grantee}",
        R::ModelProvider,
        V::Share,
        Route,
    ),
    bind(
        "DELETE",
        "/api/providers/{id}/shares/{grantee}",
        R::ModelProvider,
        V::Share,
        Route,
    ),
    bind(
        "GET",
        "/api/playbooks/drafts/co-draft",
        R::PlaybookDraft,
        V::Read,
        Route,
    ),
    bind(
        "GET",
        "/api/playbooks/drafts/skill",
        R::PlaybookDraft,
        V::Read,
        Route,
    ),
    bind(
        "GET",
        "/api/playbook-drafts",
        R::PlaybookDraft,
        V::Read,
        Route,
    ),
    bind(
        "POST",
        "/api/playbook-drafts",
        R::PlaybookDraft,
        V::Create,
        Route,
    ),
    bind(
        "POST",
        "/api/playbook-drafts/from-git",
        R::PlaybookDraft,
        V::Create,
        Route,
    ),
    bind(
        "GET",
        "/api/playbook-drafts/{id}",
        R::PlaybookDraft,
        V::Read,
        Route,
    ),
    bind(
        "DELETE",
        "/api/playbook-drafts/{id}",
        R::PlaybookDraft,
        V::Delete,
        Route,
    ),
    bind(
        "PUT",
        "/api/playbook-drafts/{id}/owner",
        R::PlaybookDraft,
        V::Transfer,
        Route,
    ),
    bind(
        "GET",
        "/api/playbook-drafts/{id}/files",
        R::PlaybookDraft,
        V::Read,
        Route,
    ),
    bind(
        "GET",
        "/api/playbook-drafts/{id}/origin/files",
        R::PlaybookDraft,
        V::Read,
        Route,
    ),
    bind(
        "GET",
        "/api/playbook-drafts/{id}/preview",
        R::PlaybookDraft,
        V::Read,
        Route,
    ),
    bind(
        "GET",
        "/api/playbook-drafts/{id}/tarball",
        R::PlaybookDraft,
        V::Read,
        Route,
    ),
    bind(
        "POST",
        "/api/playbook-drafts/{id}/versions",
        R::PlaybookDraft,
        V::Update,
        Route,
    ),
    bind(
        "POST",
        "/api/playbook-drafts/{id}/graduate",
        R::PlaybookDraft,
        V::Approve,
        Route,
    ),
    bind(
        "POST",
        "/api/playbook-drafts/{id}/publish",
        R::PlaybookDraft,
        V::Publish,
        Route,
    ),
    bind(
        "POST",
        "/api/playbook-drafts/{id}/launch",
        R::PlaybookDraft,
        V::Launch,
        Route,
    ),
    bind(
        "POST",
        "/api/playbooks/import/candidates",
        R::PackImport,
        V::Create,
        Route,
    ),
    bind(
        "POST",
        "/api/playbooks/imports",
        R::PackImport,
        V::Create,
        Route,
    ),
    bind(
        "GET",
        "/api/playbooks/imports/{id}",
        R::PackImport,
        V::Read,
        Route,
    ),
    bind(
        "POST",
        "/api/playbooks/imports/{id}/compile",
        R::PackImport,
        V::Update,
        Route,
    ),
    bind(
        "POST",
        "/api/playbooks/imports/{id}/discard",
        R::PackImport,
        V::Update,
        Route,
    ),
    bind(
        "POST",
        "/api/playbooks/imports/{id}/draft",
        R::PackImport,
        V::Update,
        Route,
    ),
    bind(
        "POST",
        "/api/playbooks/imports/{id}/register",
        R::PackImport,
        V::Approve,
        Route,
    ),
    bind(
        "PUT",
        "/api/playbooks/imports/{id}/owner",
        R::PackImport,
        V::Transfer,
        Route,
    ),
    bind("GET", "/api/playbook-runs", R::Run, V::Read, Route),
    bind("GET", "/api/playbook-runs/{key}", R::Run, V::Read, Route),
    bind("GET", "/api/one-shots", R::OneShot, V::Read, Route),
    bind("POST", "/api/one-shots", R::OneShot, V::Create, Route),
    bind(
        "DELETE",
        "/api/one-shots/{id}",
        R::OneShot,
        V::Delete,
        Route,
    ),
    bind("GET", "/api/schedules", R::StandingLaunch, V::Read, Route),
    bind(
        "POST",
        "/api/schedules",
        R::StandingLaunch,
        V::Create,
        Route,
    ),
    bind(
        "POST",
        "/api/schedules/preview",
        R::StandingLaunch,
        V::Read,
        Route,
    ),
    bind(
        "GET",
        "/api/schedules/{id}",
        R::StandingLaunch,
        V::Read,
        Route,
    ),
    bind(
        "PUT",
        "/api/schedules/{id}",
        R::StandingLaunch,
        V::Update,
        Route,
    ),
    bind(
        "DELETE",
        "/api/schedules/{id}",
        R::StandingLaunch,
        V::Delete,
        Route,
    ),
    bind("GET", "/api/watches", R::StandingLaunch, V::Read, Route),
    bind("POST", "/api/watches", R::StandingLaunch, V::Create, Route),
    bind(
        "POST",
        "/api/watches/preview",
        R::StandingLaunch,
        V::Read,
        Route,
    ),
    bind(
        "GET",
        "/api/watches/trackers",
        R::StandingLaunch,
        V::Read,
        Route,
    ),
    bind(
        "GET",
        "/api/watches/{id}",
        R::StandingLaunch,
        V::Read,
        Route,
    ),
    bind(
        "PUT",
        "/api/watches/{id}",
        R::StandingLaunch,
        V::Update,
        Route,
    ),
    bind(
        "DELETE",
        "/api/watches/{id}",
        R::StandingLaunch,
        V::Delete,
        Route,
    ),
    bind(
        "POST",
        "/api/watches/{id}/enabled",
        R::StandingLaunch,
        V::Update,
        Route,
    ),
    bind(
        "GET",
        "/api/watches/{id}/hits",
        R::StandingLaunch,
        V::Read,
        Route,
    ),
    bind(
        "DELETE",
        "/api/watches/{id}/hits/{item}",
        R::StandingLaunch,
        V::Update,
        Route,
    ),
    bind("GET", "/api/providers", R::ModelProvider, V::Read, Route),
    bind("POST", "/api/providers", R::ModelProvider, V::Create, Route),
    bind(
        "PUT",
        "/api/providers/{id}",
        R::ModelProvider,
        V::Update,
        Route,
    ),
    bind(
        "DELETE",
        "/api/providers/{id}",
        R::ModelProvider,
        V::Delete,
        Route,
    ),
    bind(
        "PUT",
        "/api/providers/{id}/owner",
        R::ModelProvider,
        V::Transfer,
        Route,
    ),
    bind("GET", "/api/secrets", R::Secret, V::Read, Route),
    bind("POST", "/api/secrets", R::Secret, V::Create, Route),
    bind("GET", "/api/secrets/{id}", R::Secret, V::Read, Route),
    bind("DELETE", "/api/secrets/{id}", R::Secret, V::Delete, Route),
    bind(
        "POST",
        "/api/secrets/{id}/rotate",
        R::Secret,
        V::Rotate,
        Route,
    ),
    bind(
        "PUT",
        "/api/secrets/{id}/owner",
        R::Secret,
        V::Transfer,
        Route,
    ),
    bind(
        "POST",
        "/api/secrets/{id}/bindings",
        R::Secret,
        V::Bind,
        Route,
    ),
    bind(
        "DELETE",
        "/api/secrets/{id}/bindings/{binding_id}",
        R::Secret,
        V::Bind,
        Route,
    ),
    bind("GET", "/api/secrets/{id}/audit", R::Secret, V::Read, Route),
    bind("GET", "/api/keys", R::ApiKey, V::Read, Route),
    bind("POST", "/api/keys", R::ApiKey, V::Create, Route),
    bind("DELETE", "/api/keys/{id}", R::ApiKey, V::Delete, Route),
    bind("GET", "/api/credentials/me", R::ApiKey, V::Read, Route),
    bind("DELETE", "/api/credentials/me", R::ApiKey, V::Delete, Route),
    bind("GET", "/api/prefs", R::UserPrefs, V::Read, Route),
    bind("PUT", "/api/prefs", R::UserPrefs, V::Update, Route),
    bind("GET", "/api/prefs/editor", R::UserPrefs, V::Read, Route),
    bind("PUT", "/api/prefs/editor", R::UserPrefs, V::Update, Route),
    bind("GET", "/api/prefs/pickers", R::UserPrefs, V::Read, Route),
    bind("PUT", "/api/prefs/pickers", R::UserPrefs, V::Update, Route),
    bind("GET", "/api/teams", R::Team, V::Read, Route),
    bind("POST", "/api/teams", R::Team, V::Create, Route),
    bind("GET", "/api/teams/{slug}", R::Team, V::Read, Route),
    bind("PUT", "/api/teams/{slug}", R::Team, V::Update, Route),
    bind("DELETE", "/api/teams/{slug}", R::Team, V::Delete, Route),
    bind(
        "PUT",
        "/api/teams/{slug}/members",
        R::Team,
        V::ManageMembers,
        Route,
    ),
    bind("GET", "/api/teams/{slug}/audit", R::Team, V::Read, Route),
];

/// The autoresearch lane's bindings, served only when that lane is built.
#[cfg(feature = "autoresearch")]
static AUTORESEARCH_BINDINGS: &[Binding] = &[
    bind("GET", "/api/autopilot", R::Platform, V::Read, Route),
    bind("POST", "/api/autopilot", R::Platform, V::Update, Platform),
    bind("GET", "/api/issues", R::Issue, V::Read, Route),
    bind("GET", "/api/issues/facets", R::Issue, V::Read, Route),
    bind("POST", "/api/issues/rerank", R::Issue, V::Launch, Platform),
    bind("GET", "/api/issues/{key}", R::Issue, V::Read, Route),
    bind("GET", "/api/issues/{key}/journey", R::Issue, V::Read, Route),
    bind("GET", "/api/issues/{key}/builds", R::Build, V::Read, Route),
    bind(
        "GET",
        "/api/issues/{key}/scope-report",
        R::Issue,
        V::Read,
        Route,
    ),
    bind(
        "GET",
        "/api/issues/{key}/scope-transcript",
        R::Issue,
        V::Read,
        Route,
    ),
    bind(
        "POST",
        "/api/issues/{key}/bump",
        R::Issue,
        V::Update,
        Platform,
    ),
    bind(
        "POST",
        "/api/issues/{key}/redispatch",
        R::Issue,
        V::Launch,
        Platform,
    ),
    bind(
        "POST",
        "/api/issues/{key}/rerank",
        R::Issue,
        V::Launch,
        Platform,
    ),
    bind(
        "POST",
        "/api/issues/{key}/scope",
        R::Issue,
        V::Launch,
        Platform,
    ),
    bind(
        "GET",
        "/api/approvals/{scope_id}/evidence",
        R::Issue,
        V::Read,
        Route,
    ),
    bind("POST", "/api/jira", R::Issue, V::Create, Platform),
    bind("POST", "/api/scenarios", R::Issue, V::Create, Platform),
    bind(
        "POST",
        "/api/scenarios/{key}/approve",
        R::Issue,
        V::Approve,
        Platform,
    ),
    bind("GET", "/api/turns", R::Issue, V::Read, Route),
    bind("GET", "/api/turns/{pod_name}", R::Issue, V::Read, Route),
    bind(
        "GET",
        "/api/turns/{pod_name}/live",
        R::Issue,
        V::Read,
        Route,
    ),
    bind("GET", "/api/repos", R::Repo, V::Read, Route),
    bind("POST", "/api/repos", R::Repo, V::Create, Platform),
    bind("DELETE", "/api/repos/{repo}", R::Repo, V::Delete, Platform),
    bind(
        "POST",
        "/api/repos/{repo}/pause",
        R::Repo,
        V::Update,
        Platform,
    ),
    bind(
        "POST",
        "/api/repos/{repo}/resume",
        R::Repo,
        V::Update,
        Platform,
    ),
    bind("GET", "/api/builds", R::Build, V::Read, Route),
    bind(
        "POST",
        "/api/builds/{id}/rebuild",
        R::Build,
        V::Update,
        Platform,
    ),
    bind("POST", "/api/packs/launch", R::Playbook, V::Launch, Route),
];

/// Every binding this build serves.
pub fn bindings() -> impl Iterator<Item = &'static Binding> {
    let core = BINDINGS.iter();
    #[cfg(feature = "autoresearch")]
    let core = core.chain(AUTORESEARCH_BINDINGS.iter());
    core
}

/// The binding for a served method and matched path template.
pub fn lookup(method: &Method, path: &str) -> Option<&'static Binding> {
    bindings().find(|b| b.method == method.as_str() && b.path == path)
}

/// The binding middleware: refuse an unbound route, decide a platform route, pass the rest.
pub async fn enforce(State(state): State<ApiState>, req: Request, next: Next) -> Response {
    let Some(matched) = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string())
    else {
        return next.run(req).await;
    };
    let method = req.method().clone();
    let Some(binding) = lookup(&method, &matched) else {
        return (
            StatusCode::FORBIDDEN,
            Json(ErrorBody::new(format!(
                "no authorization binding for {method} {matched}"
            ))),
        )
            .into_response();
    };
    if binding.resolver == Resolver::Route {
        return next.run(req).await;
    }
    let (mut parts, body) = req.into_parts();
    let caller = match Caller::from_request_parts(&mut parts, &state).await {
        Ok(caller) => caller,
        Err(refusal) => return refusal,
    };
    let resource = Resource::platform(binding.action.resource, parts.uri.path());
    let decision =
        match crate::authz::owner::decide(&state, &caller, binding.action, &resource).await {
            Ok(decision) => decision,
            Err(denied) => return denied.into_response(),
        };
    let mut req = Request::from_parts(parts, body);
    req.extensions_mut().insert(decision);
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn every_binding_names_a_defined_action_and_no_pair_repeats() {
        let mut seen = BTreeSet::new();
        for b in bindings() {
            assert!(
                b.action.resource.defines(b.action.verb),
                "{} {} binds {} which the type does not define",
                b.method,
                b.path,
                b.action
            );
            assert!(
                seen.insert((b.method, b.path)),
                "{} {} bound twice",
                b.method,
                b.path
            );
        }
        assert_eq!(
            lookup(&Method::POST, "/api/issues/{key}/park").map(|b| b.action.to_string()),
            Some("issue:update".to_string())
        );
        assert!(lookup(&Method::PATCH, "/api/issues/{key}/park").is_none());
    }

    /// Every method and path the OpenAPI document serves has a binding, and every binding names
    /// a served route (RFC-0003 C-ENFORCEMENT: the bindings are checkable against the route
    /// table). The two catch-alls are spelled with axum's `{*param}` in the table.
    #[test]
    fn the_binding_table_matches_the_served_routes() {
        let spec: serde_json::Value =
            serde_json::from_str(&crate::api::openapi_spec().expect("spec")).expect("json");
        let mut served = BTreeSet::new();
        for (path, item) in spec["paths"].as_object().expect("paths") {
            for method in item.as_object().expect("item").keys() {
                served.insert((method.to_uppercase(), path.clone()));
            }
        }
        served.insert(("GET".to_string(), "/api/openapi.json".to_string()));
        let bound: BTreeSet<(String, String)> = bindings()
            .map(|b| {
                (
                    b.method.to_string(),
                    b.path
                        .replace("{*key}", "{key}")
                        .replace("{*path}", "{path}"),
                )
            })
            .collect();
        let unbound: Vec<_> = served.difference(&bound).collect();
        assert!(
            unbound.is_empty(),
            "served routes without a binding: {unbound:?}"
        );
        let stale: Vec<_> = bound.difference(&served).collect();
        assert!(
            stale.is_empty(),
            "bindings for routes that are not served: {stale:?}"
        );
    }
}
