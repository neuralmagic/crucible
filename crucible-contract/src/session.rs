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
    #[serde(default)]
    pub total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
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
    #[serde(default)]
    pub needs: String,
    #[serde(default)]
    pub required: bool,
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
    /// `phase` is `baseline` or `iteration`; `iter` is meaningful only for the latter.
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
        best_score: f64,
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
        baseline_score: f64,
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
    /// one of `finished`/`solved`/`budget`/`stopped`/`escalated`/`error`. The viewer keys its
    /// terminal state off this line: a dead stream with no `Shutdown` line means the pod died
    /// mid-run rather than exiting cleanly.
    Shutdown {
        outcome: String,
        reason: String,
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
    serde_json::from_str::<Envelope>(t).ok().map(|e| e.event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{ModelUsage, RawStream, Tokens};
    use crate::identity::{ComponentIdentity, RigIdentity};

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
            total: None,
            phase: None,
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
            SessionEvent::AgentDone,
            SessionEvent::Budget {
                spent: 1.23,
                elapsed_secs: 372,
            },
            SessionEvent::Summary {
                rows: vec![row.clone()],
                gate: "test".into(),
                best_score: 210.0,
            },
            SessionEvent::Escalation {
                category: "harness-limitation".into(),
                reason: "tokenload reads 0 for all endpoints across 3 configs".into(),
                evidence: "inspect-rig: all pods tokenLoad=0".into(),
            },
            SessionEvent::Segment {
                fingerprint: "a1b2c3d4e5f60718".into(),
                baseline_score: 277.0,
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
                    measure_cmd: "./measure.sh".into(),
                    direction: "lower".into(),
                    rig: RigIdentity::default(),
                    digest: "v1:3333333333333333".into(),
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
        ] {
            assert_round_trips(ev);
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
                needs: "any".into(),
                required: true,
            }],
        });
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
    fn blank_and_torn_lines_decode_to_none() {
        assert!(decode("").is_none());
        assert!(decode("   ").is_none());
        assert!(decode(r#"{"v":1,"kind":"age"#).is_none());
    }
}
