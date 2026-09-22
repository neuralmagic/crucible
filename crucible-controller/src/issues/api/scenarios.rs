use crate::api::dto::*;
use crate::api::state::*;
use crate::daemon::queue::IssueKey;
use crate::issues::model::InputKind;
use crate::playbooks::api::import::import_refusal;
use crate::playbooks::api::registry::{UNREADABLE_MANIFEST, undispatchable};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// --- scenario adopt + approve (admin-gated, ledgered) -------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct AdoptScenarioBody {
    title: String,
    body: String,
    /// Hint list, not a binding target: `affected_repos[0]` becomes the clone target
    /// (`issues.repo`); the full list rides the scope pod's goal framing for the pack agent to
    /// weigh, and it may propose a different repo set.
    affected_repos: Vec<String>,
    justification: String,
    #[serde(default)]
    authoritative: bool,
    /// Optional branch or tag to clone the target repo at. Omitted (or null) ⇒ the repo's default
    /// branch, which is what every adoption did before this existed.
    #[serde(default)]
    git_ref: Option<String>,
    /// Optional name of a broker codegen tool contract (one of `GET /api/config/broker-contracts`).
    /// Naming one makes the scenario GPU-measured through the broker's Kueue jobs; omitting it (or
    /// null) leaves the pack measuring locally on the loop pod.
    #[serde(default)]
    codegen_contract: Option<String>,
    /// Optional path to a pack the repo already carries, relative to the checkout root. Naming one
    /// makes the scope turn validate and freeze that pack instead of spending an agent turn
    /// drafting one; omitting it (or null) keeps the propose path.
    #[serde(default)]
    pack_path: Option<String>,
    /// Which cluster every pod this scenario dispatches must land on, from
    /// `GET /api/dispatch-targets`. Absent selects the controller's configured default.
    #[serde(default)]
    dispatch_target: Option<String>,
    /// Which registered inference provider this scenario's agent runs against, from
    /// `GET /api/config/providers`. Absent resolves through the configured defaults at each
    /// dispatch, which is what every adoption did before the registry existed.
    #[serde(default)]
    provider: Option<String>,
    /// The model to ask that provider for. Free text, not restricted to the curated list; absent
    /// takes the provider's own default model.
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct ScenarioAck {
    key: String,
    affected_repos: Vec<String>,
    tier: String,
    actor: Option<String>,
    /// The validated ref the turn pods will clone at, echoed back; `None` = default branch.
    git_ref: Option<String>,
    /// The configured contract name this scenario is measured under, echoed back; `None` = local
    /// measure.
    codegen_contract: Option<String>,
    /// The in-repo pack this scenario validates, echoed back; `None` = the scope turn drafts one.
    pack_path: Option<String>,
    /// The cluster this scenario was authorized to dispatch onto, resolved from the caller's
    /// eligible set.
    dispatch_target: String,
    /// The inference provider pinned on the row, echoed back; `None` resolves through the
    /// configured defaults at dispatch.
    provider: Option<String>,
    /// The model pinned alongside it; `None` takes the resolved provider's default.
    model: Option<String>,
}

/// Launch an already-authored autoresearch pack without an LLM scope turn. The controller freezes
/// the named repo/ref/path, compiles it with its pinned engine, and treats this admin POST as the
/// approval for that exact tarball.
#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct LaunchPackBody {
    repo: String,
    #[serde(default)]
    git_ref: Option<String>,
    #[serde(default)]
    path: Option<String>,
    justification: String,
    /// Which registered inference provider this pack's agent runs against, from
    /// `GET /api/config/providers`. Absent resolves through the configured defaults at dispatch.
    #[serde(default)]
    provider: Option<String>,
    /// The model to ask that provider for; absent takes the provider's own default.
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct LaunchPackAck {
    key: String,
    repo: String,
    rev: String,
    path: String,
    pack_digest: String,
    status: String,
    actor: Option<String>,
    /// The inference provider pinned on the row, echoed back; `None` resolves through the
    /// configured defaults at dispatch.
    provider: Option<String>,
    /// The model pinned alongside it; `None` takes the resolved provider's default.
    model: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/packs/launch",
    request_body = LaunchPackBody,
    responses(
        (status = 201, description = "Frozen pack accepted and queued directly for a run", body = LaunchPackAck),
        (status = 403, description = "Caller is not in the admin whitelist", body = ErrorBody),
        (status = 422, description = "Bad repo/ref/path or the pack does not compile", body = ErrorBody),
        (status = 502, description = "Cloning the repository failed", body = ErrorBody)
    )
)]
pub(crate) async fn launch_pack(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
    caller: crate::authz::Caller,
    Json(body): Json<LaunchPackBody>,
) -> Response {
    if let Some(msg) =
        require_non_empty(&[("repo", &body.repo), ("justification", &body.justification)])
    {
        return unprocessable(msg);
    }
    let git_ref = match require_git_ref(body.git_ref.as_deref()) {
        Ok(r) => r,
        Err(msg) => {
            return unprocessable(msg);
        }
    };
    let pin = match crate::playbooks::api::providers::require_agent_pin(
        &state,
        &caller,
        body.provider.as_deref(),
        body.model.as_deref(),
    )
    .await
    {
        Ok(Ok(pin)) => pin,
        Ok(Err(refusal)) => {
            return unprocessable(refusal.message);
        }
        Err(e) => return AppError::from(e).into_response(),
    };
    let repo = normalize_repo(body.repo.trim()).to_string();
    let path = body.path.unwrap_or_default().trim().to_string();
    if let Err(msg) = crate::playbooks::registry::validate_path(&path) {
        return unprocessable(msg);
    }

    let git = match crate::playbooks::registry::PackGit::resolve(state.pack_pr_app.as_ref()).await {
        Ok(g) => g,
        Err(e) => return AppError::from(e).into_response(),
    };
    let owned = (repo.clone(), git_ref.clone(), path.clone());
    let fetched = match tokio::task::spawn_blocking(move || {
        let (repo, git_ref, path) = owned;
        let fetched = crate::playbooks::registry::fetch_pack(
            &git,
            crate::playbooks::registry::PackSource {
                repo: &repo,
                git_ref: git_ref.as_deref(),
                path: &path,
                expected_rev: None,
            },
        )?;
        let preview = crate::playbooks::preview::preview_pack_for(
            fetched.pack.path(),
            crate::playbooks::registry::PackWorkflowKind::Autoresearch,
            &std::collections::BTreeMap::new(),
            crate::playbooks::preview::Unvalued::Refuse,
        )?;
        Ok::<_, crate::playbooks::registry::RegisterError>((fetched, preview))
    })
    .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return import_refusal(e),
        Err(e) => return AppError::from(anyhow::Error::from(e)).into_response(),
    };
    let (fetched, preview) = fetched;
    if !preview.diagnostics.is_empty() || preview.graph.is_none() {
        return unprocessable(format!(
            "pack did not compile: {}",
            preview.diagnostics.join("; ")
        ));
    }
    let Some(agent) = preview.agent.as_ref() else {
        return undispatchable(UNREADABLE_MANIFEST.to_string());
    };
    if let Some(refusal) = state.dispatch.refusal(agent) {
        return undispatchable(refusal);
    }

    let key = format!("scenario:{}", uuid::Uuid::now_v7());
    let digest =
        match crate::playbooks::packs::store_pack_tarball(state.db.pool(), &key, &fetched.tar_gz)
            .await
        {
            Ok(d) => d,
            Err(e) => return AppError::from(e).into_response(),
        };
    let actor = identity.as_deref().unwrap_or("unknown");
    let justification = body.justification.trim();
    let title = format!(
        "direct pack: {repo}/{}",
        if path.is_empty() { "." } else { &path }
    );
    let description = format!(
        "Direct launch of {repo} at {} path {:?}. Authorization: {justification}",
        fetched.rev, path
    );
    if let Err(e) = crate::issues::store::adopt_direct_pack(
        state.db.pool(),
        &crate::issues::model::NewDirectPack {
            key: &key,
            title: &title,
            body: &description,
            repo: &repo,
            git_ref: &fetched.rev,
            pack_digest: &digest,
            created_by: actor,
        },
    )
    .await
    {
        return AppError::from(e).into_response();
    }
    if let Err(e) = crate::issues::store::set_agent_dispatch(
        state.db.pool(),
        &key,
        pin.provider.as_deref(),
        pin.model.as_deref(),
    )
    .await
    {
        return AppError::from(e).into_response();
    }
    state
        .audit(
            crate::event_log::Event::now(
                &key,
                "awaiting-approval",
                "awaiting-approval",
                Some(&format!("direct pack launch authorized: {justification}")),
                Some(&digest),
            )
            .by(Some(actor)),
            "launch_pack",
        )
        .await;
    state.queue.enqueue_urgent(IssueKey(key.clone()));
    (
        StatusCode::CREATED,
        Json(LaunchPackAck {
            key,
            repo,
            rev: fetched.rev,
            path,
            pack_digest: digest,
            status: "awaiting-approval".to_string(),
            actor: identity.0,
            provider: pin.provider,
            model: pin.model,
        }),
    )
        .into_response()
}

/// `affected_repos` must be non-empty and every entry must be non-blank once trimmed; returns the
/// trimmed, slug-normalized list on success.
fn require_repos(repos: &[String]) -> Result<Vec<String>, String> {
    if repos.is_empty() {
        return Err("affected_repos must be non-empty".to_string());
    }
    let trimmed: Vec<String> = repos
        .iter()
        .map(|r| normalize_repo(r.trim()).to_string())
        .collect();
    if trimmed.iter().any(|r| r.is_empty()) {
        return Err("affected_repos entries must be non-empty".to_string());
    }
    Ok(trimmed)
}

/// Validate an optional in-repo pack path through the engine's own parser, so a caller learns at
/// adoption with a field-level 422 instead of watching a pod fail. Present-but-blank is a caller
/// mistake, not a synonym for "draft one".
pub(crate) fn require_pack_path(pack_path: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = pack_path else {
        return Ok(None);
    };
    crucible::deploy::PackPath::parse(raw)
        .map(|p| Some(p.as_str().to_string()))
        .map_err(|e| e.to_string())
}

/// Resolve the optional codegen-contract name against the deploy's configured set. Only the NAME is
/// stored: the JSON is re-read from config at every dispatch, so a redeploy that revises a contract
/// revises it for every scenario already adopted against it. An unknown name is a 422 naming the
/// configured set — the alternative, accepting it, would park the scenario at its first run dispatch
/// with a much worse error.
fn require_codegen_contract(
    name: Option<&str>,
    configured: &crate::config::BrokerContracts,
) -> Result<Option<String>, String> {
    let Some(raw) = name else {
        return Ok(None);
    };
    let n = raw.trim();
    if n.is_empty() {
        return Err(
            "codegen_contract must be non-empty when present (omit it for local measure)"
                .to_string(),
        );
    }
    if configured.get(n).is_none() {
        let names = configured.names();
        let known = if names.is_empty() {
            "none are configured".to_string()
        } else {
            names.join(", ")
        };
        return Err(format!(
            "codegen_contract {n:?} is not configured on this controller (configured: {known})"
        ));
    }
    Ok(Some(n.to_string()))
}

#[utoipa::path(
    post,
    path = "/api/scenarios",
    request_body = AdoptScenarioBody,
    responses(
        (status = 201, description = "Scenario adopted", body = ScenarioAck),
        (status = 403, description = "Caller is not in the admin whitelist", body = ErrorBody),
        (status = 422, description = "title/body/justification must be non-empty, affected_repos must be a non-empty list of non-blank entries, git_ref must be a plausible branch/tag name, codegen_contract must name a configured contract", body = ErrorBody)
    )
)]
pub(crate) async fn adopt_scenario(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
    caller: crate::authz::Caller,
    Json(body): Json<AdoptScenarioBody>,
) -> Response {
    if let Some(msg) = require_non_empty(&[
        ("title", &body.title),
        ("body", &body.body),
        ("justification", &body.justification),
    ]) {
        return unprocessable(msg);
    }
    let affected_repos = match require_repos(&body.affected_repos) {
        Ok(repos) => repos,
        Err(msg) => {
            return unprocessable(msg);
        }
    };
    let git_ref = match require_git_ref(body.git_ref.as_deref()) {
        Ok(r) => r,
        Err(msg) => {
            return unprocessable(msg);
        }
    };
    let codegen_contract =
        match require_codegen_contract(body.codegen_contract.as_deref(), &state.broker_contracts) {
            Ok(c) => c,
            Err(msg) => {
                return unprocessable(msg);
            }
        };
    let pack_path = match require_pack_path(body.pack_path.as_deref()) {
        Ok(p) => p,
        Err(msg) => {
            return unprocessable(msg);
        }
    };
    let pin = match crate::playbooks::api::providers::require_agent_pin(
        &state,
        &caller,
        body.provider.as_deref(),
        body.model.as_deref(),
    )
    .await
    {
        Ok(Ok(pin)) => pin,
        Ok(Err(refusal)) => {
            return unprocessable(refusal.message);
        }
        Err(e) => return AppError::from(e).into_response(),
    };
    let (title, text, justification) = (
        body.title.trim().to_string(),
        body.body.trim().to_string(),
        body.justification.trim().to_string(),
    );
    // A scenario declares no pack yet — the scope turn drafts one — so eligibility is checked
    // against the engine's default substrate. The pack's own backend is re-checked at launch.
    let dispatch_target = match crate::playbooks::api::saver::resolve_dispatch_target(
        &state,
        &caller,
        None,
        body.dispatch_target.as_deref(),
    )
    .await
    {
        Ok(target) => target,
        Err(refusal) => return refusal,
    };
    let actor = identity.as_deref().unwrap_or("unknown");

    let key = match crate::issues::store::adopt_scenario(
        state.db.pool(),
        &title,
        &text,
        &affected_repos,
        body.authoritative,
        crate::issues::store::AdoptPins {
            git_ref: git_ref.as_deref(),
            codegen_contract: codegen_contract.as_deref(),
            pack_path: pack_path.as_deref(),
        },
        actor,
    )
    .await
    {
        Ok(key) => key,
        Err(e) => return AppError::from(e).into_response(),
    };
    // Pinned before the reconcile kick below, so the first scope turn already dispatches onto the
    // cluster this adoption was authorized for.
    if let Err(e) =
        crate::issues::store::set_dispatch_target(state.db.pool(), &key, Some(&dispatch_target))
            .await
    {
        return AppError::from(e).into_response();
    }
    if let Err(e) = crate::issues::store::set_agent_dispatch(
        state.db.pool(),
        &key,
        pin.provider.as_deref(),
        pin.model.as_deref(),
    )
    .await
    {
        return AppError::from(e).into_response();
    }

    // The ledgered human-authorization stamp: the R1 reconcile branch (kind.has_upstream() ==
    // false) is what actually bypasses the caps — this event is the audit trail for who did it
    // and why, not a gate anything reads.
    state
        .audit(
            crate::event_log::Event::now(
                &key,
                "new",
                "new",
                Some(&format!("scenario adopted: {justification}")),
                None,
            )
            .by(Some(actor)),
            "adopt_scenario",
        )
        .await;

    // Kick reconcile so the adopted row flows New -> Scoped -> AwaitingApproval right away
    // instead of sitting at `new` until the next discovery-triggered pass or a restart.
    state.queue.enqueue_urgent(IssueKey(key.clone()));

    (
        StatusCode::CREATED,
        Json(ScenarioAck {
            key,
            affected_repos,
            tier: crucible_contract::Tier::T1.as_str().to_string(),
            actor: identity.0,
            git_ref,
            codegen_contract,
            pack_path,
            dispatch_target,
            provider: pin.provider,
            model: pin.model,
        }),
    )
        .into_response()
}

// --- jira adopt (admin-gated, ledgered) --------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct AdoptJiraBody {
    /// The Jira issue key, e.g. `ACME-1234`. The controller fetches its title/body server-side.
    issue_key: String,
    /// The site label baked into the stored key (`jira:{site}:{PROJ-N}`), e.g. `example`. Optional —
    /// when blank, it's derived from the configured Jira base URL's host.
    #[serde(default)]
    site: String,
    /// Hint list, not a binding target: `affected_repos[0]` becomes the clone target; the full list
    /// rides the scope pod's goal framing (same semantics as a scenario's affected repos).
    affected_repos: Vec<String>,
    justification: String,
    #[serde(default)]
    authoritative: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct JiraAck {
    key: String,
    issue_key: String,
    title: String,
    affected_repos: Vec<String>,
    tier: String,
    actor: Option<String>,
}

/// Adopt a Jira issue by key: fetch its title/body once (controller-side, with the operator's
/// creds), then store it exactly like a scenario so it rides the same non-upstream scope->loop path.
/// The loop never sees Jira creds. Unconfigured Jira ⇒ 503; a malformed key ⇒ 422; a fetch failure ⇒
/// 502. On success the row lands at `new` and skips the ranker's gates (adoption is the authorization).
#[utoipa::path(
    post,
    path = "/api/jira",
    request_body = AdoptJiraBody,
    responses(
        (status = 201, description = "Jira issue adopted", body = JiraAck),
        (status = 403, description = "Caller is not in the admin whitelist", body = ErrorBody),
        (status = 422, description = "issue_key/justification must be valid, affected_repos must be a non-empty list of non-blank entries", body = ErrorBody),
        (status = 502, description = "Jira fetch failed", body = ErrorBody),
        (status = 503, description = "Jira is not configured on this controller", body = ErrorBody)
    )
)]
pub(crate) async fn adopt_jira(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
    Json(body): Json<AdoptJiraBody>,
) -> Response {
    let Some(jira_cfg) = state.jira.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorBody::new(
                "Jira is not configured (set JIRA_BASE_URL, JIRA_EMAIL, JIRA_API_TOKEN)"
                    .to_string(),
            )),
        )
            .into_response();
    };
    if let Some(msg) = require_non_empty(&[
        ("issue_key", &body.issue_key),
        ("justification", &body.justification),
    ]) {
        return unprocessable(msg);
    }
    let affected_repos = match require_repos(&body.affected_repos) {
        Ok(repos) => repos,
        Err(msg) => {
            return unprocessable(msg);
        }
    };
    let site = if body.site.trim().is_empty() {
        jira_cfg.site_label()
    } else {
        body.site.trim().to_string()
    };
    let jira_ref = match crate::launches::jira::JiraRef::parse(&site, body.issue_key.trim()) {
        Ok(r) => r,
        Err(e) => {
            return unprocessable(format!("{e:#}"));
        }
    };
    let issue = match crate::launches::jira::fetch_jira_issue(&jira_cfg, &jira_ref).await {
        Ok(issue) => issue,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorBody::new(format!(
                    "fetching {}: {e:#}",
                    jira_ref.issue_key()
                ))),
            )
                .into_response();
        }
    };
    let justification = body.justification.trim().to_string();
    let actor = identity.as_deref().unwrap_or("unknown");
    let stored_key = jira_ref.storage_key();

    let key = match crate::issues::store::adopt_jira(
        state.db.pool(),
        &stored_key,
        &issue.title,
        &issue.body,
        &affected_repos,
        body.authoritative,
        actor,
    )
    .await
    {
        Ok(key) => key,
        Err(e) => return AppError::from(e).into_response(),
    };

    // The ledgered human-authorization stamp (the caps bypass is the has_upstream()==false reconcile
    // branch; this event is the audit trail for who adopted it and why, not a gate).
    state
        .audit(
            crate::event_log::Event::now(
                &key,
                "new",
                "new",
                Some(&format!(
                    "jira {} adopted: {justification}",
                    jira_ref.issue_key()
                )),
                None,
            )
            .by(Some(actor)),
            "adopt_jira",
        )
        .await;

    state.queue.enqueue_urgent(IssueKey(key.clone()));

    (
        StatusCode::CREATED,
        Json(JiraAck {
            key,
            issue_key: jira_ref.issue_key(),
            title: issue.title,
            affected_repos,
            tier: crucible_contract::Tier::T1.as_str().to_string(),
            actor: identity.0,
        }),
    )
        .into_response()
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct ScenarioApproveAck {
    key: String,
    scope_id: i64,
    approved_by: String,
    approved_at: String,
}

/// The UI-native replacement for the GitHub draft-PR approval gate, for any adopted non-upstream
/// item (scenario or Jira) with no upstream to open a PR against: stamp `approved_at` on its pending
/// scope directly. Reconcile's existing `is_approved()` gate then launches the run — same
/// "approved_at gates the run" semantics the draft-PR poll uses, fed from this endpoint instead of a
/// PR label.
#[utoipa::path(
    post,
    path = "/api/scenarios/{key}/approve",
    params(
        ("key" = String, Path, description = "Scenario key (percent-encoded)")
    ),
    responses(
        (status = 200, description = "Scope approved", body = ScenarioApproveAck),
        (status = 403, description = "Caller is not in the admin whitelist", body = ErrorBody),
        (status = 404, description = "Not a tracked scenario, or not yet scoped", body = ErrorBody),
        (status = 409, description = "Scope was already approved by someone else", body = ScenarioApproveAck)
    )
)]
pub(crate) async fn approve_scenario(
    State(state): State<ApiState>,
    Path(key): Path<String>,
    identity: crate::identity::session::Identity,
    _admin: crate::identity::auth::AdminGuard,
) -> Response {
    let issue = match crate::issues::store::get_issue(state.db.pool(), &key).await {
        Ok(Some(issue)) => issue,
        Ok(None) => {
            return not_found(format!("{key}: not tracked"));
        }
        Err(e) => return AppError::from(e).into_response(),
    };
    // The UI approval serves any adopted, non-upstream item (scenario or Jira) — both lack a GitHub
    // draft PR to open the approval against. A GitHub row goes through the draft-PR approval instead.
    if !matches!(
        issue.kind,
        InputKind::Scenario { .. } | InputKind::Jira { .. }
    ) {
        return not_found(format!("{key}: not an adopted scenario or jira item"));
    }
    let scope = match crate::issues::store::latest_scope_for_issue(state.db.pool(), &key).await {
        Ok(Some(scope)) => scope,
        Ok(None) => {
            return not_found(format!("{key}: no scope pack awaiting approval"));
        }
        Err(e) => return AppError::from(e).into_response(),
    };

    let actor = identity.as_deref().unwrap_or("unknown").to_string();
    let approved_at = crate::clock::now_rfc3339();
    match crate::issues::store::record_approval(state.db.pool(), scope.id, &actor, &approved_at)
        .await
    {
        Ok(true) => {}
        // `approved_at IS NOT NULL` already — someone beat this caller to the stamp. Report the
        // stamp that actually landed instead of a fabricated 200, and skip the audit event: the
        // ledger already has the real approval.
        Ok(false) => {
            return (
                StatusCode::CONFLICT,
                Json(ScenarioApproveAck {
                    key,
                    scope_id: scope.id,
                    approved_by: scope.approved_by.unwrap_or_default(),
                    approved_at: scope.approved_at.unwrap_or_default(),
                }),
            )
                .into_response();
        }
        Err(e) => return AppError::from(e).into_response(),
    }
    state
        .audit(
            crate::event_log::Event::now(
                &key,
                issue.status.as_str(),
                issue.status.as_str(),
                Some("approved via UI"),
                None,
            )
            .by(Some(&actor)),
            "approve_scenario",
        )
        .await;

    // Kick reconcile so the now-approved scope launches right away instead of waiting on the
    // next discovery-triggered pass.
    state.queue.enqueue_urgent(IssueKey(key.clone()));

    (
        StatusCode::OK,
        Json(ScenarioApproveAck {
            key,
            scope_id: scope.id,
            approved_by: actor,
            approved_at,
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pack path is refused at adoption, with the engine's own diagnostic, rather than being
    /// discovered by a pod that fails.
    #[test]
    fn a_pack_path_is_refused_at_adoption() {
        for bad in [
            "",
            "  ",
            "/etc",
            "../secrets",
            "packs/../../etc",
            "pack dir",
        ] {
            assert!(require_pack_path(Some(bad)).is_err(), "{bad:?}");
        }
        assert_eq!(
            require_pack_path(Some("examples/selfhost/")).expect("valid"),
            Some("examples/selfhost".to_string())
        );
        assert_eq!(require_pack_path(None).expect("absent"), None);
    }

    #[test]
    fn normalize_repo_reduces_github_urls_to_slugs() {
        assert_eq!(
            normalize_repo("https://github.com/llm-d/llm-d-router"),
            "llm-d/llm-d-router"
        );
        assert_eq!(
            normalize_repo("https://github.com/llm-d/llm-d-router/"),
            "llm-d/llm-d-router"
        );
        assert_eq!(normalize_repo("https://github.com/o/r.git"), "o/r");
        assert_eq!(normalize_repo("http://github.com/o/r"), "o/r");
    }

    #[test]
    fn normalize_repo_passes_through_slugs_and_non_github_urls() {
        assert_eq!(normalize_repo("o/r"), "o/r");
        assert_eq!(
            normalize_repo("https://gitlab.com/o/r.git"),
            "https://gitlab.com/o/r.git"
        );
        assert_eq!(
            normalize_repo("git@github.com:o/r.git"),
            "git@github.com:o/r.git"
        );
    }

    #[test]
    fn require_repos_normalizes_and_rejects_blanks() {
        let got = require_repos(&[
            " https://github.com/llm-d/llm-d-router ".to_string(),
            "o/r".to_string(),
        ])
        .expect("valid");
        assert_eq!(
            got,
            vec!["llm-d/llm-d-router".to_string(), "o/r".to_string()]
        );
        assert!(require_repos(&[]).is_err());
        assert!(require_repos(&["  ".to_string()]).is_err());
    }
}
