//! A call task in the Graph pane: its target's graph drawn inside the call's frame, from the
//! target run's progress once dispatched and from the pack's lock before.
//!
//! ```text
//!   subgraph g3 [" "]
//!     t3[["↗ ship · release · github"]]     the call task
//!     t3c0["build"]                          target node 0
//!     t3c1[["≡ test ×4 (3 ok)"]]             a folded group: its members' first index
//!     t3cmore["+12 more"]
//!     t3 ~~~ t3c0                            places each root under the call
//!     t3c0 --> t3c1
//!   end
//! ```

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crucible_contract::calls::{CallsLock, LOCK_FILE, NodeStatus, ProgressGraph};
use crucible_contract::session::TaskStatus;

use crate::live::calls::{LiveCall, newest};
use crate::pack::PackFiles;
use crate::session::call::badge;
use crate::session::{TaskState, TaskView};

/// A call frame with at most this many nodes opens unfolded.
pub(crate) const HANDFUL: usize = 6;

/// The most target nodes one frame draws.
const MOST: usize = 40;

/// What the graph knows of every call's target.
pub(crate) struct Targets<'a> {
    calls: &'a [LiveCall],
    lock: Option<CallsLock>,
}

impl<'a> Targets<'a> {
    /// `pack`'s lock is the graph each call draws before it dispatches.
    pub(crate) fn new(calls: &'a [LiveCall], pack: Option<&PackFiles>) -> Self {
        let lock = pack
            .and_then(|pack| pack.file(LOCK_FILE))
            .and_then(|file| file.text.as_deref())
            .and_then(|text| CallsLock::parse(text).ok());
        Targets { calls, lock }
    }

    /// The target run of unmapped call `task`, once dispatched.
    pub(crate) fn call(&self, task: &TaskView) -> Option<&'a LiveCall> {
        task.call.as_ref()?;
        newest(self.calls, &task.name, None)
    }

    /// What `task`'s frame draws: its target run's progress, else the lock's graph, pending.
    pub(crate) fn graph(&self, task: &TaskView) -> Option<Cow<'_, ProgressGraph>> {
        let call = task.call.as_ref()?;
        if let Some(live) = self.call(task) {
            return Some(Cow::Borrowed(&live.view.state.progress));
        }
        let locked = self.lock.as_ref()?.calls.get(&call.alias)?;
        Some(Cow::Owned(locked.interface.graph.pending()))
    }
}

/// `↗ ship · release · github`: the task, its alias when that differs, and the target kind.
pub(crate) fn label(task: &TaskView) -> Option<String> {
    let call = task.call.as_ref()?;
    let mut parts = vec![task.name.clone()];
    if call.alias.as_str() != task.name {
        parts.push(call.alias.to_string());
    }
    parts.push(badge(call.kind).to_string());
    Some(parts.join(" \u{b7} "))
}

/// The groups of `graph` with more than one node, by name, and their members' indexes.
fn groups(graph: &ProgressGraph) -> HashMap<&str, Vec<usize>> {
    let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
    for (n, node) in graph.nodes.iter().enumerate() {
        if let Some(group) = node.group.as_deref() {
            groups.entry(group).or_default().push(n);
        }
    }
    groups.retain(|_, members| members.len() > 1);
    groups
}

/// `task/group`, the key a group's fold is kept under.
pub(crate) fn group_key(task: &str, group: &str) -> String {
    format!("{task}/{group}")
}

/// For each node, the node drawn for it: itself, or its folded group's first member.
fn drawn_as(task: &str, graph: &ProgressGraph, open: &HashSet<String>) -> Vec<usize> {
    let groups = groups(graph);
    graph
        .nodes
        .iter()
        .enumerate()
        .map(|(n, node)| {
            node.group
                .as_deref()
                .filter(|group| !open.contains(&group_key(task, group)))
                .and_then(|group| groups.get(group))
                .and_then(|members| members.first().copied())
                .unwrap_or(n)
        })
        .collect()
}

/// Whether `task`'s frame draws its target's nodes: small graphs unless folded by hand, large
/// ones only once unfolded by hand.
pub(crate) fn unfolded(task: &TaskView, graph: &ProgressGraph, open: &HashSet<String>) -> bool {
    let shown: HashSet<usize> = drawn_as(&task.name, graph, open).into_iter().collect();
    (shown.len() <= HANDFUL) != open.contains(&task.name)
}

/// What a frame's target node `n` is when pressed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Pressed {
    /// A folded group: show its members.
    Group(String),
    /// A group shown whole or a plain node: open its link.
    Node(usize),
}

/// What pressing node `n` of `task`'s target graph does.
pub(crate) fn pressed(
    task: &str,
    graph: &ProgressGraph,
    open: &HashSet<String>,
    n: usize,
) -> Option<Pressed> {
    let node = graph.nodes.get(n)?;
    let folded = node
        .group
        .as_deref()
        .filter(|group| groups(graph).contains_key(group))
        .filter(|group| !open.contains(&group_key(task, group)));
    Some(match folded {
        Some(group) => Pressed::Group(group_key(task, group)),
        None => Pressed::Node(n),
    })
}

/// `t3c5` is node 5 of task 3's target graph.
pub(crate) fn node_id(id: &str) -> Option<(usize, usize)> {
    let (task, node) = id.strip_prefix('t')?.split_once('c')?;
    Some((task.parse().ok()?, node.parse().ok()?))
}

/// Where a whole group stands: running while any member runs or some are left, failing when one
/// failed, else what all of them settled as.
fn group_status(members: &[NodeStatus]) -> NodeStatus {
    let settled: Vec<TaskStatus> = members
        .iter()
        .filter_map(|status| match status {
            NodeStatus::Settled(status) => Some(*status),
            _ => None,
        })
        .collect();
    if members.contains(&NodeStatus::Running) {
        return NodeStatus::Running;
    }
    if settled.is_empty() {
        return NodeStatus::Pending;
    }
    if settled.len() < members.len() {
        return NodeStatus::Running;
    }
    settled
        .iter()
        .find(|status| {
            !status.passed() && !matches!(status, TaskStatus::Skipped | TaskStatus::NotTaken)
        })
        .or_else(|| settled.iter().find(|status| status.passed()))
        .or_else(|| settled.first())
        .copied()
        .map_or(NodeStatus::Pending, NodeStatus::Settled)
}

/// The lines a frame draws inside `subgraph g<i>` after the call's own node.
#[derive(Default)]
pub(crate) struct Frame {
    pub(crate) body: String,
    /// Each drawn node's id and status.
    pub(crate) states: Vec<(String, NodeStatus)>,
    /// Nodes and edges, against the diagram's item limit.
    pub(crate) items: usize,
    /// The edge lines in `body`, which `linkStyle` counts.
    pub(crate) links: usize,
}

/// Text safe inside a quoted node label.
fn clean(text: &str) -> String {
    text.replace(['"', '|'], "'")
}

/// Task `i`'s target graph as the lines of its frame.
pub(crate) fn frame(i: usize, task: &str, graph: &ProgressGraph, open: &HashSet<String>) -> Frame {
    let drawn = drawn_as(task, graph, open);
    let mut body = String::new();
    let mut states = Vec::new();
    let mut shown: Vec<usize> = Vec::new();
    for (n, rep) in drawn.iter().enumerate() {
        if *rep != n {
            continue;
        }
        if shown.len() == MOST {
            break;
        }
        shown.push(n);
    }
    let kept: HashSet<usize> = shown.iter().copied().collect();
    for &n in &shown {
        let Some(node) = graph.nodes.get(n) else {
            continue;
        };
        let members: Vec<NodeStatus> = drawn
            .iter()
            .enumerate()
            .filter(|(_, rep)| **rep == n)
            .filter_map(|(m, _)| graph.nodes.get(m).map(|node| node.status))
            .collect();
        let folded = members.len() > 1;
        let id = format!("t{i}c{n}");
        if folded {
            let group = node.group.as_deref().unwrap_or(node.label.as_str());
            let ok = members
                .iter()
                .filter(|s| matches!(s, NodeStatus::Settled(status) if status.passed()))
                .count();
            body.push_str(&format!(
                "    {id}[[\"\u{2261} {} \u{d7}{} ({ok} ok)\"]]\n",
                clean(group),
                members.len()
            ));
            states.push((id, group_status(&members)));
        } else {
            body.push_str(&format!("    {id}[\"{}\"]\n", clean(&node.label)));
            states.push((id, node.status));
        }
    }
    let index: HashMap<&str, usize> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(n, node)| (node.id.as_str(), n))
        .collect();
    let mut edges: Vec<(usize, usize)> = Vec::new();
    for edge in &graph.edges {
        let (Some(&from), Some(&to)) = (index.get(edge.from.as_str()), index.get(edge.to.as_str()))
        else {
            continue;
        };
        let (Some(&from), Some(&to)) = (drawn.get(from), drawn.get(to)) else {
            continue;
        };
        if from != to && kept.contains(&from) && kept.contains(&to) && !edges.contains(&(from, to))
        {
            edges.push((from, to));
        }
    }
    let fed: HashSet<usize> = edges.iter().map(|(_, to)| *to).collect();
    let mut nodes = shown.len();
    let mut links = 0;
    for &n in shown.iter().filter(|n| !fed.contains(n)) {
        body.push_str(&format!("    t{i} ~~~ t{i}c{n}\n"));
        links += 1;
    }
    for (from, to) in &edges {
        body.push_str(&format!("    t{i}c{from} --> t{i}c{to}\n"));
        links += 1;
    }
    let more = (drawn.len() - drawn.iter().filter(|rep| kept.contains(rep)).count())
        + graph.omitted as usize;
    if more > 0 {
        body.push_str(&format!(
            "    t{i}cmore[\"+{more} more\"]\n    t{i} ~~~ t{i}cmore\n"
        ));
        nodes += 1;
        links += 1;
    }
    Frame {
        body,
        states,
        items: nodes + links,
        links,
    }
}

/// A target node's status as the task state the graph colours.
pub(crate) fn state(status: NodeStatus) -> TaskState {
    match status {
        NodeStatus::Pending => TaskState::Pending,
        NodeStatus::Running => TaskState::Running(1),
        NodeStatus::Settled(status) => TaskState::Settled(status),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crucible_contract::calls::{NodeStatus, ProgressGraph};
    use crucible_contract::session::TaskStatus;
    use serde_json::json;

    use crate::ui::graph_call::{
        HANDFUL, Pressed, frame, group_key, group_status, node_id, pressed, unfolded,
    };

    fn graph(value: serde_json::Value) -> ProgressGraph {
        serde_json::from_value(value).unwrap()
    }

    fn matrix() -> ProgressGraph {
        graph(json!({
            "nodes": [
                {"id": "build", "label": "build", "status": "pass"},
                {"id": "t1", "label": "test (linux)", "status": "pass", "group": "test"},
                {"id": "t2", "label": "test (mac)", "status": "running", "group": "test"},
                {"id": "t3", "label": "test (win)", "status": "pending", "group": "test"},
                {"id": "ship", "label": "ship \"it\"", "status": "pending"}
            ],
            "edges": [
                {"from": "build", "to": "t1"}, {"from": "build", "to": "t2"},
                {"from": "build", "to": "t3"}, {"from": "t1", "to": "ship"},
                {"from": "t2", "to": "ship"}, {"from": "t3", "to": "ship"},
                {"from": "build", "to": "gone"}
            ]
        }))
    }

    #[test]
    fn a_matrix_group_folds_to_one_node_until_it_is_opened() {
        let none = HashSet::new();
        let folded = frame(2, "ship", &matrix(), &none);
        assert_eq!(
            folded.body,
            concat!(
                "    t2c0[\"build\"]\n",
                "    t2c1[[\"\u{2261} test \u{d7}3 (1 ok)\"]]\n",
                "    t2c4[\"ship 'it'\"]\n",
                "    t2 ~~~ t2c0\n",
                "    t2c0 --> t2c1\n",
                "    t2c1 --> t2c4\n",
            )
        );
        assert_eq!(folded.states[1], ("t2c1".to_string(), NodeStatus::Running));
        assert_eq!(folded.items, 6);
        let open = HashSet::from([group_key("ship", "test")]);
        let shown = frame(2, "ship", &matrix(), &open);
        for part in ["t2c2[\"test (mac)\"]", "t2c0 --> t2c3", "t2c3 --> t2c4"] {
            assert!(shown.body.contains(part), "{part} in\n{}", shown.body);
        }
        assert_eq!(
            pressed("ship", &matrix(), &none, 2),
            Some(Pressed::Group("ship/test".into()))
        );
        assert_eq!(pressed("ship", &matrix(), &open, 2), Some(Pressed::Node(2)));
        assert_eq!(pressed("ship", &matrix(), &none, 0), Some(Pressed::Node(0)));
        assert_eq!(pressed("ship", &matrix(), &none, 9), None);
    }

    #[test]
    fn a_large_graph_opens_folded_and_a_small_one_unfolded_until_toggled() {
        let task = {
            let mut fold = crate::session::RunFold::default();
            fold.apply_line(r#"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":[{"name":"ship","kind":"call","required":true,"call":{"alias":"release","target":"playbook"}}]}"#);
            fold.tasks.remove(0)
        };
        let small = matrix();
        let nodes: Vec<serde_json::Value> = (0..=HANDFUL)
            .map(|n| json!({"id": format!("n{n}"), "label": format!("n{n}"), "status": "pending"}))
            .collect();
        let large = graph(json!({"nodes": nodes}));
        let none = HashSet::new();
        let toggled = HashSet::from(["ship".to_string()]);
        assert!(unfolded(&task, &small, &none));
        assert!(!unfolded(&task, &small, &toggled));
        assert!(!unfolded(&task, &large, &none));
        assert!(unfolded(&task, &large, &toggled));
    }

    #[test]
    fn a_frame_counts_what_it_leaves_out() {
        let nodes: Vec<serde_json::Value> = (0..45)
            .map(|n| json!({"id": format!("n{n}"), "label": format!("n{n}"), "status": "pass"}))
            .collect();
        let big = graph(json!({"nodes": nodes, "omitted": 3}));
        let drawn = frame(0, "t", &big, &HashSet::new());
        assert!(drawn.body.contains("t0c39[\"n39\"]"), "{}", drawn.body);
        assert!(!drawn.body.contains("t0c40["), "{}", drawn.body);
        assert!(
            drawn.body.contains("t0cmore[\"+8 more\"]"),
            "{}",
            drawn.body
        );
    }

    #[test]
    fn a_group_runs_until_every_member_settles_and_fails_when_one_did() {
        let pass = NodeStatus::Settled(TaskStatus::Pass);
        let fail = NodeStatus::Settled(TaskStatus::Fail);
        assert_eq!(
            group_status(&[NodeStatus::Pending, NodeStatus::Pending]),
            NodeStatus::Pending
        );
        assert_eq!(
            group_status(&[pass, NodeStatus::Pending]),
            NodeStatus::Running
        );
        assert_eq!(group_status(&[pass, fail]), fail);
        assert_eq!(group_status(&[pass, pass]), pass);
        assert_eq!(
            group_status(&[pass, NodeStatus::Settled(TaskStatus::Skipped)]),
            pass
        );
    }

    #[test]
    fn target_node_ids_name_their_task_and_node() {
        assert_eq!(node_id("t3c5"), Some((3, 5)));
        for nothing in ["t3", "t3cmore", "g3", "t3i0", "tc1", "t3c"] {
            assert_eq!(node_id(nothing), None, "{nothing}");
        }
    }
}
