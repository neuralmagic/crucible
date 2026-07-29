//! The typed `request_trace` surface + the two pure decisions at the broker's heart: the
//! content key ([`trace_id`]) and the judge-impact classifier ([`judge_impact`]).

use serde::{Deserialize, Serialize};

/// What the agent asks for: a trace calibrated for these run parameters. Mirrors `bench`'s
/// `BENCH_*` workload knobs (the regime that *defines* the measurement) plus the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceParams {
    pub model: String,
    pub concurrency: usize,
    pub prefixes: usize,
    pub max_tokens: u32,
    pub long_tokens: u32,
    pub long_frac: f64,
}

/// The content-addressable key for a trace: a readable, order-stable canonical key shared with
/// the domain's external trace cache.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TraceId(pub String);

/// Whether granting a request would change the frozen judge. This is the integrity-critical
/// axis of the decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgeImpact {
    /// Same regime the harness baseline was frozen at, a missing-calibration *fill*. Safe to
    /// resolve and continue.
    InScope,
    /// A different regime (concurrency / workload / model). A grant must *re-scope* (fresh
    /// baseline + a new fingerprint segment), never a silent continue.
    JudgeChanging,
}

/// What the broker decides for a `request_trace`. The agent acts on `status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Resolution {
    /// Cache hit: calibrated `MOCK_*` knobs are ready; the turn continues GPU-free.
    Hit {
        trace_id: TraceId,
        knobs: serde_json::Value,
    },
    /// No trace yet, but the request was admissible; a capture is queued. Poll the handle.
    Submitted { trace_id: TraceId, handle: String },
    /// Admissible but no GPU headroom right now: deferred with backoff; the loop continues on
    /// fallback knobs rather than blocking.
    Deferred { trace_id: TraceId, reason: String },
    /// A judge-changing grant that needs a human/policy yes first; an approval ask is open
    /// (draft PR or control channel). Poll its handle.
    PendingApproval { trace_id: TraceId, approval: String },
    /// A judge-changing request with no approval surface reachable (degraded headless): the caller
    /// should escalate-halt for a human re-scope.
    Escalate { trace_id: TraceId, reason: String },
}

/// The content key for a set of run parameters. Pure + deterministic, so the same regime always
/// maps to the same trace across the loop and (eventually) CI.
pub fn trace_id(p: &TraceParams) -> TraceId {
    TraceId(format!(
        "model={};c={};p={};mt={};lt={};lf={:.4}",
        p.model, p.concurrency, p.prefixes, p.max_tokens, p.long_tokens, p.long_frac
    ))
}

/// Classify a request against the regime the harness baseline is frozen at: identical regime =>
/// an in-scope fill; anything else => judge-changing (must re-scope, never silently continue).
pub fn judge_impact(req: &TraceParams, frozen: &TraceParams) -> JudgeImpact {
    if trace_id(req) == trace_id(frozen) {
        JudgeImpact::InScope
    } else {
        JudgeImpact::JudgeChanging
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> TraceParams {
        TraceParams {
            model: "claude-opus-4-6".into(),
            concurrency: 12,
            prefixes: 8,
            max_tokens: 8,
            long_tokens: 0,
            long_frac: 0.0,
        }
    }

    #[test]
    fn trace_id_is_deterministic_and_regime_sensitive() {
        let a = params();
        let b = params();
        assert_eq!(trace_id(&a), trace_id(&b), "same params => same id");

        let mut hotter = params();
        hotter.concurrency = 48;
        assert_ne!(
            trace_id(&a),
            trace_id(&hotter),
            "a different concurrency is a different regime"
        );
    }

    #[test]
    fn same_regime_is_in_scope_different_regime_is_judge_changing() {
        let frozen = params();
        assert_eq!(judge_impact(&frozen, &frozen), JudgeImpact::InScope);

        let mut bigger_workload = params();
        bigger_workload.long_frac = 0.2;
        bigger_workload.long_tokens = 256;
        assert_eq!(
            judge_impact(&bigger_workload, &frozen),
            JudgeImpact::JudgeChanging,
            "changing the workload shape moves the goalpost"
        );
    }

    #[test]
    fn resolution_tags_on_the_wire() {
        let hit = Resolution::Hit {
            trace_id: TraceId("x".into()),
            knobs: serde_json::json!({"MOCK_ITL_MS": 9.0}),
        };
        let s = serde_json::to_string(&hit).unwrap();
        assert!(s.contains(r#""status":"hit""#), "{s}");
    }
}
