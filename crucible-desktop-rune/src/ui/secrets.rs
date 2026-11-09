//! The Secrets page: the secrets the caller owns, and for the one picked its bindings, a rotate
//! form and its audit trail. Values never reach the app except as typed into the rotate field.

use rune::{El, Tx, col, row, text};
use rune_kit::{Button, ButtonVariant, SidebarRow, TextInput};

use crate::App;
use crate::admin::{Action, SecretView, relative};
use crate::ui::desk::{card, frame, heading, landed, list, none, now, reading};
use crate::ui::{Paint, tag};

const LIST: f32 = 300.0;

pub(crate) fn page(app: &App, p: &Paint) -> El {
    let note = match &app.admin.secrets {
        Some(Ok(secrets)) => secrets.len().to_string(),
        _ => String::new(),
    };
    let page = frame("secrets", "Secrets", note, app, p);
    let secrets = match landed(&app.admin.secrets, "the secrets", p) {
        Err(standin) => return page.child(*standin),
        Ok(secrets) => secrets,
    };
    if secrets.is_empty() {
        return page.child(none("secrets-none", "No secrets", p));
    }
    let mut sorted: Vec<&crux::dto::Secret> = secrets.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.owner.cmp(&b.owner)));
    let rows = sorted
        .into_iter()
        .map(|secret| {
            let id = secret.id.clone();
            SidebarRow::new(secret.name.clone())
                .subtitle(format!("{} \u{b7} {}", secret.kind, secret.owner))
                .height(46.0)
                .selected(app.desk.secret.as_deref() == Some(secret.id.as_str()))
                .on_select(move |app: &mut App, _: &mut Tx| app.desk.pick_secret(&id))
                .build(p.k)
                .key(format!("secret-{}", secret.id))
        })
        .collect();
    let picked = match (&app.desk.secret, &app.admin.secret) {
        (Some(id), Some((read, loaded))) if id == read => match landed(loaded, "the secret", p) {
            Ok(view) => detail(view, app, p),
            Err(standin) => *standin,
        },
        (Some(_), _) => reading("the secret", p),
        (None, _) => none("secret-unpicked", "Pick a secret", p),
    };
    page.child(
        row()
            .key("secrets-body")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .gap(16.0)
            .child(
                card("secrets-list-card", list("secrets-list", rows, p), p)
                    .w(LIST)
                    .h_full(),
            )
            .child(
                col()
                    .key("secret-detail")
                    .grow(1.0)
                    .min_w(0.0)
                    .h_full()
                    .min_h(0.0)
                    .gap(16.0)
                    .child(picked),
            ),
    )
}

fn detail(view: &SecretView, app: &App, p: &Paint) -> El {
    let secret = &view.detail.secret;
    let mut facts = vec![
        secret.kind.clone(),
        secret.visibility.clone(),
        secret.mode.clone(),
    ];
    if let Some(version) = secret.current_version {
        facts.push(format!("v{version}"));
    }
    if !secret.updated_at.is_empty() {
        facts.push(format!("updated {}", relative(&secret.updated_at, now())));
    }
    let rotatable = secret.mode == "managed";
    let mut head = row().w_full().gap(10.0).center_y().child(
        col()
            .grow(1.0)
            .min_w(0.0)
            .gap(4.0)
            .child(
                row()
                    .gap(8.0)
                    .center_y()
                    .child(
                        text(secret.name.clone())
                            .font(p.k.ty.ui_medium)
                            .color(p.c.text_base),
                    )
                    .child(tag(&secret.owner, p)),
            )
            .child(
                text(facts.join(" \u{b7} "))
                    .font(p.k.ty.small)
                    .color(p.c.text_muted),
            ),
    );
    if rotatable && app.desk.rotate.is_none() {
        head = head.child(
            Button::new("Rotate")
                .variant(ButtonVariant::Neutral)
                .disabled(app.desk.busy())
                .on_press(|app: &mut App| app.desk.rotate = Some(rune::TextEdit::new("")))
                .build(p.k)
                .key("secret-rotate"),
        );
    }
    let mut body = col().w_full().gap(14.0).child(head);
    if let Some(value) = &app.desk.rotate {
        body = body.child(rotate_form(&secret.id, value, app, p));
    }
    body = body.child(bindings(view, app, p));
    col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(16.0)
        .child(card("secret-card", body, p))
        .child(card("secret-audit-card", audit(view, p), p).grow(1.0))
}

fn rotate_form(id: &str, value: &rune::TextEdit, app: &App, p: &Paint) -> El {
    let ready = !value.text.is_empty() && !app.desk.busy();
    let save = {
        let id = id.to_string();
        move |app: &mut App| {
            let Some(value) = app.desk.rotate.as_ref().map(|edit| edit.text.clone()) else {
                return;
            };
            if value.is_empty() {
                return;
            }
            app.act(Action::Rotate {
                secret: id.clone(),
                value,
            });
        }
    };
    let submit = save.clone();
    row()
        .key("secret-rotate-form")
        .w_full()
        .gap(8.0)
        .center_y()
        .child(
            TextInput::new(value)
                .secure()
                .placeholder("New value")
                .width(Some(320.0))
                .on_edit(|app: &mut App| match &mut app.desk.rotate {
                    Some(edit) => edit,
                    None => &mut app.scratch,
                })
                .on_submit(move |app: &mut App, _: &mut Tx| submit(app))
                .build(p.k)
                .key("secret-rotate-value"),
        )
        .child(
            Button::new("Save")
                .variant(ButtonVariant::Submit)
                .disabled(!ready)
                .on_press(save)
                .build(p.k)
                .key("secret-rotate-save"),
        )
        .child(
            Button::new("Cancel")
                .variant(ButtonVariant::Ghost)
                .on_press(|app: &mut App| app.desk.rotate = None)
                .build(p.k)
                .key("secret-rotate-cancel"),
        )
}

fn bindings(view: &SecretView, app: &App, p: &Paint) -> El {
    let mut column = col()
        .key("secret-bindings")
        .w_full()
        .gap(6.0)
        .child(heading("Bound at", p));
    if view.detail.bindings.is_empty() {
        return column.child(none("secret-bindings-none", "Nowhere", p));
    }
    for binding in &view.detail.bindings {
        let mut projected = format!("{} {}", binding.projection_kind, binding.projection);
        if !binding.declared_name.is_empty() {
            projected = format!("{} \u{2192} {projected}", binding.declared_name);
        }
        let (secret, id) = (binding.secret_id.clone(), binding.id.clone());
        let armed = app.desk.armed.as_deref() == Some(binding.id.as_str());
        column = column.child(
            row()
                .key(format!("binding-{}", binding.id))
                .w_full()
                .gap(10.0)
                .center_y()
                .child(
                    text(format!("{}:{}", binding.scope_kind, binding.scope_id))
                        .font(p.mono(12.0, 520.0))
                        .color(p.c.text_base),
                )
                .child(
                    text(projected)
                        .font(p.mono(12.0, 420.0))
                        .color(p.c.text_muted)
                        .grow(1.0)
                        .min_w(0.0)
                        .truncate(),
                )
                .child(
                    Button::new(if armed { "Confirm unbind" } else { "Unbind" })
                        .variant(if armed {
                            ButtonVariant::Danger
                        } else {
                            ButtonVariant::Ghost
                        })
                        .disabled(app.desk.busy())
                        .on_press(move |app: &mut App| {
                            if app.desk.armed.as_deref() == Some(id.as_str()) {
                                app.act(Action::Unbind {
                                    secret: secret.clone(),
                                    binding: id.clone(),
                                })
                            } else {
                                app.desk.armed = Some(id.clone());
                            }
                        })
                        .build(p.k)
                        .key(format!("unbind-{}", binding.id)),
                ),
        );
    }
    column
}

fn audit(view: &SecretView, p: &Paint) -> El {
    let now = now();
    let rows: Vec<El> = view
        .audit
        .iter()
        .map(|entry| {
            let mut what = vec![entry.action.clone()];
            what.extend(entry.subject.clone());
            what.extend(entry.detail.clone());
            row()
                .key(format!("audit-{}", entry.id))
                .w_full()
                .py(6.0)
                .gap(10.0)
                .center_y()
                .child(
                    text(relative(&entry.at, now))
                        .font(p.k.ty.small)
                        .color(p.c.text_faint)
                        .w(64.0),
                )
                .child(
                    text(what.join(" \u{b7} "))
                        .font(p.k.ty.ui)
                        .color(p.c.text_base)
                        .grow(1.0)
                        .min_w(0.0)
                        .truncate(),
                )
                .child(
                    text(entry.actor.clone().unwrap_or_default())
                        .font(p.k.ty.small)
                        .color(p.c.text_muted),
                )
        })
        .collect();
    let body = if rows.is_empty() {
        none("audit-none", "No entries", p)
    } else {
        list("secret-audit", rows, p)
    };
    col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(6.0)
        .child(heading("Audit", p))
        .child(body)
}
