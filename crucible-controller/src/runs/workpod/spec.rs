//! `TurnSpec` — the two agent-turn kinds' (grounded-rank, scope) shared four-phase dispatch/collect
//! machine, factored out from the near-duplicate bodies in `run.rs`. Scoped to `AgentTurn` ONLY:
//! `WorkKind::Run` is deliberately not a `TurnSpec` (it books no `work_pods`-row CAS, has no
//! adopt-on-restart peek, renders a Pod+ConfigMap pair, and books `record_run` not `record_turn` —
//! see the design's escape-hatch test).

#![allow(clippy::disallowed_macros)]

use crate::client::Db;
use crate::config::ControllerCfg;
use crate::issues::engine;
use crate::runs::workpod::*;
use anyhow::{Context, Result};
use crucible::deploy::DigestResolver;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// One turn pod watched to completion: either a parseable result, or a named reason why not
/// (failed/timed-out/unparseable/(scope) pack-not-landed).
pub(crate) enum TurnRunOutcome<R> {
    Result(R),
    NoResult { reason: String },
}

/// One turn folded through the single-booking CAS.
pub(crate) enum TurnCollected<R> {
    /// CAS won: cost booked, succeeded pod GC'd.
    Result { result: R, pod_name: String },
    /// CAS won: row failed, pod retained (debuggable).
    Failed { reason: String },
    /// CAS lost: a concurrent collector already owns this turn.
    AlreadyCollected,
}

/// What one non-blocking dispatch decided.
pub(crate) enum TurnDispatch<R> {
    Launched,
    Collected(TurnCollected<R>),
    Failed { reason: String },
}

/// A launch (render-or-create) failure, the single rendering point for `work_pods.error` and (scope
/// only) `ScopeOutcome::Failed`. Deliberately has NO join/panic variant: a `spawn_blocking` JoinError
/// is the OUTER error of [`spawn_turn_pod`] and `?`-propagates out of the caller untouched — it must
/// never become a `LaunchFailure`, or it would flow into `fail_launch`'s CAS-fail, wrongly turning a
/// "row stays Running for the startup sweep" outcome into a CAS-failed row.
pub(crate) enum LaunchFailure {
    /// The render (render + stamp) step failed. No added framing: the render error's own text is
    /// the whole message.
    RenderFailed(String),
    /// The dispatcher's pod create failed. `pod_noun` is `spec.pod_noun()` ("turn pod" / "scope turn
    /// pod"), rendered as `creating the {pod_noun}`.
    PodCreateFailed {
        pod_noun: &'static str,
        detail: String,
    },
}

impl std::fmt::Display for LaunchFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LaunchFailure::RenderFailed(detail) => write!(f, "{detail}"),
            LaunchFailure::PodCreateFailed { pod_noun, detail } => {
                write!(f, "creating the {pod_noun}: {detail}")
            }
        }
    }
}

/// The per-kind deltas of one agent-turn kind. Static-dispatch only (concrete ZST specs; no
/// `async_trait`, no `dyn TurnSpec` — `Self::Result` isn't dyn-compatible, and two ZST kinds make
/// static dispatch free). The async methods await the already-`Send`-boxed `dyn PodDispatcher`
/// (and, for `parse`, the drop-box store); everything else is sync.
pub(crate) trait TurnSpec: Send + Sync {
    type Result: Send;

    fn kind(&self) -> WorkKind;
    fn pod_name(&self, issue_key: &str) -> String;
    fn pod_noun(&self) -> &'static str;
    fn deadline(&self, cfg: &ControllerCfg) -> Duration;
    fn sandbox_image(&self, cfg: &ControllerCfg) -> Result<String>;
    fn build_work_pod_spec(
        &self,
        cfg: &ControllerCfg,
        issue_key: &str,
        repo_url: &str,
        max_cost: f64,
        sandbox_image: String,
        inputs: TurnInputs,
    ) -> WorkPodSpec;
    fn cost_usd(&self, r: &Self::Result) -> f64;
    fn summary(&self, r: &Self::Result) -> String;
    /// The metric literal `record_turn` books on success ("verdict" / "report") — STAYS STABLE.
    fn success_metric(&self) -> &'static str;
    async fn parse(
        &self,
        terminal: &TerminalState,
        logs: &str,
        pool: &sqlx::PgPool,
        pod: &str,
    ) -> Result<TurnRunOutcome<Self::Result>>;

    /// Watch an EXISTING pod to a parsed result. Subsumes `run_turn_pod` + `run_scope_turn_pod`.
    async fn watch(
        &self,
        d: &dyn PodDispatcher,
        cfg: &ControllerCfg,
        pool: &sqlx::PgPool,
        cluster: &str,
        ns: &str,
        name: &str,
    ) -> Result<TurnRunOutcome<Self::Result>> {
        let terminal = d
            .await_terminal(cluster, ns, name, self.deadline(cfg))
            .await
            .with_context(|| format!("watching the {}", self.pod_noun()))?;
        if terminal.phase == TurnPhase::TimedOut {
            return Ok(TurnRunOutcome::NoResult {
                reason: format!(
                    "{} {name} did not finish within {}s",
                    self.pod_noun(),
                    self.deadline(cfg).as_secs()
                ),
            });
        }
        let logs = d
            .logs(cluster, ns, name)
            .await
            .with_context(|| format!("reading the {}'s logs", self.pod_noun()))?;
        self.parse(&terminal, &logs, pool, name).await
    }

    /// Adopt an orphaned running pod for this issue → fold through the one collect tail.
    async fn adopt(
        &self,
        db: &Db,
        cfg: &ControllerCfg,
        d: Arc<dyn PodDispatcher>,
        issue_key: &str,
    ) -> Result<Option<TurnCollected<Self::Result>>>
    where
        Self: Sized,
    {
        let Some(running) = crate::runs::work_pods::find_running_work_pod(
            db.pool(),
            self.kind().label_value(),
            issue_key,
        )
        .await?
        else {
            return Ok(None);
        };
        // Adoption uses the cluster recorded on the row, which stays correct even after
        // `dispatch_cluster` is reconfigured.
        let cluster = running.cluster.clone();
        let ns = d.pod_namespace(&cluster, &cfg.pod_namespace).await?;
        let outcome = self
            .watch(d.as_ref(), cfg, db.pool(), &cluster, &ns, &running.pod_name)
            .await;
        collect(
            self,
            db,
            d.as_ref(),
            cfg,
            issue_key,
            &running.pod_name,
            &cluster,
            &ns,
            outcome,
            &running.created_at,
        )
        .await
        .map(Some)
    }
}

/// Whether a collection CAS was won or lost — the single-booking guard.
enum CasResult {
    Won,
    Lost,
}

#[derive(Clone, Copy)]
pub(crate) enum TurnSite {
    Dispatch,
    Collect,
}
impl TurnSite {
    fn as_str(self) -> &'static str {
        match self {
            TurnSite::Dispatch => "dispatch",
            TurnSite::Collect => "collect",
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum TurnArm {
    LaunchFailed,
    NoResult,
    DispatchError,
}
impl TurnArm {
    fn as_str(self) -> &'static str {
        match self {
            TurnArm::LaunchFailed => "launch-failed",
            TurnArm::NoResult => "no-result",
            TurnArm::DispatchError => "dispatch-error",
        }
    }

    /// Whether this arm's turn actually ran long enough to sample `turn_duration`.
    fn observes_duration(self) -> bool {
        matches!(self, TurnArm::NoResult)
    }
}

/// The generic collection tail every adoption/dispatch funnels through: the single-booking CAS
/// lives in exactly the two free fns below ([`book_turn_success`] on the win arm, [`fail_turn`] on
/// both failure arms).
#[allow(clippy::too_many_arguments)]
async fn collect<S: TurnSpec>(
    spec: &S,
    db: &Db,
    d: &dyn PodDispatcher,
    cfg: &ControllerCfg,
    issue_key: &str,
    name: &str,
    cluster: &str,
    ns: &str,
    outcome: Result<TurnRunOutcome<S::Result>>,
    created_at: &str,
) -> Result<TurnCollected<S::Result>> {
    let kind = spec.kind();
    match outcome {
        Ok(TurnRunOutcome::Result(result)) => {
            match book_turn_success(
                db,
                d,
                kind,
                name,
                cluster,
                ns,
                &spec.summary(&result),
                spec.cost_usd(&result),
                spec.success_metric(),
                created_at,
            )
            .await?
            {
                CasResult::Won => Ok(TurnCollected::Result {
                    result,
                    pod_name: name.to_string(),
                }),
                CasResult::Lost => Ok(TurnCollected::AlreadyCollected),
            }
        }
        Ok(TurnRunOutcome::NoResult { reason }) => {
            match fail_turn(
                db,
                d,
                kind,
                name,
                cfg,
                &reason,
                created_at,
                issue_key,
                TurnSite::Collect,
                TurnArm::NoResult,
            )
            .await?
            {
                CasResult::Won => Ok(TurnCollected::Failed { reason }),
                CasResult::Lost => Ok(TurnCollected::AlreadyCollected),
            }
        }
        Err(e) => {
            let reason = format!("{e:#}");
            match fail_turn(
                db,
                d,
                kind,
                name,
                cfg,
                &reason,
                created_at,
                issue_key,
                TurnSite::Collect,
                TurnArm::DispatchError,
            )
            .await?
            {
                CasResult::Won => Ok(TurnCollected::Failed { reason }),
                CasResult::Lost => Ok(TurnCollected::AlreadyCollected),
            }
        }
    }
}

/// SUCCESS booking, single-sourced. CAS WIN: GC the pod, `ledger_append`, `record_turn` +
/// `observe_turn_duration`, `drain_freed_slot`. CAS LOST: `record_turn(kind,"already-collected")`,
/// touch nothing else — this asymmetry (a lost success CAS ticks a counter, a lost failure CAS
/// ticks nothing) is load-bearing.
#[allow(clippy::too_many_arguments)]
async fn book_turn_success(
    db: &Db,
    d: &dyn PodDispatcher,
    kind: WorkKind,
    name: &str,
    cluster: &str,
    ns: &str,
    summary: &str,
    cost: f64,
    succeeded_metric: &'static str,
    created_at: &str,
) -> Result<CasResult> {
    let won = crate::runs::work_pods::try_finish_running_work_pod(
        db.pool(),
        name,
        WorkPodState::Collected,
        Some(summary),
        None,
    )
    .await?;
    if !won {
        if let Some(m) = db.metrics() {
            m.record_turn(kind.label_value(), "already-collected");
        }
        return Ok(CasResult::Lost);
    }
    let _ = d.delete(cluster, ns, name).await;
    db.ledger_append(None, kind.cost_tag(), cost).await?;
    if let Some(m) = db.metrics() {
        m.record_turn(kind.label_value(), succeeded_metric);
        m.observe_turn_duration(kind.label_value(), turn_age_secs(created_at));
    }
    drain_freed_slot(db, kind).await;
    Ok(CasResult::Won)
}

/// FAILURE booking, single-sourced. CAS WIN: `sweep_failed_pod_overflow`, `record_turn(kind,
/// "failed")` (+ `observe_turn_duration` on the no-result arm), `drain_freed_slot`, the structured warn.
/// CAS LOST: emit NOTHING — no metric on a lost failure CAS (unlike the success arm above).
#[allow(clippy::too_many_arguments)]
async fn fail_turn(
    db: &Db,
    d: &dyn PodDispatcher,
    kind: WorkKind,
    name: &str,
    cfg: &ControllerCfg,
    reason: &str,
    created_at: &str,
    issue_key: &str,
    site: TurnSite,
    arm: TurnArm,
) -> Result<CasResult> {
    let won = crate::runs::work_pods::try_finish_running_work_pod(
        db.pool(),
        name,
        WorkPodState::Failed,
        None,
        Some(reason),
    )
    .await?;
    if !won {
        return Ok(CasResult::Lost);
    }
    sweep_failed_pod_overflow(db, d, &cfg.pod_namespace, cfg.effective().failed_pod_keep).await;
    if let Some(m) = db.metrics() {
        m.record_turn(kind.label_value(), "failed");
        if arm.observes_duration() {
            m.observe_turn_duration(kind.label_value(), turn_age_secs(created_at));
        }
    }
    drain_freed_slot(db, kind).await;
    tracing::warn!(
        kind = kind.label_value(),
        site = site.as_str(),
        arm = arm.as_str(),
        %issue_key,
        %reason,
        "turn produced no usable result",
    );
    Ok(CasResult::Won)
}

/// `spawn_blocking(render + stamp)` then `dispatcher.create`. The OUTER `Result` is the
/// `spawn_blocking` `JoinError` ALONE — the caller `?`-propagates it out (row stays `Running` for
/// the startup sweep, never CAS-failed). The INNER `Result<(), LaunchFailure>` is render-or-create:
/// a render error carries no added context (`LaunchFailure::RenderFailed`); a create error carries
/// `spec.pod_noun()` (`LaunchFailure::PodCreateFailed`).
///
/// The traced boundary for a turn: the reconcile tick that decided to spend creates no span, so this
/// (a pod is being rendered and created — scope, grounded rank) is the root the trace hangs off.
#[tracing::instrument(
    skip(d, spec, profile, digests),
    fields(kind = spec.kind.label_value(), issue_key = %spec.issue_key, pod_name = %spec.pod_name)
)]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn spawn_turn_pod(
    d: &dyn PodDispatcher,
    spec: &WorkPodSpec,
    pod_noun: &'static str,
    profile: &Path,
    digests: Option<Arc<dyn DigestResolver>>,
    cluster: &str,
    ns: &str,
    delivery: &crate::secrets::deliver::Delivery,
) -> Result<Result<(), LaunchFailure>> {
    let owner = owner_reference_from_env();
    let spec_cl = spec.clone();
    let profile = profile.to_path_buf();
    let secret_name = crate::secrets::deliver::secret_name(&spec.pod_name);
    let for_stamp = delivery.clone();
    let stamp_name = secret_name.clone();
    let rendered =
        tokio::task::spawn_blocking(move || -> Result<k8s_openapi::api::core::v1::Pod> {
            let mut pod = render_turn_pod(&spec_cl, &profile, digests)?;
            stamp_pod(&mut pod, &spec_cl, owner);
            crate::secrets::deliver::stamp(&mut pod, &stamp_name, &for_stamp);
            set_container_env(
                &mut pod,
                crate::issues::engine::ITEM_ENV,
                &spec_cl.issue_key,
            );
            Ok(pod)
        })
        .await
        .with_context(|| format!("joining the {pod_noun} render task"))?; // OUTER: JoinError -> caller `?`
    Ok(match rendered {
        Ok(mut pod) => {
            // Inject the dispatching span's W3C trace context so the turn pod's engine re-roots its
            // scope/rank span under it (a no-op when the controller isn't exporting spans).
            crate::runs::workpod::trace::inject_dispatch_context(&mut pod);
            match d.create(cluster, ns, pod).await {
                Ok(created) => {
                    create_turn_secret(d, cluster, ns, &secret_name, delivery, &created, pod_noun)
                        .await
                }
                Err(e) => Err(LaunchFailure::PodCreateFailed {
                    pod_noun,
                    detail: format!("{e:#}"),
                }),
            }
        }
        Err(e) => Err(LaunchFailure::RenderFailed(format!("{e:#}"))),
    })
}

/// The values the turn pod's container refers to, owner-referenced to the pod that just came back
/// so the cluster collects them with it. The pod waits in `ContainerCreating` on the reference
/// until this lands; a failure here is a launch failure, because the pod would never start.
async fn create_turn_secret(
    d: &dyn PodDispatcher,
    cluster: &str,
    ns: &str,
    secret_name: &str,
    delivery: &crate::secrets::deliver::Delivery,
    created: &k8s_openapi::api::core::v1::Pod,
    pod_noun: &'static str,
) -> Result<(), LaunchFailure> {
    if delivery.is_empty() {
        return Ok(());
    }
    let object = crate::secrets::deliver::secret_object(secret_name, ns, delivery, created);
    if object.metadata.owner_references.is_none() {
        return Err(LaunchFailure::PodCreateFailed {
            pod_noun,
            detail: "the pod create response carried no UID, so its Secret would outlive every \
                     turn that could have used it"
                .to_string(),
        });
    }
    d.create_secret(cluster, ns, object)
        .await
        .map_err(|e| LaunchFailure::PodCreateFailed {
            pod_noun,
            detail: format!("creating the turn Secret: {e:#}"),
        })
}

/// The launch-failure block shared by the two TURN kinds: CAS-fail the row, `sweep_failed_pod_overflow`,
/// `record_turn(kind,"failed")`, and the structured warn (`site=Dispatch, arm=LaunchFailed`). `Run`
/// does NOT use this — `dispatch_run` terminalizes with `set_work_pod_state` (no CAS).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn fail_launch(
    db: &Db,
    d: &dyn PodDispatcher,
    kind: WorkKind,
    name: &str,
    hub_namespace: &str,
    reason: &LaunchFailure,
    failed_pod_keep: u32,
    issue_key: &str,
) {
    let rendered = reason.to_string();
    let _ = crate::runs::work_pods::try_finish_running_work_pod(
        db.pool(),
        name,
        WorkPodState::Failed,
        None,
        Some(&rendered),
    )
    .await;
    sweep_failed_pod_overflow(db, d, hub_namespace, failed_pod_keep).await;
    if let Some(m) = db.metrics() {
        m.record_turn(kind.label_value(), "failed");
    }
    tracing::warn!(
        kind = kind.label_value(),
        site = TurnSite::Dispatch.as_str(),
        arm = TurnArm::LaunchFailed.as_str(),
        %issue_key,
        reason = %rendered,
        "turn produced no usable result",
    );
}

/// The generic NO-QUEUE turn dispatcher (scope IS this). Opens with the adopt-first guard, else
/// inserts a `Running` row → `spawn_turn_pod` → on an inner `Err`, `fail_launch` then
/// `TurnDispatch::Failed{reason}`; on the OUTER `JoinError`, the caller's `?` bails out leaving the
/// row `Running`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_turn<S: TurnSpec>(
    spec: &S,
    db: &Db,
    cfg: &ControllerCfg,
    d: Arc<dyn PodDispatcher>,
    issue_key: &str,
    repo_url: &str,
    max_cost: f64,
    inputs: TurnInputs,
) -> Result<TurnDispatch<S::Result>> {
    let kind = spec.kind();
    let profile = cfg.deploy_profile.clone().with_context(|| {
        format!(
            "dispatch_turn needs a deploy profile ({} executor = pod); validated at startup",
            kind.label_value()
        )
    })?;
    let sandbox_image = spec.sandbox_image(cfg)?;

    if let Some(running) =
        crate::runs::work_pods::find_running_work_pod(db.pool(), kind.label_value(), issue_key)
            .await?
    {
        let running_ns = d
            .pod_namespace(&running.cluster, &cfg.pod_namespace)
            .await?;
        if !turn_pod_adoptable(d.as_ref(), &running.cluster, &running_ns, &running.pod_name).await {
            return Ok(TurnDispatch::Launched);
        }
        if let Some(collected) = spec.adopt(db, cfg, d.clone(), issue_key).await? {
            return Ok(TurnDispatch::Collected(collected));
        }
        // The row vanished between the peek and the collect (a concurrent sweep won it); fall
        // through to a fresh admission below.
    }

    let targets: Vec<_> = crate::runs::contract::profile_target(&profile)
        .into_iter()
        .collect();
    if let Err(failure) = admit_contract(kind.into(), &targets).await {
        let rejection = failure.into_rejection()?;
        crate::runs::contract::refuse(db, issue_key, &rejection).await?;
        return Ok(TurnDispatch::Failed {
            reason: rejection.to_string(),
        });
    }

    // An admitted engine that still has no field for what this turn needs is the same kind of
    // deterministic refusal: park it here rather than letting the render fail every pass.
    if let Some(target) = targets.first()
        && let Some(unsupported) =
            UnsupportedTurnOption::for_turn(kind, inputs.codegen_contract.as_deref())
    {
        let rejection = crate::runs::contract::ContractRejection::unsupported_option(
            kind.into(),
            target.reference(),
            crate::runs::workpod::recorded_engine_version(target).await,
            unsupported.to_string(),
        );
        crate::runs::contract::refuse(db, issue_key, &rejection).await?;
        return Ok(TurnDispatch::Failed {
            reason: rejection.to_string(),
        });
    }

    // The provider's key, read before anything is written: a turn that cannot pay for the service
    // it was pointed at must leave no work-pod row and no pod behind.
    let delivery = match inputs.inference_provider.as_ref() {
        None => crate::secrets::deliver::Delivery::default(),
        Some(provider) => {
            match crate::secrets::deliver::provider_delivery(
                db.pool(),
                cfg.secret_provider.as_ref(),
                provider,
            )
            .await?
            {
                Ok(delivery) => delivery,
                Err(refusal) => {
                    return Ok(TurnDispatch::Failed {
                        reason: refusal.to_string(),
                    });
                }
            }
        }
    };

    // `build_work_pod_spec` mints the pod name (it embeds a nanosecond-suffixed call to
    // `pod_name()`), so the row insert and every later reference to this dispatch's pod MUST read
    // it off `wp_spec.pod_name` — a second, independent `spec.pod_name(issue_key)` call here would
    // mint a DIFFERENT name and silently orphan the CAS below.
    let wp_spec =
        spec.build_work_pod_spec(cfg, issue_key, repo_url, max_cost, sandbox_image, inputs);
    let pod_name = wp_spec.pod_name.clone();
    let cluster = crate::runs::workpod::issue_dispatch_cluster(db, cfg, issue_key).await?;
    crate::runs::work_pods::insert_work_pod(
        db.pool(),
        &wp_spec.new_row(WorkPodState::Running, &cluster),
    )
    .await?;

    let ns = d.pod_namespace(&cluster, &cfg.pod_namespace).await?;
    match spawn_turn_pod(
        d.as_ref(),
        &wp_spec,
        spec.pod_noun(),
        &profile,
        cfg.digest_resolver(),
        &cluster,
        &ns,
        &delivery,
    )
    .await?
    {
        Ok(()) => {
            if let Some(m) = db.metrics() {
                m.record_turn(kind.label_value(), "launched");
            }
            Ok(TurnDispatch::Launched)
        }
        Err(e) => {
            fail_launch(
                db,
                d.as_ref(),
                kind,
                &pod_name,
                &cfg.pod_namespace,
                &e,
                cfg.effective().failed_pod_keep,
                issue_key,
            )
            .await;
            Ok(TurnDispatch::Failed {
                reason: e.to_string(),
            })
        }
    }
}

/// A code-grounded triage-ranking turn.
pub(crate) struct GroundedRankSpec;

impl TurnSpec for GroundedRankSpec {
    type Result = engine::GroundedVerdict;

    fn kind(&self) -> WorkKind {
        WorkKind::AgentTurn(TurnKind::GroundedRank)
    }
    fn pod_name(&self, issue_key: &str) -> String {
        grounded_rank_pod_name(issue_key)
    }
    fn pod_noun(&self) -> &'static str {
        "turn pod"
    }
    fn deadline(&self, _cfg: &ControllerCfg) -> Duration {
        GROUNDED_RANK_TIMEOUT
    }
    fn sandbox_image(&self, cfg: &ControllerCfg) -> Result<String> {
        cfg.grounded_sandbox_image.clone().context(
            "dispatch_grounded_rank needs a sandbox image (grounded_executor = pod); validated at startup",
        )
    }
    fn build_work_pod_spec(
        &self,
        _cfg: &ControllerCfg,
        issue_key: &str,
        repo_url: &str,
        max_cost: f64,
        sandbox_image: String,
        inputs: TurnInputs,
    ) -> WorkPodSpec {
        // A rank turn ignores the scope-only knobs (tier/goal/authoritative) but still clones, so
        // it honours `git_ref`.
        WorkPodSpec::grounded_rank(
            self.pod_name(issue_key),
            issue_key.to_string(),
            repo_url.to_string(),
            max_cost,
            sandbox_image,
            inputs.git_ref,
        )
    }
    fn cost_usd(&self, v: &Self::Result) -> f64 {
        v.cost_usd
    }
    fn summary(&self, v: &Self::Result) -> String {
        format!("{} (${:.4})", v.disposition.as_str(), v.cost_usd)
    }
    fn success_metric(&self) -> &'static str {
        "verdict"
    }
    async fn parse(
        &self,
        terminal: &TerminalState,
        logs: &str,
        _pool: &sqlx::PgPool,
        pod: &str,
    ) -> Result<TurnRunOutcome<Self::Result>> {
        match collect_verdict(terminal.termination_message(), logs) {
            Ok(v) => Ok(TurnRunOutcome::Result(v)),
            Err(e) => Ok(TurnRunOutcome::NoResult {
                reason: format!(
                    "turn pod {pod} phase={:?} produced no verdict: {e:#}",
                    terminal.phase
                ),
            }),
        }
    }
}

/// A scope-propose turn.
pub(crate) struct ScopeSpec;

impl TurnSpec for ScopeSpec {
    type Result = engine::ScopeReport;

    fn kind(&self) -> WorkKind {
        WorkKind::AgentTurn(TurnKind::Scope)
    }
    fn pod_name(&self, issue_key: &str) -> String {
        scope_pod_name(issue_key)
    }
    fn pod_noun(&self) -> &'static str {
        "scope turn pod"
    }
    fn deadline(&self, cfg: &ControllerCfg) -> Duration {
        scope_deadline(cfg.effective().scope_gaming_rounds)
    }
    fn sandbox_image(&self, cfg: &ControllerCfg) -> Result<String> {
        Ok(cfg.scope_sandbox_image.clone().unwrap_or_else(|| {
            cfg.grounded_sandbox_image
                .clone()
                .unwrap_or_else(|| "missing-sandbox-image".to_string())
        }))
    }
    fn build_work_pod_spec(
        &self,
        cfg: &ControllerCfg,
        issue_key: &str,
        repo_url: &str,
        max_cost: f64,
        sandbox_image: String,
        inputs: TurnInputs,
    ) -> WorkPodSpec {
        WorkPodSpec::scope(
            self.pod_name(issue_key),
            issue_key.to_string(),
            repo_url.to_string(),
            max_cost,
            sandbox_image,
            cfg.effective().scope_gaming_rounds,
            cfg.effective().scope_skip_gaming_review,
            inputs,
        )
    }
    fn cost_usd(&self, r: &Self::Result) -> f64 {
        r.cost_usd()
    }
    fn summary(&self, r: &Self::Result) -> String {
        let survived = if r.survived() { "PASS" } else { "FAIL" };
        format!("{survived} (${:.4})", r.cost_usd())
    }
    fn success_metric(&self) -> &'static str {
        "report"
    }
    async fn parse(
        &self,
        terminal: &TerminalState,
        logs: &str,
        pool: &sqlx::PgPool,
        pod: &str,
    ) -> Result<TurnRunOutcome<Self::Result>> {
        match collect_scope_report(terminal.termination_message(), logs) {
            Ok(mut report) => {
                let manifest = manifest_from_message(terminal.termination_message());
                if let Err(reason) =
                    apply_dropbox_artifacts(&mut report, &manifest, pool, pod).await
                {
                    return Ok(TurnRunOutcome::NoResult {
                        reason: format!("scope turn pod {pod}: {reason}"),
                    });
                }
                Ok(TurnRunOutcome::Result(report))
            }
            Err(e) => Ok(TurnRunOutcome::NoResult {
                reason: format!(
                    "scope turn pod {pod} phase={:?} produced no report: {e:#}",
                    terminal.phase
                ),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Metrics;
    use crucible_contract::{ArtifactKind, ArtifactRef, Envelope, EnvelopeKind};
    use k8s_openapi::api::core::v1::Pod;
    use std::sync::Mutex;

    /// A genuine boundary double (not a mock-in-the-middle): a scripted [`PodDispatcher`] whose
    /// every knob a fixture sets explicitly, so the same script drives both the OLD (`run.rs`) and
    /// NEW (this module) code paths identically.
    struct ScriptedDispatcher {
        phase: TurnPhase,
        message: Option<String>,
        logs: String,
        /// `await_terminal` fails instead of returning a phase — the hard-error collect arm.
        watch_err: bool,
        /// `create` fails — the launch-failure create-context fixture.
        create_err: bool,
        /// Injects the collection race: a concurrent collector flips the row straight to
        /// `collected` right as this dispatcher answers the LAST call before the CAS (`logs` on
        /// the success/no-result arms, `await_terminal` on the hard-error arm) — the CAS-lost
        /// fixtures. `None` means no race.
        race: Option<sqlx::PgPool>,
        /// Every pod handed to `create`, whole — the traceparent fixtures assert on its env.
        created: Arc<Mutex<Vec<Pod>>>,
        deleted: Arc<Mutex<Vec<String>>>,
        /// The `cluster` argument of every call, in order — the hub-spoke routing fixtures.
        clusters: Arc<Mutex<Vec<String>>>,
    }

    impl ScriptedDispatcher {
        fn succeeded(logs: &str) -> Self {
            ScriptedDispatcher {
                phase: TurnPhase::Succeeded,
                message: None,
                logs: logs.to_string(),
                watch_err: false,
                create_err: false,
                race: None,
                created: Arc::new(Mutex::new(Vec::new())),
                deleted: Arc::new(Mutex::new(Vec::new())),
                clusters: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn saw_cluster(&self, cluster: &str) {
            self.clusters
                .lock()
                .expect("lock")
                .push(cluster.to_string());
        }

        async fn spring_the_race(&self, pod_name: &str) {
            if let Some(pool) = &self.race {
                sqlx::query("UPDATE work_pods SET state = 'collected' WHERE pod_name = $1")
                    .bind(pod_name)
                    .execute(pool)
                    .await
                    .expect("spring the race");
            }
        }
    }

    #[async_trait::async_trait]
    impl PodDispatcher for ScriptedDispatcher {
        async fn create(&self, cluster: &str, _ns: &str, mut pod: Pod) -> Result<Pod> {
            self.saw_cluster(cluster);
            if self.create_err {
                anyhow::bail!("kube create denied");
            }
            self.created.lock().expect("lock").push(pod.clone());
            pod.metadata.uid = Some("fake-uid".to_string());
            Ok(pod)
        }
        async fn await_terminal(
            &self,
            cluster: &str,
            _ns: &str,
            name: &str,
            _t: Duration,
        ) -> Result<TerminalState> {
            self.saw_cluster(cluster);
            if self.watch_err {
                self.spring_the_race(name).await;
                anyhow::bail!("watch blew up");
            }
            Ok(TerminalState {
                phase: self.phase,
                message: self.message.clone(),
            })
        }
        async fn logs(&self, cluster: &str, _ns: &str, name: &str) -> Result<String> {
            self.saw_cluster(cluster);
            self.spring_the_race(name).await;
            Ok(self.logs.clone())
        }
        async fn delete(&self, cluster: &str, _ns: &str, name: &str) -> Result<()> {
            self.saw_cluster(cluster);
            self.deleted.lock().expect("lock").push(name.to_string());
            Ok(())
        }
    }

    fn pod_cfg(profile: &Path, sandbox: &str) -> ControllerCfg {
        crate::testing::cfg_from_args([
            "ctl",
            "--deploy-profile",
            &profile.to_string_lossy(),
            "--render-no-pin",
            "--grounded-sandbox-image",
            sandbox,
            "--scope-sandbox-image",
            sandbox,
        ])
    }

    async fn new_db() -> Db {
        let pool = crate::client::connect(&crate::test_ledger_url())
            .await
            .expect("db");
        Db::new(pool).with_metrics(Metrics::new().expect("metrics"))
    }

    async fn seed_running(db: &Db, kind: WorkKind, issue_key: &str, pod_name: &str) {
        seed_running_on(db, kind, issue_key, pod_name, "hub").await
    }

    async fn seed_running_on(
        db: &Db,
        kind: WorkKind,
        issue_key: &str,
        pod_name: &str,
        cluster: &str,
    ) {
        crate::runs::work_pods::insert_work_pod(
            db.pool(),
            &NewWorkPod {
                pod_name: pod_name.to_string(),
                kind: kind.label_value().to_string(),
                issue_key: Some(issue_key.to_string()),
                state: WorkPodState::Running,
                cost_tag: kind.cost_tag().to_string(),
                cluster: cluster.to_string(),
            },
        )
        .await
        .expect("seed running row");
    }

    async fn work_pod(db: &Db, pod_name: &str) -> WorkPodRow {
        crate::runs::work_pods::get_work_pod(db.pool(), pod_name)
            .await
            .expect("query")
            .expect("row exists")
    }

    /// The one `failed` row for `issue_key` — dispatch mints the pod name at dispatch time
    /// (a clock-suffixed name), so a launch-failure fixture can't look it up by name.
    async fn failed_row_for_issue(db: &Db, issue_key: &str) -> WorkPodRow {
        crate::runs::work_pods::work_pods_in_states(db.pool(), &[WorkPodState::Failed])
            .await
            .expect("query")
            .into_iter()
            .find(|r| r.issue_key.as_deref() == Some(issue_key))
            .expect("a failed row for the issue")
    }

    // --- collect-tail fixtures: `TurnSpec::adopt`, exercised via the public `adopt_*_turn` entry ---

    #[tokio::test]
    async fn fixture_verdict() {
        let tmp = tempfile::tempdir().expect("tmp");
        let db = new_db().await;
        let cfg = pod_cfg(&tmp.path().join("profile.toml"), "img");
        let logs = "CRUCIBLE_VERDICT: {\"tier\":\"T2\",\"rationale\":\"one live service\",\"confidence\":\"low\",\"cost_usd\":0.25,\"over_budget\":false}\n";
        seed_running(
            &db,
            WorkKind::AgentTurn(TurnKind::GroundedRank),
            "o/r#1",
            "pod-a",
        )
        .await;
        let d = Arc::new(ScriptedDispatcher::succeeded(logs));

        let out = adopt_grounded_turn(&db, &cfg, d.clone(), "o/r#1")
            .await
            .expect("adopt")
            .expect("a running row");

        assert!(matches!(out, DispatchOutcome::Verdict(_)));
        assert_eq!(work_pod(&db, "pod-a").await.state, WorkPodState::Collected);
        assert_eq!(d.deleted.lock().expect("lock").len(), 1);
        assert_eq!(
            db.metrics()
                .unwrap()
                .turns_total("grounded-rank", "verdict"),
            1
        );
        let today = crate::clock::today_utc();
        assert!(
            (crate::ledger::ledger_day_total(db.pool(), &today)
                .await
                .unwrap()
                - 0.25)
                .abs()
                < 1e-9
        );
    }

    #[tokio::test]
    async fn fixture_no_verdict() {
        let tmp = tempfile::tempdir().expect("tmp");
        let db = new_db().await;
        let cfg = pod_cfg(&tmp.path().join("profile.toml"), "img");
        seed_running(
            &db,
            WorkKind::AgentTurn(TurnKind::GroundedRank),
            "o/r#2",
            "pod-b",
        )
        .await;
        let d = Arc::new(ScriptedDispatcher::succeeded("garbage, no marker here"));

        let out = adopt_grounded_turn(&db, &cfg, d, "o/r#2")
            .await
            .expect("adopt")
            .expect("a running row");

        assert!(matches!(out, DispatchOutcome::Failed));
        assert_eq!(work_pod(&db, "pod-b").await.state, WorkPodState::Failed);
        // The no-result arm samples turn_duration.
        assert_eq!(
            db.metrics().unwrap().turns_total("grounded-rank", "failed"),
            1
        );
        assert_eq!(
            db.metrics().unwrap().turn_duration_samples("grounded-rank"),
            1
        );
    }

    #[tokio::test]
    async fn fixture_hard_error() {
        let tmp = tempfile::tempdir().expect("tmp");
        let db = new_db().await;
        let cfg = pod_cfg(&tmp.path().join("profile.toml"), "img");
        seed_running(
            &db,
            WorkKind::AgentTurn(TurnKind::GroundedRank),
            "o/r#3",
            "pod-c",
        )
        .await;
        let mut d = ScriptedDispatcher::succeeded("");
        d.watch_err = true;
        let d = Arc::new(d);

        let out = adopt_grounded_turn(&db, &cfg, d, "o/r#3")
            .await
            .expect("adopt")
            .expect("a running row");

        assert!(matches!(out, DispatchOutcome::Failed));
        // The hard-error arm does NOT sample turn_duration.
        assert_eq!(
            db.metrics().unwrap().turn_duration_samples("grounded-rank"),
            0
        );
        assert_eq!(
            db.metrics().unwrap().turns_total("grounded-rank", "failed"),
            1
        );
    }

    /// A ScopeReport termination-envelope JSON, with an optional manifest entry so a fixture can
    /// exercise `apply_dropbox_artifacts` on the envelope path (the marker-only fallback carries no
    /// manifest at all).
    fn scope_envelope(
        digest: Option<&str>,
        passed: bool,
        cost: f64,
        pack_ref: Option<ArtifactRef>,
    ) -> String {
        let payload = serde_json::json!({
            "stages": [{"name": "freeze", "passed": passed, "detail": "ok"}],
            "digest": digest,
            "cost": cost,
        });
        let mut env = Envelope::new(EnvelopeKind::ScopeReport, payload);
        if let Some(p) = pack_ref {
            env.artifacts.push(p);
        }
        env.to_capped_json().expect("encode envelope")
    }

    #[tokio::test]
    async fn fixture_report() {
        let tmp = tempfile::tempdir().expect("tmp");
        let db = new_db().await;
        let cfg = pod_cfg(&tmp.path().join("profile.toml"), "img");
        let message = scope_envelope(Some("v1:beef"), true, 0.4, None);
        seed_running(&db, WorkKind::AgentTurn(TurnKind::Scope), "o/r#4", "pod-d").await;
        let mut d = ScriptedDispatcher::succeeded("");
        d.message = Some(message);
        let d = Arc::new(d);

        let out = adopt_scope_turn(&db, &cfg, d, "o/r#4")
            .await
            .expect("adopt")
            .expect("a running row");

        assert!(matches!(out, ScopeOutcome::Report { .. }));
        assert_eq!(work_pod(&db, "pod-d").await.state, WorkPodState::Collected);
        assert_eq!(db.metrics().unwrap().turns_total("scope", "report"), 1);
    }

    #[tokio::test]
    async fn fixture_no_report() {
        let tmp = tempfile::tempdir().expect("tmp");
        let db = new_db().await;
        let cfg = pod_cfg(&tmp.path().join("profile.toml"), "img");
        seed_running(&db, WorkKind::AgentTurn(TurnKind::Scope), "o/r#5", "pod-e").await;
        let d = Arc::new(ScriptedDispatcher::succeeded("no marker at all"));

        let out = adopt_scope_turn(&db, &cfg, d, "o/r#5")
            .await
            .expect("adopt")
            .expect("a running row");

        assert!(matches!(out, ScopeOutcome::Failed(_)));
        assert_eq!(db.metrics().unwrap().turns_total("scope", "failed"), 1);
    }

    /// A surviving scope report whose manifest names a pack the drop-box never received (the
    /// artifact store is empty) — a loud `NoResult`, never a silently missing deliverable.
    #[tokio::test]
    async fn fixture_pack_missing() {
        let tmp = tempfile::tempdir().expect("tmp");
        let db = new_db().await;
        let cfg = pod_cfg(&tmp.path().join("profile.toml"), "img");

        let pack_ref = ArtifactRef {
            kind: ArtifactKind::ScopePack,
            digest: "sha256:deadbeef".to_string(),
            bytes: 4,
            delivered: true,
        };
        let message = scope_envelope(Some("v1:beef"), true, 0.1, Some(pack_ref));
        seed_running(&db, WorkKind::AgentTurn(TurnKind::Scope), "o/r#6", "pod-f").await;
        let mut d = ScriptedDispatcher::succeeded("");
        d.message = Some(message);
        let d = Arc::new(d);

        let out = adopt_scope_turn(&db, &cfg, d, "o/r#6")
            .await
            .expect("adopt")
            .expect("a running row");

        let ScopeOutcome::Failed(reason) = out else {
            panic!("expected a loud NoResult, got {out:?}");
        };
        assert!(reason.contains("scope pack not recovered"), "{reason}");
    }

    // --- CAS-loss fixtures, split per arm (R2) ------------------------------------------------

    #[tokio::test]
    async fn fixture_cas_lost_on_success() {
        let tmp = tempfile::tempdir().expect("tmp");
        let db = new_db().await;
        let cfg = pod_cfg(&tmp.path().join("profile.toml"), "img");
        let logs = "CRUCIBLE_VERDICT: {\"tier\":\"T1\",\"rationale\":\"x\",\"cost_usd\":0.9,\"over_budget\":false}\n";
        seed_running(
            &db,
            WorkKind::AgentTurn(TurnKind::GroundedRank),
            "o/r#7",
            "pod-g",
        )
        .await;
        let mut d = ScriptedDispatcher::succeeded(logs);
        d.race = Some(db.pool().clone());
        let d = Arc::new(d);

        let out = adopt_grounded_turn(&db, &cfg, d.clone(), "o/r#7")
            .await
            .expect("adopt")
            .expect("a running row");

        assert!(matches!(out, DispatchOutcome::AlreadyCollected));
        // Only the "already-collected" tick moves; nothing else does.
        assert_eq!(
            db.metrics()
                .unwrap()
                .turns_total("grounded-rank", "already-collected"),
            1
        );
        assert_eq!(
            db.metrics()
                .unwrap()
                .turns_total("grounded-rank", "verdict"),
            0
        );
        assert!(
            d.deleted.lock().expect("lock").is_empty(),
            "no GC on a lost CAS"
        );
        let today = crate::clock::today_utc();
        assert!(
            crate::ledger::ledger_day_total(db.pool(), &today)
                .await
                .unwrap()
                .abs()
                < 1e-9,
            "the loser books no cost"
        );
    }

    /// Drives both the NoResult and hard-Err collect arms through a lost CAS — neither ticks any
    /// metric (unlike the success arm above), the load-bearing asymmetry R2 pins.
    #[tokio::test]
    async fn fixture_cas_lost_on_failure() {
        for watch_err in [false, true] {
            let tmp = tempfile::tempdir().expect("tmp");
            let db = new_db().await;
            let cfg = pod_cfg(&tmp.path().join("profile.toml"), "img");
            seed_running(
                &db,
                WorkKind::AgentTurn(TurnKind::GroundedRank),
                "o/r#8",
                "pod-h",
            )
            .await;
            let mut d = ScriptedDispatcher::succeeded("no marker");
            d.watch_err = watch_err;
            d.race = Some(db.pool().clone());
            let d = Arc::new(d);

            let out = adopt_grounded_turn(&db, &cfg, d.clone(), "o/r#8")
                .await
                .expect("adopt")
                .expect("a running row");

            assert!(matches!(out, DispatchOutcome::AlreadyCollected));
            // Zero metric movement on a lost FAILURE cas — unlike the success arm.
            let m = db.metrics().unwrap();
            assert_eq!(m.turns_total("grounded-rank", "already-collected"), 0);
            assert_eq!(m.turns_total("grounded-rank", "failed"), 0);
            assert_eq!(m.turn_duration_samples("grounded-rank"), 0);
            assert!(d.deleted.lock().expect("lock").is_empty());
        }
    }

    // --- dispatch-tail (launch) fixtures (R1) -------------------------------------------------

    /// The shared launch-tail invariant across both turn kinds: a pod-create failure surfaces as a
    /// `TurnDispatch::Failed` reason that (a) rides `LaunchFailure::PodCreateFailed`'s
    /// `spec.pod_noun()`-carried wording and (b) lands verbatim on the `work_pods.error` row.
    async fn conformance_dispatch_create_fail<S: TurnSpec>(
        spec: &S,
        issue_key: &str,
        repo_url: &str,
        max_cost: f64,
    ) -> String {
        let _g = crate::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tmp");
        let db = new_db().await;
        let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
        let cfg = pod_cfg(&profile, "img");
        let mut d = ScriptedDispatcher::succeeded("");
        d.create_err = true;
        let d = Arc::new(d);

        let out = dispatch_turn(
            spec,
            &db,
            &cfg,
            d,
            issue_key,
            repo_url,
            max_cost,
            TurnInputs::default(),
        )
        .await
        .expect("dispatch");

        let TurnDispatch::Failed { reason } = out else {
            panic!("expected a launch failure, got a non-Failed TurnDispatch");
        };
        let row = failed_row_for_issue(&db, issue_key).await;
        assert_eq!(row.error.as_deref(), Some(reason.as_str()));
        reason
    }

    #[tokio::test]
    async fn fixture_dispatch_create_fail_grounded() {
        let reason = conformance_dispatch_create_fail(&GroundedRankSpec, "o/r#9", "u", 0.0).await;
        assert!(
            reason.starts_with("creating the turn pod: kube create denied"),
            "{reason}"
        );
    }

    /// The scope kind's own create-context wording ("creating the scope turn pod") — pinned
    /// separately since grounded and scope use different `pod_noun()`s.
    #[tokio::test]
    async fn fixture_dispatch_create_fail_scope() {
        let reason = conformance_dispatch_create_fail(&ScopeSpec, "o/r#10", "o/r", 5.0).await;
        assert!(
            reason.starts_with("creating the scope turn pod: kube create denied"),
            "{reason}"
        );
    }

    #[tokio::test]
    async fn fixture_dispatch_render_fail() {
        let _g = crate::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tmp");
        let db = new_db().await;
        // A profile that does not parse breaks the render before any pod exists.
        let profile = tmp.path().join("profile.toml");
        std::fs::write(&profile, "unused").unwrap();
        let cfg = pod_cfg(&profile, "img");
        let d = Arc::new(ScriptedDispatcher::succeeded(""));

        let out = dispatch_turn(
            &GroundedRankSpec,
            &db,
            &cfg,
            d,
            "o/r#11",
            "u",
            0.0,
            TurnInputs::default(),
        )
        .await
        .expect("dispatch");

        let TurnDispatch::Failed { reason } = out else {
            panic!("expected a launch failure");
        };
        // No added framing on a render failure — the engine's own error text (unlike the
        // create-fail fixture's pod_noun-carried context).
        assert!(reason.contains("parsing deploy profile"), "{reason}");
        assert!(!reason.starts_with("creating the"), "{reason}");
        let row = failed_row_for_issue(&db, "o/r#11").await;
        assert_eq!(row.error.as_deref(), Some(reason.as_str()));
    }

    /// The `spawn_blocking` JoinError early-bail: a genuine panic inside the blocking closure never
    /// becomes a [`LaunchFailure`] — it `?`-propagates OUT as the OUTER `Result`'s bare error,
    /// exactly the pattern [`spawn_turn_pod`] uses (`.await.with_context(...)?` on the outer join,
    /// before any `LaunchFailure` can be constructed). Production `render_turn_pod`/`stamp_pod` are
    /// panic-free by house rule (no unwrap/expect outside tests), so this pins the TYPE-LEVEL
    /// contract with the identical join pattern rather than routing an artificial panic through the
    /// full render subprocess: no `LaunchFailure` variant exists for a join failure (see the enum's
    /// doc), and the `?` here means a panic can never reach `fail_launch`'s CAS-fail — the row stays
    /// `Running` for the startup sweep, exactly as today's `run.rs:648`/`923` early-bail.
    #[tokio::test]
    async fn fixture_dispatch_join_panic_never_becomes_a_launch_failure() {
        async fn joins_like_spawn_turn_pod() -> Result<Result<(), LaunchFailure>> {
            let rendered = tokio::task::spawn_blocking(|| -> Result<()> { panic!("boom") })
                .await
                .with_context(|| "joining the turn pod render task")?; // OUTER: JoinError -> `?`
            Ok(rendered.map_err(|e| LaunchFailure::RenderFailed(format!("{e:#}"))))
        }
        let outer = joins_like_spawn_turn_pod().await;
        assert!(
            outer.is_err(),
            "a panicked join is the OUTER error, never an Ok(Err(LaunchFailure))"
        );
    }

    /// Locks [`LaunchFailure::PodCreateFailed`]'s rendering for the scope kind's `pod_noun` —
    /// the exact wording `dispatch_scope`'s `ScopeOutcome::Failed` (and `work_pods.error`) carry
    /// on a create failure.
    #[test]
    fn scope_launch_failure_reason_names_the_scope_turn_pod() {
        let failure = LaunchFailure::PodCreateFailed {
            pod_noun: ScopeSpec.pod_noun(),
            detail: "kube create denied".to_string(),
        };
        assert_eq!(
            failure.to_string(),
            "creating the scope turn pod: kube create denied"
        );
    }

    // --- dispatch → pod trace hand-off ---------------------------------------------------------

    /// A recording PRODUCER dispatch span stamps its exact W3C `traceparent` onto the created turn
    /// pod's container env — the full controller-side half of the scope/rank trace stitch, driven
    /// through the real `dispatch_turn` → `spawn_turn_pod` path.
    #[tokio::test]
    async fn dispatch_stamps_the_producer_traceparent_on_the_turn_pod() {
        use opentelemetry::trace::{TraceContextExt as _, TracerProvider as _};
        use tracing::Instrument as _;
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        use tracing_subscriber::layer::SubscriberExt as _;

        let _g = crate::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tmp");
        let db = new_db().await;
        let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
        let cfg = pod_cfg(&profile, "img");
        let d = Arc::new(ScriptedDispatcher::succeeded(""));

        // A provider with no exporter still mints valid, sampled span contexts — enough for the
        // injection to serialize a real traceparent. Thread-local default only: the tokio test is
        // current-thread, so the dispatch future polls under it.
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
        let layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("test"));
        let _sub = tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));
        let span = tracing::info_span!("dispatch_scope", otel.kind = "producer");
        let expected = {
            let sc = span.context().span().span_context().clone();
            format!(
                "00-{:032x}-{:016x}-01",
                u128::from_be_bytes(sc.trace_id().to_bytes()),
                u64::from_be_bytes(sc.span_id().to_bytes())
            )
        };

        let out = dispatch_turn(
            &ScopeSpec,
            &db,
            &cfg,
            d.clone(),
            "o/r#31",
            "o/r",
            5.0,
            TurnInputs::default(),
        )
        .instrument(span)
        .await
        .expect("dispatch");
        assert!(matches!(out, TurnDispatch::Launched));

        let created = d.created.lock().expect("lock");
        assert_eq!(created.len(), 1);
        let containers = &created[0].spec.as_ref().expect("pod spec").containers;
        assert!(!containers.is_empty());
        for c in containers {
            let tp = c
                .env
                .as_ref()
                .and_then(|env| env.iter().find(|v| v.name == "TRACEPARENT"))
                .and_then(|v| v.value.clone())
                .expect("TRACEPARENT env on the turn container");
            // The injected parent shares the dispatch span's trace-id, but the span-id is
            // `spawn_turn_pod`'s own span (the traced boundary for a turn), a child of the
            // dispatch — so assert the trace lineage, not the exact span-id.
            let parts: Vec<&str> = tp.split('-').collect();
            let expected_parts: Vec<&str> = expected.split('-').collect();
            assert_eq!(parts.len(), 4, "well-formed traceparent: {tp}");
            assert_eq!(parts[0], "00");
            assert_eq!(parts[1], expected_parts[1], "same trace id as the dispatch");
            assert_ne!(parts[2], "0000000000000000", "a real parent span id");
            assert_eq!(parts[3], "01", "sampled flag preserved");
        }
    }

    /// Without a recording span (the stderr-only controller — every other fixture in this module),
    /// the created turn pod carries no `TRACEPARENT` at all: never a bogus all-zeros parent.
    #[tokio::test]
    async fn dispatch_without_a_recording_span_injects_no_traceparent() {
        let _g = crate::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tmp");
        let db = new_db().await;
        let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
        let cfg = pod_cfg(&profile, "img");
        let d = Arc::new(ScriptedDispatcher::succeeded(""));

        let out = dispatch_turn(
            &GroundedRankSpec,
            &db,
            &cfg,
            d.clone(),
            "o/r#32",
            "o/r",
            0.0,
            TurnInputs::default(),
        )
        .await
        .expect("dispatch");
        assert!(matches!(out, TurnDispatch::Launched));

        let created = d.created.lock().expect("lock");
        assert_eq!(created.len(), 1);
        for c in &created[0].spec.as_ref().expect("pod spec").containers {
            assert!(
                !c.env
                    .iter()
                    .flatten()
                    .any(|v| v.name == "TRACEPARENT" || v.name == "TRACESTATE"),
                "no trace env without a recording dispatch span"
            );
        }
    }

    /// The engine resolves its `tracker-comment` default target from `$CRUCIBLE_ITEM`, so a turn
    /// pod carries the item its turn is parameterized by. Without it the engine refuses every
    /// tracker write the turn's agent attempts.
    #[tokio::test]
    async fn a_turn_pod_carries_the_item_its_turn_is_parameterized_by() {
        let _g = crate::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tmp");
        let db = new_db().await;
        let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
        let cfg = pod_cfg(&profile, "img");
        let d = Arc::new(ScriptedDispatcher::succeeded(""));

        let out = dispatch_turn(
            &ScopeSpec,
            &db,
            &cfg,
            d.clone(),
            "o/r#33",
            "o/r",
            5.0,
            TurnInputs::default(),
        )
        .await
        .expect("dispatch");
        assert!(matches!(out, TurnDispatch::Launched));

        let created = d.created.lock().expect("lock");
        assert_eq!(created.len(), 1);
        let containers = &created[0].spec.as_ref().expect("pod spec").containers;
        assert!(!containers.is_empty());
        for c in containers {
            assert_eq!(
                c.env
                    .iter()
                    .flatten()
                    .find(|v| v.name == crate::issues::engine::ITEM_ENV)
                    .and_then(|v| v.value.as_deref()),
                Some("o/r#33")
            );
        }
    }

    // --- hub-spoke routing --------------------------------------------------------------------

    /// A fresh dispatch targets `cfg.dispatch_cluster` and records that cluster on the
    /// `work_pods` row.
    #[tokio::test]
    async fn dispatch_launches_on_the_configured_cluster_and_books_it_on_the_row() {
        let _g = crate::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tmp");
        let db = new_db().await;
        let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
        let mut cfg = pod_cfg(&profile, "img");
        cfg.dispatch_cluster = "wharf".to_string();
        let d = Arc::new(ScriptedDispatcher::succeeded(""));

        let out = dispatch_turn(
            &ScopeSpec,
            &db,
            &cfg,
            d.clone(),
            "o/r#40",
            "o/r",
            5.0,
            TurnInputs::default(),
        )
        .await
        .expect("dispatch");

        assert!(matches!(out, TurnDispatch::Launched));
        assert_eq!(*d.clusters.lock().expect("lock"), vec!["wharf"]);
        let row = crate::runs::work_pods::work_pods_in_states(db.pool(), &[WorkPodState::Running])
            .await
            .expect("query")
            .into_iter()
            .find(|r| r.issue_key.as_deref() == Some("o/r#40"))
            .expect("the running row");
        assert_eq!(row.cluster, "wharf");
    }

    /// Collection uses the cluster recorded on the row, not `cfg.dispatch_cluster`, so turns
    /// already running on a previously configured cluster are still collected there.
    #[tokio::test]
    async fn adoption_follows_the_row_cluster_not_the_configured_one() {
        let tmp = tempfile::tempdir().expect("tmp");
        let db = new_db().await;
        let mut cfg = pod_cfg(&tmp.path().join("profile.toml"), "img");
        cfg.dispatch_cluster = "hub".to_string();
        let logs = "CRUCIBLE_VERDICT: {\"tier\":\"T2\",\"rationale\":\"x\",\"cost_usd\":0.1,\"over_budget\":false}\n";
        seed_running_on(
            &db,
            WorkKind::AgentTurn(TurnKind::GroundedRank),
            "o/r#41",
            "pod-w",
            "wharf",
        )
        .await;
        let d = Arc::new(ScriptedDispatcher::succeeded(logs));

        adopt_grounded_turn(&db, &cfg, d.clone(), "o/r#41")
            .await
            .expect("adopt")
            .expect("a running row");

        // watch (await_terminal + logs) and the success GC all went to the row's cluster.
        let seen = d.clusters.lock().expect("lock").clone();
        assert_eq!(seen, vec!["wharf", "wharf", "wharf"]);
    }

    // --- ZST specs carry no state, kind()/pod_noun() are stable literals ----------------------

    #[test]
    fn grounded_and_scope_specs_report_stable_kind_and_noun() {
        assert_eq!(
            GroundedRankSpec.kind(),
            WorkKind::AgentTurn(TurnKind::GroundedRank)
        );
        assert_eq!(GroundedRankSpec.pod_noun(), "turn pod");
        assert_eq!(ScopeSpec.kind(), WorkKind::AgentTurn(TurnKind::Scope));
        assert_eq!(ScopeSpec.pod_noun(), "scope turn pod");
    }
}
