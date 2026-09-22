//! Runtime config overrides, backed by a watched ConfigMap (Lane O2).
//!
//! The controller parses ALL config once at startup (clap/env, [`crate::config`]). That stays the
//! DEFAULTS layer. This module adds a second, runtime-mutable layer on top: a small, closed set of
//! numeric caps + boolean toggles an operator can retune WITHOUT a rollout, by editing a ConfigMap
//! (or `PUT /api/config/overrides`). The resolution order is `default < env < override`, and every
//! knob's effective value carries a [`Source`] tag so the UI can show WHERE a value came from.
//!
//! ## What is overridable (and what is emphatically not)
//!
//! Exactly the knobs enumerated in [`Knob::ALL`] — [`crate::config::Profile`] numerics and
//! toggles, and the [`crate::config::ControllerCfg`] toggles/knobs. Nothing else. Executors,
//! sandbox/loop images, namespaces, watched repos, the admin list, DB/state paths: all
//! **restart-only by design**. Those pick object identities
//! (which cluster, which image digest, which repos) that half-built runtime state already depends
//! on; flipping them under a live daemon would strand in-flight pods and split the ledger. A cap or
//! a toggle, by contrast, only changes the NEXT gate decision — safe to move live.
//!
//! ## On-disk format: JSON, one key
//!
//! The override set lives under a single ConfigMap key, `overrides.json`, as a JSON object of the
//! same shape as [`OverrideSet`]. JSON (not TOML) because the same struct already round-trips as
//! JSON on the API boundary (`PUT`/`GET /api/config`) — one codec end to end, no second parser in
//! the watch path. `deny_unknown_fields` makes a typo'd key a loud parse error (kept last-good,
//! never a silently-ignored knob).
//!
//! ## The cache + the watch
//!
//! Reads are cheap: the live [`OverrideSet`] sits behind an `RwLock<Arc<_>>` (the same "atomic-ish
//! swap, cheap clone-on-read" shape [`crate::daemon::autopilot_flag`] uses for its `AtomicBool`, one type up
//! because an `OverrideSet` isn't a single word). A re-list poll loop ([`watch_loop`]) re-reads the
//! ConfigMap on an interval, parses + validates, and swaps the cache on success. Invalid content
//! KEEPS the last-good set, warns, emits an error event, and bumps a reload-failure metric — a
//! fat-fingered edit never crashes the daemon or half-applies. The kube I/O is behind the
//! [`ConfigMapApi`] trait (the [`crate::runs::workpod::PodDispatcher`] discipline) so tests drive it with
//! an in-memory fake and production reaches the cluster through [`KubeConfigMapApi`].

#![allow(clippy::disallowed_macros)]

use crate::metrics::Metrics;
use anyhow::{Context, Result};
use crucible_contract::Tier;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// The single ConfigMap key the override JSON lives under.
const OVERRIDES_KEY: &str = "overrides.json";

/// The default ConfigMap name (overridable via `CONTROLLER_OVERRIDES_CONFIGMAP`).
pub(crate) const DEFAULT_CONFIGMAP_NAME: &str = "crucible-controller-overrides";

/// Where a knob's effective value came from, in precedence order `default < env < override`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The compiled-in default (`Profile::default` / the clap `default_value_t`).
    Default,
    /// An environment variable set at startup (the deploy render / clap `env`).
    Env,
    /// A runtime override from the watched ConfigMap.
    Override,
}

/// The closed set of overridable knobs. The ONLY knobs a runtime override may touch; the enum makes
/// that exhaustive (a new overridable knob is a new variant, wired everywhere the compiler points).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Knob {
    DiscoverySecs,
    PerReconcileCost,
    MaxConcurrentPods,
    MaxScopesPerDay,
    DailyCostCeiling,
    RankCostFallbackUsd,
    GroundedRankPodCap,
    GroundedRankDailyTurns,
    FailedPodKeep,
    ScopeGamingRounds,
    ScopeSkipGamingReview,
    AllowT3,
    PrescopeGrounded,
    RankHorizonDays,
    AllowedTiers,
    RunIterations,
    RunMaxCost,
    AllowDraftHeadSchedules,
}

impl Knob {
    /// Every knob, for iterating the API view + the override-active gauge.
    pub(crate) const ALL: [Knob; 18] = [
        Knob::DiscoverySecs,
        Knob::PerReconcileCost,
        Knob::MaxConcurrentPods,
        Knob::MaxScopesPerDay,
        Knob::DailyCostCeiling,
        Knob::RankCostFallbackUsd,
        Knob::GroundedRankPodCap,
        Knob::GroundedRankDailyTurns,
        Knob::FailedPodKeep,
        Knob::ScopeGamingRounds,
        Knob::ScopeSkipGamingReview,
        Knob::AllowT3,
        Knob::PrescopeGrounded,
        Knob::RankHorizonDays,
        Knob::AllowedTiers,
        Knob::RunIterations,
        Knob::RunMaxCost,
        Knob::AllowDraftHeadSchedules,
    ];

    /// The knob's stable name — the JSON key in the ConfigMap, the metric label, and the API name.
    /// Kept identical to the [`OverrideSet`] serde field so the three never drift.
    fn name(self) -> &'static str {
        match self {
            Knob::DiscoverySecs => "discovery_secs",
            Knob::PerReconcileCost => "per_reconcile_cost",
            Knob::MaxConcurrentPods => "max_concurrent_pods",
            Knob::MaxScopesPerDay => "max_scopes_per_day",
            Knob::DailyCostCeiling => "daily_cost_ceiling",
            Knob::RankCostFallbackUsd => "rank_cost_fallback_usd",
            Knob::GroundedRankPodCap => "grounded_rank_pod_cap",
            Knob::GroundedRankDailyTurns => "grounded_rank_daily_turns",
            Knob::FailedPodKeep => "failed_pod_keep",
            Knob::ScopeGamingRounds => "scope_gaming_rounds",
            Knob::ScopeSkipGamingReview => "scope_skip_gaming_review",
            Knob::AllowT3 => "allow_t3",
            Knob::PrescopeGrounded => "prescope_grounded",
            Knob::RankHorizonDays => "rank_horizon_days",
            Knob::AllowedTiers => "allowed_tiers",
            Knob::RunIterations => "run_iterations",
            Knob::RunMaxCost => "run_max_cost",
            Knob::AllowDraftHeadSchedules => "allow_draft_head_schedules",
        }
    }

    /// The environment variable the DEFAULTS layer reads this knob from, for the env-vs-default
    /// source discrimination (clap can't tell you post-parse whether a value came from env).
    fn env_var(self) -> &'static str {
        match self {
            Knob::DiscoverySecs => "CONTROLLER_DISCOVERY_CADENCE_SECS",
            Knob::PerReconcileCost => "CONTROLLER_PER_RECONCILE_COST_USD",
            Knob::MaxConcurrentPods => "CONTROLLER_MAX_CONCURRENT_PODS",
            Knob::MaxScopesPerDay => "CONTROLLER_MAX_SCOPES_PER_DAY",
            Knob::DailyCostCeiling => "CONTROLLER_DAILY_COST_CEILING_USD",
            Knob::RankCostFallbackUsd => "CONTROLLER_RANK_COST_FALLBACK_USD",
            Knob::GroundedRankPodCap => "CONTROLLER_GROUNDED_RANK_POD_CAP",
            Knob::GroundedRankDailyTurns => "CONTROLLER_GROUNDED_RANK_DAILY_TURNS",
            Knob::FailedPodKeep => "CONTROLLER_FAILED_POD_KEEP",
            Knob::ScopeGamingRounds => "CONTROLLER_SCOPE_GAMING_ROUNDS",
            Knob::ScopeSkipGamingReview => "CONTROLLER_SCOPE_SKIP_GAMING_REVIEW",
            Knob::AllowT3 => "CONTROLLER_ALLOW_T3",
            Knob::PrescopeGrounded => "CONTROLLER_PRESCOPE_GROUNDED",
            Knob::RankHorizonDays => "CONTROLLER_RANK_HORIZON_DAYS",
            Knob::AllowedTiers => "CONTROLLER_ALLOWED_TIERS",
            Knob::RunIterations => "CONTROLLER_RUN_ITERATIONS",
            Knob::RunMaxCost => "CONTROLLER_RUN_MAX_COST_USD",
            Knob::AllowDraftHeadSchedules => "CONTROLLER_ALLOW_DRAFT_HEAD_SCHEDULES",
        }
    }

    /// A one-line human description for the `GET /api/config` view.
    fn description(self) -> &'static str {
        match self {
            Knob::DiscoverySecs => "Seconds between discovery sweeps.",
            Knob::PerReconcileCost => "Per-reconcile cost ceiling in USD (scope + rank turns).",
            Knob::MaxConcurrentPods => "Max concurrent loop pods admitted at once.",
            Knob::MaxScopesPerDay => "Max new scopes started per UTC day.",
            Knob::DailyCostCeiling => {
                "Global daily cost ceiling in USD; crossing it parks autopilot."
            }
            Knob::RankCostFallbackUsd => {
                "Fallback USD ledgered for a tier-ranking call with no self-reported cost."
            }
            Knob::GroundedRankPodCap => "Max concurrent grounded-rank work pods.",
            Knob::GroundedRankDailyTurns => "Max grounded-rank turns dispatched per UTC day.",
            Knob::FailedPodKeep => {
                "Max retained failed work pods kept for debugging (newest first); the rest are \
                 swept immediately. 0 keeps none."
            }
            Knob::ScopeGamingRounds => {
                "Max concern→refine→re-review cycles a scope turn's gaming review may spend \
                 (the last look is always final). 1 = one cycle, the historical behavior."
            }
            Knob::ScopeSkipGamingReview => {
                "Skip the adversarial gaming review entirely on a scope turn, overriding \
                 scope_gaming_rounds. Operator escape hatch for demo/bring-up; off by default."
            }
            Knob::AllowT3 => {
                "DEPRECATED alias: true unions t3 into allowed_tiers. Include T3 \
                 (multi-component-live-rig) issues in the autopilot's pick."
            }
            Knob::PrescopeGrounded => {
                "Require a code-grounded confirmation before every scope turn."
            }
            Knob::RankHorizonDays => {
                "Only consider issues with upstream activity in the last N days (0 = off)."
            }
            Knob::AllowedTiers => {
                "The tiers the autopilot's pick will scope (comma-separated t0|t1|t2|t3); \
                 everything else is deferred."
            }
            Knob::RunIterations => "Agent iterations a controller-dispatched loop run gets (>= 1).",
            Knob::RunMaxCost => {
                "Per-run cost ceiling in USD for a controller-dispatched loop run (0 = unlimited)."
            }
            Knob::AllowDraftHeadSchedules => {
                "Allow recurring schedules to follow mutable draft heads. Development escape hatch; off by default."
            }
        }
    }
}

/// The serde mirror of the overridable knobs: every field `Option`, so an override set states only
/// the knobs it wants to move and leaves the rest to the defaults layer. `deny_unknown_fields`
/// rejects a typo'd key (a loud parse error → kept last-good, never a silent no-op). An empty `{}`
/// deserializes to all-`None` — the boot-time create-if-missing seed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields, default)]
pub struct OverrideSet {
    pub(crate) discovery_secs: Option<u64>,
    pub(crate) per_reconcile_cost: Option<f64>,
    pub(crate) max_concurrent_pods: Option<u32>,
    pub(crate) max_scopes_per_day: Option<u32>,
    pub(crate) daily_cost_ceiling: Option<f64>,
    pub(crate) rank_cost_fallback_usd: Option<f64>,
    pub(crate) grounded_rank_pod_cap: Option<u32>,
    pub(crate) grounded_rank_daily_turns: Option<u32>,
    pub(crate) failed_pod_keep: Option<u32>,
    pub(crate) scope_gaming_rounds: Option<u32>,
    pub(crate) scope_skip_gaming_review: Option<bool>,
    pub(crate) allow_t3: Option<bool>,
    pub(crate) prescope_grounded: Option<bool>,
    pub(crate) rank_horizon_days: Option<u32>,
    pub(crate) run_iterations: Option<u32>,
    pub(crate) run_max_cost: Option<f64>,
    pub(crate) allow_draft_head_schedules: Option<bool>,
    /// The lowercase `t0|t1|t2|t3` spelling ([`Tier::as_str_lower`]/[`Tier::parse_lower`]) — kept
    /// as raw strings (not `Vec<Tier>`) so the wire format needs nothing more than
    /// `serde`/`utoipa` already derive on this struct; [`OverrideSet::validate`] is what rejects a
    /// garbage tier, same as every other knob's bound.
    pub(crate) allowed_tiers: Option<Vec<String>>,
}

impl OverrideSet {
    /// Whether `knob` is currently overridden (drives the source tag + the `override_active` gauge).
    fn is_set(&self, knob: Knob) -> bool {
        match knob {
            Knob::DiscoverySecs => self.discovery_secs.is_some(),
            Knob::PerReconcileCost => self.per_reconcile_cost.is_some(),
            Knob::MaxConcurrentPods => self.max_concurrent_pods.is_some(),
            Knob::MaxScopesPerDay => self.max_scopes_per_day.is_some(),
            Knob::DailyCostCeiling => self.daily_cost_ceiling.is_some(),
            Knob::RankCostFallbackUsd => self.rank_cost_fallback_usd.is_some(),
            Knob::GroundedRankPodCap => self.grounded_rank_pod_cap.is_some(),
            Knob::GroundedRankDailyTurns => self.grounded_rank_daily_turns.is_some(),
            Knob::FailedPodKeep => self.failed_pod_keep.is_some(),
            Knob::ScopeGamingRounds => self.scope_gaming_rounds.is_some(),
            Knob::ScopeSkipGamingReview => self.scope_skip_gaming_review.is_some(),
            Knob::AllowT3 => self.allow_t3.is_some(),
            Knob::PrescopeGrounded => self.prescope_grounded.is_some(),
            Knob::RankHorizonDays => self.rank_horizon_days.is_some(),
            Knob::AllowedTiers => self.allowed_tiers.is_some(),
            Knob::RunIterations => self.run_iterations.is_some(),
            Knob::RunMaxCost => self.run_max_cost.is_some(),
            Knob::AllowDraftHeadSchedules => self.allow_draft_head_schedules.is_some(),
        }
    }

    /// The knobs whose override value differs between `self` and `prev` (set/cleared/changed) — the
    /// changed-keys diff the audit event records on a `PUT`.
    pub(crate) fn changed_keys(&self, prev: &OverrideSet) -> Vec<&'static str> {
        let now = serde_json::to_value(self).unwrap_or_default();
        let was = serde_json::to_value(prev).unwrap_or_default();
        Knob::ALL
            .into_iter()
            .filter(|&k| now.get(k.name()) != was.get(k.name()))
            .map(Knob::name)
            .collect()
    }

    /// Validate the override values against sane bounds. Returns the list of every rejection (not
    /// just the first) so a `PUT` can 400 with all of them at once. Numerics must be finite and
    /// non-negative; caps have loose upper bounds to catch a fat-fingered runaway.
    pub(crate) fn validate(&self) -> Result<(), Vec<String>> {
        let mut errs = Vec::new();
        let finite_nonneg = |errs: &mut Vec<String>, name: &str, v: Option<f64>| {
            if let Some(v) = v
                && (!v.is_finite() || v < 0.0)
            {
                errs.push(format!(
                    "{name} must be a finite, non-negative number (got {v})"
                ));
            }
        };
        if let Some(s) = self.discovery_secs
            && s == 0
        {
            errs.push("discovery_secs must be >= 1 (0 would busy-loop discovery)".to_string());
        }
        finite_nonneg(&mut errs, "per_reconcile_cost", self.per_reconcile_cost);
        finite_nonneg(&mut errs, "daily_cost_ceiling", self.daily_cost_ceiling);
        finite_nonneg(
            &mut errs,
            "rank_cost_fallback_usd",
            self.rank_cost_fallback_usd,
        );
        finite_nonneg(&mut errs, "run_max_cost", self.run_max_cost);
        if let Some(i) = self.run_iterations
            && !(1..=1000).contains(&i)
        {
            errs.push(format!(
                "run_iterations must be within 1..=1000 (0 would cut the run off immediately) \
                     (got {i})"
            ));
        }
        if let Some(p) = self.max_concurrent_pods
            && p > 100
        {
            errs.push(format!("max_concurrent_pods must be <= 100 (got {p})"));
        }
        if let Some(s) = self.max_scopes_per_day
            && s > 10_000
        {
            errs.push(format!("max_scopes_per_day must be <= 10000 (got {s})"));
        }
        if let Some(k) = self.failed_pod_keep
            && k > 10_000
        {
            errs.push(format!("failed_pod_keep must be <= 10000 (got {k})"));
        }
        if let Some(r) = self.scope_gaming_rounds
            && !(0..=10).contains(&r)
        {
            errs.push(format!(
                "scope_gaming_rounds must be within 0..=10 (0 = review once, reject on first \
                 concern) (got {r})"
            ));
        }
        if let Some(tiers) = &self.allowed_tiers {
            for t in tiers {
                if let Err(e) = Tier::parse_lower(t) {
                    errs.push(format!("allowed_tiers: {e}"));
                }
            }
        }
        if errs.is_empty() { Ok(()) } else { Err(errs) }
    }
}

/// The DEFAULTS-layer snapshot: the clap/env-resolved values of exactly the overridable knobs,
/// captured once when the store is built. Immutable for the process's life; the override layer is
/// what moves at runtime.
#[derive(Debug, Clone)]
pub struct BaseConfig {
    pub(crate) discovery_secs: u64,
    pub(crate) per_reconcile_cost: f64,
    pub(crate) max_concurrent_pods: u32,
    pub(crate) max_scopes_per_day: u32,
    pub(crate) daily_cost_ceiling: f64,
    pub(crate) rank_cost_fallback_usd: f64,
    pub(crate) grounded_rank_pod_cap: u32,
    pub(crate) grounded_rank_daily_turns: u32,
    pub(crate) failed_pod_keep: u32,
    pub(crate) scope_gaming_rounds: u32,
    pub(crate) scope_skip_gaming_review: bool,
    pub(crate) allow_t3: bool,
    pub(crate) prescope_grounded: bool,
    pub(crate) rank_horizon_days: u32,
    pub(crate) allowed_tiers: Vec<Tier>,
    pub(crate) run_iterations: u32,
    pub(crate) run_max_cost: f64,
    pub(crate) allow_draft_head_schedules: bool,
}

/// The resolved effective values the reconcile/dispatch paths read per cycle — `default < env <
/// override` already folded per field. Cheap to build (an `RwLock` read + a field copy), so the hot
/// path calls [`ControllerCfg::effective`] freely.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveConfig {
    discovery_secs: u64,
    pub(crate) per_reconcile_cost: f64,
    pub(crate) max_concurrent_pods: u32,
    pub(crate) max_scopes_per_day: u32,
    pub(crate) daily_cost_ceiling: f64,
    pub(crate) rank_cost_fallback_usd: f64,
    pub(crate) grounded_rank_pod_cap: u32,
    pub(crate) grounded_rank_daily_turns: u32,
    pub failed_pod_keep: u32,
    pub(crate) scope_gaming_rounds: u32,
    pub(crate) scope_skip_gaming_review: bool,
    pub(crate) allow_t3: bool,
    pub(crate) prescope_grounded: bool,
    pub(crate) rank_horizon_days: u32,
    /// The tiers the autopilot's pick will scope. Always includes `T3` when `allow_t3` resolves
    /// true — the deprecated alias is a union over whatever `allowed_tiers` itself resolved to,
    /// never a separate gate consulted on its own (see [`EffectiveConfig::resolve`]).
    pub(crate) allowed_tiers: Vec<Tier>,
    pub(crate) run_iterations: u32,
    pub(crate) run_max_cost: f64,
    pub(crate) allow_draft_head_schedules: bool,
}

impl EffectiveConfig {
    /// Fold the override layer over the base: an override wins per field, else the base value stands.
    pub(crate) fn resolve(base: &BaseConfig, ov: &OverrideSet) -> Self {
        let allow_t3 = ov.allow_t3.unwrap_or(base.allow_t3);
        let mut allowed_tiers: Vec<Tier> = match &ov.allowed_tiers {
            Some(strs) => strs
                .iter()
                .filter_map(|s| Tier::parse_lower(s).ok())
                .collect(),
            None => base.allowed_tiers.clone(),
        };
        // The deprecated `allow_t3` alias unions T3 in rather than being a second, independently
        // consulted gate — `allowed_tiers.contains(&Tier::T3)` is the one place callers need to
        // check.
        if allow_t3 && !allowed_tiers.contains(&Tier::T3) {
            allowed_tiers.push(Tier::T3);
        }
        EffectiveConfig {
            discovery_secs: ov.discovery_secs.unwrap_or(base.discovery_secs),
            per_reconcile_cost: ov.per_reconcile_cost.unwrap_or(base.per_reconcile_cost),
            max_concurrent_pods: ov.max_concurrent_pods.unwrap_or(base.max_concurrent_pods),
            max_scopes_per_day: ov.max_scopes_per_day.unwrap_or(base.max_scopes_per_day),
            daily_cost_ceiling: ov.daily_cost_ceiling.unwrap_or(base.daily_cost_ceiling),
            rank_cost_fallback_usd: ov
                .rank_cost_fallback_usd
                .unwrap_or(base.rank_cost_fallback_usd),
            grounded_rank_pod_cap: ov
                .grounded_rank_pod_cap
                .unwrap_or(base.grounded_rank_pod_cap),
            grounded_rank_daily_turns: ov
                .grounded_rank_daily_turns
                .unwrap_or(base.grounded_rank_daily_turns),
            failed_pod_keep: ov.failed_pod_keep.unwrap_or(base.failed_pod_keep),
            scope_gaming_rounds: ov.scope_gaming_rounds.unwrap_or(base.scope_gaming_rounds),
            scope_skip_gaming_review: ov
                .scope_skip_gaming_review
                .unwrap_or(base.scope_skip_gaming_review),
            allow_t3,
            prescope_grounded: ov.prescope_grounded.unwrap_or(base.prescope_grounded),
            rank_horizon_days: ov.rank_horizon_days.unwrap_or(base.rank_horizon_days),
            allowed_tiers,
            run_iterations: ov.run_iterations.unwrap_or(base.run_iterations),
            run_max_cost: ov.run_max_cost.unwrap_or(base.run_max_cost),
            allow_draft_head_schedules: ov
                .allow_draft_head_schedules
                .unwrap_or(base.allow_draft_head_schedules),
        }
    }

    /// The discovery cadence as a [`Duration`] (the daemon loop reads this per tick).
    pub fn discovery_interval(&self) -> Duration {
        Duration::from_secs(self.discovery_secs)
    }

    /// The effective value of `knob` as JSON, for the `GET /api/config` view.
    fn value_json(&self, knob: Knob) -> serde_json::Value {
        use serde_json::Value;
        match knob {
            Knob::DiscoverySecs => Value::from(self.discovery_secs),
            Knob::PerReconcileCost => Value::from(self.per_reconcile_cost),
            Knob::MaxConcurrentPods => Value::from(self.max_concurrent_pods),
            Knob::MaxScopesPerDay => Value::from(self.max_scopes_per_day),
            Knob::DailyCostCeiling => Value::from(self.daily_cost_ceiling),
            Knob::RankCostFallbackUsd => Value::from(self.rank_cost_fallback_usd),
            Knob::GroundedRankPodCap => Value::from(self.grounded_rank_pod_cap),
            Knob::GroundedRankDailyTurns => Value::from(self.grounded_rank_daily_turns),
            Knob::FailedPodKeep => Value::from(self.failed_pod_keep),
            Knob::ScopeGamingRounds => Value::from(self.scope_gaming_rounds),
            Knob::ScopeSkipGamingReview => Value::from(self.scope_skip_gaming_review),
            Knob::AllowT3 => Value::from(self.allow_t3),
            Knob::PrescopeGrounded => Value::from(self.prescope_grounded),
            Knob::RankHorizonDays => Value::from(self.rank_horizon_days),
            Knob::AllowedTiers => Value::from(
                self.allowed_tiers
                    .iter()
                    .map(|t| t.as_str_lower().to_string())
                    .collect::<Vec<_>>(),
            ),
            Knob::RunIterations => Value::from(self.run_iterations),
            Knob::RunMaxCost => Value::from(self.run_max_cost),
            Knob::AllowDraftHeadSchedules => Value::from(self.allow_draft_head_schedules),
        }
    }
}

/// Which of the overridable knobs had their env var set at startup — captured once so the source
/// tag can say `env` vs `default` (clap folds both into one parsed value and can't tell you which).
#[derive(Debug, Clone, Default)]
pub struct EnvSources {
    set: std::collections::HashSet<&'static str>,
}

impl EnvSources {
    /// Probe the process environment once for every knob's env var.
    fn detect() -> Self {
        let set = Knob::ALL
            .into_iter()
            .filter(|k| {
                std::env::var(k.env_var())
                    .ok()
                    .is_some_and(|v| !v.is_empty())
            })
            .map(Knob::env_var)
            .collect();
        EnvSources { set }
    }

    fn is_env_set(&self, knob: Knob) -> bool {
        self.set.contains(knob.env_var())
    }
}

// -------------------------------------------------------------------------------------------------
// The kube boundary — a small trait so tests use an in-memory fake and production reaches the cluster
// (the `PodDispatcher` discipline).
// -------------------------------------------------------------------------------------------------

/// One observed ConfigMap: its `resourceVersion` (for optimistic replace) + the value under
/// [`OVERRIDES_KEY`] (`None` when the key is absent — a fresh/empty CM).
#[derive(Debug, Clone, PartialEq)]
pub struct CmSnapshot {
    pub(crate) resource_version: String,
    pub(crate) payload: Option<String>,
}

/// The outcome of an optimistic [`ConfigMapApi::replace`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceOutcome {
    /// The replace landed.
    Applied,
    /// The `resourceVersion` was stale (someone else wrote first) — the caller re-reads and retries.
    Conflict,
}

/// The cluster boundary for the overrides ConfigMap: read it, create it (boot seed), and
/// resource-version-guarded replace it (the `PUT` write). Async, awaited on the daemon's runtime
/// (the trait is `dyn`, so `async_trait` boxes the futures). Production is [`KubeConfigMapApi`]; a
/// test installs an in-memory fake so the store + watch + write logic run without a cluster.
#[async_trait::async_trait]
pub trait ConfigMapApi: Send + Sync {
    /// Read the CM, or `None` if it doesn't exist.
    async fn get(&self, namespace: &str, name: &str) -> Result<Option<CmSnapshot>>;
    /// Create the CM with `payload` under [`OVERRIDES_KEY`] (boot create-if-missing).
    async fn create(&self, namespace: &str, name: &str, payload: &str) -> Result<()>;
    /// Replace the CM's data, guarded on `resource_version` (optimistic concurrency).
    async fn replace(
        &self,
        namespace: &str,
        name: &str,
        payload: &str,
        resource_version: &str,
    ) -> Result<ReplaceOutcome>;
}

/// The reload outcome, mapped to the `crucible_config_reloads_total{outcome}` metric label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadOutcome {
    /// Parsed + validated + swapped a new set (or confirmed the same one).
    Ok,
    /// The CM content failed to parse or validate — kept last-good.
    Invalid,
    /// The kube read itself failed — kept last-good.
    Error,
}

impl ReloadOutcome {
    fn as_str(self) -> &'static str {
        match self {
            ReloadOutcome::Ok => "ok",
            ReloadOutcome::Invalid => "invalid",
            ReloadOutcome::Error => "error",
        }
    }
}

/// The runtime override store: the immutable defaults snapshot + env-source map, the live override
/// cache behind an `RwLock<Arc<_>>`, and the ConfigMap coordinates + kube boundary the watch/write
/// use. Cloneable (an `Arc` inside) so it threads onto [`ControllerCfg`] and into [`crate::api`]
/// without a global.
#[derive(Clone)]
pub struct ConfigStore(Arc<Inner>);

struct Inner {
    base: BaseConfig,
    env: EnvSources,
    cache: RwLock<Arc<OverrideSet>>,
    policy_gate: Arc<tokio::sync::RwLock<()>>,
    cm_name: String,
    cm_namespace: String,
    api: Arc<dyn ConfigMapApi>,
    metrics: Option<Metrics>,
    /// The append-only audit log an invalid reload records an error event to (the same log the API
    /// writes the `PUT` audit event to). `None` in tests / when no ledger is threaded.
    events: Option<crate::event_log::EventLog>,
}

impl std::fmt::Debug for ConfigStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigStore")
            .field("cm_name", &self.0.cm_name)
            .field("cm_namespace", &self.0.cm_namespace)
            .field("overrides", &self.current())
            .finish_non_exhaustive()
    }
}

impl ConfigStore {
    /// Build a store: snapshot the defaults off `cfg`, detect env sources, and seed the cache empty.
    /// The watch ([`ConfigStore::reload_once`]) fills it from the CM on boot.
    pub fn new(
        base: BaseConfig,
        cm_name: String,
        cm_namespace: String,
        api: Arc<dyn ConfigMapApi>,
        metrics: Option<Metrics>,
        events: Option<crate::event_log::EventLog>,
    ) -> Self {
        let store = ConfigStore(Arc::new(Inner {
            base,
            env: EnvSources::detect(),
            cache: RwLock::new(Arc::new(OverrideSet::default())),
            policy_gate: Arc::new(tokio::sync::RwLock::new(())),
            cm_name,
            cm_namespace,
            api,
            metrics,
            events,
        }));
        store.refresh_active_gauge();
        store
    }

    /// The current override set (a cheap `Arc` clone). Never blocks a writer for long — reads take
    /// the lock only to clone the `Arc`.
    pub(crate) fn current(&self) -> Arc<OverrideSet> {
        self.0
            .cache
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|p| p.into_inner().clone())
    }

    /// The resolved effective config (the hot read path).
    pub fn effective(&self) -> EffectiveConfig {
        EffectiveConfig::resolve(&self.0.base, &self.current())
    }

    /// Serialize recurring-policy transitions against schedule saves and firing sweeps.
    pub(crate) async fn recurring_policy_read(&self) -> tokio::sync::OwnedRwLockReadGuard<()> {
        self.0.policy_gate.clone().read_owned().await
    }

    pub(crate) async fn recurring_policy_write(&self) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        self.0.policy_gate.clone().write_owned().await
    }

    /// The source tag for `knob`: `override` if set, else `env` if its env var was set at startup,
    /// else `default`.
    fn source_of(&self, knob: Knob) -> Source {
        if self.current().is_set(knob) {
            Source::Override
        } else if self.0.env.is_env_set(knob) {
            Source::Env
        } else {
            Source::Default
        }
    }

    /// The per-knob view the `GET /api/config` endpoint serves: name, effective value, source,
    /// overridable flag, description.
    pub(crate) fn views(&self) -> Vec<KnobView> {
        let eff = self.effective();
        Knob::ALL
            .into_iter()
            .map(|k| KnobView {
                name: k.name().to_string(),
                value: eff.value_json(k),
                source: self.source_of(k),
                overridable: true,
                description: k.description().to_string(),
            })
            .collect()
    }

    /// Swap the live override set + refresh the override-active gauge. The one place the cache moves.
    fn swap(&self, next: Arc<OverrideSet>) {
        match self.0.cache.write() {
            Ok(mut g) => *g = next,
            Err(p) => *p.into_inner() = next,
        }
        self.refresh_active_gauge();
    }

    /// Set the `crucible_config_override_active{knob}` gauge to 1/0 for every knob from the current
    /// set — called on every swap so the gauge always mirrors the live overrides.
    fn refresh_active_gauge(&self) {
        if let Some(m) = &self.0.metrics {
            let ov = self.current();
            for knob in Knob::ALL {
                m.set_config_override_active(knob.name(), ov.is_set(knob));
            }
        }
    }

    /// Create the ConfigMap with an empty override set if it doesn't exist yet (boot). A no-op when
    /// it's already there. Idempotent, so every replica racing this on startup is harmless.
    pub async fn ensure_configmap(&self) -> Result<()> {
        if self
            .0
            .api
            .get(&self.0.cm_namespace, &self.0.cm_name)
            .await
            .context("reading the overrides ConfigMap")?
            .is_some()
        {
            return Ok(());
        }
        let empty = serde_json::to_string_pretty(&OverrideSet::default())
            .context("serializing the empty override seed")?;
        self.0
            .api
            .create(&self.0.cm_namespace, &self.0.cm_name, &empty)
            .await
            .context("creating the overrides ConfigMap")
    }

    /// Re-read the CM once, parse + validate, and swap the cache on success. On any failure KEEP the
    /// last-good set, warn, event, and bump the reload-failure metric — never crash, never
    /// half-apply. The watch loop calls this on an interval; the [`ReloadOutcome`] is returned for
    /// tests + the metric.
    pub async fn reload_once(&self) -> ReloadOutcome {
        let _policy_transition = self.recurring_policy_write().await;
        let outcome = match self.0.api.get(&self.0.cm_namespace, &self.0.cm_name).await {
            Ok(Some(snap)) => self.apply_snapshot(snap.payload.as_deref()).await,
            Ok(None) => {
                // The CM vanished (deleted out from under us): keep last-good, don't panic.
                tracing::warn!(
                    cm = %self.0.cm_name,
                    "overrides ConfigMap is gone; keeping the last-good override set"
                );
                ReloadOutcome::Error
            }
            Err(e) => {
                tracing::warn!(
                    cm = %self.0.cm_name,
                    error = format!("{e:#}"),
                    "reading the overrides ConfigMap failed; keeping the last-good override set"
                );
                ReloadOutcome::Error
            }
        };
        if let Some(m) = &self.0.metrics {
            m.record_config_reload(outcome.as_str());
        }
        outcome
    }

    /// Parse + validate one CM payload and swap on success. A missing key parses as the empty set.
    async fn apply_snapshot(&self, payload: Option<&str>) -> ReloadOutcome {
        let parsed: Result<OverrideSet, String> = match payload {
            None => Ok(OverrideSet::default()),
            Some(raw) if raw.trim().is_empty() => Ok(OverrideSet::default()),
            Some(raw) => serde_json::from_str::<OverrideSet>(raw).map_err(|e| e.to_string()),
        };
        let next = match parsed {
            Ok(set) => set,
            Err(e) => {
                self.report_invalid(&format!("parse failed: {e}")).await;
                return ReloadOutcome::Invalid;
            }
        };
        if let Err(errs) = next.validate() {
            self.report_invalid(&format!("validation failed: {}", errs.join("; ")))
                .await;
            return ReloadOutcome::Invalid;
        }
        self.swap(Arc::new(next));
        ReloadOutcome::Ok
    }

    /// Warn + append an error event for an invalid CM (kept last-good). Best-effort event write —
    /// a failed audit append is logged, never propagated (a bad CM must not wedge the watch).
    async fn report_invalid(&self, detail: &str) {
        tracing::warn!(
            cm = %self.0.cm_name,
            detail,
            "overrides ConfigMap content is invalid; keeping the last-good override set"
        );
        if let Some(events) = &self.0.events {
            let reason = format!("invalid overrides ConfigMap kept last-good: {detail}");
            if let Err(e) = events
                .append(&crate::event_log::Event::now(
                    "config",
                    "reload",
                    "invalid",
                    Some(&reason),
                    None,
                ))
                .await
            {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "overrides: audit event append failed"
                );
            }
        }
    }

    /// Write a validated override set to the CM (the `PUT` path): read-modify-write on the current
    /// `resourceVersion`, retry once on a conflict, then apply-and-swap locally so the caller reads
    /// its own write immediately (the watch re-confirms the same content later, an idempotent
    /// no-op). Returns the new effective config.
    pub(crate) async fn write_overrides(&self, next: &OverrideSet) -> Result<EffectiveConfig> {
        let payload = serde_json::to_string_pretty(next).context("serializing the override set")?;
        // Up to two attempts: the first may lose a resourceVersion race, the re-read + retry wins.
        for _ in 0..2 {
            let snap = self
                .0
                .api
                .get(&self.0.cm_namespace, &self.0.cm_name)
                .await
                .context("reading the overrides ConfigMap before write")?;
            let outcome = match snap {
                Some(snap) => {
                    self.0
                        .api
                        .replace(
                            &self.0.cm_namespace,
                            &self.0.cm_name,
                            &payload,
                            &snap.resource_version,
                        )
                        .await?
                }
                None => {
                    // The CM doesn't exist yet — create it with the new content.
                    self.0
                        .api
                        .create(&self.0.cm_namespace, &self.0.cm_name, &payload)
                        .await?;
                    ReplaceOutcome::Applied
                }
            };
            match outcome {
                ReplaceOutcome::Applied => {
                    // Apply-and-swap immediately for read-your-writes; the watch confirms later.
                    self.swap(Arc::new(next.clone()));
                    return Ok(self.effective());
                }
                ReplaceOutcome::Conflict => continue,
            }
        }
        anyhow::bail!("overrides ConfigMap write conflicted twice; retry the request")
    }
}

#[cfg(test)]
impl ConfigStore {
    /// Test-only: a store with `ov` already applied and no real kube boundary, for exercising the
    /// migrated read sites (a reconcile/dispatch path reading `cfg.effective()`). Snapshots the
    /// defaults off `cfg`, then swaps the override set in directly.
    pub(crate) fn seeded_for_test(base: BaseConfig, ov: OverrideSet) -> Self {
        struct NoCm;
        #[async_trait::async_trait]
        impl ConfigMapApi for NoCm {
            async fn get(&self, _ns: &str, _name: &str) -> Result<Option<CmSnapshot>> {
                Ok(None)
            }
            async fn create(&self, _ns: &str, _name: &str, _payload: &str) -> Result<()> {
                Ok(())
            }
            async fn replace(
                &self,
                _ns: &str,
                _name: &str,
                _payload: &str,
                _rv: &str,
            ) -> Result<ReplaceOutcome> {
                Ok(ReplaceOutcome::Applied)
            }
        }
        let store = ConfigStore::new(
            base,
            DEFAULT_CONFIGMAP_NAME.to_string(),
            "ns".to_string(),
            Arc::new(NoCm),
            None,
            None,
        );
        store.swap(Arc::new(ov));
        store
    }
}

/// One knob's row in the `GET /api/config` response.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct KnobView {
    name: String,
    #[schema(value_type = Object)]
    value: serde_json::Value,
    source: Source,
    overridable: bool,
    description: String,
}

/// The re-list poll loop: re-read the CM every `interval`, swap on a valid change, keep last-good
/// otherwise, until `shutdown` fires. Informer-style (a poll, not a kube watch stream) so the whole
/// path is testable against the fake — [`ConfigStore::reload_once`] is the unit under test, this is
/// just its timer.
pub async fn watch_loop(
    store: ConfigStore,
    interval: Duration,
    shutdown: Arc<tokio::sync::Notify>,
) {
    let mut timer = tokio::time::interval(interval);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Pinned + enabled once so a `notify_waiters` during a reload isn't lost (the daemon loop's
    // shutdown pattern).
    let shutdown_signal = shutdown.notified();
    tokio::pin!(shutdown_signal);
    shutdown_signal.as_mut().enable();
    loop {
        tokio::select! {
            _ = timer.tick() => {
                let _ = store.reload_once().await;
            }
            _ = shutdown_signal.as_mut() => break,
        }
    }
}

/// The production [`ConfigMapApi`]: get/create/replace the ConfigMap over the kube API. Plain async
/// kube calls on the daemon's runtime (the [`crate::runs::workpod::KubePodDispatcher`] pattern). Reached
/// only in-cluster; a test installs the fake, so this is compile-tested + exercised by an
/// `#[ignore]`d live test.
pub struct KubeConfigMapApi;

impl KubeConfigMapApi {
    async fn api(namespace: &str) -> Result<kube::Api<k8s_openapi::api::core::v1::ConfigMap>> {
        crate::install_crypto_provider();
        let client = kube::Client::try_default()
            .await
            .context("connecting to the Kubernetes API for the overrides ConfigMap")?;
        Ok(kube::Api::namespaced(client, namespace))
    }
}

#[async_trait::async_trait]
impl ConfigMapApi for KubeConfigMapApi {
    async fn get(&self, namespace: &str, name: &str) -> Result<Option<CmSnapshot>> {
        let api = Self::api(namespace).await?;
        match api.get_opt(name).await.context("getting the ConfigMap")? {
            None => Ok(None),
            Some(cm) => {
                let resource_version = cm.metadata.resource_version.unwrap_or_default();
                let payload = cm.data.as_ref().and_then(|d| d.get(OVERRIDES_KEY)).cloned();
                Ok(Some(CmSnapshot {
                    resource_version,
                    payload,
                }))
            }
        }
    }

    async fn create(&self, namespace: &str, name: &str, payload: &str) -> Result<()> {
        use k8s_openapi::api::core::v1::ConfigMap;
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
        let api = Self::api(namespace).await?;
        let cm = ConfigMap {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                ..Default::default()
            },
            data: Some(std::collections::BTreeMap::from([(
                OVERRIDES_KEY.to_string(),
                payload.to_string(),
            )])),
            ..Default::default()
        };
        api.create(&kube::api::PostParams::default(), &cm)
            .await
            .context("creating the overrides ConfigMap")?;
        Ok(())
    }

    async fn replace(
        &self,
        namespace: &str,
        name: &str,
        payload: &str,
        resource_version: &str,
    ) -> Result<ReplaceOutcome> {
        use k8s_openapi::api::core::v1::ConfigMap;
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
        let api = Self::api(namespace).await?;
        let cm = ConfigMap {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                // The resourceVersion guard: a replace with a stale rv is a 409 Conflict.
                resource_version: Some(resource_version.to_string()),
                ..Default::default()
            },
            data: Some(std::collections::BTreeMap::from([(
                OVERRIDES_KEY.to_string(),
                payload.to_string(),
            )])),
            ..Default::default()
        };
        match api
            .replace(name, &kube::api::PostParams::default(), &cm)
            .await
        {
            Ok(_) => Ok(ReplaceOutcome::Applied),
            Err(kube::Error::Api(ae)) if ae.code == 409 => Ok(ReplaceOutcome::Conflict),
            Err(e) => Err(anyhow::Error::new(e).context("replacing the overrides ConfigMap")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// An in-memory `ConfigMapApi` — a plain struct, not a mock framework — holding one CM's
    /// (resourceVersion, payload). Bumps the rv on every write so the optimistic-replace path is
    /// exercised for real.
    #[derive(Default)]
    struct FakeCm {
        state: Mutex<Option<(u64, String)>>,
    }

    impl FakeCm {
        fn with(payload: &str) -> Self {
            FakeCm {
                state: Mutex::new(Some((1, payload.to_string()))),
            }
        }
        fn set_payload(&self, payload: &str) {
            let mut g = self.state.lock().expect("lock");
            let rv = g.as_ref().map(|(rv, _)| rv + 1).unwrap_or(1);
            *g = Some((rv, payload.to_string()));
        }
    }

    #[async_trait::async_trait]
    impl ConfigMapApi for FakeCm {
        async fn get(&self, _ns: &str, _name: &str) -> Result<Option<CmSnapshot>> {
            Ok(self
                .state
                .lock()
                .expect("lock")
                .as_ref()
                .map(|(rv, p)| CmSnapshot {
                    resource_version: rv.to_string(),
                    payload: Some(p.clone()),
                }))
        }
        async fn create(&self, _ns: &str, _name: &str, payload: &str) -> Result<()> {
            *self.state.lock().expect("lock") = Some((1, payload.to_string()));
            Ok(())
        }
        async fn replace(
            &self,
            _ns: &str,
            _name: &str,
            payload: &str,
            resource_version: &str,
        ) -> Result<ReplaceOutcome> {
            let mut g = self.state.lock().expect("lock");
            let cur = g.as_ref().map(|(rv, _)| rv.to_string());
            if cur.as_deref() != Some(resource_version) {
                return Ok(ReplaceOutcome::Conflict);
            }
            let rv = g.as_ref().map(|(rv, _)| rv + 1).unwrap_or(1);
            *g = Some((rv, payload.to_string()));
            Ok(ReplaceOutcome::Applied)
        }
    }

    fn store_with(api: Arc<dyn ConfigMapApi>) -> ConfigStore {
        ConfigStore::new(
            crate::testing::cfg_from_args(["ctl"]).base_config(),
            DEFAULT_CONFIGMAP_NAME.to_string(),
            "ns".to_string(),
            api,
            None,
            None,
        )
    }

    #[test]
    fn empty_overrides_resolve_to_the_defaults() {
        // Hold the env lock: sibling tests set knob env vars, and base_cfg reads the environ.
        let _g = crate::ENV_LOCK.blocking_lock();
        let store = store_with(Arc::new(FakeCm::with("{}")));
        let eff = store.effective();
        // The Profile / ControllerCfg defaults.
        assert_eq!(eff.discovery_secs, 300);
        assert_eq!(eff.max_concurrent_pods, 2);
        assert_eq!(eff.daily_cost_ceiling, 50.0);
        assert!(!eff.allow_t3);
        assert!(eff.prescope_grounded);
        assert_eq!(eff.rank_horizon_days, 0);
        assert_eq!(eff.run_iterations, 6);
        assert_eq!(eff.run_max_cost, 25.0);
    }

    #[tokio::test]
    async fn run_iteration_and_budget_knobs_override_and_reject_bad_values() {
        let fake = Arc::new(FakeCm::with(
            r#"{"run_iterations": 10, "run_max_cost": 40.0}"#,
        ));
        let store = store_with(fake.clone());
        assert_eq!(store.reload_once().await, ReloadOutcome::Ok);
        assert_eq!(store.effective().run_iterations, 10);
        assert_eq!(store.effective().run_max_cost, 40.0);
        assert_eq!(store.source_of(Knob::RunIterations), Source::Override);
        assert_eq!(store.source_of(Knob::RunMaxCost), Source::Override);

        // 0 iterations would cut the run off immediately (the very bug we're fixing); a negative
        // budget is nonsense — both keep last-good.
        for bad in [r#"{"run_iterations": 0}"#, r#"{"run_max_cost": -1.0}"#] {
            fake.set_payload(bad);
            assert_eq!(store.reload_once().await, ReloadOutcome::Invalid, "{bad}");
            assert_eq!(store.effective().run_iterations, 10, "kept last-good");
            assert_eq!(store.effective().run_max_cost, 40.0, "kept last-good");
        }
    }

    #[tokio::test]
    async fn precedence_default_env_override_per_field() {
        let _g = crate::ENV_LOCK.lock().await;
        // max_concurrent_pods is env-set (so its base source is `env`); the rest stay default.
        unsafe {
            std::env::set_var("CONTROLLER_MAX_CONCURRENT_PODS", "5");
        }
        let cfg = crate::testing::cfg_from_args(["ctl"]);
        unsafe {
            std::env::remove_var("CONTROLLER_MAX_CONCURRENT_PODS");
        }
        assert_eq!(
            cfg.profile.max_concurrent_pods, 5,
            "clap read the env value"
        );

        // Build the store while the env var is set so EnvSources sees it.
        unsafe {
            std::env::set_var("CONTROLLER_MAX_CONCURRENT_PODS", "5");
        }
        let api = Arc::new(FakeCm::with(
            r#"{"max_concurrent_pods": 1, "allow_t3": true}"#,
        ));
        let store = ConfigStore::new(
            cfg.base_config(),
            DEFAULT_CONFIGMAP_NAME.to_string(),
            "ns".to_string(),
            api,
            None,
            None,
        );
        unsafe {
            std::env::remove_var("CONTROLLER_MAX_CONCURRENT_PODS");
        }
        assert_eq!(store.reload_once().await, ReloadOutcome::Ok);

        let eff = store.effective();
        // override wins over env
        assert_eq!(eff.max_concurrent_pods, 1);
        assert_eq!(store.source_of(Knob::MaxConcurrentPods), Source::Override);
        // override over default
        assert!(eff.allow_t3);
        assert_eq!(store.source_of(Knob::AllowT3), Source::Override);
        // untouched + never env-set: the default value, source default
        assert_eq!(eff.max_scopes_per_day, 20);
        assert_eq!(store.source_of(Knob::MaxScopesPerDay), Source::Default);
        assert_eq!(store.source_of(Knob::DailyCostCeiling), Source::Default);
    }

    #[tokio::test]
    async fn env_source_shows_when_not_overridden() {
        let _g = crate::ENV_LOCK.lock().await;
        unsafe {
            std::env::set_var("CONTROLLER_DAILY_COST_CEILING_USD", "12.5");
        }
        let cfg = crate::testing::cfg_from_args(["ctl"]);
        let store = ConfigStore::new(
            cfg.base_config(),
            DEFAULT_CONFIGMAP_NAME.to_string(),
            "ns".to_string(),
            Arc::new(FakeCm::with("{}")),
            None,
            None,
        );
        unsafe {
            std::env::remove_var("CONTROLLER_DAILY_COST_CEILING_USD");
        }
        assert_eq!(store.reload_once().await, ReloadOutcome::Ok);
        assert_eq!(store.effective().daily_cost_ceiling, 12.5);
        assert_eq!(store.source_of(Knob::DailyCostCeiling), Source::Env);
    }

    #[tokio::test]
    async fn malformed_json_keeps_last_good() {
        let fake = Arc::new(FakeCm::with(r#"{"max_concurrent_pods": 7}"#));
        let store = store_with(fake.clone());
        assert_eq!(store.reload_once().await, ReloadOutcome::Ok);
        assert_eq!(store.effective().max_concurrent_pods, 7);

        // Now the CM turns to garbage: keep the last-good 7, report Invalid.
        fake.set_payload("{ this is not json");
        assert_eq!(store.reload_once().await, ReloadOutcome::Invalid);
        assert_eq!(store.effective().max_concurrent_pods, 7);
    }

    #[tokio::test]
    async fn unknown_key_is_rejected_and_keeps_last_good() {
        let fake = Arc::new(FakeCm::with(r#"{"max_concurrent_pods": 3}"#));
        let store = store_with(fake.clone());
        assert_eq!(store.reload_once().await, ReloadOutcome::Ok);
        assert_eq!(store.effective().max_concurrent_pods, 3);

        fake.set_payload(r#"{"max_concurrent_pods": 9, "bogus_knob": 1}"#);
        assert_eq!(
            store.reload_once().await,
            ReloadOutcome::Invalid,
            "deny_unknown_fields rejects the typo'd key"
        );
        assert_eq!(
            store.effective().max_concurrent_pods,
            3,
            "the last-good value survives an invalid reload"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn invalid_reload_writes_an_audit_event(pool: sqlx::PgPool) {
        let events = crate::event_log::EventLog::new(pool);
        let fake = Arc::new(FakeCm::with(r#"{"max_concurrent_pods": 3}"#));
        let store = ConfigStore::new(
            crate::testing::cfg_from_args(["ctl"]).base_config(),
            DEFAULT_CONFIGMAP_NAME.to_string(),
            "ns".to_string(),
            fake.clone(),
            None,
            Some(events.clone()),
        );
        assert_eq!(store.reload_once().await, ReloadOutcome::Ok);
        fake.set_payload(r#"{"bogus": 1}"#);
        assert_eq!(store.reload_once().await, ReloadOutcome::Invalid);
        let logged = events.read_for_key("config").await.expect("read events");
        assert_eq!(logged.len(), 1, "one error event for the invalid reload");
        assert_eq!(logged[0].to, "invalid");
    }

    #[tokio::test]
    async fn scope_gaming_rounds_overrides_and_rejects_out_of_bounds() {
        let fake = Arc::new(FakeCm::with(r#"{"scope_gaming_rounds": 3}"#));
        let store = store_with(fake.clone());
        assert_eq!(store.reload_once().await, ReloadOutcome::Ok);
        assert_eq!(store.effective().scope_gaming_rounds, 3);
        assert_eq!(store.source_of(Knob::ScopeGamingRounds), Source::Override);

        // 0 is a real posture (review once, reject on first concern), not unset.
        fake.set_payload(r#"{"scope_gaming_rounds": 0}"#);
        assert_eq!(store.reload_once().await, ReloadOutcome::Ok);
        assert_eq!(store.effective().scope_gaming_rounds, 0);

        // 11 is a fat-fingered runaway; the whole set keeps last-good.
        fake.set_payload(r#"{"scope_gaming_rounds": 11}"#);
        assert_eq!(store.reload_once().await, ReloadOutcome::Invalid);
        assert_eq!(store.effective().scope_gaming_rounds, 0, "kept last-good");
    }

    #[tokio::test]
    async fn scope_skip_gaming_review_overrides() {
        let fake = Arc::new(FakeCm::with(r#"{"scope_skip_gaming_review": true}"#));
        let store = store_with(fake.clone());
        assert_eq!(store.reload_once().await, ReloadOutcome::Ok);
        assert!(store.effective().scope_skip_gaming_review);
        assert_eq!(
            store.source_of(Knob::ScopeSkipGamingReview),
            Source::Override
        );

        fake.set_payload("{}");
        assert_eq!(store.reload_once().await, ReloadOutcome::Ok);
        assert!(
            !store.effective().scope_skip_gaming_review,
            "clearing the override falls back to the off-by-default base"
        );
    }

    #[tokio::test]
    async fn out_of_bounds_value_keeps_last_good() {
        let fake = Arc::new(FakeCm::with(r#"{"daily_cost_ceiling": 25.0}"#));
        let store = store_with(fake.clone());
        assert_eq!(store.reload_once().await, ReloadOutcome::Ok);
        assert_eq!(store.effective().daily_cost_ceiling, 25.0);

        fake.set_payload(r#"{"daily_cost_ceiling": -1.0}"#);
        assert_eq!(store.reload_once().await, ReloadOutcome::Invalid);
        assert_eq!(store.effective().daily_cost_ceiling, 25.0);
    }

    #[tokio::test]
    async fn reload_swap_is_visible_to_a_reader_handle() {
        let fake = Arc::new(FakeCm::with("{}"));
        let store = store_with(fake.clone());
        // A second handle (the "reader") shares the same Arc-backed cache.
        let reader = store.clone();
        assert_eq!(reader.effective().max_scopes_per_day, 20);

        fake.set_payload(r#"{"max_scopes_per_day": 2}"#);
        assert_eq!(store.reload_once().await, ReloadOutcome::Ok);
        // The swap the watch performed is visible through the independent reader handle.
        assert_eq!(reader.effective().max_scopes_per_day, 2);
    }

    #[tokio::test]
    async fn ensure_configmap_creates_when_absent() {
        let fake = Arc::new(FakeCm::default());
        let store = store_with(fake.clone());
        store.ensure_configmap().await.expect("create");
        // The seed is an empty object.
        let snap = fake.get("ns", "n").await.expect("get").expect("exists");
        let parsed: OverrideSet =
            serde_json::from_str(&snap.payload.unwrap()).expect("valid empty seed");
        assert_eq!(parsed, OverrideSet::default());
    }

    #[tokio::test]
    async fn write_overrides_persists_and_applies_immediately() {
        let fake = Arc::new(FakeCm::with("{}"));
        let store = store_with(fake.clone());
        let next = OverrideSet {
            max_concurrent_pods: Some(4),
            allow_t3: Some(true),
            ..Default::default()
        };
        let eff = store.write_overrides(&next).await.expect("write");
        // Read-your-writes: the effective config reflects the write immediately (before any watch).
        assert_eq!(eff.max_concurrent_pods, 4);
        assert!(eff.allow_t3);
        // And it landed in the CM, so a fresh reload confirms the same thing.
        assert_eq!(store.reload_once().await, ReloadOutcome::Ok);
        assert_eq!(store.effective().max_concurrent_pods, 4);
    }

    #[test]
    fn changed_keys_diffs_the_override_set() {
        let prev = OverrideSet {
            max_concurrent_pods: Some(2),
            ..Default::default()
        };
        let next = OverrideSet {
            max_concurrent_pods: Some(5),
            allow_t3: Some(true),
            ..Default::default()
        };
        let mut changed = next.changed_keys(&prev);
        changed.sort_unstable();
        assert_eq!(changed, vec!["allow_t3", "max_concurrent_pods"]);
    }

    #[test]
    fn every_knob_name_is_an_override_set_field() {
        let json = serde_json::to_value(OverrideSet::default()).expect("serialize");
        let obj = json.as_object().expect("an object");
        for knob in Knob::ALL {
            assert!(
                obj.contains_key(knob.name()),
                "Knob::{knob:?} names {:?}, which is not an OverrideSet field",
                knob.name()
            );
        }
        assert_eq!(obj.len(), Knob::ALL.len());
    }

    #[test]
    fn validate_collects_all_rejections() {
        let bad = OverrideSet {
            discovery_secs: Some(0),
            per_reconcile_cost: Some(-1.0),
            max_concurrent_pods: Some(1000),
            ..Default::default()
        };
        let errs = bad.validate().expect_err("should reject");
        assert_eq!(errs.len(), 3, "one message per bad field: {errs:?}");
    }

    #[test]
    fn validate_rejects_a_garbage_tier_in_allowed_tiers() {
        let bad = OverrideSet {
            allowed_tiers: Some(vec!["t0".to_string(), "t9".to_string()]),
            ..Default::default()
        };
        let errs = bad.validate().expect_err("should reject");
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("allowed_tiers"), "{errs:?}");
        assert!(errs[0].contains("t9"), "{errs:?}");
    }

    #[test]
    fn validate_accepts_every_parseable_tier_case_insensitively() {
        let ok = OverrideSet {
            allowed_tiers: Some(vec!["t0".to_string(), "T1".to_string(), "t2".to_string()]),
            ..Default::default()
        };
        ok.validate().expect("t0/T1/t2 all parse");
    }

    #[test]
    fn resolve_parses_allowed_tiers_override_and_falls_back_to_base_when_unset() {
        let base = BaseConfig {
            allow_draft_head_schedules: false,
            discovery_secs: 300,
            per_reconcile_cost: 10.0,
            max_concurrent_pods: 2,
            max_scopes_per_day: 20,
            daily_cost_ceiling: 50.0,
            rank_cost_fallback_usd: 0.05,
            grounded_rank_pod_cap: 4,
            grounded_rank_daily_turns: 50,
            failed_pod_keep: 20,
            scope_gaming_rounds: 1,
            scope_skip_gaming_review: false,
            allow_t3: false,
            prescope_grounded: true,
            rank_horizon_days: 0,
            allowed_tiers: vec![Tier::T0, Tier::T1],
            run_iterations: 6,
            run_max_cost: 25.0,
        };
        // No override: the base's allowed_tiers stands.
        let eff = EffectiveConfig::resolve(&base, &OverrideSet::default());
        assert_eq!(eff.allowed_tiers, vec![Tier::T0, Tier::T1]);

        // An override narrows it to t0 only.
        let narrowed = OverrideSet {
            allowed_tiers: Some(vec!["t0".to_string()]),
            ..Default::default()
        };
        let eff = EffectiveConfig::resolve(&base, &narrowed);
        assert_eq!(eff.allowed_tiers, vec![Tier::T0]);
    }

    #[tokio::test]
    async fn views_tag_every_knob_overridable_with_a_source() {
        let store = store_with(Arc::new(FakeCm::with(r#"{"allow_t3": true}"#)));
        assert_eq!(store.reload_once().await, ReloadOutcome::Ok);
        let views = store.views();
        assert_eq!(views.len(), Knob::ALL.len());
        assert!(views.iter().all(|v| v.overridable));
        let t3 = views
            .iter()
            .find(|v| v.name == "allow_t3")
            .expect("t3 view");
        assert_eq!(t3.source, Source::Override);
        assert_eq!(t3.value, serde_json::Value::Bool(true));
    }
}
