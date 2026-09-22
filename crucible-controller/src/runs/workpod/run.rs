#![allow(clippy::disallowed_macros)]

use crate::client::Db;
use crate::config::ControllerCfg;
use crate::issues::engine::{self, GroundedVerdict};
use crate::playbooks::providers::{AgentSelection, ModelProvider};
use crate::runs::workpod::spec::{self, TurnSpec as _};
use crate::runs::workpod::*;
use anyhow::{Context, Result, bail};
use crucible::deploy::{DigestResolver, PackDelivery, PlaybookLaunch, RenderOpts, render_yaml};
use crucible_contract::{ArtifactKind, ArtifactRef, Envelope, EnvelopeKind, content_digest};
use k8s_openapi::api::core::v1::{ConfigMap, Container, EnvVar, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------------------------
// WorkKind::Run — the full autoresearch loop pod, folded onto the primitive. Rendered from the pack
// manifest exactly as before, but dispatched + tracked + GC'd through `work_pods`. Its collection is
// out-of-band: the shared pod watch drives the `session.jsonl` ingest, so the dispatch here only ever
// CREATEs the pod (never blocks a reconcile for the run's multi-hour life).
// ---------------------------------------------------------------------------------------------

/// The controller-owned k8s object name for a loop-run pod: `crucible-run-<sanitized-run-id>`,
/// DNS-1123-label safe (lowercase alphanumerics + `-`, ≤63 chars). The controller owns the name
/// (overriding whatever the manifest render emitted) so its `work_pods` PK + the pod's ownerRef both
/// key on a value it chose. The run id already carries a per-launch epoch suffix, so this is unique
/// per launch of an issue.
pub fn run_pod_name(run_id: &str) -> String {
    let prefix = "crucible-run-";
    let budget = 63 - prefix.len();
    let sani: String = sanitize_dns_label(run_id).chars().take(budget).collect();
    let sani = sani.trim_matches('-');
    format!("{prefix}{sani}")
}

/// What a run pod is launched with, beyond the pack itself. Grouped rather than passed as five
/// adjacent scalars: the loop's knobs and a playbook's are different sets, and the enum is what
/// keeps a playbook from silently inheriting the loop's `--iterations`.
#[derive(Debug, Clone, PartialEq)]
pub enum RunRenderOpts {
    /// The scored autoresearch loop: the controller's iteration + budget knobs, and the fork a
    /// kept candidate opens its draft PR against.
    Loop {
        iterations: u32,
        max_cost: f64,
        pr_repo: Option<String>,
        agent: AgentSelection,
    },
    /// One launch of a playbook pack: the graph runs once, so no iteration budget, and the values
    /// and ceilings come off the stored launch row rather than the controller's config.
    Playbook {
        params: Vec<(String, String)>,
        max_cost: f64,
        max_time: crate::model::MaxTime,
        agent: AgentSelection,
    },
}

impl RunRenderOpts {
    /// The loop knobs for one issue: the controller's effective iteration + budget knobs, and the
    /// fork its stored repo opens its kept-commits draft PR against, from `pr_repo_map`. The caller
    /// passes the repo explicitly because adopted scenarios have opaque `scenario:{uuid}` keys;
    /// deriving a repo from an issue key works only for GitHub issues. An unmapped repo renders no
    /// `--pr-repo`, so the loop opens a PR only if the pack's own `[publish] pr_repo` names one.
    /// Never upstream.
    pub fn for_loop(cfg: &ControllerCfg, repo: &str, agent: AgentSelection) -> Self {
        let eff = cfg.effective();
        RunRenderOpts::Loop {
            iterations: eff.run_iterations,
            max_cost: eff.run_max_cost,
            pr_repo: cfg.pr_repo_for(repo),
            agent,
        }
    }

    /// The harness + model this run renders with, whichever kind it is.
    pub(crate) fn agent(&self) -> &AgentSelection {
        match self {
            RunRenderOpts::Loop { agent, .. } | RunRenderOpts::Playbook { agent, .. } => agent,
        }
    }

    /// The tracker item this run is parameterized by, exported as
    /// [`crate::issues::engine::ITEM_ENV`]. A playbook launch has no upstream item, so the engine's
    /// `tracker-comment` default resolves to nothing and refuses every write of that kind.
    pub(crate) fn tracker_item<'a>(&self, issue_key: &'a str) -> Option<&'a str> {
        match self {
            RunRenderOpts::Loop { .. } => Some(issue_key),
            RunRenderOpts::Playbook { .. } => None,
        }
    }

    /// The engine's render options for this run: what `crucible deploy render --pack` would parse
    /// off its flags. `cm_name` is the pack ConfigMap's run-unique name; `digests` the image-pinning
    /// resolver ([`crate::config::ControllerCfg::digest_resolver`]).
    fn render_opts(
        &self,
        cm_name: &str,
        digests: Option<Arc<dyn DigestResolver>>,
    ) -> Result<RenderOpts> {
        let pack = Some(PackDelivery {
            configmap_name: cm_name.to_string(),
        });
        let agent = self.agent();
        Ok(match self {
            RunRenderOpts::Loop {
                iterations,
                max_cost,
                pr_repo,
                ..
            } => RenderOpts {
                iterations: *iterations,
                max_cost: *max_cost,
                digests,
                pr_repo: pr_repo.clone().filter(|r| !r.is_empty()),
                pack,
                harness: agent.harness,
                model: agent.model.clone(),
                ..RenderOpts::default()
            },
            RunRenderOpts::Playbook {
                params,
                max_cost,
                max_time,
                ..
            } => RenderOpts {
                max_cost: *max_cost,
                digests,
                pack,
                playbook: Some(PlaybookLaunch {
                    max_time: max_time.engine()?,
                    max_cost: *max_cost,
                    params: params.iter().cloned().collect(),
                }),
                harness: agent.harness,
                model: agent.model.clone(),
                ..RenderOpts::default()
            },
        })
    }
}

/// Render one loop-run pod through the linked engine (`crucible::deploy::render_yaml` with pack
/// delivery, the library form of `crucible deploy render --pack`), returning the parsed, unstamped
/// [`Pod`] AND the pack [`ConfigMap`] the pod mounts (the pack lives only on the controller PVC,
/// never baked into the loop image). Blocking (pack + profile reads, image pinning), so the caller
/// runs it under `spawn_blocking`. `cm_name` is the run-unique ConfigMap object name the controller
/// owns; the render uses it for both the CM's name and the pod volume that references it, so the
/// two never drift. `opts` carries the per-kind run knobs ([`RunRenderOpts`]).
pub fn render_run_docs(
    pack_out: &Path,
    profile_path: &Path,
    cm_name: &str,
    opts: &RunRenderOpts,
    digests: Option<Arc<dyn DigestResolver>>,
) -> Result<(Pod, Option<ConfigMap>)> {
    let manifest = pack_out.join("crucible.toml");
    let yaml = render_yaml(
        &manifest,
        profile_path,
        &opts.render_opts(cm_name, digests)?,
    )?;
    extract_run_docs(&yaml)
}

/// Pull the `Pod` and (optional) `ConfigMap` documents out of a (possibly multi-doc) rendered
/// manifest — the loop pod, its RBAC/NetworkPolicy, and, under pack delivery, the pack ConfigMap. A
/// baked-domain render carries no ConfigMap, so `None` is the backward-compatible case. The Pod is
/// required; its absence is a render bug.
fn extract_run_docs(yaml: &str) -> Result<(Pod, Option<ConfigMap>)> {
    let mut pod = None;
    let mut cm = None;
    for doc in serde_norway::Deserializer::from_str(yaml) {
        let value = serde_norway::Value::deserialize(doc).context("parsing a rendered YAML doc")?;
        match value.get("kind").and_then(|k| k.as_str()) {
            Some("Pod") => {
                pod = Some(
                    serde_norway::from_value::<Pod>(value)
                        .context("decoding the rendered loop Pod")?,
                );
            }
            Some("ConfigMap") => {
                cm = Some(
                    serde_norway::from_value::<ConfigMap>(value)
                        .context("decoding the rendered pack ConfigMap")?,
                );
            }
            _ => {}
        }
    }
    let pod = pod.context("the rendered manifest contained no Pod document")?;
    Ok((pod, cm))
}

/// Stamp the controller-owned metadata onto the rendered pack ConfigMap so it is GC'd, selectable, and
/// attributable exactly like the pod it feeds: the managed-by pod-watch selector, the exact issue key
/// as an annotation, and — critically — an ownerReference to the POD (not the controller Deployment).
/// The k8s garbage collector then cascade-deletes the CM the instant the pod is deleted, at every
/// current and future pod-delete site (collection, the failed-pod sweeps, namespace teardown) with no
/// bespoke cleanup — the CM's lifetime is glued to the pod's. The render already set the CM's name +
/// namespace to the run-unique values, so this only adds ownership + selectors.
fn stamp_run_configmap(cm: &mut ConfigMap, pod: &Pod, issue_key: &str) {
    stamp_managed_meta(&mut cm.metadata, issue_key, owner_reference_to_pod(pod));
}

/// An ownerReference pointing at a just-created pod (its UID populated by the API server), for
/// cascade-GC'ing the pack ConfigMap with the pod. `None` when the pod has no name/UID yet (a fake
/// dispatcher that doesn't stamp a UID) — the CM is still created, just not cascade-owned, and the
/// managed-by selector keeps it sweepable.
fn owner_reference_to_pod(pod: &Pod) -> Option<OwnerReference> {
    let name = pod.metadata.name.clone().filter(|s| !s.is_empty())?;
    let uid = pod.metadata.uid.clone().filter(|s| !s.is_empty())?;
    Some(OwnerReference {
        api_version: "v1".to_string(),
        kind: "Pod".to_string(),
        name,
        uid,
        controller: Some(true),
        block_owner_deletion: Some(true),
    })
}

/// Stamp the controller-owned metadata onto a rendered loop-run pod: override its object name with
/// the controller-chosen [`run_pod_name`] (so the `work_pods` PK + ownerRef key on it), the exact
/// issue key + run id as annotations (both round-trip verbatim), the lossy issue-key label hint, the
/// `work-kind = run` label a sweep reconciles on, the managed-by pod-watch selector, and — when the
/// controller knows its own identity — an ownerReference for k8s cascade GC. `deploy render` set none
/// of these (it knows nothing of work kinds), so unlike the turn's [`stamp_pod`] this stamps them all.
///
/// `codegen_contract` is the resolved contract JSON for a broker-measured issue, projected onto the
/// loop container as `BROKER_CODEGEN_TOOLS_OVERLAY`. It is stamped here rather than passed to
/// `crucible deploy render` because the contract is a controller-side, per-issue fact: the render CLI
/// is shared with hand-run deploys and knows nothing about adopted scenarios.
pub fn stamp_run_pod(
    pod: &mut Pod,
    pod_name: &str,
    issue_key: &str,
    run_id: &str,
    owner: Option<OwnerReference>,
    codegen_contract: Option<&str>,
) {
    pod.metadata.name = Some(pod_name.to_string());
    apply_managed_meta(pod, issue_key, owner);
    pod.metadata
        .labels
        .get_or_insert_with(Default::default)
        .insert(
            WORK_KIND_LABEL.to_string(),
            WorkKind::Run.label_value().to_string(),
        );
    pod.metadata
        .annotations
        .get_or_insert_with(Default::default)
        .insert(
            crate::daemon::RUN_ID_ANNOTATION.to_string(),
            run_id.to_string(),
        );
    if let Some(contract) = codegen_contract {
        set_container_env(pod, CODEGEN_OVERLAY_ENV, contract);
    }
}

/// The env var the broker reads its per-scenario tool contract from, merged over
/// `BROKER_CODEGEM_TOOLS_DEFAULTS` (the deploy profile's cluster-wide default).
const CODEGEN_OVERLAY_ENV: &str = "BROKER_CODEGEN_TOOLS_OVERLAY";

/// Set one variable on every main container, replacing any pre-existing copy so a re-stamp is
/// idempotent. Init containers are left alone: they clone and stage, they don't run the engine.
pub(crate) fn set_container_env(pod: &mut Pod, name: &str, value: &str) {
    let Some(spec) = pod.spec.as_mut() else {
        return;
    };
    for c in spec.containers.iter_mut() {
        let env = c.env.get_or_insert_with(Default::default);
        env.retain(|v| v.name != name);
        env.push(EnvVar {
            name: name.to_string(),
            value: Some(value.to_string()),
            value_from: None,
        });
    }
}

/// The default identity a run's commits are authored and committed as. The env form outranks
/// `user.name`/`user.email` config, including the `-c` a pack's `setup_cmd` passes.
///
/// All four or none, per container: a container that sets any one of them is left alone, since
/// half a name and half an email is a commit by nobody.
pub(crate) fn stamp_git_identity(pod: &mut Pod, who: &crate::secrets::github_app::BotIdentity) {
    const VARS: [&str; 4] = [
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
    ];
    let Some(spec) = pod.spec.as_mut() else {
        return;
    };
    for container in spec.containers.iter_mut() {
        let env = container.env.get_or_insert_with(Default::default);
        if env.iter().any(|v| VARS.contains(&v.name.as_str())) {
            continue;
        }
        for (var, value) in [
            ("GIT_AUTHOR_NAME", &who.name),
            ("GIT_AUTHOR_EMAIL", &who.email),
            ("GIT_COMMITTER_NAME", &who.name),
            ("GIT_COMMITTER_EMAIL", &who.email),
        ] {
            env.push(EnvVar {
                name: var.to_string(),
                value: Some(value.to_string()),
                value_from: None,
            });
        }
    }
}

/// Override a rendered loop pod's container images with the pinned digests of the scope's succeeded
/// builds (the freshness guarantee): a container whose image REPO (registry/repo, tag or digest
/// stripped) matches a build's `image` is rewritten to that build's `registry/repo@sha256:…`, so the
/// run never executes on a stale or half-built image. Applies to both init and main containers. A
/// no-op when no container matches — the static manifest image stands, exactly as before the feature.
pub(crate) fn apply_build_digests(pod: &mut Pod, build_digests: &BTreeMap<String, String>) {
    if build_digests.is_empty() {
        return;
    }
    let Some(spec) = pod.spec.as_mut() else {
        return;
    };
    let mut containers: Vec<&mut Container> = spec.containers.iter_mut().collect();
    if let Some(init) = spec.init_containers.as_mut() {
        containers.extend(init.iter_mut());
    }
    for c in containers {
        if let Some(image) = &c.image
            && let Some(digest) = build_digests.get(image_repo(image))
        {
            c.image = Some(digest.clone());
        }
    }
}

/// The repo of an image ref: the registry/repo with any `@sha256:…` digest or `:tag` stripped
/// (`ghcr.io/org/x:m1` → `ghcr.io/org/x`, `ghcr.io/org/x@sha256:…` → `ghcr.io/org/x`). A registry
/// port (`localhost:5000/x`) is preserved because the `:` before the last `/` isn't a tag.
fn image_repo(image: &str) -> &str {
    let no_digest = image.split('@').next().unwrap_or(image);
    match no_digest.rfind('/') {
        Some(slash) => match no_digest[slash..].find(':') {
            Some(colon) => &no_digest[..slash + colon],
            None => no_digest,
        },
        None => no_digest.split(':').next().unwrap_or(no_digest),
    }
}

/// Whether a loop run was launched onto a fresh pod, or declined because the per-kind concurrency cap
/// (`max_concurrent_pods`) is full. Unlike a turn, a declined run never queues a `work_pods` row: the
/// issue itself stays at `awaiting-approval` — the durable, dedupe-per-issue queue the reconcile
/// re-drives when a slot frees — so a second work-pod queue would only double-track it.
#[derive(Debug)]
pub enum RunAdmission {
    Launched {
        pod_name: String,
        /// The cluster and namespace the pod was actually created on, for the run row to record.
        location: crate::runs::model::RunLocation,
    },
    Capped,
    /// The scope's bindings did not resolve for this launcher. Nothing was written and no pod was
    /// created; the caller parks the issue with `reason` so a human can bind or re-bind.
    SecretsRefused {
        reason: String,
    },
    /// The loop image or the pack's sandbox image carries another contract version. The issue is
    /// already ledgered and parked; nothing was written and no pod was created.
    ContractRejected,
}

/// What a dispatch needs to resolve the run's secrets: the scope whose bindings apply, who is
/// launching, and what content the launch runs. `None` at a call site means the launch resolves no
/// bindings at all — an import test-fire, which has no scope to bind against.
#[derive(Debug, Clone)]
pub struct LaunchSecrets {
    pub scope: crate::secrets::launch::Scope,
    pub launcher: crate::authz::model::Principals,
    /// The published revision the bindings must match, or `Draft` for a test-fire, which only
    /// unpinned bindings follow.
    pub revision: crate::secrets::launch::OwnedRevision,
    /// Where the bound values are read from at dispatch, to be written into the run.s Secret
    /// ([[ADR-0036]]). `None` leaves a scope that binds nothing launchable and one that binds
    /// something refused, rather than started without what it declared.
    pub provider: Option<Arc<dyn crate::secrets::provider::SecretProvider>>,
    /// The model provider this dispatch resolved to, whose registered key (if it named one) is
    /// projected alongside the scope's own bindings. `None` for a dispatch that resolved no
    /// provider, which is every dispatch under an empty registry.
    pub inference_provider: Option<ModelProvider>,
    /// The stored exposure of the exact revision being launched; a binding the agent can read has
    /// to be disclosed by it. `None` is absent-legacy, where the check is skipped.
    pub exposure: Option<crate::playbooks::exposure::Exposure>,
}

/// Record the refusal against the run's metrics and hand back the admission it resolves to.
fn secrets_refused(db: &Db, reason: String) -> RunAdmission {
    if let Some(m) = db.metrics() {
        m.record_turn(WorkKind::Run.label_value(), "secrets-refused");
    }
    RunAdmission::SecretsRefused { reason }
}

/// Dispatch one loop run onto the WorkPod primitive: check the concurrency cap (`max_concurrent_pods`
/// vs the in-flight run count, under the global daily ceiling the reconcile already holds), then
/// render the pack's loop pod through the linked engine, stamp it controller-owned, and CREATE it —
/// non-blocking. A run is hours long, so unlike a grounded turn it is never watched in-band here: its
/// completion arrives out-of-band on the shared pod watch, which ingests `session.jsonl`. A
/// `work_pods` row (kind `run`) is recorded BEFORE the pod is created, so a crash mid-create still
/// leaves a row the startup sweep reconciles. The run's cost is booked exactly once by that session
/// ingest — never by this primitive.
///
/// The cap counts `issues` at `running` (`count_running`, one run per running issue — today's
/// authoritative concurrent-run count), not the `work_pods` rows: gating on the existing count
/// preserves the exact bound and handles pre-primitive runs (which carry no `work_pods` row) with no
/// drift between two sources of truth. The `work_pods` row is the tracking/GC ledger on top.
// PRODUCER so Tempo's service-graph processor pairs this dispatch with the loop pod's CONSUMER
// `run` span and draws the async controller → crucible edge.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    skip(db, cfg, dispatcher, pack_out, build_digests),
    fields(otel.kind = "producer", %issue_key, %run_id)
)]
pub async fn dispatch_run(
    db: &Db,
    cfg: &ControllerCfg,
    dispatcher: Arc<dyn PodDispatcher>,
    issue_key: &str,
    run_id: &str,
    pack_out: &Path,
    build_digests: &BTreeMap<String, String>,
    codegen_contract: Option<&str>,
    opts: RunRenderOpts,
    secrets: Option<&LaunchSecrets>,
) -> Result<RunAdmission> {
    // The concurrency cap first — a declined launch needs no render config. The cap counts `issues`
    // at `running` (one run per running issue, the authoritative concurrent-run count today), under
    // the global daily ceiling the reconcile already enforced.
    let eff = cfg.effective();
    let active =
        u32::try_from(crate::runs::store::count_running(db.pool()).await?).unwrap_or(u32::MAX);
    if active >= eff.max_concurrent_pods {
        if let Some(m) = db.metrics() {
            m.record_turn(WorkKind::Run.label_value(), "capped");
        }
        return Ok(RunAdmission::Capped);
    }

    let profile = cfg.deploy_profile.clone().context(
        "dispatch_run needs a deploy profile: set CONTROLLER_DEPLOY_PROFILE (or --deploy-profile) \
         to the profile the loop-pod render reads",
    )?;

    let targets: Vec<crate::runs::contract::DispatchTarget> =
        crate::runs::contract::profile_target(&profile)
            .into_iter()
            .collect();
    if let Err(failure) = admit_contract(WorkKind::Run.into(), &targets).await {
        let rejection = failure.into_rejection()?;
        crate::runs::contract::refuse(db, issue_key, &rejection).await?;
        return Ok(RunAdmission::ContractRejected);
    }

    // Resolve the issue's contract NAME to the configured JSON here, before any row is written: a
    // name that no longer resolves (a redeploy dropped it from CONTROLLER_BROKER_CONTRACTS) must
    // fail the dispatch loudly. Silently launching would run a broker-measured pack against the
    // cluster-wide defaults, which measure a different workload entirely.
    let overlay = match codegen_contract {
        Some(name) => Some(cfg.broker_contracts.get(name).map(str::to_string).with_context(|| {
            format!(
                "issue names codegen contract {name:?}, which this controller does not have \
                 configured: set it in CONTROLLER_BROKER_CONTRACTS"
            )
        })?),
        None => None,
    };

    // Resolve the scope's bindings BEFORE anything is written: a refusal must leave no work-pod
    // row, no Secret, and no pod behind.
    let mints = match secrets {
        Some(ls) => {
            let declared = crate::secrets::manifest::declared_secrets(pack_out)?;
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
                Ok(mints) => mints,
                Err(refusal) => return Ok(secrets_refused(db, refusal.to_string())),
            }
        }
        None => Vec::new(),
    };

    let pod_name = run_pod_name(run_id);
    let cluster = crate::runs::workpod::issue_dispatch_cluster(db, cfg, issue_key).await?;
    // The provider's endpoint and key, resolved and read before the row insert so a refusal leaves
    // no work-pod row behind; a variable the scope's own bindings also claim is refused the same
    // way once both are known.
    let provider_extra = match secrets.and_then(|s| s.inference_provider.as_ref()) {
        None => None,
        Some(provider) => {
            let reader = secrets.and_then(|s| s.provider.as_ref());
            match crate::secrets::deliver::provider_delivery(db.pool(), reader, provider).await? {
                Ok(extra) => {
                    let taken =
                        extra
                            .env
                            .iter()
                            .chain(extra.plain_env.iter())
                            .find_map(|(var, _)| {
                                mints
                                    .iter()
                                    .find(|m| {
                                        m.item.projection_kind
                                            == crate::secrets::ProjectionKind::Env
                                            && m.item.projection == *var
                                    })
                                    .map(|m| (var.clone(), m.secret_name.clone()))
                            });
                    if let Some((var, bound)) = taken {
                        return Ok(secrets_refused(
                            db,
                            format!(
                                "provider {} sets {var}, which this scope's binding of {bound} \
                                 already holds; one of the two would be overwritten",
                                provider.id
                            ),
                        ));
                    }
                    Some(extra)
                }
                Err(refusal) => return Ok(secrets_refused(db, refusal.to_string())),
            }
        }
    };
    crate::runs::work_pods::insert_work_pod(
        db.pool(),
        &NewWorkPod {
            pod_name: pod_name.clone(),
            kind: WorkKind::Run.label_value().to_string(),
            issue_key: Some(issue_key.to_string()),
            state: WorkPodState::Running,
            cost_tag: WorkKind::Run.cost_tag().to_string(),
            cluster: cluster.clone(),
        },
    )
    .await?;

    // The bound values, read from Vault here so a run that cannot get them fails before it costs a
    // pod. They reach the pod as one mounted Secret; the spec names it and carries nothing else.
    let (mut delivery, pushes_as_the_app) = match mints.is_empty() {
        true => (crate::secrets::deliver::Delivery::default(), false),
        false => {
            let reader = secrets
                .and_then(|s| s.provider.clone())
                .context("this scope binds a secret and this deployment can read no values")?;
            let mut rows = Vec::with_capacity(mints.len());
            let mut values = Vec::with_capacity(mints.len());
            for mint in &mints {
                let row = crate::secrets::store::get(db.pool(), &mint.item.secret_id)
                    .await?
                    .with_context(|| {
                        format!(
                            "the binding for {} names no secret",
                            mint.item.declared_name
                        )
                    })?;
                let value = reader.value_of(&row).await?;
                // A reference-mode secret's far end can rotate into a controller key after the
                // binding was made, and this is the last stop before the value reaches a pod.
                crate::secrets::check_agent_visible_value(
                    row.visibility,
                    &crate::secrets::SecretValue::new(&value),
                )
                .with_context(|| format!("redeeming {} for this run", row.name))?;
                rows.push(row);
                values.push(value);
            }
            let by_the_app =
                crate::secrets::minter::any_bound(&rows, crate::secrets::minter::Minter::GithubApp);
            (
                crate::secrets::deliver::assemble(&mints, &rows, &values)?,
                by_the_app,
            )
        }
    };
    if let Some(extra) = provider_extra {
        delivery
            .merge(extra)
            .map_err(|e| anyhow::anyhow!("folding the provider's delivery into the run's: {e}"))?;
    }
    // A run pushing as the App has to have the identity, so a failed resolve fails the dispatch.
    // Everywhere else it is a default, and a failure degrades to the pack's own.
    let git_identity = match cfg.github_app.as_ref() {
        Some(app) => match app.identity().await {
            Ok(who) => Some(who),
            Err(e) if pushes_as_the_app => {
                return Err(e)
                    .context("resolving the App's git identity for a run that pushes as it");
            }
            Err(e) => {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "could not resolve the App's git identity; this run's commits keep whatever \
                     identity its pack sets"
                );
                None
            }
        },
        None => None,
    };
    let secret_name = crate::secrets::deliver::secret_name(&pod_name);

    let namespace = dispatcher
        .pod_namespace(&cluster, &cfg.pod_namespace)
        .await?;
    let digests = cfg.digest_resolver();
    let owner = owner_reference_from_env();
    let pack = pack_out.to_path_buf();
    let issue = issue_key.to_string();
    let run = run_id.to_string();
    let name = pod_name.clone();
    let public_url = cfg.public_url.clone();
    let secret_for_stamp = secret_name.clone();
    let delivery_for_stamp = delivery.clone();
    // The pack ConfigMap's run-unique name: render stamps it onto both the CM and the pod's volume
    // ref, so the controller never has to reconcile two names.
    let cm_name = format!("{pod_name}-pack");
    // The render reads the pack and pins images, so it stays on a blocking thread; the pod + CM
    // creates are plain async kube calls awaited here.
    let rendered = tokio::task::spawn_blocking(move || -> Result<(Pod, Option<ConfigMap>)> {
        let (mut pod, cm) = render_run_docs(&pack, &profile, &cm_name, &opts, digests)?;
        stamp_run_pod(&mut pod, &name, &issue, &run, owner, overlay.as_deref());
        if let Some(item) = opts.tracker_item(&issue) {
            set_container_env(&mut pod, crate::issues::engine::ITEM_ENV, item);
        }
        if let Some(url) = public_url.as_deref() {
            set_container_env(&mut pod, "CRUCIBLE_UI_BASE_URL", url.trim_end_matches('/'));
        }
        if let Some(who) = git_identity.as_ref() {
            stamp_git_identity(&mut pod, who);
        }
        crate::secrets::deliver::stamp(&mut pod, &secret_for_stamp, &delivery_for_stamp);
        Ok((pod, cm))
    })
    .await
    .context("joining the loop-run render task")?;
    let issue_for_cm = issue_key.to_string();
    // Create the pod FIRST (so its UID exists to owner-ref the CM to), then the CM. A pack pod waits
    // in ContainerCreating on its init-container's ConfigMap mount until the CM lands a beat later; the
    // CM owner-ref'd to the pod means k8s cascade-GCs it whenever the pod is deleted, so no delete site
    // needs to know the CM exists.
    let created = match rendered {
        Ok((mut pod, cm)) => {
            // Pin the run pod's image(s) to the digests the pack's builds produced, so the measurement
            // can never run on a stale or half-built image. A no-op for a build-free pack (empty map)
            // — the static manifest image stands.
            apply_build_digests(&mut pod, build_digests);
            // Inject this dispatch's W3C trace context so the loop pod's engine re-roots its `run`
            // span under it (a no-op when the controller isn't exporting spans).
            crate::runs::workpod::trace::inject_dispatch_context(&mut pod);
            match dispatcher
                .create(&cluster, &namespace, pod)
                .await
                .context("creating the loop-run pod")
            {
                Ok(created_pod) => {
                    if let Some(uid) = created_pod
                        .metadata
                        .uid
                        .as_deref()
                        .filter(|uid| !uid.is_empty())
                    {
                        crate::runs::work_pods::set_work_pod_uid(db.pool(), &pod_name, uid).await?;
                    }
                    // The values, owner-referenced to the pod that just came back so the cluster
                    // collects them with it. The pod waits in ContainerCreating on the mount until
                    // this lands, the same beat the pack ConfigMap takes.
                    if !delivery.is_empty() {
                        let object = crate::secrets::deliver::secret_object(
                            &secret_name,
                            &namespace,
                            &delivery,
                            &created_pod,
                        );
                        if object.metadata.owner_references.is_none() {
                            bail!(
                                "the pod create response carried no UID, so its Secret would \
                                 outlive every run that could have used it"
                            )
                        }
                        dispatcher
                            .create_secret(&cluster, &namespace, object)
                            .await
                            .context("creating the run Secret")?;
                    }
                    if let Some(mut cm) = cm {
                        stamp_run_configmap(&mut cm, &created_pod, &issue_for_cm);
                        dispatcher
                            .create_configmap(&cluster, &namespace, cm)
                            .await
                            .context("creating the pack ConfigMap")
                    } else {
                        Ok(())
                    }
                }
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    };

    match created {
        Ok(()) => {
            if let Some(m) = db.metrics() {
                m.record_turn(WorkKind::Run.label_value(), "launched");
            }
            Ok(RunAdmission::Launched {
                pod_name,
                location: crate::runs::model::RunLocation::new(cluster, Some(namespace)),
            })
        }
        Err(e) => {
            // Retain a failed-to-launch row with the real reason (swept after retention), then let the
            // reconcile see the error and retry the issue from `awaiting-approval`.
            let reason = format!("{e:#}");
            crate::runs::work_pods::set_work_pod_state(
                db.pool(),
                &pod_name,
                WorkPodState::Failed,
                None,
                Some(&reason),
            )
            .await?;
            sweep_failed_pod_overflow(
                db,
                dispatcher.as_ref(),
                &cfg.pod_namespace,
                cfg.effective().failed_pod_keep,
            )
            .await;
            if let Some(m) = db.metrics() {
                m.record_turn(WorkKind::Run.label_value(), "failed");
            }
            Err(e.context("dispatching the loop run"))
        }
    }
}

/// How a loop run ended, as the completion edge observed it. Collection outlives the pod, so the
/// `work_pods` note it leaves is the ledger's last word on the run and has to agree with the
/// outcome the session carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunDisposition {
    Finished,
    Errored,
}

impl RunDisposition {
    /// The `work_pods` note collection records for a run that ended this way.
    fn collection_note(self) -> &'static str {
        match self {
            Self::Finished => "loop run finished; session ingested",
            Self::Errored => "loop run errored; session ingested",
        }
    }
}

/// Collect a terminal loop run's pod: mark its `work_pods` row `collected` with the note
/// `disposition` calls for, and GC the pod. Its `session.jsonl` result was already ingested and its
/// cost booked once by the reconcile completion edge — this only closes the work-pod ledger +
/// deletes the pod. A no-op if the run has no `work_pods` row (a pre-primitive run) or was already
/// collected/swept. The delete is best-effort: a leaked pod is swept later, never fatal to the
/// completion.
pub async fn collect_run_pod(
    db: &Db,
    dispatcher: &dyn PodDispatcher,
    hub_namespace: &str,
    pod_name: &str,
    disposition: RunDisposition,
) -> Result<()> {
    let Some(row) = crate::runs::work_pods::get_work_pod(db.pool(), pod_name).await? else {
        return Ok(());
    };
    if matches!(row.state, WorkPodState::Collected | WorkPodState::Swept) {
        return Ok(());
    }
    crate::runs::work_pods::set_work_pod_state(
        db.pool(),
        pod_name,
        WorkPodState::Collected,
        Some(disposition.collection_note()),
        None,
    )
    .await?;
    if let Some(ns) =
        crate::runs::workpod::row_namespace(dispatcher, &row.cluster, hub_namespace).await
    {
        let _ = dispatcher.delete(&row.cluster, &ns, pod_name).await;
    }
    Ok(())
}

/// Prefer the kubelet-captured termination-message envelope, falling back to the marker log scrape
/// (compat: a new controller reads either an old engine image's marker or a new image's envelope —
/// both directions). A parseable verdict envelope is authoritative, INCLUDING its `{"error":…}`
/// no-verdict payload, so only an absent/unparseable/wrong-kind message reaches the marker scrape.
pub(crate) fn collect_verdict(message: Option<&str>, logs: &str) -> Result<GroundedVerdict> {
    if let Some(result) = message.and_then(verdict_from_termination) {
        return result;
    }
    parse_verdict_logs(logs)
}

/// Decode a verdict from the termination message: `Some` once the message parses to a verdict-kind
/// envelope (the inner `Result` then carries the verdict or the no-verdict error), `None` when the
/// message isn't our envelope at all (empty, old-image marker text, or a wrong-kind envelope) so the
/// caller can fall back to the marker scrape.
fn verdict_from_termination(message: &str) -> Option<Result<GroundedVerdict>> {
    let env: Envelope = serde_json::from_str(message.trim()).ok()?;
    if env.kind != EnvelopeKind::Verdict {
        return None;
    }
    let line = serde_json::to_string(&env.payload).ok()?;
    Some(engine::verdict_from_json_line(&line))
}

/// What one [`dispatch_grounded_rank`] call decided/produced. The caller's obligations differ per
/// variant, so this is a real enum instead of a lossy `Option`:
///   * [`DispatchOutcome::Verdict`] — use it; its cost is ALREADY ledgered (single-booking
///     invariant, see the collection site below). The caller must NOT ledger it again.
///   * [`DispatchOutcome::Launched`] — a turn pod was created (fresh, non-blocking) OR one was
///     already in flight for this issue: either way NO verdict exists yet. The caller defers exactly
///     like `Queued` (no tier finalize, no text fallback); the completion watch re-drives the issue
///     on the pod's terminal edge, where the adopt-first pre-pass collects it through the same tail.
///   * [`DispatchOutcome::Queued`] — the turn is deferred behind the per-kind cap/budget, never
///     dropped: the caller must NOT finalize a rank from the text verdict; the next reconcile pass
///     re-enters dispatch and drains the queued row when a slot/budget frees.
///   * [`DispatchOutcome::Failed`] — the turn ran and produced nothing usable (real error recorded
///     on the `work_pods` row); the caller keeps the text tier, exactly as the local arm on failure.
///   * [`DispatchOutcome::AlreadyCollected`] — a concurrent collector (a timeout sweep or a
///     re-drive) won the row's terminal CAS and is booking the cost + applying the verdict; this
///     caller must do NOTHING (not finalize, not keep the text tier), the winner owns it.
#[derive(Debug, Clone)]
pub enum DispatchOutcome {
    Verdict(GroundedVerdict),
    Launched,
    Queued,
    Failed,
    AlreadyCollected,
}

/// Dispatch one grounded-rank turn on the WorkPod primitive: check the per-kind cap + daily budget
/// (under the global ceiling the reconcile already holds), then either queue the turn (backpressure —
/// deduped per issue, drained on a later dispatch, never dropped) or spawn a pod and RETURN
/// [`DispatchOutcome::Launched`] without awaiting it. See [`DispatchOutcome`] for the caller's
/// contract.
///
/// NON-BLOCKING: the turn pod runs 5-40 minutes; the single serial queue worker must never park on
/// one, or the per-kind concurrency cap can't fan out (at most one turn would ever run). So dispatch
/// creates the pod and returns; the completion watch re-drives the issue on the pod's terminal edge,
/// where the adopt-first reconcile pre-pass collects it through the shared tail. A hung pod is reaped
/// out-of-band by [`sweep_timed_out_turns`].
///
/// The `dispatcher` is the cluster boundary (production [`KubePodDispatcher`], a fake in tests). The
/// render (pack + profile reads, image pinning) runs under `spawn_blocking`; the pod create is a
/// single async kube call.
// PRODUCER so Tempo's service-graph processor pairs this dispatch with the turn pod's CONSUMER
// `rank_grounded` span and draws the async controller → crucible edge.
#[tracing::instrument(
    skip(db, cfg, dispatcher),
    fields(otel.kind = "producer", %issue_key)
)]
pub(crate) async fn dispatch_grounded_rank(
    db: &Db,
    cfg: &ControllerCfg,
    dispatcher: Arc<dyn PodDispatcher>,
    issue_key: &str,
    repo_url: &str,
    git_ref: Option<&str>,
) -> Result<DispatchOutcome> {
    let kind = WorkKind::AgentTurn(TurnKind::GroundedRank);
    let profile = cfg.deploy_profile.clone().context(
        "dispatch_grounded_rank needs a deploy profile (grounded_executor = pod); validated at startup",
    )?;
    let sandbox_image = cfg.grounded_sandbox_image.clone().context(
        "dispatch_grounded_rank needs a sandbox image (grounded_executor = pod); validated at startup",
    )?;

    // ADOPTION (non-blocking): a turn already `running` for this issue must never be double-launched.
    // PEEK its pod first ([`turn_pod_adoptable`], a 1s poll) so the worker never parks on a live turn:
    //   * terminal (or vanished): COLLECT it on the shared tail — the peek guarantees the bounded
    //     await inside resolves on its first poll, so it never actually waits.
    //   * still running: return `Launched` — a turn is in flight, defer to its completion edge.
    // The adopt-first reconcile pre-pass already collects a terminal orphan before any gate; this is
    // the guard for a turn that went (or stayed) running past it in the same reconcile pass.
    if let Some(running) =
        crate::runs::work_pods::find_running_work_pod(db.pool(), kind.label_value(), issue_key)
            .await?
    {
        let running_ns = dispatcher
            .pod_namespace(&running.cluster, &cfg.pod_namespace)
            .await?;
        if !turn_pod_adoptable(
            dispatcher.as_ref(),
            &running.cluster,
            &running_ns,
            &running.pod_name,
        )
        .await
        {
            return Ok(DispatchOutcome::Launched);
        }
        if let Some(outcome) = adopt_grounded_turn(db, cfg, dispatcher.clone(), issue_key).await? {
            return Ok(outcome);
        }
        // The row vanished between the peek and the collect (a concurrent sweep won it); fall through
        // to a fresh admission below.
    }

    let targets: Vec<_> = crate::runs::contract::profile_target(&profile)
        .into_iter()
        .collect();
    if let Err(failure) = admit_contract(kind.into(), &targets).await {
        let rejection = failure.into_rejection()?;
        crate::runs::contract::refuse(db, issue_key, &rejection).await?;
        return Ok(DispatchOutcome::Failed);
    }

    // Resolve the overridable caps once (default < env < override).
    let eff = cfg.effective();
    // Cap + budget check, layered under the global ceiling the reconcile already enforced.
    let active =
        crate::runs::work_pods::count_active_work_pods(db.pool(), kind.label_value()).await?;
    let today = crate::clock::today_utc();
    let turns_today =
        crate::runs::work_pods::count_work_pod_turns_on_day(db.pool(), kind.label_value(), &today)
            .await?;

    let cluster = crate::runs::workpod::issue_dispatch_cluster(db, cfg, issue_key).await?;
    // An earlier over-cap dispatch may have left a queued row for this issue: reuse its reserved
    // pod name (so queue → spawn consumes ONE row) and never insert a duplicate.
    let queued_row =
        crate::runs::work_pods::find_queued_work_pod(db.pool(), kind.label_value(), issue_key)
            .await?;
    let pod_name = queued_row
        .as_ref()
        .map(|r| r.pod_name.clone())
        .unwrap_or_else(|| grounded_rank_pod_name(issue_key));
    let spec = WorkPodSpec::grounded_rank(
        pod_name,
        issue_key.to_string(),
        repo_url.to_string(),
        eff.per_reconcile_cost,
        sandbox_image,
        git_ref.map(str::to_string),
    );

    if admit(
        active,
        eff.grounded_rank_pod_cap,
        turns_today,
        eff.grounded_rank_daily_turns,
    ) == Admission::Queue
    {
        // Backpressure: persist a queued row (no TTL, one per issue) so the turn isn't lost, and
        // defer. The reconcile re-drives the issue later; the dispatch above drains the row when a
        // slot/budget frees.
        if queued_row.is_none() {
            crate::runs::work_pods::insert_work_pod(
                db.pool(),
                &spec.new_row(WorkPodState::Queued, &cluster),
            )
            .await?;
        }
        if let Some(m) = db.metrics() {
            m.record_turn(kind.label_value(), "queued");
        }
        return Ok(DispatchOutcome::Queued);
    }

    // Spawn: record the running row BEFORE creating the pod, so a crash mid-create still leaves a row
    // the startup sweep can reconcile against the cluster. A queued row is promoted in place
    // (consuming it — nothing stays `queued` once its turn actually runs); its `created_at` resets to
    // now, so the daily turn budget counts the day the turn actually dispatched, not the day it queued.
    if let Some(queued) = &queued_row {
        crate::runs::work_pods::promote_queued_work_pod(db.pool(), &spec.pod_name).await?;
        // The drain is where the queue wait is observable — the row's age is how long it waited.
        if let Some(m) = db.metrics()
            && let Ok(waited) = elapsed_secs_since(&queued.created_at)
        {
            m.observe_queue_wait(waited);
        }
    } else {
        crate::runs::work_pods::insert_work_pod(
            db.pool(),
            &spec.new_row(WorkPodState::Running, &cluster),
        )
        .await?;
    }

    // Non-blocking launch: render + create the pod, then RETURN `Launched`. The running row is
    // already persisted (above), so a crash mid-create leaves a row the startup sweep reconciles.
    // The render/create/launch-failure machinery is the shared turn-dispatch tail
    // ([`spec::spawn_turn_pod`]/[`spec::fail_launch`]) — only the admission/queue block above stays
    // bespoke to grounded rank (scope has none).
    let namespace = dispatcher
        .pod_namespace(&cluster, &cfg.pod_namespace)
        .await?;
    match spec::spawn_turn_pod(
        dispatcher.as_ref(),
        &spec,
        spec::GroundedRankSpec.pod_noun(),
        &profile,
        cfg.digest_resolver(),
        &cluster,
        &namespace,
        // A rank turn runs the triage harness the profile configures, so it resolves no provider
        // and spends no provider key.
        &crate::secrets::deliver::Delivery::default(),
    )
    .await?
    {
        Ok(()) => {
            if let Some(m) = db.metrics() {
                m.record_turn(kind.label_value(), "launched");
            }
            Ok(DispatchOutcome::Launched)
        }
        Err(e) => {
            spec::fail_launch(
                db,
                dispatcher.as_ref(),
                kind,
                &spec.pod_name,
                &cfg.pod_namespace,
                &e,
                eff.failed_pod_keep,
                issue_key,
            )
            .await;
            Ok(DispatchOutcome::Failed)
        }
    }
}

/// The adopt-only entry for a grounded turn: if a `running` work-pod row exists for this issue,
/// re-enter the watch/collect on its existing pod and fold the result through the single shared
/// collection tail ([`crate::runs::workpod::spec::TurnSpec::adopt`]). `None` = no running row (the caller
/// decides whether to dispatch fresh; the adopt-first reconcile pre-pass then does NOTHING — this
/// entry never renders, creates, or queues a pod, so calling it can never turn into new spend).
///
/// Traced: the caller only reaches it once the pod is terminal, so entering here means a real
/// collection (scrape the pod's logs, parse the verdict, book the cost), never an idle poll.
#[tracing::instrument(skip(db, cfg, dispatcher), fields(%issue_key))]
pub(crate) async fn adopt_grounded_turn(
    db: &Db,
    cfg: &ControllerCfg,
    dispatcher: Arc<dyn PodDispatcher>,
    issue_key: &str,
) -> Result<Option<DispatchOutcome>> {
    Ok(spec::GroundedRankSpec
        .adopt(db, cfg, dispatcher, issue_key)
        .await?
        .map(grounded_into_dispatch_outcome))
}

/// [`spec::TurnCollected`] → [`DispatchOutcome`]: `Failed` drops the reason (the unit variant here
/// keeps the caller on the text tier without a string to thread), `AlreadyCollected` maps straight
/// across.
fn grounded_into_dispatch_outcome(
    collected: spec::TurnCollected<GroundedVerdict>,
) -> DispatchOutcome {
    match collected {
        spec::TurnCollected::Result { result, .. } => DispatchOutcome::Verdict(result),
        spec::TurnCollected::Failed { .. } => DispatchOutcome::Failed,
        spec::TurnCollected::AlreadyCollected => DispatchOutcome::AlreadyCollected,
    }
}

/// The outcome of a scope turn dispatched on the WorkPod primitive.
#[derive(Debug, Clone)]
pub enum ScopeOutcome {
    /// The scope turn completed and produced a ScopeReport, with its cost already booked.
    /// `pod_name` is the turn pod that ran it, so the caller can persist the report with the
    /// operator's handle for cluster-log correlation.
    Report {
        report: engine::ScopeReport,
        pod_name: String,
    },
    /// The turn failed (no usable report), with the reason. The caller events it on the issue.
    Failed(String),
    /// A scope turn pod was created (fresh, non-blocking) OR one was already in flight for this
    /// issue: no report exists yet. The caller does NOTHING and leaves the issue at its current
    /// status — the completion watch re-drives it on the pod's terminal edge, where the adopt-first
    /// pre-pass collects the report and transitions the issue.
    Launched,
    /// A concurrent collector (a timeout sweep, or another re-drive of the key) won the row's
    /// terminal CAS and is folding the report + booking the cost; this caller must do NOTHING (the
    /// winner persists the report and transitions the issue).
    AlreadyCollected,
}

/// Dispatch one scope-propose turn on the WorkPod primitive: render a turn pod that runs
/// `crucible scope --propose --json --marker` and CREATE it, then RETURN [`ScopeOutcome::Launched`]
/// without awaiting. Same non-blocking discipline as [`dispatch_grounded_rank`]: a scope turn runs
/// 40-120+ minutes (longer with gaming-review cycles), so the single serial worker must never park
/// on one. Collection is out-of-band — the completion watch re-drives the issue on the pod's terminal
/// edge and the adopt-first pre-pass scrapes the report off the logs. A hung pod is reaped by
/// [`sweep_timed_out_turns`], whose scope deadline scales with the gaming allowance ([`scope_deadline`]).
///
/// Unlike grounded ranking, scope turns have no per-kind concurrency cap or daily turn budget (those
/// gates are enforced by the reconcile cascade: scopes/day, daily ceiling, etc.), so there is no
/// admission/queue step here — scope IS the generic no-queue dispatcher,
/// [`spec::dispatch_turn`]`(&ScopeSpec, ...)`.
// PRODUCER so Tempo's service-graph processor pairs this dispatch with the turn pod's CONSUMER
// `scope` span and draws the async controller → crucible edge.
#[tracing::instrument(
    skip(db, cfg, dispatcher, inputs),
    fields(otel.kind = "producer", %issue_key)
)]
pub async fn dispatch_scope(
    db: &Db,
    cfg: &ControllerCfg,
    dispatcher: Arc<dyn PodDispatcher>,
    issue_key: &str,
    repo_url: &str,
    max_cost: f64,
    inputs: TurnInputs,
) -> Result<ScopeOutcome> {
    Ok(scope_dispatch_into_outcome(
        spec::dispatch_turn(
            &spec::ScopeSpec,
            db,
            cfg,
            dispatcher,
            issue_key,
            repo_url,
            max_cost,
            inputs,
        )
        .await?,
    ))
}

/// [`spec::TurnDispatch`] → [`ScopeOutcome`]: `Launched` maps straight across, `Collected` folds
/// through the existing [`scope_into_outcome`] tail mapper, `Failed{reason}` keeps the reason
/// (`ScopeOutcome::Failed` carries a `String`, matching the launch-fail create-context string
/// surviving into the reconcile event).
fn scope_dispatch_into_outcome(dispatch: spec::TurnDispatch<engine::ScopeReport>) -> ScopeOutcome {
    match dispatch {
        spec::TurnDispatch::Launched => ScopeOutcome::Launched,
        spec::TurnDispatch::Collected(collected) => scope_into_outcome(collected),
        spec::TurnDispatch::Failed { reason } => ScopeOutcome::Failed(reason),
    }
}

/// The reconcile adopt-first pre-pass's PEEK: is there a `running` work-pod row for this
/// `(kind, issue_key)` whose pod can be adopted right now — already terminal, or vanished —
/// without blocking the single queue worker on a still-running turn? Folds
/// `find_running_work_pod` + [`turn_pod_adoptable`] into the one guard both the scope and
/// grounded pre-pass blocks repeated. `Some(row)` only when adoption is safe NOW; a still-running
/// orphan (or no orphan at all) is `None` — left for the shared pod-completion watch to re-drive
/// this key when the pod goes terminal.
pub(crate) async fn peek_running_adoptable(
    db: &Db,
    cfg: &ControllerCfg,
    dispatcher: &dyn PodDispatcher,
    kind: &str,
    issue_key: &str,
) -> Result<Option<WorkPodRow>> {
    let Some(row) =
        crate::runs::work_pods::find_running_work_pod(db.pool(), kind, issue_key).await?
    else {
        return Ok(None);
    };
    let ns = dispatcher
        .pod_namespace(&row.cluster, &cfg.pod_namespace)
        .await?;
    if turn_pod_adoptable(dispatcher, &row.cluster, &ns, &row.pod_name).await {
        Ok(Some(row))
    } else {
        Ok(None)
    }
}

/// The adopt-only entry for a scope turn: if a `running` work-pod row exists for this issue,
/// re-enter the watch/collect on its existing pod and fold the result through the single shared
/// collection tail ([`crate::runs::workpod::spec::TurnSpec::adopt`]). `None` = no running row. Same
/// contract as [`adopt_grounded_turn`]: never renders, creates, or queues a pod.
///
/// Traced for the same reason as [`adopt_grounded_turn`]: reaching it means collecting a finished
/// scope turn, which is work worth a trace.
#[tracing::instrument(skip(db, cfg, dispatcher), fields(%issue_key))]
pub(crate) async fn adopt_scope_turn(
    db: &Db,
    cfg: &ControllerCfg,
    dispatcher: Arc<dyn PodDispatcher>,
    issue_key: &str,
) -> Result<Option<ScopeOutcome>> {
    Ok(spec::ScopeSpec
        .adopt(db, cfg, dispatcher, issue_key)
        .await?
        .map(scope_into_outcome))
}

/// [`spec::TurnCollected`] → [`ScopeOutcome`]: `Failed` keeps the reason (`ScopeOutcome::Failed`
/// carries a `String`, unlike the grounded mapper's unit variant), `AlreadyCollected` maps straight
/// across.
fn scope_into_outcome(collected: spec::TurnCollected<engine::ScopeReport>) -> ScopeOutcome {
    match collected {
        spec::TurnCollected::Result { result, pod_name } => ScopeOutcome::Report {
            report: result,
            pod_name,
        },
        spec::TurnCollected::Failed { reason } => ScopeOutcome::Failed(reason),
        spec::TurnCollected::AlreadyCollected => ScopeOutcome::AlreadyCollected,
    }
}

/// Prefer the kubelet-captured termination-message envelope for the scope report, falling back to
/// the marker log scrape (compat, exactly as [`collect_verdict`]). A parseable scope-report envelope
/// is authoritative; only an absent/unparseable/wrong-kind message reaches the marker scrape.
pub(crate) fn collect_scope_report(
    message: Option<&str>,
    logs: &str,
) -> Result<engine::ScopeReport> {
    if let Some(result) = message.and_then(|m| scope_report_from_termination(m, logs)) {
        return result;
    }
    parse_scope_report_logs(logs)
}

/// Decode a scope report from the termination message: `Some` once the message parses to a
/// scope-report envelope, `None` when it isn't our envelope so the caller falls back to the marker.
fn scope_report_from_termination(message: &str, logs: &str) -> Option<Result<engine::ScopeReport>> {
    let env: Envelope = serde_json::from_str(message.trim()).ok()?;
    if env.kind != EnvelopeKind::ScopeReport {
        return None;
    }
    Some(scope_report_from_envelope(env, logs))
}

/// Reconstruct the scope report from the kubelet-captured envelope: the report core rides the
/// termination message, but the pack + transcript are log markers, so they attach from the logs
/// exactly as [`parse_scope_report_logs`] does. `raw` is the report's own JSON (what the
/// `scope_reports` store keeps), matching the marker path where `raw` is the marker line body.
fn scope_report_from_envelope(env: Envelope, logs: &str) -> Result<engine::ScopeReport> {
    let raw = serde_json::to_string(&env.payload)
        .context("re-serializing the scope report from the termination envelope")?;
    let mut report: engine::ScopeReport = serde_json::from_value(env.payload)
        .context("parsing the scope report from the termination envelope")?;
    report.raw = raw;
    report.transcript_gz = parse_scope_transcript_logs(logs);
    (report.pack_tgz, report.pack_error) = parse_scope_pack_logs(logs);
    Ok(report)
}

/// The Tier 1 artifacts manifest carried by a scope-report termination message, or empty when the
/// message is absent/unparseable/not our envelope (the old-engine marker path — no manifest, so the
/// drop-box preference is a no-op and the log-scraped payloads stand).
pub(crate) fn manifest_from_message(message: Option<&str>) -> Vec<ArtifactRef> {
    let Some(m) = message else {
        return Vec::new();
    };
    let Ok(env) = serde_json::from_str::<Envelope>(m.trim()) else {
        return Vec::new();
    };
    if env.kind != EnvelopeKind::ScopeReport {
        return Vec::new();
    }
    env.artifacts
}

/// Prefer the drop-box artifacts over log-scraped payloads, validating each against its manifest
/// digest. The controller trusts the kubelet-authenticated manifest, then checks the drop-box against
/// it — so a missing, undelivered, or digest-mismatched artifact is caught without trusting the POST
/// path at all.
///
/// A surviving scope's PACK is required: if the manifest lists it but the drop-box can't hand back
/// the exact bytes, this returns `Err` with a loud reason (→ a NoReport that lands on the work_pods
/// row). The transcript is best-effort — a problem there is logged, not fatal, matching the marker
/// path's long-standing discipline (a garbled transcript never failed a report).
pub(crate) async fn apply_dropbox_artifacts(
    report: &mut engine::ScopeReport,
    manifest: &[ArtifactRef],
    pool: &sqlx::PgPool,
    pod: &str,
) -> Result<(), String> {
    let survived = report.digest.is_some() && report.stages.iter().all(|s| s.passed);
    for art in manifest {
        match art.kind {
            ArtifactKind::ScopePack => match resolve_dropbox_artifact(pool, pod, art).await {
                Ok(bytes) => {
                    report.pack_tgz = Some(bytes);
                    report.pack_error = None;
                }
                Err(problem) => {
                    // A dead proposal has no pack to deliver; only a survival's missing pack is fatal.
                    if survived {
                        return Err(format!(
                            "scope pack not recovered from the drop-box: {problem}"
                        ));
                    }
                }
            },
            ArtifactKind::ScopeTranscript => match resolve_dropbox_artifact(pool, pod, art).await {
                Ok(bytes) => report.transcript_gz = Some(bytes),
                Err(problem) => {
                    tracing::warn!(
                        pod,
                        %problem,
                        "scope transcript not recovered from the drop-box (best-effort)"
                    );
                }
            },
            // A scope turn never uploads a loop run's evidence or (in R2) an otel-log; ignore any
            // stray entry.
            ArtifactKind::RunSession | ArtifactKind::RunFiles | ArtifactKind::OtelLog => {}
        }
    }
    Ok(())
}

/// Resolve one manifest entry to its drop-box bytes, validating the content digest. `Err` when the
/// pod marked it undelivered, no artifact is stored, or the stored digest doesn't match the
/// manifest.
async fn resolve_dropbox_artifact(
    pool: &sqlx::PgPool,
    pod: &str,
    art: &ArtifactRef,
) -> Result<Vec<u8>, String> {
    if !art.delivered {
        return Err(format!(
            "the manifest marks {} delivered:false (the pod's POST failed after retries)",
            art.kind
        ));
    }
    let owner = crate::runs::blob_store::ArtifactOwner::PodEvidence {
        pod: pod.to_string(),
    };
    let payload = crate::runs::blob_store::get_artifact(pool, &owner, art.kind.as_str())
        .await
        .map_err(|e| format!("reading drop-box {}: {e:#}", art.kind))?
        .ok_or_else(|| format!("no {} in the drop-box store", art.kind))?;
    let digest = content_digest(&payload.data);
    if digest != art.digest {
        return Err(format!(
            "{} digest {digest} != manifest {} (integrity mismatch)",
            art.kind, art.digest
        ));
    }
    Ok(payload.data)
}

/// Seconds elapsed since an RFC3339 `…Z` stamp (the ledger's format), for observing how long a
/// queued turn waited before it drained. Never negative (a clock skew clamps to zero).
pub(crate) fn elapsed_secs_since(ts: &str) -> Result<f64> {
    let then = parse_ts(ts)?;
    Ok(std::time::SystemTime::now()
        .duration_since(then)
        .unwrap_or(Duration::ZERO)
        .as_secs_f64())
}

/// A turn's wall-clock duration at collection, from its running row's `created_at` dispatch stamp —
/// the turn-duration metric's source now that collection is out-of-band (the start isn't on this
/// stack). A skewed or unparseable stamp yields 0 rather than a bogus negative/huge sample.
pub(crate) fn turn_age_secs(created_at: &str) -> f64 {
    elapsed_secs_since(created_at).unwrap_or(0.0)
}
