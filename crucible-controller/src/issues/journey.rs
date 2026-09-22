//! The issue journey: a server-assembled, typed timeline of one issue's full story, served by
//! `GET /api/issues/{key}/journey`. Where [`crate::api::dto::issue_detail`] returns the raw provenance
//! graph (issue → scopes → runs → candidates + the event log), the journey folds those same rows
//! into an ordered sequence of [`JourneyStep`]s a client renders straight down the page — discovery,
//! ranking, grounding, scoping, the approval gate, each run, each kept PR, and the terminal.
//!
//! Everything is sourced from existing rows + the event log ([`crate::event_log`]) — no new tables.
//! A stage that never happened is simply absent (a brand-new issue yields one `discovered` step).
//! Steps are emitted in canonical lifecycle order, which is monotonic in time for the normal flow
//! (new → scoped → awaiting-approval → running → pr-open/done, with parked as a branch), so no
//! timestamp sort is needed; `at` is per-step provenance for display, best-effort per its source.

use crate::client::Db;
use crate::event_log::EventRecord;
use crate::issues::refine_trail::{RoundKind, RoundOutcome, extract_trail};
use crate::model::{ParkReason, Status};
use anyhow::Result;
use serde::Serialize;
use utoipa::ToSchema;

/// One issue's full timeline. `steps` is chronological; a new issue has exactly one (`discovered`).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct JourneyDto {
    steps: Vec<JourneyStep>,
}

/// One step in an issue's journey. A closed, `kind`-tagged enum: the client matches on `kind` and
/// reads that variant's fields. `at` is an RFC3339 UTC stamp when the step's source carried one,
/// else `None`.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JourneyStep {
    /// The issue entered the system (row created / first recorded event). Always the first step.
    Discovered { at: Option<String> },
    /// The ranker assigned a tier. `disposition` is `tier` normally, or `stale` when a grounded
    /// verdict later found the ask already implemented (also surfaced as the `stale` terminal).
    /// Ranker *confidence* is transient at reconcile time (metrics only, never persisted), so it is
    /// not carried here.
    Ranked {
        at: Option<String>,
        tier: Option<String>,
        disposition: String,
    },
    /// A code-grounded verdict was recorded for the issue (`issues.grounded_content_hash` is set).
    Grounded {
        at: Option<String>,
        disposition: String,
    },
    /// A scope pack was frozen. `refine_rounds` / `adversary` come from the pack's `SCOPE.md` refine
    /// trail (the same parser `GET /api/approvals/{scope_id}/evidence` uses); `adversary` is
    /// `passed` | `concerns` | `none`.
    Scoped {
        at: Option<String>,
        refine_rounds: usize,
        adversary: String,
    },
    /// A human opened the approval gate (`scopes.approved_by` / `approved_at`).
    Approval {
        at: Option<String>,
        approved_by: Option<String>,
    },
    /// One declared image build the approved pack blocked measurement on (the `building` reconcile
    /// state). Sits between the approval and the run: the run launches only once every build pins a
    /// digest. `digest` is the pinned `image@sha256:…` on success; `evidence` is the build-log
    /// pointer (a GH Actions run URL or a pod-log reference) on failure/timeout, rendered as the
    /// failure-evidence link. `live` is true while the build is still pending/dispatched.
    Build {
        at: Option<String>,
        name: String,
        backend: String,
        state: String,
        image: String,
        digest: Option<String>,
        evidence: Option<String>,
        /// Wall-clock build duration in seconds (`dispatched_at` → `finished_at`), null until both
        /// stamps exist.
        duration_secs: Option<i64>,
        live: bool,
    },
    /// One run the issue launched. `live` is true while the run is still `running`.
    Run {
        at: Option<String>,
        run_id: String,
        status: String,
        best_score: Option<f64>,
        live: bool,
    },
    /// One kept draft PR the run opened (the pr-ingestion data). `repo` is the issue's repo.
    Pr {
        at: Option<String>,
        url: String,
        repo: String,
    },
    /// Terminal: the issue was parked (machine or human), with the recorded reason.
    Parked {
        at: Option<String>,
        by: Option<String>,
        reason: Option<String>,
    },
    /// Terminal: a finished run that kept nothing (no PR opened).
    Done { at: Option<String> },
    /// Terminal: the grounded ranker found the ask already implemented in the checkout.
    Stale {
        at: Option<String>,
        evidence: Option<String>,
    },
}

/// The first event whose `to` status matches `to` (oldest such transition — the first time the
/// issue entered that state), for a step's `at`.
fn first_at_to(events: &[EventRecord], to: &str) -> Option<String> {
    events.iter().find(|e| e.to == to).map(|e| e.ts.clone())
}

/// Whole seconds between two RFC3339 stamps (`from` → `to`), or `None` when either is absent or
/// unparseable — the build's wall-clock duration for the journey `build` step.
fn duration_secs(from: Option<&str>, to: Option<&str>) -> Option<i64> {
    let start: jiff::Timestamp = from?.parse().ok()?;
    let end: jiff::Timestamp = to?.parse().ok()?;
    Some((end.as_second() - start.as_second()).max(0))
}

/// The `passed` | `concerns` | `none` summary of a scope's adversary round: the last adversary round
/// in the trail decides (passed → `passed`, anything else → `concerns`); no adversary round → `none`.
fn adversary_summary(rounds: &[crate::issues::refine_trail::RoundRecord]) -> &'static str {
    match rounds.iter().rev().find(|r| r.kind == RoundKind::Adversary) {
        Some(r) => match r.outcome {
            RoundOutcome::Passed => "passed",
            _ => "concerns",
        },
        None => "none",
    }
}

/// Assemble the full journey for one issue, or `None` when the issue is untracked (the handler
/// 404s). The `scoped` step's refine trail comes off the stored pack tarball's `SCOPE.md` (the
/// same read the approval-evidence endpoint makes).
pub(crate) async fn assemble_journey(db: &Db, key: &str) -> Result<Option<JourneyDto>> {
    let Some(issue) = crate::issues::store::get_issue(db.pool(), key).await? else {
        return Ok(None);
    };
    let events = db.events().read_for_key(key).await?;
    let scopes = crate::issues::store::list_scopes_for_issue(db.pool(), key).await?;

    let is_stale = issue.status == Status::Parked
        && issue.park_reason().is_some_and(|r| {
            matches!(
                r,
                ParkReason::StaleRankHorizon { .. } | ParkReason::StaleAlreadyImplemented { .. }
            )
        });

    let mut steps = Vec::new();

    // 1. Discovered — always. The first recorded event is the closest proxy for "arrived" (issues
    //    carry no created_at column); a never-transitioned issue has no event, so `at` is None.
    steps.push(JourneyStep::Discovered {
        at: events.first().map(|e| e.ts.clone()),
    });

    // 2. Ranked — the row carries the tier; disposition distinguishes a normal tier verdict from a
    //    stale grounded supersession. A stale-parked issue may have no tier (stale never writes the
    //    tier column), so this step is present when either a tier was recorded or the issue is stale.
    if issue.tier.is_some() || is_stale {
        steps.push(JourneyStep::Ranked {
            // Both the text-rank and grounded verdicts log a `new → new` event; the first is the
            // closest stamp for "ranked".
            at: events
                .iter()
                .find(|e| e.from == "new" && e.to == "new")
                .map(|e| e.ts.clone()),
            tier: issue.tier.clone(),
            disposition: if is_stale { "stale" } else { "tier" }.to_string(),
        });
    }

    // 3. Grounded — a code-grounded verdict is on record for the issue.
    if issue.grounded_content_hash.is_some() {
        steps.push(JourneyStep::Grounded {
            at: events
                .iter()
                .rfind(|e| e.from == "new" && e.to == "new")
                .map(|e| e.ts.clone()),
            disposition: if is_stale { "stale" } else { "tier" }.to_string(),
        });
    }

    // 4. Scoped — a pack was frozen. The stored tarball is keyed by issue, so a re-scoped issue
    //    still has one current `SCOPE.md`; emit a single scoped step reflecting it.
    if !scopes.is_empty() {
        let rounds = match crate::playbooks::packs::read_pack_file(db.pool(), key, "SCOPE.md").await
        {
            Ok(Some(md)) => extract_trail(&md),
            Ok(None) | Err(_) => Vec::new(),
        };
        steps.push(JourneyStep::Scoped {
            at: first_at_to(&events, "scoped"),
            refine_rounds: rounds.len(),
            adversary: adversary_summary(&rounds).to_string(),
        });
    }

    // 5. Approval — a human recorded an approval on a scope (the approved_at approval signal).
    for scope in &scopes {
        if scope.approved_at.is_some() {
            steps.push(JourneyStep::Approval {
                at: scope.approved_at.clone(),
                approved_by: scope.approved_by.clone(),
            });
        }
    }

    // 6. Builds — the `building` reconcile state. An approved pack that declares `[build.<name>]`
    //    blocks its run until every build pins a digest, so builds sit between the
    //    approval and the runs. Emitted oldest-first (the DB order), each carrying its terminal outcome:
    //    the pinned digest on success or the build-log evidence pointer on failure/timeout.
    for b in crate::builds::store::builds_for_issue(db.pool(), key).await? {
        let live = !b.state.is_terminal();
        steps.push(JourneyStep::Build {
            at: Some(b.created_at.clone()),
            name: b.name,
            backend: b.backend.as_str().to_string(),
            state: b.state.as_str().to_string(),
            image: b.image,
            digest: b.digest_ref,
            evidence: b.evidence_url,
            duration_secs: duration_secs(b.dispatched_at.as_deref(), b.finished_at.as_deref()),
            live,
        });
    }

    // 7. Runs + their kept PRs. Gather every run across the issue's scopes, ordered by run_id (the
    //    time-first stamp makes lexical order chronological), and emit each run followed by the
    //    distinct kept-candidate PRs it opened.
    let mut runs = Vec::new();
    for scope in &scopes {
        for run in crate::runs::store::list_runs_for_scope(db.pool(), scope.id).await? {
            runs.push(run);
        }
    }
    runs.sort_by(|a, b| a.run_id.cmp(&b.run_id));
    for run in &runs {
        let live = run.status == "running";
        steps.push(JourneyStep::Run {
            at: crate::runs::model::run_id_created(&run.run_id),
            run_id: run.run_id.clone(),
            status: run.status.clone(),
            best_score: run.best_score,
            live,
        });
        let mut seen = std::collections::HashSet::new();
        for cand in crate::runs::store::list_candidates_for_run(db.pool(), &run.run_id).await? {
            if cand.decision.as_deref() != Some("keep") {
                continue;
            }
            if let Some(url) = cand.pr_url
                && seen.insert(url.clone())
            {
                steps.push(JourneyStep::Pr {
                    at: crate::runs::model::run_id_created(&run.run_id),
                    url,
                    repo: issue.repo.clone(),
                });
            }
        }
    }

    // 8. Terminal. `pr-open` is a resting state (the Pr steps already stand for it), so only the
    //    genuinely terminal states add a step. A stale-park reads as `stale`, any other park as
    //    `parked`.
    match issue.status {
        Status::Parked if is_stale => steps.push(JourneyStep::Stale {
            at: first_at_to(&events, "parked"),
            evidence: issue.parked_reason.clone(),
        }),
        Status::Parked => steps.push(JourneyStep::Parked {
            at: first_at_to(&events, "parked"),
            by: issue.parked_by.map(|p| p.as_str().to_string()),
            reason: issue.parked_reason.clone(),
        }),
        Status::Done => steps.push(JourneyStep::Done {
            at: first_at_to(&events, "done"),
        }),
        _ => {}
    }

    Ok(Some(JourneyDto { steps }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Db;
    use crate::event_log::Event;
    use crate::issues::model::{NewIssue, NewScope};
    use crate::model::{ParkedBy, Status};
    use crate::runs::model::{NewCandidate, NewRun};
    use sqlx::PgPool;

    fn new_issue(key: &str) -> NewIssue {
        NewIssue {
            key: key.into(),
            repo: key.split('#').next().unwrap_or(key).into(),
            priority: 0,
            evidence_url: None,
            title: None,
            author: None,
            body: None,
            labels: Vec::new(),
            upstream_updated_at: None,
        }
    }

    /// Store a pack tarball whose SCOPE.md carries a two-round refine trail (propose + a passing
    /// adversary round).
    async fn store_scope_md(db: &Db, key: &str) {
        let dir = tempfile::tempdir().expect("tempdir");
        let md = r#"# SCOPE.md

**Round trail:**

```json
[
  {"round":1,"kind":"propose","judge_block":"","cost":0.0,"outcome":{"result":"passed"}},
  {"round":2,"kind":"adversary","judge_block":"","cost":0.1,"outcome":{"result":"passed"}}
]
```
"#;
        std::fs::write(dir.path().join("SCOPE.md"), md).expect("write SCOPE.md");
        crate::playbooks::packs::store_pack_tree(db.pool(), key, dir.path())
            .await
            .expect("store pack");
    }

    /// The kinds of a step sequence, for order assertions.
    fn kinds(j: &JourneyDto) -> Vec<&'static str> {
        j.steps
            .iter()
            .map(|s| match s {
                JourneyStep::Discovered { .. } => "discovered",
                JourneyStep::Ranked { .. } => "ranked",
                JourneyStep::Grounded { .. } => "grounded",
                JourneyStep::Scoped { .. } => "scoped",
                JourneyStep::Approval { .. } => "approval",
                JourneyStep::Build { .. } => "build",
                JourneyStep::Run { .. } => "run",
                JourneyStep::Pr { .. } => "pr",
                JourneyStep::Parked { .. } => "parked",
                JourneyStep::Done { .. } => "done",
                JourneyStep::Stale { .. } => "stale",
            })
            .collect()
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn full_story_yields_the_expected_step_sequence(pool: PgPool) -> Result<()> {
        let db = Db::new(pool);
        let key = "o/r#1";
        crate::issues::store::upsert_issue(db.pool(), &new_issue(key)).await?;

        // Ranked (tier on the row + the new→new rationale event) and grounded (grounded hash).
        db.events()
            .append(&Event::now(
                key,
                "new",
                "new",
                Some("ranked T1"),
                Some("h1"),
            ))
            .await?;
        crate::issues::store::set_ranked_tier(db.pool(), key, "T1", "perf", "h1").await?;
        crate::issues::store::apply_grounded_result(db.pool(), key, Some("T1"), "h1").await?;

        // Scoped: a scope row + its SCOPE.md refine trail, and the new→scoped transition.
        assert!(
            crate::issues::store::claim_issue(db.pool(), key, Status::New, Status::Scoped).await?
        );
        db.events()
            .append(&Event::now(key, "new", "scoped", Some("pack passed"), None))
            .await?;
        let scope_id = crate::issues::store::insert_scope(
            db.pool(),
            &NewScope {
                issue: key.into(),
                pack_digest: Some("v1:abc".into()),
                check_outcome: Some("PASS".into()),
            },
        )
        .await?;
        store_scope_md(&db, key).await;

        // Approval: a human approval.
        assert!(
            crate::issues::store::record_approval(
                db.pool(),
                scope_id,
                "wren",
                "2026-07-03T10:00:00Z"
            )
            .await?
        );

        // Run + a kept PR, then rest at pr-open.
        crate::runs::store::insert_run(
            db.pool(),
            &NewRun {
                run_id: "20260703T120000Z-x".into(),
                scope: Some(scope_id),
                issue: None,
                identity_digest: None,
                status: "finished".into(),
                pod: None,
                session_uri: None,
                best_score: Some(240.0),
                cost_usd: Some(1.0),
            },
        )
        .await?;
        crate::runs::store::insert_candidate(
            db.pool(),
            &NewCandidate {
                run_id: "20260703T120000Z-x".into(),
                kind: Some("deep".into()),
                lane: None,
                iter: Some(1),
                score: Some(240.0),
                decision: Some("keep".into()),
                worktree: None,
                sandbox: None,
                pr_url: Some("https://github.com/o/r/pull/9".into()),
                branch: Some("autoresearch/20260703T120000Z-x/0".into()),
            },
        )
        .await?;

        let j = assemble_journey(&db, key).await?.expect("issue exists");
        assert_eq!(
            kinds(&j),
            vec![
                "discovered",
                "ranked",
                "grounded",
                "scoped",
                "approval",
                "run",
                "pr"
            ]
        );

        // Spot-check the payloads that come from the different sources.
        match &j.steps[1] {
            JourneyStep::Ranked {
                tier, disposition, ..
            } => {
                assert_eq!(tier.as_deref(), Some("T1"));
                assert_eq!(disposition, "tier");
            }
            other => panic!("step 1 should be ranked: {other:?}"),
        }
        match &j.steps[3] {
            JourneyStep::Scoped {
                refine_rounds,
                adversary,
                ..
            } => {
                assert_eq!(*refine_rounds, 2);
                assert_eq!(adversary, "passed");
            }
            other => panic!("step 3 should be scoped: {other:?}"),
        }
        match &j.steps[4] {
            JourneyStep::Approval {
                approved_by, at, ..
            } => {
                assert_eq!(approved_by.as_deref(), Some("wren"));
                assert_eq!(at.as_deref(), Some("2026-07-03T10:00:00Z"));
            }
            other => panic!("step 4 should be approval: {other:?}"),
        }
        match &j.steps[5] {
            JourneyStep::Run { run_id, live, .. } => {
                assert_eq!(run_id, "20260703T120000Z-x");
                assert!(!live, "a finished run is not live");
            }
            other => panic!("step 5 should be run: {other:?}"),
        }
        match &j.steps[6] {
            JourneyStep::Pr { url, repo, .. } => {
                assert_eq!(url, "https://github.com/o/r/pull/9");
                assert_eq!(repo, "o/r");
            }
            other => panic!("step 6 should be pr: {other:?}"),
        }
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_declared_build_appears_between_approval_and_run(pool: PgPool) -> Result<()> {
        use crate::builds::model::{BuildBackendKind, BuildState, NewBuild};
        let db = Db::new(pool);
        let key = "o/r#7";
        crate::issues::store::upsert_issue(db.pool(), &new_issue(key)).await?;
        assert!(
            crate::issues::store::claim_issue(db.pool(), key, Status::New, Status::Scoped).await?
        );
        let scope_id = crate::issues::store::insert_scope(
            db.pool(),
            &NewScope {
                issue: key.into(),
                pack_digest: Some("v1:abc".into()),
                check_outcome: Some("PASS".into()),
            },
        )
        .await?;
        assert!(
            crate::issues::store::record_approval(
                db.pool(),
                scope_id,
                "wren",
                "2026-07-03T10:00:00Z"
            )
            .await?
        );
        // A build that succeeded (pins a digest) — the run then launches on it.
        let build_id = crate::builds::store::insert_build(
            db.pool(),
            &NewBuild {
                scope: Some(scope_id),
                name: "loop".into(),
                image: "quay.io/x/loop".into(),
                tag: "e2e".into(),
                context_digest: "ctx".into(),
                backend: BuildBackendKind::Cluster,
                timeout_secs: 1800,
            },
        )
        .await?;
        crate::builds::store::set_build_succeeded(
            db.pool(),
            build_id,
            "quay.io/x/loop@sha256:dead",
        )
        .await?;
        crate::runs::store::insert_run(
            db.pool(),
            &NewRun {
                run_id: "20260703T120000Z-x".into(),
                scope: Some(scope_id),
                issue: None,
                identity_digest: None,
                status: "finished".into(),
                pod: None,
                session_uri: None,
                best_score: Some(240.0),
                cost_usd: Some(1.0),
            },
        )
        .await?;

        let j = assemble_journey(&db, key).await?.expect("issue exists");
        assert_eq!(
            kinds(&j),
            vec!["discovered", "scoped", "approval", "build", "run"]
        );
        match &j.steps[3] {
            JourneyStep::Build {
                name,
                backend,
                state,
                image,
                digest,
                evidence,
                live,
                ..
            } => {
                assert_eq!(name, "loop");
                assert_eq!(backend, "cluster");
                assert_eq!(state, "succeeded");
                assert_eq!(image, "quay.io/x/loop");
                assert_eq!(digest.as_deref(), Some("quay.io/x/loop@sha256:dead"));
                assert!(evidence.is_none(), "a succeeded build has no evidence link");
                assert!(!live, "a succeeded build is not live");
            }
            other => panic!("step 3 should be build: {other:?}"),
        }

        // A failed build surfaces its evidence pointer and no digest.
        let fail_id = crate::builds::store::insert_build(
            db.pool(),
            &NewBuild {
                scope: Some(scope_id),
                name: "web".into(),
                image: "ghcr.io/x/web".into(),
                tag: "e2e".into(),
                context_digest: "ctx2".into(),
                backend: BuildBackendKind::GithubActions,
                timeout_secs: 1800,
            },
        )
        .await?;
        crate::builds::store::set_build_failed(
            db.pool(),
            fail_id,
            BuildState::Failed,
            Some("https://github.com/o/r/actions/runs/9"),
        )
        .await?;
        let j = assemble_journey(&db, key).await?.expect("issue exists");
        let web = j
            .steps
            .iter()
            .find_map(|s| match s {
                JourneyStep::Build {
                    name,
                    evidence,
                    digest,
                    ..
                } if name == "web" => Some((evidence.clone(), digest.clone())),
                _ => None,
            })
            .expect("web build step");
        assert_eq!(
            web.0.as_deref(),
            Some("https://github.com/o/r/actions/runs/9")
        );
        assert!(web.1.is_none(), "a failed build pins no digest");
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_brand_new_issue_has_exactly_one_step(pool: PgPool) -> Result<()> {
        let db = Db::new(pool);
        crate::issues::store::upsert_issue(db.pool(), &new_issue("o/r#2")).await?;
        let j = assemble_journey(&db, "o/r#2").await?.expect("issue exists");
        assert_eq!(kinds(&j), vec!["discovered"]);
        assert!(
            matches!(&j.steps[0], JourneyStep::Discovered { at } if at.is_none()),
            "a never-transitioned issue has no timestamp"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_stale_park_ends_in_a_stale_terminal(pool: PgPool) -> Result<()> {
        let db = Db::new(pool);
        let key = "o/r#3";
        crate::issues::store::upsert_issue(db.pool(), &new_issue(key)).await?;
        assert!(
            crate::issues::transitions::park(
                db.pool(),
                db.events(),
                key,
                Status::New,
                &ParkReason::StaleAlreadyImplemented {
                    rationale: String::new(),
                },
                ParkedBy::Machine,
            )
            .await?
        );
        let j = assemble_journey(&db, key).await?.expect("issue exists");
        // No tier was written, but the stale disposition still surfaces a ranked step + the terminal.
        assert_eq!(kinds(&j), vec!["discovered", "ranked", "stale"]);
        match j.steps.last().expect("a terminal") {
            JourneyStep::Stale { evidence, .. } => {
                assert!(evidence.as_deref().unwrap_or("").starts_with("stale"));
            }
            other => panic!("last step should be stale: {other:?}"),
        }
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_done_issue_ends_in_a_done_terminal(pool: PgPool) -> Result<()> {
        let db = Db::new(pool);
        let key = "o/r#4";
        crate::issues::store::upsert_issue(db.pool(), &new_issue(key)).await?;
        assert!(
            crate::issues::store::claim_issue(db.pool(), key, Status::New, Status::Running).await?
        );
        crate::issues::transitions::transition(
            db.pool(),
            db.events(),
            key,
            Status::Running,
            Status::Done,
            Some("run finished; outcome ingested"),
            None,
        )
        .await?;
        let j = assemble_journey(&db, key).await?.expect("issue exists");
        assert_eq!(*kinds(&j).last().expect("a terminal"), "done");
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_unknown_issue_is_none(pool: PgPool) -> Result<()> {
        let db = Db::new(pool);
        assert!(assemble_journey(&db, "o/r#404").await?.is_none());
        Ok(())
    }
}
