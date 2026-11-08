//! The Data pane: what reaches the selected task from each dependency, what the pack says it
//! emits, and what it emitted.

use rune::{El, ScrollSpec, Tx, col, icon, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize};
use rune_theme_opencode::icons as oc;
use serde_json::Value;

use crate::App;
use crate::inspect::{self, Check, Flow, Inspect, Via};
use crate::session::TaskView;
use crate::ui::tasks::{label_ink, section, state_label, state_mark};
use crate::ui::{Paint, Pane, mono, tag};

/// Rows one JSON tree draws before it stops.
const TREE_ROWS: usize = 1500;
/// Entries one object or list shows before it counts the rest.
const TREE_ENTRIES: usize = 200;
const INSTANCE_CHIPS: usize = 48;
const INDENT: f32 = 14.0;

pub(crate) fn data_view(pane: &Pane, app: &App, p: &Paint) -> El {
    let task = app
        .expanded
        .as_deref()
        .and_then(|name| pane.tasks.iter().find(|task| task.name == name));
    let Some(task) = task else {
        return col().key("data-none").w_full().grow(1.0).center().child(
            text("No task selected".to_string())
                .font(p.k.ty.ui)
                .color(p.c.text_muted),
        );
    };
    let mut body = col()
        .key(format!("data-{}", task.name))
        .w_full()
        .gap(12.0)
        .padding(14.0)
        .child(header(task, p));
    let flows = inspect::flows(&pane.tasks, task);
    if !flows.is_empty() {
        let mut list = col().w_full().gap(8.0);
        for flow in &flows {
            list = list.child(flow_card(flow, &app.inspect, p));
        }
        body = body.child(section("Inputs", p)).child(list);
    }
    let output = inspect::output(task, app.inspect.instance.as_deref());
    if !task.emits.is_empty() || !task.files.is_empty() {
        body = body
            .child(section("Declared", p))
            .child(declared(task, output, &app.inspect, p));
    }
    body = body.child(section("Output", p));
    if !task.instances.is_empty() {
        body = body.child(instance_chips(task, app.inspect.instance.as_deref(), p));
    }
    body = body.child(match output {
        Some(value) => col()
            .w_full()
            .gap(6.0)
            .child(json_box(
                tree(value, &format!("out:{}", task.name), &app.inspect, p),
                p,
            ))
            .child(row().child(copy_button("Copy JSON", value, "data-copy-output", p))),
        None => faint("No output yet", p),
    });
    col()
        .key("data-scroll")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_edge_fade(8.0)
        .child(body)
}

fn header(task: &TaskView, p: &Paint) -> El {
    let state = task.state();
    row()
        .w_full()
        .gap(8.0)
        .center_y()
        .child(state_mark(state, p))
        .child(
            text(task.name.clone())
                .font(mono(13.0, 600.0))
                .color(p.c.text_base)
                .min_w(0.0)
                .truncate(),
        )
        .child(tag(&task.kind, p))
        .child(
            text(state_label(state))
                .font(mono(10.5, 400.0))
                .color(label_ink(state, p)),
        )
}

fn flow_card(flow: &Flow, inspect: &Inspect, p: &Paint) -> El {
    let picked = inspect.from.as_deref() == Some(flow.from);
    let (via, path) = match &flow.via {
        Via::Whole => ("whole output".to_string(), "all".to_string()),
        Via::Over(field) => (format!("over .{field}"), format!("over.{field}")),
        Via::Keyed(field) => (format!("keyed .{field}"), format!("keyed.{field}")),
    };
    let head =
        row()
            .w_full()
            .gap(8.0)
            .center_y()
            .child(
                text(flow.from.to_string())
                    .font(mono(11.0, 600.0))
                    .color(p.c.text_base),
            )
            .child(icon(&oc::CHEVRON_RIGHT, 12.0).color(p.c.icon_faint))
            .child(
                text(via)
                    .font(mono(10.5, 400.0))
                    .color(p.c.text_muted)
                    .grow(1.0)
                    .min_w(0.0)
                    .truncate(),
            )
            .children(flow.value.map(|value| {
                copy_button("Copy", value, &format!("copy-in-{}-{path}", flow.from), p)
            }));
    let body = match flow.value {
        Some(value) => tree(value, &format!("in:{}:{path}", flow.from), inspect, p),
        None => faint("Not settled", p),
    };
    col()
        .key(format!("data-in-{}-{path}", flow.from))
        .w_full()
        .gap(6.0)
        .padding(10.0)
        .radius(p.k.radius.md)
        .fill(p.c.bg_base)
        .border(
            if picked { 1.0 } else { p.hair },
            if picked {
                p.c.text_accent
            } else {
                p.c.border_muted
            },
        )
        .child(head)
        .child(body)
}

fn declared(task: &TaskView, output: Option<&Value>, inspect: &Inspect, p: &Paint) -> El {
    let mut list = col().w_full().gap(3.0);
    for emit in &task.emits {
        let ty = emit
            .ty
            .as_ref()
            .map_or("any".to_string(), ToString::to_string);
        let (mark, ink) = match inspect::check(emit, output) {
            Check::Waiting => (String::new(), p.c.text_faint),
            Check::Missing => ("missing".to_string(), p.fail),
            Check::Fits => ("\u{2713}".to_string(), p.pass),
            Check::Refused(held) => (format!("holds {held}"), p.fail),
        };
        list = list.child(
            row()
                .key(format!("emit-{}", emit.field))
                .w_full()
                .gap(8.0)
                .child(
                    text(emit.field.clone())
                        .font(mono(10.5, 500.0))
                        .color(p.c.text_base)
                        .w(160.0)
                        .truncate(),
                )
                .child(
                    text(ty)
                        .font(mono(10.5, 400.0))
                        .color(p.c.text_muted)
                        .grow(1.0)
                        .min_w(0.0)
                        .truncate(),
                )
                .child(text(mark).font(mono(10.5, 500.0)).color(ink)),
        );
    }
    for file in &task.files {
        let schema = file.schema.as_ref();
        list = list.child(
            row()
                .key(format!("file-{}", file.path))
                .w_full()
                .gap(8.0)
                .child(
                    text(file.path.clone())
                        .font(mono(10.5, 500.0))
                        .color(p.c.text_base)
                        .w(160.0)
                        .truncate(),
                )
                .child(
                    text(schema.map_or("file".to_string(), ToString::to_string))
                        .font(mono(10.5, 400.0))
                        .color(p.c.text_muted)
                        .grow(1.0)
                        .min_w(0.0)
                        .truncate(),
                ),
        );
        if let Some(document) = schema.and_then(inspect::document) {
            let path = format!("schema:{}:{}", task.name, file.path);
            list = list.child(json_box(tree(&document, &path, inspect, p), p));
        }
    }
    list
}

fn instance_chips(task: &TaskView, picked: Option<&str>, p: &Paint) -> El {
    let shown = inspect::output(task, picked);
    let mut chips = row().w_full().gap(4.0).wrap();
    for instance in task.instances.iter().take(INSTANCE_CHIPS) {
        let on = instance
            .result
            .as_ref()
            .and_then(|r| r.output.as_ref())
            .is_some_and(|out| shown.is_some_and(|s| std::ptr::eq(s, out)));
        let key = instance.key.clone();
        chips = chips.child(
            row()
                .key(format!("instance-{key}"))
                .role("button")
                .label(format!("Show {}[{key}]", task.name))
                .h(20.0)
                .px(7.0)
                .center_y()
                .radius(p.k.radius.sm)
                .fill(if on { p.c.bg_accent } else { p.c.layer_02 })
                .border(p.hair, p.c.border_muted)
                .hover(p.c.hover)
                .focusable(true)
                .on_press(move |app: &mut App| app.inspect.instance = Some(key.clone()))
                .child(
                    text(instance.key.clone())
                        .font(mono(10.0, 500.0))
                        .color(if on {
                            p.c.text_base
                        } else {
                            label_ink(instance.state(), p)
                        }),
                ),
        );
    }
    chips
}

fn json_box(tree: El, p: &Paint) -> El {
    col()
        .w_full()
        .max_h(420.0)
        .padding(8.0)
        .radius(p.k.radius.md)
        .fill(p.c.bg_base)
        .border(p.hair, p.c.border_muted)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_xy()
        .child(tree)
}

fn copy_button(label: &str, value: &Value, key: &str, p: &Paint) -> El {
    let pretty = serde_json::to_string_pretty(value).unwrap_or_default();
    Button::new(label)
        .variant(ButtonVariant::Neutral)
        .control_size(ControlSize::Small)
        .on_press_tx(move |_: &mut App, tx: &mut Tx| tx.copy(pretty.clone()))
        .build(p.k)
        .key(key.to_string())
}

fn faint(label: &str, p: &Paint) -> El {
    text(label.to_string())
        .font(mono(10.5, 400.0))
        .color(p.c.text_faint)
}

/// `value` as a tree of rows, objects and lists folding open or shut at a click.
fn tree(value: &Value, root: &str, inspect: &Inspect, p: &Paint) -> El {
    let mut walk = Walk {
        inspect,
        p,
        budget: TREE_ROWS,
        rows: Vec::new(),
    };
    walk.value(None, value, root, 0);
    if walk.budget == 0 {
        walk.rows
            .push(faint("\u{2026} more than the pane draws; copy the JSON", p));
    }
    col().w_full().children(walk.rows)
}

struct Walk<'a> {
    inspect: &'a Inspect,
    p: &'a Paint,
    /// Rows left to draw.
    budget: usize,
    rows: Vec<El>,
}

impl Walk<'_> {
    fn value(&mut self, label: Option<String>, value: &Value, path: &str, depth: usize) {
        if self.budget == 0 {
            return;
        }
        self.budget -= 1;
        let p = self.p;
        let key = label.map(|label| {
            text(format!("{label}:"))
                .font(mono(10.5, 400.0))
                .color(p.c.text_muted)
        });
        let line = |depth: usize| {
            row()
                .key(path.to_string())
                .w_full()
                .h(18.0)
                .pl(INDENT * depth as f32)
                .gap(6.0)
                .center_y()
        };
        let children: Vec<(String, &Value)> = match value {
            Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), v)).collect(),
            Value::Array(items) => items
                .iter()
                .enumerate()
                .map(|(i, v)| (i.to_string(), v))
                .collect(),
            scalar => {
                self.rows
                    .push(line(depth).children(key).child(scalar_text(scalar, p)));
                return;
            }
        };
        let open = self.inspect.is_open(path, depth);
        let summary = match value {
            Value::Object(_) => format!("{{{}}}", children.len()),
            _ => format!("[{}]", children.len()),
        };
        let toggle = path.to_string();
        self.rows.push(
            line(depth)
                .role("button")
                .label(format!("{} {path}", if open { "Fold" } else { "Unfold" }))
                .radius(p.k.radius.sm)
                .hover(p.c.hover)
                .on_press(move |app: &mut App| app.inspect.flip(&toggle))
                .child(
                    icon(
                        if open {
                            &oc::CHEVRON_DOWN
                        } else {
                            &oc::CHEVRON_RIGHT
                        },
                        11.0,
                    )
                    .color(p.c.icon_faint),
                )
                .children(key)
                .child(text(summary).font(mono(10.5, 400.0)).color(p.c.text_faint)),
        );
        if !open {
            return;
        }
        for (name, child) in children.iter().take(TREE_ENTRIES) {
            self.value(
                Some(name.clone()),
                child,
                &format!("{path}/{name}"),
                depth + 1,
            );
        }
        if children.len() > TREE_ENTRIES && self.budget > 0 {
            self.rows
                .push(line(depth + 1).key(format!("{path}/+")).child(faint(
                    &format!("+{} more", children.len() - TREE_ENTRIES),
                    p,
                )));
        }
    }
}

fn scalar_text(value: &Value, p: &Paint) -> El {
    let (shown, ink) = match value {
        Value::String(s) => (format!("{s:?}"), p.c.text_base),
        Value::Number(n) => (n.to_string(), p.c.text_accent),
        other => (other.to_string(), p.c.text_muted),
    };
    text(shown)
        .font(mono(10.5, 400.0))
        .color(ink)
        .grow(1.0)
        .min_w(0.0)
        .truncate()
        .selectable(true)
        .selection_color(p.k.selection())
}
