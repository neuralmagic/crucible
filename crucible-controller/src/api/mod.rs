//! The `/api` router and its OpenAPI document. The handlers live in the feature slices
//! (`crate::issues::api`, `crate::runs::api`, ...); this module mounts them and generates the spec
//! served at `GET /api/openapi.json`.
//!
//! Every matched route, GET included, passes through [`crate::authz::bindings::enforce`], which
//! looks the method and route template up in [`crate::authz::bindings::BINDINGS`] and answers 403
//! when there is no entry. A `Resolver::Route` binding passes through once found (the handler
//! decides on the resource it resolves); a `Resolver::Platform` binding takes a policy decision in
//! the middleware.

use axum::Router;
use axum::routing::get;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

pub mod state;
use state::*;

#[cfg(feature = "autoresearch")]
mod autoresearch;

pub(crate) mod dto;
pub(crate) mod events;
pub(crate) mod metrics;
pub(crate) mod system;
#[cfg(test)]
pub(crate) mod tests;

/// The OpenAPI specification for all `/api/*` routes. Generated at compile time from handler
/// annotations and served at `GET /api/openapi.json`.
#[derive(OpenApi)]
#[openapi(
    paths(
        system::healthz,
        system::version,
        system::whoami,
        crate::identity::api::credentials::get_credential,
        crate::identity::api::credentials::revoke_credential,
        crate::identity::api::keys::list_keys,
        crate::identity::api::keys::mint_key,
        crate::identity::api::keys::revoke_key,
        system::get_access,
        crate::identity::api::prefs::get_prefs,
        crate::identity::api::prefs::put_prefs,
        crate::identity::api::prefs::get_editor_prefs,
        crate::identity::api::prefs::put_editor_prefs,
        crate::identity::api::prefs::get_picker_prefs,
        crate::identity::api::prefs::put_picker_prefs,
        system::overview,
        dto::funnel,
        crate::runs::api::runs::list_runs,
        crate::images::api::list_images,
        crate::images::api::refresh_images,
        crate::playbooks::api::images::rank_images,
        crate::runs::api::cluster_view::list_clusters,
        crate::runs::api::cluster_view::list_dispatch_targets,
        crate::runs::api::runs::get_run,
        crate::runs::api::runs::get_run_iterations,
        crate::runs::api::runs::get_run_graph,
        crate::runs::api::evidence::get_task_evidence,
        crate::runs::api::evidence::get_run_log,
        crate::runs::api::evidence::get_run_files,
        crate::runs::api::evidence::get_run_file,
        crate::runs::api::runs::list_run_artifacts,
        crate::runs::api::runs::get_artifact,
        crate::runs::api::flow::get_run_flow_enriched,
        crate::runs::api::runs::export_runs,
        crate::runs::api::runs::export_iterations,
        crate::runs::api::runs::live_run,
        system::ledger_summary,
        system::ledger_by_tag,
        events::list_events,
        events::events_stream,
        crate::issues::api::approvals::get_approvals,
        crate::issues::api::overrides::park_issue,
        crate::issues::api::overrides::unpark_issue,
        crate::issues::api::reconcile_manual::trigger_reconcile,
        crate::playbooks::api::registry::register_playbook,
        crate::playbooks::api::import::import_candidates,
        crate::playbooks::api::import::propose_pack_import,
        crate::playbooks::api::import::get_pack_import,
        crate::playbooks::api::import::compile_pack_import,
        crate::playbooks::api::import::register_pack_import,
        crate::playbooks::api::import::discard_pack_import,
        crate::playbooks::api::import::draft_from_pack_import,
        crate::playbooks::api::import::create_draft_from_git,
        crate::playbooks::api::drafts::create_playbook_draft,
        crate::playbooks::api::drafts::list_playbook_drafts,
        crate::playbooks::api::drafts::get_playbook_draft,
        crate::playbooks::api::drafts::get_playbook_draft_files,
        crate::playbooks::api::drafts::get_playbook_draft_preview,
        crate::playbooks::api::drafts::get_playbook_draft_origin_files,
        crate::playbooks::api::drafts::get_playbook_draft_tarball,
        crate::playbooks::api::drafts::save_playbook_draft,
        crate::playbooks::api::drafts::delete_playbook_draft,
        crate::playbooks::api::drafts::launch_playbook_draft,
        crate::playbooks::api::drafts::graduate_playbook_draft,
        crate::playbooks::api::drafts::publish_playbook_draft,
        crate::playbooks::api::registry::list_playbooks,
        crate::playbooks::api::registry::get_playbook,
        crate::playbooks::api::registry::get_playbook_schema,
        crate::playbooks::api::registry::launch_playbook,
        crate::launches::api::playbook_runs::list_playbook_runs,
        crate::launches::api::playbook_runs::get_playbook_run,
        crate::launches::api::playbook_runs::create_one_shot,
        crate::launches::api::playbook_runs::list_one_shots,
        crate::launches::api::playbook_runs::cancel_one_shot,
        crate::launches::api::schedules::create_schedule,
        crate::launches::api::schedules::list_schedules,
        crate::launches::api::schedules::get_schedule,
        crate::launches::api::schedules::update_schedule,
        crate::launches::api::schedules::delete_schedule,
        crate::launches::api::schedules::preview_schedule,
        crate::launches::api::watches::create_watch,
        crate::launches::api::watches::list_watches,
        crate::launches::api::watches::list_watch_trackers,
        crate::launches::api::watches::preview_watch,
        crate::launches::api::watches::get_watch,
        crate::launches::api::watches::update_watch,
        crate::launches::api::watches::delete_watch,
        crate::launches::api::watches::set_watch_enabled,
        crate::launches::api::watches::list_watch_hits,
        crate::launches::api::watches::reset_watch_hit,
        crate::launches::api::emissions::emit_run,
        crate::runs::api::external_runs::put_external_run_session,
        crate::daemon::api::overrides::get_config,
        crate::daemon::api::overrides::put_config_overrides,
        crate::daemon::api::overrides::get_broker_contracts,
        crate::daemon::api::overrides::get_playbook_caps,
        crate::playbooks::api::providers::get_dispatch_providers,
        crate::playbooks::api::providers::list_providers,
        crate::playbooks::api::providers::register_provider,
        crate::playbooks::api::providers::update_provider,
        crate::playbooks::api::providers::delete_provider,
        crate::playbooks::api::providers::put_dispatch_default,
        crate::playbooks::api::providers::delete_dispatch_default,
        crate::playbooks::api::drafts::get_co_draft_skill,
        crate::playbooks::api::drafts::get_co_draft,
        crate::secrets::api::register_secret,
        crate::secrets::api::list_secrets,
        crate::secrets::api::get_secret,
        crate::secrets::api::delete_secret,
        crate::secrets::api::rotate_secret,
        crate::secrets::api::transfer_secret,
        crate::secrets::api::bind_secret,
        crate::secrets::api::unbind_secret,
        crate::secrets::api::list_secret_audit,
        crate::authz::api::list_teams,
        crate::authz::api::create_team,
        crate::authz::api::get_team,
        crate::authz::api::rename_team,
        crate::authz::api::delete_team,
        crate::authz::api::put_members,
        crate::authz::api::team_audit,
        crate::authz::api::list_actions,
        crate::authz::api::get_schema,
        crate::authz::api::list_policy_sets,
        crate::authz::api::create_policy_set,
        crate::authz::api::get_policy_set,
        crate::authz::api::activate_policy_set,
        crate::authz::transfer::transfer_playbook,
        crate::authz::transfer::transfer_playbook_draft,
        crate::authz::transfer::transfer_pack_import,
        crate::authz::transfer::transfer_provider,
        crate::authz::shares::list_playbook_shares,
        crate::authz::shares::grant_playbook_share,
        crate::authz::shares::revoke_playbook_share,
        crate::authz::shares::list_playbook_draft_shares,
        crate::authz::shares::grant_playbook_draft_share,
        crate::authz::shares::revoke_playbook_draft_share,
        crate::authz::shares::list_pack_import_shares,
        crate::authz::shares::grant_pack_import_share,
        crate::authz::shares::revoke_pack_import_share,
        crate::authz::shares::list_provider_shares,
        crate::authz::shares::grant_provider_share,
        crate::authz::shares::revoke_provider_share,
    ),
    components(schemas(
        system::Health,
        system::Whoami,
        crate::identity::auth::Role,
        system::AccessDto,
        crate::identity::api::prefs::PrefsDto,
        crate::identity::api::prefs::EditorPrefsDto,
        crate::identity::api::prefs::PickerPrefsDto,
        dto::Overview,
        dto::StatusCount,
        dto::TierCount,
        dto::FunnelDto,
        dto::FunnelStage,
        dto::CapMetric,
        dto::CostMetric,
        dto::LongText,
        dto::IssueDto,
        dto::ScopeDto,
        dto::RunDto,
        crate::runs::api::runs::RunRowDto,
        crate::images::api::CatalogDto,
        crate::images::api::CatalogImageDto,
        crate::images::api::CatalogRepositoryDto,
        crucible_capability::CapabilityDoc,
        crucible_capability::Unsatisfied,
        crate::playbooks::preflight::ImagePreflight,
        crate::playbooks::preflight::RankedCatalog,
        crate::playbooks::preflight::RankedImage,
        crate::playbooks::preflight::ExcludedImage,
        crate::playbooks::api::images::RankImagesBody,
        dto::CandidateDto,
        dto::RunDetail,
        dto::PlanTaskDto,
        dto::TaskResultDto,
        dto::RunGraphDto,
        dto::GraphOutputDto,
        dto::OutputSourceDto,
        dto::OutputTargetDto,
        dto::ExposureDto,
        crate::runs::artifacts::ArtifactManifest,
        crate::runs::artifacts::ArtifactEntry,
        dto::ScopeDetail,
        dto::EventDto,
        dto::LedgerSummaryDto,
        dto::LedgerDayDto,
        system::LedgerByTagDto,
        system::LedgerTagDto,
        dto::ApprovalsDto,
        dto::PendingImportDto,
        dto::AwaitingApprovalDto,
        dto::KeptPrDto,
        dto::RepoHealthDto,
        crate::issues::api::overrides::OverrideAck,
        crate::playbooks::api::registry::RegisterPlaybookBody,
        crate::playbooks::api::registry::RegisterAck,
        crate::playbooks::api::import::ImportCandidatesBody,
        crate::playbooks::api::import::ImportCandidateDto,
        crate::playbooks::api::import::ImportCandidatesDto,
        crate::playbooks::api::import::ProposeImportBody,
        crate::playbooks::api::import::CompileImportBody,
        crate::playbooks::api::import::ImportCompileDto,
        crate::playbooks::api::import::RegisterImportBody,
        crate::playbooks::api::import::ImportDraftBody,
        crate::playbooks::api::import::PackImportDto,
        crate::playbooks::api::drafts::CreateDraftBody,
        crate::playbooks::api::drafts::SaveDraftBody,
        crate::playbooks::api::drafts::LaunchDraftBody,
        crate::playbooks::api::drafts::GraduateDraftBody,
        crate::playbooks::api::drafts::GraduateAck,
        crate::playbooks::api::drafts::PublishDraftBody,
        crate::playbooks::api::drafts::PlaybookDraftDto,
        crate::playbooks::api::drafts::PlaybookDraftDetail,
        crate::playbooks::api::drafts::DraftVersionDto,
        crate::playbooks::api::drafts::DraftCompileDto,
        crate::playbooks::api::drafts::DraftFilesDto,
        crate::playbooks::api::drafts::StaleBaseBody,
        crate::playbooks::api::drafts::CoDraftDto,
        crate::playbooks::api::drafts::CoDraftStepDto,
        crate::playbooks::drafts::Diagnostic,
        crate::playbooks::drafts::DiagnosticKind,
        crate::playbooks::plan_graph::WorkflowGraphDto,
        crate::playbooks::plan_graph::GraphNodeDto,
        crate::playbooks::plan_graph::GraphEdgeDto,
        crate::playbooks::plan_graph::FanOutDto,
        crate::playbooks::plan_graph::TaskKind,
        crate::playbooks::plan_graph::Needs,
        crate::playbooks::plan_graph::Join,
        crate::playbooks::api::registry::PlaybookDto,
        crate::playbooks::api::registry::PlaybookSourceDto,
        crate::playbooks::api::registry::SandboxResourcesDto,
        crate::playbooks::api::registry::PlaybookDetailDto,
        crate::playbooks::api::registry::LaunchPlaybookBody,
        crate::playbooks::api::registry::PlaybookLaunchAck,
        crate::launches::api::playbook_runs::PlaybookRunDto,
        crate::launches::api::playbook_runs::PlaybookLaunchDetailDto,
        crate::launches::api::playbook_runs::LaunchDispatchDto,
        crate::launches::api::playbook_runs::LaunchRunDto,
        crate::launches::api::playbook_runs::DispatchState,
        crate::launches::api::playbook_runs::OneShotDto,
        crate::launches::api::playbook_runs::OneShotView,
        crate::launches::api::playbook_runs::CreateOneShotBody,
        crate::launches::api::schedules::ScheduleDto,
        crate::launches::api::schedules::ScheduleBody,
        crate::launches::api::schedules::SchedulePreviewBody,
        crate::launches::api::schedules::SchedulePreviewDto,
        crate::launches::api::watches::WatchDto,
        crate::launches::api::watches::WatchBody,
        crate::launches::api::watches::WatchHitDto,
        crate::launches::api::watches::WatchEnabledBody,
        crate::launches::api::watches::WatchPreviewBody,
        crate::launches::api::watches::WatchPreviewDto,
        crate::launches::api::watches::TrackersDto,
        dto::ValidationErrorBody,
        crate::playbooks::registry::FieldError,
        ErrorBody,
        crate::daemon::api::overrides::ConfigDto,
        crate::daemon::overrides_store::KnobView,
        crate::daemon::overrides_store::Source,
        crate::daemon::overrides_store::OverrideSet,
        crate::daemon::api::overrides::ConfigOverridesBody,
        crate::daemon::api::overrides::BrokerContractsDto,
        crate::daemon::api::overrides::PlaybookCapsDto,
        crate::playbooks::api::providers::ProviderDto,
        crate::playbooks::api::providers::ProviderDetailDto,
        crate::playbooks::api::providers::ProviderBody,
        crate::playbooks::api::providers::RegisterProviderBody,
        crate::playbooks::api::providers::DispatchDefaultDto,
        crate::playbooks::api::providers::DispatchDefaultBody,
        crate::playbooks::api::providers::DispatchProvidersDto,
        crate::playbooks::providers::ProviderKind,
        crate::playbooks::providers::WorkloadClass,
        crate::playbooks::providers::DefaultScope,
        crate::secrets::api::SecretDto,
        crate::secrets::api::SecretDetailDto,
        crate::secrets::api::SecretBindingDto,
        crate::secrets::api::SecretAuditDto,
        crate::secrets::api::PackSecretsDto,
        crate::secrets::api::DeclaredSecretDto,
        crate::secrets::api::RegisterSecretBody,
        crate::secrets::api::RotateSecretBody,
        crate::secrets::api::TransferSecretBody,
        crate::secrets::api::BindSecretBody,
        crate::secrets::SecretKind,
        crate::secrets::Visibility,
        crate::secrets::ConsumerClass,
        crate::secrets::SecretMode,
        crate::secrets::ScopeKind,
        crate::secrets::ProjectionKind,
        crate::secrets::AuditAction,
        crate::authz::api::TeamDto,
        crate::authz::api::MemberDto,
        crate::authz::api::MemberBody,
        crate::authz::api::CreateTeamBody,
        crate::authz::api::RenameTeamBody,
        crate::authz::api::PutMembersBody,
        crate::authz::api::TeamDeleteRefusal,
        crate::authz::api::TeamMembershipDto,
        crate::authz::api::AuthzAuditDto,
        crate::authz::model::TeamSlug,
        crate::authz::model::TeamRole,
        crate::authz::model::MemberKind,
        crate::authz::model::Membership,
        crate::authz::model::Via,
        crate::authz::store::OwnedResource,
        crate::authz::api::ActionDto,
        crate::authz::api::PolicySetDto,
        crate::authz::api::PolicySetBody,
        crate::authz::api::ActivationDto,
        crate::authz::action::ResourceType,
        crate::authz::action::Verb,
        crate::authz::decision::DenialBody,
        crate::authz::transfer::TransferBody,
        crate::authz::transfer::TransferDto,
        crate::authz::shares::ShareBody,
        crate::authz::shares::ShareDto,
        crate::authz::decision::ShareRole,
        crate::authz::decision::Decision,
    ))
)]
struct ApiDoc;

/// Return the OpenAPI spec as JSON. Can be called without a running server for typegen workflows.
pub(crate) fn openapi_spec() -> anyhow::Result<String> {
    #[allow(unused_mut)]
    let mut doc = ApiDoc::openapi();
    #[cfg(feature = "autoresearch")]
    doc.merge(autoresearch::AutoresearchDoc::openapi());
    Ok(doc.to_json()?)
}

/// Build the API router alone (used by tests that only exercise `/api/*` + `/healthz`; `spa`
/// merges its pages onto the same state). Uses utoipa-axum's OpenApiRouter so every route is
/// type-checked against the spec at compile time — a handler can't be routed without its schema.
pub fn router(state: ApiState) -> Router {
    let router = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(system::healthz))
        .routes(routes!(system::version))
        .routes(routes!(system::whoami))
        .routes(routes!(
            crate::identity::api::credentials::get_credential,
            crate::identity::api::credentials::revoke_credential
        ))
        .routes(routes!(
            crate::identity::api::keys::list_keys,
            crate::identity::api::keys::mint_key
        ))
        .routes(routes!(crate::identity::api::keys::revoke_key))
        .routes(routes!(system::get_access))
        .routes(routes!(system::overview))
        .routes(routes!(dto::funnel))
        .routes(routes!(crate::runs::api::runs::list_runs))
        .routes(routes!(crate::images::api::list_images))
        .routes(routes!(crate::images::api::refresh_images))
        .routes(routes!(crate::playbooks::api::images::rank_images))
        .routes(routes!(crate::runs::api::cluster_view::list_clusters))
        .routes(routes!(
            crate::runs::api::cluster_view::list_dispatch_targets
        ))
        .routes(routes!(crate::runs::api::runs::get_run))
        .routes(routes!(crate::runs::api::runs::get_run_iterations))
        .routes(routes!(crate::runs::api::runs::get_run_graph))
        .routes(routes!(crate::runs::api::evidence::get_task_evidence))
        .routes(routes!(crate::runs::api::evidence::get_run_log))
        .routes(routes!(crate::runs::api::evidence::get_run_files))
        .routes(routes!(crate::runs::api::runs::list_run_artifacts))
        .routes(routes!(crate::runs::api::flow::get_run_flow_enriched))
        .routes(routes!(crate::runs::api::runs::export_runs))
        .routes(routes!(crate::runs::api::runs::export_iterations))
        .routes(routes!(crate::runs::api::runs::live_run))
        .routes(routes!(system::ledger_summary))
        .routes(routes!(system::ledger_by_tag))
        .routes(routes!(events::list_events))
        .routes(routes!(events::events_stream))
        .routes(routes!(crate::issues::api::approvals::get_approvals))
        .routes(routes!(crate::issues::api::overrides::park_issue))
        .routes(routes!(crate::issues::api::overrides::unpark_issue))
        .routes(routes!(
            crate::issues::api::reconcile_manual::trigger_reconcile
        ))
        .routes(routes!(crate::playbooks::api::registry::register_playbook))
        .routes(routes!(crate::playbooks::api::import::import_candidates))
        .routes(routes!(crate::playbooks::api::import::propose_pack_import))
        .routes(routes!(crate::playbooks::api::import::get_pack_import))
        .routes(routes!(crate::playbooks::api::import::compile_pack_import))
        .routes(routes!(crate::playbooks::api::import::register_pack_import))
        .routes(routes!(crate::playbooks::api::import::discard_pack_import))
        .routes(routes!(
            crate::playbooks::api::import::draft_from_pack_import
        ))
        .routes(routes!(
            crate::playbooks::api::import::create_draft_from_git
        ))
        .routes(routes!(
            crate::playbooks::api::drafts::create_playbook_draft,
            crate::playbooks::api::drafts::list_playbook_drafts
        ))
        .routes(routes!(
            crate::playbooks::api::drafts::get_playbook_draft,
            crate::playbooks::api::drafts::delete_playbook_draft
        ))
        .routes(routes!(
            crate::playbooks::api::drafts::get_playbook_draft_files
        ))
        .routes(routes!(
            crate::playbooks::api::drafts::get_playbook_draft_preview
        ))
        .routes(routes!(
            crate::playbooks::api::drafts::get_playbook_draft_origin_files
        ))
        .routes(routes!(
            crate::playbooks::api::drafts::get_playbook_draft_tarball
        ))
        .routes(routes!(crate::playbooks::api::drafts::save_playbook_draft))
        .routes(routes!(
            crate::playbooks::api::drafts::launch_playbook_draft
        ))
        .routes(routes!(
            crate::playbooks::api::drafts::graduate_playbook_draft
        ))
        .routes(routes!(
            crate::playbooks::api::drafts::publish_playbook_draft
        ))
        .routes(routes!(crate::playbooks::api::drafts::get_co_draft_skill))
        .routes(routes!(crate::playbooks::api::drafts::get_co_draft))
        .routes(routes!(crate::playbooks::api::registry::list_playbooks))
        .routes(routes!(crate::playbooks::api::registry::get_playbook))
        .routes(routes!(
            crate::playbooks::api::registry::get_playbook_schema
        ))
        .routes(routes!(crate::playbooks::api::registry::launch_playbook))
        .routes(routes!(
            crate::launches::api::playbook_runs::list_playbook_runs
        ))
        .routes(routes!(
            crate::launches::api::playbook_runs::get_playbook_run
        ))
        .routes(routes!(
            crate::launches::api::playbook_runs::create_one_shot
        ))
        .routes(routes!(crate::launches::api::playbook_runs::list_one_shots))
        .routes(routes!(
            crate::launches::api::playbook_runs::cancel_one_shot
        ))
        .routes(routes!(
            crate::launches::api::schedules::create_schedule,
            crate::launches::api::schedules::list_schedules
        ))
        .routes(routes!(crate::launches::api::schedules::preview_schedule))
        .routes(routes!(
            crate::launches::api::schedules::get_schedule,
            crate::launches::api::schedules::update_schedule,
            crate::launches::api::schedules::delete_schedule
        ))
        .routes(routes!(
            crate::launches::api::watches::create_watch,
            crate::launches::api::watches::list_watches
        ))
        .routes(routes!(crate::launches::api::watches::list_watch_trackers))
        .routes(routes!(crate::launches::api::watches::preview_watch))
        .routes(routes!(
            crate::launches::api::watches::get_watch,
            crate::launches::api::watches::update_watch,
            crate::launches::api::watches::delete_watch
        ))
        .routes(routes!(crate::launches::api::watches::set_watch_enabled))
        .routes(routes!(crate::launches::api::watches::list_watch_hits))
        .routes(routes!(crate::launches::api::watches::reset_watch_hit))
        .routes(routes!(crate::launches::api::emissions::emit_run))
        .routes(routes!(
            crate::runs::api::external_runs::put_external_run_session
        ))
        .routes(routes!(crate::daemon::api::overrides::get_config))
        .routes(routes!(crate::daemon::api::overrides::put_config_overrides))
        .routes(routes!(crate::daemon::api::overrides::get_broker_contracts))
        .routes(routes!(crate::daemon::api::overrides::get_playbook_caps))
        .routes(routes!(
            crate::playbooks::api::providers::get_dispatch_providers
        ))
        .routes(routes!(
            crate::playbooks::api::providers::list_providers,
            crate::playbooks::api::providers::register_provider
        ))
        .routes(routes!(
            crate::playbooks::api::providers::update_provider,
            crate::playbooks::api::providers::delete_provider
        ))
        .routes(routes!(
            crate::playbooks::api::providers::put_dispatch_default,
            crate::playbooks::api::providers::delete_dispatch_default
        ))
        .routes(routes!(
            crate::secrets::api::register_secret,
            crate::secrets::api::list_secrets
        ))
        .routes(routes!(
            crate::secrets::api::get_secret,
            crate::secrets::api::delete_secret
        ))
        .routes(routes!(crate::secrets::api::rotate_secret))
        .routes(routes!(crate::secrets::api::transfer_secret))
        .routes(routes!(crate::secrets::api::bind_secret))
        .routes(routes!(crate::secrets::api::unbind_secret))
        .routes(routes!(crate::secrets::api::list_secret_audit))
        .routes(routes!(
            crate::authz::api::list_teams,
            crate::authz::api::create_team
        ))
        .routes(routes!(
            crate::authz::api::get_team,
            crate::authz::api::rename_team,
            crate::authz::api::delete_team
        ))
        .routes(routes!(crate::authz::api::put_members))
        .routes(routes!(crate::authz::api::team_audit))
        .routes(routes!(crate::authz::api::list_actions))
        .routes(routes!(crate::authz::api::get_schema))
        .routes(routes!(
            crate::authz::api::list_policy_sets,
            crate::authz::api::create_policy_set
        ))
        .routes(routes!(crate::authz::api::get_policy_set))
        .routes(routes!(crate::authz::api::activate_policy_set))
        .routes(routes!(crate::authz::transfer::transfer_playbook))
        .routes(routes!(crate::authz::transfer::transfer_playbook_draft))
        .routes(routes!(crate::authz::transfer::transfer_pack_import))
        .routes(routes!(crate::authz::transfer::transfer_provider))
        .routes(routes!(crate::authz::shares::list_playbook_shares))
        .routes(routes!(
            crate::authz::shares::grant_playbook_share,
            crate::authz::shares::revoke_playbook_share
        ))
        .routes(routes!(crate::authz::shares::list_playbook_draft_shares))
        .routes(routes!(
            crate::authz::shares::grant_playbook_draft_share,
            crate::authz::shares::revoke_playbook_draft_share
        ))
        .routes(routes!(crate::authz::shares::list_pack_import_shares))
        .routes(routes!(
            crate::authz::shares::grant_pack_import_share,
            crate::authz::shares::revoke_pack_import_share
        ))
        .routes(routes!(crate::authz::shares::list_provider_shares))
        .routes(routes!(
            crate::authz::shares::grant_provider_share,
            crate::authz::shares::revoke_provider_share
        ));
    #[cfg(feature = "autoresearch")]
    let router = if state.autoresearch {
        autoresearch::routes(router)
    } else {
        router
    };
    let (router, api) = router.split_for_parts();

    router
        // The artifact proxy is a catch-all (`{*path}` captures `diffs/<file>`, which carries a
        // slash), so it's routed manually — the `routes!` macro would derive a single-segment axum
        // route from its OpenAPI `{path}` param. It stays in `ApiDoc::paths()` for the docs.
        .route(
            "/api/runs/{run_id}/artifacts/{*path}",
            get(crate::runs::api::runs::get_artifact),
        )
        // Same catch-all reason: a captured file's key carries the task directory's slash.
        .route(
            "/api/runs/{run_id}/files/{*key}",
            get(crate::runs::api::evidence::get_run_file),
        )
        // Routed manually so the body cap can sit on just these two methods: `DefaultBodyLimit`
        // rejects an oversized PUT with 413 before the JSON is buffered or parsed. Stays in
        // `ApiDoc::paths()` for the docs, like the artifact proxy.
        .route(
            "/api/prefs",
            get(crate::identity::api::prefs::get_prefs)
                .put(crate::identity::api::prefs::put_prefs)
                .layer(axum::extract::DefaultBodyLimit::max(
                    crate::identity::api::prefs::MAX_PREFS_BYTES,
                )),
        )
        .route(
            "/api/prefs/editor",
            get(crate::identity::api::prefs::get_editor_prefs)
                .put(crate::identity::api::prefs::put_editor_prefs)
                .layer(axum::extract::DefaultBodyLimit::max(
                    crate::identity::api::prefs::MAX_PREFS_BYTES,
                )),
        )
        .route(
            "/api/prefs/pickers",
            get(crate::identity::api::prefs::get_picker_prefs)
                .put(crate::identity::api::prefs::put_picker_prefs)
                .layer(axum::extract::DefaultBodyLimit::max(
                    crate::identity::api::prefs::MAX_PREFS_BYTES,
                )),
        )
        .route("/api/openapi.json", get(move || async { Json(api) }))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::authz::bindings::enforce,
        ))
        .layer(axum::middleware::from_fn(trace_requests))
        .with_state(state)
}

/// One server span per API request, named `<METHOD> <route template>` so Datadog groups by route,
/// not by concrete path. `/healthz` is exempt — probe traffic would drown everything else. For a
/// streaming response (SSE, artifact proxy) the span closes at the response head, which is the
/// handler's work; the stream itself is not the handler.
pub(crate) async fn trace_requests(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use tracing::Instrument as _;
    let route = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or_else(|| req.uri().path().to_string(), |p| p.as_str().to_string());
    if route == "/healthz" {
        return next.run(req).await;
    }
    let span = tracing::info_span!(
        "http.request",
        otel.name = format!("{} {}", req.method(), route),
        otel.kind = "server",
        http.method = %req.method(),
        http.route = %route,
        http.status_code = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
    );
    async move {
        let resp = next.run(req).await;
        let span = tracing::Span::current();
        span.record("http.status_code", resp.status().as_u16());
        if resp.status().is_server_error() {
            span.record("otel.status_code", "ERROR");
        }
        resp
    }
    .instrument(span)
    .await
}
