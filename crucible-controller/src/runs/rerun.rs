//! Rerunning a finished playbook run from some of its tasks (RFC-0002:C-PLAYBOOK-RERUN): what a
//! rerun would reuse, decided by the engine's own rule against the pack the rerun runs, and the
//! parent's evidence laid out for the engine to read.

use crate::client::Db;
use crate::launches::model::PlaybookLaunch;
use anyhow::{Context, Result};
use crucible::plan::ir::TaskName;
use crucible::plan::rerun::RerunError;
use std::collections::BTreeMap;
use std::path::Path;

/// Why a run cannot be rerun as asked.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RerunRefusal {
    #[error("run not found: {0}")]
    NoRun(String),
    #[error("run {0} is not a playbook launch")]
    NotPlaybook(String),
    #[error("run {0} has not finished, so it cannot be rerun")]
    NotFinished(String),
    #[error("the pack this rerun would run is gone: {0}")]
    NoPack(String),
    #[error("the pack this rerun would run does not compile: {0}")]
    Compile(String),
    #[error(transparent)]
    Rerun(RerunError),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// One task a rerun reuses, as the parent settled it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, utoipa::ToSchema)]
pub struct ReusedTaskDto {
    pub task: String,
    pub cost_usd: f64,
}

/// What rerunning a run from `from` would do, against the pack that rerun would run.
#[derive(Debug, Clone, PartialEq, serde::Serialize, utoipa::ToSchema)]
pub struct RerunPreviewDto {
    pub run_id: String,
    pub playbook: String,
    /// The draft version the rerun runs: the newest save. Null for a registered playbook.
    pub draft_version: Option<i64>,
    /// Main-graph tasks reused, in the order the parent settled them.
    pub reused: Vec<ReusedTaskDto>,
    /// Main-graph tasks dispatched again.
    pub rerun: Vec<String>,
    /// What the reused tasks cost the runs that settled them.
    pub reused_usd: f64,
    /// Tasks the rerun would reuse that changed since the parent; non-empty means it is refused
    /// until they are added to `from` or the edit is undone.
    pub changed: Vec<String>,
}

/// The parent run and the launch it came from.
async fn parent(db: &Db, run_id: &str) -> Result<(String, PlaybookLaunch), RerunRefusal> {
    let run = crate::runs::store::get_run(db.pool(), run_id)
        .await?
        .ok_or_else(|| RerunRefusal::NoRun(run_id.to_string()))?;
    let key = run
        .issue
        .ok_or_else(|| RerunRefusal::NotPlaybook(run_id.to_string()))?;
    let launch = crate::launches::store::get_playbook_launch(db.pool(), &key)
        .await?
        .ok_or_else(|| RerunRefusal::NotPlaybook(run_id.to_string()))?;
    Ok((key, launch))
}

/// Preview a rerun of `run_id` from `from`.
pub(crate) async fn preview(
    db: &Db,
    run_id: &str,
    from: &[String],
) -> Result<RerunPreviewDto, RerunRefusal> {
    let (_, launch) = parent(db, run_id).await?;
    let session = crate::runs::blob_store::get_run_session(db.pool(), run_id)
        .await?
        .ok_or_else(|| RerunRefusal::NotFinished(run_id.to_string()))?;
    let (draft_version, tree) = match launch.draft_version {
        Some(_) => crate::playbooks::drafts::version_tree(db.pool(), &launch.playbook, None)
            .await?
            .map(|(version, tree)| (Some(version), tree)),
        None => crate::playbooks::registry::pack(db.pool(), &launch.playbook)
            .await?
            .map(|tree| (None, tree)),
    }
    .ok_or_else(|| RerunRefusal::NoPack(launch.playbook.clone()))?;
    let params: BTreeMap<String, String> = launch.params.iter().cloned().collect();
    let from: Vec<TaskName> = from.iter().cloned().map(TaskName).collect();
    let run = run_id.to_string();
    let playbook = launch.playbook.clone();
    tokio::task::spawn_blocking(move || {
        let pack = crate::playbooks::packs::materialize_tree(&tree)?;
        let plan = crate::playbooks::registry::compiled_plan(pack.path(), &params)
            .map_err(RerunRefusal::Compile)?;
        decide(&plan, &run, &playbook, draft_version, &session, &from)
    })
    .await
    .context("joining the rerun preview")?
}

fn decide(
    plan: &crucible::plan::ir::ValidPlan,
    run: &str,
    playbook: &str,
    draft_version: Option<i64>,
    session: &str,
    from: &[TaskName],
) -> Result<RerunPreviewDto, RerunRefusal> {
    let parent =
        crucible::plan::rerun::Parent::from_log(run, session).map_err(RerunRefusal::Rerun)?;
    let mut dto = RerunPreviewDto {
        run_id: run.to_string(),
        playbook: playbook.to_string(),
        draft_version,
        reused: Vec::new(),
        rerun: Vec::new(),
        reused_usd: 0.0,
        changed: Vec::new(),
    };
    match crucible::plan::rerun::plan(plan, &parent, from) {
        Ok(rerun) => {
            dto.reused = rerun
                .prior
                .order
                .iter()
                .filter(|name| rerun.reused.contains(*name))
                .filter_map(|name| {
                    Some(ReusedTaskDto {
                        task: name.0.clone(),
                        cost_usd: rerun.prior.results.get(name)?.cost_usd,
                    })
                })
                .collect();
            dto.rerun = rerun.rerun.iter().map(|t| t.0.clone()).collect();
            dto.reused_usd = rerun.prior.reused_usd();
        }
        Err(RerunError::DefinitionsChanged(changed)) => {
            dto.changed = changed.into_iter().map(|t| t.0).collect();
        }
        Err(e) => return Err(RerunRefusal::Rerun(e)),
    }
    Ok(dto)
}

/// Refuse a rerun launch of `playbook` from `parent_run` that the engine would refuse anyway, so
/// the launcher hears why before anything is adopted.
pub(crate) async fn admit(
    db: &Db,
    playbook: &str,
    draft: bool,
    parent_run: &str,
    from: &[String],
) -> Result<(), RerunRefusal> {
    let (_, launch) = parent(db, parent_run).await?;
    if launch.playbook != playbook || launch.draft_version.is_some() != draft {
        return Err(RerunRefusal::NotPlaybook(format!(
            "{parent_run} (it is a run of {}, not {playbook})",
            launch.playbook
        )));
    }
    let preview = preview(db, parent_run, from).await?;
    if !preview.changed.is_empty() {
        return Err(RerunRefusal::Rerun(RerunError::DefinitionsChanged(
            preview.changed.into_iter().map(TaskName).collect(),
        )));
    }
    Ok(())
}

/// Lay `parent_run`'s evidence out under `dest` as the engine reads a parent state directory:
/// `session.jsonl`, `files/` and `git-memory/`. A local parent's evidence is still on this
/// machine's scratch disk; a pod parent's was adopted into the store at completion.
pub(crate) async fn materialize_parent(
    db: &Db,
    scratch_root: &Path,
    parent_run: &str,
    dest: &Path,
) -> Result<()> {
    let session = crate::runs::blob_store::get_run_session(db.pool(), parent_run)
        .await?
        .with_context(|| format!("run {parent_run} has no stored session log"))?;
    let local = crate::runs::task_evidence::run_dir(scratch_root, parent_run)
        .join("pack")
        .join("state");
    let bundles = if local.join(crucible::plan::rerun::GIT_MEMORY_DIR).is_dir() {
        None
    } else {
        let files = crate::runs::blob_store::get_run_bundle(
            db.pool(),
            parent_run,
            crucible_contract::ArtifactKind::RunFiles,
        )
        .await?;
        let commits = crate::runs::blob_store::get_run_bundle(
            db.pool(),
            parent_run,
            crucible_contract::ArtifactKind::RunCommits,
        )
        .await?
        .with_context(|| format!("run {parent_run} kept no git memory"))?;
        Some((files, commits))
    };
    let dest = dest.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<()> {
        std::fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;
        std::fs::write(dest.join("session.jsonl"), session)
            .context("writing the parent's session log")?;
        match bundles {
            None => {
                for dir in ["files", crucible::plan::rerun::GIT_MEMORY_DIR] {
                    if local.join(dir).is_dir() {
                        copy_tree(&local.join(dir), &dest.join(dir))?;
                    }
                }
            }
            Some((files, commits)) => {
                if let Some(files) = files {
                    unpack(&files, &dest.join("files"))?;
                }
                unpack(&commits, &dest.join(crucible::plan::rerun::GIT_MEMORY_DIR))?;
            }
        }
        Ok(())
    })
    .await
    .context("joining the parent evidence write")?
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry.with_context(|| format!("reading {}", from.display()))?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Unpack a stored bundle's regular files under `to`. Entry paths come from a pod, so one that
/// is absolute or climbs out of `to` is refused rather than followed.
fn unpack(tar_gz: &[u8], to: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(flate2::read::MultiGzDecoder::new(tar_gz));
    for entry in archive.entries().context("reading a run bundle")? {
        let mut entry = entry.context("reading a run bundle entry")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .context("decoding a run bundle entry path")?
            .into_owned();
        if !path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
        {
            return Err(UnsafeEntry(path.display().to_string()).into());
        }
        let target = to.join(&path);
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut file = std::fs::File::create(&target)
            .with_context(|| format!("creating {}", target.display()))?;
        std::io::copy(&mut entry, &mut file)
            .with_context(|| format!("writing {}", target.display()))?;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("a run bundle entry {0:?} leaves the directory it unpacks into")]
struct UnsafeEntry(String);

#[cfg(test)]
mod tests {
    use crate::client::Db;
    use crate::runs::model::NewRun;
    use crate::runs::rerun::{RerunRefusal, admit, materialize_parent, preview, unpack};
    use crate::testing::fixtures::write_playbook_pack;
    use sqlx::PgPool;
    use std::collections::BTreeMap;

    const WORKFLOW: &str = concat!(
        "params = {}\n",
        "scan = command(name = \"scan\", run = \"echo scan\")\n",
        "fix = command(name = \"fix\", run = \"echo fix\", depends_on = [scan])\n",
        "check = command(name = \"check\", run = \"echo check\", depends_on = [fix])\n",
        "workflow(type = \"playbook\", tasks = [scan, fix, check])\n",
    );

    async fn pin(pool: &PgPool, source: &str) {
        let dir = tempfile::tempdir().unwrap();
        let root = write_playbook_pack(dir.path(), source);
        let tree = crucible_contract::pack_tree::walk_dir(&root).unwrap().tree;
        crate::testing::pin_playbook(
            pool,
            tree,
            &serde_json::json!({"version": 1, "outputs": []}),
        )
        .await;
    }

    /// The session a finished run of `source` leaves when `check` fails.
    fn session(source: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let root = write_playbook_pack(dir.path(), source);
        let plan = crate::playbooks::registry::compiled_plan(&root, &BTreeMap::new()).unwrap();
        let defs = crucible::plan::rerun::definition_digests(&plan);
        let tasks: Vec<serde_json::Value> = plan
            .tasks_topo()
            .map(|t| serde_json::json!({"name": t.name.0, "kind": "command", "definition": defs[&t.name]}))
            .collect();
        let row = |task: &str, status: &str, cost: f64| {
            serde_json::json!({"v": 1, "kind": "task_result", "task": task, "status": status, "attempts": 1, "cost_usd": cost})
                .to_string()
        };
        [
            serde_json::json!({"v": 1, "kind": "plan_admitted", "plan_version": 1, "budget_usd": 1.0, "tasks": tasks}).to_string(),
            row("scan", "pass", 0.25),
            row("fix", "pass", 0.5),
            row("check", "fail", 0.125),
            r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":"short-circuited at check"}"#.to_string(),
        ]
        .join("\n")
    }

    async fn finished_run(pool: &PgPool, run_id: &str, key: &str, log: &str) {
        crate::runs::store::insert_run(
            pool,
            &NewRun {
                run_id: run_id.to_string(),
                scope: None,
                issue: Some(key.to_string()),
                identity_digest: None,
                status: "done".to_string(),
                pod: None,
                session_uri: None,
                best_score: None,
                cost_usd: None,
            },
        )
        .await
        .unwrap();
        crate::runs::blob_store::put_run_session(pool, run_id, log.as_bytes())
            .await
            .unwrap();
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_preview_reuses_what_passed_and_reruns_from_the_named_task(pool: PgPool) {
        pin(&pool, WORKFLOW).await;
        crate::testing::launch_playbook(&pool, "playbook:survey:1").await;
        finished_run(&pool, "run-parent", "playbook:survey:1", &session(WORKFLOW)).await;
        let db = Db::new(pool.clone());

        let failed = preview(&db, "run-parent", &[]).await.unwrap();
        assert_eq!(
            failed
                .reused
                .iter()
                .map(|r| r.task.as_str())
                .collect::<Vec<_>>(),
            ["scan", "fix"]
        );
        assert_eq!(failed.rerun, ["check"]);
        assert!((failed.reused_usd - 0.75).abs() < 1e-9);
        assert_eq!(failed.draft_version, None);
        assert!(failed.changed.is_empty());

        let from_fix = preview(&db, "run-parent", &["fix".to_string()])
            .await
            .unwrap();
        assert_eq!(from_fix.rerun, ["check", "fix"]);

        // Editing the task being rerun is the point; editing one the rerun would reuse is named.
        pin(&pool, &WORKFLOW.replace("echo check", "echo checked")).await;
        assert!(
            preview(&db, "run-parent", &[])
                .await
                .unwrap()
                .changed
                .is_empty()
        );
        pin(&pool, &WORKFLOW.replace("echo fix", "echo fixed")).await;
        assert_eq!(
            preview(&db, "run-parent", &[]).await.unwrap().changed,
            ["fix"]
        );
        assert!(
            preview(&db, "run-parent", &["fix".to_string()])
                .await
                .unwrap()
                .changed
                .is_empty()
        );
        assert!(matches!(
            admit(&db, "survey", false, "run-parent", &[]).await,
            Err(RerunRefusal::Rerun(_))
        ));
        admit(&db, "survey", false, "run-parent", &["fix".to_string()])
            .await
            .unwrap();
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_rerun_is_refused_for_a_run_it_cannot_continue(pool: PgPool) {
        pin(&pool, WORKFLOW).await;
        crate::testing::launch_playbook(&pool, "playbook:survey:1").await;
        let db = Db::new(pool.clone());
        assert!(matches!(
            preview(&db, "nobody", &[]).await,
            Err(RerunRefusal::NoRun(_))
        ));

        let open = session(WORKFLOW);
        let open = open.rsplit_once('\n').unwrap().0;
        finished_run(&pool, "run-open", "playbook:survey:1", open).await;
        assert!(matches!(
            preview(&db, "run-open", &[]).await,
            Err(RerunRefusal::Rerun(
                crucible::plan::rerun::RerunError::NotFinished
            ))
        ));

        finished_run(&pool, "run-done", "playbook:survey:1", &session(WORKFLOW)).await;
        assert!(matches!(
            preview(&db, "run-done", &["nope".to_string()]).await,
            Err(RerunRefusal::Rerun(
                crucible::plan::rerun::RerunError::UnknownTask(_)
            ))
        ));
        assert!(matches!(
            admit(&db, "other", false, "run-done", &[]).await,
            Err(RerunRefusal::NotPlaybook(_))
        ));
        assert!(
            matches!(
                admit(&db, "survey", true, "run-done", &[]).await,
                Err(RerunRefusal::NotPlaybook(_)),
            ),
            "a draft rerun may not reuse a registered launch's run"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn the_preview_route_answers_with_the_rerun_or_why_not(pool: PgPool) {
        pin(&pool, WORKFLOW).await;
        crate::testing::launch_playbook(&pool, "playbook:survey:1").await;
        finished_run(&pool, "run-parent", "playbook:survey:1", &session(WORKFLOW)).await;
        let app = crate::api::router(crate::api::state::ApiState::test(
            Db::new(pool.clone()),
            std::sync::Arc::new(NoSink),
        ));
        let get = |uri: &'static str| {
            let app = app.clone();
            async move {
                let response = tower::ServiceExt::oneshot(
                    app,
                    axum::http::Request::get(uri)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
                let status = response.status();
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                (
                    status,
                    serde_json::from_slice::<serde_json::Value>(&body).unwrap_or_default(),
                )
            }
        };
        let (status, body) = get("/api/runs/run-parent/rerun?from=fix").await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        assert_eq!(body["reused"][0]["task"], "scan");
        assert_eq!(body["rerun"], serde_json::json!(["check", "fix"]));
        let (status, _) = get("/api/runs/nobody/rerun").await;
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
        let (status, body) = get("/api/runs/run-parent/rerun?from=nope").await;
        assert_eq!(
            status,
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "{body}"
        );
        let (status, _) = get("/api/runs/%2E%2E/rerun").await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    }

    struct NoSink;

    impl crate::daemon::queue::OverrideSink for NoSink {
        fn submit(&self, _ov: crate::daemon::queue::Override) {}
    }

    fn tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, body) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *body).unwrap();
        }
        let tar = builder.into_inner().unwrap();
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut enc, &tar).unwrap();
        enc.finish().unwrap()
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_pod_parent_is_laid_out_from_its_stored_bundles(pool: PgPool) {
        pin(&pool, WORKFLOW).await;
        crate::testing::launch_playbook(&pool, "playbook:survey:1").await;
        let log = session(WORKFLOW);
        finished_run(&pool, "run-pod", "playbook:survey:1", &log).await;
        let start = serde_json::json!({
            "run": "run-pod", "started_at": "2026-10-21T12:00:00Z", "base": "abc",
            "base_tree": "def", "plan": "sha256:p",
        })
        .to_string();
        crate::runs::blob_store::put_run_bundle(
            &pool,
            "run-pod",
            crucible_contract::ArtifactKind::RunCommits,
            tar_gz(&[
                ("tasks.json", br#"["fix"]"#),
                ("000000.patch", b"diff"),
                ("playbook-run.json", start.as_bytes()),
            ]),
        )
        .await
        .unwrap();
        crate::runs::blob_store::put_run_bundle(
            &pool,
            "run-pod",
            crucible_contract::ArtifactKind::RunFiles,
            tar_gz(&[("scan/targets.json", b"[]")]),
        )
        .await
        .unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let dest = scratch.path().join("parent");
        materialize_parent(&Db::new(pool.clone()), scratch.path(), "run-pod", &dest)
            .await
            .unwrap();
        let parent = crucible::plan::rerun::ParentDir::read(&dest).unwrap();
        assert_eq!(parent.start.run, "run-pod");
        assert_eq!(
            std::fs::read_to_string(dest.join("files/scan/targets.json")).unwrap(),
            "[]"
        );
        assert_eq!(
            crucible::plan::rerun::read_git_memory(&dest).unwrap()[0].task,
            "fix"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_local_parent_is_copied_off_the_scratch_disk(pool: PgPool) {
        pin(&pool, WORKFLOW).await;
        crate::testing::launch_playbook(&pool, "playbook:survey:1").await;
        finished_run(&pool, "run-local", "playbook:survey:1", &session(WORKFLOW)).await;
        let scratch = tempfile::tempdir().unwrap();
        let state = crate::runs::task_evidence::run_dir(scratch.path(), "run-local")
            .join("pack")
            .join("state");
        std::fs::create_dir_all(state.join("git-memory")).unwrap();
        std::fs::write(state.join("git-memory/tasks.json"), "[]").unwrap();
        std::fs::create_dir_all(state.join("files/fix")).unwrap();
        std::fs::write(state.join("files/fix/out.txt"), "kept").unwrap();
        let dest = scratch.path().join("parent");
        materialize_parent(&Db::new(pool.clone()), scratch.path(), "run-local", &dest)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("files/fix/out.txt")).unwrap(),
            "kept"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("git-memory/tasks.json")).unwrap(),
            "[]"
        );
        assert!(dest.join("session.jsonl").is_file());
    }

    #[test]
    fn a_bundle_entry_that_climbs_out_is_refused() {
        let mut builder = tar::Builder::new(Vec::new());
        let body = b"pwned";
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        // `append_data` refuses `..`, so write the name the way a hostile pod would.
        header.as_gnu_mut().unwrap().name[..9].copy_from_slice(b"../escape");
        header.set_cksum();
        builder.append(&header, &body[..]).unwrap();
        let tar = builder.into_inner().unwrap();
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut enc, &tar).unwrap();
        let bundle = enc.finish().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let err = unpack(&bundle, &dir.path().join("in")).unwrap_err();
        assert!(
            format!("{err:#}").contains("leaves the directory"),
            "{err:#}"
        );
        assert!(!dir.path().join("escape").exists());
    }
}
