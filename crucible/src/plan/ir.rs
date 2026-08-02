use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Task identity: cache key component, wire label, UI label. Unique within a plan.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskName(pub String);

impl fmt::Display for TaskName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for TaskName {
    fn from(s: &str) -> Self {
        TaskName(s.to_string())
    }
}

/// Grading direction for reducers, mirroring the judge's convention.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Lower,
    Higher,
}

/// Authorable operations that require orchestrator capabilities to execute.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineOp {
    /// Run the loop's candidate-producing turn.
    Propose,
    /// `World::apply`: make the candidate live (a failure = unscoreable, discard).
    Apply,
    /// `Judge::measure`: score the live candidate.
    Measure,
    /// `Judge::decide`: rule keep/discard against the run's best.
    Decide,
    /// The wide tournament's scoring stage: apply an upstream candidate diff to the main
    /// workspace, `World::apply`, measure with the frozen judge, restore. Serialized by
    /// construction (never isolation-marked), because candidates share one deployment.
    MeasureDiff,
}

/// Where a task executes. Authorable (`isolation = "worktree"`); a runner that cannot
/// honor it must refuse the task loudly rather than silently ignore it: see
/// [`crate::plan::runner::ShellRunner`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// A private clone of the workspace; the task's edits travel out as a captured diff
    /// in its output, never as workspace state.
    Worktree,
}

/// How dependency outputs join into a task's inputs (`join = "all" | "passed"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Join {
    /// Every dependency must pass; anything else blocks the task (the default, and the
    /// only behavior before isolated fan-out).
    #[default]
    All,
    /// Dispatch once every dependency is terminal, folding only the passing outputs: a
    /// reducer over a lossy fan-out (the wide `top_k`: skipped/failed candidates just
    /// don't rank), or a join over reviewers where one being advisory must not stop the run.
    Passed,
}

/// What a task *is*. The executor owns advancement; agents only ever run inside `Agent` tasks.
///
/// Internally tagged so TOML and JSON authoring read naturally:
/// `kind = "agent"` / `kind = "command"` / `kind = "top_k"`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskKind {
    /// An agent turn. Harness, model family, and effort are per-task knobs: the openshell
    /// heterogeneity axis. `None` inherits the manifest's `[agent]` defaults.
    Agent {
        prompt: String,
        #[serde(default)]
        harness: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        effort: Option<String>,
    },
    /// A plan-authored command. Trusted scripts require frozen manifest injects.
    Command { command: String },
    /// Engine-builtin deterministic fold: keep the k best upstream outputs by `score`.
    TopK { k: u32, direction: Direction },
    /// A capability-owned engine operation.
    Engine {
        op: EngineOp,
        /// Typed input; dependencies still control scheduling.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<TaskName>,
    },
}

impl TaskKind {
    /// Stable wire label for the kind (`SessionEvent::TaskResult.task_kind`, UI classes).
    pub fn label(&self) -> &'static str {
        match self {
            TaskKind::Agent { .. } => "agent",
            TaskKind::Command { .. } => "command",
            TaskKind::TopK { .. } => "top_k",
            TaskKind::Engine { op, .. } => match op {
                EngineOp::Propose => "engine_propose",
                EngineOp::Apply => "engine_apply",
                EngineOp::Measure => "engine_measure",
                EngineOp::Decide => "engine_decide",
                EngineOp::MeasureDiff => "engine_measure_diff",
            },
        }
    }
}

fn default_needs() -> String {
    "any".to_string()
}
fn default_required() -> bool {
    true
}

/// One unit of work in a plan. Always "task", never node/stage/step/rung.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub name: TaskName,
    #[serde(flatten)]
    pub task: TaskKind,
    #[serde(default)]
    pub depends_on: Vec<TaskName>,
    /// Durable logical session; shared names must be dependency-ordered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Substrate capability this task needs; `"any"` runs everywhere.
    #[serde(default = "default_needs")]
    pub needs: String,
    /// Required tasks gate plan validity; advisory failures block dependents only.
    #[serde(default = "default_required")]
    pub required: bool,
    /// Isolated execution (see [`Isolation`]); absent = run in the shared workspace.
    #[serde(default)]
    pub isolation: Option<Isolation>,
    /// Dependency-join semantics (see [`Join`]).
    #[serde(default)]
    pub join: Join,
}

/// Executor-enforced accounting limit; overruns fail the plan.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PlanBudget {
    pub usd: f64,
}

/// A versioned work graph. `reason` is reserved for replanning.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub version: u32,
    #[serde(default)]
    pub reason: Option<String>,
    pub budget: PlanBudget,
    #[serde(rename = "task", default)]
    pub tasks: Vec<Task>,
}

/// A plan that passed structural validation, carrying its topological order.
/// The executor only accepts this type: an unvalidated `Plan` cannot run.
#[derive(Debug)]
pub struct ValidPlan {
    plan: Plan,
    /// Indices into `plan.tasks`, dependency-first.
    topo: Vec<usize>,
}

impl ValidPlan {
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    pub fn tasks_topo(&self) -> impl Iterator<Item = &Task> {
        self.topo.iter().map(|&i| &self.plan.tasks[i])
    }

    #[allow(dead_code)]
    pub fn get(&self, name: &TaskName) -> Option<&Task> {
        self.plan.tasks.iter().find(|t| &t.name == name)
    }
}

impl Plan {
    /// Parse JSON without validating it.
    pub fn from_json_str(s: &str) -> Result<Plan> {
        serde_json::from_str(s).context("PLAN.json does not parse as a plan")
    }

    /// Parse the pack-authored TOML form (`version`, `[budget]`, `[[task]]`).
    pub fn from_toml_str(s: &str) -> Result<Plan> {
        toml::from_str(s).context("plan TOML does not parse")
    }

    /// Validate structure and compute dependency order.
    pub fn validate(self) -> Result<ValidPlan> {
        if self.version != 1 {
            bail!(
                "unsupported plan version {}; this build supports only version 1",
                self.version
            );
        }
        if self.tasks.is_empty() {
            bail!("plan declares no tasks");
        }
        if !self.budget.usd.is_finite() || self.budget.usd <= 0.0 {
            bail!(
                "plan budget.usd must be a positive number, got {}",
                self.budget.usd
            );
        }
        let mut index: BTreeMap<&TaskName, usize> = BTreeMap::new();
        for (i, t) in self.tasks.iter().enumerate() {
            if t.name.0.trim().is_empty() {
                bail!("task #{i} has an empty name");
            }
            if index.insert(&t.name, i).is_some() {
                bail!("duplicate task name {:?}", t.name.0);
            }
        }
        for t in &self.tasks {
            let mut seen = BTreeSet::new();
            for d in &t.depends_on {
                if d == &t.name {
                    bail!("task {:?} depends on itself", t.name.0);
                }
                if !index.contains_key(d) {
                    bail!("task {:?} depends on unknown task {:?}", t.name.0, d.0);
                }
                if !seen.insert(d) {
                    bail!("task {:?} lists dependency {:?} twice", t.name.0, d.0);
                }
            }
            if let TaskKind::TopK { k, .. } = &t.task {
                if *k == 0 {
                    bail!("task {:?}: top_k k must be >= 1", t.name.0);
                }
                if t.depends_on.is_empty() {
                    bail!(
                        "task {:?}: top_k needs at least one dependency to fold",
                        t.name.0
                    );
                }
            }
            if let Some(session) = &t.session {
                if session.is_empty()
                    || session.len() > 64
                    || !session
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                {
                    bail!(
                        "task {:?} has invalid session {:?}; use 1-64 ASCII letters, digits, `.`, `_`, or `-`",
                        t.name.0,
                        session
                    );
                }
                if !matches!(
                    t.task,
                    TaskKind::Agent { .. }
                        | TaskKind::Engine {
                            op: EngineOp::Propose,
                            ..
                        }
                ) {
                    bail!(
                        "task {:?} sets session, but only agent and engine propose tasks can resume an agent",
                        t.name.0
                    );
                }
                if t.isolation.is_some() {
                    bail!(
                        "task {:?} sets session {:?}, but durable sessions cannot use disposable isolation",
                        t.name.0,
                        session
                    );
                }
            }
        }
        // Kahn's algorithm; leftovers mean a cycle. The ready set is a min-heap on the
        // declaration index so the order is deterministic and declaration-stable: ties
        // dispatch in the order the author wrote them, which the UI, the cache, and the
        // budget cutoff all depend on.
        let n = self.tasks.len();
        let mut indegree = vec![0usize; n];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (i, t) in self.tasks.iter().enumerate() {
            indegree[i] = t.depends_on.len();
            for d in &t.depends_on {
                dependents[index[d]].push(i);
            }
        }
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;
        let mut ready: BinaryHeap<Reverse<usize>> =
            (0..n).filter(|&i| indegree[i] == 0).map(Reverse).collect();
        let mut topo = Vec::with_capacity(n);
        while let Some(Reverse(i)) = ready.pop() {
            topo.push(i);
            for &j in &dependents[i] {
                indegree[j] -= 1;
                if indegree[j] == 0 {
                    ready.push(Reverse(j));
                }
            }
        }
        if topo.len() != n {
            let stuck: Vec<&str> = (0..n)
                .filter(|&i| indegree[i] > 0)
                .map(|i| self.tasks[i].name.0.as_str())
                .collect();
            bail!(
                "plan has a dependency cycle involving: {}",
                stuck.join(", ")
            );
        }
        // One native conversation is serial, so shared sessions require an ordering path.
        let reaches = |from: usize, to: usize| {
            let mut stack = vec![from];
            let mut seen = BTreeSet::new();
            while let Some(i) = stack.pop() {
                if !seen.insert(i) {
                    continue;
                }
                for dependent in &dependents[i] {
                    if *dependent == to {
                        return true;
                    }
                    stack.push(*dependent);
                }
            }
            false
        };
        let mut sessions: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for (i, task) in self.tasks.iter().enumerate() {
            if let Some(session) = task.session.as_deref() {
                sessions.entry(session).or_default().push(i);
            }
        }
        for (session, tasks) in sessions {
            for (offset, left) in tasks.iter().enumerate() {
                for right in &tasks[offset + 1..] {
                    if !reaches(*left, *right) && !reaches(*right, *left) {
                        bail!(
                            "tasks {:?} and {:?} share session {:?} but are not dependency-ordered",
                            self.tasks[*left].name.0,
                            self.tasks[*right].name.0,
                            session
                        );
                    }
                }
            }
        }
        Ok(ValidPlan { plan: self, topo })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str, deps: &[&str]) -> Task {
        Task {
            name: name.into(),
            task: TaskKind::Agent {
                prompt: "p".into(),
                harness: None,
                model: None,
                effort: None,
            },
            depends_on: deps.iter().map(|d| (*d).into()).collect(),
            session: None,
            needs: "any".into(),
            required: true,
            isolation: None,
            join: Join::default(),
        }
    }

    fn plan(tasks: Vec<Task>) -> Plan {
        Plan {
            version: 1,
            reason: None,
            budget: PlanBudget { usd: 5.0 },
            tasks,
        }
    }

    #[test]
    fn valid_chain_topo_orders_dependencies_first() {
        let p = plan(vec![
            agent("b", &["a"]),
            agent("a", &[]),
            agent("c", &["b"]),
        ]);
        let v = p.validate().unwrap();
        let order: Vec<&str> = v.tasks_topo().map(|t| t.name.0.as_str()).collect();
        let pos = |n: &str| order.iter().position(|x| *x == n).unwrap();
        assert!(pos("a") < pos("b"));
        assert!(pos("b") < pos("c"));
    }

    #[test]
    fn topo_is_declaration_stable_for_independent_tasks() {
        let p = plan(vec![agent("z", &[]), agent("m", &[]), agent("a", &[])]);
        let v = p.validate().unwrap();
        let order: Vec<&str> = v.tasks_topo().map(|t| t.name.0.as_str()).collect();
        assert_eq!(
            order,
            vec!["z", "m", "a"],
            "ties break in declaration order"
        );
    }

    #[test]
    fn duplicate_names_rejected() {
        let err = plan(vec![agent("a", &[]), agent("a", &[])])
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("duplicate task name"));
    }

    #[test]
    fn unknown_dependency_rejected() {
        let err = plan(vec![agent("a", &["ghost"])]).validate().unwrap_err();
        assert!(err.to_string().contains("unknown task"));
    }

    #[test]
    fn self_dependency_rejected() {
        let err = plan(vec![agent("a", &["a"])]).validate().unwrap_err();
        assert!(err.to_string().contains("depends on itself"));
    }

    #[test]
    fn cycle_rejected_and_named() {
        let err = plan(vec![agent("a", &["b"]), agent("b", &["a"])])
            .validate()
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cycle"));
        assert!(msg.contains('a') && msg.contains('b'));
    }

    #[test]
    fn unsupported_plan_versions_are_rejected() {
        for version in [0, 2, u32::MAX] {
            let mut p = plan(vec![agent("a", &[])]);
            p.version = version;
            let err = p.validate().unwrap_err();
            assert!(err.to_string().contains("supports only version 1"));
        }
    }

    #[test]
    fn shared_sessions_must_be_serial_and_nonisolated() {
        let mut first = agent("first", &[]);
        first.session = Some("solver".into());
        let mut next = agent("next", &["first"]);
        next.session = Some("solver".into());
        assert!(plan(vec![first.clone(), next]).validate().is_ok());

        let mut racing = agent("racing", &[]);
        racing.session = Some("solver".into());
        let err = plan(vec![first.clone(), racing]).validate().unwrap_err();
        assert!(err.to_string().contains("not dependency-ordered"));

        first.isolation = Some(Isolation::Worktree);
        let err = plan(vec![first]).validate().unwrap_err();
        assert!(err.to_string().contains("cannot use disposable isolation"));
    }

    #[test]
    fn zero_or_negative_budget_rejected() {
        for usd in [0.0, -1.0, f64::NAN] {
            let mut p = plan(vec![agent("a", &[])]);
            p.budget = PlanBudget { usd };
            assert!(p.validate().is_err(), "budget {usd} should be rejected");
        }
    }

    #[test]
    fn top_k_without_dependencies_rejected() {
        let t = Task {
            name: "pick".into(),
            task: TaskKind::TopK {
                k: 1,
                direction: Direction::Lower,
            },
            depends_on: vec![],
            session: None,
            needs: "any".into(),
            required: true,
            isolation: None,
            join: Join::default(),
        };
        let err = plan(vec![t]).validate().unwrap_err();
        assert!(err.to_string().contains("at least one dependency"));
    }

    #[test]
    fn engine_tasks_are_authorable_data_but_legacy_kind_aliases_are_rejected() {
        let authored = "version = 1\n[budget]\nusd = 1.0\n[[task]]\nname = \"score\"\nkind = \"engine\"\nop = \"measure\"\n";
        let plan = Plan::from_toml_str(authored).unwrap();
        assert!(matches!(
            plan.tasks[0].task,
            TaskKind::Engine {
                op: EngineOp::Measure,
                source: None
            }
        ));

        for kind in ["engine_apply", "engine_measure", "engine_decide"] {
            let src = format!(
                "version = 1\n[budget]\nusd = 1.0\n[[task]]\nname = \"x\"\nkind = \"{kind}\"\n"
            );
            assert!(
                Plan::from_toml_str(&src).is_err(),
                "legacy kind alias {kind:?} must not parse"
            );
        }
    }

    #[test]
    fn toml_front_end_parses_all_kinds() {
        let src = r#"
            version = 1
            [budget]
            usd = 2.5

            [[task]]
            name = "propose-a"
            kind = "agent"
            prompt = "try the cache approach"
            model = "opus"
            effort = "high"

            [[task]]
            name = "propose-b"
            kind = "agent"
            prompt = "try the algorithm swap"
            harness = "hermes"

            [[task]]
            name = "measure-a"
            kind = "command"
            command = "bench.sh"
            depends_on = ["propose-a"]
            needs = "gpu"

            [[task]]
            name = "measure-b"
            kind = "command"
            command = "bench.sh"
            depends_on = ["propose-b"]
            needs = "gpu"

            [[task]]
            name = "pick"
            kind = "top_k"
            k = 1
            direction = "lower"
            depends_on = ["measure-a", "measure-b"]
        "#;
        let v = Plan::from_toml_str(src).unwrap().validate().unwrap();
        assert_eq!(v.plan().tasks.len(), 5);
        let pick = v.get(&"pick".into()).unwrap();
        assert!(matches!(
            pick.task,
            TaskKind::TopK {
                k: 1,
                direction: Direction::Lower
            }
        ));
        let b = v.get(&"propose-b".into()).unwrap();
        match &b.task {
            TaskKind::Agent { harness, .. } => assert_eq!(harness.as_deref(), Some("hermes")),
            other => panic!("expected agent task, got {other:?}"),
        }
    }

    #[test]
    fn json_front_end_round_trips() {
        let p = plan(vec![agent("a", &[]), agent("b", &["a"])]);
        let json = serde_json::to_string(&p).unwrap();
        let back = Plan::from_json_str(&json).unwrap().validate().unwrap();
        assert_eq!(back.plan().tasks.len(), 2);
    }
}
