//! The MCP tool surface, served by the controller at `/mcp` over its own router.
//!
//! Tool descriptions are context the model pays for on every turn, so they are terse and there are
//! not many of them. Each tool decodes one [`crate::ops`] operation and runs it.

#![allow(clippy::disallowed_macros)]

use crate::client::Client;
use crate::ops::blueprints::{BlueprintUse, Blueprints};
use crate::ops::drafts::{
    DraftCreate, DraftDelete, DraftFiles, DraftGraduate, DraftLaunch, DraftPreview, DraftPublish,
    DraftPull, DraftPush, DraftSaveFiles,
};
use crate::ops::issues::{
    Adopt, ApprovalDetail, Approvals, Approve, Bump, Contracts, Issue, Issues, Park, Reconcile,
    Redispatch, Turn, Turns, Unpark,
};
use crate::ops::launches::{Schedules, Watch, Watches, Webhook, WebhookDeliveries, Webhooks};
use crate::ops::playbooks::{
    Launch, PlaybookCaps, PlaybookImport, PlaybookRun, PlaybookRuns, PlaybookSchema, Playbooks,
};
use crate::ops::runs::{Graph, Run, RunLog, Runs};
use crate::ops::secrets::{SecretBind, Secrets};
use crate::ops::{Operation, Whoami};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use std::sync::Arc;

/// One shared client; every tool is one operation run against it.
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

    /// Every tool as `tools/list` returns it.
    pub fn tools() -> Vec<rmcp::model::Tool> {
        Self::tool_router().list_all()
    }

    /// An error becomes a line the model can read and act on, rather than a protocol fault. The
    /// CLI does the opposite with the same `Result`: it exits non-zero.
    async fn run<O: Operation>(&self, op: &O) -> String {
        op.run(&self.client)
            .await
            .unwrap_or_else(|e| format!("error: {e:#}"))
    }

    #[tool(
        description = "List controller issues as a compact table: KEY STATUS TIER REF CONTRACT \
        UPSTREAM TITLE. Scenario issues have no upstream. Filter by kind/status; newest first, \
        capped at `limit` rows (default 50)."
    )]
    async fn crucible_issues(&self, Parameters(a): Parameters<Issues>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "One issue in full: provenance, the whole park reason, every scope with its \
        approval PR and pack digest, each run and candidate, and the state-transition trail."
    )]
    async fn crucible_issue(&self, Parameters(a): Parameters<Issue>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "Adopt a scenario: create a controller issue with no upstream. Returns the \
        new key. A 422 names the field that failed, verbatim."
    )]
    async fn crucible_adopt(&self, Parameters(a): Parameters<Adopt>) -> String {
        self.run(&a).await
    }

    #[tool(description = "Park an issue: stop working it, and record why.")]
    async fn crucible_park(&self, Parameters(a): Parameters<Park>) -> String {
        self.run(&a).await
    }

    #[tool(description = "Unpark an issue so the controller picks it up again.")]
    async fn crucible_unpark(&self, Parameters(a): Parameters<Unpark>) -> String {
        self.run(&a).await
    }

    #[tool(description = "Change an issue's priority. Higher runs sooner.")]
    async fn crucible_bump(&self, Parameters(a): Parameters<Bump>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "Wake the controller's reconcile loop. Takes no issue key (it is a full \
        pass), returns before the pass runs, and repeated calls coalesce."
    )]
    async fn crucible_reconcile(&self, Parameters(a): Parameters<Reconcile>) -> String {
        self.run(&a).await
    }

    #[tool(description = "Re-dispatch an issue's run. Costs money; the justification is recorded.")]
    async fn crucible_redispatch(&self, Parameters(a): Parameters<Redispatch>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "List work-pod turns: POD KIND STATE ISSUE CREATED. Filter by issue, kind, \
        or state. Use crucible_turn for one turn's full failure chain."
    )]
    async fn crucible_turns(&self, Parameters(a): Parameters<Turns>) -> String {
        self.run(&a).await
    }

    #[tool(description = "One turn with its result and error text intact.")]
    async fn crucible_turn(&self, Parameters(a): Parameters<Turn>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "One window of a run's engine output, resumable by cursor. A running pod is \
        read live; once it finishes its ingested session is served, which is the only copy that \
        outlives the pod. Returns at most 200 lines and reports the cursor to resume from, so \
        read it in windows rather than expecting the whole log."
    )]
    async fn crucible_run_log(&self, Parameters(a): Parameters<RunLog>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "A run's task graph in dependency order, each task with its LATEST status, \
        cost, duration and note. Set mermaid=true for a flowchart."
    )]
    async fn crucible_graph(&self, Parameters(a): Parameters<Graph>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "The broker contract names this controller accepts — the only legal values \
        for crucible_adopt's codegen_contract."
    )]
    async fn crucible_contracts(&self, Parameters(a): Parameters<Contracts>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "Approve a scenario or jira issue's scope pack, opening the approval to a run. \
        GitHub issues approve on their draft PR instead; use crucible_approval for that URL."
    )]
    async fn crucible_approve(&self, Parameters(a): Parameters<Approve>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "One issue's approval gate: the PR URL to act on and the pack digest under \
        it, plus whether anyone has approved."
    )]
    async fn crucible_approval(&self, Parameters(a): Parameters<ApprovalDetail>) -> String {
        self.run(&a).await
    }

    #[tool(description = "Every scope currently awaiting approval, plus the PRs already kept.")]
    async fn crucible_approvals(&self, Parameters(a): Parameters<Approvals>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "Propose a playbook pack import: fetch it at a ref, compile it server-side, \
        and store the preview. Returns the import id, the pinned rev, the schema digest, the \
        engine's diagnostics and a preview URL to hand a human. An admin registers it from there."
    )]
    async fn crucible_playbook_import(&self, Parameters(a): Parameters<PlaybookImport>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "List the secrets the caller owns: id, name, kind, visibility, mode, owner. \
        Never a value. The id is what crucible_secret_bind takes."
    )]
    async fn crucible_secrets(&self, Parameters(a): Parameters<Secrets>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "Bind a secret to a scope so a run there receives it. A draft's launch is \
        refused until every `[[secret]]` its manifest declares is bound at scope_kind `playbook`, \
        scope_id = the draft id, by someone who owns the secret. projection is the env var name \
        or the absolute file path the value arrives as."
    )]
    async fn crucible_secret_bind(&self, Parameters(a): Parameters<SecretBind>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "Create a draft pack in the authoring studio, seeded from a registered \
        playbook or a minimal skeleton. Returns version 1, its diagnostics, and the studio link; \
        author with crucible_draft_pull and crucible_draft_push from there, test-fire it with \
        crucible_draft_launch, and promote it with crucible_draft_graduate."
    )]
    async fn crucible_draft_create(&self, Parameters(a): Parameters<DraftCreate>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "Write a draft pack's files at one save into a local directory, so you can \
        read, edit and run them with ordinary tools. Returns the version it wrote, which is the \
        base_version crucible_draft_push needs. Prefer this to crucible_draft_files for anything \
        beyond a glance: the files may hold edits a human made in the studio."
    )]
    async fn crucible_draft_pull(&self, Parameters(a): Parameters<DraftPull>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "Save a local directory back as the next draft version. The directory IS \
        the pack: its whole tree is the save, so a file you deleted on disk is deleted from the \
        pack. Returns the new version, its diagnostics and the studio link. If another editor \
        saved since base_version, nothing is written and the refusal names the version to re-pull \
        and merge onto."
    )]
    async fn crucible_draft_push(&self, Parameters(a): Parameters<DraftPush>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "Read a draft pack's files at one save inline: the whole `{path: content}` \
        map, the version, who saved it, and its diagnostics. Use crucible_draft_pull instead for \
        anything bigger than a one-file look — it puts the same files on disk. The version this \
        returns is the base_version a save needs."
    )]
    async fn crucible_draft_files(&self, Parameters(a): Parameters<DraftFiles>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "Save a draft pack from an inline file map: the whole map, compiled on \
        save, appended as the next version. A path missing from the map is deleted from the pack, \
        so every file has to be resent — use crucible_draft_push instead for anything bigger than \
        a one-file touch. If another editor saved since base_version, nothing is written and the \
        refusal names the version to re-read and merge onto."
    )]
    async fn crucible_draft_save(&self, Parameters(a): Parameters<DraftSaveFiles>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "What a draft version compiled to, without launching it: the schema digest \
        its params are validated against and the engine's diagnostics. Read this before \
        crucible_draft_launch — a version with no schema digest never compiled and cannot be \
        launched."
    )]
    async fn crucible_draft_preview(&self, Parameters(a): Parameters<DraftPreview>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "Test-fire a draft pack. Runs the newest save, validated against its stored \
        schema and held to the same admin cost ceiling a registered launch is. Costs money \
        against max_cost. Returns the issue key to watch it by with crucible_playbook_run. Pass \
        schema_digest to be refused rather than launched if a save landed since you read it. \
        Pass provider (and optionally model) to run the pack's agent tasks on a registered \
        inference provider instead of the crucible.toml [agent] harness and model."
    )]
    async fn crucible_draft_launch(&self, Parameters(a): Parameters<DraftLaunch>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "Delete a draft, every version of it, and its secret bindings, shares and \
        schedules. Admin only. Gone is gone; the id is free for crucible_draft_create again."
    )]
    async fn crucible_draft_delete(&self, Parameters(a): Parameters<DraftDelete>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "Graduate a draft to a registered playbook: push its newest compiling \
        version as a branch under repo/path and open the export PR. Returns the PR URL. The draft \
        retires itself once that merged pack is imported. A draft already graduated is refused \
        with the open PR in the refusal."
    )]
    async fn crucible_draft_graduate(&self, Parameters(a): Parameters<DraftGraduate>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "Publish a draft with no review: register its newest compiling version \
        as a registered playbook. The draft stays live and publishing it again re-pins the same \
        playbook. Only principals the active policy grants playbook_draft:publish may; everyone \
        else graduates through crucible_draft_graduate."
    )]
    async fn crucible_draft_publish(&self, Parameters(a): Parameters<DraftPublish>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "List the blueprints you can read: ID OWNER REV PUBLIC SECRETS DESCRIPTION. \
        A blueprint is a registered playbook offered as a starting point; SECRETS counts what \
        crucible_blueprint_use would carry for you and what it would leave to bind."
    )]
    async fn crucible_blueprints(&self, Parameters(a): Parameters<Blueprints>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "Start a draft from a blueprint: version 1 is its pack. Bindings of the \
        owning team's secrets carry when you are in that team; every other declared secret comes \
        back as a placeholder with its reason. Bind each with crucible_secret_bind at scope_kind \
        `playbook`, scope_id = draft_id; a launch with one unbound is refused. Author it from \
        there like any draft, starting with crucible_draft_pull."
    )]
    async fn crucible_blueprint_use(&self, Parameters(a): Parameters<BlueprintUse>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "List registered playbooks: ID REPO REV BY DESCRIPTION. These are what \
        crucible_launch can start."
    )]
    async fn crucible_playbooks(&self, Parameters(a): Parameters<Playbooks>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "The admin caps a launch's max_cost and max_time are bounded by. Read \
        before crucible_launch or crucible_draft_launch: asking for more than either is a 422."
    )]
    async fn crucible_playbook_caps(&self, Parameters(a): Parameters<PlaybookCaps>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "The JSON Schema a playbook's params are validated against. Read this \
        before crucible_launch; guessing field names is how a launch 422s."
    )]
    async fn crucible_playbook_schema(&self, Parameters(a): Parameters<PlaybookSchema>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "Launch a playbook. Costs money against max_cost, which the controller caps \
        at the admin ceiling. Returns the issue key to watch it by with crucible_playbook_run. \
        Pass provider (and optionally model) to run the pack's agent tasks on a registered \
        inference provider instead of the crucible.toml [agent] harness and model."
    )]
    async fn crucible_launch(&self, Parameters(a): Parameters<Launch>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "Playbook launches as a table: KEY PLAYBOOK STATUS N COST/CAP AGENT BY \
        WHY. AGENT is the provider and model the launch pinned, or a dash for the platform \
        default. WHY names what stopped one — a secrets refusal, a park, or a drifted schema."
    )]
    async fn crucible_playbook_runs(&self, Parameters(a): Parameters<PlaybookRuns>) -> String {
        self.run(&a).await
    }

    #[tool(description = "One playbook launch whole: its params, caps, and any refusal, as JSON.")]
    async fn crucible_playbook_run(&self, Parameters(a): Parameters<PlaybookRun>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "The runs leaderboard: RUN NAME STATUS REPO ISSUE BEST COST CLUSTER CREATED. \
        Filter by status, repo, or the cluster it was dispatched to."
    )]
    async fn crucible_runs(&self, Parameters(a): Parameters<Runs>) -> String {
        self.run(&a).await
    }

    #[tool(description = "One run with its candidates, as JSON.")]
    async fn crucible_run(&self, Parameters(a): Parameters<Run>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "Recurrences: ID PLAYBOOK CRON STATE NEXT LAST FAILS OWNER. STATE is \
        `blocked` when the owner must sign in again — enabled, and still will not fire."
    )]
    async fn crucible_schedules(&self, Parameters(a): Parameters<Schedules>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "Tracker watches: ID PLAYBOOK TRACKER QUERY STATE SWEPT LAUNCHED FAILS OWNER. \
        A watch launches its playbook once per item its query matches, never again for the same \
        item. STATE is `blocked` when the owner must sign in again."
    )]
    async fn crucible_watches(&self, Parameters(a): Parameters<Watches>) -> String {
        self.run(&a).await
    }

    #[tool(description = "One tracker watch with its full authorization, as JSON.")]
    async fn crucible_watch(&self, Parameters(a): Parameters<Watch>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "Webhooks: ID PLAYBOOK VERIFIER STATE RATE DELIVERED FAILS OWNER. A webhook \
        launches its playbook from deliveries a sender outside the controller posts. STATE is \
        `blocked` when the owner must sign in again."
    )]
    async fn crucible_webhooks(&self, Parameters(a): Parameters<Webhooks>) -> String {
        self.run(&a).await
    }

    #[tool(description = "One webhook with its verifier, transform, and authorization, as JSON.")]
    async fn crucible_webhook(&self, Parameters(a): Parameters<Webhook>) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "A webhook's recorded deliveries, newest first: ID RECEIVED OUTCOME LAUNCH \
        REASON. OUTCOME is queued until processed, then launched, filtered, duplicate, \
        throttled, or failed."
    )]
    async fn crucible_webhook_deliveries(
        &self,
        Parameters(a): Parameters<WebhookDeliveries>,
    ) -> String {
        self.run(&a).await
    }

    #[tool(
        description = "The endpoint and credential this session actually uses, and who the \
        controller says that makes you. Run this first when a mutation 403s."
    )]
    async fn crucible_whoami(&self) -> String {
        self.run(&Whoami {}).await
    }
}

// Route off the stored router rather than the macro's default `Self::tool_router()`, which rebuilds
// the whole table on every single request.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for CrucibleMcp {
    /// Advertise the `tools` capability during `initialize`. Without it a spec-compliant client
    /// connects, never calls `tools/list`, and the server looks connected while exposing nothing.
    fn get_info(&self) -> ServerConfig {
        // ServerConfig is #[non_exhaustive], so it has to be built by mutation rather than a struct
        // literal.
        let mut info = ServerConfig::default();
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
        let tools = CrucibleMcp::tools();
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
        let names: Vec<String> = CrucibleMcp::tools()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(names.len(), 46, "tools: {names:?}");
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
            "crucible_webhooks",
            "crucible_webhook",
            "crucible_webhook_deliveries",
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
            "crucible_draft_publish",
            "crucible_blueprints",
            "crucible_blueprint_use",
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
        let (schema, _) = tool_of("crucible_issues");
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

        let (publish_schema, publish) = tool_of("crucible_draft_publish");
        assert!(publish_schema.contains("playbook"), "{publish_schema}");
        assert!(publish.contains("playbook_draft:publish"), "{publish}");
    }

    /// A model that starts from a blueprint has to learn from the tools alone where the catalog
    /// leads and how to fill what did not carry.
    #[test]
    fn the_blueprint_tools_lead_to_a_use_and_to_filling_its_placeholders() {
        let (_, catalog) = tool_of("crucible_blueprints");
        assert!(catalog.contains("crucible_blueprint_use"), "{catalog}");

        let (schema, using) = tool_of("crucible_blueprint_use");
        for field in ["\"id\"", "\"draft_id\"", "\"description\""] {
            assert!(schema.contains(field), "{field} missing: {schema}");
        }
        assert!(using.contains("placeholder"), "{using}");
        assert!(using.contains("crucible_secret_bind"), "{using}");
        assert!(using.contains("crucible_draft_pull"), "{using}");
    }

    /// An error must arrive as readable text, not as an MCP fault the model can't see into.
    #[tokio::test]
    async fn errors_flatten_into_a_readable_line() {
        let cfg = crate::config::Config {
            url: "http://127.0.0.1:9".into(),
            auth: crate::config::Auth::None,
        };
        let mcp = CrucibleMcp::new(Arc::new(Client::connect(&cfg).expect("connect")));
        let out = mcp.run(&Contracts { json: false }).await;
        assert!(out.starts_with("error: "), "{out}");
    }
}
