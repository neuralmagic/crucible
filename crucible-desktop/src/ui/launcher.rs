//! The launch sheet: a registered playbook's params schema as a form, its ceilings, and the
//! button that starts it.

use rune::{El, TextEdit, Tx, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, Dialog, SegmentedControl, Spinner, Switch, TextInput};
use rune_theme_opencode::icons as oc;

use crate::form::{Kind, Scale, duration, minutes};
use crate::live::Target;
use crate::ui::desk::{field, pills};
use crate::ui::slider::Slider;
use crate::ui::{Paint, Tone};
use crate::{App, Stage};

/// The sheet's width, its side padding, and the width its fields fill.
const SHEET: f32 = 520.0;
const PAD: f32 = 20.0;
const INNER: f32 = SHEET - 2.0 * PAD;
/// The tallest the form gets before it scrolls, and how much it gives up while a refusal shows
/// under it: the sheet does not scroll, so anything taller than the window is cut off.
const BODY: f32 = 520.0;
const REFUSAL: f32 = 72.0;

/// Which of the form's text fields an edit goes to.
#[derive(Clone, Copy)]
enum Slot {
    Answer(usize),
    Cost,
    Time,
    Model,
}

/// The form's edit for `slot`, or a scratch edit once the form has closed.
fn edit(app: &mut App, slot: Slot) -> &mut TextEdit {
    if let Some(launcher) = app.launcher.as_mut() {
        let found = match slot {
            Slot::Answer(i) => launcher.answers.get_mut(i),
            Slot::Cost => Some(&mut launcher.cost),
            Slot::Time => Some(&mut launcher.time),
            Slot::Model => Some(&mut launcher.model),
        };
        if let Some(edit) = found {
            return edit;
        }
    }
    &mut app.scratch
}

/// The Launch button for `playbook`, when it is registered or the shown run is its draft's.
pub(crate) fn launch_button(app: &App, playbook: &str, p: &Paint) -> Option<El> {
    let target = app.launch_target(playbook)?;
    Some(
        Button::new("Launch")
            .variant(ButtonVariant::Submit)
            .on_press(move |app: &mut App| app.open_launcher(target.clone()))
            .build(p.k)
            .key("open-launcher"),
    )
}

/// Writes the shown failed run up for a coding agent and copies it.
pub(crate) fn fix_button(app: &App, p: &Paint) -> Option<El> {
    let launch = app.selected_live()?;
    let detail = app.selected_detail()?;
    if launch.status == "running" || detail.verdict == crate::model::Verdict::Valid {
        return None;
    }
    app.draft_target()?;
    Some(
        Button::new("Fix with agent")
            .variant(ButtonVariant::Neutral)
            .on_press_tx(|app: &mut App, tx: &mut rune::Tx| {
                if let Some(brief) = app.hand_off(None) {
                    tx.copy(brief);
                }
            })
            .build(p.k)
            .help("Copy a brief for a coding agent to fix the pack and rerun")
            .key("fix-with-agent"),
    )
}

/// The Rerun button for a finished run that did not pass.
pub(crate) fn rerun_button(app: &App, p: &Paint) -> Option<El> {
    app.rerunnable()?;
    if app.selected_detail()?.verdict == crate::model::Verdict::Valid {
        return None;
    }
    Some(
        Button::new("Rerun")
            .variant(ButtonVariant::Neutral)
            .on_press(|app: &mut App| app.open_rerun(Vec::new()))
            .build(p.k)
            .help("Run what did not pass again, reusing what did")
            .key("open-rerun"),
    )
}

/// The sheet, mounted as an overlay while a form is open.
pub(crate) fn sheet(app: &App, p: &Paint, layer: &mut Vec<El>) {
    let Some(launcher) = &app.launcher else {
        return;
    };
    let body = match &launcher.stage {
        Stage::Reading => row()
            .gap(8.0)
            .center_y()
            .py(12.0)
            .child(Spinner::new().size(14.0).build(p.k))
            .child(
                text("Reading the params schema\u{2026}")
                    .font(p.k.ty.ui)
                    .color(p.c.text_muted),
            ),
        Stage::Failed(error) => problem(error, p),
        Stage::Ready | Stage::Sending => form(app, p),
    };
    let blocked = launcher.rerun.as_ref().and_then(|r| r.blocked()).is_some();
    let (verb, going) = if launcher.rerun.is_some() {
        ("Rerun", "Rerunning\u{2026}")
    } else if launcher.schedule.is_some() {
        ("Save schedule", "Saving\u{2026}")
    } else {
        ("Launch", "Launching\u{2026}")
    };
    let sending = launcher.stage == Stage::Sending;
    let ready = matches!(launcher.stage, Stage::Ready | Stage::Sending);
    let launch = if sending {
        Button::new(going)
            .variant(ButtonVariant::Loading)
            .disabled(true)
    } else {
        Button::new(verb)
            .variant(ButtonVariant::Submit)
            .disabled(!ready || blocked)
            .on_press(|app: &mut App| app.submit_launch())
    };
    let editing = launcher
        .schedule
        .as_ref()
        .is_some_and(|s| s.stored.is_some());
    let title = match (&launcher.target, &launcher.rerun) {
        (target, Some(_)) => format!("Rerun {}", target.id()),
        (target, None) if editing => format!("Edit schedule of {}", target.id()),
        (Target::Draft(id), None) => format!("Launch draft {id}"),
        (Target::Playbook(id), None) => format!("Launch {id}"),
    };
    let body = col()
        .key("launcher-body")
        .w_full()
        .max_h(if launcher.refusal.is_some() {
            BODY - REFUSAL
        } else {
            BODY
        })
        .px(PAD)
        .scroll(rune::ScrollSpec {
            stick_bottom: false,
        })
        .scroll_edge_fade(8.0)
        .child(body);
    Dialog::new("launcher", title)
        .open(true)
        .fit()
        .width(SHEET)
        .on_close(|app: &mut App| app.launcher = None)
        .child(p.k.scroll_bar(body).w_full())
        .child(
            col()
                .w_full()
                .children(launcher.refusal.as_ref().map(|why| {
                    row()
                        .key("launcher-refusal")
                        .w_full()
                        .px(PAD)
                        .pt(8.0)
                        .child(problem(why, p))
                })),
        )
        .child(
            row()
                .w_full()
                .gap(8.0)
                .px(PAD)
                .pt(8.0)
                .pb(16.0)
                .child(el().grow(1.0))
                .child(
                    Button::new("Cancel")
                        .variant(ButtonVariant::Neutral)
                        .on_press(|app: &mut App| app.launcher = None)
                        .build(p.k)
                        .key("launcher-cancel"),
                )
                .child(launch.build(p.k).key("launcher-send")),
        )
        .build(p.k, layer);
}

fn problem(message: &str, p: &Paint) -> El {
    let s = p.tone(Tone::Danger);
    row()
        .w_full()
        .gap(8.0)
        .px(10.0)
        .py(8.0)
        .radius(p.k.radius.md)
        .fill(s.bg)
        .border(p.hair, s.border)
        .child(icon(&oc::WARNING, 14.0).color(s.icon))
        .child(
            text(message.to_string())
                .font(p.k.ty.ui)
                .color(s.fg)
                .grow(1.0)
                .min_w(0.0)
                .wrap(),
        )
}

fn label(name: &str, help: &str, required: bool, p: &Paint) -> El {
    let mut row_ = row().gap(4.0).center_y().child(
        text(name.to_string())
            .font(p.k.ty.ui_medium)
            .color(p.c.text_base),
    );
    if required {
        row_ = row_.child(el().size(5.0, 5.0).radius(2.5).fill(p.c.bg_accent));
    }
    if !help.is_empty() {
        row_ = row_.help(help.to_string());
    }
    row_
}

/// What a rerun reuses and dispatches again, or why it cannot go yet.
fn rerun_summary(rerun: &crate::studio::Rerun, p: &Paint) -> El {
    let from = if rerun.from.is_empty() {
        "what did not pass".to_string()
    } else {
        rerun.from.join(", ")
    };
    let mut summary = col()
        .w_full()
        .gap(10.0)
        .key("rerun-summary")
        .child(fact("Run", rerun.run.clone(), p))
        .child(fact("From", from, p));
    match &rerun.preview {
        None => {
            summary = summary.child(
                row()
                    .gap(8.0)
                    .center_y()
                    .child(Spinner::new().size(14.0).build(p.k))
                    .child(
                        text("Reading what the rerun reuses\u{2026}")
                            .font(p.k.ty.ui)
                            .color(p.c.text_muted),
                    ),
            );
        }
        Some(Err(error)) => summary = summary.child(problem(error, p)),
        Some(Ok(preview)) => {
            let reused = preview
                .reused
                .iter()
                .map(|task| task.task.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            summary = summary
                .child(fact(
                    "Reuses",
                    if reused.is_empty() {
                        "nothing".to_string()
                    } else {
                        format!("{reused} (${:.2})", preview.reused_usd)
                    },
                    p,
                ))
                .child(fact("Runs", preview.rerun.join(", "), p));
            if !preview.changed.is_empty() {
                summary = summary
                    .child(problem(
                        &format!(
                            "Changed since the run, so they cannot be reused: {}",
                            preview.changed.join(", ")
                        ),
                        p,
                    ))
                    .child(
                        row().child(
                            Button::new("Rerun those too")
                                .variant(ButtonVariant::Neutral)
                                .on_press(|app: &mut App| app.widen_rerun())
                                .build(p.k)
                                .key("rerun-widen"),
                        ),
                    );
            }
        }
    }
    summary
}

fn fact(name: &str, value: String, p: &Paint) -> El {
    row()
        .w_full()
        .gap(10.0)
        .child(
            text(name.to_string())
                .font(p.k.ty.ui)
                .color(p.c.text_muted)
                .w(70.0),
        )
        .child(
            text(value)
                .font(p.k.ty.ui)
                .color(p.c.text_base)
                .grow(1.0)
                .min_w(0.0)
                .selectable(true)
                .selection_color(p.k.selection())
                .wrap(),
        )
}

fn form(app: &App, p: &Paint) -> El {
    let Some(launcher) = &app.launcher else {
        return el();
    };
    if let Some(rerun) = &launcher.rerun {
        return col()
            .w_full()
            .gap(16.0)
            .pt(4.0)
            .child(rerun_summary(rerun, p))
            .child(el().h(p.hair).w_full().fill(p.c.border_muted))
            .child(runs_on(app, p))
            .child(ceilings(app, p));
    }
    let mut fields = col().w_full().gap(14.0);
    for (i, field) in launcher.fields.iter().enumerate() {
        let answer = launcher
            .answers
            .get(i)
            .map(|a| a.text.as_str())
            .unwrap_or_default();
        let issue = launcher
            .problems
            .iter()
            .find(|(at, _)| *at == i)
            .map(|(_, why)| why.clone());
        let control = match &field.kind {
            Kind::Bool => Switch::new(answer == "true")
                .label(field.name.clone())
                .hide_label()
                .on_toggle(move |app: &mut App| {
                    let on = edit(app, Slot::Answer(i)).text == "true";
                    *edit(app, Slot::Answer(i)) = TextEdit::new(if on { "false" } else { "true" });
                })
                .build(p.k),
            Kind::Choice(values) if values.len() <= 4 => {
                let picks = values.clone();
                SegmentedControl::new(values, values.iter().position(|v| v == answer))
                    .label(field.name.clone())
                    .width(Some(110.0 * values.len() as f32))
                    .on_pick(move |app: &mut App, pick: usize, _: &mut Tx| {
                        if let Some(value) = picks.get(pick) {
                            *edit(app, Slot::Answer(i)) = TextEdit::new(value);
                        }
                    })
                    .build(p.k)
            }
            _ => TextInput::new(launcher.answers.get(i).unwrap_or(&app.scratch))
                .placeholder(field.default.clone())
                .width(Some(INNER))
                .invalid(issue.is_some())
                .on_edit(move |app: &mut App| edit(app, Slot::Answer(i)))
                .build(p.k),
        };
        fields = fields.child(
            col()
                .key(format!("field-{}", field.name))
                .w_full()
                .gap(6.0)
                .child(label(&field.name, &field.description, field.required, p))
                .child(control)
                .children(issue.map(|why| text(why).font(p.k.ty.small).color(p.c.danger.fg))),
        );
    }
    if launcher.fields.is_empty() {
        fields = fields.child(
            text("This playbook takes no params.")
                .font(p.k.ty.ui)
                .color(p.c.text_muted),
        );
    }
    let editing = launcher
        .schedule
        .as_ref()
        .is_some_and(|s| s.stored.is_some());
    col()
        .w_full()
        .gap(16.0)
        .pt(4.0)
        .children((!editing).then(|| when(app, p)))
        .child(fields)
        .child(el().h(p.hair).w_full().fill(p.c.border_muted))
        .child(runs_on(app, p))
        .child(ceilings(app, p))
        .children(
            launcher
                .schedule
                .as_ref()
                .map(|s| schedule_fields(s, app, p)),
        )
}

/// Launch now, or save a schedule that launches on a cron.
fn when(app: &App, p: &Paint) -> El {
    let scheduled = app.launcher.as_ref().is_some_and(|l| l.schedule.is_some());
    SegmentedControl::new(&["Run now", "On a schedule"], Some(scheduled as usize))
        .label("When")
        .width(Some(INNER))
        .on_pick(|app: &mut App, i: usize, _: &mut Tx| {
            let owner = app.owners().into_iter().next().unwrap_or_default();
            if let Some(launcher) = &mut app.launcher {
                launcher.schedule = (i == 1).then(|| crate::admin::ScheduleForm::new(&owner));
                launcher.refusal = None;
            }
        })
        .build(p.k)
        .key("launch-when")
}

/// The cron, its zone and the owner of a schedule.
fn schedule_fields(form: &crate::admin::ScheduleForm, app: &App, p: &Paint) -> El {
    let mut owners = app.owners();
    if !form.owner.is_empty() && !owners.contains(&form.owner) {
        owners.insert(0, form.owner.clone());
    }
    let owners: Vec<(String, String)> = owners.into_iter().map(|o| (o.clone(), o)).collect();
    col()
        .key("schedule-fields")
        .w_full()
        .gap(12.0)
        .child(
            row()
                .w_full()
                .gap(12.0)
                .child(field(
                    "Cron",
                    TextInput::new(&form.cron)
                        .placeholder("0 9 * * MON-FRI")
                        .width(Some((INNER - 12.0) / 2.0))
                        .on_edit(|app: &mut App| {
                            match app.launcher.as_mut().and_then(|l| l.schedule.as_mut()) {
                                Some(form) => &mut form.cron,
                                None => &mut app.scratch,
                            }
                        })
                        .build(p.k)
                        .key("schedule-cron"),
                    p,
                ))
                .child(field(
                    "Time zone",
                    TextInput::new(&form.tz)
                        .placeholder("UTC")
                        .width(Some((INNER - 12.0) / 2.0))
                        .on_edit(|app: &mut App| {
                            match app.launcher.as_mut().and_then(|l| l.schedule.as_mut()) {
                                Some(form) => &mut form.tz,
                                None => &mut app.scratch,
                            }
                        })
                        .build(p.k)
                        .key("schedule-tz"),
                    p,
                )),
        )
        .child(field(
            "Owner",
            pills(
                "schedule-owner",
                &owners,
                Some(&form.owner),
                |app: &mut App, owner: &str| {
                    if let Some(form) = app.launcher.as_mut().and_then(|l| l.schedule.as_mut()) {
                        form.owner = owner.to_string();
                    }
                },
                p,
            ),
            p,
        ))
}

/// The cluster the launch dispatches onto, and the provider and model its agent runs against.
fn runs_on(app: &App, p: &Paint) -> El {
    let Some(launcher) = &app.launcher else {
        return el();
    };
    let provider = launcher.provider.clone().unwrap_or_default();
    let placeholder = launcher
        .default_models
        .iter()
        .find(|(id, _)| *id == provider)
        .map(|(_, model)| model.clone())
        .unwrap_or_else(|| "the provider's default".to_string());
    col()
        .key("runs-on")
        .w_full()
        .gap(12.0)
        .children((!launcher.targets.is_empty()).then(|| {
            field(
                "Target",
                pills(
                    "launch-target",
                    &launcher.targets,
                    launcher.dispatch.as_deref(),
                    |app: &mut App, name: &str| {
                        if let Some(launcher) = app.launcher.as_mut() {
                            launcher.dispatch = Some(name.to_string());
                        }
                    },
                    p,
                ),
                p,
            )
        }))
        .child(field(
            "Agent",
            pills(
                "launch-provider",
                &launcher.providers,
                Some(provider.as_str()),
                |app: &mut App, id: &str| {
                    if let Some(launcher) = app.launcher.as_mut() {
                        launcher.provider = (!id.is_empty()).then(|| id.to_string());
                    }
                },
                p,
            ),
            p,
        ))
        .child(field(
            "Model",
            TextInput::new(&launcher.model)
                .placeholder(placeholder)
                .width(Some(INNER))
                .on_edit(|app: &mut App| edit(app, Slot::Model))
                .build(p.k)
                .key("launch-model"),
            p,
        ))
}

fn ceilings(app: &App, p: &Paint) -> El {
    let Some(launcher) = &app.launcher else {
        return el();
    };
    let (cap_cost, cap_time) = &launcher.caps;
    row()
        .w_full()
        .gap(12.0)
        .child(
            col()
                .grow(1.0)
                .gap(6.0)
                .child(label(
                    "Max cost",
                    &format!("At most ${cap_cost:.2}"),
                    true,
                    p,
                ))
                .child(
                    TextInput::new(&launcher.cost)
                        .placeholder("5.00")
                        .width(Some((INNER - 12.0) / 2.0))
                        .on_edit(|app: &mut App| edit(app, Slot::Cost))
                        .build(p.k),
                )
                .children((*cap_cost > 0.0).then(|| {
                    let scale = Scale::dollars(*cap_cost);
                    let asked = launcher
                        .cost
                        .text
                        .trim()
                        .trim_start_matches('$')
                        .parse()
                        .unwrap_or(0.0);
                    capped(
                        Slider {
                            key: "launch-cost-slider".into(),
                            label: "Max cost".into(),
                            frac: scale.frac(asked),
                            steps: scale.steps(),
                            set: set_cost,
                        }
                        .build(p),
                        format!("${cap_cost:.2}"),
                        p,
                    )
                })),
        )
        .child(
            col()
                .grow(1.0)
                .gap(6.0)
                .child(label("Max time", &format!("At most {cap_time}"), true, p))
                .child(
                    TextInput::new(&launcher.time)
                        .placeholder("30m")
                        .width(Some((INNER - 12.0) / 2.0))
                        .on_edit(|app: &mut App| edit(app, Slot::Time))
                        .build(p.k),
                )
                .children(minutes(cap_time).map(|cap| {
                    let scale = Scale::minutes(cap);
                    let asked = minutes(&launcher.time.text).unwrap_or(0) as f64;
                    capped(
                        Slider {
                            key: "launch-time-slider".into(),
                            label: "Max time".into(),
                            frac: scale.frac(asked),
                            steps: scale.steps(),
                            set: set_time,
                        }
                        .build(p),
                        cap_time.clone(),
                        p,
                    )
                })),
        )
}

/// A slider with the cap it stops at beside its end.
fn capped(slider: El, cap: String, p: &Paint) -> El {
    row()
        .w((INNER - 12.0) / 2.0)
        .gap(8.0)
        .center_y()
        .child(row().grow(1.0).min_w(0.0).child(slider))
        .child(text(cap).font(p.k.ty.small).color(p.c.text_faint))
}

fn set_cost(app: &mut App, frac: f32) {
    if let Some(launcher) = app.launcher.as_mut() {
        let cost = Scale::dollars(launcher.caps.0).at(frac);
        launcher.cost = TextEdit::new(format!("{cost:.2}"));
    }
}

fn set_time(app: &mut App, frac: f32) {
    if let Some(launcher) = app.launcher.as_mut()
        && let Some(cap) = minutes(&launcher.caps.1)
    {
        let time = Scale::minutes(cap).at(frac).round() as u64;
        launcher.time = TextEdit::new(duration(time));
    }
}
