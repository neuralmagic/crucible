//! `playbook` targets (RFC-0008 C-TARGETS, C-CHILD-RUN): a target run is a launch of the target
//! playbook of its own, made with the authority of the calling run's launch owner.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context as _;
use crucible_contract::calls::{
    CallKind, CallStatus, CallTarget, FileRef, NodeDetail, NodeLink, NodeStatus, Pin, PlaybookRef,
    ProgressEdge, ProgressGraph, ProgressNode, TargetHandle, TargetInterface,
};
use crucible_contract::emits::{DeclaredFile, EmitWire};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Digest as _;

use crate::api::state::ApiState;
use crate::calls::{
    CallCtx, CallError, CallRecord, DispatchSpec, Dispatched, Outcome, Parent, Polled, Resolved,
    TargetBackend,
};
use crate::playbooks::api::admission::{Admissible as _, LaunchPack, LaunchRequest, LaunchTarget};

/// Launches and follows playbook target runs. Without a request's state it follows and cancels
/// target runs but launches none.
pub(crate) struct PlaybookBackend {
    state: Option<ApiState>,
}

impl PlaybookBackend {
    pub(crate) fn new(state: Option<ApiState>) -> Self {
        PlaybookBackend { state }
    }
}

/// A compiled target pack: its interface and the task whose output is its result.
#[derive(Debug, Clone, PartialEq)]
struct Compiled {
    interface: TargetInterface,
    result: Option<String>,
}

#[derive(Deserialize)]
struct Plan {
    #[serde(default)]
    result: Option<String>,
    #[serde(default, rename = "task")]
    tasks: Vec<PlanTask>,
}

#[derive(Deserialize)]
struct PlanTask {
    name: String,
    kind: String,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    emits: crucible::plan::ir::Emits,
    #[serde(default)]
    emits_files: Vec<DeclaredFile>,
}

/// The interface of the pack at `root`: its params block, its result task's emits and
/// emits_files, and its compiled graph with every node pending. Blocking.
fn compile(
    root: &Path,
    variant: Option<&crucible::manifest::variant::VariantName>,
) -> Result<Compiled, String> {
    let source = crate::playbooks::registry::workflow_source_for(
        root,
        crate::playbooks::registry::PackWorkflowKind::Playbook,
        variant,
    )
    .map_err(|e| e.to_string())?;
    let text = std::fs::read_to_string(&source)
        .map_err(|e| format!("reading {}: {e}", source.display()))?;
    let schema = crucible::plan::starlark::declared_params(&text, &source)
        .map_err(|e| crucible::errors::report(&e))?;
    let values = crate::playbooks::preview::effective(
        Some(&schema),
        &BTreeMap::new(),
        crate::playbooks::preview::Unvalued::Placeholder,
    );
    let compiled = crucible::plan::starlark::compile_file_with(
        &source,
        crucible::plan::starlark::parent_or_cwd(&source),
        &values,
    )
    .map_err(|e| crucible::errors::report(&e))?;
    let plan: Plan = serde_json::from_str(&compiled.canonical_json)
        .map_err(|e| format!("the engine's compiled plan did not parse: {e}"))?;
    Ok(interface_of(schema, plan))
}

fn interface_of(params: Value, plan: Plan) -> Compiled {
    let result_task = plan
        .result
        .as_deref()
        .and_then(|r| plan.tasks.iter().find(|t| t.name == r));
    let result = result_task
        .map(|t| {
            t.emits
                .fields()
                .into_iter()
                .map(|(field, ty)| EmitWire {
                    field: field.0.clone(),
                    ty: ty.cloned(),
                })
                .collect()
        })
        .unwrap_or_default();
    let result_files = result_task
        .map(|t| t.emits_files.clone())
        .unwrap_or_default();
    let edges = plan
        .tasks
        .iter()
        .flat_map(|t| {
            t.depends_on.iter().map(|dep| ProgressEdge {
                from: dep.clone(),
                to: t.name.clone(),
            })
        })
        .collect();
    let nodes = plan
        .tasks
        .iter()
        .map(|t| ProgressNode {
            id: t.name.clone(),
            label: t.name.clone(),
            status: NodeStatus::Pending,
            group: None,
            started: None,
            ended: None,
            detail: Some(NodeDetail::PlaybookTask {
                task_kind: t.kind.clone(),
            }),
            link: None,
        })
        .collect();
    Compiled {
        interface: TargetInterface {
            params,
            result,
            result_files,
            graph: ProgressGraph::bounded(nodes, edges),
        },
        result: plan.result,
    }
}

/// [`compile`] a stored tree off the async runtime. A pack that does not compile is a refusal.
async fn compile_tree(
    tree: crucible_contract::pack_tree::PackTree,
    variant: Option<crucible::manifest::variant::VariantName>,
) -> Result<Compiled, CallError> {
    tokio::task::spawn_blocking(move || {
        let scratch = tempfile::tempdir().context("a scratch dir for the target pack")?;
        let root = scratch.path().join("pack");
        crate::playbooks::packs::write_checked_tree(&tree, &root)?;
        Ok::<_, anyhow::Error>(compile(&root, variant.as_ref()))
    })
    .await
    .context("joining the target compile")??
    .map_err(|e| CallError::Refused(format!("the target pack does not compile: {e}")))
}

/// What a registered row pins: its tree, else its legacy bytes.
fn revision_digest(row: &crate::playbooks::registry::PlaybookRow) -> String {
    row.tree_digest
        .as_ref()
        .map(|t| t.to_string())
        .unwrap_or_else(|| row.tar_digest.clone())
}

async fn registered(
    pool: &sqlx::PgPool,
    name: &str,
) -> Result<crate::playbooks::registry::PlaybookRow, CallError> {
    crate::playbooks::registry::get(pool, name)
        .await?
        .ok_or_else(|| CallError::Refused(format!("no registered playbook {name:?}")))
}

async fn registered_tree(
    pool: &sqlx::PgPool,
    name: &str,
) -> Result<crucible_contract::pack_tree::PackTree, CallError> {
    crate::playbooks::registry::pack(pool, name)
        .await?
        .ok_or_else(|| CallError::Refused(format!("no registered playbook {name:?}")))
}

fn draft_refusal(e: crate::playbooks::drafts::DraftError) -> CallError {
    use crate::playbooks::drafts::DraftError;
    match e {
        DraftError::Internal(e) => CallError::Internal(e),
        other => CallError::Refused(other.to_string()),
    }
}

/// A value as the launch form's text, which the target's params schema reads back.
fn param_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Write each file the call binds where the target run reads it, under the controller's scratch
/// root, and answer the launch value of each: the staged file's reference as JSON.
async fn stage_files(
    ctx: &CallCtx,
    spec: &DispatchSpec<'_>,
) -> Result<BTreeMap<String, String>, CallError> {
    let mut staged = BTreeMap::new();
    for (param, file) in spec.files {
        let bytes = crate::calls::store::input_file(ctx.db.pool(), &spec.parent.run, &file.digest)
            .await?
            .ok_or_else(|| {
                CallError::Refused(format!(
                    "{param} binds {}, which the calling run never uploaded",
                    file.digest
                ))
            })?;
        let name = file
            .path
            .as_deref()
            .and_then(|p| Path::new(p).file_name())
            .and_then(|n| n.to_str())
            .unwrap_or(param.as_str());
        let dir = ctx
            .scratch_dir
            .join("call-inputs")
            .join(&spec.call.0)
            .join(param);
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(name);
        tokio::fs::write(&path, &bytes)
            .await
            .with_context(|| format!("staging {}", path.display()))?;
        let mut reference = json!({
            "path": path.to_string_lossy(),
            "digest": file.digest,
            "size": file.size,
        });
        if let Some(media_type) = &file.media_type {
            reference["media_type"] = json!(media_type);
        }
        staged.insert(param.clone(), reference.to_string());
    }
    Ok(staged)
}

/// The note a refusing launch check answered with.
async fn refused_by(response: axum::response::Response) -> CallError {
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap_or_default();
    let note = match serde_json::from_slice::<Value>(&body) {
        Ok(v) => {
            let error = v.get("error").and_then(Value::as_str).unwrap_or("refused");
            let fields: Vec<String> = v
                .get("fields")
                .and_then(Value::as_array)
                .map(|fields| {
                    fields
                        .iter()
                        .map(|f| {
                            let field = f.get("field").and_then(Value::as_str).unwrap_or("");
                            let message = f.get("message").and_then(Value::as_str).unwrap_or("");
                            match field.is_empty() {
                                true => message.to_string(),
                                false => format!("{field}: {message}"),
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            match fields.is_empty() {
                true => error.to_string(),
                false => format!("{error}: {}", fields.join("; ")),
            }
        }
        Err(_) => String::from_utf8_lossy(&body).into_owned(),
    };
    CallError::Refused(format!("the target launch was refused ({status}): {note}"))
}

/// Refuse unless the calling run's launch owner may launch `target` (RFC-0003 C-DECISION).
async fn authorize(
    state: &ApiState,
    parent: &Parent,
    target: LaunchTarget<'_>,
) -> Result<(), CallError> {
    let owner = crate::authz::model::Principal::stored(parent.owner.as_deref());
    let subject =
        crate::authz::firing::subject_for_owner(state.db.pool(), &owner, parent.groups.as_ref())
            .await
            .map_err(|e| CallError::Refused(format!("{owner} cannot launch a target: {e:#}")))?;
    let action = crate::authz::action::Action {
        resource: crate::authz::action::ResourceType::Playbook,
        verb: crate::authz::action::Verb::Launch,
    };
    let decision = state.policy.current().authorize(
        &subject,
        action,
        &target.resource(),
        jiff::Timestamp::now().as_second(),
    );
    match decision.allowed {
        true => Ok(()),
        false => Err(CallError::Refused(format!(
            "{owner} may not launch playbook {}: {}",
            target.id(),
            decision.reason()
        ))),
    }
}

fn launch_of(call: &CallRecord) -> Result<String, CallError> {
    call.detail
        .get("launch")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| CallError::Unrecorded {
            call: call.id.clone(),
            what: "target launch",
        })
}

fn result_task(call: &CallRecord) -> Option<String> {
    call.detail
        .get("result")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The newest run of launch `key` and its status.
async fn latest_run(pool: &sqlx::PgPool, key: &str) -> anyhow::Result<Option<(String, String)>> {
    sqlx::query_as("SELECT run_id, status FROM runs WHERE issue = $1 ORDER BY seq DESC LIMIT 1")
        .bind(key)
        .fetch_optional(pool)
        .await
        .context("reading a target launch's newest run")
}

/// One captured file of run `run` by its `<task>/<path>` key.
async fn run_file(ctx: &CallCtx, run: &str, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
    match crate::runs::blob_store::get_run_bundle(
        ctx.db.pool(),
        run,
        crucible_contract::ArtifactKind::RunFiles,
    )
    .await?
    {
        Some(bundle) => crate::runs::run_files::read_bundle(&bundle, key),
        None => Ok(crate::runs::run_files::read_local(&ctx.scratch_dir, run, key).await),
    }
}

fn file_key(task: &str, path: &str) -> Option<String> {
    crate::runs::task_evidence::task_segment(task).map(|dir| format!("{dir}/{path}"))
}

async fn passed(ctx: &CallCtx, call: &CallRecord, run: &str) -> Result<Outcome, CallError> {
    let Some(task) = result_task(call) else {
        return Ok(Outcome {
            status: CallStatus::Passed,
            note: String::new(),
            result: json!({}),
            result_files: Vec::new(),
        });
    };
    let session = crate::runs::task_evidence::run_session(ctx.db.pool(), &ctx.scratch_dir, run)
        .await?
        .unwrap_or_default();
    let result = crate::runs::task_evidence::session_result(&session, &task)
        .and_then(|r| r.payload)
        .unwrap_or_else(|| json!({}));
    let mut result_files = Vec::new();
    for declared in &call.interface.result_files {
        let Some(key) = file_key(&task, &declared.path) else {
            continue;
        };
        let Some(bytes) = run_file(ctx, run, &key).await? else {
            return Ok(Outcome::failed(format!(
                "target run {run} passed without its result file {}",
                declared.path
            )));
        };
        result_files.push(FileRef {
            digest: format!("sha256:{:x}", sha2::Sha256::digest(&bytes)),
            size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            path: Some(declared.path.clone()),
            media_type: None,
        });
    }
    Ok(Outcome {
        status: CallStatus::Passed,
        note: String::new(),
        result,
        result_files,
    })
}

/// The target run's graph now.
async fn progress(ctx: &CallCtx, call: &CallRecord) -> Result<ProgressGraph, CallError> {
    let launch = launch_of(call)?;
    let Some((run, _)) = latest_run(ctx.db.pool(), &launch).await? else {
        return Ok(call.interface.graph.pending());
    };
    let session = crate::runs::task_evidence::run_session(ctx.db.pool(), &ctx.scratch_dir, &run)
        .await?
        .unwrap_or_default();
    Ok(crate::calls::progress::fold(
        &session,
        &run,
        &call.interface.graph,
    ))
}

async fn outcome(ctx: &CallCtx, call: &CallRecord) -> Result<Option<Outcome>, CallError> {
    let launch = launch_of(call)?;
    let pool = ctx.db.pool();
    let parked = || async {
        let issue = crate::issues::store::get_issue(pool, &launch).await?;
        Ok::<_, anyhow::Error>(match issue {
            None => Some(format!("the target launch {launch} is gone")),
            Some(issue) if issue.status == crate::model::Status::Parked => Some(
                issue
                    .parked_reason
                    .unwrap_or_else(|| "the target launch parked".to_string()),
            ),
            Some(_) => None,
        })
    };
    match latest_run(pool, &launch).await? {
        None => Ok(parked()
            .await?
            .map(|why| Outcome::failed(format!("the target launch did not start: {why}")))),
        Some((_, status)) if status == "running" => Ok(None),
        Some((run, status)) if status == "finished" => passed(ctx, call, &run).await.map(Some),
        Some((run, status)) => {
            let why = parked()
                .await?
                .map(|w| format!(": {w}"))
                .unwrap_or_default();
            Ok(Some(Outcome::failed(format!(
                "target run {run} ended {status}{why}"
            ))))
        }
    }
}

#[async_trait::async_trait]
impl TargetBackend for PlaybookBackend {
    async fn interface(&self, ctx: &CallCtx, target: &CallTarget) -> Result<Resolved, CallError> {
        let CallTarget::Playbook { playbook } = target else {
            return Err(CallError::Refused(format!(
                "a {} target is not a playbook",
                target.kind()
            )));
        };
        let pool = ctx.db.pool();
        match playbook {
            PlaybookRef::Registered(name) => {
                let row = registered(pool, name).await?;
                let tree = registered_tree(pool, name).await?;
                let compiled = compile_tree(tree, row.variant.clone()).await?;
                Ok(Resolved {
                    pin: Pin::Revision {
                        playbook: name.clone(),
                        digest: revision_digest(&row),
                    },
                    interface: compiled.interface,
                })
            }
            PlaybookRef::Draft(id) => {
                let (_, tree) = crate::playbooks::drafts::newest_compiling(pool, id)
                    .await
                    .map_err(draft_refusal)?;
                let compiled = compile_tree(tree, None).await?;
                Ok(Resolved {
                    pin: Pin::DraftHead { draft: id.clone() },
                    interface: compiled.interface,
                })
            }
        }
    }

    async fn dispatch(
        &self,
        ctx: &CallCtx,
        spec: &DispatchSpec<'_>,
    ) -> Result<Dispatched, CallError> {
        let Some(state) = &self.state else {
            return Err(CallError::FollowOnly(CallKind::Playbook));
        };
        let CallTarget::Playbook { playbook } = spec.target else {
            return Err(CallError::Refused(format!(
                "call target {} is not a playbook target",
                spec.alias
            )));
        };
        let pool = state.db.pool();
        let (pack, tree, variant) = match playbook {
            PlaybookRef::Registered(name) => {
                let row = registered(pool, name).await?;
                let Pin::Revision { digest, .. } = &spec.locked.pin else {
                    return Err(CallError::Refused(format!(
                        "calls.lock pins call target {} to something other than a revision",
                        spec.alias
                    )));
                };
                let now = revision_digest(&row);
                if now != *digest {
                    return Err(CallError::Refused(format!(
                        "playbook {name} was registered again since calls.lock was written \
                         (locked {digest}, now {now}); rewrite the lock"
                    )));
                }
                let tree = registered_tree(pool, name).await?;
                let variant = row.variant.clone();
                (LaunchPack::Registered(row), tree, variant)
            }
            PlaybookRef::Draft(id) => {
                if !spec.parent.draft {
                    return Err(CallError::Refused(format!(
                        "call target {} names draft {id}, which only a draft pack may call",
                        spec.alias
                    )));
                }
                let draft = crate::playbooks::drafts::get(pool, id)
                    .await?
                    .ok_or_else(|| CallError::Refused(format!("no draft {id:?}")))?;
                if draft.retired_at.is_some() {
                    return Err(CallError::Refused(format!(
                        "draft {id} retired when its graduated pack was imported"
                    )));
                }
                let latest = crate::playbooks::drafts::latest(pool, id)
                    .await?
                    .ok_or_else(|| CallError::Refused(format!("draft {id} has no saves")))?;
                let version = latest.version;
                let head = crate::playbooks::api::admission::DraftHead::compiled(draft, latest)
                    .ok_or_else(|| {
                        CallError::Refused(format!(
                            "version {version} of draft {id} does not compile"
                        ))
                    })?;
                let (_, tree) = crate::playbooks::drafts::version_tree(pool, id, Some(version))
                    .await?
                    .ok_or_else(|| CallError::Refused(format!("no draft {id:?}")))?;
                (LaunchPack::Draft(head), tree, None)
            }
        };
        let compiled = compile_tree(tree, variant).await?;
        let digest = compiled
            .interface
            .digest()
            .context("digesting the target interface")?;
        if digest != spec.locked.interface_digest {
            return Err(CallError::Refused(format!(
                "the interface of call target {} changed since calls.lock was written: {}",
                spec.alias,
                crate::calls::lock::drift(&spec.locked.interface, &compiled.interface)
            )));
        }
        authorize(state, spec.parent, pack.target()).await?;

        let caps = &state.playbook_caps;
        let budget = spec.budget.unwrap_or_default();
        let max_cost = budget
            .cost_usd
            .filter(|c| c.is_finite() && *c > 0.0)
            .map_or(caps.max_cost, |c| c.min(caps.max_cost));
        let max_time = budget
            .wall_clock_secs
            .filter(|s| *s > 0)
            .map_or(caps.max_time.secs(), |s| s.min(caps.max_time.secs()));
        let max_time = format!("{max_time}s");
        let params: BTreeMap<String, String> = spec
            .values
            .iter()
            .map(|(k, v)| (k.clone(), param_text(v)))
            .collect();
        let staged = stage_files(ctx, spec).await?;
        let authorized = match crate::playbooks::api::admission::admit_staged(
            state,
            pack,
            LaunchRequest {
                params: &params,
                max_cost,
                max_time: &max_time,
                schema_digest: None,
            },
            &staged,
        )
        .await
        {
            Ok(authorized) => authorized,
            Err(response) => return Err(refused_by(response).await),
        };
        let target = authorized.pack.target();
        if let Err(response) = crate::playbooks::api::registry::authorize_image(
            state,
            target.agent(),
            None,
            target.repo(),
        )
        .await
        {
            return Err(refused_by(response).await);
        }

        let key = format!("playbook:{}:{}", target.id(), uuid::Uuid::now_v7());
        let reserved = crate::runs::model::new_run_id(&key);
        let description = match &authorized.pack {
            LaunchPack::Registered(row) => row.description.clone(),
            LaunchPack::Draft(head) => {
                format!("draft {}: {}", head.draft.id, head.draft.description)
            }
        };
        let title = format!(
            "{description} (called by {} {})",
            spec.parent.run, spec.task
        );
        let launch = crate::launches::model::NewPlaybookLaunch {
            choice_snapshots: Some(&authorized.choice_snapshots),
            playbook: target.id(),
            repo: target.launch_repo(),
            title: &title,
            params: &authorized.params,
            schema_digest: target.schema_digest(),
            max_cost: authorized.max_cost,
            max_time: &authorized.max_time,
            advance_dedupe: false,
            dedupe_schedule: None,
            origin: crate::model::LaunchOrigin::Call,
            draft_version: target.draft_version(),
            created_by: spec.parent.created_by.as_deref(),
            owner: spec.parent.owner.as_deref(),
            launcher_groups: spec.parent.groups.as_ref(),
            rerun: None,
            dispatch: Default::default(),
        };
        let adopted = match &authorized.pack {
            LaunchPack::Registered(row) => {
                use crate::launches::store::AdoptPlaybookOutcome;
                match crate::launches::store::adopt_playbook_launch(
                    pool,
                    &key,
                    &launch,
                    row.revision(),
                )
                .await?
                {
                    AdoptPlaybookOutcome::Adopted => Ok(()),
                    AdoptPlaybookOutcome::UnknownPlaybook => {
                        Err(format!("no registered playbook {:?}", row.id))
                    }
                    AdoptPlaybookOutcome::SchemaDrifted { current } => Err(format!(
                        "playbook {} was registered again during the dispatch (schema {current})",
                        row.id
                    )),
                    AdoptPlaybookOutcome::Repinned { rev } => Err(format!(
                        "playbook {} was registered again at {rev} during the dispatch",
                        row.id
                    )),
                }
            }
            LaunchPack::Draft(head) => {
                use crate::launches::store::AdoptDraftOutcome;
                let exposure =
                    crate::playbooks::drafts::exposure_of(pool, &head.draft.id, head.version)
                        .await
                        .map_err(draft_refusal)?
                        .ok_or_else(|| {
                            CallError::Refused(format!("no draft {:?}", head.draft.id))
                        })?;
                match crate::launches::store::adopt_draft_launch(pool, &key, &launch, &exposure)
                    .await?
                {
                    AdoptDraftOutcome::Adopted => Ok(()),
                    AdoptDraftOutcome::UnknownDraft => Err(format!("no draft {:?}", head.draft.id)),
                    AdoptDraftOutcome::Saved { current } => Err(format!(
                        "draft {} was saved during the dispatch (now {current})",
                        head.draft.id
                    )),
                }
            }
        };
        adopted.map_err(CallError::Refused)?;
        crate::calls::store::link_child(
            pool,
            &key,
            spec.parent,
            spec.task,
            spec.instance,
            &reserved,
        )
        .await?;
        state
            .audit(
                crate::event_log::Event::now(
                    &key,
                    "new",
                    "new",
                    Some(&format!(
                        "called by run {} task {} as call target {}, call {} ({} params, max_cost \
                         {}, max_time {})",
                        spec.parent.run,
                        spec.task,
                        spec.alias,
                        spec.call,
                        params.len(),
                        authorized.max_cost,
                        authorized.max_time
                    )),
                    None,
                )
                .by(spec.parent.created_by.as_deref()),
                "call_dispatch",
            )
            .await;
        state
            .queue
            .enqueue_urgent(crate::daemon::queue::IssueKey(key.clone()));
        Ok(Dispatched {
            handle: TargetHandle::Playbook {
                run: reserved.clone(),
            },
            link: Some(NodeLink::Run {
                run: reserved,
                task: None,
            }),
            detail: json!({"launch": key, "result": compiled.result}),
        })
    }

    async fn poll(&self, ctx: &CallCtx, call: &CallRecord) -> Result<Polled, CallError> {
        let progress = match progress(ctx, call).await {
            Ok(progress) => progress,
            Err(e) => {
                tracing::warn!(call = %call.id, error = %e.note(), "reading a call's progress failed");
                call.progress.clone()
            }
        };
        Ok(Polled {
            progress,
            outcome: outcome(ctx, call).await?,
        })
    }

    async fn result_file(
        &self,
        ctx: &CallCtx,
        call: &CallRecord,
        path: &str,
    ) -> Result<Option<Vec<u8>>, CallError> {
        let (Some(task), Some(TargetHandle::Playbook { .. })) = (result_task(call), &call.handle)
        else {
            return Ok(None);
        };
        if !call
            .result_files
            .iter()
            .any(|f| f.path.as_deref() == Some(path))
        {
            return Ok(None);
        }
        let launch = launch_of(call)?;
        let Some((run, _)) = latest_run(ctx.db.pool(), &launch).await? else {
            return Ok(None);
        };
        let Some(key) = file_key(&task, path) else {
            return Ok(None);
        };
        Ok(run_file(ctx, &run, &key).await?)
    }

    async fn cancel(&self, ctx: &CallCtx, call: &CallRecord) -> Result<(), CallError> {
        let launch = launch_of(call)?;
        crate::daemon::overrides::OverrideStore::new(ctx.db.pool().clone())
            .stash(crate::daemon::queue::Override {
                key: crate::daemon::queue::IssueKey(launch.clone()),
                kind: crate::daemon::queue::OverrideKind::Cancel,
                reason: Some(format!("cancelled by its calling run {}", call.parent_run)),
                priority: None,
                actor: None,
            })
            .await?;
        if let Some(state) = &self.state {
            state
                .queue
                .enqueue_urgent(crate::daemon::queue::IssueKey(launch));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::calls::playbook::*;
    use crate::testing::fixtures::write_playbook_pack;

    #[test]
    fn a_pack_compiles_to_its_params_result_and_graph() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = write_playbook_pack(
            dir.path(),
            "params = {\"pr\": {\"type\": \"int\", \"required\": True}}\n\
             scan = command(name = \"scan\", run = \"true\")\n\
             report = command(name = \"report\", run = \"true\", depends_on = [scan], \
             emits = {\"verdict\": \"string\"}, emits_files = [\"report.md\"])\n\
             workflow(type = \"playbook\", tasks = [scan, report], result = report)\n",
        );
        let compiled = compile(&root, None).expect("compiles");
        assert_eq!(compiled.result.as_deref(), Some("report"));
        let interface = &compiled.interface;
        assert!(
            interface.params["properties"]["pr"].is_object(),
            "{}",
            interface.params
        );
        assert_eq!(interface.result.len(), 1);
        assert_eq!(interface.result[0].field, "verdict");
        assert!(interface.result[0].ty.is_some());
        assert_eq!(interface.result_files[0].path, "report.md");
        let ids: Vec<&str> = interface
            .graph
            .nodes
            .iter()
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(ids, ["scan", "report"]);
        assert!(
            interface
                .graph
                .nodes
                .iter()
                .all(|n| n.status == NodeStatus::Pending)
        );
        assert_eq!(interface.graph.edges.len(), 1);
        assert_eq!(
            compile(&root, None).expect("again").interface.digest().ok(),
            interface.digest().ok(),
            "the same pack compiles to the same interface"
        );
    }

    #[test]
    fn a_pack_with_no_result_has_an_empty_result() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = write_playbook_pack(
            dir.path(),
            "a = command(name = \"a\", run = \"true\")\n\
             workflow(type = \"playbook\", tasks = [a])\n",
        );
        let compiled = compile(&root, None).expect("compiles");
        assert_eq!(compiled.result, None);
        assert!(compiled.interface.result.is_empty());
        assert!(compiled.interface.result_files.is_empty());
    }

    #[test]
    fn values_reach_the_form_as_text_it_reads_back() {
        assert_eq!(param_text(&json!("main")), "main");
        assert_eq!(param_text(&json!(3)), "3");
        assert_eq!(param_text(&json!(true)), "true");
        assert_eq!(param_text(&json!({"a": [1]})), r#"{"a":[1]}"#);
        assert_eq!(param_text(&json!(["x", "y,z"])), r#"["x","y,z"]"#);
    }
}
