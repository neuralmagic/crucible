//! Local playbook dispatch: run a launch's engine as a supervised subprocess on the controller's
//! own machine instead of a work pod. Gated by `CONTROLLER_PLAYBOOK_EXECUTOR=local`, so a cluster
//! deployment cannot drift onto it.
//!
//! Everything downstream is the pod path's: the same `--param`/`--max-cost`/`--max-time` argv the
//! pod render is given, the same `session.jsonl` the pod publishes, and the same
//! [`crate::runs::completion::complete_run`] ingest that folds it into `runs`, `candidates` and the
//! ledger. What differs is where the process lives — a per-launch directory under the controller's
//! state dir, whose tree survives the run for anyone who wants to read it — and the `runs.dispatch`
//! stamp that lets the UI say a run never left this machine.

use crate::client::Db;
use crate::config::ControllerCfg;
use crate::model::{ParkReason, ParkedBy, Status};
use crate::runs::task_evidence::{engine_log, run_dir};
use crate::runs::workpod::RunRenderOpts;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// How long past its own wall-clock ceiling a run's subprocess is given before the supervisor
/// kills it. The engine enforces `--max-time` itself; this is the backstop for a process that
/// stopped honoring it.
const CEILING_SLACK: Duration = Duration::from_secs(120);

/// How many trailing output lines ride a failure that published no session.
const TAIL_LINES: usize = 40;

/// A launch running as a subprocess: the child, and the directory it runs in.
pub struct LocalRun {
    child: tokio::process::Child,
    dir: PathBuf,
    run_id: String,
    deadline: Duration,
}

/// Where the engine publishes its session log inside a local run's directory.
fn session_log(dir: &Path) -> PathBuf {
    dir.join("pack").join("state").join("session.jsonl")
}

/// Where the engine keeps its forge storage (the report handoff among it) inside a local run's
/// directory. The pod mounts an emptyDir at the same role.
fn forge_root(dir: &Path) -> PathBuf {
    dir.join("forge")
}

/// Mark every file in an unpacked pack executable, as the pod's pack ConfigMap mount does
/// (`default_mode: 0o755`). A draft stores its files with no mode, so its scripts would
/// otherwise reach a local run as 0644 and fail where the same pack runs in a pod.
fn grant_pack_exec(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            grant_pack_exec(&path)?;
        } else if meta.is_file() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .with_context(|| format!("marking {} executable", path.display()))?;
        }
    }
    Ok(())
}

/// The host facts every local run keeps: its process identity, through which the host harness
/// finds its own login, and the Podman API socket an OpenShell sandbox is booted against.
const HOST_ENV: [&str; 4] = ["PATH", "HOME", "USER", "OPENSHELL_PODMAN_SOCKET"];

/// The environment one local run is spawned with. Local mode has no registry and no grant, so the
/// only credentials handed over as environment are the ones an operator named in `allowlist`;
/// everything else the controller's own environment holds stays with the controller.
///
/// `item` comes from the launch, never from the inherited set: the engine reads
/// [`crate::issues::engine::ITEM_ENV`] as the tracker item a run may comment on, and a value the controller
/// happens to hold addresses somebody else's.
///
/// An allowlisted value reaches the agent, so the pack has to disclose it, exactly as a bound
/// agent-visible secret must on the pod path. `disclosed` of `None` is a revision that stored no
/// exposure, which is granted none of them.
fn run_env(
    inherited: impl Iterator<Item = (String, String)>,
    allowlist: &[String],
    item: Option<&str>,
    disclosed: Option<&crate::playbooks::exposure::Exposure>,
) -> Result<Vec<(String, String)>, UndisclosedGrant> {
    let mut env = Vec::new();
    for (name, value) in inherited {
        if name == crate::issues::engine::ITEM_ENV {
            continue;
        }
        let allowlisted = allowlist.contains(&name);
        if allowlisted && !disclosed.is_some_and(|e| e.covers_agent_credential(&name)) {
            return Err(UndisclosedGrant { name });
        }
        if HOST_ENV.contains(&name.as_str()) || name.starts_with("CRUCIBLE_") || allowlisted {
            env.push((name, value));
        }
    }
    env.extend(item.map(|i| (crate::issues::engine::ITEM_ENV.to_string(), i.to_string())));
    Ok(env)
}

/// An allowlisted value the pack's exposure does not disclose as an agent-context credential.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "{name} is in the local secret allowlist and the pack's stored exposure discloses no \
     agent-context credential under that name, so this run is not handed it"
)]
pub struct UndisclosedGrant {
    pub name: String,
}

/// The engine argv one local run is spawned with: the pack's own manifest plus the launch's
/// ceilings and values, exactly as [`RunRenderOpts`] renders them into a pod.
fn run_argv(manifest: &Path, opts: &RunRenderOpts) -> Vec<String> {
    let mut argv = vec![
        "plan".to_string(),
        "run".to_string(),
        "--manifest".to_string(),
        manifest.to_string_lossy().to_string(),
    ];
    if let RunRenderOpts::Playbook {
        params,
        max_cost,
        max_time,
        ..
    } = opts
    {
        argv.push("--max-cost".to_string());
        argv.push(format!("{max_cost}"));
        argv.push("--max-time".to_string());
        argv.push(max_time.as_str().to_string());
        for (name, value) in params {
            argv.push("--param".to_string());
            argv.push(format!("{name}={value}"));
        }
    }
    let agent = opts.agent();
    if let Some(harness) = agent
        .harness
        .and_then(|h| clap::ValueEnum::to_possible_value(&h))
    {
        argv.push("--harness".to_string());
        argv.push(harness.get_name().to_string());
    }
    if let Some(model) = &agent.model {
        argv.push("--model".to_string());
        argv.push(model.clone());
    }
    argv
}

/// Unpack the launch's stored pack into its own directory and spawn the engine on it. Returns as
/// soon as the child is running: a playbook run is minutes to hours, and the serial reconcile
/// worker may not park on it. The caller records the run row, then hands the child to
/// [`supervise`]. `None` is the concurrency cap declining the launch, the same bound a work-pod
/// dispatch is held to.
pub async fn start(
    db: &Db,
    cfg: &ControllerCfg,
    issue_key: &str,
    run_id: &str,
    opts: RunRenderOpts,
) -> Result<Option<LocalRun>> {
    if u32::try_from(crate::runs::store::count_running(db.pool()).await?).unwrap_or(u32::MAX)
        >= cfg.effective().max_concurrent_pods
    {
        return Ok(None);
    }
    // Absolute: the engine is spawned with the pack as its cwd, so a scratch root that is
    // configured relatively (the default `state`) would resolve against the wrong directory.
    let dir = std::path::absolute(run_dir(cfg.scratch_root(), run_id))
        .context("resolving the local run directory")?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("clearing the local run dir {}", dir.display()))?;
    }
    let slug = crate::model::sanitize_key(issue_key);
    let tar_gz = crate::runs::blob_store::get_pack_tarball(db.pool(), &slug)
        .await?
        .with_context(|| format!("no stored pack for playbook launch {issue_key}"))?;
    let pack = dir.join("pack");
    let unpack_to = pack.clone();
    tokio::task::spawn_blocking(move || {
        crate::playbooks::packs::unpack_pack_tgz(&tar_gz, &unpack_to)?;
        grant_pack_exec(&unpack_to)
    })
    .await
    .context("joining the local pack unpack")?
    .context("unpacking the launch's pack for a local run")?;
    crate::launches::schedules::ScheduleStore::new(db.clone())
        .stage_cursor_file(issue_key, &pack)
        .await?;

    let bin = crate::issues::engine::resolve_bin();
    let argv = run_argv(&pack.join("crucible.toml"), &opts);
    let deadline = match &opts {
        RunRenderOpts::Playbook { max_time, .. } => {
            Duration::from_secs(max_time.secs()) + CEILING_SLACK
        }
        RunRenderOpts::Loop { .. } => CEILING_SLACK,
    };
    let exposure = crate::launches::store::exposure_for_issue(db.pool(), issue_key).await?;
    let mut env = run_env(
        std::env::vars(),
        &cfg.local_secret_allowlist,
        opts.tracker_item(issue_key),
        exposure.as_ref(),
    )
    .with_context(|| format!("preparing the local environment for {issue_key}"))?;
    env.push((
        "FORGE_STORAGE_ROOT".to_string(),
        forge_root(&dir).to_string_lossy().into_owned(),
    ));
    let child = tokio::process::Command::new(&bin)
        .args(&argv)
        .current_dir(&pack)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning `{} plan run` for {run_id}", bin.display()))?;
    tracing::info!(%issue_key, %run_id, dir = %dir.display(), "playbook launch running locally");
    Ok(Some(LocalRun {
        child,
        dir,
        run_id: run_id.to_string(),
        deadline,
    }))
}

/// Drive one local run to its end: stream its output into the log, kill it if it outlives its own
/// ceiling, and fold whatever session it published into the ledger through the pod path's
/// completion. A run that published nothing parks the launch with the supervisor's account of how
/// it ended, so a failed local run is never a silently `running` row.
pub async fn supervise(db: Db, key: String, run: LocalRun) {
    let LocalRun {
        mut child,
        dir,
        run_id,
        deadline,
    } = run;
    let mut tail = Vec::new();
    let mut streams = tokio::task::JoinSet::new();
    let log = open_engine_log(&dir).await;
    // Both pipes are drained concurrently with the wait, so a chatty run can never fill a pipe
    // buffer and wedge the engine.
    if let Some(out) = child.stdout.take() {
        let run_id = run_id.clone();
        let log = log.clone();
        streams.spawn(async move { pump(BufReader::new(out), &run_id, "stdout", log).await });
    }
    if let Some(err) = child.stderr.take() {
        let run_id = run_id.clone();
        let log = log.clone();
        streams.spawn(async move { pump(BufReader::new(err), &run_id, "stderr", log).await });
    }

    let outcome = match tokio::time::timeout(deadline, child.wait()).await {
        Ok(Ok(status)) => format!("engine exited {status}"),
        Ok(Err(e)) => format!("waiting on the engine failed: {e}"),
        Err(_) => {
            let _ = child.kill().await;
            format!(
                "the engine outlived its ceiling by {}s and was killed",
                CEILING_SLACK.as_secs()
            )
        }
    };
    while let Some(joined) = streams.join_next().await {
        if let Ok(lines) = joined {
            tail.extend(lines);
        }
    }

    let session = std::fs::read_to_string(session_log(&dir)).unwrap_or_default();
    if session.trim().is_empty() {
        let tail = tail
            .iter()
            .rev()
            .take(TAIL_LINES)
            .rev()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        let detail = format!("{outcome}. Last output:\n{tail}");
        tracing::warn!(%key, %run_id, "{detail}");
        settle_no_session(&db, &key, &run_id, detail).await;
        return;
    }
    if let Err(e) =
        crate::runs::completion::complete_run(&db, &key, None, &run_id, None, &session, None).await
    {
        tracing::warn!(%key, %run_id, error = %format!("{e:#}"), "ingesting a local run failed");
    }
}

/// Stamp a local run that published no session terminal, and park its launch. The run row settles
/// whatever the park does — the run is over either way, and a row left reading `running` is a
/// phantom. The park itself is conditional on the launch still being `running`, so an issue a
/// human already moved stays where they put it.
async fn settle_no_session(db: &Db, key: &str, run_id: &str, detail: String) {
    if let Err(e) = crate::runs::store::set_run_status(db.pool(), run_id, "no-session").await {
        tracing::warn!(%key, %run_id, error = %format!("{e:#}"), "stamping a failed local run failed");
    }
    if let Err(e) = crate::issues::transitions::park(
        db.pool(),
        db.events(),
        key,
        Status::Running,
        &ParkReason::LocalRunFailed {
            run_id: run_id.to_string(),
            detail,
        },
        ParkedBy::Machine,
    )
    .await
    {
        tracing::warn!(%key, %run_id, error = %format!("{e:#}"), "parking a failed local run failed");
    }
}

/// Settle the local runs a restart orphaned. The supervisor lives in the daemon's process, so a
/// controller that goes down mid-run loses the child's exit and the launch would sit at `running`
/// forever. Whatever session the engine had written by then is ingested exactly as the supervisor
/// would have; a run that published nothing parks. Called once at startup, before the queue's
/// re-enqueue drives anything.
pub async fn adopt_orphans(db: &Db, cfg: &ControllerCfg) -> Result<()> {
    let orphans = crate::runs::store::running_local_runs(db.pool()).await?;
    for (key, run_id) in orphans {
        let dir = run_dir(cfg.scratch_root(), &run_id);
        let session = std::fs::read_to_string(session_log(&dir)).unwrap_or_default();
        if session.trim().is_empty() {
            tracing::warn!(%key, %run_id, "local run orphaned by a restart with no session");
            settle_no_session(
                db,
                &key,
                &run_id,
                "the controller restarted while the run was supervised locally".to_string(),
            )
            .await;
            continue;
        }
        tracing::info!(%key, %run_id, "adopting a local run orphaned by a restart");
        if let Err(e) =
            crate::runs::completion::complete_run(db, &key, None, &run_id, None, &session, None)
                .await
        {
            tracing::warn!(%key, %run_id, error = %format!("{e:#}"), "adopting an orphaned local run failed");
        }
    }
    Ok(())
}

/// The file both pipes are mirrored into, served by `GET /api/runs/{run_id}/log`. A log that
/// cannot be opened is no reason to refuse the run: the output still reaches the tracing subscriber
/// and the failure tail.
async fn open_engine_log(dir: &Path) -> Option<Arc<Mutex<tokio::fs::File>>> {
    let path = engine_log(dir);
    match tokio::fs::File::create(&path).await {
        Ok(file) => Some(Arc::new(Mutex::new(file))),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "keeping no engine log for a local run");
            None
        }
    }
}

/// Read one of the child's pipes to EOF, logging every line, mirroring it into the run's engine
/// log, and keeping it for a failure tail.
async fn pump<R>(
    reader: BufReader<R>,
    run_id: &str,
    stream: &str,
    log: Option<Arc<Mutex<tokio::fs::File>>>,
) -> Vec<String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = reader.lines();
    let mut kept = Vec::new();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!(%run_id, %stream, "{line}");
        if let Some(log) = &log
            && let Err(e) = log
                .lock()
                .await
                .write_all(format!("{line}\n").as_bytes())
                .await
        {
            tracing::debug!(%run_id, error = %e, "writing the engine log failed");
        }
        kept.push(line);
        if kept.len() > TAIL_LINES * 4 {
            kept.drain(..TAIL_LINES);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_unpacked_pack_file_is_executable_like_the_pod_mount() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("inbox")).expect("mkdir");
        std::fs::write(dir.path().join("role.sh"), "#!/bin/sh\n").expect("write");
        std::fs::write(dir.path().join("inbox/a.md"), "a").expect("write");
        std::os::unix::fs::symlink("role.sh", dir.path().join("alias.sh")).expect("symlink");
        grant_pack_exec(dir.path()).expect("grant");
        for f in ["role.sh", "inbox/a.md"] {
            let mode = std::fs::metadata(dir.path().join(f))
                .expect("stat")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755, "{f}");
        }
    }

    /// Local mode has no registry, so the subprocess starts from nothing: its host facts,
    /// the run's own `CRUCIBLE_*` set, and whatever an operator named. Everything else the
    /// controller holds — its database URL, its Vault login, its tokens — stays with the controller.
    #[test]
    fn a_local_run_inherits_only_host_facts_the_crucible_set_and_the_allowlist() {
        let inherited = [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/wren"),
            ("USER", "wren"),
            ("OPENSHELL_PODMAN_SOCKET", "/run/podman.sock"),
            ("CRUCIBLE_BIN", "/opt/crucible"),
            ("DATABASE_URL", "postgres://secret"),
            ("VAULT_SECRET_ID", "hunter2"),
            ("ANTHROPIC_API_KEY", "sk-live"),
            ("GH_TOKEN", "ghp_live"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()));
        let env = run_env(
            inherited,
            &["GH_TOKEN".to_string()],
            None,
            Some(&disclosing("GH_TOKEN")),
        )
        .expect("a disclosed allowlisted value is handed over");
        let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            [
                "PATH",
                "HOME",
                "USER",
                "OPENSHELL_PODMAN_SOCKET",
                "CRUCIBLE_BIN",
                "GH_TOKEN"
            ]
        );
    }

    /// The allowlist names what the operator is willing to hand over; the pack still has to say
    /// the agent will read it, the same as a bound agent-visible secret on the pod path.
    #[test]
    fn an_allowlisted_value_the_pack_does_not_disclose_refuses_the_run() {
        let inherited = || {
            [("PATH", "/usr/bin"), ("GH_TOKEN", "ghp_live")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
        };
        let allowlist = ["GH_TOKEN".to_string()];
        assert_eq!(
            run_env(inherited(), &allowlist, None, Some(&disclosing("OTHER"))),
            Err(UndisclosedGrant {
                name: "GH_TOKEN".to_string()
            })
        );
        assert_eq!(
            run_env(inherited(), &allowlist, None, None),
            Err(UndisclosedGrant {
                name: "GH_TOKEN".to_string()
            }),
            "a revision with no stored exposure discloses nothing"
        );
        let absent = [("PATH", "/usr/bin")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()));
        assert_eq!(
            run_env(absent, &allowlist, None, None).expect("nothing to hand over"),
            vec![("PATH".to_string(), "/usr/bin".to_string())],
            "an allowlisted name the controller does not hold grants nothing"
        );
    }

    #[test]
    fn an_empty_allowlist_leaves_a_local_run_with_no_credentials_at_all() {
        let inherited = [("PATH", "/usr/bin"), ("GH_TOKEN", "ghp_live")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()));
        let env = run_env(inherited, &[], None, None).expect("nothing allowlisted");
        assert_eq!(env, vec![("PATH".to_string(), "/usr/bin".to_string())]);
    }

    fn disclosing(name: &str) -> crate::playbooks::exposure::Exposure {
        crate::playbooks::exposure::Exposure {
            version: 1,
            outputs: Vec::new(),
            capabilities: vec![crate::playbooks::exposure::Capability::Known(
                crate::playbooks::exposure::KnownCapability::Credential {
                    name: name.to_string(),
                    context: crate::playbooks::exposure::CredentialContext::Agent,
                    system: None,
                    scope: None,
                },
            )],
        }
    }

    /// The engine's `tracker-comment` default target: the launch's own item, and never one the
    /// controller's environment happened to carry.
    #[test]
    fn the_launchs_item_replaces_whatever_the_controller_environment_held() {
        let inherited = || {
            [
                ("PATH", "/usr/bin"),
                (crate::issues::engine::ITEM_ENV, "owner/repo#1"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
        };
        assert_eq!(
            run_env(inherited(), &[], Some("owner/repo#9"), None).expect("no allowlist"),
            vec![
                ("PATH".to_string(), "/usr/bin".to_string()),
                (
                    crate::issues::engine::ITEM_ENV.to_string(),
                    "owner/repo#9".to_string()
                ),
            ]
        );
        assert_eq!(
            run_env(inherited(), &[], None, None).expect("no allowlist"),
            vec![("PATH".to_string(), "/usr/bin".to_string())],
            "a launch with no upstream item exports none"
        );
    }

    /// A playbook launch is parameterized by its own params, not by an upstream tracker item.
    #[test]
    fn a_playbook_launch_names_no_tracker_item() {
        let playbook = RunRenderOpts::Playbook {
            params: Vec::new(),
            max_cost: 1.0,
            max_time: MaxTime::parse("30m").expect("duration"),
            agent: crate::playbooks::providers::AgentSelection::default(),
        };
        assert_eq!(playbook.tracker_item("playbook:fences:0199"), None);
        let looped = RunRenderOpts::Loop {
            iterations: 1,
            max_cost: 1.0,
            pr_repo: None,
            agent: crate::playbooks::providers::AgentSelection::default(),
        };
        assert_eq!(looped.tracker_item("owner/repo#3"), Some("owner/repo#3"));
    }

    use super::*;
    use crate::model::MaxTime;

    #[test]
    fn the_argv_is_the_pod_paths_flags_against_a_local_manifest() {
        let opts = RunRenderOpts::Playbook {
            params: vec![
                ("repo".to_string(), "owner/pack".to_string()),
                ("depth".to_string(), "-deep".to_string()),
            ],
            max_cost: 4.5,
            max_time: MaxTime::parse("30m").expect("duration"),
            agent: crate::playbooks::providers::AgentSelection::default(),
        };
        assert_eq!(
            run_argv(Path::new("/runs/r1/pack/crucible.toml"), &opts),
            vec![
                "plan",
                "run",
                "--manifest",
                "/runs/r1/pack/crucible.toml",
                "--max-cost",
                "4.5",
                "--max-time",
                "30m",
                "--param",
                "repo=owner/pack",
                "--param",
                "depth=-deep",
            ]
        );
    }

    /// The pair the registry resolved reaches `plan run` as the flags the engine parses, spelled
    /// the way its `--harness` value enum reads them.
    #[test]
    fn a_resolved_provider_reaches_the_local_argv() {
        let opts = RunRenderOpts::Playbook {
            params: vec![],
            max_cost: 4.5,
            max_time: MaxTime::parse("30m").expect("duration"),
            agent: crate::playbooks::providers::AgentSelection {
                harness: Some(crucible::manifest::Harness::Codex),
                model: Some("gpt-5.6-luna".to_string()),
            },
        };
        let argv = run_argv(Path::new("/runs/r1/pack/crucible.toml"), &opts);
        assert_eq!(
            &argv[argv.len() - 4..],
            ["--harness", "codex", "--model", "gpt-5.6-luna"]
        );
    }

    /// The mirror `GET /api/runs/{run_id}/log` serves: both pipes land in `engine.log` under the
    /// run's dir, and a directory that cannot hold one degrades to logging only.
    #[tokio::test]
    async fn pumped_output_is_mirrored_into_the_engine_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = open_engine_log(dir.path()).await;
        assert!(log.is_some(), "the log opens in a real directory");
        let out = pump(
            BufReader::new(&b"line one\nline two\n"[..]),
            "r1",
            "stdout",
            log.clone(),
        )
        .await;
        let err = pump(
            BufReader::new(&b"complaint\n"[..]),
            "r1",
            "stderr",
            log.clone(),
        )
        .await;
        assert_eq!(out, vec!["line one", "line two"]);
        assert_eq!(err, vec!["complaint"]);
        if let Some(log) = log {
            log.lock().await.flush().await.expect("flush");
        }
        let written = std::fs::read_to_string(engine_log(dir.path())).expect("the mirror exists");
        assert_eq!(written, "line one\nline two\ncomplaint\n");

        let gone = open_engine_log(&dir.path().join("missing")).await;
        assert!(gone.is_none(), "an unwritable dir keeps no log");
        let still = pump(BufReader::new(&b"tail\n"[..]), "r1", "stdout", gone).await;
        assert_eq!(still, vec!["tail"], "output still reaches the failure tail");
    }

    /// A run directory is per launch and safe to build from a run id, which carries the launch
    /// key's colons.
    #[test]
    fn the_run_dir_is_per_launch_and_inside_the_scratch_root() {
        let root = Path::new("/var/state");
        let dir = run_dir(root, "playbook:fences:0199-1");
        assert!(dir.starts_with(root.join("local-runs")), "{dir:?}");
        assert!(!dir.to_string_lossy().contains(':'), "{dir:?}");
        assert_ne!(dir, run_dir(root, "playbook:fences:0199-2"));
    }
}
