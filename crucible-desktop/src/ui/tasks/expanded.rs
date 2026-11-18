//! An opened task card: its output, files, links and the instances of a fan-out.

use crate::App;
use crate::session::{Instance, TaskState, TaskView};
use crate::ui::Paint;
use crate::ui::tasks::rounds::{rounds, said};
use crate::ui::tasks::{button, fact_row, label_ink, link_chip, section, state_label};
use crucible_contract::session::TaskStatus;
use rune::{El, ScrollSpec, Tx, col, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize};

const OUTPUT_CHARS: usize = 12_000;
const INSTANCE_CHIPS: usize = 48;

pub(in crate::ui::tasks) fn expanded(
    task: &TaskView,
    tasks: &[TaskView],
    rerun: bool,
    editable: bool,
    link: Option<String>,
    p: &Paint,
) -> El {
    let fix =
        rerun && matches!(task.state(), TaskState::Settled(status) if status != TaskStatus::Pass);
    let mut panel = col()
        .key(format!("task-open-{}", task.name))
        .w_full()
        .gap(10.0)
        .pt(8.0);

    let mut grid = col().w_full().gap(3.0);
    for (label, value) in [
        ("After", task.depends_on.join(", ")),
        ("Over", task.over.clone()),
        ("When", task.when.clone()),
        ("Session", task.session.clone()),
        ("Timeout", task.timeout.clone()),
        (
            "Emits",
            task.emits
                .iter()
                .map(|emit| emit.field.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        ),
        (
            "Files",
            task.files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        ),
        (
            "Reused",
            task.result
                .as_ref()
                .and_then(|r| r.reused_from.clone())
                .unwrap_or_default(),
        ),
    ] {
        if !value.is_empty() {
            grid = grid.child(fact_row(label, value, p));
        }
    }
    panel = panel.child(grid);

    if !task.command.trim().is_empty() {
        panel = panel.child(section("Runs", p)).child(
            text(task.command.trim().to_string())
                .key(format!("task-command-{}", task.name))
                .font(p.mono(10.5, 400.0))
                .color(p.c.text_base)
                .selectable(true)
                .selection_color(p.k.selection())
                .wrap(),
        );
    }
    if !task.instances.is_empty() {
        panel = panel
            .child(section("Instances", p))
            .child(instances(task, p));
    }
    if !task.rounds.is_empty() {
        panel = panel
            .child(section("Rounds", p))
            .child(rounds(task, tasks, p));
    }
    let repairs = task.repairs();
    if !repairs.is_empty() {
        let mut list = col().w_full().gap(4.0);
        for repair in repairs {
            let mut item = col().w_full().gap(2.0).child(
                text(format!("{} \u{b7} ${:.4}", repair.label, repair.cost_usd))
                    .font(p.mono(10.5, 500.0))
                    .color(p.c.text_base),
            );
            for note in &repair.notes {
                item = item.child(
                    text(format!("\u{2022} {note}"))
                        .font(p.mono(10.0, 400.0))
                        .color(p.c.text_muted)
                        .wrap(),
                );
            }
            list = list.child(item);
        }
        panel = panel.child(section("Repairs", p)).child(list);
    }
    let links = task.links();
    if !links.is_empty() {
        let mut list = col().w_full().gap(3.0);
        for link in links {
            list = list.child(
                row()
                    .w_full()
                    .gap(6.0)
                    .center_y()
                    .child(link_chip(link, p))
                    .child(
                        text(link.kind.replace('_', " "))
                            .font(p.mono(10.0, 400.0))
                            .color(p.c.text_muted),
                    )
                    .child(
                        text(link.url.clone())
                            .font(p.mono(10.0, 400.0))
                            .color(p.c.text_muted)
                            .grow(1.0)
                            .min_w(0.0)
                            .truncate()
                            .selectable(true)
                            .selection_color(p.k.selection()),
                    ),
            );
        }
        panel = panel.child(section("Links", p)).child(list);
    }
    if let Some(output) = task.result.as_ref().and_then(|r| r.output.as_ref()) {
        let mut pretty = serde_json::to_string_pretty(output).unwrap_or_default();
        if pretty.len() > OUTPUT_CHARS {
            let cut = pretty
                .char_indices()
                .nth(OUTPUT_CHARS)
                .map_or(pretty.len(), |(at, _)| at);
            pretty.truncate(cut);
            pretty.push_str("\n\u{2026}");
        }
        panel = panel.child(section("Output", p)).child(
            col()
                .w_full()
                .max_h(260.0)
                .padding(10.0)
                .radius(p.k.radius.md)
                .fill(p.c.bg_base)
                .border(p.hair, p.c.border_muted)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .scroll_xy()
                .child(
                    text(pretty)
                        .font(p.mono(10.0, 400.0))
                        .color(p.c.text_base)
                        .selectable(true)
                        .selection_color(p.k.selection()),
                ),
        );
    }
    let name = task.name.clone();
    let from = task.name.clone();
    panel.child(
        row()
            .w_full()
            .gap(6.0)
            .child(button("Show in log", p, move |app: &mut App| {
                app.filter_log(Some(&name))
            }))
            .children(editable.then(|| {
                let name = task.name.clone();
                button("Edit", p, move |app: &mut App| app.edit_task(&name))
                    .key(format!("edit-{}", task.name))
            }))
            .children((task.kind == "agent").then(|| {
                let name = task.name.clone();
                button("Transcript", p, move |app: &mut App| {
                    app.run.expanded = Some(name.clone());
                    app.show_pane(crate::workspace::PaneKind::Transcript);
                })
                .key(format!("transcript-{}", task.name))
            }))
            .children(rerun.then(|| {
                let key = format!("rerun-from-{from}");
                button("Rerun from here", p, move |app: &mut App| {
                    app.open_rerun(vec![from.clone()])
                })
                .key(key)
            }))
            .children(fix.then(|| {
                let key = format!("fix-{}", task.name);
                let task = task.name.clone();
                Button::new("Fix with agent")
                    .variant(ButtonVariant::Neutral)
                    .control_size(ControlSize::Small)
                    .on_press_tx(move |app: &mut App, tx: &mut Tx| {
                        if let Some(brief) = app.hand_off(Some(&task)) {
                            tx.copy(brief);
                        }
                    })
                    .build(p.k)
                    .key(key)
            }))
            .children(link.map(|url| {
                Button::new("Copy link")
                    .variant(ButtonVariant::Neutral)
                    .control_size(ControlSize::Small)
                    .on_press_tx(move |_: &mut App, tx: &mut Tx| tx.copy(url.clone()))
                    .build(p.k)
                    .key(format!("copy-link-{}", task.name))
            })),
    )
}

fn instances(task: &TaskView, p: &Paint) -> El {
    let mut list = col().w_full().gap(2.0);
    for instance in task.instances.iter().take(INSTANCE_CHIPS) {
        list = list.child(instance_row(&task.name, instance, p));
    }
    if task.instances.len() > INSTANCE_CHIPS {
        list = list.child(
            text(format!("+{} more", task.instances.len() - INSTANCE_CHIPS))
                .font(p.mono(10.0, 400.0))
                .color(p.c.text_muted),
        );
    }
    list
}

fn instance_row(task: &str, instance: &Instance, p: &Paint) -> El {
    let state = instance.state();
    let full = format!("{task}[{}]", instance.key);
    let mut detail = Vec::new();
    if let Some(result) = &instance.result {
        if result.attempts > 1 {
            detail.push(format!("{} tries", result.attempts));
        }
        if result.cost_usd > 0.0 {
            detail.push(format!("${:.4}", result.cost_usd));
        }
        detail.extend(said(result));
    } else if let Some(attempt) = instance.attempt {
        detail.push(format!("try {}", attempt.number));
    }
    row()
        .role("button")
        .label(format!("Show {full} in the log"))
        .w_full()
        .h(22.0)
        .px(6.0)
        .gap(7.0)
        .center_y()
        .radius(p.k.radius.sm)
        .hover(p.c.hover)
        .focusable(true)
        .on_press(move |app: &mut App| app.filter_log(Some(&full)))
        .child(
            text(instance.key.clone())
                .font(p.mono(10.5, 500.0))
                .color(p.c.text_base)
                .w(150.0)
                .truncate(),
        )
        .child(
            text(state_label(state))
                .font(p.mono(10.0, 400.0))
                .color(label_ink(state, p))
                .w(70.0),
        )
        .child(
            text(detail.join(" \u{b7} "))
                .font(p.mono(10.0, 400.0))
                .color(p.c.text_muted)
                .grow(1.0)
                .min_w(0.0)
                .truncate(),
        )
}
