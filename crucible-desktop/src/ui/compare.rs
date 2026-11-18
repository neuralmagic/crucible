//! The Compare pane: the selected run against an earlier launch of the same playbook, task by
//! task, and for a fan-out instance by instance.

use rune::{Color, El, ScrollSpec, col, row, text};
use serde_json::Value;

use crate::App;
use crate::diff::{self, FieldChange, InstanceDiff, Presence, TaskDiff};
use crate::ui::tasks::section;
use crate::ui::{Paint, Pane};

/// Launches the picker offers.
const CANDIDATES: usize = 12;
/// Field changes one task or instance lists.
const FIELDS: usize = 12;
/// Instances one task lists.
const INSTANCES: usize = 60;
/// Characters of a value a row shows.
const VALUE_CHARS: usize = 90;

pub(crate) fn compare_view(pane: &Pane, app: &App, p: &Paint) -> El {
    let shell = |child: El| {
        col()
            .key("compare-scroll")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .scroll_edge_fade(8.0)
            .child(col().w_full().gap(12.0).padding(14.0).child(child))
    };
    if app.selected_live().is_none() {
        return shell(faint("Controller launches only", p));
    }
    let candidates = app.compare_candidates();
    let Some(base) = app.compare_base() else {
        return shell(faint("No earlier launch that ran", p));
    };
    let mut body = col()
        .w_full()
        .gap(12.0)
        .child(picker(&candidates, &base.key, p));
    let read = app
        .remote
        .live_base
        .as_ref()
        .filter(|read| read.key == base.key);
    let Some(read) = read else {
        return shell(body.child(faint(&format!("Reading {}", base.created), p)));
    };
    if let Some(error) = &read.error {
        return shell(
            body.child(
                text(error.clone())
                    .font(p.mono(10.5, 400.0))
                    .color(p.fail)
                    .wrap(),
            ),
        );
    }
    let diffs = diff::diff(&read.fold.tasks, pane.tasks);
    let (changed, same): (Vec<&TaskDiff>, Vec<&TaskDiff>) =
        diffs.iter().partition(|diff| !diff.is_same());
    body = body.child(summary(&changed, same.len(), p));
    for task in changed {
        body = body.child(task_card(task, p));
    }
    shell(body)
}

fn picker(candidates: &[&crate::live::LivePlaybook], base: &str, p: &Paint) -> El {
    let mut chips = row().w_full().gap(4.0).wrap().center_y().child(
        text("against".to_string())
            .font(p.k.ty.small)
            .color(p.c.text_faint)
            .pr(4.0),
    );
    for launch in candidates.iter().take(CANDIDATES) {
        let on = launch.key == base;
        let key = launch.key.clone();
        chips = chips.child(
            row()
                .key(format!("compare-{}", launch.key))
                .role("button")
                .label(format!("Compare against {}", launch.created))
                .h(22.0)
                .px(8.0)
                .center_y()
                .radius(p.k.radius.sm)
                .fill(if on { p.c.bg_accent } else { p.c.layer_02 })
                .border(p.hair, p.c.border_muted)
                .hover(p.c.hover)
                .focusable(true)
                .on_press(move |app: &mut App| app.run.compare_to = Some(key.clone()))
                .child(
                    text(launch.landed_at.clone())
                        .font(p.mono(10.0, 500.0))
                        .color(if on { p.c.text_base } else { p.c.text_muted }),
                ),
        );
    }
    chips
}

fn summary(changed: &[&TaskDiff], same: usize, p: &Paint) -> El {
    let count = |presence: Presence| {
        changed
            .iter()
            .flat_map(|t| &t.instances)
            .filter(|i| i.presence == presence)
            .count()
    };
    let mut parts = vec![format!("{} changed", changed.len()), format!("{same} same")];
    let (added, removed) = (count(Presence::Added), count(Presence::Removed));
    let moved = changed
        .iter()
        .flat_map(|t| &t.instances)
        .filter(|i| i.presence == Presence::Both)
        .count();
    if added + removed + moved > 0 {
        parts.push(format!("instances +{added} \u{2212}{removed} ~{moved}"));
    }
    text(parts.join(" \u{b7} "))
        .font(p.mono(11.0, 500.0))
        .color(p.c.text_muted)
}

fn task_card(task: &TaskDiff, p: &Paint) -> El {
    let name = task.name.clone();
    let mut head = row()
        .w_full()
        .gap(8.0)
        .center_y()
        .child(mark(task.presence, p))
        .child(
            text(task.name.clone())
                .font(p.mono(11.5, 600.0))
                .color(p.c.text_base)
                .role("button")
                .label(format!("Open {}", task.name))
                .on_press(move |app: &mut App| app.pick_task(&name, None)),
        );
    if let Some((before, after)) = &task.state {
        head = head.child(state_change(before, after, p));
    }
    let mut card = col()
        .key(format!("compare-task-{}", task.name))
        .w_full()
        .gap(6.0)
        .padding(10.0)
        .radius(p.k.radius.md)
        .fill(p.c.bg_base)
        .border(p.hair, p.c.border_muted)
        .child(head);
    if !task.fields.is_empty() {
        card = card.child(field_rows(&task.fields, 0.0, p));
    }
    if !task.instances.is_empty() {
        card = card.child(section("Instances", p));
        for instance in task.instances.iter().take(INSTANCES) {
            card = card.child(instance_rows(instance, p));
        }
        if task.instances.len() > INSTANCES {
            card = card.child(faint(
                &format!("+{} more", task.instances.len() - INSTANCES),
                p,
            ));
        }
    }
    card
}

fn instance_rows(instance: &InstanceDiff, p: &Paint) -> El {
    let mut head = row()
        .w_full()
        .gap(8.0)
        .center_y()
        .child(mark(instance.presence, p))
        .child(
            text(instance.key.clone())
                .font(p.mono(10.5, 500.0))
                .color(p.c.text_base)
                .min_w(0.0)
                .truncate(),
        );
    if let Some((before, after)) = &instance.state {
        head = head.child(state_change(before, after, p));
    }
    let mut rows = col().w_full().gap(2.0).child(head);
    if instance.presence == Presence::Both && !instance.fields.is_empty() {
        rows = rows.child(field_rows(&instance.fields, 18.0, p));
    }
    rows
}

fn field_rows(fields: &[FieldChange], indent: f32, p: &Paint) -> El {
    let mut rows = col().w_full().gap(2.0).pl(indent);
    for field in fields.iter().take(FIELDS) {
        rows = rows.child(
            row()
                .w_full()
                .gap(6.0)
                .child(
                    text(field.path.clone())
                        .font(p.mono(10.5, 400.0))
                        .color(p.c.text_muted)
                        .w(170.0)
                        .truncate(),
                )
                .child(value(field.before.as_ref(), p.fail, p))
                .child(
                    text("\u{2192}".to_string())
                        .font(p.mono(10.5, 400.0))
                        .color(p.c.text_faint),
                )
                .child(value(field.after.as_ref(), p.pass, p)),
        );
    }
    if fields.len() > FIELDS {
        rows = rows.child(faint(&format!("+{} more fields", fields.len() - FIELDS), p));
    }
    rows
}

fn value(value: Option<&Value>, ink: Color, p: &Paint) -> El {
    let shown = match value {
        None => "\u{2014}".to_string(),
        Some(value) => {
            let mut s = value.to_string();
            if s.chars().count() > VALUE_CHARS {
                s = s.chars().take(VALUE_CHARS).collect::<String>() + "\u{2026}";
            }
            s
        }
    };
    text(shown)
        .font(p.mono(10.5, 400.0))
        .color(if value.is_some() { ink } else { p.c.text_faint })
        .grow(1.0)
        .min_w(0.0)
        .truncate()
        .selectable(true)
        .selection_color(p.k.selection())
}

fn state_change(before: &str, after: &str, p: &Paint) -> El {
    text(format!(
        "{} \u{2192} {}",
        before.replace('_', " "),
        after.replace('_', " ")
    ))
    .font(p.mono(10.5, 400.0))
    .color(p.c.text_muted)
}

fn mark(presence: Presence, p: &Paint) -> El {
    let (glyph, ink) = match presence {
        Presence::Added => ("+", p.pass),
        Presence::Removed => ("\u{2212}", p.fail),
        Presence::Both => ("~", p.c.text_faint),
    };
    text(glyph.to_string())
        .font(p.mono(11.0, 600.0))
        .color(ink)
        .w(10.0)
}

fn faint(label: &str, p: &Paint) -> El {
    text(label.to_string())
        .font(p.mono(10.5, 400.0))
        .color(p.c.text_faint)
}
