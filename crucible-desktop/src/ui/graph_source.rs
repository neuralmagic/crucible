//! The Graph pane's mermaid flowchart: the source drawn from a run's tasks and their call
//! targets, the colours it names, and the node and subgraph ids it mints.

use std::collections::{HashMap, HashSet};

use crucible_contract::decision::Answer;
use crucible_contract::session::TaskStatus;
use rune::Color;

use crate::session::{Instance, TaskState, TaskView};
use crate::ui::Paint;
use crate::ui::graph_call::{self, At, Targets};

pub(crate) fn graph_frame<'a>(tasks: &'a [TaskView], id: &str) -> Option<(At<'a>, &'a TaskView)> {
    let rest = id.strip_prefix('g')?;
    let (i, k) = match rest.split_once('k') {
        Some((i, k)) => (i, Some(k.parse::<usize>().ok()?)),
        None => (rest, None),
    };
    let i = i.parse::<usize>().ok()?;
    let task = tasks.get(i)?;
    let instance = match k {
        Some(k) => Some((k, task.instances.get(k)?.key.as_str())),
        None => None,
    };
    let at = At {
        i,
        task: &task.name,
        instance,
    };
    Some((at, task))
}

/// How a task in the compiled pack differs from the run's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Edit {
    Added,
    Changed,
}

const TITLE_PAD: char = '\u{2800}';

fn title_pad(kind: crucible_contract::calls::CallKind) -> String {
    let width = match kind.presentation().icon {
        Some(_) => 14,
        None => 8,
    };
    std::iter::repeat_n(TITLE_PAD, width).collect()
}

fn definition(task: &TaskView) -> impl PartialEq + '_ {
    (
        (
            &task.kind,
            &task.depends_on,
            &task.over,
            &task.when,
            &task.keyed,
        ),
        (
            &task.emits,
            &task.files,
            &task.session,
            &task.timeout,
            task.required,
        ),
    )
}

pub(crate) fn edits(run: &[TaskView], compiled: &[TaskView]) -> HashMap<String, Edit> {
    compiled
        .iter()
        .filter_map(|task| match run.iter().find(|r| r.name == task.name) {
            None => Some((task.name.clone(), Edit::Added)),
            Some(was) if definition(was) != definition(task) => {
                Some((task.name.clone(), Edit::Changed))
            }
            Some(_) => None,
        })
        .collect()
}

pub(crate) fn graph_node<'a>(
    tasks: &'a [TaskView],
    id: &str,
) -> Option<(&'a TaskView, Option<&'a str>)> {
    let rest = id.strip_prefix('t')?;
    let rest = rest.strip_suffix("more").unwrap_or(rest);
    let (task, instance) = match rest.split_once('i') {
        Some((task, instance)) => (task, Some(instance.parse::<usize>().ok()?)),
        None => (rest, None),
    };
    let task = tasks.get(task.parse::<usize>().ok()?)?;
    match instance {
        None => Some((task, None)),
        Some(k) => Some((task, Some(task.instances.get(k)?.key.as_str()))),
    }
}

/// A node's fill, outline and text.
#[derive(Clone, Copy)]
struct Swatch {
    fill: Color,
    stroke: Color,
    text: Color,
}

impl Swatch {
    fn with(self, stroke: Color) -> Swatch {
        Swatch { stroke, ..self }
    }
}

/// The colours the source names.
pub(crate) struct Ink {
    pass: Swatch,
    fail: Swatch,
    running: Swatch,
    skipped: Swatch,
    job: Swatch,
    accent: Color,
    faded: Color,
    critical: Color,
    added: Color,
    changed: Color,
    revise: Color,
}

impl Ink {
    pub(crate) fn of(p: &Paint) -> Ink {
        let swatch = |s: rune_kit::StateColors| Swatch {
            fill: s.bg,
            stroke: s.icon,
            text: s.fg,
        };
        Ink {
            pass: swatch(p.c.success),
            fail: swatch(p.c.danger),
            running: swatch(p.c.warning),
            skipped: Swatch {
                fill: p.c.layer_02,
                stroke: p.c.border_strong,
                text: p.c.text_muted,
            },
            job: Swatch {
                fill: p.c.layer_03,
                stroke: p.c.border_strong,
                text: p.c.text_base,
            },
            accent: p.c.text_accent,
            faded: p.c.border_muted,
            critical: p.c.text_muted,
            added: p.pass,
            changed: p.c.warning.icon,
            revise: p.c.text_accent,
        }
    }
}

fn glyph(task: &TaskView) -> &'static str {
    if task.call.is_some() {
        "\u{2197}"
    } else if task.kind == "route" {
        "\u{25c7}"
    } else if task.is_fanout() {
        "\u{2261}"
    } else if task.kind == "agent" {
        "\u{2726}"
    } else if task.kind == "command" {
        "\u{203a}"
    } else if task.kind == "engine" {
        "\u{2699}"
    } else {
        "\u{b7}"
    }
}

fn clean(text: &str) -> String {
    text.replace(['"', '|'], "'")
}

fn finished(task: &TaskView) -> Option<jiff::Timestamp> {
    task.result.as_ref().and_then(|r| r.at).or_else(|| {
        task.instances
            .iter()
            .filter_map(|i| i.result.as_ref().and_then(|r| r.at))
            .max()
    })
}

fn critical_path(tasks: &[TaskView]) -> HashSet<(usize, usize)> {
    let index = |name: &str| tasks.iter().position(|task| task.name == name);
    let mut path = HashSet::new();
    let Some(mut at) = (0..tasks.len())
        .filter_map(|i| finished(&tasks[i]).map(|t| (t, i)))
        .max()
        .map(|(_, i)| i)
    else {
        return path;
    };
    while let Some(from) = tasks[at]
        .depends_on
        .iter()
        .filter_map(|dep| index(dep))
        .filter_map(|i| finished(&tasks[i]).map(|t| (t, i)))
        .max()
        .map(|(_, i)| i)
    {
        if !path.insert((from, at)) {
            break;
        }
        at = from;
    }
    path
}

fn ancestors(tasks: &[TaskView]) -> Vec<HashSet<usize>> {
    let index = |name: &str| tasks.iter().position(|task| task.name == name);
    (0..tasks.len())
        .map(|i| {
            let mut seen = HashSet::new();
            let mut stack: Vec<usize> = tasks[i]
                .depends_on
                .iter()
                .filter_map(|d| index(d))
                .collect();
            while let Some(at) = stack.pop() {
                if at != i && seen.insert(at) {
                    stack.extend(tasks[at].depends_on.iter().filter_map(|d| index(d)));
                }
            }
            seen
        })
        .collect()
}

const UNFOLDED: usize = 24;

const ITEMS: usize = rune_mermaid::MAX_ITEMS;

const ROW: usize = 6;

fn instances_each(tasks: &[TaskView], unfolded: impl Fn(&TaskView) -> bool, calls: usize) -> usize {
    let taken = taken(tasks, calls);
    let per: usize = tasks
        .iter()
        .filter(|task| unfolded(task))
        .map(|task| 2 + task.depends_on.len())
        .sum();
    (ITEMS.saturating_sub(taken) / per.max(1)).min(UNFOLDED)
}

fn taken(tasks: &[TaskView], calls: usize) -> usize {
    let edges: usize = tasks
        .iter()
        .map(|task| 2 * task.depends_on.len() + task.revise.len())
        .sum();
    let framed = tasks.iter().filter(|task| !task.instances.is_empty());
    tasks.len() + edges + 3 * framed.count() + calls
}

fn answer(instance: &Instance, question: &str) -> Option<String> {
    let recorded = instance.result.as_ref()?.output.as_ref()?.get(question)?;
    let answer: Answer = serde_json::from_value(recorded.clone()).ok()?;
    Some(if answer.labels.is_empty() {
        answer.label.as_str().to_string()
    } else {
        let labels: Vec<&str> = answer.labels.iter().map(|label| label.as_str()).collect();
        labels.join(" / ")
    })
}

fn first_answer(instance: &Instance) -> Option<String> {
    let output = instance.result.as_ref()?.output.as_ref()?.as_object()?;
    output
        .keys()
        .find_map(|question| answer(instance, question))
}

/// What the graph shows beyond the tasks themselves.
pub(crate) struct Shown<'a> {
    pub(crate) open: Option<&'a str>,
    pub(crate) folded: &'a HashSet<String>,
    pub(crate) marks: &'a HashMap<String, Edit>,
    pub(crate) targets: &'a Targets<'a>,
    pub(crate) call_folds: &'a HashSet<String>,
    pub(crate) picked: Option<(&'a str, &'a str)>,
}

impl Shown<'_> {
    fn frame(
        &self,
        at: At,
        graph: &crucible_contract::calls::ProgressGraph,
        task: &TaskView,
    ) -> graph_call::Frame {
        let key = at.key();
        if !graph_call::unfolded(&key, graph, self.call_folds) {
            return graph_call::Frame::default();
        }
        let picked = self
            .picked
            .filter(|(frame, _)| *frame == key)
            .map(|(_, node)| node);
        let kind = task
            .call
            .as_ref()
            .map_or(crucible_contract::calls::CallKind::Playbook, |call| {
                call.kind
            });
        graph_call::frame(at, graph, self.call_folds, picked, kind)
    }
}

pub(crate) fn mermaid_source<'a>(tasks: &'a [TaskView], shown: &Shown, ink: &Ink) -> String {
    let mut source = String::from("flowchart TD\n");
    for (class, s) in [
        ("pass", ink.pass),
        ("fail", ink.fail),
        ("running", ink.running),
        ("skipped", ink.skipped),
    ] {
        source.push_str(&format!(
            "  classDef {class} fill:{},stroke:{},color:{}\n",
            s.fill.hex(),
            s.stroke.hex(),
            s.text.hex()
        ));
    }
    for kind in crucible_contract::calls::CallKind::ALL {
        if kind.presentation().nodes != crucible_contract::calls::NodeLook::Jobs {
            continue;
        }
        let card = ink
            .job
            .with(crate::ui::graph_fx::accent(kind).map_or(ink.job.stroke, |a| a.alpha(0.75)));
        for (state, s) in [
            ("", card),
            ("_pass", card),
            ("_fail", card.with(ink.fail.stroke)),
            ("_running", card.with(ink.running.stroke)),
            ("_skipped", card.with(ink.skipped.stroke)),
        ] {
            source.push_str(&format!(
                "  classDef {}{state} fill:{},stroke:{},color:{}\n",
                job_class(kind),
                s.fill.hex(),
                s.stroke.hex(),
                s.text.hex()
            ));
        }
    }
    let frames: Vec<Option<graph_call::Frame>> = tasks
        .iter()
        .enumerate()
        .map(|(i, task)| {
            if !task.instances.is_empty() {
                return None;
            }
            let graph = shown.targets.graph(task, None)?;
            let at = At {
                i,
                task: &task.name,
                instance: None,
            };
            Some(shown.frame(at, &graph, task))
        })
        .collect();
    let open_frame = |i: usize| {
        frames
            .get(i)
            .and_then(Option::as_ref)
            .is_some_and(|frame| !frame.body.is_empty())
    };
    let id = |name: &str| match tasks.iter().position(|task| task.name == name) {
        Some(i) if open_frame(i) => format!("g{i}"),
        Some(i) => format!("t{i}"),
        None => {
            let cleaned: String = name
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            format!("x{cleaned}")
        }
    };
    let framed_calls = frames.iter().flatten().map(|frame| frame.items + 3).sum();
    let unfolded =
        |task: &TaskView| !task.instances.is_empty() && !shown.folded.contains(&task.name);
    let each = instances_each(tasks, unfolded, framed_calls);
    let drawn = |task: &'a TaskView| -> &'a [Instance] {
        let shown = if unfolded(task) { each } else { 0 };
        task.instances
            .get(..shown.min(task.instances.len()))
            .unwrap_or_default()
    };
    let one_row = |task: &'a TaskView| -> &'a [Instance] {
        let instances = drawn(task);
        if instances.len() == task.instances.len() && instances.len() <= ROW {
            instances
        } else {
            &[]
        }
    };
    let drawn_items: usize = tasks
        .iter()
        .map(|task| drawn(task).len() * (3 + task.depends_on.len()))
        .sum();
    let mut left = ITEMS.saturating_sub(taken(tasks, framed_calls) + drawn_items);
    let mut classes: Vec<(String, String)> = Vec::new();
    let mut picked: Vec<String> = Vec::new();
    let mut edge = 0;
    for (i, task) in tasks.iter().enumerate() {
        let named = graph_call::label(task, shown.targets.call(task, None))
            .unwrap_or_else(|| task.name.clone());
        let mut label = format!("{} {}", glyph(task), clean(&named));
        if !task.instances.is_empty() {
            label.push_str(&format!(
                " \u{d7}{} ({} ok)",
                task.instances.len(),
                task.instance_counts().passed
            ));
        }
        if let (Some(revised), Some(round)) = (&task.revised, task.rounds.last()) {
            label.push_str(&format!(" r{}/{}", round.n, revised.max_rounds));
        }
        let (open, close) = if task.kind == "route" {
            ("{{", "}}")
        } else if task.is_fanout() || task.call.is_some() {
            ("[[", "]]")
        } else if task.kind == "agent" {
            ("(", ")")
        } else {
            ("[", "]")
        };
        let node = format!("t{i}{open}\"{label}\"{close}");
        if task.instances.is_empty() {
            match frames.get(i).and_then(Option::as_ref) {
                Some(frame) if !frame.body.is_empty() => {
                    source.push_str(&format!(
                        "  subgraph g{i} [\"{}{}\"]\n{}  end\n",
                        clean(&named),
                        title_pad(frame.kind),
                        frame.body
                    ));
                    edge += frame.links;
                    for (id, status) in &frame.states {
                        if let Some(class) = frame_class(frame.kind, *status) {
                            classes.push((id.clone(), class));
                        }
                    }
                    picked.extend(frame.picked.clone());
                }
                Some(_) => {
                    source.push_str(&format!("  subgraph g{i} [\" \"]\n    {node}\n  end\n"))
                }
                None => source.push_str(&format!("  {node}\n")),
            }
            continue;
        }
        let instances = drawn(task);
        source.push_str(&format!("  subgraph g{i} [\" \"]\n    {node}\n"));
        let anchor = |k: usize| match k.checked_sub(ROW) {
            Some(over) => format!("t{i}i{over}"),
            None => format!("t{i}"),
        };
        for (k, instance) in instances.iter().enumerate() {
            let mut label = clean(&instance.key);
            if let Some(answer) = first_answer(instance).filter(|_| task.kind == "route") {
                label.push_str(&format!(" \u{b7} {}", clean(&answer)));
            }
            let node = format!("t{i}i{k}[\"{label}\"]");
            let at = At {
                i,
                task: &task.name,
                instance: Some((k, &instance.key)),
            };
            let frame = shown
                .targets
                .graph(task, Some(&instance.key))
                .map(|graph| shown.frame(at, &graph, task))
                .filter(|frame| frame.items < left)
                .or_else(|| task.call.is_some().then(graph_call::Frame::default));
            match frame {
                Some(frame) => {
                    left = left.saturating_sub(frame.items + 1);
                    source.push_str(&format!(
                        "    subgraph {} [\" \"]\n      {node}\n{}    end\n",
                        at.subgraph(),
                        frame.body
                    ));
                    edge += frame.links;
                    for (id, status) in &frame.states {
                        if let Some(class) = frame_class(frame.kind, *status) {
                            classes.push((id.clone(), class));
                        }
                    }
                    picked.extend(frame.picked);
                }
                None => source.push_str(&format!("    {node}\n")),
            }
            source.push_str(&format!("    {} ~~~ t{i}i{k}\n", anchor(k)));
            edge += 1;
            if let Some(class) = state_class(instance.state()) {
                classes.push((format!("t{i}i{k}"), class.to_string()));
            }
        }
        if !instances.is_empty() && task.instances.len() > instances.len() {
            source.push_str(&format!(
                "    t{i}more[\"+{} more\"]\n    {} ~~~ t{i}more\n",
                task.instances.len() - instances.len(),
                anchor(instances.len())
            ));
            edge += 1;
        }
        source.push_str("  end\n");
    }
    let critical = critical_path(tasks);
    let above = ancestors(tasks);
    let (mut faded, mut heavy) = (Vec::new(), Vec::new());
    for (to, task) in tasks.iter().enumerate() {
        let passed_over = matches!(
            task.state(),
            TaskState::Settled(TaskStatus::NotTaken | TaskStatus::Skipped)
        );
        for dependency in &task.depends_on {
            let field = |reference: &str| {
                reference
                    .split_once('.')
                    .filter(|(producer, _)| producer == dependency)
                    .map(|(_, field)| clean(field))
            };
            let over = field(&task.over);
            let mut labels: Vec<String> = over.iter().map(|f| format!("each .{f}")).collect();
            labels.extend(
                task.keyed
                    .iter()
                    .filter_map(|k| field(k))
                    .map(|f| format!(".{f}")),
            );
            if let Some(call) = &task.call {
                labels.extend(call.bound_by(dependency).map(clean));
            }
            if let Some((_, answers)) = task
                .when
                .split_once(" in ")
                .filter(|(asked, _)| field(asked).is_some())
            {
                labels.push(clean(&answers.replace('|', " / ")));
            }
            let arrow = if passed_over {
                "-.->"
            } else if over.is_some() {
                "==>"
            } else {
                "-->"
            };
            let from = tasks.iter().position(|t| t.name == *dependency);
            let producer = from.and_then(|f| tasks.get(f));
            let pairs: Vec<(usize, &Instance, usize, &Instance)> = producer
                .filter(|producer| !task.over.is_empty() && task.over == producer.over)
                .map(|producer| (one_row(producer), one_row(task)))
                .map(|(producers, instances)| {
                    instances
                        .iter()
                        .enumerate()
                        .filter_map(|(k, instance)| {
                            let j = producers.iter().position(|p| p.key == instance.key)?;
                            Some((j, producers.get(j)?, k, instance))
                        })
                        .collect()
                })
                .unwrap_or_default();
            if let (Some(f), false) = (from, pairs.is_empty()) {
                let question = task
                    .when
                    .split_once(" in ")
                    .and_then(|(asked, _)| field(asked));
                source.push_str(&format!("  g{f} ~~~ t{to}\n"));
                edge += 1;
                for (j, produced, k, instance) in pairs {
                    let untaken = matches!(
                        instance.state(),
                        TaskState::Settled(TaskStatus::NotTaken | TaskStatus::Skipped)
                    );
                    let label = question
                        .as_deref()
                        .and_then(|q| answer(produced, q))
                        .map(|a| format!("|{}|", clean(&a)))
                        .unwrap_or_default();
                    let arrow = if untaken { "-.->" } else { "-->" };
                    source.push_str(&format!("  t{f}i{j} {arrow}{label} t{to}i{k}\n"));
                    if untaken {
                        faded.push(edge.to_string());
                    }
                    edge += 1;
                }
                continue;
            }
            let implied = from.is_some_and(|f| {
                task.depends_on.iter().any(|other| {
                    tasks
                        .iter()
                        .position(|t| t.name == *other)
                        .is_some_and(|o| above[o].contains(&f))
                })
            });
            if labels.is_empty() && implied {
                continue;
            }
            let label = if labels.is_empty() {
                String::new()
            } else {
                format!("|{}|", labels.join(", "))
            };
            let source_id = match (from, producer) {
                (Some(f), Some(producer)) if !producer.instances.is_empty() => format!("g{f}"),
                _ => id(dependency),
            };
            let target_id = if open_frame(to) {
                format!("g{to}")
            } else {
                format!("t{to}")
            };
            source.push_str(&format!("  {source_id} {arrow}{label} {target_id}\n"));
            if passed_over {
                faded.push(edge.to_string());
            } else if from.is_some_and(|f| critical.contains(&(f, to))) {
                heavy.push(edge.to_string());
            }
            edge += 1;
        }
    }
    let mut revise = Vec::new();
    for (from, task) in tasks.iter().enumerate() {
        let rounds = task.rounds.last().map_or(0, |round| round.n);
        for target in &task.revise {
            let label = match rounds {
                0 => format!("revise \u{2264}{}", task.max_rounds),
                n => format!("revise {n}/{}", task.max_rounds),
            };
            source.push_str(&format!("  t{from} -.->|{label}| {}\n", id(target)));
            revise.push(edge.to_string());
            edge += 1;
        }
    }
    if !revise.is_empty() {
        source.push_str(&format!(
            "  linkStyle {} stroke:{},stroke-width:1.5\n",
            revise.join(","),
            ink.revise.hex()
        ));
    }
    if !faded.is_empty() {
        source.push_str(&format!(
            "  linkStyle {} stroke:{}\n",
            faded.join(","),
            ink.faded.hex()
        ));
    }
    if !heavy.is_empty() {
        source.push_str(&format!(
            "  linkStyle {} stroke:{},stroke-width:2.25\n",
            heavy.join(","),
            ink.critical.hex()
        ));
    }
    for (i, task) in tasks.iter().enumerate() {
        if open_frame(i) {
            continue;
        }
        if let Some(class) = state_class(task.state()) {
            classes.push((format!("t{i}"), class.to_string()));
        }
    }
    for (node, class) in classes {
        source.push_str(&format!("  class {node} {class}\n"));
    }
    for node in picked {
        source.push_str(&format!(
            "  style {node} stroke:{},stroke-width:2\n",
            ink.accent.hex()
        ));
    }
    for (i, task) in tasks.iter().enumerate() {
        let ink = match shown.marks.get(&task.name) {
            Some(Edit::Added) => ink.added,
            Some(Edit::Changed) => ink.changed,
            None => continue,
        };
        if open_frame(i) {
            continue;
        }
        source.push_str(&format!("  style t{i} stroke:{}\n", ink.hex()));
    }
    if let Some(open) = shown
        .open
        .filter(|open| tasks.iter().any(|task| task.name == *open))
    {
        source.push_str(&format!(
            "  style {} stroke:{}\n",
            id(open),
            ink.accent.hex()
        ));
    }
    source
}

fn job_class(kind: crucible_contract::calls::CallKind) -> String {
    format!("job_{}", kind.as_str())
}

fn frame_class(
    kind: crucible_contract::calls::CallKind,
    status: crucible_contract::calls::NodeStatus,
) -> Option<String> {
    let state = state_class(graph_call::state(status));
    if kind.presentation().nodes != crucible_contract::calls::NodeLook::Jobs {
        return state.map(str::to_string);
    }
    let suffix = match state {
        None => "",
        Some("pass") => "_pass",
        Some("fail") => "_fail",
        Some("running") => "_running",
        Some(_) => "_skipped",
    };
    Some(format!("{}{suffix}", job_class(kind)))
}

fn state_class(state: TaskState) -> Option<&'static str> {
    Some(match state {
        TaskState::Pending => return None,
        TaskState::Running(_) => "running",
        TaskState::Settled(status) if status.passed() => "pass",
        TaskState::Settled(TaskStatus::Skipped | TaskStatus::NotTaken) => "skipped",
        TaskState::Settled(_) => "fail",
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use rune::Color;

    use crate::live::calls::LiveCall;
    use crate::pack::PackFiles;
    use crate::session::{RunFold, TaskView};
    use crate::ui::graph_call::Targets;
    use crate::ui::graph_source::{
        Edit, ITEMS, Ink, ROW, Shown, Swatch, UNFOLDED, critical_path, edits, graph_frame,
        graph_node, mermaid_source,
    };

    const RUN: &str = concat!(
        r#"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":["#,
        r#"{"name":"scan","kind":"command","required":true},"#,
        r#"{"name":"pick","kind":"route","depends_on":["scan"],"required":true},"#,
        r#"{"name":"fix","kind":"agent","depends_on":["scan","pick"],"required":true,"over":"scan.cves","keyed":["scan.repo"],"when":"pick.risky in yes|maybe"},"#,
        r#"{"name":"note","kind":"agent","depends_on":["fix"],"required":false},"#,
        r#"{"name":"skip \"me\"","kind":"command","depends_on":["pick"],"required":true}"#,
        r#"]}"#,
        "\n",
        r#"{"v":1,"kind":"task_result","task":"skip \"me\"","status":"not_taken","attempts":0,"cost_usd":0.0,"secs":0.0,"note":""}"#,
        "\n",
    );

    const TIMED: &str = concat!(
        r#"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":["#,
        r#"{"name":"a","kind":"command","required":true},"#,
        r#"{"name":"b","kind":"command","depends_on":["a"],"required":true},"#,
        r#"{"name":"c","kind":"agent","depends_on":["a"],"required":true,"over":"a.items"},"#,
        r#"{"name":"d","kind":"command","depends_on":["b","c"],"required":true}"#,
        r#"]}"#,
        "\n",
        r#"{"v":1,"kind":"task_result","task":"a","status":"pass","attempts":1,"cost_usd":0.0,"secs":1.0,"note":"","at":"2026-11-06T10:00:00Z"}"#,
        "\n",
        r#"{"v":1,"kind":"task_result","task":"b","status":"pass","attempts":1,"cost_usd":0.0,"secs":1.0,"note":"","at":"2026-11-06T10:05:00Z"}"#,
        "\n",
        r#"{"v":1,"kind":"task_result","task":"c[x]","status":"pass","attempts":1,"cost_usd":0.0,"secs":1.0,"note":"","at":"2026-11-06T10:12:00Z"}"#,
        "\n",
        r#"{"v":1,"kind":"task_result","task":"c[y \"q\"]","status":"fail","attempts":1,"cost_usd":0.0,"secs":1.0,"note":"","at":"2026-11-06T10:20:00Z"}"#,
        "\n",
        r#"{"v":1,"kind":"task_result","task":"d","status":"pass","attempts":1,"cost_usd":0.0,"secs":1.0,"note":"","at":"2026-11-06T10:30:00Z"}"#,
        "\n",
    );

    fn ink() -> Ink {
        let s = Swatch {
            fill: Color::BLACK,
            stroke: Color::WHITE,
            text: Color::WHITE,
        };
        Ink {
            pass: s,
            fail: s,
            running: s,
            skipped: s,
            job: s,
            accent: Color::WHITE,
            faded: Color::BLACK,
            critical: Color::WHITE,
            added: Color::WHITE,
            changed: Color::BLACK,
            revise: Color::TRANSPARENT,
        }
    }

    fn fold(text: &str) -> Vec<TaskView> {
        let mut fold = RunFold::default();
        for line in text.lines() {
            fold.apply_line(line);
        }
        fold.tasks
    }

    fn source(tasks: &[TaskView], open: Option<&str>, folded: &[&str]) -> String {
        called(tasks, open, folded, &Targets::new(&[], None), &[])
    }

    fn called(
        tasks: &[TaskView],
        open: Option<&str>,
        folded: &[&str],
        targets: &Targets,
        call_folds: &[&str],
    ) -> String {
        picking(tasks, open, folded, targets, call_folds, None)
    }

    fn picking(
        tasks: &[TaskView],
        open: Option<&str>,
        folded: &[&str],
        targets: &Targets,
        call_folds: &[&str],
        picked: Option<(&str, &str)>,
    ) -> String {
        let folded: HashSet<String> = folded.iter().map(ToString::to_string).collect();
        let call_folds: HashSet<String> = call_folds.iter().map(ToString::to_string).collect();
        let source = mermaid_source(
            tasks,
            &Shown {
                open,
                folded: &folded,
                marks: &std::collections::HashMap::new(),
                targets,
                call_folds: &call_folds,
                picked,
            },
            &ink(),
        );
        assert!(rune_mermaid::supported(&source), "{source}");
        source
    }

    fn contains_all(source: &str, parts: &[&str]) {
        for part in parts {
            assert!(source.contains(part), "{part} in\n{source}");
        }
    }

    #[test]
    fn nodes_take_the_shape_and_glyph_of_what_runs_them() {
        contains_all(
            &source(&fold(RUN), None, &[]),
            &[
                "t0[\"\u{203a} scan\"]",
                "t1{{\"\u{25c7} pick\"}}",
                "t2[[\"\u{2261} fix\"]]",
                "t3(\"\u{2726} note\")",
                "t4[\"\u{203a} skip 'me'\"]",
            ],
        );
    }

    #[test]
    fn edges_say_how_data_reaches_each_task_and_only_untaken_ones_are_dotted() {
        let source = source(&fold(RUN), Some("fix"), &[]);
        contains_all(
            &source,
            &[
                "t0 ==>|each .cves, .repo| t2",
                "t1 -->|yes / maybe| t2",
                "t2 --> t3",
                "t1 -.-> t4",
                "t0 --> t1",
                "linkStyle 4 stroke:",
                "style t2 stroke:",
            ],
        );
        assert!(
            !source.contains("stroke-width"),
            "no times, no critical path"
        );
    }

    #[test]
    fn the_critical_path_runs_back_through_whatever_settled_last() {
        let tasks = fold(TIMED);
        assert_eq!(critical_path(&tasks), HashSet::from([(0, 2), (2, 3)]));
        assert!(critical_path(&fold(RUN)).is_empty());
        contains_all(
            &source(&tasks, None, &["c"]),
            &["linkStyle 1,3 stroke:#ffffffff,stroke-width:2.25"],
        );
    }

    #[test]
    fn a_fanout_draws_its_instances_and_keeps_its_edges_until_it_is_folded() {
        let tasks = fold(TIMED);
        contains_all(
            &source(&tasks, None, &["a"]),
            &[
                "subgraph g2 [\" \"]",
                "t2[[\"\u{2261} c \u{d7}2 (1 ok)\"]]",
                "t2i0[\"x\"]",
                "t2i1[\"y 'q'\"]",
                "t2 ~~~ t2i1",
                "t0 ==>|each .items| t2",
                "g2 --> t3",
                "class t2i0 pass",
                "class t2i1 fail",
                "linkStyle 3,5 stroke:",
            ],
        );
        let folded = source(&tasks, None, &["c"]);
        contains_all(
            &folded,
            &[
                "subgraph g2 [\" \"]\n    t2[[\"\u{2261} c \u{d7}2 (1 ok)\"]]\n  end",
                "t0 ==>|each .items| t2",
                "g2 --> t3",
            ],
        );
        assert!(
            !folded.contains("t2i0") && !folded.contains("~~~"),
            "{folded}"
        );
    }

    fn answered(task: &str, verdict: &str) -> String {
        format!(
            r#"{{"v":1,"kind":"task_result","task":"{task}","status":"pass","attempts":1,"cost_usd":0.0,"secs":1.0,"note":"","output":{{"verdict":{{"label":"{verdict}","confidence":1.0,"probabilities":{{}}}}}}}}"#
        )
    }

    fn settled(task: &str, status: &str) -> String {
        format!(
            r#"{{"v":1,"kind":"task_result","task":"{task}","status":"{status}","attempts":1,"cost_usd":0.0,"secs":1.0,"note":""}}"#
        )
    }

    fn triaged(keys: &[&str], verdict: impl Fn(usize) -> &'static str) -> Vec<TaskView> {
        let mut lines = vec![
            concat!(
                r#"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":["#,
                r#"{"name":"scan","kind":"command","required":true},"#,
                r#"{"name":"triage","kind":"route","depends_on":["scan"],"required":true,"over":"scan.cves"},"#,
                r#"{"name":"fix","kind":"agent","depends_on":["scan","triage"],"required":true,"over":"scan.cves","when":"triage.verdict in fix"},"#,
                r#"{"name":"report","kind":"command","depends_on":["fix"],"required":true}"#,
                r#"]}"#,
            )
            .to_string(),
        ];
        for (k, key) in keys.iter().enumerate() {
            lines.push(answered(&format!("triage[{key}]"), verdict(k)));
        }
        for (k, key) in keys.iter().enumerate() {
            let status = if verdict(k) == "fix" {
                "pass"
            } else {
                "not_taken"
            };
            lines.push(settled(&format!("fix[{key}]"), status));
        }
        fold(&lines.join("\n"))
    }

    #[test]
    fn fanouts_over_one_list_join_instance_to_instance_with_what_the_route_answered() {
        let tasks = triaged(&["a", "b"], |k| ["fix", "skip"][k]);
        let source = source(&tasks, None, &[]);
        contains_all(
            &source,
            &[
                "t1i0[\"a \u{b7} fix\"]",
                "t1i1[\"b \u{b7} skip\"]",
                "t2i0[\"a\"]",
                "t0 ==>|each .cves| t1",
                "t0 ==>|each .cves| t2",
                "g1 ~~~ t2",
                "t1i0 -->|fix| t2i0",
                "t1i1 -.->|skip| t2i1",
                "g2 --> t3",
                "linkStyle 8 stroke:#000000ff",
                "class t2i1 skipped",
            ],
        );
        assert!(!source.contains("t1 -->"), "{source}");

        for folded in ["triage", "fix"] {
            let source = self::source(&tasks, None, &[folded]);
            contains_all(&source, &["-->|fix| t2\n"]);
            assert!(!source.contains("-->|fix| t2i0"), "{source}");
        }
    }

    #[test]
    fn instances_sit_in_rows_under_their_fanout_and_the_rest_are_counted() {
        let keys: Vec<String> = (0..UNFOLDED + 5).map(|k| format!("k{k}")).collect();
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        let source = source(&triaged(&keys, |_| "fix"), None, &[]);
        let last = UNFOLDED - 1;
        contains_all(
            &source,
            &[
                &format!("t1 ~~~ t1i{}", ROW - 1),
                &format!("t1i0 ~~~ t1i{ROW}"),
                &format!("t1i{} ~~~ t1i{last}", last - ROW),
                &format!("t1more[\"+5 more\"]\n    t1i{} ~~~ t1more", UNFOLDED - ROW),
                "g1 -->|fix| t2\n",
            ],
        );
        assert!(!source.contains(&format!("t1i{UNFOLDED}[")), "{source}");
    }

    #[test]
    fn only_fanouts_that_fit_one_row_join_instance_to_instance() {
        let keys: Vec<String> = (0..ROW + 1).map(|k| format!("k{k}")).collect();
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        let last = ROW - 1;
        let one_row = source(&triaged(&keys[..ROW], |_| "fix"), None, &[]);
        contains_all(&one_row, &[&format!("t1i{last} -->|fix| t2i{last}")]);
        let two_rows = source(&triaged(&keys, |_| "fix"), None, &[]);
        contains_all(&two_rows, &["g1 -->|fix| t2\n", &format!("t2i{ROW}[")]);
        assert!(!two_rows.contains("-->|fix| t2i"), "{two_rows}");
    }

    #[test]
    fn a_gated_loop_of_62_turns_draws() {
        let mut tasks = vec![r#"{"name":"start","kind":"command","required":true}"#.to_string()];
        let mut scored = "start".to_string();
        for i in 1..=62 {
            let gate = format!("{scored}-go");
            tasks.push(format!(
                r#"{{"name":"{gate}","kind":"route","depends_on":["{scored}"],"required":true}}"#
            ));
            tasks.push(format!(
                r#"{{"name":"turn_{i}","kind":"agent","depends_on":["{gate}"],"required":false,"when":"{gate}.go in yes"}}"#
            ));
            scored = format!("score_{i}");
            tasks.push(format!(
                r#"{{"name":"{scored}","kind":"command","depends_on":["turn_{i}","{gate}"],"required":true,"when":"{gate}.go in yes"}}"#
            ));
        }
        tasks.push(format!(
            r#"{{"name":"final","kind":"command","depends_on":["{scored}"],"required":true}}"#
        ));
        let admitted = format!(
            r#"{{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":[{}]}}"#,
            tasks.join(",")
        );
        let source = source(&fold(&admitted), None, &[]);
        assert_eq!(
            (tasks.len(), source.matches("-->").count()),
            (188, 249),
            "{source}"
        );
        let style = rune_mermaid::DiagramStyle::neutral(true);
        assert!(
            rune_mermaid::render_with_hover(
                &source,
                &style,
                rune_mermaid::Hover::Off,
                Color::WHITE
            )
            .is_some(),
            "{source}"
        );
    }

    #[test]
    fn a_run_of_many_fanouts_draws_fewer_instances_each_and_still_lays_out() {
        let stage = |i: usize| {
            let before = match i.checked_sub(1) {
                Some(b) => format!(r#""list","s{b}""#),
                None => r#""list""#.to_string(),
            };
            format!(
                r#"{{"name":"s{i}","kind":"command","depends_on":[{before}],"required":true,"over":"list.items"}}"#
            )
        };
        let stages: Vec<String> = (0..12).map(stage).collect();
        let mut lines = vec![format!(
            r#"{{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":[{{"name":"list","kind":"command","required":true}},{}]}}"#,
            stages.join(",")
        )];
        for i in 0..12 {
            for k in 0..40 {
                lines.push(settled(&format!("s{i}[k{k}]"), "pass"));
            }
        }
        let source = source(&fold(&lines.join("\n")), None, &[]);
        let drawn = source.matches("-->").count() + source.matches("~~~").count();
        let nodes = source.matches("[\"").count();
        assert!(drawn + nodes <= ITEMS, "{drawn} edges, {nodes} nodes");
        contains_all(&source, &["g1 --> t2\n", "t12i0[\"k0\"]", "t12more[\"+"]);
        assert!(
            !source.contains(&format!("t1i{}[", UNFOLDED - 1)),
            "{source}"
        );
        let style = rune_mermaid::DiagramStyle::neutral(true);
        assert!(
            rune_mermaid::render_with_hover(
                &source,
                &style,
                rune_mermaid::Hover::Off,
                Color::WHITE
            )
            .is_some(),
            "{source}"
        );
    }

    #[test]
    fn an_edit_marks_the_tasks_it_adds_or_redefines_but_not_how_they_ran() {
        let run = fold(TIMED);
        let mut compiled = fold(RUN);
        compiled.extend(fold(TIMED).into_iter().map(|mut task| {
            task.result = None;
            if task.name == "b" {
                task.depends_on = vec!["c".into()];
            }
            task
        }));
        let marks = edits(&run, &compiled);
        assert_eq!(marks.get("b"), Some(&Edit::Changed));
        assert_eq!(marks.get("scan"), Some(&Edit::Added));
        assert_eq!(
            marks.get("a"),
            None,
            "a settled run is not a different definition"
        );
        let folded = HashSet::new();
        let source = mermaid_source(
            &compiled,
            &Shown {
                open: None,
                folded: &folded,
                marks: &marks,
                targets: &Targets::new(&[], None),
                call_folds: &folded,
                picked: None,
            },
            &ink(),
        );
        let b = compiled.iter().position(|t| t.name == "b").unwrap();
        assert!(
            source.contains(&format!("style t{b} stroke:#000000ff")),
            "{source}"
        );
    }

    #[test]
    fn a_reviser_draws_its_revise_edge_back_to_its_target() {
        let all = include_str!("../../fixtures/revise-session.jsonl");
        let plan = all.lines().next().unwrap();
        contains_all(
            &source(&fold(plan), None, &[]),
            &[
                "t1 -.->|revise \u{2264}3| t0",
                "t0[\"\u{203a} author\"]",
                "linkStyle 2 stroke:#00000000,stroke-width:1.5",
            ],
        );
        let ran = source(&fold(all), None, &[]);
        contains_all(
            &ran,
            &[
                "t0[\"\u{203a} author r2/3\"]",
                "t1[\"\u{203a} review r2/3\"]",
                "t0 --> t1",
                "t1 -.->|revise 2/3| t0",
                "t1 --> t2",
                "class t0 pass",
            ],
        );
        assert!(
            !ran.contains("t0 -.-> t1"),
            "a revise loop is not a passed-over edge"
        );
    }

    const IMPLIED: &str = concat!(
        r#"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":["#,
        r#"{"name":"a","kind":"command","required":true},"#,
        r#"{"name":"b","kind":"command","depends_on":["a"],"required":true},"#,
        r#"{"name":"c","kind":"command","depends_on":["a","b"],"required":true},"#,
        r#"{"name":"d","kind":"command","depends_on":["a","b","c"],"required":true,"keyed":["a.k"]},"#,
        r#"{"name":"e","kind":"command","depends_on":["a","d"],"required":true}"#,
        r#"]}"#,
        "\n",
        r#"{"v":1,"kind":"task_result","task":"e","status":"not_taken","attempts":0,"cost_usd":0.0,"secs":0.0,"note":""}"#,
        "\n",
    );

    #[test]
    fn a_dependency_another_one_leads_back_to_is_not_drawn_unless_it_carries_data() {
        let source = source(&fold(IMPLIED), None, &[]);
        let edges: Vec<&str> = source
            .lines()
            .map(str::trim)
            .filter(|line| line.contains("->"))
            .collect();
        assert_eq!(
            edges,
            [
                "t0 --> t1",
                "t1 --> t2",
                "t0 -->|.k| t3",
                "t2 --> t3",
                "t3 -.-> t4"
            ],
            "{source}"
        );
        contains_all(&source, &["linkStyle 4 stroke:#000000ff"]);
    }

    #[test]
    fn a_task_that_names_everything_before_it_still_lays_out() {
        let tasks: Vec<String> = (0..36)
            .map(|i| {
                let before: Vec<String> = (0..i).map(|d| format!(r#""t{d}""#)).collect();
                format!(
                    r#"{{"name":"t{i}","kind":"command","depends_on":[{}],"required":true}}"#,
                    before.join(",")
                )
            })
            .collect();
        let plan = format!(
            r#"{{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":[{}]}}"#,
            tasks.join(",")
        );
        let source = source(&fold(&plan), None, &[]);
        assert_eq!(source.matches("-->").count(), 35, "{source}");
        let style = rune_mermaid::DiagramStyle::neutral(true);
        assert!(
            rune_mermaid::render_with_hover(
                &source,
                &style,
                rune_mermaid::Hover::Off,
                Color::WHITE
            )
            .is_some(),
            "{source}"
        );
    }

    #[test]
    fn ids_lead_back_to_tasks_and_instances() {
        let tasks = fold(TIMED);
        let named = |id: &str| graph_node(&tasks, id).map(|(t, k)| (t.name.as_str(), k));
        assert_eq!(named("t2"), Some(("c", None)));
        assert_eq!(named("t2i1"), Some(("c", Some("y \"q\""))));
        assert_eq!(named("t2more"), Some(("c", None)));
        for nothing in ["t9", "t2i7", "t9more", "g2", "xscan", "t", "tmore"] {
            assert_eq!(named(nothing), None, "{nothing}");
        }
        let framed = |id: &str| graph_frame(&tasks, id).map(|(_, task)| task.name.as_str());
        assert_eq!(framed("g2"), Some("c"));
        assert_eq!(
            graph_frame(&tasks, "g2k1").map(|(at, _)| (at.i, at.instance)),
            Some((2, Some((1, "y \"q\""))))
        );
        for nothing in ["g9", "g", "t2", "g2i0", "g2k7", "g2k", "gk1"] {
            assert_eq!(framed(nothing), None, "{nothing}");
        }
    }

    const CALLED: &str = concat!(
        r#"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":["#,
        r#"{"name":"build","kind":"command","required":true},"#,
        r#"{"name":"ship","kind":"call","depends_on":["build"],"required":true,"call":{"alias":"release","target":"github_workflow","params":{"version":"build"}}},"#,
        r#"{"name":"note","kind":"command","depends_on":["ship"],"required":false}"#,
        r#"]}"#,
        "\n",
        r#"{"v":1,"kind":"call_dispatched","task":"ship","attempt":1,"alias":"release","call":"c-1","handle":{"kind":"github_workflow","repo":"acme/app","run_id":42}}"#,
        "\n",
        r#"{"v":1,"kind":"task_result","task":"note","status":"not_taken","attempts":0,"cost_usd":0.0,"secs":0.0,"note":""}"#,
        "\n",
    );

    fn workflow(lint: &str, legs: [&str; 2]) -> serde_json::Value {
        serde_json::json!({
            "nodes": [
                {"id": "lint", "label": "lint", "status": lint,
                 "link": {"kind": "external", "url": "https://github.com/acme/app/actions/runs/42/job/1"}},
                {"id": "t1", "label": "test (linux)", "status": legs[0], "group": "test"},
                {"id": "t2", "label": "test (mac)", "status": legs[1], "group": "test"}
            ],
            "edges": [{"from": "lint", "to": "t1"}, {"from": "lint", "to": "t2"}]
        })
    }

    fn shipping() -> Vec<LiveCall> {
        let view = serde_json::from_value(serde_json::json!({
            "task": "ship",
            "attempt": 1,
            "alias": "release",
            "kind": "github_workflow",
            "state": {
                "call": "c-1",
                "status": "running",
                "handle": {"kind": "github_workflow", "repo": "acme/app", "run_id": 42},
                "progress": workflow("pass", ["pass", "running"])
            }
        }))
        .unwrap();
        vec![LiveCall { view, launch: None }]
    }

    #[test]
    fn a_call_draws_its_target_in_its_frame_until_it_is_folded() {
        let tasks = fold(CALLED);
        let calls = shipping();
        let targets = Targets::new(&calls, None);
        let open = called(&tasks, None, &[], &targets, &[]);
        contains_all(
            &open,
            &[
                concat!(
                    "  subgraph g1 [\"ship \u{b7} release \u{b7} app\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\"]\n",
                    "    t1c0(\"\u{2713} lint\")\n",
                    "    t1c1([\"\u{25cf} test \u{d7}2 (1 ok)\"])\n",
                    "    t1c0 --> t1c1\n",
                    "  end\n",
                ),
                "t0 -->|version| g1\n",
                "g1 -.-> t2\n",
                "linkStyle 2 stroke:",
                "class t1c0 job_github_workflow_pass",
                "class t1c1 job_github_workflow_running",
            ],
        );
        let style = rune_mermaid::DiagramStyle::neutral(true);
        assert!(
            rune_mermaid::render_with_hover(&open, &style, rune_mermaid::Hover::Off, Color::WHITE)
                .is_some(),
            "{open}"
        );

        let legs = called(&tasks, None, &[], &targets, &["ship/test"]);
        contains_all(
            &legs,
            &[
                "t1c1(\"\u{2713} test (linux)\")",
                "t1c2(\"\u{25cf} test (mac)\")",
                "t1c0 --> t1c2",
                "linkStyle 3 stroke:",
            ],
        );

        let folded = called(&tasks, None, &[], &targets, &["ship"]);
        contains_all(
            &folded,
            &[
                "  subgraph g1 [\" \"]\n    t1[[\"\u{2197} ship \u{b7} release \u{b7} app\"]]\n  end\n",
                "t0 -->|version| t1\n",
                "linkStyle 1 stroke:",
            ],
        );
        assert!(!folded.contains("t1c"), "{folded}");
        assert_eq!(
            graph_frame(&tasks, "g1").map(|(_, t)| t.name.as_str()),
            Some("ship")
        );

        let picked = picking(&tasks, None, &[], &targets, &[], Some(("ship", "t2")));
        contains_all(&picked, &["  style t1c1 stroke:#ffffffff,stroke-width:2\n"]);
        let elsewhere = picking(&tasks, None, &[], &targets, &[], Some(("other", "t2")));
        assert!(!elsewhere.contains("stroke-width:2\n"), "{elsewhere}");
    }

    #[test]
    fn a_github_call_titles_its_frame_and_node_with_the_run_number() {
        let tasks = fold(CALLED);
        let mut calls = shipping();
        calls[0].view.state.run_number = Some(10);
        let targets = Targets::new(&calls, None);
        let open = called(&tasks, None, &[], &targets, &[]);
        contains_all(
            &open,
            &["  subgraph g1 [\"ship \u{b7} release \u{b7} app #10\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\"]\n"],
        );
        let folded = called(&tasks, None, &[], &targets, &["ship"]);
        contains_all(
            &folded,
            &["    t1[[\"\u{2197} ship \u{b7} release \u{b7} app #10\"]]\n"],
        );
        let unnumbered = called(&tasks, None, &[], &Targets::new(&shipping(), None), &[]);
        contains_all(
            &unnumbered,
            &["  subgraph g1 [\"ship \u{b7} release \u{b7} app\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\"]\n"],
        );
    }

    const GITLAB_CALLED: &str = concat!(
        r#"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":["#,
        r#"{"name":"build","kind":"command","required":true},"#,
        r#"{"name":"deploy","kind":"call","depends_on":["build"],"required":true,"call":{"alias":"ship","target":"gitlab_pipeline","params":{"target":"build"}}}"#,
        r#"]}"#,
        "\n",
        r#"{"v":1,"kind":"call_dispatched","task":"deploy","attempt":1,"alias":"ship","call":"c-1","handle":{"kind":"gitlab_pipeline","host":"gitlab.example.com","project":"group/sub/app","pipeline_id":4242}}"#,
        "\n",
    );

    #[test]
    fn a_gitlab_call_frames_its_jobs_in_gitlab_orange_under_the_project_and_pipeline_number() {
        let tasks = fold(GITLAB_CALLED);
        let view = serde_json::from_value(serde_json::json!({
            "task": "deploy",
            "attempt": 1,
            "alias": "ship",
            "kind": "gitlab_pipeline",
            "state": {
                "call": "c-1",
                "status": "running",
                "run_number": 17,
                "handle": {"kind": "gitlab_pipeline", "host": "gitlab.example.com", "project": "group/sub/app", "pipeline_id": 4242},
                "progress": workflow("pass", ["pass", "running"])
            }
        }))
        .unwrap();
        let calls = vec![LiveCall { view, launch: None }];
        let targets = Targets::new(&calls, None);
        let open = called(&tasks, None, &[], &targets, &[]);
        contains_all(
            &open,
            &[
                "  subgraph g1 [\"deploy \u{b7} ship \u{b7} app #17\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\u{2800}\"]\n",
                "class t1c0 job_gitlab_pipeline_pass",
                "class t1c1 job_gitlab_pipeline_running",
                "classDef job_gitlab_pipeline fill:",
            ],
        );
        let stroke = |class: &str| {
            open.lines()
                .find(|l| l.contains(&format!("classDef {class} fill:")))
                .and_then(|l| l.split("stroke:").nth(1))
                .map(|rest| rest.split(',').next().unwrap_or_default().to_owned())
        };
        assert!(stroke("job_gitlab_pipeline").is_some(), "{open}");
        assert_ne!(stroke("job_gitlab_pipeline"), stroke("job_github_workflow"));
        let style = rune_mermaid::DiagramStyle::neutral(true);
        assert!(
            rune_mermaid::render_with_hover(&open, &style, rune_mermaid::Hover::Off, Color::WHITE)
                .is_some(),
            "{open}"
        );
    }

    const MAPPED: &str = concat!(
        r#"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":["#,
        r#"{"name":"build","kind":"command","required":true},"#,
        r#"{"name":"triage","kind":"call","depends_on":["build"],"required":true,"over":"build.items","call":{"alias":"triage","target":"playbook"}}"#,
        r#"]}"#,
        "\n",
        r#"{"v":1,"kind":"call_dispatched","task":"triage","instance":"a","attempt":1,"alias":"triage","call":"c-a","handle":{"kind":"playbook","run":"run-a"}}"#,
        "\n",
        r#"{"v":1,"kind":"task_result","task":"triage[a]","status":"pass","attempts":1,"cost_usd":0.0,"secs":1.0,"note":""}"#,
        "\n",
        r#"{"v":1,"kind":"task_result","task":"triage[b]","status":"fail","attempts":1,"cost_usd":0.0,"secs":1.0,"note":""}"#,
        "\n",
    );

    #[test]
    fn a_mapped_call_draws_a_frame_per_instance_and_folds_with_the_fanout() {
        let tasks = fold(MAPPED);
        let view = serde_json::from_value(serde_json::json!({
            "task": "triage",
            "instance": "a",
            "attempt": 1,
            "alias": "triage",
            "kind": "playbook",
            "state": {
                "call": "c-a",
                "status": "passed",
                "handle": {"kind": "playbook", "run": "run-a"},
                "progress": {
                    "nodes": [
                        {"id": "scan", "label": "scan", "status": "pass"},
                        {"id": "fix", "label": "fix", "status": "pass"}
                    ],
                    "edges": [{"from": "scan", "to": "fix"}]
                }
            }
        }))
        .unwrap();
        let calls = vec![LiveCall { view, launch: None }];
        let targets = Targets::new(&calls, None);
        let open = called(&tasks, None, &[], &targets, &[]);
        contains_all(
            &open,
            &[
                concat!(
                    "    subgraph g1k0 [\" \"]\n",
                    "      t1i0[\"a\"]\n",
                    "    t1i0c0[\"scan\"]\n",
                    "    t1i0c1[\"fix\"]\n",
                    "    t1i0 ~~~ t1i0c0\n",
                    "    t1i0c0 --> t1i0c1\n",
                    "    end\n",
                    "    t1 ~~~ t1i0\n",
                ),
                "    subgraph g1k1 [\" \"]\n      t1i1[\"b\"]\n    end\n    t1 ~~~ t1i1\n",
                "class t1i0c1 pass",
                "class t1i1 fail",
            ],
        );
        let style = rune_mermaid::DiagramStyle::neutral(true);
        assert!(
            rune_mermaid::render_with_hover(&open, &style, rune_mermaid::Hover::Off, Color::WHITE)
                .is_some(),
            "{open}"
        );

        let one = called(&tasks, None, &[], &targets, &["triage[a]"]);
        assert!(one.contains("subgraph g1k0"), "{one}");
        assert!(!one.contains("t1i0c"), "{one}");

        let folded = called(&tasks, None, &["triage"], &targets, &[]);
        assert!(!folded.contains("g1k"), "{folded}");
        assert!(!folded.contains("t1i"), "{folded}");
    }

    #[test]
    fn a_call_with_no_target_graph_is_a_plain_node_and_a_lock_draws_one_pending() {
        use crucible_contract::calls::{
            CallAlias, CallsLock, GitRefName, GithubRepo, LOCK_FILE, LOCK_VERSION, LockedCall, Pin,
            TargetInterface, WorkflowPath,
        };

        let plan = CALLED.lines().next().unwrap();
        let tasks = fold(plan);
        let bare = source(&tasks, None, &[]);
        contains_all(
            &bare,
            &["  t1[[\"\u{2197} ship \u{b7} release \u{b7} github\"]]\n"],
        );
        assert!(!bare.contains("subgraph"), "{bare}");

        let pin = Pin::GithubRef {
            repo: GithubRepo::parse("acme/app").unwrap(),
            workflow: WorkflowPath::parse(".github/workflows/release.yml").unwrap(),
            git_ref: GitRefName::parse("main").unwrap(),
            commit: "abc123".into(),
        };
        let interface = TargetInterface {
            params: serde_json::json!({"type": "object"}),
            result: Vec::new(),
            result_files: Vec::new(),
            graph: serde_json::from_value(workflow("pass", ["fail", "pass"])).unwrap(),
        };
        let lock = CallsLock {
            version: LOCK_VERSION,
            calls: [(
                CallAlias::parse("release").unwrap(),
                LockedCall::new(pin, interface).unwrap(),
            )]
            .into(),
        };
        let pack = PackFiles::new(
            "test".into(),
            [(LOCK_FILE.to_string(), Some(lock.render().unwrap()))],
        );
        let locked = called(&tasks, None, &[], &Targets::new(&[], Some(&pack)), &[]);
        contains_all(
            &locked,
            &[
                "subgraph g1",
                "t1c0(\"\u{25cb} lint\")",
                "t1c1([\"\u{25cb} test \u{d7}2 (0 ok)\"])",
            ],
        );
        contains_all(
            &locked,
            &[
                "class t1c0 job_github_workflow\n",
                "class t1c1 job_github_workflow\n",
            ],
        );
    }
}
