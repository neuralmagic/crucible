//! The Schedules page: every schedule the caller may see, with a switch to pause or resume each.

use rune::{El, col, row, text};
use rune_kit::Switch;

use crate::admin::{Action, relative};
use crate::ui::desk::{card, frame, landed, none, now, scrolled, stacked};
use crate::ui::{Paint, Tone, flag, mono, tag};
use crate::{App, Selection};

pub(crate) fn page(app: &App, p: &Paint) -> El {
    let note = match &app.admin.schedules {
        Some(Ok(list)) => {
            let on = list.iter().filter(|s| s.enabled).count();
            format!("{} \u{b7} {on} enabled", list.len())
        }
        _ => String::new(),
    };
    let page = frame("schedules", "Schedules", note, app, p);
    let schedules = match landed(&app.admin.schedules, "the schedules", p) {
        Err(standin) => return page.child(*standin),
        Ok(schedules) => schedules,
    };
    if schedules.is_empty() {
        return page.child(none("schedules-none", "No schedules", p));
    }
    let mut sorted: Vec<&crux::dto::Schedule> = schedules.iter().collect();
    sorted.sort_by(|a, b| {
        b.enabled
            .cmp(&a.enabled)
            .then_with(|| a.next_due_at.cmp(&b.next_due_at))
            .then_with(|| a.playbook.cmp(&b.playbook))
    });
    let now = now();
    let rows = sorted
        .into_iter()
        .map(|schedule| schedule_row(schedule, app, now, p))
        .collect();
    page.child(scrolled(
        "schedules-scroll",
        card("schedules-card", stacked("schedules-list", rows, p), p),
        p,
    ))
}

fn schedule_row(schedule: &crux::dto::Schedule, app: &App, now: i64, p: &Paint) -> El {
    let enabled = schedule.enabled;
    let toggled = schedule.clone();
    let switch = Switch::new(enabled)
        .label(format!("{} enabled", schedule.playbook))
        .hide_label()
        .disabled(app.desk.busy())
        .on_toggle(move |app: &mut App| {
            app.act(Action::Enable {
                schedule: Box::new(toggled.clone()),
                on: !enabled,
            })
        })
        .build(p.k)
        .key(format!("schedule-switch-{}", schedule.id));

    let playbook = schedule.playbook.clone();
    let mut title = row().gap(8.0).center_y().child(
        text(schedule.playbook.clone())
            .key(format!("schedule-playbook-{}", schedule.id))
            .font(p.k.ty.ui_medium)
            .color(p.c.text_base)
            .role("link")
            .focusable(true)
            .hover(p.c.hover)
            .help("Open its runs")
            .matched(crate::ui::title_flight(&schedule.playbook))
            .on_press(move |app: &mut App| app.select(Selection::playbook(&playbook))),
    );
    if schedule.target_kind == "draft_head" {
        title = title.child(tag("draft head", p));
    }
    if schedule.consecutive_failures > 0 {
        let failures = schedule.consecutive_failures;
        title = title.child(flag(
            &format!(
                "{failures} failed firing{}",
                if failures == 1 { "" } else { "s" }
            ),
            if enabled { Tone::Warning } else { Tone::Danger },
            None,
            p,
        ));
    }

    let mut when = vec![schedule.cron_expr.clone()];
    if !schedule.tz.is_empty() && schedule.tz != "UTC" {
        when.push(schedule.tz.clone());
    }
    if schedule.max_cost > 0.0 {
        when.push(format!("${:.2}", schedule.max_cost));
    }
    if !schedule.max_time.is_empty() {
        when.push(schedule.max_time.clone());
    }
    if let Some(owner) = &schedule.owner_principal {
        when.push(owner.clone());
    }

    let next = match (&schedule.next_due_at, enabled) {
        (_, false) => "paused".to_string(),
        (Some(at), true) => format!("next {}", relative(at, now)),
        (None, true) => "no next firing".to_string(),
    };
    let last = schedule
        .last_fired_at
        .as_deref()
        .map_or("never fired".to_string(), |at| {
            format!("fired {}", relative(at, now))
        });

    row()
        .key(format!("schedule-{}", schedule.id))
        .w_full()
        .py(10.0)
        .gap(14.0)
        .center_y()
        .child(switch)
        .child(
            col().grow(1.0).min_w(0.0).gap(4.0).child(title).child(
                text(when.join(" \u{b7} "))
                    .font(mono(12.0, 420.0))
                    .color(p.c.text_muted)
                    .truncate(),
            ),
        )
        .child(
            col()
                .gap(4.0)
                .items_end()
                .child(text(next).font(p.k.ty.ui).color(if enabled {
                    p.c.text_base
                } else {
                    p.c.text_faint
                }))
                .child(text(last).font(p.k.ty.small).color(p.c.text_muted)),
        )
}
