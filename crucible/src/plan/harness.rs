//! The real-harness runner. `Agent` tasks execute through [`crate::agent::run_turn`], the
//! same path the loop uses (local claude, hermes, openshell, or the free `command` backend),
//! with the task's harness/model/effort overriding the manifest's `[agent]` defaults.
//! `Command` tasks run in the workspace via the shell runner.
//!
//! Result contract: the turn writes its structured output to `PLAN_TASK_RESULT.json` in the
//! workspace root; the runner drains it (read + remove) after the turn, like the loop drains
//! its sentinel files. No file, no pass.

use std::collections::BTreeMap;

use clap::ValueEnum;

use serde_json::Value;

use crate::agent::event::{AgentEvent, RawStream};
use crate::agent::harness::HarnessRuntime;
use crate::plan::exec::{Attempt, AttemptOutcome, BatchItem, TaskRunner, TransportFailure};
use crate::plan::ir::{Isolation, Task, TaskKind, TaskName};
use crate::plan::runner::ShellRunner;
use crucible_contract::TransportCause;
use std::path::{Path, PathBuf};

use std::sync::atomic::{AtomicU64, Ordering};

use crate::args::{Args, Paths};

const RESULT_FILE: &str = "PLAN_TASK_RESULT.json";

use crate::plan::STAGED_INPUTS;

/// The most a single run may capture. Operator-owned, and the run's bound rather than any one
/// task's: a pipeline that captures a little at every stage can still fill a disk.
const MAX_CAPTURED_BYTES: u64 = 256 * 1024 * 1024;

/// A declared path resolved inside the workspace, or why it is refused. Symlinks are refused per
/// component and a second name is refused outright, for the same reason the pack's own paths are:
/// resolving a path cannot see a hard link.
fn confined(workspace: &Path, declared: &str) -> Result<PathBuf, String> {
    let mut path = workspace.to_path_buf();
    for component in Path::new(declared).components() {
        let std::path::Component::Normal(component) = component else {
            return Err(format!(
                "declared file {declared:?} is not workspace-relative"
            ));
        };
        path.push(component);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!("declared file {declared:?} traverses a symlink"));
            }
            Ok(metadata) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if metadata.is_file() && metadata.nlink() > 1 {
                        return Err(format!(
                            "declared file {declared:?} has {} names; a captured file has one",
                            metadata.nlink()
                        ));
                    }
                }
                let _ = metadata;
            }
            Err(_) => {}
        }
    }
    Ok(path)
}

fn captured_dir(state: &Path, task: &str) -> PathBuf {
    state.join("files").join(task)
}

fn captured_path(state: &Path, task: &str, declared: &str) -> PathBuf {
    captured_dir(state, task).join(declared)
}

/// The mode the engine gives a captured file, regardless of what the source carried. A
/// declared output derived from a staged input arrives 0444, and a capture that kept that mode
/// would make the next run's copy fail with EACCES.
fn engine_mode(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644));
    }
    #[cfg(not(unix))]
    {
        if let Ok(metadata) = std::fs::metadata(path) {
            let mut permissions = metadata.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            let _ = std::fs::set_permissions(path, permissions);
        }
    }
}

fn read_only(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o444));
    }
    #[cfg(not(unix))]
    {
        if let Ok(metadata) = std::fs::metadata(path) {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(true);
            let _ = std::fs::set_permissions(path, permissions);
        }
    }
}

/// What a declared path held at one moment, as its size and the digest of its content.
#[derive(Debug, PartialEq, Eq)]
enum Fingerprint {
    Absent,
    /// Something is there that could not be read as a file, so authorship is undecidable and
    /// the comparison must refuse rather than guess.
    Unreadable,
    Content {
        bytes: u64,
        content: git2::Oid,
    },
}

fn fingerprint(path: &Path) -> Fingerprint {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Fingerprint::Absent;
    };
    match git2::Oid::hash_file(git2::ObjectType::Blob, path) {
        Ok(content) => Fingerprint::Content {
            bytes: metadata.len(),
            content,
        },
        Err(_) => Fingerprint::Unreadable,
    }
}

/// Each declared path's content in the root the attempt is about to run in, taken before it
/// runs. A failing attempt owns a declared path only where this changed: a file an earlier task
/// left in the workspace is that task's evidence, and publishing it would report a sibling's
/// reading as the failure's.
struct PriorContents(BTreeMap<String, Fingerprint>);

impl PriorContents {
    fn of(workspace: &Path, declared: &[String]) -> Self {
        PriorContents(
            declared
                .iter()
                .map(|path| {
                    let held = confined(workspace, path)
                        .map_or(Fingerprint::Unreadable, |resolved| fingerprint(&resolved));
                    (path.clone(), held)
                })
                .collect(),
        )
    }

    /// Whether the attempt wrote `declared`, now resolved at `path`.
    fn wrote(&self, declared: &str, path: &Path) -> bool {
        let now = fingerprint(path);
        matches!(now, Fingerprint::Content { .. })
            && self.0.get(declared).is_some_and(|before| *before != now)
    }
}

/// One file an ancestor declared, as the producer's name and the path it declared.
pub struct StagedInput {
    pub producer: String,
    pub declared: String,
}

pub struct HarnessRunner {
    pub args: Args,
    pub paths: Paths,
    /// Whether a settled task commits the shared workspace. True for a playbook, whose git
    /// memory is per task; false for the scored loop, which owns the same repository for
    /// keep/discard of whole candidates and would find per-task commits in the middle of it.
    pub commit_per_task: bool,
    /// Bytes captured by this run so far. C-TASK-FILES bounds the run, not any one task: a
    /// pipeline capturing a little at every stage can still fill a disk.
    pub captured_bytes: AtomicU64,
    /// What each dispatched task is owed on the file channel, recorded by [`TaskRunner::stage`]
    /// and materialized into that task's own root when it runs. A task with no entry is one the
    /// executor never staged, and its `inputs/` is left alone.
    pub staged: BTreeMap<TaskName, Vec<StagedInput>>,
}

impl TaskRunner for HarnessRunner {
    fn run(&mut self, task: &Task, attempt: u32, inputs: &BTreeMap<TaskName, Value>) -> Attempt {
        run_task(
            &Dispatch {
                args: &self.args,
                paths: &self.paths,
                captured_bytes: &self.captured_bytes,
                staged: self.staged.get(&task.name).map(Vec::as_slice),
            },
            task,
            attempt,
            inputs,
            None,
        )
    }

    /// Record what this task's ancestors declared, for [`run_task`] to lay down under
    /// `inputs/<producer>/` in whichever root the task runs in.
    ///
    /// It is recorded rather than copied because the root is not known yet: an isolated task's
    /// worktree does not exist until it is dispatched, and a batch of isolated tasks has one
    /// root each with a different ancestor set, so a single shared `inputs/` cannot serve them.
    fn stage(&mut self, task: &Task, producers: &[&Task]) -> Result<(), String> {
        let files = producers
            .iter()
            .flat_map(|producer| {
                producer.emits_files.iter().map(|declared| StagedInput {
                    producer: producer.name.0.clone(),
                    declared: declared.clone(),
                })
            })
            .collect();
        self.staged.insert(task.name.clone(), files);
        Ok(())
    }

    fn has_captured_files(&self, task: &Task) -> bool {
        captured_dir(&self.paths.state, &task.name.0).is_dir()
    }

    /// Discards the set published under this task's name, and for a mapped node the sets
    /// published under its instances' names too: a node that did not expand this run has no
    /// instance rows, so nothing else in the run ever reaches them.
    fn drop_captured(&mut self, task: &Task) {
        let _ = std::fs::remove_dir_all(captured_dir(&self.paths.state, &task.name.0));
        if task.over.is_none() {
            return;
        }
        let Ok(entries) = std::fs::read_dir(self.paths.state.join("files")) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if crate::plan::exec::is_instance_of(&task.name, &TaskName(name.to_string())) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    /// Commit what a passing task did, and discard what a failing one did.
    ///
    /// The "only when" half is the load-bearing one. Leaving a failed task's edits in the shared
    /// tree does not merely fail to commit them: the next task to pass sweeps them into its own
    /// commit, so the failed task contributes after all. Discarding at the point of failure is
    /// what keeps that from happening.
    ///
    /// An isolated task contributes nothing either way. Its worktree is discarded by contract
    /// and only its declared output continues, so there is no workspace change to record.
    fn settled(&mut self, task: &Task, passed: bool) {
        if !self.commit_per_task || task.isolation.is_some() {
            return;
        }
        let workspace = &self.paths.workspace;
        let outcome = if passed {
            crucible_vcs::git_memory::snapshot(workspace, &format!("task {}", task.name))
                .map(|_| ())
        } else {
            crucible_vcs::vcs::head_sha(workspace)
                .and_then(|head| crucible_vcs::git_memory::restore(workspace, &head, &[]))
        };
        if let Err(error) = outcome {
            // Git memory failing is worth saying out loud: a resumed run rebuilds from these
            // commits, so a silent miss here is a silent gap there.
            eprintln!(
                "[{}] git memory ({}) failed: {error:#}",
                task.name,
                if passed { "commit" } else { "discard" }
            );
        }
    }

    /// Isolated tasks that are ready together run concurrently, each in its own worktree.
    /// Concurrency is what isolation buys: two reviewers reading the same artifact would
    /// otherwise race on the single `PLAN_TASK_RESULT.json` in the shared workspace.
    fn run_many(&mut self, batch: &[BatchItem<'_>]) -> Vec<Attempt> {
        if batch.len() == 1 {
            let b = &batch[0];
            return vec![run_task(
                &Dispatch {
                    args: &self.args,
                    paths: &self.paths,
                    captured_bytes: &self.captured_bytes,
                    staged: self.staged.get(&b.task.name).map(Vec::as_slice),
                },
                b.task,
                b.attempt,
                &b.inputs,
                None,
            )];
        }
        // Every item clones the same workspace, so its pending state is captured once here:
        // concurrent `git add -A` in one repo races on `.git/index.lock`.
        let pending = match crate::plan::worktree::capture_diff(&self.paths.workspace) {
            Ok(p) => p,
            Err(e) => {
                let note = format!("capturing the workspace's uncommitted state failed: {e:#}");
                return batch
                    .iter()
                    .map(|_| Attempt::failed(0.0, note.clone()))
                    .collect();
            }
        };
        let captured = &self.captured_bytes;
        let staged = &self.staged;
        std::thread::scope(|scope| {
            let handles: Vec<_> = batch
                .iter()
                .map(|b| {
                    let args = self.args.clone();
                    let paths = self.paths.clone();
                    let pending = pending.as_str();
                    scope.spawn(move || {
                        run_task(
                            &Dispatch {
                                args: &args,
                                paths: &paths,
                                captured_bytes: captured,
                                staged: staged.get(&b.task.name).map(Vec::as_slice),
                            },
                            b.task,
                            b.attempt,
                            &b.inputs,
                            Some(pending),
                        )
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join().unwrap_or_else(|_| {
                        Attempt::failed(0.0, "task thread panicked".to_string())
                    })
                })
                .collect()
        })
    }
}

/// What one dispatch needs besides the task: the run's configuration and paths, the run's
/// captured-byte total, and the files this task's ancestors declared.
struct Dispatch<'a> {
    args: &'a Args,
    paths: &'a Paths,
    captured_bytes: &'a AtomicU64,
    staged: Option<&'a [StagedInput]>,
}

/// Dispatch one task, in the shared workspace or in a private worktree. `pending` is the
/// shared workspace's uncommitted patch when a concurrent caller already captured it for
/// the whole batch; `None` means capture it here.
fn run_task(
    cx: &Dispatch<'_>,
    task: &Task,
    attempt: u32,
    inputs: &BTreeMap<TaskName, Value>,
    pending: Option<&str>,
) -> Attempt {
    let Dispatch {
        args,
        paths,
        captured_bytes,
        staged,
    } = *cx;
    let Some(Isolation::Worktree) = task.isolation else {
        if let Err(e) = materialize_inputs(&paths.state, &paths.workspace, staged) {
            return Attempt::transport(TransportCause::Workspace, e);
        }
        let before = PriorContents::of(&paths.workspace, &task.emits_files);
        let attempt_out = prepare_and_run(args, paths, task, attempt, inputs);
        return capture_declared(
            paths,
            &paths.workspace,
            task,
            attempt_out,
            captured_bytes,
            &before,
        );
    };
    // A private clone of the workspace. Its edits are discarded on cleanup: what leaves an
    // isolated task is its declared output, so this is for review/analysis work, not for
    // coding tasks whose diff has to survive (the wide tournament carries those out itself).
    let root = paths.state.join("plan-iso");
    if let Err(e) = std::fs::create_dir_all(&root) {
        return Attempt::transport(
            TransportCause::Workspace,
            format!("creating the isolation root failed: {e}"),
        );
    }
    let worktree = root.join(task_worktree_name(&task.name));
    let captured;
    let pending = match pending {
        Some(p) => p,
        None => match crate::plan::worktree::capture_diff(&paths.workspace) {
            Ok(p) => {
                captured = p;
                &captured
            }
            Err(e) => {
                return Attempt::failed(
                    0.0,
                    format!("capturing the workspace's uncommitted state failed: {e:#}"),
                );
            }
        },
    };
    if let Err(e) = crate::plan::worktree::setup(&paths.workspace, &worktree, pending) {
        return Attempt::transport(
            TransportCause::Workspace,
            format!("worktree setup failed: {e:#}"),
        );
    }
    // `inputs/` is excluded from the workspace's git memory, so neither the clone nor the
    // pending patch carries it. Lay this task's own staged set down here, or an isolated
    // consumer never sees what its ancestors declared.
    if let Err(e) = materialize_inputs(&paths.state, &worktree, staged) {
        return Attempt::transport(TransportCause::Workspace, e);
    }
    let iso = Paths::for_worktree(worktree.clone(), paths.skills.clone());
    let _ = std::fs::create_dir_all(&iso.state);
    let before = PriorContents::of(&iso.workspace, &task.emits_files);
    let attempt_out = prepare_and_run(args, &iso, task, attempt, inputs);
    // Before the worktree goes: a declared file is part of the task's output, not part of the
    // workspace state isolation discards, so it has to be taken while the tree is still there.
    let attempt_out = capture_declared(
        paths,
        &iso.workspace,
        task,
        attempt_out,
        captured_bytes,
        &before,
    );
    let _ = std::fs::remove_dir_all(&worktree);
    attempt_out
}

/// Take a settled attempt's declared files, or withhold the whole set.
///
/// A declared file that is absent after an otherwise-passing attempt is output drift, and it
/// fails at the task that promised it rather than as a mystery in whatever depended on it. It is
/// not retried: a task that ran and did not produce what it promised will not produce it twice.
///
/// A failing attempt's set is captured too, for a consumer joining `settled`, and it is captured
/// before [`TaskRunner::settled`] discards the workspace. On that path a declared path counts
/// only when this attempt wrote it, which `before` (taken in the same root, before the attempt
/// ran) decides.
///
/// Capture goes to a temporary directory and is published with a rename, so `state/files/<task>`
/// exists only for a task that delivered everything it declared: a partial capture cannot reach
/// a descendant, and the rename replaces the previous run's copy rather than writing into it.
fn capture_declared(
    paths: &Paths,
    workspace: &Path,
    task: &Task,
    attempt: Attempt,
    captured: &AtomicU64,
    before: &PriorContents,
) -> Attempt {
    let failing = match &attempt.outcome {
        AttemptOutcome::Pass(_) => false,
        AttemptOutcome::Fail { .. } => true,
        AttemptOutcome::Skipped(..) | AttemptOutcome::Transport(_) => return attempt,
    };
    if task.emits_files.is_empty() {
        return attempt;
    }
    let staging = paths.state.join("files.tmp").join(&task.name.0);
    let _ = std::fs::remove_dir_all(&staging);
    let mut charged = 0u64;
    let taken = (|| -> Result<(), String> {
        for declared in &task.emits_files {
            let from = confined(workspace, declared)?;
            let size = match std::fs::metadata(&from) {
                Ok(metadata) if metadata.is_file() => metadata.len(),
                Ok(_) => {
                    return Err(format!("declared file {declared:?} is not a regular file"));
                }
                Err(_) => {
                    return Err(format!(
                        "declared file {declared:?} is absent after a {} attempt",
                        if failing { "failing" } else { "passing" }
                    ));
                }
            };
            if failing && !before.wrote(declared, &from) {
                return Err(format!(
                    "declared file {declared:?} holds what it held before the attempt started, so \
                     this attempt did not write it"
                ));
            }
            let total = captured
                .fetch_add(size, Ordering::Relaxed)
                .saturating_add(size);
            charged = charged.saturating_add(size);
            if total > MAX_CAPTURED_BYTES {
                return Err(format!(
                    "this run's declared files exceed {MAX_CAPTURED_BYTES} bytes"
                ));
            }
            let to = staging.join(declared);
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("capturing {declared:?}: {error}"))?;
            }
            std::fs::copy(&from, &to)
                .map_err(|error| format!("capturing {declared:?}: {error}"))?;
            engine_mode(&to);
        }
        Ok(())
    })();
    let published = captured_dir(&paths.state, &task.name.0);
    let outcome = taken.and_then(|()| {
        if let Some(parent) = published.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("publishing the files {} declared: {error}", task.name))?;
        }
        let _ = std::fs::remove_dir_all(&published);
        std::fs::rename(&staging, &published)
            .map_err(|error| format!("publishing the files {} declared: {error}", task.name))
    });
    if let Err(why) = outcome {
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_dir_all(&published);
        captured.fetch_sub(charged, Ordering::Relaxed);
        return withheld(attempt, why);
    }
    attempt
}

/// A capture that published nothing. A passing attempt becomes the measured failure, since it
/// promised the set; an attempt that had already failed keeps its status. Either way the
/// attempt's own reading is retained and the capture problem joins the note.
fn withheld(attempt: Attempt, why: String) -> Attempt {
    let outcome = match attempt.outcome {
        AttemptOutcome::Fail { note, output } => AttemptOutcome::Fail {
            note: format!("{note}; {why}"),
            output,
        },
        AttemptOutcome::Pass(output) => AttemptOutcome::Fail {
            note: why,
            output: Some(output),
        },
        _ => AttemptOutcome::fail(why),
    };
    Attempt {
        outcome,
        cost_usd: attempt.cost_usd,
    }
}

/// Lay a task's staged inputs down under `<root>/inputs/<producer>/`, replacing whatever a
/// previous dispatch left there.
///
/// Namespaced by producer so two tasks declaring the same path cannot collide, and made
/// read-only so a consumer cannot edit what it was handed and pass the edit on as if the
/// producer had said it. `None` is a task the executor never staged: its root is left alone.
fn materialize_inputs(
    state: &Path,
    root: &Path,
    staged: Option<&[StagedInput]>,
) -> Result<(), String> {
    let Some(staged) = staged else {
        return Ok(());
    };
    let inputs = root.join(STAGED_INPUTS);
    let _ = std::fs::remove_dir_all(&inputs);
    for file in staged {
        let from = captured_path(state, &file.producer, &file.declared);
        if !from.exists() {
            continue;
        }
        let to = inputs.join(&file.producer).join(&file.declared);
        if let Some(parent) = to.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            return Err(format!("staging {}: {error}", to.display()));
        }
        let _ = std::fs::remove_file(&to);
        if let Err(error) = std::fs::copy(&from, &to) {
            return Err(format!("staging {}: {error}", to.display()));
        }
        read_only(&to);
    }
    Ok(())
}

fn prepare_and_run(
    args: &Args,
    paths: &Paths,
    task: &Task,
    attempt: u32,
    inputs: &BTreeMap<TaskName, Value>,
) -> Attempt {
    for (src, dst) in &args.workflow_frozen_injects {
        if let Err(e) = crate::manifest::apply_inject(src, &paths.workspace.join(dst)) {
            return Attempt::transport(
                TransportCause::Workspace,
                format!(
                    "restoring frozen inject {} -> {} failed: {e:#}",
                    src.display(),
                    dst.display()
                ),
            );
        }
    }
    let out = run_in(args, paths, task, attempt, inputs);
    Attempt {
        outcome: out.outcome.settle_declared(),
        cost_usd: out.cost_usd,
    }
}

/// One task against a specific workspace. `Command` tasks go to the shell runner; `Agent`
/// tasks run through the real [`crate::agent::run_turn`] with the task's knob overrides.
fn run_in(
    args: &Args,
    paths: &Paths,
    task: &Task,
    attempt: u32,
    inputs: &BTreeMap<TaskName, Value>,
) -> Attempt {
    let (prompt, harness, model, effort) = match &task.task {
        TaskKind::Agent {
            prompt,
            harness,
            model,
            effort,
        } => (prompt, harness, model, effort),
        TaskKind::Command { .. } | TaskKind::Evaluate { .. } | TaskKind::Report { .. } => {
            let mut shell = ShellRunner {
                workdir: paths.workspace.clone(),
                agent_cmd: None,
            };
            return if task.isolation == Some(Isolation::Worktree) {
                shell.run_in_prepared_worktree(task, inputs)
            } else {
                shell.run(task, attempt, inputs)
            };
        }
        TaskKind::TopK { .. } => {
            return Attempt::failed(0.0, "reducer task reached the runner".to_string());
        }
        TaskKind::Engine { .. } => {
            return Attempt::failed(0.0, "engine task reached a non-loop runner".to_string());
        }
    };

    // Per-task knob overrides on a cloned Args: the heterogeneity axis. Unknown values
    // are a measured failure: a plan naming a harness we can't parse is wrong, not
    // unlucky.
    let mut args = args.clone();
    // Name the task to the turn. A deterministic stand-in needs to know which task it is
    // without matching prompt prose, and a real harness gets it for free in its transcript.
    args.env
        .push((crate::plan::TASK_NAME_ENV.to_string(), task.name.0.clone()));
    if let Some(h) = harness {
        match crate::manifest::Harness::from_str(h, true) {
            Ok(h) => args.harness = Some(h),
            Err(e) => {
                return Attempt::failed(0.0, format!("task names unknown harness {h:?}: {e}"));
            }
        }
    }
    if let Some(m) = model {
        args.model = Some(m.clone());
    }
    if let Some(e) = effort {
        match crate::manifest::ReasoningEffort::from_str(e, true) {
            Ok(e) => args.reasoning_effort = Some(e),
            Err(err) => {
                return Attempt::failed(0.0, format!("task names unknown effort {e:?}: {err}"));
            }
        }
    }
    if let Err(e) = crate::cli::workspace::install_toolbox(
        paths,
        &args.workflow_toolbox_exclude,
        args.harness().skills_dir(),
    ) {
        return Attempt::transport(
            TransportCause::Workspace,
            format!("installing the task toolbox failed: {e:#}"),
        );
    }

    let inputs_json = match serde_json::to_string_pretty(inputs) {
        Ok(j) => j,
        Err(e) => return Attempt::failed(0.0, format!("inputs not serializable: {e}")),
    };
    let full_prompt = format!(
        "{prompt}\n\n## Task inputs\n\nUpstream task results, as JSON:\n\n{inputs_json}\n\n\
         ## Result contract\n\nWhen done, write your final result as a single JSON object \
         to `{RESULT_FILE}` in the workspace root. The run is graded on that file."
    );

    let result_path = paths.workspace.join(RESULT_FILE);
    // Drain any stale result so a pass can only come from THIS turn.
    let _ = std::fs::remove_file(&result_path);

    let name = task.name.0.clone();
    let mut transport_error: Option<TransportFailure> = None;
    let prepared =
        match crate::agent::agent_session::prepare_named(&paths.state, task.session.as_deref()) {
            Ok(prepared) => prepared,
            Err(note) => return Attempt::transport(TransportCause::Workspace, note),
        };
    let turn = crate::agent::run_turn_with_session(
        &args,
        paths,
        &full_prompt,
        false,
        prepared.as_ref(),
        |line, stream, ev| {
            if !line.trim().is_empty() && stream == RawStream::Stderr {
                eprintln!("[{name}] {line}");
            }
            if let Some(failure) = ev.and_then(agent_transport_error) {
                transport_error = Some(failure);
            }
        },
    );
    let cost = turn.cost_usd;
    if let Some(failure) = turn.failure() {
        transport_error = Some(TransportFailure::new(
            failure.transport_cause(),
            failure.to_string(),
        ));
    }
    if let Some(note) = crate::agent::agent_session::commit_if_ok(
        &paths.state,
        prepared.as_ref(),
        transport_error.is_none(),
    ) {
        transport_error = Some(TransportFailure::new(TransportCause::Workspace, note));
    }

    match std::fs::read_to_string(&result_path) {
        Ok(body) => {
            let _ = std::fs::remove_file(&result_path);
            match serde_json::from_str::<Value>(&body) {
                Ok(v) => Attempt {
                    outcome: AttemptOutcome::Pass(v),
                    cost_usd: cost,
                },
                Err(e) => Attempt::failed(cost, format!("{RESULT_FILE} is not valid JSON: {e}")),
            }
        }
        Err(_) => match transport_error {
            Some(failure) => Attempt {
                outcome: AttemptOutcome::Transport(failure),
                cost_usd: cost,
            },
            None => Attempt::failed(
                cost,
                format!("turn ended without writing {RESULT_FILE} — nothing to grade"),
            ),
        },
    }
}

/// An agent-stream error the turn cannot recover from. A typed `Error` event is the provider or
/// the CLI refusing the turn; an errored `Result` is the agent itself ending badly.
fn agent_transport_error(event: &AgentEvent) -> Option<TransportFailure> {
    match event {
        AgentEvent::Error {
            error_type,
            message,
        } => Some(TransportFailure::new(
            TransportCause::Provider,
            format!("{error_type}: {message}"),
        )),
        AgentEvent::Result {
            is_error: true,
            error,
            ..
        } => Some(TransportFailure::new(
            TransportCause::Agent,
            error
                .as_deref()
                .unwrap_or("agent turn ended with an unspecified error"),
        )),
        _ => None,
    }
}

fn task_worktree_name(name: &TaskName) -> String {
    let digest = crucible_contract::artifact::content_digest(name.0.as_bytes());
    format!("task-{}", digest.trim_start_matches("sha256:"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::exec::{ExecCfg, PlanExit, Substrate, TaskStatus};

    /// The executor's own transitions are in its table; a test that trips one fails here.
    fn execute(
        plan: &crate::plan::ir::ValidPlan,
        substrate: &Substrate,
        cfg: ExecCfg,
        runner: &mut dyn crate::plan::exec::TaskRunner,
        on_result: impl FnMut(&crate::plan::ir::Task, &crate::plan::exec::TaskResult),
    ) -> crate::plan::exec::PlanOutcome {
        crate::plan::exec::execute(plan, substrate, cfg, runner, on_result)
            .expect("an executor transition its table does not list")
    }
    use crate::plan::ir::{Join, Plan, Stage};

    fn scratch(tag: &str) -> (std::path::PathBuf, Paths) {
        let dir = std::env::temp_dir().join(format!("crucible-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("workspace")).unwrap();
        std::fs::create_dir_all(dir.join("state")).unwrap();
        let paths =
            crate::args::Paths::for_manifest(dir.join("workspace"), dir.join("state"), &dir, None);
        (dir, paths)
    }

    fn emitting(name: &str, files: &[&str]) -> crate::plan::ir::Task {
        crate::plan::ir::Task {
            name: name.into(),
            task: TaskKind::Command {
                command: "true".into(),
            },
            depends_on: vec![],
            session: None,
            needs: "any".into(),
            required: true,
            isolation: None,
            join: Join::default(),
            stage: Stage::Iteration,
            emits: Vec::new(),
            emits_files: files.iter().map(|f| (*f).to_string()).collect(),
            over: None,
            max_fanout: None,
        }
    }

    fn passing() -> Attempt {
        Attempt {
            outcome: AttemptOutcome::Pass(Value::Null),
            cost_usd: 0.0,
        }
    }

    /// The declared paths as they stood before a dispatch. The passing path never consults
    /// them, so a test driving that path can take them at any point.
    fn prior(paths: &Paths, task: &crate::plan::ir::Task) -> PriorContents {
        PriorContents::of(&paths.workspace, &task.emits_files)
    }

    fn note(attempt: &Attempt) -> String {
        match &attempt.outcome {
            AttemptOutcome::Fail { note, .. } => note.clone(),
            other => panic!("expected a measured failure, got {other:?}"),
        }
    }

    /// A task that delivered some of what it declared delivered none of it. Publishing what was
    /// copied before the missing file would hand a descendant a partial output from a task the
    /// JSON channel reports as failed.
    #[test]
    fn a_partial_capture_publishes_nothing() {
        let (dir, paths) = scratch("capture-partial");
        std::fs::write(paths.workspace.join("A.md"), "present\n").unwrap();
        let task = emitting("draft", &["A.md", "B.md"]);
        let counter = AtomicU64::new(0);
        let before = prior(&paths, &task);
        let out = capture_declared(
            &paths,
            &paths.workspace,
            &task,
            passing(),
            &counter,
            &before,
        );
        let note = note(&out);
        assert!(note.contains("B.md") && note.contains("absent"), "{note}");
        assert!(
            !paths.state.join("files/draft").exists(),
            "a partial capture was published"
        );
        assert!(
            !paths.state.join("files.tmp/draft").exists(),
            "the staging directory outlived the failure"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A declared output copied from a staged input carries the staged input's 0444. Capturing
    /// that mode makes every later run of the same task fail with EACCES, and re-running is a
    /// supported flow.
    #[test]
    fn a_captured_file_takes_the_engines_mode_not_the_sources() {
        let (dir, paths) = scratch("capture-mode");
        let source = paths.workspace.join("A.md");
        std::fs::write(&source, "read-only\n").unwrap();
        read_only(&source);
        let task = emitting("draft", &["A.md"]);
        let counter = AtomicU64::new(0);
        let before = prior(&paths, &task);
        for round in 1..=3 {
            let out = capture_declared(
                &paths,
                &paths.workspace,
                &task,
                passing(),
                &counter,
                &before,
            );
            assert!(
                matches!(out.outcome, AttemptOutcome::Pass(_)),
                "round {round}: {:?}",
                out.outcome
            );
            let captured = paths.state.join("files/draft/A.md");
            assert_eq!(std::fs::read_to_string(&captured).unwrap(), "read-only\n");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&captured).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o644, "round {round}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The captured-bytes bound is the run's, not one task's: a pipeline capturing a little at
    /// every stage still has to hit it.
    #[test]
    fn the_captured_bytes_bound_counts_the_run() {
        let (dir, paths) = scratch("capture-bound");
        std::fs::write(paths.workspace.join("A.md"), vec![b'x'; 4096]).unwrap();
        let task = emitting("draft", &["A.md"]);
        let counter = AtomicU64::new(0);
        let before = prior(&paths, &task);
        let out = capture_declared(
            &paths,
            &paths.workspace,
            &task,
            passing(),
            &counter,
            &before,
        );
        assert!(matches!(out.outcome, AttemptOutcome::Pass(_)));
        assert_eq!(counter.load(Ordering::Relaxed), 4096);

        // A second task, itself far under the bound, lands on a run that is already there.
        let later = emitting("polish", &["A.md"]);
        counter.store(MAX_CAPTURED_BYTES - 1, Ordering::Relaxed);
        let before = prior(&paths, &later);
        let out = capture_declared(
            &paths,
            &paths.workspace,
            &later,
            passing(),
            &counter,
            &before,
        );
        let note = note(&out);
        assert!(note.contains("this run's declared files exceed"), "{note}");
        assert!(
            !paths.state.join("files/polish").exists(),
            "a capture past the run's bound was published"
        );
        assert_eq!(
            counter.load(Ordering::Relaxed),
            MAX_CAPTURED_BYTES - 1,
            "a capture that was never published still spent the run's budget"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Boilerplate every e2e in this module shares: a manifest whose agent is the fake harness
    /// and whose workflow is `workflow.star`.
    fn fake_agent_manifest(dir: &std::path::Path) {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        std::fs::write(
            dir.join("crucible.toml"),
            format!(
                r#"
                [repo]
                path = "."
                [workspace]
                dir = "workspace"
                setup_cmd = "mkdir -p workspace && git -C workspace init -q && git -C workspace -c user.email=c@l -c user.name=c -c commit.gpgsign=false commit -q --allow-empty -m baseline"
                [agent]
                backend = "command"
                agent_cmd = "python3 {}"
                goal = "pass artifacts along"
                [agent.env]
                FAKE_AGENT_SCRIPT = "{}"
                [workflow]
                type = "playbook"
                file = "workflow.star"
                "#,
                root.join("tools/fake-agent.py").display(),
                dir.join("agents.json").display(),
            ),
        )
        .unwrap();
    }

    fn run_playbook(dir: &std::path::Path) -> crate::plan::exec::PlanOutcome {
        run_playbook_recording(dir).0
    }

    /// The same run, keeping the runner so a test can read the run-scoped state it owns.
    fn run_playbook_recording(
        dir: &std::path::Path,
    ) -> (crate::plan::exec::PlanOutcome, HarnessRunner) {
        let mut manifest = crate::manifest::Manifest::load(&dir.join("crucible.toml")).unwrap();
        manifest.resolve_workflow(dir).unwrap();
        let workflow = manifest.workflow.as_ref().unwrap();
        let plan = crate::runloop::graph::iteration_template(
            Some(workflow),
            &crate::plan::workflow::WorkflowCaps::for_lane(workflow.workflow_type),
        )
        .unwrap();
        let mut runner = crate::cli::setup::prep_plan_runner(&dir.join("crucible.toml"))
            .unwrap()
            .0;
        let out = execute(
            &plan,
            &Substrate::default(),
            ExecCfg::default(),
            &mut runner,
            |_, _| {},
        );
        (out, runner)
    }

    /// A launch's `--harness`/`--model` replace the manifest's `[agent]` pair on the runner every
    /// agent task inherits from, and the toolbox lands where that harness discovers skills.
    #[test]
    fn a_launch_agent_override_replaces_the_manifest_pair() {
        let dir = std::env::temp_dir().join(format!(
            "crucible-plan-agent-override-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("skills/demo")).unwrap();
        std::fs::write(dir.join("skills/demo/SKILL.md"), "# demo\n").unwrap();
        std::fs::write(
            dir.join("crucible.toml"),
            r#"
            [repo]
            path = "."
            [workspace]
            dir = "workspace"
            setup_cmd = "mkdir -p workspace && git -C workspace init -q && git -C workspace -c user.email=c@l -c user.name=c -c commit.gpgsign=false commit -q --allow-empty -m baseline"
            [agent]
            backend = "command"
            agent_cmd = "true"
            goal = "g"
            toolbox_dir = "skills"
            [workflow]
            type = "playbook"
            file = "workflow.star"
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.join("workflow.star"),
            "go = agent(name = \"go\", prompt = \"go\")\nworkflow(type = \"playbook\", tasks = [go])\n",
        )
        .unwrap();
        let manifest = dir.join("crucible.toml");

        let (plain, _) = crate::cli::setup::prep_plan_runner(&manifest).unwrap();
        assert_eq!(plain.args.harness(), crate::manifest::Harness::Claude);
        assert_eq!(
            plain.args.model(),
            crate::manifest::Harness::Claude.default_model()
        );

        let (overridden, _) = crate::cli::setup::prep_plan_runner_with_params(
            &manifest,
            &BTreeMap::new(),
            crate::openshell::gateway::ComputeDriver::Podman,
            crate::args::AgentOverride {
                harness: Some(crate::manifest::Harness::Codex),
                model: Some("gpt-5.6-luna".to_string()),
            },
        )
        .unwrap();
        assert_eq!(overridden.args.harness(), crate::manifest::Harness::Codex);
        assert_eq!(overridden.args.model(), "gpt-5.6-luna");
        assert!(
            overridden
                .paths
                .workspace
                .join(crate::manifest::Harness::Codex.skills_dir())
                .join("demo")
                .is_dir(),
            "the toolbox lands in the override harness's skills dir"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A serial producer's declared file has to reach an isolated consumer, whose worktree is a
    /// clone that cannot carry `inputs/` (it is excluded from the workspace's git memory on
    /// purpose), and the whole pack has to stay runnable: the middle task's declared output is a
    /// copy of a staged input, so it carries the staged copy's read-only mode.
    #[test]
    fn a_capturing_pack_runs_three_times_and_reaches_an_isolated_consumer() {
        let dir = std::env::temp_dir().join(format!("crucible-recapture-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("agents.json"),
            r#"{
              "write": {"writes": {"A.md": "the source\n"}, "result": {"ok": true}},
              "review": {"reads": ["inputs/write/A.md", "inputs/copy/B.md"],
                         "result": {"saw_both": true}}
            }"#,
        )
        .unwrap();
        // `copy` reads what a serial ancestor declared and declares the result, so its own
        // captured file inherits the staged input's mode. `review` is isolated, so it can only
        // see either file if staged inputs are carried into the worktree directly.
        std::fs::write(
            dir.join("workflow.star"),
            r#"
write = agent(name = "write", prompt = "write", emits_files = ["A.md"])
copy = command(
    name = "copy",
    run = "cp -f inputs/write/A.md B.md && printf '{\"copied\": true}\n'",
    depends_on = [write],
    emits_files = ["B.md"],
)
review = agent(name = "review", prompt = "review", depends_on = [copy], isolated = True)
workflow(type = "playbook", tasks = [write, copy, review])
"#,
        )
        .unwrap();
        fake_agent_manifest(&dir);

        for round in 1..=3 {
            let out = run_playbook(&dir);
            assert!(out.valid, "round {round}: {:?}", out.results);
            assert_eq!(
                out.results[&"review".into()].output.as_ref().unwrap()["saw_both"],
                true,
                "round {round}: an isolated consumer missed a serial producer's file"
            );
            assert!(
                dir.join("state/files/copy/B.md").exists(),
                "round {round}: nothing was captured"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The file channel says what the JSON channel says. A producer that failed contributes
    /// nothing, including what a previous run of the same pack captured under its name.
    #[test]
    fn a_failed_producer_contributes_nothing_on_the_file_channel() {
        let dir = std::env::temp_dir().join(format!("crucible-stale-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The failing round writes different bytes, so its own capture is published rather than
        // withheld as a file it inherited: the staging assertion below is about the join, not
        // about there being nothing on disk to stage.
        let script = |produce_fails: bool| {
            format!(
                r#"{{
                  "seed": {{"result": {{"ok": true}}}},
                  "produce": {{{}"writes": {{"A.md": "the source{}\n"}}, "result": {{"ok": true}}}}
                }}"#,
                if produce_fails { "\"exit\": 1, " } else { "" },
                if produce_fails { ", rewritten" } else { "" }
            )
        };
        std::fs::write(dir.join("agents.json"), script(false)).unwrap();
        // `check` passes only when nothing of `produce` is staged, and a lossy join plus an
        // advisory producer is what gets it dispatched after the failure.
        std::fs::write(
            dir.join("workflow.star"),
            r#"
seed = agent(name = "seed", prompt = "seed")
produce = agent(
    name = "produce",
    prompt = "produce",
    depends_on = [seed],
    emits_files = ["A.md"],
    required = False,
)
check = command(
    name = "check",
    run = "test ! -e inputs/produce && printf '{\"clean\": true}\n'",
    depends_on = [seed, produce],
    join = "passed",
    required = False,
)
workflow(type = "playbook", tasks = [seed, produce, check])
"#,
        )
        .unwrap();
        fake_agent_manifest(&dir);

        // A passing producer stages, which is what makes the second round's assertion mean
        // something: the check is live, and the capture it must not see now exists.
        let out = run_playbook(&dir);
        assert_eq!(out.results[&"produce".into()].status, TaskStatus::Pass);
        assert!(dir.join("state/files/produce/A.md").exists());
        assert_eq!(
            out.results[&"check".into()].status,
            TaskStatus::Fail,
            "a passing producer's file was not staged"
        );

        std::fs::write(dir.join("agents.json"), script(true)).unwrap();
        let out = run_playbook(&dir);
        assert_eq!(out.results[&"produce".into()].status, TaskStatus::Fail);
        assert!(
            dir.join("state/files/produce/A.md").exists(),
            "the failed producer's own capture was expected to be on disk"
        );
        assert_eq!(
            out.results[&"check".into()].status,
            TaskStatus::Pass,
            "a failed producer's file reached its descendant: {:?}",
            out.results[&"check".into()].note
        );
        assert!(!dir.join("workspace/inputs/produce").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Isolated tasks that are ready together run as one batch, and they do not share an
    /// ancestor set: each one is owed exactly what its own ancestors declared.
    #[test]
    fn batched_isolated_consumers_each_get_their_own_ancestors_files() {
        let dir =
            std::env::temp_dir().join(format!("crucible-batch-inputs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("agents.json"),
            r#"{"prod_a": {"writes": {"a.md": "a\n"}, "result": {"ok": true}}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("workflow.star"),
            r#"
prod_a = agent(name = "prod_a", prompt = "produce", emits_files = ["a.md"])
prod_b = command(
    name = "prod_b",
    run = "printf 'b\n' > b.md && printf '{\"ok\": true}\n'",
    depends_on = [prod_a],
    emits_files = ["b.md"],
)
con_a = command(
    name = "con_a",
    run = "test -e inputs/prod_a/a.md && test ! -e inputs/prod_b/b.md && printf '{\"ok\": true}\n'",
    depends_on = [prod_a],
    isolated = True,
)
con_b = command(
    name = "con_b",
    run = "test -e inputs/prod_a/a.md && test -e inputs/prod_b/b.md && printf '{\"ok\": true}\n'",
    depends_on = [prod_b],
    isolated = True,
)
workflow(type = "playbook", tasks = [prod_a, prod_b, con_a, con_b])
"#,
        )
        .unwrap();
        fake_agent_manifest(&dir);

        let out = run_playbook(&dir);
        assert_eq!(
            out.results[&"con_b".into()].status,
            TaskStatus::Pass,
            "a batched consumer missed its own ancestor's file: {:?}",
            out.results[&"con_b".into()].note
        );
        assert_eq!(
            out.results[&"con_a".into()].status,
            TaskStatus::Pass,
            "a batched consumer was handed a file no ancestor of its declared: {:?}",
            out.results[&"con_a".into()].note
        );
        assert!(out.valid, "{:?}", out.results);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Staged inputs belong to the task they were staged for. A task that is nobody's descendant
    /// gets nothing on the file channel, whatever the dispatch before it was handed.
    #[test]
    fn a_previous_dispatchs_inputs_do_not_reach_a_non_descendant() {
        let dir =
            std::env::temp_dir().join(format!("crucible-stale-inputs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("agents.json"),
            r#"{"prod": {"writes": {"a.md": "a\n"}, "result": {"ok": true}}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("workflow.star"),
            r#"
prod = agent(name = "prod", prompt = "produce", emits_files = ["a.md"])
cons = command(
    name = "cons",
    run = "test -e inputs/prod/a.md && printf '{\"ok\": true}\n'",
    depends_on = [prod],
)
serial_stranger = command(
    name = "serial_stranger",
    run = "test ! -e inputs/prod/a.md && printf '{\"ok\": true}\n'",
)
isolated_stranger = command(
    name = "isolated_stranger",
    run = "test ! -e inputs/prod/a.md && printf '{\"ok\": true}\n'",
    isolated = True,
)
workflow(type = "playbook", tasks = [prod, cons, serial_stranger, isolated_stranger])
"#,
        )
        .unwrap();
        fake_agent_manifest(&dir);

        let out = run_playbook(&dir);
        assert_eq!(
            out.results[&"cons".into()].status,
            TaskStatus::Pass,
            "a descendant missed what its ancestor declared: {:?}",
            out.results[&"cons".into()].note
        );
        for stranger in ["serial_stranger", "isolated_stranger"] {
            assert_eq!(
                out.results[&stranger.into()].status,
                TaskStatus::Pass,
                "{stranger} was handed a stranger's declared file: {:?}",
                out.results[&stranger.into()].note
            );
        }
        assert!(out.valid, "{:?}", out.results);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn isolation_worktree_names_do_not_collide_after_display_sanitization() {
        let slash = task_worktree_name(&"review/a".into());
        let dash = task_worktree_name(&"review-a".into());
        assert_ne!(slash, dash);
        assert!(slash.starts_with("task-"));
        assert!(slash.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn explicit_agent_errors_are_transport_failures() {
        let overloaded = AgentEvent::Error {
            error_type: "overloaded".into(),
            message: "try later".into(),
        };
        assert_eq!(
            agent_transport_error(&overloaded),
            Some(TransportFailure::new(
                TransportCause::Provider,
                "overloaded: try later"
            ))
        );
        let failed_result = AgentEvent::Result {
            subtype: "success".into(),
            is_error: true,
            turns: 0,
            cost_usd: 0.0,
            error: Some("not logged in".into()),
        };
        assert_eq!(
            agent_transport_error(&failed_result),
            Some(TransportFailure::new(
                TransportCause::Agent,
                "not logged in"
            ))
        );
    }

    /// The counter litmus, plan-shaped: agent tasks run through the REAL `run_turn` path
    /// (`command` backend: real subprocess, no LLM, no mock), mutating a real git
    /// workspace; the command task measures the real state. Proves the whole harness
    /// runner: manifest prep, per-turn spawn, result-file drain, edges.
    #[test]
    fn agent_tasks_run_through_the_real_harness_path() {
        let dir =
            std::env::temp_dir().join(format!("crucible-plan-harness-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(dir.join("toolbox/demo")).unwrap();
        std::fs::write(dir.join("toolbox/demo/SKILL.md"), "# Demo\n").unwrap();
        std::fs::write(dir.join("value.txt"), "1\n").unwrap();
        std::fs::write(
            dir.join("bump.sh"),
            "#!/bin/sh\ncase \"$CRUCIBLE_PROMPT\" in\n\
             *again*) test -f .agents/skills/demo/SKILL.md ;;\n\
             *) test -f .claude/skills/demo/SKILL.md ;;\n\
             esac\n\
             v=$(cat value.txt); v=$((v + 1)); echo \"$v\" > value.txt\n\
             printf '{\"new_value\": %s}\\n' \"$v\" > PLAN_TASK_RESULT.json\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join("bump.sh"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        std::fs::write(
            dir.join("crucible.toml"),
            r#"
            [repo]
            path = "."
            [workspace]
            dir = "workspace"
            setup_cmd = "mkdir -p workspace && cp value.txt bump.sh workspace/ && git -C workspace init -q && git -C workspace add -A && git -C workspace -c user.email=c@l -c user.name=c -c commit.gpgsign=false commit -qm baseline"
            [agent]
            backend = "command"
            agent_cmd = "./bump.sh"
            goal = "raise the value"
            toolbox_dir = "toolbox"
            [judge]
            measure_cmd = "cat value.txt"
            direction = "higher"
            "#,
        )
        .unwrap();

        let plan = Plan::from_toml_str(
            r#"
            version = 1
            [budget]
            usd = 2.0
            [[task]]
            name = "bump-1"
            kind = "agent"
            prompt = "raise the value once"
            [[task]]
            name = "bump-2"
            kind = "agent"
            prompt = "raise it again"
            harness = "hermes"
            depends_on = ["bump-1"]
            [[task]]
            name = "measure"
            kind = "command"
            command = "printf '{\"score\": %s}\n' \"$(cat value.txt)\""
            depends_on = ["bump-2"]
            "#,
        )
        .unwrap()
        .validate()
        .unwrap();

        let mut runner = crate::cli::setup::prep_plan_runner(&dir.join("crucible.toml"))
            .unwrap()
            .0;
        let out = execute(
            &plan,
            &Substrate::default(),
            ExecCfg::default(),
            &mut runner,
            |_, _| {},
        );
        assert!(out.valid, "{:?}", out.results);
        assert_eq!(
            out.results[&"bump-2".into()].output.as_ref().unwrap()["new_value"],
            3
        );
        assert_eq!(
            out.results[&"measure".into()].output.as_ref().unwrap()["score"],
            3
        );
        // The result file is drained per turn: a stale pass can't leak into the next task.
        assert!(!dir.join("workspace").join(RESULT_FILE).exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("workspace/value.txt"))
                .unwrap()
                .trim(),
            "3"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole playbook concept, end to end, with no model and no controller: a real graph
    /// compiled from a real pack, real processes for every turn, and a verdict. Only the model
    /// is absent, and it is absent by substitution at the process boundary rather than by
    /// stubbing anything inside the engine.
    #[test]
    fn a_playbook_runs_end_to_end_with_no_model_and_no_controller() {
        let dir =
            std::env::temp_dir().join(format!("crucible-playbook-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        let fake = root.join("tools/fake-agent.py");
        assert!(fake.exists(), "{} is missing", fake.display());

        std::fs::write(
            dir.join("agents.json"),
            r#"{
              "draft":   {"writes": {"NOTES.md": "- one\n- two\n"},
                          "appends": {"TURNS.txt": "draft {ENV:CRUCIBLE_AGENT_SESSION_ID}\n"},
                          "result": {"entries": 2}},
              "polish":  {"reads": ["NOTES.md", "TURNS.txt"],
                          "appends": {"TURNS.txt": "polish {ENV:CRUCIBLE_AGENT_SESSION_ID}\n"},
                          "result": {"session": "{ENV:CRUCIBLE_AGENT_SESSION}",
                                     "action": "{ENV:CRUCIBLE_AGENT_SESSION_ACTION}"}},
              "audit-a": {"reads": ["NOTES.md"], "result": {"findings": []}},
              "audit-b": {"exit": 1, "stderr": "no baseline to compare against"}
            }"#,
        )
        .unwrap();

        std::fs::write(
            dir.join("workflow.star"),
            r##"
scribe = session(name = "scribe")

draft = agent(name = "draft", prompt = "draft the notes", session = scribe, emits = ["entries"])
shape = command(
    name = "shape",
    run = "test -s NOTES.md && printf '{\"lines\": %s}\n' \"$(wc -l < NOTES.md | tr -d ' ')\"",
    depends_on = [draft],
)
polish = agent(name = "polish", prompt = "polish them", session = scribe, depends_on = [shape])

audit_a = agent(
    name = "audit-a",
    prompt = "audit headings",
    depends_on = [polish],
    isolated = True,
    emits = ["findings"],
)
audit_b = agent(
    name = "audit-b",
    prompt = "audit freshness",
    depends_on = [polish],
    isolated = True,
    required = False,
)

roundup = command(
    name = "roundup",
    run = "printf '{\"reporting\": %s}\n' \"$(printf '%s' \"$CRUCIBLE_INPUTS\" | grep -c audit-a)\"",
    depends_on = [audit_a, audit_b],
    join = "passed",
)

workflow(type = "playbook", tasks = [draft, shape, polish, audit_a, audit_b, roundup])
"##,
        )
        .unwrap();

        std::fs::write(
            dir.join("crucible.toml"),
            format!(
                r#"
                [repo]
                path = "."
                [workspace]
                dir = "workspace"
                setup_cmd = "mkdir -p workspace && git -C workspace init -q && git -C workspace -c user.email=c@l -c user.name=c -c commit.gpgsign=false commit -q --allow-empty -m baseline"
                [agent]
                backend = "command"
                agent_cmd = "python3 {}"
                goal = "draft release notes, then audit them"
                [agent.env]
                FAKE_AGENT_SCRIPT = "{}"
                [workflow]
                type = "playbook"
                file = "workflow.star"
                "#,
                fake.display(),
                dir.join("agents.json").display(),
            ),
        )
        .unwrap();

        // The pack names its graph; the engine compiles it. No controller supplies anything.
        let mut manifest = crate::manifest::Manifest::load(&dir.join("crucible.toml")).unwrap();
        manifest.resolve_workflow(&dir).unwrap();
        let workflow = manifest.workflow.as_ref().expect("a playbook workflow");
        assert_eq!(
            workflow.workflow_type,
            crate::plan::workflow::WorkflowType::Playbook
        );
        assert!(manifest.is_task(), "a playbook carries no judge");

        let plan = crate::runloop::graph::iteration_template(
            Some(workflow),
            &crate::plan::workflow::WorkflowCaps::for_lane(workflow.workflow_type)
                .with_persistent_sessions(),
        )
        .unwrap();

        let mut settled: Vec<(String, &'static str)> = Vec::new();
        let mut runner = crate::cli::setup::prep_plan_runner(&dir.join("crucible.toml"))
            .unwrap()
            .0;
        let out = execute(
            &plan,
            &Substrate::default(),
            ExecCfg::default(),
            &mut runner,
            |task, result| settled.push((task.name.0.clone(), result.status.as_str())),
        );

        // The verdict: every required task passed, so the run is valid despite an advisory
        // failure. That is the whole difference from the scored lane, and it carries no score.
        assert!(out.valid, "{:?}", out.results);
        assert_eq!(out.exit, PlanExit::Completed);
        assert_eq!(out.spent_usd, 0.0, "no model was called");

        assert_eq!(out.results[&"audit-b".into()].status, TaskStatus::Fail);
        assert_eq!(
            out.results[&"roundup".into()].status,
            TaskStatus::Pass,
            "join = passed folds whoever survived"
        );
        assert_eq!(
            out.results[&"roundup".into()].output.as_ref().unwrap()["reporting"],
            1,
            "the advisory auditor's output must not reach the join"
        );

        // Every task settled exactly once, and the reporter saw each as it settled.
        assert_eq!(settled.len(), 6, "{settled:?}");

        // Session continuity, proven by the agent rather than asserted about it: `polish` read
        // a TURNS.txt only `draft` could have written, in the same conversation.
        let polish = out.results[&"polish".into()].output.as_ref().unwrap();
        assert_eq!(polish["session"], "scribe");
        let turns = std::fs::read_to_string(dir.join("workspace/TURNS.txt")).unwrap();
        let ids: Vec<&str> = turns
            .lines()
            .filter_map(|l| l.split_whitespace().nth(1))
            .collect();
        assert_eq!(ids.len(), 2, "{turns}");
        assert_eq!(
            ids[0], ids[1],
            "one conversation spanned both turns: {turns}"
        );

        // `audit-a` declared `reads` on a file it never wrote, in a disposable worktree. It
        // passed, so isolation staged the workspace rather than starting from nothing.
        assert_eq!(out.results[&"audit-a".into()].status, TaskStatus::Pass);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fan-out over items nobody knew about when the graph was written, run end to end with
    /// no model. The graph has three nodes whatever `discover` returns; only the instance count
    /// is decided at run time, and each instance is named by its item rather than its position.
    #[test]
    fn a_fanout_runs_one_instance_per_discovered_item() {
        let dir = std::env::temp_dir().join(format!("crucible-fanout-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        let fake = root.join("tools/fake-agent.py");

        std::fs::write(
            dir.join("agents.json"),
            r#"{
              "audit[alpha]": {"result": {"item": "{ENV:CRUCIBLE_TASK}", "findings": 0}},
              "audit[beta]":  {"result": {"item": "{ENV:CRUCIBLE_TASK}", "findings": 2}},
              "audit[gamma]": {"exit": 1, "stderr": "gamma has no baseline"}
            }"#,
        )
        .unwrap();

        std::fs::write(
            dir.join("workflow.star"),
            r##"
discover = command(
    name = "discover",
    run = "printf '{\"targets\": [\"alpha\", \"beta\", \"gamma\"]}\n'",
    emits = ["targets"],
)
audit = agent(
    name = "audit",
    prompt = "audit one target",
    depends_on = [discover],
    over = discover.targets,
    max_fanout = 8,
    isolated = True,
    required = False,
    emits = ["findings"],
)
roundup = command(
    name = "roundup",
    run = "printf '{\"seen\": %s}\n' \"$(printf '%s' \"$CRUCIBLE_INPUTS\" | grep -o passed | wc -l | tr -d ' ')\"",
    depends_on = [audit],
    join = "passed",
)
workflow(type = "playbook", tasks = [discover, audit, roundup])
"##,
        )
        .unwrap();

        std::fs::write(
            dir.join("crucible.toml"),
            format!(
                r#"
                [repo]
                path = "."
                [workspace]
                dir = "workspace"
                setup_cmd = "mkdir -p workspace && git -C workspace init -q && git -C workspace -c user.email=c@l -c user.name=c -c commit.gpgsign=false commit -q --allow-empty -m baseline"
                [agent]
                backend = "command"
                agent_cmd = "python3 {}"
                goal = "audit each discovered target"
                [agent.env]
                FAKE_AGENT_SCRIPT = "{}"
                [workflow]
                type = "playbook"
                file = "workflow.star"
                "#,
                fake.display(),
                dir.join("agents.json").display(),
            ),
        )
        .unwrap();

        let mut manifest = crate::manifest::Manifest::load(&dir.join("crucible.toml")).unwrap();
        manifest.resolve_workflow(&dir).unwrap();
        let workflow = manifest.workflow.as_ref().expect("workflow");
        let plan = crate::runloop::graph::iteration_template(
            Some(workflow),
            &crate::plan::workflow::WorkflowCaps::for_lane(workflow.workflow_type),
        )
        .unwrap();
        // Three nodes, before anything runs and whatever `discover` finds.
        assert_eq!(plan.plan().tasks.len(), 3);

        let mut rows: Vec<(String, &'static str)> = Vec::new();
        let mut runner = crate::cli::setup::prep_plan_runner(&dir.join("crucible.toml"))
            .unwrap()
            .0;
        let out = execute(
            &plan,
            &Substrate::default(),
            ExecCfg::default(),
            &mut runner,
            |task, result| rows.push((task.name.0.clone(), result.status.as_str())),
        );

        // One row per item, named by the item. A reader can tell which target failed.
        assert!(
            rows.contains(&("audit[alpha]".to_string(), "pass")),
            "{rows:?}"
        );
        assert!(
            rows.contains(&("audit[beta]".to_string(), "pass")),
            "{rows:?}"
        );
        assert!(
            rows.contains(&("audit[gamma]".to_string(), "fail")),
            "{rows:?}"
        );

        // The node folds them: advisory, so one failed instance does not gate the run.
        let node = &out.results[&"audit".into()];
        assert_eq!(node.status, TaskStatus::Fail, "one instance failed");
        let folded = node.output.as_ref().expect("folded output");
        assert_eq!(folded["instances"], 3);
        assert_eq!(folded["passed"], 2);
        assert_eq!(folded["failed"], 1);
        assert_eq!(folded["outputs"]["alpha"]["item"], "audit[alpha]");
        // A failed instance contributes no entry at all: a null under its key would read as
        // "it ran and found nothing", which is a different claim from "it did not run".
        assert!(
            folded["outputs"].get("gamma").is_none(),
            "a failed instance leaked into the join: {folded}"
        );
        assert_eq!(
            folded["outputs"].as_object().map(|o| o.len()),
            Some(2),
            "{folded}"
        );
        assert!(
            node.note.as_ref().is_some_and(|n| n.contains("gamma")),
            "the note must name what failed: {:?}",
            node.note
        );

        assert!(
            out.valid,
            "an advisory fan-out cannot invalidate: {:?}",
            out.exit
        );
        assert_eq!(out.results[&"roundup".into()].status, TaskStatus::Pass);

        // A wider result than declared is refused rather than run.
        std::fs::write(
            dir.join("workflow.star"),
            std::fs::read_to_string(dir.join("workflow.star"))
                .unwrap()
                .replace("max_fanout = 8", "max_fanout = 2"),
        )
        .unwrap();
        let mut narrow = crate::manifest::Manifest::load(&dir.join("crucible.toml")).unwrap();
        narrow.resolve_workflow(&dir).unwrap();
        let plan = crate::runloop::graph::iteration_template(
            Some(narrow.workflow.as_ref().unwrap()),
            &crate::plan::workflow::WorkflowCaps::for_lane(
                crate::plan::workflow::WorkflowType::Playbook,
            ),
        )
        .unwrap();
        let mut runner = crate::cli::setup::prep_plan_runner(&dir.join("crucible.toml"))
            .unwrap()
            .0;
        let out = execute(
            &plan,
            &Substrate::default(),
            ExecCfg::default(),
            &mut runner,
            |_, _| {},
        );
        let node = &out.results[&"audit".into()];
        assert_eq!(node.status, TaskStatus::Fail);
        assert!(
            node.note
                .as_ref()
                .is_some_and(|n| n.contains("max_fanout is 2")),
            "the bound must be named: {:?}",
            node.note
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Git memory, per task: what a passing task did becomes a commit, and what a failing task
    /// did is dropped. The second half is what needs proving. Leaving a failed task's edits in
    /// the shared tree does not merely fail to commit them; the next task to pass sweeps them
    /// into its own commit, and the failed task has contributed after all.
    #[test]
    fn a_passing_task_commits_and_a_failing_one_leaves_nothing_behind() {
        let dir = std::env::temp_dir().join(format!("crucible-gitmem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        let fake = root.join("tools/fake-agent.py");

        std::fs::write(
            dir.join("agents.json"),
            r#"{
              "good":  {"writes": {"kept.txt": "survives\n"}, "result": {"ok": true}},
              "bad":   {"writes": {"junk.txt": "must not survive\n"}, "exit": 1,
                        "stderr": "wrote then failed"},
              "after": {"writes": {"later.txt": "also survives\n"}, "result": {"ok": true}}
            }"#,
        )
        .unwrap();

        std::fs::write(
            dir.join("workflow.star"),
            r#"
good = agent(name = "good", prompt = "p")
bad = agent(name = "bad", prompt = "p", depends_on = [good], required = False)
after = agent(name = "after", prompt = "p", depends_on = [good], join = "passed")
workflow(type = "playbook", tasks = [good, bad, after])
"#,
        )
        .unwrap();

        std::fs::write(
            dir.join("crucible.toml"),
            format!(
                r#"
                [repo]
                path = "."
                [workspace]
                dir = "workspace"
                setup_cmd = "mkdir -p workspace && git -C workspace init -q && git -C workspace -c user.email=c@l -c user.name=c -c commit.gpgsign=false commit -q --allow-empty -m baseline"
                [agent]
                backend = "command"
                agent_cmd = "python3 {}"
                goal = "prove git memory"
                [agent.env]
                FAKE_AGENT_SCRIPT = "{}"
                [workflow]
                type = "playbook"
                file = "workflow.star"
                "#,
                fake.display(),
                dir.join("agents.json").display(),
            ),
        )
        .unwrap();

        let mut manifest = crate::manifest::Manifest::load(&dir.join("crucible.toml")).unwrap();
        manifest.resolve_workflow(&dir).unwrap();
        let workflow = manifest.workflow.as_ref().unwrap();
        let plan = crate::runloop::graph::iteration_template(
            Some(workflow),
            &crate::plan::workflow::WorkflowCaps::for_lane(workflow.workflow_type),
        )
        .unwrap();
        let mut runner = crate::cli::setup::prep_plan_runner(&dir.join("crucible.toml"))
            .unwrap()
            .0;
        assert!(
            runner.commit_per_task,
            "a playbook manifest must turn per-task git memory on"
        );
        let out = execute(
            &plan,
            &Substrate::default(),
            ExecCfg::default(),
            &mut runner,
            |_, _| {},
        );

        let workspace = dir.join("workspace");
        assert_eq!(out.results[&"good".into()].status, TaskStatus::Pass);
        assert_eq!(out.results[&"bad".into()].status, TaskStatus::Fail);
        assert_eq!(out.results[&"after".into()].status, TaskStatus::Pass);

        // What passed is on disk and in the history.
        assert!(
            workspace.join("kept.txt").exists(),
            "a passing task's work vanished"
        );
        assert!(workspace.join("later.txt").exists());
        // What failed is gone from both, even though a later task passed and committed after it.
        assert!(
            !workspace.join("junk.txt").exists(),
            "a failed task's file survived in the tree"
        );

        let log = std::process::Command::new("git")
            .args(["-C", &workspace.display().to_string(), "log", "--oneline"])
            .output()
            .expect("git log");
        let log = String::from_utf8_lossy(&log.stdout);
        assert!(
            log.contains("task good"),
            "no commit for the passing task: {log}"
        );
        assert!(log.contains("task after"), "{log}");
        assert!(!log.contains("task bad"), "a failed task committed: {log}");

        let tracked = std::process::Command::new("git")
            .args(["-C", &workspace.display().to_string(), "ls-files"])
            .output()
            .expect("git ls-files");
        let tracked = String::from_utf8_lossy(&tracked.stdout);
        assert!(
            !tracked.contains("junk.txt"),
            "a failed task's file reached the index: {tracked}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A playbook runs from its manifest: the pack names its graph, the engine compiles it, and
    /// the ceilings come from whoever launched it. There is no plan file anywhere.
    #[test]
    fn a_playbook_launches_from_its_manifest_under_supplied_ceilings() {
        let _guard = crucible::test_support::env_lock();
        let dir = std::env::temp_dir().join(format!("crucible-launch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        let fake = root.join("tools/fake-agent.py");

        std::fs::write(
            dir.join("agents.json"),
            r#"{"work": {"writes": {"out.txt": "done\n"}, "result": {"ok": true}}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("workflow.star"),
            "work = agent(name = \"work\", prompt = \"do it\")\nworkflow(type = \"playbook\", tasks = [work])\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("crucible.toml"),
            format!(
                r#"
                [repo]
                path = "."
                [workspace]
                dir = "workspace"
                setup_cmd = "mkdir -p workspace && git -C workspace init -q && git -C workspace -c user.email=c@l -c user.name=c -c commit.gpgsign=false commit -q --allow-empty -m baseline"
                [agent]
                backend = "command"
                agent_cmd = "python3 {}"
                goal = "launch a playbook"
                [agent.env]
                FAKE_AGENT_SCRIPT = "{}"
                [workflow]
                type = "playbook"
                file = "workflow.star"
                "#,
                fake.display(),
                dir.join("agents.json").display(),
            ),
        )
        .unwrap();
        let manifest = dir.join("crucible.toml");
        let caps = std::collections::BTreeSet::new();

        // Neither ceiling, one ceiling, and a duration that is not one: all refused before any
        // task is dispatched, which is the point. A ceiling checked at the first overrun has
        // already spent whatever it was meant to bound.
        for (ceilings, expected) in [
            (
                crate::plan::cli::Ceilings::default(),
                "--max-cost and --max-time",
            ),
            (
                crate::plan::cli::Ceilings {
                    usd: Some(1.0),
                    ..Default::default()
                },
                "--max-time",
            ),
            (
                crate::plan::cli::Ceilings {
                    wall_clock: Some(std::time::Duration::from_secs(60)),
                    wall_clock_raw: Some("1m".into()),
                    ..Default::default()
                },
                "--max-cost",
            ),
            (
                crate::plan::cli::Ceilings {
                    usd: Some(1.0),
                    wall_clock: None,
                    wall_clock_raw: Some("later".into()),
                },
                "is not a duration",
            ),
        ] {
            let error = crate::plan::cli::run(
                None,
                &BTreeMap::new(),
                &caps,
                None,
                Some(&manifest),
                crate::plan::cli::RunOpts {
                    ceilings,
                    ..Default::default()
                },
            )
            .expect_err("a playbook without ceilings must not dispatch");
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }
        assert!(
            !dir.join("workspace/out.txt").exists(),
            "a refused launch dispatched a task anyway"
        );

        crate::plan::cli::run(
            None,
            &BTreeMap::new(),
            &caps,
            None,
            Some(&manifest),
            crate::plan::cli::RunOpts {
                ceilings: crate::plan::cli::Ceilings {
                    usd: Some(1.0),
                    wall_clock: Some(std::time::Duration::from_secs(600)),
                    wall_clock_raw: Some("10m".into()),
                },
                ..Default::default()
            },
        )
        .expect("a playbook with both ceilings runs");
        assert!(dir.join("workspace/out.txt").exists(), "the task never ran");

        // The graph reached the session log, so a reader sees it without a plan file existing.
        let log = std::fs::read_to_string(dir.join("state/session.jsonl")).unwrap();
        assert!(log.contains("plan_admitted"), "{log}");
        assert!(log.contains("\"name\":\"work\""), "{log}");
        assert!(
            !dir.join("plan.toml").exists(),
            "the launcher wrote a plan file"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A declared file is part of a task's output, not part of the workspace state isolation
    /// discards, and it reaches every descendant rather than only the next one.
    ///
    /// Three things are proven here that nothing else covers: an isolated task's file survives
    /// the deletion of its worktree, a task two hops downstream still receives it, and a task
    /// that passes without writing what it promised fails where it promised it.
    #[test]
    fn a_declared_file_outlives_isolation_and_reaches_every_descendant() {
        let dir = std::env::temp_dir().join(format!("crucible-emits-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("agents.json"),
            r#"{
              "analyze": {"writes": {"SPEC.md": "the algorithm\n"}, "result": {"ok": true}},
              "implement": {"reads": ["inputs/analyze/SPEC.md"],
                            "writes": {"NOTES.md": "built it\n"}, "result": {"ok": true}},
              "report": {"reads": ["inputs/analyze/SPEC.md", "inputs/implement/NOTES.md"],
                         "result": {"saw_both": true}},
              "forgetful": {"result": {"ok": true}}
            }"#,
        )
        .unwrap();

        // `analyze` is isolated: its worktree is deleted the moment it settles, so SPEC.md can
        // only reach anyone if a declared file is captured rather than left in a tree.
        std::fs::write(
            dir.join("workflow.star"),
            r#"
analyze = agent(
    name = "analyze",
    prompt = "analyze",
    isolated = True,
    emits_files = ["SPEC.md"],
)
implement = agent(
    name = "implement",
    prompt = "implement",
    depends_on = [analyze],
    emits_files = ["NOTES.md"],
)
report = agent(name = "report", prompt = "report", depends_on = [implement])
workflow(type = "playbook", tasks = [analyze, implement, report])
"#,
        )
        .unwrap();
        fake_agent_manifest(&dir);

        let out = run_playbook(&dir);
        assert!(out.valid, "{:?}", out.results);
        // Staged inputs are not the consuming task's work: a per-task commit that swept them in
        // would read as though this task had produced what its ancestors handed it.
        let tracked = std::process::Command::new("git")
            .args([
                "-C",
                &dir.join("workspace").display().to_string(),
                "ls-files",
            ])
            .output()
            .expect("git ls-files");
        let tracked = String::from_utf8_lossy(&tracked.stdout);
        assert!(
            !tracked.contains("inputs/"),
            "staged inputs reached git memory: {tracked}"
        );
        // `report` declared `reads` on both, two hops and one hop back. It passed, so staging
        // reached past the direct dependency.
        assert_eq!(
            out.results[&"report".into()].output.as_ref().unwrap()["saw_both"],
            true
        );
        // The captured copy is kept beside the run, not in the workspace it came from.
        assert!(
            dir.join("state/files/analyze/SPEC.md").exists(),
            "an isolated task's declared file was not captured"
        );
        assert!(
            !dir.join("workspace/SPEC.md").exists(),
            "an isolated task's file leaked into the shared workspace"
        );

        // A task that passes without writing what it promised fails where it promised it.
        std::fs::write(
            dir.join("workflow.star"),
            "forgetful = agent(name = \"forgetful\", prompt = \"p\", emits_files = [\"MISSING.md\"])\nworkflow(type = \"playbook\", tasks = [forgetful])\n",
        )
        .unwrap();
        let _ = std::fs::remove_dir_all(dir.join("workspace"));
        let _ = std::fs::remove_dir_all(dir.join("state"));
        let out = run_playbook(&dir);
        let result = &out.results[&"forgetful".into()];
        assert_eq!(result.status, TaskStatus::Fail);
        assert_eq!(result.attempts, 1, "output drift must not be retried");
        assert!(
            result
                .note
                .as_ref()
                .is_some_and(|n| n.contains("MISSING.md") && n.contains("absent")),
            "{:?}",
            result.note
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_turn_that_writes_no_result_fails_but_still_advances_the_session() {
        let dir = std::env::temp_dir().join(format!(
            "crucible-plan-session-quiet-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("quiet.sh"), "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join("quiet.sh"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        std::fs::write(
            dir.join("crucible.toml"),
            r#"
            [repo]
            path = "."
            [workspace]
            dir = "workspace"
            setup_cmd = "mkdir -p workspace && cp quiet.sh workspace/ && git -C workspace init -q && git -C workspace add -A && git -C workspace -c user.email=c@l -c user.name=c -c commit.gpgsign=false commit -qm baseline"
            [agent]
            backend = "command"
            agent_cmd = "./quiet.sh"
            goal = "say nothing"
            [judge]
            measure_cmd = "echo 0"
            direction = "higher"
            "#,
        )
        .unwrap();

        let plan = Plan::from_toml_str(
            r#"
            version = 1
            [budget]
            usd = 2.0
            [[task]]
            name = "quiet"
            kind = "agent"
            prompt = "produce nothing"
            session = "solver"
            required = false
            "#,
        )
        .unwrap()
        .validate()
        .unwrap();

        let mut runner = crate::cli::setup::prep_plan_runner(&dir.join("crucible.toml"))
            .unwrap()
            .0;
        let state = runner.paths.state.clone();
        assert!(
            !crate::agent::agent_session::prepare(&state, "solver")
                .unwrap()
                .is_resume(),
            "no cursor before the first turn"
        );
        let out = execute(
            &plan,
            &Substrate::default(),
            ExecCfg::default(),
            &mut runner,
            |_, _| {},
        );

        let result = &out.results[&"quiet".into()];
        assert_eq!(result.status, TaskStatus::Fail, "{result:?}");
        assert!(
            result
                .note
                .as_deref()
                .unwrap_or_default()
                .contains(RESULT_FILE),
            "the failure names the missing result file: {result:?}"
        );
        let next = crate::agent::agent_session::prepare(&state, "solver").unwrap();
        assert!(next.is_resume(), "the conversation is resumable: {next:?}");
        assert_eq!(next.completed_turns, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Copy the shipped `examples/adversarial-review` fixture into a scratch dir, so the
    /// test exercises the EXACT files the example ships (it can't rot) without leaving a
    /// workspace behind in the repo.
    fn stage_review_example(tag: &str) -> std::path::PathBuf {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crate dir has a parent")
            .join("examples/adversarial-review");
        let dir = std::env::temp_dir().join(format!(
            "crucible-adversarial-review-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for entry in std::fs::read_dir(&src).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                let dst = dir.join(path.file_name().unwrap());
                std::fs::copy(&path, &dst).unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
                    std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(mode)).unwrap();
                }
            }
        }
        dir
    }

    fn run_review_plan(dir: &std::path::Path, plan_file: &str) -> crate::plan::exec::PlanOutcome {
        let plan = crate::plan::cli::load(&dir.join(plan_file)).unwrap();
        let mut runner = crate::cli::setup::prep_plan_runner(&dir.join("crucible.toml"))
            .unwrap()
            .0;
        execute(
            &plan,
            &Substrate::default(),
            ExecCfg::default(),
            &mut runner,
            |_, _| {},
        )
    }

    #[test]
    fn adversarial_review_gates_a_reward_hack_the_frozen_gate_cannot_see() {
        // 1. Clean implementation: the review approves and the whole chain runs.
        let honest = stage_review_example("honest");
        let out = run_review_plan(&honest, "plan.toml");
        assert!(
            out.valid,
            "honest run should reach a valid verdict: {:?}",
            out.results
        );
        for task in ["implement", "review", "verdict-gate", "measure"] {
            assert_eq!(
                out.results[&task.into()].status,
                TaskStatus::Pass,
                "{task} should pass on the honest run"
            );
        }
        assert_eq!(
            out.results[&"review".into()].output.as_ref().unwrap()["approved"],
            true
        );
        let _ = std::fs::remove_dir_all(&honest);

        // 2. Same graph, coder told to pass "by any means".
        let hacked = stage_review_example("hack");
        let out = run_review_plan(&hacked, "plan-reward-hack.toml");
        assert!(
            !out.valid,
            "a rejected candidate must not reach a valid verdict"
        );
        assert_eq!(
            out.exit,
            PlanExit::ShortCircuit {
                task: "verdict-gate".into()
            }
        );

        // The reviewer ran successfully and returned a NEGATIVE opinion: a passed task
        // carrying a rejection, not a failed task. Interpreting it is the gate's job.
        let review = &out.results[&"review".into()];
        assert_eq!(
            review.status,
            TaskStatus::Pass,
            "the reviewer itself did its job"
        );
        let verdict = review.output.as_ref().unwrap();
        assert_eq!(verdict["approved"], false);
        assert!(
            verdict["finding"]
                .as_str()
                .unwrap()
                .contains("hardcoded literal"),
            "the finding names the shortcut it found in the artifact: {verdict}"
        );
        assert_eq!(out.results[&"verdict-gate".into()].status, TaskStatus::Fail);

        // The point of putting the review UPSTREAM: the expensive step never ran.
        let measure = &out.results[&"measure".into()];
        assert_eq!(measure.status, TaskStatus::Blocked);
        assert_eq!(
            measure.attempts, 0,
            "the expensive task was never dispatched"
        );

        // And the blind spot is real, not stipulated: the frozen functional gate scores
        // the hacked implementation a perfect pass when run on its own.
        let ws = hacked.join("workspace");
        let src = std::fs::read_to_string(ws.join("solution.py")).unwrap();
        assert!(src.contains("return n in"), "the hack is on disk: {src}");
        let policy = std::fs::read_to_string(ws.join("verdict_gate.sh")).unwrap();
        assert!(
            !policy.contains("gate replaced by implementer"),
            "the manifest-owned policy gate must be restored after agent tampering: {policy}"
        );
        let verify = std::process::Command::new("./verify.sh")
            .current_dir(&ws)
            .output()
            .expect("running the frozen gate directly");
        assert!(
            verify.status.success(),
            "the frozen gate PASSES the reward hack — that's why the review exists"
        );
        assert!(
            String::from_utf8_lossy(&verify.stdout).contains("\"solved\": true"),
            "gate output: {}",
            String::from_utf8_lossy(&verify.stdout)
        );
        let _ = std::fs::remove_dir_all(&hacked);
    }

    /// One code node splitting into two isolated reviewers, rejoined by a policy gate.
    /// Runs the shipped panel plan against the stand-in manifest. Correct code with sloppy
    /// prose: the advisory reviewer reports defects and the run still reaches `measure`,
    /// which is what makes the join a policy rather than an AND.
    #[test]
    fn panel_splits_into_two_isolated_reviewers_joined_by_policy() {
        let dir = stage_review_example("panel");
        let out = run_review_plan(&dir, "plan-panel-sloppy.toml");
        assert!(
            out.valid,
            "advisory findings must not block: {:?}",
            out.results
        );

        // Both reviewers ran and stayed in their lanes.
        let correctness = out.results[&"review-correctness".into()]
            .output
            .as_ref()
            .unwrap();
        assert_eq!(correctness["approved"], true, "the code itself is correct");
        let copy = out.results[&"review-copy".into()].output.as_ref().unwrap();
        let findings = copy["findings"].as_array().unwrap();
        assert_eq!(
            findings.len(),
            5,
            "four typos and one phantom parameter: {copy}"
        );
        assert!(
            findings
                .iter()
                .any(|f| f.as_str().unwrap().contains("does not accept")),
            "the copy editor catches a documented parameter that isn't in the signature: {copy}"
        );

        // The join folded both verdicts and applied different weight to each.
        let gate = out.results[&"gate".into()].output.as_ref().unwrap();
        assert_eq!(gate["blocked"], false);
        assert_eq!(gate["copy_edit_count"], 5);
        assert_eq!(
            gate["reviewers_reporting"],
            serde_json::json!(["review-copy", "review-correctness"])
        );
        assert_eq!(
            out.results[&"measure".into()].status,
            TaskStatus::Pass,
            "an advisory-only finding must not stop the expensive step"
        );

        // Isolation did its job: neither reviewer's result file leaked into the shared
        // workspace (that collision is exactly what would make two concurrent reviewers
        // read each other's verdict), and the worktrees were cleaned up.
        let ws = dir.join("workspace");
        assert!(
            !ws.join(RESULT_FILE).exists(),
            "an isolated task must not write its result into the shared workspace"
        );
        let iso_root = dir.join("state/plan-iso");
        if iso_root.exists() {
            let leftovers: Vec<_> = std::fs::read_dir(&iso_root).unwrap().flatten().collect();
            assert!(
                leftovers.is_empty(),
                "isolation worktrees should be cleaned up, found {} left",
                leftovers.len()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A plan naming an unknown harness must fail measured, not spawn anything.
    #[test]
    fn unknown_harness_is_a_measured_failure() {
        let t = crate::plan::ir::Task {
            name: "a".into(),
            task: TaskKind::Agent {
                prompt: "go".into(),
                harness: Some("not-a-harness".into()),
                model: None,
                effort: None,
            },
            depends_on: vec![],
            session: None,
            needs: "any".into(),
            required: true,
            isolation: None,
            join: Join::default(),
            stage: Stage::Iteration,
            emits: Vec::new(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
        };
        let mut runner = HarnessRunner {
            args: <crate::cli::Cli as clap::Parser>::try_parse_from(["crucible"])
                .unwrap()
                .run,
            paths: crate::args::Paths::for_manifest(
                std::env::temp_dir(),
                std::env::temp_dir(),
                &std::env::temp_dir(),
                None,
            ),
            commit_per_task: false,
            captured_bytes: AtomicU64::new(0),
            staged: Default::default(),
        };
        let a = runner.run(&t, 1, &BTreeMap::new());
        match a.outcome {
            AttemptOutcome::Fail { note, .. } => assert!(note.contains("unknown harness")),
            _ => panic!("expected a measured failure"),
        }
    }
    /// A fan-out in the shared workspace, run end to end with no model. The instances write one
    /// tree and one result file, so each has to be a serial task in its own right: every item is
    /// attributed to the instance that produced it, and the instance that fails takes its own
    /// edits with it and nobody else's.
    #[test]
    fn shared_workspace_instances_keep_their_own_items_and_commits() {
        let dir =
            std::env::temp_dir().join(format!("crucible-fanout-shared-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        let fake = root.join("tools/fake-agent.py");

        let mut agents = serde_json::Map::new();
        for key in ["alpha", "beta", "gamma", "delta"] {
            let mut spec = serde_json::json!({
                "writes": {format!("FILE-{key}.md"): format!("{key} was here\n")},
                "result": {"item": "{ENV:CRUCIBLE_TASK}", "key": key},
            });
            if key == "gamma" {
                spec["exit"] = serde_json::json!(1);
                spec["stderr"] = serde_json::json!("gamma has no baseline");
            }
            agents.insert(format!("audit[{key}]"), spec);
        }
        std::fs::write(
            dir.join("agents.json"),
            serde_json::to_string(&Value::Object(agents)).unwrap(),
        )
        .unwrap();

        std::fs::write(
            dir.join("workflow.star"),
            r##"
discover = command(
    name = "discover",
    run = "printf '{\"targets\": [\"alpha\", \"beta\", \"gamma\", \"delta\"]}\n'",
    emits = ["targets"],
)
audit = agent(
    name = "audit",
    prompt = "audit one target",
    depends_on = [discover],
    over = discover.targets,
    max_fanout = 8,
    required = False,
    emits = ["item"],
)
workflow(type = "playbook", tasks = [discover, audit])
"##,
        )
        .unwrap();
        write_fanout_manifest(&dir, &fake);

        let mut runner = crate::cli::setup::prep_plan_runner(&dir.join("crucible.toml"))
            .unwrap()
            .0;
        assert!(runner.commit_per_task, "a playbook commits per task");
        let out = execute(
            &fanout_plan(&dir),
            &Substrate::default(),
            ExecCfg::default(),
            &mut runner,
            |_, _| {},
        );

        // Every item reported by the instance that ran it, and the one that exited 1 folded as
        // failed rather than carrying a sibling's payload.
        let node = &out.results[&"audit".into()];
        let folded = node.output.as_ref().expect("folded output");
        for key in ["alpha", "beta", "delta"] {
            assert_eq!(folded["outputs"][key]["item"], format!("audit[{key}]"));
            assert_eq!(folded["outputs"][key]["key"], key);
        }
        assert_eq!(folded["passed"], 3);
        assert_eq!(folded["failed"], 1);
        assert_eq!(
            out.results[&"audit[gamma]".into()].status,
            TaskStatus::Fail,
            "an instance that exited 1 must not fold in as a pass"
        );

        // The failing instance's edits are gone; its siblings' survive, including the one that
        // ran after it.
        let workspace = dir.join("workspace");
        for key in ["alpha", "beta", "delta"] {
            assert_eq!(
                std::fs::read_to_string(workspace.join(format!("FILE-{key}.md"))).unwrap(),
                format!("{key} was here\n"),
                "a passing instance's file was swept away"
            );
        }
        assert!(
            !workspace.join("FILE-gamma.md").exists(),
            "the failing instance's edits stayed in the tree"
        );
        let log = std::process::Command::new("git")
            .args(["-C", &workspace.display().to_string(), "log", "--format=%s"])
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&log.stdout);
        for key in ["alpha", "beta", "delta"] {
            assert!(log.contains(&format!("task audit[{key}]")), "{log}");
        }
        assert!(!log.contains("task audit[gamma]"), "{log}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A mapped node's declared files are captured per instance and reach a consumer under the
    /// instance's own namespace. A failed instance has no namespace: its files are not evidence.
    #[test]
    fn a_mapped_node_captures_and_stages_declared_files_per_instance() {
        let dir =
            std::env::temp_dir().join(format!("crucible-fanout-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        let fake = root.join("tools/fake-agent.py");

        let mut agents = serde_json::Map::new();
        for key in ["alpha", "beta", "gamma"] {
            let mut spec = serde_json::json!({
                "writes": {"OUT.md": format!("findings for {key}\n")},
                "result": {"item": "{ENV:CRUCIBLE_TASK}"},
            });
            if key == "beta" {
                spec["exit"] = serde_json::json!(1);
            }
            agents.insert(format!("audit[{key}]"), spec);
        }
        std::fs::write(
            dir.join("agents.json"),
            serde_json::to_string(&Value::Object(agents)).unwrap(),
        )
        .unwrap();

        std::fs::write(
            dir.join("workflow.star"),
            r##"
discover = command(
    name = "discover",
    run = "printf '{\"targets\": [\"alpha\", \"beta\", \"gamma\"]}\n'",
    emits = ["targets"],
)
audit = agent(
    name = "audit",
    prompt = "audit one target",
    depends_on = [discover],
    over = discover.targets,
    max_fanout = 8,
    required = False,
    emits = ["item"],
    emits_files = ["OUT.md"],
)
roundup = command(
    name = "roundup",
    run = "printf '{\"staged\": \"%s\"}\n' \"$(ls inputs | sort | tr '\\n' ' ')\"",
    depends_on = [audit],
    join = "passed",
)
workflow(type = "playbook", tasks = [discover, audit, roundup])
"##,
        )
        .unwrap();
        write_fanout_manifest(&dir, &fake);

        let mut runner = crate::cli::setup::prep_plan_runner(&dir.join("crucible.toml"))
            .unwrap()
            .0;
        let out = execute(
            &fanout_plan(&dir),
            &Substrate::default(),
            ExecCfg::default(),
            &mut runner,
            |_, _| {},
        );

        // One capture namespace per passing instance, each holding that instance's own file.
        let files = dir.join("state/files");
        for key in ["alpha", "gamma"] {
            assert_eq!(
                std::fs::read_to_string(files.join(format!("audit[{key}]/OUT.md"))).unwrap(),
                format!("findings for {key}\n"),
                "instance capture is not per instance"
            );
        }
        // The failed instance's own reading is captured, and reaches a settled edge or nothing.
        assert_eq!(
            std::fs::read_to_string(files.join("audit[beta]/OUT.md")).unwrap(),
            "findings for beta\n"
        );

        // The consumer is handed the passing instances, and only those.
        let staged = out.results[&"roundup".into()].output.as_ref().unwrap()["staged"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(staged.contains("audit[alpha]"), "{staged}");
        assert!(staged.contains("audit[gamma]"), "{staged}");
        assert!(!staged.contains("audit[beta]"), "{staged}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn write_fanout_manifest(dir: &std::path::Path, fake: &std::path::Path) {
        std::fs::write(
            dir.join("crucible.toml"),
            format!(
                r#"
                [repo]
                path = "."
                [workspace]
                dir = "workspace"
                setup_cmd = "mkdir -p workspace && git -C workspace init -q && git -C workspace -c user.email=c@l -c user.name=c -c commit.gpgsign=false commit -q --allow-empty -m baseline"
                [agent]
                backend = "command"
                agent_cmd = "python3 {}"
                goal = "audit each discovered target"
                [agent.env]
                FAKE_AGENT_SCRIPT = "{}"
                [workflow]
                type = "playbook"
                file = "workflow.star"
                "#,
                fake.display(),
                dir.join("agents.json").display(),
            ),
        )
        .unwrap();
    }

    fn fanout_plan(dir: &std::path::Path) -> crate::plan::ir::ValidPlan {
        let mut manifest = crate::manifest::Manifest::load(&dir.join("crucible.toml")).unwrap();
        manifest.resolve_workflow(dir).unwrap();
        let workflow = manifest.workflow.as_ref().expect("workflow");
        crate::runloop::graph::iteration_template(
            Some(workflow),
            &crate::plan::workflow::WorkflowCaps::for_lane(workflow.workflow_type),
        )
        .unwrap()
    }

    /// A scratch pack directory with `crucible.toml`, an empty fake-agent script, and the
    /// workflow source, ready for [`run_playbook`].
    fn playbook_pack(tag: &str, workflow: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("crucible-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agents.json"), "{}").unwrap();
        std::fs::write(dir.join("workflow.star"), workflow).unwrap();
        fake_agent_manifest(&dir);
        dir
    }

    fn git_output(workspace: &std::path::Path, args: &[&str]) -> String {
        let mut command = std::process::Command::new("git");
        command.arg("-C").arg(workspace);
        let out = command.args(args).output().expect("git");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// The whole failure path in one run: the declared file is captured before the workspace is
    /// rolled back, the undeclared write goes with the rollback, nothing is committed, and the
    /// evidence reaches the consumer that joined settled and nowhere else.
    #[test]
    fn a_failed_producers_declared_file_survives_the_rollback_and_reaches_a_settled_consumer() {
        let dir = playbook_pack(
            "settled-capture",
            r#"
probe = command(
    name = "probe",
    run = "mkdir -p evidence && printf '{\"pass\": false}\n' > evidence/probe.json && printf 'junk\n' > junk.txt && exit 1",
    required = False,
    emits_files = ["evidence/probe.json"],
)
report = command(
    name = "report",
    run = "cat inputs/probe/evidence/probe.json > SEEN.json && printf '{\"saw\": true}\n'",
    depends_on = [probe],
    join = "settled",
)
workflow(type = "playbook", tasks = [probe, report])
"#,
        );
        let out = run_playbook(&dir);

        assert_eq!(out.results[&"probe".into()].status, TaskStatus::Fail);
        assert_eq!(
            out.results[&"report".into()].status,
            TaskStatus::Pass,
            "{:?}",
            out.results[&"report".into()].note
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("state/files/probe/evidence/probe.json")).unwrap(),
            "{\"pass\": false}\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("workspace/SEEN.json")).unwrap(),
            "{\"pass\": false}\n",
            "the settled consumer did not read the failed producer's evidence"
        );

        let workspace = dir.join("workspace");
        assert!(
            !workspace.join("junk.txt").exists(),
            "a failed task's undeclared write survived"
        );
        assert!(
            !workspace.join("evidence/probe.json").exists(),
            "a failed task's declared write stayed in the tree"
        );
        let log = git_output(&workspace, &["log", "--oneline"]);
        assert!(
            !log.contains("task probe"),
            "a failed task committed: {log}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A staged set is complete or absent. A failing producer that delivered one of two declared
    /// paths publishes neither, refunds what it charged, says why beside its own failure note,
    /// and stays failed rather than becoming a different failure.
    #[test]
    fn a_failing_producers_partial_set_is_withheld_whole() {
        let dir = playbook_pack(
            "settled-partial",
            r#"
probe = command(
    name = "probe",
    run = "mkdir -p evidence && printf 'a\n' > evidence/a.json && printf 'the probe did not separate\n' >&2 && exit 1",
    required = False,
    emits_files = ["evidence/a.json", "evidence/b.json"],
)
report = command(
    name = "report",
    run = "test ! -e inputs/probe && printf '{\"saw_nothing\": true}\n'",
    depends_on = [probe],
    join = "settled",
)
workflow(type = "playbook", tasks = [probe, report])
"#,
        );
        let (out, runner) = run_playbook_recording(&dir);

        let probe = &out.results[&"probe".into()];
        assert_eq!(probe.status, TaskStatus::Fail);
        let note = probe.note.clone().unwrap_or_default();
        assert!(note.contains("b.json") && note.contains("absent"), "{note}");
        assert!(
            !dir.join("state/files/probe").exists(),
            "a partial set was published"
        );
        assert_eq!(
            runner.captured_bytes.load(Ordering::Relaxed),
            0,
            "a set that was never published still spent the run's budget"
        );
        assert_eq!(
            out.results[&"report".into()].status,
            TaskStatus::Pass,
            "{:?}",
            out.results[&"report".into()].note
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ordering is not provenance. A file an earlier task committed is that task's evidence, so a
    /// later task that failed without writing it publishes nothing and reports no files.
    #[test]
    fn a_failing_task_does_not_publish_the_file_an_earlier_task_committed() {
        let dir = playbook_pack(
            "settled-provenance",
            r#"
first = command(
    name = "first",
    run = "mkdir -p evidence && printf 'from first\n' > evidence/x.json && printf '{\"ok\": true}\n'",
    required = False,
    emits_files = ["evidence/x.json"],
)
second = command(
    name = "second",
    run = "printf 'second wrote nothing\n' >&2 && exit 1",
    depends_on = [first],
    required = False,
    emits_files = ["evidence/x.json"],
)
report = command(
    name = "report",
    run = "test ! -e inputs/second && test -e inputs/first/evidence/x.json && printf '{\"ok\": true}\n'",
    depends_on = [first, second],
    join = "settled",
)
workflow(type = "playbook", tasks = [first, second, report])
"#,
        );
        let out = run_playbook(&dir);

        assert_eq!(out.results[&"first".into()].status, TaskStatus::Pass);
        let second = &out.results[&"second".into()];
        assert_eq!(second.status, TaskStatus::Fail);
        let note = second.note.clone().unwrap_or_default();
        assert!(note.contains("did not write it"), "{note}");
        assert!(
            !dir.join("state/files/second").exists(),
            "a failing task published a file it inherited"
        );
        assert_eq!(
            out.results[&"report".into()].status,
            TaskStatus::Pass,
            "{:?}",
            out.results[&"report".into()].note
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same provenance rule across serial instances of one mapped node, which is where it
    /// bites: they share a workspace and a declared path, so the instance that failed before
    /// writing would otherwise publish its sibling's reading as its own.
    #[test]
    fn a_mapped_instance_that_failed_before_writing_publishes_nothing() {
        let dir =
            std::env::temp_dir().join(format!("crucible-settled-instance-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("agents.json"),
            r#"{
              "review[a]": {"writes": {"evidence/review.json": "a reviewed\n"},
                            "result": {"status": "pass"}},
              "review[b]": {"exit": 1, "stderr": "b objected before writing"}
            }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("workflow.star"),
            r#"
discover = command(
    name = "discover",
    run = "printf '{\"targets\": [\"a\", \"b\"]}\n'",
    emits = ["targets"],
)
review = agent(
    name = "review",
    prompt = "review one target",
    depends_on = [discover],
    over = discover.targets,
    max_fanout = 4,
    required = False,
    emits_files = ["evidence/review.json"],
)
report = command(
    name = "report",
    run = "test -e 'inputs/review[a]/evidence/review.json' && test ! -e 'inputs/review[b]' && printf '{\"ok\": true}\n'",
    depends_on = [review],
    join = "settled",
)
workflow(type = "playbook", tasks = [discover, review, report])
"#,
        )
        .unwrap();
        fake_agent_manifest(&dir);
        let out = run_playbook(&dir);

        assert_eq!(out.results[&"review[a]".into()].status, TaskStatus::Pass);
        assert_eq!(out.results[&"review[b]".into()].status, TaskStatus::Fail);
        assert!(
            dir.join("state/files/review[a]/evidence/review.json")
                .exists()
        );
        assert!(
            !dir.join("state/files/review[b]").exists(),
            "the failing instance published its sibling's file"
        );
        assert_eq!(
            out.results[&"report".into()].status,
            TaskStatus::Pass,
            "{:?}",
            out.results[&"report".into()].note
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same rule for the veto an agent turn actually has: an instance with no exit code
    /// settles itself failing by returning "status": "fail", and that is its terminal state
    /// before its files are taken, not after.
    #[test]
    fn a_mapped_instance_that_vetoes_itself_publishes_nothing() {
        let dir =
            std::env::temp_dir().join(format!("crucible-settled-veto-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("agents.json"),
            r#"{
              "review[a]": {"writes": {"evidence/review.json": "a reviewed\n"},
                            "result": {"status": "pass", "agree": true}},
              "review[b]": {"result": {"status": "fail", "note": "b objected", "agree": false}}
            }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("workflow.star"),
            r#"
discover = command(
    name = "discover",
    run = "printf '{\"targets\": [\"a\", \"b\"]}\n'",
    emits = ["targets"],
)
review = agent(
    name = "review",
    prompt = "review one target",
    depends_on = [discover],
    over = discover.targets,
    max_fanout = 4,
    required = False,
    emits = ["status", "agree"],
    emits_files = ["evidence/review.json"],
)
report = command(
    name = "report",
    run = "test -e 'inputs/review[a]/evidence/review.json' && test ! -e 'inputs/review[b]' && printf '{\"ok\": true}\n'",
    depends_on = [review],
    join = "settled",
)
workflow(type = "playbook", tasks = [discover, review, report])
"#,
        )
        .unwrap();
        fake_agent_manifest(&dir);
        let out = run_playbook(&dir);

        assert_eq!(out.results[&"review[a]".into()].status, TaskStatus::Pass);
        let vetoed = &out.results[&"review[b]".into()];
        assert_eq!(vetoed.status, TaskStatus::Fail);
        assert_eq!(
            vetoed.output.as_ref().expect("the vetoing object")["agree"],
            false
        );
        let note = vetoed.note.clone().unwrap_or_default();
        assert!(note.starts_with("b objected"), "{note}");
        assert!(note.contains("did not write it"), "{note}");
        assert_eq!(
            std::fs::read_to_string(dir.join("state/files/review[a]/evidence/review.json"))
                .unwrap(),
            "a reviewed\n"
        );
        assert!(
            !dir.join("state/files/review[b]").exists(),
            "the vetoing instance published its sibling's file"
        );
        assert_eq!(
            out.results[&"report".into()].status,
            TaskStatus::Pass,
            "{:?}",
            out.results[&"report".into()].note
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A task that vetoed itself has already failed, so an absent declared file cannot make it a
    /// different failure: the capture problem joins its own note and the object it vetoed with
    /// still reaches a settled consumer.
    #[test]
    fn a_self_declared_failure_keeps_its_object_when_its_declared_file_is_absent() {
        let dir = playbook_pack(
            "settled-veto-solo",
            r#"
solo = agent(
    name = "solo",
    prompt = "grade the stimulus",
    required = False,
    emits = ["status", "separates"],
    emits_files = ["evidence/solo.json"],
)
workflow(type = "playbook", tasks = [solo])
"#,
        );
        std::fs::write(
            dir.join("agents.json"),
            r#"{"solo": {"result": {"status": "fail", "note": "solo vetoed", "separates": false}}}"#,
        )
        .unwrap();
        let (out, runner) = run_playbook_recording(&dir);

        let solo = &out.results[&"solo".into()];
        assert_eq!(solo.status, TaskStatus::Fail);
        assert_eq!(
            solo.output.as_ref().expect("the vetoing object")["separates"],
            false
        );
        let note = solo.note.clone().unwrap_or_default();
        assert!(note.starts_with("solo vetoed"), "{note}");
        assert!(note.contains("absent after a failing attempt"), "{note}");
        assert!(
            !dir.join("state/files/solo").exists(),
            "a task that published nothing left a set behind"
        );
        assert_eq!(runner.captured_bytes.load(Ordering::Relaxed), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A task that declared itself skipped measured nothing, so its declared files are not
    /// evidence: an absent one is not drift, and one it happened to leave behind is neither
    /// published nor charged to the run.
    #[test]
    fn a_self_declared_skip_captures_nothing() {
        let dir = playbook_pack(
            "settled-skip",
            r#"
wrote = command(
    name = "wrote",
    run = "mkdir -p evidence && printf 'no reading\n' > evidence/wrote.json && printf '{\"status\": \"skipped\", \"note\": \"no rig here\"}\n'",
    required = False,
    emits_files = ["evidence/wrote.json"],
)
silent = command(
    name = "silent",
    run = "printf '{\"status\": \"skipped\"}\n'",
    required = False,
    emits_files = ["evidence/silent.json"],
)
workflow(type = "playbook", tasks = [wrote, silent])
"#,
        );
        let (out, runner) = run_playbook_recording(&dir);

        for name in ["wrote", "silent"] {
            let result = &out.results[&TaskName::from(name)];
            assert_eq!(
                result.status,
                TaskStatus::Skipped,
                "{name}: {:?}",
                result.note
            );
            assert!(
                !dir.join("state/files").join(name).exists(),
                "{name} published a set"
            );
        }
        assert_eq!(runner.captured_bytes.load(Ordering::Relaxed), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `state` outlives a run, so a set from an earlier one would otherwise stand for a task that
    /// never ran this time. A task that settles without evidence takes its old set with it.
    #[test]
    fn a_blocked_task_removes_the_set_an_earlier_run_published() {
        let dir = playbook_pack(
            "settled-stale",
            r#"
gate = command(
    name = "gate",
    run = "test ! -f ../fail-gate && printf '{\"ok\": true}\n'",
    required = False,
)
producer = command(
    name = "producer",
    run = "mkdir -p evidence && printf 'p\n' > evidence/p.json && printf '{\"ok\": true}\n'",
    depends_on = [gate],
    required = False,
    emits_files = ["evidence/p.json"],
)
report = command(
    name = "report",
    run = "test ! -e inputs/producer && printf '{\"ok\": true}\n'",
    depends_on = [producer],
    join = "settled",
    required = False,
)
workflow(type = "playbook", tasks = [gate, producer, report])
"#,
        );
        let out = run_playbook(&dir);
        assert_eq!(out.results[&"producer".into()].status, TaskStatus::Pass);
        assert!(dir.join("state/files/producer/evidence/p.json").exists());

        std::fs::write(dir.join("fail-gate"), "").unwrap();
        let out = run_playbook(&dir);
        assert_eq!(out.results[&"gate".into()].status, TaskStatus::Fail);
        assert_eq!(out.results[&"producer".into()].status, TaskStatus::Blocked);
        assert!(
            !dir.join("state/files/producer").exists(),
            "a blocked task kept the set an earlier run published"
        );
        assert_eq!(
            out.results[&"report".into()].status,
            TaskStatus::Pass,
            "{:?}",
            out.results[&"report".into()].note
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The withhold path clears the same stale set the blocked path does: a task that failed
    /// without writing what it declared must not have the set it published while passing stand
    /// for the attempt that captured nothing.
    #[test]
    fn a_withheld_capture_removes_the_set_an_earlier_run_published() {
        let dir = playbook_pack(
            "settled-withhold-stale",
            r#"
producer = command(
    name = "producer",
    run = "test ! -f ../fail-gate && mkdir -p evidence && printf 'p\n' > evidence/p.json && printf '{\"ok\": true}\n'",
    required = False,
    emits_files = ["evidence/p.json"],
)
report = command(
    name = "report",
    run = "python3 -c 'import json, os; v = json.loads(os.environ[\"CRUCIBLE_INPUTS\"]); print(json.dumps({\"files\": v[\"producer\"][\"files\"]}))'",
    depends_on = [producer],
    join = "settled",
)
workflow(type = "playbook", tasks = [producer, report])
"#,
        );
        let out = run_playbook(&dir);
        assert_eq!(out.results[&"producer".into()].status, TaskStatus::Pass);
        assert!(dir.join("state/files/producer/evidence/p.json").exists());
        assert_eq!(
            out.results[&"report".into()].output.as_ref().unwrap()["files"],
            true,
            "a passing producer's set was not staged"
        );

        std::fs::write(dir.join("fail-gate"), "").unwrap();
        let out = run_playbook(&dir);
        let producer = &out.results[&"producer".into()];
        assert_eq!(producer.status, TaskStatus::Fail, "{:?}", producer.note);
        assert!(
            !dir.join("state/files/producer").exists(),
            "a withheld capture kept the set an earlier run published"
        );
        let report = &out.results[&"report".into()];
        assert_eq!(report.status, TaskStatus::Pass, "{:?}", report.note);
        assert_eq!(
            report.output.as_ref().unwrap()["files"],
            false,
            "a withheld capture was reported as staged"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bytes from an attempt that never reached the work are not a reading, so a transport
    /// failure and a self-declared skip publish nothing and charge nothing.
    #[test]
    fn a_transport_failure_and_a_skip_capture_nothing() {
        let (dir, paths) = scratch("capture-unmeasured");
        std::fs::write(paths.workspace.join("A.md"), "present\n").unwrap();
        let task = emitting("draft", &["A.md"]);
        let counter = AtomicU64::new(0);
        let before = prior(&paths, &task);
        for outcome in [
            AttemptOutcome::Transport(TransportFailure::new(
                TransportCause::Other,
                "the broker hung up",
            )),
            AttemptOutcome::Skipped(Value::Null, "not applicable here".to_string()),
        ] {
            let out = capture_declared(
                &paths,
                &paths.workspace,
                &task,
                Attempt {
                    outcome,
                    cost_usd: 0.0,
                },
                &counter,
                &before,
            );
            assert!(
                !matches!(out.outcome, AttemptOutcome::Fail { .. }),
                "an unmeasured attempt was turned into a failure: {:?}",
                out.outcome
            );
            assert!(
                !paths.state.join("files/draft").exists(),
                "an unmeasured attempt published a set"
            );
            assert_eq!(counter.load(Ordering::Relaxed), 0);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The edge rule end to end: a failed producer's evidence is laid down for the consumer that
    /// declared it and for nothing further along, so a descendant cannot read a failed
    /// grandparent's file as if it had passed.
    #[test]
    fn a_failed_producers_evidence_stops_at_the_edge_that_declared_it() {
        let dir = playbook_pack(
            "settled-grandparent",
            r#"
probe = command(
    name = "probe",
    run = "mkdir -p evidence && printf '{\"pass\": false}\n' > evidence/probe.json && exit 1",
    required = False,
    emits_files = ["evidence/probe.json"],
)
mid = command(
    name = "mid",
    run = "test -f inputs/probe/evidence/probe.json && printf '{\"ok\": true}\n'",
    depends_on = [probe],
    join = "settled",
    required = False,
)
tip = command(
    name = "tip",
    run = "test ! -e inputs/probe && printf '{\"ok\": true}\n'",
    depends_on = [mid],
    join = "settled",
)
workflow(type = "playbook", tasks = [probe, mid, tip])
"#,
        );
        let out = run_playbook(&dir);

        assert_eq!(out.results[&"probe".into()].status, TaskStatus::Fail);
        for name in ["mid", "tip"] {
            let result = &out.results[&TaskName::from(name)];
            assert_eq!(result.status, TaskStatus::Pass, "{name}: {:?}", result.note);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole envelope as a task actually reads it: status, note, retained output and the
    /// staged-file flag, for a dependency that failed and one that was blocked behind it.
    #[test]
    fn a_settled_consumer_reads_the_whole_envelope_out_of_its_inputs() {
        let dir = playbook_pack(
            "settled-envelope",
            r#"
probe = command(
    name = "probe",
    run = "mkdir -p evidence && printf 'e\n' > evidence/probe.json && printf '{\"pass\": false, \"margin\": 0.02}\n' && printf 'the rungs did not separate\n' >&2 && exit 3",
    required = False,
    emits_files = ["evidence/probe.json"],
)
deliver = command(
    name = "deliver",
    run = "printf '{\"ok\": true}\n'",
    depends_on = [probe],
    required = False,
)
report = command(
    name = "report",
    run = "python3 -c 'import json, os; print(json.dumps(json.loads(os.environ[\"CRUCIBLE_INPUTS\"])))'",
    depends_on = [probe, deliver],
    join = "settled",
)
workflow(type = "playbook", tasks = [probe, deliver, report])
"#,
        );
        let out = run_playbook(&dir);

        assert_eq!(out.results[&"deliver".into()].status, TaskStatus::Blocked);
        let report = &out.results[&"report".into()];
        assert_eq!(report.status, TaskStatus::Pass, "{:?}", report.note);
        let seen = report.output.as_ref().expect("the echoed envelope");
        assert_eq!(seen["probe"]["status"], "fail");
        assert_eq!(seen["probe"]["output"]["margin"], 0.02);
        assert_eq!(seen["probe"]["files"], true);
        assert!(
            seen["probe"]["note"]
                .as_str()
                .unwrap_or_default()
                .contains("exit 3"),
            "{}",
            seen["probe"]["note"]
        );
        assert_eq!(seen["deliver"]["status"], "blocked");
        assert_eq!(seen["deliver"]["output"], serde_json::Value::Null);
        assert_eq!(seen["deliver"]["files"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// Provenance cannot rest on git's opinion of the workspace: a declared path an earlier
    /// passing task wrote is still there when a later task fails without writing it, and a
    /// `.gitignore` covering that path makes it invisible to every cleanliness test.
    #[test]
    fn an_ignored_path_an_earlier_task_wrote_is_not_the_failing_tasks_evidence() {
        let dir = playbook_pack(
            "settled-ignored-provenance",
            r#"
first = command(
    name = "first",
    run = "printf 'evidence/\n' > .gitignore && mkdir -p evidence && printf 'from first\n' > evidence/x.json && printf '{\"ok\": true}\n'",
    required = False,
    emits_files = ["evidence/x.json"],
)
second = command(
    name = "second",
    run = "printf 'second wrote nothing\n' >&2 && exit 1",
    depends_on = [first],
    required = False,
    emits_files = ["evidence/x.json"],
)
report = command(
    name = "report",
    run = "test ! -e inputs/second && test -e inputs/first/evidence/x.json && printf '{\"ok\": true}\n'",
    depends_on = [first, second],
    join = "settled",
)
workflow(type = "playbook", tasks = [first, second, report])
"#,
        );
        let out = run_playbook(&dir);

        assert_eq!(out.results[&"first".into()].status, TaskStatus::Pass);
        assert_eq!(
            std::fs::read_to_string(dir.join("workspace/evidence/x.json")).unwrap_or_default(),
            "from first\n",
            "the ignored file has to survive into the next task for this to prove anything"
        );
        let second = &out.results[&"second".into()];
        assert_eq!(second.status, TaskStatus::Fail);
        let note = second.note.clone().unwrap_or_default();
        assert!(note.contains("did not write it"), "{note}");
        assert!(
            !dir.join("state/files/second").exists(),
            "a failing task published an ignored file it inherited"
        );
        let report = &out.results[&"report".into()];
        assert_eq!(report.status, TaskStatus::Pass, "{:?}", report.note);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half of the same rule: a failing task that did rewrite the path owns what it
    /// wrote, and the settled consumer reads this attempt's content and not its predecessor's.
    #[test]
    fn a_failing_task_that_rewrote_a_declared_path_publishes_its_own_content() {
        let dir = playbook_pack(
            "settled-rewrote-provenance",
            r#"
first = command(
    name = "first",
    run = "printf 'evidence/\n' > .gitignore && mkdir -p evidence && printf 'from first\n' > evidence/x.json && printf '{\"ok\": true}\n'",
    required = False,
    emits_files = ["evidence/x.json"],
)
second = command(
    name = "second",
    run = "mkdir -p evidence && printf 'from second\n' > evidence/x.json && exit 1",
    depends_on = [first],
    required = False,
    emits_files = ["evidence/x.json"],
)
report = command(
    name = "report",
    run = "grep -q 'from second' inputs/second/evidence/x.json && printf '{\"ok\": true}\n'",
    depends_on = [first, second],
    join = "settled",
)
workflow(type = "playbook", tasks = [first, second, report])
"#,
        );
        let out = run_playbook(&dir);

        assert_eq!(out.results[&"second".into()].status, TaskStatus::Fail);
        assert_eq!(
            std::fs::read_to_string(dir.join("state/files/second/evidence/x.json"))
                .unwrap_or_default(),
            "from second\n",
            "the failing task's own write was withheld"
        );
        let report = &out.results[&"report".into()];
        assert_eq!(report.status, TaskStatus::Pass, "{:?}", report.note);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Output drift converts a passing attempt into a measured failure, and a measured failure
    /// keeps its reading: the reporter that joins settled needs the number the probe printed,
    /// not a null where the missing file used to be.
    #[test]
    fn a_pass_converted_by_an_absent_declared_file_keeps_its_reading() {
        let dir = playbook_pack(
            "settled-withheld-output",
            r#"
probe = command(
    name = "probe",
    run = "printf '{\"pass\": true, \"margin\": 0.42}\n'",
    required = False,
    emits_files = ["evidence/probe.json"],
)
report = command(
    name = "report",
    run = "python3 -c 'import json, os; e = json.loads(os.environ[\"CRUCIBLE_INPUTS\"])[\"probe\"]; print(json.dumps({\"seen\": e[\"status\"], \"margin\": e[\"output\"][\"margin\"], \"files\": e[\"files\"]}))'",
    depends_on = [probe],
    join = "settled",
)
workflow(type = "playbook", tasks = [probe, report])
"#,
        );
        let out = run_playbook(&dir);

        let probe = &out.results[&"probe".into()];
        assert_eq!(probe.status, TaskStatus::Fail);
        let note = probe.note.clone().unwrap_or_default();
        assert!(note.contains("absent"), "{note}");
        assert_eq!(
            probe.output.as_ref().and_then(|o| o.get("margin")),
            Some(&serde_json::json!(0.42)),
            "the converted attempt dropped its own reading"
        );
        let report = &out.results[&"report".into()];
        assert_eq!(report.status, TaskStatus::Pass, "{:?}", report.note);
        let seen = report.output.as_ref().expect("the echoed entry");
        assert_eq!(seen["seen"], "fail");
        assert_eq!(seen["margin"], 0.42);
        assert_eq!(seen["files"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// A mapped node that settles blocked publishes no instance rows, so its own drop is the
    /// only thing that can reach the instance sets an earlier run left behind.
    #[test]
    fn a_mapped_node_that_never_expands_drops_the_instance_sets_an_earlier_run_published() {
        let dir = playbook_pack(
            "settled-stale-instances",
            r#"
discover = command(
    name = "discover",
    run = "test ! -f ../fail-gate && printf '{\"targets\": [\"alpha\"]}\n'",
    required = False,
    emits = ["targets"],
)
audit = command(
    name = "audit",
    run = "mkdir -p evidence && printf 'a\n' > evidence/a.json && printf '{\"ok\": true}\n'",
    depends_on = [discover],
    over = discover.targets,
    max_fanout = 2,
    required = False,
    emits_files = ["evidence/a.json"],
)
workflow(type = "playbook", tasks = [discover, audit])
"#,
        );
        let out = run_playbook(&dir);
        assert_eq!(out.results[&"audit[alpha]".into()].status, TaskStatus::Pass);
        assert!(
            dir.join("state/files/audit[alpha]/evidence/a.json")
                .exists(),
            "the instance never published a set to go stale"
        );

        std::fs::write(dir.join("fail-gate"), "").unwrap();
        let out = run_playbook(&dir);
        assert_eq!(out.results[&"discover".into()].status, TaskStatus::Fail);
        assert_eq!(out.results[&"audit".into()].status, TaskStatus::Blocked);
        assert!(
            !dir.join("state/files/audit[alpha]").exists(),
            "a node that never expanded kept an earlier run's instance set"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
