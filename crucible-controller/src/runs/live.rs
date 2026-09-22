//! Read-only live relay: stream a running run's session events over SSE by dialing its loop pod's
//! in-process control bridge (`crucible/src/control.rs`).
//!
//! `GET /api/runs/:run_id/live` resolves the run → its pod → the pod IP, opens a plain TCP NDJSON
//! connection to `podIP:<control_port>`, subscribes with a `tail` command (replaying from a given
//! seq for resume), and forwards each broadcast session line as an SSE `session` event. It also
//! issues a `status` command on connect and every ~10s, forwarding each snapshot as an SSE `status`
//! event. When the stream ends it emits one `end` event carrying the reason.
//!
//! Every event carries an SSE id: a [`StreamCursor`] rendered as `<session-seq>.<log-ordinal>`,
//! the two independent positions a viewer resumes from. An `EventSource`'s `Last-Event-ID` parses
//! back into that pair; only the session half is ever written into a bridge `tail from_seq`.
//!
//! The bridge dial only works on the controller's own cluster — it is a raw TCP connection to the
//! pod IP. A run dispatched to a spoke streams through [`spoke_relay_task`] instead: the pod's log
//! stream over the spoke's kube client, forwarded as `log` events, switching to `session` events
//! once the run wrapper prints [`crucible_contract::RUN_SESSION_DELIMITER`] and cats the session
//! NDJSON. Lower fidelity (no status snapshots, and session lines only arrive at the end), but it
//! is the pod's real output rather than a 404 against a cluster the pod was never on.
//!
//! Strictly read-only: the ONLY bridge command sent is `status` (plus the `tail` subscribe). No
//! steer/pause/abort — that's a future admin-gated lane. Each viewer gets its own dial (the bridge
//! is thread-per-accept); when the SSE client disconnects, the relay task observes the dropped
//! receiver and drops the TCP connection promptly, so no dial leaks to a pod.

use crate::api::state::{ApiState, ErrorBody};
use anyhow::{Context, Result};
use axum::Json;
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// How often the relay re-requests a `Status` snapshot from the bridge, so a viewer that joined
/// between the loop's own status moves still gets a fresh phase/spend/best-score periodically.
const STATUS_INTERVAL: Duration = Duration::from_secs(10);

/// The NDJSON command that asks the bridge for a status snapshot (read-only).
const STATUS_CMD: &[u8] = b"{\"cmd\":\"status\"}\n";

/// Bound on the relay→SSE channel: a viewer that stalls can buffer this many events before the
/// relay task blocks on `send`, which naturally backpressures the read from the pod.
const CHANNEL_CAP: usize = 256;

/// The bridge's monotonic stamp on a `session.jsonl` line. The only value that may be written into
/// a `tail from_seq` command or compared against one, on either relay path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SessionSeq(u64);

/// A per-connection count of the pod log lines the spoke relay forwarded before the run wrapper's
/// session delimiter. Always zero on the hub path, which has no `log` events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct LogOrdinal(u64);

impl SessionSeq {
    pub(crate) const fn new(seq: u64) -> Self {
        SessionSeq(seq)
    }

    fn next(self) -> Self {
        SessionSeq(self.0.saturating_add(1))
    }
}

impl std::fmt::Display for SessionSeq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl LogOrdinal {
    pub(crate) const fn new(ord: u64) -> Self {
        LogOrdinal(ord)
    }

    fn next(self) -> Self {
        LogOrdinal(self.0.saturating_add(1))
    }
}

impl std::fmt::Display for LogOrdinal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A viewer's position in one live stream: how far it has read the session feed, and how far it has
/// read the pod's pre-delimiter log output. Rendered as the SSE id `<session>.<log>` and parsed back
/// out of `Last-Event-ID`; a bare integer with no separator is a session seq, which is what every
/// client sent before log events carried ids.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct StreamCursor {
    session: SessionSeq,
    log: LogOrdinal,
}

impl StreamCursor {
    /// The resume point of a client that only knows a session seq (`?from_seq=N`).
    pub(crate) const fn from_session(seq: u64) -> Self {
        StreamCursor {
            session: SessionSeq::new(seq),
            log: LogOrdinal::new(0),
        }
    }

    /// Move onto the position an event just delivered. Status, phase and end carry no position of
    /// their own; they are stamped with the cursor as it stands.
    fn advance(&mut self, ev: &RelayEvent) {
        match ev {
            RelayEvent::Session { seq, .. } => self.session = *seq,
            RelayEvent::Log { ord, .. } => self.log = *ord,
            RelayEvent::Status(_) | RelayEvent::Phase(_) | RelayEvent::End(_) => {}
        }
    }
}

impl std::fmt::Display for StreamCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.session, self.log)
    }
}

impl std::str::FromStr for StreamCursor {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let s = s.trim();
        match s.split_once('.') {
            Some((session, log)) => Ok(StreamCursor {
                session: SessionSeq::new(session.parse()?),
                log: LogOrdinal::new(log.parse()?),
            }),
            None => Ok(StreamCursor::from_session(s.parse()?)),
        }
    }
}

/// Why a live relay ended — the payload of the terminal SSE `end` event, so the UI can tell a
/// clean finish from a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EndReason {
    /// The run exists but isn't `running` (nothing live to stream).
    RunNotRunning,
    /// The run is running but its pod is gone / has no IP yet (torn down, or not scheduled).
    PodGone,
    /// The bridge closed the connection (typically the run finished).
    BridgeClosed,
    /// The pod's log stream ended, carrying its final phase — the spoke relay's clean finish.
    Completed(String),
    /// The relay ran past its wall-clock bound. The viewer reconnects to keep watching.
    Timeout,
    /// A dial/IO/resolve failure, with detail.
    Error(String),
}

impl EndReason {
    /// The `end` event's `data` string.
    fn as_data(&self) -> String {
        match self {
            EndReason::RunNotRunning => "run-not-running".to_string(),
            EndReason::PodGone => "pod-gone".to_string(),
            EndReason::BridgeClosed => "bridge-closed".to_string(),
            EndReason::Completed(phase) => format!("completed: {phase}"),
            EndReason::Timeout => "timeout".to_string(),
            EndReason::Error(detail) => format!("error: {detail}"),
        }
    }
}

/// One event the relay forwards from the bridge to the SSE client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RelayEvent {
    /// A session.jsonl broadcast line, carrying its monotonic `seq`. `line` is the raw NDJSON.
    Session { seq: SessionSeq, line: String },
    /// A status snapshot (the bridge's `StatusSnapshot` JSON, as a compact string).
    Status(String),
    /// The pod's phase, on connect and on change. Only the spoke relay emits this; the bridge
    /// carries richer status snapshots.
    Phase(String),
    /// A plain log line off the pod, already length-capped, carrying its per-connection ordinal.
    /// Spoke relay only.
    Log { ord: LogOrdinal, line: String },
    /// The terminal event; the stream closes right after.
    End(EndReason),
}

/// Classify one NDJSON frame off the bridge. Two frame kinds share the socket: session broadcasts
/// (a JSON object carrying `seq`, no `ok`) and command replies (carry `ok`). Only the `status`
/// reply is forwarded; the `tail` ack and other replies are internal. Non-JSON / unexpected shapes
/// are dropped (`None`).
fn classify(line: &str) -> Option<RelayEvent> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    let obj = value.as_object()?;
    if obj.contains_key("ok") {
        // A command reply. Forward only the status snapshot; ignore the tail ack and the rest.
        if obj.get("cmd").and_then(Value::as_str) == Some("status")
            && let Some(status) = obj.get("status")
        {
            return Some(RelayEvent::Status(status.to_string()));
        }
        return None;
    }
    // A session broadcast: it always carries the monotonic seq the bridge stamped on.
    let seq = obj.get("seq").and_then(Value::as_u64)?;
    Some(RelayEvent::Session {
        seq: SessionSeq::new(seq),
        line: line.trim().to_string(),
    })
}

/// Spawn the per-connection relay task and hand back the receiving end of its event channel. The
/// task dials `addr`, subscribes with `tail from_seq`, and forwards events until the bridge closes,
/// an IO error hits, or the receiver is dropped (SSE client disconnect) — at which point it drops
/// the TCP connection.
fn spawn_relay(
    addr: SocketAddr,
    from_seq: SessionSeq,
    metrics: Option<crate::metrics::Metrics>,
) -> mpsc::Receiver<RelayEvent> {
    let (tx, rx) = mpsc::channel(CHANNEL_CAP);
    tokio::spawn(relay_task(addr, from_seq, tx, metrics));
    rx
}

#[tracing::instrument(skip(tx, metrics), fields(%addr, %from_seq))]
async fn relay_task(
    addr: SocketAddr,
    from_seq: SessionSeq,
    tx: mpsc::Sender<RelayEvent>,
    metrics: Option<crate::metrics::Metrics>,
) {
    // The gauge guard lives for the whole task; dropping it (any exit path) decrements the count.
    let _guard = metrics
        .as_ref()
        .map(crate::metrics::Metrics::live_stream_guard);

    let stream = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(e) => {
            if let Some(m) = &metrics {
                m.record_live_error("connect");
            }
            tracing::warn!(error = %e, "live relay: dialing the control bridge failed");
            let _ = tx
                .send(RelayEvent::End(EndReason::Error(e.to_string())))
                .await;
            return;
        }
    };
    let (rd, mut wr) = stream.into_split();

    // Subscribe to the live feed (replaying from `from_seq`) and ask for an initial snapshot. A
    // write failure this early means the bridge went away — a clean bridge-closed, not a relay fault.
    let tail = format!("{{\"cmd\":\"tail\",\"from_seq\":{from_seq}}}\n");
    if wr.write_all(tail.as_bytes()).await.is_err() || wr.write_all(STATUS_CMD).await.is_err() {
        let _ = tx.send(RelayEvent::End(EndReason::BridgeClosed)).await;
        return;
    }

    let mut lines = BufReader::new(rd).lines();
    let mut ticker = tokio::time::interval(STATUS_INTERVAL);
    ticker.tick().await; // consume the immediate first tick (we already sent one status above)

    loop {
        tokio::select! {
            // The SSE client went away: the receiver is dropped. Stop and drop the TCP stream.
            _ = tx.closed() => break,
            // Periodic status re-request. A failed write = the bridge closed (run finished).
            _ = ticker.tick() => {
                if wr.write_all(STATUS_CMD).await.is_err() {
                    let _ = tx.send(RelayEvent::End(EndReason::BridgeClosed)).await;
                    break;
                }
            }
            line = lines.next_line() => match line {
                Ok(Some(l)) => {
                    if let Some(ev) = classify(&l)
                        && tx.send(ev).await.is_err() {
                            break; // receiver dropped between polls
                        }
                }
                // Clean EOF: the bridge closed the connection (the run finished / pod went away).
                Ok(None) => {
                    let _ = tx.send(RelayEvent::End(EndReason::BridgeClosed)).await;
                    break;
                }
                // A reset/broken-pipe is the same close race surfacing as an error rather than EOF —
                // treat it as bridge-closed. Any other IO error is a genuine relay fault (counted).
                Err(e) if is_disconnect(&e) => {
                    let _ = tx.send(RelayEvent::End(EndReason::BridgeClosed)).await;
                    break;
                }
                Err(e) => {
                    if let Some(m) = &metrics {
                        m.record_live_error("io");
                    }
                    let _ = tx.send(RelayEvent::End(EndReason::Error(e.to_string()))).await;
                    break;
                }
            },
        }
    }
}

/// Hard wall-clock bound on one spoke relay connection, matching the turn relay's: a wedged pod
/// cannot pin a stream forever, and the browser's `EventSource` reconnects on its own.
const SPOKE_STREAM_DEADLINE: Duration = Duration::from_secs(60 * 60);

/// How often the spoke relay re-polls the pod for a phase change.
const SPOKE_PHASE_POLL: Duration = Duration::from_secs(3);

/// Defensive cap on a single forwarded log line (a loop pod's agent can print anything).
const LOG_LINE_CAP: usize = 4096;

/// Spawn the spoke relay and hand back its event receiver. Same channel discipline as
/// [`spawn_relay`]: a stalled viewer backpressures the read from the pod.
fn spawn_spoke_relay(
    clusters: std::sync::Arc<crate::runs::clusters::ClusterClients>,
    location: crate::runs::model::RunLocation,
    hub_namespace: String,
    pod_name: String,
    resume: StreamCursor,
    metrics: Option<crate::metrics::Metrics>,
) -> mpsc::Receiver<RelayEvent> {
    let (tx, rx) = mpsc::channel(CHANNEL_CAP);
    tokio::spawn(spoke_relay_task(
        clusters,
        location,
        hub_namespace,
        pod_name,
        resume,
        tx,
        metrics,
    ));
    rx
}

/// The spoke relay's per-connection state. Both phases (wait for an attachable container, then
/// follow the log stream) need the same five things, and `last_phase` is the only thing that
/// crosses between them.
struct SpokeRelay {
    api: kube::Api<k8s_openapi::api::core::v1::Pod>,
    pod_name: String,
    tx: mpsc::Sender<RelayEvent>,
    deadline: tokio::time::Instant,
    last_phase: Option<String>,
}

/// What one phase poll saw.
enum PhaseStep {
    /// A container has run, so the log stream can be attached.
    Attachable,
    /// The pod exists but has not started a container yet.
    Waiting,
    /// The pod is gone, the viewer left, or the API would not answer.
    Stop(Option<EndReason>),
}

impl SpokeRelay {
    async fn send(&self, ev: RelayEvent) -> bool {
        self.tx.send(ev).await.is_ok()
    }

    async fn end(&self, reason: EndReason) {
        let _ = self.tx.send(RelayEvent::End(reason)).await;
    }

    /// The pod's phase right now, forwarded when it differs from the last one seen.
    async fn observe_phase(&mut self) -> PhaseStep {
        let pod = match self.api.get_opt(&self.pod_name).await {
            Ok(Some(pod)) => pod,
            Ok(None) => return PhaseStep::Stop(Some(EndReason::PodGone)),
            Err(e) => return PhaseStep::Stop(Some(EndReason::Error(format!("{e:#}")))),
        };
        let phase = pod
            .status
            .and_then(|s| s.phase)
            .unwrap_or_else(|| "Unknown".to_string());
        if self.last_phase.as_deref() != Some(&phase)
            && !self.send(RelayEvent::Phase(phase.clone())).await
        {
            return PhaseStep::Stop(None);
        }
        let attachable = matches!(phase.as_str(), "Running" | "Succeeded" | "Failed");
        self.last_phase = Some(phase);
        match attachable {
            true => PhaseStep::Attachable,
            false => PhaseStep::Waiting,
        }
    }

    /// Poll until a container has run. Pending covers scheduling and the loop image pull, which is
    /// the stretch a viewer most wants narrated, so each change is forwarded while waiting.
    async fn wait_until_attachable(&mut self, metrics: Option<&crate::metrics::Metrics>) -> bool {
        loop {
            match self.observe_phase().await {
                PhaseStep::Attachable => return true,
                PhaseStep::Waiting => {}
                PhaseStep::Stop(reason) => {
                    if let Some(EndReason::Error(_)) = &reason
                        && let Some(m) = metrics
                    {
                        m.record_live_error("resolve");
                    }
                    if let Some(reason) = reason {
                        self.end(reason).await;
                    }
                    return false;
                }
            }
            tokio::select! {
                _ = self.tx.closed() => return false,
                _ = tokio::time::sleep_until(self.deadline) => {
                    self.end(EndReason::Timeout).await;
                    return false;
                }
                _ = tokio::time::sleep(SPOKE_PHASE_POLL) => {}
            }
        }
    }

    /// The phase to report when the log stream ends: the pod's current one, or the last one seen
    /// if it has already been collected.
    async fn final_phase(&self) -> String {
        match self.api.get_opt(&self.pod_name).await {
            Ok(Some(pod)) => pod
                .status
                .and_then(|s| s.phase)
                .unwrap_or_else(|| "Unknown".to_string()),
            _ => self
                .last_phase
                .clone()
                .unwrap_or_else(|| "Unknown".to_string()),
        }
    }

    /// Follow the pod's log stream, interleaving phase polls, until it ends. Log EOF is what ends
    /// the stream, not the phase poll, so the last lines are never cut off by a racing read.
    async fn pump(&mut self, resume: StreamCursor, metrics: Option<&crate::metrics::Metrics>) {
        use futures_util::{AsyncBufReadExt as _, StreamExt as _};

        let params = kube::api::LogParams {
            follow: true,
            ..Default::default()
        };
        let stream = match self.api.log_stream(&self.pod_name, &params).await {
            Ok(s) => s,
            Err(e) => {
                if let Some(m) = metrics {
                    m.record_live_error("logs");
                }
                self.end(EndReason::Error(format!("{e:#}"))).await;
                return;
            }
        };
        let mut lines = stream.lines();
        let mut ticker = tokio::time::interval(SPOKE_PHASE_POLL);
        ticker.tick().await;

        let mut cursor = PodCursor::default();
        loop {
            tokio::select! {
                _ = self.tx.closed() => return,
                _ = tokio::time::sleep_until(self.deadline) => {
                    self.end(EndReason::Timeout).await;
                    return;
                }
                // A transient kube error mid-poll is not terminal; the log stream is the anchor.
                _ = ticker.tick() => {
                    if let PhaseStep::Stop(reason) = self.observe_phase().await {
                        if let Some(EndReason::PodGone) = &reason {
                            self.end(EndReason::PodGone).await;
                            return;
                        }
                        if reason.is_none() {
                            return;
                        }
                    }
                }
                line = lines.next() => match line {
                    Some(Ok(l)) => {
                        if let Some(ev) = classify_pod_line(&l, &mut cursor, resume)
                            && !self.send(ev).await
                        {
                            return;
                        }
                    }
                    Some(Err(_)) | None => {
                        self.end(EndReason::Completed(self.final_phase().await)).await;
                        return;
                    }
                },
            }
        }
    }
}

/// Follow a spoke run's pod: phase on connect and on change, then the log stream. Everything after
/// the run wrapper's session delimiter is the `cat` of `state/session.jsonl`, so those lines are
/// forwarded as `session` events with the same monotonic seq the bridge would have given them —
/// which is what makes `Last-Event-ID` resume work on this path too.
#[tracing::instrument(skip(clusters, tx, metrics), fields(cluster = %location.cluster, %pod_name, %resume))]
async fn spoke_relay_task(
    clusters: std::sync::Arc<crate::runs::clusters::ClusterClients>,
    location: crate::runs::model::RunLocation,
    hub_namespace: String,
    pod_name: String,
    resume: StreamCursor,
    tx: mpsc::Sender<RelayEvent>,
    metrics: Option<crate::metrics::Metrics>,
) {
    let _guard = metrics
        .as_ref()
        .map(crate::metrics::Metrics::live_stream_guard);
    let api = match spoke_pod_api(&clusters, &location, &hub_namespace).await {
        Ok(api) => api,
        Err(e) => {
            if let Some(m) = &metrics {
                m.record_live_error("connect");
            }
            let _ = tx
                .send(RelayEvent::End(EndReason::Error(format!("{e:#}"))))
                .await;
            return;
        }
    };
    let mut relay = SpokeRelay {
        api,
        pod_name,
        tx,
        deadline: tokio::time::Instant::now() + SPOKE_STREAM_DEADLINE,
        last_phase: None,
    };
    if relay.wait_until_attachable(metrics.as_ref()).await {
        relay.pump(resume, metrics.as_ref()).await;
    }
}

/// The pod API for a run's recorded location. The recorded namespace is authoritative — it is what
/// the dispatch actually resolved — and a run backfilled before the column existed falls back to
/// asking the cluster.
pub(crate) async fn spoke_pod_api(
    clusters: &crate::runs::clusters::ClusterClients,
    location: &crate::runs::model::RunLocation,
    hub_namespace: &str,
) -> Result<kube::Api<k8s_openapi::api::core::v1::Pod>> {
    let namespace = match &location.namespace {
        Some(ns) => ns.clone(),
        None => clusters
            .pod_namespace(&location.cluster, hub_namespace)
            .await
            .with_context(|| {
                format!(
                    "resolving the pod namespace on cluster {}",
                    location.cluster
                )
            })?,
    };
    let client = clusters.client(&location.cluster).await.with_context(|| {
        format!(
            "connecting to cluster {} for the live relay",
            location.cluster
        )
    })?;
    Ok(kube::Api::namespaced(client, &namespace))
}

/// Where a spoke relay's read of the pod log stream has got to. The kube log stream is re-opened
/// from the top on every connection, so both counts restart at zero and the resume point does the
/// filtering.
#[derive(Debug, Default)]
struct PodCursor {
    in_session: bool,
    session: SessionSeq,
    log: LogOrdinal,
}

/// Classify one line off a loop pod's log stream. Before the wrapper's session delimiter every line
/// is pod output carrying a log ordinal; after it, every line is a session NDJSON record carrying a
/// seq. Lines at or below the matching half of `resume` are already on the client and are dropped.
fn classify_pod_line(
    line: &str,
    cursor: &mut PodCursor,
    resume: StreamCursor,
) -> Option<RelayEvent> {
    let trimmed = line.trim_end();
    if !cursor.in_session {
        if trimmed.contains(crucible_contract::RUN_SESSION_DELIMITER) {
            cursor.in_session = true;
            return None;
        }
        cursor.log = cursor.log.next();
        if cursor.log <= resume.log {
            return None;
        }
        return Some(RelayEvent::Log {
            ord: cursor.log,
            line: cap_line(trimmed, LOG_LINE_CAP),
        });
    }
    if trimmed.is_empty() {
        return None;
    }
    cursor.session = cursor.session.next();
    (cursor.session > resume.session).then(|| RelayEvent::Session {
        seq: cursor.session,
        line: trimmed.to_string(),
    })
}

/// Whether an IO error is just the peer closing the connection (a finished run's bridge dropping the
/// socket), which surfaces as a reset/abort/broken-pipe/unexpected-EOF depending on timing rather
/// than a clean EOF — not a relay fault.
fn is_disconnect(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(
        e.kind(),
        ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof
    )
}

/// Map one [`RelayEvent`] to its SSE frame, stamped with the stream position it leaves the viewer
/// at — that id is what comes back as `Last-Event-ID` on a reconnect.
fn sse_event(ev: &RelayEvent, cursor: StreamCursor) -> SseEvent {
    let frame = SseEvent::default().id(cursor.to_string());
    match ev {
        RelayEvent::Session { line, .. } => frame.event("session").data(line.clone()),
        RelayEvent::Status(snapshot) => frame.event("status").data(snapshot.clone()),
        RelayEvent::Phase(phase) => frame.event("phase").data(phase.clone()),
        RelayEvent::Log { line, .. } => frame.event("log").data(line.clone()),
        RelayEvent::End(reason) => frame.event("end").data(reason.as_data()),
    }
}

/// Build the SSE response from a live relay's event receiver: forward each event, and let the
/// stream end when the relay closes the channel (right after the terminal `end` event). The fold
/// carries the [`StreamCursor`] that stamps every frame's id.
fn relay_sse(rx: mpsc::Receiver<RelayEvent>, resume: StreamCursor) -> Response {
    let stream = futures_util::stream::unfold((rx, resume), |(mut rx, mut cursor)| async move {
        let ev = rx.recv().await?;
        cursor.advance(&ev);
        Some((Ok::<_, Infallible>(sse_event(&ev, cursor)), (rx, cursor)))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// A one-shot SSE response that emits a single terminal `end` event and closes — the shape a known
/// but non-streamable run returns (not `running`, or its pod is gone), so the UI distinguishes it
/// from an unknown run (a 404).
fn end_sse(reason: EndReason) -> Response {
    let stream = futures_util::stream::once(async move {
        Ok::<_, Infallible>(sse_event(&RelayEvent::End(reason), StreamCursor::default()))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Resolve a running run's pod to the control-bridge socket address by getting the Pod and reading
/// `status.podIP`. Hub only: the dial is raw TCP to the pod IP, reachable only from inside the
/// controller's own cluster. `Ok(None)` when the pod exists but has no IP yet (or is gone); `Err`
/// on a kube failure.
async fn resolve_pod_addr(
    clusters: &crate::runs::clusters::ClusterClients,
    namespace: &str,
    control_port: u16,
    pod_name: &str,
) -> Result<Option<SocketAddr>> {
    use k8s_openapi::api::core::v1::Pod;

    let client = clusters
        .client(crate::runs::clusters::HUB_CLUSTER)
        .await
        .context("connecting to the Kubernetes API for the live relay")?;
    let api: kube::Api<Pod> = kube::Api::namespaced(client, namespace);
    let pod = api
        .get(pod_name)
        .await
        .with_context(|| format!("getting pod {pod_name} for the live relay"))?;
    let ip = pod
        .status
        .and_then(|s| s.pod_ip)
        .filter(|ip| !ip.is_empty());
    match ip {
        Some(ip) => {
            let addr: IpAddr = ip
                .parse()
                .with_context(|| format!("parsing pod IP {ip:?}"))?;
            Ok(Some(SocketAddr::new(addr, control_port)))
        }
        None => Ok(None),
    }
}

/// The `GET /api/runs/:run_id/live` handler body. Resolves the run and returns:
///   * `404` for an unknown run (so the UI can distinguish it),
///   * a single-`end` SSE for a known-but-non-running run (`run-not-running`) or a running run with
///     no reachable pod (`pod-gone`),
///   * the live SSE relay for a running run whose pod resolves.
///
/// `resume` is where the viewer left off (the default = from the bridge's oldest retained line and
/// the pod log's first line).
pub(crate) async fn live_response(
    state: &ApiState,
    run_id: &str,
    resume: StreamCursor,
) -> Response {
    let run = match crate::runs::store::get_run(state.db.pool(), run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(ErrorBody::new(format!("run not found: {run_id}"))),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("reading run {run_id}: {e:#}"),
            )
                .into_response();
        }
    };

    if run.status != "running" {
        return end_sse(EndReason::RunNotRunning);
    }
    let Some(pod) = run.pod.filter(|p| !p.is_empty()) else {
        return end_sse(EndReason::PodGone);
    };

    let metrics = state.db.metrics().cloned();
    // The bridge dial only reaches the controller's own cluster; a spoke run streams its pod logs
    // instead, on the cluster the dispatch recorded.
    if !run.location.is_hub() {
        let rx = spawn_spoke_relay(
            state.clusters.clone(),
            run.location.clone(),
            state.pod_namespace.clone(),
            pod,
            resume,
            metrics,
        );
        return relay_sse(rx, resume);
    }
    let namespace = run
        .location
        .namespace
        .as_deref()
        .unwrap_or(&state.pod_namespace);
    let addr = match resolve_pod_addr(&state.clusters, namespace, state.control_port, &pod).await {
        Ok(Some(addr)) => addr,
        Ok(None) => return end_sse(EndReason::PodGone),
        Err(e) => {
            if let Some(m) = &metrics {
                m.record_live_error("resolve");
            }
            tracing::warn!(%run_id, %pod, error = %format!("{e:#}"), "live relay: resolving the pod failed");
            return end_sse(EndReason::Error(format!("resolving pod: {e:#}")));
        }
    };

    let rx = spawn_relay(addr, resume.session, metrics);
    relay_sse(rx, resume)
}

/// Truncate to at most `cap` bytes on a char boundary, marking the cut.
pub(crate) fn cap_line(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    /// A minimal faithful stand-in for the loop-pod control bridge: a real localhost TCP listener
    /// speaking the actual NDJSON protocol. On accept it reads commands; on `tail from_seq=N` it
    /// replays retained lines (seq > N) then streams the ones pushed live; on `status` it replies
    /// with the real reply shape. Backed by a shared, test-driven set of lines + a status snapshot.
    struct FakeBridge {
        addr: SocketAddr,
        shared: Arc<Mutex<Shared>>,
        // A sender the accept loop clones per connection so a test can push live lines post-connect.
        live_tx: tokio::sync::broadcast::Sender<(u64, String)>,
    }

    #[derive(Default)]
    struct Shared {
        /// Retained (seq, raw-line) history for replay.
        history: Vec<(u64, String)>,
        /// The current status snapshot JSON returned on a `status` command.
        status: String,
        /// Set true once at least one connection has been fully closed by the client — lets the
        /// disconnect test observe that the dial was dropped.
        client_disconnected: bool,
    }

    impl FakeBridge {
        async fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");
            let shared = Arc::new(Mutex::new(Shared {
                status: r#"{"phase":"searching","iter":3,"spend":0.5,"paused":false}"#.to_string(),
                ..Shared::default()
            }));
            let (live_tx, _) = tokio::sync::broadcast::channel(64);

            let accept_shared = shared.clone();
            let accept_tx = live_tx.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((sock, _)) = listener.accept().await else {
                        break;
                    };
                    let conn_shared = accept_shared.clone();
                    let live_rx = accept_tx.subscribe();
                    tokio::spawn(serve_conn(sock, conn_shared, live_rx));
                }
            });

            FakeBridge {
                addr,
                shared,
                live_tx,
            }
        }

        fn push_history(&self, seq: u64, line: &str) {
            self.shared
                .lock()
                .expect("lock")
                .history
                .push((seq, line.to_string()));
        }

        /// Broadcast a live line to every connected client (post-subscribe).
        fn push_live(&self, seq: u64, line: &str) {
            let _ = self.live_tx.send((seq, line.to_string()));
        }

        fn client_disconnected(&self) -> bool {
            self.shared.lock().expect("lock").client_disconnected
        }
    }

    async fn serve_conn(
        sock: TcpStream,
        shared: Arc<Mutex<Shared>>,
        mut live_rx: tokio::sync::broadcast::Receiver<(u64, String)>,
    ) {
        let (rd, mut wr) = sock.into_split();
        let mut lines = BufReader::new(rd).lines();
        loop {
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(cmd)) => {
                        let v: Value = match serde_json::from_str(cmd.trim()) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let name = v.get("cmd").and_then(Value::as_str).or_else(|| v.as_str());
                        match name {
                            Some("tail") => {
                                let from_seq = v.get("from_seq").and_then(Value::as_u64).unwrap_or(0);
                                let replay: Vec<(u64, String)> = shared
                                    .lock()
                                    .expect("lock")
                                    .history
                                    .iter()
                                    .filter(|(seq, _)| *seq > from_seq)
                                    .cloned()
                                    .collect();
                                for (_, l) in replay {
                                    if wr.write_all(format!("{l}\n").as_bytes()).await.is_err() {
                                        return;
                                    }
                                }
                                let _ = wr
                                    .write_all(
                                        format!("{{\"ok\":true,\"cmd\":\"tail\",\"from_seq\":{from_seq}}}\n")
                                            .as_bytes(),
                                    )
                                    .await;
                            }
                            Some("status") => {
                                let snapshot = shared.lock().expect("lock").status.clone();
                                let reply =
                                    format!("{{\"ok\":true,\"cmd\":\"status\",\"status\":{snapshot}}}\n");
                                if wr.write_all(reply.as_bytes()).await.is_err() {
                                    return;
                                }
                            }
                            _ => {}
                        }
                    }
                    // Client closed the socket — record it so the disconnect test can observe it.
                    Ok(None) | Err(_) => {
                        shared.lock().expect("lock").client_disconnected = true;
                        return;
                    }
                },
                live = live_rx.recv() => {
                    if let Ok((_, l)) = live
                        && wr.write_all(format!("{l}\n").as_bytes()).await.is_err() {
                            return;
                        }
                }
            }
        }
    }

    /// Drain events from a relay receiver until predicate matches or a timeout elapses.
    async fn recv_until<F>(rx: &mut mpsc::Receiver<RelayEvent>, mut pred: F) -> RelayEvent
    where
        F: FnMut(&RelayEvent) -> bool,
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let ev = tokio::time::timeout(remaining, rx.recv())
                .await
                .expect("relay event before timeout")
                .expect("relay channel open");
            if pred(&ev) {
                return ev;
            }
        }
    }

    #[tokio::test]
    async fn relays_session_lines_with_seq_and_status_snapshots() {
        let bridge = FakeBridge::start().await;
        bridge.push_history(1, r#"{"seq":1,"kind":"note","m":"a"}"#);
        bridge.push_history(2, r#"{"seq":2,"kind":"note","m":"b"}"#);

        let mut rx = spawn_relay(bridge.addr, SessionSeq::default(), None);

        // Both retained session lines arrive with their seq.
        let first = recv_until(&mut rx, |e| matches!(e, RelayEvent::Session { .. })).await;
        assert_eq!(
            first,
            RelayEvent::Session {
                seq: SessionSeq::new(1),
                line: r#"{"seq":1,"kind":"note","m":"a"}"#.to_string()
            }
        );
        let second = recv_until(&mut rx, |e| matches!(e, RelayEvent::Session { .. })).await;
        assert!(matches!(second, RelayEvent::Session { seq, .. } if seq == SessionSeq::new(2)));

        // The initial status command produced a snapshot event.
        let status = recv_until(&mut rx, |e| matches!(e, RelayEvent::Status(_))).await;
        let RelayEvent::Status(s) = status else {
            unreachable!()
        };
        assert!(
            s.contains("\"phase\":\"searching\""),
            "snapshot forwarded: {s}"
        );

        // A live line pushed after subscribe streams through too.
        bridge.push_live(3, r#"{"seq":3,"kind":"note","m":"c"}"#);
        let live = recv_until(
            &mut rx,
            |e| matches!(e, RelayEvent::Session { seq, .. } if *seq == SessionSeq::new(3)),
        )
        .await;
        assert!(matches!(live, RelayEvent::Session { seq, .. } if seq == SessionSeq::new(3)));
    }

    #[tokio::test]
    async fn resume_from_seq_skips_already_seen_lines() {
        let bridge = FakeBridge::start().await;
        bridge.push_history(1, r#"{"seq":1,"m":"a"}"#);
        bridge.push_history(2, r#"{"seq":2,"m":"b"}"#);
        bridge.push_history(3, r#"{"seq":3,"m":"c"}"#);

        // Resume after seq 2: only seq 3 replays.
        let mut rx = spawn_relay(bridge.addr, SessionSeq::new(2), None);
        let ev = recv_until(&mut rx, |e| matches!(e, RelayEvent::Session { .. })).await;
        assert!(
            matches!(ev, RelayEvent::Session { seq, .. } if seq == SessionSeq::new(3)),
            "first session line after from_seq=2 must be seq 3, got {ev:?}"
        );
    }

    #[tokio::test]
    async fn bridge_close_yields_end_bridge_closed() {
        // A listener that accepts then immediately drops the connection (bridge went away).
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            if let Ok((sock, _)) = listener.accept().await {
                drop(sock);
            }
        });

        let mut rx = spawn_relay(addr, SessionSeq::default(), None);
        let end = recv_until(&mut rx, |e| matches!(e, RelayEvent::End(_))).await;
        assert_eq!(end, RelayEvent::End(EndReason::BridgeClosed));
    }

    #[tokio::test]
    async fn client_disconnect_drops_the_tcp_dial() {
        let bridge = FakeBridge::start().await;
        bridge.push_history(1, r#"{"seq":1,"m":"a"}"#);

        let mut rx = spawn_relay(bridge.addr, SessionSeq::default(), None);
        // Receive at least one event so we know the dial is established.
        let _ = recv_until(&mut rx, |e| matches!(e, RelayEvent::Session { .. })).await;

        // Drop the receiver: the relay task must notice and drop its TCP connection, which the fake
        // bridge observes as the client-side close.
        drop(rx);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            if bridge.client_disconnected() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            bridge.client_disconnected(),
            "relay must drop the pod dial promptly when the SSE client disconnects"
        );
    }

    #[tokio::test]
    async fn connect_failure_yields_end_error() {
        // Nothing listening on this port → dial fails → a terminal error end, error counted.
        let addr: SocketAddr = "127.0.0.1:1".parse().expect("addr");
        let metrics = crate::metrics::Metrics::new().expect("metrics");
        let mut rx = spawn_relay(addr, SessionSeq::default(), Some(metrics));
        let end = recv_until(&mut rx, |e| matches!(e, RelayEvent::End(_))).await;
        assert!(
            matches!(end, RelayEvent::End(EndReason::Error(_))),
            "got {end:?}"
        );
    }

    /// The spoke relay's whole job: pod output while the run is in flight, then the session NDJSON
    /// the wrapper cats after the delimiter, carrying the seq a resuming viewer keys on.
    #[test]
    fn the_delimiter_switches_pod_output_to_session_lines() {
        let mut cursor = PodCursor::default();
        let resume = StreamCursor::default();

        assert_eq!(
            classify_pod_line(
                "Trying to pull ghcr.io/x/sandbox:latest...",
                &mut cursor,
                resume
            ),
            Some(RelayEvent::Log {
                ord: LogOrdinal::new(1),
                line: "Trying to pull ghcr.io/x/sandbox:latest...".to_string()
            })
        );
        assert_eq!(
            classify_pod_line(
                &format!(
                    "=== SESSION (rc=0) === {}",
                    crucible_contract::RUN_SESSION_DELIMITER
                ),
                &mut cursor,
                resume
            ),
            None,
            "the delimiter itself is not forwarded"
        );
        assert_eq!(
            classify_pod_line(r#"{"v":1,"kind":"row"}"#, &mut cursor, resume),
            Some(RelayEvent::Session {
                seq: SessionSeq::new(1),
                line: r#"{"v":1,"kind":"row"}"#.to_string()
            })
        );
        assert_eq!(
            classify_pod_line(r#"{"v":1,"kind":"end"}"#, &mut cursor, resume),
            Some(RelayEvent::Session {
                seq: SessionSeq::new(2),
                line: r#"{"v":1,"kind":"end"}"#.to_string()
            })
        );
        assert_eq!(
            classify_pod_line("", &mut cursor, resume),
            None,
            "a blank line consumes no seq"
        );
    }

    /// Resume: an `EventSource` reconnecting with `Last-Event-ID` must not replay what it has, and
    /// the seq it resumes onto has to keep counting from the same place.
    #[test]
    fn a_resume_drops_the_lines_the_viewer_already_has() {
        let mut cursor = PodCursor {
            in_session: true,
            ..PodCursor::default()
        };
        let resume = StreamCursor::from_session(2);

        assert_eq!(
            classify_pod_line(r#"{"n":1}"#, &mut cursor, resume),
            None,
            "already delivered"
        );
        assert_eq!(
            classify_pod_line(r#"{"n":2}"#, &mut cursor, resume),
            None,
            "already delivered"
        );
        assert_eq!(
            classify_pod_line(r#"{"n":3}"#, &mut cursor, resume),
            Some(RelayEvent::Session {
                seq: SessionSeq::new(3),
                line: r#"{"n":3}"#.to_string()
            }),
            "the first unseen line keeps its original seq"
        );
    }

    /// The pod's log stream is re-opened from the top on every connection, so a reconnecting viewer
    /// would see every pre-delimiter line again if the log ordinal did not filter them.
    #[test]
    fn a_resume_drops_the_pod_log_lines_the_viewer_already_has() {
        let mut cursor = PodCursor::default();
        let resume = StreamCursor {
            session: SessionSeq::new(0),
            log: LogOrdinal::new(2),
        };

        assert_eq!(classify_pod_line("pulling", &mut cursor, resume), None);
        assert_eq!(classify_pod_line("starting", &mut cursor, resume), None);
        assert_eq!(
            classify_pod_line("iter 1", &mut cursor, resume),
            Some(RelayEvent::Log {
                ord: LogOrdinal::new(3),
                line: "iter 1".to_string()
            }),
            "the first unseen log line keeps its original ordinal"
        );
    }

    /// The SSE id is two cursors, not one number: a log ordinal must never be readable as a session
    /// seq, or it would be handed to the bridge as a `tail from_seq` and skip real session lines.
    #[test]
    fn the_sse_id_round_trips_both_halves() {
        let cursor = StreamCursor {
            session: SessionSeq::new(12),
            log: LogOrdinal::new(348),
        };
        assert_eq!(cursor.to_string(), "12.348");
        assert_eq!("12.348".parse::<StreamCursor>(), Ok(cursor));
        assert_eq!(
            " 12 ".parse::<StreamCursor>(),
            Ok(StreamCursor::from_session(12)),
            "a bare integer is the session seq every pre-existing client sent"
        );
        assert!("12.".parse::<StreamCursor>().is_err());
        assert!("nope".parse::<StreamCursor>().is_err());
    }

    /// An agent can print anything; one line must not be able to blow the SSE frame out, and a
    /// multi-byte char straddling the cap must not take the relay task down with it.
    #[test]
    fn a_runaway_log_line_is_capped() {
        let mut cursor = PodCursor::default();
        for long in ["x".repeat(LOG_LINE_CAP * 3), "é".repeat(LOG_LINE_CAP)] {
            let Some(RelayEvent::Log { line, .. }) =
                classify_pod_line(&long, &mut cursor, StreamCursor::default())
            else {
                panic!("a pre-delimiter line is pod output");
            };
            assert!(line.len() <= LOG_LINE_CAP + "…[truncated]".len());
            assert!(line.ends_with("…[truncated]"));
        }
    }

    /// One parsed SSE frame off the wire.
    #[derive(Debug)]
    struct Frame {
        id: String,
        event: String,
        data: String,
    }

    fn parse_frames(body: &str) -> Vec<Frame> {
        body.split("\n\n")
            .map(|block| {
                let mut frame = Frame {
                    id: String::new(),
                    event: String::new(),
                    data: String::new(),
                };
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("id:") {
                        frame.id = v.trim().to_string();
                    } else if let Some(v) = line.strip_prefix("event:") {
                        frame.event = v.trim().to_string();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        frame.data = v.trim().to_string();
                    }
                }
                frame
            })
            .filter(|f| !f.event.is_empty())
            .collect()
    }

    /// Render a relay's events through the real SSE response and read the wire bytes back.
    async fn sse_frames(events: Vec<RelayEvent>, resume: StreamCursor) -> Vec<Frame> {
        let (tx, rx) = mpsc::channel(CHANNEL_CAP);
        for ev in events {
            tx.send(ev).await.expect("relay channel open");
        }
        drop(tx);
        let bytes = axum::body::to_bytes(relay_sse(rx, resume).into_body(), usize::MAX)
            .await
            .expect("sse body");
        parse_frames(std::str::from_utf8(&bytes).expect("utf8 body"))
    }

    fn classify_all(pod_output: &[String], resume: StreamCursor) -> Vec<RelayEvent> {
        let mut cursor = PodCursor::default();
        pod_output
            .iter()
            .filter_map(|l| classify_pod_line(l, &mut cursor, resume))
            .collect()
    }

    /// The whole point of the composite id: a viewer watching a mixed log/phase/session stream that
    /// drops mid-flight and reconnects with `Last-Event-ID` gets every remaining line exactly once.
    /// The spoke re-opens the pod log stream from the top on reconnect, so the second pass sees the
    /// full output again and must drop precisely what the first pass delivered.
    #[tokio::test]
    async fn a_reconnect_across_a_mixed_stream_delivers_every_line_exactly_once() {
        let pod_output: Vec<String> = [
            "pulling the sandbox image",
            "loop: iter 1",
            "loop: iter 2",
            &format!(
                "=== SESSION (rc=0) === {}",
                crucible_contract::RUN_SESSION_DELIMITER
            ),
            r#"{"n":1}"#,
            r#"{"n":2}"#,
            r#"{"n":3}"#,
        ]
        .iter()
        .map(|l| l.to_string())
        .collect();

        // First connection: phase on connect, then the pod output, cut after the first session line.
        let mut first = vec![RelayEvent::Phase("Running".to_string())];
        first.extend(
            classify_all(&pod_output, StreamCursor::default())
                .into_iter()
                .take(4),
        );
        let first_frames = sse_frames(first, StreamCursor::default()).await;

        assert!(
            first_frames.iter().all(|f| !f.id.is_empty()),
            "every frame carries an id, phase included: {first_frames:?}"
        );
        let last_id = &first_frames.last().expect("frames").id;
        assert_eq!(last_id, "1.3", "one session line after three log lines");
        let resume: StreamCursor = last_id.parse().expect("the emitted id parses back");

        // The reconnect: the same pod output from the top, filtered by what the viewer already has.
        let second_frames = sse_frames(classify_all(&pod_output, resume), resume).await;
        assert_eq!(
            second_frames
                .iter()
                .map(|f| (f.event.as_str(), f.id.as_str()))
                .collect::<Vec<_>>(),
            vec![("session", "2.3"), ("session", "3.3")],
            "the replayed logs and the delivered session line are dropped, not resent"
        );

        let delivered: Vec<&str> = first_frames
            .iter()
            .chain(second_frames.iter())
            .filter(|f| f.event == "log" || f.event == "session")
            .map(|f| f.data.as_str())
            .collect();
        assert_eq!(
            delivered,
            vec![
                "pulling the sandbox image",
                "loop: iter 1",
                "loop: iter 2",
                r#"{"n":1}"#,
                r#"{"n":2}"#,
                r#"{"n":3}"#,
            ],
            "each pod log line and each session line lands exactly once, in order"
        );
    }

    /// The hub path has no log events, so its ids stay session-only and a `tail from_seq` built from
    /// one can never be poisoned by a log ordinal.
    #[tokio::test]
    async fn a_hub_stream_stamps_ids_on_every_kind() {
        let events = vec![
            RelayEvent::Status(r#"{"phase":"searching"}"#.to_string()),
            RelayEvent::Session {
                seq: SessionSeq::new(7),
                line: r#"{"seq":7}"#.to_string(),
            },
            RelayEvent::Status(r#"{"phase":"scoring"}"#.to_string()),
            RelayEvent::End(EndReason::BridgeClosed),
        ];
        let frames = sse_frames(events, StreamCursor::from_session(6)).await;
        assert_eq!(
            frames
                .iter()
                .map(|f| (f.event.as_str(), f.id.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("status", "6.0"),
                ("session", "7.0"),
                ("status", "7.0"),
                ("end", "7.0"),
            ]
        );
        assert_eq!(
            "7.0".parse::<StreamCursor>().expect("id parses").session,
            SessionSeq::new(7),
            "the session half is what a resume feeds the bridge"
        );
    }

    /// The end reasons are a wire contract the UI branches on.
    #[test]
    fn the_spoke_end_reasons_render_as_the_ui_reads_them() {
        assert_eq!(
            EndReason::Completed("Succeeded".to_string()).as_data(),
            "completed: Succeeded"
        );
        assert_eq!(EndReason::Timeout.as_data(), "timeout");
        assert_eq!(EndReason::PodGone.as_data(), "pod-gone");
    }

    /// The routing decision the run-progress page failed on: a run's recorded cluster is what says
    /// whether the control bridge is dialable at all.
    #[test]
    fn only_a_hub_run_is_bridge_dialable() {
        assert!(crate::runs::model::RunLocation::hub().is_hub());
        assert!(
            crate::runs::model::RunLocation::new("hub", Some("autoresearch".to_string())).is_hub()
        );
        assert!(!crate::runs::model::RunLocation::new("wharf", None).is_hub());
        assert!(
            !crate::runs::model::RunLocation::new("personal:abc", None).is_hub(),
            "a personal target is never the controller's own cluster"
        );
    }
}
