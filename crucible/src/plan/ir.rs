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

/// The canonical loop's engine-owned stages: the driver's `World`/`Judge` calls as tasks.
/// Propose is not here — it's a plain `Agent` task; the executor never touches git or the
/// judge itself, the loop runner does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineOp {
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
/// honor it must refuse the task loudly rather than silently ignore it — see
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
    /// only pre-S3 behavior).
    #[default]
    All,
    /// Dispatch once every dependency is terminal, folding only the passing outputs — a
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
    /// An agent turn. Harness, model family, and effort are per-task knobs — the openshell
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
    /// A frozen deterministic command (the `measure_cmd` shape, generalized).
    Command { command: String },
    /// Engine-builtin deterministic fold: keep the k best upstream outputs by `score`.
    TopK { k: u32, direction: Direction },
    /// An engine-owned loop stage. Engine-constructable only: the variant is serde-skipped,
    /// so no TOML/JSON front-end can author one — packs and agent plans never sequence the
    /// `World`/`Judge` directly.
    #[serde(skip)]
    Engine(EngineOp),
}

impl TaskKind {
    /// Stable wire label for the kind (`SessionEvent::TaskResult.task_kind`, UI classes).
    pub fn label(&self) -> &'static str {
        match self {
            TaskKind::Agent { .. } => "agent",
            TaskKind::Command { .. } => "command",
            TaskKind::TopK { .. } => "top_k",
            TaskKind::Engine(EngineOp::Apply) => "engine_apply",
            TaskKind::Engine(EngineOp::Measure) => "engine_measure",
            TaskKind::Engine(EngineOp::Decide) => "engine_decide",
            TaskKind::Engine(EngineOp::MeasureDiff) => "engine_measure_diff",
        }
    }
}

fn default_needs() -> String {
    "any".to_string()
}
fn default_required() -> bool {
    true
}

/// One unit of work in a plan (vocabulary per ADR-0022: task, never node/stage/step/rung).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub name: TaskName,
    #[serde(flatten)]
    pub task: TaskKind,
    #[serde(default)]
    pub depends_on: Vec<TaskName>,
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

/// Executor-enforced ceiling. Spending past it fails closed — it is not advice.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PlanBudget {
    pub usd: f64,
}

/// A versioned work graph. Frozen once validated; a replan is a new `Plan` with
/// `version + 1` and a recorded `reason`, never a mutation.
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
/// The executor only accepts this type — an unvalidated `Plan` cannot run.
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

    /// Tasks in dependency-first order.
    pub fn tasks_topo(&self) -> impl Iterator<Item = &Task> {
        self.topo.iter().map(|&i| &self.plan.tasks[i])
    }

    #[allow(dead_code)] // test-only until the judge wiring consumes plans by task name
    pub fn get(&self, name: &TaskName) -> Option<&Task> {
        self.plan.tasks.iter().find(|t| &t.name == name)
    }
}

impl Plan {
    /// Parse the agent-emitted `PLAN.json` sentinel body. Parsing is not admission —
    /// call `validate` then `AdmissionCaps::admit` before anything runs.
    pub fn from_json_str(s: &str) -> Result<Plan> {
        serde_json::from_str(s).context("PLAN.json does not parse as a plan")
    }

    /// Parse the pack-authored TOML form (`version`, `[budget]`, `[[task]]`).
    pub fn from_toml_str(s: &str) -> Result<Plan> {
        toml::from_str(s).context("plan TOML does not parse")
    }

    /// Structural validation: unique names, edges resolve, acyclic, budget declared,
    /// replans carry a reason. Returns the frozen, topologically ordered plan.
    pub fn validate(self) -> Result<ValidPlan> {
        if self.version == 0 {
            bail!("plan version must be >= 1");
        }
        if self.version > 1 && self.reason.as_deref().unwrap_or("").trim().is_empty() {
            bail!(
                "plan version {} is a replan and must record a reason",
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
        }
        // Kahn's algorithm; leftovers mean a cycle. The ready set is a min-heap on the
        // declaration index so the order is deterministic and declaration-stable — ties
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
        Ok(ValidPlan { plan: self, topo })
    }
}

/// The authorship/admission split: an agent may *write* any plan; what *runs* is capped by
/// grants the manifest or a human already made. A plan cannot raise its own ceiling.
// Test-only until the PLAN.json sentinel drain lands and admits through this.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub struct AdmissionCaps {
    pub max_usd: f64,
    pub max_tasks: usize,
}

impl AdmissionCaps {
    #[allow(dead_code)] // see the struct note
    pub fn admit(&self, plan: &ValidPlan) -> Result<()> {
        let p = plan.plan();
        if p.budget.usd > self.max_usd {
            bail!(
                "plan budget ${} exceeds the admitted cap ${} — plans cannot self-authorize spend",
                p.budget.usd,
                self.max_usd
            );
        }
        if p.tasks.len() > self.max_tasks {
            bail!(
                "plan declares {} tasks, cap is {} — plans cannot self-authorize scope",
                p.tasks.len(),
                self.max_tasks
            );
        }
        Ok(())
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
    fn replan_without_reason_rejected() {
        let mut p = plan(vec![agent("a", &[])]);
        p.version = 2;
        let err = p.validate().unwrap_err();
        assert!(err.to_string().contains("must record a reason"));
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
            needs: "any".into(),
            required: true,
            isolation: None,
            join: Join::default(),
        };
        let err = plan(vec![t]).validate().unwrap_err();
        assert!(err.to_string().contains("at least one dependency"));
    }

    #[test]
    fn admission_caps_budget() {
        let v = plan(vec![agent("a", &[])]).validate().unwrap();
        let err = AdmissionCaps {
            max_usd: 1.0,
            max_tasks: 10,
        }
        .admit(&v)
        .unwrap_err();
        assert!(err.to_string().contains("cannot self-authorize spend"));
        assert!(
            AdmissionCaps {
                max_usd: 5.0,
                max_tasks: 10
            }
            .admit(&v)
            .is_ok()
        );
    }

    #[test]
    fn admission_caps_task_count() {
        let v = plan(vec![agent("a", &[]), agent("b", &[])])
            .validate()
            .unwrap();
        let err = AdmissionCaps {
            max_usd: 100.0,
            max_tasks: 1,
        }
        .admit(&v)
        .unwrap_err();
        assert!(err.to_string().contains("cannot self-authorize scope"));
    }

    #[test]
    fn engine_tasks_are_not_authorable() {
        for kind in ["engine", "engine_apply", "engine_measure", "engine_decide"] {
            let src = format!(
                "version = 1\n[budget]\nusd = 1.0\n[[task]]\nname = \"x\"\nkind = \"{kind}\"\n"
            );
            assert!(
                Plan::from_toml_str(&src).is_err(),
                "kind {kind:?} must not parse — engine tasks are engine-constructable only"
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
