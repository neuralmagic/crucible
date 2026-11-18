//! The Admin page's policy tab: the stored policy sets, storing a new one, activating one, and
//! asking how the active set decides an action for a user.

use crux::dto::{Explanation, PolicySet};
use rune::{El, Tx, col, el, row, text};
use rune_kit::{Button, ButtonVariant, SidebarRow, TextArea, TextInput};

use crate::App;
use crate::admin::{Action, relative};
use crate::principals::Want;
use crate::ui::desk::{card, confirm, field, heading, landed, mono, none, now, split, suggestions};
use crate::ui::{Paint, tag};

const LIST: f32 = 300.0;

fn short(digest: &str) -> &str {
    digest.get(..12).unwrap_or(digest)
}

pub(crate) fn tab(app: &App, p: &Paint) -> El {
    let sets = match landed(&app.remote.admin.policy_sets, "the policy sets", p) {
        Err(standin) => return *standin,
        Ok(sets) => sets,
    };
    let now = now();
    let picked = app
        .remote
        .desk
        .policy
        .as_ref()
        .and_then(|digest| sets.iter().find(|set| &set.digest == digest))
        .or_else(|| sets.iter().find(|set| set.active));
    let rows = sets
        .iter()
        .map(|set| {
            let digest = set.digest.clone();
            let by = set.created_by.as_deref().unwrap_or("configuration");
            let state = if set.active { "active \u{b7} " } else { "" };
            SidebarRow::new(short(&set.digest).to_string())
                .subtitle(format!(
                    "{state}{} \u{b7} {by}",
                    relative(&set.created_at, now)
                ))
                .height(46.0)
                .selected(
                    app.remote.desk.policy_draft.is_none()
                        && picked.is_some_and(|shown| shown.digest == set.digest),
                )
                .on_select(move |app: &mut App, _: &mut Tx| {
                    app.remote.desk.armed = None;
                    app.remote.desk.policy_draft = None;
                    app.remote.desk.policy = Some(digest.clone());
                })
                .build(p.k)
                .key(format!("policy-{}", short(&set.digest)))
        })
        .collect();
    let shown = col()
        .w_full()
        .gap(16.0)
        .child(card("policy-explain-card", explain(app, p), p))
        .children(
            app.remote
                .desk
                .policy_file
                .as_ref()
                .map(|path| external(path, app, p)),
        )
        .child(match (&app.remote.desk.policy_draft, picked) {
            (Some(draft), _) => editor(draft, app, p),
            (None, Some(set)) => detail(set, app, p),
            (None, None) => none("policy-none", "No policy sets", p),
        });
    split("policy", rows, LIST, shown, p)
}

fn detail(set: &PolicySet, app: &App, p: &Paint) -> El {
    let (digest, source) = (set.digest.clone(), set.text.clone());
    let mut head = row()
        .w_full()
        .gap(10.0)
        .center_y()
        .child(
            text(short(&set.digest).to_string())
                .font(p.mono(12.0, 520.0))
                .color(p.c.text_base)
                .help(set.digest.clone()),
        )
        .children(set.active.then(|| tag("active", p)))
        .child(el().grow(1.0))
        .child(
            Button::new("Edit as new")
                .variant(ButtonVariant::Ghost)
                .disabled(app.remote.desk.busy())
                .on_press(move |app: &mut App| app.edit_policy(&digest, &source))
                .build(p.k)
                .key("policy-edit"),
        );
    if !set.active {
        head = head.child(confirm(
            "policy-activate",
            &format!("policy-activate:{}", set.digest),
            ("Activate", "Confirm activate"),
            true,
            Action::ActivatePolicy {
                digest: set.digest.clone(),
            },
            app,
            p,
        ));
    }
    card(
        "policy-card",
        col()
            .key(format!("policy-shown-{}", short(&set.digest)))
            .w_full()
            .gap(12.0)
            .child(head)
            .child(mono("policy-text", set.text.clone(), p)),
        p,
    )
}

/// The scratch copy open in the user's editor, to store once they have saved it there.
fn external(path: &std::path::Path, app: &App, p: &Paint) -> El {
    let reopen = path.to_path_buf();
    card(
        "policy-file-card",
        col()
            .key("policy-file")
            .w_full()
            .gap(10.0)
            .child(heading("New policy set, in your editor", p))
            .child(mono("policy-file-path", path.display().to_string(), p))
            .child(
                row()
                    .gap(8.0)
                    .child(
                        Button::new("Store")
                            .variant(ButtonVariant::Submit)
                            .disabled(app.remote.desk.busy())
                            .on_press(|app: &mut App| app.store_policy_file())
                            .build(p.k)
                            .key("policy-file-store"),
                    )
                    .child(
                        Button::new("Reopen")
                            .variant(ButtonVariant::Ghost)
                            .on_press(move |app: &mut App| app.open_path_in_editor(&reopen, 1))
                            .build(p.k)
                            .key("policy-file-reopen"),
                    )
                    .child(
                        Button::new("Cancel")
                            .variant(ButtonVariant::Ghost)
                            .on_press(|app: &mut App| app.remote.desk.policy_file = None)
                            .build(p.k)
                            .key("policy-file-cancel"),
                    ),
            ),
        p,
    )
}

fn editor(draft: &rune::TextEdit, app: &App, p: &Paint) -> El {
    let ready = !draft.text.trim().is_empty() && !app.remote.desk.busy();
    card(
        "policy-editor-card",
        col()
            .key("policy-editor")
            .w_full()
            .gap(12.0)
            .child(heading("New policy set", p))
            .child(
                TextArea::new(draft)
                    .rows(28)
                    .width(None)
                    .on_edit(|app: &mut App| match &mut app.remote.desk.policy_draft {
                        Some(draft) => draft,
                        None => &mut app.scratch,
                    })
                    .build(p.k)
                    .key("policy-draft"),
            )
            .child(
                row()
                    .gap(8.0)
                    .child(
                        Button::new("Store")
                            .variant(ButtonVariant::Submit)
                            .disabled(!ready)
                            .on_press(|app: &mut App| {
                                if let Some(draft) = &app.remote.desk.policy_draft {
                                    let text = draft.text.clone();
                                    app.act(Action::CreatePolicy { text });
                                }
                            })
                            .build(p.k)
                            .key("policy-store"),
                    )
                    .child(
                        Button::new("Cancel")
                            .variant(ButtonVariant::Ghost)
                            .on_press(|app: &mut App| app.remote.desk.policy_draft = None)
                            .build(p.k)
                            .key("policy-cancel"),
                    ),
            ),
        p,
    )
}

fn explain(app: &App, p: &Paint) -> El {
    let (login, action, resource) = &app.remote.desk.explain;
    let ready =
        !login.text.trim().is_empty() && !action.text.trim().is_empty() && !app.remote.desk.busy();
    let input = |label: &str,
                 key: &str,
                 value: &rune::TextEdit,
                 placeholder: &str,
                 pick: fn(&mut App) -> &mut rune::TextEdit| {
        field(
            label,
            TextInput::new(value)
                .placeholder(placeholder.to_string())
                .width(Some(220.0))
                .on_edit(pick)
                .build(p.k)
                .key(key.to_string()),
            p,
        )
        .w(220.0)
    };
    let mut body = col()
        .key("policy-explain")
        .w_full()
        .gap(10.0)
        .child(heading("Explain", p))
        .child(
            row()
                .w_full()
                .gap(12.0)
                .items_end()
                .flex_wrap()
                .child(input("Login", "explain-login", login, "", |app| {
                    &mut app.remote.desk.explain.0
                }))
                .child(input(
                    "Action",
                    "explain-action",
                    action,
                    "team:read",
                    |app| &mut app.remote.desk.explain.1,
                ))
                .child(input(
                    "Resource",
                    "explain-resource",
                    resource,
                    "any",
                    |app| &mut app.remote.desk.explain.2,
                ))
                .child(
                    Button::new("Explain")
                        .variant(ButtonVariant::Neutral)
                        .disabled(!ready)
                        .on_press(|app: &mut App| {
                            let (login, action, resource) = &app.remote.desk.explain;
                            let resource = resource.text.trim();
                            app.act(Action::Explain {
                                login: login.text.trim().to_string(),
                                action: action.text.trim().to_string(),
                                resource: (!resource.is_empty()).then(|| resource.to_string()),
                            });
                        })
                        .build(p.k)
                        .key("explain-run"),
                ),
        )
        .children(suggestions(
            "explain-login-suggest",
            &login.text,
            Want::Member(crux::dto::MemberKind::User),
            |app: &mut App, picked: &str| {
                app.remote.desk.explain.0 = rune::TextEdit::new(picked);
            },
            app,
            p,
        ));
    if let Some(explained) = &app.remote.admin.explained {
        body = body.child(answer(explained, p));
    }
    body
}

fn answer(explained: &Explanation, p: &Paint) -> El {
    let verdict = if explained.allowed {
        "allowed"
    } else {
        "denied"
    };
    let rules = if explained.rules.is_empty() {
        "no rule".to_string()
    } else {
        explained.rules.join(", ")
    };
    row()
        .key("explain-answer")
        .w_full()
        .gap(10.0)
        .child(
            text(format!(
                "{} {verdict} {}",
                explained.login, explained.action
            ))
            .font(p.k.ty.ui_medium)
            .color(if explained.allowed { p.pass } else { p.fail }),
        )
        .child(
            text(rules)
                .font(p.mono(12.0, 420.0))
                .color(p.c.text_muted)
                .selectable(true)
                .wrap()
                .grow(1.0)
                .min_w(0.0),
        )
}
