//! The runner a manifest run's tasks go through: each task gets its root (the shared workspace or
//! a private worktree), its ancestors' declared files staged under `inputs/`, and its own
//! declared files captured once it settles. What runs the task inside that root is a
//! [`Dispatch`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crucible_contract::TransportCause;
use crucible_contract::emits::DeclaredFile;
use serde_json::Value;

use crate::deadline::Deadline;
use crate::paths::Paths;
use crate::plan::STAGED_INPUTS;
use crate::plan::admission::{Admission, Lane, Refusal};
use crate::plan::attempt::{Attempt, AttemptOutcome, BatchItem};
use crate::plan::exec::TaskRunner;
use crate::plan::files::CapturedFiles;
use crate::plan::ir::{Isolation, Task, TaskName};

/// Runs one attempt of one task in the root prepared for it.
pub trait Dispatch: Sync {
    /// Called once before the plan's clock starts.
    fn prepare(&mut self) {}

    fn run_in(&self, paths: &Paths, task: &Task, job: Job<'_>) -> Attempt;

    /// What `task`'s attempts run on, for its result event; None for a task that runs no agent.
    fn task_agent(&self, _task: &Task) -> Option<crucible_contract::session::TaskAgent> {
        None
    }
}

/// The most a single run may capture. Operator-owned, and the run's bound rather than any one
/// task's: a pipeline that captures a little at every stage can still fill a disk.
pub const MAX_CAPTURED_BYTES: u64 = 256 * 1024 * 1024;

/// A declared path resolved inside the workspace, or why it is refused. Symlinks are refused per
/// component and a second name is refused outright, for the same reason the pack's own paths are:
/// resolving a path cannot see a hard link.
pub fn confined(workspace: &Path, declared: &str) -> Result<PathBuf, String> {
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

/// The mode the engine gives a captured file, regardless of what the source carried. A
/// declared output derived from a staged input arrives 0444, and a capture that kept that mode
/// would make the next run's copy fail with EACCES.
pub fn engine_mode(path: &Path) {
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
    fn of(workspace: &Path, declared: &[DeclaredFile]) -> Self {
        PriorContents(
            declared
                .iter()
                .map(|file| {
                    let held = confined(workspace, &file.path)
                        .map_or(Fingerprint::Unreadable, |resolved| fingerprint(&resolved));
                    (file.path.clone(), held)
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
    pub producer: TaskName,
    pub declared: String,
}

pub struct WorkspaceRunner<D> {
    /// What runs one task once its root is ready.
    pub dispatch: D,
    pub paths: Paths,
    /// The frozen injects laid back over a task's root before each attempt.
    pub frozen_injects: Vec<(PathBuf, PathBuf)>,
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
    /// What the run may still spend, as the executor last reported it.
    pub budget_left: f64,
}

impl<D> WorkspaceRunner<D> {
    /// Count what an interrupted process of this run already captured against the run's bound,
    /// which covers the run and not any one process of it.
    pub fn take_over(&mut self) {
        self.captured_bytes =
            AtomicU64::new(tree_bytes(CapturedFiles::new(&self.paths.state).root()));
    }
}

fn tree_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => tree_bytes(&entry.path()),
            Ok(kind) if kind.is_file() => entry.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
}

impl<D: Dispatch> TaskRunner for WorkspaceRunner<D> {
    fn prepare(&mut self) {
        self.dispatch.prepare();
    }

    fn run(
        &mut self,
        task: &Task,
        attempt: u32,
        inputs: &BTreeMap<TaskName, Value>,
        lane: Option<&Lane<'_>>,
    ) -> Attempt {
        let deadline = lane.and_then(Lane::deadline);
        run_task(
            &Cx {
                dispatch: &self.dispatch,
                paths: &self.paths,
                frozen_injects: &self.frozen_injects,
                captured_bytes: &self.captured_bytes,
                staged: self.staged.get(&task.name).map(Vec::as_slice),
            },
            task,
            Job {
                attempt,
                inputs,
                deadline,
                budget_left: self.budget_left,
                lane,
            },
            None,
        )
    }

    fn budget_left(&mut self, usd: f64) {
        self.budget_left = usd;
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
                    producer: producer.name.clone(),
                    declared: declared.path.clone(),
                })
            })
            .collect();
        self.staged.insert(task.name.clone(), files);
        Ok(())
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
            crucible_vcs::git_memory::snapshot(
                workspace,
                &crucible_vcs::git_memory::task_label(&task.name.0),
            )
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
    fn run_many(
        &mut self,
        batch: &[BatchItem<'_>],
        admission: &Admission,
    ) -> Vec<Result<Attempt, Refusal>> {
        // Every item clones the same workspace, so its pending state is captured once here:
        // concurrent `git add -A` in one repo races on `.git/index.lock`.
        let pending = match batch {
            [_] => None,
            _ => match crate::plan::worktree::capture_diff(&self.paths.workspace) {
                Ok(p) => Some(p),
                Err(e) => {
                    let note = format!("capturing the workspace's uncommitted state failed: {e:#}");
                    return batch
                        .iter()
                        .map(|_| Ok(Attempt::failed(0.0, note.clone())))
                        .collect();
                }
            },
        };
        let budget_left = self.budget_left;
        let (dispatch, paths, frozen_injects, captured_bytes, staged) = (
            &self.dispatch,
            &self.paths,
            &self.frozen_injects,
            &self.captured_bytes,
            &self.staged,
        );
        admission.run_all(batch, |b, lane| {
            run_task(
                &Cx {
                    dispatch,
                    paths,
                    frozen_injects,
                    captured_bytes,
                    staged: staged.get(&b.task.name).map(Vec::as_slice),
                },
                b.task,
                Job {
                    attempt: b.attempt,
                    inputs: &b.inputs,
                    deadline: lane.deadline(),
                    budget_left,
                    lane: Some(lane),
                },
                pending.as_deref(),
            )
        })
    }
}

/// What one dispatch needs besides the task: the run's configuration and paths, the run's
/// captured-byte total, and the files this task's ancestors declared.
struct Cx<'a, D> {
    dispatch: &'a D,
    paths: &'a Paths,
    frozen_injects: &'a [(PathBuf, PathBuf)],
    captured_bytes: &'a AtomicU64,
    staged: Option<&'a [StagedInput]>,
}

/// One attempt of one task: its number, its inputs, when it must be over by, what the run may
/// still spend on it, and the lane it reports its spend through while it runs.
#[derive(Clone, Copy)]
pub struct Job<'a> {
    pub attempt: u32,
    pub inputs: &'a BTreeMap<TaskName, Value>,
    pub deadline: Option<Deadline>,
    pub budget_left: f64,
    pub lane: Option<&'a Lane<'a>>,
}

/// Dispatch one task, in the shared workspace or in a private worktree. `pending` is the
/// shared workspace's uncommitted patch when a concurrent caller already captured it for
/// the whole batch; `None` means capture it here.
fn run_task<D: Dispatch>(
    cx: &Cx<'_, D>,
    task: &Task,
    job: Job<'_>,
    pending: Option<&str>,
) -> Attempt {
    let Cx {
        dispatch,
        paths,
        frozen_injects,
        captured_bytes,
        staged,
    } = *cx;
    let Some(Isolation::Worktree) = task.isolation else {
        if let Err(e) = materialize_inputs(&paths.state, &paths.workspace, staged) {
            return Attempt::transport(TransportCause::Workspace, e);
        }
        let before = PriorContents::of(&paths.workspace, &task.emits_files);
        let attempt_out = prepare_and_run(dispatch, frozen_injects, paths, task, job);
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
    let attempt_out = prepare_and_run(dispatch, frozen_injects, &iso, task, job);
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
/// fails at the task that promised it rather than as a mystery in whatever depended on it. So is
/// a declared file whose captured content its schema does not admit. Neither is retried: a task
/// that ran and did not produce what it promised will not produce it twice.
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
        AttemptOutcome::TimedOut(_) => {
            CapturedFiles::new(&paths.state).drop(task);
            return attempt;
        }
    };
    if task.emits_files.is_empty() {
        return attempt;
    }
    let staging = paths
        .state
        .join("files.tmp")
        .join(task.name.dir_name().as_ref());
    let _ = std::fs::remove_dir_all(&staging);
    let mut charged = 0u64;
    let taken = (|| -> Result<(), String> {
        for file in &task.emits_files {
            let declared = &file.path;
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
            if !failing && file.schema.is_some() {
                let bytes = std::fs::read(&to)
                    .map_err(|error| format!("reading captured {declared:?}: {error}"))?;
                if let Some(why) = file.refusal(&bytes) {
                    return Err(why);
                }
            }
        }
        Ok(())
    })();
    let published = CapturedFiles::new(&paths.state).dir(&task.name);
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
        repairs: attempt.repairs,
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
    let files = CapturedFiles::new(state);
    for file in staged {
        let from = files.path(&file.producer, &file.declared);
        if !from.exists() {
            continue;
        }
        let to = inputs
            .join(file.producer.dir_name().as_ref())
            .join(&file.declared);
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

fn prepare_and_run<D: Dispatch>(
    dispatch: &D,
    frozen_injects: &[(PathBuf, PathBuf)],
    paths: &Paths,
    task: &Task,
    job: Job<'_>,
) -> Attempt {
    for (src, dst) in frozen_injects {
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
    let out = dispatch.run_in(paths, task, job);
    Attempt {
        outcome: out.outcome.settle_declared(),
        cost_usd: out.cost_usd,
        repairs: out.repairs,
    }
}

fn task_worktree_name(name: &TaskName) -> String {
    let digest = crucible_contract::artifact::content_digest(name.0.as_bytes());
    format!("task-{}", digest.trim_start_matches("sha256:"))
}

#[cfg(test)]
mod tests {
    use crate::plan::attempt::TransportFailure;
    use crate::plan::ir::{Join, Stage, TaskKind};
    use crate::plan::workspace_runner::*;

    fn scratch(tag: &str) -> (PathBuf, Paths) {
        let dir = std::env::temp_dir().join(format!("crucible-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("workspace")).unwrap();
        std::fs::create_dir_all(dir.join("state")).unwrap();
        let paths = Paths::for_manifest(dir.join("workspace"), dir.join("state"), &dir, None);
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
            emits: crate::plan::ir::Emits::default(),
            emits_files: files.iter().map(|f| DeclaredFile::from(*f)).collect(),
            over: None,
            max_fanout: None,
            keyed: Vec::new(),
            when: None,
            revise: None,
            timeout: None,
            history: None,
            repair: 0,
            optional: Vec::new(),
        }
    }

    fn passing() -> Attempt {
        Attempt {
            outcome: AttemptOutcome::Pass(Value::Null),
            cost_usd: 0.0,
            repairs: Vec::new(),
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

    #[test]
    fn isolation_worktree_names_do_not_collide_after_display_sanitization() {
        let slash = task_worktree_name(&"review/a".into());
        let dash = task_worktree_name(&"review-a".into());
        assert_ne!(slash, dash);
        assert!(slash.starts_with("task-"));
        assert!(slash.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
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
                    repairs: Vec::new(),
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
}
