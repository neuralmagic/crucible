//! The Graph pane: the run's tasks as a mermaid flowchart, shaped and labelled like the web
//! graph, where a node opens its task and an edge opens what flows along it.

use crucible_contract::session::TaskStatus;
use rune::{Color, El, col, text};

use crate::App;
use crate::session::{TaskState, TaskView};
use crate::ui::{Paint, Pane, empty_run, mono};

pub(crate) fn graph_view(pane: &Pane, app: &App, p: &Paint) -> El {
    if pane.tasks.is_empty() {
        return col()
            .key("graph-empty")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .child(empty_run(pane, p));
    }
    let source = mermaid_source(&pane.tasks, app.expanded.as_deref(), &Ink::of(p));
    let mut style = rune_mermaid::DiagramStyle::neutral(p.dark);
    style.font = mono(11.5, 400.0);
    style.max_width = 1160.0;
    style.surface = Color::TRANSPARENT;
    style.surface_border = Color::TRANSPARENT;
    style.node_fill = p.c.layer_02;
    style.node_border = p.c.border_base;
    style.line = p.c.border_strong;
    style.text = p.c.text_base;
    style.muted = p.c.text_muted;
    let decorate = |target: &rune_mermaid::Target, spot: El| {
        let (task, from, help) = match target {
            rune_mermaid::Target::Node(id) => {
                let Some(task) = graph_task(&pane.tasks, id) else {
                    return spot;
                };
                (task, None, task.name.clone())
            }
            rune_mermaid::Target::Edge { from, to } => {
                let (Some(from), Some(task)) =
                    (graph_task(&pane.tasks, from), graph_task(&pane.tasks, to))
                else {
                    return spot;
                };
                let help = format!("What {} hands {}", from.name, task.name);
                (task, Some(from.name.clone()), help)
            }
        };
        let name = task.name.clone();
        spot.cursor(rune::Cursor::Pointer)
            .help(help)
            .on_press(move |app: &mut App| app.pick_task(&name, from.as_deref()))
    };
    let card = rune_mermaid::render_with(
        &source,
        &style,
        rune_mermaid::Hover::Edges,
        style.text.alpha(0.9),
        &decorate,
    )
    .unwrap_or_else(|| {
        col().key("graph-fallback").w_full().padding(24.0).child(
            text("This run's graph did not read as a flowchart.".to_string())
                .font(p.k.ty.ui)
                .color(p.c.text_muted),
        )
    });
    col()
        .key("graph-scroll")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .items_center()
        .scroll(rune::ScrollSpec {
            stick_bottom: false,
        })
        .scroll_xy()
        .scroll_edge_fade(8.0)
        .child(card)
}

/// The task a [`mermaid_source`] node id names.
fn graph_task<'a>(tasks: &'a [TaskView], id: &str) -> Option<&'a TaskView> {
    tasks.get(id.strip_prefix('t')?.parse::<usize>().ok()?)
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

/// The run's tasks as a mermaid flowchart, task `i` as node `t<i>`.
///
/// ```text
///   route  {{◇ name}}   fan-out  [[≡ name ×n]]   agent  (✦ name)   command  [› name]
///   a --> b             b reads a
///   a ==>|each .f| b    b runs once per element of a.f
///   a -->|.k| b         b gets its own entry of a.k
///   a -->|yes / no| b   b runs on those answers of route a
///   a -.-> b            b is advisory or was not taken (faded when not taken)
/// ```
///
/// Node colour carries each task's state and the open task has an accent outline.
fn mermaid_source(tasks: &[TaskView], open: Option<&str>, ink: &Ink) -> String {
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
    for task in tasks {
        let mut label = format!("{} {}", glyph(task), clean(&task.name));
        if !task.instances.is_empty() {
            label.push_str(&format!(
                " \u{d7}{} ({} ok)",
                task.instances.len(),
                task.instance_counts().passed
            ));
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
        source.push_str(&format!("  {}{open}\"{label}\"{close}\n", id(&task.name)));
    }
    let mut faded = Vec::new();
    let mut edge = 0;
    for task in tasks {
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
            let arrow = if passed_over || !task.required {
                "-.->"
            } else if over.is_some() {
                "==>"
            } else {
                "-->"
            };
            let label = if labels.is_empty() {
                String::new()
            } else {
                format!("|{}|", labels.join(", "))
            };
            source.push_str(&format!(
                "  {} {arrow}{label} {}\n",
                id(dependency),
                id(&task.name)
            ));
            if passed_over {
                faded.push(edge);
            }
            edge += 1;
        }
    }
    if !faded.is_empty() {
        let list: Vec<String> = faded.iter().map(ToString::to_string).collect();
        source.push_str(&format!(
            "  linkStyle {} stroke:{}\n",
            list.join(","),
            ink.faded.hex()
        ));
    }
    for task in tasks {
        let class = match task.state() {
            TaskState::Pending => continue,
            TaskState::Running(_) => "running",
            TaskState::Settled(status) if status.passed() => "pass",
            TaskState::Settled(TaskStatus::Skipped | TaskStatus::NotTaken) => "skipped",
            TaskState::Settled(_) => "fail",
        };
        source.push_str(&format!("  class {} {class}\n", id(&task.name)));
    }
    if let Some(open) = open.filter(|open| tasks.iter().any(|task| task.name == *open)) {
        source.push_str(&format!(
            "  style {} stroke:{}\n",
            id(open),
            ink.accent.hex()
        ));
    }
    source
}

#[cfg(test)]
mod tests {
    use rune::Color;

    use crate::session::RunFold;
    use crate::ui::graph::{Ink, Swatch, graph_task, mermaid_source};

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
        }
    }

    fn tasks() -> Vec<crate::session::TaskView> {
        let mut fold = RunFold::default();
        for line in RUN.lines() {
            fold.apply_line(line);
        }
        fold.tasks
    }

    #[test]
    fn nodes_take_the_shape_and_glyph_of_what_runs_them() {
        let source = mermaid_source(&tasks(), None, &ink());
        for node in [
            "t0[\"\u{203a} scan\"]",
            "t1{{\"\u{25c7} pick\"}}",
            "t2[[\"\u{2261} fix\"]]",
            "t3(\"\u{2726} note\")",
            "t4[\"\u{203a} skip 'me'\"]",
        ] {
            assert!(source.contains(node), "{node} in\n{source}");
        }
    }

    #[test]
    fn edges_say_how_data_reaches_each_task() {
        let source = mermaid_source(&tasks(), None, &ink());
        for edge in [
            "t0 ==>|each .cves, .repo| t2",
            "t1 -->|yes / maybe| t2",
            "t2 -.-> t3",
            "t1 -.-> t4",
            "t0 --> t1",
            "linkStyle 4 stroke:",
        ] {
            assert!(source.contains(edge), "{edge} in\n{source}");
        }
    }

    #[test]
    fn the_source_draws_and_its_ids_lead_back_to_tasks() {
        let tasks = tasks();
        let source = mermaid_source(&tasks, Some("fix"), &ink());
        assert!(source.contains("style t2 stroke:"), "{source}");
        assert!(rune_mermaid::supported(&source), "{source}");
        assert_eq!(
            graph_task(&tasks, "t2").map(|t| t.name.as_str()),
            Some("fix")
        );
        assert!(graph_task(&tasks, "t9").is_none() && graph_task(&tasks, "xscan").is_none());
    }
}
