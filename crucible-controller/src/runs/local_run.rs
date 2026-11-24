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
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// How long past its own wall-clock ceiling a run's subprocess is given before the supervisor
/// kills it. The engine enforces `--max-time` itself; this is the backstop for a process that
/// stopped honoring it.
const CEILING_SLACK: Duration = Duration::from_secs(120);

/// How many trailing output lines ride a failure that published no session.
const TAIL_LINES: usize = 40;

/// How long the supervisor keeps draining the engine's pipes after the engine exits.
const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(10);

/// How long an engine past its ceiling has to stop its own children before its group is killed.
const INTERRUPT_GRACE: Duration = Duration::from_secs(5);

/// A launch running as a subprocess: the child, its process group, and the directory it runs in.
pub struct LocalRun {
    child: tokio::process::Child,
    group: ProcessGroup,
    dir: PathBuf,
    run_id: String,
    deadline: Duration,
}

/// The process group a local run's engine leads. Dropping it SIGKILLs the group.
struct ProcessGroup(nix::unistd::Pid);

impl ProcessGroup {
    fn led_by(child: &tokio::process::Child) -> Result<Self> {
        let pid = child
            .id()
            .context("the spawned engine was reaped before its process group was recorded")?;
        let pid = i32::try_from(pid).with_context(|| format!("engine pid {pid} is not a pid_t"))?;
        Ok(Self(nix::unistd::Pid::from_raw(pid)))
    }

    /// SIGINT the group. The engine answers it by stopping the children it put in groups of
    /// their own.
    fn interrupt(&self) {
        self.signal(nix::sys::signal::Signal::SIGINT);
    }

    fn signal(&self, signal: nix::sys::signal::Signal) {
        match nix::sys::signal::killpg(self.0, signal) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
            Err(e) => {
                tracing::warn!(pgid = %self.0, %signal, error = %e, "signalling a local run's process group failed")
            }
        }
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.signal(nix::sys::signal::Signal::SIGKILL);
    }
}

/// The last [`TAIL_LINES`] lines of a run's output, across both pipes.
#[derive(Clone, Default)]
struct Tail(std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>);

impl Tail {
    fn push(&self, line: String) {
        let mut lines = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lines.len() == TAIL_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    fn text(&self) -> String {
        let lines = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Where the engine publishes its session log inside a local run's directory.
fn session_log(dir: &Path) -> PathBuf {
    dir.join("pack").join("state").join("session.jsonl")
}

/// The engine's forge storage root inside a local run's directory.
fn forge_root(dir: &Path) -> PathBuf {
    dir.join("forge")
}

/// Host variables every local run inherits.
const HOST_ENV: [&str; 4] = ["PATH", "HOME", "USER", "OPENSHELL_PODMAN_SOCKET"];

/// The environment one local run is spawned with. Local mode has no registry and no grant, so the
/// only credentials handed over as environment are the ones an operator named in `allowlist`;
/// everything else the controller's own environment holds stays with the controller.
///
/// `item` comes from the launch, never from the inherited set: the engine reads
/// [`crate::runs::engine::ITEM_ENV`] as the tracker item a run may comment on, and a value the controller
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
        if name == crate::runs::engine::ITEM_ENV {
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
    env.extend(item.map(|i| (crate::runs::engine::ITEM_ENV.to_string(), i.to_string())));
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
        variant,
        ..
    } = opts
    {
        argv.push("--max-cost".to_string());
        argv.push(format!("{max_cost}"));
        argv.push("--max-time".to_string());
        argv.push(max_time.as_str().to_string());
        if let Some(variant) = variant {
            argv.push("--variant".to_string());
            argv.push(variant.to_string());
        }
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

/// Write the launch's stored pack into its own directory, checked against the stored tree, write
/// the run's inputs over it, and spawn the engine on it. Returns as
/// soon as the child is running: a playbook run is minutes to hours, and the serial reconcile
/// worker may not park on it. The caller has already claimed the issue and recorded the run row,
/// and hands the child to [`supervise`].
pub(crate) async fn start(
    db: &Db,
    cfg: &ControllerCfg,
    issue_key: &str,
    run_id: &str,
    opts: RunRenderOpts,
) -> Result<LocalRun> {
    // Absolute: the engine is spawned with the pack as its cwd, so a scratch root that is
    // configured relatively (the default `state`) would resolve against the wrong directory.
    let dir = std::path::absolute(run_dir(cfg.scratch_root(), run_id))
        .context("resolving the local run directory")?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("clearing the local run dir {}", dir.display()))?;
    }
    let slug = crate::model::sanitize_key(issue_key);
    let tree = crate::runs::blob_store::get_pack(db.pool(), &slug)
        .await?
        .with_context(|| format!("no stored pack for playbook launch {issue_key}"))?;
    let pack = dir.join("pack");
    let write_to = pack.clone();
    tokio::task::spawn_blocking(move || {
        crate::playbooks::packs::write_checked_tree(&tree, &write_to)
    })
    .await
    .context("joining the local pack write")?
    .context("writing the launch's pack for a local run")?;
    let inputs = crate::runs::workpod::run_inputs(db, issue_key, &pack).await?;
    crate::playbooks::packs::write_files(&inputs, &pack)
        .context("writing the run's inputs over its pack")?;

    let bin = crate::runs::engine::resolve_bin();
    let mut argv = run_argv(&pack.join("crucible.toml"), &opts);
    if let RunRenderOpts::Playbook {
        rerun: Some(rerun), ..
    } = &opts
    {
        let parent = dir.join("parent");
        crate::runs::rerun::materialize_parent(db, cfg.scratch_root(), &rerun.run, &parent)
            .await
            .with_context(|| format!("laying out the evidence of {} for its rerun", rerun.run))?;
        argv.push("--rerun-of".to_string());
        argv.push(parent.to_string_lossy().into_owned());
        for task in &rerun.from {
            argv.push("--from".to_string());
            argv.push(task.clone());
        }
    }
    let deadline = match &opts {
        RunRenderOpts::Playbook { max_time, .. } => {
            Duration::from_secs(max_time.secs()) + CEILING_SLACK
        }
        RunRenderOpts::Loop { .. } => CEILING_SLACK,
    };
    let exposure = crate::launches::store::exposure_for_issue(db.pool(), issue_key).await??;
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
    env.retain(|(name, _)| {
        ![
            crucible_contract::ENV_RUN_NAME,
            crucible_contract::ENV_CONTROLLER_URL,
            crucible_contract::ENV_POD_TOKEN,
        ]
        .contains(&name.as_str())
    });
    env.extend(controller_env(db, cfg, run_id, deadline).await?);
    env.push((
        crucible_contract::ENV_RUN_NAME.to_string(),
        run_id.to_string(),
    ));
    let child = tokio::process::Command::new(&bin)
        .args(&argv)
        .current_dir(&pack)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawning `{} plan run` for {run_id}", bin.display()))?;
    let group = ProcessGroup::led_by(&child)?;
    tracing::info!(%issue_key, %run_id, dir = %dir.display(), "playbook launch running locally");
    Ok(LocalRun {
        child,
        group,
        dir,
        run_id: run_id.to_string(),
        deadline,
    })
}

/// Drive one local run to its end: stream its output into the log, kill it if it outlives its own
/// ceiling, and fold whatever session it published into the ledger through the pod path's
/// completion. A run that published nothing parks the launch with the supervisor's account of how
/// it ended, so a failed local run is never a silently `running` row.
pub async fn supervise(db: Db, key: String, run: LocalRun) {
    let LocalRun {
        mut child,
        group,
        dir,
        run_id,
        deadline,
    } = run;
    let tail = Tail::default();
    let mut streams = tokio::task::JoinSet::new();
    let log = EngineLog::open(&dir, &run_id).await;
    // Both pipes are drained concurrently with the wait, so a chatty run can never fill a pipe
    // buffer and wedge the engine.
    if let Some(out) = child.stdout.take() {
        let run_id = run_id.clone();
        let log = log.as_ref().map(EngineLog::sink);
        let tail = tail.clone();
        streams.spawn(async move { pump(BufReader::new(out), &run_id, "stdout", log, tail).await });
    }
    if let Some(err) = child.stderr.take() {
        let run_id = run_id.clone();
        let log = log.as_ref().map(EngineLog::sink);
        let tail = tail.clone();
        streams.spawn(async move { pump(BufReader::new(err), &run_id, "stderr", log, tail).await });
    }

    let outcome = match tokio::time::timeout(deadline, child.wait()).await {
        Ok(Ok(status)) => {
            drop(group);
            format!("engine exited {status}")
        }
        Ok(Err(e)) => {
            drop(group);
            format!("waiting on the engine failed: {e}")
        }
        Err(_) => {
            group.interrupt();
            let _ = tokio::time::timeout(INTERRUPT_GRACE, child.wait()).await;
            drop(group);
            let _ = child.wait().await;
            format!(
                "the engine outlived its ceiling by {}s and was killed",
                CEILING_SLACK.as_secs()
            )
        }
    };
    let drained = tokio::time::timeout(PIPE_DRAIN_GRACE, async {
        while streams.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        streams.shutdown().await;
    }
    if let Some(log) = log {
        log.close().await;
    }

    let session = std::fs::read_to_string(session_log(&dir)).unwrap_or_default();
    if session.trim().is_empty() {
        let detail = format!("{outcome}. Last output:\n{}", tail.text());
        tracing::warn!(%key, %run_id, "{detail}");
        settle_no_session(&db, &key, &run_id, detail).await;
        return;
    }
    if let Err(e) = complete(&db, &key, &run_id, &session).await {
        tracing::warn!(%key, %run_id, error = %format!("{e:#}"), "ingesting a local run failed");
    }
}

/// Store the session a local run published as its artifact, as the pod path does, then fold it.
async fn complete(db: &Db, key: &str, run_id: &str, session: &str) -> Result<()> {
    crate::runs::blob_store::put_run_session(db.pool(), run_id, session.as_bytes()).await?;
    crate::runs::completion::complete_run(db, key, None, run_id, None, session, None).await?;
    Ok(())
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
        if let Err(e) = complete(db, &key, &run_id, &session).await {
            tracing::warn!(%key, %run_id, error = %format!("{e:#}"), "adopting an orphaned local run failed");
        }
    }
    Ok(())
}

/// How many lines may wait on the engine log's writer before a pump waits with them.
const ENGINE_LOG_BACKLOG: usize = 64;

/// The file both pipes are mirrored into, served by `GET /api/runs/{run_id}/log`. One task owns
/// the file and writes the lines the pumps send it.
struct EngineLog {
    lines: mpsc::Sender<String>,
    writer: tokio::task::JoinHandle<()>,
}

impl EngineLog {
    /// A log that cannot be opened is no reason to refuse the run: the output still reaches the
    /// tracing subscriber and the failure tail.
    async fn open(dir: &Path, run_id: &str) -> Option<Self> {
        let path = engine_log(dir);
        match tokio::fs::File::create(&path).await {
            Ok(file) => {
                let (lines, inbox) = mpsc::channel(ENGINE_LOG_BACKLOG);
                let writer = tokio::spawn(write_engine_log(file, inbox, run_id.to_owned()));
                Some(EngineLog { lines, writer })
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "keeping no engine log for a local run");
                None
            }
        }
    }

    fn sink(&self) -> mpsc::Sender<String> {
        self.lines.clone()
    }

    /// Wait until every line sent so far is in the file. Returns once every sink is dropped.
    async fn close(self) {
        drop(self.lines);
        let _ = self.writer.await;
    }
}

async fn write_engine_log(
    mut file: tokio::fs::File,
    mut lines: mpsc::Receiver<String>,
    run_id: String,
) {
    while let Some(line) = lines.recv().await {
        if let Err(e) = file.write_all(format!("{line}\n").as_bytes()).await {
            tracing::debug!(%run_id, error = %e, "writing the engine log failed");
        }
    }
    if let Err(e) = file.flush().await {
        tracing::debug!(%run_id, error = %e, "flushing the engine log failed");
    }
}

/// Where a local run reaches the controller that spawned it when no public URL is configured: the
/// controller's default loopback bind.
const LOCAL_CONTROLLER_URL: &str = "http://127.0.0.1:8870";

/// What a local run reaches its own controller with: the controller's URL and a pod token for this
/// run, which revokes when the run completes.
async fn controller_env(
    db: &Db,
    cfg: &ControllerCfg,
    run_id: &str,
    lifetime: std::time::Duration,
) -> Result<Vec<(String, String)>> {
    let token = crate::runs::pod_token::mint(
        db.pool(),
        &crate::runs::pod_token::Subject::Run(run_id.to_owned()),
        None,
        lifetime,
    )
    .await?;
    Ok(vec![
        (
            crucible_contract::ENV_CONTROLLER_URL.to_string(),
            cfg.public_url
                .clone()
                .unwrap_or_else(|| LOCAL_CONTROLLER_URL.to_string()),
        ),
        (crucible_contract::ENV_POD_TOKEN.to_string(), token),
    ])
}

/// Read one of the child's pipes to EOF, logging every line, mirroring it into the run's engine
/// log, and keeping it for a failure tail.
async fn pump<R>(
    reader: BufReader<R>,
    run_id: &str,
    stream: &str,
    log: Option<mpsc::Sender<String>>,
    tail: Tail,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!(%run_id, %stream, "{line}");
        if let Some(log) = &log {
            let _ = log.send(line.clone()).await;
        }
        tail.push(line);
    }
}

#[cfg(test)]
mod tests {
    /// Local mode has no registry, so the subprocess starts from nothing: the host variables,
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
                (crate::runs::engine::ITEM_ENV, "owner/repo#1"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
        };
        assert_eq!(
            run_env(inherited(), &[], Some("owner/repo#9"), None).expect("no allowlist"),
            vec![
                ("PATH".to_string(), "/usr/bin".to_string()),
                (
                    crate::runs::engine::ITEM_ENV.to_string(),
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
            rerun: None,
            variant: None,
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

    use crate::model::MaxTime;
    use crate::runs::local_run::*;

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
            rerun: None,
            variant: None,
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

    /// A launch's manifest variant reaches `plan run` ahead of its params.
    #[test]
    fn a_variant_reaches_the_local_argv() {
        let opts = RunRenderOpts::Playbook {
            params: vec![("topic".to_string(), "t".to_string())],
            max_cost: 4.5,
            max_time: MaxTime::parse("30m").expect("duration"),
            agent: crate::playbooks::providers::AgentSelection::default(),
            rerun: None,
            variant: Some(crucible::knobs::VariantName::parse("offline").expect("name")),
        };
        let argv = run_argv(Path::new("/runs/r1/pack/crucible.toml"), &opts);
        assert_eq!(
            argv[8..],
            ["--variant", "offline", "--param", "topic=t"],
            "{argv:?}"
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
                harness: Some(crucible::knobs::Harness::Codex),
                model: Some("gpt-5.6-luna".to_string()),
            },
            rerun: None,
            variant: None,
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
        let log = EngineLog::open(dir.path(), "r1")
            .await
            .expect("the log opens in a real directory");
        let tail = Tail::default();
        pump(
            BufReader::new(&b"line one\nline two\n"[..]),
            "r1",
            "stdout",
            Some(log.sink()),
            tail.clone(),
        )
        .await;
        pump(
            BufReader::new(&b"complaint\n"[..]),
            "r1",
            "stderr",
            Some(log.sink()),
            tail.clone(),
        )
        .await;
        assert_eq!(tail.text(), "line one\nline two\ncomplaint");
        log.close().await;
        let written = std::fs::read_to_string(engine_log(dir.path())).expect("the mirror exists");
        assert_eq!(written, "line one\nline two\ncomplaint\n");

        let gone = EngineLog::open(&dir.path().join("missing"), "r1").await;
        assert!(gone.is_none(), "an unwritable dir keeps no log");
        let still = Tail::default();
        pump(
            BufReader::new(&b"tail\n"[..]),
            "r1",
            "stdout",
            None,
            still.clone(),
        )
        .await;
        assert_eq!(
            still.text(),
            "tail",
            "output still reaches the failure tail"
        );
    }

    #[test]
    fn the_tail_keeps_only_the_last_lines() {
        let tail = Tail::default();
        for n in 0..TAIL_LINES + 2 {
            tail.push(n.to_string());
        }
        let text = tail.text();
        let kept: Vec<&str> = text.lines().collect();
        assert_eq!(kept.len(), TAIL_LINES);
        assert_eq!(kept.first().copied(), Some("2"));
        assert_eq!(
            kept.last().copied(),
            Some((TAIL_LINES + 1).to_string().as_str())
        );
    }

    fn spawn_group_with_straggler(leader_waits: bool) -> (tokio::process::Child, ProcessGroup) {
        let script = if leader_waits {
            "sleep 300 & echo $!; wait"
        } else {
            "sleep 300 & echo $!"
        };
        let child = tokio::process::Command::new("sh")
            .args(["-c", script])
            .stdout(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sh");
        let group = ProcessGroup::led_by(&child).expect("the child has a pid");
        (child, group)
    }

    async fn straggler_pid(child: &mut tokio::process::Child) -> nix::unistd::Pid {
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("stdout is piped"))
            .read_line(&mut line)
            .await
            .expect("read the background pid");
        nix::unistd::Pid::from_raw(line.trim().parse().expect("a pid"))
    }

    fn alive(pid: nix::unistd::Pid) -> bool {
        nix::sys::signal::kill(pid, None).is_ok()
    }

    /// A shutdown drops the supervisor mid-run: the engine and whatever it started go with it,
    /// not just the direct child.
    #[tokio::test]
    async fn dropping_a_process_group_kills_what_the_engine_spawned() {
        let (mut child, group) = spawn_group_with_straggler(true);
        let straggler = straggler_pid(&mut child).await;
        assert!(alive(straggler), "the background sleep is running");
        drop(group);
        child.wait().await.expect("reap the killed leader");
        until_dead(straggler).await;
    }

    /// The engine exiting on its own takes the group with it.
    #[tokio::test]
    async fn an_engine_that_exits_normally_still_takes_its_straggler() {
        let (mut child, group) = spawn_group_with_straggler(false);
        let straggler = straggler_pid(&mut child).await;
        child.wait().await.expect("the leader exits on its own");
        assert!(alive(straggler), "the background sleep outlived its leader");
        drop(group);
        until_dead(straggler).await;
    }

    async fn until_dead(pid: nix::unistd::Pid) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while alive(pid) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "pid {pid} outlived its process group's kill"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// An engine stand-in: it writes `boom` to stderr, starts a child that leaves the group
    /// holding its pipes and prints the child's pid, then waits. SIGINT makes it stop the child
    /// and exit 130, as the engine does; SIGTERM makes it exit and leave the child behind.
    const ENGINE_WITH_OUTSIDER: &str = r#"
$| = 1;
my $outsider;
$SIG{INT} = sub { kill "TERM", $outsider; exit 130 };
$SIG{TERM} = sub { exit 0 };
print STDERR "boom\n";
$outsider = fork();
if ($outsider == 0) { setpgrp(0, 0); print "$$\n"; exec "sleep", "300" }
waitpid($outsider, 0);
"#;

    async fn local_run_with_outsider(
        dir: &Path,
        run_id: &str,
        deadline: Duration,
    ) -> (LocalRun, nix::unistd::Pid) {
        let mut child = tokio::process::Command::new("perl")
            .args(["-e", ENGINE_WITH_OUTSIDER])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .expect("spawn perl");
        let outsider = straggler_pid(&mut child).await;
        let group = ProcessGroup::led_by(&child).expect("the child has a pid");
        let run = LocalRun {
            child,
            group,
            dir: dir.to_path_buf(),
            run_id: run_id.to_string(),
            deadline,
        };
        (run, outsider)
    }

    /// A `running` launch and its local run's row, as dispatch leaves them for the supervisor.
    async fn running_launch(db: &Db, key: &str, run_id: &str) -> Result<()> {
        crate::issues::store::upsert_issue(
            db.pool(),
            &crate::NewIssue {
                key: key.to_string(),
                repo: "o/r".to_string(),
                priority: 0,
                evidence_url: None,
                title: None,
                author: None,
                body: None,
                labels: Vec::new(),
                upstream_updated_at: None,
            },
        )
        .await?;
        assert!(
            crate::issues::store::claim_issue(db.pool(), key, Status::New, Status::Running).await?
        );
        crate::runs::store::insert_run(
            db.pool(),
            &crate::NewRun {
                run_id: run_id.to_string(),
                scope: None,
                issue: Some(key.to_string()),
                identity_digest: None,
                status: "running".to_string(),
                pod: None,
                session_uri: None,
                best_score: None,
                cost_usd: None,
            },
        )
        .await?;
        Ok(())
    }

    async fn parked_reason(db: &Db, key: &str) -> Result<String> {
        let issue = crate::issues::store::get_issue(db.pool(), key)
            .await?
            .context("the launch's issue")?;
        assert_eq!(issue.status, Status::Parked);
        Ok(issue.parked_reason.unwrap_or_default())
    }

    /// A process outside the engine's group that keeps the engine's stderr open does not hold the
    /// run open, and what the pipe carried before the cut still reaches the park and the log.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_pipe_held_open_outside_the_group_does_not_hold_the_run_open(
        pool: sqlx::PgPool,
    ) -> Result<()> {
        let db = Db::new(pool);
        let dir = tempfile::tempdir()?;
        running_launch(&db, "o/r#1", "run-held").await?;
        let (run, outsider) =
            local_run_with_outsider(dir.path(), "run-held", Duration::from_secs(60)).await;
        nix::sys::signal::kill(run.group.0, nix::sys::signal::Signal::SIGTERM)?;

        tokio::time::timeout(
            PIPE_DRAIN_GRACE * 3,
            supervise(db.clone(), "o/r#1".to_string(), run),
        )
        .await
        .context("the supervisor waited on a pipe that never closes")?;

        let held = alive(outsider);
        let _ = nix::sys::signal::kill(outsider, nix::sys::signal::Signal::SIGKILL);
        assert!(held, "the group's kill missed the pipe's holder");
        let reason = parked_reason(&db, "o/r#1").await?;
        assert!(reason.contains("Last output:\nboom"), "{reason}");
        let status: String = sqlx::query_scalar("SELECT status FROM runs WHERE run_id = $1")
            .bind("run-held")
            .fetch_one(db.pool())
            .await?;
        assert_eq!(status, "no-session");
        assert_eq!(std::fs::read_to_string(engine_log(dir.path()))?, "boom\n");
        Ok(())
    }

    /// An engine past its ceiling is interrupted before its group is killed, so it stops the
    /// children that group does not hold.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_engine_past_its_ceiling_stops_its_children_outside_the_group(
        pool: sqlx::PgPool,
    ) -> Result<()> {
        let db = Db::new(pool);
        let dir = tempfile::tempdir()?;
        running_launch(&db, "o/r#1", "run-late").await?;
        let (run, outsider) =
            local_run_with_outsider(dir.path(), "run-late", Duration::from_millis(200)).await;

        tokio::time::timeout(
            PIPE_DRAIN_GRACE,
            supervise(db.clone(), "o/r#1".to_string(), run),
        )
        .await
        .context("the engine's child outlived it and held the pipes")?;

        until_dead(outsider).await;
        let reason = parked_reason(&db, "o/r#1").await?;
        assert!(reason.contains("outlived its ceiling"), "{reason}");
        Ok(())
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
