//! The calls a run dispatched, read from the controller's call progress, and the launches their
//! playbook target runs belong to.

use std::collections::HashMap;

use crucible_contract::calls::{CALL_PROGRESS_PATH, CallView, NodeLink, TargetHandle};
use serde::Deserialize;

use crate::net::{Connection, within};

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
    use crate::live::calls::{Jump, LiveCall, callers, newest};
    use crucible_contract::calls::CallView;
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
}
