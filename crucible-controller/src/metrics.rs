//! The controller's Prometheus surface: a [`Metrics`] context struct that owns one registry and
//! every metric family, hung on the [`crate::client::Db`] (the object already threaded through every
//! choke point) and rendered at `GET /metrics` on the existing axum server.
//!
//! Two kinds of metric live here, split by how they're kept fresh:
//!   * **Event metrics** (counters + histograms) are moved at the choke point that observes the
//!     event — spend at [`crate::client::Db::ledger_append`], verdicts in `apply_verdict`, runs in
//!     `complete_run`, turns + queue-wait in [`crate::runs::workpod`], reconcile duration around
//!     [`crate::issues::reconcile::reconcile`]. They live for the process's life and only ever go up.
//!   * **State gauges** that mirror the DB (workpods by kind/state, approvals pending, the day's spend
//!     and its ceiling) are recomputed on scrape in [`Metrics::gather`] — a single query per family
//!     beats sprinkling gauge writes across every transition and can never drift from the table.
//!
//! There is NO global registry: `Metrics` is Clone (an `Arc` inside) and passed explicitly. The
//! entrypoint builds one and attaches it to the ledger; tests build their own. A `Db` with no
//! metrics attached (every test that doesn't opt in) simply skips the increments.

use anyhow::{Context, Result};
use prometheus::{
    CounterVec, Encoder, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec,
    IntGaugeVec, Opts, Registry, TextEncoder,
};
use std::sync::Arc;

/// The content type of the Prometheus text exposition format, echoed on the `/metrics` response.
pub(crate) const TEXT_CONTENT_TYPE: &str = "text/plain; version=0.0.4";

/// The metric registry plus every family, behind one `Arc` so cloning a [`Db`] is cheap.
#[derive(Clone)]
pub struct Metrics(Arc<Inner>);

struct Inner {
    registry: Registry,

    // --- run + turn outcomes (moved at ingest / dispatch) -------------------------------------
    runs_total: IntCounterVec,
    run_duration_seconds: Histogram,
    run_iterations: Histogram,
    run_best_score: Histogram,
    turns_total: IntCounterVec,
    turn_duration_seconds: HistogramVec,

    // --- declarative image builds ------------------------------------------------------------
    builds_total: IntCounterVec,

    // --- ranking + spend ---------------------------------------------------------------------
    rank_verdicts_total: IntCounterVec,
    spend_usd_total: CounterVec,

    // --- workpods + approvals (durations moved; counts refreshed on scrape) -----------------------
    workpod_queue_wait_seconds: Histogram,
    approval_latency_seconds: Histogram,

    // --- reconcile + upstream ----------------------------------------------------------------
    reconcile_duration_seconds: HistogramVec,
    github_api_requests_total: IntCounterVec,
    ingest_failures_total: IntCounter,

    // --- live relay (read-only SSE over the loop-pod control bridge) --------------------------
    live_streams: prometheus::IntGauge,
    live_relay_errors_total: IntCounterVec,

    // --- report-site parity: artifact proxy + parquet exports ---------------------------------
    artifact_requests_total: IntCounterVec,
    flow_enriched_total: IntCounterVec,
    exports_total: IntCounterVec,

    // --- runtime config overrides (Lane O2) ---------------------------------------------------
    config_override_active: IntGaugeVec,
    config_reloads_total: IntCounterVec,

    // --- state gauges, set on scrape from the DB ---------------------------------------------
    daily_spend_usd: prometheus::Gauge,
    daily_ceiling_usd: prometheus::Gauge,
    workpods: IntGaugeVec,
    approvals_pending: IntGaugeVec,
    builds: IntGaugeVec,
}

/// Buckets sized minutes-to-hours: a loop run is a multi-minute-to-multi-hour affair.
fn run_duration_buckets() -> Vec<f64> {
    vec![
        30.0, 60.0, 120.0, 300.0, 600.0, 1200.0, 1800.0, 3600.0, 7200.0, 14400.0, 28800.0,
    ]
}

/// Buckets sized seconds-to-tens-of-minutes: one bounded agent turn (a grounded-rank pod).
fn turn_duration_buckets() -> Vec<f64> {
    vec![
        1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1200.0, 1800.0,
    ]
}

impl Metrics {
    /// Build a fresh registry and register every family. Fallible only in principle (the names are
    /// static and valid, the registry is fresh, so a duplicate/format error can't happen at
    /// runtime) — surfaced as a `Result` so the caller never has to `unwrap`.
    pub fn new() -> Result<Self> {
        let registry = Registry::new();

        let runs_total = IntCounterVec::new(
            Opts::new(
                "crucible_runs_total",
                "Loop runs ingested, by outcome and repo.",
            ),
            &["outcome", "repo"],
        )?;
        let run_duration_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "crucible_run_duration_seconds",
                "Wall-clock duration of an ingested loop run.",
            )
            .buckets(run_duration_buckets()),
        )?;
        let run_iterations = Histogram::with_opts(
            HistogramOpts::new(
                "crucible_run_iterations",
                "Deep-loop iterations recorded for an ingested run.",
            )
            .buckets(vec![1.0, 2.0, 3.0, 5.0, 8.0, 13.0, 21.0, 34.0, 55.0]),
        )?;
        let run_best_score = Histogram::with_opts(
            HistogramOpts::new(
                "crucible_run_best_score",
                "Best score an ingested run reached (domain units; exponential buckets).",
            )
            .buckets(
                prometheus::exponential_buckets(1.0, 2.0, 16).context("run_best_score buckets")?,
            ),
        )?;
        let turns_total = IntCounterVec::new(
            Opts::new(
                "crucible_turns_total",
                "Controller-dispatched work turns, by kind and outcome.",
            ),
            &["kind", "outcome"],
        )?;
        let turn_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "crucible_turn_duration_seconds",
                "Wall-clock duration of a dispatched agent turn, by kind.",
            )
            .buckets(turn_duration_buckets()),
            &["kind"],
        )?;

        let builds_total = IntCounterVec::new(
            Opts::new(
                "crucible_builds_total",
                "Declarative image builds by outcome (dispatched/succeeded/failed/timed-out/capped).",
            ),
            &["outcome"],
        )?;

        let rank_verdicts_total = IntCounterVec::new(
            Opts::new(
                "crucible_rank_verdicts_total",
                "Ranking verdicts applied, by tier, disposition, confidence and whether grounded.",
            ),
            &["tier", "disposition", "confidence", "grounded"],
        )?;
        let spend_usd_total = CounterVec::new(
            Opts::new("crucible_spend_usd_total", "USD ledgered, by cost tag."),
            &["cost_tag"],
        )?;

        let workpod_queue_wait_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "crucible_workpod_queue_wait_seconds",
                "Time a work turn spent queued before it was promoted to running.",
            )
            .buckets(vec![
                1.0, 5.0, 15.0, 60.0, 300.0, 900.0, 1800.0, 3600.0, 21600.0,
            ]),
        )?;
        let approval_latency_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "crucible_approval_latency_seconds",
                "Human-approval turnaround, from a scope entering awaiting-approval to its approval.",
            )
            .buckets(vec![
                60.0, 300.0, 900.0, 3600.0, 14400.0, 43200.0, 86400.0, 259200.0,
            ]),
        )?;

        let reconcile_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "crucible_reconcile_duration_seconds",
                "Duration of one reconcile step, by outcome.",
            )
            .buckets(vec![
                0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 15.0, 30.0, 60.0, 120.0,
            ]),
            &["outcome"],
        )?;
        let github_api_requests_total = IntCounterVec::new(
            Opts::new(
                "crucible_github_api_requests_total",
                "GitHub API requests the controller made, by outcome.",
            ),
            &["outcome"],
        )?;
        let ingest_failures_total = IntCounter::new(
            "crucible_ingest_failures_total",
            "Run-completion ingests that failed to fold a session log.",
        )?;

        let live_streams = prometheus::IntGauge::new(
            "crucible_live_streams",
            "Open read-only live-relay SSE streams dialing loop-pod control bridges.",
        )?;
        let live_relay_errors_total = IntCounterVec::new(
            Opts::new(
                "crucible_live_relay_errors_total",
                "Live-relay failures, by reason (connect/resolve/io).",
            ),
            &["reason"],
        )?;

        let artifact_requests_total = IntCounterVec::new(
            Opts::new(
                "crucible_artifact_requests_total",
                "Run-artifact proxy requests, by outcome (ok/rejected/not_found/fetch_error/…).",
            ),
            &["outcome"],
        )?;
        let flow_enriched_total = IntCounterVec::new(
            Opts::new(
                "crucible_flow_enriched_total",
                "Span-enriched flow renders requested, by outcome (ok/cache_hit/render_error/…).",
            ),
            &["outcome"],
        )?;
        let exports_total = IntCounterVec::new(
            Opts::new(
                "crucible_exports_total",
                "On-demand parquet analytics exports served, by table (runs/iterations).",
            ),
            &["table"],
        )?;

        let config_override_active = IntGaugeVec::new(
            Opts::new(
                "crucible_config_override_active",
                "1 when a config knob is currently set by a runtime override, else 0, by knob.",
            ),
            &["knob"],
        )?;
        let config_reloads_total = IntCounterVec::new(
            Opts::new(
                "crucible_config_reloads_total",
                "Overrides ConfigMap reloads, by outcome (ok/invalid/error).",
            ),
            &["outcome"],
        )?;

        let daily_spend_usd = prometheus::Gauge::new(
            "crucible_daily_spend_usd",
            "USD ledgered so far on the current UTC day (refreshed on scrape).",
        )?;
        let daily_ceiling_usd = prometheus::Gauge::new(
            "crucible_daily_ceiling_usd",
            "The global daily spend ceiling in USD (refreshed on scrape).",
        )?;
        let workpods = IntGaugeVec::new(
            Opts::new(
                "crucible_workpods",
                "Work pods tracked in the ledger, by kind and state (queued = queue depth).",
            ),
            &["kind", "state"],
        )?;
        let approvals_pending = IntGaugeVec::new(
            Opts::new(
                "crucible_approvals_pending",
                "Human approvals currently awaiting action.",
            ),
            &["approval"],
        )?;
        let builds = IntGaugeVec::new(
            Opts::new(
                "crucible_builds",
                "Image builds tracked in the ledger, by backend and state (in-flight = pending+dispatched).",
            ),
            &["backend", "state"],
        )?;

        registry.register(Box::new(runs_total.clone()))?;
        registry.register(Box::new(run_duration_seconds.clone()))?;
        registry.register(Box::new(run_iterations.clone()))?;
        registry.register(Box::new(run_best_score.clone()))?;
        registry.register(Box::new(turns_total.clone()))?;
        registry.register(Box::new(turn_duration_seconds.clone()))?;
        registry.register(Box::new(builds_total.clone()))?;
        registry.register(Box::new(rank_verdicts_total.clone()))?;
        registry.register(Box::new(spend_usd_total.clone()))?;
        registry.register(Box::new(workpod_queue_wait_seconds.clone()))?;
        registry.register(Box::new(approval_latency_seconds.clone()))?;
        registry.register(Box::new(reconcile_duration_seconds.clone()))?;
        registry.register(Box::new(github_api_requests_total.clone()))?;
        registry.register(Box::new(ingest_failures_total.clone()))?;
        registry.register(Box::new(live_streams.clone()))?;
        registry.register(Box::new(live_relay_errors_total.clone()))?;
        registry.register(Box::new(artifact_requests_total.clone()))?;
        registry.register(Box::new(flow_enriched_total.clone()))?;
        registry.register(Box::new(exports_total.clone()))?;
        registry.register(Box::new(config_override_active.clone()))?;
        registry.register(Box::new(config_reloads_total.clone()))?;
        registry.register(Box::new(daily_spend_usd.clone()))?;
        registry.register(Box::new(daily_ceiling_usd.clone()))?;
        registry.register(Box::new(workpods.clone()))?;
        registry.register(Box::new(approvals_pending.clone()))?;
        registry.register(Box::new(builds.clone()))?;

        Ok(Metrics(Arc::new(Inner {
            registry,
            runs_total,
            run_duration_seconds,
            run_iterations,
            run_best_score,
            turns_total,
            turn_duration_seconds,
            builds_total,
            rank_verdicts_total,
            spend_usd_total,
            workpod_queue_wait_seconds,
            approval_latency_seconds,
            reconcile_duration_seconds,
            github_api_requests_total,
            ingest_failures_total,
            live_streams,
            live_relay_errors_total,
            artifact_requests_total,
            flow_enriched_total,
            exports_total,
            config_override_active,
            config_reloads_total,
            daily_spend_usd,
            daily_ceiling_usd,
            workpods,
            approvals_pending,
            builds,
        })))
    }

    /// A run-artifact proxy request finished with `outcome` (ok/rejected/run_not_found/not_found/
    /// no_evidence/not_modified/fetch_error).
    pub(crate) fn record_artifact_request(&self, outcome: &str) {
        self.0
            .artifact_requests_total
            .with_label_values(&[outcome])
            .inc();
    }

    /// A span-enriched flow request finished with `outcome` (ok/cache_hit/bad_trace/no_evidence/
    /// no_creds/unsupported/fetch_error/render_error/not_modified).
    pub(crate) fn record_flow_enriched(&self, outcome: &str) {
        self.0
            .flow_enriched_total
            .with_label_values(&[outcome])
            .inc();
    }

    /// A parquet analytics export for `table` (runs/iterations) was served.
    pub(crate) fn record_export(&self, table: &str) {
        self.0.exports_total.with_label_values(&[table]).inc();
    }

    // --- choke-point moves --------------------------------------------------------------------

    /// A finished run was ingested (`complete_run`). `iterations` is the deep-loop candidate count.
    pub(crate) fn record_run(
        &self,
        outcome: &str,
        repo: &str,
        duration_secs: Option<f64>,
        iterations: u64,
        best_score: Option<f64>,
    ) {
        self.0.runs_total.with_label_values(&[outcome, repo]).inc();
        if let Some(d) = duration_secs.filter(|d| d.is_finite() && *d >= 0.0) {
            self.0.run_duration_seconds.observe(d);
        }
        self.0.run_iterations.observe(iterations as f64);
        if let Some(s) = best_score.filter(|s| s.is_finite()) {
            self.0.run_best_score.observe(s);
        }
    }

    /// A controller-dispatched work turn reached an outcome (`crate::runs::workpod` dispatch).
    pub(crate) fn record_turn(&self, kind: &str, outcome: &str) {
        self.0.turns_total.with_label_values(&[kind, outcome]).inc();
    }

    /// A declarative image build reached `outcome` (`dispatched`/`succeeded`/`failed`/`timed-out`/
    /// `capped`) — moved at the [`crate::builds::lifecycle`] dispatch/poll choke points.
    pub(crate) fn record_build(&self, outcome: &str) {
        self.0.builds_total.with_label_values(&[outcome]).inc();
    }

    /// A dispatched turn's measured wall-clock duration.
    pub(crate) fn observe_turn_duration(&self, kind: &str, secs: f64) {
        if secs.is_finite() && secs >= 0.0 {
            self.0
                .turn_duration_seconds
                .with_label_values(&[kind])
                .observe(secs);
        }
    }

    /// A ranking verdict was applied (`apply_verdict`'s CAS-winning branches).
    pub(crate) fn record_verdict(
        &self,
        tier: &str,
        disposition: &str,
        confidence: &str,
        grounded: bool,
    ) {
        self.0
            .rank_verdicts_total
            .with_label_values(&[tier, disposition, confidence, bool_label(grounded)])
            .inc();
    }

    /// USD was ledgered under `cost_tag` ([`crate::client::Db::ledger_append`]).
    pub(crate) fn record_spend(&self, cost_tag: &str, usd: f64) {
        // A counter can't decrease; a capped/zero row moves nothing, a negative would panic.
        if usd.is_finite() && usd > 0.0 {
            self.0
                .spend_usd_total
                .with_label_values(&[cost_tag])
                .inc_by(usd);
        }
    }

    /// A queued turn was promoted; `secs` is how long it waited.
    pub(crate) fn observe_queue_wait(&self, secs: f64) {
        if secs.is_finite() && secs >= 0.0 {
            self.0.workpod_queue_wait_seconds.observe(secs);
        }
    }

    /// A human approval landed; `secs` is the turnaround since the approval opened.
    pub(crate) fn observe_approval_latency(&self, secs: f64) {
        if secs.is_finite() && secs >= 0.0 {
            self.0.approval_latency_seconds.observe(secs);
        }
    }

    /// One reconcile step finished (`ok`/`error`) in `secs`.
    pub(crate) fn observe_reconcile(&self, outcome: &str, secs: f64) {
        if secs.is_finite() && secs >= 0.0 {
            self.0
                .reconcile_duration_seconds
                .with_label_values(&[outcome])
                .observe(secs);
        }
    }

    /// A GitHub API call completed (`ok`/`error`).
    pub(crate) fn record_github(&self, ok: bool) {
        self.0
            .github_api_requests_total
            .with_label_values(&[if ok { "ok" } else { "error" }])
            .inc();
    }

    /// A run-completion ingest failed to fold its session log.
    pub(crate) fn record_ingest_failure(&self) {
        self.0.ingest_failures_total.inc();
    }

    /// A read-only live-relay SSE stream opened (dialing a loop-pod control bridge).
    pub(crate) fn live_stream_opened(&self) {
        self.0.live_streams.inc();
    }

    /// A live-relay SSE stream closed (client disconnected or the bridge ended). Paired with
    /// [`Metrics::live_stream_opened`] via a drop guard so the gauge can never leak.
    pub(crate) fn live_stream_closed(&self) {
        self.0.live_streams.dec();
    }

    /// Open a live-relay stream on the gauge and hand back the guard that closes it on drop, so a
    /// disconnect or any early return can never leak the count.
    pub(crate) fn live_stream_guard(&self) -> LiveStreamGuard {
        self.live_stream_opened();
        LiveStreamGuard(self.clone())
    }

    /// A live relay failed for `reason` (`connect`/`resolve`/`io`).
    pub(crate) fn record_live_error(&self, reason: &str) {
        self.0
            .live_relay_errors_total
            .with_label_values(&[reason])
            .inc();
    }

    /// A runtime overrides ConfigMap reload finished (`ok`/`invalid`/`error`).
    pub(crate) fn record_config_reload(&self, outcome: &str) {
        self.0
            .config_reloads_total
            .with_label_values(&[outcome])
            .inc();
    }

    /// Set the override-active gauge for `knob` (1 when overridden, else 0). Moved on every override
    /// swap so the gauge mirrors the live override set.
    pub(crate) fn set_config_override_active(&self, knob: &str, active: bool) {
        self.0
            .config_override_active
            .with_label_values(&[knob])
            .set(i64::from(active));
    }

    // --- scrape -------------------------------------------------------------------------------

    /// Set the DB-mirrored gauges from `state` and encode the whole registry in the Prometheus
    /// text format. The `/metrics` route reads the state and calls this on every scrape.
    pub(crate) fn gather(&self, state: GaugeState) -> Result<String> {
        self.0.daily_spend_usd.set(state.daily_spend_usd);
        if let Some(c) = state.daily_ceiling_usd {
            self.0.daily_ceiling_usd.set(c);
        }
        // Reset each vec so a family that dropped to zero disappears, then set every observed pair.
        self.0.workpods.reset();
        for (kind, pod_state, n) in &state.workpods {
            self.0
                .workpods
                .with_label_values(&[kind.as_str(), pod_state])
                .set(*n);
        }
        self.0.approvals_pending.reset();
        self.0
            .approvals_pending
            .with_label_values(&["approval"])
            .set(state.approvals_pending);
        self.0.builds.reset();
        for (backend, build_state, n) in &state.builds {
            self.0
                .builds
                .with_label_values(&[backend, build_state])
                .set(*n);
        }
        let mut buf = Vec::new();
        let encoder = TextEncoder::new();
        encoder
            .encode(&self.0.registry.gather(), &mut buf)
            .context("encoding the Prometheus text exposition")?;
        String::from_utf8(buf).context("Prometheus text exposition was not UTF-8")
    }

    #[cfg(test)]
    pub(crate) fn registry(&self) -> &Registry {
        &self.0.registry
    }

    /// The current `crucible_turns_total{kind,outcome}` value — a delta-assertion helper for the
    /// differential harness (no injectable metrics recorder exists, so tests read the real counter
    /// before/after driving a fixture). Reading via `with_label_values` never increments; an
    /// untouched series reads 0.
    #[cfg(test)]
    pub(crate) fn turns_total(&self, kind: &str, outcome: &str) -> u64 {
        self.0.turns_total.with_label_values(&[kind, outcome]).get()
    }

    /// The current `crucible_turn_duration_seconds{kind}` sample COUNT (not the sum) — enough to
    /// assert whether `observe_turn_duration` fired for `kind`, without depending on wall-clock
    /// duration values.
    #[cfg(test)]
    pub(crate) fn turn_duration_samples(&self, kind: &str) -> u64 {
        self.0
            .turn_duration_seconds
            .with_label_values(&[kind])
            .get_sample_count()
    }
}

/// Holds one open live-relay stream on the gauge; dropping it closes the stream. Built through
/// [`Metrics::live_stream_guard`].
pub(crate) struct LiveStreamGuard(Metrics);

impl Drop for LiveStreamGuard {
    fn drop(&mut self) {
        self.0.live_stream_closed();
    }
}

/// The stable string a `bool` label renders as (never a `Debug`-formatted `true`/`false` drift).
fn bool_label(b: bool) -> &'static str {
    if b { "true" } else { "false" }
}

/// The DB-mirrored gauge values one scrape sets before rendering. Read by the `/metrics` route,
/// which owns the queries; the registry itself never touches the database.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct GaugeState {
    pub(crate) daily_spend_usd: f64,
    pub(crate) daily_ceiling_usd: Option<f64>,
    /// `(kind, state, count)` for every observed work-pod pair.
    pub(crate) workpods: Vec<(String, &'static str, i64)>,
    pub(crate) approvals_pending: i64,
    /// `(backend, state, count)` for every observed build pair.
    pub(crate) builds: Vec<(&'static str, &'static str, i64)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Db;

    #[test]
    fn new_registers_every_family_without_collision() {
        let m = Metrics::new().expect("metrics build");
        // Touch one series per family so `gather` emits it.
        m.record_run("finished", "o/r", Some(120.0), 3, Some(240.0));
        m.record_turn("grounded-rank", "verdict");
        m.observe_turn_duration("grounded-rank", 12.0);
        m.record_verdict("T0", "tier", "high", true);
        m.record_spend("rank", 0.05);
        m.observe_queue_wait(30.0);
        m.observe_approval_latency(3600.0);
        m.observe_reconcile("ok", 0.2);
        m.record_github(true);
        m.record_ingest_failure();
        m.live_stream_opened();
        m.record_live_error("io");
        m.record_config_reload("ok");
        m.set_config_override_active("allow_t3", true);

        let encoder = TextEncoder::new();
        let mut buf = Vec::new();
        encoder
            .encode(&m.registry().gather(), &mut buf)
            .expect("encode");
        let text = String::from_utf8(buf).expect("utf8");

        for family in [
            "crucible_runs_total",
            "crucible_run_duration_seconds",
            "crucible_run_iterations",
            "crucible_run_best_score",
            "crucible_turns_total",
            "crucible_turn_duration_seconds",
            "crucible_rank_verdicts_total",
            "crucible_spend_usd_total",
            "crucible_workpod_queue_wait_seconds",
            "crucible_approval_latency_seconds",
            "crucible_reconcile_duration_seconds",
            "crucible_github_api_requests_total",
            "crucible_ingest_failures_total",
            "crucible_live_streams",
            "crucible_live_relay_errors_total",
            "crucible_config_reloads_total",
            "crucible_config_override_active",
        ] {
            assert!(text.contains(family), "scrape is missing {family}:\n{text}");
        }
        assert!(text.contains(r#"cost_tag="rank""#));
        assert!(text.contains(r#"grounded="true""#));
    }

    struct NoopSink;
    impl crate::daemon::queue::OverrideSink for NoopSink {
        fn submit(&self, _ov: crate::daemon::queue::Override) {}
    }

    /// The `/metrics` route returns 200, the Prometheus text content type, and a rendered family —
    /// with a DB-mirrored gauge (the ceiling) reflecting the caps on the state.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn metrics_route_renders_text_exposition(pool: sqlx::PgPool) -> anyhow::Result<()> {
        use tower::ServiceExt;
        let metrics = Metrics::new()?;
        let db = Db::new(pool).with_metrics(metrics.clone());
        // Book some spend so a counter family is non-empty.
        db.ledger_append(None, "rank", 0.05).await?;

        let state = crate::api::state::ApiState {
            caps: Some(crate::api::state::Caps {
                max_concurrent_pods: 2,
                max_scopes_per_day: 20,
                daily_cost_ceiling: 50.0,
            }),
            ..crate::api::state::ApiState::test(db, std::sync::Arc::new(NoopSink))
        };
        let app = crate::api::metrics::router(state);
        let res = app
            .oneshot(
                axum::http::Request::get("/metrics")
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("route");
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        assert_eq!(
            res.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some(TEXT_CONTENT_TYPE)
        );
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(
            text.contains(r#"crucible_spend_usd_total{cost_tag="rank"} 0.05"#),
            "{text}"
        );
        // The scrape-time gauge picked up the ceiling from the caps on the state.
        assert!(text.contains("crucible_daily_ceiling_usd 50"), "{text}");
        Ok(())
    }

    #[test]
    fn spend_counter_ignores_zero_and_negative() {
        let m = Metrics::new().expect("metrics build");
        m.record_spend("capped", 0.0);
        m.record_spend("weird", -1.0);
        m.record_spend("rank", 0.25);
        let text = {
            let encoder = TextEncoder::new();
            let mut buf = Vec::new();
            encoder
                .encode(&m.registry().gather(), &mut buf)
                .expect("encode");
            String::from_utf8(buf).expect("utf8")
        };
        // Only the positive move created a series.
        assert!(
            text.contains(r#"crucible_spend_usd_total{cost_tag="rank"} 0.25"#),
            "{text}"
        );
        assert!(!text.contains(r#"cost_tag="capped""#));
        assert!(!text.contains(r#"cost_tag="weird""#));
    }
}
