//! The broker: ties cache-resolve → judge-impact classify → admission → approval into the one
//! `request_trace` decision. This is the server-side choke point the agent calls; it holds no
//! domain logic of its own beyond the routing; the trace cache, headroom, and approval live
//! behind the three trait boundaries.

use crate::admission::{Admission, AdmissionDecision, GpuNeed};
use crate::approval::{ApprovalBackend, ApprovalChannel, ApprovalRequest};
use crate::bounds::Bounds;
use crate::types::{JudgeImpact, Resolution, TraceParams, judge_impact, trace_id};
use anyhow::Result;
use crucible_contract::outputs::OutputKind;
use std::sync::Arc;

/// Resolve a trace against the cache (local `traces/` + S3). `Some(knobs)` on a hit, `None` on a
/// miss. The real impl is domain-provided; the broker depends only on this boundary.
pub trait TraceResolver {
    fn resolve(&self, params: &TraceParams) -> Result<Option<serde_json::Value>>;
}

/// An always-miss resolver for domains that don't use the trace cache at all (e.g. a composite deployment that
/// only needs the build/deploy/measure tools). `request_trace` then always reports a miss; those domains
/// simply never call it. Lets the generic broker binary run without a domain-specific cache backend.
pub struct NullResolver;

impl TraceResolver for NullResolver {
    fn resolve(&self, _: &TraceParams) -> Result<Option<serde_json::Value>> {
        Ok(None)
    }
}

/// The mediated-provisioning broker. The three backends (cache resolve, admission, approval) are
/// all boxed trait objects so the concrete impls are a runtime choice the domain binary injects:
/// the resolver is domain-specific, the approval channel is draft-PR vs control-bridge. The
/// engine spawns this binary; it never links it.
pub struct Broker {
    pub(crate) resolver: Box<dyn TraceResolver + Send + Sync>,
    pub(crate) admission: Box<dyn Admission + Send + Sync>,
    pub(crate) approval: Box<dyn ApprovalBackend + Send + Sync>,
    pub(crate) approval_channel: ApprovalChannel,
    /// The regime the harness baseline is frozen at (the judge-impact reference).
    pub(crate) frozen: TraceParams,
    /// Pure-headless with no reachable approval surface: judge-changing misses escalate-halt
    /// instead of opening an approval that no one will see.
    pub(crate) degraded_headless: bool,
    /// The run's output bounds, shared with the tool surface: an approval backend that opens a
    /// draft PR spends the `draft-pr` bound here, at the write.
    pub(crate) bounds: Arc<Bounds>,
}

/// What one `request_trace` produced: the broker's decision, or the bound that refused the
/// approval write it would have performed.
pub(crate) enum TraceOutcome {
    Decided(Resolution),
    Refused(String),
}

impl Broker {
    /// The one tool the agent calls. Never spends GPU or changes the judge directly; it decides
    /// and returns; the caller acts on the [`TraceOutcome`]. The one write it can perform is the
    /// approval draft PR, which spends the run's `draft-pr` bound before it is opened.
    pub(crate) fn request_trace(&self, params: &TraceParams) -> Result<TraceOutcome> {
        let id = trace_id(params);

        // 1. A cache HIT is always GPU-free and judge-neutral: hand back knobs, continue.
        if let Some(knobs) = self.resolver.resolve(params)? {
            return Ok(TraceOutcome::Decided(Resolution::Hit {
                trace_id: id,
                knobs,
            }));
        }

        // 2. MISS: the decision turns on whether granting it would move the goalpost.
        let resolution = match judge_impact(params, &self.frozen) {
            // Judge-changing: never auto-grant. Needs a human re-scope, or escalate-halt when
            // there's no approval surface to reach.
            JudgeImpact::JudgeChanging if self.degraded_headless => Resolution::Escalate {
                trace_id: id,
                reason: "judge-changing request with no approval surface (degraded headless); \
                         a re-scope needs a human"
                    .into(),
            },
            JudgeImpact::JudgeChanging => {
                if let Some(repo) = self.approval.draft_pr_repo()
                    && let Err(detail) =
                        self.bounds
                            .admit("request_trace", OutputKind::DraftPr, Some(repo))
                {
                    return Ok(TraceOutcome::Refused(detail));
                }
                let handle = self.approval.open(&ApprovalRequest {
                    trace_id: id.clone(),
                    summary: format!(
                        "re-scope: capture+baseline at concurrency={} long_frac={} (model {})",
                        params.concurrency, params.long_frac, params.model
                    ),
                    est_gpus: 1,
                })?;
                Resolution::PendingApproval {
                    trace_id: id,
                    approval: handle,
                }
            }
            // In-scope fill (the current frozen regime, just uncalibrated): no judge change, so
            // it only needs GPU headroom, so admission-gate it.
            JudgeImpact::InScope => match self.admission.check(GpuNeed { gpus: 1 })? {
                AdmissionDecision::Admit => {
                    let handle = self.admission.submit(params)?;
                    Resolution::Submitted {
                        trace_id: id,
                        handle,
                    }
                }
                AdmissionDecision::Defer { reason } => Resolution::Deferred {
                    trace_id: id,
                    reason,
                },
            },
        };
        Ok(TraceOutcome::Decided(resolution))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::ApprovalState;
    use crucible_contract::outputs::{
        BoundSource, OUTPUTS_WIRE_VERSION, ResolvedOutput, ResolvedOutputs, ResolvedTarget,
    };
    use std::collections::{BTreeMap, HashSet};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn frozen() -> TraceParams {
        TraceParams {
            model: "claude-opus-4-6".into(),
            concurrency: 12,
            prefixes: 8,
            max_tokens: 8,
            long_tokens: 0,
            long_frac: 0.0,
        }
    }

    /// A real in-memory trace cache (not a mock): it hits for the ids placed in `present`.
    struct MapCache {
        present: HashSet<String>,
    }
    impl TraceResolver for MapCache {
        fn resolve(&self, params: &TraceParams) -> Result<Option<serde_json::Value>> {
            Ok(self
                .present
                .contains(&trace_id(params).0)
                .then(|| serde_json::json!({"MOCK_ITL_MS": 9.0})))
        }
    }

    /// Real arithmetic headroom: admit while free capacity stays above the reserve.
    struct Headroom {
        free: u32,
        reserve: u32,
    }
    impl Admission for Headroom {
        fn check(&self, need: GpuNeed) -> Result<AdmissionDecision> {
            Ok(if self.free >= need.gpus + self.reserve {
                AdmissionDecision::Admit
            } else {
                AdmissionDecision::Defer {
                    reason: format!("only {} GPU(s) free, reserve {}", self.free, self.reserve),
                }
            })
        }
        fn submit(&self, _params: &TraceParams) -> Result<String> {
            Ok("test-capture-job".into())
        }
    }

    /// Counts the opens it served and hands back a fixed handle. `repo` is the draft-PR target it
    /// declares to the bound; `None` is a backend that writes nothing (the control channel).
    struct PrApproval {
        repo: Option<String>,
        opened: Arc<AtomicUsize>,
    }

    impl PrApproval {
        fn on(repo: &str) -> Self {
            Self {
                repo: Some(repo.into()),
                opened: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// A no-write backend: the control channel opens nothing on a forge.
        fn writing_nothing() -> Self {
            Self {
                repo: None,
                opened: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// The open counter, shared with the test after the backend is boxed into the broker.
        fn counter(&self) -> Arc<AtomicUsize> {
            self.opened.clone()
        }
    }

    impl ApprovalBackend for PrApproval {
        fn open(&self, _req: &ApprovalRequest) -> Result<String> {
            self.opened.fetch_add(1, Ordering::SeqCst);
            Ok("pr#1".into())
        }
        fn poll(&self, _handle: &str) -> Result<ApprovalState> {
            Ok(ApprovalState::Pending)
        }
        fn draft_pr_repo(&self) -> Option<&str> {
            self.repo.as_deref()
        }
    }

    /// Draft-PR bounds with `count` writes allowed against one fixed repo.
    fn draft_pr_bounds(count: u32, repo: &str) -> Arc<Bounds> {
        Arc::new(Bounds::new(
            ResolvedOutputs {
                version: OUTPUTS_WIRE_VERSION,
                outputs: vec![ResolvedOutput {
                    kind: OutputKind::DraftPr,
                    count,
                    target: Some(ResolvedTarget::Fixed { fixed: repo.into() }),
                    source: BoundSource::Manifest,
                }],
            },
            BTreeMap::new(),
            None,
        ))
    }

    fn broker(present: &[&str], free: u32, degraded: bool) -> Broker {
        broker_with(
            present,
            free,
            degraded,
            PrApproval::on("owner/repo"),
            draft_pr_bounds(4, "owner/repo"),
        )
    }

    fn broker_with(
        present: &[&str],
        free: u32,
        degraded: bool,
        approval: PrApproval,
        bounds: Arc<Bounds>,
    ) -> Broker {
        Broker {
            resolver: Box::new(MapCache {
                present: present.iter().map(|s| s.to_string()).collect(),
            }),
            admission: Box::new(Headroom { free, reserve: 1 }),
            approval: Box::new(approval),
            approval_channel: ApprovalChannel::DraftPr,
            frozen: frozen(),
            degraded_headless: degraded,
            bounds,
        }
    }

    /// A judge-changing request: hotter than the frozen regime, so the approval path runs.
    fn hotter() -> TraceParams {
        TraceParams {
            concurrency: 48,
            ..frozen()
        }
    }

    /// The resolution a decided outcome carries; a refusal fails the test naming its detail.
    fn decided(outcome: TraceOutcome) -> Resolution {
        match outcome {
            TraceOutcome::Decided(r) => r,
            TraceOutcome::Refused(detail) => panic!("refused: {detail}"),
        }
    }

    #[test]
    fn cache_hit_returns_knobs_regardless_of_regime() {
        let f = frozen();
        let id = trace_id(&f).0;
        let b = broker(&[&id], 0, false); // no GPU free, still a hit needs none
        assert!(matches!(
            decided(b.request_trace(&f).unwrap()),
            Resolution::Hit { .. }
        ));
    }

    #[test]
    fn in_scope_miss_with_headroom_submits() {
        let b = broker(&[], 4, false);
        assert!(matches!(
            decided(b.request_trace(&frozen()).unwrap()),
            Resolution::Submitted { .. }
        ));
    }

    #[test]
    fn in_scope_miss_without_headroom_defers() {
        let b = broker(&[], 1, false); // free 1, need 1 + reserve 1 => defer
        assert!(matches!(
            decided(b.request_trace(&frozen()).unwrap()),
            Resolution::Deferred { .. }
        ));
    }

    #[test]
    fn judge_changing_miss_opens_approval() {
        let b = broker(&[], 8, false);
        assert!(matches!(
            decided(b.request_trace(&hotter()).unwrap()),
            Resolution::PendingApproval { .. }
        ));
    }

    #[test]
    fn the_approval_draft_pr_spends_the_draft_pr_count() {
        let approval = PrApproval::on("owner/repo");
        let opened = approval.counter();
        let b = broker_with(&[], 8, false, approval, draft_pr_bounds(1, "owner/repo"));

        assert!(matches!(
            decided(b.request_trace(&hotter()).unwrap()),
            Resolution::PendingApproval { .. }
        ));

        let mut hotter_still = hotter();
        hotter_still.concurrency = 64;
        let refusal = match b.request_trace(&hotter_still).unwrap() {
            TraceOutcome::Refused(detail) => detail,
            TraceOutcome::Decided(r) => panic!("a second draft PR beyond count 1: {r:?}"),
        };
        assert!(refusal.contains("draft-pr"), "{refusal}");
        assert_eq!(
            opened.load(Ordering::SeqCst),
            1,
            "the refused request must not reach the forge"
        );
    }

    #[test]
    fn an_approval_repo_outside_the_declared_target_is_refused() {
        let approval = PrApproval::on("owner/repo");
        let opened = approval.counter();
        let b = broker_with(
            &[],
            8,
            false,
            approval,
            draft_pr_bounds(4, "owner/elsewhere"),
        );

        let refusal = match b.request_trace(&hotter()).unwrap() {
            TraceOutcome::Refused(detail) => detail,
            TraceOutcome::Decided(r) => panic!("an out-of-scope draft PR: {r:?}"),
        };
        assert!(refusal.contains("owner/repo"), "{refusal}");
        assert_eq!(opened.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_backend_that_opens_no_pr_spends_no_draft_pr_count() {
        let approval = PrApproval::writing_nothing();
        let opened = approval.counter();
        let b = broker_with(&[], 8, false, approval, draft_pr_bounds(0, "owner/repo"));

        assert!(matches!(
            decided(b.request_trace(&hotter()).unwrap()),
            Resolution::PendingApproval { .. }
        ));
        assert_eq!(opened.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn judge_changing_miss_headless_escalates() {
        let b = broker(&[], 8, true);
        assert!(
            matches!(
                decided(b.request_trace(&hotter()).unwrap()),
                Resolution::Escalate { .. }
            ),
            "no approval surface => escalate-halt, never auto-admit a judge change"
        );
    }
}
