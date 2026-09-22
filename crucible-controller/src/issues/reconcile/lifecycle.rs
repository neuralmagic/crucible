use crate::client::Db;
use crate::config::ControllerCfg;
use crate::event_log::Event;
use crate::issues::approvals;
use crate::issues::model::Issue;
use crate::model::{ParkReason, ParkedBy, Status};
use crate::runs::completion::ingest_completion;
use crate::runs::model::{NewRun, new_run_id};
use anyhow::{Context, Result};

/// Park the issue (machine) on a permanent build-plan failure. A malformed `[build]` table can never
/// plan — record the spec error as the reason and stop, rather than returning `Err` and letting the
/// queue retry a deterministic failure `park_after` times before parking on a stringified anyhow.
/// `from` is the caller's current state (`awaiting-approval` at the plan-time check, `building` once
/// the run is build-blocked).
async fn park_on_permanent_plan_error(
    db: &Db,
    issue_key: &str,
    from: Status,
    err: &anyhow::Error,
) -> Result<()> {
    let reason = ParkReason::ImageBuildFailed {
        evidence: Some(crate::model::truncate_chain(&format!("{err:#}"))),
    };
    crate::issues::transitions::park(
        db.pool(),
        db.events(),
        issue_key,
        from,
        &reason,
        ParkedBy::Machine,
    )
    .await?;
    Ok(())
}

async fn plan_builds_or_park(
    db: &Db,
    issue: &Issue,
    from: Status,
    pack_dir: &std::path::Path,
) -> Result<Option<Vec<crate::builds::lifecycle::BuildRequest>>> {
    match crate::builds::lifecycle::plan_builds(pack_dir) {
        Ok(r) => Ok(Some(r)),
        Err(e) if crate::builds::lifecycle::is_permanent_plan_error(&e) => {
            park_on_permanent_plan_error(db, &issue.key, from, &e).await?;
            Ok(None)
        }
        Err(e) => Err(e).with_context(|| format!("planning builds for {}", issue.key)),
    }
}

/// `awaiting-approval`: a no-op until a human records approval on the scope (the approval watch
/// stamps `approved_at`). Once approved, and within the daily ceiling, dispatch the loop run onto the
/// WorkPod primitive ([`crate::runs::workpod::dispatch_run`]) → `running`. The primitive holds the per-kind
/// concurrency cap (`max_concurrent_pods`): over it the dispatch declines ([`RunAdmission::Capped`])
/// and the row simply stays `awaiting-approval`, the durable per-issue queue re-driven when a slot
/// frees (no separate work-pod queue row — the issue state IS the queue for a run).
pub(super) async fn reconcile_awaiting(db: &Db, cfg: &ControllerCfg, issue: &Issue) -> Result<()> {
    let Some(scope) = crate::issues::store::latest_scope_for_issue(db.pool(), &issue.key).await?
    else {
        return Ok(());
    };
    // The pack may have drifted since freeze — mark it stale + refresh the approval-PR comment so the
    // human at the approval sees it (advisory: a hiccup logs, never parks the row).
    if let Err(e) = approvals::reconcile_staleness(
        db,
        &issue.key,
        Status::AwaitingApproval,
        &scope,
        &issue.kind,
    )
    .await
    {
        tracing::warn!(issue_key = %issue.key, error = format!("{e:#}"), "approvals: staleness check failed");
    }
    if !scope.is_approved() {
        // The approval is closed: wait. Comments may mark the pack stale but never launch it.
        return Ok(());
    }

    let day = crate::clock::today_utc();
    if db
        .decline_if_over_ceiling(&day, cfg.effective().daily_cost_ceiling)
        .await?
    {
        return Ok(());
    }

    // A pack that declares image builds can't launch until every image has a pinned digest. Enter
    // the `building` state and let [`reconcile_building`] own the dispatch/poll — a pack with no
    // `[build]` block launches directly here, exactly as before the feature. Planning is a
    // hermetic manifest read (no `spawn_blocking`).
    let pack = crate::playbooks::packs::materialize_pack_or_empty(db.pool(), &issue.key).await?;
    let Some(requests) =
        plan_builds_or_park(db, issue, Status::AwaitingApproval, pack.path()).await?
    else {
        return Ok(());
    };
    if !requests.is_empty() {
        crate::issues::transitions::transition(
            db.pool(),
            db.events(),
            &issue.key,
            Status::AwaitingApproval,
            Status::Building,
            Some("pack declares image builds; blocking the run until they pin"),
            None,
        )
        .await?;
        return Ok(());
    }

    launch_approved_run(
        db,
        cfg,
        issue,
        &scope,
        Status::AwaitingApproval,
        pack.path(),
    )
    .await
}

/// Launch the approved pack's loop run and advance `from` → `running` (`from` is
/// `awaiting-approval` for a build-free pack, `building` once its images pinned). Mints the run id,
/// dispatches the loop pod through the WorkPod primitive (non-blocking), and — on a won CAS —
/// records the run + logs the transition + clears any redispatch stash. A full concurrency cap
/// declines with a `capped` ledger row, leaving the issue at `from` to re-drive when a slot frees.
async fn launch_approved_run(
    db: &Db,
    cfg: &ControllerCfg,
    issue: &Issue,
    scope: &crate::issues::model::Scope,
    from: Status,
    pack_dir: &std::path::Path,
) -> Result<()> {
    // Mint the run id before the dispatch so the pod is stamped with it (and the completion edge maps
    // back). The primitive renders the pack's loop pod, stamps it controller-owned (managed-by +
    // issue-key + work-kind labels the pod watch/sweep read), tracks it in `work_pods`, and creates
    // it — non-blocking, so this reconcile returns while the (hours-long) run proceeds.
    let run_id = new_run_id(&issue.key);
    // The digests the scope's builds pinned, keyed by image repo — the loop-pod render rewrites
    // any matching container image to `repo@sha256:…` so the run consumes the built image, not the
    // static manifest one. Empty for a build-free pack (the common case).
    let build_digests: std::collections::BTreeMap<String, String> =
        crate::builds::store::builds_for_scope(db.pool(), scope.id)
            .await?
            .into_iter()
            .filter_map(|b| b.digest_ref.map(|d| (b.image, d)))
            .collect();
    let dispatch = crate::playbooks::providers::resolve_for_issue(
        db.pool(),
        issue,
        crate::playbooks::providers::WorkloadClass::Autoresearch,
    )
    .await?;
    let admission = crate::runs::workpod::dispatch_run(
        db,
        cfg,
        crate::runs::workpod::active_dispatcher(),
        &issue.key,
        &run_id,
        pack_dir,
        &build_digests,
        // The NAME; the dispatch resolves it against the configured set and projects the JSON onto
        // the loop container as BROKER_CODEGEN_TOOLS_OVERLAY.
        issue.codegen_contract.as_deref(),
        crate::runs::workpod::RunRenderOpts::for_loop(
            cfg,
            &issue.repo,
            crate::playbooks::providers::AgentSelection::from_resolved(dispatch.as_ref()),
        ),
        // A loop run is machine-driven: it carries no session, so a repo that binds a secret
        // refuses until somebody launches it with an identity behind them.
        Some(&crate::runs::workpod::LaunchSecrets {
            scope: crate::secrets::launch::Scope::repo(&issue.repo),
            launcher: crate::authz::model::Principals::default(),
            revision: crate::secrets::launch::OwnedRevision::Published(None),
            provider: cfg.secret_provider.clone(),
            inference_provider: dispatch.map(|d| d.provider),
            exposure: scope.exposure.clone(),
        }),
    )
    .await;
    // Surface the real render/create failure before the retry/park machinery eats it: the bare
    // "dispatching the loop run" context alone never reaches the event log, so a bad pack manifest
    // (the un-runnable-freeze class this branch fixes on the scope side) died invisibly. Best-effort
    // event, then propagate the untouched error so the row stays at `from` and retries.
    let admission = match admission {
        Ok(a) => a,
        Err(e) => {
            let evidence = crate::model::truncate_chain(&format!("{e:#}"));
            let _ = db
                .events()
                .append(&Event::now(
                    &issue.key,
                    from.as_str(),
                    from.as_str(),
                    Some("loop-run dispatch failed"),
                    Some(&evidence),
                ))
                .await;
            return Err(e);
        }
    };
    let (pod_name, location) = match admission {
        crate::runs::workpod::RunAdmission::Launched { pod_name, location } => (pod_name, location),
        crate::runs::workpod::RunAdmission::SecretsRefused { reason } => {
            crate::issues::transitions::park(
                db.pool(),
                db.events(),
                &issue.key,
                from,
                &crate::model::ParkReason::SecretsUnresolved { detail: reason },
                crate::model::ParkedBy::Machine,
            )
            .await?;
            return Ok(());
        }
        crate::runs::workpod::RunAdmission::Capped => {
            // The per-kind concurrency cap is full: decline, record `capped`, and leave the row at
            // `from` to re-drive when a slot frees.
            db.ledger_append(None, "capped", 0.0).await?;
            return Ok(());
        }
        crate::runs::workpod::RunAdmission::ContractRejected => return Ok(()),
    };

    let mut tx = db.pool().begin().await?;
    if crate::issues::store::claim_issue(&mut *tx, &issue.key, from, Status::Running).await? {
        crate::runs::store::insert_run(
            &mut *tx,
            &NewRun {
                run_id: run_id.clone(),
                scope: Some(scope.id),
                issue: Some(issue.key.clone()),
                identity_digest: scope.pack_digest.clone(),
                status: "running".to_string(),
                pod: Some(pod_name),
                session_uri: None,
                best_score: None,
                cost_usd: None,
            },
        )
        .await?;
        crate::runs::store::set_run_location(&mut *tx, &run_id, &location).await?;
        let ev = Event::now(
            &issue.key,
            from.as_str(),
            "running",
            Some("loop pod launched"),
            Some(&run_id),
        );
        crate::event_log::insert(&mut *tx, &ev).await?;
        // A redispatch's one-shot stash has served its purpose (it reopened the approval and exempted
        // this dispatch from the autopilot pause); clear it so a later re-drive can't double-launch.
        // A no-op for the normal (non-redispatch) launch — the column is already NULL.
        crate::runs::work_pods::clear_redispatch(&mut *tx, &issue.key).await?;
        tx.commit().await?;
        db.events().publish(&ev);
    }
    Ok(())
}

/// `building`: the approved pack's declared image builds are in flight; the run stays blocked
/// until every image has a pinned digest. Each pass reconciles the build PLAN against the ledger
/// ([`crate::builds::lifecycle::drive_scope_builds`]) — (re)dispatching any missing/still-pending build
/// idempotently under the per-kind cap, polling the dispatched ones. Every build `succeeded` with
/// a digest → launch the run (`building` → `running`). A failed/timed-out build (or one the
/// backend keeps refusing) parks the issue (machine) with the build-log pointer as the reason. NOT gated on
/// the autopilot pause — a build in flight is in-progress work, like a `running` run, not new spend.
pub(super) async fn reconcile_building(db: &Db, cfg: &ControllerCfg, issue: &Issue) -> Result<()> {
    let Some(scope) = crate::issues::store::latest_scope_for_issue(db.pool(), &issue.key).await?
    else {
        return Ok(());
    };
    let pack = crate::playbooks::packs::materialize_pack_or_empty(db.pool(), &issue.key).await?;
    let Some(requests) = plan_builds_or_park(db, issue, Status::Building, pack.path()).await?
    else {
        return Ok(());
    };
    if requests.is_empty() {
        // The pack no longer declares builds (a re-freeze dropped them, or a spurious state): the run
        // is no longer build-blocked, so self-heal back to the approval where the standing approval
        // re-launches it directly.
        crate::issues::transitions::transition(
            db.pool(),
            db.events(),
            &issue.key,
            Status::Building,
            Status::AwaitingApproval,
            Some("pack declares no image builds — self-healed back to the approval gate"),
            None,
        )
        .await?;
        return Ok(());
    }

    // A scope whose FIXED build demand exceeds the per-kind cap can never be admitted (the run needs
    // every image, so the whole set has to be in flight together). Park with a clear reason rather
    // than re-driving a dispatch that will only ever be capped.
    let cap = cfg.profile.build_pod_cap;
    if requests.len() > cap as usize {
        let reason = ParkReason::BuildCapUnadmittable {
            declared: requests.len(),
            cap,
        };
        crate::issues::transitions::park(
            db.pool(),
            db.events(),
            &issue.key,
            Status::Building,
            &reason,
            ParkedBy::Machine,
        )
        .await?;
        return Ok(());
    }

    match crate::builds::lifecycle::drive_scope_builds(db, cfg, &issue.key, scope.id, &requests)
        .await?
    {
        crate::builds::lifecycle::BuildsProgress::AllReady => {
            // Every image is built + pinned — the block lifts. Launch the run exactly as a build-free
            // pack would, advancing `building` → `running`.
            launch_approved_run(db, cfg, issue, &scope, Status::Building, pack.path()).await
        }
        // Still dispatching/building (or transiently capped): stay `building`, re-driven next pass.
        crate::builds::lifecycle::BuildsProgress::Waiting => Ok(()),
        crate::builds::lifecycle::BuildsProgress::Blocked { evidence } => {
            // A build failed or timed out: park with the build-log pointer as the reason (a machine
            // park a re-freeze/content change can revive), so the next person needs no pod forensics.
            let reason = ParkReason::ImageBuildFailed { evidence };
            crate::issues::transitions::park(
                db.pool(),
                db.events(),
                &issue.key,
                Status::Building,
                &reason,
                ParkedBy::Machine,
            )
            .await?;
            Ok(())
        }
    }
}

/// `running`: normal completion stays event-driven — the kube pod watch sees the pod finish and
/// calls [`complete_run`] (ingest + advance) in seconds. A reconcile of a live run acts on two
/// exceptions: a **runless** `running` row (no live run behind the status — an unparked no-session
/// park, or a crash-lost `record_run`) self-heals back to the approval gate, where a standing
/// approval re-dispatches; and the upstream issue **closing** stops the run (via the run's
/// own stop machinery, so a kept candidate still lands its draft PR) and parks it "upstream closed" —
/// budget is not spent on solved problems. A no-op when GitHub isn't configured or the issue is open.
pub(super) async fn reconcile_running(db: &Db, cfg: &ControllerCfg, issue: &Issue) -> Result<()> {
    // The pod-completion edge: the pod watch enqueued this key because its loop pod
    // finished, so fold the published session log in and advance to `done`. `false` means the run is
    // still in flight (a stale/upstream-poll re-enqueue), so fall through to the upstream-close check.
    match ingest_completion(db, cfg, &issue.key).await {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(issue_key = %issue.key, error = format!("{e:#}"), "ingest: completion check failed")
        }
    }
    // Self-heal the runless-`running` wedge: no run row is live for this issue (the last run went
    // terminal — e.g. a no-session park later unparked back to `running` — or a crash lost the row
    // between the status claim and `record_run`). The completion edge already fired or never will,
    // so nothing event-driven converges this. Fall back to the approval gate: a standing approved
    // scope re-dispatches on the next pass, an unapproved one waits at the approval — both correct.
    if crate::runs::store::running_run_for_issue(db.pool(), &issue.key)
        .await?
        .is_none()
    {
        crate::issues::transitions::transition(
            db.pool(),
            db.events(),
            &issue.key,
            Status::Running,
            Status::AwaitingApproval,
            Some("running with no live run — self-healed back to the approval gate"),
            None,
        )
        .await?;
        return Ok(());
    }
    if let Err(e) = approvals::reconcile_upstream_close(db, cfg, &issue.key, &issue.kind).await {
        tracing::warn!(issue_key = %issue.key, error = format!("{e:#}"), "approvals: upstream-close check failed");
    }
    Ok(())
}

/// `parked`: a *human* park is sticky — no comment overrides an operator. A *machine*
/// stale-park auto-unparks to `new` once `upstream_updated_at` moves back inside the rank
/// horizon (the poll source refreshes it); the next reconcile pass scopes the revived row.
/// Other machine parks stay resting, which keeps a deliberate startup re-enqueue of parked
/// rows harmless.
pub(super) async fn reconcile_parked(db: &Db, cfg: &ControllerCfg, issue: &Issue) -> Result<()> {
    if issue.parked_by == Some(ParkedBy::Machine)
        && issue
            .park_reason()
            .is_some_and(|r| r.auto_unparkable_on_activity())
        && cfg.effective().rank_horizon_days > 0
    {
        let cutoff = jiff::Timestamp::now()
            - jiff::Span::new().hours(i64::from(cfg.effective().rank_horizon_days) * 24);
        let cutoff_rfc3339 = crate::clock::stamp(cutoff);
        let now_within_horizon = issue
            .upstream_updated_at
            .as_deref()
            .is_some_and(|t| t >= cutoff_rfc3339.as_str());
        if now_within_horizon {
            crate::issues::transitions::unpark(
                db.pool(),
                db.events(),
                &issue.key,
                Some("stale-park auto-unparked: upstream activity resumed"),
                None,
            )
            .await?;
        }
    }
    Ok(())
}
