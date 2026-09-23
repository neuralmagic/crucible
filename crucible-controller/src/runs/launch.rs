//! `new` for a playbook launch: dispatch the pack straight onto the run pod. There is no scope
//! turn ahead of it — the pack already exists, pinned and registered — and no approval gate, since
//! the validated POST that wrote the launch row is the human authorization. The row's params and
//! ceilings are re-read here rather than carried on the queue, because a dequeued key carries no
//! payload.

use crate::client::Db;
use crate::config::{ControllerCfg, PlaybookExecutor};
use crate::event_log::Event;
use crate::issues::model::Issue;
use crate::model::{ParkReason, ParkedBy, Status};
use crate::playbooks::providers::WorkloadClass;
use crate::runs::model::new_run_id;
use crate::runs::model::{NewRun, RunDispatch};
use anyhow::{Context, Result};

/// The reason stamped on an attempt that could not start the engine. The launch view reads its
/// dispatch state back off these events, so the text is a shared constant rather than a literal.
pub(crate) const PLAYBOOK_DISPATCH_FAILED: &str = "playbook dispatch failed";

pub(crate) async fn launch(db: &Db, cfg: &ControllerCfg, issue: &Issue) -> Result<()> {
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
                crate::secrets::launch::OwnedRevision::Published(
                    pack.as_ref().map(|p| p.rev.clone()),
                ),
                pack.and_then(|p| p.agent),
            )
        }
    };
    let dispatch =
        crate::playbooks::providers::resolve_for_issue(db.pool(), issue, WorkloadClass::Playbook)
            .await?;
    // The same preflight the launch endpoint ran, against the catalog as it stands now and the
    // harness this dispatch actually resolved. A refusal parks rather than spends.
    let image = match pack_agent.as_ref() {
        Some(pack_agent) => {
            let resolved =
                dispatch
                    .as_ref()
                    .map(|d| crate::playbooks::preflight::ResolvedHarness {
                        harness: d.harness,
                        provider: d.provider.id.clone(),
                        source: if issue.agent_provider.is_some() {
                            crate::playbooks::preflight::HarnessSource::Pin
                        } else {
                            crate::playbooks::preflight::HarnessSource::Default
                        },
                    });
            let catalog = crate::images::store::list_images(db.pool()).await?;
            let verdict =
                crate::playbooks::preflight::preflight(pack_agent, resolved.as_ref(), &catalog);
            if verdict.refused() {
                crate::issues::transitions::park(
                    db.pool(),
                    db.events(),
                    &issue.key,
                    Status::New,
                    &ParkReason::ImagePreflightRefused {
                        image: verdict
                            .reference
                            .unwrap_or_else(|| "(no image)".to_string()),
                        detail: verdict.refusals.join("; "),
                    },
                    ParkedBy::Machine,
                )
                .await?;
                return Ok(());
            }
            crate::runs::model::RunImage {
                reference: verdict.reference,
                digest: verdict.digest,
                capability_digest: verdict.capability_digest,
                overridden: verdict.overridden,
            }
        }
        None => crate::runs::model::RunImage::default(),
    };
    let agent = crate::playbooks::providers::AgentSelection::from_resolved(dispatch.as_ref());
    let exposure = match launch.exposure.clone() {
        Some(exposure) => Some(exposure),
        None => crate::playbooks::exposure::registered(db.pool(), &launch.playbook)
            .await?
            .flatten(),
    };
    let secrets = Some(crate::runs::workpod::LaunchSecrets {
        scope: crate::secrets::launch::Scope::playbook(&launch.playbook),
        launcher: crate::authz::model::Principals::new(
            launch.created_by.as_deref(),
            &launch.launcher_groups,
        ),
        revision,
        provider: cfg.secret_provider.clone(),
        inference_provider: dispatch.map(|d| d.provider),
        exposure,
    });
    let opts = crate::runs::workpod::RunRenderOpts::Playbook {
        params: launch.params,
        max_cost: launch.max_cost,
        max_time: launch.max_time,
        agent,
    };
    let started = match cfg.playbook_executor {
        PlaybookExecutor::Pod => {
            dispatch_pod(db, cfg, issue, &run_id, opts, secrets.as_ref()).await
        }
        PlaybookExecutor::Local => {
            let engine =
                crate::runs::contract::DispatchTarget::Binary(crate::runs::engine::resolve_bin());
            match crate::runs::workpod::admit_contract(
                crate::runs::contract::RequestKind::LocalRun,
                &[engine],
            )
            .await
            {
                Ok(()) => match resolve_local_secrets(db, issue, secrets.as_ref()).await? {
                    Some(refusal) => Ok(Some(Started::SecretsRefused(refusal))),
                    None => crate::runs::local_run::start(db, cfg, &issue.key, &run_id, opts)
                        .await
                        .map(|run| run.map(|r| Started::Local(Box::new(r)))),
                },
                Err(failure) => {
                    crate::runs::contract::refuse(db, &issue.key, &failure.into_rejection()?)
                        .await?;
                    return Ok(());
                }
            }
        }
    };
    let started = match started {
        Ok(Some(s)) => s,
        // Capped: the issue stays at `new`, and the reconcile re-drives it when a slot frees.
        Ok(None) => {
            db.ledger_append(None, "capped", 0.0).await?;
            return Ok(());
        }
        Err(e) => {
            let _ = db
                .events()
                .append(&Event::now(
                    &issue.key,
                    "new",
                    "new",
                    Some(PLAYBOOK_DISPATCH_FAILED),
                    Some(&crate::model::truncate_chain(&format!("{e:#}"))),
                ))
                .await;
            return Err(e);
        }
    };

    if let Started::SecretsRefused(detail) = &started {
        crate::issues::transitions::park(
            db.pool(),
            db.events(),
            &issue.key,
            Status::New,
            &ParkReason::SecretsUnresolved {
                detail: detail.clone(),
            },
            ParkedBy::Machine,
        )
        .await?;
        return Ok(());
    }
    let (pod, dispatch, location, reason) = match &started {
        Started::Pod { pod_name, location } => (
            Some(pod_name.clone()),
            RunDispatch::Pod,
            location.clone(),
            "playbook pod launched",
        ),
        Started::Local(_) => (
            None,
            RunDispatch::Local,
            crate::runs::model::RunLocation::hub(),
            "playbook running locally",
        ),
        // Handled above; the issue is parked and nothing is dispatched.
        Started::SecretsRefused(_) | Started::ContractRejected => return Ok(()),
    };
    let mut tx = db.pool().begin().await?;
    let claimed =
        crate::issues::store::claim_issue(&mut *tx, &issue.key, Status::New, Status::Running)
            .await?;
    if claimed {
        crate::runs::store::insert_run(
            &mut *tx,
            &NewRun {
                run_id: run_id.clone(),
                scope: None,
                issue: Some(issue.key.clone()),
                identity_digest: None,
                status: "running".to_string(),
                pod,
                session_uri: None,
                best_score: None,
                cost_usd: None,
            },
        )
        .await?;
        crate::runs::store::attribute_run(&mut *tx, &run_id).await?;
        crate::runs::store::set_run_dispatch(&mut *tx, &run_id, dispatch).await?;
        crate::runs::store::set_run_location(&mut *tx, &run_id, &location).await?;
        crate::runs::store::set_run_image(&mut *tx, &run_id, &image).await?;
        let ev = Event::now(&issue.key, "new", "running", Some(reason), Some(&run_id));
        crate::event_log::insert(&mut *tx, &ev).await?;
        tx.commit().await?;
        db.events().publish(&ev);
    }
    // The subprocess is supervised out of band, exactly as the pod watch drives a pod's
    // completion — and only once the run row it will fold into is committed.
    if let Started::Local(run) = started {
        let db = db.clone();
        let key = issue.key.clone();
        tokio::spawn(async move { crate::runs::local_run::supervise(db, key, *run).await });
    }
    Ok(())
}

/// The local executor runs the same binding resolution as the pod arm and additionally refuses
/// any launch that resolves a binding at all: a local run delivers no secrets, and running a
/// scope without the secrets it bound would be a silent downgrade rather than a refusal.
async fn resolve_local_secrets(
    db: &Db,
    issue: &Issue,
    secrets: Option<&crate::runs::workpod::LaunchSecrets>,
) -> Result<Option<String>> {
    let Some(ls) = secrets else { return Ok(None) };
    let Some(pack) = crate::playbooks::packs::materialize_pack(db.pool(), &issue.key).await? else {
        return Ok(None);
    };
    let declared = crate::secrets::manifest::declared_secrets(pack.path())?;
    match crate::secrets::launch::resolve(
        db.pool(),
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

/// What a dispatch started. `None` from [`dispatch_pod`] is the concurrency cap declining it.
enum Started {
    Pod {
        pod_name: String,
        location: crate::runs::model::RunLocation,
    },
    Local(Box<crate::runs::local_run::LocalRun>),
    /// The scope's bindings did not resolve for this launch. Nothing ran.
    SecretsRefused(String),
    /// The dispatch image's contract version is not this controller's. Ledgered and parked by the
    /// dispatch; nothing ran.
    ContractRejected,
}

async fn dispatch_pod(
    db: &Db,
    cfg: &ControllerCfg,
    issue: &Issue,
    run_id: &str,
    opts: crate::runs::workpod::RunRenderOpts,
    secrets: Option<&crate::runs::workpod::LaunchSecrets>,
) -> Result<Option<Started>> {
    let pack = crate::playbooks::packs::materialize_pack(db.pool(), &issue.key)
        .await?
        .with_context(|| format!("no stored pack for playbook launch {}", issue.key))?;
    crate::launches::schedules::ScheduleStore::new(db.clone())
        .stage_cursor_file(&issue.key, pack.path())
        .await?;
    let admission = crate::runs::workpod::dispatch_run(
        db,
        cfg,
        crate::runs::workpod::active_dispatcher(),
        &issue.key,
        run_id,
        pack.path(),
        &std::collections::BTreeMap::new(),
        None,
        opts,
        secrets,
    )
    .await?;
    Ok(match admission {
        crate::runs::workpod::RunAdmission::Launched { pod_name, location } => {
            Some(Started::Pod { pod_name, location })
        }
        crate::runs::workpod::RunAdmission::Capped => None,
        crate::runs::workpod::RunAdmission::SecretsRefused { reason } => {
            Some(Started::SecretsRefused(reason))
        }
        crate::runs::workpod::RunAdmission::ContractRejected => Some(Started::ContractRejected),
    })
}
