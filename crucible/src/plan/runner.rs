//! A real runner: tasks execute as subprocesses, nothing is simulated. `Command` tasks run
//! their plan-authored command; `Agent` tasks run the command-backend stand-in supplied by the
//! caller (the same trick as `AgentBackend::Command`: real process, scripted brain). The
//! full harness/broker runners replace this one; the executor contract is identical.
//!
//! Output contract, mirroring `measure_cmd`: the last non-empty stdout line is the task's
//! JSON result. Nonzero exit is a measured failure; failure to spawn is transport.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

use crate::plan::exec::{Attempt, AttemptOutcome, TaskRunner};
use crate::plan::ir::{Task, TaskKind, TaskName};

pub struct ShellRunner {
    pub workdir: PathBuf,
    /// Stand-in command for `Agent` tasks (receives the prompt and knobs via env). `None`
    /// means agent tasks are refused: `plan run` without `--agent-cmd` is command-only.
    pub agent_cmd: Option<String>,
}

impl TaskRunner for ShellRunner {
    fn run(&mut self, task: &Task, _attempt: u32, inputs: &BTreeMap<TaskName, Value>) -> Attempt {
        if task.isolation.is_some() {
            // Fail loud: a runner that quietly ran an isolation-marked task in the shared
            // workdir would hand back a result the plan's author has no reason to trust.
            return fail(format!(
                "task {} requests isolation, which this runner cannot provide (run the plan \
                 with --manifest so tasks get a real workspace to clone)",
                task.name
            ));
        }
        let mut cmd = Command::new("sh");
        cmd.arg("-c").current_dir(&self.workdir);
        cmd.env("CRUCIBLE_TASK", &task.name.0);
        match serde_json::to_string(inputs) {
            Ok(json) => {
                cmd.env("CRUCIBLE_INPUTS", json);
            }
            Err(e) => {
                return fail(format!("inputs not serializable: {e}"));
            }
        }
        match &task.task {
            TaskKind::Command { command } => {
                cmd.arg(command);
            }
            TaskKind::Agent {
                prompt,
                harness,
                model,
                effort,
            } => {
                let Some(agent_cmd) = &self.agent_cmd else {
                    return fail(
                        "agent task refused: this runner has no agent command (pass --agent-cmd)"
                            .to_string(),
                    );
                };
                cmd.arg(agent_cmd);
                cmd.env("CRUCIBLE_PROMPT", prompt);
                if let Some(h) = harness {
                    cmd.env("CRUCIBLE_HARNESS", h);
                }
                if let Some(m) = model {
                    cmd.env("CRUCIBLE_MODEL", m);
                }
                if let Some(e) = effort {
                    cmd.env("CRUCIBLE_EFFORT", e);
                }
            }
            TaskKind::TopK { .. } => {
                // The executor owns reducers; reaching the runner is an executor bug.
                return fail("reducer task reached the runner".to_string());
            }
            TaskKind::Engine { .. } => {
                // Engine ops only exist in the loop's canonical template; its runner
                // handles them, this one can't.
                return fail("engine task reached a non-loop runner".to_string());
            }
        }
        let out = match cmd.output() {
            Ok(out) => out,
            Err(e) => {
                return Attempt {
                    outcome: AttemptOutcome::Transport(format!("spawn failed: {e}")),
                    cost_usd: 0.0,
                };
            }
        };
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return fail(format!(
                "exit {}: {}",
                out.status.code().unwrap_or(-1),
                stderr.lines().last().unwrap_or("").trim()
            ));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let Some(last) = stdout.lines().rev().find(|l| !l.trim().is_empty()) else {
            return fail("no output: expected a JSON result on the last stdout line".to_string());
        };
        match serde_json::from_str::<Value>(last.trim()) {
            Ok(v) => Attempt {
                outcome: AttemptOutcome::Pass(v),
                cost_usd: 0.0,
            },
            Err(e) => fail(format!(
                "last stdout line is not JSON ({e}): {}",
                last.trim()
            )),
        }
    }
}

fn fail(note: String) -> Attempt {
    Attempt {
        outcome: AttemptOutcome::Fail(note),
        cost_usd: 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::exec::{ExecCfg, PlanExit, Substrate, TaskStatus, execute};
    use crate::plan::ir::{Direction, Join, Plan, PlanBudget};

    fn runner() -> ShellRunner {
        ShellRunner {
            workdir: std::env::temp_dir(),
            agent_cmd: None,
        }
    }

    fn command(name: &str, cmd: &str, deps: &[&str]) -> Task {
        Task {
            name: name.into(),
            task: TaskKind::Command {
                command: cmd.into(),
            },
            depends_on: deps.iter().map(|d| (*d).into()).collect(),
            session: None,
            needs: "any".into(),
            required: true,
            isolation: None,
            join: Join::default(),
        }
    }

    fn run_plan(tasks: Vec<Task>, agent_cmd: Option<&str>) -> crate::plan::exec::PlanOutcome {
        let plan = Plan {
            version: 1,
            reason: None,
            budget: PlanBudget { usd: 1.0 },
            tasks,
        }
        .validate()
        .unwrap();
        let mut r = ShellRunner {
            agent_cmd: agent_cmd.map(String::from),
            ..runner()
        };
        execute(
            &plan,
            &Substrate::default(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        )
    }

    #[test]
    fn real_commands_flow_real_json_downstream() {
        // Upstream emits a score; downstream proves it actually received it via
        // CRUCIBLE_INPUTS by extracting and re-emitting the value with real tools.
        let out = run_plan(
            vec![
                command("emit", r#"echo '{"score": 7}'"#, &[]),
                command(
                    "check",
                    r#"python3 -c 'import json,os; v=json.loads(os.environ["CRUCIBLE_INPUTS"]); print(json.dumps({"score": v["emit"]["score"] * 2}))'"#,
                    &["emit"],
                ),
            ],
            None,
        );
        assert!(out.valid, "{:?}", out.results);
        assert_eq!(
            out.results[&"check".into()].output.as_ref().unwrap()["score"],
            14
        );
    }

    #[test]
    fn nonzero_exit_is_a_measured_failure_not_transport() {
        let out = run_plan(vec![command("boom", "echo doomed >&2; exit 3", &[])], None);
        let r = &out.results[&"boom".into()];
        assert_eq!(r.status, TaskStatus::Fail);
        assert_eq!(r.attempts, 1, "measured failures never retry");
        assert!(r.note.as_ref().unwrap().contains("exit 3"));
        assert!(r.note.as_ref().unwrap().contains("doomed"));
    }

    #[test]
    fn non_json_output_is_a_measured_failure() {
        let out = run_plan(vec![command("chatty", "echo not json at all", &[])], None);
        let r = &out.results[&"chatty".into()];
        assert_eq!(r.status, TaskStatus::Fail);
        assert!(r.note.as_ref().unwrap().contains("not JSON"));
    }

    #[test]
    fn agent_task_without_agent_cmd_is_refused() {
        let t = Task {
            name: "a".into(),
            task: TaskKind::Agent {
                prompt: "go".into(),
                harness: None,
                model: None,
                effort: None,
            },
            depends_on: vec![],
            session: None,
            needs: "any".into(),
            required: true,
            isolation: None,
            join: Join::default(),
        };
        let out = run_plan(vec![t], None);
        assert_eq!(out.results[&"a".into()].status, TaskStatus::Fail);
        assert!(matches!(out.exit, PlanExit::ShortCircuit { .. }));
    }

    #[test]
    fn agent_stand_in_receives_prompt_and_knobs() {
        let t = Task {
            name: "a".into(),
            task: TaskKind::Agent {
                prompt: "improve the cache".into(),
                harness: Some("hermes".into()),
                model: Some("codex".into()),
                effort: None,
            },
            depends_on: vec![],
            session: None,
            needs: "any".into(),
            required: true,
            isolation: None,
            join: Join::default(),
        };
        let out = run_plan(
            vec![t],
            Some(
                r#"python3 -c 'import json,os; print(json.dumps({"prompt": os.environ["CRUCIBLE_PROMPT"], "harness": os.environ["CRUCIBLE_HARNESS"], "model": os.environ["CRUCIBLE_MODEL"]}))'"#,
            ),
        );
        assert!(out.valid, "{:?}", out.results);
        let v = out.results[&"a".into()].output.as_ref().unwrap();
        assert_eq!(v["prompt"], "improve the cache");
        assert_eq!(v["harness"], "hermes");
        assert_eq!(v["model"], "codex");
    }

    /// A runner that cannot isolate must say so, not quietly run the task in the shared
    /// workdir and hand back a result the plan's author has no reason to trust.
    #[test]
    fn isolation_request_is_refused_loudly_by_the_shell_runner() {
        let mut t = command("iso", "echo '{}'", &[]);
        t.isolation = Some(crate::plan::ir::Isolation::Worktree);
        let out = run_plan(vec![t], None);
        let r = &out.results[&"iso".into()];
        assert_eq!(r.status, TaskStatus::Fail);
        assert!(
            r.note.as_ref().unwrap().contains("cannot provide"),
            "the refusal names the problem: {:?}",
            r.note
        );
    }

    #[test]
    fn end_to_end_tournament_with_real_processes() {
        // The wide shape: two proposers (scripted agents), two real measures, one fold.
        // Scores are computed by the measure commands, not injected by the test.
        let agent = |name: &str| Task {
            name: name.into(),
            task: TaskKind::Agent {
                prompt: format!("approach {name}"),
                harness: None,
                model: None,
                effort: None,
            },
            depends_on: vec![],
            session: None,
            needs: "any".into(),
            required: true,
            isolation: None,
            join: Join::default(),
        };
        let measure = |name: &str, dep: &str| {
            command(
                name,
                // Real work: score = byte length of the upstream approach string.
                r#"python3 -c 'import json,os; v=json.loads(os.environ["CRUCIBLE_INPUTS"]); a=list(v.values())[0]["approach"]; print(json.dumps({"score": len(a)}))'"#,
                &[dep],
            )
        };
        let pick = Task {
            name: "pick".into(),
            task: TaskKind::TopK {
                k: 1,
                direction: Direction::Lower,
            },
            depends_on: vec!["m-short".into(), "m-long".into()],
            session: None,
            needs: "any".into(),
            required: true,
            isolation: None,
            join: Join::default(),
        };
        let out = run_plan(
            vec![
                agent("p-short"),
                agent("p-longer-name"),
                measure("m-short", "p-short"),
                measure("m-long", "p-longer-name"),
                pick,
            ],
            Some(r#"echo "{\"approach\": \"$CRUCIBLE_PROMPT\"}""#),
        );
        assert!(out.valid, "{:?}", out.results);
        let kept = &out.results[&"pick".into()].output.as_ref().unwrap()["kept"];
        // "approach p-short" is shorter than "approach p-longer-name": lower wins.
        assert_eq!(kept[0]["task"], "m-short");
    }
}
