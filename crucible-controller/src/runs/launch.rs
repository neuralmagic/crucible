//! `new` for a playbook launch: dispatch the pack straight onto the run pod. There is no scope
//! turn ahead of it — the pack already exists, pinned and registered — and no approval gate, since
//! the validated POST that wrote the launch row is the human authorization. The row's params and
//! ceilings are re-read here rather than carried on the queue, because a dequeued key carries no
//! payload.

use crate::client::Db;
use crate::config::{ControllerCfg, PlaybookExecutor};
use crate::issues::model::Issue;
use crate::model::{ParkReason, ParkedBy, Status};
use crate::playbooks::providers::WorkloadClass;
use crate::runs::claim::{PLAYBOOK_DISPATCH_FAILED, RunClaim, RunOrigin, RunPlacement};
use crate::runs::model::{RunDispatch, RunLocation, new_run_id};
use anyhow::{Context, Result};

/// Dispatch `issue`'s playbook launch, parking it when its pack is unconvertible.
pub(crate) async fn launch(db: &Db, cfg: &ControllerCfg, issue: &Issue) -> Result<()> {
    let Err(e) = start(db, cfg, issue).await else {
        return Ok(());
    };
    let Some(refusal) = e.downcast_ref::<crate::playbooks::pack_trees::Unconvertible>() else {
        return Err(e);
    };
    crate::issues::transitions::park(
        db.pool(),
        db.events(),
        &issue.key,
        Status::New,
        &ParkReason::PackUnconvertible {
            digest: refusal.digest.clone(),
            reason: refusal.reason.clone(),
        },
        ParkedBy::Machine,
    )
    .await?;
    Ok(())
}

/// Why the engine would refuse `launch` at startup, judged against the pack it froze. `None` when
/// it would start, or when no pack is stored for the dispatch to fail on.
async fn plan_refusal(
    db: &Db,
    key: &str,
    launch: &crate::launches::model::PlaybookLaunch,
) -> Result<Option<String>> {
    let slug = crate::model::sanitize_key(key);
    let Some(tree) = crate::runs::blob_store::get_pack(db.pool(), &slug).await? else {
        return Ok(None);
    };
    let checked = crate::playbooks::registry::check_tree_runnable(
        tree,
        launch.params.iter().cloned().collect(),
        launch.max_time.clone(),
        launch.variant.clone(),
    )
    .await?;
    Ok(checked.err().map(|field| field.message))
}

/// The same image preflight the launch endpoint ran, against the catalog as it stands now and the
/// harness this dispatch actually resolved. The inner `Err` is why the launch parks.
async fn preflight_image(
    db: &Db,
    issue: &Issue,
    pack_agent: Option<&crate::playbooks::dispatch::PackAgent>,
    dispatch: Option<&crate::playbooks::providers::ResolvedDispatch>,
) -> Result<Result<crate::runs::model::RunImage, ParkReason>> {
    let Some(pack_agent) = pack_agent else {
        return Ok(Ok(crate::runs::model::RunImage::default()));
    };
    let resolved = dispatch.map(|d| crate::playbooks::preflight::ResolvedHarness {
        harness: d.harness,
        provider: d.provider.id.clone(),
        source: if issue.agent_provider.is_some() {
            crate::playbooks::preflight::HarnessSource::Pin
        } else {
            crate::playbooks::preflight::HarnessSource::Default
        },
    });
    let catalog = crate::images::store::list_images(db.pool()).await?;
    let pack = crate::playbooks::preflight::preflight_pack(pack_agent, resolved.as_ref(), &catalog);
    if pack.refused() {
        return Ok(Err(ParkReason::ImagePreflightRefused {
            image: pack
                .image
                .reference
                .unwrap_or_else(|| "(no image)".to_string()),
            detail: pack
                .refusals
                .iter()
                .map(|r| match r.field.as_str() {
                    "sandbox_image" => r.message.clone(),
                    field => format!("{field}: {}", r.message),
                })
                .collect::<Vec<_>>()
                .join("; "),
        }));
    }
    let verdict = pack.image;
    Ok(Ok(crate::runs::model::RunImage {
        reference: verdict.reference,
        digest: verdict.digest,
        capability_digest: verdict.capability_digest,
        overridden: verdict.overridden,
    }))
}

async fn start(db: &Db, cfg: &ControllerCfg, issue: &Issue) -> Result<()> {
    let day = crate::clock::today_utc();
    if db
        .decline_if_over_ceiling(&day, cfg.effective().daily_cost_ceiling)
        .await?
    {
        return Ok(());
    }

    let Some(launch) = crate::launches::store::get_playbook_launch(db.pool(), &issue.key).await?
    else {
        crate::issues::transitions::park(
            db.pool(),
            db.events(),
            &issue.key,
            Status::New,
            &ParkReason::PlaybookLaunchMissing,
            ParkedBy::Machine,
        )
        .await?;
        return Ok(());
    };
    let refusal = match plan_refusal(db, &issue.key, &launch).await {
        Ok(refusal) => refusal,
        Err(e) => {
            return Err(crate::runs::claim::record_dispatch_failure(
                db,
                &issue.key,
                Status::New,
                PLAYBOOK_DISPATCH_FAILED,
                e,
            )
            .await);
        }
    };
    if let Some(detail) = refusal {
        crate::issues::transitions::park(
            db.pool(),
            db.events(),
            &issue.key,
            Status::New,
            &ParkReason::PlanRefused { detail },
            ParkedBy::Machine,
        )
        .await?;
        return Ok(());
    }

    let run_id = new_run_id(&issue.key);
    // Both a draft test-fire and a registered launch resolve the playbook scope's bindings, against
    // the principals the launch row recorded. What differs is the revision: a registered launch is
    // checked against the one it is pinned to, while a draft has no reviewed revision, so only an
    // unpinned binding follows it.
    let (revision, pack_agent) = match launch.draft_version {
        Some(_) => (
            crate::secrets::launch::OwnedRevision::Draft,
            crate::playbooks::drafts::latest(db.pool(), &launch.playbook)
                .await?
                .and_then(|v| v.agent),
        ),
        None => {
            let pack = crate::playbooks::registry::get(db.pool(), &launch.playbook).await?;
            (
                crate::launches::store::launch_revision(db.pool(), &issue.key, &launch.playbook)
                    .await?,
                pack.and_then(|p| p.agent),
            )
        }
    };
    let dispatch =
        crate::playbooks::providers::resolve_for_issue(db.pool(), issue, WorkloadClass::Playbook)
            .await?;
    let image = match preflight_image(db, issue, pack_agent.as_ref(), dispatch.as_ref()).await? {
        Ok(image) => image,
        Err(reason) => {
            crate::issues::transitions::park(
                db.pool(),
                db.events(),
                &issue.key,
                Status::New,
                &reason,
                ParkedBy::Machine,
            )
            .await?;
            return Ok(());
        }
    };
    let agent = crate::playbooks::providers::AgentSelection::from_resolved(dispatch.as_ref());
    let decision = crate::playbooks::providers::resolve_decision(
        db.pool(),
        Some(&issue.repo),
        WorkloadClass::Playbook,
    )
    .await?;
    let inference = dispatch
        .iter()
        .map(|d| d.inference())
        .chain(decision)
        .collect();
    let exposure =
        crate::launches::store::launch_exposure(db.pool(), &issue.key, &launch).await??;
    let scope = crate::secrets::launch::Scope::playbook(&launch.playbook);
    let launcher = crate::authz::resolve::dispatch_principals(
        db.pool(),
        launch.created_by.as_deref(),
        &launch.launcher_groups,
    )
    .await?;
    let secrets = Some(crate::runs::workpod::LaunchSecrets {
        scope,
        launcher,
        revision,
        provider: cfg.secret_provider.clone(),
        inference,
        exposure,
    });
    let opts = crate::runs::workpod::RunRenderOpts::Playbook {
        params: launch.params,
        max_cost: launch.max_cost,
        max_time: launch.max_time,
        agent,
        rerun: launch.rerun,
        variant: launch.variant,
    };
    let claim = RunClaim {
        key: &issue.key,
        from: Status::New,
        run_id: &run_id,
        origin: RunOrigin::Playbook { image },
    };
    let started = match cfg.playbook_executor {
        PlaybookExecutor::Pod => dispatch_pod(db, cfg, issue, &claim, opts, secrets.as_ref()).await,
        PlaybookExecutor::Local => {
            start_local(db, cfg, issue, &claim, opts, secrets.as_ref()).await
        }
    };
    let started = match started {
        Ok(started) => started,
        Err(e) => return Err(claim.record_failure(db, e).await),
    };
    match started {
        // Capped: the issue stays at `new`, and the reconcile re-drives it when a slot frees.
        Started::Capped => db.ledger_append(None, "capped", 0.0).await,
        Started::SecretsRefused(detail) => {
            crate::issues::transitions::park(
                db.pool(),
                db.events(),
                &issue.key,
                Status::New,
                &ParkReason::SecretsUnresolved { detail },
                ParkedBy::Machine,
            )
            .await?;
            Ok(())
        }
        Started::ContractRejected | Started::ClaimLost => Ok(()),
        Started::Pod => {
            crate::runs::agent_tools::attribute_launched(db, cfg, &issue.key, &run_id).await;
            Ok(())
        }
        // The subprocess is supervised out of band, exactly as the pod watch drives a pod's
        // completion — and only once the run row it will fold into is committed.
        Started::Local(run) => {
            crate::runs::agent_tools::attribute_launched(db, cfg, &issue.key, &run_id).await;
            let db = db.clone();
            let key = issue.key.clone();
            tokio::spawn(async move { crate::runs::local_run::supervise(db, key, *run).await });
            Ok(())
        }
    }
}

/// Run the launch as a local subprocess: the same contract and binding checks as the pod arm,
/// then the claim, then the engine.
async fn start_local(
    db: &Db,
    cfg: &ControllerCfg,
    issue: &Issue,
    claim: &RunClaim<'_>,
    opts: crate::runs::workpod::RunRenderOpts,
    secrets: Option<&crate::runs::workpod::LaunchSecrets>,
) -> Result<Started> {
    let engine = crate::runs::contract::DispatchTarget::Binary(crate::runs::engine::resolve_bin());
    if let Err(failure) = cfg
        .runtime
        .admit_contract(crate::runs::contract::RequestKind::LocalRun, &[engine])
        .await
    {
        crate::issues::transitions::refuse(db, &issue.key, &failure.into_rejection()?).await?;
        return Ok(Started::ContractRejected);
    }
    if let Some(refusal) = resolve_local_secrets(db, cfg, issue, secrets).await? {
        return Ok(Started::SecretsRefused(refusal));
    }
    if u32::try_from(crate::runs::store::count_running(db.pool()).await?).unwrap_or(u32::MAX)
        >= cfg.effective().max_concurrent_pods
    {
        return Ok(Started::Capped);
    }
    let name = crate::runs::names::reserve(db.pool()).await?;
    let claimed = claim
        .claim(
            db,
            &RunPlacement {
                name: &name,
                dispatch: RunDispatch::Local,
                location: &RunLocation::hub(),
                work_pod: None,
                stamp_dispatcher: cfg.issues_agent_tokens(),
            },
        )
        .await?;
    if !claimed {
        return Ok(Started::ClaimLost);
    }
    match crate::runs::local_run::start(db, cfg, &issue.key, claim.run_id, opts).await {
        Ok(run) => Ok(Started::Local(Box::new(run))),
        Err(e) => Err(claim.release(db, None, e).await),
    }
}

/// The local executor runs the same binding resolution as the pod arm and additionally refuses
/// any launch that resolves a binding at all: a local run delivers no secrets, and running a
/// scope without the secrets it bound would be a silent downgrade rather than a refusal.
async fn resolve_local_secrets(
    db: &Db,
    cfg: &ControllerCfg,
    issue: &Issue,
    secrets: Option<&crate::runs::workpod::LaunchSecrets>,
) -> Result<Option<String>> {
    let Some(ls) = secrets else { return Ok(None) };
    let Some(pack) = crate::playbooks::packs::materialize_pack(db.pool(), &issue.key).await? else {
        return Ok(None);
    };
    let variant = crate::launches::store::launch_variant(db.pool(), &issue.key).await?;
    let declared = crate::secrets::manifest::declared_secrets(pack.path(), variant.as_ref())?;
    match crate::secrets::launch::resolve(
        db.pool(),
        &*cfg.runtime.policy()?,
        &ls.scope,
        &declared,
        &ls.launcher,
        ls.revision.as_revision(),
        ls.exposure.as_ref(),
    )
    .await?
    {
        Err(refusal) => Ok(Some(refusal.to_string())),
        Ok(mints) if !mints.is_empty() => Ok(Some(format!(
            "the local executor delivers no secrets, and this launch resolved {} binding(s); \
             dispatch it to a pod executor or unbind the secrets",
            mints.len()
        ))),
        Ok(_) => Ok(None),
    }
}

/// What a dispatch started, or why it started nothing.
enum Started {
    Pod,
    Local(Box<crate::runs::local_run::LocalRun>),
    /// The concurrency cap declined it. Nothing ran.
    Capped,
    /// The scope's bindings did not resolve for this launch. Nothing ran.
    SecretsRefused(String),
    /// The dispatch image's contract version is not this controller's. Ledgered and parked by the
    /// dispatch; nothing ran.
    ContractRejected,
    /// The issue left `new` before the dispatch claimed it. Nothing ran.
    ClaimLost,
}

async fn dispatch_pod(
    db: &Db,
    cfg: &ControllerCfg,
    issue: &Issue,
    claim: &RunClaim<'_>,
    opts: crate::runs::workpod::RunRenderOpts,
    secrets: Option<&crate::runs::workpod::LaunchSecrets>,
) -> Result<Started> {
    let pack = crate::playbooks::packs::materialize_pack(db.pool(), &issue.key)
        .await?
        .with_context(|| format!("no stored pack for playbook launch {}", issue.key))?;
    let admission = crate::runs::workpod::dispatch_run(
        db,
        cfg,
        cfg.runtime.dispatcher.clone(),
        claim,
        &pack,
        &std::collections::BTreeMap::new(),
        None,
        opts,
        secrets,
    )
    .await?;
    Ok(match admission {
        crate::runs::workpod::RunAdmission::Launched { .. } => Started::Pod,
        crate::runs::workpod::RunAdmission::Capped => Started::Capped,
        crate::runs::workpod::RunAdmission::SecretsRefused { reason } => {
            Started::SecretsRefused(reason)
        }
        crate::runs::workpod::RunAdmission::ContractRejected => Started::ContractRejected,
        crate::runs::workpod::RunAdmission::ClaimLost => Started::ClaimLost,
    })
}
