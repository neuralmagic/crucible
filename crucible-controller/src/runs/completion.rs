#![allow(clippy::disallowed_macros)]

use crate::client::Db;
use crate::config::ControllerCfg;
use crate::issues::model::split_issue_key;
use crate::model::{ParkReason, ParkedBy, Status};
use crate::runs::ingest;
use crate::runs::workpod::RunDisposition;
use anyhow::Result;

/// Fold a finished run into the ledger and advance the issue: ingest the session log
/// (runs + per-candidate rows + the run's cost) then transition `running` → `pr-open` when the run
/// opened at least one draft PR, else `running` → `done`. Called by the pod-watch completion edge —
/// kept here so the ingest → advance logic is the reconcile module's, not the watcher's.
/// `pr-open` is record-only (a human merges the PR; [`reconcile_step`] takes no action on it), so
/// an issue resting there is waiting on a human, distinct from a `done` run that kept nothing.
///
/// `session` is the run's NDJSON, already persisted in the artifact store under `run_id`
/// ([`crate::runs::blob_store::put_run_session`]); the stored `session_uri` points there.
///
/// Returns how the run ended, so a caller that closes the pod ledger afterwards records the run's
/// real outcome instead of assuming the success path.
///
/// This is where the completion edge's span lives — the ingest is the work, whereas the
/// [`ingest_completion`] check that precedes it is a poll that finds nothing on almost every tick.
#[tracing::instrument(skip(db, session, emission), fields(issue_key = %key, %run_id))]
pub async fn complete_run(
    db: &Db,
    key: &str,
    scope_id: Option<i64>,
    run_id: &str,
    pod: Option<&str>,
    session: &str,
    emission: Option<&crate::launches::emission::EmissionCtx>,
) -> Result<RunDisposition> {
    let session_uri = crate::runs::blob_store::run_session_uri(run_id);
    let parsed = match ingest::ingest_session(
        db,
        &ingest::IngestTarget {
            run_id,
            scope_id,
            issue: Some(key),
            pod,
            session_uri: &session_uri,
        },
        session,
    )
    .await
    {
        Ok(parsed) => parsed,
        Err(e) => {
            if let Some(m) = db.metrics() {
                m.record_ingest_failure();
            }
            return Err(e);
        }
    };
    if let Some(m) = db.metrics() {
        // Repo (not the run id) is the run metric's bounded dimension.
        let repo = split_issue_key(key)
            .map(|(repo, _)| repo)
            .unwrap_or_else(|_| "unknown".to_string());
        let outcome = parsed.outcome.as_deref().unwrap_or("incomplete");
        let iterations = parsed
            .candidates
            .iter()
            .filter(|c| c.kind == "deep")
            .count() as u64;
        m.record_run(
            outcome,
            &repo,
            parsed.elapsed_secs,
            iterations,
            parsed.best_score,
        );
    }
    // A run that ERRORED parks (retryable — `done` means solved-or-exhausted, and completing on an
    // error buried the first live loop failure as terminal; Will's call 2026-07-07). The engine's
    // own shutdown reason and the failing task's note ride the park so the next person needs no
    // session forensics.
    if parsed.outcome.as_deref() == Some("error") {
        let park_reason = ParkReason::RunErrored {
            engine_reason: parsed.outcome_reason.clone(),
            cause: parsed
                .failure_cause()
                .as_deref()
                .map(crate::model::truncate_chain),
        };
        crate::issues::transitions::park(
            db.pool(),
            db.events(),
            key,
            Status::Running,
            &park_reason,
            ParkedBy::Machine,
        )
        .await?;
        return Ok(RunDisposition::Errored);
    }
    // A run that opened draft PR(s) rests at `pr-open` (waiting on a human merge); one that kept
    // nothing lands `done`. The first opened PR is the transition's evidence pointer.
    let (to, reason, evidence) = match parsed.pr_links.first() {
        Some(pr) => (
            Status::PrOpen,
            "run finished; draft PR opened",
            Some(pr.url.as_str()),
        ),
        None => (Status::Done, "run finished; outcome ingested", None),
    };
    let reason = match parsed.transport_losses() {
        0 => reason.to_string(),
        1 => format!("{reason}; 1 task attempt died on transport"),
        n => format!("{reason}; {n} task attempts died on transport"),
    };
    let advanced = crate::issues::transitions::transition(
        db.pool(),
        db.events(),
        key,
        Status::Running,
        to,
        Some(&reason),
        evidence,
    )
    .await?;
    // Only a run that finished its whole graph is evidence its inputs were processed; a ceiling or
    // a stop leaves a partial result the next firing must not skip past.
    if advanced && matches!(parsed.outcome.as_deref(), Some("finished" | "solved")) {
        crate::launches::schedules::ScheduleStore::new(db.clone())
            .advance_cursor(key, run_id, &parsed.result)
            .await?;
    }
    // File the tracker review trail (epic + one task per PR) AFTER the transition so a CAS loss
    // (duplicate completion enqueue) can't double-file. Best-effort: the run is complete either
    // way, and the emissions ledger makes a retry on the next reconcile safe.
    if let (Some(ctx), Some(scope_id)) = (emission, scope_id)
        && !parsed.pr_links.is_empty()
    {
        match crate::launches::emission::emit_for_completed_run(
            db,
            ctx,
            key,
            scope_id,
            run_id,
            &parsed.pr_links,
        )
        .await
        {
            Ok(out) if out.created > 0 || out.updated > 0 => {
                tracing::info!(
                    issue_key = %key,
                    created = out.created,
                    updated = out.updated,
                    "emission: review trail filed"
                )
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(
                error = format!("{e:#}"),
                issue_key = %key,
                "emission: review-trail filing failed (continuing)"
            ),
        }
    }
    Ok(RunDisposition::Finished)
}

/// The pod-completion edge: if the `running` run behind `key` has
/// published a complete session log, fold it into the ledger and advance `running` → `done`. Safe to
/// call on *any* reconcile of a `running` row (the pod watch enqueues on completion, but an upstream
/// poll may re-enqueue a still-live run): a run whose log isn't stored yet returns `Ok(false)` and
/// the caller falls through to the other `running` edges. Idempotent — once advanced to `done` there
/// is no `running` run to find, so a duplicate enqueue is a clean no-op. This is what makes
/// crash recovery work cleanly: a restarted daemon re-enqueues the still-`running` row and ingests the
/// completion it missed while down. Returns whether it ingested.
///
/// Not span-instrumented: this runs on every reconcile of a `running` row and answers "no" for the
/// whole life of the run, so a span here is a 0-second trace per tick. [`complete_run`] — the branch
/// that actually ingests — carries the span.
pub async fn ingest_completion(db: &Db, cfg: &ControllerCfg, key: &str) -> Result<bool> {
    let Some(run) = crate::runs::store::running_run_for_issue(db.pool(), key).await? else {
        return Ok(false);
    };
    // The artifact store first (an explicit `session_uri`, or the run-session artifact a local
    // launcher stored), else scrape the finished pod's own logs — a loop-run pod publishes only
    // onto its emptyDir, so for the pod path the scrape IS the normal completion edge.
    let session = match locate_session(db, &run).await? {
        Some(content) => Some(content),
        None => scrape_pod_session(db, cfg, key, &run).await?,
    };
    let Some(session) = session else {
        return Ok(false); // no complete log yet — the run is still in flight
    };
    if let Err(e) = ingest::validate_terminal_session(&session) {
        fail_run_without_terminal_evidence(
            db,
            key,
            &run.run_id,
            run.pod.as_deref(),
            ParkReason::TerminalSessionInvalid {
                run_id: run.run_id.clone(),
                detail: e.to_string(),
            },
        )
        .await?;
        return Ok(false);
    }
    let emission = crate::launches::emission::EmissionCtx::from_cfg(cfg);
    let disposition = complete_run(
        db,
        key,
        run.scope,
        &run.run_id,
        run.pod.as_deref(),
        &session,
        emission.as_ref(),
    )
    .await?;
    // The run's session is ingested and its cost booked (once, in `complete_run`'s ingest):
    // close the WorkPod ledger row + GC the run pod. Best-effort — a leaked pod is swept
    // later, never fatal to the completion.
    if let Some(pod) = run.pod.as_deref()
        && let Err(e) = crate::runs::workpod::collect_run_pod(
            db,
            crate::runs::workpod::active_dispatcher().as_ref(),
            &cfg.pod_namespace,
            pod,
            disposition,
        )
        .await
    {
        tracing::warn!(pod_name = %pod, issue_key = %key, error = format!("{e:#}"), "workpod: collecting run pod failed (continuing)");
    }
    Ok(true)
}

/// Locate a run's published session log: its recorded `session_uri` if it names the artifact store
/// or a readable local file, else the store keyed by the run id (the seam a local launcher — or a
/// dispatcher fake — publishes through). `None` means no log exists yet (the run hasn't finished
/// publishing). An S3 URI is out of v1's scope (the controller targets local/GitWorld domains
/// first), so it's a hard error, not a silent skip.
async fn locate_session(db: &Db, run: &crate::runs::model::RunningRun) -> Result<Option<String>> {
    if let Some(uri) = run.session_uri.as_deref() {
        if uri.starts_with("s3://") {
            anyhow::bail!(
                "run {} published to S3 ({uri}); S3 completion ingest is not wired in v1 \
                 (local/GitWorld domains only)",
                run.run_id
            );
        }
        if crate::runs::blob_store::run_id_of_session_uri(uri).is_none() {
            let p = std::path::Path::new(uri);
            if p.exists() {
                return Ok(Some(
                    std::fs::read_to_string(p)
                        .map_err(|e| anyhow::anyhow!("reading session log {uri}: {e}"))?,
                ));
            }
        }
    }
    crate::runs::blob_store::get_run_session(db.pool(), &run.run_id).await
}

/// Prefer the Tier 2 drop-box run-session over the log scrape. A loop pod carrying the ingest env
/// POSTs its `state/session.jsonl` (gzipped) to the drop-box as its final act, so a present
/// artifact is the authoritative copy. `None` = no drop-box artifact (an old engine image, or a
/// run whose POST never landed): the caller falls back to the `SESSION`-delimiter scrape, which
/// stays the fallback until R4. A corrupt artifact returns `Err` and the caller also falls back to
/// the scrape rather than wedging the completion.
async fn adopt_dropbox_run_session(db: &Db, pod: &str) -> Result<Option<String>> {
    let owner = crate::runs::blob_store::ArtifactOwner::PodEvidence {
        pod: pod.to_string(),
    };
    let Some(payload) = crate::runs::blob_store::get_artifact(
        db.pool(),
        &owner,
        crucible_contract::ArtifactKind::RunSession.as_str(),
    )
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(crate::runs::blob_store::gunzip_maybe(&payload.data)?))
}

/// Re-own the pod's `run-files` drop-box artifact to the run, so the files a run's tasks captured
/// outlive the pod exactly as long as its session does ([`crate::runs::run_files`]). The bytes are the
/// gzipped tar the loop POSTed and are stored unchanged; the tar is only ever read on the way out.
///
/// Independent of the session: a run whose tasks declared no files POSTs no artifact and there is
/// nothing to adopt, and a failure here is logged and dropped rather than wedging the completion —
/// the session is the run's authoritative record and must land regardless.
async fn adopt_dropbox_run_files(db: &Db, run_id: &str, pod: &str) -> Result<bool> {
    let owner = crate::runs::blob_store::ArtifactOwner::PodEvidence {
        pod: pod.to_string(),
    };
    let Some(payload) = crate::runs::blob_store::get_artifact(
        db.pool(),
        &owner,
        crucible_contract::ArtifactKind::RunFiles.as_str(),
    )
    .await?
    else {
        return Ok(false);
    };
    crate::runs::blob_store::put_run_files(db.pool(), run_id, payload.data).await?;
    Ok(true)
}

/// The pod leg of the completion edge: a loop-run pod writes `session.jsonl` onto its own emptyDir
/// (never the controller's storage), and its wrapper `cat`s the file to stdout behind a delimiter —
/// so when nothing is stored, peek the pod's phase and, once terminal, take the drop-box artifact
/// or scrape that dump ([`crate::runs::workpod::extract_run_session_logs`]). Either way the session is
/// persisted as the run's artifact first ([`crate::runs::blob_store::put_run_session`]), which keeps the
/// rest of the ingest path (and crash recovery: a restarted daemon re-reads the stored artifact,
/// not the by-then-GC'd pod) unchanged.
///
/// Outcomes: `Some(session)` — recovered, ingest proceeds. `None` quietly — the pod is still
/// running (or its phase/logs were momentarily unreadable; the next completion event or startup
/// re-enqueue re-drives). A TERMINAL pod whose logs carry no session material is the loud path:
/// the logs will never grow one, so machine-park the issue with the reason (the
/// `run_scope_and_transition` dead-proposal discipline — visible in the UI, never a silent
/// `running` forever), stamp the run row `no-session`, and retain the pod as `failed` for
/// `kubectl logs`.
async fn scrape_pod_session(
    db: &Db,
    cfg: &ControllerCfg,
    key: &str,
    run: &crate::runs::model::RunningRun,
) -> Result<Option<String>> {
    use crate::runs::workpod::TurnPhase;

    let Some(pod) = run.pod.as_deref() else {
        return Ok(None); // a local/pre-primitive run: no pod to scrape, keep waiting on the store
    };
    let dispatcher = crate::runs::workpod::active_dispatcher();
    // The work_pods row records the pod's cluster; a run predating the ledger has no row and
    // only ever ran on the hub.
    let work_pod = crate::runs::work_pods::get_work_pod(db.pool(), pod).await?;
    let cluster = work_pod
        .as_ref()
        .map(|row| row.cluster.clone())
        .unwrap_or_else(|| crate::runs::clusters::HUB_CLUSTER.to_string());
    let Some(ns) =
        crate::runs::workpod::row_namespace(dispatcher.as_ref(), &cluster, &cfg.pod_namespace)
            .await
    else {
        return Ok(None);
    };
    // A zero-timeout peek: one GET, current phase or TimedOut (still running).
    match dispatcher
        .await_terminal(&cluster, &ns, pod, std::time::Duration::ZERO)
        .await
    {
        Ok(t) if t.phase != TurnPhase::TimedOut => {}
        Ok(t) => {
            if let Some(detail) = t.waiting_detail() {
                crate::runs::work_pods::set_work_pod_state(
                    db.pool(),
                    pod,
                    crate::runs::workpod::WorkPodState::Running,
                    None,
                    Some(detail),
                )
                .await?;
            }
            return Ok(None);
        }
        Err(e) => {
            if matches!(
                crate::runs::workpod::retry::classify_chain(&e),
                Some(crate::runs::workpod::retry::KubeFailure::Gone)
            ) {
                fail_run_without_terminal_evidence(
                    db,
                    key,
                    &run.run_id,
                    Some(pod),
                    ParkReason::RunPodLost {
                        run_id: run.run_id.clone(),
                        pod: pod.to_string(),
                        last_observation: work_pod.and_then(|row| row.error),
                    },
                )
                .await?;
                return Ok(None);
            }
            tracing::warn!(issue_key = %key, run_id = %run.run_id, pod_name = %pod, error = format!("{e:#}"), "run completion: pod phase unreadable; retrying on the next completion event");
            return Ok(None);
        }
    }
    match adopt_dropbox_run_files(db, &run.run_id, pod).await {
        Ok(true) => {
            tracing::info!(issue_key = %key, run_id = %run.run_id, pod_name = %pod, "run completion: captured files adopted from the Tier 2 drop-box");
        }
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(issue_key = %key, run_id = %run.run_id, pod_name = %pod, error = format!("{e:#}"), "run completion: adopting the drop-box run-files failed; the run keeps its session and loses its captured files");
        }
    }
    // The pod is deleted minutes after this, and its log is the only copy of the engine's own
    // output (gateway boot, agent stderr, the wrapper's exit), so keep it whichever way the session
    // arrives. Best-effort: a run completes without it.
    let logs = match dispatcher.logs(&cluster, &ns, pod).await {
        Ok(logs) => Some(logs),
        Err(e) => {
            tracing::warn!(issue_key = %key, run_id = %run.run_id, pod_name = %pod, error = format!("{e:#}"), "run completion: reading the finished pod's logs failed");
            None
        }
    };
    if let Some(logs) = &logs {
        let engine = crate::runs::workpod::engine_log_of(logs);
        if !engine.trim().is_empty()
            && let Err(e) =
                crate::runs::blob_store::put_run_engine_log(db.pool(), &run.run_id, engine).await
        {
            tracing::warn!(issue_key = %key, run_id = %run.run_id, pod_name = %pod, error = format!("{e:#}"), "run completion: keeping the pod's engine log failed");
        }
    }
    // Prefer the Tier 2 drop-box run-session: a new loop pod POSTs it and the wrapper then skips
    // the `SESSION` delimiter, so the drop-box IS the completion edge. The log scrape below stays
    // the fallback for old images and failed POSTs.
    match adopt_dropbox_run_session(db, pod).await {
        Ok(Some(session)) => {
            crate::runs::blob_store::put_run_session(db.pool(), &run.run_id, session.as_bytes())
                .await?;
            tracing::info!(issue_key = %key, run_id = %run.run_id, pod_name = %pod, "run completion: session adopted from the Tier 2 drop-box");
            return Ok(Some(session));
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(issue_key = %key, run_id = %run.run_id, pod_name = %pod, error = format!("{e:#}"), "run completion: drop-box run-session unreadable; falling back to the log scrape");
        }
    }
    let Some(logs) = logs else {
        tracing::warn!(issue_key = %key, run_id = %run.run_id, pod_name = %pod, "run completion: no pod log to scrape the session from; retrying on the next completion event");
        return Ok(None);
    };
    use crate::runs::workpod::RunSessionScrape;
    let reason = match crate::runs::workpod::extract_run_session_logs(&logs) {
        RunSessionScrape::Found(payload) => {
            crate::runs::blob_store::put_run_session(db.pool(), &run.run_id, payload.as_bytes())
                .await?;
            tracing::info!(issue_key = %key, run_id = %run.run_id, pod_name = %pod, "run completion: session log scraped from the finished pod's logs");
            return Ok(Some(payload));
        }
        // Both no-session outcomes are the loud path (machine-park, run `no-session`, retain the pod
        // as `failed`), but with an HONEST reason: the delimiter-but-empty case is a loop that died
        // before publishing (its rc + pre-delimiter tail ARE the failure), distinct from a missing
        // delimiter (rotation/truncation). The evidence rides the park reason so the next person reads
        // it in the UI instead of running `kubectl logs` on a since-GC'd pod.
        RunSessionScrape::DelimiterButNoSession { rc, tail } => ParkReason::NoSessionEmpty {
            run_id: run.run_id.clone(),
            rc,
            tail,
        },
        RunSessionScrape::NoDelimiter { tail } => ParkReason::NoSessionDelimiter {
            run_id: run.run_id.clone(),
            tail,
        },
    };
    tracing::warn!(issue_key = %key, run_id = %run.run_id, pod_name = %pod, "run completion: {reason}");
    crate::issues::transitions::park_failed_run(
        db.pool(),
        db.events(),
        key,
        &reason,
        &run.run_id,
        "no-session",
        Some(pod),
    )
    .await?;
    Ok(None)
}

async fn fail_run_without_terminal_evidence(
    db: &Db,
    key: &str,
    run_id: &str,
    pod: Option<&str>,
    reason: ParkReason,
) -> Result<()> {
    crate::issues::transitions::park_failed_run(
        db.pool(),
        db.events(),
        key,
        &reason,
        run_id,
        "infrastructure-error",
        pod,
    )
    .await?;
    Ok(())
}
