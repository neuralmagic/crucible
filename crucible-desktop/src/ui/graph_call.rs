//! A call task in the Graph pane: its target's graph drawn inside the call's frame, from the
//! target run's progress once dispatched and from the pack's lock before. A mapped call draws a
//! frame per instance, `g<i>k<k>` around instance `t<i>i<k>`, inside the fan-out's frame.
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

use crucible_contract::calls::{
    BackendIcon, CallKind, CallsLock, LOCK_FILE, NodeDetail, NodeLink, NodeLook, NodeStatus,
    ProgressGraph, TargetHandle,
};
use crucible_contract::session::TaskStatus;
use rune::{El, Tx, el, icon, row, text};
use rune_theme_opencode::icons as oc;

use crate::App;
use crate::live::calls::{CallNode, Jump, LiveCall, NodeAsk, frame_name, newest, open_label};
use crate::pack::PackFiles;
use crate::session::call::badge;
use crate::session::{TaskState, TaskView};
use crate::ui::graph::HANDLE;
use crate::ui::{Paint, Pane, graph_fx};

pub(crate) const HANDFUL: usize = 6;

const MOST: usize = 40;

/// What the graph knows of every call's target.
pub(crate) struct Targets<'a> {
    calls: &'a [LiveCall],
    lock: Option<CallsLock>,
}

impl<'a> Targets<'a> {
    pub(crate) fn new(calls: &'a [LiveCall], pack: Option<&PackFiles>) -> Self {
        let lock = pack
            .and_then(|pack| pack.file(LOCK_FILE))
            .and_then(|file| file.text.as_deref())
            .and_then(|text| CallsLock::parse(text).ok());
        Targets { calls, lock }
    }

    pub(crate) fn call(&self, task: &TaskView, instance: Option<&str>) -> Option<&'a LiveCall> {
        task.call.as_ref()?;
        newest(self.calls, &task.name, instance)
    }

    pub(crate) fn graph(
        &self,
        task: &TaskView,
        instance: Option<&str>,
    ) -> Option<Cow<'_, ProgressGraph>> {
        let call = task.call.as_ref()?;
        if let Some(live) = self.call(task, instance) {
            return Some(Cow::Borrowed(&live.view.state.progress));
        }
        let locked = self.lock.as_ref()?.calls.get(&call.alias)?;
        Some(Cow::Owned(locked.interface.graph.pending()))
    }
}

pub(crate) fn label(task: &TaskView, live: Option<&LiveCall>) -> Option<String> {
    let call = task.call.as_ref()?;
    let mut parts = vec![task.name.clone()];
    if call.alias.as_str() != task.name {
        parts.push(call.alias.to_string());
    }
    let repo = call.dispatched.iter().rev().find_map(|d| match &d.handle {
        TargetHandle::GithubWorkflow { repo, .. } => Some(
            repo.as_str()
                .rsplit('/')
                .next()
                .unwrap_or(repo.as_str())
                .to_string(),
        ),
        TargetHandle::TektonWatch { commit, .. } => {
            Some(commit.get(..12).unwrap_or(commit).to_string())
        }
        TargetHandle::Playbook { .. } => None,
    });
    match (repo, call.kind) {
        (Some(repo), _) => parts.push(repo),
        (None, CallKind::GithubWorkflow | CallKind::Tekton) => {
            parts.push(badge(call.kind).to_string())
        }
        (None, CallKind::Playbook) => {}
    }
    let mut label = parts.join(" \u{b7} ");
    if let Some(number) = live.and_then(|live| live.view.state.run_number) {
        label.push_str(&format!(" #{number}"));
    }
    Some(label)
}

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

/// One call frame: task `i`, or instance `k` of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct At<'a> {
    pub(crate) i: usize,
    pub(crate) task: &'a str,
    pub(crate) instance: Option<(usize, &'a str)>,
}

impl At<'_> {
    pub(crate) fn anchor(&self) -> String {
        match self.instance {
            Some((k, _)) => format!("t{}i{k}", self.i),
            None => format!("t{}", self.i),
        }
    }

    pub(crate) fn subgraph(&self) -> String {
        match self.instance {
            Some((k, _)) => format!("g{}k{k}", self.i),
            None => format!("g{}", self.i),
        }
    }

    pub(crate) fn key(&self) -> String {
        frame_name(self.task, self.instance.map(|(_, key)| key))
    }
}

pub(crate) fn group_key(frame: &str, group: &str) -> String {
    format!("{frame}/{group}")
}

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

pub(crate) fn unfolded(frame: &str, graph: &ProgressGraph, open: &HashSet<String>) -> bool {
    let shown: HashSet<usize> = drawn_as(frame, graph, open).into_iter().collect();
    (shown.len() <= HANDFUL) != open.contains(frame)
}

/// What a frame's target node `n` is when pressed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Pressed {
    Group(String),
    Node(usize),
}

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

/// A target node's place: its task, the instance whose frame holds it, and its index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NodeId {
    pub(crate) task: usize,
    pub(crate) instance: Option<usize>,
    pub(crate) node: usize,
}

pub(crate) fn node_id(id: &str) -> Option<NodeId> {
    let (frame, node) = id.strip_prefix('t')?.split_once('c')?;
    let (task, instance) = match frame.split_once('i') {
        Some((task, k)) => (task, Some(k.parse().ok()?)),
        None => (frame, None),
    };
    Some(NodeId {
        task: task.parse().ok()?,
        instance,
        node: node.parse().ok()?,
    })
}

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

/// The lines a frame draws inside `subgraph g<i>`.
pub(crate) struct Frame {
    pub(crate) body: String,
    pub(crate) states: Vec<(String, NodeStatus)>,
    pub(crate) items: usize,
    pub(crate) links: usize,
    pub(crate) picked: Option<String>,
    pub(crate) kind: CallKind,
}

impl Default for Frame {
    fn default() -> Self {
        Frame {
            body: String::new(),
            states: Vec::new(),
            items: 0,
            links: 0,
            picked: None,
            kind: CallKind::Playbook,
        }
    }
}

fn mark(status: NodeStatus) -> &'static str {
    match status {
        NodeStatus::Pending => "\u{25cb}",
        NodeStatus::Running => "\u{25cf}",
        NodeStatus::Settled(status) if status.passed() => "\u{2713}",
        NodeStatus::Settled(TaskStatus::Skipped | TaskStatus::NotTaken) => "\u{2013}",
        NodeStatus::Settled(_) => "\u{2717}",
    }
}

fn took(started: Option<&str>, ended: Option<&str>) -> Option<String> {
    let started: jiff::Timestamp = started?.parse().ok()?;
    let ended: jiff::Timestamp = ended?.parse().ok()?;
    let secs = ended.duration_since(started).as_secs().max(0);
    Some(match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, secs % 3600 / 60),
    })
}

fn clean(text: &str) -> String {
    text.replace(['"', '|'], "'")
}

pub(crate) fn frame(
    at: At,
    graph: &ProgressGraph,
    open: &HashSet<String>,
    picked: Option<&str>,
    kind: CallKind,
) -> Frame {
    let jobs = kind.presentation().nodes == NodeLook::Jobs;
    let (anchor, key) = (at.anchor(), at.key());
    let drawn = drawn_as(&key, graph, open);
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
        let id = format!("{anchor}c{n}");
        if folded {
            let group = node.group.as_deref().unwrap_or(node.label.as_str());
            let ok = members
                .iter()
                .filter(|s| matches!(s, NodeStatus::Settled(status) if status.passed()))
                .count();
            let status = group_status(&members);
            let label = format!("{} \u{d7}{} ({ok} ok)", clean(group), members.len());
            body.push_str(&if jobs {
                format!("    {id}([\"{} {label}\"])\n", mark(status))
            } else {
                format!("    {id}[[\"\u{2261} {label}\"]]\n")
            });
            states.push((id, status));
        } else {
            let label = clean(&node.label);
            body.push_str(&if jobs {
                let took = took(node.started.as_deref(), node.ended.as_deref())
                    .map(|t| format!(" \u{b7} {t}"))
                    .unwrap_or_default();
                let step = step_note(node.detail.as_ref(), node.status)
                    .map(|s| format!(" \u{b7} {}", clean(&s)))
                    .unwrap_or_default();
                format!("    {id}(\"{} {label}{took}{step}\")\n", mark(node.status))
            } else {
                format!("    {id}[\"{label}\"]\n")
            });
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
    let anchored = at.instance.is_some();
    for &n in shown.iter().filter(|n| !fed.contains(n) && anchored) {
        body.push_str(&format!("    {anchor} ~~~ {anchor}c{n}\n"));
        links += 1;
    }
    for (from, to) in &edges {
        body.push_str(&format!("    {anchor}c{from} --> {anchor}c{to}\n"));
        links += 1;
    }
    let more = (drawn.len() - drawn.iter().filter(|rep| kept.contains(rep)).count())
        + graph.omitted as usize;
    if more > 0 {
        body.push_str(&format!("    {anchor}cmore[\"+{more} more\"]\n"));
        nodes += 1;
        if anchored {
            body.push_str(&format!("    {anchor} ~~~ {anchor}cmore\n"));
            links += 1;
        }
    }
    let picked = picked
        .and_then(|id| graph.nodes.iter().position(|node| node.id == id))
        .and_then(|n| drawn.get(n))
        .filter(|rep| kept.contains(rep))
        .map(|rep| format!("{anchor}c{rep}"));
    Frame {
        body,
        states,
        items: nodes + links,
        links,
        picked,
        kind,
    }
}

fn info_button(key: &str, p: &Paint) -> El {
    let key = key.to_string();
    el().key(format!("graph-call-info-{key}"))
        .role("button")
        .label("Call info")
        .size(HANDLE, HANDLE)
        .center()
        .cursor(rune::Cursor::Pointer)
        .help("Info")
        .on_press(move |app: &mut App| app.toggle_frame_info(&key))
        .child(icon(&oc::INFO, 12.0).color(p.c.icon_faint))
}

fn info_rows(call: &LiveCall) -> Vec<(&'static str, String)> {
    let state = &call.view.state;
    let mut rows = vec![
        ("Call", call.view.alias.as_str().to_string()),
        ("Backend", call.view.kind.presentation().name.to_string()),
    ];
    match &state.handle {
        TargetHandle::Playbook { run } => rows.push(("Run", run.clone())),
        TargetHandle::GithubWorkflow { repo, run_id } => {
            rows.push(("Repo", repo.as_str().to_string()));
            rows.push((
                "Run",
                match state.run_number {
                    Some(number) => format!("#{number} ({run_id})"),
                    None => run_id.to_string(),
                },
            ));
        }
        TargetHandle::TektonWatch {
            cluster,
            namespace,
            commit,
        } => {
            rows.push(("Cluster", cluster.clone()));
            rows.push(("Namespace", namespace.clone()));
            rows.push(("Commit", commit.clone()));
        }
    }
    rows.push(("Status", format!("{:?}", state.status).to_lowercase()));
    if call.view.attempt > 1 {
        rows.push(("Attempt", call.view.attempt.to_string()));
    }
    if !state.note.is_empty() {
        rows.push(("Note", state.note.clone()));
    }
    rows
}

fn info_card(call: &LiveCall, key: &str, p: &Paint) -> El {
    let rows = info_rows(call).into_iter().map(|(name, value)| {
        row()
            .gap(8.0)
            .child(text(name).font(p.k.ty.ui).color(p.c.text_muted).w(64.0))
            .child(
                text(value)
                    .font(p.k.ty.ui)
                    .color(p.c.text_base)
                    .wrap()
                    .grow(1.0),
            )
    });
    rune::col()
        .key(format!("graph-call-info-card-{key}"))
        .pin(None, Some(4.0), Some(4.0), None)
        .w(260.0)
        .gap(4.0)
        .padding(10.0)
        .radius(8.0)
        .fill(p.c.layer_02)
        .border(0.5, p.c.border_base)
        .children(rows)
}

fn external_run(call: &LiveCall, key: &str, p: &Paint) -> Option<El> {
    let presentation = call.view.kind.presentation();
    let mark = mark_icon(presentation.icon?);
    let Some(NodeLink::External { url }) = &call.view.state.link else {
        return None;
    };
    let jump = Jump::Web(url.as_str().to_string());
    Some(
        row()
            .key(format!("graph-call-external-{key}"))
            .role("link")
            .label(presentation.open)
            .gap(3.0)
            .px(5.0)
            .py(2.0)
            .center_y()
            .radius(6.0)
            .fill(p.c.layer_02)
            .border(0.5, p.c.border_base)
            .cursor(rune::Cursor::Pointer)
            .help(url.as_str().to_string())
            .on_press_tx(move |app: &mut App, tx: &mut Tx| app.jump(&jump, tx))
            .child(icon(mark, 12.0).color(p.c.icon_base))
            .child(icon(&oc::ARROW_UP_RIGHT, 10.0).color(p.c.icon_faint)),
    )
}

fn step_note(detail: Option<&NodeDetail>, status: NodeStatus) -> Option<String> {
    let runs: Vec<Vec<(&str, NodeStatus)>> = match detail? {
        NodeDetail::GithubJob { steps } => {
            vec![steps.iter().map(|s| (s.name.as_str(), s.status)).collect()]
        }
        NodeDetail::TektonTask { task_runs } => task_runs
            .iter()
            .map(|run| {
                run.steps
                    .iter()
                    .map(|s| (s.name.as_str(), s.status))
                    .collect()
            })
            .collect(),
        NodeDetail::PlaybookTask { .. } => return None,
    };
    let running = runs.iter().find_map(|steps| {
        steps
            .iter()
            .position(|(_, s)| *s == NodeStatus::Running)
            .and_then(|k| {
                steps
                    .get(k)
                    .map(|(name, _)| format!("{name} {}/{}", k + 1, steps.len()))
            })
    });
    if running.is_some() {
        return running;
    }
    let failed = matches!(status, NodeStatus::Settled(s) if !s.passed());
    runs.iter()
        .flatten()
        .find(|(_, s)| failed && matches!(s, NodeStatus::Settled(s) if !s.passed()))
        .map(|(name, _)| format!("failed at {name}"))
}

fn mark_icon(mark: BackendIcon) -> &'static rune::Icon {
    match mark {
        BackendIcon::Github => &oc::GITHUB,
        BackendIcon::Tekton => &oc::BRANCH_OUT,
    }
}

pub(crate) fn frame_spot(
    spot: El,
    at: At,
    task: &TaskView,
    targets: &Targets,
    app: &App,
    p: &Paint,
) -> El {
    let instance = at.instance.map(|(_, key)| key);
    let call = targets.call(task, instance);
    let id = at.subgraph();
    let hovered = app.run.frame_hover.as_deref() == Some(id.as_str());
    let look = graph_fx::look(call, &app.afterglows);
    let key = at.key();
    let key_for_info = key.clone();
    let unfolded = targets
        .graph(task, instance)
        .is_some_and(|graph| unfolded(&key, &graph, &app.run.call_folds));
    let (glyph, help) = if unfolded {
        (&oc::CHEVRON_DOWN, format!("Fold {key}"))
    } else {
        (&oc::CHEVRON_RIGHT, format!("Show {key}"))
    };
    let fold = el()
        .key(format!("graph-call-fold-{key}"))
        .size(HANDLE, HANDLE)
        .center()
        .cursor(rune::Cursor::Pointer)
        .help(help)
        .on_press(move |app: &mut App| app.toggle_call(&key))
        .child(icon(glyph, 10.0).color(p.c.icon_faint));
    let open = call.and_then(|call| {
        let says = open_label(call.view.kind);
        call.open().map(|jump| {
            el().key(format!("graph-call-open-{}", at.key()))
                .role("button")
                .label(says)
                .size(HANDLE, HANDLE)
                .center()
                .cursor(rune::Cursor::Pointer)
                .help(says)
                .on_press_tx(move |app: &mut App, tx: &mut Tx| app.jump(&jump, tx))
                .child(icon(&oc::ARROW_UP_RIGHT, 12.0).color(p.c.icon_faint))
        })
    });
    let open = call
        .and_then(|call| external_run(call, &at.key(), p))
        .or(open);
    let spot = match call.and_then(|call| graph_fx::accent(call.view.kind)) {
        Some(accent) => spot
            .fill(accent.alpha(0.07))
            .border(1.0, accent.alpha(0.5))
            .radius(8.0),
        None => spot,
    };
    spot.children(graph_fx::effect(look, hovered, p))
        .child(
            row()
                .pin(None, Some(-22.0), Some(0.0), None)
                .gap(2.0)
                .center_y()
                .children(open)
                .children(call.map(|_| info_button(&key_for_info, p)))
                .child(fold),
        )
        .children(
            call.filter(|_| app.run.frame_info.as_deref() == Some(key_for_info.as_str()))
                .map(|call| info_card(call, &key_for_info, p)),
        )
        .on_hover_change(move |app: &mut App, inside: bool, _: &mut Tx| {
            if inside {
                app.run.frame_hover = Some(id.clone());
            } else if app.run.frame_hover.as_deref() == Some(id.as_str()) {
                app.run.frame_hover = None;
            }
        })
}

pub(crate) fn target_spot(spot: El, pane: &Pane, targets: &Targets, app: &App, id: &str) -> El {
    let Some(NodeId {
        task: i,
        instance: k,
        node: n,
    }) = node_id(id)
    else {
        return spot;
    };
    let Some(task) = pane.tasks.get(i) else {
        return spot;
    };
    let instance = match k {
        Some(k) => match task.instances.get(k) {
            Some(instance) => Some((k, instance.key.as_str())),
            None => return spot,
        },
        None => None,
    };
    let at = At {
        i,
        task: &task.name,
        instance,
    };
    let key = instance.map(|(_, key)| key);
    let Some(graph) = targets.graph(task, key) else {
        return spot;
    };
    let pointer = |spot: El, help: String| spot.cursor(rune::Cursor::Pointer).help(help);
    let node = match pressed(&at.key(), &graph, &app.run.call_folds, n) {
        Some(Pressed::Group(fold)) => {
            return pointer(spot, "Show the group".to_string())
                .on_press(move |app: &mut App| app.toggle_call(&fold));
        }
        Some(Pressed::Node(n)) => graph.nodes.get(n),
        None => None,
    };
    let Some(node) = node else {
        return spot;
    };
    let live = targets.call(task, key);
    let jump = node
        .link
        .as_ref()
        .zip(live)
        .and_then(|(link, call)| call.jump(link));
    match (live, pane.run_id.clone()) {
        (Some(live), Some(run)) => {
            let picked = CallNode {
                ask: NodeAsk {
                    run,
                    call: live.view.state.call.clone(),
                    node: node.id.clone(),
                },
                frame: at.key(),
                label: node.label.clone(),
                jump,
            };
            pointer(spot, node.label.clone())
                .on_press(move |app: &mut App| app.pick_call_node(picked.clone()))
        }
        _ => match jump {
            Some(jump) => pointer(spot, node.label.clone())
                .on_press_tx(move |app: &mut App, tx: &mut Tx| app.jump(&jump, tx)),
            None => spot.help(node.label.clone()),
        },
    }
}

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

    use crucible_contract::calls::{CallKind, NodeStatus, ProgressGraph};
    use crucible_contract::session::TaskStatus;
    use serde_json::json;

    use crate::ui::graph_call::{
        At, HANDFUL, NodeId, Pressed, frame, group_key, group_status, node_id, pressed, took,
        unfolded,
    };

    fn at(i: usize, task: &str) -> At<'_> {
        At {
            i,
            task,
            instance: None,
        }
    }

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
    fn a_github_frame_draws_jobs_as_marked_cards_with_their_duration() {
        let mut jobs = matrix();
        jobs.nodes[0].started = Some("2026-11-25T12:00:00Z".into());
        jobs.nodes[0].ended = Some("2026-11-25T12:01:05Z".into());
        let drawn = frame(
            at(2, "ship"),
            &jobs,
            &HashSet::new(),
            None,
            CallKind::GithubWorkflow,
        );
        assert!(
            drawn.body.starts_with(concat!(
                "    t2c0(\"\u{2713} build \u{b7} 1m 5s\")\n",
                "    t2c1([\"\u{25cf} test \u{d7}3 (1 ok)\"])\n",
                "    t2c4(\"\u{25cb} ship 'it'\")\n",
            )),
            "{}",
            drawn.body
        );
        assert_eq!(drawn.kind, CallKind::GithubWorkflow);
        assert_eq!(
            took(Some("2026-11-25T12:00:00Z"), Some("2026-11-25T12:00:04Z")).as_deref(),
            Some("4s")
        );
        assert_eq!(
            took(Some("2026-11-25T10:00:00Z"), Some("2026-11-25T12:03:00Z")).as_deref(),
            Some("2h 3m")
        );
        assert_eq!(took(Some("2026-11-25T12:00:00Z"), None), None);
    }

    #[test]
    fn a_matrix_group_folds_to_one_node_until_it_is_opened() {
        let none = HashSet::new();
        let folded = frame(at(2, "ship"), &matrix(), &none, None, CallKind::Playbook);
        assert_eq!(
            folded.body,
            concat!(
                "    t2c0[\"build\"]\n",
                "    t2c1[[\"\u{2261} test \u{d7}3 (1 ok)\"]]\n",
                "    t2c4[\"ship 'it'\"]\n",
                "    t2c0 --> t2c1\n",
                "    t2c1 --> t2c4\n",
            )
        );
        assert_eq!(folded.states[1], ("t2c1".to_string(), NodeStatus::Running));
        assert_eq!(folded.items, 5);
        let open = HashSet::from([group_key("ship", "test")]);
        let shown = frame(at(2, "ship"), &matrix(), &open, None, CallKind::Playbook);
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
        let small = matrix();
        let nodes: Vec<serde_json::Value> = (0..=HANDFUL)
            .map(|n| json!({"id": format!("n{n}"), "label": format!("n{n}"), "status": "pending"}))
            .collect();
        let large = graph(json!({"nodes": nodes}));
        let none = HashSet::new();
        let toggled = HashSet::from(["ship".to_string()]);
        assert!(unfolded("ship", &small, &none));
        assert!(!unfolded("ship", &small, &toggled));
        assert!(!unfolded("ship", &large, &none));
        assert!(unfolded("ship", &large, &toggled));
    }

    #[test]
    fn a_frame_counts_what_it_leaves_out() {
        let nodes: Vec<serde_json::Value> = (0..45)
            .map(|n| json!({"id": format!("n{n}"), "label": format!("n{n}"), "status": "pass"}))
            .collect();
        let big = graph(json!({"nodes": nodes, "omitted": 3}));
        let drawn = frame(at(0, "t"), &big, &HashSet::new(), None, CallKind::Playbook);
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
    fn target_node_ids_name_their_task_instance_and_node() {
        assert_eq!(
            node_id("t3c5"),
            Some(NodeId {
                task: 3,
                instance: None,
                node: 5
            })
        );
        assert_eq!(
            node_id("t3i2c5"),
            Some(NodeId {
                task: 3,
                instance: Some(2),
                node: 5
            })
        );
        for nothing in [
            "t3",
            "t3cmore",
            "g3",
            "t3i0",
            "tc1",
            "t3c",
            "t3ic1",
            "t3i2cmore",
        ] {
            assert_eq!(node_id(nothing), None, "{nothing}");
        }
    }

    #[test]
    fn an_instance_frame_names_its_nodes_and_folds_under_its_instance() {
        let instance = At {
            i: 4,
            task: "triage",
            instance: Some((1, "b")),
        };
        assert_eq!(
            (instance.anchor(), instance.subgraph(), instance.key()),
            (
                "t4i1".to_string(),
                "g4k1".to_string(),
                "triage[b]".to_string()
            )
        );
        let drawn = frame(
            instance,
            &matrix(),
            &HashSet::new(),
            None,
            CallKind::Playbook,
        );
        for part in [
            "    t4i1c0[\"build\"]\n",
            "    t4i1 ~~~ t4i1c0\n",
            "    t4i1c1 --> t4i1c4\n",
        ] {
            assert!(drawn.body.contains(part), "{part} in\n{}", drawn.body);
        }
        let open = HashSet::from([group_key("triage[b]", "test")]);
        let legs = frame(instance, &matrix(), &open, None, CallKind::Playbook);
        assert!(
            legs.body.contains("t4i1c3[\"test (win)\"]"),
            "{}",
            legs.body
        );
        assert!(unfolded("triage[b]", &matrix(), &HashSet::new()));
        assert!(!unfolded(
            "triage[b]",
            &matrix(),
            &HashSet::from(["triage[b]".to_string()])
        ));
    }

    #[test]
    fn a_job_node_names_its_running_or_failed_step() {
        use crucible_contract::calls::{NodeDetail, NodeStatus, TektonStep, TektonTaskRun};
        use crucible_contract::session::TaskStatus;
        let step = |name: &str, status| TektonStep {
            name: name.into(),
            container: None,
            status,
            started: None,
            ended: None,
        };
        let pass = NodeStatus::Settled(TaskStatus::Pass);
        let fail = NodeStatus::Settled(TaskStatus::Fail);
        let task = |steps| NodeDetail::TektonTask {
            task_runs: vec![TektonTaskRun {
                name: "tr".into(),
                pod: None,
                steps,
            }],
        };
        let running = task(vec![
            step("prepare", pass),
            step("build", NodeStatus::Running),
            step("push", NodeStatus::Pending),
        ]);
        assert_eq!(
            crate::ui::graph_call::step_note(Some(&running), NodeStatus::Running).as_deref(),
            Some("build 2/3")
        );
        let failed = task(vec![step("prepare", pass), step("build", fail)]);
        assert_eq!(
            crate::ui::graph_call::step_note(Some(&failed), fail).as_deref(),
            Some("failed at build")
        );
        let done = task(vec![step("prepare", pass)]);
        assert_eq!(crate::ui::graph_call::step_note(Some(&done), pass), None);
        assert_eq!(
            crate::ui::graph_call::step_note(None, NodeStatus::Running),
            None
        );
    }
}
