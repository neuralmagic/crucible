//! The Graph pane: the run's tasks as a mermaid flowchart, shaped and labelled like the web
//! graph, where a node opens its task and an edge opens what flows along it. The wheel or a
//! drag moves the flowchart, and a pinch or Cmd with the wheel zooms it.

use std::collections::{HashMap, HashSet};

use crucible_contract::decision::Answer;
use crucible_contract::session::TaskStatus;
use rune::{Color, Drag, El, MagnifyEvent, Tx, WheelEvent, col, el, icon, row, text};
use rune_kit::{
    Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, SegmentedControl,
};
use rune_theme_opencode::icons as oc;

use crate::camera;
use crate::session::{Instance, TaskState, TaskView};
use crate::ui::{Paint, Pane, empty_run};
use crate::{App, GraphSource};

pub(crate) fn graph_view(pane: &Pane, app: &App, p: &Paint) -> El {
    let head = preview_head(pane, p);
    if pane.tasks.is_empty() {
        return col()
            .key("graph-empty")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .children(head)
            .child(empty_run(pane, p));
    }
    let marks = if pane.previewing && !pane.run_tasks.is_empty() {
        edits(pane.run_tasks, pane.tasks)
    } else {
        HashMap::new()
    };
    let shown = Shown {
        open: app.run.expanded.as_deref(),
        folded: &app.run.folded,
        marks: &marks,
    };
    let source = mermaid_source(pane.tasks, &shown, &Ink::of(p));
    let mut style = rune_mermaid::DiagramStyle::neutral(p.dark);
    style.font = p.mono(11.5, 400.0);
    style.max_width = f32::MAX;
    if compact(app) {
        style.node_pad = [14.0, 6.0];
        style.flow_gaps = [14.0, 20.0, 8.0];
    } else {
        style.node_pad = [18.0, 10.0];
    }
    style.surface = Color::TRANSPARENT;
    style.surface_border = Color::TRANSPARENT;
    style.node_fill = p.c.layer_02;
    style.node_border = p.c.border_base;
    style.line = p.c.border_strong;
    style.text = p.c.text_base;
    style.muted = p.c.text_muted;
    let decorate = |target: &rune_mermaid::Target, spot: El| {
        let pointer = |spot: El, help: String| spot.cursor(rune::Cursor::Pointer).help(help);
        match target {
            rune_mermaid::Target::Node(id) => match graph_node(pane.tasks, id) {
                Some((task, Some(key))) => instance_spot(spot, task, key),
                Some((task, None)) => {
                    let name = task.name.clone();
                    pointer(spot, name.clone())
                        .on_press(move |app: &mut App| app.pick_task(&name, None))
                }
                None => spot,
            },
            rune_mermaid::Target::Edge { from, to } => {
                let (Some((from, _)), Some((task, key))) =
                    (graph_node(pane.tasks, from), graph_node(pane.tasks, to))
                else {
                    return spot;
                };
                if from.name == task.name {
                    return spot;
                }
                if let Some(key) = key {
                    return instance_spot(spot, task, key);
                }
                let (name, by) = (task.name.clone(), from.name.clone());
                if task.depends_on.contains(&from.name) {
                    pointer(spot, format!("What {by} hands {name}"))
                        .on_press(move |app: &mut App| app.pick_task(&name, Some(&by)))
                } else if from.revise.contains(&task.name) {
                    pointer(spot, format!("{name}'s rounds, each sent back by {by}"))
                        .on_press(move |app: &mut App| app.pick_task(&name, None))
                } else {
                    spot
                }
            }
            rune_mermaid::Target::Frame(id) => match graph_frame(pane.tasks, id) {
                Some(task) => spot.child(fold_handle(task, app.run.folded.contains(&task.name), p)),
                None => spot,
            },
        }
    };
    let body = match rune_mermaid::render_drawing_with(
        &source,
        &style,
        rune_mermaid::Hover::Edges,
        style.text.alpha(0.9),
        &decorate,
    ) {
        Some(drawing) => canvas(drawing, pane.tasks, app, p),
        None => col().key("graph-fallback").w_full().padding(24.0).child(
            text("This run's graph did not read as a flowchart.".to_string())
                .font(p.k.ty.ui)
                .color(p.c.text_muted),
        ),
    };
    col()
        .key("graph-pane")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(8.0)
        .children(head)
        .child(body)
}

/// Whether the run on screen draws compact: as it was switched, else as the setting says.
fn compact(app: &App) -> bool {
    app.run.compact_graph.unwrap_or(app.settings.compact_graph)
}

const CANVAS: &str = "graph-canvas";

/// The least a diagram wider than the pane is first drawn at.
const FIT: f32 = 0.6;

/// The diagram under the run's camera, top and centre in the pane and shrunk towards its width,
/// with the camera's controls over it.
fn canvas(drawing: rune_mermaid::Drawing, tasks: &[TaskView], app: &App, p: &Paint) -> El {
    let [w, h] = drawing.size;
    let pane = rune::memo::layout_size(CANVAS);
    let fit = pane.map_or(1.0, |[across, _]| (across / w.max(1.0)).clamp(FIT, 1.0));
    let half = pane.map_or(0.0, |[across, _]| across / 2.0);
    let middle = [0.0, pane.map_or(0.0, |[_, down]| down / 2.0)];
    let camera = &app.run.camera;
    let fitted = el()
        .key("graph-fit")
        .size(w, h)
        .origin(0.5, 0.0)
        .scale(fit)
        .instant_changes()
        .child(drawing.el);
    let world = el()
        .key("graph-world")
        .size(w, h)
        .origin(0.5, 0.0)
        .live_offset(camera.pan())
        .live_scale(camera.zoom())
        .child(fitted);
    col()
        .key(CANVAS)
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .items_center()
        .clip_soft(0.0)
        .on_wheel(move |app: &mut App, e: WheelEvent, tx: &mut Tx| {
            let camera = &mut app.run.camera;
            if e.mods.cmd || e.mods.ctrl {
                camera.zoom_by((e.dy / 100.0).exp(), [e.x - half, e.y]);
            } else {
                camera.pan_by([e.dx, e.dy]);
            }
            tx.live_only();
            true
        })
        .on_magnify(move |app: &mut App, e: MagnifyEvent, tx: &mut Tx| {
            app.run.camera.zoom_by(1.0 + e.delta, [e.x - half, e.y]);
            tx.live_only();
            true
        })
        .on_drag(|app: &mut App, drag: &mut Drag, tx: &mut Tx| {
            app.run.camera.drag(drag);
            tx.live_only();
        })
        .child(world)
        .child(controls(tasks, app, middle, p))
}

/// Zoom out, zoom in and the first view again, then every fan-out folded or shown.
fn controls(tasks: &[TaskView], app: &App, middle: [f32; 2], p: &Paint) -> El {
    let button = |icon: &'static rune::Icon, says: &str| {
        IconButton::new(icon)
            .variant(IconButtonVariant::GhostMuted)
            .label(says)
            .help(says)
    };
    let fanouts: Vec<String> = tasks
        .iter()
        .filter(|task| !task.instances.is_empty())
        .map(|task| task.name.clone())
        .collect();
    let all_folded = fanouts.iter().all(|name| app.run.folded.contains(name));
    let density = if compact(app) {
        "Roomy graph"
    } else {
        "Compact graph"
    };
    let fold = (!fanouts.is_empty()).then(|| {
        let (icon, says) = if all_folded {
            (&oc::EXPAND, "Show every fan-out's instances")
        } else {
            (&oc::COLLAPSE, "Fold every fan-out")
        };
        button(icon, says)
            .on_press(move |app: &mut App| {
                for name in &fanouts {
                    if all_folded {
                        app.run.folded.remove(name);
                    } else {
                        app.run.folded.insert(name.clone());
                    }
                }
            })
            .build(p.k)
            .key("graph-fold-all")
    });
    row()
        .key("graph-controls")
        .pin(None, Some(8.0), Some(8.0), None)
        .padding(2.0)
        .center_y()
        .radius(8.0)
        .fill(p.c.layer_02)
        .border(0.5, p.c.border_base)
        .child(
            button(&oc::OUTLINE_MAGNIFIER_MINUS, "Zoom out")
                .on_press(move |app: &mut App| app.run.camera.zoom_by(1.0 / camera::STEP, middle))
                .build(p.k)
                .key("graph-zoom-out"),
        )
        .child(
            button(&oc::OUTLINE_MAGNIFIER_PLUS, "Zoom in")
                .on_press(move |app: &mut App| app.run.camera.zoom_by(camera::STEP, middle))
                .build(p.k)
                .key("graph-zoom-in"),
        )
        .child(
            button(&oc::RESET, "Reset view")
                .on_press(|app: &mut App| app.run.camera.reset())
                .build(p.k)
                .key("graph-reset"),
        )
        .children(fold)
        .child(
            button(&oc::DOT_GRID, density)
                .on_press(|app: &mut App| app.run.compact_graph = Some(!compact(app)))
                .build(p.k)
                .key("graph-density"),
        )
}

/// The side of the square at the corner of a fan-out's frame that folds and unfolds it.
const HANDLE: f32 = 16.0;

/// The corner of a fan-out's frame: a press folds its instances away or shows them.
fn fold_handle(task: &TaskView, folded: bool, p: &Paint) -> El {
    let name = task.name.clone();
    let (glyph, help) = if folded {
        (&oc::CHEVRON_RIGHT, format!("Show {name}'s instances"))
    } else {
        (&oc::CHEVRON_DOWN, format!("Fold {name}'s instances"))
    };
    el().key(format!("graph-fold-{name}"))
        .absolute(0.0, 0.0)
        .size(HANDLE, HANDLE)
        .center()
        .cursor(rune::Cursor::Pointer)
        .help(help)
        .on_press(move |app: &mut App| app.toggle_folded(&name))
        .child(icon(glyph, 10.0).color(p.c.icon_faint))
}

/// The fan-out a [`mermaid_source`] subgraph id, `g<i>`, frames.
fn graph_frame<'a>(tasks: &'a [TaskView], id: &str) -> Option<&'a TaskView> {
    tasks.get(id.strip_prefix('g')?.parse::<usize>().ok()?)
}

/// A spot that opens instance `key` of `task`.
fn instance_spot(spot: El, task: &TaskView, key: &str) -> El {
    let (name, key) = (task.name.clone(), key.to_string());
    spot.cursor(rune::Cursor::Pointer)
        .help(format!("{name}[{key}]"))
        .on_press(move |app: &mut App| app.pick_instance(&name, &key))
}

/// How a task in the compiled pack differs from the run's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edit {
    Added,
    Changed,
}

/// What a task is, apart from how it ran.
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

/// The compiled pack's tasks that are new or defined differently from the run's.
fn edits(run: &[TaskView], compiled: &[TaskView]) -> HashMap<String, Edit> {
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

/// Above the graph while there is a compiled pack: which to show, what the edit changed, and
/// why it does not compile.
fn preview_head(pane: &Pane, p: &Paint) -> Option<El> {
    let preview = pane.preview.as_ref()?;
    let has_run =
        pane.previewing && !pane.run_tasks.is_empty() || !pane.previewing && !pane.tasks.is_empty();
    let mut head = col().key("preview-head").w_full().gap(6.0);
    let mut bar = row().w_full().gap(10.0).center_y();
    if has_run {
        const SOURCES: [GraphSource; 2] = [GraphSource::Run, GraphSource::Preview];
        bar = bar.child(
            SegmentedControl::new(&["Run", "Working copy"], Some(usize::from(pane.previewing)))
                .label("Graph source")
                .width(Some(250.0))
                .on_pick(|app: &mut App, i: usize, _: &mut Tx| {
                    app.remote.graph_source = Some(SOURCES[i])
                })
                .build(p.k)
                .key("graph-source"),
        );
    }
    if pane.previewing && !pane.run_tasks.is_empty() {
        let marks = edits(pane.run_tasks, pane.tasks);
        let added = marks.values().filter(|e| **e == Edit::Added).count();
        let changed = marks.len() - added;
        let removed: Vec<&str> = pane
            .run_tasks
            .iter()
            .filter(|r| !pane.tasks.iter().any(|t| t.name == r.name))
            .map(|r| r.name.as_str())
            .collect();
        let mut said = vec![format!("+{added} new"), format!("~{changed} changed")];
        if !removed.is_empty() {
            said.push(format!(
                "\u{2212}{} removed: {}",
                removed.len(),
                removed.join(", ")
            ));
        }
        bar = bar.child(
            text(said.join(" \u{b7} "))
                .font(p.mono(10.5, 400.0))
                .color(p.c.text_muted)
                .grow(1.0)
                .min_w(0.0)
                .truncate(),
        );
    }
    head = head.child(bar);
    for (i, problem) in preview.problems.iter().enumerate() {
        let mut line = row()
            .key(format!("preview-problem-{i}"))
            .w_full()
            .gap(8.0)
            .child(
                text(problem.message.clone())
                    .font(p.mono(10.5, 400.0))
                    .color(p.fail)
                    .grow(1.0)
                    .min_w(0.0)
                    .max_lines(3)
                    .wrap()
                    .selectable(true),
            );
        if let Some((path, at)) = problem.at.clone() {
            line = line.child(
                Button::new("Open")
                    .variant(ButtonVariant::Neutral)
                    .control_size(ControlSize::Small)
                    .on_press(move |app: &mut App| app.open_path_in_editor(&path, at as usize))
                    .build(p.k),
            );
        }
        head = head.child(line);
    }
    Some(head)
}

/// What a [`mermaid_source`] node id names: task `t<i>`, instance `t<i>i<k>` of it, or
/// `t<i>more`, which stands for the task.
fn graph_node<'a>(tasks: &'a [TaskView], id: &str) -> Option<(&'a TaskView, Option<&'a str>)> {
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

/// The colours the source names.
struct Ink {
    pass: Swatch,
    fail: Swatch,
    running: Swatch,
    skipped: Swatch,
    /// The open task's outline.
    accent: Color,
    /// Edges into a task the run passed over.
    faded: Color,
    /// Edges on the critical path.
    critical: Color,
    /// Outlines of tasks a preview adds and redefines.
    added: Color,
    changed: Color,
    /// Edges from a reviser back to the tasks it revises.
    revise: Color,
}

impl Ink {
    fn of(p: &Paint) -> Ink {
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
            accent: p.c.text_accent,
            faded: p.c.border_muted,
            critical: p.c.text_muted,
            added: p.pass,
            changed: p.c.warning.icon,
            revise: p.c.text_accent,
        }
    }
}

/// What runs a task, as the web graph marks it.
fn glyph(task: &TaskView) -> &'static str {
    if task.kind == "route" {
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

/// Text safe inside a quoted node label or a `|…|` edge label.
fn clean(text: &str) -> String {
    text.replace(['"', '|'], "'")
}

/// When a task settled: its own time, else its last instance's.
fn finished(task: &TaskView) -> Option<jiff::Timestamp> {
    task.result.as_ref().and_then(|r| r.at).or_else(|| {
        task.instances
            .iter()
            .filter_map(|i| i.result.as_ref().and_then(|r| r.at))
            .max()
    })
}

/// The chain of dependency edges, as `(from, to)` task indexes, that held the run up: back from
/// the task that settled last, each step to the dependency that settled last. Empty when the
/// run recorded no settle times.
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

/// For each task, by index, every task it depends on through one or more others.
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

/// The most instances the graph draws under one fan-out.
const UNFOLDED: usize = 24;

/// The nodes, edges and subgraphs rune-mermaid reads before it gives a source up.
const ITEMS: usize = 400;

/// How many instances a fan-out draws side by side.
const ROW: usize = 6;

/// How many instances each unfolded fan-out can draw with the source still inside [`ITEMS`]:
/// an instance is a node, the edge that places it, and at most an edge from each dependency.
fn instances_each(tasks: &[TaskView], unfolded: impl Fn(&TaskView) -> bool) -> usize {
    let edges: usize = tasks
        .iter()
        .map(|task| 2 * task.depends_on.len() + task.revise.len())
        .sum();
    let framed = tasks.iter().filter(|task| !task.instances.is_empty());
    let taken = tasks.len() + edges + 3 * framed.count();
    let per: usize = tasks
        .iter()
        .filter(|task| unfolded(task))
        .map(|task| 2 + task.depends_on.len())
        .sum();
    (ITEMS.saturating_sub(taken) / per.max(1)).min(UNFOLDED)
}

/// The labels a route's output records for `question`.
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

/// A route instance's first answer.
fn first_answer(instance: &Instance) -> Option<String> {
    let output = instance.result.as_ref()?.output.as_ref()?.as_object()?;
    output
        .keys()
        .find_map(|question| answer(instance, question))
}

/// What the graph shows beyond the tasks themselves.
struct Shown<'a> {
    /// The open task, outlined.
    open: Option<&'a str>,
    /// Fan-outs drawn without their instances.
    folded: &'a HashSet<String>,
    /// Tasks the compiled pack adds or redefines, outlined.
    marks: &'a HashMap<String, Edit>,
}

/// The run's tasks as a mermaid flowchart, task `i` as node `t<i>`; a fan-out that ran is
/// subgraph `g<i>` holding its node and, unless it is folded, its instances `t<i>i<k>` in rows
/// under it.
///
/// ```text
///   route  {{◇ name}}   fan-out  [[≡ name ×n]]   agent  (✦ name)   command  [› name]
///   a --> b             b reads a, and no other dependency of b already leads back to a
///   a ==>|each .f| b    b runs once per element of a.f
///   a -->|.k| b         b gets its own entry of a.k
///   a -->|yes / no| b   b runs on those answers of route a
///   a -.-> b            b was not taken, faded
///   a[k] -->|yes| b[k]  a and b spread over the same list and each fits one row, so b's
///                       instance reads a's
///   r -.->|revise 2/3| a  r failing sends a back for another round, of at most 3
/// ```
///
/// Node colour carries each task's state, the open task has an accent outline, and the edges
/// that held the run up are drawn heavier.
fn mermaid_source<'a>(tasks: &'a [TaskView], shown: &Shown, ink: &Ink) -> String {
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
    let id = |name: &str| match tasks.iter().position(|task| task.name == name) {
        Some(i) => format!("t{i}"),
        None => {
            let cleaned: String = name
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            format!("x{cleaned}")
        }
    };
    let unfolded =
        |task: &TaskView| !task.instances.is_empty() && !shown.folded.contains(&task.name);
    let each = instances_each(tasks, unfolded);
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
    let mut classes: Vec<(String, &'static str)> = Vec::new();
    let mut edge = 0;
    for (i, task) in tasks.iter().enumerate() {
        let mut label = format!("{} {}", glyph(task), clean(&task.name));
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
        } else if task.is_fanout() {
            ("[[", "]]")
        } else if task.kind == "agent" {
            ("(", ")")
        } else {
            ("[", "]")
        };
        let node = format!("t{i}{open}\"{label}\"{close}");
        if task.instances.is_empty() {
            source.push_str(&format!("  {node}\n"));
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
            source.push_str(&format!(
                "    t{i}i{k}[\"{label}\"]\n    {} ~~~ t{i}i{k}\n",
                anchor(k)
            ));
            edge += 1;
            if let Some(class) = state_class(instance.state()) {
                classes.push((format!("t{i}i{k}"), class));
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
            source.push_str(&format!("  {source_id} {arrow}{label} t{to}\n"));
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
        if let Some(class) = state_class(task.state()) {
            classes.push((format!("t{i}"), class));
        }
    }
    for (node, class) in classes {
        source.push_str(&format!("  class {node} {class}\n"));
    }
    for (i, task) in tasks.iter().enumerate() {
        let ink = match shown.marks.get(&task.name) {
            Some(Edit::Added) => ink.added,
            Some(Edit::Changed) => ink.changed,
            None => continue,
        };
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

    use crate::session::{RunFold, TaskView};
    use crate::ui::graph::{
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

    /// `a` feeds `b` and the fan-out `c`, both feed `d`; `c` settles after `b`.
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
        let folded: HashSet<String> = folded.iter().map(ToString::to_string).collect();
        let source = mermaid_source(
            tasks,
            &Shown {
                open,
                folded: &folded,
                marks: &std::collections::HashMap::new(),
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

    /// `scan` lists, route `triage` and `fix` both spread over the list, and `fix` runs where
    /// `triage` said to.
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

    /// `a` feeds `b`, `b` feeds `c`, and `d` names all three, `a` for its own entry of `a.k`.
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
        let framed = |id: &str| graph_frame(&tasks, id).map(|task| task.name.as_str());
        assert_eq!(framed("g2"), Some("c"));
        for nothing in ["g9", "g", "t2", "g2i0"] {
            assert_eq!(framed(nothing), None, "{nothing}");
        }
    }
}
