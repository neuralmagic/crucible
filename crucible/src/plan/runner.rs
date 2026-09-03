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
use crate::plan::ir::{Direction, Task, TaskKind, TaskName};

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
        self.run_in_workdir(task, inputs)
    }
}

impl ShellRunner {
    /// Run in a worktree prepared by the outer runner without clearing its isolation marker.
    pub fn run_in_prepared_worktree(
        &mut self,
        task: &Task,
        inputs: &BTreeMap<TaskName, Value>,
    ) -> Attempt {
        if task.isolation != Some(crate::plan::ir::Isolation::Worktree) {
            return fail(format!(
                "task {} was not declared for worktree isolation",
                task.name
            ));
        }
        self.run_in_workdir(task, inputs)
    }

    fn run_in_workdir(&mut self, task: &Task, inputs: &BTreeMap<TaskName, Value>) -> Attempt {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").current_dir(&self.workdir);
        cmd.env(crate::plan::TASK_NAME_ENV, &task.name.0);
        match serde_json::to_string(inputs) {
            Ok(json) => {
                cmd.env("CRUCIBLE_INPUTS", json);
            }
            Err(e) => {
                return fail(format!("inputs not serializable: {e}"));
            }
        }
        match &task.task {
            TaskKind::Command { command } | TaskKind::Evaluate { command, .. } => {
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
            TaskKind::Report {
                template, result, ..
            } => {
                return match crucible_broker::report::deliver(
                    Some(template),
                    result.as_ref().map(|name| name.0.as_str()),
                ) {
                    Ok(output) => Attempt {
                        outcome: AttemptOutcome::Pass(
                            serde_json::from_str(&output).unwrap_or_else(|_| {
                                serde_json::json!({
                                    "status": "delivered"
                                })
                            }),
                        ),
                        cost_usd: 0.0,
                    },
                    Err(error) => fail(error),
                };
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
                stderr_tail(&stderr)
            ));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let Some(last) = stdout.lines().rev().find(|l| !l.trim().is_empty()) else {
            return fail("no output: expected a JSON result on the last stdout line".to_string());
        };
        match serde_json::from_str::<Value>(last.trim()) {
            Ok(v) => evaluation_attempt(task, v),
            Err(e) => fail(format!(
                "last stdout line is not JSON ({e}): {}",
                last.trim()
            )),
        }
    }
}

/// Lines kept from the end of stderr. A build/pytest verdict plus its immediate
/// context fits well inside this; more would drown the row.
const TAIL_LINES: usize = 40;
/// Per-line clip. Long torch stack frames and nvcc command echoes are mostly path
/// noise, and one of them must not eat the whole budget.
const TAIL_LINE_CHARS: usize = 240;
/// Whole-tail clip.
const TAIL_CHARS: usize = 4000;

/// Compress a process's stderr into a single line for a failure note.
///
/// The note is rendered into a markdown table cell in RESULTS.md, so the result must
/// be one line with no unescaped `|`. Truncation drops from the FRONT because the
/// verdict (the compiler error, the pytest summary) is at the end.
fn stderr_tail(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let joined = lines[lines.len().saturating_sub(TAIL_LINES)..]
        .iter()
        .map(|line| line.chars().take(TAIL_LINE_CHARS).collect::<String>())
        .map(|line| line.replace('|', "\\|"))
        .collect::<Vec<_>>()
        .join(" ⏎ ");
    let over = joined.chars().count().saturating_sub(TAIL_CHARS);
    if over == 0 {
        return joined;
    }
    // One char of the budget goes to the marker so the note admits it lost its head.
    std::iter::once('…')
        .chain(joined.chars().skip(over + 1))
        .collect()
}

/// Broker log handles an evaluation record may carry (`"logs": ["job-abc.log"]`).
/// Handles are bare filenames minted by the broker's log store, so a `|` or a
/// newline means the record is lying; escape rather than trust it, since the note
/// lands in a markdown table cell.
fn log_handles(object: &serde_json::Map<String, Value>) -> Vec<String> {
    let Some(array) = object.get("logs").and_then(Value::as_array) else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(Value::as_str)
        .map(|handle| {
            handle
                .chars()
                .filter(|c| !c.is_control())
                .collect::<String>()
                .replace('|', "\\|")
                .trim()
                .to_string()
        })
        .filter(|handle| !handle.is_empty())
        .collect()
}

fn evaluation_attempt(task: &Task, mut value: Value) -> Attempt {
    let TaskKind::Evaluate {
        threshold,
        direction,
        ..
    } = task.task
    else {
        return Attempt {
            outcome: AttemptOutcome::Pass(value),
            cost_usd: 0.0,
        };
    };
    let Some(object) = value.as_object_mut() else {
        return fail("evaluate result must be a JSON object".to_string());
    };
    // `pass` may narrow a threshold gate, never widen it.
    let within_threshold = match (threshold, direction) {
        (Some(threshold), Some(Direction::Lower)) => object
            .get("score")
            .and_then(Value::as_f64)
            .is_some_and(|score| score <= threshold),
        (Some(threshold), Some(Direction::Higher)) => object
            .get("score")
            .and_then(Value::as_f64)
            .is_some_and(|score| score >= threshold),
        (None, None) => true,
        _ => false,
    };
    let declared_pass = match object.get("pass") {
        None => true,
        Some(Value::Bool(passed)) => *passed,
        Some(_) => return fail("evaluate result `pass` must be a boolean".to_string()),
    };
    let passed = within_threshold && declared_pass;
    object.insert("pass".to_string(), Value::Bool(passed));
    // A passing record needs no suffix: the whole object, `logs` included, flows on as
    // the task output. Only the fail note is lossy, so that is where handles are spent.
    let handles = log_handles(object);
    if passed {
        Attempt {
            outcome: AttemptOutcome::Pass(value),
            cost_usd: 0.0,
        }
    } else {
        fail(format!(
            "evaluation {} did not pass{}{}",
            task.name,
            threshold
                .map(|threshold| format!(" threshold {threshold}"))
                .unwrap_or_default(),
            if handles.is_empty() {
                String::new()
            } else {
                format!(" logs: {}", handles.join(","))
            }
        ))
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
    use crate::plan::ir::{Direction, Join, Plan, PlanBudget, Stage};

    fn runner() -> ShellRunner {
        ShellRunner {
            workdir: std::env::temp_dir(),
            agent_cmd: None,
        }
    }

    fn evaluate(name: &str, cmd: &str, threshold: Option<f64>) -> Task {
        Task {
            name: name.into(),
            task: TaskKind::Evaluate {
                command: cmd.into(),
                threshold,
                direction: threshold.map(|_| Direction::Lower),
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
            stage: Stage::Iteration,
            emits: Vec::new(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
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
    fn evaluate_enforces_threshold_and_normalizes_pass() {
        let evaluate = |name: &str, score: f64| Task {
            name: name.into(),
            task: TaskKind::Evaluate {
                command: format!("echo '{{\"score\": {score}}}'"),
                threshold: Some(10.0),
                direction: Some(Direction::Lower),
            },
            depends_on: vec![],
            session: None,
            needs: "any".into(),
            required: true,
            isolation: None,
            join: Join::All,
            stage: Stage::Iteration,
            emits: Vec::new(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
        };
        let passed = run_plan(vec![evaluate("latency", 9.5)], None);
        assert_eq!(passed.results[&"latency".into()].status, TaskStatus::Pass);
        assert_eq!(
            passed.results[&"latency".into()].output.as_ref().unwrap()["pass"],
            true
        );
        let failed = run_plan(vec![evaluate("latency", 10.5)], None);
        assert_eq!(failed.results[&"latency".into()].status, TaskStatus::Fail);
    }

    #[test]
    fn explicit_pass_narrows_a_threshold_but_cannot_widen_it() {
        let evaluate = |name: &str, output: &str| Task {
            name: name.into(),
            task: TaskKind::Evaluate {
                command: format!("echo '{output}'"),
                threshold: Some(1.0),
                direction: Some(Direction::Lower),
            },
            depends_on: vec![],
            session: None,
            needs: "any".into(),
            required: false,
            isolation: None,
            join: Join::All,
            stage: Stage::Iteration,
            emits: Vec::new(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
        };
        let over = run_plan(
            vec![evaluate("over", r#"{"score": 100, "pass": true}"#)],
            None,
        );
        assert_eq!(over.results[&"over".into()].status, TaskStatus::Fail);
        let objected = run_plan(
            vec![evaluate("objected", r#"{"score": 0.5, "pass": false}"#)],
            None,
        );
        assert_eq!(
            objected.results[&"objected".into()].status,
            TaskStatus::Fail
        );
        let ok = run_plan(vec![evaluate("ok", r#"{"score": 0.5}"#)], None);
        assert_eq!(ok.results[&"ok".into()].status, TaskStatus::Pass);
    }

    #[test]
    fn an_unthresholded_evaluation_is_decided_by_its_script() {
        let evaluate = |name: &str, output: &str| Task {
            name: name.into(),
            task: TaskKind::Evaluate {
                command: format!("echo '{output}'"),
                threshold: None,
                direction: None,
            },
            depends_on: vec![],
            session: None,
            needs: "any".into(),
            required: false,
            isolation: None,
            join: Join::All,
            stage: Stage::Iteration,
            emits: Vec::new(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
        };
        let green = run_plan(vec![evaluate("green", r#"{"pass": true}"#)], None);
        assert_eq!(green.results[&"green".into()].status, TaskStatus::Pass);
        let red = run_plan(vec![evaluate("red", r#"{"pass": false}"#)], None);
        assert_eq!(red.results[&"red".into()].status, TaskStatus::Fail);
    }

    #[test]
    fn malformed_evaluation_pass_fails_closed() {
        let task = Task {
            name: "malformed".into(),
            task: TaskKind::Evaluate {
                command: r#"echo '{"pass": "false"}'"#.into(),
                threshold: None,
                direction: None,
            },
            depends_on: vec![],
            session: None,
            needs: "any".into(),
            required: false,
            isolation: None,
            join: Join::All,
            stage: Stage::Iteration,
            emits: Vec::new(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
        };
        let out = run_plan(vec![task], None);
        let result = &out.results[&"malformed".into()];
        assert_eq!(result.status, TaskStatus::Fail);
        assert!(
            result
                .note
                .as_deref()
                .unwrap_or_default()
                .contains("boolean"),
            "failure explains the contract: {result:?}"
        );
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
            stage: Stage::Iteration,
            emits: Vec::new(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
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
            stage: Stage::Iteration,
            emits: Vec::new(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
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
            stage: Stage::Iteration,
            emits: Vec::new(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
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
            stage: Stage::Iteration,
            emits: Vec::new(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
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

    #[test]
    fn stderr_tail_drops_blank_lines_and_keeps_order() {
        assert_eq!(stderr_tail(""), "");
        assert_eq!(stderr_tail("\n\n   \n"), "");
        assert_eq!(stderr_tail("a\n\n  b  \n\nc\n"), "a ⏎ b ⏎ c");
    }

    #[test]
    fn stderr_tail_clips_each_line() {
        let long = "x".repeat(1000);
        let tail = stderr_tail(&format!("{long}\nverdict\n"));
        let first = tail.split(" ⏎ ").next().unwrap();
        assert_eq!(first.chars().count(), TAIL_LINE_CHARS);
        assert!(tail.ends_with("verdict"), "{tail}");
    }

    #[test]
    fn stderr_tail_keeps_only_the_last_lines() {
        let stderr: String = (0..100).map(|i| format!("line{i}\n")).collect();
        let tail = stderr_tail(&stderr);
        let lines: Vec<&str> = tail.split(" ⏎ ").collect();
        assert_eq!(lines.len(), TAIL_LINES);
        assert_eq!(lines[0], "line60");
        assert_eq!(lines[TAIL_LINES - 1], "line99");
    }

    #[test]
    fn stderr_tail_truncates_from_the_front() {
        // 40 lines x 240 chars blows the whole-tail budget; the verdict at the end
        // is exactly what must survive.
        let mut stderr: String = (0..39).map(|_| format!("{}\n", "y".repeat(300))).collect();
        stderr.push_str("error: the actual verdict\n");
        let tail = stderr_tail(&stderr);
        assert_eq!(tail.chars().count(), TAIL_CHARS);
        assert!(tail.starts_with('…'), "{}", &tail[..8]);
        assert!(tail.ends_with("error: the actual verdict"), "{tail}");
    }

    #[test]
    fn stderr_tail_escapes_pipes() {
        assert_eq!(stderr_tail("a | b\n"), "a \\| b");
    }

    #[test]
    fn nonzero_exit_note_carries_a_multi_line_tail() {
        let out = run_plan(
            vec![command("boom", "echo err1 >&2; echo err2 >&2; exit 3", &[])],
            None,
        );
        let r = &out.results[&"boom".into()];
        assert_eq!(r.status, TaskStatus::Fail);
        assert_eq!(r.note.as_deref(), Some("exit 3: err1 ⏎ err2"));
    }

    #[test]
    fn failed_evaluation_note_carries_the_log_handles() {
        let out = run_plan(
            vec![evaluate(
                "gate",
                r#"echo '{"score": 50, "logs": ["job-a.log", "job-b.log"]}'"#,
                Some(10.0),
            )],
            None,
        );
        let note = out.results[&"gate".into()].note.clone().unwrap();
        assert!(note.contains("threshold 10"), "{note}");
        assert!(note.ends_with(" logs: job-a.log,job-b.log"), "{note}");
    }

    #[test]
    fn evaluation_log_handles_ignore_non_strings_and_escape_pipes() {
        // One plan per record: a required task's failure blocks whatever follows it.
        let note = |command: &str| {
            let out = run_plan(vec![evaluate("gate", command, Some(10.0))], None);
            out.results[&"gate".into()].note.clone().unwrap()
        };
        let mixed = note(r#"echo '{"score": 50, "logs": ["ok.log", 7, null, ""]}'"#);
        assert!(mixed.ends_with(" logs: ok.log"), "{mixed}");
        let hostile = note(r#"echo '{"score": 50, "logs": ["a|b.log"]}'"#);
        assert!(hostile.ends_with(" logs: a\\|b.log"), "{hostile}");
        let empty = note(r#"echo '{"score": 50, "logs": []}'"#);
        assert!(!empty.contains("logs:"), "{empty}");
    }

    #[test]
    fn passing_evaluation_keeps_the_log_handles_in_its_output() {
        let out = run_plan(
            vec![evaluate(
                "gate",
                r#"echo '{"score": 5, "logs": ["job-a.log"]}'"#,
                Some(10.0),
            )],
            None,
        );
        let r = &out.results[&"gate".into()];
        assert_eq!(r.status, TaskStatus::Pass);
        let output = r.output.as_ref().unwrap();
        assert_eq!(output["pass"], true);
        assert_eq!(output["logs"][0], "job-a.log");
    }
}
