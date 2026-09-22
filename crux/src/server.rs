//! The MCP tool surface, served by the controller at `/mcp` over its own router.
//!
//! Tool descriptions are context the model pays for on every turn, so they are terse and there are
//! not many of them.

#![allow(clippy::disallowed_macros)]

use crate::client::{AdoptBody, Client};
use crate::ops;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use std::sync::Arc;

fn default_body_max() -> usize {
    6000
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IssuesArgs {
    /// Input kind: `github`, `scenario`, or `jira`. Omit for all.
    #[serde(default)]
    pub kind: Option<String>,
    /// Status, one of `new`, `scoped`, `awaiting-approval`, `building`, `running`, `pr-open`,
    /// `parked`, `done`. Omit for all.
    #[serde(default)]
    pub status: Option<String>,
    /// Max rows. Default 50, applied by this server: the controller returns every match.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Return the controller's raw JSON instead of the compact table.
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IssueArgs {
    /// Issue key, e.g. `owner/repo#12` or `scenario:<uuid>`.
    pub key: String,
    /// Cut the issue body at this many chars. Default 6000.
    #[serde(default = "default_body_max")]
    pub body_max: usize,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlaybookRunsArgs {
    /// Status, e.g. `running`, `done`, `parked`. Omit for all.
    #[serde(default)]
    pub status: Option<String>,
    /// Only launches of this playbook id.
    #[serde(default)]
    pub playbook: Option<String>,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunsArgs {
    /// Status, e.g. `running`, `finished`, `incomplete`. Omit for all.
    #[serde(default)]
    pub status: Option<String>,
    /// Repository, `owner/repo`.
    #[serde(default)]
    pub repo: Option<String>,
    /// Only runs dispatched to this cluster target.
    #[serde(default)]
    pub dispatch_target: Option<String>,
    /// Max rows. The controller's own default is 50.
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunArgs {
    /// Run id, as `crucible_runs` lists it.
    pub run_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlaybookIdArgs {
    /// Playbook id, as `crucible_playbooks` lists it.
    pub id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LaunchArgs {
    /// Playbook id, as `crucible_playbooks` lists it.
    pub id: String,
    /// The parameters object its schema accepts. Read `crucible_playbook_schema` first.
    #[serde(default)]
    pub params: serde_json::Value,
    /// Dollar ceiling for this launch. The controller refuses anything above the admin cap.
    pub max_cost: f64,
    /// Wall-clock ceiling, e.g. `30m`, `2h`.
    pub max_time: String,
    /// Cluster to dispatch onto. Omit for the controller's default.
    #[serde(default)]
    pub dispatch_target: Option<String>,
    /// Registered inference provider id the run's agent talks to (see GET /api/config/providers),
    /// replacing the pack manifest's `[agent]` harness. Omit to resolve through the configured
    /// defaults.
    #[serde(default)]
    pub provider: Option<String>,
    /// The model to ask that provider for, replacing the manifest's `[agent].model`. Needs
    /// `provider`; omit for the provider's default.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KeyArgs {
    /// Issue key.
    pub key: String,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ParkArgs {
    pub key: String,
    /// Why. Required, and it lands in the event log against your name.
    pub reason: String,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UnparkArgs {
    pub key: String,
    /// Why it should run again.
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BumpArgs {
    pub key: String,
    /// The new priority. Higher runs sooner.
    pub priority: i64,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JustifiedArgs {
    pub key: String,
    /// Why this is worth the spend. Recorded against your identity.
    pub justification: String,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TurnsArgs {
    /// Only turns for this issue key.
    #[serde(default)]
    pub issue: Option<String>,
    /// Work kind: `grounded-rank`, `scope`, or `run`.
    #[serde(default)]
    pub kind: Option<String>,
    /// Lifecycle state: `queued`, `running`, `succeeded`, `failed`, `collected`, `swept`.
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TurnArgs {
    /// The turn's pod name.
    pub pod: String,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunLogArgs {
    pub run_id: String,
    /// Line to resume from, as a previous window's `cursor=` line reported it. Omit to start at
    /// the first line the controller served.
    #[serde(default)]
    pub cursor: Option<usize>,
    /// Lines in this window. Capped at 200, which is also the default.
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GraphArgs {
    pub run_id: String,
    /// Emit a mermaid flowchart instead of the ASCII table.
    #[serde(default)]
    pub mermaid: bool,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WatchArgs {
    /// Watch id, as `crucible_watches` lists it.
    pub id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JsonOnlyArgs {
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AdoptArgs {
    /// One-line title.
    pub title: String,
    /// The full ask, as prose. This is what the scoping turn reads.
    pub body: String,
    /// Repos this touches. The first is the clone target.
    pub affected_repos: Vec<String>,
    /// Why the controller should spend money on this.
    pub justification: String,
    /// Whether the body is authoritative (skips grounded ranking).
    #[serde(default)]
    pub authoritative: bool,
    /// Branch or tag to clone. Omit for the repo's default branch.
    #[serde(default)]
    pub git_ref: Option<String>,
    /// A configured broker contract name (see `crucible_contracts`). Omit for local measure.
    #[serde(default)]
    pub codegen_contract: Option<String>,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ImportArgs {
    /// `owner/repo` or a clone URL.
    pub repo: String,
    /// Branch or tag to fetch. Omit for the repo's default branch.
    #[serde(default)]
    pub git_ref: Option<String>,
    /// The pack directory inside the repo. Omit for the repo root.
    #[serde(default)]
    pub path: Option<String>,
    /// The registry id you propose registering this pack under. Prefills the review page.
    #[serde(default)]
    pub id: Option<String>,
    /// The description you propose. Prefills the review page.
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DraftFilesArgs {
    pub draft_id: String,
    /// Which save to read. Omit for the newest — which is what a save must be based on.
    #[serde(default)]
    pub version: Option<i64>,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DraftPullArgs {
    pub draft_id: String,
    /// The local directory to write the pack into. Created if it does not exist.
    pub dir: String,
    /// Which save to pull. Omit for the newest — which is what a save must be based on.
    #[serde(default)]
    pub version: Option<i64>,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DraftPushArgs {
    pub draft_id: String,
    /// The local directory holding the pack. Its whole tree is the save.
    pub dir: String,
    /// The version this directory was pulled at, from crucible_draft_pull. A save whose base is
    /// no longer the newest is refused, not merged.
    pub base_version: i64,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SecretBindArgs {
    /// The secret's id, as crucible_secrets lists it.
    pub secret_id: String,
    /// `repo`, `playbook`, or `domain`. A draft is a `playbook` scope under its draft id.
    pub scope_kind: String,
    pub scope_id: String,
    /// `env` or `file`.
    pub projection_kind: String,
    /// The environment variable name, or the absolute file path.
    pub projection: String,
    /// The manifest's `[[secret]] name` this binding satisfies. Omit for the secret's own name.
    #[serde(default)]
    pub declared_name: Option<String>,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DraftIdArgs {
    pub draft_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DraftPreviewArgs {
    pub draft_id: String,
    /// Which save to compile. Omit for the newest — which is what a launch runs.
    #[serde(default)]
    pub version: Option<i64>,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DraftLaunchArgs {
    pub draft_id: String,
    /// The form values the newest save's schema accepts, as strings. Read the schema digest and
    /// the diagnostics from crucible_draft_preview first.
    #[serde(default)]
    pub params: std::collections::BTreeMap<String, String>,
    /// Dollar ceiling for this launch. The controller refuses anything above the admin cap.
    pub max_cost: f64,
    /// Wall-clock ceiling, e.g. `30m`, `2h`.
    pub max_time: String,
    /// The schema digest these values were filled against, from crucible_draft_preview. A save
    /// that landed since is refused rather than launched against a schema that moved.
    #[serde(default)]
    pub schema_digest: Option<String>,
    /// Cluster to dispatch onto. Omit for the controller's default.
    #[serde(default)]
    pub dispatch_target: Option<String>,
    /// Registered inference provider id the run's agent talks to (see GET /api/config/providers),
    /// replacing the pack manifest's `[agent]` harness. Omit to resolve through the configured
    /// defaults.
    #[serde(default)]
    pub provider: Option<String>,
    /// The model to ask that provider for, replacing the manifest's `[agent].model`. Needs
    /// `provider`; omit for the provider's default.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DraftGraduateArgs {
    pub draft_id: String,
    /// `owner/repo` the export PR opens against.
    pub repo: String,
    /// The pack directory inside that repo. Omit for the repo root.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DraftCreateArgs {
    /// Lowercase slug; the studio page and every launch of the draft are keyed by it.
    pub draft_id: String,
    pub description: String,
    /// A registered playbook id to seed version 1 from; omitted opens on a minimal skeleton.
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DraftSaveArgs {
    pub draft_id: String,
    /// The version these edits were made against, from crucible_draft_files. A save whose base is
    /// no longer the newest is refused, not merged.
    pub base_version: i64,
    /// The whole pack, `{path: content}`. A path missing from the map is deleted from the pack.
    pub files: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub json: bool,
}

/// One shared client; every tool is a call plus a render.
#[derive(Clone)]
pub struct CrucibleMcp {
    client: Arc<Client>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl CrucibleMcp {
    pub fn new(client: Arc<Client>) -> Self {
        Self {
            client,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "List controller issues as a compact table: KEY STATUS TIER REF CONTRACT \
        UPSTREAM TITLE. Scenario issues have no upstream. Filter by kind/status; newest first, \
        capped at `limit` rows (default 50)."
    )]
    async fn crucible_issues(&self, Parameters(a): Parameters<IssuesArgs>) -> String {
        flatten(
            ops::issues(
                &self.client,
                a.kind.as_deref(),
                a.status.as_deref(),
                a.limit,
                a.json,
            )
            .await,
        )
    }

    #[tool(
        description = "One issue in full: provenance, the whole park reason, every scope with its \
        approval PR and pack digest, each run and candidate, and the state-transition trail."
    )]
    async fn crucible_issue(&self, Parameters(a): Parameters<IssueArgs>) -> String {
        flatten(ops::issue(&self.client, &a.key, a.body_max, a.json).await)
    }

    #[tool(
        description = "Adopt a scenario: create a controller issue with no upstream. Returns the \
        new key. A 422 names the field that failed, verbatim."
    )]
    async fn crucible_adopt(&self, Parameters(a): Parameters<AdoptArgs>) -> String {
        flatten(
            ops::adopt(
                &self.client,
                &AdoptBody {
                    title: a.title,
                    body: a.body,
                    affected_repos: a.affected_repos,
                    justification: a.justification,
                    authoritative: a.authoritative,
                    git_ref: a.git_ref,
                    codegen_contract: a.codegen_contract,
                },
                a.json,
            )
            .await,
        )
    }

    #[tool(description = "Park an issue: stop working it, and record why.")]
    async fn crucible_park(&self, Parameters(a): Parameters<ParkArgs>) -> String {
        flatten(ops::park(&self.client, &a.key, &a.reason, a.json).await)
    }

    #[tool(description = "Unpark an issue so the controller picks it up again.")]
    async fn crucible_unpark(&self, Parameters(a): Parameters<UnparkArgs>) -> String {
        flatten(ops::unpark(&self.client, &a.key, a.reason.as_deref(), a.json).await)
    }

    #[tool(description = "Change an issue's priority. Higher runs sooner.")]
    async fn crucible_bump(&self, Parameters(a): Parameters<BumpArgs>) -> String {
        flatten(
            ops::bump(
                &self.client,
                &a.key,
                a.priority,
                a.reason.as_deref(),
                a.json,
            )
            .await,
        )
    }

    #[tool(
        description = "Wake the controller's reconcile loop. Takes no issue key (it is a full \
        pass), returns before the pass runs, and repeated calls coalesce."
    )]
    async fn crucible_reconcile(&self, Parameters(a): Parameters<JsonOnlyArgs>) -> String {
        flatten(ops::reconcile(&self.client, a.json).await)
    }

    #[tool(description = "Re-dispatch an issue's run. Costs money; the justification is recorded.")]
    async fn crucible_redispatch(&self, Parameters(a): Parameters<JustifiedArgs>) -> String {
        flatten(ops::redispatch(&self.client, &a.key, &a.justification, a.json).await)
    }

    #[tool(
        description = "List work-pod turns: POD KIND STATE ISSUE CREATED. Filter by issue, kind, \
        or state. Use crucible_turn for one turn's full failure chain."
    )]
    async fn crucible_turns(&self, Parameters(a): Parameters<TurnsArgs>) -> String {
        flatten(
            ops::turns(
                &self.client,
                a.issue.as_deref(),
                a.kind.as_deref(),
                a.state.as_deref(),
                a.json,
            )
            .await,
        )
    }

    #[tool(description = "One turn with its result and error text intact.")]
    async fn crucible_turn(&self, Parameters(a): Parameters<TurnArgs>) -> String {
        flatten(ops::turn(&self.client, &a.pod, a.json).await)
    }

    #[tool(
        description = "One window of a run's engine output, resumable by cursor. A running pod is \
        read live; once it finishes its ingested session is served, which is the only copy that \
        outlives the pod. Returns at most 200 lines and reports the cursor to resume from, so \
        read it in windows rather than expecting the whole log."
    )]
    async fn crucible_run_log(&self, Parameters(a): Parameters<RunLogArgs>) -> String {
        flatten(
            ops::run_log(
                &self.client,
                &a.run_id,
                a.cursor.unwrap_or(0),
                a.limit,
                a.json,
            )
            .await,
        )
    }

    #[tool(
        description = "A run's task graph in dependency order, each task with its LATEST status, \
        cost, duration and note. Set mermaid=true for a flowchart."
    )]
    async fn crucible_graph(&self, Parameters(a): Parameters<GraphArgs>) -> String {
        flatten(ops::graph(&self.client, &a.run_id, a.mermaid, a.json).await)
    }

    #[tool(
        description = "The broker contract names this controller accepts — the only legal values \
        for crucible_adopt's codegen_contract."
    )]
    async fn crucible_contracts(&self, Parameters(a): Parameters<JsonOnlyArgs>) -> String {
        flatten(ops::contracts(&self.client, a.json).await)
    }

    #[tool(
        description = "Approve a scenario or jira issue's scope pack, opening the approval to a run. \
        GitHub issues approve on their draft PR instead; use crucible_approval for that URL."
    )]
    async fn crucible_approve(&self, Parameters(a): Parameters<KeyArgs>) -> String {
        flatten(ops::approve(&self.client, &a.key, a.json).await)
    }

    #[tool(
        description = "One issue's approval gate: the PR URL to act on and the pack digest under \
        it, plus whether anyone has approved."
    )]
    async fn crucible_approval(&self, Parameters(a): Parameters<KeyArgs>) -> String {
        flatten(ops::approval_detail(&self.client, &a.key, a.json).await)
    }

    #[tool(description = "Every scope currently awaiting approval, plus the PRs already kept.")]
    async fn crucible_approvals(&self, Parameters(a): Parameters<JsonOnlyArgs>) -> String {
        flatten(ops::approvals(&self.client, a.json).await)
    }

    #[tool(
        description = "Propose a playbook pack import: fetch it at a ref, compile it server-side, \
        and store the preview. Returns the import id, the pinned rev, the schema digest, the \
        engine's diagnostics and a preview URL to hand a human. An admin registers it from there."
    )]
    async fn crucible_playbook_import(&self, Parameters(a): Parameters<ImportArgs>) -> String {
        flatten(
            ops::playbook_import(
                &self.client,
                &a.repo,
                a.git_ref.as_deref(),
                a.path.as_deref(),
                a.id.as_deref(),
                a.description.as_deref(),
                a.json,
            )
            .await,
        )
    }

    #[tool(
        description = "List the secrets the caller owns: id, name, kind, visibility, mode, owner. \
        Never a value. The id is what crucible_secret_bind takes."
    )]
    async fn crucible_secrets(&self, Parameters(a): Parameters<JsonOnlyArgs>) -> String {
        flatten(ops::secrets(&self.client, a.json).await)
    }

    #[tool(
        description = "Bind a secret to a scope so a run there receives it. A draft's launch is \
        refused until every `[[secret]]` its manifest declares is bound at scope_kind `playbook`, \
        scope_id = the draft id, by someone who owns the secret. projection is the env var name \
        or the absolute file path the value arrives as."
    )]
    async fn crucible_secret_bind(&self, Parameters(a): Parameters<SecretBindArgs>) -> String {
        flatten(
            ops::secret_bind(
                &self.client,
                ops::SecretBind {
                    secret_id: &a.secret_id,
                    scope_kind: &a.scope_kind,
                    scope_id: &a.scope_id,
                    projection_kind: &a.projection_kind,
                    projection: &a.projection,
                    declared_name: a.declared_name.as_deref(),
                },
                a.json,
            )
            .await,
        )
    }

    #[tool(
        description = "Create a draft pack in the authoring studio, seeded from a registered \
        playbook or a minimal skeleton. Returns version 1, its diagnostics, and the studio link; \
        author with crucible_draft_pull and crucible_draft_push from there, test-fire it with \
        crucible_draft_launch, and promote it with crucible_draft_graduate."
    )]
    async fn crucible_draft_create(&self, Parameters(a): Parameters<DraftCreateArgs>) -> String {
        flatten(
            ops::draft_create(
                &self.client,
                &a.draft_id,
                &a.description,
                a.template.as_deref(),
                a.json,
            )
            .await,
        )
    }

    #[tool(
        description = "Write a draft pack's files at one save into a local directory, so you can \
        read, edit and run them with ordinary tools. Returns the version it wrote, which is the \
        base_version crucible_draft_push needs. Prefer this to crucible_draft_files for anything \
        beyond a glance: the files may hold edits a human made in the studio."
    )]
    async fn crucible_draft_pull(&self, Parameters(a): Parameters<DraftPullArgs>) -> String {
        flatten(
            ops::draft_pull(
                &self.client,
                &a.draft_id,
                std::path::Path::new(&a.dir),
                a.version,
                a.json,
            )
            .await,
        )
    }

    #[tool(
        description = "Save a local directory back as the next draft version. The directory IS \
        the pack: its whole tree is the save, so a file you deleted on disk is deleted from the \
        pack. Returns the new version, its diagnostics and the studio link. If another editor \
        saved since base_version, nothing is written and the refusal names the version to re-pull \
        and merge onto."
    )]
    async fn crucible_draft_push(&self, Parameters(a): Parameters<DraftPushArgs>) -> String {
        flatten(
            ops::draft_push(
                &self.client,
                &a.draft_id,
                std::path::Path::new(&a.dir),
                a.base_version,
                a.json,
            )
            .await,
        )
    }

    #[tool(
        description = "Read a draft pack's files at one save inline: the whole `{path: content}` \
        map, the version, who saved it, and its diagnostics. Use crucible_draft_pull instead for \
        anything bigger than a one-file look — it puts the same files on disk. The version this \
        returns is the base_version a save needs."
    )]
    async fn crucible_draft_files(&self, Parameters(a): Parameters<DraftFilesArgs>) -> String {
        flatten(ops::draft_files(&self.client, &a.draft_id, a.version, a.json).await)
    }

    #[tool(
        description = "Save a draft pack from an inline file map: the whole map, compiled on \
        save, appended as the next version. A path missing from the map is deleted from the pack, \
        so every file has to be resent — use crucible_draft_push instead for anything bigger than \
        a one-file touch. If another editor saved since base_version, nothing is written and the \
        refusal names the version to re-read and merge onto."
    )]
    async fn crucible_draft_save(&self, Parameters(a): Parameters<DraftSaveArgs>) -> String {
        flatten(ops::draft_save(&self.client, &a.draft_id, a.base_version, &a.files, a.json).await)
    }

    #[tool(
        description = "What a draft version compiled to, without launching it: the schema digest \
        its params are validated against and the engine's diagnostics. Read this before \
        crucible_draft_launch — a version with no schema digest never compiled and cannot be \
        launched."
    )]
    async fn crucible_draft_preview(&self, Parameters(a): Parameters<DraftPreviewArgs>) -> String {
        flatten(ops::draft_preview(&self.client, &a.draft_id, a.version, a.json).await)
    }

    #[tool(
        description = "Test-fire a draft pack. Runs the newest save, validated against its stored \
        schema and held to the same admin cost ceiling a registered launch is. Costs money \
        against max_cost. Returns the issue key to watch it by with crucible_playbook_run. Pass \
        schema_digest to be refused rather than launched if a save landed since you read it. \
        Pass provider (and optionally model) to run the pack's agent tasks on a registered \
        inference provider instead of the crucible.toml [agent] harness and model."
    )]
    async fn crucible_draft_launch(&self, Parameters(a): Parameters<DraftLaunchArgs>) -> String {
        flatten(
            ops::draft_launch(
                &self.client,
                &a.draft_id,
                ops::DraftLaunch {
                    params: a.params,
                    max_cost: a.max_cost,
                    max_time: a.max_time,
                    schema_digest: a.schema_digest,
                    dispatch_target: a.dispatch_target,
                    provider: a.provider,
                    model: a.model,
                },
                a.json,
            )
            .await,
        )
    }

    #[tool(
        description = "Delete a draft and every version of it. Admin only. Gone is gone; the id \
        is free for crucible_draft_create again."
    )]
    async fn crucible_draft_delete(&self, Parameters(a): Parameters<DraftIdArgs>) -> String {
        flatten(ops::draft_delete(&self.client, &a.draft_id).await)
    }

    #[tool(
        description = "Graduate a draft to a registered playbook: push its newest compiling \
        version as a branch under repo/path and open the export PR. Returns the PR URL. The draft \
        retires itself once that merged pack is imported. A draft already graduated is refused \
        with the open PR in the refusal."
    )]
    async fn crucible_draft_graduate(
        &self,
        Parameters(a): Parameters<DraftGraduateArgs>,
    ) -> String {
        flatten(
            ops::draft_graduate(
                &self.client,
                &a.draft_id,
                &a.repo,
                a.path.as_deref(),
                a.json,
            )
            .await,
        )
    }

    #[tool(
        description = "List registered playbooks: ID REPO REV BY DESCRIPTION. These are what \
        crucible_launch can start."
    )]
    async fn crucible_playbooks(&self, Parameters(a): Parameters<JsonOnlyArgs>) -> String {
        flatten(ops::playbooks(&self.client, a.json).await)
    }

    #[tool(
        description = "The admin caps a launch's max_cost and max_time are bounded by. Read \
        before crucible_launch or crucible_draft_launch: asking for more than either is a 422."
    )]
    async fn crucible_playbook_caps(&self, Parameters(a): Parameters<JsonOnlyArgs>) -> String {
        flatten(ops::playbook_caps(&self.client, a.json).await)
    }

    #[tool(
        description = "The JSON Schema a playbook's params are validated against. Read this \
        before crucible_launch; guessing field names is how a launch 422s."
    )]
    async fn crucible_playbook_schema(&self, Parameters(a): Parameters<PlaybookIdArgs>) -> String {
        flatten(ops::playbook_schema(&self.client, &a.id).await)
    }

    #[tool(
        description = "Launch a playbook. Costs money against max_cost, which the controller caps \
        at the admin ceiling. Returns the issue key to watch it by with crucible_playbook_run. \
        Pass provider (and optionally model) to run the pack's agent tasks on a registered \
        inference provider instead of the crucible.toml [agent] harness and model."
    )]
    async fn crucible_launch(&self, Parameters(a): Parameters<LaunchArgs>) -> String {
        flatten(
            ops::launch(
                &self.client,
                &a.id,
                ops::Launch {
                    params: a.params,
                    max_cost: a.max_cost,
                    max_time: a.max_time,
                    dispatch_target: a.dispatch_target,
                    provider: a.provider,
                    model: a.model,
                },
                a.json,
            )
            .await,
        )
    }

    #[tool(
        description = "Playbook launches as a table: KEY PLAYBOOK STATUS N COST/CAP AGENT BY \
        WHY. AGENT is the provider and model the launch pinned, or a dash for the platform \
        default. WHY names what stopped one — a secrets refusal, a park, or a drifted schema."
    )]
    async fn crucible_playbook_runs(&self, Parameters(a): Parameters<PlaybookRunsArgs>) -> String {
        flatten(
            ops::playbook_runs(
                &self.client,
                a.status.as_deref(),
                a.playbook.as_deref(),
                a.json,
            )
            .await,
        )
    }

    #[tool(description = "One playbook launch whole: its params, caps, and any refusal, as JSON.")]
    async fn crucible_playbook_run(&self, Parameters(a): Parameters<KeyArgs>) -> String {
        flatten(ops::playbook_run(&self.client, &a.key).await)
    }

    #[tool(
        description = "The runs leaderboard: RUN STATUS REPO ISSUE BEST COST CLUSTER CREATED. \
        Filter by status, repo, or the cluster it was dispatched to."
    )]
    async fn crucible_runs(&self, Parameters(a): Parameters<RunsArgs>) -> String {
        flatten(
            ops::runs(
                &self.client,
                a.status.as_deref(),
                a.repo.as_deref(),
                a.dispatch_target.as_deref(),
                a.limit,
                a.json,
            )
            .await,
        )
    }

    #[tool(description = "One run with its candidates, as JSON.")]
    async fn crucible_run(&self, Parameters(a): Parameters<RunArgs>) -> String {
        flatten(ops::run(&self.client, &a.run_id).await)
    }

    #[tool(
        description = "Recurrences: ID PLAYBOOK CRON STATE NEXT LAST FAILS OWNER. STATE is \
        `blocked` when the owner must sign in again — enabled, and still will not fire."
    )]
    async fn crucible_schedules(&self, Parameters(a): Parameters<JsonOnlyArgs>) -> String {
        flatten(ops::schedules(&self.client, a.json).await)
    }

    #[tool(
        description = "Tracker watches: ID PLAYBOOK TRACKER QUERY STATE SWEPT LAUNCHED FAILS OWNER. \
        A watch launches its playbook once per item its query matches, never again for the same \
        item. STATE is `blocked` when the owner must sign in again."
    )]
    async fn crucible_watches(&self, Parameters(a): Parameters<JsonOnlyArgs>) -> String {
        flatten(ops::watches(&self.client, a.json).await)
    }

    #[tool(description = "One tracker watch with its full authorization, as JSON.")]
    async fn crucible_watch(&self, Parameters(a): Parameters<WatchArgs>) -> String {
        flatten(ops::watch(&self.client, &a.id).await)
    }

    #[tool(
        description = "The endpoint and credential this session actually uses, and who the \
        controller says that makes you. Run this first when a mutation 403s."
    )]
    async fn crucible_whoami(&self) -> String {
        flatten(ops::whoami(&self.client).await)
    }
}

/// An error becomes a line the model can read and act on, rather than a protocol fault. The CLI
/// does the opposite with the same `Result`: it exits non-zero.
fn flatten(r: anyhow::Result<String>) -> String {
    r.unwrap_or_else(|e| format!("error: {e:#}"))
}

// Route off the stored router rather than the macro's default `Self::tool_router()`, which rebuilds
// the whole table on every single request.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for CrucibleMcp {
    /// Advertise the `tools` capability during `initialize`. Without it a spec-compliant client
    /// connects, never calls `tools/list`, and the server looks connected while exposing nothing.
    fn get_info(&self) -> ServerInfo {
        // ServerInfo is #[non_exhaustive], so it has to be built by mutation rather than a struct
        // literal.
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
            "The crucible controller. Read tools return compact text; pass json=true for raw \
             payloads. Mutations echo the identity the controller recorded them against."
                .into(),
        );
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_of(name: &str) -> (String, String) {
        let tools = CrucibleMcp::tool_router().list_all();
        let tool = tools
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("{name} missing"));
        (
            serde_json::to_string(&tool.input_schema).expect("schema"),
            tool.description.clone().unwrap_or_default().to_string(),
        )
    }

    /// The context cost of this server is paid on every turn, whether or not a tool is called.
    /// Growth should be a decision, not a drift.
    #[test]
    fn the_tool_surface_stays_small() {
        let names: Vec<String> = CrucibleMcp::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(names.len(), 40, "tools: {names:?}");
        for required in [
            "crucible_issues",
            "crucible_draft_delete",
            "crucible_playbook_caps",
            "crucible_secrets",
            "crucible_secret_bind",
            "crucible_issue",
            "crucible_adopt",
            "crucible_park",
            "crucible_unpark",
            "crucible_bump",
            "crucible_reconcile",
            "crucible_redispatch",
            "crucible_turns",
            "crucible_turn",
            "crucible_graph",
            "crucible_run_log",
            "crucible_watches",
            "crucible_watch",
            "crucible_contracts",
            "crucible_approve",
            "crucible_approval",
            "crucible_playbook_import",
            "crucible_draft_pull",
            "crucible_draft_push",
            "crucible_draft_files",
            "crucible_draft_save",
            "crucible_draft_preview",
            "crucible_draft_launch",
            "crucible_draft_graduate",
            "crucible_playbooks",
            "crucible_playbook_schema",
            "crucible_launch",
            "crucible_playbook_runs",
            "crucible_playbook_run",
            "crucible_runs",
            "crucible_run",
            "crucible_schedules",
            "crucible_whoami",
        ] {
            assert!(
                names.contains(&required.to_string()),
                "{required} missing: {names:?}"
            );
        }
    }

    /// The tool description is the only schema the calling model sees, so the status vocabulary in
    /// it has to be the controller's, hyphens included.
    #[test]
    fn the_issue_status_description_names_the_statuses_the_controller_accepts() {
        let tools = CrucibleMcp::tool_router().list_all();
        let issues = tools
            .iter()
            .find(|t| t.name == "crucible_issues")
            .expect("crucible_issues");
        let schema = serde_json::to_string(&issues.input_schema).expect("schema");
        for accepted in crate::dto::IssueStatus::ALL {
            assert!(
                schema.contains(accepted.wire()),
                "{accepted} missing: {schema}"
            );
        }
        for invented in ["scoping", "awaiting_approval"] {
            assert!(
                !schema.contains(invented),
                "{invented} still advertised: {schema}"
            );
        }
        assert!(schema.contains("limit"), "{schema}");
    }

    /// A model that reads only the schema has to learn two things from it: a push carries the base
    /// the pull printed, and the directory is the whole pack, so a file it deleted is a file the
    /// save deletes.
    #[test]
    fn the_directory_tools_advertise_their_base_and_their_whole_map_semantics() {
        let (pull_schema, pull_description) = tool_of("crucible_draft_pull");
        assert!(pull_schema.contains("dir"), "{pull_schema}");
        assert!(pull_schema.contains("version"), "{pull_schema}");
        assert!(
            pull_description.contains("base_version"),
            "{pull_description}"
        );

        let (push_schema, push_description) = tool_of("crucible_draft_push");
        assert!(push_schema.contains("dir"), "{push_schema}");
        assert!(push_schema.contains("base_version"), "{push_schema}");
        assert!(push_description.contains("deleted"), "{push_description}");

        for inline in ["crucible_draft_files", "crucible_draft_save"] {
            let (_, description) = tool_of(inline);
            assert!(
                description.contains("crucible_draft_pull")
                    || description.contains("crucible_draft_push"),
                "{inline} does not point at the directory tools: {description}"
            );
        }
    }

    /// The authoring loop only closes if a model can find the next step from the tool it just
    /// called: create points at launch and graduate, and a launch carries the digest the preview
    /// printed so a save that landed since refuses it.
    #[test]
    fn the_draft_lifecycle_tools_point_at_each_other_and_carry_the_digest() {
        let (_, create) = tool_of("crucible_draft_create");
        assert!(create.contains("crucible_draft_launch"), "{create}");
        assert!(create.contains("crucible_draft_graduate"), "{create}");

        let (launch_schema, launch) = tool_of("crucible_draft_launch");
        assert!(launch_schema.contains("schema_digest"), "{launch_schema}");
        assert!(launch_schema.contains("max_cost"), "{launch_schema}");
        assert!(launch_schema.contains("max_time"), "{launch_schema}");
        assert!(launch_schema.contains("provider"), "{launch_schema}");
        assert!(launch_schema.contains("model"), "{launch_schema}");
        assert!(launch.contains("crucible_playbook_run"), "{launch}");

        // A registered launch pins the pair the same way a test-fire does.
        let (registered_schema, registered) = tool_of("crucible_launch");
        assert!(
            registered_schema.contains("provider"),
            "{registered_schema}"
        );
        assert!(registered_schema.contains("model"), "{registered_schema}");
        assert!(registered.contains("provider"), "{registered}");
        assert!(registered.contains("crucible_playbook_run"), "{registered}");

        let (preview_schema, preview) = tool_of("crucible_draft_preview");
        assert!(preview_schema.contains("version"), "{preview_schema}");
        assert!(preview.contains("crucible_draft_launch"), "{preview}");

        let (graduate_schema, _) = tool_of("crucible_draft_graduate");
        assert!(graduate_schema.contains("repo"), "{graduate_schema}");
        assert!(graduate_schema.contains("path"), "{graduate_schema}");
    }

    /// An error must arrive as readable text, not as an MCP fault the model can't see into.
    #[test]
    fn errors_flatten_into_a_readable_line() {
        let out = flatten(Err(anyhow::anyhow!(
            "HTTP 422 Unprocessable Entity: {{\"error\":\"x\"}}"
        )));
        assert!(out.starts_with("error: HTTP 422"), "{out}");
    }
}
