//! The fetcher's side of calls: the calls a run dispatched, the call that dispatched a run, and
//! the logs of their targets' nodes.

use crate::live::calls::{self, LiveCall};
use crate::live::fetcher::Fetcher;
use crate::live::{LiveDetail, LivePlaybook, LiveView};
use crate::net::Connection;
use crate::session::RunFold;

/// The origin of a launch a call dispatched.
const CALLED: &str = "call";

impl Fetcher {
    /// The calls run `run` of `launch` dispatched, re-read on a poll while its plan has a call
    /// task. Each target launch learns the call that dispatched it.
    pub(in crate::live::fetcher) async fn calls(
        &mut self,
        connection: &Connection,
        launch: &LivePlaybook,
        run: Option<&str>,
        fold: &RunFold,
        poll: bool,
    ) -> Vec<LiveCall> {
        let held = self
            .detail
            .as_ref()
            .filter(|detail| detail.key == launch.key && detail.run_id.as_deref() == run)
            .map(|detail| detail.calls.clone())
            .unwrap_or_default();
        let Some(run) = run else {
            return Vec::new();
        };
        let calling = fold.tasks.iter().any(|task| task.call.is_some());
        if !poll || !calling {
            return held;
        }
        let Ok(views) = calls::progress(connection, run).await else {
            return held;
        };
        let mut read = Vec::with_capacity(views.len());
        for view in views {
            let target = match &view.state.handle {
                crucible_contract::calls::TargetHandle::Playbook { run } => {
                    calls::launch_of(connection, &mut self.run_launches, run).await
                }
                crucible_contract::calls::TargetHandle::GithubWorkflow { .. } => None,
            };
            read.push(LiveCall {
                view,
                launch: target,
            });
        }
        for (target, by) in calls::callers(&read, run, &launch.key) {
            if let LiveView::Ready { launches, .. } = &mut self.view
                && let Some(listed) = launches.iter_mut().find(|l| l.key == target)
                && listed.called_by.is_none()
            {
                listed.called_by = Some(by.clone());
            }
            self.callers.insert(target, by);
        }
        read
    }
}

impl Fetcher {
    /// The log of the node the window asked for, when it is a node of one of `detail`'s calls.
    /// A node that had ended when its log was read is not read again; a running one is re-read
    /// on each poll.
    pub(in crate::live::fetcher) async fn node_log(
        &mut self,
        connection: &Connection,
        detail: &LiveDetail,
        poll: bool,
    ) -> Option<calls::NodeLog> {
        let ask = self.node_ask.clone()?;
        if detail.run_id.as_deref() != Some(ask.run.as_str()) {
            return None;
        }
        let ended = detail
            .calls
            .iter()
            .find(|call| call.view.state.call == ask.call)
            .and_then(|call| call.node(&ask.node))
            .is_none_or(|node| node.status.ended());
        let fresh = match self.node_logs.get(&ask) {
            Some((Ok(_), true)) => false,
            Some(_) => poll,
            None => true,
        };
        if fresh {
            let text = calls::node_log(connection, &ask).await;
            self.node_logs.insert(ask.clone(), (text, ended));
        }
        let (text, _) = self.node_logs.get(&ask)?;
        Some(calls::NodeLog {
            text: text.clone(),
            ask,
        })
    }

    /// Names the call that dispatched `launch`, as its run's detail says, when neither the
    /// launch list nor a calling run on screen has. True when the list changed.
    pub(in crate::live::fetcher) async fn learn_caller(
        &mut self,
        connection: &Connection,
        launch: &LivePlaybook,
        run: Option<&str>,
    ) -> bool {
        let Some(run) = run else {
            return false;
        };
        if launch.origin != CALLED
            || launch.called_by.is_some()
            || self.callers.contains_key(&launch.key)
        {
            return false;
        }
        let parent = match self.parents.get(run) {
            Some(parent) => parent.clone(),
            None => {
                let Some(parent) = calls::parent_of(connection, run).await else {
                    return false;
                };
                self.parents.insert(run.to_string(), parent.clone());
                parent
            }
        };
        let Some(parent) = parent else {
            return false;
        };
        let Some(caller) = calls::launch_of(connection, &mut self.run_launches, &parent.run).await
        else {
            return false;
        };
        let by = parent.called_by(caller);
        self.callers.insert(launch.key.clone(), by.clone());
        if let LiveView::Ready { launches, .. } = &mut self.view
            && let Some(listed) = launches.iter_mut().find(|l| l.key == launch.key)
        {
            listed.called_by = Some(by);
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use tokio::sync::mpsc;

    use crate::live::LivePlaybook;
    use crate::live::calls::{LiveCall, NodeAsk, NodeLog};
    use crate::live::fetcher::Fetcher;
    use crate::live::shape::build_detail;
    use crate::session::RunFold;

    const LOG: &str = "/api/runs/run-parent/calls/c-1/nodes/build%20it/log";

    /// A controller holding a parent run that called `child` into a launch of its own, the
    /// child launch's list row with no caller named, and one target node's log. Every request
    /// path is counted.
    fn controller() -> (String, Arc<Mutex<HashMap<String, usize>>>) {
        let asked: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
        let counted = Arc::clone(&asked);
        let (url, _) = crate::testserver::serve(move |request| {
            let path = request.split_whitespace().nth(1).unwrap_or_default();
            *counted.lock().unwrap().entry(path.to_string()).or_default() += 1;
            match path {
                "/api/whoami" => r#"{"user":"wren","role":"user"}"#.into(),
                "/api/playbook-runs" => json!([{
                    "key": "playbook:child:1", "playbook": "child",
                    "origin": "call", "status": "done", "created_at": "2026-11-25T10:00:00Z",
                }])
                .to_string(),
                "/api/runs/run-child" => json!({"run": {
                    "issue_key": "playbook:child:1",
                    "parent": {"run": "run-parent", "task": "child", "instance": "k"},
                }})
                .to_string(),
                "/api/runs/run-parent" => {
                    json!({"run": {"issue_key": "playbook:parent:1"}}).to_string()
                }
                LOG => "##[group]Run make\nbuilt\n".into(),
                _ => "[]".into(),
            }
        });
        (url, asked)
    }

    fn fetcher(url: &str, keys: &std::path::Path) -> (Fetcher, crate::net::Net) {
        let net = crate::net::Net::uncached_for_tests();
        let (wake, _rx) = mpsc::unbounded_channel();
        let fetcher = Fetcher::new(
            net.connector(crate::testserver::target("t", url, keys)),
            wake,
        );
        (fetcher, net)
    }

    fn asked(counts: &Mutex<HashMap<String, usize>>, path: &str) -> usize {
        counts.lock().unwrap().get(path).copied().unwrap_or(0)
    }

    #[test]
    fn a_dispatched_launch_learns_its_caller_from_its_run_once() {
        let (url, counts) = controller();
        let keys = tempfile::tempdir().unwrap();
        let (mut fetcher, net) = fetcher(&url, keys.path());
        net.handle().unwrap().block_on(async {
            fetcher.refresh_list().await.unwrap();
            let child: LivePlaybook = fetcher.view.launches()[0].clone();
            assert_eq!(child.called_by, None);
            let connection = fetcher.connection().await.unwrap();

            assert!(
                fetcher
                    .learn_caller(&connection, &child, Some("run-child"))
                    .await
            );
            let by = fetcher.view.launches()[0].called_by.clone().unwrap();
            assert_eq!(
                (
                    by.run.as_str(),
                    by.launch.as_str(),
                    by.task.as_str(),
                    by.instance.as_deref()
                ),
                ("run-parent", "playbook:parent:1", "child", Some("k"))
            );

            fetcher.refresh_list().await.unwrap();
            let relisted = fetcher.view.launches()[0].clone();
            assert_eq!(
                relisted.called_by,
                Some(by),
                "the list keeps what was learned"
            );
            assert!(
                !fetcher
                    .learn_caller(&connection, &child, Some("run-child"))
                    .await
            );
            assert_eq!(asked(&counts, "/api/runs/run-child"), 1);
            assert_eq!(asked(&counts, "/api/runs/run-parent"), 1);

            let launched = LivePlaybook {
                origin: "draft".into(),
                key: "playbook:other:1".into(),
                ..child
            };
            assert!(
                !fetcher
                    .learn_caller(&connection, &launched, Some("run-other"))
                    .await
            );
            assert_eq!(
                asked(&counts, "/api/runs/run-other"),
                0,
                "only a called launch asks"
            );
        });
    }

    fn calling(node: &str) -> Vec<LiveCall> {
        let view = serde_json::from_value(json!({
            "task": "ci",
            "attempt": 1,
            "alias": "ci",
            "kind": "github_workflow",
            "state": {
                "call": "c-1",
                "status": "running",
                "handle": {"kind": "github_workflow", "repo": "acme/app", "run_id": 7},
                "progress": {"nodes": [{"id": "build it", "label": "build", "status": node}]},
            },
        }))
        .unwrap();
        vec![LiveCall { view, launch: None }]
    }

    #[test]
    fn a_node_log_is_read_once_asked_again_while_it_runs_and_kept_once_it_ended() {
        let (url, counts) = controller();
        let keys = tempfile::tempdir().unwrap();
        let (mut fetcher, net) = fetcher(&url, keys.path());
        net.handle().unwrap().block_on(async {
            let connection = fetcher.connection().await.unwrap();
            let launch = LivePlaybook::default();
            let mut detail = build_detail(&launch, RunFold::default(), Vec::new());
            detail.run_id = Some("run-parent".into());
            detail.calls = calling("running");
            assert_eq!(fetcher.node_log(&connection, &detail, true).await, None);

            let ask = NodeAsk {
                run: "run-parent".into(),
                call: crucible_contract::calls::CallId("c-1".into()),
                node: "build it".into(),
            };
            fetcher.node_ask = Some(ask.clone());
            let read = fetcher.node_log(&connection, &detail, false).await;
            assert_eq!(
                read,
                Some(NodeLog {
                    ask: ask.clone(),
                    text: Ok("##[group]Run make\nbuilt\n".into()),
                })
            );
            assert_eq!(asked(&counts, LOG), 1);
            fetcher.node_log(&connection, &detail, false).await;
            assert_eq!(asked(&counts, LOG), 1, "a stream batch reads no log");
            fetcher.node_log(&connection, &detail, true).await;
            assert_eq!(
                asked(&counts, LOG),
                2,
                "a poll reads a running node's log again"
            );

            detail.calls = calling("pass");
            fetcher.node_log(&connection, &detail, true).await;
            assert_eq!(asked(&counts, LOG), 3, "the read that saw it end");
            fetcher.node_log(&connection, &detail, true).await;
            assert_eq!(asked(&counts, LOG), 3, "an ended node's log is kept");

            detail.run_id = Some("run-other".into());
            assert_eq!(fetcher.node_log(&connection, &detail, true).await, None);
        });
    }
}
