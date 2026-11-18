//! The API keys page: mint a key and see its secret once, revoke one, and sign MCP clients out.

use rune::{El, col, el, row, text};
use rune_kit::{Button, ButtonVariant, Switch, TextInput};

use crate::App;
use crate::admin::{Action, relative};
use crate::apikeys::{KeyState, by_client, mcp_install};
use crate::ui::desk::{
    card, confirm, fact, field, frame, heading, landed, mono, none, now, scrolled, stacked,
};
use crate::ui::{Paint, Tone, flag, tag};

const NAME: f32 = 280.0;
const DAYS: f32 = 120.0;

pub(crate) fn page(app: &App, p: &Paint) -> El {
    let note = match &app.remote.admin.keys {
        Some(Ok(keys)) => {
            let now = now();
            let live = keys
                .iter()
                .filter(|key| KeyState::of(key, now) == KeyState::Live)
                .count();
            format!("{live} active")
        }
        _ => String::new(),
    };
    let page = frame("keys", "API keys", note, None, app, p);
    let keys = match landed(&app.remote.admin.keys, "the API keys", p) {
        Err(standin) => return page.child(*standin),
        Ok(keys) => keys,
    };
    let mut body = col().key("keys-body").w_full().gap(16.0);
    if let Some(minted) = &app.remote.admin.minted {
        body = body.child(minted_card(minted, p));
    }
    body = body
        .child(card("key-form-card", mint_form(app, p), p))
        .child(card("keys-card", key_list(keys, app, p), p));
    if let Some(Ok(mcp)) = &app.remote.admin.mcp
        && mcp.enabled
    {
        body = body.child(card("mcp-card", mcp_clients(&mcp.sign_ins, app, p), p));
    }
    page.child(scrolled("keys-scroll", body, p))
}

fn minted_card(minted: &crux::dto::MintedKey, p: &Paint) -> El {
    let mut body = col()
        .key("key-minted")
        .w_full()
        .gap(8.0)
        .child(heading("Shown once", p))
        .child(mono("key-minted-secret", minted.secret.clone(), p));
    if let Some(url) = &minted.mcp_url {
        body = body.child(fact(
            "MCP client",
            "key-minted-install",
            mcp_install(&minted.secret, url),
            p,
        ));
    }
    card("key-minted-card", body, p)
}

fn mint_form(app: &App, p: &Paint) -> El {
    let form = &app.remote.desk.new_key;
    let asked = form.ask();
    let problem = match &asked {
        Err(why) if !form.name.text.trim().is_empty() => Some(why.clone()),
        _ => None,
    };
    let mut fields = row().w_full().gap(16.0).items_end().child(
        field(
            "Name",
            TextInput::new(&form.name)
                .width(Some(NAME))
                .on_edit(|app: &mut App| &mut app.remote.desk.new_key.name)
                .build(p.k)
                .key("key-name"),
            p,
        )
        .w(NAME),
    );
    if !form.forever {
        fields = fields.child(
            field(
                "Expires in days",
                TextInput::new(&form.days)
                    .width(Some(DAYS))
                    .on_edit(|app: &mut App| &mut app.remote.desk.new_key.days)
                    .build(p.k)
                    .key("key-days"),
                p,
            )
            .w(DAYS),
        );
    }
    col()
        .key("key-form")
        .w_full()
        .gap(12.0)
        .child(fields)
        .child(
            Switch::new(form.forever)
                .label("Never expires")
                .on_toggle(|app: &mut App| {
                    app.remote.desk.new_key.forever = !app.remote.desk.new_key.forever;
                })
                .build(p.k)
                .key("key-forever"),
        )
        .children(problem.map(|why| text(why).font(p.k.ty.small).color(p.fail)))
        .child(
            row().child(
                Button::new("Mint key")
                    .variant(ButtonVariant::Submit)
                    .disabled(asked.is_err() || app.remote.desk.busy())
                    .on_press(|app: &mut App| {
                        if let Ok((name, days)) = app.remote.desk.new_key.ask() {
                            app.act(Action::MintKey { name, days });
                        }
                    })
                    .build(p.k)
                    .key("key-mint"),
            ),
        )
}

fn key_list(keys: &[crux::dto::ApiKey], app: &App, p: &Paint) -> El {
    if keys.is_empty() {
        return none("keys-none", "No API keys", p);
    }
    let now = now();
    let mut sorted: Vec<&crux::dto::ApiKey> = keys.iter().collect();
    sorted.sort_by(|a, b| {
        (KeyState::of(a, now) != KeyState::Live)
            .cmp(&(KeyState::of(b, now) != KeyState::Live))
            .then_with(|| b.created_at.cmp(&a.created_at))
    });
    let rows = sorted
        .into_iter()
        .map(|key| key_row(key, KeyState::of(key, now), app, now, p))
        .collect();
    stacked("keys-list", rows, p)
}

fn key_row(key: &crux::dto::ApiKey, state: KeyState, app: &App, now: i64, p: &Paint) -> El {
    let status = match state {
        KeyState::Live => tag(state.label(), p),
        KeyState::Expired => flag(state.label(), Tone::Warning, None, p),
        KeyState::Revoked => flag(state.label(), Tone::Danger, None, p),
    };
    let when = |label: &str, stamp: &Option<String>| {
        stamp
            .as_deref()
            .map(|at| format!("{label} {}", relative(at, now)))
    };
    let facts: Vec<String> = match state {
        KeyState::Revoked => when("revoked", &key.revoked_at).into_iter().collect(),
        KeyState::Expired => when("expired", &key.expires_at).into_iter().collect(),
        KeyState::Live => [
            Some(when("used", &key.last_used_at).unwrap_or_else(|| "never used".into())),
            Some(when("expires", &key.expires_at).unwrap_or_else(|| "no expiry".into())),
        ]
        .into_iter()
        .flatten()
        .collect(),
    };
    let mut line = row()
        .key(format!("key-{}", key.id))
        .w_full()
        .py(10.0)
        .gap(14.0)
        .center_y()
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(4.0)
                .child(
                    row()
                        .gap(8.0)
                        .center_y()
                        .child(
                            text(key.name.clone())
                                .font(p.k.ty.ui_medium)
                                .color(p.c.text_base)
                                .truncate(),
                        )
                        .child(status),
                )
                .child(
                    text(facts.join(" \u{b7} "))
                        .font(p.mono(12.0, 420.0))
                        .color(p.c.text_muted)
                        .truncate(),
                ),
        );
    if state == KeyState::Live {
        line = line.child(confirm(
            format!("key-revoke-{}", key.id),
            &key.id,
            ("Revoke", "Confirm revoke"),
            true,
            Action::RevokeKey { id: key.id.clone() },
            app,
            p,
        ));
    }
    line
}

fn mcp_clients(sign_ins: &[crux::dto::McpSignIn], app: &App, p: &Paint) -> El {
    let sign_out = |label: &str, key: String, client: Option<String>| {
        Button::new(label.to_string())
            .variant(ButtonVariant::Ghost)
            .disabled(app.remote.desk.busy())
            .on_press(move |app: &mut App| {
                app.act(Action::SignOutMcp {
                    client: client.clone(),
                })
            })
            .build(p.k)
            .key(key)
    };
    let mut head = row()
        .w_full()
        .center_y()
        .child(heading("MCP clients", p))
        .child(el().grow(1.0));
    if sign_ins.is_empty() {
        return col()
            .key("mcp-clients")
            .w_full()
            .gap(8.0)
            .child(head)
            .child(none("mcp-none", "None signed in", p));
    }
    head = head.child(sign_out("Sign out all", "mcp-sign-out-all".into(), None));
    let now = now();
    let rows = by_client(sign_ins)
        .into_iter()
        .map(|client| {
            let mut block = col()
                .key(format!("mcp-client-{}", client.id))
                .w_full()
                .py(10.0)
                .gap(6.0)
                .child(
                    row()
                        .w_full()
                        .gap(8.0)
                        .center_y()
                        .child(
                            text(client.label.clone())
                                .font(p.k.ty.ui_medium)
                                .color(p.c.text_base),
                        )
                        .child(
                            text(client.id.to_string())
                                .font(p.mono(12.0, 420.0))
                                .color(p.c.text_faint)
                                .grow(1.0)
                                .min_w(0.0)
                                .truncate(),
                        )
                        .child(sign_out(
                            "Sign out",
                            format!("mcp-sign-out-{}", client.id),
                            Some(client.id.to_string()),
                        )),
                );
            for sign_in in client.sign_ins {
                block = block.child(
                    row()
                        .w_full()
                        .gap(14.0)
                        .child(
                            text(sign_in.resource.clone())
                                .font(p.mono(12.0, 420.0))
                                .color(p.c.text_base)
                                .grow(1.0)
                                .min_w(0.0)
                                .truncate(),
                        )
                        .child(
                            text(sign_in.cluster.clone())
                                .font(p.mono(12.0, 420.0))
                                .color(p.c.text_muted),
                        )
                        .child(
                            text(format!("used {}", relative(&sign_in.last_used_at, now)))
                                .font(p.mono(12.0, 420.0))
                                .color(p.c.text_muted),
                        ),
                );
            }
            block
        })
        .collect();
    col()
        .key("mcp-clients")
        .w_full()
        .gap(4.0)
        .child(head)
        .child(stacked("mcp-list", rows, p))
}
