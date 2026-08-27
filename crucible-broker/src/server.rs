//! The MCP wire: exposes the broker's tools to the sandboxed agent. The agent reaches only this;
//! the privilege + admission logic stay server-side. The trace cache, headroom, and approval
//! backends are injected behind the trait boundaries (mediated provisioning).

use crate::approval::{ApprovalBackend, ApprovalChannel};
use crate::broker::{Broker, TraceResolver};
use crate::codegen::CodegenState;
use crate::control_approval::ControlApproval;
use crate::deploy::DeployRegistry;
use crate::draft_pr::DraftPrApproval;
use crate::gpu_check::GpuCheck;
use crate::types::{Resolution, TraceParams};
use jira_mcp::{JiraClient, render};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, schemars, tool, tool_router};
use serde::Deserialize;
use std::sync::Arc;

/// MCP tool arguments (a JSON-schema'd struct on the wire), mapped to the core [`TraceParams`].
/// Kept separate so the core type stays free of the `schemars` dependency.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RequestTraceArgs {
    /// The model the trace should be captured against.
    model: String,
    /// Request concurrency the workload drives (a regime-defining knob).
    concurrency: usize,
    /// Number of distinct prefixes in the workload.
    prefixes: usize,
    /// Default output length.
    max_tokens: u32,
    /// Long-request output length (0 = homogeneous).
    long_tokens: u32,
    /// Fraction of requests that are "long".
    long_frac: f64,
}

/// Args for `distress`: signal the operator, and at `error` severity suspend the run.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DistressArgs {
    /// How loud: `info`/`warn` note and continue, `error` suspends the run and pages the operator.
    severity: crate::distress::Severity,
    /// What is wrong, in one line. Must be non-empty.
    reason: String,
    /// Concrete artifacts backing the claim (log handles, RESULTS rows).
    #[serde(default)]
    evidence: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReportArgs {}

/// Args for `check_capture`: poll an opened approval by its handle (the PR url).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CheckCaptureArgs {
    /// The approval handle from a `pending_approval` resolution (the draft PR url).
    handle: String,
}

/// Args for `build_epp`: none — the broker builds the agent's current sandbox edits.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BuildEppArgs {}

/// Args for `measure`: none — the broker runs the judge's exact measurement engine-side.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MeasureArgs {}

/// Args for `deploy_composite`: none — the broker builds + rolls the agent's current composite edits.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeployCompositeArgs {}

/// Args for `deploy_status`: poll one in-flight (or finished) composite deploy.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeployStatusArgs {
    /// The `deploy_id` from a `deploy_composite` `building` reply.
    deploy_id: String,
    /// Opt in to the raw apply-hook log lines since `since` (the scannable transcript). Default false:
    /// a routine poll wants only the phase. Pull the log when a phase stalls or the deploy `failed`.
    #[serde(default)]
    include_log: bool,
    /// Log offset — return only lines AFTER this index. Pass the previous poll's `next_offset` to tail
    /// just the new lines instead of re-reading the whole log. Defaults to 0 (from the top).
    #[serde(default)]
    since: usize,
    /// Long-poll: block up to this many seconds (server-clamped) until the phase changes or the
    /// deploy finishes, then return. 0 (the default) answers immediately. Pass e.g. 60 while a
    /// deploy is rolling so one call spans a whole quiet stretch instead of many no-change polls.
    #[serde(default)]
    wait_seconds: u64,
}

/// Args for `deploy_candidate`: which image to roll onto the rig.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeployCandidateArgs {
    /// The image ref to deploy. Omit to deploy the latest `build_epp` candidate.
    #[serde(default)]
    image_ref: Option<String>,
}

/// Args for `profile`: capture a profile of the deployed candidate (profiler support over MCP).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProfileArgs {
    /// Which component to profile on a multi-component rig. Omit when the rig
    /// has a single profileable component; if it has several, an omitted target errors with the list.
    #[serde(default)]
    target: Option<String>,
    /// The profile kind (backend-defined). pprof backend: `profile` (CPU), `heap`, `allocs`,
    /// `mutex`, `block`, `goroutine`.
    kind: String,
    /// Capture window in seconds (used by time-based kinds like CPU `profile`). Clamped to [1,300].
    #[serde(default = "default_profile_seconds")]
    seconds: u32,
}

fn default_profile_seconds() -> u32 {
    10
}

/// Args for `jira_search`: a JQL query over the mediated JIRA instance.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JiraSearchArgs {
    /// JQL, e.g. `project = PROJ AND labels = my-team ORDER BY created DESC`.
    jql: String,
    /// Max issues to return (clamped to [1,100]). Defaults to 25.
    #[serde(default = "default_jira_limit")]
    limit: u32,
}

fn default_jira_limit() -> u32 {
    25
}

/// Args for `jira_get_issue`: fetch one issue by key.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JiraGetIssueArgs {
    /// The issue key, e.g. `PROJ-1234`.
    issue_key: String,
    /// Return the FULL raw issue (every field) instead of the curated record. Default false: you get a
    /// tight record (summary/status/type/labels/description + the curated custom fields, nulls omitted).
    /// Set true only when you need a field the curated view drops.
    #[serde(default)]
    raw: bool,
}

/// Args for `jira_add_comment`: post a comment to an issue.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JiraAddCommentArgs {
    /// The issue key, e.g. `PROJ-1234`.
    issue_key: String,
    /// The comment body (Markdown / JIRA wiki markup).
    comment: String,
}

/// Args for `profile_query`: run a read-only analysis query over a captured profile.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProfileQueryArgs {
    /// Which component to query on a multi-component rig — must match the
    /// `target` you captured under (the `profile` reply echoes it). Omit for a single-component rig.
    #[serde(default)]
    target: Option<String>,
    /// The backend-defined query. pprof backend: `top`, `list=<FuncRegex>` (per-line cost in the
    /// real source — the one you want), `peek=<Func>`, `traces`, `tree`, `text`. Read-only.
    query: String,
    /// Which capture to query (a `handle` from `profile`). Omit to query the most recent capture.
    #[serde(default)]
    handle: Option<String>,
}

/// Args for `codegen_build`: which build path to derive the candidate through.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodegenBuildArgs {
    /// The build mode: `derive` (fast: pinned base + your edits + editable install, pure-Python briefs)
    /// or `full` (slow kernel-compile path). Only modes the scenario declares are accepted. Default `derive`.
    #[serde(default = "default_build_mode")]
    mode: String,
}

fn default_build_mode() -> String {
    "derive".to_string()
}

/// Args for `codegen_benchmark`: measure a built candidate on the GPU.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodegenBenchmarkArgs {
    /// The digest-pinned image from a `codegen_build` `built` result (the ONLY runnable handle).
    digest: String,
    /// Optional comparison toggles: env-name → value. Only names AND values the scenario declared are
    /// accepted; anything else is rejected. The workload itself is frozen and NOT tunable here.
    #[serde(default)]
    toggles: std::collections::BTreeMap<String, String>,
    /// Optional repetition count for noise reduction; accepted only when the scenario declares it,
    /// within its declared bounds.
    #[serde(default)]
    reps: Option<u32>,
}

/// Args for `codegen_lm_eval`: measure a built candidate's correctness on the GPU.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodegenLmEvalArgs {
    /// The digest-pinned image from a `codegen_build` `built` result.
    digest: String,
    /// Optional number of eval examples; accepted only when the scenario declares it, within its
    /// declared bounds.
    #[serde(default)]
    limit: Option<u32>,
}

/// Args for `codegen_profile`: capture a GPU trace of a built candidate.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodegenProfileArgs {
    /// The digest-pinned image from a `codegen_build` `built` result.
    digest: String,
}

/// Args for `codegen_jobs`: none — it lists this broker's own submitted GPU jobs.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodegenJobsArgs {}

/// Args for `fetch_log`: read a captured job/build log by handle.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FetchLogArgs {
    /// A log handle from a `logs` array (benchmark/lm_eval/build/profile reply).
    handle: String,
    /// Byte offset to read from — pass the previous reply's `next_offset` to tail. Default 0.
    #[serde(default)]
    offset: usize,
}

/// Args for `fetch_trace`: read a captured (binary) profile trace by handle.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FetchTraceArgs {
    /// A `trace` handle from a `codegen_profile` `profiled` reply.
    handle: String,
    /// Byte offset to read from — pass the previous reply's `next_offset` to tail. Default 0.
    #[serde(default)]
    offset: usize,
}

impl From<RequestTraceArgs> for TraceParams {
    fn from(a: RequestTraceArgs) -> Self {
        TraceParams {
            model: a.model,
            concurrency: a.concurrency,
            prefixes: a.prefixes,
            max_tokens: a.max_tokens,
            long_tokens: a.long_tokens,
            long_frac: a.long_frac,
        }
    }
}

/// The MCP server: the broker's tools plus, when enabled, the proxied JIRA read+comment tools.
#[derive(Clone)]
pub struct McpServer {
    broker: Arc<Broker>,
    /// Native JIRA Cloud client (server-side mediation), shared across sessions. `None` when JIRA isn't
    /// configured; the `jira_*` tools then report `disabled` instead of vanishing.
    jira: Option<Arc<JiraClient>>,
    /// In-flight + finished composite deploys (async `deploy_composite` + `deploy_status` polling).
    /// INJECTED rather than built here: in stateless mode the http service makes a fresh `McpServer` per
    /// request, so a per-instance registry would lose the deploy between the `deploy_composite` call
    /// that registers it and the `deploy_status` call that polls it. main() owns the one shared Arc.
    deploys: Arc<DeployRegistry>,
    /// Process-lifetime memo for the code-gen build/measure tools. INJECTED like `deploys`: in
    /// stateless mode a fresh `McpServer` is built per request, so a per-instance memo would never
    /// hit; main() owns the one shared Arc.
    codegen: Arc<CodegenState>,
    tool_router: ToolRouter<Self>,
}

#[tool_router(vis = "pub(crate)")]
impl McpServer {
    /// Build the server around an injected trace `resolver` (the domain-specific cache backend), the
    /// frozen regime (the judge-impact reference), and the headless flag. The draft-PR approval
    /// target is `BROKER_FORK` / `BROKER_BASE` (defaults: the wseaton llm-d-router fork, `main`).
    pub fn new(
        resolver: Box<dyn TraceResolver + Send + Sync>,
        frozen: TraceParams,
        degraded_headless: bool,
        jira: Option<Arc<JiraClient>>,
        deploys: Arc<DeployRegistry>,
        codegen: Arc<CodegenState>,
    ) -> Self {
        // Approval channel is a runtime choice (async approval). `BROKER_APPROVAL=control` selects the
        // attended, token-free control-bridge surface; otherwise the headless draft-PR default.
        let (approval, approval_channel): (
            Box<dyn ApprovalBackend + Send + Sync>,
            ApprovalChannel,
        ) = if std::env::var("BROKER_APPROVAL").as_deref() == Ok("control") {
            (Box::new(ControlApproval), ApprovalChannel::ControlChannel)
        } else {
            let repo =
                std::env::var("BROKER_FORK").unwrap_or_else(|_| "wseaton/llm-d-router".into());
            let base = std::env::var("BROKER_BASE").unwrap_or_else(|_| "main".into());
            (
                Box::new(DraftPrApproval { repo, base }),
                ApprovalChannel::DraftPr,
            )
        };
        // Flavor the domain-specific description phrases from the deployment's env; with none set
        // the routed text is exactly the generic attribute text.
        let mut tool_router = Self::tool_router();
        crate::describe::apply(&mut tool_router, &crate::describe::DescVars::from_env());
        Self {
            broker: Arc::new(Broker {
                resolver,
                admission: Box::new(GpuCheck::from_env()),
                approval,
                approval_channel,
                frozen,
                degraded_headless,
            }),
            jira,
            deploys,
            codegen,
            tool_router,
        }
    }

    #[tool(
        description = "Request a calibrated GPU-free latency trace for the given run parameters. \
        Returns a JSON Resolution: hit{knobs} (use them, continue) | submitted | deferred | \
        pending_approval | escalate. The server admits or denies; you never provision directly."
    )]
    async fn request_trace(&self, Parameters(args): Parameters<RequestTraceArgs>) -> String {
        let params: TraceParams = args.into();
        match self.broker.request_trace(&params) {
            Ok(res) => {
                // A judge-changing grant opened an approval. For the draft-PR channel the broker watches
                // it out-of-band and fires a re-scope to the loop's control bridge on the human yes.
                // For the control channel the operator approves over that bridge directly (the loop
                // resolves it), so there is nothing to watch here.
                if let Resolution::PendingApproval { trace_id, approval } = &res
                    && self.broker.approval_channel == ApprovalChannel::DraftPr
                {
                    crate::watch::spawn_approval_watch(
                        self.broker.clone(),
                        approval.clone(),
                        trace_id.0.clone(),
                    );
                }
                serde_json::to_string(&res)
                    .unwrap_or_else(|e| format!(r#"{{"status":"error","error":"{e}"}}"#))
            }
            Err(e) => format!(r#"{{"status":"error","error":"{e}"}}"#),
        }
    }

    #[tool(
        description = "Signal the operator. severity=error suspends the run and pages the \
        operator; invoke it only when the run cannot succeed without operator action: the same \
        environment failure has recurred across iterations, a required capability is missing, or \
        the goal is infeasible as specified. It is not for hard-but-possible tasks. \
        severity=info|warn post a status note and the run continues; use them for cheap \
        operator-should-know signals. reason must be non-empty; evidence should reference \
        concrete artifacts (log handles, RESULTS rows)."
    )]
    async fn distress(&self, Parameters(args): Parameters<DistressArgs>) -> String {
        if args.reason.trim().is_empty() {
            return json_err("reason must be non-empty: say what the operator has to fix");
        }
        // Writes the handoff file and posts the webhook (blocking IO); keep it off the reactor,
        // and off `tokio::task::spawn_blocking` so the events land on the tool span.
        match crate::telemetry::spawn_blocking(move || {
            crate::distress::distress(args.severity, &args.reason, &args.evidence)
        })
        .await
        {
            Ok(Ok(reply)) => reply,
            Ok(Err(e)) => json_err(&e),
            Err(e) => json_err(&format!("distress task failed: {e}")),
        }
    }

    #[tool(
        description = "Post the engine-authored workflow status to the configured Crucible Slack channel. Call once from a final reporting or epilogue task. This tool takes no message content: task names, statuses, spend, and the Crucible artifact link come from trusted engine state. It does not suspend the run; use distress when operator action is required."
    )]
    async fn report(&self, Parameters(_args): Parameters<ReportArgs>) -> String {
        match crate::telemetry::spawn_blocking(|| crate::report::deliver(None, None)).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(e)) => json_err(&e),
            Err(e) => json_err(&format!("report task failed: {e}")),
        }
    }

    #[tool(
        description = "Check whether an opened capture/re-scope approval has been granted. Pass \
        the handle (draft PR url) from a pending_approval resolution. Returns \
        {\"state\":\"pending|approved|denied\"}."
    )]
    async fn check_capture(&self, Parameters(args): Parameters<CheckCaptureArgs>) -> String {
        match self.broker.approval.poll(&args.handle) {
            Ok(state) => format!("{{\"state\":\"{state:?}\"}}").to_lowercase(),
            Err(e) => format!(r#"{{"state":"error","error":"{e}"}}"#),
        }
    }

    #[tool(
        description = "Build your CURRENT edits into a candidate image. The loop pod \
        pulls your working tree out of the sandbox, runs the container build, and pushes it — you \
        never hold build creds. Returns a JSON status: built{image_ref, cached} (ready to deploy; \
        cached=true means this exact tree was already built and pushed, so nothing was rebuilt) | \
        compile_error{log} (fix the errors and call again) | wrap_up{reason} (your candidate budget \
        for this turn is spent — commit your best CANDIDATE.md and END the turn) | disabled | error. \
        Edit, build, fix, repeat until it builds, then deploy_candidate."
    )]
    async fn build_epp(&self, Parameters(_): Parameters<BuildEppArgs>) -> String {
        // The build shells buildah/openshell for minutes; keep it off the async reactor.
        crate::telemetry::spawn_blocking(crate::build::build_epp)
            .await
            .unwrap_or_else(|e| format!(r#"{{"status":"error","error":"build task failed: {e}"}}"#))
    }

    #[tool(
        description = "Roll the live rig onto a built candidate so the judge measures it. Pass \
        image_ref from a build_epp `built` result, or omit it to deploy the latest candidate. \
        Returns a JSON status: deployed{image_ref} | disabled | error."
    )]
    async fn deploy_candidate(&self, Parameters(args): Parameters<DeployCandidateArgs>) -> String {
        crate::telemetry::spawn_blocking(move || crate::build::deploy_candidate(args.image_ref))
            .await
            .unwrap_or_else(|e| {
                format!(r#"{{"status":"error","error":"deploy task failed: {e}"}}"#)
            })
    }

    #[tool(
        description = "Build + deploy your CURRENT composite candidate (every changed component) to the \
        live rig so the judge measures its real effect. This is ASYNC and returns IMMEDIATELY — the loop \
        pod pulls your working tree, then rebuilds the changed component(s) (you never hold build creds), \
        rolls the rig, and verifies the rig's invariant ON A BACKGROUND THREAD. The full roll takes \
        MINUTES; do NOT wait or retry. You get back building{deploy_id} — then poll \
        deploy_status(deploy_id) to scan progress until it's done. (Also: disabled | wrap_up{reason} = \
        this turn's candidate budget is spent, commit your best CANDIDATE.md and END the turn | error.) A \
        compile_error costs nothing; a completed live deploy consumes one candidate. One deploy runs at a \
        time — calling again while one is in flight just returns its deploy_id."
    )]
    async fn deploy_composite(&self, Parameters(_): Parameters<DeployCompositeArgs>) -> String {
        // Pulls the sandbox tree (openshell download, blocking IO) before kicking the hook on a
        // background thread; keep that off the async reactor.
        let reg = self.deploys.clone();
        crate::telemetry::spawn_blocking(move || crate::deploy::deploy_composite(&reg))
            .await
            .unwrap_or_else(|e| {
                format!(r#"{{"status":"error","error":"deploy task failed: {e}"}}"#)
            })
    }

    #[tool(
        description = "Poll a composite deploy you kicked off with deploy_composite. Returns \
        {phase, done, result?}: phase is a short live stage label the rig reports as it progresses \
        (it ends at `deployed` or `failed`); treat `done` as the authoritative completion signal. \
        When done is true, result is the terminal reply (deployed{image_ref} | compile_error{log} | \
        error{...}). Prefer LONG-POLLING over rapid re-calls: pass wait_seconds (e.g. 60) and the \
        call blocks server-side until the phase changes or the deploy finishes, so you spend one \
        call per stage instead of one per few seconds — the roll takes minutes. Set include_log=true \
        to also get the raw apply-hook transcript (pass the previous reply's next_offset as `since` \
        to tail just the new lines) — do that when a phase stalls or you hit failed, to see exactly \
        what the build/roll printed."
    )]
    async fn deploy_status(&self, Parameters(args): Parameters<DeployStatusArgs>) -> String {
        // May block for the long-poll window (a condvar wait); keep it off the async reactor.
        let deploys = self.deploys.clone();
        crate::telemetry::spawn_blocking(move || {
            crate::deploy::deploy_status(
                &deploys,
                &args.deploy_id,
                args.include_log,
                args.since,
                args.wait_seconds,
            )
        })
        .await
        .unwrap_or_else(|e| format!(r#"{{"status":"error","error":"status task failed: {e}"}}"#))
    }

    #[tool(
        description = "Measure the currently-deployed candidate's fitness through the JUDGE's exact path. \
        The loop pod runs the gate's measurement engine-side (in-cluster, straight to the rig — NOT \
        the port-forward tunnel you'd be stuck with), so the number you get back is the SAME one the \
        judge gates on. Returns the judge's JSON record ({valid, score, detail{...}}). Use this after \
        deploy_candidate instead of running bench yourself — it removes the tunnel confound entirely."
    )]
    async fn measure(&self, Parameters(_): Parameters<MeasureArgs>) -> String {
        // Shells the judge's measure_cmd for minutes; keep it off the async reactor.
        crate::telemetry::spawn_blocking(crate::measure::measure)
            .await
            .unwrap_or_else(|e| {
                format!(r#"{{"status":"error","error":"measure task failed: {e}"}}"#)
            })
    }

    #[tool(
        description = "Capture a PROFILE of the deployed candidate's internals so you optimize the \
        MEASURED hot path instead of guessing (an end-to-end gate can't isolate one component's hot \
        path; a profile can). The loop pod captures it (it has the cluster reach + token + tooling you \
        don't) and returns a handle. kind is backend-defined; for a pprof backend: `profile` \
        (CPU, the usual one — capture it WHILE a `measure` is in flight for a hot profile), `heap`, \
        `allocs`, `mutex`, `block`. Then read it with `profile_query`. On a multi-component rig pass \
        `target` to pick what to profile; omit it when there's only one. \
        Returns JSON: captured{handle, target, bytes} | disabled | error."
    )]
    async fn profile(&self, Parameters(args): Parameters<ProfileArgs>) -> String {
        // Capture shells a (possibly seconds-long) command; keep it off the async reactor.
        crate::telemetry::spawn_blocking(move || {
            crate::profile::profile(args.target.as_deref(), &args.kind, args.seconds)
        })
        .await
        .unwrap_or_else(|e| format!(r#"{{"status":"error","error":"profile task failed: {e}"}}"#))
    }

    #[tool(
        description = "Query a captured profile (read-only, text). Pass the kind of read you want; \
        for a pprof backend: `top` (hottest functions), `list=<FuncRegex>` (per-line ns/alloc \
        in the REAL source — this is the one that tells you exactly which lines to change), \
        `peek=<Func>`, `traces`, `tree`. Omit handle to query your most recent `profile` capture. On a \
        multi-component rig pass the same `target` you captured under. Server/file-writing queries are \
        refused. Use this to FIND what to change, then build → deploy → `measure` to SCORE it. Returns \
        JSON: query{output} | disabled | error."
    )]
    async fn profile_query(&self, Parameters(args): Parameters<ProfileQueryArgs>) -> String {
        crate::telemetry::spawn_blocking(move || {
            crate::profile::profile_query(args.target.as_deref(), &args.query, args.handle)
        })
        .await
        .unwrap_or_else(|e| {
            format!(r#"{{"status":"error","error":"profile_query task failed: {e}"}}"#)
        })
    }

    #[tool(
        description = "Build your CURRENT workspace edits into a runnable, digest-pinned candidate \
        image. The loop pod pulls your working tree out of the sandbox and derives the image (you never \
        hold build creds). Pass mode=`derive` (fast, default) or `full` (kernel compile) — only modes the \
        scenario declares are accepted. Returns JSON: built{tree_hash, digest, mode, cached, log} (pass \
        digest to codegen_benchmark/codegen_lm_eval; an identical tree replays the digest with \
        cached=true; `log` is the build-log handle) | job_failed{reason, logs} (build error — fetch_log \
        the handle, fix, rebuild) | rejected_kwarg | disabled | error. The build log streams into its \
        handle AS THE BUILD RUNS, so a concurrent session can fetch_log it to watch progress."
    )]
    async fn codegen_build(&self, Parameters(args): Parameters<CodegenBuildArgs>) -> String {
        let state = self.codegen.clone();
        crate::telemetry::spawn_blocking(move || crate::codegen::build(&state, &args.mode))
            .await
            .unwrap_or_else(|e| format!(r#"{{"status":"error","error":"build task failed: {e}"}}"#))
    }

    #[tool(
        description = "Benchmark a built candidate on the GPU through the FROZEN workload (model, TP, \
        ISL/OSL, and every bench flag are server-side — you cannot tune what you're graded on). The loop \
        pod submits a Kueue-admitted job, waits, and parses the result. Pass `digest` from codegen_build; \
        optionally `toggles` (env-name→value, only scenario-declared names AND values) for A/B comparison \
        runs, and `reps` (scenario-declared bounds) to average out noise. Returns JSON: measured{metrics, objective{key, \
        direction}, logs, cached} — `objective` names the metric the gate optimizes (e.g. tpot_ms, lower \
        wins). Repeat calls on the same digest+toggles are memoized (cached=true): a re-issue after the \
        job finished is a free cache hit, NEVER a resubmission. Also: pending{job, log, hint} (the wait \
        budget ran out while the job was still queued/running — it KEEPS running; re-issue this exact \
        call to re-attach and keep waiting, do NOT sleep-loop) | job_failed{reason, logs} (crash — \
        fetch_log to investigate) | rejected_kwarg | budget_exhausted | disabled | error. The call \
        BLOCKS until the job resolves or the server wait budget (default 20 min, above typical Kueue \
        admission) runs out; for progress, poll codegen_jobs + fetch_log (its handle) from a \
        concurrent session."
    )]
    async fn codegen_benchmark(
        &self,
        Parameters(args): Parameters<CodegenBenchmarkArgs>,
    ) -> String {
        let state = self.codegen.clone();
        let toggles: Vec<(String, String)> = args.toggles.into_iter().collect();
        crate::telemetry::spawn_blocking(move || {
            crate::codegen::benchmark(&state, &args.digest, &toggles, args.reps)
        })
        .await
        .unwrap_or_else(|e| format!(r#"{{"status":"error","error":"benchmark task failed: {e}"}}"#))
    }

    #[tool(
        description = "Run the FROZEN lm_eval correctness suite against a built candidate on the GPU (the \
        gate re-runs this frozen; use it as your dev check). Pass `digest` from codegen_build; optionally \
        `limit` (eval example count, scenario-declared bounds). Returns JSON: measured{metrics, objective{key, direction}, logs, \
        cached} — objective is the score to maximize. Memoized per digest+limit (a re-issue after the \
        job finished is a free cache hit, never a resubmission). Also: pending{job, log, hint} (wait \
        budget ran out, job still queued/running — re-issue this exact call to re-attach) | \
        job_failed{reason, logs} | rejected_kwarg | budget_exhausted | disabled | error. The call \
        BLOCKS until the job resolves or the server wait budget (default 20 min) runs out; for \
        progress, poll codegen_jobs + fetch_log from a concurrent session."
    )]
    async fn codegen_lm_eval(&self, Parameters(args): Parameters<CodegenLmEvalArgs>) -> String {
        let state = self.codegen.clone();
        crate::telemetry::spawn_blocking(move || {
            crate::codegen::lm_eval(&state, &args.digest, args.limit)
        })
        .await
        .unwrap_or_else(|e| format!(r#"{{"status":"error","error":"lm_eval task failed: {e}"}}"#))
    }

    #[tool(
        description = "Capture a GPU PROFILE (trace) of a built candidate over the FROZEN profiling \
        workload so you optimize the MEASURED hot path instead of guessing. The loop pod submits a \
        Kueue-admitted job (like codegen_benchmark — it HOLDS GPUs and counts against your budget), \
        the frozen capture command writes its trace artifact, and the broker hands you a handle. Pass \
        `digest` from codegen_build. Returns JSON: profiled{trace, logs, cached} — read `trace` with \
        fetch_trace (binary artifact) and `logs` with fetch_log. Memoized per digest (cached=true — a \
        re-issue after the job finished is a free cache hit, never a resubmission). Also: \
        unconfigured{reason} (this rig/scenario declares no profile — nothing to run) | pending{job, \
        log, hint} (wait budget ran out, job still queued/running — re-issue this exact call to \
        re-attach) | job_failed{reason, logs} | budget_exhausted | disabled | error. The call BLOCKS \
        until the job resolves or the server wait budget (default 20 min) runs out; for progress, poll \
        codegen_jobs + fetch_log from a concurrent session."
    )]
    async fn codegen_profile(&self, Parameters(args): Parameters<CodegenProfileArgs>) -> String {
        let state = self.codegen.clone();
        crate::telemetry::spawn_blocking(move || crate::codegen::profile(&state, &args.digest))
            .await
            .unwrap_or_else(|e| {
                format!(r#"{{"status":"error","error":"profile task failed: {e}"}}"#)
            })
    }

    #[tool(
        description = "List the GPU jobs THIS broker submitted for you (read-only; in-flight plus the \
        last few terminal ones). Each entry: {name, kind (bench|lmeval|profile), digest, started_at, \
        log, lifecycle (queued|admitted|running|succeeded|failed|unknown), kueue{queue_name, \
        admitted?, pending_reason?}, pod{phase, started_at}?}. Use it from a concurrent session while \
        a codegen_benchmark/lm_eval/profile call blocks: `lifecycle` tells you whether the job is \
        still waiting in the Kueue queue (pending_reason says why) or running, and `log` is the \
        handle to fetch_log-tail for live output. Live Kueue/pod detail is BEST-EFFORT under a short \
        server-side deadline — an unavailable cluster view degrades an in-flight entry to lifecycle \
        `unknown` (admitted omitted when unknown). The list is in-memory and bounded (last 20): a \
        broker restart forgets it, and more than 20 concurrent submissions can evict a still-running \
        entry. Returns JSON: jobs{jobs: [...]} | disabled."
    )]
    async fn codegen_jobs(&self, Parameters(_): Parameters<CodegenJobsArgs>) -> String {
        // Live entries query the cluster (workload + pod lookups); keep it off the async reactor.
        let state = self.codegen.clone();
        tokio::task::spawn_blocking(move || crate::codegen::jobs(&state))
            .await
            .unwrap_or_else(|e| format!(r#"{{"status":"error","error":"jobs task failed: {e}"}}"#))
    }

    #[tool(
        description = "Read a captured job/build log by handle (from a benchmark/lm_eval/build/profile \
        reply). Pass `handle` and an optional byte `offset` (use the previous reply's `next_offset` to \
        tail). Returns JSON: log{text, next_offset, total_bytes} | error. Handles are LIVE: build and \
        measure logs stream into them while the work runs, so re-fetching a handle from an in-flight \
        call (with `offset` from the last read) tails its progress from a concurrent session. A live \
        log only ever GROWS while its call runs (append-consistent), so an offset cursor stays valid; \
        once the call returns, the handle holds the complete authoritative log. For a binary `trace` \
        handle use fetch_trace instead — this decodes as text and would corrupt a gzip artifact."
    )]
    async fn fetch_log(&self, Parameters(args): Parameters<FetchLogArgs>) -> String {
        let state = self.codegen.clone();
        crate::telemetry::spawn_blocking(move || {
            crate::codegen::fetch_log(&state, &args.handle, args.offset)
        })
        .await
        .unwrap_or_else(|e| format!(r#"{{"status":"error","error":"fetch_log task failed: {e}"}}"#))
    }

    #[tool(
        description = "Read a captured profile `trace` (a binary gzip artifact) by handle, base64'd. \
        Pass the `trace` handle from a codegen_profile `profiled` reply and an optional byte `offset` \
        (pass the previous reply's `next_offset` to pull the next window; concatenate the decoded \
        windows to reconstruct the artifact). Returns JSON: trace{base64, next_offset, total_bytes} | \
        error. Use this (not fetch_log) for a trace so the gzip bytes survive intact."
    )]
    async fn fetch_trace(&self, Parameters(args): Parameters<FetchTraceArgs>) -> String {
        let state = self.codegen.clone();
        crate::telemetry::spawn_blocking(move || {
            crate::codegen::fetch_trace(&state, &args.handle, args.offset)
        })
        .await
        .unwrap_or_else(|e| {
            format!(r#"{{"status":"error","error":"fetch_trace task failed: {e}"}}"#)
        })
    }

    #[tool(
        description = "Search JIRA by JQL (read-only, mediated — the broker holds the credentials, you \
        never see a token). Pass `jql` (e.g. `project = PROJ AND labels = my-team`) and an optional \
        `limit`. Returns COMPACT rows {key, summary, type, status, labels} — call jira_get_issue for \
        detail. Returns {\"status\":\"disabled\"} when JIRA isn't configured. \
        Issues here may be AUTHORITATIVE for feature requirements but INTERNAL: reason about them freely in \
        your thinking and notes, but NEVER reference the internal SOURCE in anything public — not the \
        project key, epic name, ticket key, title, or a quote — in code comments, commit messages, \
        PR/issue bodies, or summaries. Don't even write 'the internal epic requires X'; state X as an \
        engineering requirement on its own merits. Public artifacts attribute only public sources \
        (upstream RFCs/PRs, the code)."
    )]
    async fn jira_search(&self, Parameters(args): Parameters<JiraSearchArgs>) -> String {
        let Some(jira) = &self.jira else {
            return jira_disabled();
        };
        match jira.search(&args.jql, args.limit).await {
            Ok(rows) => render::search_rows(&rows),
            Err(e) => json_err(&format!("{e:#}")),
        }
    }

    #[tool(
        description = "Fetch one JIRA issue by key (read-only, mediated). Pass `issue_key` (e.g. \
        `PROJ-1234`). Returns a CURATED record by default — summary/status/type/priority/labels/ \
        components/description plus the custom fields that matter (intelligence_requested, target_version, \
        team, epic_link, product_manager, RICE/reach/confidence/effort, release_type, sfdc_cases_links, \
        blocked), with null fields omitted. Set `raw=true` for the full untouched issue when you need a \
        field the curated view drops. {\"status\":\"disabled\"} when JIRA isn't configured. The issue is \
        AUTHORITATIVE for requirements but INTERNAL — reason about it freely, but never name the internal \
        source (project/epic/ticket key, title, quote) in public artifacts (code comments, commit \
        messages, PR/issue bodies, summaries); state requirements on their own merits, attributing only \
        public sources."
    )]
    async fn jira_get_issue(&self, Parameters(args): Parameters<JiraGetIssueArgs>) -> String {
        let Some(jira) = &self.jira else {
            return jira_disabled();
        };
        // `raw` keeps the untouched issue reachable; the default is the compact rendering, which
        // carries the same facts for roughly a fifth of the tokens.
        match jira.get_issue(&args.issue_key, false).await {
            Ok(v) if args.raw => v.to_string(),
            Ok(v) => render::issue(&v, jira.config(), MAX_ISSUE_CHARS),
            Err(e) => json_err(&format!("{e:#}")),
        }
    }

    #[tool(
        description = "Post a comment to a JIRA issue (read+comment is the ONLY write — no create/ \
        transition/edit exist). Pass `issue_key` and the `comment` body (plain text / JIRA wiki). \
        Returns {id, url} on success, or {\"status\":\"disabled\"} when JIRA isn't configured."
    )]
    async fn jira_add_comment(&self, Parameters(args): Parameters<JiraAddCommentArgs>) -> String {
        let Some(jira) = &self.jira else {
            return jira_disabled();
        };
        match jira.add_comment(&args.issue_key, &args.comment).await {
            Ok(id) => format!(
                r#"{{"id":"{id}","url":"{}"}}"#,
                jira.browse_url(&args.issue_key)
            ),
            Err(e) => json_err(&format!("{e:#}")),
        }
    }
}

/// Prose cap for one rendered issue. Generous (an RFE's requirements are the point of reading it)
/// but bounded, so a pathological description can't eat the turn's context.
const MAX_ISSUE_CHARS: usize = 12_000;

/// The `disabled` reply when JIRA env (`JIRA_URL`/`JIRA_USERNAME`/`JIRA_API_TOKEN`) isn't set.
fn jira_disabled() -> String {
    r#"{"status":"disabled","reason":"JIRA is not configured (set JIRA_URL + JIRA_USERNAME + JIRA_API_TOKEN on the loop pod)"}"#
        .to_string()
}

/// A safely-escaped `{"status":"error","error":"…"}` line (serde handles the quoting, so an upstream
/// message with quotes/newlines can't break the JSON the agent parses).
fn json_err(msg: &str) -> String {
    serde_json::json!({ "status": "error", "error": msg }).to_string()
}

/// Hand-written (not `#[tool_handler]`) so EVERY tool invocation funnels through one instrumented
/// `call_tool`: the codegen four, fetch_log/fetch_trace, and the generic build/deploy/measure/
/// profile/JIRA tools all get a span with zero per-tool boilerplate. `call_tool`/`list_tools`/
/// `get_tool` mirror the macro's expansion exactly, plus the span.
///
/// DO NOT "simplify" this back to `#[tool_handler]`. The macro dispatches through the STATIC
/// `Self::tool_router()`, while these route through the INSTANCE `self.tool_router` that
/// [`McpServer::new`] has already run [`crate::describe::apply`] over. Swapping them compiles
/// clean and silently serves the unflavored descriptions, dropping every `BROKER_DESC_*` override
/// from the agent's tool list.
impl ServerHandler for McpServer {
    /// Advertise the `tools` capability in the initialize handshake. Without this the default
    /// `get_info` reports `capabilities.tools = None`, so a spec-compliant client (Claude Code)
    /// completes `initialize`, sees no tools capability, and never calls `tools/list`; the server
    /// looks "connected" but exposes zero tools.
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        use tracing::Instrument as _;
        // Telemetry off (the default): the macro-expansion path exactly; no span build, no
        // turn-file/env reads, no reply re-parse. The instrumented path is opt-in via the endpoint.
        if !crate::telemetry::enabled() {
            let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
            return self.tool_router.call(tcc).await;
        }
        let span = tool_span(&request, &context.extensions);
        async {
            let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
            let result = self.tool_router.call(tcc).await;
            record_outcome(&tracing::Span::current(), &result);
            result
        }
        .instrument(span)
        .await
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        let mut tools = self.tool_router.list_all();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(rmcp::model::ListToolsResult::with_all_items(tools))
    }

    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        self.tool_router.get(name).cloned()
    }
}

/// One span per MCP tool invocation, exported as `mcp.<tool>` with the turn token attached so
/// Tempo queries correlate broker spans with the turn's gateway spans even without a parent link.
/// Parenting, most specific first:
/// 1. the request's own `_meta` trace context, which the spec makes the source of truth;
/// 2. a W3C `traceparent` HTTP header from the client (rmcp injects the request's
///    `http::request::Parts` into the extensions), the mirror an intermediary can read;
/// 3. the engine-written PER-TURN traceparent file (`crate::turn::current_traceparent`, re-read
///    each call, the primary engine→broker channel, since this process outlives many turns);
/// 4. the `TRACEPARENT` process env, ONLY meaningful for a process whose whole lifetime is one
///    turn (a rendered turn pod); for the long-lived broker it would be turn-blind, which is why
///    the file wins.
///
/// No parent found = the span roots itself; a malformed value degrades the same way, never errors.
fn tool_span(
    request: &rmcp::model::CallToolRequestParams,
    extensions: &rmcp::model::Extensions,
) -> tracing::Span {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    let tool = request.name.as_ref();
    let span = tracing::info_span!(
        "mcp_tool",
        otel.name = format!("mcp.{tool}"),
        otel.kind = "server",
        otel.status_code = tracing::field::Empty,
        tool = %tool,
        turn = %crate::turn::current_token(),
        status = tracing::field::Empty,
        digest = tracing::field::Empty,
        tree_hash = tracing::field::Empty,
        mode = tracing::field::Empty,
        cached = tracing::field::Empty,
        job = tracing::field::Empty,
        queue = tracing::field::Empty,
        gpu_minutes = tracing::field::Empty,
    );
    if let Some(parent) = parent_context(request, extensions)
        && let Err(e) = span.set_parent(parent)
    {
        tracing::warn!(error = %e, "parenting the tool span on the traceparent failed");
    }
    span
}

/// The tool span's remote parent, walking the four sources in the order [`tool_span`] documents.
fn parent_context(
    request: &rmcp::model::CallToolRequestParams,
    extensions: &rmcp::model::Extensions,
) -> Option<opentelemetry::Context> {
    use rmcp::model::RequestParamsMeta as _;
    // `axum::http::request::Parts` must be the SAME `http` crate version rmcp inserts, or this
    // lookup silently misses and every client-parented span roots itself instead.
    let headers = extensions
        .get::<axum::http::request::Parts>()
        .map(|p| &p.headers);
    let header = |name: &str| {
        headers
            .and_then(|h| h.get(name))
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    extract_parent(
        request.traceparent().map(str::to_string),
        request.tracestate().map(str::to_string),
    )
    .or_else(|| extract_parent(header("traceparent"), header("tracestate")))
    .or_else(|| extract_parent(Some(crate::turn::current_traceparent()), None))
    .or_else(|| {
        extract_parent(
            std::env::var("TRACEPARENT").ok(),
            std::env::var("TRACESTATE").ok(),
        )
    })
}

/// The client's remote parent from W3C carrier values, or `None` when `traceparent` is absent/
/// blank/invalid (all-zeros). Pure; mirrors the engine's extraction.
fn extract_parent(
    traceparent: Option<String>,
    tracestate: Option<String>,
) -> Option<opentelemetry::Context> {
    use opentelemetry::propagation::TextMapPropagator as _;
    use opentelemetry::trace::TraceContextExt as _;
    use opentelemetry_sdk::propagation::TraceContextPropagator;

    let traceparent = traceparent.filter(|s| !s.trim().is_empty())?;
    let mut carrier = std::collections::HashMap::new();
    carrier.insert("traceparent".to_string(), traceparent);
    if let Some(ts) = tracestate.filter(|s| !s.trim().is_empty()) {
        carrier.insert("tracestate".to_string(), ts);
    }
    let cx = TraceContextPropagator::new().extract(&carrier);
    cx.span().span_context().is_valid().then_some(cx)
}

/// Enrich the tool span from the reply: every broker tool answers a JSON object with a `status`
/// tag, so the outcome (built/measured/rejected_kwarg/...) plus the proof-relevant fields ride the
/// span. An `error`/`job_failed` outcome (or a protocol error) marks the span ERROR for Tempo.
fn record_outcome(
    span: &tracing::Span,
    result: &Result<rmcp::model::CallToolResponse, rmcp::ErrorData>,
) {
    match result {
        Ok(rmcp::model::CallToolResponse::Complete(reply)) => {
            let Some(text) = reply.content.first().and_then(|c| c.as_text()) else {
                return;
            };
            for (key, value) in reply_attrs(&text.text) {
                span.record(key, value.as_str());
            }
        }
        Err(e) => {
            span.record("otel.status_code", "ERROR");
            span.record(
                "status",
                cap_attr(format!("protocol_error: {}", e.message)).as_str(),
            );
        }
        Ok(_) => {}
    }
}

/// Span-attribute value ceiling: outcome tags and digests are short; anything longer is a payload
/// that doesn't belong on a span.
const ATTR_VALUE_MAX: usize = 256;

/// Cap an attribute value at [`ATTR_VALUE_MAX`] characters (char-boundary safe).
fn cap_attr(s: String) -> String {
    if s.chars().count() <= ATTR_VALUE_MAX {
        s
    } else {
        s.chars().take(ATTR_VALUE_MAX).collect()
    }
}

/// The span-attribute allowlist pulled from a tool reply's JSON (pure). `status` carries the
/// outcome tag; the scalar proof fields (digest, tree_hash, mode, cached) follow when present; a
/// failure status adds the ERROR mark.
fn reply_attrs(text: &str) -> Vec<(&'static str, String)> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut push = |key: &'static str| {
        if let Some(s) = v.get(key).map(scalar_string).filter(|s| !s.is_empty()) {
            out.push((key, cap_attr(s)));
        }
    };
    push("status");
    push("digest");
    push("tree_hash");
    push("mode");
    push("cached");
    if matches!(
        v.get("status").and_then(serde_json::Value::as_str),
        Some("error") | Some("job_failed")
    ) {
        out.push(("otel.status_code", "ERROR".to_string()));
    }
    out
}

/// A JSON scalar as its attribute string; objects/arrays/null are dropped (empty).
fn scalar_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frozen() -> TraceParams {
        TraceParams {
            model: "claude-opus-4-6".into(),
            concurrency: 12,
            prefixes: 8,
            max_tokens: 8,
            long_tokens: 0,
            long_frac: 0.0,
        }
    }

    #[test]
    fn reply_attrs_extracts_the_allowlist_and_marks_failures() {
        // A successful build reply: status + the scalar proof fields.
        let attrs = reply_attrs(
            r#"{"status":"built","tree_hash":"abc123","digest":"ghcr.io/example/img@sha256:def","mode":"derive","cached":false,"log":"build-1.log"}"#,
        );
        let get = |k: &str| {
            attrs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("status"), Some("built"));
        assert_eq!(get("digest"), Some("ghcr.io/example/img@sha256:def"));
        assert_eq!(get("tree_hash"), Some("abc123"));
        assert_eq!(get("mode"), Some("derive"));
        assert_eq!(get("cached"), Some("false"));
        // Non-allowlisted keys never leak onto the span.
        assert_eq!(get("log"), None);
        assert_eq!(get("otel.status_code"), None, "built is not an error");

        // Failure statuses carry the ERROR mark; other outcomes (rejected_kwarg etc.) don't.
        let failed = reply_attrs(r#"{"status":"job_failed","reason":"crashed","logs":["x.log"]}"#);
        assert!(
            failed
                .iter()
                .any(|(k, v)| *k == "otel.status_code" && v == "ERROR")
        );
        let err = reply_attrs(r#"{"status":"error","error":"boom"}"#);
        assert!(
            err.iter()
                .any(|(k, v)| *k == "otel.status_code" && v == "ERROR")
        );
        let rejected = reply_attrs(r#"{"status":"rejected_kwarg","reason":"no"}"#);
        assert!(rejected.iter().all(|(k, _)| *k != "otel.status_code"));

        // Metrics objects are non-scalar and dropped; non-JSON text records nothing.
        let measured =
            reply_attrs(r#"{"status":"measured","metrics":{"tpot_ms":9.1},"cached":true}"#);
        let get = |k: &str| {
            measured
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("status"), Some("measured"));
        assert_eq!(get("cached"), Some("true"));
        assert!(reply_attrs("not json at all").is_empty());
    }

    #[test]
    fn attribute_values_are_length_capped() {
        let long = "x".repeat(1000);
        let capped = cap_attr(long);
        assert_eq!(capped.chars().count(), ATTR_VALUE_MAX);
        // A short value passes through untouched, and the cap is char-boundary safe.
        assert_eq!(cap_attr("built".into()), "built");
        let multibyte = "λ".repeat(300);
        assert_eq!(cap_attr(multibyte).chars().count(), ATTR_VALUE_MAX);
        // The cap applies to reply attrs end-to-end.
        let attrs = reply_attrs(&format!(r#"{{"status":"{}"}}"#, "s".repeat(500)));
        let status = &attrs.iter().find(|(k, _)| *k == "status").unwrap().1;
        assert_eq!(status.chars().count(), ATTR_VALUE_MAX);
    }

    #[test]
    fn trace_context_arrives_by_meta_and_by_header() {
        use opentelemetry::trace::TraceContextExt as _;
        use rmcp::model::RequestParamsMeta as _;

        let meta_id = "0af7651916cd43dd8448eb211c80319c";
        let header_id = "4bf92f3577b34da6a3ce929d0e0e4736";
        let trace_id = |cx: opentelemetry::Context| cx.span().span_context().trace_id().to_string();

        // A 2026-07-28 client puts trace context in the request's `_meta` (SEP-414).
        let mut request = rmcp::model::CallToolRequestParams::new("build_candidate");
        request.set_traceparent(&format!("00-{meta_id}-b7ad6b7169203331-01"));
        let empty = rmcp::model::Extensions::new();
        assert_eq!(
            parent_context(&request, &empty).map(trace_id).as_deref(),
            Some(meta_id)
        );

        // An older client mirrors it into the HTTP header, which rmcp hands us as `Parts`.
        let (parts, ()) = axum::http::Request::builder()
            .header("traceparent", format!("00-{header_id}-00f067aa0ba902b7-01"))
            .body(())
            .expect("building a request with a traceparent header")
            .into_parts();
        let mut extensions = rmcp::model::Extensions::new();
        extensions.insert(parts);
        let bare = rmcp::model::CallToolRequestParams::new("build_candidate");
        assert_eq!(
            parent_context(&bare, &extensions).map(trace_id).as_deref(),
            Some(header_id),
            "the header lookup missed — check that axum and rmcp agree on the `http` version"
        );

        // With both present the body wins: the spec makes it the source of truth and the header a
        // mirror an intermediary may have rewritten.
        assert_eq!(
            parent_context(&request, &extensions)
                .map(trace_id)
                .as_deref(),
            Some(meta_id)
        );
    }

    #[test]
    fn traceparent_extraction_parses_w3c_and_degrades_on_garbage() {
        use opentelemetry::trace::TraceContextExt as _;
        // A valid W3C traceparent (the engine-injected TRACEPARENT env / a client header) yields
        // a context whose trace_id matches the carrier value.
        let cx = extract_parent(
            Some("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".into()),
            None,
        )
        .expect("a valid traceparent must parse");
        assert_eq!(
            cx.span().span_context().trace_id().to_string(),
            "0af7651916cd43dd8448eb211c80319c"
        );
        assert!(cx.span().span_context().is_remote());

        // Malformed / blank / all-zeros values degrade to None (root spans), never an error.
        assert!(extract_parent(Some("garbage".into()), None).is_none());
        assert!(extract_parent(Some("  ".into()), None).is_none());
        assert!(extract_parent(None, Some("vendor=state".into())).is_none());
        assert!(
            extract_parent(
                Some("00-00000000000000000000000000000000-0000000000000000-01".into()),
                None
            )
            .is_none(),
            "an all-zeros context is invalid"
        );
    }

    /// The initialize handshake MUST advertise the `tools` capability, else a spec-compliant
    /// client connects but never calls `tools/list` and sees zero tools (the live Claude Code bug).
    /// Uses the always-miss [`crate::broker::NullResolver`]; the handshake doesn't care what the cache
    /// returns, only that a resolver is injected.
    #[test]
    fn get_info_advertises_tools_capability() {
        use crate::broker::NullResolver;
        use rmcp::ServerHandler;
        let info = McpServer::new(
            Box::new(NullResolver),
            frozen(),
            false,
            None,
            Arc::new(DeployRegistry::new()),
            Arc::new(CodegenState::new()),
        )
        .get_info();
        assert!(
            info.capabilities.tools.is_some(),
            "server must declare the tools capability so clients call tools/list"
        );
    }

    /// The distress tool must reach the agent's tool list with the guardrail intact: the
    /// description IS the policy that keeps `error` for genuinely doomed runs, and a required
    /// `severity` is what keeps a casual call from suspending.
    #[test]
    fn distress_is_listed_with_its_guardrail_and_required_severity() {
        use crate::broker::NullResolver;
        use rmcp::ServerHandler;
        let server = McpServer::new(
            Box::new(NullResolver),
            frozen(),
            false,
            None,
            Arc::new(DeployRegistry::new()),
            Arc::new(CodegenState::new()),
        );
        let tool = server
            .get_tool("distress")
            .expect("distress must be listed");
        let desc = tool.description.clone().unwrap_or_default();
        assert!(desc.contains("severity=error suspends the run"), "{desc}");
        assert!(desc.contains("not for hard-but-possible tasks"), "{desc}");
        let schema = serde_json::to_value(&tool.input_schema).unwrap();
        let required: Vec<String> = schema["required"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        assert!(required.contains(&"severity".to_string()), "{schema}");
        assert!(required.contains(&"reason".to_string()), "{schema}");
        assert!(
            !required.contains(&"evidence".to_string()),
            "evidence is optional: {schema}"
        );
    }

    #[test]
    fn report_is_zero_argument_and_explains_when_to_use_it() {
        use crate::broker::NullResolver;
        let server = McpServer::new(
            Box::new(NullResolver),
            frozen(),
            false,
            None,
            Arc::new(DeployRegistry::new()),
            Arc::new(CodegenState::new()),
        );
        let tool = server.get_tool("report").expect("report must be listed");
        let schema = serde_json::to_value(&tool.input_schema).unwrap();
        assert!(
            schema["properties"]
                .as_object()
                .is_none_or(|p| p.is_empty()),
            "{schema}"
        );
        let desc = tool.description.clone().unwrap_or_default();
        assert!(desc.contains("final reporting or epilogue task"), "{desc}");
        assert!(desc.contains("use distress"), "{desc}");
    }

    /// Regression guard for the stateless-mode bug: the http service builds a FRESH `McpServer` per
    /// request, so a deploy registered by the `deploy_composite` request must still be visible to the
    /// later `deploy_status` request. That only holds if both per-request servers share ONE registry
    /// injected from main rather than a per-instance one. Here: two servers from the same Arc, a deploy
    /// staged through the first, found through the second.
    #[test]
    fn per_request_servers_share_one_deploy_registry() {
        use crate::broker::NullResolver;
        let deploys = Arc::new(DeployRegistry::new());
        let factory = || {
            McpServer::new(
                Box::new(NullResolver),
                frozen(),
                false,
                None,
                deploys.clone(),
                Arc::new(CodegenState::new()),
            )
        };
        let server_a = factory();
        let server_b = factory();
        // A deploy id minted against the shared registry...
        let id = server_a.deploys.debug_register();
        // ...is resolvable through a DIFFERENT per-request server's registry.
        let status = crate::deploy::deploy_status(&server_b.deploys, &id, false, 0, 0);
        assert!(
            !status.contains(r#""status":"unknown""#),
            "a deploy registered on one per-request server must be visible on another: {status}"
        );
    }
}
