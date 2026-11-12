//! One openshell-sandboxed agent turn, driven over the gateway's gRPC API
//! ([`crate::openshell::grpc`]); the `openshell` CLI is used only for file upload/download.
//!
//! Per turn: ensure the gateway is up → mint a Vertex token and create/update the provider →
//! create the sandbox (attaching that provider) → apply the egress policy → upload the
//! workspace → targeted-upload each relayed cred to its sandbox path → upload the env script
//! and the prompt → optionally restore a private Claude session → exec claude (prompt over stdin), streaming its `stream-json` into the shared
//! [`crate::agent::turn::StreamPump`] → sweep the sandbox log for egress denials
//! (blocked-connection attempts are turn telemetry) → download the workspace and private session back → delete
//! the sandbox (per-turn-fresh, so a discarded iteration leaves no residue).
//!
//! The sandbox name is derived per (process, workspace), so parallel wide-round candidates and parallel
//! crucible processes sharing one gateway never collide on a fixed name.

use crate::agent::event::{AgentEvent, RawStream};
use crate::agent::harness::{McpServer, SandboxAuth, SandboxLayout, TurnArtifacts, backend};
use crate::agent::relay;
use crate::agent::sandbox_env::{EnvSource, SandboxEnv};
use crate::agent::tool_chain::StopReason;
use crate::agent::turn::{TurnFailure, TurnOutcome};
use crate::args::{Paths, SandboxTurn};
use crate::knobs::Harness;
use crate::openshell::grpc::Gateway;
use crate::openshell::{gateway, grpc, policy, provider, sandbox};
use anyhow::{Context, Result};
use crucible::harness::{ApiFamily, AuthProvider, TranscriptLocator};
use crucible::openshell::endpoint::Endpoint;
use crucible_contract::TransportCause;
use crucible_harness::redact::Redactor;
use std::time::Instant;

/// Which step of the turn an orchestration error came from, read off the typed errors in its
/// chain. The gateway is checked first: a sandbox call that failed because the gateway was down
/// carries both, and the gateway is the thing to fix.
pub(crate) fn transport_cause(e: &anyhow::Error) -> TransportCause {
    if e.chain()
        .any(|c| c.downcast_ref::<gateway::GatewayError>().is_some())
    {
        return TransportCause::Gateway;
    }
    if e.chain()
        .any(|c| c.downcast_ref::<grpc::GrpcError>().is_some())
    {
        return TransportCause::Sandbox;
    }
    match e
        .chain()
        .find_map(|c| c.downcast_ref::<OpenshellCliError>())
    {
        Some(OpenshellCliError::AgentExit { .. } | OpenshellCliError::ExecStreamBroke { .. }) => {
            TransportCause::Agent
        }
        _ => TransportCause::Other,
    }
}

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
    #[error(
        "the {harness} harness authenticates with a direct API key, and neither OPENAI_API_KEY \
         nor ANTHROPIC_API_KEY is set"
    )]
    NoApiKey { harness: &'static str },
}

/// Why a turn's model API key cannot be bound to its endpoint. The egress proxy injects the key,
/// so the endpoint has to be an allowlisted rule whose TLS the proxy terminates.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum InferenceKeyError {
    #[error(
        "the inference endpoint {endpoint} is not in the sandbox egress allowlist; its API key is \
         injected only on an allowlisted endpoint (add {endpoint} to [agent.openshell].endpoints)"
    )]
    NotAllowlisted { endpoint: String },
    #[error(
        "the inference endpoint {endpoint} is in [agent.openshell].tls_skip_endpoints; the egress \
         proxy injects its API key, which needs the TLS it skips (remove {endpoint} from the list)"
    )]
    TlsSkipped { endpoint: String },
}
use crucible_harness::otel::OtelCollector;
use std::sync::atomic::Ordering;
use tokio::fs;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Run one openshell turn. Mirrors `agent::run_turn`'s contract: drives `sink` per output
/// line/event and returns the turn's [`TurnOutcome`]. Any orchestration failure surfaces as an
/// [`AgentEvent::Error`] through the sink and lands in the outcome's
/// [`TurnFailure::Orchestration`], carrying whatever the turn had already spent. Every secret the
/// turn hands the sandbox is masked out of what reaches `sink` and the outcome.
pub fn turn(
    turn: &SandboxTurn<'_>,
    p: &Paths,
    prompt: &str,
    json: bool,
    session: Option<&crate::agent::agent_session::SessionTurn>,
    deadline: Option<crucible::deadline::Deadline>,
    mut sink: impl FnMut(&str, RawStream, Option<&AgentEvent>),
) -> TurnOutcome {
    let redactor = Redactor::with_rules(turn.secret_rules.clone());
    let mut sink = |line: &str, stream: RawStream, ev: Option<&AgentEvent>| {
        let ev = ev.map(|ev| redactor.event(ev));
        sink(&redactor.text(line), stream, ev.as_deref());
    };
    // The whole turn is async; the engine runtime drives it. The one `block_on` in the openshell
    // path. A missing runtime surfaces like any orchestration failure (through the sink, a $0
    // no-op turn).
    let handle = match turn.engine {
        Some(h) => h,
        None => {
            let message = "the engine runtime is not initialized (EngineCtx::new was not called \
                           before an async call site) — this is an engine wiring bug"
                .to_string();
            let ev = AgentEvent::Error {
                error_type: "openshell".into(),
                message: message.clone(),
            };
            sink("", RawStream::Stderr, Some(&ev));
            return TurnOutcome::failed(
                0.0,
                TurnFailure::Orchestration {
                    cause: TransportCause::Other,
                    message,
                },
            );
        }
    };
    let spent = CostMeter::default();
    let at = deadline.map(|d| d.at);
    let outcome = handle.block_on(try_turn(
        turn, p, prompt, json, session, at, &spent, &redactor, &mut sink,
    ));
    if outcome.is_err()
        && let Some(deadline) = deadline
        && Instant::now() >= deadline.at
    {
        return TurnOutcome::failed(spent.get(), TurnFailure::DeadlineExceeded(deadline));
    }
    match outcome {
        Ok(cost) => TurnOutcome::completed(cost),
        Err(e) => {
            let failure = masked(failure_of(e), &redactor);
            if let TurnFailure::Orchestration { message, .. } = &failure {
                let ev = AgentEvent::Error {
                    error_type: "openshell".into(),
                    message: message.clone(),
                };
                sink("", RawStream::Stderr, Some(&ev));
            }
            TurnOutcome::failed(spent.get(), failure)
        }
    }
}

/// What a turn that unwound with `e` failed as: a tool plugin stop, or the orchestration step the
/// error came from.
fn failure_of(e: anyhow::Error) -> TurnFailure {
    match e.downcast::<StopReason>() {
        Ok(reason) => TurnFailure::Stopped(reason),
        Err(e) => TurnFailure::Orchestration {
            cause: transport_cause(&e),
            message: format!("{e:#}"),
        },
    }
}

/// `failure` with every secret `redactor` holds masked out of its text.
fn masked(failure: TurnFailure, redactor: &Redactor) -> TurnFailure {
    match failure {
        TurnFailure::Orchestration { cause, message } => TurnFailure::Orchestration {
            cause,
            message: redactor.text(&message).into_owned(),
        },
        TurnFailure::Stopped(StopReason::Wedged(wedged)) => {
            TurnFailure::Stopped(StopReason::Wedged(crate::agent::tool_chain::Wedged {
                calls: wedged
                    .calls
                    .iter()
                    .map(|call| redactor.text(call).into_owned())
                    .collect(),
                repeats: wedged.repeats,
            }))
        }
        TurnFailure::Spawn(message) => TurnFailure::Spawn(redactor.text(&message).into_owned()),
        deadline @ TurnFailure::DeadlineExceeded(_) => deadline,
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
/// (normally or via `?`), instead of leaking a task that keeps polling `crate::process::STOP`.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Spawn the per-turn Ctrl-C bridge: poll [`crate::process::STOP`] every 50ms and trip `cancel` when it is
/// set, or when the turn's deadline passes, so the exec stream (and the cancellable upload/download
/// children) unwind promptly. STOP stays the single source of truth the loop driver already owns;
/// this only translates it into the token the async I/O `select!`s on. The returned guard aborts
/// the task at turn end.
fn spawn_stop_bridge(cancel: CancellationToken, deadline: Option<Instant>) -> AbortOnDrop {
    AbortOnDrop(tokio::spawn(async move {
        while !crate::process::STOP.load(Ordering::SeqCst)
            && deadline.is_none_or(|at| Instant::now() < at)
        {
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

/// Where a named `[agent.codex].api_key` is carried today: the deployment injects it as
/// `OPENAI_API_KEY_<name>` ([[secret_env]] in the deploy profile), and the unnamed default is
/// `OPENAI_API_KEY`. This is the one place a manifest's key name becomes a carrier, so an
/// openshell provider that can deliver the credential replaces this function rather than the
/// manifest schema.
fn codex_api_key_env(selected: Option<&str>) -> String {
    match selected {
        Some(name) => format!("{DEFAULT_CODEX_API_KEY_ENV}_{name}"),
        None => DEFAULT_CODEX_API_KEY_ENV.to_string(),
    }
}

/// Resolve the API-key half of Codex's dual auth. `None` means the caller should use the ChatGPT
/// OAuth mint. The explicit modes never silently cross over; only `auto` falls back. The key name
/// is manifest-validated ([`crate::manifest::Manifest::load`]) and resolved by
/// [`codex_api_key_env`].
fn selected_codex_api_key(
    cfg: &crate::manifest::CodexCfg,
    inference: &crate::agent::inference::InferenceEnv,
) -> Result<Option<String>> {
    use crate::manifest::CodexAuthMode;

    if let (Some(_), Some(key)) = (&inference.selection, &inference.openai_key) {
        if cfg.api_key.is_some() || cfg.auth == CodexAuthMode::Chatgpt {
            tracing::info!(
                manifest_key = cfg.api_key.as_deref().unwrap_or(""),
                manifest_auth = ?cfg.auth,
                "the injected agent binding's credential replaces [agent.codex]'s own"
            );
        }
        return Ok(Some(key.clone()));
    }

    let env_name = codex_api_key_env(cfg.api_key.as_deref());
    let key = || {
        std::env::var(&env_name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    };
    match cfg.auth {
        CodexAuthMode::Chatgpt => Ok(None),
        CodexAuthMode::Auto => Ok(key()),
        CodexAuthMode::Api => key().map(Some).with_context(|| {
            format!(
                "{env_name} unset or empty; [agent.codex].auth = \"api\" requires its selected API key"
            )
        }),
    }
}

#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    name = "openshell_turn",
    skip_all,
    fields(
        backend = "openshell",
        workspace = %p.workspace.display(),
        model = %turn.agent.model,
        sandbox = tracing::field::Empty,
        // Queryable without waiting for the run/iteration ancestors to export (long spans only
        // reach the backend when they END): which logical session and turn this is.
        session = session.map(|s| s.logical_name.as_str()).unwrap_or(""),
        turn = session.map(|s| s.completed_turns + 1).unwrap_or(1),
    )
)]
async fn try_turn(
    turn: &SandboxTurn<'_>,
    p: &Paths,
    prompt: &str,
    json: bool,
    session: Option<&crate::agent::agent_session::SessionTurn>,
    deadline: Option<Instant>,
    spent: &CostMeter,
    redactor: &Redactor,
    sink: &mut impl FnMut(&str, RawStream, Option<&AgentEvent>),
) -> Result<f64> {
    // Ctrl-C plumbing for the whole turn: STOP → this token, tripped by a bridge task; the exec
    // stream and the upload/download children `select!` it. The guard aborts the bridge at turn
    // end (including any `?` bail below).
    let cancel = CancellationToken::new();
    let _bridge = spawn_stop_bridge(cancel.clone(), deadline);

    // Env values and relay files are provisioned below, so the disclosure gate runs here, before
    // the sandbox exists (RFC-0001:C-CAPABILITY-DISCLOSURE).
    if let Some(covered) = turn.disclosure {
        crate::exposure::refuse_uncovered(covered, turn.agent.env, turn.relay)?;
    }

    // The agent harness for this turn (claude default): argv grammar, env script, seed files,
    // stream decoder, and the post-turn transcript contract all come from here.
    let harness = turn.agent.harness;
    let backend = backend(harness);

    // 1. Gateway up (idempotent, boots on the first turn, no-ops after). `None` = the
    //    default supervisor emulator image; the *agent* image is `--from` on create below.
    stage(sink, "starting the openshell gateway");
    let access = gateway::GatewayAccess::from_env()?;
    let version_warning = gateway::ensure_running(turn.compute_driver, &access, turn.gateway_boot)
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
    let gw = Gateway::connect(&access.dial()?)
        .context("connecting to the openshell gateway over gRPC")?;

    // 2. Resolve this harness's model credential: Vertex's static credential becomes a gateway
    //    provider the metadata emulator serves to claude/hermes, and to opencode/pi when the
    //    env names no key (see `provider` docs); codex gets the auth mode selected by
    //    `[agent.codex]`: an API key from the named env or a host-refreshed ChatGPT OAuth token,
    //    seeded as auth.json (step 7b). An API key, whatever the harness, becomes a static
    //    credential the proxy injects on the inference endpoint alone (step 4): the sandbox only
    //    ever holds the gateway's placeholder for it.
    let inference = crate::agent::inference::InferenceEnv::from_process_env()?;
    for key in [&inference.anthropic_key, &inference.openai_key]
        .into_iter()
        .flatten()
    {
        redactor.add(key);
    }
    let auth = crate::agent::harness::resolve_auth(harness, &inference);
    let gen_ai_system =
        crate::agent::harness::gen_ai_system(auth, turn.agent.model, &inference, turn.agent.env);
    let sandbox_auth = match auth {
        AuthProvider::Vertex => {
            let token = provider::mint_vertex_token()
                .await
                .context("minting the Vertex access token")?;
            redactor.add(&token);
            let vertex = crate::agent::inference::VertexConfig::from_env(turn.agent.env);
            ensure_provider(&gw, &token, &vertex.project, &vertex.region).await?;
            SandboxAuth::Gateway
        }
        AuthProvider::AnthropicKey => SandboxAuth::AnthropicKey,
        AuthProvider::ApiKey => {
            if inference.openai_key.is_none() && inference.anthropic_key.is_none() {
                return Err(OpenshellCliError::NoApiKey {
                    harness: harness.as_str(),
                }
                .into());
            }
            SandboxAuth::ApiKey
        }
        AuthProvider::Codex => SandboxAuth::Codex(
            match selected_codex_api_key(turn.agent.codex, &inference)? {
                Some(key) => provider::CodexAuth::ApiKey(key),
                None => provider::CodexAuth::ChatGpt(
                    provider::mint_codex_token()
                        .await
                        .context("minting the codex ChatGPT access token")?,
                ),
            },
        ),
    };
    match sandbox_auth.codex() {
        Some(provider::CodexAuth::ApiKey(key)) => redactor.add(key),
        Some(provider::CodexAuth::ChatGpt(token)) => {
            redactor.add(&token.access_token);
            redactor.add(&token.id_token);
        }
        None => {}
    }
    let model_key = InferenceKey::resolve(auth, &inference, &sandbox_auth, &turn.agent)?;
    if let Some(key) = &model_key {
        ensure_static_provider(&gw, &StaticProvider::inference(key.var), key.value).await?;
    }

    // 2b. Sandbox S3 reads (the read half of the S3 role split): a gateway-minted `aws-s3`
    //     provider signs sandbox S3 egress at the proxy via the read-only sandbox role, which
    //     the gateway holds through the loop pod's projected web-identity token and assumes
    //     again for each mint. Gated on the rendered env, absent, no provider and no S3 signing.
    let aws_provider = match crate::openshell::gateway::aws_sandbox_role() {
        Some(arn) => {
            ensure_aws_provider(&gw, &arn).await?;
            true
        }
        None => false,
    };

    // 2c. Broker token hardening: the per-run bearer token becomes a static credential on the
    //     `crucible-broker` provider, and the sandbox's `.mcp.json` carries only the
    //     `openshell:resolve:env:` placeholder. The proxy resolves it at egress, solely for the
    //     policy endpoint bound to this provider (the step-4 `credential_binding`), so the real
    //     token never enters the sandbox and a leaked placeholder resolves nowhere else.
    let agent_servers = crate::openshell::agent_servers::AgentServers::from_env()
        .context("reading the run's agent servers")?;
    agent_servers.check_names(
        turn.broker.enabled.then_some(turn.broker.name.as_str()),
        turn.mcp.iter().map(|s| s.key.as_str()),
    )?;
    let broker_auth = BrokerAuth::resolve(
        turn.broker.enabled,
        turn.broker_token,
        !agent_servers.is_empty(),
    );
    if let Some(token) = turn.broker_token {
        redactor.add(token);
    }
    for (key, value) in turn.agent.env {
        if turn.relayed_secrets.values().any(|var| var == key) {
            redactor.add(value);
        }
    }
    if let (BrokerAuth::Provider(_), Some(token)) = (&broker_auth, turn.broker_token) {
        ensure_static_provider(&gw, &StaticProvider::broker(), token).await?;
    }

    // 2d. The run's MCP servers that take agent tokens: each a token-grant provider whose tokens
    //     the sandbox's supervisor obtains from the controller and injects on traffic to that
    //     server alone. One served in this pod is started first.
    let mut binaries = policy::resolve_binaries(turn.openshell, harness.spec().binaries);
    agent_servers.add_binaries(&mut binaries);
    for s in agent_servers.iter() {
        tokio::task::block_in_place(|| s.start_local(turn.buildit_socket))
            .with_context(|| format!("starting the run's {}", s.server.name))?;
        ensure_token_grant_provider(
            &gw,
            s.provider_name().as_str(),
            agent_servers.token_grant_profile(s, &binaries),
        )
        .await?;
    }

    // 3. Create the sandbox, attaching the managed provider. Best-effort delete first: a prior
    //    turn killed mid-create, or whose cleanup delete failed, leaves the name behind, and a
    //    bare create then fails "already exists" for the rest of the run. Clearing it makes
    //    create idempotent. Labels make the sandbox discoverable via `list --selector`.
    let name = match turn.run_id {
        Some(run) => sandbox::name_for(&p.workspace, run),
        None => sandbox::fresh_name_for(&p.workspace),
    };
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
            turn.sandbox_image.unwrap_or("the default image"),
            grpc::pull_timeout().as_secs()
        ),
    );
    let labels = [
        ("crucible-pid".to_string(), std::process::id().to_string()),
        ("crucible-workspace".to_string(), label_value(&basename)),
    ];
    let mut providers = match auth {
        AuthProvider::Vertex => vec![provider::PROVIDER_NAME.clone()],
        AuthProvider::Codex | AuthProvider::AnthropicKey | AuthProvider::ApiKey => Vec::new(),
    };
    if aws_provider {
        providers.push(provider::AWS_PROVIDER_NAME.clone());
    }
    providers.extend(broker_auth.provider().cloned());
    providers.extend(model_key.as_ref().map(|k| k.var.provider_name().clone()));
    providers.extend(agent_servers.iter().map(|s| s.provider_name()));
    let sandbox_id = gw
        .create_sandbox(
            &name,
            turn.sandbox_image,
            &providers,
            &labels,
            &turn.openshell.read_only_paths,
            turn.sandbox_resources,
        )
        .await
        .context("creating the openshell sandbox")?;

    // 3a. Bind the sandbox to this run at the controller, which issues the sandbox's agent tokens
    //     only once it is bound. A failed bind leaves the sandbox without tokens, not the run
    //     without its turn.
    let agent_tokens = std::env::var(crucible_contract::sandbox::ENV_AGENT_TOKEN_ENDPOINT).ok();
    if let Some(target) = binds_sandboxes(
        agent_tokens.as_deref(),
        crate::report::sandbox_bind::BindTarget::from_env(),
    ) && let Err(e) = match sandbox_id {
        Some(sandbox_id) => {
            let binding = crucible_contract::sandbox::SandboxBinding {
                sandbox_id,
                workdir: Some(SandboxLayout::workdir(&basename)),
            };
            crate::report::sandbox_bind::bind_sandbox(&target, &binding).await
        }
        None => Err(crate::report::sandbox_bind::BindError::NoSandboxId(
            name.clone(),
        )),
    } {
        let ev = AgentEvent::Raw {
            text: format!("sandbox bind failed (its agent token requests will be refused): {e:#}"),
            stream: RawStream::Stderr,
        };
        sink("", RawStream::Stderr, Some(&ev));
    }

    // 3b. Hand the broker this turn's boundary: a fresh token at `<storage>/turn-token`. The
    //     broker's per-turn candidate budget resets when the value changes. Best-effort, on a
    //     host without the forge storage dir (a laptop run) the budget just stays uncapped.
    //     Alongside it, this turn's W3C traceparent at `<storage>/turn-traceparent`: the broker is
    //     run-lifetime, so it reads the per-turn parent from this file fresh per call (a static pod
    //     env would collapse every turn's broker spans onto the first turn's trace).
    if turn.broker.enabled {
        if let Err(e) = write_turn_token().await {
            let ev = AgentEvent::Raw {
                text: format!("turn-token write failed (candidate budget stays uncapped): {e:#}"),
                stream: RawStream::Stderr,
            };
            sink("", RawStream::Stderr, Some(&ev));
        }
        if let Err(e) = write_traceparent_files(
            &forge::storage_root(),
            crate::agent::engine::current_trace_env(),
        )
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
    let collector = if harness.spec().otel_capable
        && crate::agent::turn::otel_enabled(turn.agent.env)
    {
        // The agent's exporter can't attribute its spans to a loop turn; the forwarder stamps
        // session + turn as resource attributes at the same boundary where it re-parents.
        let forward = crate::agent::turn::otel_forward(turn.agent.env).map(|f| {
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
            redactor.clone(),
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
        let driver = turn.compute_driver;
        let broker_url = if turn.broker.enabled {
            Some(crate::manifest::resolve_broker_url(
                turn.broker,
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
            turn.openshell,
            &policy::default_endpoints(harness),
            broker_ep.as_ref(),
        );
        if let Some(c) = &collector {
            endpoints.push(c.sandbox_egress(driver.broker_host()));
        }
        // A custom base URL is a host the harness's built-in allowlist never named.
        if let Some(ep) = inference
            .egress_endpoint_for(harness)?
            .map(|e| e.to_string())
            && !endpoints.contains(&ep)
        {
            endpoints.push(ep);
        }
        // Each `[mcp]` server in the turn's scope, appended after the deny list like the broker.
        let mcp: Vec<_> = turn
            .mcp
            .iter()
            .filter(|s| turn.mcp_scope.contains(&s.key))
            .map(|s| (s, format!("http://{}:{}/mcp", driver.broker_host(), s.port)))
            .collect();
        for (_, url) in &mcp {
            let ep = crate::manifest::broker_endpoint_from_url(url)?.to_string();
            if !endpoints.contains(&ep) {
                endpoints.push(ep);
            }
        }
        let mut credential_bindings: Vec<grpc::EndpointCredentialBinding> = broker_ep
            .as_ref()
            .zip(broker_auth.provider())
            .map(|(ep, provider)| grpc::EndpointCredentialBinding {
                host: ep.host.clone(),
                port: ep.port,
                provider: provider.clone(),
            })
            .into_iter()
            .collect();
        for ep in agent_servers
            .iter()
            .map(|s| s.egress_endpoint().to_string())
        {
            if !endpoints.contains(&ep) {
                endpoints.push(ep);
            }
        }
        if auth == AuthProvider::Vertex {
            credential_bindings.extend(policy::VERTEX_CREDENTIAL_HOSTS.iter().map(|host| {
                grpc::EndpointCredentialBinding {
                    host: (*host).to_string(),
                    port: 443,
                    provider: provider::PROVIDER_NAME.clone(),
                }
            }));
        }
        if let Some(key) = &model_key {
            credential_bindings.push(key.binding(&endpoints, &turn.openshell.tls_skip_endpoints)?);
        }
        gw.update_policy_wait_with_tls_skip(
            &name,
            &binaries,
            &endpoints,
            &credential_bindings,
            &turn.openshell.tls_skip_endpoints,
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
        for rf in turn.relay {
            let rendered = relay::render(&p.workspace, rf)
                .with_context(|| format!("rendering relay `{}`", rf.dest))?;
            for value in &rendered.values {
                redactor.add(value);
            }
            let tmp = write_temp("cred", &rendered.content).await?;
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
        let mut turn_env =
            SandboxEnv::from_manifest(turn.agent.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        if let Some(c) = &collector {
            for (k, v) in
                crucible_harness::otel::otel_env(&c.sandbox_endpoint(driver.broker_host()))
            {
                turn_env.set(EnvSource::Otel, &k, &v);
            }
        }
        inference_env(
            &mut turn_env,
            harness,
            auth,
            &inference,
            model_key.as_ref().map(|k| k.var),
        );
        let env_tmp = write_temp("env", &turn_env.script(harness.spec())).await?;
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
        //     broker and the turn's `[mcp]` servers, loaded via `--mcp-config`). The URLs were
        //     resolved in step 4 so each URL and its egress entry agree. Each `[mcp]` server gets
        //     a fresh token naming this sandbox and its workdir.
        let seed_token = match &broker_auth {
            BrokerAuth::Provider(Placeholder::Issued) => {
                Some(issued_placeholder(&gw, &name, provider::BROKER_CRED_KEY, &cancel).await?)
            }
            BrokerAuth::Provider(Placeholder::Canonical) => {
                Some(provider::broker_token_placeholder())
            }
            BrokerAuth::None => turn.broker_token.map(str::to_string),
        };
        let tokens = crate::control::mcp::grant_all(
            mcp.iter().map(|(s, _)| *s),
            &name,
            &SandboxLayout::workdir(&basename),
        )?;
        for token in &tokens {
            redactor.add(token);
        }
        let mut servers: Vec<McpServer<'_>> = broker_url
            .iter()
            .map(|url| McpServer {
                name: &turn.broker.name,
                url,
                token: seed_token.as_deref(),
            })
            .collect();
        servers.extend(mcp.iter().zip(&tokens).map(|((s, url), token)| McpServer {
            name: &s.key,
            url,
            token: Some(token),
        }));
        servers.extend(agent_servers.iter().map(|s| McpServer {
            name: s.server.name.as_str(),
            url: &s.server.url,
            token: None,
        }));
        let issued = match (&sandbox_auth, &model_key) {
            (SandboxAuth::Codex(provider::CodexAuth::ApiKey(_)), Some(key)) => {
                Some(issued_placeholder(&gw, &name, key.var.env(), &cancel).await?)
            }
            _ => None,
        };
        let seeds = backend.seed_files(
            &turn.agent,
            &servers,
            &sandbox_auth.seeded(issued),
            &inference,
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
        let mcp_seeded = !servers.is_empty();
        let argv = match session {
            Some(session) => backend
                .sandbox_session_argv(&turn.agent, mcp_seeded, session)
                .context("building continuing sandbox harness argv")?,
            None => backend.sandbox_argv(&turn.agent, mcp_seeded),
        };
        let wrapper = crate::agent::harness::exec_wrapper(&basename, &argv);
        let exec_opts = ExecOpts {
            json,
            cancel: &cancel,
        };
        // A deep self-check turn outlives the ~1h Vertex token (the agent waits on many
        // multi-minute GPU jobs), so re-mint into the provider slot while claude runs, its
        // google-auth re-queries the metadata emulator as the cached token nears expiry. Codex has
        // no mid-turn refresher: API keys do not expire during a turn, and the OAuth access token
        // is fixed in the seeded auth file; refreshing it here could not update the sandbox.
        let refresher = match auth {
            AuthProvider::Vertex => Some(tokio::spawn({
                let gw = gw.clone();
                let redactor = redactor.clone();
                async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(25 * 60)).await;
                        match provider::mint_vertex_token().await {
                            Ok(t) => {
                                redactor.add(&t);
                                if let Err(e) = gw
                                    .update_provider(
                                        provider::PROVIDER_NAME.as_str(),
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
            AuthProvider::Codex | AuthProvider::AnthropicKey | AuthProvider::ApiKey => None,
        };
        let full = crate::agent::turn::tool_io_full(turn.agent.env);
        let pump = crate::agent::turn::StreamPump::new(
            backend.decoder(&turn.agent, meters.as_ref(), full),
            (!full).then(|| backend.decoder(&turn.agent, None, true)),
            crate::agent::tool_chain::ToolChain::declared(turn.tool_plugins),
            turn.pricing,
        );
        let exec_result =
            exec_and_stream(&gw, &name, &wrapper, pump, &exec_opts, spent, sink).await;
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
        spent.raise(cost.total());

        // 8a. Agent exited: roll the captured OTLP jsonl up into the `otel_summary` event
        //     (authoritative cost, per-model usage, API latency).
        if let Some(c) = &collector
            && let Some(summary) = c.summary()
        {
            let ev = summary.to_event();
            cost.observe(&ev);
            spent.raise(cost.total());
            sink("", RawStream::Stdout, Some(&ev));
        }

        // 8b. Sweep the sandbox log for egress denials before teardown. An agent probing
        //     blocked endpoints mid-turn is signal (reward-hacking telemetry), so denials
        //     land in the run log. Best-effort: a logs failure never fails the turn.
        replay_sandbox_log(&gw, &name, LogReplay::DenialsOnly, sink).await;

        // 9. Download the workspace back (the agent's edits round-trip to the host).
        stage(sink, "agent turn done — downloading the workspace");
        let sandbox_workdir = SandboxLayout::workdir(&basename);
        retrieve_workspace(&name, &sandbox_workdir, p.workspace.as_path(), &cancel).await?;

        // Save the updated native transcript before telemetry parsing and teardown. It is private
        // 0600 runtime state, never appended to session.jsonl or published with the run record.
        if let Some(session) = session {
            persist_private_session(p, session, &gw, &name, &cancel).await?;
        }

        // The post-turn transcript fetch: pure telemetry garnish for claude (export-gated), but
        // a backfill harness's ONLY source of result events + cost, so for those it runs
        // unconditionally and a failure is loud (see `graft_turn_telemetry`).
        if harness.spec().backfill_required
            || crate::agent::engine::TurnExport::resolve().emits_anything()
        {
            graft_turn_telemetry(
                harness,
                gen_ai_system,
                &gw,
                &name,
                &cancel,
                redactor,
                &mut cost,
                sink,
            )
            .await;
            spent.raise(cost.total());
        }
        Ok(cost.total())
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
    for server in turn.mcp {
        if let Err(e) = server.tokens.revoke(&name) {
            tracing::warn!("revoking {name}'s token on [mcp.{}]: {e:#}", server.key);
        }
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
            ts_ms: grpc::event_time_ms(line),
            level: line.level.clone(),
            target: line.target.clone(),
            message: line.message.trim_end().to_string(),
            denial,
            fields: line.fields.clone().into_iter().collect(),
        };
        sink(line.message.trim_end(), RawStream::Stderr, Some(&ev));
    }
}

/// Ensure the `google-cloud` profile, then probe the provider (`GetProvider`) → create on the
/// first turn, update (swap the token) thereafter. The token rides the request body, never an
/// argv or a log line.
async fn ensure_provider(gw: &Gateway, token: &str, project: &str, region: &str) -> Result<()> {
    gw.ensure_provider_profile(provider::google_cloud_profile())
        .await
        .context("importing the google-cloud provider profile")?;
    if !gw.provider_exists(provider::PROVIDER_NAME.as_str()).await
        && gw
            .create_provider(
                provider::PROVIDER_NAME.as_str(),
                provider::CRED_KEY,
                token,
                provider::PROVIDER_TYPE,
                project,
                region,
            )
            .await
            .context("creating the Vertex provider")?
            == grpc::ProviderCreate::Created
    {
        return Ok(());
    }
    gw.update_provider(provider::PROVIDER_NAME.as_str(), provider::CRED_KEY, token)
        .await
        .context("updating the Vertex provider token")
}

#[derive(Debug, thiserror::Error)]
pub enum IssuedPlaceholderError {
    #[error(
        "the gateway issued no {key} placeholder to sandbox {sandbox}; an endpoint-bound \
         credential resolves only through the placeholder issued to the workload"
    )]
    Missing { key: &'static str, sandbox: String },
}

/// The placeholder the gateway issued the sandbox for provider credential `key`. Endpoint-bound
/// credentials resolve only through this revision-scoped value, never the canonical
/// `openshell:resolve:env:<key>` form, so seeded configs must carry what the workload was given.
async fn issued_placeholder(
    gw: &Gateway,
    sandbox: &str,
    key: &'static str,
    cancel: &CancellationToken,
) -> Result<String> {
    let command = vec!["printenv".to_string(), key.to_string()];
    let mut issued: Option<String> = None;
    gw.exec(sandbox, &command, cancel, |line| {
        let value = line.trim();
        if issued.is_none() && value.starts_with(openshell_core::secrets::PLACEHOLDER_PREFIX_PUBLIC)
        {
            issued = Some(value.to_string());
        }
    })
    .await
    .with_context(|| format!("reading the {key} placeholder issued to sandbox {sandbox}"))?;
    issued.ok_or_else(|| {
        IssuedPlaceholderError::Missing {
            key,
            sandbox: sandbox.to_string(),
        }
        .into()
    })
}

/// Where to bind this turn's sandbox: the run at the controller, and only when the controller
/// issues agent tokens (`CRUCIBLE_AGENT_TOKEN_ENDPOINT`), since nothing reads a binding otherwise.
fn binds_sandboxes(
    agent_token_endpoint: Option<&str>,
    target: Option<crate::report::sandbox_bind::BindTarget>,
) -> Option<crate::report::sandbox_bind::BindTarget> {
    agent_token_endpoint
        .filter(|endpoint| !endpoint.trim().is_empty())
        .and(target)
}

/// How the sandbox's `.mcp.json` names the broker token.
#[derive(Debug, PartialEq, Eq)]
enum Placeholder {
    /// [`provider::broker_token_placeholder`].
    Canonical,
    /// The placeholder the gateway issued the workload, read from the sandbox.
    Issued,
}

/// How the sandbox authenticates to the run's broker.
#[derive(Debug, PartialEq, Eq)]
enum BrokerAuth {
    /// No broker provider this turn: the seed carries whatever token the run holds, in clear.
    None,
    /// The token is a static credential on the broker provider and the seed carries only a
    /// placeholder, so the real token never enters the sandbox. The issued placeholder only
    /// alongside a token grant; otherwise the canonical one.
    Provider(Placeholder),
}

impl BrokerAuth {
    fn resolve(enabled: bool, token: Option<&str>, token_grant: bool) -> Self {
        match (enabled, token, token_grant) {
            (true, Some(_), true) => BrokerAuth::Provider(Placeholder::Issued),
            (true, Some(_), false) => BrokerAuth::Provider(Placeholder::Canonical),
            _ => BrokerAuth::None,
        }
    }

    /// The gateway provider the sandbox attaches and the broker endpoint binds to.
    fn provider(&self) -> Option<&'static grpc::ProviderName> {
        match self {
            BrokerAuth::Provider(_) => Some(&provider::BROKER_PROVIDER_NAME),
            BrokerAuth::None => None,
        }
    }
}

/// An endpointless static-credential provider: its profile, its provider name, and the credential
/// key its placeholder resolves through.
struct StaticProvider {
    profile: openshell_core::proto::ProviderProfile,
    name: grpc::ProviderName,
    cred_key: &'static str,
}

impl StaticProvider {
    fn broker() -> Self {
        Self {
            profile: provider::broker_profile(),
            name: provider::BROKER_PROVIDER_NAME.clone(),
            cred_key: provider::BROKER_CRED_KEY,
        }
    }

    fn inference(var: provider::InferenceKeyVar) -> Self {
        Self {
            profile: var.profile(),
            name: var.provider_name().clone(),
            cred_key: var.env(),
        }
    }
}

/// The model API key a turn's harness sends, and the endpoint it sends it to.
#[derive(Debug, PartialEq, Eq)]
struct InferenceKey<'a> {
    var: provider::InferenceKeyVar,
    value: &'a str,
    endpoint: Endpoint,
}

impl<'a> InferenceKey<'a> {
    /// The key this turn authenticates to its model with, when it uses one: the direct Anthropic
    /// key, Codex's selected API key, or the key a provider-table harness's endpoint takes. `None`
    /// for Vertex, Codex's ChatGPT sign-in, and an endpoint the env names no key for.
    fn resolve(
        auth: AuthProvider,
        inference: &'a crate::agent::inference::InferenceEnv,
        sandbox_auth: &'a SandboxAuth,
        agent: &crate::args::TurnAgent<'_>,
    ) -> Result<Option<InferenceKey<'a>>> {
        use crate::agent::harness::ApiEndpoint;
        use crate::agent::inference::{ANTHROPIC_API_URL, OPENAI_API_URL, egress_endpoint};
        use provider::InferenceKeyVar::{Anthropic, OpenAi};
        let (var, value, base_url) = match auth {
            AuthProvider::Vertex => return Ok(None),
            AuthProvider::AnthropicKey => match &inference.anthropic_key {
                Some(key) => (
                    Anthropic,
                    key,
                    inference
                        .anthropic_base_url
                        .clone()
                        .unwrap_or_else(|| ANTHROPIC_API_URL.to_string()),
                ),
                None => return Ok(None),
            },
            AuthProvider::Codex => match sandbox_auth.codex() {
                Some(provider::CodexAuth::ApiKey(key)) => (
                    OpenAi,
                    key,
                    inference
                        .openai_base_url
                        .clone()
                        .unwrap_or_else(|| OPENAI_API_URL.to_string()),
                ),
                Some(provider::CodexAuth::ChatGpt(_)) | None => return Ok(None),
            },
            AuthProvider::ApiKey => {
                match crate::agent::harness::api_endpoint(agent.model, inference, agent.env) {
                    ApiEndpoint::OpenAi { base_url, .. } => match &inference.openai_key {
                        Some(key) => (OpenAi, key, base_url),
                        None => return Ok(None),
                    },
                    ApiEndpoint::Anthropic { base_url } => match &inference.anthropic_key {
                        Some(key) => (Anthropic, key, base_url),
                        None => return Ok(None),
                    },
                    ApiEndpoint::Vertex(_) => return Ok(None),
                }
            }
        };
        Ok(Some(InferenceKey {
            var,
            value,
            endpoint: egress_endpoint(&base_url)
                .with_context(|| format!("the inference endpoint {base_url:?}"))?,
        }))
    }

    /// The policy binding that has the proxy inject this key on its endpoint, which `endpoints`
    /// must name exactly and `tls_skip` must not.
    fn binding(
        &self,
        endpoints: &[String],
        tls_skip: &[String],
    ) -> Result<grpc::EndpointCredentialBinding, InferenceKeyError> {
        let (host, port) = (&self.endpoint.host, self.endpoint.port);
        let names_it = |spec: &String| {
            spec.parse::<Endpoint>()
                .is_ok_and(|e| &e.host == host && e.port == port)
        };
        let endpoint = format!("{host}:{port}");
        if tls_skip.iter().any(names_it) {
            return Err(InferenceKeyError::TlsSkipped { endpoint });
        }
        if !endpoints.iter().any(names_it) {
            return Err(InferenceKeyError::NotAllowlisted { endpoint });
        }
        Ok(grpc::EndpointCredentialBinding {
            host: host.clone(),
            port,
            provider: self.var.provider_name().clone(),
        })
    }
}

/// Import the endpointless profile and set this run's token on the provider, update-or-create like
/// the Vertex provider: the token changes every run, the provider object survives across runs on a
/// shared gateway.
async fn ensure_static_provider(gw: &Gateway, p: &StaticProvider, token: &str) -> Result<()> {
    let profile_id = p.profile.id.clone();
    gw.ensure_provider_profile(p.profile.clone())
        .await
        .with_context(|| format!("importing the {} provider profile", p.name))?;
    if !gw.provider_exists(p.name.as_str()).await
        && gw
            .create_static_provider(p.name.as_str(), p.cred_key, token, &profile_id)
            .await
            .with_context(|| format!("creating the {} provider", p.name))?
            == grpc::ProviderCreate::Created
    {
        return Ok(());
    }
    gw.update_provider(p.name.as_str(), p.cred_key, token)
        .await
        .with_context(|| format!("updating the {} provider token", p.name))
}

/// Import a token-grant profile and create its provider `name`, which holds no secret: the
/// supervisor obtains each token itself. A provider already registered under the name on this
/// gateway is left as it is.
async fn ensure_token_grant_provider(
    gw: &Gateway,
    name: &str,
    profile: openshell_core::proto::ProviderProfile,
) -> Result<()> {
    let profile_id = profile.id.clone();
    gw.ensure_provider_profile(profile)
        .await
        .with_context(|| format!("importing the {name} token-grant profile"))?;
    gw.ensure_minted_provider(name, &profile_id)
        .await
        .with_context(|| format!("creating the {name} token-grant provider"))
}

/// Idempotent AWS provider setup: import the `aws-s3` profile, create the provider if absent,
/// (re)configure its STS `AssumeRole` refresh, then rotate once so the credentials exist BEFORE
/// the sandbox's first request, the proxy fails closed on unminted credentials and the refresh
/// worker's tick is up to 60s away. Re-configuring each turn keeps a rotated role ARN current.
/// The gateway's own identity is the same role (see `gateway::aws_sandbox_role`), so the mint
/// is a self-assume.
async fn ensure_aws_provider(gw: &Gateway, role_arn: &str) -> Result<()> {
    use openshell_core::proto::ProviderCredentialRefreshStrategy;
    gw.ensure_provider_profile(provider::aws_s3_profile())
        .await
        .context("importing the aws-s3 provider profile")?;
    gw.ensure_minted_provider(
        provider::AWS_PROVIDER_NAME.as_str(),
        provider::AWS_PROVIDER_TYPE,
    )
    .await
    .context("creating the aws-s3 provider")?;
    let mut material = std::collections::HashMap::from([
        ("role_arn".to_string(), role_arn.to_string()),
        ("session_name".to_string(), "crucible-sandbox".to_string()),
    ]);
    if let Ok(region) = std::env::var("CRUCIBLE_AWS_SANDBOX_REGION")
        && !region.trim().is_empty()
    {
        material.insert("aws_region".to_string(), region.trim().to_string());
    }
    gw.configure_provider_refresh(
        provider::AWS_PROVIDER_NAME.as_str(),
        provider::AWS_PRIMARY_CRED,
        ProviderCredentialRefreshStrategy::AwsStsAssumeRole,
        material,
    )
    .await
    .context("configuring the aws-s3 assume-role refresh")?;
    gw.rotate_provider_credential(
        provider::AWS_PROVIDER_NAME.as_str(),
        provider::AWS_PRIMARY_CRED,
    )
    .await
    .context("pre-warming the aws-s3 credentials")
}

/// The exec turn's knobs, bundled so [`exec_and_stream`] keeps a small signature: the front-end
/// mode and the turn's cancellation token.
struct ExecOpts<'a> {
    json: bool,
    cancel: &'a CancellationToken,
}

/// Exec the agent over the gateway's `ExecSandbox` stream, driving `pump` with the stdout lines the
/// stream yields, and return the turn's running cost. The stream's stderr is replayed through the
/// sink after the agent exits, mirroring the old CLI-child behavior. `cancel` unwinds the exec on
/// Ctrl-C (see [`Gateway::exec`]); a tool chain stop unwinds only the exec, so teardown still runs.
async fn exec_and_stream(
    gw: &Gateway,
    name: &str,
    command: &[String],
    mut pump: crate::agent::turn::StreamPump,
    opts: &ExecOpts<'_>,
    spent: &CostMeter,
    sink: &mut impl FnMut(&str, RawStream, Option<&AgentEvent>),
) -> Result<crate::agent::turn::TurnCost> {
    let stop = opts.cancel.child_token();
    let exec = gw
        .exec(name, command, &stop, |line| {
            pump.push(line, opts.json, sink);
            if pump.stopped().is_some() {
                stop.cancel();
            }
        })
        .await?;
    let crate::agent::turn::PumpEnd { cost, stopped } = pump.finish(sink);
    let (exit_code, transport_error) = (exec.exit_code, exec.transport_error);
    // Bank it here, not at the caller's `?`: the agent can bill real tokens and still exit
    // non-zero below, and a turn that spent money is never reported free.
    spent.raise(cost.total());
    if let Some(reason) = stopped {
        return Err(reason.into());
    }

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

/// The model-reach variables this turn's sandbox carries: base URLs, never a key. A direct
/// Anthropic key replaces the Vertex selectors the manifest set (Claude Code prefers Vertex
/// whenever they are present), and a custom base URL rides under the name its harness reads. The
/// gateway issues the placeholder for `key` under its variable, so a manifest entry of that name
/// is dropped rather than left to shadow it.
pub(crate) fn inference_env(
    env: &mut SandboxEnv,
    harness: Harness,
    auth: AuthProvider,
    inference: &crate::agent::inference::InferenceEnv,
    key: Option<provider::InferenceKeyVar>,
) {
    use crate::agent::inference::{ANTHROPIC_BASE_URL, OPENAI_BASE_URL};
    if auth == AuthProvider::AnthropicKey {
        env.remove(crate::agent::inference::VERTEX_SELECTORS);
    }
    if let Some(var) = key {
        env.remove(&[var.env()]);
    }
    let anthropic = (ANTHROPIC_BASE_URL, &inference.anthropic_base_url);
    let openai = (OPENAI_BASE_URL, &inference.openai_base_url);
    let urls = match harness.spec().api {
        ApiFamily::Anthropic => vec![anthropic],
        ApiFamily::OpenAi => vec![openai],
        ApiFamily::Both => vec![openai, anthropic],
    };
    for (var, url) in urls {
        if let Some(url) = url.as_deref() {
            env.set(EnvSource::Inference, var, url);
        }
    }
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
/// turn's (possibly raised) cost. Spans and content logs carry nothing `redactor` holds.
#[allow(clippy::too_many_arguments)]
async fn graft_turn_telemetry(
    harness: Harness,
    gen_ai_system: &'static str,
    gw: &Gateway,
    name: &str,
    cancel: &CancellationToken,
    redactor: &Redactor,
    cost: &mut crate::agent::turn::TurnCost,
    sink: &mut impl FnMut(&str, RawStream, Option<&AgentEvent>),
) {
    let fetched = tokio::time::timeout(
        harness.spec().transcript_fetch_timeout,
        fetch_transcript(harness, gw, name, cancel),
    )
    .await;
    let Ok(Some(bytes)) = fetched else {
        if harness.spec().backfill_required {
            let ev = AgentEvent::Error {
                error_type: "transcript".into(),
                message: "transcript fetch failed — the turn's result and cost are unavailable"
                    .into(),
            };
            sink("", RawStream::Stderr, Some(&ev));
        }
        return;
    };

    let export = crate::agent::engine::TurnExport::resolve();
    // One infallible parse over the in-memory transcript feeds span export and (backfill
    // harnesses) the turn's result; skip it when nothing consumes it.
    let artifacts = if export.spans() || harness.spec().backfill_required {
        backend(harness).parse_transcript(&bytes)
    } else {
        TurnArtifacts::default()
    };
    if export.emits_anything() {
        let span = tracing::Span::current();
        let mask = |s: &str| redactor.text(s).into_owned();
        if export.spans() {
            let tool_calls: Vec<_> = artifacts
                .tool_calls
                .iter()
                .cloned()
                .map(|call| call.masked(mask))
                .collect();
            crate::agent::engine::synthesize_tool_spans(
                &span,
                &tool_calls,
                std::time::SystemTime::now(),
            );
        }
        if export.content() {
            // Content logs come from the harness's own transcript reader (claude maps its jsonl;
            // hermes: Phase B), off the same in-memory bytes, no re-read.
            let records: Vec<_> = backend(harness)
                .content_records(&bytes)
                .into_iter()
                .map(|record| record.masked(mask))
                .collect();
            crate::agent::engine::emit_conversation_logs(&span, &records, gen_ai_system);
        }
    }

    if harness.spec().backfill_required {
        for ev in &artifacts.events {
            cost.observe(ev);
            sink("", RawStream::Stdout, Some(ev));
        }
        if let Some(c) = artifacts.cost_usd {
            cost.report(c);
        }
    }
}

const PRIVATE_SESSION_DIR: &str = "agent-session-private";

fn private_session_paths(
    p: &Paths,
    session: &crate::agent::agent_session::SessionTurn,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = p.state.join(PRIVATE_SESSION_DIR).join(&session.provider_id);
    (root.join("locator"), root.join("transcript.jsonl"))
}

fn valid_private_remote(remote: &str, session: &crate::agent::agent_session::SessionTurn) -> bool {
    Harness::Claude
        .spec()
        .transcript
        .jsonl_tree()
        .is_some_and(|(root, _)| remote.starts_with(&format!("{root}/")))
        && remote.ends_with(&format!("/{}.jsonl", session.provider_id))
        && !remote.contains("..")
        && std::path::Path::new(remote).is_absolute()
}

/// Restore one Claude Code transcript into a new sandbox at the exact private path where the
/// previous sandbox wrote it. The locator is validated against the pinned config root and opaque
/// session UUID before it is used as an upload target.
async fn restore_private_session(
    p: &Paths,
    session: &crate::agent::agent_session::SessionTurn,
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
    session: &crate::agent::agent_session::SessionTurn,
    gw: &Gateway,
    name: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    let (root, glob) = Harness::Claude
        .spec()
        .transcript
        .jsonl_tree()
        .context("claude keeps its transcripts in a jsonl tree")?;
    let remote = newest_transcript_path(gw, name, root, glob, cancel)
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
    let locator = harness.spec().transcript;
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

#[cfg(test)]
mod tests {
    #[test]
    fn a_sandbox_is_bound_only_where_the_controller_issues_agent_tokens() {
        use crate::openshell::run::binds_sandboxes;
        let target = || {
            crate::report::sandbox_bind::BindTarget::new(
                "http://crucible-controller.ns.svc:8080",
                "run-a",
                "crr_id_secret",
            )
        };
        let endpoint = "http://crucible-controller.ns.svc:8080/oauth/token";
        assert!(binds_sandboxes(Some(endpoint), Some(target())).is_some());
        assert!(
            binds_sandboxes(None, Some(target())).is_none(),
            "a controller without the agent issuer serves no bind route"
        );
        assert!(binds_sandboxes(Some(" "), Some(target())).is_none());
        assert!(binds_sandboxes(Some(endpoint), None).is_none());
    }

    #[test]
    fn the_broker_placeholder_reads_the_sandbox_only_alongside_a_token_grant() {
        use crate::openshell::run::{BrokerAuth, Placeholder};
        assert_eq!(
            BrokerAuth::resolve(true, Some("t"), false),
            BrokerAuth::Provider(Placeholder::Canonical)
        );
        assert_eq!(
            BrokerAuth::resolve(true, Some("t"), true),
            BrokerAuth::Provider(Placeholder::Issued)
        );
        assert_eq!(BrokerAuth::resolve(true, None, true), BrokerAuth::None);
        assert_eq!(
            BrokerAuth::resolve(false, Some("t"), true),
            BrokerAuth::None
        );
        assert_eq!(
            BrokerAuth::resolve(true, Some("t"), false).provider(),
            Some(&crate::openshell::provider::BROKER_PROVIDER_NAME)
        );
        assert_eq!(BrokerAuth::None.provider(), None);
        assert_eq!(
            crate::openshell::provider::broker_token_placeholder(),
            format!(
                "{}BROKER_TOKEN",
                openshell_core::secrets::PLACEHOLDER_PREFIX_PUBLIC
            )
        );
    }

    use crate::openshell::run::*;

    #[test]
    fn an_orchestration_error_is_classified_by_the_typed_error_in_its_chain() {
        let gateway = anyhow::Error::from(gateway::GatewayError::HealthTimeout {
            last_status: String::new(),
            log_tail: String::new(),
        })
        .context("ensuring the openshell gateway is up");
        assert_eq!(transport_cause(&gateway), TransportCause::Gateway);
        let sandbox = anyhow::Error::from(grpc::GrpcError::SandboxErrorPhase {
            name: "ci-1".into(),
        })
        .context("creating the openshell sandbox");
        assert_eq!(transport_cause(&sandbox), TransportCause::Sandbox);
        let exited = anyhow::Error::from(OpenshellCliError::AgentExit { code: 127 });
        assert_eq!(transport_cause(&exited), TransportCause::Agent);
        let broke = anyhow::Error::from(OpenshellCliError::ExecStreamBroke {
            code: tonic::Code::Unavailable,
            message: "reset".into(),
        });
        assert_eq!(transport_cause(&broke), TransportCause::Agent);
        let cli = anyhow::Error::from(OpenshellCliError::Failed {
            label: "provider register".into(),
            stderr: "denied".into(),
        });
        assert_eq!(transport_cause(&cli), TransportCause::Other);
        assert_eq!(
            transport_cause(&anyhow::Error::from(std::io::Error::other("no runtime"))),
            TransportCause::Other
        );
    }

    #[test]
    fn an_injected_codex_key_replaces_the_manifests_in_every_auth_mode() {
        use crate::manifest::{CodexAuthMode, CodexCfg};
        let injected = crate::agent::inference::InferenceEnv {
            openai_key: Some("sk-bound".into()),
            selection: Some(crate::agent::inference::AgentSelection {
                protocol: crucible_contract::inference::InferenceProtocol::Responses,
                model: "m".into(),
                price: None,
            }),
            ..Default::default()
        };
        for auth in [
            CodexAuthMode::Auto,
            CodexAuthMode::Api,
            CodexAuthMode::Chatgpt,
        ] {
            let cfg = CodexCfg {
                auth,
                api_key: Some("CRUCIBLE_TEST_NEVER_SET".into()),
                ..CodexCfg::default()
            };
            assert_eq!(
                selected_codex_api_key(&cfg, &injected).unwrap().as_deref(),
                Some("sk-bound"),
                "{auth:?}"
            );
        }
    }

    #[test]
    fn an_ambient_openai_key_does_not_replace_the_manifests_codex_choice() {
        use crate::manifest::{CodexAuthMode, CodexCfg};
        let ambient = crate::agent::inference::InferenceEnv {
            openai_key: Some("sk-ambient".into()),
            ..Default::default()
        };
        let cfg = CodexCfg {
            auth: CodexAuthMode::Chatgpt,
            ..CodexCfg::default()
        };
        assert_eq!(selected_codex_api_key(&cfg, &ambient).unwrap(), None);
    }

    #[test]
    fn codex_auth_selection_can_switch_between_named_keys_and_chatgpt() {
        let _guard = crucible::test_support::env_lock();
        const KEY: &str = "CRUCIBLE_TEST_WORK";
        let key_env = codex_api_key_env(Some(KEY));
        assert_eq!(key_env, "OPENAI_API_KEY_CRUCIBLE_TEST_WORK");
        assert_eq!(codex_api_key_env(None), DEFAULT_CODEX_API_KEY_ENV);
        unsafe { std::env::set_var(&key_env, "sk-work") };

        let mut cfg = crate::manifest::CodexCfg {
            auth: crate::manifest::CodexAuthMode::Api,
            api_key: Some(KEY.to_string()),
            ..Default::default()
        };
        assert_eq!(
            selected_codex_api_key(&cfg, &Default::default())
                .unwrap()
                .as_deref(),
            Some("sk-work")
        );

        cfg.auth = crate::manifest::CodexAuthMode::Auto;
        assert_eq!(
            selected_codex_api_key(&cfg, &Default::default())
                .unwrap()
                .as_deref(),
            Some("sk-work")
        );

        cfg.auth = crate::manifest::CodexAuthMode::Chatgpt;
        assert_eq!(
            selected_codex_api_key(&cfg, &Default::default()).unwrap(),
            None
        );
        unsafe { std::env::remove_var(&key_env) };

        cfg.auth = crate::manifest::CodexAuthMode::Auto;
        assert_eq!(
            selected_codex_api_key(&cfg, &Default::default()).unwrap(),
            None
        );
    }

    #[test]
    fn explicit_api_auth_never_silently_falls_back_to_chatgpt() {
        let _guard = crucible::test_support::env_lock();
        const KEY: &str = "CRUCIBLE_TEST_MISSING";
        let key_env = codex_api_key_env(Some(KEY));
        unsafe { std::env::remove_var(&key_env) };
        let cfg = crate::manifest::CodexCfg {
            auth: crate::manifest::CodexAuthMode::Api,
            api_key: Some(KEY.to_string()),
            ..Default::default()
        };
        let err = selected_codex_api_key(&cfg, &Default::default())
            .expect_err("explicit API mode needs its key");
        assert!(err.to_string().contains(&key_env), "{err}");
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
            TurnFailure::Orchestration {
                cause: TransportCause::Agent,
                message: OpenshellCliError::AgentExit { code: 1 }.to_string(),
            },
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
    fn a_plugin_stop_is_a_stopped_turn_through_any_context() {
        let wedged = crate::agent::tool_chain::Wedged {
            calls: vec!["Bash `$ go list -deps ./cmd/server`".into()],
            repeats: 25,
        };
        let reason = StopReason::Wedged(wedged);
        let e = anyhow::Error::from(reason.clone()).context("running the agent in sandbox s-1");
        assert_eq!(failure_of(e), TurnFailure::Stopped(reason));
        let other = anyhow::Error::from(OpenshellCliError::Failed {
            label: "workdir upload".into(),
            stderr: "connection reset".into(),
        })
        .context("uploading the workspace");
        assert!(matches!(
            failure_of(other),
            TurnFailure::Orchestration { message, .. } if message.contains("connection reset")
        ));
    }

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
            let (sandbox_root, glob) = harness
                .spec()
                .transcript
                .jsonl_tree()
                .unwrap_or_else(|| panic!("{harness:?} reads a jsonl tree"));
            let script = newest_jsonl_script(sandbox_root, glob);
            assert!(script.starts_with("ls -1t "), "{script}");
            assert!(script.ends_with(" 2>/dev/null | head -n 1"), "{script}");
            let pattern = script
                .trim_start_matches("ls -1t ")
                .trim_end_matches(" 2>/dev/null | head -n 1");
            assert_eq!(pattern, format!("{sandbox_root}/{glob}"));
        }
        assert_eq!(
            newest_jsonl_script("/sandbox/.claude/projects", "*/*.jsonl"),
            "ls -1t /sandbox/.claude/projects/*/*.jsonl 2>/dev/null | head -n 1"
        );
        assert_eq!(
            newest_jsonl_script("/sandbox/.codex/sessions", "*/*/*/rollout-*.jsonl"),
            "ls -1t /sandbox/.codex/sessions/*/*/*/rollout-*.jsonl 2>/dev/null | head -n 1"
        );
    }

    const ANTHROPIC_KEY: &str = "sk-ant-real-0123456789abcdef";
    const OPENAI_KEY: &str = "sk-oai-real-0123456789abcdef";
    const CODEX_KEY: &str = "sk-codex-real-0123456789abcdef";
    const PLACEHOLDER: &str = "the-placeholder-the-gateway-issued";

    fn inference(
        anthropic_key: Option<&str>,
        anthropic_url: Option<&str>,
        openai_key: Option<&str>,
        openai_url: Option<&str>,
    ) -> crate::agent::inference::InferenceEnv {
        crate::agent::inference::InferenceEnv {
            anthropic_key: anthropic_key.map(str::to_string),
            anthropic_base_url: anthropic_url.map(str::to_string),
            openai_key: openai_key.map(str::to_string),
            openai_base_url: openai_url.map(str::to_string),
            ..Default::default()
        }
    }

    fn agent<'a>(
        harness: Harness,
        model: &'a str,
        codex: &'a crate::manifest::CodexCfg,
    ) -> crate::args::TurnAgent<'a> {
        crate::args::TurnAgent {
            harness,
            model,
            reasoning_effort: None,
            disallowed_tools: &[],
            allowed_tools: &[],
            codex,
            env: &[],
        }
    }

    /// Every harness family on the auth it resolves for a keyed env: claude on a direct Anthropic
    /// key, codex on its API key, opencode and pi on the endpoint's key.
    fn keyed_turns() -> Vec<(Harness, AuthProvider, SandboxAuth)> {
        vec![
            (
                Harness::Claude,
                AuthProvider::AnthropicKey,
                SandboxAuth::AnthropicKey,
            ),
            (
                Harness::Codex,
                AuthProvider::Codex,
                SandboxAuth::Codex(provider::CodexAuth::ApiKey(CODEX_KEY.into())),
            ),
            (Harness::OpenCode, AuthProvider::ApiKey, SandboxAuth::ApiKey),
            (Harness::Pi, AuthProvider::ApiKey, SandboxAuth::ApiKey),
        ]
    }

    fn assert_no_real_key(text: &str) {
        for key in [ANTHROPIC_KEY, OPENAI_KEY, CODEX_KEY] {
            assert!(!text.contains(key), "{text}");
        }
    }

    #[test]
    fn a_direct_key_turn_drops_the_vertex_selectors_and_never_carries_the_key() {
        let mut env = SandboxEnv::from_manifest([
            ("CLAUDE_CODE_USE_VERTEX", "1"),
            ("ANTHROPIC_VERTEX_PROJECT_ID", "proj"),
            ("ANTHROPIC_API_KEY", "set-by-the-manifest"),
            ("KEEP_ME", "yes"),
        ]);
        let inference = inference(
            Some(ANTHROPIC_KEY),
            Some("https://claude.corp/v1"),
            None,
            None,
        );
        inference_env(
            &mut env,
            Harness::Claude,
            AuthProvider::AnthropicKey,
            &inference,
            Some(provider::InferenceKeyVar::Anthropic),
        );
        let get = |k: &str| env.get(k);
        assert_eq!(get("CLAUDE_CODE_USE_VERTEX"), None);
        assert_eq!(get("ANTHROPIC_VERTEX_PROJECT_ID"), None);
        assert_eq!(get("KEEP_ME"), Some("yes"));
        assert_eq!(
            get("ANTHROPIC_API_KEY"),
            None,
            "the gateway's placeholder is the only value the variable may hold"
        );
        assert_eq!(get("ANTHROPIC_BASE_URL"), Some("https://claude.corp/v1"));
    }

    #[test]
    fn no_harness_family_writes_a_key_into_the_env_script() {
        let codex = crate::manifest::CodexCfg::default();
        let inference = inference(
            Some(ANTHROPIC_KEY),
            Some("https://claude.corp/v1"),
            Some(OPENAI_KEY),
            Some("http://vllm.internal:8000/v1"),
        );
        for (harness, auth, sandbox_auth) in keyed_turns() {
            let agent = agent(harness, "claude-opus-4-6", &codex);
            let key = InferenceKey::resolve(auth, &inference, &sandbox_auth, &agent)
                .unwrap()
                .unwrap_or_else(|| panic!("{harness:?} resolves a key"));
            let mut env = SandboxEnv::from_manifest([(key.var.env(), "manifest-value")]);
            inference_env(&mut env, harness, auth, &inference, Some(key.var));
            let script = env.script(harness.spec());
            assert_no_real_key(&script);
            assert!(!script.contains(key.var.env()), "{harness:?}: {script}");
            assert!(
                script.contains("https://claude.corp/v1")
                    || script.contains("http://vllm.internal:8000/v1"),
                "{harness:?}: {script}"
            );
        }
    }

    #[test]
    fn each_auth_resolves_the_key_its_harness_sends_and_where_it_sends_it() {
        use provider::InferenceKeyVar::{Anthropic, OpenAi};
        let codex = crate::manifest::CodexCfg::default();
        let codex_key = SandboxAuth::Codex(provider::CodexAuth::ApiKey(CODEX_KEY.into()));
        let chatgpt = SandboxAuth::Codex(provider::CodexAuth::ChatGpt(provider::CodexToken {
            access_token: "at".into(),
            account_id: "acct".into(),
            id_token: "id".into(),
        }));
        let enmaas = Some("https://api.enmaas.devshift.net/v1");
        let vllm = Some("http://vllm.internal:8000/v1");
        let cases: Vec<(_, _, _, _, _, Option<(_, &str, &str)>)> = vec![
            (
                Harness::Claude,
                "claude-opus-4-6",
                AuthProvider::AnthropicKey,
                &SandboxAuth::AnthropicKey,
                inference(Some(ANTHROPIC_KEY), enmaas, None, None),
                Some((Anthropic, ANTHROPIC_KEY, "api.enmaas.devshift.net:443")),
            ),
            (
                Harness::Claude,
                "claude-opus-4-6",
                AuthProvider::AnthropicKey,
                &SandboxAuth::AnthropicKey,
                inference(Some(ANTHROPIC_KEY), None, None, None),
                Some((Anthropic, ANTHROPIC_KEY, "api.anthropic.com:443")),
            ),
            (
                Harness::Claude,
                "claude-opus-4-6",
                AuthProvider::Vertex,
                &SandboxAuth::Gateway,
                inference(None, None, None, None),
                None,
            ),
            (
                Harness::Codex,
                "gpt-5.6-sol",
                AuthProvider::Codex,
                &codex_key,
                inference(None, None, Some(OPENAI_KEY), vllm),
                Some((OpenAi, CODEX_KEY, "vllm.internal:8000")),
            ),
            (
                Harness::Codex,
                "gpt-5.6-sol",
                AuthProvider::Codex,
                &codex_key,
                inference(None, None, None, None),
                Some((OpenAi, CODEX_KEY, "api.openai.com:443")),
            ),
            (
                Harness::Codex,
                "gpt-5.6-sol",
                AuthProvider::Codex,
                &chatgpt,
                inference(None, None, None, None),
                None,
            ),
            (
                Harness::OpenCode,
                "rits/zai-org/glm-5-3",
                AuthProvider::ApiKey,
                &SandboxAuth::ApiKey,
                inference(Some(ANTHROPIC_KEY), None, Some(OPENAI_KEY), enmaas),
                Some((OpenAi, OPENAI_KEY, "api.enmaas.devshift.net:443")),
            ),
            (
                Harness::Pi,
                "claude-opus-4-6",
                AuthProvider::ApiKey,
                &SandboxAuth::ApiKey,
                inference(Some(ANTHROPIC_KEY), None, None, None),
                Some((Anthropic, ANTHROPIC_KEY, "api.anthropic.com:443")),
            ),
            (
                Harness::OpenCode,
                "claude-opus-4-6",
                AuthProvider::ApiKey,
                &SandboxAuth::ApiKey,
                inference(Some(ANTHROPIC_KEY), None, Some(OPENAI_KEY), None),
                Some((Anthropic, ANTHROPIC_KEY, "api.anthropic.com:443")),
            ),
            (
                Harness::Pi,
                "gpt-5.6-sol",
                AuthProvider::ApiKey,
                &SandboxAuth::ApiKey,
                inference(Some(ANTHROPIC_KEY), None, Some(OPENAI_KEY), None),
                Some((OpenAi, OPENAI_KEY, "api.openai.com:443")),
            ),
            (
                Harness::OpenCode,
                "rits/zai-org/glm-5-3",
                AuthProvider::ApiKey,
                &SandboxAuth::ApiKey,
                inference(Some(ANTHROPIC_KEY), None, None, vllm),
                None,
            ),
        ];
        for (harness, model, auth, sandbox_auth, inference, want) in &cases {
            let agent = agent(*harness, model, &codex);
            let got = InferenceKey::resolve(*auth, inference, sandbox_auth, &agent).unwrap();
            let got = got.map(|k| {
                (
                    k.var,
                    k.value,
                    format!("{}:{}", k.endpoint.host, k.endpoint.port),
                )
            });
            let want = want.map(|(var, value, at)| (var, value, at.to_string()));
            assert_eq!(got, want, "{harness:?} {auth:?} {model}");
        }
    }

    #[test]
    fn a_key_binds_only_to_an_exactly_allowlisted_endpoint_the_proxy_inspects() {
        let key = InferenceKey {
            var: provider::InferenceKeyVar::Anthropic,
            value: ANTHROPIC_KEY,
            endpoint: "api.anthropic.com:443:full".parse().unwrap(),
        };
        let allow = |specs: &[&str]| specs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let binding = key
            .binding(
                &allow(&["github.com:443:full", "api.anthropic.com:443:read-write"]),
                &[],
            )
            .expect("allowlisted");
        assert_eq!(binding.host, "api.anthropic.com");
        assert_eq!(binding.port, 443);
        assert_eq!(binding.provider, provider::ANTHROPIC_KEY_PROVIDER_NAME);
        let not_allowlisted = Err(InferenceKeyError::NotAllowlisted {
            endpoint: "api.anthropic.com:443".into(),
        });
        for endpoints in [
            allow(&[]),
            allow(&["*.anthropic.com:443:full"]),
            allow(&["api.anthropic.com:8443:full"]),
        ] {
            assert_eq!(
                key.binding(&endpoints, &[]).map(|b| b.provider),
                not_allowlisted.clone(),
                "{endpoints:?}"
            );
        }
        assert_eq!(
            key.binding(
                &allow(&["api.anthropic.com:443:full"]),
                &allow(&["api.anthropic.com:443"])
            )
            .map(|b| b.provider),
            Err(InferenceKeyError::TlsSkipped {
                endpoint: "api.anthropic.com:443".into(),
            })
        );
        assert!(
            key.binding(
                &allow(&["api.anthropic.com:443:full"]),
                &allow(&["registry.internal:443"])
            )
            .is_ok()
        );
    }

    #[test]
    fn no_seeded_file_carries_a_real_key_for_any_harness() {
        let codex = crate::manifest::CodexCfg::default();
        let inference = inference(
            Some(ANTHROPIC_KEY),
            Some("https://claude.corp/v1"),
            Some(OPENAI_KEY),
            Some("http://vllm.internal:8000/v1"),
        );
        let broker = provider::broker_token_placeholder();
        let servers = [McpServer {
            name: "broker",
            url: "http://host.openshell.internal:8849/mcp",
            token: Some(&broker),
        }];
        for (harness, _, sandbox_auth) in keyed_turns() {
            let agent = agent(harness, "claude-opus-4-6", &codex);
            for issued in [Some(PLACEHOLDER.to_string()), None] {
                let seeds = backend(harness).seed_files(
                    &agent,
                    &servers,
                    &sandbox_auth.seeded(issued.clone()),
                    &inference,
                );
                for seed in &seeds {
                    assert_no_real_key(&seed.content);
                }
                if harness == Harness::Codex && issued.is_some() {
                    let auth = seeds
                        .iter()
                        .find(|s| s.dest == crate::agent::harness::codex::AUTH)
                        .expect("codex seeds auth.json");
                    assert!(auth.content.contains(PLACEHOLDER), "{}", auth.content);
                }
            }
        }
    }

    #[test]
    fn a_failure_never_carries_a_registered_secret() {
        let redactor = Redactor::default();
        redactor.add(OPENAI_KEY);
        let orchestration = masked(
            TurnFailure::Orchestration {
                cause: TransportCause::Other,
                message: format!("openshell seed upload failed: {OPENAI_KEY}"),
            },
            &redactor,
        );
        let wedged = masked(
            TurnFailure::Stopped(StopReason::Wedged(crate::agent::tool_chain::Wedged {
                calls: vec![format!(
                    "Bash `$ curl -H 'Authorization: Bearer {OPENAI_KEY}'`"
                )],
                repeats: 25,
            })),
            &redactor,
        );
        let spawn = masked(TurnFailure::Spawn(OPENAI_KEY.into()), &redactor);
        for failure in [orchestration, wedged, spawn] {
            let shown = format!("{failure} {failure:?}");
            assert_no_real_key(&shown);
            assert!(shown.contains(crucible_harness::redact::MASK), "{shown}");
        }
    }

    #[test]
    fn private_session_locator_is_pinned_to_claude_and_the_opaque_id() {
        let session = crate::agent::agent_session::SessionTurn {
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
