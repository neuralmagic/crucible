//! The typed gRPC boundary to the local OpenShell gateway: Health / sandbox / provider / policy /
//! exec / logs RPCs over a lazily-connected mTLS [`Channel`], all natively `async` and awaited
//! from [`crate::openshell::run::turn`]'s one `block_on` on the shared engine runtime.
//! [`Gateway::exec`] consumes the tonic server stream directly, handing stdout lines to a caller
//! callback and `select!`ing a [`CancellationToken`] so Ctrl-C drops the stream and cancels the
//! RPC server-side (there is no local child to signal). Boot and file upload/download stay on the
//! CLI, see [`crate::openshell::gateway`] and [`crate::openshell::run`].

use crate::openshell::gateway::{GATEWAY_NAME, GATEWAY_PORT};
use anyhow::{Context, Result};
use openshell_core::auth::EdgeAuthInterceptor;
use openshell_core::proto::open_shell_client::OpenShellClient;
use openshell_core::proto::{
    AddNetworkRule, ConfigureProviderRefreshRequest, CreateProviderRequest, CreateSandboxRequest,
    DeleteSandboxRequest, ExecSandboxRequest, FilesystemPolicy, GetProviderRequest,
    GetSandboxLogsRequest, GetSandboxPolicyStatusRequest, GetSandboxRequest, HealthRequest,
    NetworkBinary, NetworkEndpoint, NetworkPolicyRule, PolicyMergeOperation, PolicyStatus,
    Provider, ProviderCredentialRefreshStrategy, RotateProviderCredentialRequest, SandboxCondition,
    SandboxLogLine, SandboxPhase, SandboxPolicy, SandboxSpec, SandboxTemplate, ServiceStatus,
    UpdateConfigRequest, UpdateProviderRequest, exec_sandbox_event::Payload as ExecPayload,
    policy_merge_operation::Operation as MergeOp,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

/// The concrete client type: the generated OpenShell client over the mTLS channel, wrapped in the
/// trace-propagating interceptor (which composes the no-op, mTLS-only edge auth).
type Client = OpenShellClient<InterceptedService<Channel, TracingInterceptor>>;

/// A tonic metadata injector for W3C trace context (`opentelemetry::propagation::Injector`).
struct MetadataInjector<'a>(&'a mut tonic::metadata::MetadataMap);

impl opentelemetry::propagation::Injector for MetadataInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let Ok(name) = tonic::metadata::MetadataKey::from_bytes(key.as_bytes())
            && let Ok(val) = value.parse()
        {
            self.0.insert(name, val);
        }
    }
}

/// Composes the edge auth interceptor with W3C trace-context injection: it injects
/// `traceparent`/`tracestate` from the active tracing span's OTel context into each RPC's metadata.
///
/// The gateway at the pinned rev does NOT extract this server-side yet (verified: no extraction on
/// the fork), so this is future-proofing for when the fork/upstream grows context extraction, the
/// spans would then stitch across the RPC boundary. When no OTLP layer is installed the global
/// propagator is the no-op default, so injection adds nothing beyond a cheap context lookup: zero
/// per-call overhead in the common (telemetry-off) case.
#[derive(Clone)]
struct TracingInterceptor {
    inner: EdgeAuthInterceptor,
}

impl tonic::service::Interceptor for TracingInterceptor {
    fn call(
        &mut self,
        req: tonic::Request<()>,
    ) -> std::result::Result<tonic::Request<()>, tonic::Status> {
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        let mut req = self.inner.call(req)?;
        let cx = tracing::Span::current().context();
        opentelemetry::global::get_text_map_propagator(|prop| {
            prop.inject_context(&cx, &mut MetadataInjector(req.metadata_mut()));
        });
        Ok(req)
    }
}

/// The poll-loop client: same channel and the same no-op edge auth, but its trace context rides
/// with the sampled flag cleared, see [`PollInterceptor`].
type PollClient = OpenShellClient<InterceptedService<Channel, PollInterceptor>>;

/// The interceptor for status-poll ticks. Same auth as [`TracingInterceptor`], but it injects the
/// current trace context with the sampled flag OFF.
///
/// `wait_ready` and the policy wait tick every 500ms to 1s for minutes, and the gateway turns each tick
/// into a span: a five-minute turn's waterfall came back with ~570 identical 0ms children. Dropping
/// injection entirely would only demote those children into ~570 root traces; an unsampled parent
/// makes the gateway's `ParentBased` sampler drop the span outright, while the trace id still lands
/// in its logs. The loop's story goes on the enclosing client span instead, see
/// [`record_poll_summary`].
#[derive(Clone)]
struct PollInterceptor {
    inner: EdgeAuthInterceptor,
}

impl tonic::service::Interceptor for PollInterceptor {
    fn call(
        &mut self,
        req: tonic::Request<()>,
    ) -> std::result::Result<tonic::Request<()>, tonic::Status> {
        use opentelemetry::trace::TraceContextExt as _;
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        let mut req = self.inner.call(req)?;
        let active = tracing::Span::current().context();
        let sampled_off = unsample(active.span().span_context());
        // An invalid span context (no OTLP layer installed) injects nothing.
        let cx = opentelemetry::Context::current().with_remote_span_context(sampled_off);
        opentelemetry::global::get_text_map_propagator(|prop| {
            prop.inject_context(&cx, &mut MetadataInjector(req.metadata_mut()));
        });
        Ok(req)
    }
}

/// The same span context with the sampled flag cleared: same trace, "do not record" downstream.
fn unsample(cx: &opentelemetry::trace::SpanContext) -> opentelemetry::trace::SpanContext {
    opentelemetry::trace::SpanContext::new(
        cx.trace_id(),
        cx.span_id(),
        opentelemetry::trace::TraceFlags::default(),
        cx.is_remote(),
        cx.trace_state().clone(),
    )
}

/// How a status-poll loop ended, recorded as the `outcome` attribute on the enclosing span.
const POLL_READY: &str = "ready";
const POLL_TIMEOUT: &str = "timeout";
const POLL_ERROR: &str = "error";

/// Record a poll loop's story on the enclosing span: attempts, elapsed, and how it ended. This is
/// what replaces the per-tick child spans the polls used to leave behind.
fn record_poll_summary(attempts: u64, started: Instant, outcome: &str) {
    let span = tracing::Span::current();
    span.record("attempts", attempts);
    span.record(
        "elapsed_ms",
        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    );
    span.record("outcome", outcome);
}

/// How long a sandbox gets to reach `Ready` once its image is pulled (mirrors the CLI's
/// `OPENSHELL_PROVISION_TIMEOUT`, default 300s).
fn provision_timeout() -> Duration {
    let secs = std::env::var("OPENSHELL_PROVISION_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    Duration::from_secs(secs)
}

/// The cap on the whole wait while the image may still be pulling. The first create after a node
/// roll pulls a multi-GB agent image, which routinely outlasts [`provision_timeout`]: counting the
/// short deadline from create killed the sandbox mid-pull and burned another full pull on the
/// retry. Overridable via `OPENSHELL_PULL_TIMEOUT`.
pub(crate) fn pull_timeout() -> Duration {
    let secs = std::env::var("OPENSHELL_PULL_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1800);
    Duration::from_secs(secs)
}

/// Condition reasons that prove the image is on the node. Kubelet reports the pull and the
/// container lifecycle through these; matched case-insensitively because the driver passes the
/// platform's spelling through untouched.
const PULL_DONE_REASONS: [&str; 3] = ["pulled", "started", "created"];

/// Condition types whose `True` status implies the containers are running, hence pulled.
const PULLED_IMPLYING_TYPES: [&str; 2] = ["ContainersReady", "Ready"];

/// Whether the sandbox's conditions carry evidence that the image pull finished. A driver that
/// never populates conditions yields no evidence, and the wait stays bounded by [`pull_timeout`].
fn pull_complete(conditions: &[SandboxCondition]) -> bool {
    conditions.iter().any(|c| {
        PULL_DONE_REASONS
            .iter()
            .any(|r| c.reason.eq_ignore_ascii_case(r))
            || (PULLED_IMPLYING_TYPES.contains(&c.r#type.as_str()) && c.status == "True")
    })
}

/// The instant the ready-wait gives up: the pull cap until pull completion is observed, then the
/// earlier of that cap and the provision window measured from the pull. Keeping the pull cap in
/// the `min` means late evidence cannot extend the wait past the outer bound.
fn ready_deadline(
    started: Instant,
    pull_done_at: Option<Instant>,
    pull_cap: Duration,
    ready_cap: Duration,
) -> Instant {
    let outer = started + pull_cap;
    match pull_done_at {
        None => outer,
        Some(t) => outer.min(t + ready_cap),
    }
}

/// The gateway's local mTLS client-cert directory for the registered gateway (`gateway add
/// --local --name ci` writes `ca.crt`/`tls.crt`/`tls.key` here). Honors `XDG_CONFIG_HOME`,
/// falling back to `~/.config`, exactly as the CLI resolves it.
fn mtls_dir() -> Result<PathBuf> {
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(x) if !x.is_empty() => PathBuf::from(x),
        _ => {
            let home = std::env::var("HOME").context("HOME unset; cannot locate gateway certs")?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base
        .join("openshell")
        .join("gateways")
        .join(GATEWAY_NAME)
        .join("mtls"))
}

/// The gateway URL. Local, TLS+mTLS, on the fixed bind port.
fn server_url() -> String {
    format!("https://localhost:{GATEWAY_PORT}")
}

/// Build a lazily-connecting mTLS channel from the gateway's local certs. Lazy means no I/O,
/// but `connect_lazy` still must run inside a tokio runtime context: the hyper executor it
/// registers spawns tasks via the ambient handle and panics without one, must run inside the
/// engine runtime context (the turn's `block_on` or a test runtime). Fails only when the certs
/// are unreadable (e.g. the gateway has not generated them yet, the health probe treats that as
/// "not up").
fn build_channel() -> Result<Channel> {
    let dir = mtls_dir()?;
    let ca = std::fs::read(dir.join("ca.crt"))
        .with_context(|| format!("reading gateway CA cert in {}", dir.display()))?;
    let cert = std::fs::read(dir.join("tls.crt"))
        .with_context(|| format!("reading gateway client cert in {}", dir.display()))?;
    let key = std::fs::read(dir.join("tls.key"))
        .with_context(|| format!("reading gateway client key in {}", dir.display()))?;
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(ca))
        .identity(Identity::from_pem(cert, key));
    let endpoint = Endpoint::from_shared(server_url())
        .context("parsing gateway endpoint URL")?
        .connect_timeout(Duration::from_secs(10))
        .tls_config(tls)
        .context("configuring gateway mTLS")?;
    Ok(endpoint.connect_lazy())
}

/// Wrap a channel in the OpenShell client. Edge auth is a no-op (the local gateway authenticates
/// the caller by its mTLS client cert, not a bearer token); the [`TracingInterceptor`] adds W3C
/// trace-context injection on top.
fn client_for(channel: Channel) -> Client {
    OpenShellClient::with_interceptor(
        channel,
        TracingInterceptor {
            inner: EdgeAuthInterceptor::noop(),
        },
    )
}

/// A one-shot gateway health probe: just a lazy mTLS channel. Cheap to build, so the boot poll
/// loop can call [`HealthProbe::healthy`] repeatedly. Constructed and awaited within the engine
/// runtime context (the engine runtime, entered by the turn's outer `block_on`).
pub struct HealthProbe {
    channel: Channel,
}

impl HealthProbe {
    /// Build a probe, or `None` when the gateway certs are not present yet (nothing to probe).
    /// Must be called within a tokio runtime context, `build_channel`'s `connect_lazy` registers
    /// the hyper executor on the ambient runtime.
    pub fn new() -> Option<Self> {
        let channel = build_channel().ok()?;
        Some(Self { channel })
    }

    /// Whether the gateway answers a `Health` RPC with a non-unhealthy status. Any transport or
    /// RPC error (gateway not listening, mTLS not ready) reads as "not up".
    pub async fn healthy(&self) -> bool {
        let mut client = client_for(self.channel.clone());
        match tokio::time::timeout(Duration::from_secs(5), client.health(HealthRequest {})).await {
            Ok(Ok(resp)) => !matches!(
                resp.into_inner().status(),
                ServiceStatus::Unhealthy | ServiceStatus::Unspecified
            ),
            _ => false,
        }
    }

    /// The gateway's self-reported version string (a git-describe form like
    /// `0.0.82-dev.11+gbc8342bb`, or a bare `X.Y.Z` on a release build). `None` when the
    /// `Health` RPC fails or reports an empty version.
    pub async fn report_version(&self) -> Option<String> {
        let mut client = client_for(self.channel.clone());
        match tokio::time::timeout(Duration::from_secs(5), client.health(HealthRequest {})).await {
            Ok(Ok(resp)) => Some(resp.into_inner().version).filter(|v| !v.is_empty()),
            _ => None,
        }
    }
}

/// The minimum gateway semver triple this client is known to work against. Bump it whenever
/// crucible starts calling an RPC an older gateway lacks: protobuf itself never versions, so an
/// old gateway doesn't fail the handshake, it returns UNIMPLEMENTED mid-turn. This constant is
/// the "minimum protocol version" that turns that late surprise into an upfront, actionable
/// failure. Comparison is on the numeric triple only (pre-release/`-dev` suffixes ignored):
/// `0.0.82-dev.11` counts as 0.0.82.
pub const MIN_GATEWAY_VERSION: (u64, u64, u64) = (0, 0, 82);

/// The openshell-core git rev this binary compiled against, embedded from Cargo.lock by
/// crucible's build script ("unknown" when the dep is not a git source).
pub const EXPECTED_GATEWAY_REV: &str = env!("CRUCIBLE_OPENSHELL_REV");

/// The version-gate verdict for a gateway's self-reported version string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionGate {
    /// Version parses, meets the minimum, and (when both sides are known) the embedded commit
    /// matches the compile-time rev.
    Ok,
    /// Version parses but is below [`MIN_GATEWAY_VERSION`], hard fail, the gateway image must
    /// be rebuilt from the pinned fork rev.
    TooOld { reported: String },
    /// Semver is new enough but the `+g<sha>` commit differs from the compile-time rev, warn:
    /// the RPC surface probably matches, the build provenance does not.
    RevMismatch { reported_commit: String },
    /// The version string didn't parse as `X.Y.Z[...]`, warn, never hard-fail (a format change
    /// upstream must not brick the loop).
    Unparseable { reported: String },
}

/// Gate a gateway's reported version against a minimum triple and an expected git rev. Pure,
/// so the older/equal/newer/garbage table is unit-testable without a live gateway.
fn gate_version(reported: &str, min: (u64, u64, u64), expected_rev: &str) -> VersionGate {
    let Some(triple) = parse_version_triple(reported) else {
        return VersionGate::Unparseable {
            reported: reported.to_string(),
        };
    };
    if triple < min {
        return VersionGate::TooOld {
            reported: reported.to_string(),
        };
    }
    // Rev comparison only when both sides are known: a bare release version carries no commit,
    // and a non-git openshell-core dep embeds "unknown", neither is a mismatch.
    if expected_rev != "unknown"
        && let Some(commit) = embedded_commit(reported)
        && !expected_rev.starts_with(&commit)
    {
        return VersionGate::RevMismatch {
            reported_commit: commit,
        };
    }
    VersionGate::Ok
}

/// Gate `reported` against this binary's [`MIN_GATEWAY_VERSION`] + [`EXPECTED_GATEWAY_REV`].
pub fn check_gateway_version(reported: &str) -> VersionGate {
    gate_version(reported, MIN_GATEWAY_VERSION, EXPECTED_GATEWAY_REV)
}

/// Parse the `X.Y.Z` numeric triple from a version string, tolerating a leading `v`
/// (git-describe strings are often v-prefixed; bare `v` is not valid semver). Deliberately the
/// TRIPLE, not a `semver::Version` with its `Ord`: strict semver ranks a pre-release below its
/// release (`0.0.82-dev.11 < 0.0.82`), which would fail the pinned rev against its own minimum.
///
/// When `semver` rejects the string, a lenient fallback takes the leading dotted triple before
/// any `-`/`+`: RPM release suffixes can carry identifiers semver forbids (an `.el10_2` dist
/// tag's underscore, for example), and those strings still gate meaningfully on their triple.
fn parse_version_triple(version: &str) -> Option<(u64, u64, u64)> {
    let v = version.trim().trim_start_matches('v');
    if let Ok(parsed) = semver::Version::parse(v) {
        return Some((parsed.major, parsed.minor, parsed.patch));
    }
    let core = v.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let triple = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(triple)
}

/// Extract the short commit from a git-describe version's build metadata (`…+g<sha>`, read via
/// `semver::Version::build`). `None` when the string isn't semver or the metadata isn't a
/// `g<hex>` commit.
fn embedded_commit(version: &str) -> Option<String> {
    let parsed = semver::Version::parse(version.trim().trim_start_matches('v')).ok()?;
    let commit = parsed.build.strip_prefix('g')?;
    (!commit.is_empty() && commit.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| commit.to_string())
}

/// The per-turn gRPC handle: one reused mTLS channel. Every method is `async`, awaited under the
/// turn's async flow (driven by the engine runtime's `block_on` at the turn boundary).
#[derive(Clone)]
pub struct Gateway {
    channel: Channel,
}

/// The tail of a completed sandbox exec: the command's stderr, replayed as lines after the
/// agent exits (the exit code is not surfaced, matching the old CLI-child path).
pub struct ExecResult {
    pub stderr_lines: Vec<String>,
}

impl Gateway {
    /// Connect to the local gateway (assumes [`crate::openshell::gateway::ensure_running`] already
    /// booted and registered it, so the certs exist). Must be called within a tokio runtime
    /// context, `build_channel`'s `connect_lazy` registers the hyper executor on the ambient
    /// runtime.
    pub fn connect() -> Result<Self> {
        let channel = build_channel()?;
        Ok(Self { channel })
    }

    fn client(&self) -> Client {
        client_for(self.channel.clone())
    }

    /// A client for the status-poll loops only: its ticks are invisible to the trace backend.
    fn poll_client(&self) -> PollClient {
        OpenShellClient::with_interceptor(
            self.channel.clone(),
            PollInterceptor {
                inner: EdgeAuthInterceptor::noop(),
            },
        )
    }

    /// Resolve a sandbox name to its gateway object id (`GetSandbox`, canonical name→id lookup).
    #[tracing::instrument(skip_all, fields(rpc = "get_sandbox", sandbox = name))]
    async fn sandbox_id(&self, name: &str) -> Result<String> {
        let mut client = self.client();
        let sandbox = client
            .get_sandbox(GetSandboxRequest {
                name: name.to_string(),
                ..Default::default()
            })
            .await
            .map_err(GrpcError::rpc(format!("get_sandbox({name})")))?
            .into_inner()
            .sandbox
            .ok_or_else(|| GrpcError::SandboxMissing {
                name: name.to_owned(),
            })?;
        sandbox
            .metadata
            .map(|m| m.id)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                GrpcError::SandboxHasNoId {
                    name: name.to_owned(),
                }
                .into()
            })
    }

    /// Create the sandbox and wait for it to reach `Ready`. `--no-auto-providers` is expressed
    /// by passing exactly the providers we manage (the gateway attaches only those). Labels ride
    /// the create request; the image lands on the template. No boot command is run, the
    /// sandbox stays up for a later [`Gateway::exec`], so the old `-- true` no-op is gone.
    #[tracing::instrument(skip_all, fields(rpc = "create_sandbox", sandbox = name))]
    pub async fn create_sandbox(
        &self,
        name: &str,
        image: Option<&str>,
        providers: &[String],
        labels: &[(String, String)],
        read_only_paths: &[String],
    ) -> Result<()> {
        let template = image.map(|img| SandboxTemplate {
            image: img.to_string(),
            ..SandboxTemplate::default()
        });
        // The filesystem half of the policy is STATIC: the gateway refuses to change it after
        // creation, so it rides the create request. The network half stays absent here and is
        // merged in afterwards by `update_policy_wait` (merge ops are additive, and only network
        // ops exist), so declaring paths cannot disturb egress. `include_workdir` must stay true —
        // it is what makes the agent's own workspace writable.
        let policy = sandbox_filesystem_policy(read_only_paths);
        let request = CreateSandboxRequest {
            spec: Some(SandboxSpec {
                providers: providers.to_vec(),
                template,
                policy,
                ..SandboxSpec::default()
            }),
            name: name.to_string(),
            labels: labels.iter().cloned().collect(),
            ..Default::default()
        };
        let mut client = self.client();
        client
            .create_sandbox(request)
            .await
            .map_err(GrpcError::rpc(format!("create_sandbox({name})")))?;
        self.wait_ready(name).await
    }

    /// Poll `GetSandbox` until the sandbox is `Ready` (or `Error`/timeout). A fresh create starts
    /// in `Provisioning`; the wait covers the in-pod image pull. The deadline is split: the
    /// generous [`pull_timeout`] until the status conditions show the pull finished, then
    /// [`provision_timeout`] from that point (see [`ready_deadline`]). The ticks ride the untraced
    /// [`Gateway::poll_client`]; the loop's story lands on this span.
    #[tracing::instrument(skip_all, fields(
        rpc = "wait_ready",
        sandbox = name,
        attempts = tracing::field::Empty,
        elapsed_ms = tracing::field::Empty,
        pull_ms = tracing::field::Empty,
        outcome = tracing::field::Empty,
    ))]
    async fn wait_ready(&self, name: &str) -> Result<()> {
        let started = Instant::now();
        let pull_cap = pull_timeout();
        let ready_cap = provision_timeout();
        let mut pull_done_at: Option<Instant> = None;
        let mut client = self.poll_client();
        let mut attempts: u64 = 0;

        let (outcome, result) = loop {
            attempts += 1;
            let status =
                match client
                    .get_sandbox(GetSandboxRequest {
                        name: name.to_string(),
                        ..Default::default()
                    })
                    .await
                {
                    Ok(resp) => resp.into_inner().sandbox.and_then(|s| s.status),
                    Err(s) => {
                        break (
                            POLL_ERROR,
                            Err(GrpcError::rpc(format!(
                                "get_sandbox({name}) while waiting for ready"
                            ))(s)
                            .into()),
                        );
                    }
                };
            let phase = status.as_ref().map_or(0, |s| s.phase);
            if pull_done_at.is_none()
                && status
                    .as_ref()
                    .is_some_and(|s| pull_complete(&s.conditions))
            {
                let now = Instant::now();
                pull_done_at = Some(now);
                tracing::Span::current().record(
                    "pull_ms",
                    u64::try_from((now - started).as_millis()).unwrap_or(u64::MAX),
                );
            }
            match SandboxPhase::try_from(phase) {
                Ok(SandboxPhase::Ready) => break (POLL_READY, Ok(())),
                Ok(SandboxPhase::Error) => {
                    break (
                        POLL_ERROR,
                        Err(GrpcError::SandboxErrorPhase {
                            name: name.to_owned(),
                        }
                        .into()),
                    );
                }
                _ => {}
            }
            if Instant::now() >= ready_deadline(started, pull_done_at, pull_cap, ready_cap) {
                let stage = match pull_done_at {
                    None => ReadyStage::PullIncomplete {
                        seconds: started.elapsed().as_secs(),
                    },
                    Some(t) => ReadyStage::AfterPull {
                        seconds: t.elapsed().as_secs(),
                    },
                };
                break (
                    POLL_TIMEOUT,
                    Err(GrpcError::SandboxNotReady {
                        name: name.to_owned(),
                        stage,
                    }
                    .into()),
                );
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };

        record_poll_summary(attempts, started, outcome);
        result
    }

    /// Delete the sandbox by name (`DeleteSandbox`). Idempotent: a missing sandbox is success
    /// (the RPC reports it as `deleted: false` or a NotFound status, both swallowed here); only
    /// other failures surface.
    #[tracing::instrument(skip_all, fields(rpc = "delete_sandbox", sandbox = name))]
    pub async fn delete_sandbox(&self, name: &str) -> Result<()> {
        let mut client = self.client();
        match client
            .delete_sandbox(DeleteSandboxRequest {
                name: name.to_string(),
                ..Default::default()
            })
            .await
        {
            Ok(_) => Ok(()),
            Err(s) if s.code() == tonic::Code::NotFound => Ok(()),
            Err(s) => Err(GrpcError::rpc(format!("delete_sandbox({name})"))(s).into()),
        }
    }

    /// Poll until the sandbox is actually GONE (`GetSandbox` returns NotFound). Deletion is
    /// asynchronous (finalizers, pod teardown), so a delete followed by an immediate create races
    /// the terminating CR and loses with "already exists" — once leaked, that collision burned
    /// every remaining iteration of a run. Bounded by the same provisioning timeout as
    /// [`Gateway::wait_ready`].
    #[tracing::instrument(skip_all, fields(rpc = "wait_deleted", sandbox = name))]
    pub async fn wait_deleted(&self, name: &str) -> Result<()> {
        let deadline = Instant::now() + provision_timeout();
        let mut client = self.poll_client();
        loop {
            match client
                .get_sandbox(GetSandboxRequest {
                    name: name.to_string(),
                    ..Default::default()
                })
                .await
            {
                Err(s) if s.code() == tonic::Code::NotFound => return Ok(()),
                Err(s) => {
                    return Err(GrpcError::rpc(format!(
                        "get_sandbox({name}) while waiting for delete"
                    ))(s)
                    .into());
                }
                Ok(_) => {}
            }
            if Instant::now() >= deadline {
                return Err(GrpcError::SandboxStillExists {
                    name: name.to_owned(),
                    seconds: provision_timeout().as_secs(),
                }
                .into());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Whether the provider exists (`GetProvider` succeeds). Create-on-first-turn,
    /// update-thereafter, same as the old `provider get` probe.
    #[tracing::instrument(skip_all, fields(rpc = "get_provider", provider = name))]
    pub async fn provider_exists(&self, name: &str) -> bool {
        let mut client = self.client();
        client
            .get_provider(GetProviderRequest {
                name: name.to_string(),
                ..Default::default()
            })
            .await
            .is_ok()
    }

    /// Register the `google-cloud` provider with a static minted token (`CreateProvider`). The
    /// token rides the provider's `credentials` map over the wire, never an argv, never logged.
    #[tracing::instrument(skip_all, fields(rpc = "create_provider", provider = name))]
    pub async fn create_provider(
        &self,
        name: &str,
        cred_key: &str,
        token: &str,
        provider_type: &str,
        project: &str,
        region: &str,
    ) -> Result<()> {
        let mut provider = build_provider(name, cred_key, token);
        provider.r#type = provider_type.to_string();
        provider.config = HashMap::from([
            ("project_id".to_string(), project.to_string()),
            ("region".to_string(), region.to_string()),
        ]);
        let mut client = self.client();
        client
            .create_provider(CreateProviderRequest {
                provider: Some(provider),
                ..Default::default()
            })
            .await
            .map(|_| ())
            // The token is in the request body, not this message; but keep any surfaced
            // error clean regardless.
            .map_err(GrpcError::rpc(format!("create_provider({name})")))
            .map_err(Into::into)
    }

    /// Register a provider whose credentials are gateway-minted (`CreateProvider` with an EMPTY
    /// credentials map): the refresh worker populates them once a refresh is configured. `r#type`
    /// names a provider profile (e.g. `aws-s3`), whose SigV4-signed egress endpoints attach with
    /// the provider, no hand-authored policy rule.
    #[tracing::instrument(skip_all, fields(rpc = "create_provider", provider = name))]
    pub async fn create_minted_provider(&self, name: &str, provider_type: &str) -> Result<()> {
        let provider = Provider {
            metadata: Some(openshell_core::proto::datamodel::v1::ObjectMeta {
                name: name.to_string(),
                ..Default::default()
            }),
            r#type: provider_type.to_string(),
            ..Default::default()
        };
        let mut client = self.client();
        client
            .create_provider(CreateProviderRequest {
                provider: Some(provider),
                ..Default::default()
            })
            .await
            .map(|_| ())
            .map_err(GrpcError::rpc(format!("create_provider({name})")))
            .map_err(Into::into)
    }

    /// Flip one global gateway setting (`UpdateConfig`, global scope, single-key mutation).
    /// Idempotent, re-setting the same value is a no-op server-side.
    #[tracing::instrument(skip_all, fields(rpc = "update_config", setting = key))]
    pub async fn set_global_bool_setting(&self, key: &str, value: bool) -> Result<()> {
        use openshell_core::proto::{SettingValue, setting_value};
        let mut client = self.client();
        client
            .update_config(UpdateConfigRequest {
                global: true,
                setting_key: key.to_string(),
                setting_value: Some(SettingValue {
                    value: Some(setting_value::Value::BoolValue(value)),
                }),
                ..Default::default()
            })
            .await
            .map(|_| ())
            .map_err(GrpcError::rpc(format!("update_config(setting {key})")))
            .map_err(Into::into)
    }

    /// Point a credential's refresh at an STS web-identity mint (`ConfigureProviderRefresh`).
    /// Configuring only STORES the refresh state, the worker mints on its next tick (≤60s), so
    /// follow with [`Gateway::rotate_provider_credential`] before the sandbox's first request
    /// (the proxy fails closed on unminted credentials).
    #[tracing::instrument(skip_all, fields(rpc = "configure_provider_refresh", provider = name))]
    pub async fn configure_provider_refresh(
        &self,
        name: &str,
        credential_key: &str,
        strategy: ProviderCredentialRefreshStrategy,
        material: HashMap<String, String>,
    ) -> Result<()> {
        let mut client = self.client();
        client
            .configure_provider_refresh(ConfigureProviderRefreshRequest {
                provider: name.to_string(),
                credential_key: credential_key.to_string(),
                strategy: strategy.into(),
                material,
                ..Default::default()
            })
            .await
            .map(|_| ())
            .map_err(GrpcError::rpc(format!(
                "configure_provider_refresh({name})"
            )))
            .map_err(Into::into)
    }

    /// Mint a credential synchronously right now (`RotateProviderCredential`), the pre-warm that
    /// closes the configure→worker-tick window.
    #[tracing::instrument(skip_all, fields(rpc = "rotate_provider_credential", provider = name))]
    pub async fn rotate_provider_credential(&self, name: &str, credential_key: &str) -> Result<()> {
        let mut client = self.client();
        client
            .rotate_provider_credential(RotateProviderCredentialRequest {
                provider: name.to_string(),
                credential_key: credential_key.to_string(),
                ..Default::default()
            })
            .await
            .map(|_| ())
            .map_err(GrpcError::rpc(format!(
                "rotate_provider_credential({name})"
            )))
            .map_err(Into::into)
    }

    /// Swap a freshly minted token into the existing provider (`UpdateProvider`). An empty
    /// `type` leaves the provider's type unchanged; only the one credential is replaced.
    #[tracing::instrument(skip_all, fields(rpc = "update_provider", provider = name))]
    pub async fn update_provider(&self, name: &str, cred_key: &str, token: &str) -> Result<()> {
        let provider = build_provider(name, cred_key, token);
        let mut client = self.client();
        client
            .update_provider(UpdateProviderRequest {
                provider: Some(provider),
                credential_expires_at_ms: HashMap::new(),
                ..Default::default()
            })
            .await
            .map(|_| ())
            .map_err(GrpcError::rpc(format!("update_provider({name})")))
            .map_err(Into::into)
    }

    /// Push the egress allowlist and block until the sandbox loads it (`UpdateConfig` +
    /// `GetSandboxPolicyStatus` poll, the gRPC equivalent of `policy update --wait`). Each
    /// endpoint becomes one incremental add-rule; the agent binaries attach to every rule. The
    /// `UpdateConfig` push is traced; the status ticks after it are not (they ride
    /// [`Gateway::poll_client`] and summarize onto this span).
    #[tracing::instrument(skip_all, fields(
        rpc = "update_config",
        phase = "policy_wait",
        sandbox = name,
        attempts = tracing::field::Empty,
        elapsed_ms = tracing::field::Empty,
        outcome = tracing::field::Empty,
    ))]
    pub async fn update_policy_wait(
        &self,
        name: &str,
        binaries: &[String],
        endpoints: &[String],
    ) -> Result<()> {
        let merge_operations = build_merge_operations(binaries, endpoints)?;
        let version = self
            .client()
            .update_config(UpdateConfigRequest {
                name: name.to_string(),
                merge_operations,
                ..UpdateConfigRequest::default()
            })
            .await
            .map(|r| r.into_inner().version)
            .map_err(GrpcError::rpc(format!("update_config({name})")))?;

        // Poll until the pushed revision is loaded (or superseded by a newer one, both mean the
        // supervisor is running a live policy), failing on a load error or timeout.
        let started = Instant::now();
        let deadline = started + Duration::from_secs(60);
        let mut client = self.poll_client();
        let mut attempts: u64 = 0;

        let (outcome, result) = loop {
            attempts += 1;
            let status = match client
                .get_sandbox_policy_status(GetSandboxPolicyStatusRequest {
                    name: name.to_string(),
                    version,
                    global: false,
                    ..Default::default()
                })
                .await
            {
                Ok(resp) => resp
                    .into_inner()
                    .revision
                    .map(|rev| (rev.status, rev.load_error)),
                Err(s) => {
                    break (
                        POLL_ERROR,
                        Err(GrpcError::rpc(format!("get_sandbox_policy_status({name})"))(s).into()),
                    );
                }
            };
            if let Some((code, load_error)) = status {
                match PolicyStatus::try_from(code) {
                    Ok(PolicyStatus::Loaded | PolicyStatus::Superseded) => {
                        break (POLL_READY, Ok(()));
                    }
                    Ok(PolicyStatus::Failed) => {
                        break (
                            POLL_ERROR,
                            Err(GrpcError::PolicyLoadFailed {
                                version,
                                load_error,
                            }
                            .into()),
                        );
                    }
                    _ => {}
                }
            }
            if Instant::now() >= deadline {
                break (
                    POLL_TIMEOUT,
                    Err(GrpcError::PolicyLoadTimeout {
                        version,
                        name: name.to_owned(),
                    }
                    .into()),
                );
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        };

        record_poll_summary(attempts, started, outcome);
        result
    }

    /// Fetch the sandbox's recent supervisor logs (`GetSandboxLogs`), best-effort. Returns an
    /// empty vec on any failure, denial telemetry must never fail the turn. Fetched at `INFO`
    /// (not `WARN`): the proxy's `CONNECT denied` events are OCSF-level, which the gateway ranks
    /// as INFO, so a WARN floor would drop them.
    #[tracing::instrument(skip_all, fields(rpc = "get_sandbox_logs", sandbox = name))]
    pub async fn sandbox_logs(&self, name: &str) -> Vec<SandboxLogLine> {
        let Ok(sandbox_id) = self.sandbox_id(name).await else {
            return Vec::new();
        };
        let mut client = self.client();
        client
            .get_sandbox_logs(GetSandboxLogsRequest {
                sandbox_id,
                lines: 1000,
                since_ms: 0,
                sources: vec!["sandbox".to_string()],
                min_level: "INFO".to_string(),
                ..Default::default()
            })
            .await
            .map(|r| r.into_inner().logs)
            .unwrap_or_default()
    }

    /// Stream `ExecSandbox` output. Consumes the tonic server stream directly: each stdout
    /// [`ExecSandboxEvent`](openshell_core::proto::ExecSandboxEvent) chunk is split into complete
    /// lines and handed to `on_stdout_line` (the caller feeds them to a
    /// [`crate::agent::StreamPump`]); stderr is collected and returned in the [`ExecResult`]. The
    /// exit code is not surfaced (the old CLI-child path ignored it too).
    ///
    /// Cancellation: `select!`s `cancel` against `stream.message()`. On trip the stream is dropped,
    /// which cancels the RPC server-side and kills the remote command (there is no local child to
    /// signal). Bytes and lines are preserved in order, and a clean stream end flushes any trailing
    /// partial stdout/stderr line.
    #[tracing::instrument(skip_all, fields(rpc = "exec_sandbox", phase = "exec", sandbox = name))]
    pub async fn exec(
        &self,
        name: &str,
        command: &[String],
        cancel: &CancellationToken,
        mut on_stdout_line: impl FnMut(&str),
    ) -> Result<ExecResult> {
        let sandbox_id = self.sandbox_id(name).await?;
        let request = ExecSandboxRequest {
            sandbox_id,
            command: command.to_vec(),
            ..ExecSandboxRequest::default()
        };

        // Open the stream first so a start failure (e.g. sandbox not ready) surfaces here rather
        // than half-way through the pump.
        let mut client = self.client();
        let mut stream = client
            .exec_sandbox(request)
            .await
            .map_err(GrpcError::rpc(format!("exec_sandbox({name})")))?
            .into_inner();

        let mut stdout = LineSplitter::default();
        let mut stderr = LineSplitter::default();
        let mut stderr_lines: Vec<String> = Vec::new();
        loop {
            tokio::select! {
                // `biased`: poll the stream arm first so a final output line that is already
                // ready gets drained even when cancel fires in the same tick. Cancellation still
                // wins on the next iteration, so Ctrl-C latency is bounded by one message-arm poll.
                biased;
                msg = stream.message() => match msg {
                    Ok(Some(event)) => match event.payload {
                        Some(ExecPayload::Stdout(out)) => {
                            stdout.push(&out.data, &mut on_stdout_line);
                        }
                        Some(ExecPayload::Stderr(err)) => {
                            stderr.push(&err.data, |line| stderr_lines.push(line.to_string()));
                        }
                        Some(ExecPayload::Exit(_)) | None => {}
                    },
                    Ok(None) => break,     // stream ended cleanly
                    Err(_status) => break, // transport/RPC error, end the pump
                },
                // Ctrl-C (via the run-module bridge): drop the stream, which cancels the RPC
                // server-side. Trailing partials are still flushed below.
                _ = cancel.cancelled() => break,
            }
        }
        // Flush any trailing (unterminated) partial lines.
        stdout.finish(&mut on_stdout_line);
        stderr.finish(|line| stderr_lines.push(line.to_string()));
        Ok(ExecResult { stderr_lines })
    }
}

/// The common `Provider` shape for the create/update RPCs: named metadata plus the one static
/// credential. `r#type` and `config` stay empty (update semantics); create patches them in.
fn build_provider(name: &str, cred_key: &str, token: &str) -> Provider {
    Provider {
        metadata: Some(openshell_core::proto::datamodel::v1::ObjectMeta {
            name: name.to_string(),
            ..Default::default()
        }),
        r#type: String::new(),
        credentials: HashMap::from([(cred_key.to_string(), token.to_string())]),
        config: HashMap::new(),
        credential_expires_at_ms: HashMap::new(),
        ..Default::default()
    }
}

/// The domain's declared paths ADDED to openshell's restrictive default, or `None` when it declared
/// none (the gateway then applies that same default itself).
///
/// Extending rather than authoring is load-bearing: the default is what grants `/usr`, `/etc`,
/// `/proc` and, critically, read-write `/dev/null`. A policy listing only the domain's paths takes
/// those away and the sandbox breaks before it runs anything — bash cannot initialize without
/// `/dev/null`, so every tool call fails with a shell that never started.
fn sandbox_filesystem_policy(read_only_paths: &[String]) -> Option<SandboxPolicy> {
    if read_only_paths.is_empty() {
        return None;
    }
    let mut policy = openshell_policy::restrictive_default_policy();
    let filesystem = policy
        .filesystem
        .get_or_insert_with(FilesystemPolicy::default);
    filesystem.include_workdir = true;
    for path in read_only_paths {
        if !filesystem.read_only.contains(path) {
            filesystem.read_only.push(path.clone());
        }
    }
    Some(policy)
}

/// Whether a supervisor log line reports a policy denial. The precise signals are the OCSF
/// network/HTTP `DENIED` action token (the proxy emits `CONNECT denied …` as an OCSF event,
/// which carries no structured fields, so its message is the marker) and any structured
/// `action`/`denial_stage` field a non-OCSF denial line might carry. A conservative message
/// substring stays as the backstop across denial stages (connect/forward/ssrf/l7/bypass).
pub fn is_denial(line: &SandboxLogLine) -> bool {
    if line.fields.get("action").is_some_and(|a| is_deny_action(a))
        || line.fields.contains_key("denial_stage")
    {
        return true;
    }
    let m = line.message.to_ascii_lowercase();
    m.contains("denied") || m.contains("denial") || m.contains("deny")
}

/// Whether a structured `action` field value names a denial.
fn is_deny_action(action: &str) -> bool {
    let a = action.to_ascii_lowercase();
    a.contains("deny") || a.contains("denied") || a.contains("block")
}

/// Build the incremental `UpdateConfig` merge operations: one add-rule per endpoint, each rule
/// carrying every agent binary. Mirrors the CLI's `policy update --binary … --add-endpoint …`
/// plan (the binaries attach to each added rule; descendants inherit egress from a parent).
fn build_merge_operations(
    binaries: &[String],
    endpoints: &[String],
) -> Result<Vec<PolicyMergeOperation>> {
    let net_binaries: Vec<NetworkBinary> = dedup(binaries)
        .into_iter()
        .map(|path| NetworkBinary {
            path,
            ..NetworkBinary::default()
        })
        .collect();
    endpoints
        .iter()
        .map(|spec| {
            let endpoint = parse_endpoint_spec(spec)?;
            let rule_name = generated_rule_name(&endpoint.host, endpoint.port);
            let rule = NetworkPolicyRule {
                name: rule_name.clone(),
                endpoints: vec![endpoint],
                binaries: net_binaries.clone(),
            };
            Ok(PolicyMergeOperation {
                operation: Some(MergeOp::AddRule(AddNetworkRule {
                    rule_name,
                    rule: Some(rule),
                })),
            })
        })
        .collect()
}

/// The rule name the CLI generates for an endpoint (`allow_<host>_<port>`, host punctuation
/// mapped to `_`). Kept byte-identical so a stored policy lines up with the CLI's.
fn generated_rule_name(host: &str, port: u32) -> String {
    let sanitized: String = host
        .replace(['.', '-'], "_")
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    format!("allow_{sanitized}_{port}")
}

/// Parse crucible's `host:port[:access[:protocol[:enforcement]]]` endpoint form (the shape
/// [`crate::openshell::policy`] resolves) into a [`NetworkEndpoint`], validating each segment the way the
/// CLI's `--add-endpoint` parser does. crucible does not emit the endpoint-options segment, so
/// this stops at enforcement.
/// A gateway RPC that came back non-OK. `call` names the RPC and its subject; tonic's `Status`
/// is the source, so the chain reads `create_sandbox(ci): status: ResourceExhausted, ...`.
#[derive(Debug, thiserror::Error)]
pub enum GrpcError {
    #[error("{call}")]
    Rpc {
        call: String,
        #[source]
        status: tonic::Status,
    },
    #[error("sandbox '{name}' missing from response")]
    SandboxMissing { name: String },
    #[error("sandbox '{name}' has no id")]
    SandboxHasNoId { name: String },
    #[error("sandbox '{name}' entered the error phase during provisioning")]
    SandboxErrorPhase { name: String },
    #[error("sandbox '{name}' {stage}")]
    SandboxNotReady { name: String, stage: ReadyStage },
    #[error("sandbox '{name}' still exists {seconds}s after delete")]
    SandboxStillExists { name: String, seconds: u64 },
    #[error("policy version {version} failed to load: {load_error}")]
    PolicyLoadFailed { version: u32, load_error: String },
    #[error("timed out waiting for policy version {version} to load on '{name}'")]
    PolicyLoadTimeout { version: u32, name: String },
}

/// Which half of the split ready deadline expired, so the message says whether the image never
/// landed (a slow registry or a dead node) or the sandbox stalled after the pull (a broken image
/// or supervisor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyStage {
    PullIncomplete { seconds: u64 },
    AfterPull { seconds: u64 },
}

impl std::fmt::Display for ReadyStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadyStage::PullIncomplete { seconds } => {
                write!(f, "not ready after {seconds}s (image pull incomplete)")
            }
            ReadyStage::AfterPull { seconds } => {
                write!(f, "not ready {seconds}s after image pull completed")
            }
        }
    }
}

impl GrpcError {
    /// A `map_err` argument: `.map_err(GrpcError::rpc(format!("get_sandbox({name})")))`.
    fn rpc(call: impl Into<String>) -> impl FnOnce(tonic::Status) -> Self {
        let call = call.into();
        move |status| GrpcError::Rpc { call, status }
    }
}

/// A rejected endpoint spec. The offending spec is named once here rather than re-interpolated
/// into every message.
#[derive(Debug, thiserror::Error, PartialEq)]
#[error("endpoint '{spec}' {problem}")]
pub struct EndpointSpecError {
    spec: String,
    problem: EndpointProblem,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum EndpointProblem {
    #[error("must be host:port[:access[:protocol[:enforcement]]]")]
    Shape,
    #[error("has an invalid host segment")]
    Host,
    #[error("has a non-numeric port")]
    NonNumericPort,
    #[error("port must be in 1..=65535")]
    PortRange,
    #[error("access must be read-only, read-write, or full")]
    Access,
    #[error("protocol must be rest, websocket, or sql")]
    Protocol,
    #[error("enforcement must be enforce or audit")]
    Enforcement,
    #[error("cannot set enforcement without a protocol")]
    EnforcementWithoutProtocol,
}

fn parse_endpoint_spec(spec: &str) -> Result<NetworkEndpoint, EndpointSpecError> {
    let reject = |problem| EndpointSpecError {
        spec: spec.to_owned(),
        problem,
    };
    let parts: Vec<&str> = spec.split(':').collect();
    if !(2..=5).contains(&parts.len()) {
        return Err(reject(EndpointProblem::Shape));
    }
    let host = parts[0].trim();
    if host.is_empty() || host.contains(char::is_whitespace) || host.contains('/') {
        return Err(reject(EndpointProblem::Host));
    }
    let port: u32 = parts[1]
        .trim()
        .parse()
        .map_err(|_| reject(EndpointProblem::NonNumericPort))?;
    if port == 0 || port > 65535 {
        return Err(reject(EndpointProblem::PortRange));
    }
    let access = parts.get(2).copied().unwrap_or("").trim();
    let protocol = parts.get(3).copied().unwrap_or("").trim();
    let enforcement = parts.get(4).copied().unwrap_or("").trim();
    if !access.is_empty() && !matches!(access, "read-only" | "read-write" | "full") {
        return Err(reject(EndpointProblem::Access));
    }
    if !protocol.is_empty() && !matches!(protocol, "rest" | "websocket" | "sql") {
        return Err(reject(EndpointProblem::Protocol));
    }
    if !enforcement.is_empty() && !matches!(enforcement, "enforce" | "audit") {
        return Err(reject(EndpointProblem::Enforcement));
    }
    if !enforcement.is_empty() && protocol.is_empty() {
        return Err(reject(EndpointProblem::EnforcementWithoutProtocol));
    }
    Ok(NetworkEndpoint {
        host: host.to_string(),
        port,
        ports: vec![port],
        protocol: protocol.to_string(),
        enforcement: enforcement.to_string(),
        access: access.to_string(),
        ..NetworkEndpoint::default()
    })
}

/// Dedupe while preserving first-seen order, dropping empties (matches the CLI's binary dedupe).
fn dedup(values: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for v in values {
        let t = v.trim();
        if !t.is_empty() && !out.iter().any(|e| e == t) {
            out.push(t.to_string());
        }
    }
    out
}

/// Split a stream of byte-chunks into complete lines, invoking a callback per line. Used for both
/// exec output streams: stdout lines flow to the [`crate::agent::StreamPump`], stderr lines into a
/// collected `Vec`. The async exec has no `impl Read` path, so it splits bytes into lines itself,
/// in order. The trailing partial (unterminated) line is flushed by
/// [`LineSplitter::finish`].
#[derive(Default)]
struct LineSplitter {
    pending: Vec<u8>,
}

impl LineSplitter {
    /// Feed a byte-chunk; call `on_line` once per complete (newline-terminated) line, in order.
    /// Invalid UTF-8 is replaced lossily and the line still flows, deliberate: a garbled byte
    /// must not stop the stream, unlike the old CLI reader that errored on the first bad UTF-8.
    fn push(&mut self, data: &[u8], mut on_line: impl FnMut(&str)) {
        for &b in data {
            if b == b'\n' {
                // Match `BufReader::lines`: a CRLF-terminated line yields the bare line, so drop
                // the `\r` that precedes the `\n`.
                if self.pending.last() == Some(&b'\r') {
                    self.pending.pop();
                }
                on_line(&String::from_utf8_lossy(&self.pending));
                self.pending.clear();
            } else {
                self.pending.push(b);
            }
        }
    }

    /// Flush any trailing partial line (bytes not yet terminated by a newline).
    fn finish(self, mut on_line: impl FnMut(&str)) {
        if !self.pending.is_empty() {
            on_line(&String::from_utf8_lossy(&self.pending));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway self-signed cert (CN=localhost, EC P-256) used only to exercise channel
    /// construction; no handshake ever happens and the key protects nothing.
    const TEST_CERT: &str = "-----BEGIN CERTIFICATE-----
MIIBfzCCASWgAwIBAgIUMrUAKSap7EEPSyPUvNJJlBKuecQwCgYIKoZIzj0EAwIw
FDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI2MDcxMjIwNTA0OFoYDzIxMjYwNjE4
MjA1MDQ4WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjO
PQMBBwNCAAQoCi2fOMqPco3TuNZ5gwxaZl4gCq8rNAs3Qi4IFMAAxYrhR37mDaC+
SF9/hSrEZ7SG15q5kMg1dWvOUN72/IgGo1MwUTAdBgNVHQ4EFgQUWHZzEykpCQ+B
Ju16mlLtw7xdJcUwHwYDVR0jBBgwFoAUWHZzEykpCQ+BJu16mlLtw7xdJcUwDwYD
VR0TAQH/BAUwAwEB/zAKBggqhkjOPQQDAgNIADBFAiAf5u/q+F+hgrxNt86Uc6KC
MywdKHgEBbqIe8VDfpXgqgIhAOYKPzUxFaOt2+omWpMeykO0MIqSnRh0AfxsisBk
pYBZ
-----END CERTIFICATE-----
";
    // A throwaway self-signed test key, generated for this fixture only. concat! keeps the
    // PEM header on a commentable line so scanners can be waved off inline.
    const TEST_KEY: &str = concat!(
        "-----BEGIN PRIVATE KEY-----\n", // gitleaks:allow
        "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgIAXG5IR2/OyX5u7Q\n",
        "rIW5swUeMZgzjyrDDrpO0Gi2IyuhRANCAAQoCi2fOMqPco3TuNZ5gwxaZl4gCq8r\n",
        "NAs3Qi4IFMAAxYrhR37mDaC+SF9/hSrEZ7SG15q5kMg1dWvOUN72/IgG\n",
        "-----END PRIVATE KEY-----\n",
    );

    /// Channel construction must run inside a tokio runtime context: `connect_lazy` builds the TLS
    /// connector and registers the hyper executor eagerly, which panics outside a runtime (and,
    /// without an explicit tonic crypto feature, on an ambiguous rustls provider). Both
    /// [`HealthProbe::new`] and [`Gateway::connect`] are called from within the turn's `block_on`
    /// on the engine runtime, this test pins that invariant by building them inside a real
    /// (test-owned) runtime's `block_on` and asserting the lazy channel builds without a live
    /// server. Real (throwaway) certs are required: a parse failure would return `Err` before
    /// reaching the panicking path and prove nothing.
    #[test]
    fn channel_construction_runs_on_the_engine_runtime() {
        let _guard = crate::test_env_lock();
        let dir = std::env::temp_dir().join(format!("crucible-grpc-certs-{}", std::process::id()));
        let mtls = dir.join("openshell/gateways/ci/mtls");
        std::fs::create_dir_all(&mtls).unwrap();
        std::fs::write(mtls.join("ca.crt"), TEST_CERT).unwrap();
        std::fs::write(mtls.join("tls.crt"), TEST_CERT).unwrap();
        std::fs::write(mtls.join("tls.key"), TEST_KEY).unwrap();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }

        // A stand-in engine runtime; production builds these under the turn's `block_on`.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (probe, gateway) = rt.block_on(async { (HealthProbe::new(), Gateway::connect()) });

        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        let _ = std::fs::remove_dir_all(&dir);

        assert!(probe.is_some(), "probe builds when the cert files exist");
        assert!(
            gateway.is_ok(),
            "gateway connects lazily without a live server: {:?}",
            gateway.err()
        );
    }

    #[test]
    fn mtls_dir_lands_under_the_named_gateway() {
        let _guard = crate::test_env_lock();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", "/cfg");
        }
        let dir = mtls_dir().unwrap();
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        assert_eq!(dir, PathBuf::from("/cfg/openshell/gateways/ci/mtls"));
    }

    /// The sandbox is landlock-confined to its workdir, so a domain that ships a pipeline or a
    /// toolbox elsewhere in the image must declare those paths or the agent gets `Permission
    /// denied` on files it can plainly see. The filesystem half is static (the gateway rejects
    /// changing it later), so it has to ride the create request, and `include_workdir` must survive
    /// — dropping it would take the agent's own workspace with it.
    #[test]
    fn declared_paths_ride_the_create_request_as_a_static_filesystem_policy() {
        let policy = sandbox_filesystem_policy(&[
            "/opt/ai_auto_analyze".to_string(),
            "/opt/crucible-tools".to_string(),
        ])
        .expect("declared paths must produce a policy");
        let fs = policy.filesystem.expect("filesystem half is set");
        assert!(fs.include_workdir, "the workspace must stay writable");
        for declared in ["/opt/ai_auto_analyze", "/opt/crucible-tools"] {
            assert!(
                fs.read_only.iter().any(|p| p == declared),
                "declared path {declared} must be readable: {:?}",
                fs.read_only
            );
        }
        // The default's paths must survive. /dev/null especially: without it the sandbox's shell
        // cannot initialize, so every tool call fails before it runs.
        assert!(
            fs.read_write.iter().any(|p| p == "/dev/null"),
            "/dev/null must stay read-write: {:?}",
            fs.read_write
        );
        for base in ["/usr", "/etc", "/proc"] {
            assert!(
                fs.read_only.iter().any(|p| p == base),
                "default path {base} must survive: {:?}",
                fs.read_only
            );
        }
        assert!(
            policy.network_policies.is_empty(),
            "egress is merged in later; creating must not assert a network half"
        );
    }

    /// No declared paths -> no policy at all, so the sandbox keeps the gateway's default rather
    /// than a crucible-authored one that happens to be empty.
    #[test]
    fn no_declared_paths_sends_no_policy() {
        assert!(sandbox_filesystem_policy(&[]).is_none());
    }

    #[test]
    fn generated_rule_name_sanitizes_host_punctuation() {
        assert_eq!(
            generated_rule_name("aiplatform.googleapis.com", 443),
            "allow_aiplatform_googleapis_com_443"
        );
        assert_eq!(
            generated_rule_name("*.github.com", 443),
            "allow__github_com_443"
        );
    }

    #[test]
    fn parse_endpoint_l4_access_only() {
        let ep = parse_endpoint_spec("github.com:443:full").unwrap();
        assert_eq!(ep.host, "github.com");
        assert_eq!(ep.port, 443);
        assert_eq!(ep.ports, vec![443]);
        assert_eq!(ep.access, "full");
        assert!(ep.protocol.is_empty());
        assert!(ep.enforcement.is_empty());
    }

    #[test]
    fn parse_endpoint_with_protocol_and_enforcement() {
        let ep = parse_endpoint_spec("api.example.com:443:read-write:rest:enforce").unwrap();
        assert_eq!(ep.access, "read-write");
        assert_eq!(ep.protocol, "rest");
        assert_eq!(ep.enforcement, "enforce");
    }

    #[test]
    fn parse_endpoint_rejects_bad_segments() {
        assert!(parse_endpoint_spec("host-only").is_err());
        assert!(parse_endpoint_spec("host:notaport").is_err());
        assert!(parse_endpoint_spec("host:70000").is_err());
        assert!(parse_endpoint_spec("host:443:write-ish").is_err());
        assert!(parse_endpoint_spec("host:443:full:ftp").is_err());
        // enforcement without a protocol segment is rejected.
        assert!(parse_endpoint_spec("host:443:full::enforce").is_err());
    }

    #[test]
    fn merge_operations_attach_binaries_to_every_rule() {
        let ops = build_merge_operations(
            &["/usr/local/bin/claude".to_string()],
            &[
                "github.com:443:full".to_string(),
                "aiplatform.googleapis.com:443:read-write".to_string(),
            ],
        )
        .unwrap();
        assert_eq!(ops.len(), 2);
        for op in &ops {
            let Some(MergeOp::AddRule(add)) = &op.operation else {
                panic!("expected an add-rule op");
            };
            let rule = add.rule.as_ref().unwrap();
            assert_eq!(add.rule_name, rule.name);
            assert_eq!(rule.binaries.len(), 1);
            assert_eq!(rule.binaries[0].path, "/usr/local/bin/claude");
            assert_eq!(rule.endpoints.len(), 1);
        }
        // Rule names are derived from the endpoint host+port.
        let names: Vec<&str> = ops
            .iter()
            .filter_map(|o| match &o.operation {
                Some(MergeOp::AddRule(a)) => Some(a.rule_name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            names,
            [
                "allow_github_com_443",
                "allow_aiplatform_googleapis_com_443"
            ]
        );
    }

    #[test]
    fn merge_operations_dedupe_binaries() {
        let ops = build_merge_operations(
            &[
                "/usr/local/bin/claude".to_string(),
                "/usr/local/bin/claude".to_string(),
                "".to_string(),
            ],
            &["github.com:443:full".to_string()],
        )
        .unwrap();
        let Some(MergeOp::AddRule(add)) = &ops[0].operation else {
            panic!("expected an add-rule op");
        };
        assert_eq!(
            add.rule.as_ref().unwrap().binaries.len(),
            1,
            "deduped + empties dropped"
        );
    }

    fn log_line(level: &str, message: &str, fields: &[(&str, &str)]) -> SandboxLogLine {
        SandboxLogLine {
            sandbox_id: "s".into(),
            timestamp_ms: 0,
            level: level.into(),
            target: "proxy".into(),
            message: message.into(),
            source: "sandbox".into(),
            fields: fields
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn is_denial_matches_ocsf_denied_action_token() {
        // The proxy's CONNECT-denied telemetry is an OCSF line with an empty fields map; the
        // DENIED action lives in the shorthand message.
        assert!(is_denial(&log_line(
            "OCSF",
            "NET:OPEN [MED] DENIED claude(42) -> evil.example.com:443 [policy:- engine:opa]",
            &[],
        )));
    }

    #[test]
    fn is_denial_matches_structured_fields() {
        assert!(is_denial(&log_line(
            "WARN",
            "connection blocked",
            &[("action", "deny")]
        )));
        assert!(is_denial(&log_line(
            "WARN",
            "blocked",
            &[("denial_stage", "bypass")]
        )));
    }

    #[test]
    fn is_denial_matches_message_substring_fallback() {
        assert!(is_denial(&log_line("WARN", "CONNECT denied host:443", &[])));
        assert!(is_denial(&log_line(
            "INFO",
            "ssrf_denied: internal address",
            &[]
        )));
    }

    #[test]
    fn is_denial_passes_clean_lines() {
        assert!(!is_denial(&log_line(
            "INFO",
            "uploaded workspace in 1.2s",
            &[]
        )));
        assert!(!is_denial(&log_line(
            "OCSF",
            "NET:OPEN [INF] ALLOWED claude(42) -> api.anthropic.com:443",
            &[]
        )));
    }

    // --- gateway version gate ---

    const REV: &str = "bc8342bb53730f43bd9e27fc2f890745fbcb607f";

    #[test]
    fn version_triple_parses_release_dev_and_v_prefixed_forms() {
        assert_eq!(parse_version_triple("0.0.82"), Some((0, 0, 82)));
        assert_eq!(
            parse_version_triple("0.0.82-dev.11+gbc8342bb"),
            Some((0, 0, 82))
        );
        assert_eq!(parse_version_triple("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version_triple(" 0.0.82 "), Some((0, 0, 82)));
        assert_eq!(parse_version_triple("0.0.82+gdeadbeef"), Some((0, 0, 82)));
    }

    #[test]
    fn version_triple_rejects_garbage() {
        assert_eq!(parse_version_triple(""), None);
        assert_eq!(parse_version_triple("m-dev"), None);
        assert_eq!(parse_version_triple("0.0"), None);
        assert_eq!(parse_version_triple("0.0.82.1"), None);
        assert_eq!(parse_version_triple("a.b.c"), None);
    }

    #[test]
    fn embedded_commit_reads_the_g_suffix_only() {
        assert_eq!(
            embedded_commit("0.0.82-dev.11+gbc8342bb").as_deref(),
            Some("bc8342bb")
        );
        assert_eq!(embedded_commit("0.0.82"), None);
        assert_eq!(embedded_commit("0.0.82+notgittish"), None);
        assert_eq!(embedded_commit("0.0.82+g"), None);
    }

    #[test]
    fn gate_passes_equal_and_newer_versions_with_matching_rev() {
        let min = (0, 0, 82);
        assert_eq!(
            gate_version("0.0.82-dev.11+gbc8342bb", min, REV),
            VersionGate::Ok
        );
        assert_eq!(
            gate_version("0.0.82", min, REV),
            VersionGate::Ok,
            "bare release, no commit"
        );
        assert_eq!(
            gate_version("0.1.0", min, REV),
            VersionGate::Ok,
            "newer minor"
        );
        assert_eq!(
            gate_version("1.0.0", min, REV),
            VersionGate::Ok,
            "newer major"
        );
    }

    #[test]
    fn gate_hard_fails_older_versions() {
        let min = (0, 0, 82);
        assert_eq!(
            gate_version("0.0.81", min, REV),
            VersionGate::TooOld {
                reported: "0.0.81".into()
            }
        );
        assert_eq!(
            gate_version("0.0.55-dev.9+gaada3da8", min, REV),
            VersionGate::TooOld {
                reported: "0.0.55-dev.9+gaada3da8".into()
            },
            "the old Copr build must be rejected"
        );
    }

    #[test]
    fn gate_handles_rpm_style_version_strings() {
        // The Copr NVR-ish shape: valid semver as it happens (every dash-separated identifier
        // is alphanumeric), so it gates through the semver path on its triple.
        let rpm =
            "0.0.55-1.20260605134545513516.devrobertsturlagce.metadata.emulator.11.gaada3da8.el10";
        assert_eq!(parse_version_triple(rpm), Some((0, 0, 55)));
        assert_eq!(
            gate_version(rpm, (0, 0, 82), REV),
            VersionGate::TooOld {
                reported: rpm.into()
            }
        );
        // An underscore-bearing dist tag (`.el10_2`) is NOT valid semver, this is the shape
        // the lenient fallback exists for; it still gates on the leading triple.
        let rpm_underscore = "0.0.83-1.el10_2";
        assert_eq!(parse_version_triple(rpm_underscore), Some((0, 0, 83)));
        assert_eq!(
            gate_version(rpm_underscore, (0, 0, 82), REV),
            VersionGate::Ok
        );
        assert_eq!(
            gate_version("0.0.55-1.el10_2", (0, 0, 82), REV),
            VersionGate::TooOld {
                reported: "0.0.55-1.el10_2".into()
            }
        );
    }

    #[test]
    fn gate_warns_on_rev_mismatch_when_semver_is_new_enough() {
        assert_eq!(
            gate_version("0.0.83-dev.2+gdeadbeef", (0, 0, 82), REV),
            VersionGate::RevMismatch {
                reported_commit: "deadbeef".into()
            }
        );
    }

    #[test]
    fn gate_skips_rev_comparison_when_either_side_is_unknown() {
        // Non-git dep: build.rs embedded "unknown", never a mismatch.
        assert_eq!(
            gate_version("0.0.83+gdeadbeef", (0, 0, 82), "unknown"),
            VersionGate::Ok
        );
        // Release build: no commit in the version string, never a mismatch.
        assert_eq!(gate_version("0.0.83", (0, 0, 82), REV), VersionGate::Ok);
    }

    #[test]
    fn gate_warns_never_fails_on_unparseable_versions() {
        assert_eq!(
            gate_version("m-dev", (0, 0, 82), REV),
            VersionGate::Unparseable {
                reported: "m-dev".into()
            }
        );
        assert_eq!(
            gate_version("", (0, 0, 82), REV),
            VersionGate::Unparseable {
                reported: String::new()
            }
        );
    }

    #[test]
    fn expected_rev_is_embedded_from_the_lockfile() {
        // build.rs parses Cargo.lock; on this branch the dep is a git source, so the embedded
        // value must be a full commit hash, not the fallback.
        assert_eq!(EXPECTED_GATEWAY_REV.len(), 40, "{EXPECTED_GATEWAY_REV}");
        assert!(EXPECTED_GATEWAY_REV.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn line_splitter_splits_across_chunks_in_order_and_flushes_trailing() {
        // A newline split mid-chunk, plus a trailing partial line the stream never terminates,
        // the exact shape claude's stream-json stdout arrives in over the gRPC exec stream.
        let mut ls = LineSplitter::default();
        let mut lines: Vec<String> = Vec::new();
        ls.push(b"one\ntw", |l| lines.push(l.to_string()));
        ls.push(b"o\nthree", |l| lines.push(l.to_string()));
        assert_eq!(lines, vec!["one", "two"], "complete lines emit in order");
        ls.finish(|l| lines.push(l.to_string()));
        assert_eq!(
            lines,
            vec!["one", "two", "three"],
            "the trailing partial flushes on finish"
        );
    }

    #[test]
    fn line_splitter_strips_crlf_like_bufreader() {
        // CRLF-terminated stdout must yield the bare line (matching `BufReader::lines`), while a
        // lone trailing `\r` at EOF (no newline) is left intact, same as `BufReader::lines`,
        // which only strips the `\r` that precedes a `\n`.
        let mut ls = LineSplitter::default();
        let mut lines: Vec<String> = Vec::new();
        ls.push(b"alpha\r\nbeta\r\ntail\r", |l| lines.push(l.to_string()));
        assert_eq!(lines, vec!["alpha", "beta"], "CRLF lines emit bare");
        ls.finish(|l| lines.push(l.to_string()));
        assert_eq!(
            lines,
            vec!["alpha", "beta", "tail\r"],
            "an unterminated trailing `\\r` is kept, like BufReader::lines"
        );
    }

    /// The poll ticks must ride the same trace but ask the gateway not to record: same ids, sampled
    /// flag cleared. A gateway on the default `ParentBased` sampler then drops the span instead of
    /// exporting ~570 of them under one turn.
    #[test]
    fn unsample_keeps_the_trace_but_clears_the_sampled_flag() {
        use opentelemetry::trace::{SpanContext, SpanId, TraceFlags, TraceId, TraceState};

        let sampled = SpanContext::new(
            TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").expect("valid trace id"),
            SpanId::from_hex("00f067aa0ba902b7").expect("valid span id"),
            TraceFlags::SAMPLED,
            false,
            TraceState::default(),
        );
        let quiet = unsample(&sampled);

        assert_eq!(quiet.trace_id(), sampled.trace_id());
        assert_eq!(quiet.span_id(), sampled.span_id());
        assert!(!quiet.is_sampled(), "poll ticks must not be sampled");
        assert!(
            quiet.is_valid(),
            "the context stays valid so the trace id still reaches the gateway's logs"
        );
    }

    /// An invalid (no-telemetry) context stays invalid, so the propagator injects nothing.
    #[test]
    fn unsample_leaves_an_invalid_context_invalid() {
        let quiet = unsample(&opentelemetry::trace::SpanContext::empty_context());
        assert!(!quiet.is_valid());
    }

    #[derive(Clone)]
    struct SpanBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SpanBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The loop's story replaces its per-tick spans, so it has to actually land on the enclosing
    /// span: attempts, elapsed_ms, outcome.
    #[test]
    fn poll_summary_lands_on_the_enclosing_span() {
        use tracing_subscriber::fmt::format::FmtSpan;
        use tracing_subscriber::layer::SubscriberExt as _;

        #[tracing::instrument(fields(
            attempts = tracing::field::Empty,
            elapsed_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
        ))]
        fn poll_loop() {
            record_poll_summary(7, Instant::now(), POLL_TIMEOUT);
        }

        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = SpanBuf(buf.clone());
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(move || writer.clone())
                .with_ansi(false)
                .with_span_events(FmtSpan::CLOSE),
        );
        let _guard = tracing::subscriber::set_default(subscriber);

        poll_loop();

        let output = String::from_utf8(buf.lock().expect("lock").clone()).expect("utf8");
        assert!(
            output.contains("attempts=7"),
            "tick count belongs on the span, got: {output}"
        );
        assert!(
            output.contains("outcome=\"timeout\""),
            "how the loop ended belongs on the span, got: {output}"
        );
        assert!(
            output.contains("elapsed_ms="),
            "how long it waited belongs on the span, got: {output}"
        );
    }

    fn condition(r#type: &str, status: &str, reason: &str) -> SandboxCondition {
        SandboxCondition {
            r#type: r#type.to_string(),
            status: status.to_string(),
            reason: reason.to_string(),
            message: String::new(),
            last_transition_time: String::new(),
        }
    }

    /// No conditions at all is the shape a driver that reports nothing produces: no evidence, so
    /// the wait must stay on the pull cap rather than assume the image landed.
    #[test]
    fn pull_complete_needs_evidence() {
        assert!(!pull_complete(&[]));
        assert!(!pull_complete(&[condition(
            "PodScheduled",
            "True",
            "Scheduled"
        )]));
        assert!(!pull_complete(&[condition("Ready", "False", "Pulling")]));
        assert!(!pull_complete(&[condition(
            "ContainersReady",
            "Unknown",
            ""
        )]));
    }

    /// Kubelet's spelling of the reason is passed through by the driver, so matching is
    /// case-insensitive on the whole pull/start/create family.
    #[test]
    fn pull_complete_reads_pull_reasons_case_insensitively() {
        for reason in ["Pulled", "pulled", "PULLED", "Started", "Created"] {
            assert!(
                pull_complete(&[condition("Whatever", "False", reason)]),
                "reason {reason} proves the image is on the node"
            );
        }
    }

    /// A running container implies a finished pull even when no reason survived the driver's
    /// translation, but only when the condition is actually true.
    #[test]
    fn pull_complete_reads_container_readiness_types() {
        assert!(pull_complete(&[condition("ContainersReady", "True", "")]));
        assert!(pull_complete(&[condition("Ready", "True", "")]));
        assert!(!pull_complete(&[condition("Ready", "false", "")]));
        assert!(!pull_complete(&[condition("ContainersReady", "False", "")]));
    }

    #[test]
    fn ready_deadline_without_evidence_is_the_pull_cap() {
        let started = Instant::now();
        let cap = Duration::from_secs(1800);
        assert_eq!(
            ready_deadline(started, None, cap, Duration::from_secs(300)),
            started + cap
        );
    }

    #[test]
    fn ready_deadline_after_early_evidence_is_the_ready_cap_from_the_pull() {
        let started = Instant::now();
        let pull = started + Duration::from_secs(60);
        assert_eq!(
            ready_deadline(
                started,
                Some(pull),
                Duration::from_secs(1800),
                Duration::from_secs(300)
            ),
            pull + Duration::from_secs(300)
        );
    }

    /// Evidence arriving just under the pull cap must not push the wait past it: a sandbox that
    /// pulls for 29 minutes still has to fail by 30, not by 35.
    #[test]
    fn ready_deadline_never_exceeds_the_pull_cap() {
        let started = Instant::now();
        let pull_cap = Duration::from_secs(1800);
        let pull = started + Duration::from_secs(1790);
        assert_eq!(
            ready_deadline(started, Some(pull), pull_cap, Duration::from_secs(300)),
            started + pull_cap
        );
    }

    #[test]
    fn ready_deadline_with_a_zero_ready_cap_expires_at_the_pull() {
        let started = Instant::now();
        let pull = started + Duration::from_secs(5);
        assert_eq!(
            ready_deadline(
                started,
                Some(pull),
                Duration::from_secs(1800),
                Duration::ZERO
            ),
            pull
        );
    }

    #[test]
    fn pull_timeout_defaults_to_thirty_minutes_and_honors_the_env() {
        let _guard = crate::test_env_lock();
        unsafe {
            std::env::remove_var("OPENSHELL_PULL_TIMEOUT");
        }
        assert_eq!(pull_timeout(), Duration::from_secs(1800));
        unsafe {
            std::env::set_var("OPENSHELL_PULL_TIMEOUT", "42");
        }
        let overridden = pull_timeout();
        // A garbage value falls back to the default rather than failing the turn.
        unsafe {
            std::env::set_var("OPENSHELL_PULL_TIMEOUT", "soon");
        }
        let garbage = pull_timeout();
        unsafe {
            std::env::remove_var("OPENSHELL_PULL_TIMEOUT");
        }
        assert_eq!(overridden, Duration::from_secs(42));
        assert_eq!(garbage, Duration::from_secs(1800));
    }

    /// The two timeouts have different fixes (slow registry vs broken image), so the message has
    /// to say which one fired.
    #[test]
    fn not_ready_message_names_the_stage() {
        let pulling = GrpcError::SandboxNotReady {
            name: "ci-abc".to_string(),
            stage: ReadyStage::PullIncomplete { seconds: 1800 },
        };
        assert_eq!(
            pulling.to_string(),
            "sandbox 'ci-abc' not ready after 1800s (image pull incomplete)"
        );
        let stalled = GrpcError::SandboxNotReady {
            name: "ci-abc".to_string(),
            stage: ReadyStage::AfterPull { seconds: 300 },
        };
        assert_eq!(
            stalled.to_string(),
            "sandbox 'ci-abc' not ready 300s after image pull completed"
        );
    }
}
