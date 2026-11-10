//! An agent task's attempts as turns to read in order: what it thought, said and called, what
//! each call returned, and how the turn ended, with its tokens and spend.

use crucible_contract::event::AgentEvent;
use serde_json::Value;

use crate::logline::{Body, LogLine};
use crate::session::AgentStep;

/// One step of a turn.
#[derive(Clone, Debug, PartialEq)]
pub enum Step<'a> {
    /// Its text, or only its length when the log kept no more.
    Thought {
        text: &'a str,
        chars: Option<u64>,
    },
    Said(&'a str),
    Tool {
        name: &'a str,
        summary: &'a str,
        /// What it was called with and what came back; recorded only under verbose tool IO.
        input: Option<&'a Value>,
        result: Option<&'a str>,
        failed: bool,
        subagent: bool,
    },
    Retry(String),
    Error(String),
    /// Lifecycle: model, gates, sandbox, exit.
    Note(String),
}

/// How a turn ended.
#[derive(Clone, Debug, PartialEq)]
pub struct Ending {
    pub subtype: String,
    pub failed: bool,
    pub turns: u32,
    pub cost_usd: f64,
    pub error: Option<String>,
}

/// One attempt of one task or instance.
#[derive(Clone, Debug, PartialEq)]
pub struct Turn<'a> {
    /// The task, or `task[key]` for a fan-out instance.
    pub task: &'a str,
    pub attempt: u32,
    pub model: Option<&'a str>,
    pub steps: Vec<Step<'a>>,
    /// The newest token sample: input, output, cache read, total.
    pub tokens: Option<[u64; 4]>,
    pub ending: Option<Ending>,
}

impl Turn<'_> {
    pub fn tools(&self) -> (usize, usize) {
        let calls = self.steps.iter().filter_map(|s| match s {
            Step::Tool { failed, .. } => Some(*failed),
            _ => None,
        });
        calls.fold((0, 0), |(n, bad), failed| {
            (n + 1, bad + usize::from(failed))
        })
    }
}

/// `task`'s turns, its fan-out instances' included, each instance's attempts in order and the
/// instances in the order they first spoke.
pub fn turns<'a>(events: &'a [AgentStep], task: &str) -> Vec<Turn<'a>> {
    let mine = |name: &str| {
        name == task
            || name
                .strip_prefix(task)
                .is_some_and(|rest| rest.starts_with('['))
    };
    let mut out: Vec<Turn<'a>> = Vec::new();
    for step in events.iter().filter(|e| mine(&e.task)) {
        let at = match out
            .iter()
            .position(|t| t.task == step.task && t.attempt == step.attempt)
        {
            Some(at) => at,
            None => {
                out.push(Turn {
                    task: &step.task,
                    attempt: step.attempt,
                    model: None,
                    steps: Vec::new(),
                    tokens: None,
                    ending: None,
                });
                out.len() - 1
            }
        };
        let turn = &mut out[at];
        match &step.event {
            AgentEvent::Thinking { delta } => turn.steps.push(Step::Thought {
                text: delta,
                chars: None,
            }),
            AgentEvent::Text { delta } => turn.steps.push(Step::Said(delta)),
            AgentEvent::Tool {
                name,
                summary,
                subagent,
                input,
                result,
                failed,
            } => turn.steps.push(Step::Tool {
                name,
                summary,
                input: input.as_ref(),
                result: result.as_deref(),
                failed: *failed,
                subagent: *subagent,
            }),
            AgentEvent::Tokens(t) => turn.tokens = Some([t.input, t.output, t.cache_read, t.total]),
            AgentEvent::Retry {
                attempt,
                max,
                error,
            } => turn
                .steps
                .push(Step::Retry(format!("retry {attempt}/{max}: {error}"))),
            AgentEvent::Error {
                error_type,
                message,
            } => turn
                .steps
                .push(Step::Error(format!("{error_type}: {message}"))),
            AgentEvent::Result {
                subtype,
                is_error,
                turns,
                cost_usd,
                error,
            } => {
                turn.ending = Some(Ending {
                    subtype: subtype.clone(),
                    failed: *is_error,
                    turns: *turns,
                    cost_usd: *cost_usd,
                    error: error.clone(),
                })
            }
            AgentEvent::Meta { model } | AgentEvent::Init { model, .. } => turn.model = Some(model),
            AgentEvent::SandboxLog {
                target,
                message,
                denial: true,
                ..
            } => turn
                .steps
                .push(Step::Error(format!("sandbox denied {target}: {message}"))),
            other => {
                if let Some(line) = crate::logline::LogLine::from_agent(&step.task, other) {
                    turn.steps.push(Step::Note(line.raw));
                }
            }
        }
    }
    out.sort_by_key(|t| (out_order(events, t.task), t.attempt));
    out
}

/// `task`'s turns read back from log lines, for a run whose session kept no agent events: one
/// turn per task or instance, with only what the log line kept of each step.
pub fn turns_from_log<'a>(lines: &'a [LogLine], task: &str) -> Vec<Turn<'a>> {
    let mut out: Vec<Turn<'a>> = Vec::new();
    for line in lines.iter().filter(|l| l.task.is_some() && l.is_for(task)) {
        let Some(name) = line.task.as_deref() else {
            continue;
        };
        let at = match out.iter().position(|t| t.task == name) {
            Some(at) => at,
            None => {
                out.push(Turn {
                    task: name,
                    attempt: 1,
                    model: None,
                    steps: Vec::new(),
                    tokens: None,
                    ending: None,
                });
                out.len() - 1
            }
        };
        let turn = &mut out[at];
        let step = match &line.body {
            Body::Thinking { chars, text } => Step::Thought {
                text,
                chars: *chars,
            },
            Body::Say(said) => Step::Said(said),
            Body::Tool {
                name,
                detail,
                failed,
                subagent,
            } => Step::Tool {
                name,
                summary: detail,
                input: None,
                result: None,
                failed: *failed,
                subagent: *subagent,
            },
            Body::Tokens {
                input,
                output,
                cache_read,
                total,
            } => {
                turn.tokens = Some([*input, *output, *cache_read, *total]);
                continue;
            }
            Body::Retry(said) => Step::Retry(said.clone()),
            Body::Error(said) => Step::Error(said.clone()),
            Body::TurnResult { text, failed: true } => Step::Error(text.clone()),
            Body::TurnResult { text, .. }
            | Body::Note(text)
            | Body::Sandbox(text)
            | Body::Waiting(text) => Step::Note(text.clone()),
            _ => continue,
        };
        turn.steps.push(step);
    }
    out
}

/// Where `task` first spoke in `events`.
fn out_order(events: &[AgentStep], task: &str) -> usize {
    events
        .iter()
        .position(|e| e.task == task)
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use crate::logline::LogLine;
    use crate::session::RunFold;
    use crate::transcript::{Step, turns, turns_from_log};

    #[test]
    fn a_log_without_agent_events_still_reads_as_turns() {
        let lines: Vec<LogLine> = [
            "[fix[a]] \u{1f527} mcp__buildit__build failed: exec `sh` in pod x",
            "[fix[a]] result error_max_turns turns=40 cost=$1.2000 error: max turns",
            "[fix[b]] The build passed.",
            "[other] not mine",
            "plan v1: admitted",
        ]
        .iter()
        .map(|raw| LogLine::parse(raw))
        .collect();
        let all = turns_from_log(&lines, "fix");
        let seen: Vec<(&str, usize)> = all.iter().map(|t| (t.task, t.steps.len())).collect();
        assert_eq!(seen, [("fix[a]", 2), ("fix[b]", 1)]);
        assert_eq!(all[0].tools(), (1, 1));
        assert!(matches!(all[0].steps[1], Step::Error(_)));
        assert!(matches!(all[1].steps[0], Step::Said(_)));
    }

    const RUN: &str = concat!(
        r#"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":[{"name":"fix","kind":"agent","over":"a.b"},{"name":"other","kind":"agent"}]}"#,
        "\n",
        r#"{"v":1,"kind":"task_agent","task":"fix[b]","attempt":1,"event":{"kind":"init","model":"glm","tools":3,"agents":0}}"#,
        "\n",
        r#"{"v":1,"kind":"task_agent","task":"fix[a]","attempt":1,"event":{"kind":"thinking","delta":"which file?"}}"#,
        "\n",
        r#"{"v":1,"kind":"task_agent","task":"fix[a]","attempt":1,"event":{"kind":"tool","name":"Bash","summary":"$ make","input":{"command":"make"},"result":"ok","failed":false}}"#,
        "\n",
        r#"{"v":1,"kind":"task_agent","task":"fix[a]","attempt":1,"event":{"kind":"tool","name":"Read","summary":"x.rs","failed":true}}"#,
        "\n",
        r#"{"v":1,"kind":"task_agent","task":"fix[a]","attempt":1,"event":{"kind":"tokens","input":10,"output":5,"cache_read":100,"total":115}}"#,
        "\n",
        r#"{"v":1,"kind":"task_agent","task":"fix[a]","attempt":1,"event":{"kind":"result","subtype":"error_max_turns","is_error":true,"turns":40,"cost_usd":1.2,"error":"max turns"}}"#,
        "\n",
        r#"{"v":1,"kind":"task_agent","task":"fix[a]","attempt":2,"event":{"kind":"text","delta":"done"}}"#,
        "\n",
        r#"{"v":1,"kind":"task_agent","task":"other","attempt":1,"event":{"kind":"text","delta":"not mine"}}"#,
        "\n",
    );

    #[test]
    fn a_tasks_turns_group_by_instance_and_attempt_in_the_order_they_spoke() {
        let mut fold = RunFold::default();
        for line in RUN.lines() {
            fold.apply_line(line);
        }
        let all = turns(&fold.agent_events, "fix");
        let seen: Vec<(&str, u32)> = all.iter().map(|t| (t.task, t.attempt)).collect();
        assert_eq!(seen, [("fix[b]", 1), ("fix[a]", 1), ("fix[a]", 2)]);
        assert_eq!(all[0].model, Some("glm"));

        let first = &all[1];
        assert_eq!(
            first.steps[0],
            Step::Thought {
                text: "which file?",
                chars: None
            }
        );
        let Step::Tool { input, result, .. } = &first.steps[1] else {
            panic!("{:?}", first.steps[1]);
        };
        assert_eq!(input.and_then(|i| i["command"].as_str()), Some("make"));
        assert_eq!(*result, Some("ok"));
        assert_eq!(first.tools(), (2, 1));
        assert_eq!(first.tokens, Some([10, 5, 100, 115]));
        let ending = first.ending.as_ref().unwrap();
        assert!(ending.failed && ending.turns == 40);
        assert_eq!(all[2].steps, [Step::Said("done")]);
        assert_eq!(turns(&fold.agent_events, "other").len(), 1);
        assert!(turns(&fold.agent_events, "fi").is_empty());
    }
}
