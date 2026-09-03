//! The session log wire format, the single source of truth a decoupled run emits.
//!
//! The headless loop appends one [`SessionEvent`] per NDJSON line to `state/session.jsonl`;
//! session viewers read it back. Each line is a versioned envelope: `{"v":1,"kind":"...",...}`.
//! This is the wire-only half of the format: plain-data types and the codec, with no dependency
//! on the CLI's in-process `Row`/`Phase` state (that bridge lives in `crucible::session`).

use crate::event::AgentEvent;
use crate::identity::RunIdentity;
use serde::{Deserialize, Serialize};

/// Bump only on a breaking change to the on-disk shape.
pub const WIRE_VERSION: u8 = 1;

/// How one declared grade-evidence task ended up. A lossy grade (`join = "passed"`)
/// folds only passing dependencies, so the folded score alone cannot say which
/// declared rungs never contributed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceDisposition {
    /// Ran and passed; its output was folded into the grade.
    Passed,
    /// Ran and failed its check.
    Failed,
    /// Never produced evidence: skipped, blocked, or dead on transport.
    Skipped,
}

/// One declared grade-evidence task with its disposition, recorded per graded row so a
/// candidate that "passed the ladder" is distinguishable from one that passed the rungs
/// that happened to run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceEntry {
    pub task: String,
    pub disposition: EvidenceDisposition,
    /// The terminal note for a failed/skipped task (why it did not contribute).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

impl std::fmt::Display for EvidenceEntry {
    /// Compact human form: `refcheck ✓`, `calc-diff ✗ (why)`, `tensor-pipe SKIPPED (why)`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let marker = match self.disposition {
            EvidenceDisposition::Passed => "✓",
            EvidenceDisposition::Failed => "✗",
            EvidenceDisposition::Skipped => "SKIPPED",
        };
        write!(f, "{} {marker}", self.task)?;
        if !self.note.is_empty() {
            // Bounded like candidate notes: transport errors can run to whole stack traces.
            let note: String = self.note.chars().take(120).collect();
            write!(f, " ({note})")?;
        }
        Ok(())
    }
}

/// A plain-serde mirror of `Row`, so `Row`'s fields can churn without touching the
/// on-disk contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowWire {
    pub iter: u32,
    pub decision: String,
    pub note: String,
    pub detail: String,
    #[serde(default)]
    pub diff: String,
    #[serde(default)]
    pub diffstat: String,
    #[serde(default)]
    pub score: Option<f64>,
    /// Secondary scalar for functional gates: breaks primary-score ties in the keep rule.
    /// Absent on rows without one and on logs written before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiebreak: Option<f64>,
    #[serde(default)]
    pub total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// The World snapshot token committed when this row was kept (a git world packs the commit
    /// sha). A resume restores the kept-best tree from it, so the logged best score is never
    /// paired with a tree it did not measure. Absent on non-keep rows and on older logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept_snap: Option<String>,
    /// The grade step's declared evidence set with per-task dispositions. Empty on
    /// ungraded rows and on logs written before the field existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceEntry>,
    /// The agent's whole CANDIDATE.md for this iteration (`note` is its first line-folded
    /// 120 chars, table-sized). Carried in full so the PR body can print the actual
    /// writeup instead of a mid-word truncation. Empty when the agent wrote none, and on
    /// logs written before the field existed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub candidate_md: String,
}

/// One draft PR publish-on-keep opened, on the wire so the controller's pull-ingest can fold it
/// onto the kept candidate rows' `pr_url`. A single-repo run emits one link with `name` empty; a
/// composite run emits one per touched component. Mirrors `crucible::publish::PrLink` but adds
/// `Deserialize`, for the same reason [`RowWire`] mirrors `Row`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrLinkWire {
    pub url: String,
    pub repo: String,
    /// Component name for a composite run; empty for a single-repo run.
    #[serde(default)]
    pub name: String,
    /// The per-candidate head branch the PR was opened from (`autoresearch/<run_id>/<candidate>`).
    /// `default` for logs written before this field existed.
    #[serde(default)]
    pub branch: String,
}

/// A validated plan task emitted for visualization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanTaskWire {
    pub name: String,
    /// Task kind label: `agent`, `command`, or a reducer name like `top_k`.
    pub kind: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Logical durable agent session; empty means a fresh turn.
    #[serde(default)]
    pub session: String,
    #[serde(default)]
    pub needs: String,
    #[serde(default)]
    pub required: bool,
    /// The task preserves only its declared files after a measured failure.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub capture_on_failure: bool,
    /// What the task needs of its dependencies: `all` or `passed`.
    #[serde(default)]
    pub join: String,
    /// `iteration` or `epilogue`. A consumer computing a verdict must skip the epilogue.
    #[serde(default)]
    pub stage: String,
    /// `producer.field` when this task runs once per element of an upstream list, empty
    /// otherwise. The node is one node however many instances it produces, so a renderer draws
    /// the graph from this without waiting to see how wide it gets.
    #[serde(default)]
    pub over: String,
    /// The most instances `over` may produce; 0 when the task is not mapped. A reader knows the
    /// worst-case width before any spend.
    #[serde(default)]
    pub max_fanout: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAction {
    Started,
    Resumed,
}

impl std::fmt::Display for SessionAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SessionAction::Started => "started",
            SessionAction::Resumed => "resumed",
        })
    }
}

/// Why a resumed run believed its predecessor stopped. Emitted once per resume as a
/// [`SessionEvent::Recovery`] event so operators see the classification on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryClass {
    CleanExit,
    DiedInBaseline,
    DiedInWideRound,
    DiedMidTurn,
    DiedDeciding,
    DiedInPlanTask,
    DiedAwaitingApproval,
    DiedBetweenIterations,
}

impl std::fmt::Display for RecoveryClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RecoveryClass::CleanExit => "clean_exit",
            RecoveryClass::DiedInBaseline => "died_in_baseline",
            RecoveryClass::DiedInWideRound => "died_in_wide_round",
            RecoveryClass::DiedMidTurn => "died_mid_turn",
            RecoveryClass::DiedDeciding => "died_deciding",
            RecoveryClass::DiedInPlanTask => "died_in_plan_task",
            RecoveryClass::DiedAwaitingApproval => "died_awaiting_approval",
            RecoveryClass::DiedBetweenIterations => "died_between_iterations",
        })
    }
}

/// One event in the session log. Mirrors the `crucible::reporter::Reporter` calls so the
/// viewer can rebuild identical state by folding the sequence (the same fold `App` does
/// over `UiMsg`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    Start {
        goal: String,
        /// Gate as its stable wire token (`bench`/`test`), via `Gate::as_str`.
        gate: String,
        model: String,
        namespace: String,
        iters_total: u32,
        max_cost: f64,
        max_secs: u64,
    },
    /// `phase` is `preflight`, `baseline`, or `iteration`; `iter` is meaningful only for the last.
    Phase {
        phase: String,
        #[serde(default)]
        iter: u32,
    },
    Note {
        msg: String,
    },
    Row {
        row: RowWire,
        solved: bool,
    },
    AgentStart {
        iter: u32,
    },
    /// A logical agent session is about to start or resume. The provider cursor and private
    /// reasoning are intentionally absent from the public event stream.
    AgentSession {
        session: String,
        action: SessionAction,
        /// Number of successfully completed turns before this one.
        turn: u32,
    },
    /// One agent event, nested so its own `kind` tag doesn't collide with ours.
    Agent {
        event: AgentEvent,
    },
    AgentDone,
    Budget {
        spent: f64,
        elapsed_secs: u64,
    },
    Summary {
        rows: Vec<RowWire>,
        gate: String,
        /// `None` when the run never had a finite best (a task run, or a skipped baseline that
        /// kept nothing). A plain `f64` here made those lines undecodable: the non-finite
        /// sentinel serializes to JSON `null`, which fails `f64` deserialization and silently
        /// drops the whole event.
        #[serde(default)]
        best_score: Option<f64>,
    },
    /// The agent declared the harness inadequate and the run halted for human review. Mirrors
    /// `crucible::escalation::Escalation`; carried on the wire so the record shows *why* a run
    /// stopped short.
    Escalation {
        category: String,
        reason: String,
        #[serde(default)]
        evidence: String,
    },
    /// A (re-)scope boundary: a fresh baseline and fingerprint start a new comparable segment
    /// within the run. Scores before and after the boundary are NOT comparable.
    Segment {
        fingerprint: String,
        /// `None` when the baseline was skipped (task runs always; codegen domains until a
        /// preflight seeds one). See [`SessionEvent::Summary::best_score`] for why this is
        /// not a plain `f64`.
        #[serde(default)]
        baseline_score: Option<f64>,
        regime: String,
    },
    /// The run's comparability key, computed once at setup and emitted at run start; emitted again
    /// on `--resume` so a mismatch against the original run is visible on the wire.
    Identity {
        identity: RunIdentity,
    },
    /// The draft PR(s) publish-on-keep opened for this run, emitted once after publish
    /// (best-effort, a failed append must not fail the run). The controller's pull-ingest reads
    /// it to populate `candidates.pr_url` on kept rows.
    PrLinks {
        links: Vec<PrLinkWire>,
    },
    Finished,
    /// A work graph admitted for execution.
    PlanAdmitted {
        plan_version: u32,
        #[serde(default)]
        reason: String,
        budget_usd: f64,
        tasks: Vec<PlanTaskWire>,
    },
    /// What a task proposed for a receiving orchestrator to admit. Emitted once per task that
    /// emitted any, as it settles, so what a run proposed is auditable independently of what was
    /// admitted. The emitting run never dispatches these.
    AsksEmitted {
        task: String,
        asks: Vec<crate::ask::Ask>,
    },
    /// One work-graph task attempt reached a terminal status. Covers both the plan executor
    /// (`plan_version` + `kind` + `output`) and measure-DAG walks (`iter` + `digest` + `job` +
    /// `metric`); fields outside the emitting context default to empty. `status` is one of
    /// `pass`/`fail`/`transport`/`skipped`/`blocked`/`truncated`. `trace_id`/`span_id`
    /// cross-link the ledger row to the task's OTLP span in either direction.
    TaskResult {
        task: String,
        status: String,
        #[serde(default)]
        plan_version: u32,
        /// Task kind label (`agent`/`command`/`top_k`); named to dodge the envelope's `kind` tag.
        #[serde(default)]
        task_kind: String,
        #[serde(default)]
        iter: u32,
        #[serde(default)]
        digest: String,
        #[serde(default)]
        job: String,
        #[serde(default)]
        attempts: u32,
        #[serde(default)]
        cost_usd: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metric: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<serde_json::Value>,
        #[serde(default)]
        note: String,
        #[serde(default)]
        secs: f64,
        #[serde(default)]
        trace_id: String,
        #[serde(default)]
        span_id: String,
    },
    /// The loop is exiting, emitted exactly once as the LAST line of the session log. `outcome` is
    /// one of `finished`/`solved`/`budget`/`stopped`/`escalated`/`stalled`/`error`. The viewer keys
    /// its terminal state off this line: a dead stream with no `Shutdown` line means the pod died
    /// mid-run rather than exiting cleanly.
    Shutdown {
        outcome: String,
        reason: String,
    },
    /// The loop began waiting on a mediated-provisioning approval. A dangling ApprovalWait
    /// means the run died or stopped with the approval outstanding; resume re-parks a
    /// block-mode one.
    ApprovalWait {
        handle: String,
        #[serde(default)]
        trace_id: String,
        mode: String,
    },
    /// The wait above reached a terminal outcome: `granted`/`denied`/`timeout`. Deliberately
    /// NOT emitted on a stop-while-parked (a stop doesn't resolve the ask), so a resumed run
    /// re-parks on the still-open approval.
    ApprovalResolved {
        outcome: String,
        #[serde(default)]
        reason: String,
    },
    /// A mediated write refused by the run's declared output bounds. The refusal fails the
    /// requesting tool call; it never terminates the run.
    OutputRefused {
        /// The output kind as its `OutputKind` wire token; named to dodge the envelope's `kind` tag.
        output_kind: String,
        /// The violated bound, e.g. `[outputs.tracker-comment].count = 2`.
        bound: String,
        /// The broker tool the refused call came in on.
        #[serde(default)]
        tool: String,
        /// The refusal text handed back to the agent.
        #[serde(default)]
        detail: String,
    },
    /// How this resume classified the previous shutdown. Purely a record: the loop's
    /// behavior is driven by the in-process recovery plan, not by re-reading this.
    Recovery {
        class: RecoveryClass,
        /// The iteration the interruption touched; 0 when not iteration-scoped.
        #[serde(default)]
        iter: u32,
        /// Human-readable evidence summary (dangling-turn stats, plan gap, approval handle).
        #[serde(default)]
        detail: String,
    },
}

/// Borrowing envelope so encoding doesn't clone the event.
#[derive(Serialize)]
struct EnvelopeRef<'a> {
    v: u8,
    #[serde(flatten)]
    event: &'a SessionEvent,
}

/// Owned envelope for decoding; `v` defaults so a missing version still loads.
#[derive(Deserialize)]
struct Envelope {
    #[serde(default = "default_version")]
    #[allow(dead_code)] // read for forward-compat; we accept any v for now
    v: u8,
    #[serde(flatten)]
    event: SessionEvent,
}

fn default_version() -> u8 {
    WIRE_VERSION
}

/// Encode one event as a single NDJSON line (no trailing newline). Infallible for our
/// own types; on the impossible serde error we emit a valid `note` line rather than
/// corrupt the log.
pub fn encode(ev: &SessionEvent) -> String {
    serde_json::to_string(&EnvelopeRef {
        v: WIRE_VERSION,
        event: ev,
    })
    .unwrap_or_else(|e| {
        format!(r#"{{"v":{WIRE_VERSION},"kind":"note","msg":"encode error: {e}"}}"#)
    })
}

/// Decode one NDJSON line. Returns `None` for blank or torn/partial lines so a tailing
/// reader can skip them.
pub fn decode(line: &str) -> Option<SessionEvent> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    crate::json::from_str::<Envelope>(t).ok().map(|e| e.event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{ModelUsage, RawStream, Tokens};
    use crate::identity::{ComponentIdentity, RigIdentity};

    /// A task run (and a skipped-baseline codegen run) has no finite best/baseline; those
    /// lines carry `null` and must still decode instead of being silently dropped.
    #[test]
    fn null_summary_and_segment_scores_decode() {
        let summary =
            decode(r#"{"v":1,"kind":"summary","rows":[],"gate":"task","best_score":null}"#)
                .expect("summary with null best_score decodes");
        match summary {
            SessionEvent::Summary { best_score, .. } => assert_eq!(best_score, None),
            other => panic!("wrong event: {other:?}"),
        }
        let segment =
            decode(r#"{"v":1,"kind":"segment","fingerprint":"f","baseline_score":null,"regime":"default"}"#)
                .expect("segment with null baseline_score decodes");
        match segment {
            SessionEvent::Segment { baseline_score, .. } => assert_eq!(baseline_score, None),
            other => panic!("wrong event: {other:?}"),
        }
    }

    /// Encode → decode → encode and prove the two encodings match. This sidesteps
    /// needing `PartialEq` on `AgentEvent`/`f64` while still proving a lossless trip.
    fn assert_round_trips(ev: SessionEvent) {
        let line = encode(&ev);
        assert!(line.contains(r#""v":1"#), "missing version in {line}");
        let decoded = decode(&line).unwrap_or_else(|| panic!("decode failed: {line}"));
        let reencoded = encode(&decoded);
        assert_eq!(line, reencoded, "round-trip changed the encoding");
    }

    #[test]
    fn every_variant_round_trips() {
        let row = RowWire {
            iter: 1,
            decision: "keep".into(),
            note: "p99=210 ms".into(),
            detail: "cache_hit=0.83".into(),
            diff: "diff --git a/p.go b/p.go\n@@ -1 +1 @@\n-old\n+new\n".into(),
            diffstat: "1 file changed, 1 insertion(+), 1 deletion(-)".into(),
            score: Some(210.0),
            tiebreak: Some(0.5),
            total: None,
            phase: None,
            kept_snap: Some("abc123".into()),
            evidence: vec![
                EvidenceEntry {
                    task: "refcheck".into(),
                    disposition: EvidenceDisposition::Passed,
                    note: String::new(),
                },
                EvidenceEntry {
                    task: "tensor-pipe".into(),
                    disposition: EvidenceDisposition::Skipped,
                    note: "worktree setup failed".into(),
                },
            ],
            candidate_md: "# Candidate: full writeup\n\nbody".into(),
        };
        for ev in [
            SessionEvent::Start {
                goal: "Lower p99 latency.".into(),
                gate: "bench".into(),
                model: "claude-opus-4-6".into(),
                namespace: "llm-d-pc-mock".into(),
                iters_total: 3,
                max_cost: 5.0,
                max_secs: 1800,
            },
            SessionEvent::Phase {
                phase: "preflight".into(),
                iter: 0,
            },
            SessionEvent::Phase {
                phase: "baseline".into(),
                iter: 0,
            },
            SessionEvent::Phase {
                phase: "iteration".into(),
                iter: 2,
            },
            SessionEvent::Note {
                msg: "injected operator steer".into(),
            },
            SessionEvent::Row {
                row: row.clone(),
                solved: false,
            },
            SessionEvent::AgentStart { iter: 1 },
            SessionEvent::AgentSession {
                session: "solver".into(),
                action: SessionAction::Resumed,
                turn: 2,
            },
            SessionEvent::AgentDone,
            SessionEvent::Budget {
                spent: 1.23,
                elapsed_secs: 372,
            },
            SessionEvent::Summary {
                rows: vec![row.clone()],
                gate: "test".into(),
                best_score: Some(210.0),
            },
            SessionEvent::Escalation {
                category: "harness-limitation".into(),
                reason: "tokenload reads 0 for all endpoints across 3 configs".into(),
                evidence: "inspect-rig: all pods tokenLoad=0".into(),
            },
            SessionEvent::Segment {
                fingerprint: "a1b2c3d4e5f60718".into(),
                baseline_score: Some(277.0),
                regime: "concurrency=48".into(),
            },
            SessionEvent::Identity {
                identity: RunIdentity {
                    components: vec![ComponentIdentity {
                        name: "router".into(),
                        repo: "https://github.com/example/router".into(),
                        base_sha: "deadbeef".into(),
                    }],
                    manifest_hash: "1111111111111111".into(),
                    inject_hash: "2222222222222222".into(),
                    seed_hash: String::new(),
                    measure_cmd: "./measure.sh".into(),
                    direction: "lower".into(),
                    rig: RigIdentity::default(),
                    digest: "v2:3333333333333333".into(),
                },
            },
            SessionEvent::PrLinks {
                links: vec![
                    PrLinkWire {
                        url: "https://github.com/wseaton/vllm/pull/1".into(),
                        repo: "wseaton/vllm".into(),
                        name: "vllm".into(),
                        branch: "autoresearch/run-1/0".into(),
                    },
                    PrLinkWire {
                        url: "https://github.com/wseaton/llm-d-router/pull/2".into(),
                        repo: "wseaton/llm-d-router".into(),
                        name: String::new(),
                        branch: "autoresearch/run-1/0".into(),
                    },
                ],
            },
            SessionEvent::Finished,
            SessionEvent::Shutdown {
                outcome: "finished".into(),
                reason: "all iterations completed".into(),
            },
            SessionEvent::ApprovalWait {
                handle: "https://github.com/wseaton/llm-d-router/pull/7".into(),
                trace_id: "model=Qwen/Qwen3-0.6B;c=48".into(),
                mode: "block".into(),
            },
            SessionEvent::ApprovalResolved {
                outcome: "granted".into(),
                reason: "concurrency=48".into(),
            },
            SessionEvent::Recovery {
                class: RecoveryClass::DiedMidTurn,
                iter: 4,
                detail: "turn in flight at iter 4, 132 agent events".into(),
            },
        ] {
            assert_round_trips(ev);
        }
    }

    #[test]
    fn recovery_class_tokens_are_snake_case_and_display_matches_serde() {
        for (class, token) in [
            (RecoveryClass::CleanExit, "clean_exit"),
            (RecoveryClass::DiedInBaseline, "died_in_baseline"),
            (RecoveryClass::DiedInWideRound, "died_in_wide_round"),
            (RecoveryClass::DiedMidTurn, "died_mid_turn"),
            (RecoveryClass::DiedDeciding, "died_deciding"),
            (RecoveryClass::DiedInPlanTask, "died_in_plan_task"),
            (
                RecoveryClass::DiedAwaitingApproval,
                "died_awaiting_approval",
            ),
            (
                RecoveryClass::DiedBetweenIterations,
                "died_between_iterations",
            ),
        ] {
            let line = encode(&SessionEvent::Recovery {
                class,
                iter: 0,
                detail: String::new(),
            });
            assert!(line.contains(&format!(r#""class":"{token}""#)), "{line}");
            assert_eq!(class.to_string(), token);
        }
    }

    #[test]
    fn approval_wait_minimal_line_decodes_with_defaults() {
        let ev = decode(r#"{"v":1,"kind":"approval_wait","handle":"h","mode":"continue"}"#)
            .expect("minimal approval_wait decodes");
        match ev {
            SessionEvent::ApprovalWait {
                handle,
                trace_id,
                mode,
            } => {
                assert_eq!(handle, "h");
                assert_eq!(trace_id, "");
                assert_eq!(mode, "continue");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn agent_events_round_trip_including_raw() {
        for event in [
            AgentEvent::Text {
                delta: "moving the read out of the critical section".into(),
            },
            AgentEvent::Tokens(Tokens {
                input: 1200,
                output: 340,
                cache_read: 88_000,
                cache_write: 0,
                total: 89_540,
                rate: Some(318.4),
                cost_usd: Some(0.84),
            }),
            AgentEvent::OtelSummary {
                cost_usd: 0.84,
                total: 89_540,
                models: vec![ModelUsage {
                    model: "claude-opus-4-6".into(),
                    input: 1200,
                    output: 340,
                    cache_read: 88_000,
                    cache_write: 0,
                    cost_usd: 0.84,
                }],
                api_requests: 12,
                api_ms: 80_110.0,
                active_seconds: 372.0,
            },
            // Raw stderr/scraped lines must survive the trip to the viewer.
            AgentEvent::Raw {
                text: "a stderr line".into(),
                stream: RawStream::Stderr,
            },
        ] {
            assert_round_trips(SessionEvent::Agent { event });
        }
    }

    #[test]
    fn plan_admitted_round_trips() {
        assert_round_trips(SessionEvent::PlanAdmitted {
            plan_version: 2,
            reason: "verifier refuted candidate".into(),
            budget_usd: 5.0,
            tasks: vec![PlanTaskWire {
                name: "propose-a".into(),
                kind: "agent".into(),
                depends_on: vec![],
                session: "solver".into(),
                needs: "any".into(),
                required: true,
                capture_on_failure: true,
                join: "all".into(),
                stage: "iteration".into(),
                over: "discover.targets".into(),
                max_fanout: 8,
            }],
        });
    }

    #[test]
    fn old_plan_admitted_tasks_default_failure_capture_to_false() {
        let event = decode(
            r#"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":[{"name":"probe","kind":"evaluate"}]}"#,
        )
        .expect("old plan_admitted event decodes");
        let SessionEvent::PlanAdmitted { tasks, .. } = event else {
            panic!("wrong event kind")
        };
        assert!(!tasks[0].capture_on_failure);
    }

    /// Asks reach the log as their emitting task settles, so an auditor can compare what a run
    /// proposed against what an orchestrator actually admitted.
    #[test]
    fn asks_emitted_round_trips() {
        assert_round_trips(SessionEvent::AsksEmitted {
            task: "classify".into(),
            asks: vec![
                crate::ask::Ask::new(
                    crate::ask::AskKey::new("arxiv.org/abs/2401.12345").expect("valid key"),
                    "implement-paper",
                    serde_json::json!({"paper_url": "https://arxiv.org/abs/2401.12345"}),
                )
                .expect("valid ask"),
            ],
        });
    }

    /// A hand-written line carrying a key the constructor would refuse must not decode. Without
    /// this the wire would be a way around the key rules rather than the place they are enforced.
    #[test]
    fn an_ask_with_an_unusable_key_does_not_decode() {
        let line = r#"{"v":1,"kind":"asks_emitted","task":"classify","asks":[{"key":"with space","workflow":"w","params":null}]}"#;
        assert!(decode(line).is_none(), "a bad key decoded");
    }

    #[test]
    fn task_result_round_trips_with_output_and_defaults() {
        assert_round_trips(SessionEvent::TaskResult {
            task: "measure-a".into(),
            status: "pass".into(),
            plan_version: 1,
            task_kind: "command".into(),
            iter: 0,
            digest: String::new(),
            job: String::new(),
            attempts: 1,
            cost_usd: 0.3,
            metric: None,
            output: Some(serde_json::json!({"score": 234.0})),
            note: String::new(),
            secs: 0.0,
            trace_id: String::new(),
            span_id: String::new(),
        });
        // A minimal line (old writers, other contexts) still decodes: every field but
        // task/status defaults.
        let ev = decode(r#"{"v":1,"kind":"task_result","task":"t","status":"fail"}"#)
            .expect("minimal task_result decodes");
        match ev {
            SessionEvent::TaskResult {
                task,
                status,
                attempts,
                output,
                ..
            } => {
                assert_eq!(task, "t");
                assert_eq!(status, "fail");
                assert_eq!(attempts, 0);
                assert!(output.is_none());
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn evidence_entry_renders_compactly_and_old_rows_decode_without_it() {
        let passed = EvidenceEntry {
            task: "refcheck".into(),
            disposition: EvidenceDisposition::Passed,
            note: String::new(),
        };
        assert_eq!(passed.to_string(), "refcheck ✓");
        let skipped = EvidenceEntry {
            task: "tensor-pipe".into(),
            disposition: EvidenceDisposition::Skipped,
            note: "worktree setup failed".into(),
        };
        assert_eq!(
            skipped.to_string(),
            "tensor-pipe SKIPPED (worktree setup failed)"
        );

        // A pre-evidence row line still decodes, with the field defaulting to empty.
        let ev = decode(
            r#"{"v":1,"kind":"row","row":{"iter":1,"decision":"keep","note":"n","detail":"d"},"solved":false}"#,
        )
        .expect("old row decodes");
        match ev {
            SessionEvent::Row { row, .. } => {
                assert!(row.evidence.is_empty());
                assert!(row.candidate_md.is_empty());
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn blank_and_torn_lines_decode_to_none() {
        assert!(decode("").is_none());
        assert!(decode("   ").is_none());
        assert!(decode(r#"{"v":1,"kind":"age"#).is_none());
    }
}
