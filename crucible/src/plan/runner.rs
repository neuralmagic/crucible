//! A real runner: tasks execute as subprocesses, nothing is simulated. `Command` tasks run
//! their plan-authored command; `Agent` tasks run the command-backend stand-in supplied by the
//! caller (the same trick as `AgentBackend::Command`: real process, scripted brain). The
//! full harness/broker runners replace this one; the executor contract is identical.
//!
//! Output contract, mirroring `measure_cmd`: the last non-empty stdout line is the task's
//! JSON result. Nonzero exit is a measured failure; failure to spawn is transport. A command
//! still running at its deadline is killed with its whole process group and fails measured.
//! Each output line is echoed to the engine's stderr as `[task] line` while the command runs.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};

use serde_json::Value;

use crate::crucible::Direction;
use crate::deadline::{Deadline, Supervised};
use crate::plan::admission::Lane;
use crate::plan::attempt::{Attempt, AttemptOutcome, own_note};
use crate::plan::exec::TaskRunner;
use crate::plan::ir::{Decider, Task, TaskKind, TaskName};
use crucible_contract::TransportCause;
use crucible_contract::inference::{ENV_INFERENCE, InferenceRole};

/// Upstream results as JSON, set only while they fit [`INPUTS_ENV_MAX`].
pub const INPUTS_ENV: &str = "CRUCIBLE_INPUTS";
/// Path to a file holding the same JSON, always set.
pub const INPUTS_FILE_ENV: &str = "CRUCIBLE_INPUTS_FILE";
/// Linux refuses any one environment string over 128 KiB, and the whole environment shares
/// `ARG_MAX` with argv.
const INPUTS_ENV_MAX: usize = 64 * 1024;
/// The run's buildit socket, set for command and evaluate tasks alone; `buildit` calls the server
/// there.
pub const BUILDIT_SOCKET_ENV: &str = "CRUCIBLE_BUILDIT_SOCKET";

pub struct ShellRunner {
    pub workdir: PathBuf,
    /// Stand-in command for `Agent` tasks (receives the prompt and knobs via env). `None`
    /// means agent tasks are refused: `plan run` without `--agent-cmd` is command-only.
    pub agent_cmd: Option<String>,
    /// What `[workflow].env` passes to `command` and `evaluate` tasks.
    pub env: crate::host_env::HostEnv,
    /// The run's buildit socket, exported to command and evaluate tasks as
    /// [`BUILDIT_SOCKET_ENV`].
    pub buildit_socket: Option<PathBuf>,
    /// Whether the run's `crucible-git` serves GitHub to command and evaluate tasks, which then
    /// get [`crucible_contract::git::task_env`], each variable as the loop container holds it when
    /// it does, so an edition-2 task sees what an edition-1 task inherits.
    pub github: bool,
}

impl TaskRunner for ShellRunner {
    fn run(
        &mut self,
        task: &Task,
        _attempt: u32,
        inputs: &BTreeMap<TaskName, Value>,
        lane: Option<&Lane<'_>>,
    ) -> Attempt {
        let deadline = lane.and_then(Lane::deadline);
        if task.isolation.is_some() {
            // Fail loud: a runner that quietly ran an isolation-marked task in the shared
            // workdir would hand back a result the plan's author has no reason to trust.
            return Attempt::failed(
                0.0,
                format!(
                    "task {} requests isolation, which this runner cannot provide (run the plan \
                 with --manifest so tasks get a real workspace to clone)",
                    task.name
                ),
            );
        }
        self.run_in_workdir(task, inputs, deadline)
    }
}

impl ShellRunner {
    /// Run in a worktree prepared by the outer runner without clearing its isolation marker.
    pub fn run_in_prepared_worktree(
        &mut self,
        task: &Task,
        inputs: &BTreeMap<TaskName, Value>,
        deadline: Option<Deadline>,
    ) -> Attempt {
        if task.isolation != Some(crate::plan::ir::Isolation::Worktree) {
            return Attempt::failed(
                0.0,
                format!("task {} was not declared for worktree isolation", task.name),
            );
        }
        self.run_in_workdir(task, inputs, deadline)
    }

    fn run_in_workdir(
        &mut self,
        task: &Task,
        inputs: &BTreeMap<TaskName, Value>,
        deadline: Option<Deadline>,
    ) -> Attempt {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").current_dir(&self.workdir);
        if matches!(
            task.task,
            TaskKind::Command { .. } | TaskKind::Evaluate { .. }
        ) {
            self.env.apply(&mut cmd);
        }
        crate::inference::withhold_from_task(&mut cmd);
        cmd.env_remove(BUILDIT_SOCKET_ENV);
        cmd.env(crate::plan::TASK_NAME_ENV, &task.name.0);
        let mut env_inputs = inputs.clone();
        let history = match task.task {
            TaskKind::Agent { .. } => {
                env_inputs.remove(&TaskName::reserved(crate::plan::ir::HISTORY_INPUT))
            }
            _ => None,
        };
        let json = match serde_json::to_string(&env_inputs) {
            Ok(json) => json,
            Err(e) => {
                return Attempt::failed(0.0, format!("inputs not serializable: {e}"));
            }
        };
        let inputs_file = match inputs_file(&json) {
            Ok(file) => file,
            Err(e) => {
                return Attempt::transport(
                    TransportCause::Workspace,
                    format!("writing the task inputs file: {e}"),
                );
            }
        };
        cmd.env(INPUTS_FILE_ENV, inputs_file.path());
        if json.len() <= INPUTS_ENV_MAX {
            cmd.env(INPUTS_ENV, &json);
        }
        match &task.task {
            TaskKind::Command { command } | TaskKind::Evaluate { command, .. } => {
                cmd.arg(command);
                if let Some(socket) = &self.buildit_socket {
                    cmd.env(BUILDIT_SOCKET_ENV, socket);
                }
                if self.github {
                    for (name, value) in crucible_contract::git::task_env() {
                        cmd.env(name, std::env::var(name).unwrap_or(value));
                    }
                }
            }
            TaskKind::Agent {
                prompt,
                harness,
                model,
                effort,
                sandbox,
            } => {
                if let Some(name) = sandbox {
                    return Attempt::failed(
                        0.0,
                        format!(
                            "agent task runs in sandbox {name:?}, which only the openshell \
                             backend provides; this runner runs agents as local commands"
                        ),
                    );
                }
                let Some(agent_cmd) = &self.agent_cmd else {
                    return Attempt::failed(
                        0.0,
                        "agent task refused: this runner has no agent command (pass --agent-cmd)"
                            .to_string(),
                    );
                };
                cmd.arg(agent_cmd);
                match history.as_ref().map(crate::plan::history::history_section) {
                    None => {
                        cmd.env("CRUCIBLE_PROMPT", prompt);
                    }
                    Some(Ok(section)) => {
                        cmd.env("CRUCIBLE_PROMPT", format!("{prompt}\n\n{section}"));
                    }
                    Some(Err(e)) => {
                        return Attempt::failed(0.0, format!("history not serializable: {e}"));
                    }
                }
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
                template,
                result,
                severity_field,
                ..
            } => {
                return match crucible_broker::report::deliver(
                    Some(template),
                    result
                        .as_ref()
                        .map(|name| crucible_broker::report::Selection {
                            task: &name.0,
                            severity_field: severity_field.as_ref().map(|field| field.0.as_str()),
                        }),
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
                        repairs: Vec::new(),
                    },
                    Err(error) => Attempt::failed(0.0, error.to_string()),
                };
            }
            TaskKind::Route {
                questions,
                decider: Decider::Model { min_confidence, .. },
            } => {
                let inference = match crate::inference::from_process_env() {
                    Ok(inference) => inference,
                    Err(error) => return Attempt::failed(0.0, error.to_string()),
                };
                let Some(binding) = inference.binding(InferenceRole::Decision) else {
                    return Attempt::failed(
                        0.0,
                        format!("{ENV_INFERENCE} holds no decision binding"),
                    );
                };
                let endpoint =
                    match crucible_broker::decide::Endpoint::from_binding(binding, |name| {
                        std::env::var(name).ok()
                    }) {
                        Ok(endpoint) => endpoint,
                        Err(error) => return Attempt::failed(0.0, error.to_string()),
                    };
                return crate::plan::route::model_attempt(
                    &endpoint,
                    task,
                    questions,
                    *min_confidence,
                    inputs,
                );
            }
            TaskKind::Route {
                decider: Decider::Output { .. } | Decider::Human { .. },
                ..
            } => {
                return Attempt::failed(0.0, "engine-decided route reached the runner".to_string());
            }
            TaskKind::TopK { .. } => {
                // The executor owns reducers; reaching the runner is an executor bug.
                return Attempt::failed(0.0, "reducer task reached the runner".to_string());
            }
            TaskKind::Engine { .. } => {
                // Engine ops only exist in the loop's canonical template; its runner
                // handles them, this one can't.
                return Attempt::failed(0.0, "engine task reached a non-loop runner".to_string());
            }
        }
        let out = match run_echoed(&mut cmd, deadline, &task.name.0, std::io::stderr()) {
            Ok(Finished::Exited(out)) => out,
            Ok(Finished::Killed(deadline)) => return Attempt::timed_out(0.0, deadline),
            Err(e) => {
                return Attempt::transport(TransportCause::Command, format!("spawn failed: {e}"));
            }
        };
        if !out.status.success() {
            let output = out
                .result_line
                .as_deref()
                .and_then(json_line)
                .filter(Value::is_object);
            let reason = output
                .as_ref()
                .and_then(own_note)
                .map(str::to_owned)
                .unwrap_or_else(|| stderr_tail(&out.stderr()));
            return Attempt {
                outcome: AttemptOutcome::failed_as_declared(
                    format!("exit {}: {reason}", out.status.code().unwrap_or(-1)),
                    output,
                ),
                cost_usd: 0.0,
                repairs: Vec::new(),
            };
        }
        let Some(last) = out.result_line.as_deref() else {
            return Attempt::failed(
                0.0,
                "no output: expected a JSON result on the last stdout line".to_string(),
            );
        };
        match serde_json::from_str::<Value>(last.trim()) {
            Ok(v) => evaluation_attempt(task, v),
            Err(e) => Attempt::failed(
                0.0,
                format!("last stdout line is not JSON ({e}): {}", last.trim()),
            ),
        }
    }
}

/// The task's inputs JSON in a temp file outside the workspace, removed when the handle drops.
fn inputs_file(json: &str) -> std::io::Result<tempfile::NamedTempFile> {
    let mut file = tempfile::Builder::new()
        .prefix("crucible-inputs-")
        .suffix(".json")
        .tempfile()?;
    std::io::Write::write_all(&mut file, json.as_bytes())?;
    Ok(file)
}

/// How a command ended: on its own, or killed at its deadline.
pub enum Finished {
    Exited(Captured),
    Killed(Deadline),
}

/// What an exited command left behind: its status, its last non-empty stdout line, and the last
/// [`TAIL_LINES`] non-empty stderr lines, oldest first.
pub struct Captured {
    pub status: std::process::ExitStatus,
    pub result_line: Option<String>,
    pub stderr_lines: Vec<String>,
}

impl Captured {
    pub fn stderr(&self) -> String {
        self.stderr_lines.join("\n")
    }
}

/// Longest stdout line kept as the result. Past it the line is clipped, so it fails as JSON.
const RESULT_LINE_BYTES: usize = 8 * 1024 * 1024;
/// Longest stderr line kept for the tail; [`stderr_tail`] clips far shorter.
const STDERR_LINE_BYTES: usize = 4096;
/// Longest line echoed; the rest is replaced by `…`.
const ECHO_LINE_BYTES: usize = 2048;
/// Bytes echoed per command, prefixes included, before the rest is counted instead.
const ECHO_BYTES: usize = 1024 * 1024;

/// Run `cmd` under an optional deadline, echoing each non-blank output line to `sink` as
/// `[label] line` while it runs. Stdin is closed. Both pipes are read on their own threads so a
/// command filling one cannot wedge the other.
pub fn run_echoed(
    cmd: &mut Command,
    deadline: Option<Deadline>,
    label: &str,
    sink: impl Write + Send,
) -> std::io::Result<Finished> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = Supervised::spawn(cmd, deadline)?;
    let stdout = child.child().stdout.take();
    let stderr = child.child().stderr.take();
    let echo = Mutex::new(Echo::new(label, sink));
    let (result_line, stderr_lines) = std::thread::scope(|scope| {
        let echo = &echo;
        let stderr_thread = scope.spawn(move || {
            let mut tail = VecDeque::with_capacity(TAIL_LINES);
            each_line(stderr, STDERR_LINE_BYTES, |line| {
                echo_line(echo, line);
                let line = String::from_utf8_lossy(line);
                if !line.trim().is_empty() {
                    if tail.len() == TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line.into_owned());
                }
            });
            Vec::from(tail)
        });
        let mut result_line = None;
        each_line(stdout, RESULT_LINE_BYTES, |line| {
            echo_line(echo, line);
            if !line.trim_ascii().is_empty() {
                result_line = Some(String::from_utf8_lossy(line).into_owned());
            }
        });
        (result_line, stderr_thread.join().unwrap_or_default())
    });
    echo.into_inner()
        .unwrap_or_else(PoisonError::into_inner)
        .finish();
    let reaped = child.wait()?;
    if let Some(deadline) = reaped.killed_at {
        return Ok(Finished::Killed(deadline));
    }
    Ok(Finished::Exited(Captured {
        status: reaped.status,
        result_line,
        stderr_lines,
    }))
}

/// Call `f` with each line read from `pipe`, without its `\n` or `\r\n`, clipped to `max` bytes.
fn each_line(pipe: Option<impl Read>, max: usize, mut f: impl FnMut(&[u8])) {
    let Some(pipe) = pipe else {
        return;
    };
    let mut reader = BufReader::new(pipe);
    let mut line = Vec::new();
    loop {
        let chunk = match reader.fill_buf() {
            Ok(chunk) => chunk,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if chunk.is_empty() {
            break;
        }
        let newline = chunk.iter().position(|&b| b == b'\n');
        let taken = newline.unwrap_or(chunk.len());
        let room = max.saturating_sub(line.len());
        line.extend_from_slice(&chunk[..taken.min(room)]);
        reader.consume(newline.map_or(taken, |at| at + 1));
        if newline.is_some() {
            f(line.strip_suffix(b"\r").unwrap_or(&line));
            line.clear();
        }
    }
    if !line.is_empty() {
        f(line.strip_suffix(b"\r").unwrap_or(&line));
    }
}

fn echo_line<W: Write>(echo: &Mutex<Echo<'_, W>>, line: &[u8]) {
    echo.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .line(line);
}

/// A command's output echo, capped at [`ECHO_BYTES`].
struct Echo<'a, W> {
    label: &'a str,
    sink: W,
    shown: usize,
    hidden: u64,
}

impl<'a, W: Write> Echo<'a, W> {
    fn new(label: &'a str, sink: W) -> Self {
        Echo {
            label,
            sink,
            shown: 0,
            hidden: 0,
        }
    }

    fn line(&mut self, line: &[u8]) {
        if line.trim_ascii().is_empty() {
            return;
        }
        if self.shown >= ECHO_BYTES {
            self.hidden += 1;
            return;
        }
        let clipped = &line[..line.len().min(ECHO_LINE_BYTES)];
        let text = String::from_utf8_lossy(clipped);
        let more = if clipped.len() < line.len() {
            "…"
        } else {
            ""
        };
        let echoed = format!("[{}] {text}{more}\n", self.label);
        self.shown += echoed.len();
        self.emit(&echoed);
    }

    fn finish(mut self) {
        if self.hidden > 0 {
            let notice = format!("[{}] … {} more lines not shown\n", self.label, self.hidden);
            self.emit(&notice);
        }
    }

    fn emit(&mut self, text: &str) {
        let _ = self.sink.write_all(text.as_bytes());
        let _ = self.sink.flush();
    }
}

/// The task's JSON result, where its last non-empty stdout line carries one.
pub fn json_line(line: &str) -> Option<Value> {
    serde_json::from_str::<Value>(line.trim()).ok()
}

/// Lines kept from the end of stderr. A build/pytest verdict plus its immediate
/// context fits well inside this; more would drown the row.
pub const TAIL_LINES: usize = 40;
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
            repairs: Vec::new(),
        };
    };
    let Some(object) = value.as_object_mut() else {
        return Attempt::failed(0.0, "evaluate result must be a JSON object".to_string());
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
        Some(_) => {
            return Attempt::failed(0.0, "evaluate result `pass` must be a boolean".to_string());
        }
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
            repairs: Vec::new(),
        }
    } else {
        let note = evaluation_note(&task.name, threshold, &handles, own_note(&value));
        Attempt {
            outcome: AttemptOutcome::failed_as_declared(note, Some(value)),
            cost_usd: 0.0,
            repairs: Vec::new(),
        }
    }
}

/// A failed grade's note: the result's own reason followed by the threshold and log handles it
/// was graded with, or the engine's stand-in naming them.
fn evaluation_note(
    task: &TaskName,
    threshold: Option<f64>,
    handles: &[String],
    own: Option<&str>,
) -> String {
    let context: Vec<String> = [
        threshold.map(|threshold| format!("threshold {threshold}")),
        (!handles.is_empty()).then(|| format!("logs: {}", handles.join(","))),
    ]
    .into_iter()
    .flatten()
    .collect();
    match own {
        None => std::iter::once(format!("evaluation {task} did not pass"))
            .chain(context)
            .collect::<Vec<_>>()
            .join(" "),
        Some(own) if context.is_empty() => own.to_owned(),
        Some(own) => format!("{own} ({})", context.join("; ")),
    }
}

#[cfg(test)]
mod tests {
    use crate::crucible::Direction;
    use crate::plan::exec::{ExecCfg, PlanExit, Substrate};
    use crate::plan::runner::*;
    use crucible_contract::session::TaskStatus;

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
    use crate::plan::ir::{Join, Plan, PlanBudget, Stage};

    fn runner() -> ShellRunner {
        ShellRunner {
            workdir: std::env::temp_dir(),
            agent_cmd: None,
            env: crate::host_env::HostEnv::default(),
            buildit_socket: None,
            github: false,
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
            emits: crate::plan::ir::Emits::default(),
            emits_files: Vec::new(),
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
            emits: crate::plan::ir::Emits::default(),
            emits_files: Vec::new(),
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

    fn run_plan(tasks: Vec<Task>, agent_cmd: Option<&str>) -> crate::plan::exec::PlanOutcome {
        let plan = Plan {
            version: 1,
            reason: None,
            budget: PlanBudget { usd: 1.0 },
            params: std::collections::BTreeMap::new(),
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

    /// A command task runs outside the sandbox, so it gets the base and what `[workflow].env`
    /// declares, plus the variables the engine sets for it; an engine variable nobody declared,
    /// a credential say, is not there.
    #[test]
    fn a_command_task_sees_only_the_base_its_declared_variables_and_the_engines_own() {
        let _guard = crate::test_support::env_lock();
        unsafe {
            std::env::set_var("CRUCIBLE_TEST_UNDECLARED", "leaked");
            std::env::set_var("CRUCIBLE_TEST_DECLARED", "passed");
        }
        let declared: crate::host_env::DeclaredEnv =
            serde_json::from_value(serde_json::json!(["CRUCIBLE_TEST_DECLARED"])).unwrap();
        let env = crate::host_env::HostEnv::resolve(
            crate::manifest::edition::Edition::Two,
            "[workflow]",
            &declared,
        )
        .unwrap();
        let plan = Plan {
            version: 1,
            reason: None,
            budget: PlanBudget { usd: 1.0 },
            params: std::collections::BTreeMap::new(),
            tasks: vec![command(
                "probe",
                r#"printf '{"undeclared":"%s","declared":"%s","task":"%s","path":"%s"}\n' "$CRUCIBLE_TEST_UNDECLARED" "$CRUCIBLE_TEST_DECLARED" "$CRUCIBLE_TASK" "$PATH""#,
                &[],
            )],
        }
        .validate()
        .unwrap();
        let mut r = ShellRunner { env, ..runner() };
        let out = execute(
            &plan,
            &Substrate::default(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        unsafe {
            std::env::remove_var("CRUCIBLE_TEST_UNDECLARED");
            std::env::remove_var("CRUCIBLE_TEST_DECLARED");
        }
        let probe = out.results[&"probe".into()].output.clone().unwrap();
        assert_eq!(probe["undeclared"], "", "{probe}");
        assert_eq!(probe["declared"], "passed", "{probe}");
        assert_eq!(probe["task"], "probe", "{probe}");
        assert_ne!(probe["path"], "", "{probe}");
    }

    /// An edition-1 pack's command task runs with the engine's environment, and still gets the
    /// variables the engine sets for it.
    #[test]
    fn an_edition_one_command_task_inherits_the_engines_environment() {
        let _guard = crate::test_support::env_lock();
        unsafe { std::env::set_var("CRUCIBLE_TEST_INHERITED", "kept") };
        let plan = Plan {
            version: 1,
            reason: None,
            budget: PlanBudget { usd: 1.0 },
            params: std::collections::BTreeMap::new(),
            tasks: vec![command(
                "probe",
                r#"printf '{"inherited":"%s","task":"%s"}\n' "$CRUCIBLE_TEST_INHERITED" "$CRUCIBLE_TASK""#,
                &[],
            )],
        }
        .validate()
        .unwrap();
        let mut r = ShellRunner {
            env: crate::host_env::HostEnv::inherited(),
            ..runner()
        };
        let out = execute(
            &plan,
            &Substrate::default(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        unsafe { std::env::remove_var("CRUCIBLE_TEST_INHERITED") };
        let probe = out.results[&"probe".into()].output.clone().unwrap();
        assert_eq!(probe["inherited"], "kept", "{probe}");
        assert_eq!(probe["task"], "probe", "{probe}");
    }

    /// `[workflow].env = ["NAME?"]` passes a variable the engine has to a command task, and one
    /// it lacks is absent, not empty, while the task still runs.
    #[test]
    fn a_command_task_sees_an_optional_variable_the_engine_has_and_runs_without_one_it_lacks() {
        let _guard = crate::test_support::env_lock();
        unsafe {
            std::env::set_var("CRUCIBLE_TEST_OPTIONAL_SET", "passed");
            std::env::remove_var("CRUCIBLE_TEST_OPTIONAL_UNSET");
        }
        let declared: crate::host_env::DeclaredEnv = serde_json::from_value(serde_json::json!([
            "CRUCIBLE_TEST_OPTIONAL_SET?",
            "CRUCIBLE_TEST_OPTIONAL_UNSET?"
        ]))
        .unwrap();
        let env = crate::host_env::HostEnv::resolve(
            crate::manifest::edition::Edition::Two,
            "[workflow]",
            &declared,
        );
        unsafe {
            std::env::remove_var("CRUCIBLE_TEST_OPTIONAL_SET");
        }
        let plan = Plan {
            version: 1,
            reason: None,
            budget: PlanBudget { usd: 1.0 },
            params: std::collections::BTreeMap::new(),
            tasks: vec![command(
                "probe",
                r#"printf '{"set":"%s","unset":"%s"}\n' "$CRUCIBLE_TEST_OPTIONAL_SET" "${CRUCIBLE_TEST_OPTIONAL_UNSET-absent}""#,
                &[],
            )],
        }
        .validate()
        .unwrap();
        let mut r = ShellRunner {
            env: env.unwrap(),
            ..runner()
        };
        let out = execute(
            &plan,
            &Substrate::default(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        let probe = out.results[&"probe".into()].output.clone().unwrap();
        assert_eq!(probe["set"], "passed", "{probe}");
        assert_eq!(probe["unset"], "absent", "{probe}");
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
    fn small_inputs_arrive_in_both_the_env_and_the_file() {
        let out = run_plan(
            vec![
                command("emit", r#"echo '{"score": 7}'"#, &[]),
                command(
                    "check",
                    r#"python3 -c 'import json,os; e=json.loads(os.environ["CRUCIBLE_INPUTS"]); f=json.load(open(os.environ["CRUCIBLE_INPUTS_FILE"])); print(json.dumps({"same": e == f, "score": f["emit"]["score"]}))'"#,
                    &["emit"],
                ),
            ],
            None,
        );
        assert!(out.valid, "{:?}", out.results);
        let check = out.results[&"check".into()].output.as_ref().unwrap();
        assert_eq!(check["same"], true);
        assert_eq!(check["score"], 7);
    }

    #[test]
    fn inputs_past_the_env_limit_arrive_only_in_the_file() {
        let out = run_plan(
            vec![
                command(
                    "emit",
                    r#"python3 -c 'import json; print(json.dumps({"blob": "x" * 300 * 1024}))'"#,
                    &[],
                ),
                command(
                    "check",
                    r#"python3 -c 'import json,os; f=json.load(open(os.environ["CRUCIBLE_INPUTS_FILE"])); print(json.dumps({"env": "CRUCIBLE_INPUTS" in os.environ, "len": len(f["emit"]["blob"]), "path": os.environ["CRUCIBLE_INPUTS_FILE"]}))'"#,
                    &["emit"],
                ),
            ],
            None,
        );
        assert!(out.valid, "{:?}", out.results);
        let check = out.results[&"check".into()].output.as_ref().unwrap();
        assert_eq!(check["env"], false);
        assert_eq!(check["len"], 300 * 1024);
        let path = check["path"].as_str().unwrap();
        assert!(
            !std::path::Path::new(path).exists(),
            "the inputs file outlived its task: {path}"
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

    /// The reading that says why the task failed is the reading a settled consumer came for, so
    /// a nonzero exit that still printed its result keeps it.
    #[test]
    fn a_nonzero_exit_that_printed_json_keeps_the_object() {
        let out = run_plan(
            vec![command(
                "boom",
                r#"echo '{"pass": false, "detail": "rung did not separate"}'; echo doomed >&2; exit 1"#,
                &[],
            )],
            None,
        );
        let r = &out.results[&"boom".into()];
        assert_eq!(r.status, TaskStatus::Fail);
        assert_eq!(
            r.output.as_ref().unwrap()["detail"],
            "rung did not separate"
        );
        assert!(r.note.as_ref().unwrap().contains("exit 1"));
        assert!(r.note.as_ref().unwrap().contains("doomed"));
    }

    /// A command that exits nonzero and says why in its result's `note` is recorded with that
    /// reason, not its stderr; a blank note falls back to the stderr tail.
    #[test]
    fn a_nonzero_exit_records_the_note_its_result_states() {
        let out = run_plan(
            vec![command(
                "boom",
                r#"echo '{"pass": false, "note": "  the layer cannot package test paths "}'; echo doomed >&2; exit 2"#,
                &[],
            )],
            None,
        );
        let r = &out.results[&"boom".into()];
        assert_eq!(r.status, TaskStatus::Fail);
        assert_eq!(
            r.note.as_deref(),
            Some("exit 2: the layer cannot package test paths")
        );
        assert_eq!(r.output.as_ref().unwrap()["pass"], false);

        for printed in [r#"{"note": "  "}"#, r#"{"note": 7}"#] {
            let out = run_plan(
                vec![command(
                    "boom",
                    &format!("echo '{printed}'; echo doomed >&2; exit 2"),
                    &[],
                )],
                None,
            );
            assert_eq!(
                out.results[&"boom".into()].note.as_deref(),
                Some("exit 2: doomed"),
                "{printed}"
            );
        }
    }

    /// A payload is a bonus, never a second failure mode: unparsable or non-object output leaves
    /// the failure exactly as it was.
    #[test]
    fn a_nonzero_exit_that_printed_no_object_keeps_no_output() {
        for command_line in [
            "echo not json at all; exit 4",
            "echo '[1, 2, 3]'; exit 4",
            "exit 4",
        ] {
            let out = run_plan(vec![command("boom", command_line, &[])], None);
            let r = &out.results[&"boom".into()];
            assert_eq!(r.status, TaskStatus::Fail, "{command_line}");
            assert_eq!(r.attempts, 1, "{command_line}");
            assert!(r.output.is_none(), "{command_line}: {:?}", r.output);
            assert!(
                r.note.as_ref().unwrap().contains("exit 4"),
                "{command_line}: {:?}",
                r.note
            );
        }
    }

    /// `evaluation_attempt` normalizes `pass` and then throws the record away today. The graded
    /// object is the whole point of an evaluate task, so a failing grade keeps it.
    #[test]
    fn a_failing_evaluation_keeps_its_graded_object() {
        let out = run_plan(
            vec![evaluate(
                "latency",
                r#"echo '{"score": 42.0, "detail": "p99 regressed"}'"#,
                Some(10.0),
            )],
            None,
        );
        let r = &out.results[&"latency".into()];
        assert_eq!(r.status, TaskStatus::Fail);
        let output = r.output.as_ref().unwrap();
        assert_eq!(
            output["pass"], false,
            "`pass` is normalized before it rides"
        );
        assert_eq!(output["score"], 42.0);
        assert_eq!(output["detail"], "p99 regressed");
    }

    /// The self-graded shape the playbook producers use: exit 0, `pass = false`, no threshold.
    #[test]
    fn a_self_graded_evaluation_that_declared_failure_keeps_its_object() {
        let out = run_plan(
            vec![evaluate(
                "probe",
                r#"echo '{"pass": false, "bug_present": false}'"#,
                None,
            )],
            None,
        );
        let r = &out.results[&"probe".into()];
        assert_eq!(r.status, TaskStatus::Fail);
        assert_eq!(r.output.as_ref().unwrap()["bug_present"], false);
    }

    fn settled(task: Task) -> crate::plan::exec::TaskResult {
        let name = task.name.clone();
        let mut out = run_plan(vec![task], None);
        out.results.remove(&name).unwrap()
    }

    fn printing(object: &str) -> String {
        format!("echo '{object}'")
    }

    #[test]
    fn a_failing_evaluation_records_the_note_its_result_states() {
        let r = settled(evaluate(
            "a",
            &printing(r#"{"pass": false, "note": "  own reason a "}"#),
            None,
        ));
        assert_eq!(r.status, TaskStatus::Fail);
        assert_eq!(r.note.as_deref(), Some("own reason a"));
        assert_eq!(r.output.unwrap()["note"], "  own reason a ");
    }

    #[test]
    fn a_missed_threshold_records_the_note_with_its_threshold_and_logs() {
        for (printed, note) in [
            (
                r#"{"score": 42, "note": "p99 regressed"}"#,
                "p99 regressed (threshold 10)",
            ),
            (
                r#"{"score": 42, "note": "p99 regressed", "logs": ["job-a.log", "job-b.log"]}"#,
                "p99 regressed (threshold 10; logs: job-a.log,job-b.log)",
            ),
            (
                r#"{"score": 5, "pass": false, "note": "flaky run"}"#,
                "flaky run (threshold 10)",
            ),
        ] {
            let r = settled(evaluate("latency", &printing(printed), Some(10.0)));
            assert_eq!(r.status, TaskStatus::Fail, "{printed}");
            assert_eq!(r.note.as_deref(), Some(note), "{printed}");
        }
        let r = settled(evaluate(
            "latency",
            &printing(r#"{"pass": false, "note": "no bound", "logs": ["job-a.log"]}"#),
            None,
        ));
        assert_eq!(r.note.as_deref(), Some("no bound (logs: job-a.log)"));
    }

    #[test]
    fn a_blank_or_non_string_note_falls_back_to_the_evaluations_own() {
        for note in [r#""""#, r#""   ""#, "7", "null", r#"["why"]"#] {
            let r = settled(evaluate(
                "b",
                &printing(&format!(r#"{{"pass": false, "note": {note}}}"#)),
                None,
            ));
            assert_eq!(r.status, TaskStatus::Fail, "{note}");
            assert_eq!(
                r.note.as_deref(),
                Some("evaluation b did not pass"),
                "{note}"
            );

            let r = settled(evaluate(
                "b",
                &printing(&format!(
                    r#"{{"score": 42, "logs": ["job-a.log"], "note": {note}}}"#
                )),
                Some(10.0),
            ));
            assert_eq!(
                r.note.as_deref(),
                Some("evaluation b did not pass threshold 10 logs: job-a.log"),
                "{note}"
            );
        }
    }

    #[test]
    fn a_failing_evaluation_settles_as_its_declared_fail_or_skip() {
        for (printed, threshold, status, note) in [
            (
                r#"{"pass": false, "status": "fail", "note": "own reason b"}"#,
                None,
                TaskStatus::Fail,
                "own reason b",
            ),
            (
                r#"{"pass": false, "status": "fail"}"#,
                None,
                TaskStatus::Fail,
                "evaluation e did not pass",
            ),
            (
                r#"{"score": 42, "status": "fail", "note": "regressed"}"#,
                Some(10.0),
                TaskStatus::Fail,
                "regressed (threshold 10)",
            ),
            (
                r#"{"pass": false, "status": "skipped", "note": "no gpu on this node"}"#,
                None,
                TaskStatus::Skipped,
                "no gpu on this node",
            ),
            (
                r#"{"pass": false, "status": "skipped"}"#,
                None,
                TaskStatus::Skipped,
                "evaluation e did not pass",
            ),
            (
                r#"{"score": 42, "status": "skipped", "note": "trace too short"}"#,
                Some(10.0),
                TaskStatus::Skipped,
                "trace too short (threshold 10)",
            ),
            (
                r#"{"score": 42, "status": "skipped"}"#,
                Some(10.0),
                TaskStatus::Skipped,
                "evaluation e did not pass threshold 10",
            ),
        ] {
            let r = settled(evaluate("e", &printing(printed), threshold));
            assert_eq!(r.status, status, "{printed}");
            assert_eq!(r.note.as_deref(), Some(note), "{printed}");
            assert_eq!(r.attempts, 1, "{printed}");
            let output = r.output.unwrap();
            assert_eq!(output["pass"], false, "{printed}");
            assert_eq!(output["status"], status.as_str(), "{printed}");
        }
    }

    #[test]
    fn a_passing_evaluation_settles_as_its_declared_fail_or_skip() {
        for (printed, status, note) in [
            (
                r#"{"score": 5, "status": "fail", "note": "vetoed"}"#,
                TaskStatus::Fail,
                "vetoed",
            ),
            (
                r#"{"score": 5, "status": "fail", "note": " "}"#,
                TaskStatus::Fail,
                "task declared status=fail",
            ),
            (
                r#"{"score": 5, "status": "skipped", "note": "inapplicable"}"#,
                TaskStatus::Skipped,
                "inapplicable",
            ),
            (
                r#"{"score": 5, "status": "skipped"}"#,
                TaskStatus::Skipped,
                "task declared status=skipped",
            ),
        ] {
            let r = settled(evaluate("e", &printing(printed), Some(10.0)));
            assert_eq!(r.status, status, "{printed}");
            assert_eq!(r.note.as_deref(), Some(note), "{printed}");
            assert_eq!(r.output.unwrap()["pass"], true, "{printed}");
        }
    }

    #[test]
    fn a_declared_pass_cannot_pass_a_failing_grade() {
        for (printed, threshold, note) in [
            (
                r#"{"pass": false, "status": "pass"}"#,
                None,
                "evaluation w did not pass",
            ),
            (
                r#"{"pass": false, "status": "pass", "note": "close enough"}"#,
                None,
                "close enough",
            ),
            (
                r#"{"score": 42, "status": "pass"}"#,
                Some(10.0),
                "evaluation w did not pass threshold 10",
            ),
            (
                r#"{"score": 42, "pass": true, "status": "pass"}"#,
                Some(10.0),
                "evaluation w did not pass threshold 10",
            ),
        ] {
            let r = settled(evaluate("w", &printing(printed), threshold));
            assert_eq!(r.status, TaskStatus::Fail, "{printed}");
            assert_eq!(r.note.as_deref(), Some(note), "{printed}");
            assert_eq!(r.output.unwrap()["pass"], false, "{printed}");
        }
    }

    #[test]
    fn a_nonzero_exit_settles_as_its_declared_fail_or_skip_and_never_passes() {
        for (printed, status, note) in [
            (
                r#"{"status": "pass", "note": "fine really"}"#,
                TaskStatus::Fail,
                "exit 3: fine really",
            ),
            (r#"{"status": "pass"}"#, TaskStatus::Fail, "exit 3: doomed"),
            (
                r#"{"status": "fail", "note": "own reason d"}"#,
                TaskStatus::Fail,
                "exit 3: own reason d",
            ),
            (
                r#"{"status": "skipped", "note": "no gpu"}"#,
                TaskStatus::Skipped,
                "exit 3: no gpu",
            ),
            (
                r#"{"status": "skipped"}"#,
                TaskStatus::Skipped,
                "exit 3: doomed",
            ),
        ] {
            for task in [
                command(
                    "c",
                    &format!("echo '{printed}'; echo doomed >&2; exit 3"),
                    &[],
                ),
                evaluate(
                    "c",
                    &format!("echo '{printed}'; echo doomed >&2; exit 3"),
                    None,
                ),
            ] {
                let r = settled(task);
                assert_eq!(r.status, status, "{printed}");
                assert_eq!(r.note.as_deref(), Some(note), "{printed}");
                assert_eq!(r.attempts, 1, "{printed}");
                assert_eq!(
                    r.output,
                    Some(serde_json::from_str::<Value>(printed).unwrap()),
                    "{printed}"
                );
            }
        }
    }

    #[test]
    fn a_skipped_failing_evaluation_blocks_its_dependents() {
        let out = run_plan(
            vec![
                evaluate(
                    "probe",
                    &printing(r#"{"pass": false, "status": "skipped", "note": "no gpu"}"#),
                    None,
                ),
                command("after", "echo '{}'", &["probe"]),
            ],
            None,
        );
        assert_eq!(out.results[&"probe".into()].status, TaskStatus::Skipped);
        assert_eq!(out.results[&"after".into()].status, TaskStatus::Blocked);
        assert!(!out.valid);
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
            emits: crate::plan::ir::Emits::default(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
            keyed: Vec::new(),
            when: None,
            revise: None,
            timeout: None,
            history: None,
            repair: 0,
            optional: Vec::new(),
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
            emits: crate::plan::ir::Emits::default(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
            keyed: Vec::new(),
            when: None,
            revise: None,
            timeout: None,
            history: None,
            repair: 0,
            optional: Vec::new(),
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
            emits: crate::plan::ir::Emits::default(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
            keyed: Vec::new(),
            when: None,
            revise: None,
            timeout: None,
            history: None,
            repair: 0,
            optional: Vec::new(),
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
            emits: crate::plan::ir::Emits::default(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
            keyed: Vec::new(),
            when: None,
            revise: None,
            timeout: None,
            history: None,
            repair: 0,
            optional: Vec::new(),
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
    fn an_agent_task_naming_a_sandbox_is_refused_by_the_command_runner() {
        let t = Task {
            name: "a".into(),
            task: TaskKind::Agent {
                prompt: "do it".into(),
                harness: None,
                model: None,
                effort: None,
                sandbox: Some("bare".into()),
            },
            depends_on: vec![],
            session: None,
            needs: "any".into(),
            required: true,
            isolation: None,
            join: Join::default(),
            stage: Stage::Iteration,
            emits: crate::plan::ir::Emits::default(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
            keyed: Vec::new(),
            when: None,
            revise: None,
            timeout: None,
            history: None,
            repair: 0,
            optional: Vec::new(),
        };
        let mut r = ShellRunner {
            workdir: std::env::temp_dir(),
            agent_cmd: Some("true".into()),
            env: crate::host_env::HostEnv::default(),
            buildit_socket: None,
            github: false,
        };
        let a = r.run(&t, 1, &BTreeMap::new(), None);
        match a.outcome {
            AttemptOutcome::Fail { note, .. } => {
                assert!(note.contains("sandbox \"bare\""), "{note}");
            }
            other => panic!("expected a measured failure, got {other:?}"),
        }
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
                sandbox: None,
            },
            depends_on: vec![],
            session: None,
            needs: "any".into(),
            required: true,
            isolation: None,
            join: Join::default(),
            stage: Stage::Iteration,
            emits: crate::plan::ir::Emits::default(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
            keyed: Vec::new(),
            when: None,
            revise: None,
            timeout: None,
            history: None,
            repair: 0,
            optional: Vec::new(),
        };
        let out = run_plan(vec![t], None);
        assert_eq!(out.results[&"a".into()].status, TaskStatus::Fail);
        assert!(matches!(out.exit, PlanExit::ShortCircuit { .. }));
    }

    #[test]
    fn the_buildit_socket_reaches_command_and_evaluate_tasks_and_never_an_agent() {
        let show = r#"printf '{"socket":"%s","pass":true}\n' "${CRUCIBLE_BUILDIT_SOCKET-unset}""#;
        let mut agent = command("a", "unused", &[]);
        agent.task = TaskKind::Agent {
            prompt: "build it".into(),
            harness: None,
            model: None,
            effort: None,
            sandbox: None,
        };
        let mut r = ShellRunner {
            agent_cmd: Some(show.into()),
            buildit_socket: Some("/runs/r1/pack/state/buildit.sock".into()),
            github: false,
            ..runner()
        };
        let seen =
            |r: &mut ShellRunner, task: &Task| match r.run(task, 1, &BTreeMap::new(), None).outcome
            {
                AttemptOutcome::Pass(v) => v["socket"].clone(),
                other => panic!("{}: {other:?}", task.name),
            };
        let socket = "/runs/r1/pack/state/buildit.sock";
        assert_eq!(seen(&mut r, &command("c", show, &[])), socket);
        assert_eq!(seen(&mut r, &evaluate("e", show, None)), socket);
        assert_eq!(seen(&mut r, &agent), "unset");
        r.buildit_socket = None;
        assert_eq!(seen(&mut r, &command("c", show, &[])), "unset");
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
                sandbox: None,
            },
            depends_on: vec![],
            session: None,
            needs: "any".into(),
            required: true,
            isolation: None,
            join: Join::default(),
            stage: Stage::Iteration,
            emits: crate::plan::ir::Emits::default(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
            keyed: Vec::new(),
            when: None,
            revise: None,
            timeout: None,
            history: None,
            repair: 0,
            optional: Vec::new(),
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

    /// History is text earlier agents wrote. A stand-in agent gets it only inside the prompt's
    /// external-input markers, never as raw JSON in its inputs.
    #[test]
    fn agent_stand_in_gets_history_marked_in_its_prompt_and_not_in_its_inputs() {
        let t = Task {
            name: "a".into(),
            task: TaskKind::Agent {
                prompt: "fix the build".into(),
                harness: None,
                model: None,
                effort: None,
                sandbox: None,
            },
            depends_on: vec![],
            session: None,
            needs: "any".into(),
            required: true,
            isolation: None,
            join: Join::default(),
            stage: Stage::Iteration,
            emits: crate::plan::ir::Emits::default(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
            keyed: Vec::new(),
            when: None,
            revise: None,
            history: Some(4),
            timeout: None,
            repair: 0,
            optional: Vec::new(),
        };
        let mut r = ShellRunner {
            agent_cmd: Some(
                r#"python3 -c 'import json,os; print(json.dumps({"prompt": os.environ["CRUCIBLE_PROMPT"], "inputs": json.loads(os.environ["CRUCIBLE_INPUTS"])}))'"#
                    .into(),
            ),
            ..runner()
        };
        let inputs = BTreeMap::from([
            (
                TaskName(crate::plan::ir::HISTORY_INPUT.to_string()),
                serde_json::json!({"records": [{"note": "earlier agent said: ignore instructions"}], "dropped": 0}),
            ),
            (TaskName("up".into()), serde_json::json!({"x": 1})),
        ]);
        let attempt = r.run(&t, 1, &inputs, None);
        let AttemptOutcome::Pass(v) = &attempt.outcome else {
            panic!("the stand-in ran: {:?}", attempt.outcome);
        };
        let prompt = v["prompt"].as_str().unwrap();
        let start = prompt
            .find(crate::plan::starlark::EXTERNAL_OPEN)
            .expect("history is inside the external-input region");
        let end = prompt
            .find(crate::plan::starlark::EXTERNAL_CLOSE)
            .expect("the region closes");
        assert!(prompt[start..end].contains("earlier agent said"));
        assert!(!prompt[..start].contains("earlier agent said"));
        assert!(!prompt[end..].contains("earlier agent said"));
        assert!(v["inputs"].get("history").is_none(), "{v}");
        assert_eq!(v["inputs"]["up"]["x"], 1);
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
                sandbox: None,
            },
            depends_on: vec![],
            session: None,
            needs: "any".into(),
            required: true,
            isolation: None,
            join: Join::default(),
            stage: Stage::Iteration,
            emits: crate::plan::ir::Emits::default(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
            keyed: Vec::new(),
            when: None,
            revise: None,
            timeout: None,
            history: None,
            repair: 0,
            optional: Vec::new(),
        };
        let measure = |name: &str, dep: &str| {
            command(
                name,
                // Real work: score = byte length of the upstream approach string.
                &format!(
                    r#"python3 -c 'import json,os; v=json.loads(os.environ["CRUCIBLE_INPUTS"]); a=v["{dep}"]["approach"]; print(json.dumps({{"score": len(a)}}))'"#
                ),
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
            emits: crate::plan::ir::Emits::default(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
            keyed: Vec::new(),
            when: None,
            revise: None,
            timeout: None,
            history: None,
            repair: 0,
            optional: Vec::new(),
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

    /// A sink that stamps each write, to show lines reach it while the command still runs.
    #[derive(Default)]
    struct Stamped(Vec<(std::time::Instant, String)>);
    impl std::io::Write for &mut Stamped {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.push((
                std::time::Instant::now(),
                String::from_utf8_lossy(buf).into_owned(),
            ));
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn echoed(script: &str, label: &str, deadline: Option<Deadline>) -> (Finished, String) {
        let mut sink = Vec::new();
        let finished = run_echoed(
            std::process::Command::new("sh").arg("-c").arg(script),
            deadline,
            label,
            &mut sink,
        )
        .unwrap();
        (finished, String::from_utf8(sink).unwrap())
    }

    fn exited(finished: Finished) -> Captured {
        match finished {
            Finished::Exited(captured) => captured,
            Finished::Killed(deadline) => panic!("killed at {}", deadline.note()),
        }
    }

    fn lines_of<'a>(echo: &'a str, stream: &str) -> Vec<&'a str> {
        echo.lines().filter(|line| line.contains(stream)).collect()
    }

    #[test]
    fn output_lines_echo_prefixed_with_the_instance_name_and_in_order() {
        let (finished, echo) = echoed(
            "echo out1; echo err1 >&2; echo; echo out2; echo '   '; echo err2 >&2; \
             printf 'out3\\r\\n'; printf 'err3' >&2; echo '{\"score\": 3}'",
            "task[3]",
            None,
        );
        let captured = exited(finished);
        assert!(captured.status.success());
        assert_eq!(
            lines_of(&echo, "out"),
            ["[task[3]] out1", "[task[3]] out2", "[task[3]] out3"]
        );
        assert_eq!(
            lines_of(&echo, "err"),
            ["[task[3]] err1", "[task[3]] err2", "[task[3]] err3"]
        );
        assert_eq!(lines_of(&echo, "score"), [r#"[task[3]] {"score": 3}"#]);
        assert_eq!(echo.lines().count(), 7, "blank lines were echoed: {echo}");
        assert_eq!(captured.result_line.as_deref(), Some(r#"{"score": 3}"#));
        assert_eq!(captured.stderr_lines, ["err1", "err2", "err3"]);
    }

    #[test]
    fn a_line_reaches_the_sink_while_the_command_still_runs() {
        let mut sink = Stamped::default();
        let started = std::time::Instant::now();
        let finished = run_echoed(
            std::process::Command::new("sh")
                .arg("-c")
                .arg("echo early; sleep 1; echo '{}'"),
            None,
            "slow",
            &mut sink,
        )
        .unwrap();
        let ended = std::time::Instant::now();
        assert!(exited(finished).status.success());
        let (at, line) = &sink.0[0];
        assert_eq!(line, "[slow] early\n");
        assert!(
            ended.duration_since(*at) >= std::time::Duration::from_millis(500),
            "the line waited for the exit: echoed {:?} after the start, exit at {:?}",
            at.duration_since(started),
            ended.duration_since(started)
        );
    }

    #[test]
    fn a_flood_on_both_pipes_neither_wedges_nor_echoes_past_the_cap() {
        let lines = 200_000;
        let (finished, echo) = echoed(
            &format!(
                "yes err | head -n {lines} >&2; yes noise | head -n {lines}; \
                 yes err | head -n {lines} >&2; echo '{{\"ok\": 1}}'"
            ),
            "flood",
            None,
        );
        let captured = exited(finished);
        assert!(captured.status.success());
        assert_eq!(captured.result_line.as_deref(), Some(r#"{"ok": 1}"#));
        assert_eq!(captured.stderr_lines.len(), TAIL_LINES);
        assert!(
            echo.len() <= ECHO_BYTES + ECHO_LINE_BYTES + 64,
            "echoed {} bytes",
            echo.len()
        );
        let notice = echo.lines().last().unwrap();
        let shown = echo.lines().count() - 1;
        assert_eq!(
            notice,
            format!("[flood] … {} more lines not shown", 3 * lines + 1 - shown)
        );
    }

    #[test]
    fn a_long_line_echoes_clipped_and_still_parses_as_the_result() {
        let (finished, echo) = echoed(
            r#"head -c 100000 /dev/zero | tr '\0' x; echo; printf '{"blob": "%s"}\n' "$(head -c 50000 /dev/zero | tr '\0' y)""#,
            "long",
            None,
        );
        let captured = exited(finished);
        let echoed: Vec<&str> = echo.lines().collect();
        assert_eq!(echoed.len(), 2);
        for line in echoed {
            assert_eq!(line.len(), "[long] ".len() + ECHO_LINE_BYTES + "…".len());
            assert!(line.ends_with('…'));
        }
        let result = json_line(captured.result_line.as_deref().unwrap()).unwrap();
        assert_eq!(result["blob"].as_str().unwrap().len(), 50_000);
    }

    #[test]
    fn a_command_killed_at_its_deadline_reports_the_kill_after_echoing_what_it_printed() {
        let deadline =
            Deadline::for_attempt(std::time::Instant::now(), Some("1s".parse().unwrap()), None);
        let (finished, echo) = echoed("echo started; sleep 30", "hang", deadline);
        assert!(matches!(finished, Finished::Killed(_)));
        assert_eq!(echo, "[hang] started\n");
    }

    #[test]
    fn a_noisy_command_still_settles_on_its_last_json_line() {
        let out = run_plan(
            vec![command(
                "noisy",
                r#"for i in $(seq 1 50); do echo "step $i"; echo "warn $i" >&2; done; echo '{"score": 7}'; echo"#,
                &[],
            )],
            None,
        );
        let noisy = &out.results[&"noisy".into()];
        assert_eq!(noisy.status, TaskStatus::Pass);
        assert_eq!(noisy.output.as_ref().unwrap()["score"], 7);
    }

    #[test]
    fn a_stderr_only_failure_notes_the_last_lines_of_a_long_stderr() {
        let out = run_plan(
            vec![command(
                "boom",
                "for i in $(seq 1 100); do echo err$i >&2; done; exit 2",
                &[],
            )],
            None,
        );
        let boom = &out.results[&"boom".into()];
        assert_eq!(boom.status, TaskStatus::Fail);
        let tail = (61..=100)
            .map(|i| format!("err{i}"))
            .collect::<Vec<_>>()
            .join(" ⏎ ");
        assert_eq!(
            boom.note.as_deref(),
            Some(format!("exit 2: {tail}").as_str())
        );
    }

    fn limited(mut task: Task, limit: &str) -> Task {
        task.timeout = Some(limit.parse().unwrap());
        task
    }

    fn alive(pid: i32) -> bool {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
    }

    /// The command forks a sleeper that holds stdout open, so the task ends at its limit only if
    /// the whole process group is killed, and the sleeper must not outlive the task.
    #[test]
    fn a_command_that_outlives_its_timeout_is_killed_with_its_group_and_fails_on_the_limit() {
        let dir = std::env::temp_dir().join(format!("crucible-timeout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("sleeper.pid");
        let started = std::time::Instant::now();
        let out = run_plan(
            vec![
                limited(
                    command(
                        "hang",
                        &format!("sleep 30 & echo $! > {}; wait", pidfile.display()),
                        &[],
                    ),
                    "0.5s",
                ),
                command("after", "echo '{}'", &["hang"]),
            ],
            None,
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "the run waited out the command"
        );
        let hang = &out.results[&"hang".into()];
        assert_eq!(hang.status, TaskStatus::Fail);
        assert_eq!(hang.attempts, 1, "a timed-out attempt was retried");
        assert_eq!(
            hang.note.as_deref(),
            Some("timed out: the task ran past its 0.5s limit")
        );
        assert_eq!(
            out.exit,
            PlanExit::ShortCircuit {
                task: "hang".into()
            }
        );
        assert_eq!(out.results[&"after".into()].status, TaskStatus::Blocked);
        let sleeper: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let settle = std::time::Instant::now();
        while alive(sleeper) && settle.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            !alive(sleeper),
            "the forked sleeper {sleeper} outlived its task"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_command_under_its_timeout_runs_as_it_would_without_one() {
        let out = run_plan(
            vec![
                limited(
                    command("quick", r#"sleep 0.1; echo '{"score": 3}'"#, &[]),
                    "30s",
                ),
                limited(
                    command("boom", "echo doomed >&2; exit 3", &["quick"]),
                    "30s",
                ),
            ],
            None,
        );
        let quick = &out.results[&"quick".into()];
        assert_eq!(quick.status, TaskStatus::Pass);
        assert_eq!(quick.output.as_ref().unwrap()["score"], 3);
        let boom = &out.results[&"boom".into()];
        assert_eq!(boom.status, TaskStatus::Fail);
        assert_eq!(boom.note.as_deref(), Some("exit 3: doomed"));
    }

    #[test]
    fn an_evaluate_past_its_timeout_fails_on_the_limit_not_its_grade() {
        let out = run_plan(
            vec![limited(
                evaluate("slow", r#"sleep 30; echo '{"score": 1}'"#, Some(10.0)),
                "0.3s",
            )],
            None,
        );
        let slow = &out.results[&"slow".into()];
        assert_eq!(slow.status, TaskStatus::Fail);
        assert_eq!(slow.output, None);
        assert_eq!(
            slow.note.as_deref(),
            Some("timed out: the task ran past its 0.3s limit")
        );
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
