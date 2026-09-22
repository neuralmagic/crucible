//! The autoresearch lane's routes: issues and their journey, the scope approval gate, watched
//! repos, reranking, scenarios, builds, turns and the autopilot flag. Mounted by
//! [`crate::api::router`] when `CONTROLLER_AUTORESEARCH` is on.

use crate::api::state::ApiState;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

#[derive(OpenApi)]
#[openapi(
    paths(
        crate::runs::api::runs::live_turn,
        crate::issues::api::issues::list_issues,
        crate::issues::api::issues::issue_facets,
        crate::issues::api::issues::get_issue,
        crate::issues::api::issues::get_issue_journey,
        crate::builds::api::list_builds,
        crate::builds::api::list_issue_builds,
        crate::builds::api::rebuild_build,
        crate::runs::api::turns::list_turns,
        crate::runs::api::turns::get_turn,
        crate::issues::api::approvals::get_scope_evidence,
        crate::issues::api::approvals::get_scope_report,
        crate::issues::api::approvals::get_scope_transcript,
        crate::issues::api::repos::get_repos,
        crate::issues::api::repos::add_repo,
        crate::issues::api::repos::pause_repo,
        crate::issues::api::repos::resume_repo,
        crate::issues::api::repos::unwatch_repo,
        crate::issues::api::overrides::bump_issue,
        crate::issues::api::overrides::redispatch_issue,
        crate::issues::api::rerank::rerank_issue,
        crate::issues::api::rerank::rerank_issues,
        crate::daemon::api::autopilot::get_autopilot,
        crate::daemon::api::autopilot::set_autopilot,
        crate::issues::api::overrides::scope_now,
        crate::issues::api::scenarios::launch_pack,
        crate::issues::api::scenarios::adopt_scenario,
        crate::issues::api::scenarios::approve_scenario,
        crate::issues::api::scenarios::adopt_jira,
    ),
    components(schemas(
        crate::api::dto::IssueDetail,
        crate::api::dto::IssueCommentDto,
        crate::api::dto::AutopilotDto,
        crate::api::dto::AutopilotSetBody,
        crate::issues::api::issues::IssueFacetsDto,
        crate::issues::api::issues::FacetCount,
        crate::builds::api::BuildDto,
        crate::builds::api::RebuildAck,
        crate::issues::journey::JourneyDto,
        crate::issues::journey::JourneyStep,
        crate::runs::api::turns::WorkPodDto,
        crate::issues::refine_trail::ScopeEvidenceDto,
        crate::issues::refine_trail::ScopeReportDto,
        crate::issues::refine_trail::ScopeStage,
        crate::issues::refine_trail::RoundRecord,
        crate::issues::refine_trail::RoundKind,
        crate::issues::refine_trail::RoundOutcome,
        crate::issues::refine_trail::FailureEvidence,
        crate::issues::refine_trail::Attack,
        crate::issues::refine_trail::AttackKind,
        crate::issues::refine_trail::SelftestEvidence,
        crate::issues::refine_trail::ControlEvidence,
        crate::issues::refine_trail::ReadingEvidence,
        crate::issues::api::repos::AddRepoBody,
        crate::issues::api::repos::RepoActionAck,
        crate::issues::api::rerank::RerankFilter,
        crate::issues::api::rerank::RerankAck,
        crate::issues::api::rerank::BulkRerankAck,
        crate::issues::api::overrides::ScopeNowBody,
        crate::issues::api::overrides::RedispatchBody,
        crate::issues::api::scenarios::AdoptScenarioBody,
        crate::issues::api::scenarios::ScenarioAck,
        crate::issues::api::scenarios::ScenarioApproveAck,
        crate::issues::api::scenarios::AdoptJiraBody,
        crate::issues::api::scenarios::JiraAck,
    ))
)]
pub(crate) struct AutoresearchDoc;

pub(crate) fn routes(router: OpenApiRouter<ApiState>) -> OpenApiRouter<ApiState> {
    router
        .routes(routes!(crate::runs::api::runs::live_turn))
        .routes(routes!(crate::issues::api::issues::list_issues))
        .routes(routes!(crate::issues::api::issues::issue_facets))
        .routes(routes!(crate::issues::api::issues::get_issue))
        .routes(routes!(crate::issues::api::issues::get_issue_journey))
        .routes(routes!(crate::builds::api::list_builds))
        .routes(routes!(crate::builds::api::list_issue_builds))
        .routes(routes!(crate::builds::api::rebuild_build))
        .routes(routes!(crate::runs::api::turns::list_turns))
        .routes(routes!(crate::runs::api::turns::get_turn))
        .routes(routes!(crate::issues::api::approvals::get_scope_evidence))
        .routes(routes!(crate::issues::api::approvals::get_scope_report))
        .routes(routes!(crate::issues::api::approvals::get_scope_transcript))
        .routes(routes!(crate::issues::api::repos::get_repos))
        .routes(routes!(crate::issues::api::repos::add_repo))
        .routes(routes!(crate::issues::api::repos::pause_repo))
        .routes(routes!(crate::issues::api::repos::resume_repo))
        .routes(routes!(crate::issues::api::repos::unwatch_repo))
        .routes(routes!(crate::issues::api::overrides::bump_issue))
        .routes(routes!(crate::issues::api::overrides::redispatch_issue))
        .routes(routes!(crate::issues::api::rerank::rerank_issue))
        .routes(routes!(crate::issues::api::rerank::rerank_issues))
        .routes(routes!(crate::daemon::api::autopilot::get_autopilot))
        .routes(routes!(crate::daemon::api::autopilot::set_autopilot))
        .routes(routes!(crate::issues::api::overrides::scope_now))
        .routes(routes!(crate::issues::api::scenarios::launch_pack))
        .routes(routes!(crate::issues::api::scenarios::adopt_scenario))
        .routes(routes!(crate::issues::api::scenarios::approve_scenario))
        .routes(routes!(crate::issues::api::scenarios::adopt_jira))
}
