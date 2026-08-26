//! One openshell-sandboxed agent turn, driven over the gateway's gRPC API
//! ([`crate::openshell::grpc`]); the `openshell` CLI is used only for file upload/download.
//!
//! Per turn: ensure the gateway is up → mint a Vertex token and create/update the provider →
//! create the sandbox (attaching that provider) → apply the egress policy → upload the
//! workspace → targeted-upload each relayed cred to its sandbox path → upload the env script
//! and the prompt → optionally restore a private Claude session → exec claude (prompt over stdin), streaming its `stream-json` into the shared
//! [`crate::agent::StreamPump`] → sweep the sandbox log for egress denials
//! (blocked-connection attempts are turn telemetry) → download the workspace and private session back → delete
//! the sandbox (per-turn-fresh, so a discarded iteration leaves no residue).
//!
//! The sandbox name is derived per (process, workspace), so parallel wide-round candidates and parallel
//! crucible processes sharing one gateway never collide on a fixed name.

use crate::agent::{self, TurnFailure, TurnOutcome};
use crate::event::{AgentEvent, RawStream, cost_of, estimate_cost};
use crate::harness::{
    AuthProvider, HarnessRuntime, SandboxLayout, TranscriptLocator, TurnArtifacts,
};
use crate::manifest::Harness;
use crate::openshell::grpc::Gateway;
use crate::openshell::{gateway, grpc, policy, provider, sandbox};
use crate::{Args, Paths, relay};
use anyhow::{Context, Result};

/// Failures of the `openshell` CLI shell-outs and the private-transcript path checks. The
/// surrounding turn plumbing stays `anyhow`; these are the checks this module owns.
#[derive(Debug, thiserror::Error)]
pub enum OpenshellCliError {
    #[error("openshell {label} cancelled (interrupt)")]
    Cancelled { label: String },
    #[error("openshell {label} failed: {stderr}")]
    Failed { label: String, stderr: String },
    #[error(
        "the agent exited {code} without producing a verdict; its stderr is replayed above (the \
         sandbox wrapper cds, sources the env script, then redirects the prompt, so a non-zero \
         exit can mean any of those failed before the agent ran)"
    )]
    AgentExit { code: i32 },
    #[error(
        "the sandbox exec stream broke before the agent reported an exit ({code:?}: {message}); \
         whatever the turn had already done is neither complete nor reportable"
    )]
    ExecStreamBroke { code: tonic::Code, message: String },
    #[error("private session locator is outside Claude's pinned config directory")]
    LocatorOutsideConfigDir,
    #[error("Claude transcript path does not match the admitted session id")]
    TranscriptSessionMismatch,
    #[error(
        "workspace retrieval failed after bounded retry; sandbox '{sandbox}' was preserved for operator recovery: {detail}"
    )]
    WorkspaceRecovery { sandbox: String, detail: String },
}
use crucible_harness::OtelCollector;
use std::sync::atomic::Ordering;
use tokio::fs;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Run one openshell turn. Mirrors `agent::run_turn`'s contract: drives `sink` per output
/// line/event and returns the turn's [`TurnOutcome`]. Any orchestration failure surfaces as an
/// [`AgentEvent::Error`] through the sink and lands in the outcome's
/// [`TurnFailure::Orchestration`], carrying whatever the turn had already spent.
pub fn turn(
    args: &Args,
    p: &Paths,
    prompt: &str,
    json: bool,
    session: Option<&crate::agent_session::SessionTurn>,
    mut sink: impl FnMut(&str, RawStream, Option<&AgentEvent>),
) -> TurnOutcome {
    // The whole turn is async; the engine runtime drives it. The one `block_on` in the openshell
    // path, reached from the published handle rather than threaded through `run_turn` and the
    // reporter trait (see `crate::engine`). A missing runtime surfaces like any orchestration
    // failure (through the sink, a $0 no-op turn).
    let handle = match crate::engine::handle() {
        Ok(h) => h,
        Err(e) => {
            let message = format!("{e:#}");
            let ev = AgentEvent::Error {
                error_type: "openshell".into(),
                message: message.clone(),
            };
            sink("", RawStream::Stderr, Some(&ev));
            return TurnOutcome::failed(0.0, TurnFailure::Orchestration(message));
        }
    };
    let spent = CostMeter::default();
    match handle.block_on(try_turn(args, p, prompt, json, session, &spent, &mut sink)) {
        Ok(cost) => TurnOutcome::completed(cost),
        Err(e) => {
            let message = format!("{e:#}");
            let ev = AgentEvent::Error {
                error_type: "openshell".into(),
                message: message.clone(),
            };
            sink("", RawStream::Stderr, Some(&ev));
            TurnOutcome::failed(spent.get(), TurnFailure::Orchestration(message))
        }
    }
}

/// The turn's cost as it becomes known, readable after a later step's error has unwound the flow.
/// The agent can exit having billed real tokens and the workspace download can still fail; without
/// this the turn would report $0.
#[derive(Default)]
struct CostMeter(std::cell::Cell<f64>);

impl CostMeter {
    fn raise(&self, cost: f64) {
        self.0.set(self.0.get().max(cost));
    }

    fn get(&self) -> f64 {
        self.0.get()
    }
}

/// Aborts the wrapped task on drop. Guards the STOP→cancel bridge so it stops when the turn ends
/// (normally or via `?`), instead of leaking a task that keeps polling `crate::STOP`.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Spawn the per-turn Ctrl-C bridge: poll [`crate::STOP`] every 50ms and trip `cancel` when it is
/// set, so the exec stream (and the cancellable upload/download children) unwind promptly. STOP
/// stays the single source of truth the loop driver already owns; this only translates it into the
/// token the async I/O `select!`s on. The returned guard aborts the task at turn end.
fn spawn_stop_bridge(cancel: CancellationToken) -> AbortOnDrop {
    AbortOnDrop(tokio::spawn(async move {
        while !crate::STOP.load(Ordering::SeqCst) {
            if cancel.is_cancelled() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        cancel.cancel();
    }))
}

/// Announce one orchestration stage through the sink as a `Log { level: "stage" }` event. The
/// slow steps below (`run_os` is silent on success) were invisible otherwise, most painfully the
/// sandbox create, whose in-pod podman pull of the sandbox image can run 15+ minutes with zero
/// output. The console renders it as a detail line; the scope activity feed relays it to pod stdout.
fn stage(sink: &mut impl FnMut(&str, RawStream, Option<&AgentEvent>), msg: &str) {
    // A parallel tracing event on the turn's span (exported over OTLP when enabled); NOT a second
    // copy on the human channel, the sink line below is the console/session narration.
    tracing::info!(stage = msg, "openshell stage");
    let ev = AgentEvent::Log {
        level: "stage".to_string(),
        label: "openshell".to_string(),
        value: Some(msg.to_string()),
    };
    sink(msg, RawStream::Stderr, Some(&ev));
}

const DEFAULT_CODEX_API_KEY_ENV: &str = "OPENAI_API_KEY";

/// Resolve the API-key half of Codex's dual auth. `None` means the caller should use the ChatGPT
/// OAuth mint. The explicit modes never silently cross over; only `auto` falls back.
fn selected_codex_api_key(cfg: &crate::manifest::CodexCfg) -> Result<Option<String>> {
    use crate::manifest::CodexAuthMode;

    if cfg.auth == CodexAuthMode::Chatgpt {
        return Ok(None);
    }
    let env_name = cfg
        .api_key_env
        .as_deref()
        .unwrap_or(DEFAULT_CODEX_API_KEY_ENV);
    anyhow::ensure!(
        !env_name.is_empty() && !env_name.contains(['=', '\0']),
        "[agent.codex].api_key_env is not a valid environment variable name"
    );
    let key = std::env::var(env_name)
        .ok()
        .filter(|value| !value.trim().is_empty());
    match cfg.auth {
        CodexAuthMode::Auto => Ok(key),
        CodexAuthMode::Api => key.map(Some).with_context(|| {
            format!(
                "{env_name} unset or empty; [agent.codex].auth = \"api\" requires its selected API key"
            )
        }),
        CodexAuthMode::Chatgpt => unreachable!("returned before reading the API-key environment"),
    }
}

#[tracing::instrument(
    name = "openshell_turn",
    skip_all,
    fields(
        backend = "openshell",
        workspace = %p.workspace.display(),
        model = %args.model,
        sandbox = tracing::field::Empty,
        // Queryable without waiting for the run/iteration ancestors to export (long spans only
        // reach the backend when they END): which logical session and turn this is.
        session = session.map(|s| s.logical_name.as_str()).unwrap_or(""),
        turn = session.map(|s| s.completed_turns + 1).unwrap_or(1),
    )
)]
async fn try_turn(
    args: &Args,
    p: &Paths,
    prompt: &str,
    json: bool,
    session: Option<&crate::agent_session::SessionTurn>,
    spent: &CostMeter,
    sink: &mut impl FnMut(&str, RawStream, Option<&AgentEvent>),
) -> Result<f64> {
    // Ctrl-C plumbing for the whole turn: STOP → this token, tripped by a bridge task; the exec
    // stream and the upload/download children `select!` it. The guard aborts the bridge at turn
    // end (including any `?` bail below).
    let cancel = CancellationToken::new();
    let _bridge = spawn_stop_bridge(cancel.clone());

    // The agent harness for this turn (claude default): argv grammar, env script, seed files,
    // stream decoder, and the post-turn transcript contract all come from here.
    let harness = args.harness();

    // 1. Gateway up (idempotent, boots on the first turn, no-ops after). `None` = the
    //    default supervisor emulator image; the *agent* image is `--from` on create below.
    stage(sink, "starting the openshell gateway");
    let version_warning = gateway::ensure_running(args.compute_driver, None)
        .await
        .context("ensuring the openshell gateway is up")?;
    // A degraded version check (rev mismatch, unparseable version) is turn telemetry, not a
    // failure, too-old already errored above.
    if let Some(warning) = version_warning {
        let ev = AgentEvent::Raw {
            text: warning.clone(),
            stream: RawStream::Stderr,
        };
        sink(&warning, RawStream::Stderr, Some(&ev));
    }
    // The gateway is up, open the typed gRPC client for the rest of the turn (control plane;
    // file transfer stays on the CLI). Constructed in this runtime context (`connect_lazy`).
    let gw = Gateway::connect().context("connecting to the openshell gateway over gRPC")?;

    // 2. Resolve this harness's model credential: Vertex's static credential becomes a gateway
    //    provider the metadata emulator serves to claude/hermes (see `provider` docs); codex gets
    //    the auth mode selected by `[agent.codex]`: an API key from the named env or a
    //    host-refreshed ChatGPT OAuth token. Either is seeded as auth.json (step 7b), since Codex
    //    reads the real bytes off disk and its L4 WebSocket never crosses a placeholder-resolving
    //    proxy hop.
    let codex_auth = match harness.auth_provider() {
        AuthProvider::Vertex => {
            let token = provider::mint_vertex_token()
                .await
                .context("minting the Vertex access token")?;
            let (project, region) = vertex_config(&args.env);
            ensure_provider(&gw, &token, &project, &region).await?;
            None
        }
        AuthProvider::Codex => match selected_codex_api_key(&args.codex)? {
            Some(key) => Some(provider::CodexAuth::ApiKey(key)),
            None => Some(provider::CodexAuth::ChatGpt(
                provider::mint_codex_token()
                    .await
                    .context("minting the codex ChatGPT access token")?,
            )),
        },
    };

    // 2b. Sandbox S3 reads (the read half of the S3 role split): a gateway-minted `aws-s3`
    //     provider signs sandbox S3 egress at the proxy via a read-only role assumed with the
    //     loop pod's projected web-identity token. Gated on the rendered env, absent, no
    //     provider and no S3 signing.
    let aws_provider = match std::env::var("CRUCIBLE_AWS_SANDBOX_ROLE_ARN") {
        Ok(arn) if !arn.trim().is_empty() => {
            ensure_aws_provider(&gw, arn.trim()).await?;
            true
        }
        _ => false,
    };

    // 2c. Broker token hardening: the per-run bearer token becomes a static credential on the
    //     `crucible-broker` provider, and the sandbox's `.mcp.json` carries only the
    //     `openshell:resolve:env:` placeholder. The proxy resolves it at egress, solely for the
    //     policy endpoint bound to this provider (the step-4 `credential_binding`), so the real
    //     token never enters the sandbox and a leaked placeholder resolves nowhere else.
    let broker_provider = match args.broker_token.as_deref() {
        Some(token) if args.broker.enabled => {
            ensure_broker_provider(&gw, token).await?;
            true
        }
        _ => false,
    };

    // 3. Create the sandbox, attaching the managed provider. Best-effort delete first: a prior
    //    turn whose create failed (e.g. ContainerExited mid-provision) leaves the name behind,
    //    and a bare create then fails "already exists" for the rest of the run. Clearing it
    //    makes create idempotent. Labels make the sandbox discoverable via `list --selector`.
    let name = sandbox::name_for(&p.workspace);
    tracing::Span::current().record("sandbox", name.as_str());
    let basename = workdir_basename(p)?;
    let _ = gw.delete_sandbox(&name).await;
    // Deletion is async; creating against a still-terminating CR fails "already exists".
    gw.wait_deleted(&name)
        .await
        .context("clearing a stale sandbox before create")?;
    stage(
        sink,
        &format!(
            "creating sandbox from {} (pulls the image on first use, up to {}s before the wait gives up)",
            args.sandbox_image.as_deref().unwrap_or("the default image"),
            grpc::pull_timeout().as_secs()
        ),
    );
    let labels = [
        ("crucible-pid".to_string(), std::process::id().to_string()),
        ("crucible-workspace".to_string(), label_value(&basename)),
    ];
    let mut providers = match harness.auth_provider() {
        AuthProvider::Vertex => vec![provider::PROVIDER_NAME.to_string()],
        AuthProvider::Codex => Vec::new(),
    };
    if aws_provider {
        providers.push(provider::AWS_PROVIDER_NAME.to_string());
    }
    if broker_provider {
        providers.push(provider::BROKER_PROVIDER_NAME.to_string());
    }
    gw.create_sandbox(
        &name,
        args.sandbox_image.as_deref(),
        &providers,
        &labels,
        &args.openshell.read_only_paths,
    )
    .await
    .context("creating the openshell sandbox")?;

    // 3b. Hand the broker this turn's boundary: a fresh token at `<storage>/turn-token`. The
    //     broker's per-turn candidate budget resets when the value changes. Best-effort, on a
    //     host without the forge storage dir (a laptop run) the budget just stays uncapped.
    //     Alongside it, this turn's W3C traceparent at `<storage>/turn-traceparent`: the broker is
    //     run-lifetime, so it reads the per-turn parent from this file fresh per call (a static pod
    //     env would collapse every turn's broker spans onto the first turn's trace).
    if args.broker.enabled {
        if let Err(e) = write_turn_token().await {
            let ev = AgentEvent::Raw {
                text: format!("turn-token write failed (candidate budget stays uncapped): {e:#}"),
                stream: RawStream::Stderr,
            };
            sink("", RawStream::Stderr, Some(&ev));
        }
        if let Err(e) =
            write_traceparent_files(&forge::storage_root(), crate::engine::current_trace_env())
                .await
        {
            let ev = AgentEvent::Raw {
                text: format!("turn-traceparent write failed (broker spans self-root): {e:#}"),
                stream: RawStream::Stderr,
            };
            sink("", RawStream::Stderr, Some(&ev));
        }
    }

    // In-process OTLP collector for the sandboxed turn: binds `0.0.0.0` on the turn pod so the
    // sandbox reaches it over the driver-resolved host (same egress pattern as the broker). The
    // port is fixed because the loop pod's deny-ingress NetworkPolicy names sandbox-reachable
    // ports explicitly; an OS-assigned port would be silently dropped there. Opt-in via
    // `CRUCIBLE_OTEL`; a bind failure degrades to telemetry-off (no `otel_summary`, the
    // pricing-table estimate stays the cost fallback). Its `otel.jsonl` is the `otel-log` Tier 2
    // artifact.
    let collector = if harness.otel_capable() && agent::otel_enabled(args) {
        // The agent's exporter can't attribute its spans to a loop turn; the forwarder stamps
        // session + turn as resource attributes at the same boundary where it re-parents.
        let forward = agent::otel_forward(args).map(|f| {
            let (name, turn) = session
                .map(|s| (s.logical_name.clone(), s.completed_turns + 1))
                .unwrap_or((String::new(), 1));
            f.with_attrs(vec![
                ("crucible.session".into(), name),
                ("crucible.turn".into(), turn.to_string()),
            ])
        });
        match OtelCollector::start(
            p.state.join("otel.jsonl"),
            "0.0.0.0",
            crate::openshell::gateway::OTEL_COLLECTOR_PORT,
            forward,
        ) {
            Ok(c) => Some(c),
            Err(e) => {
                let ev = AgentEvent::Raw {
                    text: format!("otel collector unavailable (cost falls back to estimate): {e}"),
                    stream: RawStream::Stderr,
                };
                sink("", RawStream::Stderr, Some(&ev));
                None
            }
        }
    } else {
        None
    };
    let meters = collector.as_ref().map(OtelCollector::meters);

    // Best-effort sandbox teardown from here on, so a mid-turn failure still cleans up.
    let result: Result<f64> = async {
        // 4. Egress policy (deny-by-default; merge domain extras over the built-ins). The
        //    broker endpoint is auto-appended when the broker is enabled, and
        //    the collector's egress rule is appended when on.
        let driver = args.compute_driver;
        let broker_url = if args.broker.enabled {
            Some(crate::manifest::resolve_broker_url(
                &args.broker,
                driver.broker_host(),
            ))
        } else {
            None
        };
        let broker_ep = match &broker_url {
            Some(url) => Some(
                crate::manifest::broker_endpoint_from_url(url)
                    .context("deriving broker egress endpoint from resolved URL")?,
            ),
            None => None,
        };
        let mut endpoints = policy::resolve_endpoints(
            &args.openshell,
            &harness.default_endpoints(),
            broker_ep.as_deref(),
        );
        if let Some(c) = &collector {
            endpoints.push(c.sandbox_egress(driver.broker_host()));
        }
        let mut credential_bindings: Vec<grpc::EndpointCredentialBinding> = broker_ep
            .as_deref()
            .filter(|_| broker_provider)
            .and_then(|ep| {
                let mut it = ep.split(':');
                let host = it.next()?.to_string();
                let port: u32 = it.next()?.parse().ok()?;
                Some(grpc::EndpointCredentialBinding {
                    host,
                    port,
                    provider: provider::BROKER_PROVIDER_NAME.to_string(),
                })
            })
            .into_iter()
            .collect();
        if harness.auth_provider() == AuthProvider::Vertex {
            credential_bindings.extend(policy::VERTEX_CREDENTIAL_HOSTS.iter().map(|host| {
                grpc::EndpointCredentialBinding {
                    host: (*host).to_string(),
                    port: 443,
                    provider: provider::PROVIDER_NAME.to_string(),
                }
            }));
        }
        gw.update_policy_wait(
            &name,
            &policy::resolve_binaries(&args.openshell, harness.default_binaries()),
            &endpoints,
            &credential_bindings,
        )
        .await
        .context("applying the sandbox egress policy")?;

        // 5. Upload the workspace (dotfiles included), landing at /sandbox/<basename>.
        stage(sink, "uploading the workspace into the sandbox");
        let ws = p.workspace.to_string_lossy().to_string();
        run_os(
            &sandbox::workdir_upload_args(&name, &ws),
            "workdir upload",
            &cancel,
        )
        .await?;

        // 6. Targeted-upload each relayed cred to its sandbox path (never via the repo).
        for rf in &args.relay {
            let content = relay::render(&p.workspace, rf)
                .with_context(|| format!("rendering relay `{}`", rf.dest))?;
            let tmp = write_temp("cred", &content).await?;
            run_os(
                &sandbox::file_upload_args(&name, &tmp.path().to_string_lossy(), &rf.dest),
                &format!("cred upload {}", rf.dest),
                &cancel,
            )
            .await?;
        }

        // 7. Upload the env script and the prompt to absolute /tmp paths. The collector's OTEL_*
        //    matrix appends after the manifest env (so a later `export` wins), pointing the agent's
        //    exporter at the driver-resolved host.
        let mut turn_env = args.env.clone();
        if let Some(c) = &collector {
            turn_env.extend(crucible_harness::otel_env(
                &c.sandbox_endpoint(driver.broker_host()),
            ));
        }
        let env_tmp = write_temp("env", &harness.env_script(&turn_env)).await?;
        run_os(
            &sandbox::file_upload_args(
                &name,
                &env_tmp.path().to_string_lossy(),
                SandboxLayout::ENV_SCRIPT,
            ),
            "env upload",
            &cancel,
        )
        .await?;
        let prompt_tmp = write_temp("prompt", prompt).await?;
        run_os(
            &sandbox::file_upload_args(
                &name,
                &prompt_tmp.path().to_string_lossy(),
                SandboxLayout::PROMPT,
            ),
            "prompt upload",
            &cancel,
        )
        .await?;

        // 7b. Seed the harness's pre-exec files (claude: `.mcp.json` toward the provisioning
        //     broker, loaded via `--mcp-config`). `broker_url` was already resolved above
        //     (step 4) so the URL and egress entry agree.
        let seed_token = if broker_provider {
            Some(provider::broker_token_placeholder())
        } else {
            args.broker_token.clone()
        };
        let seeds = harness.seed_files(
            args,
            broker_url.as_deref(),
            seed_token.as_deref(),
            codex_auth.as_ref(),
        );
        for seed in &seeds {
            let seed_tmp = write_temp("seed", &seed.content).await?;
            run_os(
                &sandbox::file_upload_args(&name, &seed_tmp.path().to_string_lossy(), seed.dest),
                &format!("seed upload {}", seed.dest),
                &cancel,
            )
            .await?;
        }

        // The sandbox itself is intentionally per-turn-fresh. A continuing Claude solver gets
        // only its private native transcript restored outside the uploaded workspace; world state
        // still comes exclusively from Crucible's retain/restore decision.
        if let Some(session) = session
            && session.is_resume()
        {
            restore_private_session(p, session, &gw, &name, &cancel).await?;
        }

        // 8. Exec the agent (prompt over stdin), streaming its stdout through the harness decoder.
        stage(sink, "sandbox ready — starting the agent");
        let argv = match session {
            Some(session) => harness
                .sandbox_session_argv(args, !seeds.is_empty(), session)
                .context("building continuing sandbox harness argv")?,
            None => harness.sandbox_argv(args, !seeds.is_empty()),
        };
        let wrapper = crate::harness::exec_wrapper(&basename, &argv);
        let exec_opts = ExecOpts {
            model: &args.model,
            json,
            cancel: &cancel,
        };
        // A deep self-check turn outlives the ~1h Vertex token (the agent waits on many
        // multi-minute GPU jobs), so re-mint into the provider slot while claude runs, its
        // google-auth re-queries the metadata emulator as the cached token nears expiry. Codex has
        // no mid-turn refresher: API keys do not expire during a turn, and the OAuth access token
        // is fixed in the seeded auth file; refreshing it here could not update the sandbox.
        let refresher = match harness.auth_provider() {
            AuthProvider::Vertex => Some(tokio::spawn({
                let gw = gw.clone();
                async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(25 * 60)).await;
                        match provider::mint_vertex_token().await {
                            Ok(t) => {
                                if let Err(e) = gw
                                    .update_provider(
                                        provider::PROVIDER_NAME,
                                        provider::CRED_KEY,
                                        &t,
                                    )
                                    .await
                                {
                                    tracing::warn!("mid-turn Vertex token refresh failed: {e:#}");
                                }
                            }
                            Err(e) => tracing::warn!("mid-turn Vertex token mint failed: {e:#}"),
                        }
                    }
                }
            })),
            AuthProvider::Codex => None,
        };
        let decoder = harness.decoder(args, meters.as_ref(), crate::agent::tool_io_full(args));
        let exec_result =
            exec_and_stream(&gw, &name, &wrapper, decoder, &exec_opts, spent, sink).await;
        if let Some(refresher) = &refresher {
            refresher.abort();
        }
        // An exec that failed is exactly when the sandbox's own account of itself matters, and it
        // is about to be deleted. Replay it before unwinding: a turn that exits non-zero having
        // written nothing to stderr is otherwise indistinguishable from one that never started.
        if exec_result.is_err() {
            replay_sandbox_log(&gw, &name, LogReplay::Tail, sink).await;
        }
        let mut cost = exec_result?;
        spent.raise(cost);

        // 8a. Agent exited: roll the captured OTLP jsonl up into the `otel_summary` event
        //     (authoritative cost, per-model usage, API latency). `cost_of`'s prefer-OTEL rule
        //     lifts the turn's cost onto the authoritative number.
        if let Some(c) = &collector
            && let Some(summary) = c.summary()
        {
            let ev = summary.to_event();
            cost = cost.max(cost_of(&ev).unwrap_or(0.0));
            spent.raise(cost);
            sink("", RawStream::Stdout, Some(&ev));
        }

        // 8b. Sweep the sandbox log for egress denials before teardown. An agent probing
        //     blocked endpoints mid-turn is signal (reward-hacking telemetry), so denials
        //     land in the run log. Best-effort: a logs failure never fails the turn.
        replay_sandbox_log(&gw, &name, LogReplay::DenialsOnly, sink).await;

        // 9. Download the workspace back (the agent's edits round-trip to the host).
        stage(sink, "agent turn done — downloading the workspace");
        let sandbox_workdir = format!("{}/{basename}", SandboxLayout::HOME);
        retrieve_workspace(&name, &sandbox_workdir, p.workspace.as_path(), &cancel).await?;

        // Save the updated native transcript before telemetry parsing and teardown. It is private
        // 0600 runtime state, never appended to session.jsonl or published with the run record.
        if let Some(session) = session {
            persist_private_session(p, session, &gw, &name, &cancel).await?;
        }

        // The post-turn transcript fetch: pure telemetry garnish for claude (export-gated), but
        // a backfill harness's ONLY source of result events + cost, so for those it runs
        // unconditionally and a failure is loud (see `graft_turn_telemetry`).
        if harness.backfill_required() || crate::engine::TurnExport::resolve().emits_anything() {
            cost = graft_turn_telemetry(harness, &gw, &name, &cancel, cost, sink).await;
            spent.raise(cost);
        }
        Ok(cost)
    }
    .await;

    // 10. Per-turn-fresh: delete the sandbox unless retrieval failed and it is the recovery copy.
    //     Clear the
    //     turn-token too: an empty token means uncapped, so the engine's own post-turn broker
    //     calls (the gate's rungs) never inherit a turn budget the agent already spent.
    if !matches!(&result, Err(e) if e.downcast_ref::<OpenshellCliError>().is_some_and(|e| matches!(e, OpenshellCliError::WorkspaceRecovery { .. })))
    {
        let _ = gw.delete_sandbox(&name).await;
    }
    let _ = fs::write(forge::storage_root().join("turn-token"), "").await;
    result
}

/// Fetch the sandbox's recent supervisor log lines and replay every policy-denial through the
/// sink (the network proxy logs blocked connections / SSRF / L7 rejections). Best-effort by
/// design: denial telemetry must never fail or block the turn, a logs RPC failure yields an
/// empty set. Classification is structured ([`grpc::is_denial`]).
/// How many trailing sandbox log lines a failed exec replays. Enough to carry a stack trace or an
/// OOM notice, bounded so a chatty sandbox cannot flood the turn's own log.
const SANDBOX_LOG_TAIL: usize = 40;

/// What the sandbox's own log is being read for.
enum LogReplay {
    /// A healthy turn: only the blocked-egress lines, which are turn telemetry (an agent probing
    /// what it cannot reach is signal).
    DenialsOnly,
    /// A failed turn: the tail, whatever it says. The agent's stderr is empty in the cases that
    /// matter most — a process killed outright, a wrapper that died before exec — and this is the
    /// only other place its last words exist.
    Tail,
}

/// Replay the sandbox's own log into the turn's stream as typed events, carrying each line's
/// level, target, fields, and whether the engine read it as a denial. Best-effort: a logs failure
/// never fails the turn.
async fn replay_sandbox_log(
    gw: &Gateway,
    name: &str,
    what: LogReplay,
    sink: &mut impl FnMut(&str, RawStream, Option<&AgentEvent>),
) {
    let lines = gw.sandbox_logs(name).await;
    let skip = match what {
        LogReplay::DenialsOnly => 0,
        LogReplay::Tail => lines.len().saturating_sub(SANDBOX_LOG_TAIL),
    };
    for line in lines.iter().skip(skip) {
        let denial = grpc::is_denial(line);
        if matches!(what, LogReplay::DenialsOnly) && !denial {
            continue;
        }
        let ev = AgentEvent::SandboxLog {
            ts_ms: line.timestamp_ms,
            level: line.level.clone(),
            target: line.target.clone(),
            message: line.message.trim_end().to_string(),
            denial,
            fields: line.fields.clone().into_iter().collect(),
        };
        sink(line.message.trim_end(), RawStream::Stderr, Some(&ev));
    }
}

/// Probe the provider (`GetProvider`) → create on the first turn, update (swap the token)
/// thereafter. The token rides the request body, never an argv or a log line.
async fn ensure_provider(gw: &Gateway, token: &str, project: &str, region: &str) -> Result<()> {
    if gw.provider_exists(provider::PROVIDER_NAME).await {
        gw.update_provider(provider::PROVIDER_NAME, provider::CRED_KEY, token)
            .await
            .context("updating the Vertex provider token")
    } else {
        gw.create_provider(
            provider::PROVIDER_NAME,
            provider::CRED_KEY,
            token,
            provider::PROVIDER_TYPE,
            project,
            region,
        )
        .await
        .context("creating the Vertex provider")
    }
}

/// Idempotent AWS provider setup: create the `aws-s3` provider if absent, (re)configure its
/// web-identity STS refresh, then rotate once so the credentials exist BEFORE the sandbox's
/// first request, the proxy fails closed on unminted credentials and the refresh worker's
/// tick is up to 60s away. Re-configuring each turn keeps a rotated role ARN current.
/// Import the endpointless broker profile and set this run's token on the `crucible-broker`
/// provider, update-or-create like the Vertex provider: the token changes every run, the
/// provider object survives across runs on a shared gateway.
async fn ensure_broker_provider(gw: &Gateway, token: &str) -> Result<()> {
    gw.import_provider_profile(provider::broker_profile())
        .await
        .context("importing the broker provider profile")?;
    if gw.provider_exists(provider::BROKER_PROVIDER_NAME).await {
        gw.update_provider(
            provider::BROKER_PROVIDER_NAME,
            provider::BROKER_CRED_KEY,
            token,
        )
        .await
        .context("updating the broker provider token")
    } else {
        gw.create_static_provider(
            provider::BROKER_PROVIDER_NAME,
            provider::BROKER_CRED_KEY,
            token,
            provider::BROKER_PROFILE_ID,
        )
        .await
        .context("creating the broker provider")
    }
}

async fn ensure_aws_provider(gw: &Gateway, role_arn: &str) -> Result<()> {
    use openshell_core::proto::ProviderCredentialRefreshStrategy;
    // The STS refresh strategy is gated behind the gateway's providers-v2 global setting
    // (default off). Idempotent flip; this gateway is crucible's own, booted per pod.
    gw.set_global_bool_setting("providers_v2_enabled", true)
        .await
        .context("enabling providers_v2 on the gateway")?;
    if !gw.provider_exists(provider::AWS_PROVIDER_NAME).await {
        gw.create_minted_provider(provider::AWS_PROVIDER_NAME, provider::AWS_PROVIDER_TYPE)
            .await
            .context("creating the aws-s3 provider")?;
    }
    let token_file = std::env::var("CRUCIBLE_AWS_SANDBOX_TOKEN_FILE")
        .unwrap_or_else(|_| "/var/run/secrets/aws/token".to_string());
    let mut material = std::collections::HashMap::from([
        ("role_arn".to_string(), role_arn.to_string()),
        ("web_identity_token_file".to_string(), token_file),
        ("session_name".to_string(), "crucible-sandbox".to_string()),
    ]);
    if let Ok(region) = std::env::var("CRUCIBLE_AWS_SANDBOX_REGION")
        && !region.trim().is_empty()
    {
        material.insert("aws_region".to_string(), region.trim().to_string());
    }
    gw.configure_provider_refresh(
        provider::AWS_PROVIDER_NAME,
        provider::AWS_PRIMARY_CRED,
        ProviderCredentialRefreshStrategy::AwsStsAssumeRole,
        material,
    )
    .await
    .context("configuring the aws-s3 web-identity refresh")?;
    gw.rotate_provider_credential(provider::AWS_PROVIDER_NAME, provider::AWS_PRIMARY_CRED)
        .await
        .context("pre-warming the aws-s3 credentials")
}

/// The exec turn's knobs, bundled so [`exec_and_stream`] keeps a small signature: the pricing
/// model for the token-estimate fallback, the front-end mode, and the turn's cancellation token.
struct ExecOpts<'a> {
    model: &'a str,
    json: bool,
    cancel: &'a CancellationToken,
}

/// Exec the agent over the gateway's `ExecSandbox` stream, driving a [`agent::StreamPump`] with the
/// stdout lines the stream yields, and return the turn's cost (estimated from tokens if none was
/// reported). The stream's stderr is replayed through the sink after the agent exits, mirroring the
/// old CLI-child behavior. `cancel` unwinds the exec on Ctrl-C (see [`Gateway::exec`]).
async fn exec_and_stream(
    gw: &Gateway,
    name: &str,
    command: &[String],
    decoder: crate::harness::StreamDecoder,
    opts: &ExecOpts<'_>,
    spent: &CostMeter,
    sink: &mut impl FnMut(&str, RawStream, Option<&AgentEvent>),
) -> Result<f64> {
    let mut pump = agent::StreamPump::new(decoder);
    let exec = gw
        .exec(name, command, opts.cancel, |line| {
            pump.push(line, opts.json, sink)
        })
        .await?;
    let (mut cost, best_tokens) = pump.finish();
    let (exit_code, transport_error) = (exec.exit_code, exec.transport_error);

    if cost == 0.0
        && let Some(t) = &best_tokens
    {
        cost = estimate_cost(opts.model, t);
    }
    // Bank it here, not at the caller's `?`: the agent can bill real tokens and still exit
    // non-zero below, and a turn that spent money is never reported free.
    spent.raise(cost);

    // Replay the agent's stderr (provisioning notes / errors) through the sink as raw lines.
    for line in exec.stderr_lines {
        let t = line.trim();
        if !t.is_empty() {
            let ev = AgentEvent::Raw {
                text: t.to_string(),
                stream: RawStream::Stderr,
            };
            sink(&line, RawStream::Stderr, Some(&ev));
        }
    }
    // The wrapper `cd`s, sources the env script, then redirects the prompt into the agent. Any of
    // those failing exits non-zero having written nothing to stdout, which is indistinguishable
    // from a turn that ran and produced no verdict unless the status is checked.
    if let Some(code) = exit_code
        && code != 0
    {
        return Err(OpenshellCliError::AgentExit { code }.into());
    }
    if let Some((code, message)) = transport_error {
        return Err(OpenshellCliError::ExecStreamBroke { code, message }.into());
    }
    Ok(cost)
}

/// Run an `openshell` command (file upload/download, the transfers that stay on the CLI) as a
/// cancellable async child, bailing with its stderr on failure. Silent on success. `kill_on_drop`
/// + the `cancel` arm make it cancellable,
/// so a Ctrl-C mid-upload kills the child and surfaces an interrupt error.
#[tracing::instrument(name = "openshell_cli", skip_all, fields(label = label))]
async fn run_os(args: &[String], label: &str, cancel: &CancellationToken) -> Result<()> {
    let mut cmd = Command::new("openshell");
    cmd.args(args).kill_on_drop(true);
    tokio::select! {
        _ = cancel.cancelled() => Err(OpenshellCliError::Cancelled { label: label.to_owned() }.into()),
        out = cmd.output() => {
            let out = out.with_context(|| format!("exec `openshell {label}`"))?;
            if !out.status.success() {
                return Err(OpenshellCliError::Failed {
                    label: label.to_owned(),
                    stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
                }
                .into());
            }
            Ok(())
        }
    }
}

/// Download into an empty sibling directory, then atomically publish the complete candidate.
/// This avoids tar hardlink collisions with pre-existing Cargo outputs and prevents a failed or
/// interrupted transfer from exposing a half-synchronized workspace to the judge.
async fn retrieve_workspace(
    sandbox_name: &str,
    remote: &str,
    workspace: &std::path::Path,
    cancel: &CancellationToken,
) -> Result<()> {
    let parent = workspace.parent().context("workspace has no parent")?;
    let mut last_error = None;
    for attempt in 1..=2 {
        let staged = tempfile::Builder::new()
            .prefix(".crucible-retrieval-")
            .tempdir_in(parent)
            .context("creating workspace retrieval staging directory")?;
        match run_os(
            &sandbox::download_args(sandbox_name, remote, &staged.path().to_string_lossy()),
            "workdir download",
            cancel,
        )
        .await
        {
            Ok(()) => {
                return publish_workspace(staged.path(), workspace)
                    .await
                    .map_err(|e| OpenshellCliError::WorkspaceRecovery {
                        sandbox: sandbox_name.to_string(),
                        detail: format!("atomic publication: {e:#}"),
                    })
                    .map_err(Into::into);
            }
            Err(e) => last_error = Some(format!("attempt {attempt}: {e:#}")),
        }
    }
    Err(OpenshellCliError::WorkspaceRecovery {
        sandbox: sandbox_name.to_string(),
        detail: last_error.unwrap_or_else(|| "download did not run".to_string()),
    }
    .into())
}

async fn publish_workspace(staged: &std::path::Path, workspace: &std::path::Path) -> Result<()> {
    let parent = workspace.parent().context("workspace has no parent")?;
    let backup = parent.join(format!(
        ".crucible-replaced-{}",
        uuid::Uuid::now_v7().simple()
    ));
    fs::rename(workspace, &backup)
        .await
        .context("moving the previous workspace aside")?;
    if let Err(e) = fs::rename(staged, workspace).await {
        let rollback = fs::rename(&backup, workspace).await;
        return Err(e).context(match rollback {
            Ok(()) => "publishing retrieved workspace (previous workspace restored)".to_string(),
            Err(r) => format!("publishing retrieved workspace; rollback also failed: {r}"),
        });
    }
    if let Err(e) = fs::remove_dir_all(&backup).await {
        tracing::warn!(path = %backup.display(), error = %e, "retrieved workspace published; old workspace cleanup deferred");
    }
    Ok(())
}

/// The Vertex project + region for the provider config, read from the manifest env,
/// with sane fallbacks.
fn vertex_config(env: &[(String, String)]) -> (String, String) {
    let get = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| env.iter().find(|(ek, _)| ek == k).map(|(_, v)| v.clone()))
    };
    let project = get(&["ANTHROPIC_VERTEX_PROJECT_ID", "GCP_PROJECT_ID"]).unwrap_or_default();
    let region = get(&["CLOUD_ML_REGION", "VERTEX_LOCATION"]).unwrap_or_else(|| "global".into());
    (project, region)
}

/// Write this turn's boundary token where the broker's candidate budget reads it
/// (`<forge storage>/turn-token`). pid + wall-clock nanos: unique per turn without global state.
async fn write_turn_token() -> Result<()> {
    let dir = forge::storage_root();
    fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("creating forge storage dir {}", dir.display()))?;
    let path = dir.join("turn-token");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    fs::write(&path, format!("{}-{nanos}", std::process::id()))
        .await
        .with_context(|| format!("writing turn token {}", path.display()))
}

/// Write this turn's W3C traceparent (and tracestate) into `dir` (`<forge storage>` in practice),
/// where the run-lifetime broker reads it fresh per call so broker spans graft under THIS turn's
/// root span. A not-recording turn (`env` = `None`) REMOVES both files: leaving the previous turn's
/// traceparent in place would parent this turn's broker calls onto the old trace. A turn without a
/// tracestate likewise clears the stale one. `dir` is a parameter so the contract is unit-testable.
async fn write_traceparent_files(
    dir: &std::path::Path,
    env: Option<(String, Option<String>)>,
) -> Result<()> {
    let tp_path = dir.join("turn-traceparent");
    let ts_path = dir.join("turn-tracestate");
    let Some((traceparent, tracestate)) = env else {
        let _ = fs::remove_file(&tp_path).await;
        let _ = fs::remove_file(&ts_path).await;
        return Ok(());
    };
    fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating forge storage dir {}", dir.display()))?;
    fs::write(&tp_path, traceparent)
        .await
        .with_context(|| format!("writing turn traceparent {}", tp_path.display()))?;
    match tracestate {
        Some(ts) => fs::write(&ts_path, ts)
            .await
            .with_context(|| format!("writing turn tracestate {}", ts_path.display()))?,
        None => {
            let _ = fs::remove_file(&ts_path).await;
        }
    }
    Ok(())
}

/// Pull the agent's transcript back once, graft tool spans + (opt-in) content logs onto the turn
/// span, and (for a backfill harness) fold the transcript's events + cost onto the turn.
/// Best-effort for claude (telemetry must never wedge the turn); loud for a backfill harness,
/// whose transcript is the turn's ONLY source of result + cost: a fetch failure emits
/// [`AgentEvent::Error`] rather than letting the turn pass as a silent $0 success. Returns the
/// turn's (possibly raised) cost.
async fn graft_turn_telemetry(
    harness: Harness,
    gw: &Gateway,
    name: &str,
    cancel: &CancellationToken,
    cost: f64,
    sink: &mut impl FnMut(&str, RawStream, Option<&AgentEvent>),
) -> f64 {
    let fetched = tokio::time::timeout(
        harness.transcript_fetch_timeout(),
        fetch_transcript(harness, gw, name, cancel),
    )
    .await;
    let Ok(Some(bytes)) = fetched else {
        if harness.backfill_required() {
            let ev = AgentEvent::Error {
                error_type: "transcript".into(),
                message: "transcript fetch failed — the turn's result and cost are unavailable"
                    .into(),
            };
            sink("", RawStream::Stderr, Some(&ev));
        }
        return cost;
    };

    let export = crate::engine::TurnExport::resolve();
    // One infallible parse over the in-memory transcript feeds span export and (backfill
    // harnesses) the turn's result; skip it when nothing consumes it.
    let artifacts = if export.spans() || harness.backfill_required() {
        harness.parse_transcript(&bytes)
    } else {
        TurnArtifacts::default()
    };
    if export.emits_anything() {
        let span = tracing::Span::current();
        if export.spans() {
            crate::engine::synthesize_tool_spans(
                &span,
                &artifacts.tool_calls,
                std::time::SystemTime::now(),
            );
        }
        if export.content() {
            // Content logs come from the harness's own transcript reader (claude maps its jsonl;
            // hermes: Phase B), off the same in-memory bytes, no re-read.
            let records = harness.content_records(&bytes);
            crate::engine::emit_conversation_logs(&span, &records);
        }
    }

    let mut cost = cost;
    if harness.backfill_required() {
        for ev in &artifacts.events {
            sink("", RawStream::Stdout, Some(ev));
        }
        if let Some(c) = artifacts.cost_usd {
            cost = cost.max(c);
        }
    }
    cost
}

const PRIVATE_SESSION_DIR: &str = "agent-session-private";

fn private_session_paths(
    p: &Paths,
    session: &crate::agent_session::SessionTurn,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = p.state.join(PRIVATE_SESSION_DIR).join(&session.provider_id);
    (root.join("locator"), root.join("transcript.jsonl"))
}

fn valid_private_remote(remote: &str, session: &crate::agent_session::SessionTurn) -> bool {
    remote.starts_with(&format!("{}/", crate::harness::claude::CLAUDE_PROJECTS))
        && remote.ends_with(&format!("/{}.jsonl", session.provider_id))
        && !remote.contains("..")
        && std::path::Path::new(remote).is_absolute()
}

/// Restore one Claude Code transcript into a new sandbox at the exact private path where the
/// previous sandbox wrote it. The locator is validated against the pinned config root and opaque
/// session UUID before it is used as an upload target.
async fn restore_private_session(
    p: &Paths,
    session: &crate::agent_session::SessionTurn,
    gw: &Gateway,
    name: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    let (locator_path, transcript_path) = private_session_paths(p, session);
    let remote = fs::read_to_string(&locator_path)
        .await
        .with_context(|| format!("reading private session locator {}", locator_path.display()))?;
    let remote = remote.trim();
    if !valid_private_remote(remote, session) {
        return Err(OpenshellCliError::LocatorOutsideConfigDir.into());
    }
    let parent = std::path::Path::new(remote)
        .parent()
        .context("private session locator has no parent")?
        .to_string_lossy()
        .to_string();
    gw.exec(
        name,
        &["mkdir".to_string(), "-p".to_string(), parent],
        cancel,
        |_| {},
    )
    .await
    .context("creating Claude's private session directory")?;
    run_os(
        &sandbox::file_upload_args(name, &transcript_path.to_string_lossy(), remote),
        "private session restore",
        cancel,
    )
    .await
}

/// Persist the updated Claude transcript outside the disposable sandbox. This is deliberately a
/// private engine file, mode 0600, and is never part of the public session-event/publish contract.
async fn persist_private_session(
    p: &Paths,
    session: &crate::agent_session::SessionTurn,
    gw: &Gateway,
    name: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    let remote = newest_transcript_path(
        gw,
        name,
        crate::harness::claude::CLAUDE_PROJECTS,
        crate::harness::claude::TRANSCRIPT_GLOB,
        cancel,
    )
    .await
    .context("Claude turn produced no resumable private transcript")?;
    if !valid_private_remote(&remote, session) {
        return Err(OpenshellCliError::TranscriptSessionMismatch.into());
    }

    let scratch = tempfile::tempdir().context("creating private session download scratch")?;
    run_os(
        &sandbox::download_args(name, &remote, &scratch.path().to_string_lossy()),
        "private session save",
        cancel,
    )
    .await?;
    let downloaded = newest_jsonl(scratch.path())
        .await
        .context("private session download contained no transcript")?;
    let bytes = fs::read(&downloaded)
        .await
        .context("reading downloaded private session transcript")?;

    let (locator_path, transcript_path) = private_session_paths(p, session);
    let root = locator_path
        .parent()
        .context("private session destination has no parent")?;
    fs::create_dir_all(root)
        .await
        .with_context(|| format!("creating private session directory {}", root.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).await?;
    }
    let locator_tmp = root.join(".locator.tmp");
    let transcript_tmp = root.join(".transcript.tmp");
    fs::write(&locator_tmp, remote.as_bytes()).await?;
    fs::write(&transcript_tmp, bytes).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&locator_tmp, std::fs::Permissions::from_mode(0o600)).await?;
        fs::set_permissions(&transcript_tmp, std::fs::Permissions::from_mode(0o600)).await?;
    }
    fs::rename(&locator_tmp, &locator_path).await?;
    fs::rename(&transcript_tmp, &transcript_path).await?;
    Ok(())
}

/// Locate the harness's transcript IN the sandbox, download just that one file (not a whole
/// directory, whose size is unbounded), and read it into memory ONCE (async, a long session's
/// jsonl runs to a few MB, a backfill db likewise). Returns the raw bytes; `None` on any failure.
/// The bytes are harness-neutral: claude's UTF-8 jsonl and hermes's binary `state.db` both ride
/// back as bytes, each harness's parse decoding its own format.
async fn fetch_transcript(
    harness: Harness,
    gw: &Gateway,
    name: &str,
    cancel: &CancellationToken,
) -> Option<Vec<u8>> {
    let locator = harness.transcript_locator();
    let remote = match &locator {
        TranscriptLocator::NewestJsonl { sandbox_root, glob } => {
            newest_transcript_path(gw, name, sandbox_root, glob, cancel).await?
        }
        TranscriptLocator::File { sandbox_path } => (*sandbox_path).to_string(),
    };
    let scratch = tempfile::tempdir().ok()?;
    run_os(
        &sandbox::download_args(name, &remote, &scratch.path().to_string_lossy()),
        "transcript download",
        cancel,
    )
    .await
    .ok()?;
    // The CLI may land the file at the local path itself or inside it as a dir entry; find it
    // either way.
    let local = match &locator {
        TranscriptLocator::NewestJsonl { .. } => newest_jsonl(scratch.path()).await?,
        TranscriptLocator::File { .. } => {
            let base = std::path::Path::new(&remote).file_name()?;
            let candidate = scratch.path().join(base);
            candidate.exists().then_some(candidate)?
        }
    };
    fs::read(&local).await.ok()
}

/// The sandbox-side path of the newest session transcript (claude writes one per session under
/// `projects/<slug>/`, codex one per session under `sessions/YYYY/MM/DD/`), or `None` when there
/// is none. `glob` is the harness's path shape below `sandbox_root` (see [`TranscriptLocator`]).
/// The returned path is sanity-checked to sit under the pinned config dir before anything
/// downloads it.
async fn newest_transcript_path(
    gw: &Gateway,
    name: &str,
    sandbox_root: &str,
    glob: &str,
    cancel: &CancellationToken,
) -> Option<String> {
    let script = newest_jsonl_script(sandbox_root, glob);
    let command = vec!["bash".to_string(), "-lc".to_string(), script];
    let mut found: Option<String> = None;
    gw.exec(name, &command, cancel, |line| {
        let t = line.trim();
        if found.is_none() && !t.is_empty() {
            found = Some(t.to_string());
        }
    })
    .await
    .ok()?;
    found.filter(|p| p.starts_with(sandbox_root) && p.ends_with(".jsonl") && !p.contains(".."))
}

/// The in-sandbox shell that names the newest transcript, newest-mtime first.
fn newest_jsonl_script(sandbox_root: &str, glob: &str) -> String {
    format!("ls -1t {sandbox_root}/{glob} 2>/dev/null | head -n 1")
}

/// The `*.jsonl` under `root` (recursively) with the newest mtime, the transcript of the session
/// that just ran. A fresh sandbox per turn means one file, but newest-wins is robust to residue;
/// `max_by_key` keeps the last on an mtime tie, so stale copies never shadow a fresh one.
async fn newest_jsonl(root: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut dated: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    for path in jsonl_files_under(root).await {
        dated.push((mtime_or_epoch(&path).await, path));
    }
    // Last-on-tie: a stale copy never shadows a fresh one written in the same mtime granularity.
    dated.into_iter().max_by_key(|(t, _)| *t).map(|(_, p)| p)
}

/// Every `*.jsonl` under `root`, depth-first; a dir that can't be read is skipped, not fatal. The
/// stack (not recursion) keeps this a flat async fn, no boxed futures for the self-call.
async fn jsonl_files_under(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(mut entries) = fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            match entry.file_type().await {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(_) if path.extension().is_some_and(|e| e == "jsonl") => files.push(path),
                _ => {}
            }
        }
    }
    files
}

/// A path's mtime, or the epoch when it can't be read (an unreadable file sorts oldest).
async fn mtime_or_epoch(path: &std::path::Path) -> std::time::SystemTime {
    fs::metadata(path)
        .await
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

/// The basename the workspace uploads under (`/sandbox/<basename>`).
fn workdir_basename(p: &Paths) -> Result<String> {
    p.workspace
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .context("workspace path has no final component")
}

/// A Kubernetes label value for `raw`: the label charset only, at most 63 characters, and an
/// alphanumeric at both ends. An isolated task's workspace basename is `task-` plus a full
/// sha256, which the gateway rejects verbatim.
fn label_value(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .take(63)
        .collect();
    cleaned
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_string()
}

/// Write `content` to a `tempfile` guard (unique name, 0600 on unix, unlinked on drop), the
/// rendered cred / env / prompt / mcp-config staged for upload. `tempfile` is a blocking crate, so
/// the create+write runs on a blocking thread rather than stalling the runtime.
async fn write_temp(tag: &str, content: &str) -> Result<tempfile::NamedTempFile> {
    let tag = tag.to_owned();
    let content = content.to_owned();
    tokio::task::spawn_blocking(move || -> Result<tempfile::NamedTempFile> {
        use std::io::Write;
        let mut f = tempfile::Builder::new()
            .prefix(&format!("crucible-{tag}-"))
            .tempfile()
            .with_context(|| format!("creating {tag} temp file"))?;
        f.write_all(content.as_bytes())
            .with_context(|| format!("writing {tag} temp file"))?;
        Ok(f)
    })
    .await
    .context("temp-file task panicked")?
}

/// The Vertex keys a manifest-less turn (`rank-grounded`, scope-propose) relays from its own
/// process env into the agent env: the claude switches plus every alias `run::vertex_config`
/// honors. A domain loop gets these from `[agent].env` (the manifest validates them); a bare
/// `git clone` turn has no manifest, so the turn pod's plain env (the deploy profile's `[env]`)
/// carries them and this relay is the explicit bridge. `vertex_config`/`env_script` stay
/// manifest-only, they never read the process env themselves.
const VERTEX_RELAY_KEYS: &[&str] = &[
    "CLAUDE_CODE_USE_VERTEX",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "CLOUD_ML_REGION",
    "GCP_PROJECT_ID",
    "VERTEX_LOCATION",
];

/// Seed `env` with the process values of [`VERTEX_RELAY_KEYS`]: only keys that are set and
/// non-empty relay, and an existing entry (a manifest-provided value) always wins.
pub fn relay_vertex_env(env: &mut Vec<(String, String)>) {
    for key in VERTEX_RELAY_KEYS {
        if env.iter().any(|(k, _)| k == key) {
            continue;
        }
        if let Ok(v) = std::env::var(key)
            && !v.is_empty()
        {
            env.push((key.to_string(), v));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_auth_selection_can_switch_between_named_keys_and_chatgpt() {
        let _guard = crate::test_env_lock();
        const KEY_ENV: &str = "CRUCIBLE_TEST_OPENAI_KEY_WORK";
        unsafe { std::env::set_var(KEY_ENV, "sk-work") };

        let mut cfg = crate::manifest::CodexCfg {
            auth: crate::manifest::CodexAuthMode::Api,
            api_key_env: Some(KEY_ENV.to_string()),
            ..Default::default()
        };
        assert_eq!(
            selected_codex_api_key(&cfg).unwrap().as_deref(),
            Some("sk-work")
        );

        cfg.auth = crate::manifest::CodexAuthMode::Auto;
        assert_eq!(
            selected_codex_api_key(&cfg).unwrap().as_deref(),
            Some("sk-work")
        );

        cfg.auth = crate::manifest::CodexAuthMode::Chatgpt;
        assert_eq!(selected_codex_api_key(&cfg).unwrap(), None);
        unsafe { std::env::remove_var(KEY_ENV) };

        cfg.auth = crate::manifest::CodexAuthMode::Auto;
        assert_eq!(selected_codex_api_key(&cfg).unwrap(), None);
    }

    #[test]
    fn explicit_api_auth_never_silently_falls_back_to_chatgpt() {
        let _guard = crate::test_env_lock();
        const KEY_ENV: &str = "CRUCIBLE_TEST_OPENAI_KEY_MISSING";
        unsafe { std::env::remove_var(KEY_ENV) };
        let cfg = crate::manifest::CodexCfg {
            auth: crate::manifest::CodexAuthMode::Api,
            api_key_env: Some(KEY_ENV.to_string()),
            ..Default::default()
        };
        let err = selected_codex_api_key(&cfg).expect_err("explicit API mode needs its key");
        assert!(err.to_string().contains(KEY_ENV));
    }

    /// A non-zero agent exit names the code and says the wrapper may have failed before the agent
    /// ran, rather than being reported as a turn that produced no verdict.
    #[test]
    fn a_non_zero_agent_exit_is_its_own_error() {
        let e = OpenshellCliError::AgentExit { code: 127 };
        let msg = e.to_string();
        assert!(
            msg.contains("127"),
            "the exit code must reach the operator: {msg}"
        );
        assert!(
            msg.contains("without producing a verdict"),
            "must distinguish itself from a parse failure: {msg}"
        );
    }

    /// The meter reports what a turn spent even when the step that would have returned the cost
    /// unwinds instead. This is the shape of the live failure: the agent billed real tokens and
    /// exited non-zero, and the turn reported $0.
    #[test]
    fn a_turn_that_billed_before_it_failed_is_not_reported_free() {
        let spent = CostMeter::default();
        // What `exec_and_stream` does once the pump has finished, before it inspects the exit code.
        spent.raise(0.20629125);
        let outcome = TurnOutcome::failed(
            spent.get(),
            TurnFailure::Orchestration(OpenshellCliError::AgentExit { code: 1 }.to_string()),
        );
        assert_eq!(outcome.cost_usd, 0.20629125);
        assert!(outcome.failure.is_some());
    }

    /// `raise` is a high-water mark, so banking the cost early and again at the end cannot
    /// double-count it, and a later smaller sample cannot lower it.
    #[test]
    fn the_meter_only_ever_rises() {
        let spent = CostMeter::default();
        spent.raise(0.25);
        spent.raise(0.25);
        assert_eq!(spent.get(), 0.25);
        spent.raise(0.10);
        assert_eq!(spent.get(), 0.25, "a smaller later sample never lowers it");
        spent.raise(0.40);
        assert_eq!(spent.get(), 0.40, "the OTEL rollup can only raise it");
    }

    /// A broken exec stream is not a finished turn. Before this it reported no exit code, and
    /// "no exit code" read as a clean one.
    #[test]
    fn a_broken_exec_stream_is_its_own_error() {
        let e = OpenshellCliError::ExecStreamBroke {
            code: tonic::Code::Unavailable,
            message: "broken pipe".to_string(),
        };
        let msg = e.to_string();
        assert!(
            msg.contains("Unavailable"),
            "the code is the classifiable part: {msg}"
        );
        assert!(msg.contains("broken pipe"), "{msg}");
        assert!(msg.contains("before the agent reported an exit"), "{msg}");
    }

    /// The locator's glob reaches each harness's transcript at its real depth: claude one segment
    /// below the projects root, codex three below the sessions root.
    #[test]
    fn the_newest_jsonl_script_matches_each_harness_tree() {
        for harness in [Harness::Claude, Harness::Codex] {
            let TranscriptLocator::NewestJsonl { sandbox_root, glob } =
                harness.transcript_locator()
            else {
                panic!("{harness:?} reads a jsonl tree");
            };
            let script = newest_jsonl_script(sandbox_root, glob);
            assert!(script.starts_with("ls -1t "), "{script}");
            assert!(script.ends_with(" 2>/dev/null | head -n 1"), "{script}");
            let pattern = script
                .trim_start_matches("ls -1t ")
                .trim_end_matches(" 2>/dev/null | head -n 1");
            assert_eq!(pattern, format!("{sandbox_root}/{glob}"));
        }
        assert_eq!(
            newest_jsonl_script(
                crate::harness::claude::CLAUDE_PROJECTS,
                crate::harness::claude::TRANSCRIPT_GLOB
            ),
            "ls -1t /sandbox/.claude/projects/*/*.jsonl 2>/dev/null | head -n 1"
        );
        assert_eq!(
            newest_jsonl_script(
                crate::harness::codex::SESSIONS,
                crate::harness::codex::TRANSCRIPT_GLOB
            ),
            "ls -1t /sandbox/.codex/sessions/*/*/*/rollout-*.jsonl 2>/dev/null | head -n 1"
        );
    }

    #[test]
    fn vertex_config_reads_manifest_keys_with_fallback() {
        let (proj, region) = vertex_config(&[
            ("ANTHROPIC_VERTEX_PROJECT_ID".into(), "proj-x".into()),
            ("CLOUD_ML_REGION".into(), "us-east5".into()),
        ]);
        assert_eq!(proj, "proj-x");
        assert_eq!(region, "us-east5");
        // Region falls back to "global" when unset.
        let (_, region2) = vertex_config(&[]);
        assert_eq!(region2, "global");
    }

    #[test]
    fn private_session_locator_is_pinned_to_claude_and_the_opaque_id() {
        let session = crate::agent_session::SessionTurn {
            logical_name: "solver".into(),
            provider_id: "018f47a0-0000-7000-8000-000000000000".into(),
            completed_turns: 1,
        };
        assert!(valid_private_remote(
            "/sandbox/.claude/projects/-sandbox-work/018f47a0-0000-7000-8000-000000000000.jsonl",
            &session
        ));
        assert!(!valid_private_remote(
            "/sandbox/.claude/projects/-sandbox-work/other.jsonl",
            &session
        ));
        assert!(!valid_private_remote(
            "/sandbox/.claude/projects/../../etc/018f47a0-0000-7000-8000-000000000000.jsonl",
            &session
        ));
        assert!(!valid_private_remote(
            "/tmp/018f47a0-0000-7000-8000-000000000000.jsonl",
            &session
        ));
        assert!(!valid_private_remote(
            "/sandbox/.claude/projects-evil/018f47a0-0000-7000-8000-000000000000.jsonl",
            &session
        ));
    }

    /// A non-recording turn CLEARS the per-turn traceparent channel, a stale file would parent
    /// the broker's calls onto the previous turn's trace.
    #[tokio::test]
    async fn traceparent_files_written_then_cleared_when_not_recording() {
        let dir = std::env::temp_dir().join(format!("turn-tp-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir).await;
        let tp = dir.join("turn-traceparent");
        let ts = dir.join("turn-tracestate");

        // Recording turn: both files land.
        write_traceparent_files(
            &dir,
            Some(("00-aaaa-bbbb-01".to_string(), Some("vendor=1".to_string()))),
        )
        .await
        .unwrap();
        assert_eq!(fs::read_to_string(&tp).await.unwrap(), "00-aaaa-bbbb-01");
        assert_eq!(fs::read_to_string(&ts).await.unwrap(), "vendor=1");

        // Recording turn without a tracestate: the stale tracestate clears.
        write_traceparent_files(&dir, Some(("00-cccc-dddd-01".to_string(), None)))
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(&tp).await.unwrap(), "00-cccc-dddd-01");
        assert!(!ts.exists(), "stale tracestate cleared");

        // Not-recording turn: BOTH files clear (never inherit the previous turn's trace).
        write_traceparent_files(&dir, None).await.unwrap();
        assert!(!tp.exists(), "stale traceparent cleared");
        assert!(!ts.exists());
        let _ = fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn label_values_fit_the_kubernetes_limit_and_charset() {
        let digest = "4c2b536afd886a46bac8276a9c5e03370c3d715adc8332ffbfc3b575cc5bc6ab";
        let long = label_value(&format!("task-{digest}"));
        assert_eq!(long.len(), 63);
        assert!(long.starts_with("task-4c2b536a"));
        assert_eq!(label_value("my workspace/v2"), "my-workspace-v2");
        assert_eq!(label_value("-.trimmed.-"), "trimmed");
        assert_eq!(label_value("vllm"), "vllm");
    }

    fn clear_relay_keys() {
        for k in VERTEX_RELAY_KEYS {
            unsafe {
                std::env::remove_var(k);
            }
        }
    }

    #[test]
    fn relay_copies_set_nonempty_keys_and_skips_empty_and_unset() {
        let _guard = crate::test_env_lock();
        clear_relay_keys();
        unsafe {
            std::env::set_var("CLAUDE_CODE_USE_VERTEX", "1");
        }
        unsafe {
            std::env::set_var("ANTHROPIC_VERTEX_PROJECT_ID", "proj-x");
        }
        unsafe {
            std::env::set_var("CLOUD_ML_REGION", "");
        }

        let mut env = Vec::new();
        relay_vertex_env(&mut env);
        clear_relay_keys();

        assert_eq!(
            env,
            vec![
                ("CLAUDE_CODE_USE_VERTEX".to_string(), "1".to_string()),
                (
                    "ANTHROPIC_VERTEX_PROJECT_ID".to_string(),
                    "proj-x".to_string()
                ),
            ]
        );
    }

    #[test]
    fn manifest_provided_values_win_over_the_process_env() {
        let _guard = crate::test_env_lock();
        clear_relay_keys();
        unsafe {
            std::env::set_var("ANTHROPIC_VERTEX_PROJECT_ID", "from-process");
        }
        unsafe {
            std::env::set_var("VERTEX_LOCATION", "us-east5");
        }

        let mut env = vec![(
            "ANTHROPIC_VERTEX_PROJECT_ID".to_string(),
            "from-manifest".to_string(),
        )];
        relay_vertex_env(&mut env);
        clear_relay_keys();

        assert_eq!(
            env,
            vec![
                (
                    "ANTHROPIC_VERTEX_PROJECT_ID".to_string(),
                    "from-manifest".to_string()
                ),
                ("VERTEX_LOCATION".to_string(), "us-east5".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn staged_publication_replaces_a_colliding_cargo_hardlink_tree_atomically() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("candidate");
        let staged = root.path().join("staged");
        fs::create_dir_all(workspace.join("target/debug/build/pkg/out"))
            .await
            .unwrap();
        fs::create_dir_all(staged.join("target/debug/build/pkg/out"))
            .await
            .unwrap();
        let collision = "target/debug/build/pkg/out/build-script-build";
        fs::write(workspace.join(collision), b"stale host output")
            .await
            .unwrap();
        let staged_source = staged.join("target/debug/build/pkg/out/build-script-source");
        fs::write(&staged_source, b"paid candidate output")
            .await
            .unwrap();
        fs::hard_link(&staged_source, staged.join(collision))
            .await
            .unwrap();
        fs::write(staged.join("agent-edit.rs"), b"complete edit")
            .await
            .unwrap();

        publish_workspace(&staged, &workspace).await.unwrap();

        assert_eq!(
            fs::read(workspace.join(collision)).await.unwrap(),
            b"paid candidate output"
        );
        assert_eq!(
            fs::read(workspace.join("agent-edit.rs")).await.unwrap(),
            b"complete edit"
        );
        assert!(!staged.exists());
    }
}
