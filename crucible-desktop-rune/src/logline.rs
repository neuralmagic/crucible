//! The engine's console output, one typed line at a time.
//!
//! The engine writes four shapes: `[crucible]` notices, `tracing` lines, `[task] …` progress from
//! each agent turn, and the end-of-run summary. Anything else is kept as plain text.
//!
//! ```text
//! [crucible] OTLP span export enabled -> …                       Notice
//! 2026-11-05T23:47:44.432Z  INFO serve_inner: input stream …     Trace
//! [edit[cache]] 🔧 Bash $ git init -q build/vllm                 Agent / Tool
//!   seed                 pass       attempts=1 cost=$0.0000  out=…  TaskRow
//! plan v1: completed — spent $0.0000 of $50                      Summary
//! ```

use crucible_contract::event::AgentEvent;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Level {
    fn parse(token: &str) -> Option<Self> {
        Some(match token {
            "ERROR" => Level::Error,
            "WARN" => Level::Warn,
            "INFO" => Level::Info,
            "DEBUG" => Level::Debug,
            "TRACE" => Level::Trace,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    Notice {
        text: String,
        warning: bool,
    },
    Trace {
        level: Level,
        target: String,
        text: String,
    },
    Tool {
        name: String,
        detail: String,
        failed: bool,
        subagent: bool,
    },
    Thinking {
        chars: Option<u64>,
        text: String,
    },
    Tokens {
        input: u64,
        output: u64,
        cache_read: u64,
        total: u64,
    },
    Retry(String),
    Error(String),
    TurnResult {
        text: String,
        failed: bool,
    },
    Waiting(String),
    Sandbox(String),
    Say(String),
    TaskRow {
        name: String,
        status: String,
        attempts: u32,
        cost: String,
        rest: String,
    },
    Summary(String),
    /// An agent lifecycle line from a session transcript: init, gates, sandbox, exit.
    Note(String),
    /// A line a command task printed, from its session's `task_output` events.
    Output {
        stderr: bool,
        text: String,
    },
    Plain(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogLine {
    /// `HH:MM:SS` when the line carried a timestamp.
    pub at: Option<String>,
    /// The task or fan-out instance an agent line came from.
    pub task: Option<String>,
    pub body: Body,
    pub raw: String,
}

/// Which switch in the log pane a line answers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Agent,
    Task,
    Engine,
    /// Library chatter: `tracing` lines below WARN.
    Chatter,
}

impl LogLine {
    /// The lines a command task printed, from one `task_output` event, each a line for `task`.
    pub fn from_output(task: &str, stderr: bool, lines: &[String]) -> Vec<Self> {
        lines
            .iter()
            .map(|text| LogLine {
                at: None,
                task: Some(task.to_string()),
                body: Body::Output {
                    stderr,
                    text: text.clone(),
                },
                raw: text.clone(),
            })
            .collect()
    }

    /// What `task_output_dropped` says: the lines past the engine's cap that were not kept.
    pub fn dropped_output(task: &str, lines: u64) -> Self {
        let text = format!("{lines} more output lines past the cap were not recorded");
        LogLine {
            at: None,
            task: Some(task.to_string()),
            body: Body::Notice {
                text: text.clone(),
                warning: true,
            },
            raw: text,
        }
    }

    /// One event from a session's agent transcript, as a log line for `task`. Undecoded output
    /// has no line.
    pub fn from_agent(task: &str, event: &AgentEvent) -> Option<Self> {
        let body = match event {
            AgentEvent::Text { delta } => Body::Say(delta.clone()),
            AgentEvent::Thinking { delta } => Body::Thinking {
                chars: None,
                text: delta.clone(),
            },
            AgentEvent::Tool {
                name,
                summary,
                subagent,
                result,
                failed,
                ..
            } => Body::Tool {
                name: name.clone(),
                detail: match result {
                    Some(result) if !result.is_empty() => format!("{summary}\n{result}"),
                    _ => summary.clone(),
                },
                failed: *failed,
                subagent: *subagent,
            },
            AgentEvent::Tokens(tokens) => Body::Tokens {
                input: tokens.input,
                output: tokens.output,
                cache_read: tokens.cache_read,
                total: tokens.total,
            },
            AgentEvent::Retry {
                attempt,
                max,
                error,
            } => Body::Retry(format!("{attempt}/{max} {error}")),
            AgentEvent::Error {
                error_type,
                message,
            } => Body::Error(format!("{error_type}: {message}")),
            AgentEvent::Result {
                subtype,
                is_error,
                turns,
                cost_usd,
                error,
            } => Body::TurnResult {
                text: match error {
                    Some(error) if *is_error => {
                        format!("{subtype} turns={turns} cost=${cost_usd:.4} error: {error}")
                    }
                    _ => format!("{subtype} turns={turns} cost=${cost_usd:.4}"),
                },
                failed: *is_error,
            },
            AgentEvent::Meta { model } => Body::Note(format!("model {model}")),
            AgentEvent::Init {
                model,
                tools,
                agents,
            } => Body::Note(format!(
                "started {model} with {tools} tools, {agents} agents"
            )),
            AgentEvent::Log { label, value, .. } => Body::Note(match value {
                Some(value) => format!("{label}: {value}"),
                None => label.clone(),
            }),
            AgentEvent::Gate {
                phase,
                name,
                status,
                error,
            } => Body::Note(match error {
                Some(error) => format!("{phase} gate {name} {status}: {error}"),
                None => format!("{phase} gate {name} {status}"),
            }),
            AgentEvent::OtelSummary {
                cost_usd,
                total,
                api_requests,
                ..
            } => Body::Note(format!(
                "{total} tokens over {api_requests} requests, ${cost_usd:.4}"
            )),
            AgentEvent::Exit { code } => Body::Note(format!("agent exited {code}")),
            AgentEvent::SandboxLog {
                target,
                message,
                denial,
                ..
            } => {
                if *denial {
                    Body::Error(format!("sandbox denied: {message}"))
                } else {
                    Body::Sandbox(format!("{target}: {message}"))
                }
            }
            AgentEvent::Raw { .. } => return None,
        };
        let mut line = LogLine {
            at: None,
            task: Some(task.to_string()),
            body,
            raw: String::new(),
        };
        line.raw = line.summary().unwrap_or_default();
        Some(line)
    }

    pub fn parse(raw: &str) -> Self {
        let raw = raw.trim_end().to_string();
        let (task, rest) = task_prefix(&raw);
        let mut line = LogLine {
            at: None,
            task: None,
            body: Body::Plain(raw.clone()),
            raw: raw.clone(),
        };
        if let Some(text) = raw.strip_prefix("[crucible] ") {
            line.body = Body::Notice {
                warning: text.starts_with("WARNING") || text.starts_with("warning"),
                text: text.to_string(),
            };
        } else if let Some(task) = task {
            line.task = Some(task.to_string());
            line.body = agent_body(rest);
        } else if let Some((at, body)) = trace(&raw) {
            line.at = Some(at);
            line.body = body;
        } else if let Some(body) = task_row(&raw) {
            line.body = body;
        } else if raw.starts_with("plan v") || raw.starts_with("verdict: ") {
            line.body = Body::Summary(raw.clone());
        }
        line
    }

    pub fn group(&self) -> Group {
        match &self.body {
            Body::Trace { level, .. } if *level > Level::Warn => Group::Chatter,
            Body::Notice { .. } | Body::Trace { .. } | Body::Summary(_) | Body::Plain(_) => {
                Group::Engine
            }
            Body::TaskRow { .. } | Body::Output { .. } => Group::Task,
            _ => Group::Agent,
        }
    }

    /// Whether the line reports something going wrong.
    pub fn is_problem(&self) -> bool {
        match &self.body {
            Body::Notice { warning, .. } => *warning,
            Body::Trace { level, .. } => *level <= Level::Warn,
            Body::Tool { failed, .. } => *failed,
            Body::Retry(_) | Body::Error(_) => true,
            Body::TurnResult { failed, .. } => *failed,
            Body::TaskRow { status, .. } => {
                !matches!(status.as_str(), "pass" | "skipped" | "not_taken")
            }
            _ => false,
        }
    }

    /// Whether the line belongs to `task`, counting its fan-out instances.
    pub fn is_for(&self, task: &str) -> bool {
        let named = match (&self.task, &self.body) {
            (Some(name), _) => name.as_str(),
            (None, Body::TaskRow { name, .. }) => name.as_str(),
            _ => return false,
        };
        named == task
            || named
                .strip_prefix(task)
                .is_some_and(|rest| rest.starts_with('['))
    }
}

/// Which lines the log pane shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogView {
    All,
    Agents,
    Problems,
    Engine,
}

impl LogView {
    pub const ALL: [LogView; 4] = [
        LogView::All,
        LogView::Agents,
        LogView::Problems,
        LogView::Engine,
    ];

    pub fn label(self) -> &'static str {
        match self {
            LogView::All => "All",
            LogView::Agents => "Agents",
            LogView::Problems => "Problems",
            LogView::Engine => "Engine",
        }
    }

    /// `chatter` lets library `tracing` lines below WARN through.
    pub fn keeps(self, line: &LogLine, chatter: bool) -> bool {
        let group = line.group();
        if group == Group::Chatter && !chatter {
            return false;
        }
        match self {
            LogView::All => true,
            LogView::Agents => group == Group::Agent,
            LogView::Problems => line.is_problem(),
            LogView::Engine => group != Group::Agent,
        }
    }
}

/// What one task's agent lines add up to, its fan-out instances included.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Activity {
    pub tools: usize,
    pub failed_tools: usize,
    pub errors: usize,
    /// The newest token total each instance reported, summed.
    pub tokens: u64,
    /// The newest thing the agent did, as one line.
    pub last: Option<String>,
}

pub fn activity(log: &[LogLine], task: &str) -> Activity {
    let mut activity = Activity::default();
    let mut totals: Vec<(&str, u64)> = Vec::new();
    for line in log
        .iter()
        .filter(|line| line.task.is_some() && line.is_for(task))
    {
        let instance = line.task.as_deref().unwrap_or(task);
        match &line.body {
            Body::Tool { failed, .. } => {
                activity.tools += 1;
                activity.failed_tools += usize::from(*failed);
            }
            Body::Error(_) | Body::Retry(_) => activity.errors += 1,
            Body::TurnResult { failed: true, .. } => activity.errors += 1,
            Body::Tokens { total, .. } => {
                match totals.iter_mut().find(|(name, _)| *name == instance) {
                    Some(slot) => slot.1 = *total,
                    None => totals.push((instance, *total)),
                }
            }
            _ => {}
        }
        if let Some(summary) = line.summary() {
            activity.last = Some(summary);
        }
    }
    activity.tokens = totals.iter().map(|(_, total)| total).sum();
    activity
}

impl LogLine {
    /// The line as one short phrase, for a task card's "last seen"; `None` for token samples.
    pub fn summary(&self) -> Option<String> {
        Some(match &self.body {
            Body::Tool {
                name,
                detail,
                failed,
                ..
            } => {
                let verb = if *failed { " failed" } else { "" };
                format!("{name}{verb} {detail}").trim_end().to_string()
            }
            Body::Thinking {
                chars: Some(chars), ..
            } => format!("thinking ({chars} chars)"),
            Body::Thinking { text, .. } => format!("thinking: {text}"),
            Body::Tokens { .. } => return None,
            Body::Retry(text) => format!("retry {text}"),
            Body::Error(text) | Body::Waiting(text) | Body::Sandbox(text) | Body::Say(text) => {
                text.clone()
            }
            Body::TurnResult { text, .. } => format!("result {text}"),
            Body::Notice { text, .. }
            | Body::Summary(text)
            | Body::Note(text)
            | Body::Output { text, .. }
            | Body::Plain(text) => text.clone(),
            Body::Trace { text, .. } => text.clone(),
            Body::TaskRow { name, status, .. } => format!("{name} {status}"),
        })
    }
}

/// `[edit[cache]] rest` → `("edit[cache]", "rest")`, brackets nested; `[crucible]` is not a task.
fn task_prefix(raw: &str) -> (Option<&str>, &str) {
    if !raw.starts_with('[') || raw.starts_with("[crucible]") {
        return (None, raw);
    }
    let mut depth = 0usize;
    for (at, c) in raw.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    let name = &raw[1..at];
                    let rest = raw[at + 1..].strip_prefix(' ');
                    return match rest {
                        Some(rest) if !name.is_empty() => (Some(name), rest),
                        _ => (None, raw),
                    };
                }
            }
            _ => {}
        }
    }
    (None, raw)
}

fn agent_body(text: &str) -> Body {
    if let Some(rest) = text.strip_prefix("\u{1f527} ") {
        return tool(rest, false);
    }
    if let Some(rest) = text.strip_prefix("\u{1f916} ") {
        return tool(rest, true);
    }
    if let Some(rest) = text.strip_prefix("\u{1f9e0} ") {
        let chars = rest
            .strip_prefix("thinking\u{2026} ")
            .and_then(|n| n.strip_suffix(" chars"))
            .and_then(|n| n.parse().ok());
        return Body::Thinking {
            chars,
            text: rest.to_string(),
        };
    }
    if let Some(rest) = text.strip_prefix("\u{1f4ca} TOKENS ") {
        let field = |name: &str| {
            rest.split_whitespace()
                .find_map(|kv| kv.strip_prefix(name)?.strip_prefix('=')?.parse().ok())
                .unwrap_or(0)
        };
        return Body::Tokens {
            input: field("in"),
            output: field("out"),
            cache_read: field("cache_r"),
            total: field("total"),
        };
    }
    if let Some(rest) = text.strip_prefix("\u{1f504} ") {
        return Body::Retry(rest.to_string());
    }
    if let Some(rest) = text.strip_prefix("\u{274c} ") {
        return Body::Error(rest.to_string());
    }
    if let Some(rest) = text.strip_prefix("result ") {
        return Body::TurnResult {
            failed: rest.contains(" error: "),
            text: rest.to_string(),
        };
    }
    if text.starts_with("waiting on the model") {
        return Body::Waiting(text.to_string());
    }
    if [
        "starting the openshell gateway",
        "creating sandbox",
        "uploading the workspace",
        "sandbox ready",
    ]
    .iter()
    .any(|lifecycle| text.starts_with(lifecycle))
    {
        return Body::Sandbox(text.to_string());
    }
    Body::Say(text.to_string())
}

/// `Bash $ ls` or `mcp__buildit__status failed: …`.
fn tool(text: &str, subagent: bool) -> Body {
    let (name, detail) = text.split_once(' ').unwrap_or((text, ""));
    let (failed, detail) = match detail.strip_prefix("failed: ") {
        Some(rest) => (true, rest),
        None => (false, detail),
    };
    Body::Tool {
        name: name.to_string(),
        detail: detail.to_string(),
        failed,
        subagent,
    }
}

/// `2026-11-05T23:47:44.432586Z  INFO serve_inner: input stream terminated`.
fn trace(raw: &str) -> Option<(String, Body)> {
    let (stamp, rest) = raw.split_once(' ')?;
    let (_, clock) = stamp.split_once('T')?;
    if !stamp.ends_with('Z') || clock.len() < 8 || !stamp.as_bytes().first()?.is_ascii_digit() {
        return None;
    }
    let rest = rest.trim_start();
    let (level, rest) = rest.split_once(' ').unwrap_or((rest, ""));
    let level = Level::parse(level)?;
    let rest = rest.trim_start();
    let (target, text) = match rest.split_once(": ") {
        Some((target, text)) if !target.contains(' ') => (target.to_string(), text.to_string()),
        _ => (String::new(), rest.to_string()),
    };
    Some((
        clock[..8].to_string(),
        Body::Trace {
            level,
            target,
            text,
        },
    ))
}

/// `  seed                 pass       attempts=1 cost=$0.0000  out={…}  (note)`.
fn task_row(raw: &str) -> Option<Body> {
    let body = raw.strip_prefix("  ")?;
    let mut words = body.split_whitespace();
    let name = words.next()?;
    let status = words.next()?;
    let attempts = words.next()?.strip_prefix("attempts=")?.parse().ok()?;
    let cost = words.next()?.strip_prefix("cost=")?;
    let rest = body
        .split_once(&format!("cost={cost}"))
        .map(|(_, rest)| rest.trim().to_string())
        .unwrap_or_default();
    Some(Body::TaskRow {
        name: name.to_string(),
        status: status.to_string(),
        attempts,
        cost: cost.to_string(),
        rest,
    })
}

#[cfg(test)]
mod tests {
    use crate::logline::{Activity, Body, Group, Level, LogLine, LogView, activity};

    #[test]
    fn engine_notices_and_traces_read_as_engine_lines() {
        let notice = LogLine::parse("[crucible] WARNING: this pack is edition 1, so …");
        assert_eq!(notice.group(), Group::Engine);
        assert!(notice.is_problem());

        let info = LogLine::parse(
            "2026-11-05T23:47:44.432710Z  INFO serve_inner: input stream terminated",
        );
        assert_eq!(info.at.as_deref(), Some("23:47:44"));
        assert_eq!(
            info.body,
            Body::Trace {
                level: Level::Info,
                target: "serve_inner".into(),
                text: "input stream terminated".into()
            }
        );
        assert_eq!(info.group(), Group::Chatter);

        let warn = LogLine::parse("2026-11-05T23:47:44.4Z  WARN the cache is cold");
        assert_eq!(warn.group(), Group::Engine);
        assert!(warn.is_problem());
        assert!(matches!(warn.body, Body::Trace { ref target, .. } if target.is_empty()));
    }

    #[test]
    fn agent_progress_names_its_instance_and_its_kind() {
        let tool = LogLine::parse("[edit[cache]] \u{1f527} Bash $ git init -q build/vllm");
        assert_eq!(tool.task.as_deref(), Some("edit[cache]"));
        assert!(tool.is_for("edit"));
        assert!(tool.is_for("edit[cache]"));
        assert!(!tool.is_for("edit[cach"));
        assert!(!tool.is_for("ed"));
        assert_eq!(
            tool.body,
            Body::Tool {
                name: "Bash".into(),
                detail: "$ git init -q build/vllm".into(),
                failed: false,
                subagent: false
            }
        );

        let failed =
            LogLine::parse("[edit[a]] \u{1f527} mcp__buildit__build failed: exec `sh` in pod x");
        assert!(failed.is_problem());
        assert!(
            matches!(failed.body, Body::Tool { failed: true, ref detail, .. } if detail == "exec `sh` in pod x")
        );

        assert_eq!(
            LogLine::parse("[edit] \u{1f9e0} thinking\u{2026} 215 chars").body,
            Body::Thinking {
                chars: Some(215),
                text: "thinking\u{2026} 215 chars".into()
            }
        );
        assert_eq!(
            LogLine::parse(
                "[edit] \u{1f4ca} TOKENS in=4241 out=137 cache_r=20480 cache_w=0 total=24858"
            )
            .body,
            Body::Tokens {
                input: 4241,
                output: 137,
                cache_read: 20480,
                total: 24858
            }
        );
        assert!(matches!(
            LogLine::parse("[edit] sandbox ready \u{2014} starting the agent").body,
            Body::Sandbox(_)
        ));
        assert!(matches!(
            LogLine::parse("[edit] waiting on the model 1m 4s (no event yet)").body,
            Body::Waiting(_)
        ));
        assert!(matches!(
            LogLine::parse("[edit] The build failed. Retrying:").body,
            Body::Say(_)
        ));
        let result =
            LogLine::parse("[edit] result error_max_turns turns=40 cost=$1.2000 error: max turns");
        assert!(result.is_problem());
        assert!(LogLine::parse("[edit] \u{274c} Error: overloaded: try later").is_problem());
        assert!(LogLine::parse("[edit] \u{1f504} Retry 1/3 529").is_problem());
        assert_eq!(result.group(), Group::Agent);
    }

    #[test]
    fn the_summary_table_reads_as_task_rows() {
        let row = LogLine::parse(
            "  discover             pass       attempts=1 cost=$0.0000  out={\"flaws\":{}}",
        );
        assert_eq!(
            row.body,
            Body::TaskRow {
                name: "discover".into(),
                status: "pass".into(),
                attempts: 1,
                cost: "$0.0000".into(),
                rest: "out={\"flaws\":{}}".into()
            }
        );
        assert_eq!(row.group(), Group::Task);
        assert!(row.is_for("discover"));
        assert!(!row.is_problem());
        assert!(LogLine::parse("  fix  fail  attempts=2 cost=$0.1000  (broke)").is_problem());
        assert!(matches!(
            LogLine::parse("plan v1: completed \u{2014} spent $0.0000 of $50").body,
            Body::Summary(_)
        ));
    }

    const RUN: &str = "[crucible] OTLP span export enabled -> http://tempo
2026-11-05T23:47:44.432710Z  INFO serve_inner: input stream terminated
2026-11-05T23:47:45.000000Z  WARN serve_inner: builder pod restarted
[edit[a]] sandbox ready \u{2014} starting the agent
[edit[a]] \u{1f527} Bash $ git init
[edit[a]] \u{1f4ca} TOKENS in=10 out=5 cache_r=0 cache_w=0 total=100
[edit[b]] \u{1f527} mcp__buildit__build failed: pod gone
[edit[a]] \u{1f4ca} TOKENS in=10 out=5 cache_r=0 cache_w=0 total=250
[edit[b]] \u{1f4ca} TOKENS in=10 out=5 cache_r=0 cache_w=0 total=40
[edit[b]] \u{274c} Error: overloaded: later
[edit[b]] The build call failed. Retrying:
[audit] \u{1f527} Read report.md
  edit                 pass       attempts=1 cost=$0.0000";

    fn lines() -> Vec<LogLine> {
        RUN.lines().map(LogLine::parse).collect()
    }

    #[test]
    fn a_task_rolls_up_its_instances_agent_lines() {
        let log = lines();
        let edit = activity(&log, "edit");
        assert_eq!(edit.tools, 2);
        assert_eq!(edit.failed_tools, 1);
        assert_eq!(edit.errors, 1);
        assert_eq!(edit.tokens, 290, "newest total per instance, summed");
        assert_eq!(
            edit.last.as_deref(),
            Some("The build call failed. Retrying:")
        );

        let one = activity(&log, "edit[a]");
        assert_eq!((one.tools, one.tokens), (1, 250));
        assert_eq!(
            activity(&log, "audit").last.as_deref(),
            Some("Read report.md")
        );
        assert_eq!(activity(&log, "nobody"), Activity::default());
    }

    #[test]
    fn each_view_keeps_its_own_lines_and_chatter_stays_hidden_by_default() {
        let log = lines();
        let shown = |view: LogView, chatter: bool| {
            log.iter().filter(|line| view.keeps(line, chatter)).count()
        };
        assert_eq!(shown(LogView::All, false), log.len() - 1);
        assert_eq!(shown(LogView::All, true), log.len());
        assert_eq!(shown(LogView::Agents, false), 9);
        assert_eq!(shown(LogView::Problems, false), 3);
        assert_eq!(shown(LogView::Engine, false), 3);
        assert_eq!(shown(LogView::Engine, true), 4);
    }

    #[test]
    fn anything_else_stays_plain() {
        for raw in [
            "",
            "  not a row",
            "[] empty",
            "[unclosed thing",
            "2026 INFO nope",
        ] {
            assert!(matches!(LogLine::parse(raw).body, Body::Plain(_)), "{raw}");
        }
    }
}
