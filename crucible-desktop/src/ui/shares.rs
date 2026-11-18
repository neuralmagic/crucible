//! Who else may use the playbook on screen, and a form to share it with one more.

use rune::{El, Tx, col, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, SegmentedControl, TextInput};

use crate::App;
use crate::admin::{Action, ROLES, not_after, valid_grantee};
use crate::ui::desk::{heading, landed, none};
use crate::ui::{Paint, tag};

const WIDTH: f32 = 268.0;

pub(crate) fn section(app: &App, p: &Paint) -> Option<El> {
    let of = app.shared_here()?;
    let loaded = match &app.remote.admin.shares {
        Some((read, loaded)) if *read == of => loaded,
        _ => &None,
    };
    let shares = match landed(loaded, "the shares", p) {
        Ok(shares) => shares,
        Err(standin) => return Some(*standin),
    };
    let mut sorted: Vec<&crux::dto::Share> = shares.iter().collect();
    sorted.sort_by(|a, b| {
        a.expired
            .cmp(&b.expired)
            .then_with(|| a.grantee.cmp(&b.grantee))
    });
    let mut column = col().key("shares").w(WIDTH).gap(8.0);
    if sorted.is_empty() {
        column = column.child(none("shares-none", "Only you", p));
    }
    for share in sorted {
        column = column.child(share_row(share, &of, app, p));
    }
    Some(column.child(if app.remote.desk.sharing {
        form(of, app, p)
    } else {
        row().child(
            Button::new("Share")
                .variant(ButtonVariant::Neutral)
                .control_size(ControlSize::Small)
                .on_press(|app: &mut App| app.remote.desk.sharing = true)
                .build(p.k)
                .key("share-open"),
        )
    }))
}

fn share_row(share: &crux::dto::Share, of: &crux::client::Shared, app: &App, p: &Paint) -> El {
    let until = match (&share.not_after, share.expired) {
        (_, true) => "expired".to_string(),
        (Some(at), false) => format!("until {}", at.get(..10).unwrap_or(at)),
        (None, false) => String::new(),
    };
    let key = format!("share:{}", share.grantee);
    let armed = app.remote.desk.armed.as_deref() == Some(key.as_str());
    let (of, grantee) = (of.clone(), share.grantee.clone());
    row()
        .key(format!("share-{}", share.grantee))
        .w_full()
        .gap(6.0)
        .center_y()
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(2.0)
                .child(
                    text(share.grantee.clone())
                        .font(p.mono(12.0, 520.0))
                        .color(if share.expired {
                            p.c.text_faint
                        } else {
                            p.c.text_base
                        })
                        .truncate(),
                )
                .children(
                    (!until.is_empty())
                        .then(|| text(until).font(p.k.ty.small).color(p.c.text_muted)),
                ),
        )
        .child(tag(&share.role, p))
        .child(
            Button::new(if armed { "Confirm" } else { "Revoke" })
                .variant(if armed {
                    ButtonVariant::Danger
                } else {
                    ButtonVariant::GhostMuted
                })
                .disabled(app.remote.desk.busy())
                .on_press(move |app: &mut App| {
                    if app.remote.desk.armed.as_deref() == Some(key.as_str()) {
                        app.act(Action::Revoke {
                            of: of.clone(),
                            grantee: grantee.clone(),
                        })
                    } else {
                        app.remote.desk.armed = Some(key.clone());
                    }
                })
                .build(p.k)
                .key(format!("revoke-{}", share.grantee)),
        )
}

fn form(of: crux::client::Shared, app: &App, p: &Paint) -> El {
    let grantee = app.remote.desk.grantee.text.trim().to_string();
    let until = not_after(&app.remote.desk.until.text);
    let ready = valid_grantee(&grantee) && until.is_ok() && !app.remote.desk.busy();
    let share = move |app: &mut App| {
        let grantee = app.remote.desk.grantee.text.trim().to_string();
        let Ok(not_after) = not_after(&app.remote.desk.until.text) else {
            return;
        };
        if !valid_grantee(&grantee) {
            return;
        }
        app.act(Action::Grant {
            of: of.clone(),
            grantee,
            role: ROLES[app.remote.desk.role.min(ROLES.len() - 1)].to_string(),
            not_after,
        });
    };
    let submit = share.clone();
    let mut column = col()
        .key("share-form")
        .w_full()
        .gap(8.0)
        .pt(6.0)
        .child(heading("Share with", p))
        .child(
            TextInput::new(&app.remote.desk.grantee)
                .placeholder("user:login or team:slug")
                .width(Some(WIDTH))
                .invalid(!grantee.is_empty() && !valid_grantee(&grantee))
                .on_edit(|app: &mut App| &mut app.remote.desk.grantee)
                .on_submit(move |app: &mut App, _: &mut Tx| submit(app))
                .build(p.k)
                .key("share-grantee"),
        )
        .children(crate::ui::desk::suggestions(
            "share-grantee-suggest",
            &app.remote.desk.grantee.text,
            crate::principals::Want::Principal,
            |app: &mut App, picked: &str| {
                app.remote.desk.grantee = rune::TextEdit::new(picked);
            },
            app,
            p,
        ))
        .child(
            SegmentedControl::new(&ROLES, Some(app.remote.desk.role))
                .label("Role")
                .width(Some(WIDTH))
                .on_pick(|app: &mut App, i: usize, _: &mut Tx| app.remote.desk.role = i)
                .build(p.k)
                .key("share-role"),
        )
        .child(
            row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(
                    TextInput::new(&app.remote.desk.until)
                        .placeholder("Until YYYY-MM-DD")
                        .invalid(until.is_err())
                        .on_edit(|app: &mut App| &mut app.remote.desk.until)
                        .build(p.k)
                        .key("share-until")
                        .grow(1.0)
                        .min_w(0.0),
                )
                .child(
                    Button::new("Cancel")
                        .variant(ButtonVariant::Ghost)
                        .on_press(|app: &mut App| app.remote.desk.sharing = false)
                        .build(p.k)
                        .key("share-cancel"),
                )
                .child(
                    Button::new("Share")
                        .variant(ButtonVariant::Submit)
                        .disabled(!ready)
                        .on_press(share)
                        .build(p.k)
                        .key("share-submit"),
                ),
        );
    if let Err(why) = until {
        column = column.child(text(why).font(p.k.ty.small).color(p.fail));
    }
    column
}
