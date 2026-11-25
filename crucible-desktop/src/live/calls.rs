//! The calls a run dispatched, read from the controller's call progress, and the launches their
//! playbook target runs belong to.

use std::collections::HashMap;

use crucible_contract::calls::{
    CALL_PROGRESS_PATH, CallId, CallKind, CallStatus, CallView, NodeLink, ProgressNode,
    TargetHandle,
};
use serde::Deserialize;
use serde_json::Value;

use crate::logline::{Body, LogLine};
use crate::net::{Connection, within};
use crate::session::TaskView;

/// The route that answers one node of a call's target graph's log as text.
pub const CALL_NODE_LOG_PATH: &str = "/api/runs/{run}/calls/{call}/nodes/{node}/log";

/// One call of the run on screen.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveCall {
    pub view: CallView,
    /// The launch key of a playbook target run, once the controller named it.
    pub launch: Option<String>,
}

impl LiveCall {
    /// The target run's id, for a playbook target.
    pub fn target_run(&self) -> Option<&str> {
        match &self.view.state.handle {
            TargetHandle::Playbook { run } => Some(run),
            TargetHandle::GithubWorkflow { .. } => None,
        }
    }

    /// Where `link` leads: a task of a launch the app can open, or a web page.
    pub fn jump(&self, link: &NodeLink) -> Option<Jump> {
        match link {
            NodeLink::External { url } => Some(Jump::Web(url.as_str().to_string())),
            NodeLink::Run { run, task } => {
                let launch = self
                    .launch
                    .clone()
                    .filter(|_| self.target_run() == Some(run.as_str()))?;
                Some(Jump::Launch {
                    launch,
                    task: task.clone(),
                })
            }
        }
    }

    /// The node `id` of its target's progress.
    pub fn node(&self, id: &str) -> Option<&ProgressNode> {
        self.view
            .state
            .progress
            .nodes
            .iter()
            .find(|node| node.id == id)
    }

    /// Where the target run opens as a whole.
    pub fn open(&self) -> Option<Jump> {
        match (&self.launch, &self.view.state.link) {
            (Some(launch), _) => Some(Jump::Launch {
                launch: launch.clone(),
                task: None,
            }),
            (None, Some(link)) => self.jump(link),
            (None, None) => None,
        }
    }
}

/// Where pressing a call's node or one of its target's nodes goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Jump {
    Launch {
        launch: String,
        task: Option<String>,
    },
    Web(String),
}

/// What opening a target run is called: a run in this app, or a workflow run on GitHub.
pub fn open_label(kind: CallKind) -> &'static str {
    match kind {
        CallKind::Playbook => "Open run",
        CallKind::GithubWorkflow => "Open on GitHub",
    }
}

/// Where unmapped call `task`'s target run opens, and what that action is called.
pub fn target(task: &TaskView, calls: &[LiveCall]) -> Option<(&'static str, Jump)> {
    let call = task.call.as_ref()?;
    if let Some(live) = newest(calls, &task.name, None) {
        return Some((open_label(live.view.kind), live.open()?));
    }
    match call.link()? {
        NodeLink::External { url } => {
            Some((open_label(call.kind), Jump::Web(url.as_str().to_string())))
        }
        NodeLink::Run { .. } => None,
    }
}

/// One node of one call's target graph, whose log the Log pane reads.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeAsk {
    pub run: String,
    pub call: CallId,
    pub node: String,
}

impl NodeAsk {
    /// The node's log route under `controller`.
    pub fn path(&self, controller: &str) -> String {
        crucible_contract::calls::url(controller, CALL_NODE_LOG_PATH, &self.run, Some(&self.call))
            .replace(
                "{node}",
                &crucible_contract::artifact::path_segment(&self.node),
            )
    }
}

/// The target node the Log pane shows: what to read, and what to call it.
#[derive(Clone, Debug, PartialEq)]
pub struct CallNode {
    pub ask: NodeAsk,
    /// The call's task, `task` or `task[key]`.
    pub frame: String,
    pub label: String,
    pub jump: Option<Jump>,
}

/// What the controller answered for a node's log.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeLog {
    pub ask: NodeAsk,
    pub text: Result<String, String>,
}

/// The log of node `ask`, as the controller has it now.
pub(in crate::live) async fn node_log(
    connection: &Connection,
    ask: &NodeAsk,
) -> Result<String, String> {
    within("the node's log", connection.text(&ask.path(""))).await
}

/// `task`, or `task[key]` for an instance of it.
pub fn frame_name(task: &str, instance: Option<&str>) -> String {
    match instance {
        Some(key) => format!("{task}[{key}]"),
        None => task.to_string(),
    }
}

/// A call's life as log lines of its task: dispatched with where its target runs, running, and
/// how it settled, with the result or the note.
pub fn lifecycle(tasks: &[TaskView], calls: &[LiveCall]) -> Vec<LogLine> {
    let line = |task: &str, text: String, warning: bool| LogLine {
        at: None,
        task: Some(task.to_string()),
        raw: format!("[{task}] {text}"),
        body: Body::Notice { text, warning },
    };
    let mut lines = Vec::new();
    for task in tasks {
        let Some(call) = &task.call else {
            continue;
        };
        for dispatched in &call.dispatched {
            let instance = dispatched.instance.as_deref();
            let frame = frame_name(&task.name, instance);
            let live = newest(calls, &task.name, instance);
            let link = live
                .and_then(|live| live.view.state.link.as_ref())
                .or(dispatched.link.as_ref());
            let target = match (link, &dispatched.handle) {
                (Some(NodeLink::External { url }), _) => url.as_str().to_string(),
                (Some(NodeLink::Run { run, .. }), _) | (None, TargetHandle::Playbook { run }) => {
                    format!("run {run}")
                }
                (None, TargetHandle::GithubWorkflow { repo, run_id }) => {
                    format!("{} run {run_id}", repo.as_str())
                }
            };
            lines.push(line(
                &frame,
                format!("dispatched {} \u{2192} {target}", call.alias.as_str()),
                false,
            ));
            let Some(live) = live else {
                continue;
            };
            let state = &live.view.state;
            if state.status != CallStatus::Pending {
                lines.push(line(&frame, "running".to_string(), false));
            }
            let said = |prefix: &str, detail: String| match detail.is_empty() {
                true => prefix.to_string(),
                false => format!("{prefix} \u{b7} {detail}"),
            };
            match state.status {
                CallStatus::Passed => {
                    lines.push(line(&frame, said("passed", brief(&state.result)), false))
                }
                CallStatus::Failed => {
                    lines.push(line(&frame, said("failed", state.note.clone()), true))
                }
                CallStatus::Cancelled => {
                    lines.push(line(&frame, said("cancelled", state.note.clone()), true))
                }
                CallStatus::Pending | CallStatus::Running => {}
            }
        }
    }
    lines
}

/// A result's fields on one line.
fn brief(result: &Value) -> String {
    match result {
        Value::Null => String::new(),
        Value::Object(fields) => fields
            .iter()
            .map(|(name, value)| match value {
                Value::String(text) => format!("{name}={text}"),
                other => format!("{name}={other}"),
            })
            .collect::<Vec<_>>()
            .join(" "),
        other => other.to_string(),
    }
}

/// The newest call of `task`, or of instance `instance` of it.
pub fn newest<'a>(
    calls: &'a [LiveCall],
    task: &str,
    instance: Option<&str>,
) -> Option<&'a LiveCall> {
    calls
        .iter()
        .filter(|call| call.view.task == task && call.view.instance.as_deref() == instance)
        .max_by_key(|call| call.view.attempt)
}

/// Every call `run` dispatched, as the controller reports them.
pub(in crate::live) async fn progress(
    connection: &Connection,
    run: &str,
) -> Result<Vec<CallView>, String> {
    let path = crucible_contract::calls::url("", CALL_PROGRESS_PATH, run, None);
    within("the calls", connection.client.get::<Vec<CallView>>(&path)).await
}

#[derive(Deserialize)]
struct RunDetail {
    run: RunRow,
}

#[derive(Deserialize)]
struct RunRow {
    #[serde(default)]
    issue_key: Option<String>,
    #[serde(default)]
    parent: Option<Parent>,
}

/// The call that dispatched a run, as the run's detail names it.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Parent {
    pub run: String,
    pub task: String,
    #[serde(default)]
    pub instance: Option<String>,
}

impl Parent {
    /// The caller as a launch knows it, once the calling run's launch is known.
    pub fn called_by(self, launch: String) -> crux::dto::CalledBy {
        crux::dto::CalledBy {
            run: self.run,
            launch,
            task: self.task,
            instance: self.instance,
        }
    }
}

/// What called run `run`: `Some(None)` when nothing did, `None` when the run could not be read.
pub(in crate::live) async fn parent_of(
    connection: &Connection,
    run: &str,
) -> Option<Option<Parent>> {
    let detail: RunDetail = within("the run", connection.client.run(run)).await.ok()?;
    Some(detail.run.parent)
}

/// The launch key run `run` belongs to, read once and kept in `known`.
pub(in crate::live) async fn launch_of(
    connection: &Connection,
    known: &mut HashMap<String, String>,
    run: &str,
) -> Option<String> {
    if let Some(launch) = known.get(run) {
        return Some(launch.clone());
    }
    let detail: RunDetail = within("the run", connection.client.run(run)).await.ok()?;
    let launch = detail.run.issue_key.filter(|key| !key.is_empty())?;
    known.insert(run.to_string(), launch.clone());
    Some(launch)
}

/// What `calls` of run `run`, launched as `launch`, say about each target launch's caller.
pub(in crate::live) fn callers(
    calls: &[LiveCall],
    run: &str,
    launch: &str,
) -> Vec<(String, crux::dto::CalledBy)> {
    calls
        .iter()
        .filter_map(|call| {
            let target = call.launch.clone()?;
            Some((
                target,
                crux::dto::CalledBy {
                    run: run.to_string(),
                    launch: launch.to_string(),
                    task: call.view.task.clone(),
                    instance: call.view.instance.clone(),
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::live::calls::{
        Jump, LiveCall, NodeAsk, callers, frame_name, lifecycle, newest, open_label, target,
    };
    use crate::logline::Body;
    use crate::session::RunFold;
    use crucible_contract::calls::CallView;
    use crucible_contract::calls::{CallId, CallKind};
    use serde_json::json;

    fn call(
        task: &str,
        instance: Option<&str>,
        attempt: u32,
        handle: serde_json::Value,
    ) -> LiveCall {
        let view: CallView = serde_json::from_value(json!({
            "task": task,
            "instance": instance,
            "attempt": attempt,
            "alias": "triage",
            "kind": handle["kind"],
            "state": {
                "call": format!("c-{task}-{attempt}"),
                "status": "running",
                "handle": handle,
                "link": {"kind": "run", "run": "run-child"},
            },
        }))
        .unwrap();
        LiveCall { view, launch: None }
    }

    #[test]
    fn a_playbook_node_opens_its_task_in_the_target_launch_and_a_web_node_its_page() {
        let mut child = call(
            "triage",
            None,
            1,
            json!({"kind": "playbook", "run": "run-child"}),
        );
        let task_link =
            serde_json::from_value(json!({"kind": "run", "run": "run-child", "task": "scan"}))
                .unwrap();
        assert_eq!(child.jump(&task_link), None, "no launch named yet");
        assert_eq!(child.open(), None);
        child.launch = Some("playbook:triage:7".into());
        assert_eq!(
            child.jump(&task_link),
            Some(Jump::Launch {
                launch: "playbook:triage:7".into(),
                task: Some("scan".into())
            })
        );
        let elsewhere = serde_json::from_value(json!({"kind": "run", "run": "run-other"})).unwrap();
        assert_eq!(child.jump(&elsewhere), None);
        assert_eq!(
            child.open(),
            Some(Jump::Launch {
                launch: "playbook:triage:7".into(),
                task: None
            })
        );
        let web = serde_json::from_value(
            json!({"kind": "external", "url": "https://github.com/acme/app/actions/runs/1/job/2"}),
        )
        .unwrap();
        assert_eq!(
            child.jump(&web),
            Some(Jump::Web(
                "https://github.com/acme/app/actions/runs/1/job/2".into()
            ))
        );
    }

    #[test]
    fn the_newest_attempt_of_a_task_or_instance_is_the_one_drawn() {
        let gh = json!({"kind": "github_workflow", "repo": "acme/app", "run_id": 1});
        let calls = [
            call("ship", None, 1, gh.clone()),
            call("ship", None, 2, gh.clone()),
            call("each", Some("a"), 1, gh.clone()),
        ];
        assert_eq!(
            newest(&calls, "ship", None).map(|c| c.view.attempt),
            Some(2)
        );
        assert_eq!(
            newest(&calls, "each", Some("a")).map(|c| c.view.attempt),
            Some(1)
        );
        assert!(newest(&calls, "each", None).is_none());
        assert!(calls[0].target_run().is_none());
    }

    #[test]
    fn a_target_launch_learns_the_run_and_task_that_called_it() {
        let mut child = call(
            "triage",
            Some("k"),
            1,
            json!({"kind": "playbook", "run": "run-child"}),
        );
        let gh = call(
            "ship",
            None,
            1,
            json!({"kind": "github_workflow", "repo": "acme/app", "run_id": 1}),
        );
        assert!(callers(&[child.clone(), gh.clone()], "run-1", "playbook:p:1").is_empty());
        child.launch = Some("playbook:triage:7".into());
        let learned = callers(&[child, gh], "run-1", "playbook:p:1");
        assert_eq!(learned.len(), 1);
        let (target, by) = &learned[0];
        assert_eq!(target, "playbook:triage:7");
        assert_eq!(
            (
                by.run.as_str(),
                by.launch.as_str(),
                by.task.as_str(),
                by.instance.as_deref()
            ),
            ("run-1", "playbook:p:1", "triage", Some("k"))
        );
    }

    /// `ship` calls a workflow; `triage` calls a playbook once per item, and `b` has not
    /// dispatched yet.
    const CALLS: &str = concat!(
        r#"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":["#,
        r#"{"name":"ship","kind":"call","required":true,"call":{"alias":"release","target":"github_workflow"}},"#,
        r#"{"name":"triage","kind":"call","required":true,"over":"x.items","call":{"alias":"triage","target":"playbook"}},"#,
        r#"{"name":"note","kind":"command","required":true}"#,
        r#"]}"#,
        "\n",
        r#"{"v":1,"kind":"call_dispatched","task":"ship","attempt":1,"alias":"release","call":"c-1","handle":{"kind":"github_workflow","repo":"acme/app","run_id":42},"link":{"kind":"external","url":"https://github.com/acme/app/actions/runs/42"}}"#,
        "\n",
        r#"{"v":1,"kind":"call_dispatched","task":"triage","instance":"a","attempt":1,"alias":"triage","call":"c-2","handle":{"kind":"playbook","run":"run-a"}}"#,
        "\n",
    );

    fn tasks() -> Vec<crate::session::TaskView> {
        let mut fold = RunFold::default();
        for line in CALLS.lines() {
            fold.apply_line(line);
        }
        fold.tasks
    }

    fn texts(lines: &[crate::logline::LogLine]) -> Vec<(String, String, bool)> {
        lines
            .iter()
            .map(|line| match &line.body {
                Body::Notice { text, warning } => (
                    line.task.clone().unwrap_or_default(),
                    text.clone(),
                    *warning,
                ),
                other => panic!("not a notice: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn a_call_logs_its_dispatch_its_run_and_how_it_settled() {
        let tasks = tasks();
        let dispatched = texts(&lifecycle(&tasks, &[]));
        assert_eq!(
            dispatched,
            [
                (
                    "ship".into(),
                    "dispatched release \u{2192} https://github.com/acme/app/actions/runs/42"
                        .into(),
                    false
                ),
                (
                    "triage[a]".into(),
                    "dispatched triage \u{2192} run run-a".into(),
                    false
                ),
            ]
        );

        let mut ship = call(
            "ship",
            None,
            1,
            json!({"kind": "github_workflow", "repo": "acme/app", "run_id": 42}),
        );
        ship.view.state.status = crucible_contract::calls::CallStatus::Passed;
        ship.view.state.result = json!({"version": "1.2", "ok": true});
        let mut triage = call(
            "triage",
            Some("a"),
            1,
            json!({"kind": "playbook", "run": "run-a"}),
        );
        triage.view.state.status = crucible_contract::calls::CallStatus::Failed;
        triage.view.state.note = "scan failed".into();
        let lines = lifecycle(&tasks, &[ship, triage]);
        assert_eq!(
            texts(&lines)[1..],
            [
                ("ship".into(), "running".into(), false),
                (
                    "ship".into(),
                    "passed \u{b7} ok=true version=1.2".into(),
                    false
                ),
                (
                    "triage[a]".into(),
                    "dispatched triage \u{2192} run run-child".into(),
                    false
                ),
                ("triage[a]".into(), "running".into(), false),
                ("triage[a]".into(), "failed \u{b7} scan failed".into(), true),
            ]
        );
        assert!(lines.iter().all(|line| line.raw.starts_with('[')));
        assert!(
            lines[4].is_for("triage"),
            "an instance's line is its task's"
        );
    }

    #[test]
    fn a_call_task_opens_its_target_run_where_it_runs() {
        let tasks = tasks();
        assert_eq!(
            target(&tasks[0], &[]),
            Some((
                "Open on GitHub",
                Jump::Web("https://github.com/acme/app/actions/runs/42".into())
            ))
        );
        assert_eq!(
            target(&tasks[1], &[]),
            None,
            "a mapped call opens per instance"
        );
        assert_eq!(target(&tasks[2], &[]), None);
        let mut child = call(
            "triage",
            None,
            1,
            json!({"kind": "playbook", "run": "run-child"}),
        );
        child.launch = Some("playbook:triage:7".into());
        let mut unmapped = tasks[1].clone();
        unmapped.instances.clear();
        assert_eq!(
            target(&unmapped, &[child]),
            Some((
                "Open run",
                Jump::Launch {
                    launch: "playbook:triage:7".into(),
                    task: None
                }
            ))
        );
        assert_eq!(open_label(CallKind::Playbook), "Open run");
        assert_eq!(frame_name("triage", Some("a")), "triage[a]");
    }

    #[test]
    fn a_node_log_route_escapes_each_part() {
        let ask = NodeAsk {
            run: "run 1".into(),
            call: CallId("c/2".into()),
            node: "test (linux)".into(),
        };
        assert_eq!(
            ask.path("http://c/"),
            "http://c/api/runs/run%201/calls/c%2F2/nodes/test%20%28linux%29/log"
        );
    }
}
