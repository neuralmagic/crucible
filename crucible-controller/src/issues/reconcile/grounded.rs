use crate::client::Db;
use crate::config::{ControllerCfg, GroundedExecutor};
use crate::issues::engine;
use crate::issues::model::Issue;
use crate::issues::ranker;
use crate::issues::reconcile::annotate;
use crate::model::{ParkReason, ParkedBy, Status};
use anyhow::Result;
use crucible_contract::{Disposition, Tier};

/// Whether a code-grounded escalation turn should run for a text-only verdict of this
/// [`ranker::Confidence`]: on a `low`-confidence verdict (the ranker itself flagged doubt), or when
/// the ranker backend is pinned to openshell outright (`CONTROLLER_RANKER_BACKEND=openshell` —
/// ground every verdict). Ranking decides IF, scope decides HOW; grounding sharpens the IF.
fn grounded_wanted(confidence: ranker::Confidence) -> bool {
    ranker_backend_is_openshell() || confidence == ranker::Confidence::Low
}

fn ranker_backend_is_openshell() -> bool {
    std::env::var("CONTROLLER_RANKER_BACKEND")
        .map(|v| v.eq_ignore_ascii_case("openshell"))
        .unwrap_or(false)
}

/// What a grounded turn attempt came back with — the caller's obligations differ per variant:
enum Grounded {
    /// A verdict to apply. `cost_ledgered` says the executor already booked the turn's cost (the
    /// pod arm books at collection — the single-booking invariant lives in [`crate::runs::workpod`]);
    /// the caller ledgers `rank-grounded` only when this is false (the local arm).
    Verdict {
        verdict: engine::GroundedVerdict,
        cost_ledgered: bool,
    },
    /// A turn pod was launched (non-blocking), or one was already in flight: no verdict exists yet.
    /// The caller defers exactly like [`Grounded::Queued`] (no tier finalize, no text fallback) —
    /// the completion watch re-drives the issue when the pod goes terminal, and the adopt-first
    /// pre-pass collects the verdict there.
    Launched,
    /// The turn queued behind the per-kind cap/budget: deferred, never dropped. The caller must
    /// NOT finalize a rank from the text verdict — the issue re-enters ranking on a later sweep,
    /// which drains the queued turn when a slot/budget frees.
    Queued,
    /// No usable verdict (turn failed / produced nothing / grounding disabled): the caller keeps
    /// the text-only verdict, exactly as before.
    Skipped,
    /// The launch was refused at the engine boundary: the rejection is on the ledger and the issue
    /// is parked. The caller finalizes NOTHING — a text fallback here would rank an issue whose
    /// grounding the operator asked for and never got.
    Refused,
    /// A concurrent collector won the turn's terminal CAS (the pod-watch re-drive racing this pass's
    /// in-band await): it books the cost + applies the verdict. The caller finalizes NOTHING — not
    /// the grounded tier, not the text fallback — and lets the winner rank the issue.
    AlreadyCollected,
}

/// Run the grounded ranking turn for an issue (see [`Grounded`] for the outcome contract). The
/// executor is an explicit config choice ([`GroundedExecutor`], validated loudly at startup), never
/// a silent env gate:
///   * `pod` (in-cluster default): dispatch a controller-owned WorkPod ([`crate::runs::workpod`]).
///   * `local` (dev machines): shell `crucible rank-grounded` against the maintained checkout.
///   * `disabled`: skip grounding entirely.
async fn run_grounded(db: &Db, cfg: &ControllerCfg, issue: &Issue) -> Result<Grounded> {
    let (key, repo) = (issue.key.as_str(), issue.repo.as_str());
    Ok(match cfg.grounded_executor {
        GroundedExecutor::Disabled => Grounded::Skipped,
        GroundedExecutor::Local => {
            let bin = crate::runs::engine::resolve_bin();
            if let Err(failure) = crate::runs::workpod::admit_contract(
                crate::runs::contract::RequestKind::LocalGroundedRank,
                &[crate::runs::contract::DispatchTarget::Binary(bin.clone())],
            )
            .await
            {
                crate::runs::contract::refuse(db, key, &failure.into_rejection()?).await?;
                return Ok(Grounded::Refused);
            }
            match run_grounded_local(cfg, key, repo, bin).await {
                Ok(verdict) => Grounded::Verdict {
                    verdict,
                    cost_ledgered: false,
                },
                Err(e) => {
                    tracing::warn!(issue_key = %key, error = format!("{e:#}"), "grounded rank (local) failed, keeping the text verdict");
                    Grounded::Skipped
                }
            }
        }
        GroundedExecutor::Pod => {
            let repo_url = crate::runs::engine::repo_clone_url(repo);
            match crate::runs::workpod::dispatch_grounded_rank(
                db,
                cfg,
                crate::runs::workpod::active_dispatcher(),
                key,
                &repo_url,
                issue.git_ref.as_deref(),
            )
            .await
            {
                Ok(crate::runs::workpod::DispatchOutcome::Verdict(verdict)) => Grounded::Verdict {
                    verdict,
                    cost_ledgered: true,
                },
                Ok(crate::runs::workpod::DispatchOutcome::Launched) => Grounded::Launched,
                Ok(crate::runs::workpod::DispatchOutcome::Queued) => Grounded::Queued,
                Ok(crate::runs::workpod::DispatchOutcome::Failed) => Grounded::Skipped,
                // A concurrent collector (the pod-watch re-drive racing this in-band await) won the
                // turn's CAS and is applying the verdict itself: this pass finalizes nothing (neither
                // the grounded tier nor the text fallback), leaving the winner to rank the issue.
                Ok(crate::runs::workpod::DispatchOutcome::AlreadyCollected) => {
                    Grounded::AlreadyCollected
                }
                Err(e) => {
                    tracing::warn!(issue_key = %key, error = format!("{e:#}"), "grounded rank WorkPod failed to dispatch, keeping the text verdict");
                    Grounded::Skipped
                }
            }
        }
    })
}

/// The `local` executor arm: shell `crucible rank-grounded` against the controller's maintained
/// per-repo checkout (a dev machine, where a `claude` CLI exists). The git checkout + the turn are
/// blocking, so they run under `spawn_blocking`.
async fn run_grounded_local(
    cfg: &ControllerCfg,
    key: &str,
    repo: &str,
    bin: std::path::PathBuf,
) -> Result<engine::GroundedVerdict> {
    let workspace = engine::checkout_dir(cfg.scratch_root(), repo);
    let repo_url = crate::runs::engine::repo_clone_url(repo);
    let max_cost = cfg.effective().per_reconcile_cost;
    let key = key.to_string();
    let verdict = tokio::task::spawn_blocking(move || -> Result<engine::GroundedVerdict> {
        engine::ensure_checkout(&repo_url, &workspace)?;
        engine::rank_grounded(&bin, &key, &workspace, max_cost, None)
    })
    .await??;
    Ok(verdict)
}

/// Apply one text-only ranking verdict, escalating to a code-grounded turn first when
/// [`grounded_wanted`]. A grounded verdict overrides the tier (same N-park / T3-defer / proceed
/// semantics as any verdict) — or, if the grounded ranker found the ask already implemented,
/// parks the issue as `stale` instead of ever touching `issues.tier` (a `stale` disposition is not
/// a tier; see [`crucible_contract::Disposition`]). A [`Grounded::Queued`] escalation records NOTHING —
/// no tier, no hash, no ledger — so the next sweep re-ranks and re-escalates until the queued turn
/// actually runs (the operator asked for grounding; a capped turn must not silently degrade to the
/// text verdict). The text call's cost ledgers as `rank`; the grounded turn's ledgers as
/// `rank-grounded` only when the executor didn't already book it (the single-booking invariant in
/// [`crate::runs::workpod`] — the pod arm books at collection). Only the CAS winner records evidence +
/// ledgers, so a race never double-charges.
pub(super) async fn apply_verdict(
    db: &Db,
    cfg: &ControllerCfg,
    issue: &Issue,
    hash: &str,
    av: ranker::Verdict,
) -> Result<TierGate> {
    // An off-affinity issue parks before the grounded escalation: affinity is a topical judgment
    // the issue text settles, so a code-grounded turn (which only refines the tier) has nothing
    // to add and must not spend on it — regardless of the verdict's confidence.
    if av.affinity == ranker::Affinity::Unrelated {
        if crate::issues::transitions::park(
            db.pool(),
            db.events(),
            &issue.key,
            Status::New,
            &ParkReason::Unrelated {
                rationale: av.rationale.clone(),
            },
            ParkedBy::Machine,
        )
        .await?
        {
            crate::issues::store::set_ranked_tier(
                db.pool(),
                &issue.key,
                av.tier.as_str(),
                av.affinity.as_str(),
                hash,
            )
            .await?;
            db.ledger_append(
                None,
                "rank",
                av.cost_usd
                    .unwrap_or(cfg.effective().rank_cost_fallback_usd),
            )
            .await?;
            if let Some(m) = db.metrics() {
                m.record_verdict(
                    av.tier.as_str(),
                    av.affinity.as_str(),
                    av.confidence.as_str(),
                    false,
                );
            }
        }
        return Ok(TierGate::Parked);
    }

    let grounded = if grounded_wanted(av.confidence) {
        run_grounded(db, cfg, issue).await?
    } else {
        Grounded::Skipped
    };
    let grounded = match grounded {
        Grounded::Launched => {
            annotate(
                db,
                &issue.key,
                Status::New,
                "grounded escalation turn launched; rank deferred until it completes (the text verdict is not finalized)",
                None,
            )
            .await?;
            return Ok(TierGate::Unranked);
        }
        Grounded::Queued => {
            annotate(
                db,
                &issue.key,
                Status::New,
                "grounded escalation queued behind the per-kind cap/budget; rank deferred (the text verdict is not finalized)",
                None,
            )
            .await?;
            return Ok(TierGate::Unranked);
        }
        // The boundary refused the launch: `refuse` already ledgered it and parked the issue, so
        // there is nothing left to finalize and nothing to retry.
        Grounded::Refused => return Ok(TierGate::Parked),
        Grounded::AlreadyCollected => {
            // The CAS winner owns this turn: finalize nothing, keep no text tier, just let the
            // winner's pass rank the issue. A quiet defer, like Queued but without an event.
            return Ok(TierGate::Unranked);
        }
        Grounded::Verdict {
            verdict,
            cost_ledgered,
        } => Some((verdict, cost_ledgered)),
        Grounded::Skipped => None,
    };
    let api_cost = av
        .cost_usd
        .unwrap_or(cfg.effective().rank_cost_fallback_usd);
    // The grounded cost the RECONCILE still owes to the ledger — `None` when there was no grounded
    // verdict OR the executor already booked it.
    let grounded_cost = grounded
        .as_ref()
        .filter(|(_, ledgered)| !ledgered)
        .map(|(g, _)| g.cost_usd);
    let grounded = grounded.map(|(g, _)| g);

    // What the grounded turn (if any) decided: a tier to proceed on, or `stale` — which is not a
    // tier and short-circuits everything below (park now, never touch `issues.tier`).
    let effective_tier = match &grounded {
        Some(g) => match g.disposition {
            Disposition::Stale => {
                if crate::issues::transitions::park(
                    db.pool(),
                    db.events(),
                    &issue.key,
                    Status::New,
                    &ParkReason::StaleAlreadyImplemented {
                        rationale: g.rationale.clone(),
                    },
                    ParkedBy::Machine,
                )
                .await?
                {
                    annotate(db, &issue.key, Status::New, &g.rationale, Some(hash)).await?;
                    db.ledger_append(None, "rank", api_cost).await?;
                    if let Some(gc) = grounded_cost {
                        db.ledger_append(None, "rank-grounded", gc).await?;
                    }
                    if let Some(m) = db.metrics() {
                        m.record_verdict("none", "stale", av.confidence.as_str(), true);
                    }
                }
                return Ok(TierGate::Parked);
            }
            Disposition::Tier(t) => t,
        },
        None => av.tier,
    };
    let rationale: &str = grounded
        .as_ref()
        .map(|g| g.rationale.as_str())
        .unwrap_or(av.rationale.as_str());

    if effective_tier == Tier::N {
        if crate::issues::transitions::park(
            db.pool(),
            db.events(),
            &issue.key,
            Status::New,
            &ParkReason::Unscopeable,
            ParkedBy::Machine,
        )
        .await?
        {
            crate::issues::store::set_ranked_tier(
                db.pool(),
                &issue.key,
                Tier::N.as_str(),
                av.affinity.as_str(),
                hash,
            )
            .await?;
            db.ledger_append(None, "rank", api_cost).await?;
            if let Some(gc) = grounded_cost {
                db.ledger_append(None, "rank-grounded", gc).await?;
            }
            if let Some(m) = db.metrics() {
                m.record_verdict("N", "tier", av.confidence.as_str(), grounded.is_some());
            }
        }
        return Ok(TierGate::Parked);
    }

    if crate::issues::store::apply_rank_result(
        db.pool(),
        &issue.key,
        effective_tier.as_str(),
        av.affinity.as_str(),
        hash,
    )
    .await?
    {
        annotate(db, &issue.key, Status::New, rationale, Some(hash)).await?;
        db.ledger_append(None, "rank", api_cost).await?;
        if let Some(gc) = grounded_cost {
            db.ledger_append(None, "rank-grounded", gc).await?;
        }
        if let Some(m) = db.metrics() {
            m.record_verdict(
                effective_tier.as_str(),
                "tier",
                av.confidence.as_str(),
                grounded.is_some(),
            );
        }
        if !cfg.effective().allowed_tiers.contains(&effective_tier) {
            annotate(
                db,
                &issue.key,
                Status::New,
                &format!(
                    "tier-deferred: {} not in the allowed-tiers set, excluded from the \
                     autopilot's pick",
                    effective_tier.as_str()
                ),
                Some(hash),
            )
            .await?;
        }
    }
    Ok(tier_gate(Some(effective_tier.as_str()), cfg))
}

/// What [`confirm_grounded_prescope`] decided: the row cleared the gate (proceed to the scope
/// turn), the grounded verdict parked it (`N`-demoted or `stale`), or it must wait for a later
/// sweep (a `T3` grounded verdict, or the grounded call itself producing no usable verdict — the
/// same "no verdict, no spend" rule [`TierGate::Unranked`] enforces for the API tier).
pub(super) enum PrescopeGate {
    Proceed,
    Parked,
    Deferred,
}

/// The pre-scope grounded confirmation gate: once [`confirm_tier`] clears an issue toward a scope
/// turn, require a code-grounded confirmation before that turn actually spends — unless
/// `content_hash` (the same hash [`confirm_tier`] just ranked against) already has a grounded
/// verdict on record, so a re-sweep of unchanged content never re-spends. Off entirely
/// when `cfg.prescope_grounded` is false (an opt-out for GPU-free / quick-loop scenarios).
///
/// Reuses the same per-repo checkout + grounded turn the low-confidence escalation arm already
/// runs ([`run_grounded`]) — one code path, two call sites. A grounded confirmation whose tier is
/// in `cfg.effective().allowed_tiers` overwrites `issues.tier` and proceeds; `N` or `stale` parks with
/// the grounded rationale as the recorded evidence; a failed/errored/verdict-less grounded call —
/// and a [`Grounded::Queued`] one — defers, extending the "no verdict, no spend" rule this gate
/// exists to enforce. The `rank-grounded` cost ledgers here only for the local arm (the pod arm
/// booked at collection — the single-booking invariant in [`crate::runs::workpod`]).
pub(super) async fn confirm_grounded_prescope(
    db: &Db,
    cfg: &ControllerCfg,
    issue: &Issue,
    content_hash: &str,
) -> Result<PrescopeGate> {
    if !cfg.effective().prescope_grounded {
        return Ok(PrescopeGate::Proceed);
    }
    if issue.grounded_content_hash.as_deref() == Some(content_hash) {
        return Ok(PrescopeGate::Proceed);
    }

    let (g, cost_ledgered) = match run_grounded(db, cfg, issue).await? {
        Grounded::Verdict {
            verdict,
            cost_ledgered,
        } => (verdict, cost_ledgered),
        Grounded::Launched => {
            annotate(
                db,
                &issue.key,
                Status::New,
                "pre-scope grounded confirmation turn launched; scope deferred until it completes",
                None,
            )
            .await?;
            return Ok(PrescopeGate::Deferred);
        }
        Grounded::Queued => {
            annotate(
                db,
                &issue.key,
                Status::New,
                "pre-scope grounded confirmation queued behind the per-kind cap/budget, scope deferred until it runs",
                None,
            )
            .await?;
            return Ok(PrescopeGate::Deferred);
        }
        Grounded::Skipped => {
            annotate(
                db,
                &issue.key,
                Status::New,
                "pre-scope grounded confirmation produced no verdict, scope deferred until it does",
                None,
            )
            .await?;
            return Ok(PrescopeGate::Deferred);
        }
        // The boundary refused the launch: `refuse` ledgered it and parked the issue.
        Grounded::Refused => return Ok(PrescopeGate::Parked),
        // A concurrent collector owns the turn — defer this pass, the winner applies the verdict and
        // the next sweep re-reads the (by then stamped) grounded hash.
        Grounded::AlreadyCollected => return Ok(PrescopeGate::Deferred),
    };

    apply_grounded_disposition(db, cfg, issue, content_hash, g, cost_ledgered).await
}

/// Fold one grounded verdict into the issue by its disposition — the shared tail of the pre-scope
/// gate ([`confirm_grounded_prescope`]) and the adopt-first pre-pass ([`adopt_orphaned_turns`]):
/// `stale`/`N` park with the grounded rationale as evidence, a tier stamps `issues.tier` + the
/// grounded hash. `cost_ledgered` says the executor already booked the turn (the pod arm books at
/// collection); only the local arm ledgers here.
pub(super) async fn apply_grounded_disposition(
    db: &Db,
    cfg: &ControllerCfg,
    issue: &Issue,
    content_hash: &str,
    g: engine::GroundedVerdict,
    cost_ledgered: bool,
) -> Result<PrescopeGate> {
    let (park_reason, recorded_tier) = match g.disposition {
        Disposition::Stale => (
            Some(ParkReason::StaleAlreadyImplemented {
                rationale: g.rationale.clone(),
            }),
            None,
        ),
        Disposition::Tier(Tier::N) => (
            Some(ParkReason::DemotedToN {
                rationale: g.rationale.clone(),
            }),
            Some(Tier::N.as_str()),
        ),
        Disposition::Tier(t) => (None, Some(t.as_str())),
    };
    let won = match &park_reason {
        Some(reason) => {
            let parked = crate::issues::transitions::park(
                db.pool(),
                db.events(),
                &issue.key,
                Status::New,
                reason,
                ParkedBy::Machine,
            )
            .await?;
            if parked {
                crate::issues::store::apply_grounded_result(
                    db.pool(),
                    &issue.key,
                    recorded_tier,
                    content_hash,
                )
                .await?;
            }
            parked
        }
        None => {
            crate::issues::store::apply_grounded_result(
                db.pool(),
                &issue.key,
                recorded_tier,
                content_hash,
            )
            .await?
        }
    };
    if won {
        annotate(
            db,
            &issue.key,
            Status::New,
            &g.rationale,
            Some(content_hash),
        )
        .await?;
        if !cost_ledgered {
            db.ledger_append(None, "rank-grounded", g.cost_usd).await?;
        }
    }
    if park_reason.is_some() {
        return Ok(PrescopeGate::Parked);
    }
    Ok(match tier_gate(recorded_tier, cfg) {
        TierGate::Proceed => PrescopeGate::Proceed,
        _ => PrescopeGate::Deferred,
    })
}

/// What [`confirm_tier`] decided `reconcile_new` should do next: keep going (the row's tier is in
/// `cfg.effective().allowed_tiers`), stop because the ranker parked it (an `N` verdict), or stop
/// because its tier is excluded from the pick — either of the latter two means `confirm_tier`
/// already did everything the outcome needs.
pub(super) enum TierGate {
    Proceed,
    Parked,
    Deferred,
    /// No verdict exists for the current content (ranking failed, or the cap declined a
    /// never-ranked row). With no heuristic to fall back on, the ranker is the gate to the
    /// scope turn — an unranked issue defers (retried on the next sweep) rather than spending
    /// a scope turn on something that might have ranked N or an excluded tier.
    Unranked,
}

/// What to do with an issue now sitting at a confirmed `tier` (fresh from the ranker, or a cache
/// hit on an already-ranked row): a tier in `cfg.effective().allowed_tiers` proceeds to the scope
/// turn, everything else is deferred rather than spending a scope turn it can't use yet — default
/// `allowed_tiers = t0,t1` (T1 now has a real propose+refine+adversary pipeline; T2/T3 still don't
/// and stay excluded until a real rig backend lands, `allow_t3` union notwithstanding).
/// `tier == None` (no confirmed tier at all) always proceeds — that shouldn't happen by the time a
/// caller reaches this gate, but it's not this gate's job to invent a park for it.
pub(super) fn tier_gate(tier: Option<&str>, cfg: &ControllerCfg) -> TierGate {
    match tier.and_then(|t| Tier::parse(t).ok()) {
        Some(t) if !cfg.effective().allowed_tiers.contains(&t) => TierGate::Deferred,
        _ => TierGate::Proceed,
    }
}
