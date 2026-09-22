//! Pull ingest: fold a finished run's session log into `runs` + per-candidate `candidates` + a `run`
//! ledger row (the controller-pull model — runs never touch the DB; the controller reads the log they
//! wrote). This makes the controller the **second consumer** of the session-log wire format
//! (`crucible::session`), so it re-declares only the fields it folds, as a plain serde mirror
//! decoupled from the engine's own type — a version-discipline cost worth paying to keep runs off the
//! DB. Per-candidate granularity is the point: one `candidates` row per wide-round lane
//! (`phase = "wide"`) and per deep-loop iteration (the source of the provenance graph and wide-round
//! resume). The one exception is the plan task node, which is stored verbatim rather than folded
//! (see [`Ev::PlanAdmitted`]) and so uses the contract crate's own type.
//!
//! The run's work graph lands alongside: `run_plans` + `run_task_results` (migration 0027), which
//! `GET /api/runs/{run_id}/graph` renders.

#![allow(clippy::disallowed_macros)]

use crate::client::Db;
use crate::runs::model::{NewCandidate, NewRun, TaskResult};
use anyhow::{Context, Result};
use crucible::plan::exec::TaskStatus;
use crucible_contract::session::PlanTaskWire;
use crucible_contract::{BlockedReasonKind, TaskBlocked};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// The subset of a `RowWire` ingest reads. Unlisted fields (diff, note, detail, total) are ignored;
/// serde skips them since we don't `deny_unknown_fields`.
#[derive(Debug, Clone, Deserialize)]
struct RowWire {
    #[serde(default)]
    iter: u32,
    #[serde(default)]
    decision: String,
    #[serde(default)]
    score: Option<f64>,
    /// `Some("wide")` for a wide-round lane row; `None` for the deep loop (mirrors
    /// `crucible::reporter::Row::phase`).
    #[serde(default)]
    phase: Option<String>,
}

/// Only the run's comparability digest is folded from the identity event.
#[derive(Debug, Clone, Deserialize)]
struct IdentityWire {
    #[serde(default)]
    digest: String,
}

/// The subset of a `PrLinkWire` ingest reads (mirrors `crucible::session::PrLinkWire`, the same
/// decoupling `RowWire` above uses). One opened draft PR: its url + repo, and the component name for
/// a composite run (empty for single-repo).
#[derive(Debug, Clone, Deserialize)]
struct PrLinkWire {
    #[serde(default)]
    url: String,
    #[serde(default)]
    repo: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    branch: String,
}

/// The session-log envelope's `kind`-tagged body, narrowed to the events ingest folds. The `v`
/// envelope field and every other event ride along and are ignored ([`Ev::Other`]).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Ev {
    Row {
        row: RowWire,
    },
    Summary {
        #[serde(default)]
        best_score: Option<f64>,
    },
    Budget {
        #[serde(default)]
        spent: f64,
        /// Wall-clock seconds elapsed at this budget checkpoint; the last one seen is the run's
        /// duration (folded into [`ParsedRun::elapsed_secs`] for the run-duration metric).
        #[serde(default)]
        elapsed_secs: Option<f64>,
    },
    Identity {
        identity: IdentityWire,
    },
    PrLinks {
        #[serde(default)]
        links: Vec<PrLinkWire>,
    },
    /// A work graph admitted for execution. Unlike the mirrors above this borrows the contract's
    /// own [`PlanTaskWire`]: the task node is re-serialized verbatim into `run_plans.graph_json`,
    /// so a local copy would be a second definition of a type we'd have to keep byte-identical.
    PlanAdmitted {
        #[serde(default)]
        plan_version: u32,
        #[serde(default)]
        tasks: Vec<PlanTaskWire>,
    },
    TaskResult {
        #[serde(default)]
        task: String,
        #[serde(default)]
        status: String,
        #[serde(default)]
        iter: u32,
        #[serde(default)]
        note: String,
        #[serde(default)]
        cost_usd: f64,
        #[serde(default)]
        secs: f64,
        /// What the task emitted, verbatim. Folded into [`ParsedRun::result`] rather than into the
        /// `task_results` row: it is the run's product, and a schedule's cursor reads a field of
        /// it after a successful run.
        #[serde(default)]
        output: Option<serde_json::Value>,
        #[serde(default)]
        blocked: Option<TaskBlocked>,
    },
    Shutdown {
        #[serde(default)]
        outcome: String,
        #[serde(default)]
        reason: String,
    },
    #[serde(other)]
    Other,
}

/// One folded candidate: a wide-round lane or a deep-loop iteration.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCandidate {
    /// `wide` for a wide-round lane, `deep` for a deep-loop iteration.
    pub(crate) kind: String,
    /// The lane index (wide rows carry it in `iter`); `None` for deep rows.
    lane: Option<i64>,
    /// The deep-loop iteration; `None` for wide rows.
    iter: Option<i64>,
    score: Option<f64>,
    decision: String,
}

/// One draft PR a run opened (the controller-local mirror of the folded `PrLinks` event). Surfaced
/// on [`ParsedRun`] so [`crate::runs::completion::complete_run`] can land `pr-open` vs `done`, and so the
/// per-candidate `pr_url` fold has the links to work from.
#[derive(Debug, Clone, PartialEq)]
pub struct PrLink {
    pub(crate) url: String,
    pub(crate) repo: String,
    /// Component name for a composite run; empty for a single-repo run.
    pub(crate) name: String,
    /// The per-candidate head branch the PR was opened from (`autoresearch/<run_id>/<candidate>`).
    pub(crate) branch: String,
}

/// One admitted work graph: its version and its task array serialized back to JSON, which is what
/// `run_plans.graph_json` stores.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedPlan {
    pub(crate) plan_version: i64,
    pub(crate) graph_json: String,
}

/// A run's session log, folded into the rows ingest will write.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedRun {
    identity_digest: Option<String>,
    pub(crate) best_score: Option<f64>,
    cost_usd: Option<f64>,
    /// Wall-clock seconds the run took (the last budget checkpoint's `elapsed_secs`), or `None` if
    /// the log carried no timed budget line.
    pub(crate) elapsed_secs: Option<f64>,
    /// The run's exit outcome from its `Shutdown` line (`finished`/`solved`/`stopped`/…), or
    /// `None` if the log has no shutdown (a run that died mid-flight — the controller sees an
    /// incomplete log and records it as such).
    pub(crate) outcome: Option<String>,
    /// The shutdown line's free-text reason (the engine's own words for WHY it exited) — the park
    /// reason when an errored run is folded back into the pipeline.
    pub(crate) outcome_reason: Option<String>,
    pub(crate) candidates: Vec<ParsedCandidate>,
    /// Every admitted plan version, in emission order (a replan appends). Re-admissions of the
    /// same version merge into one entry by task-name union, first declaration wins.
    pub(crate) plans: Vec<ParsedPlan>,
    /// Every task attempt, in emission order. A retried `(iter, task)` appears more than once;
    /// the upsert that writes them makes the last one win.
    pub(crate) task_results: Vec<TaskResult>,
    /// The draft PR(s) the run opened (from the `PrLinks` event). Empty when nothing was kept, no PR
    /// repo was configured, or the open failed — the same conditions the engine emits nothing under.
    pub(crate) pr_links: Vec<PrLink>,
    /// The run's result: each passing task's `output`, keyed by task name, last attempt winning.
    /// A schedule's cursor addresses a field of this (`$.scan.newest_created_at`); nothing else
    /// reads it, and a run whose tasks emit no output leaves it empty.
    pub(crate) result: serde_json::Map<String, serde_json::Value>,
}

/// Why a persisted session is not positive terminal workload evidence.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum TerminalSessionError {
    #[error("session has no shutdown event")]
    MissingShutdown,
    #[error("session shutdown event is invalid: {0}")]
    InvalidShutdown(String),
}

/// Require a decodable, non-empty shutdown outcome before reconciliation may finalize a run.
pub(crate) fn validate_terminal_session(content: &str) -> Result<(), TerminalSessionError> {
    let mut terminal = None;
    for line in content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("kind").and_then(serde_json::Value::as_str) != Some("shutdown") {
            continue;
        }
        terminal = Some(match crucible_contract::json::from_str::<Ev>(line) {
            Ok(Ev::Shutdown { outcome, .. }) if !outcome.trim().is_empty() => Ok(()),
            Ok(Ev::Shutdown { .. }) => Err(TerminalSessionError::InvalidShutdown(
                "outcome is empty".to_string(),
            )),
            Err(e) => Err(TerminalSessionError::InvalidShutdown(e.to_string())),
            Ok(_) => Err(TerminalSessionError::InvalidShutdown(
                "not a shutdown event".to_string(),
            )),
        });
    }
    terminal.unwrap_or(Err(TerminalSessionError::MissingShutdown))
}

impl ParsedRun {
    /// Reject a file that yielded not one recognized event. [`parse_session`] skips lines it can't
    /// read, so an HTML error page from a bad fetch, a truncated download, or the wrong file
    /// entirely folds into an all-default `ParsedRun` and would otherwise be written as a real run
    /// reading `incomplete` with nothing in it. A genuine log always carries at least one of these
    /// — even a run that died in its first seconds emits its identity.
    fn ensure_session_log(&self, session_uri: &str) -> Result<()> {
        let recognized = !self.candidates.is_empty()
            || !self.plans.is_empty()
            || !self.task_results.is_empty()
            || !self.pr_links.is_empty()
            || self.identity_digest.is_some()
            || self.cost_usd.is_some()
            || self.best_score.is_some()
            || self.outcome.is_some();
        anyhow::ensure!(
            recognized,
            "{session_uri} yielded no session events; expected NDJSON lines with a \
             kind of identity/row/budget/summary/plan_admitted/task_result/shutdown"
        );
        Ok(())
    }

    /// The note of the first task attempt that failed outright — what an errored run's park reason
    /// carries as its diagnosis. `skipped`/`blocked` attempts are downstream of that failure rather
    /// than the cause of it, so they are passed over; a status the engine does not know reads as a
    /// failure, matching [`crate::runs::task_evidence`]. `None` when no failing attempt left a note.
    pub(crate) fn failure_cause(&self) -> Option<String> {
        let failed = self
            .task_results
            .iter()
            .filter(|r| {
                matches!(
                    r.status.parse().unwrap_or(TaskStatus::Fail),
                    TaskStatus::Fail | TaskStatus::Transport | TaskStatus::Truncated
                )
            })
            .map(|r| r.note.trim())
            .find(|note| !note.is_empty())
            .map(str::to_owned);
        failed.or_else(|| {
            self.task_results.iter().find_map(|r| {
                r.blocked
                    .as_ref()
                    .map(|b| format!("{} {}", r.task, blocked_cause(b, &r.note)))
            })
        })
    }

    /// Task attempts whose final status was `transport`, one per `(iter, task)`: the run lost them
    /// to infrastructure, not to a verdict.
    pub(crate) fn transport_losses(&self) -> usize {
        let mut last: BTreeMap<(i64, &str), TaskStatus> = BTreeMap::new();
        for r in &self.task_results {
            last.insert(
                (r.iter, r.task.as_str()),
                r.status.parse().unwrap_or(TaskStatus::Fail),
            );
        }
        last.values()
            .filter(|s| **s == TaskStatus::Transport)
            .count()
    }
}

/// The sentence a typed block reads as. The staging reason lives only in the note.
fn blocked_cause(blocked: &TaskBlocked, note: &str) -> String {
    let why = match (blocked.reason, blocked.task.as_deref()) {
        (BlockedReasonKind::RequiredTaskFailed, Some(task)) => {
            format!("required task {task} failed")
        }
        (BlockedReasonKind::RequiredTaskFailed, None) => "a required task failed".to_string(),
        (BlockedReasonKind::BudgetCeiling, _) => "budget ceiling reached".to_string(),
        (BlockedReasonKind::WallClockCeiling, _) => "wall-clock ceiling reached".to_string(),
        (BlockedReasonKind::DependencyDidNotPass, _) => "dependency did not pass".to_string(),
        (BlockedReasonKind::StagingRefused, _) => format!("staging refused: {}", note.trim()),
    };
    format!("blocked: {why}")
}

/// Whether `task` is one instance of a fan-out node that also settled under its bare name.
///
/// A mapped node publishes one `task_result` per instance (`node[key]`) plus a rollup under the
/// bare node name whose `cost_usd` is already the sum of its instances, and nothing on the wire
/// marks either line. A declared task name may not contain a bracket, so `node[` … `]` names an
/// instance and nothing else; requiring the rollup to be present keeps the instances counted if a
/// run ever ends before its node settles.
fn is_folded_instance(task: &str, results: &[TaskResult]) -> bool {
    let Some((node, rest)) = task.split_once('[') else {
        return false;
    };
    !node.is_empty() && rest.ends_with(']') && results.iter().any(|r| r.task == node)
}

/// Fold a session log's NDJSON into a [`ParsedRun`]. Torn/blank/foreign lines are skipped (a
/// tailing reader's discipline), so a partially-written log still yields whatever completed.
fn parse_session(content: &str) -> ParsedRun {
    let mut run = ParsedRun::default();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(ev) = crucible_contract::json::from_str::<Ev>(line) else {
            continue;
        };
        match ev {
            Ev::Row { row } => {
                let is_wide = row.phase.as_deref() == Some("wide");
                run.candidates.push(ParsedCandidate {
                    kind: if is_wide { "wide" } else { "deep" }.to_string(),
                    lane: is_wide.then_some(row.iter as i64),
                    iter: (!is_wide).then_some(row.iter as i64),
                    score: row.score.filter(|s| s.is_finite()),
                    decision: row.decision,
                });
            }
            Ev::Summary { best_score } => {
                run.best_score = best_score.filter(|s| s.is_finite());
            }
            Ev::Budget {
                spent,
                elapsed_secs,
            } => {
                run.cost_usd = Some(spent);
                if let Some(e) = elapsed_secs.filter(|e| e.is_finite() && *e >= 0.0) {
                    run.elapsed_secs = Some(e);
                }
            }
            Ev::Identity { identity } => {
                if !identity.digest.is_empty() {
                    run.identity_digest = Some(identity.digest);
                }
            }
            Ev::PrLinks { links } => {
                run.pr_links = links
                    .into_iter()
                    .filter(|l| !l.url.is_empty())
                    .map(|l| PrLink {
                        url: l.url,
                        repo: l.repo,
                        name: l.name,
                        branch: l.branch,
                    })
                    .collect();
            }
            Ev::PlanAdmitted {
                plan_version,
                tasks,
            } => {
                let plan_version: i64 = plan_version.into();
                // The engine re-admits the same version every iteration, and the post-run
                // epilogue admits a one-task stub under it too; the (run_id, plan_version)
                // upsert is last-write-wins, so folding them naively would leave the stub as
                // the whole graph. Merge same-version events by task-name union instead:
                // first declaration of a name wins, later events append only unseen names.
                // A new version still gets its own row (replan semantics).
                if let Some(existing) = run
                    .plans
                    .iter_mut()
                    .find(|p| p.plan_version == plan_version)
                {
                    let Ok(mut merged) =
                        serde_json::from_str::<Vec<PlanTaskWire>>(&existing.graph_json)
                    else {
                        continue;
                    };
                    for t in tasks {
                        if !merged.iter().any(|m| m.name == t.name) {
                            merged.push(t);
                        }
                    }
                    let Ok(graph_json) = serde_json::to_string(&merged) else {
                        continue;
                    };
                    existing.graph_json = graph_json;
                } else {
                    // Re-serializing a value that just deserialized cleanly cannot fail, but the
                    // controller doesn't panic on a run's data: a plan that somehow won't
                    // round-trip is dropped like any other unreadable line.
                    let Ok(graph_json) = serde_json::to_string(&tasks) else {
                        continue;
                    };
                    run.plans.push(ParsedPlan {
                        plan_version,
                        graph_json,
                    });
                }
            }
            Ev::TaskResult {
                task,
                status,
                iter,
                note,
                cost_usd,
                secs,
                output,
                blocked,
            } => {
                if task.is_empty() {
                    continue;
                }
                if let Some(output) = output.filter(|o| !o.is_null())
                    && status == "pass"
                {
                    run.result.insert(task.clone(), output);
                }
                run.task_results.push(TaskResult {
                    iter: iter.into(),
                    task,
                    status,
                    note,
                    // NaN/inf would poison the column; drop those to NULL rather than store them.
                    cost_usd: Some(cost_usd).filter(|c| c.is_finite()),
                    // The plan executor emits 0.0 for a duration it never measured; no real task
                    // runs in zero time, so 0 is "unknown", not a measurement.
                    secs: Some(secs).filter(|s| s.is_finite() && *s > 0.0),
                    blocked,
                });
            }
            Ev::Shutdown { outcome, reason } => {
                if !outcome.is_empty() {
                    run.outcome = Some(outcome);
                }
                if !reason.is_empty() {
                    run.outcome_reason = Some(reason);
                }
            }
            Ev::Other => {}
        }
    }
    // A session with no budget checkpoint still spent what its tasks spent: the playbook lane runs
    // a graph once and publishes per-task costs without a running total. Summing the engine's own
    // numbers is what keeps that spend on the ledger.
    if run.cost_usd.is_none() {
        let spent: f64 = run
            .task_results
            .iter()
            .filter(|t| !is_folded_instance(&t.task, &run.task_results))
            .filter_map(|t| t.cost_usd)
            .sum();
        if spent > 0.0 {
            run.cost_usd = Some(spent);
        }
    }
    // Fallback headline: with no Summary event, the last kept deep score is the best-so-far (the
    // loop only keeps a candidate that beat the running best).
    if run.best_score.is_none() {
        run.best_score = run
            .candidates
            .iter()
            .rev()
            .find(|c| c.kind == "deep" && c.decision == "keep")
            .and_then(|c| c.score);
    }
    run
}

/// Where one ingest lands: the run's id, the scope it attaches to, the pod that produced it (the
/// dispatched path only), and the evidence pointer stored on the row.
pub struct IngestTarget<'a> {
    pub run_id: &'a str,
    pub scope_id: Option<i64>,
    /// The issue the run belongs to; `None` leaves the row's existing link alone.
    pub issue: Option<&'a str>,
    pub pod: Option<&'a str>,
    pub session_uri: &'a str,
}

/// Write a folded run: the `runs` row, its candidates, its work graph, and (when `ledger`) its
/// cost. One transaction, so a failure part-way through leaves the previous state intact instead
/// of a half-refolded run. `replace_candidates` drops the run's existing candidate rows first —
/// `candidates` has no primary key and [`crate::runs::store::insert_candidate`] is a plain INSERT,
/// so a re-fold without it duplicates every row.
async fn write_parsed_run(
    db: &Db,
    target: &IngestTarget<'_>,
    parsed: &ParsedRun,
    ledger: bool,
    replace_candidates: bool,
) -> Result<()> {
    let IngestTarget {
        run_id,
        scope_id,
        issue,
        pod,
        session_uri,
    } = *target;
    let mut tx = db
        .pool()
        .begin()
        .await
        .context("opening the ingest transaction")?;

    if replace_candidates {
        crate::runs::store::delete_candidates_for_run(&mut *tx, run_id).await?;
    }

    crate::runs::store::insert_run(
        &mut *tx,
        &NewRun {
            run_id: run_id.to_string(),
            scope: scope_id,
            issue: issue.map(str::to_string),
            identity_digest: parsed.identity_digest.clone(),
            status: parsed
                .outcome
                .clone()
                .unwrap_or_else(|| "incomplete".to_string()),
            pod: pod.map(str::to_string),
            session_uri: Some(session_uri.to_string()),
            best_score: parsed.best_score,
            cost_usd: parsed.cost_usd,
        },
    )
    .await
    .context("recording the run summary")?;

    // Fold the opened PR(s) onto the kept candidate rows' `pr_url`. Granularity: a `candidates` row
    // is a per-iteration (deep) or per-lane (wide) unit — it carries NO component identity (only
    // kind/lane/iter/decision/score), so for a composite run it cannot be matched to a specific
    // component's PR. The two honest cases:
    //   * single-repo (one distinct PR url across all links) — the whole run maps to that one PR, so
    //     every kept row carries it. This is the common path (EPP-style runs).
    //   * composite (multiple distinct PR urls) — a kept candidate is a cross-component change
    //     spanning ALL the component PRs, which a single-valued column can't hold, and the
    //     candidate/component axes are orthogonal so there's no correct per-row mapping. We leave
    //     `pr_url = None` on composite kept rows rather than invent one; the full set lives on
    //     `ParsedRun::pr_links` (→ `complete_run` still lands `pr-open`) and in the S3 `summary.json`.
    //     Per-component review-reseed for composite runs is a documented follow-up, not this fix.
    let single_pr_url: Option<&str> = {
        let mut urls: Vec<&str> = parsed.pr_links.iter().map(|l| l.url.as_str()).collect();
        urls.sort_unstable();
        urls.dedup();
        match urls.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    };
    // The branch travels with its PR: single-repo (one PR) stamps that PR's branch onto every kept
    // row alongside its url. A composite run (several distinct PR urls) leaves both NULL for the same
    // reason — a single-valued column can't hold a cross-component set.
    let single_branch: Option<&str> = single_pr_url.and_then(|url| {
        parsed
            .pr_links
            .iter()
            .find(|l| l.url == url)
            .map(|l| l.branch.as_str())
            .filter(|b| !b.is_empty())
    });
    for c in &parsed.candidates {
        let kept = c.decision == "keep";
        let pr_url = kept.then_some(single_pr_url).flatten().map(str::to_string);
        let branch = kept.then_some(single_branch).flatten().map(str::to_string);
        crate::runs::store::insert_candidate(
            &mut *tx,
            &NewCandidate {
                run_id: run_id.to_string(),
                kind: Some(c.kind.clone()),
                lane: c.lane,
                iter: c.iter,
                score: c.score,
                decision: Some(c.decision.clone()),
                worktree: None,
                sandbox: None,
                pr_url,
                branch,
            },
        )
        .await
        .context("recording a candidate")?;
    }

    // The task graph: one row per admitted plan version, one per task attempt. Both are upserts,
    // so a re-ingest of the same log replays onto the same rows instead of failing on the PK.
    for plan in &parsed.plans {
        crate::runs::task_results::upsert_run_plan(
            &mut *tx,
            run_id,
            plan.plan_version,
            &plan.graph_json,
        )
        .await
        .context("recording an admitted plan")?;
    }
    for result in &parsed.task_results {
        crate::runs::task_results::upsert_task_result(&mut *tx, run_id, result)
            .await
            .context("recording a task result")?;
    }

    let booked = ledger.then_some(parsed.cost_usd).flatten();
    if let Some(cost) = booked {
        crate::ledger::ledger_append(
            &mut *tx,
            &crate::clock::now_rfc3339(),
            Some(run_id),
            "run",
            cost,
        )
        .await
        .context("ledgering the run cost")?;
    }

    tx.commit().await.context("committing the ingest")?;

    // The spend re-export that `Db::ledger_append` composes, done here instead: the append rides
    // inside the transaction above, so the counter only moves once the commit made it real.
    if let Some(cost) = booked
        && let Some(m) = db.metrics()
    {
        m.record_spend("run", cost);
    }

    Ok(())
}

/// Ingest a finished run from its session-log content, booking its cost. `session_uri` is the
/// evidence pointer stored on the run row (the `db://run-session/…` artifact-store spelling for
/// runs the controller persists, a local path or S3 URI for adopted evidence). The write covers
/// the `runs` summary row, one `candidates` row per folded candidate, and the `kind='run'` ledger
/// entry for its cost. Idempotent by run_id only at the DB's PK level; re-ingesting the same run
/// twice is the caller's (the pod-watch) responsibility to avoid.
pub async fn ingest_session(
    db: &Db,
    target: &IngestTarget<'_>,
    content: &str,
) -> Result<ParsedRun> {
    let parsed = parse_session(content);
    write_parsed_run(db, target, &parsed, true, false).await?;
    Ok(parsed)
}

/// What one adopt did, for the CLI's summary line.
#[derive(Debug, Clone, PartialEq)]
pub struct AdoptOutcome {
    pub status: String,
    pub best_score: Option<f64>,
    pub cost_usd: Option<f64>,
    pub candidates: usize,
    /// True on a re-adopt whose cost was already on the ledger (the append was skipped).
    pub ledger_skipped: bool,
}

/// Refuse to adopt over a run the controller dispatched and is still driving: that row belongs to
/// the pod watch, which will fold the real log onto it when the pod finishes.
async fn ensure_not_dispatched(db: &Db, run_id: &str) -> Result<()> {
    let Some(existing) = crate::runs::store::get_run(db.pool(), run_id).await? else {
        return Ok(());
    };
    anyhow::ensure!(
        existing.status != "running" && existing.pod.is_none(),
        "run {run_id} is controller-dispatched and live; adopt would clobber it"
    );
    Ok(())
}

/// A local session uri is stored absolute: the artifact proxy resolves it server-side, where a
/// relative path would land against the server's cwd instead of the operator's. `s3://` (and any
/// other scheme) passes through untouched.
fn absolute_session_uri(session_uri: &str) -> String {
    if session_uri.contains("://") {
        return session_uri.to_string();
    }
    let path = Path::new(session_uri);
    if let Ok(abs) = std::fs::canonicalize(path) {
        return abs.to_string_lossy().into_owned();
    }
    // Not on this host's filesystem (adopting a log the server can see and we can't): the best we
    // can do is anchor it to the cwd the operator typed it in.
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path).to_string_lossy().into_owned(),
        Err(_) => session_uri.to_string(),
    }
}

/// Adopt a foreign run (launched outside the controller) into the ledger from its session log.
/// `session_uri` is the ORIGINAL pointer (s3:// or a local path), never the downloaded temp path —
/// the artifact proxy and Flow tab resolve evidence off it (a local one is stored absolute).
/// Idempotent by `run_id`: the runs row upserts, the run's existing candidate rows are dropped
/// before re-folding, and the `kind='run'` cost append is skipped when one is already booked (the
/// caps SUM the ledger). Refuses a file that isn't a session log and a run the controller is still
/// driving, both before the first write.
pub async fn adopt_session_file(
    db: &Db,
    run_id: &str,
    scope_id: Option<i64>,
    path: &Path,
    session_uri: &str,
) -> Result<AdoptOutcome> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading session log {}", path.display()))?;
    // Everything that can reject the adopt runs before the first write: parse, shape check, live-run
    // guard. Only then does the transaction below delete the old candidates and re-fold.
    let parsed = parse_session(&content);
    parsed.ensure_session_log(session_uri)?;
    ensure_not_dispatched(db, run_id).await?;
    let already_ledgered = crate::runs::store::run_cost_ledgered(db.pool(), run_id).await?;
    let stored_uri = absolute_session_uri(session_uri);
    let target = IngestTarget {
        run_id,
        scope_id,
        issue: None,
        pod: None,
        session_uri: &stored_uri,
    };
    write_parsed_run(db, &target, &parsed, !already_ledgered, true).await?;
    Ok(AdoptOutcome {
        status: parsed
            .outcome
            .clone()
            .unwrap_or_else(|| "incomplete".to_string()),
        best_score: parsed.best_score,
        cost_usd: parsed.cost_usd,
        candidates: parsed.candidates.len(),
        ledger_skipped: already_ledgered && parsed.cost_usd.is_some(),
    })
}

/// Resolve `--issue` for an adopt: the issue's latest scope id. An issue with no scope is an
/// error — scopes are evidence of a real scoping turn and are never fabricated.
pub async fn adopt_scope_for_issue(db: &Db, issue: &str) -> Result<i64> {
    crate::issues::store::latest_scope_for_issue(db.pool(), issue)
        .await?
        .map(|s| s.id)
        .with_context(|| format!("issue {issue} has no scope to attach; omit --issue"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_session_evidence_requires_a_decodable_shutdown_outcome() {
        assert_eq!(
            validate_terminal_session(r#"{"v":1,"kind":"identity","identity":{"digest":"x"}}"#),
            Err(TerminalSessionError::MissingShutdown)
        );
        assert_eq!(
            validate_terminal_session(r#"{"v":1,"kind":"shutdown","outcome":""}"#),
            Err(TerminalSessionError::InvalidShutdown(
                "outcome is empty".to_string()
            ))
        );
        assert!(
            validate_terminal_session(r#"{"v":1,"kind":"shutdown","outcome":"finished"}"#).is_ok()
        );
        assert_eq!(
            validate_terminal_session(
                "{\"v\":1,\"kind\":\"shutdown\",\"outcome\":\"finished\"}\n{\"v\":1,\"kind\":\"shutdown\",\"outcome\":\"\"}"
            ),
            Err(TerminalSessionError::InvalidShutdown(
                "outcome is empty".to_string()
            )),
            "the final shutdown record is authoritative"
        );
    }
    use crate::issues::model::NewIssue;
    use crate::model::Status;
    use sqlx::PgPool;
    use std::path::PathBuf;

    /// A wide-then-deep session log in the exact envelope shape `crucible::session::encode` emits
    /// (`{"v":1,"kind":…}`), including foreign lines (an `agent` event, a blank, a torn tail) the
    /// parser must skip.
    fn sample_log() -> String {
        [
            r#"{"v":1,"kind":"start","goal":"raise it","gate":"bench","model":"m","namespace":"ns","iters_total":3,"max_cost":5.0,"max_secs":900}"#,
            r#"{"v":1,"kind":"identity","identity":{"components":[],"manifest_hash":"aaaa","inject_hash":"bbbb","measure_cmd":"./m.sh","direction":"higher","rig":{},"digest":"v1:cafef00d"}}"#,
            r#"{"v":1,"kind":"row","row":{"iter":0,"decision":"wide-keep-0","note":"cand 0","detail":"","diff":"","diffstat":"","score":300.0,"total":null,"phase":"wide"},"solved":false}"#,
            r#"{"v":1,"kind":"row","row":{"iter":1,"decision":"wide-drop-1","note":"cand 1","detail":"","diff":"","diffstat":"","score":200.0,"total":null,"phase":"wide"},"solved":false}"#,
            r#"{"v":1,"kind":"agent","event":{"kind":"text","delta":"thinking out loud"}}"#,
            r#"{"v":1,"kind":"row","row":{"iter":0,"decision":"baseline","note":"","detail":"","score":200.0,"total":10},"solved":false}"#,
            "",
            r#"{"v":1,"kind":"row","row":{"iter":1,"decision":"keep","note":"","detail":"","score":260.0,"total":10},"solved":false}"#,
            r#"{"v":1,"kind":"budget","spent":2.5,"elapsed_secs":400}"#,
            r#"{"v":1,"kind":"summary","rows":[],"gate":"bench","best_score":260.0}"#,
            r#"{"v":1,"kind":"finished"}"#,
            r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":"all iterations completed"}"#,
            r#"{"v":1,"kind":"row","row":{"iter":2,"deci"#, // torn tail
        ]
        .join("\n")
    }

    /// A single-repo deep run that kept one iteration and opened one draft PR — the common P1 path.
    fn single_repo_kept_log() -> String {
        [
            r#"{"v":1,"kind":"identity","identity":{"digest":"v1:aaaa"}}"#,
            r#"{"v":1,"kind":"row","row":{"iter":0,"decision":"baseline","score":300.0},"solved":false}"#,
            r#"{"v":1,"kind":"row","row":{"iter":1,"decision":"discard","score":290.0},"solved":false}"#,
            r#"{"v":1,"kind":"row","row":{"iter":2,"decision":"keep","score":250.0},"solved":false}"#,
            r#"{"v":1,"kind":"budget","spent":1.5,"elapsed_secs":200}"#,
            r#"{"v":1,"kind":"summary","rows":[],"gate":"bench","best_score":250.0}"#,
            r#"{"v":1,"kind":"pr_links","links":[{"url":"https://github.com/o/r/pull/7","repo":"o/r","name":"","branch":"autoresearch/run-xyz/0"}]}"#,
            r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":"done"}"#,
        ]
        .join("\n")
    }

    /// The playbook lane publishes per-task costs and no running total, so a session with no
    /// budget checkpoint still books what its tasks spent. A budget event, when there is one, is
    /// the authority.
    #[test]
    fn a_session_without_a_budget_checkpoint_books_what_its_tasks_spent() {
        let log = [
            r#"{"v":1,"kind":"task_result","task":"survey","status":"pass","cost_usd":0.2774205}"#,
            r#"{"v":1,"kind":"task_result","task":"investigate","status":"pass","cost_usd":-0.0}"#,
            r#"{"v":1,"kind":"task_result","task":"roundup","status":"pass","cost_usd":0.0}"#,
        ]
        .join("\n");
        assert_eq!(parse_session(&log).cost_usd, Some(0.2774205));

        let with_budget = format!("{log}\n{}", r#"{"v":1,"kind":"budget","spent":9.0}"#);
        assert_eq!(parse_session(&with_budget).cost_usd, Some(9.0));

        let free =
            r#"{"v":1,"kind":"task_result","task":"roundup","status":"pass","cost_usd":0.0}"#;
        assert_eq!(
            parse_session(free).cost_usd,
            None,
            "a free run books nothing"
        );
    }

    /// A mapped node settles twice: once per instance, and once as a rollup whose cost is already
    /// the sum of the instances. Summing both books the fan-out's spend twice.
    #[test]
    fn a_fan_outs_instances_are_not_booked_on_top_of_its_rollup() {
        let log = [
            r#"{"v":1,"kind":"task_result","task":"survey","status":"pass","cost_usd":0.2865325}"#,
            r#"{"v":1,"kind":"task_result","task":"investigate[01]","status":"pass","cost_usd":0.3}"#,
            r#"{"v":1,"kind":"task_result","task":"investigate[02]","status":"pass","cost_usd":0.245781}"#,
            r#"{"v":1,"kind":"task_result","task":"investigate","status":"pass","cost_usd":0.545781}"#,
            r#"{"v":1,"kind":"task_result","task":"roundup","status":"pass","cost_usd":0.0}"#,
        ]
        .join("\n");
        let parsed = parse_session(&log);
        assert_eq!(parsed.cost_usd, Some(0.2865325 + 0.545781));
        assert_eq!(
            parsed.task_results.len(),
            5,
            "the instances stay on the run's graph; only the sum drops them"
        );
    }

    /// Nothing on the wire marks an instance, so the fold keys off the bracket plus a settled node
    /// of that name. A bracketed name whose node never settled carries its own cost and nothing
    /// else's, so it is still booked.
    #[test]
    fn an_instance_without_its_rollup_is_still_booked() {
        let log = [
            r#"{"v":1,"kind":"task_result","task":"investigate[01]","status":"pass","cost_usd":0.5}"#,
            r#"{"v":1,"kind":"task_result","task":"audit","status":"pass","cost_usd":0.25}"#,
        ]
        .join("\n");
        assert_eq!(parse_session(&log).cost_usd, Some(0.75));
    }

    /// A playbook run's product is what its tasks emitted: each passing task's `output`, keyed by
    /// task name, which is the document a schedule's cursor addresses. A retried task's last
    /// attempt wins, a failed one contributes nothing, and a log without outputs leaves it empty.
    #[test]
    fn parse_folds_passing_task_outputs_into_the_run_result() {
        let log = [
            r#"{"v":1,"kind":"identity","identity":{"digest":"v1:aaaa"}}"#,
            r#"{"v":1,"kind":"task_result","task":"scan","status":"fail","output":{"newest_created_at":"2026-08-01T00:00:00Z"}}"#,
            r#"{"v":1,"kind":"task_result","task":"scan","status":"pass","output":{"newest_created_at":"2026-08-23T00:00:00Z","seen":4}}"#,
            r#"{"v":1,"kind":"task_result","task":"triage","status":"pass","output":"done"}"#,
            r#"{"v":1,"kind":"task_result","task":"report","status":"pass"}"#,
            r#"{"v":1,"kind":"task_result","task":"notify","status":"pass","output":null}"#,
            r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":"graph complete"}"#,
        ]
        .join("\n");
        let parsed = parse_session(&log);
        assert_eq!(
            parsed.result["scan"]["newest_created_at"],
            serde_json::json!("2026-08-23T00:00:00Z"),
            "the passing attempt is the one the cursor reads"
        );
        assert_eq!(parsed.result["triage"], serde_json::json!("done"));
        assert!(!parsed.result.contains_key("report"), "no output, no entry");
        assert!(!parsed.result.contains_key("notify"), "null is no output");
        assert_eq!(
            parsed.task_results.len(),
            5,
            "every attempt is still ledgered"
        );

        assert!(
            parse_session(&single_repo_kept_log()).result.is_empty(),
            "a scored loop emits no task outputs and leaves the result empty"
        );
    }

    #[test]
    fn parse_folds_pr_links_and_ignores_urlless_entries() {
        let parsed = parse_session(&single_repo_kept_log());
        assert_eq!(parsed.pr_links.len(), 1);
        assert_eq!(parsed.pr_links[0].url, "https://github.com/o/r/pull/7");
        assert_eq!(parsed.pr_links[0].repo, "o/r");
        assert!(parsed.pr_links[0].name.is_empty());
        assert_eq!(parsed.pr_links[0].branch, "autoresearch/run-xyz/0");

        // A composite run emits one link per component; a url-less entry is dropped.
        let composite = [
            r#"{"v":1,"kind":"pr_links","links":[{"url":"https://github.com/o/vllm/pull/1","repo":"o/vllm","name":"vllm"},{"url":"","repo":"o/epp","name":"epp"},{"url":"https://github.com/o/epp/pull/2","repo":"o/epp","name":"epp"}]}"#,
        ]
        .join("\n");
        let parsed = parse_session(&composite);
        assert_eq!(parsed.pr_links.len(), 2, "url-less entry dropped");
        assert_eq!(parsed.pr_links[1].name, "epp");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn ingest_populates_pr_url_on_kept_rows_only(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        crate::issues::store::upsert_issue(
            db.pool(),
            &NewIssue {
                key: "o/r#1".into(),
                repo: "o/r".into(),
                priority: 0,
                evidence_url: None,
                title: None,
                author: None,
                body: None,
                labels: Vec::new(),
                upstream_updated_at: None,
            },
        )
        .await?;
        assert!(
            crate::issues::store::claim_issue(db.pool(), "o/r#1", Status::New, Status::Scoped)
                .await?
        );
        let scope_id = crate::issues::store::insert_scope(
            db.pool(),
            &crate::issues::model::NewScope {
                issue: "o/r#1".into(),
                pack_digest: Some("v1:aaaa".into()),
                check_outcome: Some("PASS".into()),
            },
        )
        .await?;

        let parsed = ingest_session(
            &db,
            &IngestTarget {
                run_id: "run-pr",
                scope_id: Some(scope_id),
                issue: None,
                pod: None,
                session_uri: "s3://b/run-pr/session.jsonl",
            },
            &single_repo_kept_log(),
        )
        .await?;
        assert_eq!(parsed.pr_links.len(), 1);

        let rows = crate::runs::store::list_candidates_for_run(db.pool(), "run-pr").await?;
        for r in &rows {
            if r.decision.as_deref() == Some("keep") {
                assert_eq!(r.pr_url.as_deref(), Some("https://github.com/o/r/pull/7"));
                // The PR's head branch rides along with its url onto the kept row.
                assert_eq!(r.branch.as_deref(), Some("autoresearch/run-xyz/0"));
            } else {
                assert!(r.pr_url.is_none(), "only kept rows carry a pr_url");
                assert!(r.branch.is_none(), "only kept rows carry a branch");
            }
        }
        // The kept-PR approval query now returns the seeded row (was empty before the P1 fix).
        let kept = crate::runs::store::kept_candidate_prs(db.pool()).await?;
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].issue, "o/r#1");
        assert_eq!(kept[0].pr_url, "https://github.com/o/r/pull/7");
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn ingest_leaves_composite_pr_url_null_and_documents_the_limit(
        pool: PgPool,
    ) -> Result<()> {
        let (db, _d) = db_with(pool);
        crate::issues::store::upsert_issue(
            db.pool(),
            &NewIssue {
                key: "o/r#2".into(),
                repo: "o/r".into(),
                priority: 0,
                evidence_url: None,
                title: None,
                author: None,
                body: None,
                labels: Vec::new(),
                upstream_updated_at: None,
            },
        )
        .await?;
        assert!(
            crate::issues::store::claim_issue(db.pool(), "o/r#2", Status::New, Status::Scoped)
                .await?
        );
        let scope_id = crate::issues::store::insert_scope(
            db.pool(),
            &crate::issues::model::NewScope {
                issue: "o/r#2".into(),
                pack_digest: None,
                check_outcome: None,
            },
        )
        .await?;
        let log = [
            r#"{"v":1,"kind":"row","row":{"iter":1,"decision":"keep","score":10.0},"solved":false}"#,
            r#"{"v":1,"kind":"pr_links","links":[{"url":"https://github.com/o/vllm/pull/1","repo":"o/vllm","name":"vllm"},{"url":"https://github.com/o/epp/pull/2","repo":"o/epp","name":"epp"}]}"#,
            r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":"done"}"#,
        ]
        .join("\n");
        let parsed = ingest_session(
            &db,
            &IngestTarget {
                run_id: "run-comp",
                scope_id: Some(scope_id),
                issue: None,
                pod: None,
                session_uri: "s3://b/x",
            },
            &log,
        )
        .await?;
        assert_eq!(parsed.pr_links.len(), 2, "full set surfaced on ParsedRun");
        // Two distinct component urls can't map onto a single-valued per-candidate column, so the
        // kept row stays NULL — the run-level set carries the composite provenance instead.
        let kept = crate::runs::store::kept_candidate_prs(db.pool()).await?;
        assert!(
            kept.is_empty(),
            "composite kept rows carry no per-candidate pr_url"
        );
        Ok(())
    }

    #[test]
    fn parse_folds_wide_lanes_and_deep_iters_distinctly() {
        let parsed = parse_session(&sample_log());
        assert_eq!(parsed.identity_digest.as_deref(), Some("v1:cafef00d"));
        assert_eq!(parsed.best_score, Some(260.0));
        assert_eq!(parsed.cost_usd, Some(2.5));
        assert_eq!(parsed.outcome.as_deref(), Some("finished"));

        assert_eq!(parsed.candidates.len(), 4, "2 wide lanes + 2 deep iters");
        let wide: Vec<_> = parsed
            .candidates
            .iter()
            .filter(|c| c.kind == "wide")
            .collect();
        assert_eq!(wide.len(), 2);
        assert_eq!(wide[0].lane, Some(0));
        assert_eq!(wide[0].iter, None);
        assert_eq!(wide[0].score, Some(300.0));
        assert_eq!(wide[0].decision, "wide-keep-0");

        let deep: Vec<_> = parsed
            .candidates
            .iter()
            .filter(|c| c.kind == "deep")
            .collect();
        assert_eq!(deep.len(), 2);
        assert_eq!(deep[1].iter, Some(1));
        assert_eq!(deep[1].lane, None);
        assert_eq!(deep[1].decision, "keep");
    }

    #[test]
    fn parse_falls_back_to_last_kept_deep_score_without_a_summary() {
        let log = [
            r#"{"v":1,"kind":"row","row":{"iter":0,"decision":"baseline","score":200.0}}"#,
            r#"{"v":1,"kind":"row","row":{"iter":1,"decision":"keep","score":190.0}}"#,
            r#"{"v":1,"kind":"row","row":{"iter":2,"decision":"reject","score":195.0}}"#,
        ]
        .join("\n");
        let parsed = parse_session(&log);
        assert_eq!(parsed.best_score, Some(190.0), "last kept, not last seen");
        assert_eq!(parsed.cost_usd, None);
        assert_eq!(parsed.outcome, None, "no shutdown line = incomplete");
    }

    /// A work-graph run: one admitted plan, then two iterations of task results — including a
    /// retry of `measure` in iteration 1 that the (run_id, iter, task) upsert must collapse.
    fn plan_run_log() -> String {
        [
            r#"{"v":1,"kind":"plan_admitted","plan_version":1,"reason":"","budget_usd":5.0,"tasks":[{"name":"propose","kind":"agent","depends_on":[],"session":"solver","needs":"all","required":true},{"name":"measure","kind":"command","depends_on":["propose"],"session":"","needs":"all","required":true}]}"#,
            r#"{"v":1,"kind":"task_result","task":"propose","status":"pass","plan_version":1,"task_kind":"agent","iter":0,"attempts":1,"cost_usd":0.75,"note":"","secs":12.0}"#,
            r#"{"v":1,"kind":"task_result","task":"measure","status":"pass","plan_version":1,"task_kind":"command","iter":0,"attempts":1,"cost_usd":0.0,"note":"","secs":30.0}"#,
            r#"{"v":1,"kind":"task_result","task":"propose","status":"pass","plan_version":1,"task_kind":"agent","iter":1,"attempts":1,"cost_usd":0.5,"note":"","secs":9.0}"#,
            r#"{"v":1,"kind":"task_result","task":"measure","status":"transport","plan_version":1,"task_kind":"command","iter":1,"attempts":1,"cost_usd":0.0,"note":"rig unreachable","secs":1.0}"#,
            r#"{"v":1,"kind":"task_result","task":"measure","status":"fail","plan_version":1,"task_kind":"command","iter":1,"attempts":2,"cost_usd":0.0,"note":"regressed","secs":28.0}"#,
            r#"{"v":1,"kind":"task_result","task":"report","status":"blocked","plan_version":1,"task_kind":"command","iter":1,"attempts":0,"cost_usd":0.0,"note":"required task measure failed","blocked":{"reason":"required_task_failed","task":"measure"},"secs":0.0}"#,
            r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":"done"}"#,
        ]
        .join("\n")
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn ingest_folds_the_plan_and_task_results(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        ingest_session(
            &db,
            &IngestTarget {
                run_id: "run-plan",
                scope_id: None,
                issue: None,
                pod: None,
                session_uri: "s3://b/x",
            },
            &plan_run_log(),
        )
        .await?;

        let plan = crate::runs::task_results::latest_run_plan(db.pool(), "run-plan")
            .await?
            .expect("the admitted plan");
        assert_eq!(plan.plan_version, 1);
        let tasks: Vec<PlanTaskWire> = serde_json::from_str(&plan.graph_json)?;
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].name, "propose");
        assert_eq!(tasks[0].session, "solver", "the session rides through");
        assert_eq!(tasks[1].depends_on, vec!["propose".to_string()]);

        let results = crate::runs::task_results::list_task_results(db.pool(), "run-plan").await?;
        assert_eq!(
            results.len(),
            5,
            "the retried (1, measure) collapsed to one"
        );
        assert_eq!(
            results
                .iter()
                .map(|r| (r.iter, r.task.as_str(), r.status.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (0, "measure", "pass"),
                (0, "propose", "pass"),
                (1, "measure", "fail"),
                (1, "propose", "pass"),
                (1, "report", "blocked"),
            ],
            "last write wins on the retry"
        );
        assert_eq!(results[2].note, "regressed");
        assert_eq!(results[2].blocked, None);
        assert_eq!(results[3].cost_usd, Some(0.5));
        assert_eq!(results[3].secs, Some(9.0));
        assert_eq!(
            results[4].blocked,
            Some(TaskBlocked {
                reason: BlockedReasonKind::RequiredTaskFailed,
                task: Some("measure".to_string()),
            }),
            "the typed reason survives the round trip beside its note"
        );

        // Re-ingesting the same log replays onto the same rows rather than tripping the PK.
        ingest_session(
            &db,
            &IngestTarget {
                run_id: "run-plan",
                scope_id: None,
                issue: None,
                pod: None,
                session_uri: "s3://b/x",
            },
            &plan_run_log(),
        )
        .await?;
        assert_eq!(
            crate::runs::task_results::list_task_results(db.pool(), "run-plan")
                .await?
                .len(),
            5
        );
        Ok(())
    }

    #[test]
    fn a_blocked_task_names_its_typed_reason_when_nothing_else_left_a_note() {
        let blocked_only = [
            r#"{"v":1,"kind":"task_result","task":"brief","status":"transport","iter":0,"note":""}"#,
            r#"{"v":1,"kind":"task_result","task":"deliver","status":"blocked","iter":0,"note":"required task brief failed","blocked":{"reason":"required_task_failed","task":"brief"}}"#,
            r#"{"v":1,"kind":"shutdown","outcome":"error","reason":"short-circuited at brief"}"#,
        ]
        .join("\n");
        let run = parse_session(&blocked_only);
        assert_eq!(
            run.failure_cause().as_deref(),
            Some("deliver blocked: required task brief failed")
        );
        assert_eq!(
            run.task_results[1].blocked,
            Some(TaskBlocked {
                reason: BlockedReasonKind::RequiredTaskFailed,
                task: Some("brief".to_string()),
            })
        );

        let noted = [
            r#"{"v":1,"kind":"task_result","task":"brief","status":"fail","iter":0,"note":"verdict refuted"}"#,
            r#"{"v":1,"kind":"task_result","task":"deliver","status":"blocked","iter":0,"note":"required task brief failed","blocked":{"reason":"required_task_failed","task":"brief"}}"#,
        ]
        .join("\n");
        assert_eq!(
            parse_session(&noted).failure_cause().as_deref(),
            Some("verdict refuted"),
            "a failing task's own note still comes first"
        );

        for (blocked, note, want) in [
            (
                r#"{"reason":"budget_ceiling"}"#,
                "",
                "scan blocked: budget ceiling reached",
            ),
            (
                r#"{"reason":"wall_clock_ceiling"}"#,
                "",
                "scan blocked: wall-clock ceiling reached",
            ),
            (
                r#"{"reason":"dependency_did_not_pass"}"#,
                "",
                "scan blocked: dependency did not pass",
            ),
            (
                r#"{"reason":"staging_refused"}"#,
                "no room for the capture set",
                "scan blocked: staging refused: no room for the capture set",
            ),
            (
                r#"{"reason":"required_task_failed"}"#,
                "",
                "scan blocked: a required task failed",
            ),
        ] {
            let log = format!(
                r#"{{"v":1,"kind":"task_result","task":"scan","status":"blocked","iter":0,"note":"{note}","blocked":{blocked}}}"#
            );
            assert_eq!(parse_session(&log).failure_cause().as_deref(), Some(want));
        }
    }

    #[test]
    fn transport_losses_count_each_task_once_by_its_final_attempt() {
        let log = [
            r#"{"v":1,"kind":"task_result","task":"triage[a]","status":"transport","iter":0,"note":"gateway down"}"#,
            r#"{"v":1,"kind":"task_result","task":"triage[b]","status":"transport","iter":0,"note":"gateway down"}"#,
            r#"{"v":1,"kind":"task_result","task":"triage[b]","status":"pass","iter":0,"note":""}"#,
            r#"{"v":1,"kind":"task_result","task":"triage","status":"fail","iter":0,"note":"1 of 2 instances failed: a"}"#,
            r#"{"v":1,"kind":"task_result","task":"roundup","status":"pass","iter":0,"note":""}"#,
            r#"{"v":1,"kind":"task_result","task":"roundup","status":"transport","iter":1,"note":"rig unreachable"}"#,
            r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":"completed"}"#,
        ]
        .join("\n");
        let run = parse_session(&log);
        assert_eq!(
            run.transport_losses(),
            2,
            "triage[b]'s retry passed; triage[a] and iteration 1's roundup were lost"
        );
        assert_eq!(parse_session("").transport_losses(), 0);
    }

    /// The deployed-run shape that motivated the merge: the engine re-admits the same 9-task DAG
    /// every iteration and the post-run epilogue admits a single-task stub, all under v1.
    fn readmitted_plan_log() -> String {
        let dag = r#"{"v":1,"kind":"plan_admitted","plan_version":1,"reason":"","budget_usd":5.0,"tasks":[{"name":"propose","kind":"agent","depends_on":[],"session":"solver","needs":"all","required":true},{"name":"apply","kind":"command","depends_on":["propose"],"session":"","needs":"all","required":true},{"name":"build","kind":"command","depends_on":["apply"],"session":"","needs":"all","required":true},{"name":"correctness","kind":"command","depends_on":["build"],"session":"","needs":"all","required":true},{"name":"latency","kind":"command","depends_on":["build"],"session":"","needs":"all","required":true},{"name":"ab-toggle","kind":"command","depends_on":["build"],"session":"","needs":"all","required":false},{"name":"mechanism","kind":"agent","depends_on":["build"],"session":"","needs":"all","required":false},{"name":"grade","kind":"agent","depends_on":["correctness","latency","ab-toggle","mechanism"],"session":"","needs":"all","required":true},{"name":"decide","kind":"command","depends_on":["grade"],"session":"","needs":"all","required":true}]}"#;
        let stub = r#"{"v":1,"kind":"plan_admitted","plan_version":1,"reason":"","budget_usd":0.0,"tasks":[{"name":"full-eval","kind":"evaluate","depends_on":[]}]}"#;
        [dag, dag, stub].join("\n")
    }

    #[test]
    fn parse_merges_readmitted_same_version_plans_by_task_name_union() {
        let parsed = parse_session(&readmitted_plan_log());
        assert_eq!(parsed.plans.len(), 1, "one row per plan version");
        assert_eq!(parsed.plans[0].plan_version, 1);
        let tasks: Vec<PlanTaskWire> = serde_json::from_str(&parsed.plans[0].graph_json).unwrap();
        assert_eq!(
            tasks.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec![
                "propose",
                "apply",
                "build",
                "correctness",
                "latency",
                "ab-toggle",
                "mechanism",
                "grade",
                "decide",
                "full-eval",
            ],
            "first-seen order, epilogue stub appended"
        );
        let grade = tasks.iter().find(|t| t.name == "grade").unwrap();
        assert_eq!(
            grade.depends_on,
            vec!["correctness", "latency", "ab-toggle", "mechanism"],
            "the DAG's edges survive the stub"
        );
        assert_eq!(tasks[0].session, "solver", "first declaration wins");
        assert!(tasks[9].depends_on.is_empty());
    }

    #[test]
    fn parse_keeps_distinct_plan_versions_as_separate_plans() {
        let log = [
            r#"{"v":1,"kind":"plan_admitted","plan_version":1,"tasks":[{"name":"propose","kind":"agent","depends_on":[]}]}"#,
            r#"{"v":1,"kind":"plan_admitted","plan_version":2,"tasks":[{"name":"propose","kind":"agent","depends_on":[]},{"name":"measure","kind":"command","depends_on":["propose"]}]}"#,
        ]
        .join("\n");
        let parsed = parse_session(&log);
        assert_eq!(parsed.plans.len(), 2, "a replan is a new row, not a merge");
        assert_eq!(parsed.plans[0].plan_version, 1);
        assert_eq!(parsed.plans[1].plan_version, 2);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn ingest_writes_the_merged_graph_not_the_epilogue_stub(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        ingest_session(
            &db,
            &IngestTarget {
                run_id: "run-readmit",
                scope_id: None,
                issue: None,
                pod: None,
                session_uri: "s3://b/y",
            },
            &readmitted_plan_log(),
        )
        .await?;
        let plan = crate::runs::task_results::latest_run_plan(db.pool(), "run-readmit")
            .await?
            .expect("the admitted plan");
        assert_eq!(plan.plan_version, 1);
        let tasks: Vec<PlanTaskWire> = serde_json::from_str(&plan.graph_json)?;
        assert_eq!(tasks.len(), 10, "9 DAG tasks + full-eval");
        Ok(())
    }

    #[test]
    fn parse_ignores_a_task_result_without_a_task_name() {
        let log = r#"{"v":1,"kind":"task_result","task":"","status":"pass"}"#;
        assert!(parse_session(log).task_results.is_empty());
    }

    fn db_with(pool: PgPool) -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        (Db::new(pool), dir)
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn ingest_writes_run_candidates_and_ledger(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        // A scope row to reference (FK on runs.scope), created off a tracked issue.
        crate::issues::store::upsert_issue(
            db.pool(),
            &NewIssue {
                key: "o/r#1".into(),
                repo: "o/r".into(),
                priority: 0,
                evidence_url: None,
                title: None,
                author: None,
                body: None,
                labels: Vec::new(),
                upstream_updated_at: None,
            },
        )
        .await?;
        assert!(
            crate::issues::store::claim_issue(db.pool(), "o/r#1", Status::New, Status::Scoped)
                .await?
        );
        let scope_id = crate::issues::store::insert_scope(
            db.pool(),
            &crate::issues::model::NewScope {
                issue: "o/r#1".into(),
                pack_digest: Some("v1:cafef00d".into()),
                check_outcome: Some("PASS".into()),
            },
        )
        .await?;

        let parsed = ingest_session(
            &db,
            &IngestTarget {
                run_id: "run-xyz",
                scope_id: Some(scope_id),
                issue: None,
                pod: Some("loop-xyz"),
                session_uri: "s3://bucket/run-xyz/session.jsonl",
            },
            &sample_log(),
        )
        .await?;
        assert_eq!(parsed.candidates.len(), 4);

        let run = sqlx::query!(
            r#"SELECT status, best_score, cost_usd, identity_digest, pod, scope FROM runs WHERE run_id = 'run-xyz'"#
        )
        .fetch_one(db.pool())
        .await?;
        assert_eq!(run.status, "finished");
        assert_eq!(run.best_score, Some(260.0));
        assert_eq!(run.cost_usd, Some(2.5));
        assert_eq!(run.identity_digest.as_deref(), Some("v1:cafef00d"));
        assert_eq!(run.pod.as_deref(), Some("loop-xyz"));
        assert_eq!(run.scope, Some(scope_id));

        let cand_count = sqlx::query!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM candidates WHERE run_id = 'run-xyz'"#
        )
        .fetch_one(db.pool())
        .await?;
        assert_eq!(cand_count.n, 4);

        let today = jiff::Timestamp::now().strftime("%Y-%m-%d").to_string();
        // The run cost lands as a `run` ledger row (kept out of the scope-count cap).
        let run_ledger = sqlx::query!(
            r#"SELECT COALESCE(SUM(cost_usd),0.0) AS "t!: f64" FROM ledger WHERE kind='run' AND run_id='run-xyz'"#
        )
        .fetch_one(db.pool())
        .await?;
        assert!((run_ledger.t - 2.5).abs() < 1e-9);
        assert_eq!(
            crate::issues::store::count_scopes_on_day(db.pool(), &today).await?,
            0,
            "a run isn't a scope"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn adopt_ingests_a_foreign_run_with_no_scope(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, sample_log())?;

        let out =
            adopt_session_file(&db, "adopted-1", None, &path, "s3://b/x/session.jsonl").await?;
        assert_eq!(out.status, "finished");
        assert_eq!(out.best_score, Some(260.0));
        assert_eq!(out.cost_usd, Some(2.5));
        assert_eq!(out.candidates, 4);
        assert!(!out.ledger_skipped);

        let run = sqlx::query!(
            r#"SELECT scope, session_uri, status FROM runs WHERE run_id = 'adopted-1'"#
        )
        .fetch_one(db.pool())
        .await?;
        assert_eq!(run.scope, None, "no placeholder issue is fabricated");
        assert_eq!(
            run.session_uri.as_deref(),
            Some("s3://b/x/session.jsonl"),
            "the stored uri is the original, never the local temp path"
        );
        assert_eq!(run.status, "finished");

        let ledger = sqlx::query!(
            r#"SELECT COALESCE(SUM(cost_usd),0.0) AS "t!: f64", COUNT(*) AS "n!: i64"
               FROM ledger WHERE kind='run' AND run_id='adopted-1'"#
        )
        .fetch_one(db.pool())
        .await?;
        assert_eq!(ledger.n, 1);
        assert!((ledger.t - 2.5).abs() < 1e-9);
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn re_adopt_updates_in_place_without_duplicates(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, sample_log())?;

        let first = adopt_session_file(&db, "adopted-2", None, &path, "s3://b/y").await?;
        assert!(!first.ledger_skipped);
        let second = adopt_session_file(&db, "adopted-2", None, &path, "s3://b/y").await?;
        assert!(second.ledger_skipped, "cost is booked exactly once");

        let runs =
            sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM runs WHERE run_id = 'adopted-2'"#)
                .fetch_one(db.pool())
                .await?;
        assert_eq!(runs.n, 1);
        let cands = sqlx::query!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM candidates WHERE run_id = 'adopted-2'"#
        )
        .fetch_one(db.pool())
        .await?;
        assert_eq!(cands.n, 4, "candidates replaced, not appended");
        let ledger = sqlx::query!(
            r#"SELECT COALESCE(SUM(cost_usd),0.0) AS "t!: f64"
               FROM ledger WHERE kind='run' AND run_id='adopted-2'"#
        )
        .fetch_one(db.pool())
        .await?;
        assert!((ledger.t - 2.5).abs() < 1e-9, "no double-charge");
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn adopt_rejects_a_file_that_is_not_a_session_log(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("session.jsonl");
        // What a fetch against a bad url actually hands back: every line unrecognizable.
        std::fs::write(&path, "<html><body>403 Forbidden</body></html>\n")?;

        let err = adopt_session_file(&db, "adopted-junk", None, &path, "s3://b/bad/session.jsonl")
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("s3://b/bad/session.jsonl"),
            "names the uri: {msg}"
        );
        assert!(
            msg.contains("no session events"),
            "says what was wrong: {msg}"
        );

        let runs =
            sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM runs WHERE run_id = 'adopted-junk'"#)
                .fetch_one(db.pool())
                .await?;
        assert_eq!(runs.n, 0, "no incomplete placeholder row is written");
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_rejected_re_adopt_leaves_the_previous_fold_intact(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        let dir = tempfile::tempdir()?;
        let good = dir.path().join("session.jsonl");
        std::fs::write(&good, sample_log())?;
        adopt_session_file(&db, "adopted-keep", None, &good, "s3://b/y").await?;

        // Garbled re-adopt: rejected before anything is deleted.
        let junk = dir.path().join("junk.jsonl");
        std::fs::write(&junk, "not json at all\n")?;
        assert!(
            adopt_session_file(&db, "adopted-keep", None, &junk, "s3://b/y")
                .await
                .is_err()
        );

        // Mid-fold failure: a scope id with no row trips the FK on the runs upsert, after the
        // candidate delete has already run inside the transaction.
        let err = adopt_session_file(&db, "adopted-keep", Some(4242), &good, "s3://b/y")
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").to_uppercase().contains("FOREIGN KEY"),
            "the failure must land mid-transaction, not before it: {err:#}"
        );

        let run =
            sqlx::query!(r#"SELECT status, best_score FROM runs WHERE run_id = 'adopted-keep'"#)
                .fetch_one(db.pool())
                .await?;
        assert_eq!(run.status, "finished", "the first fold's row survives");
        assert_eq!(run.best_score, Some(260.0));
        let cands = sqlx::query!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM candidates WHERE run_id = 'adopted-keep'"#
        )
        .fetch_one(db.pool())
        .await?;
        assert_eq!(
            cands.n, 4,
            "candidates were not destroyed by the failed re-adopt"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn adopt_refuses_a_live_dispatched_run(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, sample_log())?;

        for (run_id, status, pod) in [
            ("dispatched-running", "running", None),
            (
                "dispatched-podded",
                "finished",
                Some("loop-abc".to_string()),
            ),
        ] {
            crate::runs::store::insert_run(
                db.pool(),
                &NewRun {
                    run_id: run_id.to_string(),
                    scope: None,
                    issue: None,
                    identity_digest: None,
                    status: status.to_string(),
                    pod,
                    session_uri: None,
                    best_score: None,
                    cost_usd: None,
                },
            )
            .await?;
            let err = adopt_session_file(&db, run_id, None, &path, "s3://b/y")
                .await
                .unwrap_err();
            assert!(
                format!("{err:#}").contains("controller-dispatched and live"),
                "{run_id}: {err:#}"
            );
            let run = sqlx::query!(
                r#"SELECT status, session_uri FROM runs WHERE run_id = $1"#,
                run_id
            )
            .fetch_one(db.pool())
            .await?;
            assert_eq!(run.status, status, "the dispatched row is untouched");
            assert!(run.session_uri.is_none());
        }
        Ok(())
    }

    #[test]
    fn absolute_session_uri_anchors_local_paths_only() {
        assert_eq!(
            absolute_session_uri("s3://b/run/session.jsonl"),
            "s3://b/run/session.jsonl"
        );
        // A path this host can't see still gets anchored, to the operator's cwd.
        let anchored = absolute_session_uri("no/such/session.jsonl");
        assert!(
            Path::new(&anchored).is_absolute(),
            "relative path anchored: {anchored}"
        );
        assert!(anchored.ends_with("no/such/session.jsonl"));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn adopt_stores_a_relative_local_uri_as_an_absolute_path(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        // Relative to the test process's cwd, which is what the operator would type.
        let file = tempfile::Builder::new()
            .prefix("adopt-rel-")
            .suffix(".jsonl")
            .tempfile_in(std::env::current_dir()?)?;
        std::fs::write(file.path(), sample_log())?;
        let name = file
            .path()
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .into_owned();

        adopt_session_file(&db, "adopted-rel", None, &PathBuf::from(&name), &name).await?;

        let run = sqlx::query!(r#"SELECT session_uri FROM runs WHERE run_id = 'adopted-rel'"#)
            .fetch_one(db.pool())
            .await?;
        let stored = run.session_uri.expect("the stored uri");
        assert!(
            Path::new(&stored).is_absolute(),
            "a relative uri would resolve against the server's cwd: {stored}"
        );
        assert!(stored.ends_with(&name));
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn adopt_attaches_latest_scope_when_issue_given(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        crate::issues::store::upsert_issue(
            db.pool(),
            &NewIssue {
                key: "o/r#9".into(),
                repo: "o/r".into(),
                priority: 0,
                evidence_url: None,
                title: None,
                author: None,
                body: None,
                labels: Vec::new(),
                upstream_updated_at: None,
            },
        )
        .await?;
        assert!(
            crate::issues::store::claim_issue(db.pool(), "o/r#9", Status::New, Status::Scoped)
                .await?
        );
        let scope_id = crate::issues::store::insert_scope(
            db.pool(),
            &crate::issues::model::NewScope {
                issue: "o/r#9".into(),
                pack_digest: None,
                check_outcome: None,
            },
        )
        .await?;
        assert_eq!(adopt_scope_for_issue(&db, "o/r#9").await?, scope_id);

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, single_repo_kept_log())?;
        adopt_session_file(&db, "adopted-3", Some(scope_id), &path, "s3://b/z").await?;

        let (issue, repo) = crate::runs::store::run_issue_repo(db.pool(), "adopted-3").await?;
        assert_eq!(issue.as_deref(), Some("o/r#9"));
        assert_eq!(repo.as_deref(), Some("o/r"));
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn adopt_errors_when_issue_has_no_scope(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        crate::issues::store::upsert_issue(
            db.pool(),
            &NewIssue {
                key: "o/r#10".into(),
                repo: "o/r".into(),
                priority: 0,
                evidence_url: None,
                title: None,
                author: None,
                body: None,
                labels: Vec::new(),
                upstream_updated_at: None,
            },
        )
        .await?;
        let err = adopt_scope_for_issue(&db, "o/r#10").await.unwrap_err();
        assert!(
            format!("{err:#}").contains("no scope to attach"),
            "tells the operator to omit --issue: {err:#}"
        );
        Ok(())
    }
}
