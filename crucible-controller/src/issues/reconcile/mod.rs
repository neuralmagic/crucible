//! The reconcile function: the pure, at-most-one-transition step the queue drives per issue key.
//! It reads the row's current status + the world, performs a single status flip in lockstep with
//! one event-log line, and returns — it never loops, sleeps, or enqueues (that's the queue's
//! job). Overlapping work can't double-act because every flip is a compare-and-set claim
//! (`from` → `to`): claim first, act only if won.
//!
//! The four caps are enforced at the decision points, not around them: a scope turn
//! checks the daily ceiling + scopes/day before it spends; a launch checks the concurrent-pod cap.
//! Crossing a cap declines new spend and records a `capped` ledger row — it parks nothing (parking
//! is for a run that can't proceed, not for a budget that's full).
//!
//! The engine actions themselves (`scope --propose`, the draft-PR approval, the pod launch) are
//! subprocesses to the `crucible` binary ([`crate::issues::engine`]) run under `spawn_blocking`, because
//! this library is a *dependency* of that binary and can't call back into it as a library.

mod grounded;
mod lifecycle;
mod scope;
mod tier;

#[cfg(test)]
// The crate-wide `ENV_LOCK` (an async mutex) is held across the async reconcile so the
// `CRUCIBLE_BIN` subprocess override stays stable for the whole call.
mod tests;

use crate::client::Db;
use crate::config::ControllerCfg;
use crate::event_log::Event;
use crate::issues::model::Issue;
use crate::issues::ranker;
use crate::issues::triage;
use crate::model::Status;
use anyhow::{Context, Result};

use crate::issues::model::split_issue_key;
use grounded::apply_grounded_disposition;
use lifecycle::{reconcile_awaiting, reconcile_building, reconcile_parked, reconcile_running};
use scope::{apply_pod_scope_outcome, reconcile_new, reconcile_scoped};

/// Append a same-status event on `key`: a note on the log without a transition.
pub(crate) async fn annotate(
    db: &Db,
    key: &str,
    status: Status,
    reason: &str,
    evidence: Option<&str>,
) -> Result<()> {
    db.events()
        .append(&Event::now(
            key,
            status.as_str(),
            status.as_str(),
            Some(reason),
            evidence,
        ))
        .await
}

/// One reconcile step for `key`: dispatch on the row's current status and perform at most one
/// transition. An untracked key is a no-op (a stale queue hint — an event is a hint to
/// look, never a payload to trust).
///
/// Deliberately NOT span-instrumented. A tick is a poll, not a unit of work: every enqueue (the
/// discovery timer, the pod watch, the comment poll, a startup re-enqueue) drives one, and the
/// overwhelming majority decide there is nothing to do. A span here made each of those an
/// exported ROOT trace — an idle daemon shipping identical 0-second `reconcile` traces forever,
/// which is what buries real traces in Tempo's search. The spans live on the ACTIONS instead
/// (`spawn_turn_pod`, [`crate::runs::workpod::dispatch_run`], the adopt entries, [`complete_run`]), so a
/// trace exists exactly when the controller moved something. Events in here carry `issue_key`
/// themselves, so the logs keep their context without the span.
pub(crate) async fn reconcile(db: &Db, cfg: &ControllerCfg, key: &str) -> Result<()> {
    let start = std::time::Instant::now();
    let res = reconcile_step(db, cfg, key).await;
    if let Some(m) = db.metrics() {
        let outcome = if res.is_ok() { "ok" } else { "error" };
        m.observe_reconcile(outcome, start.elapsed().as_secs_f64());
    }
    res
}

/// One reconcile step's actual work (timed + span-wrapped by [`reconcile`]).
async fn reconcile_step(db: &Db, cfg: &ControllerCfg, key: &str) -> Result<()> {
    let Some(issue) = crate::issues::store::get_issue(db.pool(), key).await? else {
        return Ok(());
    };
    // Adopt-first: a `running` turn work-pod row for this issue is collected BEFORE any gate below —
    // the autopilot pause, the daily ceiling, the tier/rank cascade. With non-blocking dispatch this
    // is the PRIMARY collection path (not just restart recovery): dispatch creates the pod and
    // returns, the completion watch re-drives the issue on the pod's terminal edge, and this pre-pass
    // scrapes the result. Collection is not spend: the money is already gone, and every one of those
    // gates can (correctly) decline the path that would otherwise re-reach dispatch — a re-driven
    // ScopeNow, its stash deliberately cleared pre-dispatch, never re-reaches it at all.
    if adopt_orphaned_turns(db, cfg, &issue).await? {
        // The adoption's outcome drove the issue transition (scoped/parked/tier stamped); this
        // pass's `issue` snapshot is stale now, so stop here — the next enqueue continues.
        return Ok(());
    }
    match issue.status {
        // Machine-initiated spend: rank/scope cascade, draft-PR approval, pod launch. Gated on the
        // autopilot flag so an admin can pause all new spend without stopping in-flight work.
        // A stashed ScopeNow or redispatch is human-initiated and exempt — the flag pauses the
        // machine, never the humans. A non-upstream kind (an adopted scenario) is the same
        // exemption expressed as a standing capability rather than a one-shot stash: the human
        // adoption already authorized it, so autopilot pausing the machine's own discovery never
        // holds it back either.
        Status::New | Status::Scoped | Status::AwaitingApproval
            if !cfg.autopilot_enabled()
                && issue.scope_now_justification.is_none()
                && issue.redispatch_justification.is_none()
                && issue.kind.has_upstream() =>
        {
            tracing::debug!(issue_key = %key, status = %issue.status.as_str(), "autopilot disabled, skipping machine-initiated reconcile");
            Ok(())
        }
        Status::New => reconcile_new(db, cfg, &issue).await,
        Status::Scoped => reconcile_scoped(db, cfg, &issue).await,
        Status::AwaitingApproval => reconcile_awaiting(db, cfg, &issue).await,
        // In-flight build work — like a `running` run, not new machine spend, so it proceeds even
        // when the autopilot pause holds the machine-initiated states above.
        Status::Building => reconcile_building(db, cfg, &issue).await,
        Status::Running => reconcile_running(db, cfg, &issue).await,
        Status::Parked => reconcile_parked(db, cfg, &issue).await,
        // `pr-open` waits on a human merge, `done` is terminal — record only, no action.
        Status::PrOpen | Status::Done => Ok(()),
    }
}

/// The adopt-first pre-pass of [`reconcile_step`]: collect any orphaned turn — a `running`
/// work-pod row for this issue whose in-band collector is gone — through the same adoption entries
/// dispatch uses ([`crate::runs::workpod::adopt_scope_turn`] / [`crate::runs::workpod::adopt_grounded_turn`]),
/// then fold the outcome through the same tails the in-band path uses
/// ([`apply_pod_scope_outcome`] / [`apply_grounded_disposition`]). Adopt-only by construction:
/// these entries never render, create, or queue a pod, so this pre-pass can never turn into new
/// spend no matter which gates it bypassed.
///
/// Non-blocking by design: the pod's phase is PEEKED first ([`crate::runs::workpod::turn_pod_adoptable`], a 1s poll) and
/// only an already-terminal — or vanished — pod enters the collection. A still-running orphan is
/// left alone: the shared pod-completion watch re-drives this key the moment the pod goes
/// terminal, so the single queue worker never parks itself on a 40-90 minute turn here. Returns
/// whether a turn was adopted — the caller then ends the pass, the collection outcome having
/// driven the issue's transition.
async fn adopt_orphaned_turns(db: &Db, cfg: &ControllerCfg, issue: &Issue) -> Result<bool> {
    use crate::runs::workpod::{DispatchOutcome, TurnKind, WorkKind};
    let dispatcher = crate::runs::workpod::active_dispatcher();

    // The scope kind first (the expensive turn); at most one kind is expected in flight per issue.
    let scope_kind = WorkKind::AgentTurn(TurnKind::Scope).label_value();
    if crate::runs::workpod::peek_running_adoptable(
        db,
        cfg,
        dispatcher.as_ref(),
        scope_kind,
        &issue.key,
    )
    .await?
    .is_some()
        && let Some(outcome) =
            crate::runs::workpod::adopt_scope_turn(db, cfg, dispatcher.clone(), &issue.key).await?
    {
        apply_pod_scope_outcome(
            db,
            cfg,
            issue,
            outcome,
            "proposed pack passed check + selftest (scope turn collected on completion)",
        )
        .await?;
        return Ok(true);
    }

    let rank_kind = WorkKind::AgentTurn(TurnKind::GroundedRank).label_value();
    if crate::runs::workpod::peek_running_adoptable(
        db,
        cfg,
        dispatcher.as_ref(),
        rank_kind,
        &issue.key,
    )
    .await?
    .is_some()
    {
        // The hash the verdict stamps under, resolved BEFORE collecting — a fetch hiccup here
        // retries the whole pass with the row still adoptable, instead of collecting a verdict
        // it then can't apply. The recorded rank hash is the exact content a prescope-origin
        // turn was gated on; an escalation-origin turn (no rank recorded yet) falls back to
        // hashing the current content.
        let content_hash = match issue.ranked_content_hash.clone() {
            Some(h) => h,
            None if !issue.kind.has_upstream() => {
                // reconcile_new bypasses the ranker for non-upstream kinds, so they never have a
                // grounded-rank turn to adopt. Defense-in-depth: don't split a non-GitHub key or
                // fetch an upstream that doesn't exist.
                return Ok(false);
            }
            None => {
                let (repo, number) = split_issue_key(&issue.key)?;
                let gh = triage::fetch_issue(&repo, number).await.with_context(|| {
                    format!("fetching {} to adopt its grounded turn", issue.key)
                })?;
                let body = gh.body.clone().unwrap_or_default();
                ranker::content_hash(&gh.title, &body, &gh.labels)
            }
        };
        if let Some(outcome) =
            crate::runs::workpod::adopt_grounded_turn(db, cfg, dispatcher, &issue.key).await?
        {
            if let DispatchOutcome::Verdict(g) = outcome {
                // The collection CAS already booked the cost (`cost_ledgered = true`).
                apply_grounded_disposition(db, cfg, issue, &content_hash, g, true).await?;
            }
            // Failed: the row records the reason (the wedge is cleared, the issue re-drives
            // normally). AlreadyCollected: the winner owns it. Queued: unreachable — adoption
            // never queues.
            return Ok(true);
        }
    }
    Ok(false)
}
