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

use crate::event::{AgentEvent, RawStream};
use crate::plan::exec::{Attempt, AttemptOutcome, BatchItem, TaskRunner};
use crate::plan::ir::{Isolation, Task, TaskKind, TaskName};
use crate::plan::runner::ShellRunner;
use crate::{Args, Paths};

const RESULT_FILE: &str = "PLAN_TASK_RESULT.json";

pub struct HarnessRunner {
    pub args: Args,
    pub paths: Paths,
}

impl TaskRunner for HarnessRunner {
    fn run(&mut self, task: &Task, attempt: u32, inputs: &BTreeMap<TaskName, Value>) -> Attempt {
        run_task(&self.args, &self.paths, task, attempt, inputs, None)
    }

    /// Isolated tasks that are ready together run concurrently, each in its own worktree.
    /// Concurrency is what isolation buys: two reviewers reading the same artifact would
    /// otherwise race on the single `PLAN_TASK_RESULT.json` in the shared workspace.
    fn run_many(&mut self, batch: &[BatchItem<'_>]) -> Vec<Attempt> {
        if batch.len() == 1 {
            let b = &batch[0];
            return vec![run_task(
                &self.args,
                &self.paths,
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
                return batch.iter().map(|_| fail(0.0, note.clone())).collect();
            }
        };
        std::thread::scope(|scope| {
            let handles: Vec<_> = batch
                .iter()
                .map(|b| {
                    let args = self.args.clone();
                    let paths = self.paths.clone();
                    let pending = pending.as_str();
                    scope.spawn(move || {
                        run_task(&args, &paths, b.task, b.attempt, &b.inputs, Some(pending))
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| fail(0.0, "task thread panicked".to_string()))
                })
                .collect()
        })
    }
}

/// Dispatch one task, in the shared workspace or in a private worktree. `pending` is the
/// shared workspace's uncommitted patch when a concurrent caller already captured it for
/// the whole batch; `None` means capture it here.
fn run_task(
    args: &Args,
    paths: &Paths,
    task: &Task,
    attempt: u32,
    inputs: &BTreeMap<TaskName, Value>,
    pending: Option<&str>,
) -> Attempt {
    let Some(Isolation::Worktree) = task.isolation else {
        return prepare_and_run(args, paths, task, attempt, inputs);
    };
    // A private clone of the workspace. Its edits are discarded on cleanup: what leaves an
    // isolated task is its declared output, so this is for review/analysis work, not for
    // coding tasks whose diff has to survive (the wide tournament carries those out itself).
    let root = paths.state.join("plan-iso");
    if let Err(e) = std::fs::create_dir_all(&root) {
        return transport(format!("creating the isolation root failed: {e}"));
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
                return fail(
                    0.0,
                    format!("capturing the workspace's uncommitted state failed: {e:#}"),
                );
            }
        },
    };
    if let Err(e) = crate::plan::worktree::setup(&paths.workspace, &worktree, pending) {
        return transport(format!("worktree setup failed: {e:#}"));
    }
    let iso = Paths::for_worktree(worktree.clone(), paths.skills.clone());
    let _ = std::fs::create_dir_all(&iso.state);
    let attempt_out = prepare_and_run(args, &iso, task, attempt, inputs);
    let _ = std::fs::remove_dir_all(&worktree);
    attempt_out
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
            return transport(format!(
                "restoring frozen inject {} -> {} failed: {e:#}",
                src.display(),
                dst.display()
            ));
        }
    }
    run_in(args, paths, task, attempt, inputs)
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
        TaskKind::Command { .. } => {
            let mut shell = ShellRunner {
                workdir: paths.workspace.clone(),
                agent_cmd: None,
            };
            return shell.run(task, attempt, inputs);
        }
        TaskKind::TopK { .. } => {
            return fail(0.0, "reducer task reached the runner".to_string());
        }
        TaskKind::Engine { .. } => {
            return fail(0.0, "engine task reached a non-loop runner".to_string());
        }
    };

    // Per-task knob overrides on a cloned Args: the heterogeneity axis. Unknown values
    // are a measured failure: a plan naming a harness we can't parse is wrong, not
    // unlucky.
    let mut args = args.clone();
    if let Some(h) = harness {
        match crate::harness::Harness::from_str(h, true) {
            Ok(h) => args.harness = Some(h),
            Err(e) => return fail(0.0, format!("task names unknown harness {h:?}: {e}")),
        }
    }
    if let Some(m) = model {
        args.model = m.clone();
    }
    if let Some(e) = effort {
        match crate::agent::ReasoningEffort::from_str(e, true) {
            Ok(e) => args.reasoning_effort = Some(e),
            Err(err) => return fail(0.0, format!("task names unknown effort {e:?}: {err}")),
        }
    }
    if let Err(e) = crate::run::install_toolbox(
        paths,
        &args.workflow_toolbox_exclude,
        args.harness().skills_dir(),
    ) {
        return transport(format!("installing the task toolbox failed: {e:#}"));
    }

    let inputs_json = match serde_json::to_string_pretty(inputs) {
        Ok(j) => j,
        Err(e) => return fail(0.0, format!("inputs not serializable: {e}")),
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
    let mut transport_error: Option<String> = None;
    let cost = crate::agent::run_turn(&args, paths, &full_prompt, false, |line, stream, ev| {
        if !line.trim().is_empty() && stream == RawStream::Stderr {
            eprintln!("[{name}] {line}");
        }
        if let Some(note) = ev.and_then(agent_transport_error) {
            transport_error = Some(note);
        }
    });

    match std::fs::read_to_string(&result_path) {
        Ok(body) => {
            let _ = std::fs::remove_file(&result_path);
            match serde_json::from_str::<Value>(&body) {
                Ok(v) => Attempt {
                    outcome: AttemptOutcome::Pass(v),
                    cost_usd: cost,
                },
                Err(e) => fail(cost, format!("{RESULT_FILE} is not valid JSON: {e}")),
            }
        }
        Err(_) => match transport_error {
            Some(note) => Attempt {
                outcome: AttemptOutcome::Transport(note),
                cost_usd: cost,
            },
            None => fail(
                cost,
                format!("turn ended without writing {RESULT_FILE} — nothing to grade"),
            ),
        },
    }
}

fn fail(cost_usd: f64, note: String) -> Attempt {
    Attempt {
        outcome: AttemptOutcome::Fail(note),
        cost_usd,
    }
}

fn transport(note: String) -> Attempt {
    Attempt {
        outcome: AttemptOutcome::Transport(note),
        cost_usd: 0.0,
    }
}

fn agent_transport_error(event: &AgentEvent) -> Option<String> {
    match event {
        AgentEvent::Error {
            error_type,
            message,
        } => Some(format!("{error_type}: {message}")),
        AgentEvent::Result {
            is_error: true,
            error,
            ..
        } => Some(
            error
                .as_deref()
                .unwrap_or("agent turn ended with an unspecified error")
                .to_string(),
        ),
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
    use crate::plan::exec::{ExecCfg, PlanExit, Substrate, TaskStatus, execute};
    use crate::plan::ir::{Join, Plan};

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
            agent_transport_error(&overloaded).as_deref(),
            Some("overloaded: try later")
        );
        let failed_result = AgentEvent::Result {
            subtype: "success".into(),
            is_error: true,
            turns: 0,
            cost_usd: 0.0,
            error: Some("not logged in".into()),
        };
        assert_eq!(
            agent_transport_error(&failed_result).as_deref(),
            Some("not logged in")
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

        let mut runner = crate::run::prep_plan_runner(&dir.join("crucible.toml")).unwrap();
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
        let mut runner = crate::run::prep_plan_runner(&dir.join("crucible.toml")).unwrap();
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
            needs: "any".into(),
            required: true,
            isolation: None,
            join: Join::default(),
        };
        let mut runner = HarnessRunner {
            args: <crate::Cli as clap::Parser>::try_parse_from(["crucible"])
                .unwrap()
                .run,
            paths: crate::Paths::for_manifest(
                std::env::temp_dir(),
                std::env::temp_dir(),
                &std::env::temp_dir(),
                None,
            ),
        };
        let a = runner.run(&t, 1, &BTreeMap::new());
        match a.outcome {
            AttemptOutcome::Fail(note) => assert!(note.contains("unknown harness")),
            _ => panic!("expected a measured failure"),
        }
    }
}
