//! The Blueprints page: playbooks offered as starting points, and starting a draft from one.

use crux::dto::{Blueprint, BlueprintUsed};
use rune::{El, col, el, row, text};
use rune_kit::{Button, ButtonVariant, TextInput};

use crate::App;
use crate::admin::Action;
use crate::ui::desk::{card, fact, field, frame, heading, landed, none, scrolled, stacked};
use crate::ui::{Paint, tag};

pub(crate) fn page(app: &App, p: &Paint) -> El {
    let note = match &app.remote.admin.blueprints {
        Some(Ok(blueprints)) => blueprints.len().to_string(),
        _ => String::new(),
    };
    let page = frame("blueprints", "Blueprints", note, None, app, p);
    let blueprints = match landed(&app.remote.admin.blueprints, "the blueprints", p) {
        Err(standin) => return page.child(*standin),
        Ok(blueprints) => blueprints,
    };
    let mut body = col().key("blueprints-body").w_full().gap(16.0);
    if let Some(used) = &app.remote.admin.used {
        body = body.child(started(used, p));
    }
    if blueprints.is_empty() {
        return page.child(scrolled(
            "blueprints-scroll",
            body.child(none("blueprints-none", "No blueprints", p)),
            p,
        ));
    }
    let mut sorted: Vec<&Blueprint> = blueprints.iter().collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));
    let rows = sorted
        .into_iter()
        .map(|blueprint| blueprint_row(blueprint, app, p))
        .collect();
    page.child(scrolled(
        "blueprints-scroll",
        body.child(card(
            "blueprints-card",
            stacked("blueprints-list", rows, p),
            p,
        )),
        p,
    ))
}

/// What a use would bind for the caller and what it would leave them to bind.
fn secrets(blueprint: &Blueprint) -> String {
    let carried = blueprint.secrets.iter().filter(|s| s.carries).count();
    let left = blueprint.secrets.len() - carried;
    let parts: Vec<String> = [(carried, "carried"), (left, "to bind")]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect();
    if parts.is_empty() {
        "no secrets".to_string()
    } else {
        parts.join(" \u{b7} ")
    }
}

fn blueprint_row(blueprint: &Blueprint, app: &App, p: &Paint) -> El {
    let using = app
        .remote
        .desk
        .use_blueprint
        .as_ref()
        .filter(|(id, _, _)| id == &blueprint.id);
    let id = blueprint.id.clone();
    let mut head = row()
        .w_full()
        .gap(10.0)
        .center_y()
        .child(
            text(blueprint.id.clone())
                .font(p.k.ty.ui_medium)
                .color(p.c.text_base),
        )
        .children(blueprint.public.then(|| tag("public", p)))
        .child(
            text(format!("{} \u{b7} {}", blueprint.owner, secrets(blueprint)))
                .font(p.mono(12.0, 420.0))
                .color(p.c.text_muted)
                .grow(1.0)
                .min_w(0.0)
                .truncate(),
        );
    if using.is_none() {
        head = head.child(
            Button::new("Use")
                .variant(ButtonVariant::Neutral)
                .disabled(app.remote.desk.busy())
                .on_press(move |app: &mut App| {
                    app.remote.desk.use_blueprint =
                        Some((id.clone(), rune::TextEdit::new(""), rune::TextEdit::new("")));
                })
                .build(p.k)
                .key(format!("blueprint-use-{}", blueprint.id)),
        );
    }
    let mut block = col()
        .key(format!("blueprint-{}", blueprint.id))
        .w_full()
        .py(10.0)
        .gap(8.0)
        .child(head);
    if !blueprint.description.is_empty() {
        block = block.child(
            text(blueprint.description.clone())
                .font(p.k.ty.ui)
                .color(p.c.text_muted)
                .wrap(),
        );
    }
    let Some((_, to, description)) = using else {
        return block;
    };
    let ready = !to.text.trim().is_empty() && !app.remote.desk.busy();
    block
        .child(field(
            "Draft id",
            TextInput::new(to)
                .placeholder("lowercase-id")
                .width(Some(320.0))
                .on_edit(|app: &mut App| match &mut app.remote.desk.use_blueprint {
                    Some((_, to, _)) => to,
                    None => &mut app.scratch,
                })
                .build(p.k)
                .key("blueprint-draft-id"),
            p,
        ))
        .child(field(
            "Description",
            TextInput::new(description)
                .width(Some(420.0))
                .on_edit(|app: &mut App| match &mut app.remote.desk.use_blueprint {
                    Some((_, _, description)) => description,
                    None => &mut app.scratch,
                })
                .build(p.k)
                .key("blueprint-draft-description"),
            p,
        ))
        .child(
            row()
                .gap(8.0)
                .child(
                    Button::new("Start draft")
                        .variant(ButtonVariant::Submit)
                        .disabled(!ready)
                        .on_press(|app: &mut App| {
                            let Some((id, to, description)) = &app.remote.desk.use_blueprint else {
                                return;
                            };
                            let description = description.text.trim();
                            app.act(Action::UseBlueprint {
                                id: id.clone(),
                                to: to.text.trim().to_string(),
                                description: (!description.is_empty())
                                    .then(|| description.to_string()),
                            });
                        })
                        .build(p.k)
                        .key("blueprint-start"),
                )
                .child(
                    Button::new("Cancel")
                        .variant(ButtonVariant::Ghost)
                        .on_press(|app: &mut App| app.remote.desk.use_blueprint = None)
                        .build(p.k)
                        .key("blueprint-cancel"),
                ),
        )
}

fn started(used: &BlueprintUsed, p: &Paint) -> El {
    let playbook = used.id.clone();
    let mut body = col().key("blueprint-started").w_full().gap(10.0).child(
        row()
            .w_full()
            .gap(10.0)
            .center_y()
            .child(heading("Started", p))
            .child(
                text(format!("{} v{}", used.id, used.version))
                    .font(p.k.ty.ui_medium)
                    .color(p.c.text_base),
            )
            .child(el().grow(1.0))
            .child(
                Button::new("Open")
                    .variant(ButtonVariant::Neutral)
                    .on_press(move |app: &mut App| {
                        app.select(crate::Selection::playbook(&playbook));
                    })
                    .build(p.k)
                    .key("blueprint-open-draft"),
            ),
    );
    if !used.bindings.is_empty() {
        let carried: Vec<String> = used
            .bindings
            .iter()
            .map(|binding| {
                format!(
                    "{} \u{2192} {} {}",
                    binding.declared_name, binding.projection_kind, binding.projection
                )
            })
            .collect();
        body = body.child(fact("Carried", "blueprint-carried", carried.join("\n"), p));
    }
    if !used.placeholders.is_empty() {
        let left: Vec<String> = used
            .placeholders
            .iter()
            .map(|placeholder| {
                format!(
                    "{} ({})",
                    placeholder.declared_name,
                    placeholder.reason.replace('_', " ")
                )
            })
            .collect();
        body = body.child(fact("To bind", "blueprint-to-bind", left.join("\n"), p));
    }
    card("blueprint-started-card", body, p)
}
