//! A workload's lifecycle as one coarse phase, derived from the Kueue Workload's conditions.

use crate::kueue::{Workload, condition, is_true};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The object exists but Kueue has not created a Workload for it (yet, or the queue label is ignored).
    NoWorkload,
    /// Waiting in the queue for quota.
    Pending,
    /// Quota is reserved; admission checks have not all passed.
    QuotaReserved,
    /// Admitted; pods may be starting or running.
    Admitted,
    /// Removed from admission (preemption, pods-ready timeout, stop) and requeued or held.
    Evicted,
    /// `spec.active` is false; held out of the queue until resumed.
    Stopped,
    Finished,
}

#[derive(Debug, Clone, Serialize)]
pub struct PhaseReport {
    pub phase: Phase,
    pub reason: String,
    pub message: String,
}

pub fn phase(workload: Option<&Workload>) -> PhaseReport {
    let Some(w) = workload else {
        return PhaseReport {
            phase: Phase::NoWorkload,
            reason: String::new(),
            message: "no Kueue Workload owns this object yet".into(),
        };
    };
    let c = &w.status.conditions;
    let from = |phase: Phase, type_: &str| {
        let cond = condition(c, type_);
        PhaseReport {
            phase,
            reason: cond.map(|c| c.reason.clone()).unwrap_or_default(),
            message: cond.map(|c| c.message.clone()).unwrap_or_default(),
        }
    };
    if is_true(c, "Finished") {
        return from(Phase::Finished, "Finished");
    }
    if w.spec.active == Some(false) {
        return PhaseReport {
            phase: Phase::Stopped,
            reason: "Inactive".into(),
            message: "spec.active is false".into(),
        };
    }
    if is_true(c, "Evicted") {
        return from(Phase::Evicted, "Evicted");
    }
    if is_true(c, "Admitted") {
        return from(Phase::Admitted, "Admitted");
    }
    if is_true(c, "QuotaReserved") {
        return from(Phase::QuotaReserved, "QuotaReserved");
    }
    from(Phase::Pending, "QuotaReserved")
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoWorkload => "no_workload",
            Self::Pending => "pending",
            Self::QuotaReserved => "quota_reserved",
            Self::Admitted => "admitted",
            Self::Evicted => "evicted",
            Self::Stopped => "stopped",
            Self::Finished => "finished",
        }
    }

    /// Lower is further from running; the headline of a mixed set is its lowest phase.
    fn blocking_rank(self) -> u8 {
        match self {
            Self::NoWorkload => 0,
            Self::Pending => 1,
            Self::Evicted => 2,
            Self::QuotaReserved => 3,
            Self::Stopped => 4,
            Self::Admitted => 5,
            Self::Finished => 6,
        }
    }
}

/// Whether a serving workload (Deployment, StatefulSet, LeaderWorkerSet) is up: every wanted
/// replica ready and every Workload admitted. Scaled to zero is never ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Readiness {
    pub ready: bool,
    pub replicas: i64,
    pub ready_replicas: i64,
}

/// `object` is the workload's body (`spec`, `status`).
pub fn readiness(object: &Value, rollup: &Rollup) -> Readiness {
    let replicas = object
        .pointer("/spec/replicas")
        .and_then(Value::as_i64)
        .unwrap_or(1);
    let ready_replicas = object
        .pointer("/status/readyReplicas")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    Readiness {
        ready: replicas > 0
            && ready_replicas >= replicas
            && rollup.headline.phase == Phase::Admitted,
        replicas,
        ready_replicas,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Rollup {
    #[serde(flatten)]
    pub headline: PhaseReport,
    /// Workload count per phase; more than one entry means the object is partially admitted.
    pub counts: BTreeMap<&'static str, usize>,
}

/// One status for an object Kueue tracks as several Workloads (a Deployment's per-pod
/// Workloads). The headline is the phase furthest from running, so "pending" with
/// `counts.admitted > 0` reads as partially admitted.
pub fn rollup(workloads: &[&Workload]) -> Rollup {
    if workloads.is_empty() {
        let headline = phase(None);
        return Rollup {
            counts: BTreeMap::from([(headline.phase.as_str(), 0)]),
            headline,
        };
    }
    let reports: Vec<PhaseReport> = workloads.iter().map(|w| phase(Some(w))).collect();
    let mut counts = BTreeMap::new();
    for r in &reports {
        *counts.entry(r.phase.as_str()).or_insert(0) += 1;
    }
    let mut headline = reports
        .iter()
        .min_by_key(|r| r.phase.blocking_rank())
        .cloned()
        .unwrap_or_else(|| phase(None));
    if counts.len() > 1 {
        let summary: Vec<String> = counts.iter().map(|(k, v)| format!("{v} {k}")).collect();
        headline.message = format!(
            "{} of {} workloads: {}; {}",
            counts.get(headline.phase.as_str()).copied().unwrap_or(0),
            reports.len(),
            summary.join(", "),
            headline.message
        );
    }
    Rollup { headline, counts }
}

#[cfg(test)]
mod tests {
    use crate::kueue::Workload;
    use crate::status::*;
    use serde_json::json;

    fn readiness_of(object: serde_json::Value, phase: Phase) -> Readiness {
        let rollup = Rollup {
            headline: PhaseReport {
                phase,
                reason: String::new(),
                message: String::new(),
            },
            counts: BTreeMap::new(),
        };
        readiness(&object, &rollup)
    }

    #[test]
    fn a_serving_workload_is_ready_once_every_replica_is_ready_and_admitted() {
        let partly = serde_json::json!({"spec": {"replicas": 3}, "status": {"readyReplicas": 2}});
        let fully = serde_json::json!({"spec": {"replicas": 3}, "status": {"readyReplicas": 3}});
        assert_eq!(
            readiness_of(partly, Phase::Admitted),
            Readiness {
                ready: false,
                replicas: 3,
                ready_replicas: 2
            }
        );
        assert!(readiness_of(fully.clone(), Phase::Admitted).ready);
        assert!(!readiness_of(fully, Phase::Pending).ready);
        assert!(
            !readiness_of(
                serde_json::json!({"spec": {"replicas": 0}}),
                Phase::Admitted
            )
            .ready
        );
        assert_eq!(
            readiness_of(serde_json::json!({}), Phase::Admitted),
            Readiness {
                ready: false,
                replicas: 1,
                ready_replicas: 0
            },
            "replicas default to one like the apps API"
        );
    }

    fn wl(conditions: serde_json::Value, active: Option<bool>) -> Workload {
        serde_json::from_value(json!({
            "metadata": {"name": "w"},
            "spec": {"queueName": "q", "active": active},
            "status": {"conditions": conditions}
        }))
        .unwrap()
    }

    #[test]
    fn no_workload_is_its_own_phase() {
        assert_eq!(phase(None).phase, Phase::NoWorkload);
    }

    #[test]
    fn a_pending_workload_carries_kueues_reason_for_not_admitting() {
        let w = wl(
            json!([{"type": "QuotaReserved", "status": "False", "reason": "Pending",
                    "message": "couldn't assign flavors to pod set main: insufficient unused quota for nvidia.com/gpu"}]),
            None,
        );
        let p = phase(Some(&w));
        assert_eq!(p.phase, Phase::Pending);
        assert!(p.message.contains("insufficient unused quota"));
    }

    #[test]
    fn conditions_resolve_in_lifecycle_order() {
        let t = |ty: &str| json!({"type": ty, "status": "True", "reason": ty, "message": ""});
        assert_eq!(
            phase(Some(&wl(json!([t("QuotaReserved")]), None))).phase,
            Phase::QuotaReserved
        );
        assert_eq!(
            phase(Some(&wl(json!([t("QuotaReserved"), t("Admitted")]), None))).phase,
            Phase::Admitted
        );
        assert_eq!(
            phase(Some(&wl(
                json!([t("QuotaReserved"), t("Admitted"), t("Evicted")]),
                None
            )))
            .phase,
            Phase::Evicted
        );
        assert_eq!(
            phase(Some(&wl(
                json!([t("Admitted"), t("Evicted"), t("Finished")]),
                Some(false)
            )))
            .phase,
            Phase::Finished
        );
    }

    #[test]
    fn a_partially_admitted_set_headlines_pending_and_counts_both() {
        let t = |ty: &str| json!({"type": ty, "status": "True", "reason": ty, "message": ""});
        let admitted = wl(json!([t("QuotaReserved"), t("Admitted")]), None);
        let waiting = wl(
            json!([{"type": "QuotaReserved", "status": "False", "reason": "Pending", "message": "insufficient quota"}]),
            None,
        );
        let r = rollup(&[&admitted, &admitted, &waiting]);
        assert_eq!(r.headline.phase, Phase::Pending);
        assert_eq!(r.counts["admitted"], 2);
        assert_eq!(r.counts["pending"], 1);
        assert!(
            r.headline.message.contains("1 of 3"),
            "{}",
            r.headline.message
        );
        assert!(r.headline.message.contains("insufficient quota"));
    }

    #[test]
    fn a_uniform_set_reads_like_a_single_workload() {
        let t = |ty: &str| json!({"type": ty, "status": "True", "reason": ty, "message": "ok"});
        let a = wl(json!([t("Admitted")]), None);
        let r = rollup(&[&a, &a]);
        assert_eq!(r.headline.phase, Phase::Admitted);
        assert_eq!(r.headline.message, "ok");
        assert_eq!(rollup(&[]).headline.phase, Phase::NoWorkload);
    }

    #[test]
    fn an_inactive_workload_is_stopped() {
        assert_eq!(
            phase(Some(&wl(json!([]), Some(false)))).phase,
            Phase::Stopped
        );
    }
}
