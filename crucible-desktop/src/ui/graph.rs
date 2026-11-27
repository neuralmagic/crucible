//! The Graph pane: the run's tasks as a mermaid flowchart, shaped and labelled like the web
//! graph, where a node opens its task and an edge opens what flows along it. The wheel or a
//! drag moves the flowchart, and a pinch or Cmd with the wheel zooms it.

use std::collections::HashMap;

use rune::{
    Color, Drag, El, MagnifyEvent, TextHighlight, Tx, WheelEvent, col, el, icon, row, text,
};
use rune_kit::{
    Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, SegmentedControl, TextInput,
};
use rune_theme_opencode::icons as oc;

use crate::camera;
use crate::session::TaskView;
use crate::ui::detail::empty_run;
use crate::ui::graph_call::{self, Targets};
use crate::ui::graph_find::{self, Found};
use crate::ui::graph_source::{Edit, Ink, Shown, edits, graph_frame, graph_node, mermaid_source};
use crate::ui::{Paint, Pane};
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
    let targets = Targets::new(pane.calls, pane.pack.as_ref().ok().copied());
    let shown = Shown {
        open: app.run.expanded.as_deref(),
        folded: &app.run.folded,
        marks: &marks,
        targets: &targets,
        call_folds: &app.run.call_folds,
        picked: app
            .run
            .call_node
            .as_ref()
            .map(|node| (node.frame.as_str(), node.ask.node.as_str())),
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
                None => graph_call::target_spot(spot, pane, &targets, app, id),
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
                Some((at, task)) if at.instance.is_some() || task.instances.is_empty() => {
                    graph_call::frame_spot(spot, at, task, &targets, app, p)
                }
                Some((_, task)) => {
                    spot.child(fold_handle(task, app.run.folded.contains(&task.name), p))
                }
                None => spot,
            },
        }
    };
    let model = rune_mermaid::prepare_drawing(
        &source,
        &style,
        rune_mermaid::Hover::Edges,
        style.text.alpha(0.9),
    );
    let body = match model {
        Some(model) => {
            let query = app.run.graph_find.text.trim();
            let found = graph_find::found(model.labels(), model.places(), query);
            let at = app.run.graph_found;
            let marks = |key: &str| {
                found
                    .iter()
                    .enumerate()
                    .filter(|(_, label)| label.key == key)
                    .flat_map(|(n, label)| {
                        label.ranges.iter().map(move |range| {
                            let mark =
                                TextHighlight::new(range.clone(), p.c.warning.icon.alpha(0.35));
                            if at == Some(n) {
                                mark.ring(p.c.warning.icon, 1.0)
                            } else {
                                mark
                            }
                        })
                    })
                    .collect()
            };
            let drawing = model.draw_with(marks, &decorate);
            canvas(drawing, &found, pane.tasks, app, p)
        }
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

const FIT: (f32, f32) = (0.2, 1.5);

const MARGIN: f32 = 16.0;

fn fit([across, down]: [f32; 2], [w, h]: [f32; 2]) -> (f32, f32) {
    let room = |pane: f32, length: f32| (pane - 2.0 * MARGIN).max(1.0) / length.max(1.0);
    let scale = room(across, w).min(room(down, h)).clamp(FIT.0, FIT.1);
    (scale, ((down - h * scale) / 2.0).max(MARGIN))
}

fn canvas(
    drawing: rune_mermaid::Drawing,
    found: &[Found],
    tasks: &[TaskView],
    app: &App,
    p: &Paint,
) -> El {
    let [w, h] = drawing.size;
    let pane = rune::memo::layout_size(CANVAS);
    let (fit, top) = pane.map_or((1.0, MARGIN), |pane| fit(pane, [w, h]));
    let half = pane.map_or(0.0, |[across, _]| across / 2.0);
    let middle = [0.0, pane.map_or(0.0, |[_, down]| down / 2.0 - top)];
    let matches = found
        .iter()
        .map(|label| [(label.middle[0] - w / 2.0) * fit, label.middle[1] * fit])
        .collect();
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
                camera.zoom_by((e.dy / 100.0).exp(), [e.x - half, e.y - top]);
            } else {
                camera.pan_by([e.dx, e.dy]);
            }
            tx.live_only();
            true
        })
        .on_magnify(move |app: &mut App, e: MagnifyEvent, tx: &mut Tx| {
            app.run
                .camera
                .zoom_by(1.0 + e.delta, [e.x - half, e.y - top]);
            tx.live_only();
            true
        })
        .on_drag(|app: &mut App, drag: &mut Drag, tx: &mut Tx| {
            app.run.camera.drag(drag);
            tx.live_only();
        })
        .child(el().key("graph-top").h(top).w(1.0))
        .child(world)
        .child(controls(tasks, app, middle, matches, p))
}

/// How wide the find field is.
const FIND: f32 = 150.0;

const FIND_FIELD: &str = "graph-find";

/// Puts the find field away and its text with it.
fn close_find(app: &mut App) {
    app.run.graph_finding = false;
    app.run.graph_find = rune::TextEdit::default();
    app.run.graph_found = None;
}

/// The find field while it is open and the button that opens it, then zoom out, zoom in and the
/// first view again, then every fan-out folded or shown. `matches` are the labels the find
/// field's text is in, as the camera's points.
fn controls(
    tasks: &[TaskView],
    app: &App,
    middle: [f32; 2],
    matches: Vec<[f32; 2]>,
    p: &Paint,
) -> El {
    let find = app.run.graph_finding.then(|| {
        let mut find = TextInput::new(&app.run.graph_find)
            .placeholder("Find")
            .width(Some(FIND))
            .on_edit(|app: &mut App| &mut app.run.graph_find)
            .on_change(|app: &mut App, _: &mut Tx| app.run.graph_found = None)
            .on_key(|app: &mut App, key: &rune::KeyEvent, _: &mut Tx| {
                let closes = key.key == rune::Key::Escape;
                if closes {
                    close_find(app);
                }
                closes
            });
        if !app.run.graph_find.text.trim().is_empty() {
            let count = match app.run.graph_found.filter(|at| *at < matches.len()) {
                Some(at) => format!("{}/{}", at + 1, matches.len()),
                None => matches.len().to_string(),
            };
            find = find.trailing(text(count).font(p.mono(11.0, 400.0)).color(p.c.text_muted));
        }
        find.on_submit(move |app: &mut App, _: &mut Tx| {
            app.run.graph_found = graph_find::next(app.run.graph_found, matches.len());
            if let Some(shown) = app.run.graph_found.and_then(|at| matches.get(at)) {
                app.run.camera.show_at(*shown, middle);
            }
        })
        .build(p.k)
        .key(FIND_FIELD)
    });
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
        .children(find)
        .child(
            button(&oc::MAGNIFYING_GLASS, "Find")
                .on_press_tx(|app: &mut App, tx: &mut Tx| {
                    if app.run.graph_finding {
                        close_find(app);
                    } else {
                        app.run.graph_finding = true;
                        tx.focus_when_shown(FIND_FIELD, false);
                    }
                })
                .build(p.k)
                .key("graph-find-toggle"),
        )
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

pub(crate) const HANDLE: f32 = 16.0;

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

/// A spot that opens instance `key` of `task`.
fn instance_spot(spot: El, task: &TaskView, key: &str) -> El {
    let (name, key) = (task.name.clone(), key.to_string());
    spot.cursor(rune::Cursor::Pointer)
        .help(format!("{name}[{key}]"))
        .on_press(move |app: &mut App| app.pick_instance(&name, &key))
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

#[cfg(test)]
mod tests {
    use crate::ui::graph::{FIT, MARGIN, fit};

    #[test]
    fn the_graph_fits_whole_in_its_pane_and_sits_in_the_middle() {
        assert_eq!(fit([1000.0, 800.0], [500.0, 200.0]), (FIT.1, 250.0));
        let (scale, top) = fit([1000.0, 500.0], [800.0, 1000.0]);
        assert!((scale - 0.468).abs() < 1e-4, "{scale}");
        assert!((top - MARGIN).abs() < 1e-3, "{top}");
        let (scale, top) = fit([400.0, 300.0], [2000.0, 100.0]);
        assert_eq!(scale, FIT.0);
        assert!((top - 140.0).abs() < 1e-3, "{top}");
    }
}
