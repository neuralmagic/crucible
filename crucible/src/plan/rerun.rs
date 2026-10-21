//! Rerunning a finished playbook run from a task (RFC-0002:C-PLAYBOOK-RERUN): which tasks run
//! again, which of the parent's results the rerun reuses, and the parent evidence that carries
//! them.
//!
//! A parent is a finished run's state directory: its session log, run start record, captured
//! files, and git memory.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crucible_contract::session::SessionEvent;
use crucible_vcs::git_memory::TaskCommit;

use crate::plan::exec::{Prior, TaskStatus};
use crate::plan::ir::{Stage, TaskName, ValidPlan};
use crate::plan::resume::{ResumeError, RunStart, settled_row};

/// Where a playbook run keeps its git memory: one patch per task commit, in commit order.
pub const GIT_MEMORY_DIR: &str = "git-memory";
const GIT_MEMORY_INDEX: &str = "tasks.json";

/// Where a rerun reads its parent from: a finished run's state directory, or that run's evidence
/// on the controller (`https://controller/api/runs/<id>`), fetched into a directory laid out the
/// same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParentRef {
    Dir(PathBuf),
    Url(Box<url::Url>),
}

#[derive(Debug, thiserror::Error)]
#[error("--rerun-of {raw:?} is not a URL: {source}")]
pub struct BadParentUrl {
    raw: String,
    source: url::ParseError,
}

impl std::str::FromStr for ParentRef {
    type Err = BadParentUrl;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        if raw.starts_with("http://") || raw.starts_with("https://") {
            return url::Url::parse(raw)
                .map(|url| ParentRef::Url(Box::new(url)))
                .map_err(|source| BadParentUrl {
                    raw: raw.to_string(),
                    source,
                });
        }
        Ok(ParentRef::Dir(PathBuf::from(raw)))
    }
}

/// Each task's definition digest: the compiled task with its parameters bound, and the
/// definition digests of its dependencies.
pub fn definition_digests(plan: &ValidPlan) -> BTreeMap<TaskName, String> {
    let mut out: BTreeMap<TaskName, String> = BTreeMap::new();
    for task in plan.tasks_topo() {
        let mut bytes = serde_json::to_vec(task).unwrap_or_default();
        for dep in &task.depends_on {
            bytes.push(b'\n');
            bytes.extend_from_slice(out.get(dep).map(String::as_bytes).unwrap_or_default());
        }
        out.insert(
            task.name.clone(),
            crucible_contract::artifact::content_digest(&bytes),
        );
    }
    out
}

/// The node a settled row belongs to: a mapped instance `x[key]` and a revise round
/// `x[round-N]` both belong to `x`.
fn node_of(name: &str) -> &str {
    name.split_once('[').map_or(name, |(node, _)| node)
}

#[derive(Debug, thiserror::Error)]
pub enum RerunError {
    #[error("reading the parent run: {0}")]
    Parent(#[from] ResumeError),
    #[error("the parent run has not shut down, so it cannot be rerun")]
    NotFinished,
    #[error("--from names {0:?}, which is not a main-graph task of this playbook")]
    UnknownTask(String),
    #[error(
        "{} changed since the parent run; rerun from it, or undo the edit",
        .0.iter().map(|t| t.0.as_str()).collect::<Vec<_>>().join(", ")
    )]
    DefinitionsChanged(Vec<TaskName>),
    #[error("the parent run's {0} is missing")]
    MissingEvidence(String),
    #[error("{}: {source}", .path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{} is not a git memory index: {source}", .path.display())]
    BadIndex {
        path: PathBuf,
        source: serde_json::Error,
    },
}

/// A finished run, read from its session log.
#[derive(Debug)]
pub struct Parent {
    /// The run's id, credited on every row a rerun reuses from it.
    pub run: String,
    definitions: BTreeMap<TaskName, String>,
    rows: Vec<SessionEvent>,
}

impl Parent {
    /// Read `run`'s session log, refusing one that has not shut down.
    pub fn from_log(run: &str, log: &str) -> Result<Self, RerunError> {
        let mut definitions = BTreeMap::new();
        let mut rows = Vec::new();
        let mut shut_down = false;
        for event in log.lines().filter_map(crucible_contract::session::decode) {
            shut_down = false;
            match event {
                SessionEvent::PlanAdmitted { ref tasks, .. } => {
                    definitions = tasks
                        .iter()
                        .map(|t| (TaskName(t.name.clone()), t.definition.clone()))
                        .collect();
                }
                SessionEvent::TaskResult { .. } => rows.push(event),
                SessionEvent::Shutdown { .. } => shut_down = true,
                _ => {}
            }
        }
        if !shut_down {
            return Err(RerunError::NotFinished);
        }
        Ok(Parent {
            run: run.to_string(),
            definitions,
            rows,
        })
    }

    /// The status the parent settled each node with: the node's own row, the last one logged.
    fn statuses(&self) -> BTreeMap<String, String> {
        self.rows
            .iter()
            .filter_map(|event| match event {
                SessionEvent::TaskResult { task, status, .. } if !task.contains('[') => {
                    Some((task.clone(), status.clone()))
                }
                _ => None,
            })
            .collect()
    }
}

/// A finished run's state directory: its session log, captured files, and git memory, which
/// carries the record of where the run started.
#[derive(Debug)]
pub struct ParentDir {
    pub dir: PathBuf,
    pub start: RunStart,
    pub parent: Parent,
}

impl ParentDir {
    pub fn read(dir: &Path) -> Result<Self, RerunError> {
        let memory = dir.join(GIT_MEMORY_DIR);
        if !memory.is_dir() {
            return Err(RerunError::MissingEvidence("git memory".to_string()));
        }
        let start = RunStart::read(&memory)?;
        let path = dir.join("session.jsonl");
        let log = std::fs::read_to_string(&path).map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => RerunError::MissingEvidence("session log".to_string()),
            _ => RerunError::Read { path, source },
        })?;
        let parent = Parent::from_log(&start.run, &log)?;
        Ok(ParentDir {
            dir: dir.to_path_buf(),
            start,
            parent,
        })
    }

    /// The parent's captured set for `task`, when it captured one.
    fn captured(&self, task: &TaskName) -> PathBuf {
        self.dir.join("files").join(&task.0)
    }
}

/// What a rerun reuses and what it dispatches again.
#[derive(Debug)]
pub struct Rerun {
    /// The reused rows, terminal before anything dispatches.
    pub prior: Prior,
    /// Main-graph nodes whose parent results are reused.
    pub reused: BTreeSet<TaskName>,
    /// Main-graph nodes dispatched again.
    pub rerun: BTreeSet<TaskName>,
}

impl Rerun {
    /// The reused nodes' commits, in the order the parent made them, once the parent's captured
    /// sets are known to be there.
    pub fn evidence(
        &self,
        plan: &ValidPlan,
        parent: &ParentDir,
    ) -> Result<Vec<TaskCommit>, RerunError> {
        for name in &self.reused {
            let declares = plan
                .get(name)
                .is_some_and(|t| !t.emits_files.is_empty() && t.over.is_none());
            if declares && !parent.captured(name).is_dir() {
                return Err(RerunError::MissingEvidence(format!(
                    "captured files of {name}"
                )));
            }
        }
        Ok(read_git_memory(&parent.dir)?
            .into_iter()
            .filter(|c| {
                self.reused
                    .contains(&TaskName(node_of(&c.task).to_string()))
            })
            .collect())
    }

    /// Copy the reused rows' captured sets from the parent into `state`, as the parent left them.
    pub fn restore_captured(&self, parent: &ParentDir, state: &Path) -> std::io::Result<()> {
        for name in &self.prior.order {
            let from = parent.captured(name);
            if !from.is_dir() {
                continue;
            }
            let to = state.join("files").join(&name.0);
            let _ = std::fs::remove_dir_all(&to);
            copy_tree(&from, &to)?;
        }
        Ok(())
    }
}

/// Captured sets hold regular files only: the engine publishes them with `fs::copy`.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Decide what a rerun of `parent` from `from` reuses, refusing a reuse whose task changed.
pub fn plan(plan: &ValidPlan, parent: &Parent, from: &[TaskName]) -> Result<Rerun, RerunError> {
    let main: Vec<&TaskName> = plan
        .tasks_topo()
        .filter(|t| t.stage == Stage::Iteration)
        .map(|t| &t.name)
        .collect();
    if let Some(unknown) = from.iter().find(|name| !main.contains(name)) {
        return Err(RerunError::UnknownTask(unknown.0.clone()));
    }
    let statuses = parent.statuses();
    let passed = |name: &TaskName| {
        statuses
            .get(&name.0)
            .is_some_and(|s| s.parse::<TaskStatus>().is_ok_and(|s| s == TaskStatus::Pass))
    };
    let mut rerun: BTreeSet<TaskName> = from.iter().cloned().collect();
    rerun.extend(
        main.iter()
            .filter(|name| !passed(name) || !parent.definitions.contains_key(*name))
            .map(|name| (*name).clone()),
    );
    let loops = plan.revise_loops();
    loop {
        let before = rerun.len();
        for task in plan.tasks_topo() {
            if task.depends_on.iter().any(|dep| rerun.contains(dep)) {
                rerun.insert(task.name.clone());
            }
        }
        for revise in &loops {
            if revise.body.iter().any(|t| rerun.contains(&t.name)) {
                rerun.extend(revise.body.iter().map(|t| t.name.clone()));
            }
        }
        if rerun.len() == before {
            break;
        }
    }
    let (rerun, reused): (BTreeSet<TaskName>, BTreeSet<TaskName>) = main
        .into_iter()
        .cloned()
        .partition(|name| rerun.contains(name));

    let now = definition_digests(plan);
    let changed: Vec<TaskName> = reused
        .iter()
        .filter(|name| parent.definitions.get(*name) != now.get(*name))
        .cloned()
        .collect();
    if !changed.is_empty() {
        return Err(RerunError::DefinitionsChanged(changed));
    }

    let mut prior = Prior::default();
    for event in &parent.rows {
        let SessionEvent::TaskResult { task, .. } = event else {
            continue;
        };
        if !reused.contains(&TaskName(node_of(task).to_string())) {
            continue;
        }
        let Some(row) = settled_row(plan, event.clone())? else {
            continue;
        };
        prior.reused.insert(
            row.name.clone(),
            row.reused_from.unwrap_or_else(|| parent.run.clone()),
        );
        if prior.results.insert(row.name.clone(), row.result).is_none() {
            prior.order.push(row.name);
        }
    }
    Ok(Rerun {
        prior,
        reused,
        rerun,
    })
}

/// Write a run's git memory under `state`, with the record of where the run started, replacing
/// any earlier copy whole.
pub fn write_git_memory(
    state: &Path,
    start: &RunStart,
    commits: &[TaskCommit],
) -> Result<(), RerunError> {
    let dir = state.join(GIT_MEMORY_DIR);
    let staging = state.join(format!("{GIT_MEMORY_DIR}.tmp"));
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| RerunError::Read { path, source }
    };
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(io(&staging))?;
    for (i, commit) in commits.iter().enumerate() {
        let path = staging.join(format!("{i:06}.patch"));
        std::fs::write(&path, &commit.patch).map_err(io(&path))?;
    }
    let tasks: Vec<&str> = commits.iter().map(|c| c.task.as_str()).collect();
    let index = staging.join(GIT_MEMORY_INDEX);
    std::fs::write(&index, serde_json::json!(tasks).to_string()).map_err(io(&index))?;
    start.write(&staging)?;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(&staging, &dir).map_err(io(&dir))
}

/// Read the git memory a run wrote under `state`.
pub fn read_git_memory(state: &Path) -> Result<Vec<TaskCommit>, RerunError> {
    let dir = state.join(GIT_MEMORY_DIR);
    let index = dir.join(GIT_MEMORY_INDEX);
    let bytes = std::fs::read(&index).map_err(|source| match source.kind() {
        std::io::ErrorKind::NotFound => RerunError::MissingEvidence("git memory".to_string()),
        _ => RerunError::Read {
            path: index.clone(),
            source,
        },
    })?;
    let tasks: Vec<String> =
        serde_json::from_slice(&bytes).map_err(|source| RerunError::BadIndex {
            path: index.clone(),
            source,
        })?;
    tasks
        .into_iter()
        .enumerate()
        .map(|(i, task)| {
            let path = dir.join(format!("{i:06}.patch"));
            let patch = std::fs::read(&path).map_err(|source| RerunError::Read { path, source })?;
            Ok(TaskCommit { task, patch })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::plan::exec::TaskStatus;
    use crate::plan::ir::{Plan, TaskName, ValidPlan};
    use crate::plan::rerun::{
        ParentDir, ParentRef, RerunError, definition_digests, plan, read_git_memory,
        write_git_memory,
    };
    use crate::plan::resume::RunStart;
    use crucible_vcs::git_memory::TaskCommit;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tempdir() -> Dir {
        let dir = std::env::temp_dir().join(format!(
            "rerun-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Dir(dir)
    }

    const GRAPH: &str = r#"
        version = 1
        [budget]
        usd = 10.0
        [[task]]
        name = "discover"
        kind = "command"
        command = "true"
        [[task]]
        name = "fix"
        kind = "command"
        command = "edit"
        depends_on = ["discover"]
        [[task]]
        name = "test"
        kind = "command"
        command = "check"
        depends_on = ["fix"]
        [[task]]
        name = "report"
        kind = "command"
        command = "true"
        depends_on = ["test"]
        [[task]]
        name = "lint"
        kind = "command"
        command = "true"
        depends_on = ["discover"]
        [[task]]
        name = "author"
        kind = "command"
        command = "true"
        depends_on = ["discover"]
        [[task]]
        name = "repro"
        kind = "command"
        command = "true"
        depends_on = ["author"]
        revise = { task = "author", max_rounds = 2 }
        [[task]]
        name = "notify"
        kind = "command"
        command = "true"
        stage = "epilogue"
    "#;

    fn compile(src: &str) -> ValidPlan {
        Plan::from_toml_str(src).unwrap().validate().unwrap()
    }

    fn names(list: &[&str]) -> Vec<TaskName> {
        list.iter().map(|n| TaskName((*n).to_string())).collect()
    }

    fn admitted(plan: &ValidPlan) -> String {
        let defs = definition_digests(plan);
        let tasks: Vec<serde_json::Value> = plan
            .tasks_topo()
            .map(|t| {
                serde_json::json!({
                    "name": t.name.0,
                    "kind": "command",
                    "definition": defs[&t.name],
                })
            })
            .collect();
        serde_json::json!({
            "v": 1, "kind": "plan_admitted", "plan_version": 1, "budget_usd": 10.0, "tasks": tasks,
        })
        .to_string()
    }

    fn row(task: &str, status: &str, cost: f64) -> String {
        serde_json::json!({
            "v": 1, "kind": "task_result", "task": task, "status": status, "attempts": 1,
            "cost_usd": cost,
        })
        .to_string()
    }

    const SHUTDOWN: &str = r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":"done"}"#;

    /// A finished parent of `plan` that settled `rows`, with git memory `commits`.
    fn parent_dir(plan: &ValidPlan, rows: &[String], commits: &[(&str, &str)]) -> Dir {
        let dir = tempdir();
        let mut log = vec![admitted(plan)];
        log.extend(rows.iter().cloned());
        log.push(SHUTDOWN.to_string());
        std::fs::write(dir.0.join("session.jsonl"), log.join("\n")).unwrap();
        let commits: Vec<TaskCommit> = commits
            .iter()
            .map(|(task, patch)| TaskCommit {
                task: (*task).to_string(),
                patch: patch.as_bytes().to_vec(),
            })
            .collect();
        write_git_memory(&dir.0, &start(plan), &commits).unwrap();
        dir
    }

    fn start(plan: &ValidPlan) -> RunStart {
        RunStart::begin("parent-run", "abc".into(), "tree".into(), plan)
    }

    fn read(dir: &Dir) -> ParentDir {
        ParentDir::read(&dir.0).unwrap()
    }

    fn settled_all_but_test() -> Vec<String> {
        vec![
            row("discover", "pass", 0.1),
            row("fix", "pass", 1.0),
            row("lint", "pass", 0.2),
            row("author", "pass", 0.3),
            row("repro", "pass", 0.4),
            row("test", "fail", 0.5),
            row("report", "blocked", 0.0),
            row("notify", "pass", 0.0),
        ]
    }

    #[test]
    fn a_rerun_from_a_task_reuses_its_upstream_and_reruns_everything_downstream() {
        let graph = compile(GRAPH);
        let dir = parent_dir(
            &graph,
            &settled_all_but_test(),
            &[
                ("fix", "fix.patch"),
                ("lint", "lint.patch"),
                ("test", "never"),
            ],
        );
        let parent = read(&dir);
        let rerun = plan(&graph, &parent.parent, &names(&["test"])).unwrap();
        assert_eq!(
            rerun.reused,
            names(&["author", "discover", "fix", "lint", "repro"])
                .into_iter()
                .collect::<BTreeSet<_>>()
        );
        assert_eq!(
            rerun.prior.order,
            names(&["discover", "fix", "lint", "author", "repro"]),
            "reused rows keep the order the parent settled them in"
        );
        assert!(rerun.prior.reused.values().all(|from| from == "parent-run"));
        assert_eq!(
            rerun.prior.results[&TaskName("fix".into())].status,
            TaskStatus::Pass
        );
        assert_eq!(
            rerun.prior.spent_usd(),
            0.0,
            "reused spend is not this run's"
        );
        assert!((rerun.prior.reused_usd() - 2.0).abs() < 1e-9);
        assert_eq!(
            rerun
                .evidence(&graph, &parent)
                .unwrap()
                .iter()
                .map(|c| c.task.as_str())
                .collect::<Vec<_>>(),
            ["fix", "lint"],
            "only reused tasks' commits replay"
        );
        assert_eq!(
            rerun.rerun,
            names(&["report", "test"])
                .into_iter()
                .collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn a_failed_task_reruns_with_nothing_named() {
        let graph = compile(GRAPH);
        let dir = parent_dir(&graph, &settled_all_but_test(), &[]);
        let rerun = plan(&graph, &read(&dir).parent, &[]).unwrap();
        assert!(!rerun.reused.contains(&TaskName("test".into())));
        assert!(!rerun.reused.contains(&TaskName("report".into())));
        assert!(rerun.reused.contains(&TaskName("fix".into())));
        assert!(
            !rerun.prior.results.contains_key(&TaskName("notify".into())),
            "an epilogue always reruns"
        );
    }

    #[test]
    fn editing_the_task_rerun_from_is_allowed_and_an_upstream_edit_is_refused() {
        let graph = compile(GRAPH);
        let dir = parent_dir(&graph, &settled_all_but_test(), &[]);
        let parent = read(&dir).parent;

        let edited_test =
            compile(&GRAPH.replace(r#"command = "check""#, r#"command = "check --fixed""#));
        plan(&edited_test, &parent, &names(&["test"])).unwrap();

        let edited_fix =
            compile(&GRAPH.replace(r#"command = "edit""#, r#"command = "edit --again""#));
        match plan(&edited_fix, &parent, &names(&["test"])) {
            Err(RerunError::DefinitionsChanged(changed)) => {
                assert_eq!(changed, names(&["fix"]));
            }
            other => panic!("expected the fix edit refused, got {other:?}"),
        }
        let rerun = plan(&edited_fix, &parent, &names(&["fix"])).unwrap();
        assert!(!rerun.reused.contains(&TaskName("fix".into())));
        assert!(rerun.reused.contains(&TaskName("lint".into())));
    }

    #[test]
    fn an_edit_upstream_of_a_reused_task_moves_its_definition() {
        let graph = compile(GRAPH);
        let edited = compile(&GRAPH.replace(r#"command = "edit""#, r#"command = "edit --again""#));
        let (before, after) = (definition_digests(&graph), definition_digests(&edited));
        for unchanged in ["discover", "lint", "author", "repro", "notify"] {
            let name = TaskName(unchanged.into());
            assert_eq!(before[&name], after[&name], "{unchanged}");
        }
        for moved in ["fix", "test", "report"] {
            let name = TaskName(moved.into());
            assert_ne!(before[&name], after[&name], "{moved}");
        }
    }

    #[test]
    fn rerunning_any_member_of_a_revise_loop_reruns_the_whole_loop() {
        let graph = compile(GRAPH);
        let dir = parent_dir(&graph, &settled_all_but_test(), &[]);
        let rerun = plan(&graph, &read(&dir).parent, &names(&["repro"])).unwrap();
        assert!(!rerun.reused.contains(&TaskName("author".into())));
        assert!(!rerun.reused.contains(&TaskName("repro".into())));
        assert!(rerun.reused.contains(&TaskName("discover".into())));
    }

    #[test]
    fn a_mapped_node_is_reused_whole_with_its_instance_rows() {
        let graph = compile(
            r#"
            version = 1
            [budget]
            usd = 10.0
            [[task]]
            name = "discover"
            kind = "command"
            command = "true"
            emits = ["targets"]
            [[task]]
            name = "audit"
            kind = "command"
            command = "true"
            depends_on = ["discover"]
            over = { task = "discover", field = "targets" }
            max_fanout = 4
            [[task]]
            name = "wrap"
            kind = "command"
            command = "true"
            depends_on = ["audit"]
            "#,
        );
        let dir = parent_dir(
            &graph,
            &[
                row("discover", "pass", 0.1),
                row("audit[a]", "pass", 0.2),
                row("audit[b]", "pass", 0.3),
                row("audit", "pass", 0.5),
                row("wrap", "fail", 0.1),
            ],
            &[("audit[a]", "a"), ("audit[b]", "b"), ("wrap", "never")],
        );
        let parent = read(&dir);
        let rerun = plan(&graph, &parent.parent, &[]).unwrap();
        assert_eq!(
            rerun.prior.order,
            names(&["discover", "audit[a]", "audit[b]", "audit"])
        );
        assert!(
            (rerun.prior.reused_usd() - 0.6).abs() < 1e-9,
            "a node counts its items once"
        );
        assert_eq!(
            rerun
                .evidence(&graph, &parent)
                .unwrap()
                .iter()
                .map(|c| c.task.as_str())
                .collect::<Vec<_>>(),
            ["audit[a]", "audit[b]"]
        );
    }

    #[test]
    fn a_rerun_of_a_rerun_credits_the_run_that_settled_each_row() {
        let graph = compile(GRAPH);
        let mut rows = settled_all_but_test();
        rows[0] = serde_json::json!({
            "v": 1, "kind": "task_result", "task": "discover", "status": "pass", "attempts": 1,
            "cost_usd": 0.1, "reused_from": "grandparent-run",
        })
        .to_string();
        let dir = parent_dir(&graph, &rows, &[]);
        let rerun = plan(&graph, &read(&dir).parent, &names(&["test"])).unwrap();
        assert_eq!(
            rerun.prior.reused[&TaskName("discover".into())],
            "grandparent-run"
        );
        assert_eq!(rerun.prior.reused[&TaskName("fix".into())], "parent-run");
    }

    #[test]
    fn a_selection_must_name_a_main_graph_task() {
        let graph = compile(GRAPH);
        let dir = parent_dir(&graph, &settled_all_but_test(), &[]);
        let parent = read(&dir).parent;
        for bad in ["nope", "notify"] {
            assert!(
                matches!(plan(&graph, &parent, &names(&[bad])), Err(RerunError::UnknownTask(t)) if t == bad),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_parent_still_running_is_refused() {
        let graph = compile(GRAPH);
        let dir = parent_dir(&graph, &settled_all_but_test(), &[]);
        let log = std::fs::read_to_string(dir.0.join("session.jsonl")).unwrap();
        std::fs::write(
            dir.0.join("session.jsonl"),
            log.trim_end_matches(SHUTDOWN).trim_end(),
        )
        .unwrap();
        assert!(matches!(
            ParentDir::read(&dir.0),
            Err(RerunError::NotFinished)
        ));
    }

    #[test]
    fn missing_parent_evidence_is_named() {
        let graph = compile(GRAPH);
        let dir = parent_dir(&graph, &settled_all_but_test(), &[]);
        std::fs::remove_file(dir.0.join("session.jsonl")).unwrap();
        assert!(
            matches!(ParentDir::read(&dir.0), Err(RerunError::MissingEvidence(what)) if what == "session log")
        );
        std::fs::remove_dir_all(dir.0.join(super::GIT_MEMORY_DIR)).unwrap();
        assert!(
            matches!(ParentDir::read(&dir.0), Err(RerunError::MissingEvidence(what)) if what == "git memory")
        );
    }

    #[test]
    fn a_reused_task_that_captured_files_needs_them_in_the_parent() {
        let src = GRAPH.replace(
            "        command = \"true\"\n        [[task]]\n        name = \"fix\"",
            "        command = \"true\"\n        emits_files = [\"targets.json\"]\n        [[task]]\n        name = \"fix\"",
        );
        let graph = compile(&src);
        let dir = parent_dir(&graph, &settled_all_but_test(), &[]);
        let parent = read(&dir);
        let rerun = plan(&graph, &parent.parent, &[]).unwrap();
        assert!(
            matches!(rerun.evidence(&graph, &parent), Err(RerunError::MissingEvidence(what)) if what.contains("discover"))
        );
        let set = dir.0.join("files").join("discover");
        std::fs::create_dir_all(&set).unwrap();
        std::fs::write(set.join("targets.json"), "[]").unwrap();
        rerun.evidence(&graph, &parent).unwrap();
        let state = tempdir();
        rerun.restore_captured(&parent, &state.0).unwrap();
        assert_eq!(
            std::fs::read_to_string(state.0.join("files/discover/targets.json")).unwrap(),
            "[]"
        );
    }

    #[test]
    fn a_parent_that_predates_definitions_reuses_nothing() {
        let graph = compile(GRAPH);
        let dir = parent_dir(&graph, &settled_all_but_test(), &[]);
        let log = std::fs::read_to_string(dir.0.join("session.jsonl")).unwrap();
        let mut lines: Vec<String> = log.lines().map(str::to_string).collect();
        let mut admitted: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        for task in admitted["tasks"].as_array_mut().unwrap() {
            task.as_object_mut().unwrap().remove("definition");
        }
        lines[0] = admitted.to_string();
        std::fs::write(dir.0.join("session.jsonl"), lines.join("\n")).unwrap();
        assert!(matches!(
            plan(&graph, &read(&dir).parent, &names(&["test"])),
            Err(RerunError::DefinitionsChanged(_))
        ));
    }

    #[test]
    fn git_memory_round_trips_and_replaces_an_earlier_copy() {
        let dir = tempdir();
        let plan = compile(GRAPH);
        let first = vec![TaskCommit {
            task: "old".into(),
            patch: b"x".to_vec(),
        }];
        write_git_memory(&dir.0, &start(&plan), &first).unwrap();
        let second = vec![
            TaskCommit {
                task: "audit[a b]".into(),
                patch: vec![0, 255, 7],
            },
            TaskCommit {
                task: "fix".into(),
                patch: b"diff".to_vec(),
            },
        ];
        write_git_memory(&dir.0, &start(&plan), &second).unwrap();
        assert_eq!(read_git_memory(&dir.0).unwrap(), second);
        assert_eq!(
            ParentDir::read(&dir.0).err().map(|e| e.to_string()),
            Some("the parent run's session log is missing".to_string())
        );
        write_git_memory(&dir.0, &start(&plan), &[]).unwrap();
        assert_eq!(read_git_memory(&dir.0).unwrap(), Vec::new());
    }

    #[test]
    fn a_parent_is_a_url_only_when_it_says_http() {
        assert_eq!(
            "https://controller.example/api/runs/acme-1"
                .parse::<ParentRef>()
                .unwrap(),
            ParentRef::Url(Box::new(
                url::Url::parse("https://controller.example/api/runs/acme-1").unwrap()
            ))
        );
        assert!(matches!(
            "http://127.0.0.1:8080/api/runs/acme-1".parse::<ParentRef>(),
            Ok(ParentRef::Url(_))
        ));
        assert_eq!(
            "runs/acme-1/state".parse::<ParentRef>().unwrap(),
            ParentRef::Dir("runs/acme-1/state".into())
        );
        let err = "https://".parse::<ParentRef>().unwrap_err();
        assert!(err.to_string().contains("not a URL"), "{err}");
    }
}
