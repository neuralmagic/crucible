//! The plugin chain every tool and retry event of an agent turn passes through, built from the
//! manifest's `[[agent.tool_plugins]]` in declaration order. A plugin may tag the step for the
//! plugins after it, stop the turn as failed, or report at the turn's end. The chain only reads
//! the agent's event stream: stopping a turn is the caller's act.

use crate::agent::event::AgentEvent;
use crate::manifest::ToolPluginSpec;
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroUsize;
use std::time::Instant;

/// What a step's tool is for, as `classify` reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ToolClass {
    Read,
    Write,
    Exec,
    Build,
    Network,
    Mcp,
    Agent,
    Other,
}

impl ToolClass {
    fn as_str(self) -> &'static str {
        match self {
            ToolClass::Read => "read",
            ToolClass::Write => "write",
            ToolClass::Exec => "exec",
            ToolClass::Build => "build",
            ToolClass::Network => "network",
            ToolClass::Mcp => "mcp",
            ToolClass::Agent => "agent",
            ToolClass::Other => "other",
        }
    }
}

/// One tool event as the chain sees it. `classify` sets `class`; `repeat_guard` compares
/// [`ToolStep::key`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolStep {
    pub(crate) tool: String,
    pub(crate) summary: String,
    pub(crate) input: Option<Value>,
    pub(crate) result: Option<String>,
    pub(crate) failed: bool,
    pub(crate) class: Option<ToolClass>,
}

/// The tool names harnesses give their shell.
const SHELL_TOOLS: [&str; 3] = ["Bash", "bash", "shell"];
const WRITE_TOOLS: [&str; 9] = [
    "Write",
    "Edit",
    "MultiEdit",
    "NotebookEdit",
    "write",
    "edit",
    "patch",
    "apply_patch",
    "file_change",
];
const READ_TOOLS: [&str; 8] = ["Read", "Grep", "Glob", "LS", "read", "grep", "glob", "list"];
const NETWORK_TOOLS: [&str; 5] = ["WebFetch", "WebSearch", "web_search", "webfetch", "fetch"];
const AGENT_TOOLS: [&str; 2] = ["Task", "Agent"];
const BUILD_PROGRAMS: [&str; 14] = [
    "go", "cargo", "make", "npm", "pnpm", "yarn", "pip", "uv", "docker", "podman", "buildah",
    "bazel", "mvn", "gradle",
];
const NETWORK_PROGRAMS: [&str; 4] = ["curl", "wget", "ssh", "scp"];

impl ToolStep {
    fn from_event(ev: &AgentEvent) -> Option<Self> {
        let AgentEvent::Tool {
            name,
            summary,
            input,
            result,
            failed,
            ..
        } = ev
        else {
            return None;
        };
        Some(Self {
            tool: name.clone(),
            summary: summary.clone(),
            input: input.clone(),
            result: result.clone(),
            failed: *failed,
            class: None,
        })
    }

    /// A result echoed apart from its call (Claude's `tool_result`), as opposed to a call.
    pub(crate) fn is_result(&self) -> bool {
        self.input.is_none() && (self.result.is_some() || self.failed)
    }

    /// A step that says how a call went: a separate result, or a call reported with its outcome.
    pub(crate) fn is_completion(&self) -> bool {
        self.is_result() || self.result.is_some() || self.failed
    }

    /// The shell command a shell call ran, as the agent wrote it.
    fn shell_command(&self) -> Option<String> {
        if !SHELL_TOOLS.contains(&self.tool.as_str()) || self.is_result() {
            return None;
        }
        if let Some(command) = self
            .input
            .as_ref()
            .and_then(|input| input.get("command"))
            .and_then(Value::as_str)
        {
            return Some(command.to_string());
        }
        let command = self.summary.strip_prefix("$ ")?;
        Some(match command.split_once("  # ") {
            Some((command, _note)) => command.to_string(),
            None => command.to_string(),
        })
    }

    /// What makes two steps the same step: the tool, summary, input, result and failure.
    pub(crate) fn key(&self) -> String {
        let mut key = format!("{}\u{0}{}", self.tool, self.summary);
        if let Some(input) = &self.input {
            key.push('\u{0}');
            key.push_str(&input.to_string());
        }
        if let Some(result) = &self.result {
            key.push('\u{0}');
            key.push_str(result);
        }
        if self.failed {
            key.push_str("\u{0}failed");
        }
        key
    }
}

/// What a plugin decided about one event.
pub(crate) enum Flow {
    Continue,
    Stop(StopReason),
}

/// One link of the chain. Every hook defaults to letting the turn go on.
pub(crate) trait ToolPlugin: Send {
    fn on_tool(&mut self, _step: &mut ToolStep) -> Flow {
        Flow::Continue
    }

    fn on_retry(&mut self, _error: &str) -> Flow {
        Flow::Continue
    }

    /// An end-of-turn report: a label and its JSON value.
    fn report(&self) -> Option<(&'static str, Value)> {
        None
    }
}

/// Why a guard plugin stopped the turn as failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StopReason {
    #[error(transparent)]
    Wedged(Wedged),
}

/// An agent that went round the same cycle of tool calls `repeats` times in a row: one call, or
/// two or three calls taking turns, with the same results each time.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("agent wedged: {} repeated {repeats} times in a row", .calls.join(" then "))]
pub struct Wedged {
    /// The distinct calls in the cycle, each as its tool and summary, in the order made.
    pub calls: Vec<String>,
    pub repeats: usize,
}

/// The ordered plugins for one turn, and the stop that ended it.
pub(crate) struct ToolChain {
    plugins: Vec<Box<dyn ToolPlugin>>,
    stopped: Option<StopReason>,
}

impl ToolChain {
    /// The chain `args` declares, or the default chain when it declares none.
    pub(crate) fn for_args(args: &crate::args::Args) -> Self {
        match &args.tool_plugins {
            Some(specs) => Self::new(specs),
            None => Self::new(&crate::manifest::default_chain()),
        }
    }

    pub(crate) fn new(specs: &[ToolPluginSpec]) -> Self {
        let plugins = specs
            .iter()
            .map(|spec| -> Box<dyn ToolPlugin> {
                match spec {
                    ToolPluginSpec::RepeatGuard { limit } => Box::new(RepeatGuard::new(*limit)),
                    ToolPluginSpec::Classify {} => Box::new(Classify),
                    ToolPluginSpec::ToolStats {} => Box::new(ToolStats::default()),
                }
            })
            .collect();
        Self {
            plugins,
            stopped: None,
        }
    }

    /// Run one event through the chain; returns the stop the first time a plugin calls one.
    pub(crate) fn observe(&mut self, ev: &AgentEvent) -> Option<&StopReason> {
        if self.stopped.is_some() {
            return None;
        }
        let mut step = ToolStep::from_event(ev);
        let retry = match ev {
            AgentEvent::Retry { error, .. } => Some(error.as_str()),
            _ => None,
        };
        if step.is_none() && retry.is_none() {
            return None;
        }
        for plugin in &mut self.plugins {
            let flow = match (&mut step, retry) {
                (Some(step), _) => plugin.on_tool(step),
                (None, Some(error)) => plugin.on_retry(error),
                (None, None) => Flow::Continue,
            };
            if let Flow::Stop(reason) = flow {
                self.stopped = Some(reason);
                return self.stopped.as_ref();
            }
        }
        None
    }

    pub(crate) fn stopped(&self) -> Option<&StopReason> {
        self.stopped.as_ref()
    }

    /// The end-of-turn reports, in chain order.
    pub(crate) fn reports(&self) -> Vec<(&'static str, Value)> {
        self.plugins
            .iter()
            .filter_map(|plugin| plugin.report())
            .collect()
    }

    pub(crate) fn into_stopped(self) -> Option<StopReason> {
        self.stopped
    }
}

struct Classify;

fn is_mcp(tool: &str) -> bool {
    tool.starts_with("mcp__") || tool.starts_with("mcp:") || tool == "mcp_tool_call"
}

fn classify(step: &ToolStep) -> ToolClass {
    let tool = step.tool.as_str();
    if WRITE_TOOLS.contains(&tool) {
        return ToolClass::Write;
    }
    if READ_TOOLS.contains(&tool) {
        return ToolClass::Read;
    }
    if NETWORK_TOOLS.contains(&tool) {
        return ToolClass::Network;
    }
    if AGENT_TOOLS.contains(&tool) {
        return ToolClass::Agent;
    }
    if is_mcp(tool) {
        return ToolClass::Mcp;
    }
    let Some(command) = step.shell_command() else {
        return ToolClass::Other;
    };
    let programs: Vec<&str> = command
        .split(['|', ';', '&', '\n'])
        .filter_map(|segment| {
            segment
                .split_whitespace()
                .find(|word| !word.contains('=') && *word != "cd" && !word.starts_with('-'))
        })
        .collect();
    if programs.iter().any(|p| BUILD_PROGRAMS.contains(p)) {
        ToolClass::Build
    } else if programs.iter().any(|p| NETWORK_PROGRAMS.contains(p))
        || ["git clone", "git fetch", "git pull"]
            .iter()
            .any(|git| command.contains(git))
    {
        ToolClass::Network
    } else {
        ToolClass::Exec
    }
}

impl ToolPlugin for Classify {
    fn on_tool(&mut self, step: &mut ToolStep) -> Flow {
        if !step.is_result() {
            step.class = Some(classify(step));
        }
        Flow::Continue
    }
}

/// The longest call text a [`Wedged`] carries.
const CALL_CAP: usize = 200;

/// The longest cycle `repeat_guard` looks for, in events: three calls, each with its result.
const MAX_CYCLE: usize = 6;

/// Trips when the trailing steps are one cycle of up to [`MAX_CYCLE`] steps repeated `limit`
/// times: one call, a call and its result, or two or three calls taking turns. Any other step in
/// between breaks the run.
struct RepeatGuard {
    limit: NonZeroUsize,
    recent: VecDeque<(String, ToolStep)>,
}

impl RepeatGuard {
    fn new(limit: NonZeroUsize) -> Self {
        Self {
            limit,
            recent: VecDeque::new(),
        }
    }

    /// The shortest cycle the trailing steps repeat `limit` times.
    fn cycle(&self) -> Option<usize> {
        let limit = self.limit.get();
        let len = self.recent.len();
        let key = |i: usize| &self.recent[i].0;
        (1..=MAX_CYCLE).find(|&period| {
            let window = period * limit;
            len >= window
                && (0..window - period).all(|i| key(len - 1 - i) == key(len - 1 - i - period))
        })
    }
}

impl ToolPlugin for RepeatGuard {
    fn on_tool(&mut self, step: &mut ToolStep) -> Flow {
        if self.recent.len() == MAX_CYCLE * self.limit.get() {
            self.recent.pop_front();
        }
        self.recent.push_back((step.key(), step.clone()));
        let Some(cycle) = self.cycle() else {
            return Flow::Continue;
        };
        let calls = self
            .recent
            .range(self.recent.len() - cycle..)
            .map(|(_, step)| step)
            .filter(|step| !step.is_result())
            .map(|step| {
                let call: String = step.summary.chars().take(CALL_CAP).collect();
                format!("{} `{call}`", step.tool)
            })
            .collect();
        Flow::Stop(StopReason::Wedged(Wedged {
            calls,
            repeats: self.limit.get(),
        }))
    }
}

#[derive(Default)]
struct ToolStats {
    tools: BTreeMap<String, Stat>,
    classes: BTreeMap<&'static str, usize>,
    open: Option<(String, Instant)>,
    retries: usize,
    fail_streak: usize,
    longest_fail_streak: usize,
}

#[derive(Default)]
struct Stat {
    calls: usize,
    failed: usize,
    millis: u128,
}

impl ToolStats {
    fn completed(&mut self, failed: bool) {
        self.fail_streak = if failed { self.fail_streak + 1 } else { 0 };
        self.longest_fail_streak = self.longest_fail_streak.max(self.fail_streak);
    }
}

impl ToolPlugin for ToolStats {
    fn on_tool(&mut self, step: &mut ToolStep) -> Flow {
        if step.is_result() {
            if let Some((tool, started)) = self.open.take()
                && tool == step.tool
            {
                self.tools.entry(tool).or_default().millis += started.elapsed().as_millis();
            }
            if step.failed {
                self.tools.entry(step.tool.clone()).or_default().failed += 1;
            }
            self.completed(step.failed);
            return Flow::Continue;
        }
        let stat = self.tools.entry(step.tool.clone()).or_default();
        stat.calls += 1;
        stat.failed += usize::from(step.failed);
        if let Some(class) = step.class {
            *self.classes.entry(class.as_str()).or_default() += 1;
        }
        if step.is_completion() {
            self.completed(step.failed);
        }
        self.open = (!step.is_completion()).then(|| (step.tool.clone(), Instant::now()));
        Flow::Continue
    }

    fn on_retry(&mut self, _error: &str) -> Flow {
        self.retries += 1;
        Flow::Continue
    }

    fn report(&self) -> Option<(&'static str, Value)> {
        let tools: serde_json::Map<String, Value> = self
            .tools
            .iter()
            .map(|(tool, stat)| {
                (
                    tool.clone(),
                    serde_json::json!({
                        "calls": stat.calls,
                        "failed": stat.failed,
                        "ms": u64::try_from(stat.millis).unwrap_or(u64::MAX),
                    }),
                )
            })
            .collect();
        Some((
            "tool_stats",
            serde_json::json!({
                "calls": self.tools.values().map(|stat| stat.calls).sum::<usize>(),
                "retries": self.retries,
                "longest_fail_streak": self.longest_fail_streak,
                "tools": tools,
                "classes": self.classes,
            }),
        ))
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::event::AgentEvent;
    use crate::agent::tool_chain::{StopReason, ToolChain, ToolPlugin, ToolStep};
    use crate::manifest::{ToolPluginSpec, default_chain};
    use serde_json::{Value, json};
    use std::num::NonZeroUsize;

    fn tool(name: &str, summary: &str, input: Option<Value>) -> AgentEvent {
        AgentEvent::Tool {
            name: name.into(),
            summary: summary.into(),
            subagent: false,
            input,
            result: None,
            failed: false,
        }
    }

    /// A Claude shell call under full tool IO: the call, then its result as a separate event.
    fn claude_bash(command: &str, output: &str, failed: bool) -> [AgentEvent; 2] {
        [
            tool(
                "Bash",
                &format!("$ {command}"),
                Some(json!({ "command": command })),
            ),
            AgentEvent::Tool {
                name: "Bash".into(),
                summary: if failed { "failed" } else { "result" }.into(),
                subagent: false,
                input: None,
                result: Some(output.into()),
                failed,
            },
        ]
    }

    /// A Codex shell call: one event carrying the command and its exit.
    fn codex_shell(command: &str, failed: bool) -> AgentEvent {
        AgentEvent::Tool {
            name: "shell".into(),
            summary: format!("$ {command}"),
            subagent: false,
            input: Some(json!({ "command": command })),
            result: Some(String::new()),
            failed,
        }
    }

    fn stop(chain: &[ToolPluginSpec], events: &[AgentEvent]) -> Option<StopReason> {
        let mut chain = ToolChain::new(chain);
        for ev in events {
            chain.observe(ev);
        }
        chain.into_stopped()
    }

    #[test]
    fn polling_whose_output_changes_is_progress_and_identical_output_is_a_wedge() {
        let changing: Vec<AgentEvent> = (0..40)
            .flat_map(|i| claude_bash("buildit status b-1", &format!("{i}% built"), false))
            .collect();
        assert_eq!(stop(&default_chain(), &changing), None);
        let same: Vec<AgentEvent> = (0..40)
            .flat_map(|_| claude_bash("cat /tmp/deps.json", "{}", false))
            .collect();
        assert!(matches!(
            stop(&default_chain(), &same),
            Some(StopReason::Wedged(_))
        ));
    }

    fn wedged_calls(chain: &[ToolPluginSpec], events: &[AgentEvent]) -> Option<Vec<String>> {
        match stop(chain, events) {
            Some(StopReason::Wedged(wedged)) => Some(wedged.calls),
            None => None,
        }
    }

    fn guard(limit: usize) -> [ToolPluginSpec; 1] {
        [ToolPluginSpec::RepeatGuard {
            limit: NonZeroUsize::new(limit).unwrap(),
        }]
    }

    #[test]
    fn two_calls_taking_turns_with_the_same_results_are_a_wedge() {
        let cycle = |n: usize| -> Vec<AgentEvent> {
            (0..n)
                .flat_map(|_| {
                    [
                        claude_bash("go build ./...", "ok", false),
                        claude_bash("cat build.log", "error: x", false),
                    ]
                    .concat()
                })
                .collect()
        };
        assert_eq!(wedged_calls(&guard(5), &cycle(4)), None);
        assert_eq!(
            wedged_calls(&guard(5), &cycle(5)),
            Some(vec![
                "Bash `$ go build ./...`".to_string(),
                "Bash `$ cat build.log`".to_string()
            ])
        );
        let codex: Vec<AgentEvent> = (0..5)
            .flat_map(|_| [codex_shell("make", true), codex_shell("make clean", false)])
            .collect();
        assert_eq!(
            wedged_calls(&guard(5), &codex).map(|calls| calls.len()),
            Some(2),
            "a harness that reports a call with its outcome alternates in two events"
        );
    }

    #[test]
    fn three_calls_in_a_cycle_are_a_wedge() {
        let events: Vec<AgentEvent> = (0..5)
            .flat_map(|_| {
                [
                    claude_bash("make", "", true),
                    claude_bash("git diff", "", false),
                    claude_bash("git stash", "", false),
                ]
                .concat()
            })
            .collect();
        assert_eq!(wedged_calls(&guard(5), &events).map(|c| c.len()), Some(3));
    }

    #[test]
    fn calls_taking_turns_with_changing_results_are_progress() {
        let events: Vec<AgentEvent> = (0..40)
            .flat_map(|i| {
                [
                    claude_bash("go test ./...", &format!("{i} failures"), true),
                    claude_bash("git diff --stat", &format!("{i} files"), false),
                ]
                .concat()
            })
            .collect();
        assert_eq!(stop(&default_chain(), &events), None);
    }

    #[test]
    fn a_single_repeat_reports_the_call_once() {
        let events: Vec<AgentEvent> = (0..3).map(|_| tool("Bash", "$ ls", None)).collect();
        assert_eq!(
            wedged_calls(&guard(3), &events),
            Some(vec!["Bash `$ ls`".to_string()])
        );
    }

    #[test]
    fn the_same_command_in_different_directories_is_progress() {
        let events: Vec<AgentEvent> = (0..40)
            .map(|i| tool("Bash", &format!("$ cd pkg{i} && go test ./..."), None))
            .collect();
        assert_eq!(stop(&default_chain(), &events), None);
    }

    #[test]
    fn the_default_chain_stops_at_twenty_five_repeats() {
        let events: Vec<AgentEvent> = (0..25).map(|_| tool("Bash", "$ ls", None)).collect();
        assert_eq!(stop(&default_chain(), &events[..24]), None);
        assert!(stop(&default_chain(), &events).is_some());
    }

    #[test]
    fn each_step_is_classified_by_its_tool_and_command() {
        let class = |ev: AgentEvent| {
            let mut step = ToolStep::from_event(&ev).unwrap();
            crate::agent::tool_chain::Classify.on_tool(&mut step);
            step.class.map(|c| c.as_str())
        };
        for (summary, expected) in [
            ("$ ROOT=/x; cd $ROOT && go test ./...", "build"),
            ("$ curl -sS https://example.com", "network"),
            ("$ git clone https://x/y", "network"),
            ("$ grep -rn foo .", "exec"),
        ] {
            assert_eq!(
                class(tool("Bash", summary, None)),
                Some(expected),
                "{summary}"
            );
        }
        assert_eq!(class(tool("Read", "/a", None)), Some("read"));
        assert_eq!(class(tool("Edit", "a: x", None)), Some("write"));
        assert_eq!(class(tool("WebFetch", "u", None)), Some("network"));
        assert_eq!(class(tool("mcp__x__y", "", None)), Some("mcp"));
        assert_eq!(class(tool("Task", "[Explore]", None)), Some("agent"));
        assert_eq!(
            class(claude_bash("ls", "a", false)[1].clone()),
            None,
            "a result is not classified"
        );
    }

    #[test]
    fn tool_stats_report_calls_failures_classes_retries_and_the_longest_failure_run() {
        let mut chain =
            ToolChain::new(&[ToolPluginSpec::Classify {}, ToolPluginSpec::ToolStats {}]);
        let mut events = [
            claude_bash("go build ./...", "", false),
            claude_bash("go test ./...", "FAIL", true),
            claude_bash("go test ./a", "FAIL", true),
        ]
        .concat();
        events.push(AgentEvent::Retry {
            attempt: 1,
            max: 15,
            error: "overloaded".into(),
        });
        events.extend([
            codex_shell("make", true),
            codex_shell("make", true),
            codex_shell("make lint", false),
        ]);
        for ev in &events {
            assert!(chain.observe(ev).is_none());
        }
        let reports = chain.reports();
        let [(label, stats)] = reports.as_slice() else {
            panic!("one report expected, got {reports:?}");
        };
        assert_eq!(*label, "tool_stats");
        assert_eq!(stats["calls"], 6);
        assert_eq!(stats["retries"], 1);
        assert_eq!(stats["longest_fail_streak"], 4);
        assert_eq!(
            stats["tools"]["Bash"],
            json!({"calls": 3, "failed": 2, "ms": stats["tools"]["Bash"]["ms"]})
        );
        assert_eq!(stats["tools"]["shell"]["failed"], 2);
        assert_eq!(stats["classes"], json!({"build": 6}));
    }

    #[test]
    fn an_empty_chain_never_stops_a_turn_or_reports() {
        let events: Vec<AgentEvent> = (0..100).map(|_| tool("Bash", "$ ls", None)).collect();
        let mut chain = ToolChain::new(&[]);
        for ev in &events {
            assert!(chain.observe(ev).is_none());
        }
        assert!(chain.reports().is_empty());
    }

    #[test]
    fn a_guard_stops_the_chain_once_and_later_events_pass_through() {
        let limit = NonZeroUsize::new(3).unwrap();
        let mut chain = ToolChain::new(&[ToolPluginSpec::RepeatGuard { limit }]);
        let ls = tool("Bash", "$ ls", None);
        assert!(chain.observe(&ls).is_none());
        assert!(chain.observe(&ls).is_none());
        assert!(chain.observe(&ls).is_some());
        assert!(chain.observe(&ls).is_none(), "a stop is reported once");
        assert!(chain.stopped().is_some());
    }
}
