//! The schedules on the Triggers page: every one the caller may see, with a switch to pause or
//! resume each and buttons to fire it now or test-fire it. A schedule with a cursor opens a form
//! on its test firing to pick the cursor it gets.

use rune::{El, col, row, text};
use rune_kit::{Button, ButtonVariant, Switch, TextArea};

use crate::admin::{Action, CursorFrom, TestFireForm, relative};
use crate::ui::desk::{card, heading, landed, none, now, pills, stacked};
use crate::ui::{Paint, Tone, flag, tag};
use crate::{App, Selection};

/// The schedules as a card: each with a switch to pause or resume it and buttons to fire it now
/// or test-fire it.
pub(crate) fn section(app: &App, p: &Paint) -> El {
    let body = col()
        .key("schedules")
        .w_full()
        .gap(6.0)
        .child(heading("Schedules", p));
    let schedules = match landed(&app.remote.admin.schedules, "the schedules", p) {
        Err(standin) => return card("schedules-card", body.child(*standin), p),
        Ok(schedules) => schedules,
    };
    if schedules.is_empty() {
        return card(
            "schedules-card",
            body.child(none("schedules-none", "None", p)),
            p,
        );
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
    card(
        "schedules-card",
        body.child(stacked("schedules-list", rows, p)),
        p,
    )
}

fn schedule_row(schedule: &crux::dto::Schedule, app: &App, now: i64, p: &Paint) -> El {
    let enabled = schedule.enabled;
    let toggled = schedule.clone();
    let switch = Switch::new(enabled)
        .label(format!("{} enabled", schedule.playbook))
        .hide_label()
        .disabled(app.remote.desk.busy())
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

    let mut when =
        vec![crate::cron::words(&schedule.cron_expr).unwrap_or_else(|| schedule.cron_expr.clone())];
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

    let (id, firing) = (schedule.id.clone(), schedule.playbook.clone());
    let fire = Button::new("Fire now")
        .variant(ButtonVariant::GhostMuted)
        .disabled(!enabled || app.remote.desk.busy())
        .on_press(move |app: &mut App| {
            app.act(Action::FireSchedule {
                id: id.clone(),
                playbook: firing.clone(),
            })
        })
        .build(p.k)
        .key(format!("schedule-fire-{}", schedule.id));
    let (id, testing, has_cursor) = (
        schedule.id.clone(),
        schedule.playbook.clone(),
        schedule.cursor.is_some(),
    );
    let test = Button::new("Test fire")
        .variant(ButtonVariant::GhostMuted)
        .disabled(app.remote.desk.busy())
        .on_press(move |app: &mut App| {
            if !has_cursor {
                app.act(Action::TestFireSchedule {
                    id: id.clone(),
                    playbook: testing.clone(),
                    cursor: crux::dto::TestCursor::Stored,
                });
                return;
            }
            let desk = &mut app.remote.desk;
            desk.test_fire = match &desk.test_fire {
                Some(open) if open.schedule == id => None,
                _ => Some(TestFireForm::on(&id)),
            };
        })
        .build(p.k)
        .key(format!("schedule-test-fire-{}", schedule.id));
    let editing = schedule.clone();
    let edit = Button::new("Edit")
        .variant(ButtonVariant::GhostMuted)
        .disabled(app.remote.desk.busy())
        .on_press(move |app: &mut App| app.edit_schedule(&editing))
        .build(p.k)
        .key(format!("schedule-edit-{}", schedule.id));
    let form = app
        .remote
        .desk
        .test_fire
        .as_ref()
        .filter(|form| form.schedule == schedule.id)
        .map(|form| test_fire_form(form, schedule, app, p));
    let line = row()
        .w_full()
        .gap(14.0)
        .center_y()
        .child(switch)
        .child(
            col().grow(1.0).min_w(0.0).gap(4.0).child(title).child(
                text(when.join(" \u{b7} "))
                    .font(p.mono(12.0, 420.0))
                    .color(p.c.text_muted)
                    .truncate()
                    .help(schedule.cron_expr.clone()),
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
        .child(fire)
        .child(test)
        .child(edit);
    col()
        .key(format!("schedule-{}", schedule.id))
        .w_full()
        .py(10.0)
        .gap(10.0)
        .child(line)
        .children(form)
}

fn test_fire_form(form: &TestFireForm, schedule: &crux::dto::Schedule, app: &App, p: &Paint) -> El {
    let options: Vec<(String, String)> = CursorFrom::ALL
        .iter()
        .map(|from| (from.label().to_string(), from.label().to_string()))
        .collect();
    let (id, playbook) = (schedule.id.clone(), schedule.playbook.clone());
    let mut body = col()
        .key(format!("schedule-test-fire-form-{}", schedule.id))
        .w_full()
        .gap(8.0)
        .child(pills(
            "test-fire-cursor",
            &options,
            Some(form.from.label()),
            |app: &mut App, picked: &str| {
                if let (Some(form), Some(from)) = (
                    &mut app.remote.desk.test_fire,
                    CursorFrom::ALL.iter().find(|from| from.label() == picked),
                ) {
                    form.from = *from;
                }
            },
            p,
        ));
    if form.from == CursorFrom::Value {
        body = body.child(
            TextArea::new(&form.value)
                .rows(4)
                .width(None)
                .on_edit(|app: &mut App| match &mut app.remote.desk.test_fire {
                    Some(form) => &mut form.value,
                    None => &mut app.scratch,
                })
                .build(p.k)
                .key("test-fire-value"),
        );
    }
    body.child(
        row()
            .gap(8.0)
            .child(
                Button::new("Launch")
                    .variant(ButtonVariant::Submit)
                    .disabled(form.ask().is_none() || app.remote.desk.busy())
                    .on_press(move |app: &mut App| {
                        let asked = app
                            .remote
                            .desk
                            .test_fire
                            .as_ref()
                            .and_then(TestFireForm::ask);
                        if let Some(cursor) = asked {
                            app.act(Action::TestFireSchedule {
                                id: id.clone(),
                                playbook: playbook.clone(),
                                cursor,
                            });
                        }
                    })
                    .build(p.k)
                    .key("test-fire-launch"),
            )
            .child(
                Button::new("Cancel")
                    .variant(ButtonVariant::GhostMuted)
                    .on_press(|app: &mut App| app.remote.desk.test_fire = None)
                    .build(p.k)
                    .key("test-fire-cancel"),
            ),
    )
}
