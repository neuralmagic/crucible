//! Read-only live relay for a running turn pod (`GET /api/turns/:pod_name/live`): the scope/rank
//! sibling of [`crate::runs::live`]. A loop pod carries its own control bridge to dial; a turn pod is a
//! one-shot `crucible scope --propose` with nothing listening, so the only live signals are the
//! kube ones — pod phase and the log stream. This relay forwards both as SSE:
//!
//!   * `phase` — the pod's phase string (Pending/Running/Succeeded/Failed), on connect and on
//!     every observed change (polled every few seconds).
//!   * `progress` — the JSON payload of a `CRUCIBLE_SCOPE_PROGRESS:` marker line (the engine emits
//!     one at each refine-round boundary), parsed out of the log stream.
//!   * `activity` — the JSON payload of a `CRUCIBLE_SCOPE_ACTIVITY:` marker line (the engine's
//!     bounded within-round feed: tool calls, text snippets, usage, sandbox stage banners).
//!   * `log` — every other log line, length-capped defensively.
//!   * `end` — terminal, then the stream closes: `turn-not-running` (the ledger row is not
//!     `running`), `pod-gone`, `completed: <phase>` (log EOF), `timeout`, `error: …`.
//!
//! Strictly read-only and unpersisted: the terminal ScopeReport is the dispatcher's job
//! (`parse_scope_report_logs`); this is a viewport, not a record. Bounded three ways: the stream
//! ends when the pod goes terminal (log EOF) or vanishes, a hard wall-clock deadline covers a pod
//! that never finishes, and each line is capped at [`LOG_LINE_CAP`] bytes.

use crate::api::state::{ApiState, ErrorBody};
use crate::runs::workpod::{SCOPE_ACTIVITY_MARKER, SCOPE_PROGRESS_MARKER};
use anyhow::{Context, Result};
use axum::Json;
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::{AsyncBufReadExt, StreamExt};
use k8s_openapi::api::core::v1::Pod;
use std::convert::Infallible;
use std::time::Duration;
use tokio::sync::mpsc;

/// How often the relay re-polls the pod for a phase change.
const PHASE_POLL: Duration = Duration::from_secs(3);

/// Hard wall-clock bound on ONE relay connection, so a wedged pod can't pin a stream forever. Note
/// this is shorter than a gaming-heavy scope turn's deadline (`crate::runs::workpod::scope_deadline` scales
/// past 2h); a viewer watching a long turn simply reconnects when the browser's EventSource retries.
const STREAM_DEADLINE: Duration = Duration::from_secs(60 * 60);

/// Defensive cap on a single forwarded log line (an agent can print anything).
const LOG_LINE_CAP: usize = 4096;

/// Same bounded channel discipline as [`crate::runs::live`]: a stalled viewer backpressures the relay.
const CHANNEL_CAP: usize = 256;

/// One event the relay forwards to the SSE client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TurnEvent {
    /// The pod's phase string, sent on connect and on change. Carries a ` — <detail>` suffix when
    /// a container status explains a non-terminal phase (image pull, backoff).
    Phase(String),
    /// The JSON payload of one `CRUCIBLE_SCOPE_PROGRESS:` marker line.
    Progress(String),
    /// The JSON payload of one `CRUCIBLE_SCOPE_ACTIVITY:` marker line (within-round activity).
    Activity(String),
    /// A plain log line, already length-capped.
    Log(String),
    /// The terminal event; the stream closes right after.
    End(TurnEnd),
}

/// Why a turn stream ended. Rendered by [`TurnEnd::as_data`] into the `end` event's `data`, which
/// the SPA compares literally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TurnEnd {
    /// The turn exists but isn't `running` (nothing live to stream).
    NotRunning,
    /// The pod is gone: never found, or deleted mid-stream.
    PodGone,
    /// The pod's log stream ended, carrying its final phase.
    Completed(String),
    /// The relay ran past its wall-clock bound. The viewer reconnects to keep watching.
    Timeout,
    /// A connect/resolve/logs failure, with detail.
    Error(String),
}

impl TurnEnd {
    /// The `end` event's `data` string.
    fn as_data(&self) -> String {
        match self {
            TurnEnd::NotRunning => "turn-not-running".to_string(),
            TurnEnd::PodGone => "pod-gone".to_string(),
            TurnEnd::Completed(phase) => format!("completed: {phase}"),
            TurnEnd::Timeout => "timeout".to_string(),
            TurnEnd::Error(detail) => format!("error: {detail}"),
        }
    }
}

/// Classify one pod-log line: a well-formed progress/activity marker becomes its typed event; a
/// marker whose payload isn't JSON falls through as a plain log line (never trust the pod);
/// everything else is a capped `log` line.
fn classify_line(line: &str) -> TurnEvent {
    let trimmed = line.trim_end();
    if let Some(payload) = trimmed.trim_start().strip_prefix(SCOPE_PROGRESS_MARKER) {
        let payload = payload.trim();
        if serde_json::from_str::<serde_json::Value>(payload).is_ok() {
            return TurnEvent::Progress(payload.to_string());
        }
    }
    if let Some(payload) = trimmed.trim_start().strip_prefix(SCOPE_ACTIVITY_MARKER) {
        let payload = payload.trim();
        if serde_json::from_str::<serde_json::Value>(payload).is_ok() {
            return TurnEvent::Activity(payload.to_string());
        }
    }
    TurnEvent::Log(cap_line(trimmed, LOG_LINE_CAP))
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

/// Map one [`TurnEvent`] to its SSE frame.
fn sse_event(ev: &TurnEvent) -> SseEvent {
    match ev {
        TurnEvent::Phase(phase) => SseEvent::default().event("phase").data(phase.clone()),
        TurnEvent::Progress(json) => SseEvent::default().event("progress").data(json.clone()),
        TurnEvent::Activity(json) => SseEvent::default().event("activity").data(json.clone()),
        TurnEvent::Log(line) => SseEvent::default().event("log").data(line.clone()),
        TurnEvent::End(reason) => SseEvent::default().event("end").data(reason.as_data()),
    }
}

fn relay_sse(rx: mpsc::Receiver<TurnEvent>) -> Response {
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        let ev = rx.recv().await?;
        Some((Ok::<_, Infallible>(sse_event(&ev)), rx))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// A one-shot SSE response carrying a single terminal `end` event — a known but non-streamable
/// turn, distinguishable from an unknown one (a 404).
fn end_sse(reason: TurnEnd) -> Response {
    let stream = futures_util::stream::once(async move {
        Ok::<_, Infallible>(sse_event(&TurnEvent::End(reason)))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn pod_api(
    clusters: &crate::runs::clusters::ClusterClients,
    cluster: &str,
    hub_namespace: &str,
) -> Result<kube::Api<Pod>> {
    let namespace = clusters.pod_namespace(cluster, hub_namespace).await?;
    let client = clusters
        .client(cluster)
        .await
        .context("connecting to the Kubernetes API for the turn live relay")?;
    Ok(kube::Api::namespaced(client, &namespace))
}

/// The pod's raw phase plus the string the viewer sees. They differ when a container status
/// explains what a non-Running pod is waiting on — most importantly the turn image pull, which
/// otherwise reads as a bare "Pending" for minutes. `.0` drives the attachable check; `.1` rides
/// the `phase` SSE event.
fn pod_phase(pod: &Pod) -> (String, String) {
    let phase = pod
        .status
        .as_ref()
        .and_then(|s| s.phase.clone())
        .unwrap_or_else(|| "Unknown".to_string());
    let display = match waiting_detail(pod) {
        Some(d) => format!("{phase} — {d}"),
        None => phase.clone(),
    };
    (phase, display)
}

/// The first container status stuck in a `waiting` state, rendered human-first. Image-pull states
/// get friendly phrasing; anything else shows its reason verbatim.
fn waiting_detail(pod: &Pod) -> Option<String> {
    let statuses = pod.status.as_ref()?.container_statuses.as_ref()?;
    statuses.iter().find_map(|cs| {
        let waiting = cs.state.as_ref()?.waiting.as_ref()?;
        let reason = waiting.reason.as_deref()?;
        Some(match reason {
            "ContainerCreating" => "creating container (pulling the turn image)".to_string(),
            "ImagePullBackOff" | "ErrImagePull" => format!("image pull failing ({reason})"),
            other => other.to_string(),
        })
    })
}

/// Send, treating a dropped receiver (SSE client disconnect) as the stop signal.
async fn send(tx: &mpsc::Sender<TurnEvent>, ev: TurnEvent) -> bool {
    tx.send(ev).await.is_ok()
}

#[tracing::instrument(skip(clusters, tx, metrics), fields(%cluster, %namespace, %pod_name))]
async fn relay_task(
    clusters: std::sync::Arc<crate::runs::clusters::ClusterClients>,
    cluster: String,
    namespace: String,
    pod_name: String,
    tx: mpsc::Sender<TurnEvent>,
    metrics: Option<crate::metrics::Metrics>,
) {
    let _guard = metrics
        .as_ref()
        .map(crate::metrics::Metrics::live_stream_guard);
    let deadline = tokio::time::Instant::now() + STREAM_DEADLINE;

    let api = match pod_api(&clusters, &cluster, &namespace).await {
        Ok(api) => api,
        Err(e) => {
            if let Some(m) = &metrics {
                m.record_live_error("connect");
            }
            let _ = send(&tx, TurnEvent::End(TurnEnd::Error(format!("{e:#}")))).await;
            return;
        }
    };

    // Phase 1: poll until the pod has (or had) a running container, so its logs are attachable.
    // Pending covers image pulls / scheduling — exactly the black-box stretch operators asked about.
    let mut last_phase: Option<String> = None;
    loop {
        match api.get_opt(&pod_name).await {
            Ok(None) => {
                let _ = send(&tx, TurnEvent::End(TurnEnd::PodGone)).await;
                return;
            }
            Ok(Some(pod)) => {
                let (phase, display) = pod_phase(&pod);
                if last_phase.as_deref() != Some(&display)
                    && !send(&tx, TurnEvent::Phase(display.clone())).await
                {
                    return;
                }
                let attachable = matches!(phase.as_str(), "Running" | "Succeeded" | "Failed");
                last_phase = Some(display);
                if attachable {
                    break;
                }
            }
            Err(e) => {
                if let Some(m) = &metrics {
                    m.record_live_error("resolve");
                }
                let _ = send(&tx, TurnEvent::End(TurnEnd::Error(format!("{e:#}")))).await;
                return;
            }
        }
        tokio::select! {
            _ = tx.closed() => return,
            _ = tokio::time::sleep_until(deadline) => {
                let _ = send(&tx, TurnEvent::End(TurnEnd::Timeout)).await;
                return;
            }
            _ = tokio::time::sleep(PHASE_POLL) => {}
        }
    }

    // Phase 2: follow the log stream, interleaving phase polls. follow=true ends (EOF) when the
    // container terminates — that EOF, not the phase poll, is what ends the stream, so the last
    // lines are never cut off by a racing phase read.
    let params = kube::api::LogParams {
        follow: true,
        ..Default::default()
    };
    let stream = match api.log_stream(&pod_name, &params).await {
        Ok(s) => s,
        Err(e) => {
            if let Some(m) = &metrics {
                m.record_live_error("logs");
            }
            let _ = send(&tx, TurnEvent::End(TurnEnd::Error(format!("{e:#}")))).await;
            return;
        }
    };
    let mut lines = stream.lines();
    let mut ticker = tokio::time::interval(PHASE_POLL);
    ticker.tick().await; // we just observed the phase above

    loop {
        tokio::select! {
            _ = tx.closed() => return,
            _ = tokio::time::sleep_until(deadline) => {
                let _ = send(&tx, TurnEvent::End(TurnEnd::Timeout)).await;
                return;
            }
            _ = ticker.tick() => {
                match api.get_opt(&pod_name).await {
                    Ok(Some(pod)) => {
                        let (_, display) = pod_phase(&pod);
                        if last_phase.as_deref() != Some(&display) {
                            if !send(&tx, TurnEvent::Phase(display.clone())).await {
                                return;
                            }
                            last_phase = Some(display);
                        }
                    }
                    // Deleted mid-stream (the dispatcher GCs a succeeded pod fast): drain stops here.
                    Ok(None) => {
                        let _ = send(&tx, TurnEvent::End(TurnEnd::PodGone)).await;
                        return;
                    }
                    // A transient kube error mid-poll isn't terminal; the log stream is the anchor.
                    Err(_) => {}
                }
            }
            line = lines.next() => match line {
                Some(Ok(l)) => {
                    if !send(&tx, classify_line(&l)).await {
                        return;
                    }
                }
                Some(Err(_)) | None => {
                    // Log EOF: the container terminated. Report the final phase and end cleanly.
                    let final_phase = match api.get_opt(&pod_name).await {
                        Ok(Some(pod)) => pod_phase(&pod).1,
                        _ => last_phase.clone().unwrap_or_else(|| "Unknown".to_string()),
                    };
                    if last_phase.as_deref() != Some(&final_phase) {
                        let _ = send(&tx, TurnEvent::Phase(final_phase.clone())).await;
                    }
                    let _ = send(&tx, TurnEvent::End(TurnEnd::Completed(final_phase))).await;
                    return;
                }
            },
        }
    }
}

/// The `GET /api/turns/:pod_name/live` handler body. Resolves the turn in the work-pod ledger and
/// returns:
///   * `404` for a pod name the ledger has never seen,
///   * a single-`end` SSE (`turn-not-running`) for a known turn that isn't `running` — its story is
///     already on the turns page / scope report,
///   * the live SSE relay otherwise.
pub(crate) async fn live_response(state: &ApiState, pod_name: &str) -> Response {
    let row = match crate::runs::work_pods::get_work_pod(state.db.pool(), pod_name).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(ErrorBody::new(format!("turn not found: {pod_name}"))),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("reading turn {pod_name}: {e:#}"),
            )
                .into_response();
        }
    };

    if row.state != crate::runs::workpod::WorkPodState::Running {
        return end_sse(TurnEnd::NotRunning);
    }

    let metrics = state.db.metrics().cloned();
    let (tx, rx) = mpsc::channel(CHANNEL_CAP);
    tokio::spawn(relay_task(
        state.clusters.clone(),
        row.cluster.clone(),
        state.pod_namespace.clone(),
        pod_name.to_string(),
        tx,
        metrics,
    ));
    relay_sse(rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_marker_line_becomes_a_typed_progress_event() {
        let line = r#"CRUCIBLE_SCOPE_PROGRESS: {"round":2,"kind":"refine","doing":"refining","cost_so_far":0.42}"#;
        let ev = classify_line(line);
        let TurnEvent::Progress(payload) = ev else {
            panic!("expected Progress, got {ev:?}");
        };
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["round"], 2);
        assert_eq!(v["kind"], "refine");
    }

    #[test]
    fn indented_marker_still_classifies_as_progress() {
        let ev = classify_line("  CRUCIBLE_SCOPE_PROGRESS: {\"round\":1}\n");
        assert!(matches!(ev, TurnEvent::Progress(p) if p == "{\"round\":1}"));
    }

    #[test]
    fn marker_with_garbage_payload_falls_through_as_log() {
        let line = "CRUCIBLE_SCOPE_PROGRESS: not json at all";
        assert!(matches!(classify_line(line), TurnEvent::Log(l) if l == line));
    }

    #[test]
    fn activity_marker_line_becomes_a_typed_activity_event() {
        let line = r#"CRUCIBLE_SCOPE_ACTIVITY: {"kind":"tool","name":"Edit","detail":"router.go: rebalance","cost_so_far":0.12}"#;
        let ev = classify_line(line);
        let TurnEvent::Activity(payload) = ev else {
            panic!("expected Activity, got {ev:?}");
        };
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["kind"], "tool");
        assert_eq!(v["name"], "Edit");
    }

    #[test]
    fn activity_marker_with_garbage_payload_falls_through_as_log() {
        let line = "CRUCIBLE_SCOPE_ACTIVITY: definitely not json";
        assert!(matches!(classify_line(line), TurnEvent::Log(l) if l == line));
    }

    #[test]
    fn plain_line_is_a_log_event_with_trailing_newline_stripped() {
        assert_eq!(
            classify_line("[crucible scope] ingest: PASS\n"),
            TurnEvent::Log("[crucible scope] ingest: PASS".to_string())
        );
    }

    #[test]
    fn overlong_line_is_capped_on_a_char_boundary() {
        // Multibyte chars right at the cap: the cut must land on a boundary, marked as truncated.
        let line = "é".repeat(LOG_LINE_CAP); // 2 bytes each -> well past the cap
        let TurnEvent::Log(capped) = classify_line(&line) else {
            panic!("expected Log");
        };
        assert!(capped.ends_with("…[truncated]"));
        assert!(capped.len() <= LOG_LINE_CAP + "…[truncated]".len());
    }

    #[test]
    fn scope_report_marker_is_not_swallowed_as_progress() {
        // The terminal report line must stay a visible log line — the report page owns parsing it.
        let line = r#"CRUCIBLE_SCOPE_REPORT: {"stages":[]}"#;
        assert!(matches!(classify_line(line), TurnEvent::Log(_)));
    }

    /// Build a Pod with the given phase and container waiting reason via the k8s JSON shape.
    fn pod_with_status(phase: &str, waiting_reason: Option<&str>) -> Pod {
        let container_statuses = waiting_reason.map(|reason| {
            serde_json::json!([{
                "name": "turn",
                "image": "img",
                "imageID": "",
                "ready": false,
                "restartCount": 0,
                "state": { "waiting": { "reason": reason } }
            }])
        });
        let mut status = serde_json::json!({ "phase": phase });
        if let Some(cs) = container_statuses {
            status["containerStatuses"] = cs;
        }
        serde_json::from_value(serde_json::json!({ "status": status })).expect("valid Pod")
    }

    #[test]
    fn pending_pod_pulling_the_image_gets_a_phase_detail() {
        let pod = pod_with_status("Pending", Some("ContainerCreating"));
        let (phase, display) = pod_phase(&pod);
        assert_eq!(
            phase, "Pending",
            "the raw phase drives the attachable check"
        );
        assert_eq!(
            display,
            "Pending — creating container (pulling the turn image)"
        );
    }

    #[test]
    fn image_pull_backoff_reads_as_a_failing_pull() {
        let pod = pod_with_status("Pending", Some("ImagePullBackOff"));
        let (_, display) = pod_phase(&pod);
        assert_eq!(display, "Pending — image pull failing (ImagePullBackOff)");
    }

    #[test]
    fn unknown_waiting_reason_shows_verbatim_and_running_stays_bare() {
        let pod = pod_with_status("Pending", Some("CreateContainerConfigError"));
        let (_, display) = pod_phase(&pod);
        assert_eq!(display, "Pending — CreateContainerConfigError");

        let running = pod_with_status("Running", None);
        assert_eq!(pod_phase(&running), ("Running".into(), "Running".into()));

        let bare: Pod = serde_json::from_value(serde_json::json!({})).expect("empty Pod");
        assert_eq!(pod_phase(&bare), ("Unknown".into(), "Unknown".into()));
    }

    #[test]
    fn sse_frames_carry_the_event_names_the_ui_subscribes_to() {
        // SseEvent has no public accessors; its Debug output carries the wire fields.
        let phase = format!("{:?}", sse_event(&TurnEvent::Phase("Running".into())));
        assert!(phase.contains("phase") && phase.contains("Running"));
        let progress = format!("{:?}", sse_event(&TurnEvent::Progress("{}".into())));
        assert!(progress.contains("progress"));
        let activity = format!("{:?}", sse_event(&TurnEvent::Activity("{}".into())));
        assert!(activity.contains("activity"));
        let log = format!("{:?}", sse_event(&TurnEvent::Log("x".into())));
        assert!(log.contains("log"));
        let end = format!("{:?}", sse_event(&TurnEvent::End(TurnEnd::PodGone)));
        assert!(end.contains("end") && end.contains("pod-gone"));
    }

    /// The end reasons are a wire contract the UI branches on.
    #[test]
    fn the_turn_end_reasons_render_as_the_ui_reads_them() {
        assert_eq!(TurnEnd::NotRunning.as_data(), "turn-not-running");
        assert_eq!(TurnEnd::PodGone.as_data(), "pod-gone");
        assert_eq!(
            TurnEnd::Completed("Succeeded".to_string()).as_data(),
            "completed: Succeeded"
        );
        assert_eq!(TurnEnd::Timeout.as_data(), "timeout");
        assert_eq!(
            TurnEnd::Error("dial failed".to_string()).as_data(),
            "error: dial failed"
        );
    }
}
