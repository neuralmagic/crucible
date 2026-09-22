use crate::client::Db;
use crate::config::ControllerCfg;
use crate::event_log::Event;
use crate::issues::model::Issue;
use crate::issues::model::split_issue_key;
use crate::issues::ranker::{self, RankOutcome};
use crate::issues::reconcile::grounded::{TierGate, apply_verdict, tier_gate};
use crate::issues::triage;
use anyhow::{Context, Result};

/// The first step of reconciling a `new` issue. Fetches the issue's current
/// title/body/labels (the reconcile function's one GET — [`engine::scope_propose`] does its own
/// separate fetch for the goal text, but this is the only fetch *this* step needs), hashes them
/// for the ranking cache, and — unless the hash already matches a confirmed rank — runs the one
/// bounded ranking call ([`ranker::rank`]) that is the *sole* source of this issue's tier (no
/// heuristic prior; see [`crate::issues::triage`]'s module doc for why one isn't there anymore).
///
/// An `N` verdict parks the issue (machine, "unscopeable per ranker"). A verdict outside
/// `cfg.effective().allowed_tiers` records the tier but defers rather than scoping (see
/// [`tier_gate`]), logging `tier-deferred` exactly once (guarded by [`crate::client::Db::apply_rank_result`]'s
/// compare-and-set, which only fires on the content hash actually changing — a cache-hit sweep
/// never re-logs it). A malformed verdict (after `ranker::rank`'s own bounded retry) is a ranking
/// *failure*: the tier stays whatever it was (`NULL` if this is the first attempt — there is no
/// heuristic to fall back to), the failure is logged, and the issue is never parked for it. But
/// with the ranker as the sole tier source, no verdict means no scope turn: a never-ranked row
/// defers ([`TierGate::Unranked`]) and is retried on a later sweep, while a row whose older
/// content did rank proceeds on that standing verdict.
pub(super) async fn confirm_tier(db: &Db, cfg: &ControllerCfg, issue: &Issue) -> Result<TierGate> {
    let (repo, number) = split_issue_key(&issue.key)?;
    let gh = triage::fetch_issue(&repo, number).await;
    if let Some(m) = db.metrics() {
        m.record_github(gh.is_ok());
    }
    let gh = gh.with_context(|| format!("fetching {} for tier ranking", issue.key))?;
    let body = gh.body.clone().unwrap_or_default();
    let hash = ranker::content_hash(&gh.title, &body, &gh.labels);

    if issue.ranked_content_hash.as_deref() == Some(hash.as_str()) {
        // Cache hit: content unchanged since the last confirmed rank — still route through the
        // tier gate, since a T3 row's exclusion must hold on every sweep, not just the first.
        return Ok(tier_gate(issue.tier.as_deref(), cfg));
    }

    let day = crate::clock::today_utc();
    if db
        .decline_if_over_ceiling(&day, cfg.effective().daily_cost_ceiling)
        .await?
    {
        // Declined: an already-ranked row proceeds on its standing (if stale) tier; a
        // never-ranked row has no verdict at all and must not reach the scope turn.
        return Ok(match issue.tier.as_deref() {
            Some(t) => tier_gate(Some(t), cfg),
            None => TierGate::Unranked,
        });
    }

    // In-process I/O (an HTTP call, not a subprocess), so no `spawn_blocking` — unlike the
    // engine's own actions ([`engine::scope_propose`] and friends), which stay subprocesses.
    let outcome = ranker::rank(&gh.title, &body, &gh.labels).await;

    match outcome {
        RankOutcome::Verdict(av) => apply_verdict(db, cfg, issue, &hash, av).await,
        RankOutcome::Failed(reason) => {
            db.events()
                .append(&Event::now(
                    &issue.key,
                    "new",
                    "new",
                    Some(&format!(
                        "ranking failed, tier stays NULL, scope deferred until ranked: {reason}"
                    )),
                    None,
                ))
                .await?;
            // A row that ranked on earlier content keeps that tier's verdict; a never-ranked
            // row has none, and no verdict means no scope turn.
            Ok(match issue.tier.as_deref() {
                Some(t) => tier_gate(Some(t), cfg),
                None => TierGate::Unranked,
            })
        }
    }
}
