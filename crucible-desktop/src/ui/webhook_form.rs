//! The webhook form on the Triggers page: create one for a playbook, or edit a stored one.

use crux::dto::{PreviewResult, WebhookVerifier};
use rune::{El, TextEdit, col, row, text};
use rune_kit::{Button, ButtonVariant, TextArea, TextInput};

use crate::App;
use crate::admin::Action;
use crate::ui::Paint;
use crate::ui::desk::{card, field, heading, landed, pills, reading};
use crate::webhook::{Mode, WebhookForm};

const WIDE: f32 = 560.0;
const NARROW: f32 = 120.0;

fn edit(app: &mut App, pick: fn(&mut WebhookForm) -> &mut TextEdit) -> &mut TextEdit {
    match &mut app.remote.desk.webhook_form {
        Some(form) => pick(form),
        None => &mut app.scratch,
    }
}

fn change(app: &mut App, change: impl FnOnce(&mut WebhookForm)) {
    if let Some(form) = &mut app.remote.desk.webhook_form {
        change(form);
    }
}

pub(crate) fn form(form: &WebhookForm, app: &App, p: &Paint) -> El {
    let title = match &form.editing {
        Some(_) => format!("Edit webhook \u{b7} {}", form.playbook),
        None => "New webhook".to_string(),
    };
    let mut body = col()
        .key("webhook-form")
        .w_full()
        .gap(16.0)
        .child(text(title).font(p.k.ty.ui_medium).color(p.c.text_base));
    if form.editing.is_none() {
        body = body.child(playbook(form, app, p));
    }
    body = body
        .child(presets(app, p))
        .children((!form.unmatched.is_empty()).then(|| {
            text(format!("Not declared here: {}", form.unmatched.join(", ")))
                .font(p.k.ty.small)
                .color(p.c.text_muted)
        }))
        .child(verifier(form, p))
        .child(field(
            "Filter",
            TextArea::new(&form.filter)
                .rows(3)
                .width(Some(WIDE))
                .on_edit(|app: &mut App| edit(app, |form| &mut form.filter))
                .build(p.k)
                .key("webhook-filter"),
            p,
        ))
        .child(field(
            "Dedupe",
            TextInput::new(&form.dedupe)
                .width(Some(WIDE))
                .on_edit(|app: &mut App| edit(app, |form| &mut form.dedupe))
                .build(p.k)
                .key("webhook-dedupe"),
            p,
        ))
        .child(params(form, app, p))
        .child(bounds(form, p))
        .children(diagnostics(app, p))
        .child(preview(form, app, p));
    let asked = form.body();
    let ready = asked.is_ok() && form.laid_out() && !app.remote.desk.busy();
    body = body
        .children(
            asked
                .as_ref()
                .err()
                .map(|why| text(why.clone()).font(p.k.ty.small).color(p.fail)),
        )
        .child(
            row()
                .gap(8.0)
                .child(
                    Button::new(if form.editing.is_some() {
                        "Save"
                    } else {
                        "Create"
                    })
                    .variant(ButtonVariant::Submit)
                    .disabled(!ready)
                    .on_press(|app: &mut App| {
                        let Some(form) = &app.remote.desk.webhook_form else {
                            return;
                        };
                        if let Ok(body) = form.body() {
                            app.act(Action::SaveWebhook {
                                id: form.editing.clone(),
                                body: Box::new(body),
                            });
                        }
                    })
                    .build(p.k)
                    .key("webhook-save"),
                )
                .child(
                    Button::new("Check")
                        .variant(ButtonVariant::Neutral)
                        .disabled(!ready)
                        .on_press(|app: &mut App| {
                            let Some(form) = &app.remote.desk.webhook_form else {
                                return;
                            };
                            if let Ok(body) = form.body() {
                                app.act(Action::CheckWebhook {
                                    body: Box::new(body),
                                });
                            }
                        })
                        .build(p.k)
                        .key("webhook-check"),
                )
                .child(
                    Button::new("Cancel")
                        .variant(ButtonVariant::Ghost)
                        .on_press(|app: &mut App| app.remote.desk.webhook_form = None)
                        .build(p.k)
                        .key("webhook-cancel"),
                ),
        );
    card("webhook-form-card", body, p)
}

fn playbook(form: &WebhookForm, app: &App, p: &Paint) -> El {
    let registered: Vec<(String, String)> = app
        .remote
        .registered
        .iter()
        .map(|id| (id.clone(), id.clone()))
        .collect();
    field(
        "Playbook",
        pills(
            "webhook-playbook",
            &registered,
            Some(&form.playbook),
            |app: &mut App, picked: &str| change(app, |form| form.pick_playbook(picked)),
            p,
        ),
        p,
    )
}

fn presets(app: &App, p: &Paint) -> El {
    let presets = match landed(&app.remote.admin.presets, "the presets", p) {
        Ok(presets) => presets,
        Err(standin) => return field("Sender", *standin, p),
    };
    let options: Vec<(String, String)> = presets
        .iter()
        .map(|preset| (preset.id.clone(), preset.title.clone()))
        .collect();
    field(
        "Sender",
        pills(
            "webhook-preset",
            &options,
            None,
            |app: &mut App, picked: &str| {
                let Some(Ok(presets)) = &app.remote.admin.presets else {
                    return;
                };
                let Some(preset) = presets.iter().find(|preset| preset.id == picked).cloned()
                else {
                    return;
                };
                change(app, |form| form.apply(&preset));
            },
            p,
        ),
        p,
    )
}

const VERIFIERS: [(WebhookVerifier, &str); 2] = [
    (WebhookVerifier::PathToken, "path token"),
    (WebhookVerifier::HmacSha256, "HMAC-SHA256"),
];

fn verifier(form: &WebhookForm, p: &Paint) -> El {
    let label = |verifier: WebhookVerifier| {
        VERIFIERS
            .iter()
            .find(|(held, _)| *held == verifier)
            .map_or("", |(_, label)| label)
    };
    if form.editing.is_some() {
        let mut shown = label(form.verifier).to_string();
        if form.verifier == WebhookVerifier::HmacSha256 {
            shown = format!("{shown} \u{b7} {}", form.header.text);
        }
        return field(
            "Verifier",
            text(shown).font(p.k.ty.ui).color(p.c.text_muted),
            p,
        );
    }
    let options: Vec<(String, String)> = VERIFIERS
        .iter()
        .map(|(_, label)| (label.to_string(), label.to_string()))
        .collect();
    let mut column = col().w_full().gap(12.0).child(field(
        "Verifier",
        pills(
            "webhook-verifier",
            &options,
            Some(label(form.verifier)),
            |app: &mut App, picked: &str| {
                if let Some((verifier, _)) = VERIFIERS.iter().find(|(_, label)| *label == picked) {
                    change(app, |form| form.verifier = *verifier);
                }
            },
            p,
        ),
        p,
    ));
    if form.verifier == WebhookVerifier::HmacSha256 {
        column = column.child(field(
            "Signature header",
            TextInput::new(&form.header)
                .placeholder("x-hub-signature-256")
                .width(Some(320.0))
                .on_edit(|app: &mut App| edit(app, |form| &mut form.header))
                .build(p.k)
                .key("webhook-header"),
            p,
        ));
    }
    column
}

fn params(form: &WebhookForm, app: &App, p: &Paint) -> El {
    let column = col().w_full().gap(8.0).child(heading("Params", p));
    if !form.laid_out() {
        return match &app.remote.admin.webhook_schema {
            Some((playbook, Some(Err(why)))) if playbook == &form.playbook => {
                column.child(text(why.clone()).font(p.k.ty.small).color(p.fail))
            }
            _ => column.child(reading("the params", p)),
        };
    }
    if form.choices.is_empty() {
        return column.child(text("None").font(p.k.ty.ui).color(p.c.text_muted));
    }
    let modes = [
        ("fixed".to_string(), "fixed".to_string()),
        ("derived".to_string(), "derived".to_string()),
    ];
    form.choices
        .iter()
        .enumerate()
        .fold(column, |column, (at, choice)| {
            let derived = choice.mode == Mode::Derived;
            let input = TextInput::new(if derived {
                &choice.expression
            } else {
                &choice.value
            })
            .placeholder(if derived {
                "CEL over body and headers"
            } else {
                ""
            })
            .width(Some(360.0))
            .on_edit(
                move |app: &mut App| match &mut app.remote.desk.webhook_form {
                    Some(form) if at < form.choices.len() => {
                        let choice = &mut form.choices[at];
                        match choice.mode {
                            Mode::Derived => &mut choice.expression,
                            Mode::Fixed => &mut choice.value,
                        }
                    }
                    _ => &mut app.scratch,
                },
            )
            .build(p.k)
            .key(format!("webhook-param-{}", choice.name));
            column.child(
                row()
                    .key(format!("webhook-choice-{}", choice.name))
                    .w_full()
                    .gap(10.0)
                    .center_y()
                    .child(
                        text(choice.name.clone())
                            .font(p.mono(12.0, 520.0))
                            .color(p.c.text_base)
                            .w(140.0)
                            .truncate(),
                    )
                    .child(pills(
                        &format!("webhook-mode-{}", choice.name),
                        &modes,
                        Some(if derived { "derived" } else { "fixed" }),
                        move |app: &mut App, picked: &str| {
                            change(app, |form| {
                                if let Some(choice) = form.choices.get_mut(at) {
                                    choice.mode = if picked == "derived" {
                                        Mode::Derived
                                    } else {
                                        Mode::Fixed
                                    };
                                }
                            });
                        },
                        p,
                    ))
                    .child(input),
            )
        })
}

fn bounds(form: &WebhookForm, p: &Paint) -> El {
    let small = |label: &str,
                 key: &str,
                 value: &TextEdit,
                 pick: fn(&mut WebhookForm) -> &mut TextEdit,
                 width: f32| {
        field(
            label,
            TextInput::new(value)
                .width(Some(width))
                .on_edit(move |app: &mut App| edit(app, pick))
                .build(p.k)
                .key(key.to_string()),
            p,
        )
        .w(width)
    };
    row()
        .w_full()
        .gap(16.0)
        .flex_wrap()
        .child(small(
            "Launches per hour",
            "webhook-per-hour",
            &form.per_hour,
            |form| &mut form.per_hour,
            NARROW,
        ))
        .child(small(
            "Max cost",
            "webhook-cost",
            &form.cost,
            |form| &mut form.cost,
            NARROW,
        ))
        .child(small(
            "Max time",
            "webhook-time",
            &form.time,
            |form| &mut form.time,
            NARROW,
        ))
        .child(small(
            "Retention days",
            "webhook-retention",
            &form.retention,
            |form| &mut form.retention,
            NARROW,
        ))
        .child(small(
            "Dispatch target",
            "webhook-target",
            &form.target,
            |form| &mut form.target,
            200.0,
        ))
}

fn diagnostics(app: &App, p: &Paint) -> Option<El> {
    let check = app.remote.admin.webhook_check.as_ref()?;
    let column = col()
        .key("webhook-diagnostics")
        .w_full()
        .gap(4.0)
        .child(heading("Check", p));
    if check.diagnostics.is_empty() {
        return Some(column.child(text("No problems").font(p.k.ty.ui).color(p.pass)));
    }
    Some(check.diagnostics.iter().fold(column, |column, found| {
        let at = match (found.line, found.column) {
            (Some(line), Some(column)) => format!(":{line}:{column}"),
            (Some(line), None) => format!(":{line}"),
            _ => String::new(),
        };
        column.child(
            text(format!("{}{at}: {}", found.field, found.message))
                .font(p.mono(12.0, 480.0))
                .color(p.fail)
                .selectable(true)
                .wrap(),
        )
    }))
}

fn preview(form: &WebhookForm, app: &App, p: &Paint) -> El {
    let sample = form.sample();
    let mut column = col()
        .key("webhook-preview")
        .w_full()
        .gap(10.0)
        .child(heading("Preview", p))
        .child(field(
            "Sample body",
            TextArea::new(&form.sample)
                .rows(5)
                .width(Some(WIDE))
                .on_edit(|app: &mut App| edit(app, |form| &mut form.sample))
                .build(p.k)
                .key("webhook-sample"),
            p,
        ))
        .child(field(
            "Sample headers",
            TextArea::new(&form.headers)
                .rows(2)
                .width(Some(WIDE))
                .on_edit(|app: &mut App| edit(app, |form| &mut form.headers))
                .build(p.k)
                .key("webhook-sample-headers"),
            p,
        ))
        .children(
            sample
                .as_ref()
                .err()
                .map(|why| text(why.clone()).font(p.k.ty.small).color(p.fail)),
        )
        .child(
            row().child(
                Button::new("Preview")
                    .variant(ButtonVariant::Neutral)
                    .disabled(sample.is_err() || form.body().is_err() || app.remote.desk.busy())
                    .on_press(|app: &mut App| {
                        let Some(form) = &app.remote.desk.webhook_form else {
                            return;
                        };
                        if let (Ok(body), Ok((sample, headers))) = (form.body(), form.sample()) {
                            app.act(Action::PreviewWebhook {
                                body: Box::new(body),
                                sample,
                                headers,
                            });
                        }
                    })
                    .build(p.k)
                    .key("webhook-preview-run"),
            ),
        );
    if let Some(result) = &app.remote.admin.webhook_preview {
        column = column
            .child(outcome("filter", &result.filter, p))
            .child(outcome("dedupe", &result.dedupe, p))
            .child(outcome("derive", &result.derive, p));
    }
    column
}

fn outcome(label: &str, result: &PreviewResult, p: &Paint) -> El {
    let (shown, color) = match (&result.error, &result.value) {
        (Some(error), _) => (error.clone(), p.fail),
        (None, Some(value)) => (value.to_string(), p.c.text_base),
        (None, None) => ("null".to_string(), p.c.text_muted),
    };
    row()
        .key(format!("webhook-preview-{label}"))
        .w_full()
        .gap(10.0)
        .child(
            text(label.to_string())
                .font(p.mono(12.0, 520.0))
                .color(p.c.text_muted)
                .w(64.0),
        )
        .child(
            text(shown)
                .font(p.mono(12.0, 480.0))
                .color(color)
                .selectable(true)
                .wrap()
                .grow(1.0)
                .min_w(0.0),
        )
}
