//! The Providers page: the inference services runs may talk to, and the form that registers or
//! edits one.

use crux::dto::{Provider, ProviderKind};
use rune::{El, TextEdit, Tx, col, el, row, text};
use rune_kit::{Button, ButtonVariant, SidebarRow, Switch, TextArea, TextInput};

use crate::App;
use crate::admin::Action;
use crate::provider::{
    KINDS, PROTOCOLS, ProviderForm, harnesses, kind_label, may, protocol_label, reach,
};
use crate::ui::desk::{card, field, frame, landed, list, none, pills, scrolled};
use crate::ui::{Paint, tag};

const LIST: f32 = 300.0;
const WIDE: f32 = 420.0;

fn edit(app: &mut App, pick: fn(&mut ProviderForm) -> &mut TextEdit) -> &mut TextEdit {
    match &mut app.remote.desk.provider_form {
        Some(form) => pick(form),
        None => &mut app.scratch,
    }
}

fn change(app: &mut App, change: impl FnOnce(&mut ProviderForm)) {
    if let Some(form) = &mut app.remote.desk.provider_form {
        change(form);
    }
}

pub(crate) fn page(app: &App, p: &Paint) -> El {
    let note = match &app.remote.admin.providers {
        Some(Ok(providers)) => {
            let on = providers.iter().filter(|provider| provider.enabled).count();
            format!("{} \u{b7} {on} enabled", providers.len())
        }
        _ => String::new(),
    };
    let create = Button::new("New provider")
        .variant(ButtonVariant::Neutral)
        .disabled(app.remote.desk.provider_form.is_some())
        .on_press(|app: &mut App| {
            let owner = app.owners().into_iter().next().unwrap_or_default();
            app.remote.desk.provider_form = Some(ProviderForm::new(&owner));
        })
        .build(p.k)
        .key("provider-new");
    let page = frame("providers", "Providers", note, Some(create), app, p);
    let providers = match landed(&app.remote.admin.providers, "the providers", p) {
        Err(standin) => return page.child(*standin),
        Ok(providers) => providers,
    };
    let mut sorted: Vec<&Provider> = providers.iter().collect();
    sorted.sort_by(|a, b| b.enabled.cmp(&a.enabled).then_with(|| a.id.cmp(&b.id)));
    let rows = sorted
        .into_iter()
        .map(|provider| {
            let id = provider.id.clone();
            let name = provider
                .display_name
                .clone()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| provider.id.clone());
            let state = if provider.enabled { "" } else { "off \u{b7} " };
            SidebarRow::new(name)
                .subtitle(format!(
                    "{state}{} \u{b7} {}",
                    provider.kind, provider.owner
                ))
                .height(46.0)
                .selected(
                    app.remote.desk.provider_form.is_none()
                        && app.remote.desk.provider.as_deref() == Some(provider.id.as_str()),
                )
                .on_select(move |app: &mut App, _: &mut Tx| {
                    app.remote.desk.armed = None;
                    app.remote.desk.provider_form = None;
                    app.remote.desk.provider = Some(id.clone());
                })
                .build(p.k)
                .key(format!("provider-{}", provider.id))
        })
        .collect();
    let picked = app
        .remote
        .desk
        .provider
        .as_ref()
        .and_then(|id| providers.iter().find(|provider| &provider.id == id));
    let shown = match (&app.remote.desk.provider_form, picked) {
        (Some(form), _) => provider_form(form, app, p),
        (None, Some(provider)) => detail(provider, app, p),
        (None, None) if providers.is_empty() => none("providers-none", "No providers", p),
        (None, None) => none("provider-unpicked", "Pick a provider", p),
    };
    page.child(
        row()
            .key("providers-body")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .gap(16.0)
            .child(
                card("providers-list-card", list("providers-list", rows, p), p)
                    .w(LIST)
                    .h_full(),
            )
            .child(
                col()
                    .key("provider-detail")
                    .grow(1.0)
                    .min_w(0.0)
                    .h_full()
                    .min_h(0.0)
                    .child(scrolled("provider-scroll", shown, p)),
            ),
    )
}

fn fact(label: &str, value: String, p: &Paint) -> El {
    row()
        .w_full()
        .gap(12.0)
        .child(
            text(label.to_string())
                .font(p.k.ty.small_medium)
                .color(p.c.text_muted)
                .w(110.0),
        )
        .child(
            text(value)
                .font(p.mono(12.0, 480.0))
                .color(p.c.text_base)
                .selectable(true)
                .wrap()
                .grow(1.0)
                .min_w(0.0),
        )
}

fn detail(provider: &Provider, app: &App, p: &Paint) -> El {
    let name = provider
        .display_name
        .clone()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| provider.id.clone());
    let mut head = row()
        .w_full()
        .gap(10.0)
        .center_y()
        .child(text(name).font(p.k.ty.ui_medium).color(p.c.text_base))
        .child(
            text(provider.id.clone())
                .font(p.mono(12.0, 420.0))
                .color(p.c.text_faint),
        )
        .children((!provider.enabled).then(|| tag("off", p)))
        .child(el().grow(1.0));
    if may(provider, "update") {
        let (toggled, editing) = (provider.clone(), provider.clone());
        head = head
            .child(
                Switch::new(provider.enabled)
                    .label(format!("{} enabled", provider.id))
                    .hide_label()
                    .disabled(app.remote.desk.busy())
                    .on_toggle(move |app: &mut App| {
                        let mut form = ProviderForm::editing(&toggled);
                        form.enabled = !toggled.enabled;
                        if let Ok(register) = form.register() {
                            app.act(Action::SaveProvider {
                                id: Some(toggled.id.clone()),
                                register: Box::new(register),
                            });
                        }
                    })
                    .build(p.k)
                    .key("provider-enabled"),
            )
            .child(
                Button::new("Edit")
                    .variant(ButtonVariant::Ghost)
                    .disabled(app.remote.desk.busy())
                    .on_press(move |app: &mut App| {
                        app.remote.desk.provider_form = Some(ProviderForm::editing(&editing));
                    })
                    .build(p.k)
                    .key("provider-edit"),
            );
    }
    if may(provider, "delete") {
        let id = provider.id.clone();
        let key = format!("provider-delete:{id}");
        let armed = app.remote.desk.armed.as_deref() == Some(key.as_str());
        head = head.child(
            Button::new(if armed { "Confirm delete" } else { "Delete" })
                .variant(if armed {
                    ButtonVariant::Danger
                } else {
                    ButtonVariant::Ghost
                })
                .disabled(app.remote.desk.busy())
                .on_press(move |app: &mut App| {
                    if app.remote.desk.armed.as_deref() == Some(key.as_str()) {
                        app.act(Action::DeleteProvider { id: id.clone() });
                    } else {
                        app.remote.desk.armed = Some(key.clone());
                    }
                })
                .build(p.k)
                .key("provider-delete"),
        );
    }
    let mut facts = col()
        .w_full()
        .gap(8.0)
        .child(fact("Reach", reach(provider), p))
        .child(fact("Owner", provider.owner.clone(), p))
        .child(fact("Default", provider.default_model.clone(), p));
    if !provider.models.is_empty() {
        facts = facts.child(fact("Models", provider.models.join("\n"), p));
    }
    if let Some(harness) = &provider.harness {
        facts = facts.child(fact("Agent CLI", harness.clone(), p));
    }
    if !provider.roles.is_empty() {
        facts = facts.child(fact("Serves", provider.roles.join(", "), p));
    }
    if let Some(secret) = &provider.secret_name {
        let owner = provider
            .secret_owner
            .as_deref()
            .map_or(String::new(), |owner| format!(" ({owner})"));
        facts = facts.child(fact("Secret", format!("{secret}{owner}"), p));
    }
    card(
        "provider-card",
        col()
            .key(format!("provider-shown-{}", provider.id))
            .w_full()
            .gap(14.0)
            .child(head)
            .child(facts),
        p,
    )
}

fn input(
    label: &str,
    key: &str,
    value: &TextEdit,
    placeholder: &str,
    pick: fn(&mut ProviderForm) -> &mut TextEdit,
    p: &Paint,
) -> El {
    field(
        label,
        TextInput::new(value)
            .placeholder(placeholder.to_string())
            .width(Some(WIDE))
            .on_edit(move |app: &mut App| edit(app, pick))
            .build(p.k)
            .key(key.to_string()),
        p,
    )
}

fn provider_form(form: &ProviderForm, app: &App, p: &Paint) -> El {
    let editing = form.editing.is_some();
    let title = match &form.editing {
        Some(id) => format!("Edit {id}"),
        None => "New provider".to_string(),
    };
    let mut body = col()
        .key("provider-form")
        .w_full()
        .gap(14.0)
        .child(text(title).font(p.k.ty.ui_medium).color(p.c.text_base));
    if !editing {
        let owners: Vec<(String, String)> =
            app.owners().into_iter().map(|o| (o.clone(), o)).collect();
        body = body
            .child(input(
                "Id",
                "provider-id",
                &form.id,
                "lowercase-id",
                |form| &mut form.id,
                p,
            ))
            .child(field(
                "Owner",
                pills(
                    "provider-owner",
                    &owners,
                    Some(&form.owner),
                    |app: &mut App, owner: &str| change(app, |form| form.owner = owner.to_string()),
                    p,
                ),
                p,
            ));
    }
    let kinds: Vec<(String, String)> = KINDS
        .iter()
        .map(|(_, label)| (label.to_string(), label.to_string()))
        .collect();
    body = body
        .child(input(
            "Name",
            "provider-name",
            &form.name,
            "",
            |form| &mut form.name,
            p,
        ))
        .child(field(
            "Kind",
            pills(
                "provider-kind",
                &kinds,
                Some(kind_label(form.kind)),
                |app: &mut App, picked: &str| {
                    if let Some((kind, _)) = KINDS.iter().find(|(_, label)| *label == picked) {
                        change(app, |form| form.set_kind(*kind));
                    }
                },
                p,
            ),
            p,
        ));
    if form.kind == ProviderKind::Custom {
        let protocols: Vec<(String, String)> = PROTOCOLS
            .iter()
            .map(|(_, label)| (label.to_string(), label.replace('_', " ")))
            .collect();
        body = body
            .child(input(
                "Endpoint",
                "provider-endpoint",
                &form.endpoint,
                "https://host/v1",
                |form| &mut form.endpoint,
                p,
            ))
            .child(field(
                "Protocol",
                pills(
                    "provider-protocol",
                    &protocols,
                    Some(protocol_label(form.protocol)),
                    |app: &mut App, picked: &str| {
                        if let Some((protocol, _)) =
                            PROTOCOLS.iter().find(|(_, label)| *label == picked)
                        {
                            change(app, |form| form.set_protocol(*protocol));
                        }
                    },
                    p,
                ),
                p,
            ));
    }
    body = body
        .child(field(
            "Models",
            TextArea::new(&form.models)
                .rows(4)
                .width(Some(WIDE))
                .on_edit(|app: &mut App| edit(app, |form| &mut form.models))
                .build(p.k)
                .key("provider-models"),
            p,
        ))
        .child(input(
            "Default model",
            "provider-default-model",
            &form.default_model,
            "",
            |form| &mut form.default_model,
            p,
        ));
    let allowed = harnesses(form.kind, form.protocol);
    if let Some(default) = allowed.first() {
        let mut options = vec![(String::new(), format!("default ({default})"))];
        options.extend(allowed.iter().map(|h| (h.to_string(), h.to_string())));
        body = body.child(field(
            "Agent CLI",
            pills(
                "provider-harness",
                &options,
                Some(form.harness.unwrap_or("")),
                |app: &mut App, picked: &str| {
                    change(app, |form| {
                        form.harness = harnesses(form.kind, form.protocol)
                            .iter()
                            .copied()
                            .find(|held| *held == picked);
                    });
                },
                p,
            ),
            p,
        ));
    }
    if form.kind != ProviderKind::Vertex {
        body = body
            .child(input(
                "Secret",
                "provider-secret",
                &form.secret,
                "secret name",
                |form| &mut form.secret,
                p,
            ))
            .child(input(
                "Secret owner",
                "provider-secret-owner",
                &form.secret_owner,
                "user:login or team:slug",
                |form| &mut form.secret_owner,
                p,
            ))
            .children(crate::ui::desk::suggestions(
                "provider-secret-owner-suggest",
                &form.secret_owner.text,
                crate::principals::Want::Principal,
                |app: &mut App, picked: &str| {
                    change(app, |form| form.secret_owner = TextEdit::new(picked));
                },
                app,
                p,
            ));
    }
    let asked = form.register();
    let started = !form.name.text.trim().is_empty() || !form.id.text.trim().is_empty();
    body = body
        .child(
            Switch::new(form.enabled)
                .label("Enabled")
                .on_toggle(|app: &mut App| change(app, |form| form.enabled = !form.enabled))
                .build(p.k)
                .key("provider-form-enabled"),
        )
        .children(
            asked
                .as_ref()
                .err()
                .filter(|_| started)
                .map(|why| text(why.clone()).font(p.k.ty.small).color(p.fail)),
        )
        .child(
            row()
                .gap(8.0)
                .child(
                    Button::new(if editing { "Save" } else { "Register" })
                        .variant(ButtonVariant::Submit)
                        .disabled(asked.is_err() || app.remote.desk.busy())
                        .on_press(|app: &mut App| {
                            let Some(form) = &app.remote.desk.provider_form else {
                                return;
                            };
                            if let Ok(register) = form.register() {
                                app.act(Action::SaveProvider {
                                    id: form.editing.clone(),
                                    register: Box::new(register),
                                });
                            }
                        })
                        .build(p.k)
                        .key("provider-save"),
                )
                .child(
                    Button::new("Cancel")
                        .variant(ButtonVariant::Ghost)
                        .on_press(|app: &mut App| app.remote.desk.provider_form = None)
                        .build(p.k)
                        .key("provider-cancel"),
                ),
        );
    card("provider-form-card", body, p)
}
