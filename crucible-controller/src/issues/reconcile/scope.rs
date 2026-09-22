#![allow(clippy::disallowed_macros)]

use crate::client::Db;
use crate::config::ControllerCfg;
use crate::event_log::Event;
use crate::issues::approvals;
use crate::issues::engine;
use crate::issues::model::{Issue, NewScope};
use crate::issues::reconcile::annotate;
use crate::issues::reconcile::grounded::TierGate;
use crate::issues::reconcile::grounded::{PrescopeGate, confirm_grounded_prescope};
use crate::issues::reconcile::tier::confirm_tier;
use crate::model::{ParkReason, ParkedBy, Status};
use anyhow::{Context, Result};
use crucible_contract::Tier;

/// `new`: within the daily + scopes/day caps, run `crucible scope --propose`. A surviving pack →
/// `scoped`; a proposal that dies in check/selftest (or a proposer that declines) → `parked`
/// (machine) with the failing stage as the reason. Either way the turn's cost is ledgered as a
/// `scope` row (the scopes/day cap counts these).
///
/// The first step is [`confirm_tier`]: the one bounded LLM ranking call that is the sole source
/// of this issue's tier. The second is [`confirm_grounded_prescope`]: a code-grounded
/// confirmation required before the scope turn itself spends. Only then does budget reach the
/// (expensive) proposal turn.
pub(super) async fn reconcile_new(db: &Db, cfg: &ControllerCfg, issue: &Issue) -> Result<()> {
    // ScopeNow fast-path: a human triggered an immediate scope, bypassing ALL gates.
    if issue.scope_now_justification.is_some() {
        return reconcile_scope_now(db, cfg, issue).await;
    }

    // Non-upstream kinds (scenarios today) have no ranker signal and no upstream freshness to
    // gauge: the human adoption IS the tier/priority authorization, ledgered at adopt time. They
    // bypass exactly the gates ScopeNow bypasses — rank horizon, tier + grounded prescope, and the
    // daily/scopes-per-day caps — and go straight to the scope turn. Keying on has_upstream() (not
    // on the scope_now stash) makes this idempotent across pod re-drives: a scenario always takes
    // this branch regardless of stash state, so a non-blocking pod dispatch that re-drives the key
    // won't fall through into the rank-horizon gate and get parked.
    // A playbook launch has no scope turn ahead of it: the pack is registered and pinned, and the
    // validated POST that wrote its launch row is the authorization the scope/approval gates stand
    // in for elsewhere. Keyed on the kind, so a re-drive after the non-blocking dispatch takes the
    // same branch rather than falling into the scope path.
    if matches!(issue.kind, crate::issues::model::InputKind::Playbook { .. }) {
        return crate::runs::launch::launch(db, cfg, issue).await;
    }

    if !issue.kind.has_upstream() {
        let max_cost = cfg.effective().per_reconcile_cost;
        return run_scope_and_transition(db, cfg, issue, max_cost, "adopted scenario").await;
    }

    // Resolve the overridable knobs once for this reconcile pass (default < env < override).
    let eff = cfg.effective();
    let day = crate::clock::today_utc();
    if db
        .decline_if_over_ceiling(&day, eff.daily_cost_ceiling)
        .await?
    {
        return Ok(());
    }

    if eff.rank_horizon_days > 0 {
        let cutoff =
            jiff::Timestamp::now() - jiff::Span::new().hours(i64::from(eff.rank_horizon_days) * 24);
        let cutoff_rfc3339 = crate::clock::stamp(cutoff);
        let beyond_horizon = issue
            .upstream_updated_at
            .as_deref()
            .is_none_or(|t| t < cutoff_rfc3339.as_str());
        if beyond_horizon {
            let reason = ParkReason::StaleRankHorizon {
                days: eff.rank_horizon_days,
            };
            if crate::issues::transitions::park(
                db.pool(),
                db.events(),
                &issue.key,
                Status::New,
                &reason,
                ParkedBy::Machine,
            )
            .await?
            {
                db.events()
                    .append(&Event::now(
                        &issue.key,
                        "new",
                        "parked",
                        Some(&format!(
                            "upstream_updated_at {:?} older than {} days",
                            issue.upstream_updated_at, eff.rank_horizon_days
                        )),
                        None,
                    ))
                    .await?;
            }
            return Ok(());
        }
    }

    if matches!(
        confirm_tier(db, cfg, issue).await?,
        TierGate::Parked | TierGate::Deferred | TierGate::Unranked
    ) {
        return Ok(());
    }

    // Re-fetch: `confirm_tier` just recorded (or confirmed a cache hit on) this issue's tier and
    // `ranked_content_hash` — read it back so the pre-scope gate has the exact hash to key its own
    // cache against.
    let issue = crate::issues::store::get_issue(db.pool(), &issue.key)
        .await?
        .with_context(|| format!("{} vanished mid-reconcile", issue.key))?;
    let Some(ranked_hash) = issue.ranked_content_hash.clone() else {
        // Shouldn't happen (confirm_tier only proceeds once a hash is on record), but the ranker
        // is the sole gate here too — no hash on record, no spend.
        return Ok(());
    };
    if matches!(
        confirm_grounded_prescope(db, cfg, &issue, &ranked_hash).await?,
        PrescopeGate::Parked | PrescopeGate::Deferred
    ) {
        return Ok(());
    }

    // Re-check: a rank call may itself have spent against the same daily ceiling. Re-resolve so a
    // mid-reconcile override lands (the ranking gate above may have taken seconds).
    let eff = cfg.effective();
    if db
        .decline_if_over_ceiling(&day, eff.daily_cost_ceiling)
        .await?
    {
        return Ok(());
    }
    if crate::issues::store::count_scopes_on_day(db.pool(), &day).await?
        >= i64::from(eff.max_scopes_per_day)
    {
        db.ledger_append(None, "capped", 0.0).await?;
        return Ok(());
    }

    let max_cost = eff.per_reconcile_cost;
    run_scope_and_transition(
        db,
        cfg,
        &issue,
        max_cost,
        "proposed pack passed check + selftest",
    )
    .await
}

/// Folds the affected-repos hint list, the pinned git ref, and the broker codegen contract into a
/// scenario's goal text as trailing blocks, so the pack agent sees all three even though only
/// `affected_repos[0]` (= `issues.repo`) is what gets cloned. A single-entry list adds nothing
/// already implied by the clone target.
///
/// The ref sentence is NOT decoration: `--repo-ref` only controls what the turn pod checks out, and
/// the manifest the pack agent authors is what every later run clones from. Without this the agent
/// writes a manifest with no `[repo] ref`, and the run silently drops back to the default branch.
///
/// The contract block is the same story for measurement: `--broker-measure` tells the turn that
/// measurement is brokered, but the pack's `[measure]` table is what the broker actually reads at run
/// time, and it must match the overlay the controller will project — byte for byte, which is why the
/// JSON goes in verbatim rather than prose.
pub(super) fn render_goal_framing(
    body: &str,
    repos: &[String],
    git_ref: Option<&str>,
    codegen_contract: Option<&str>,
) -> String {
    let mut out = body.to_string();
    if repos.len() > 1 {
        let list = repos
            .iter()
            .map(|r| format!("- {r}"))
            .collect::<Vec<_>>()
            .join("\n");
        out.push_str(&format!(
            "\n\nAffected repos (hint from the human; the first is the cloned repo, the rest \
             are framing — propose the actual repo set your fix needs):\n{list}"
        ));
    }
    if let Some(git_ref) = git_ref {
        out.push_str(&format!(
            "\n\nThis work targets the `{git_ref}` ref of the cloned repo, not its default branch: \
             the manifest you author must pin it as `[repo] ref = \"{git_ref}\"`."
        ));
    }
    if let Some(contract) = codegen_contract {
        out.push_str(&format!(
            "\n\nThis work is measured on GPUs through the broker, not on the loop pod. The codegen \
             tool contract is:\n\n```json\n{contract}\n```\n\nThe manifest you author must carry \
             exactly this contract in its `[measure]` table, and its judge gate must drive the \
             broker MCP tools (`codegen_build` / `codegen_benchmark` / `codegen_profile`) rather \
             than running a measurement command locally."
        ));
    }
    out
}

/// Resolve the issue's codegen-contract NAME against the deploy's configured set. A name the deploy
/// no longer has is an error, not a fallback to local measure: the alternative silently scopes a
/// GPU-measured problem into a pack that measures on the loop pod, which cannot work and would only
/// be noticed hours later at the run.
pub(super) fn resolve_contract(cfg: &ControllerCfg, issue: &Issue) -> Result<Option<String>> {
    match issue.codegen_contract.as_deref() {
        Some(name) => Ok(Some(
            cfg.broker_contracts
                .get(name)
                .map(str::to_string)
                .with_context(|| {
                    format!(
                        "issue names codegen contract {name:?}, which this controller does not \
                         have configured: set it in CONTROLLER_BROKER_CONTRACTS"
                    )
                })?,
        )),
        None => Ok(None),
    }
}

/// The shared scope-turn tail of [`reconcile_new`] and [`reconcile_scope_now`]: run the turn
/// (pod or local executor), persist the structured [`engine::ScopeReport`] verbatim (success or
/// failure — the ScopeProgress UI renders the per-stage/per-round breakdown from it), then
/// transition the row: survived → `scoped`, dead proposal → machine `parked` with the failing
/// stage as the reason. `scoped_reason` is the event note a survival logs with.
pub(super) async fn run_scope_and_transition(
    db: &Db,
    cfg: &ControllerCfg,
    issue: &Issue,
    max_cost: f64,
    scoped_reason: &str,
) -> Result<()> {
    if cfg.scope_executor == crate::config::ScopeExecutor::Disabled {
        return Ok(());
    }

    let tier = issue.tier.as_deref().and_then(|t| Tier::parse(t).ok());
    // A non-upstream issue's goal is its ledgered free text, not a GitHub fetch — read it now
    // (before the blocking turn) so `scope_propose` (local executor) or `dispatch_scope` (pod
    // executor) can write it to a goal file instead of routing `--issue` into the GitHub Ingest arm.
    // `issues.repo` (= affected_repos[0]) is the clone target; the full affected_repos list rides
    // along here, folded into the same goal text, as a hint the pack agent may override. An
    // authoritative brief rides sideband as `--authoritative`, which flips the propose/refine
    // prompts from de-prescribing the goal to preserving its prescriptions. `git_ref` (scenario
    // rows only) rides BOTH sideband as `--repo-ref`, pinning the pod's own clone, and inside the
    // goal text, because the pack agent has to copy it into the manifest it authors. The codegen
    // contract rides both ways for the same reason: `--broker-measure` sideband, JSON in the text.
    let contract = resolve_contract(cfg, issue)?;
    let (goal_text, authoritative, pack_path) = if issue.kind.has_upstream() {
        (None, false, None)
    } else {
        match crate::issues::store::get_scenario(db.pool(), &issue.key).await? {
            Some(s) => (
                Some(render_goal_framing(
                    &s.body,
                    &s.affected_repos,
                    issue.git_ref.as_deref(),
                    contract.as_deref(),
                )),
                s.authoritative,
                s.pack_path,
            ),
            None => (None, false, None),
        }
    };
    let is_pod = cfg.scope_executor == crate::config::ScopeExecutor::Pod;
    if is_pod {
        let dispatch = crate::playbooks::providers::resolve_for_issue(
            db.pool(),
            issue,
            crate::playbooks::providers::WorkloadClass::Autoresearch,
        )
        .await?;
        let dispatcher = crate::runs::workpod::active_dispatcher();
        let outcome = crate::runs::workpod::dispatch_scope(
            db,
            cfg,
            dispatcher,
            &issue.key,
            &issue.repo,
            max_cost,
            crate::runs::workpod::TurnInputs {
                tier,
                goal_text,
                authoritative,
                git_ref: issue.git_ref.clone(),
                codegen_contract: issue.codegen_contract.clone(),
                pack_path,
                agent: crate::playbooks::providers::AgentSelection::from_resolved(
                    dispatch.as_ref(),
                ),
                inference_provider: dispatch.map(|d| d.provider),
            },
        )
        .await?;
        return apply_pod_scope_outcome(db, cfg, issue, outcome, scoped_reason).await;
    }
    let bin = crate::runs::engine::resolve_bin();
    if let Err(failure) = crate::runs::workpod::admit_contract(
        crate::runs::contract::RequestKind::LocalScope,
        &[crate::runs::contract::DispatchTarget::Binary(bin.clone())],
    )
    .await
    {
        crate::runs::contract::refuse(db, &issue.key, &failure.into_rejection()?).await?;
        return Ok(());
    }
    // The turn drafts into scratch; the surviving tree is tarred into `pack_tarballs`, the
    // durable pack, by `apply_scope_report`.
    let scratch = tempfile::tempdir().context("scope pack scratch dir")?;
    let out = scratch.path().join("pack");
    let key = issue.key.clone();
    let repo = issue.repo.clone();
    let gaming_rounds = cfg.effective().scope_gaming_rounds;
    let skip_gaming_review = cfg.effective().scope_skip_gaming_review;
    let turn_out = out.clone();
    let report = tokio::task::spawn_blocking(move || {
        engine::scope_propose(
            &bin,
            &key,
            &repo,
            &turn_out,
            max_cost,
            tier,
            gaming_rounds,
            skip_gaming_review,
            goal_text.as_deref(),
            authoritative,
        )
    })
    .await??;
    apply_scope_report(
        db,
        cfg,
        issue,
        report,
        None,
        PackSource::LocalTree(&out),
        scoped_reason,
    )
    .await
}

/// Fold a pod scope turn's [`crate::runs::workpod::ScopeOutcome`] into the issue — the shared tail of an
/// in-band dispatch ([`run_scope_and_transition`]) and the adopt-first pre-pass
/// ([`adopt_orphaned_turns`]): one code path, whoever collected the turn.
pub(super) async fn apply_pod_scope_outcome(
    db: &Db,
    cfg: &ControllerCfg,
    issue: &Issue,
    outcome: crate::runs::workpod::ScopeOutcome,
    scoped_reason: &str,
) -> Result<()> {
    match outcome {
        crate::runs::workpod::ScopeOutcome::Report { report, pod_name } => {
            apply_scope_report(
                db,
                cfg,
                issue,
                report,
                Some(pod_name),
                PackSource::PodLogs,
                scoped_reason,
            )
            .await
        }
        crate::runs::workpod::ScopeOutcome::Launched => {
            // Non-blocking dispatch: the scope turn pod was created (or one was already in flight)
            // and no report exists yet. Leave the issue at its current status — the completion watch
            // re-drives it on the pod's terminal edge, where the adopt-first pre-pass collects the
            // report and transitions it.
            Ok(())
        }
        crate::runs::workpod::ScopeOutcome::AlreadyCollected => {
            // A concurrent collector (a timeout sweep, or another re-drive of the key) won the
            // turn's CAS: it persists the report + transitions the issue. This pass does nothing.
            Ok(())
        }
        crate::runs::workpod::ScopeOutcome::Failed(reason) => {
            // Surface the dead dispatch on the issue itself — a WARN in the pod logs is
            // invisible to the UI, and the row would otherwise sit at `new` unexplained.
            annotate(
                db,
                &issue.key,
                issue.status,
                &format!("scope turn failed: {reason}"),
                None,
            )
            .await?;
            Ok(())
        }
    }
}

/// Where a collected turn's surviving pack lives: the pod arm scraped it off the logs as a
/// tarball (`report.pack_tgz`); the local arm wrote a working tree into scratch. Also selects the
/// ledger site (local books here; pod's collection CAS already booked).
pub(super) enum PackSource<'a> {
    PodLogs,
    LocalTree(&'a std::path::Path),
}

/// Compute the exposure of the pack just stored for `key` with the linked engine. Read back from
/// storage, not from the turn's scratch tree: an approval binds to the exact stored bytes.
/// `pr_repo` is the fork a loop run of this pack would be rendered with, so the disclosed
/// `draft-pr` default is the one the run resolves.
async fn extract_scope_exposure(
    pool: &sqlx::PgPool,
    key: &str,
    pr_repo: Option<String>,
) -> Result<crate::playbooks::exposure::Extraction> {
    let Some(pack) = crate::playbooks::packs::materialize_pack(pool, key).await? else {
        anyhow::bail!("no stored pack for {key}");
    };
    let extraction = tokio::task::spawn_blocking(move || {
        crate::playbooks::exposure::extract(pack.path(), pr_repo.as_deref())
    })
    .await
    .context("joining the scope exposure worker")?;
    Ok(crate::playbooks::exposure::Extraction::Declared(
        extraction?,
    ))
}

/// Persist one collected [`engine::ScopeReport`] and transition the issue: survived → store the
/// pack tarball + `scoped`, dead proposal → machine `parked` with the failing stage as the reason.
async fn apply_scope_report(
    db: &Db,
    cfg: &ControllerCfg,
    issue: &Issue,
    report: engine::ScopeReport,
    pod_name: Option<String>,
    source: PackSource<'_>,
    scoped_reason: &str,
) -> Result<()> {
    let is_pod = matches!(source, PackSource::PodLogs);
    let cost = report.cost_usd();

    let report_id = crate::issues::store::insert_scope_report(
        db.pool(),
        &crate::issues::model::NewScopeReport {
            issue_key: issue.key.clone(),
            pod_name,
            survived: report.survived(),
            report_json: report.raw.clone(),
        },
    )
    .await?;
    // The turn's preserved agent transcript (both executor arms deliver it gzipped). Best-effort:
    // the report row above is the evidence of record, a transcript hiccup never fails the turn.
    if let Some(gz) = &report.transcript_gz
        && let Err(e) = crate::issues::store::insert_scope_transcript(
            db.pool(),
            &crate::issues::model::NewScopeTranscript {
                scope_report_id: report_id,
                issue_key: issue.key.clone(),
                transcript_gz: gz.clone(),
            },
        )
        .await
    {
        tracing::warn!(issue_key = %issue.key, error = format!("{e:#}"), "failed to persist the scope transcript");
    }

    if report.survived() {
        // Land the durable pack tarball BEFORE the transition — everything downstream reads it
        // (`reconcile_scoped` pushes the approval-PR branches from a materialization,
        // `dispatch_run` renders the loop pod from one). A survival without a storable pack fails
        // loudly here instead of transitioning to `scoped` over nothing and wedging silently later.
        let landed = match (&source, &report.pack_tgz) {
            (PackSource::LocalTree(tree), _) => {
                crate::playbooks::packs::store_pack_tree(db.pool(), &issue.key, tree)
                    .await
                    .map(|_| ())
            }
            (PackSource::PodLogs, Some(tgz)) => {
                crate::playbooks::packs::store_pack_tarball(db.pool(), &issue.key, tgz)
                    .await
                    .map(|_| ())
            }
            (PackSource::PodLogs, None) => Err(anyhow::anyhow!(
                "the turn pod delivered no recoverable pack blob{}",
                report
                    .pack_error
                    .as_deref()
                    .map(|e| format!(": {e}"))
                    .unwrap_or_default()
            )),
        };
        if let Err(e) = landed {
            let reason = format!("scope survived but the pack handoff failed: {e:#}");
            tracing::warn!(issue_key = %issue.key, %reason, "scope pack handoff failed");
            annotate(db, &issue.key, issue.status, &reason, None).await?;
            return Ok(());
        }
        // A scope stored without its exposure would skip every enforcement that reads it, so any
        // error here — a refusal included — fails the freeze rather than storing the scope.
        let extraction = match extract_scope_exposure(
            db.pool(),
            &issue.key,
            cfg.pr_repo_for(&issue.repo),
        )
        .await
        {
            Ok(extraction) => extraction,
            Err(e) => {
                let reason =
                    format!("scope survived but its exposure could not be extracted: {e:#}");
                tracing::warn!(issue_key = %issue.key, %reason, "scope exposure extraction failed");
                annotate(db, &issue.key, issue.status, &reason, None).await?;
                return Ok(());
            }
        };
        let mut tx = db.pool().begin().await?;
        if crate::issues::store::claim_issue(&mut *tx, &issue.key, Status::New, Status::Scoped)
            .await?
        {
            let scope_id = crate::issues::store::insert_scope(
                &mut *tx,
                &NewScope {
                    issue: issue.key.clone(),
                    pack_digest: report.digest.clone(),
                    check_outcome: Some("PASS".to_string()),
                },
            )
            .await?;
            crate::issues::store::set_scope_exposure(&mut *tx, scope_id, &extraction).await?;
            let ev = Event::now(
                &issue.key,
                "new",
                "scoped",
                Some(scoped_reason),
                report.digest.as_deref(),
            );
            crate::event_log::insert(&mut *tx, &ev).await?;
            tx.commit().await?;
            db.events().publish(&ev);
            // Pod executor already booked the cost in dispatch_scope; local arm books here.
            if !is_pod {
                db.ledger_append(None, "scope", cost).await?;
            }
        }
    } else {
        let reason = report.failure_reason();
        if crate::issues::transitions::park(
            db.pool(),
            db.events(),
            &issue.key,
            Status::New,
            &reason,
            ParkedBy::Machine,
        )
        .await?
            && !is_pod
        {
            db.ledger_append(None, "scope", cost).await?;
        }
    }
    Ok(())
}

/// ScopeNow: a human triggered an immediate scope, bypassing ALL autopilot gates (rank horizon,
/// tier, grounded prescope, daily ceiling, scopes/day). Costs are still booked in the ledger
/// with the actor + justification. The ScopeNow stash is cleared after dispatch regardless of
/// outcome, so a re-enqueue doesn't double-dispatch.
async fn reconcile_scope_now(db: &Db, cfg: &ControllerCfg, issue: &Issue) -> Result<()> {
    let justification = issue
        .scope_now_justification
        .as_deref()
        .unwrap_or("ScopeNow");
    let max_cost = issue
        .scope_now_max_cost
        .unwrap_or(cfg.effective().per_reconcile_cost);

    // Clear the stash FIRST so a crash mid-dispatch doesn't re-trigger.
    crate::runs::work_pods::clear_scope_now(db.pool(), &issue.key).await?;

    run_scope_and_transition(
        db,
        cfg,
        issue,
        max_cost,
        &format!("ScopeNow: {justification}"),
    )
    .await
}

/// `scoped`: open the pack's approval-approval draft PR → `awaiting-approval`.
/// [`engine::open_pack_pr`] pushes the pack + opens the draft PR; when no pack repo is configured it
/// returns `None` and this is a no-op (the row waits at `scoped`, retried on the next re-enqueue).
/// A kind with no PR backlink (e.g. an adopted scenario) has no upstream item to open a draft PR
/// against: it skips straight to `awaiting-approval` with no `approval_pr`, for the UI-native
/// approve endpoint to stamp `approved_at` directly.
pub(super) async fn reconcile_scoped(db: &Db, cfg: &ControllerCfg, issue: &Issue) -> Result<()> {
    // A pack whose PR is already open (a prior run got that far) may have drifted from the upstream
    // issue since freeze — check before doing anything else (advisory: a hiccup logs, never parks).
    if let Some(scope) = crate::issues::store::latest_scope_for_issue(db.pool(), &issue.key).await?
        && scope.approval_pr.is_some()
        && let Err(e) =
            approvals::reconcile_staleness(db, &issue.key, Status::Scoped, &scope, &issue.kind)
                .await
    {
        tracing::warn!(issue_key = %issue.key, error = format!("{e:#}"), "approvals: staleness check failed");
    }

    if !issue.kind.accepts_pr_backlink() {
        crate::issues::transitions::transition(
            db.pool(),
            db.events(),
            &issue.key,
            Status::Scoped,
            Status::AwaitingApproval,
            Some("no PR backlink for this input kind; awaiting UI approval"),
            None,
        )
        .await?;
        return Ok(());
    }

    // No pack repo configured → the approval is off; don't materialize just to find that out.
    if engine::pack_pr_repo().is_none() {
        return Ok(());
    }
    let pack = crate::playbooks::packs::materialize_pack(db.pool(), &issue.key)
        .await?
        .with_context(|| {
            format!(
                "no stored pack for {} — cannot open its approval PR",
                issue.key
            )
        })?;
    let token = crate::runs::engine::resolve_pack_pr_token(cfg).await?;
    let Some(pr_url) = engine::open_pack_pr(&issue.key, pack.path(), token)? else {
        return Ok(());
    };
    // Record the PR before flipping the status: anything that observes `awaiting-approval` (the
    // approval poll, a crash-restarted daemon) must find `approval_pr` set, or the row wedges.
    // A crash between the two leaves the row `scoped`; the retry re-runs the idempotent open.
    if let Some(scope) = crate::issues::store::latest_scope_for_issue(db.pool(), &issue.key).await?
    {
        crate::issues::store::set_scope_approval_pr(db.pool(), scope.id, &pr_url).await?;
    }
    crate::issues::transitions::transition(
        db.pool(),
        db.events(),
        &issue.key,
        Status::Scoped,
        Status::AwaitingApproval,
        Some("pack draft PR opened"),
        Some(&pr_url),
    )
    .await?;
    Ok(())
}
