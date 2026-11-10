//! The Triggers page: what launches playbooks without a person, schedules then webhooks.

use std::collections::BTreeMap;

use crux::dto::{Webhook, WebhookDelivery, WebhookSecret};
use rune::{El, col, el, row, text};
use rune_kit::{Button, ButtonVariant, Switch};

use crate::admin::{Action, relative};
use crate::ui::desk::{card, frame, heading, landed, none, now, reading, scrolled, stacked};
use crate::ui::{Paint, Tone, flag, schedules, tag, webhook_form};
use crate::webhook::WebhookForm;
use crate::{App, Selection};

pub(crate) fn page(app: &App, p: &Paint) -> El {
    let admin = &app.remote.admin;
    let count = |one: &str, n: Option<usize>| {
        n.map(|n| format!("{n} {one}{}", if n == 1 { "" } else { "s" }))
    };
    let schedules = admin.schedules.as_ref().and_then(|s| s.as_ref().ok());
    let hooks = admin.webhooks.as_ref().and_then(|w| w.as_ref().ok());
    let note: Vec<String> = [
        count("schedule", schedules.map(Vec::len)),
        count("webhook", hooks.map(Vec::len)),
    ]
    .into_iter()
    .flatten()
    .collect();
    let create = Button::new("New webhook")
        .variant(ButtonVariant::Neutral)
        .disabled(app.remote.desk.webhook_form.is_some() || app.remote.registered.is_empty())
        .on_press(|app: &mut App| {
            if let Some(first) = app.remote.registered.first() {
                app.remote.desk.webhook_form = Some(WebhookForm::new(first));
            }
        })
        .build(p.k)
        .key("webhook-new");
    let page = frame(
        "triggers",
        "Triggers",
        note.join(" \u{b7} "),
        Some(create),
        app,
        p,
    );
    if let Some(form) = &app.remote.desk.webhook_form {
        return page.child(scrolled(
            "webhook-form-scroll",
            webhook_form::form(form, app, p),
            p,
        ));
    }
    page.child(scrolled(
        "triggers-scroll",
        col()
            .key("triggers-body")
            .w_full()
            .gap(16.0)
            .child(schedules::section(app, p))
            .child(webhooks(app, p)),
        p,
    ))
}

fn webhooks(app: &App, p: &Paint) -> El {
    let body = col()
        .key("webhooks")
        .w_full()
        .gap(6.0)
        .child(heading("Webhooks", p));
    let hooks = match landed(&app.remote.admin.webhooks, "the webhooks", p) {
        Err(standin) => return card("webhooks-card", body.child(*standin), p),
        Ok(hooks) => hooks,
    };
    if hooks.is_empty() {
        return card(
            "webhooks-card",
            body.child(none("webhooks-none", "None", p)),
            p,
        );
    }
    let mut sorted: Vec<&Webhook> = hooks.iter().collect();
    sorted.sort_by(|a, b| {
        b.enabled
            .cmp(&a.enabled)
            .then_with(|| a.playbook.cmp(&b.playbook))
            .then_with(|| a.id.cmp(&b.id))
    });
    let now = now();
    let rows = sorted
        .into_iter()
        .map(|hook| {
            let open = app.remote.desk.webhook.as_deref() == Some(hook.id.as_str());
            let mut block = col()
                .key(format!("webhook-{}", hook.id))
                .w_full()
                .child(webhook_row(hook, open, app, now, p));
            if open {
                block = block.child(detail(hook, app, now, p));
            }
            block
        })
        .collect();
    card(
        "webhooks-card",
        body.child(stacked("webhooks-list", rows, p)),
        p,
    )
}

fn webhook_row(hook: &Webhook, open: bool, app: &App, now: i64, p: &Paint) -> El {
    let (id, enabled) = (hook.id.clone(), hook.enabled);
    let switch = Switch::new(enabled)
        .label(format!("{} webhook enabled", hook.playbook))
        .hide_label()
        .disabled(app.remote.desk.busy())
        .on_toggle(move |app: &mut App| {
            app.act(Action::EnableWebhook {
                id: id.clone(),
                on: !enabled,
            })
        })
        .build(p.k)
        .key(format!("webhook-switch-{}", hook.id));

    let playbook = hook.playbook.clone();
    let mut title = row()
        .gap(8.0)
        .center_y()
        .child(
            text(hook.playbook.clone())
                .key(format!("webhook-playbook-{}", hook.id))
                .font(p.k.ty.ui_medium)
                .color(p.c.text_base)
                .role("link")
                .focusable(true)
                .hover(p.c.hover)
                .help("Open its runs")
                .on_press(move |app: &mut App| app.select(Selection::playbook(&playbook))),
        )
        .child(tag(&hook.verifier.replace('_', " "), p));
    if hook.consecutive_failures > 0 {
        let failures = hook.consecutive_failures;
        title = title.child(flag(
            &format!(
                "{failures} failed deliver{}",
                if failures == 1 { "y" } else { "ies" }
            ),
            if enabled { Tone::Warning } else { Tone::Danger },
            None,
            p,
        ));
    }

    let mut caps = Vec::new();
    if hook.max_launches_per_hour > 0 {
        caps.push(format!("{}/hour", hook.max_launches_per_hour));
    }
    if hook.max_cost > 0.0 {
        caps.push(format!("${:.2}", hook.max_cost));
    }
    if !hook.max_time.is_empty() {
        caps.push(hook.max_time.clone());
    }
    if let Some(target) = &hook.dispatch_target {
        caps.push(target.clone());
    }
    if let Some(owner) = &hook.owner_principal {
        caps.push(owner.clone());
    }

    let state = if enabled { "listening" } else { "paused" };
    let last = hook
        .last_delivery_at
        .as_deref()
        .map_or("no deliveries".to_string(), |at| {
            format!("delivered {}", relative(at, now))
        });
    let picked = hook.id.clone();
    row()
        .w_full()
        .py(10.0)
        .gap(14.0)
        .center_y()
        .child(switch)
        .child(
            col().grow(1.0).min_w(0.0).gap(4.0).child(title).child(
                text(caps.join(" \u{b7} "))
                    .font(p.mono(12.0, 420.0))
                    .color(p.c.text_muted)
                    .truncate(),
            ),
        )
        .child(
            col()
                .gap(4.0)
                .items_end()
                .child(text(state).font(p.k.ty.ui).color(if enabled {
                    p.c.text_base
                } else {
                    p.c.text_faint
                }))
                .child(text(last).font(p.k.ty.small).color(p.c.text_muted)),
        )
        .child(
            Button::new(if open { "Hide" } else { "Details" })
                .variant(ButtonVariant::GhostMuted)
                .on_press(move |app: &mut App| {
                    app.remote.desk.armed = None;
                    app.remote.desk.webhook = if open { None } else { Some(picked.clone()) };
                })
                .build(p.k)
                .key(format!("webhook-open-{}", hook.id)),
        )
}

fn mono(key: String, value: String, p: &Paint) -> El {
    text(value)
        .key(key)
        .font(p.mono(12.0, 480.0))
        .color(p.c.text_base)
        .selectable(true)
        .wrap()
}

fn fact(label: &str, key: String, value: String, p: &Paint) -> El {
    col()
        .w_full()
        .gap(4.0)
        .child(heading(label, p))
        .child(mono(key, value, p))
}

fn pairs(map: &BTreeMap<String, String>) -> String {
    map.iter()
        .map(|(name, value)| format!("{name} = {value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn detail(hook: &Webhook, app: &App, now: i64, p: &Paint) -> El {
    let id = &hook.id;
    let mut body = col()
        .key(format!("webhook-detail-{id}"))
        .w_full()
        .pb(12.0)
        .pl(52.0)
        .gap(12.0);
    if let Some(rotated) = app
        .remote
        .admin
        .rotated
        .as_ref()
        .filter(|rotated| &rotated.webhook.id == id)
    {
        body = body.child(secret(rotated, p));
    }
    if let Some(url) = &hook.delivery_url {
        body = body.child(fact("Address", format!("webhook-url-{id}"), url.clone(), p));
    }
    if let Some(filter) = hook.filter.as_ref().filter(|f| !f.is_empty()) {
        body = body.child(fact(
            "Filter",
            format!("webhook-filter-{id}"),
            filter.clone(),
            p,
        ));
    }
    if let Some(dedupe) = hook.dedupe.as_ref().filter(|d| !d.is_empty()) {
        body = body.child(fact(
            "Dedupe",
            format!("webhook-dedupe-{id}"),
            dedupe.clone(),
            p,
        ));
    }
    if !hook.derive.is_empty() {
        body = body.child(fact(
            "Derived params",
            format!("webhook-derive-{id}"),
            pairs(&hook.derive),
            p,
        ));
    }
    if !hook.params.is_empty() {
        body = body.child(fact(
            "Fixed params",
            format!("webhook-params-{id}"),
            pairs(&hook.params),
            p,
        ));
    }
    body.child(deliveries(hook, app, now, p))
        .child(controls(hook, app, p))
}

fn secret(rotated: &WebhookSecret, p: &Paint) -> El {
    let id = &rotated.webhook.id;
    let mut shown = col()
        .key(format!("webhook-secret-{id}"))
        .w_full()
        .padding(12.0)
        .gap(8.0)
        .radius(p.k.radius.md)
        .fill(p.c.layer_02)
        .border(p.hair, p.c.border_muted)
        .child(heading("Shown once", p))
        .child(mono(
            format!("webhook-secret-value-{id}"),
            rotated.secret.clone(),
            p,
        ));
    if let Some(url) = &rotated.delivery_url {
        shown = shown.child(mono(format!("webhook-secret-url-{id}"), url.clone(), p));
    }
    shown
}

fn deliveries(hook: &Webhook, app: &App, now: i64, p: &Paint) -> El {
    let column = col().w_full().gap(4.0).child(heading("Deliveries", p));
    let list = match &app.remote.admin.deliveries {
        Some((id, loaded)) if id == &hook.id => match landed(loaded, "the deliveries", p) {
            Ok(list) => list,
            Err(standin) => return column.child(*standin),
        },
        _ => return column.child(reading("the deliveries", p)),
    };
    if list.is_empty() {
        return column.child(none(
            &format!("webhook-deliveries-none-{}", hook.id),
            "None kept",
            p,
        ));
    }
    let rows = list
        .iter()
        .map(|delivery| delivery_row(delivery, now, p))
        .collect();
    column.child(stacked(&format!("webhook-deliveries-{}", hook.id), rows, p))
}

fn delivery_row(delivery: &WebhookDelivery, now: i64, p: &Paint) -> El {
    let outcome = match delivery.outcome.as_str() {
        "failed" => flag("failed", Tone::Danger, None, p),
        "throttled" => flag("throttled", Tone::Warning, None, p),
        "launched" => flag("launched", Tone::Success, None, p),
        other => tag(other, p),
    };
    let mut line = row()
        .key(format!("delivery-{}", delivery.id))
        .w_full()
        .py(6.0)
        .gap(10.0)
        .center_y()
        .child(outcome);
    line = match (&delivery.launch_key, &delivery.reason) {
        (Some(key), _) => {
            let link = crate::link::Link::parse(&format!("crucible://playbook-runs/{key}"));
            let label = text(key.clone())
                .font(p.mono(12.0, 420.0))
                .color(p.c.text_base)
                .grow(1.0)
                .min_w(0.0)
                .truncate();
            line.child(match link {
                Some(link) => label
                    .role("link")
                    .focusable(true)
                    .hover(p.c.hover)
                    .on_press(move |app: &mut App| app.follow(&link)),
                None => label,
            })
        }
        (None, Some(reason)) => line.child(
            text(reason.clone())
                .font(p.mono(12.0, 420.0))
                .color(p.c.text_muted)
                .grow(1.0)
                .min_w(0.0)
                .truncate()
                .help(reason.clone()),
        ),
        (None, None) => line.child(el().grow(1.0)),
    };
    line.child(
        text(relative(&delivery.received_at, now))
            .font(p.mono(12.0, 420.0))
            .color(p.c.text_muted),
    )
}

fn controls(hook: &Webhook, app: &App, p: &Paint) -> El {
    let armed = |what: &str| format!("webhook-{what}:{}", hook.id);
    let button = |label: &'static str, confirm: &'static str, key: String, action: Action| {
        let on = app.remote.desk.armed.as_deref() == Some(key.as_str());
        let test_key = key.replace(':', "-");
        Button::new(if on { confirm } else { label })
            .variant(if on {
                ButtonVariant::Danger
            } else {
                ButtonVariant::Ghost
            })
            .disabled(app.remote.desk.busy())
            .on_press(move |app: &mut App| {
                if app.remote.desk.armed.as_deref() == Some(key.as_str()) {
                    app.act(action.clone());
                } else {
                    app.remote.desk.armed = Some(key.clone());
                }
            })
            .build(p.k)
            .key(test_key)
    };
    let editing = hook.clone();
    row()
        .gap(8.0)
        .child(
            Button::new("Edit")
                .variant(ButtonVariant::Ghost)
                .disabled(app.remote.desk.busy())
                .on_press(move |app: &mut App| {
                    app.remote.desk.webhook_form = Some(WebhookForm::editing(&editing));
                })
                .build(p.k)
                .key(format!("webhook-edit-{}", hook.id)),
        )
        .child(button(
            "Rotate secret",
            "Confirm rotate",
            armed("rotate"),
            Action::RotateWebhook {
                id: hook.id.clone(),
            },
        ))
        .child(button(
            "Delete",
            "Confirm delete",
            armed("delete"),
            Action::DeleteWebhook {
                id: hook.id.clone(),
            },
        ))
}
