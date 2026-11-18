//! Settings' Profiles section: each controller the app can use, how it is reached, and a form to
//! add or edit one.

use rune::{El, TextEdit, Tx, col, el, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, SegmentedControl, Switch, TextInput};

use crate::App;
use crate::profile::{Auth, Form, Readiness};
use crate::ui::{Paint, rule, setting_row};

const FIELD: f32 = 400.0;

pub(crate) fn rows(app: &App, p: &Paint) -> Vec<El> {
    let configured = app.settings.profiles();
    let mut listed: Vec<(String, Result<crate::profile::Profile, String>, bool)> = configured
        .into_iter()
        .map(|(name, profile)| (name, profile, true))
        .collect();
    if listed.is_empty() {
        listed = crate::profile::usable(&app.settings)
            .into_iter()
            .map(|(name, profile)| (name, Ok(profile), false))
            .collect();
    }
    let mut rows = Vec::new();
    if let Some(why) = &app.settings.unreadable {
        rows.push(
            text(format!(
                "{} {why}. Not saving.",
                app.settings.config_path().display()
            ))
            .key("config-unreadable")
            .font(p.k.ty.small)
            .color(p.fail)
            .wrap()
            .py(10.0),
        );
        rows.push(rule("config-unreadable-sep", false, p));
    }
    for (i, (name, profile, configured)) in listed.into_iter().enumerate() {
        if i > 0 {
            rows.push(rule(&format!("profile-sep-{i}"), false, p));
        }
        rows.push(profile_row(app, &name, profile, configured, i, p));
    }
    rows.push(rule("profile-add-sep", false, p));
    match &app.profile_form {
        Some(form) => rows.extend(form_rows(form, p)),
        None => rows.push(
            row().py(10.0).child(
                Button::new("Add profile")
                    .variant(ButtonVariant::Neutral)
                    .control_size(ControlSize::Small)
                    .on_press(|app: &mut App| app.profile_form = Some(Form::new()))
                    .build(p.k)
                    .key("profile-add"),
            ),
        ),
    }
    rows.push(rule("keychain-sep", false, p));
    rows.push(setting_row(
        "Keys in keychain",
        Switch::new(app.settings.keychain)
            .label("Keys in keychain")
            .hide_label()
            .on_toggle(|app: &mut App| {
                let on = !app.settings.keychain;
                app.set_keychain(on);
            })
            .build(p.k)
            .key("keychain-switch"),
        p,
    ));
    if let Some(why) = &app.keys_error {
        rows.push(
            text(why.clone())
                .key("keys-error")
                .font(p.k.ty.small)
                .color(p.fail)
                .wrap()
                .pb(6.0),
        );
    }
    rows
}

fn profile_row(
    app: &App,
    name: &str,
    profile: Result<crate::profile::Profile, String>,
    configured: bool,
    place: usize,
    p: &Paint,
) -> El {
    let current = app.remote.target.name == name;
    let mut about = col().gap(3.0).grow(1.0).min_w(0.0).child(
        row()
            .gap(8.0)
            .center_y()
            .child(
                text(name.to_string())
                    .font(p.k.ty.ui_medium)
                    .color(p.c.text_base),
            )
            .children((place < 9).then(|| {
                text(format!("\u{2318}\u{2325}{}", place + 1))
                    .font(p.mono(10.5, 400.0))
                    .color(p.c.text_faint)
            }))
            .children(current.then(|| status(app, p))),
    );
    let mut actions = row().gap(6.0).center_y();
    match &profile {
        Ok(profile) => {
            about = about.child(
                text(profile.url.clone())
                    .font(p.mono(11.0, 400.0))
                    .color(p.c.text_muted)
                    .truncate(),
            );
            let mut route = profile.route();
            if profile.auth == Auth::Crux {
                route.push("key in crux's config".into());
            }
            if !route.is_empty() {
                about = about.child(
                    text(route.join(" \u{b7} "))
                        .font(p.mono(10.5, 400.0))
                        .color(p.c.text_faint)
                        .truncate(),
                );
            }
            if current && matches!(app.remote.target.readiness(), Readiness::Failed(_)) {
                actions = actions.child(
                    Button::new("Retry")
                        .variant(ButtonVariant::Neutral)
                        .control_size(ControlSize::Small)
                        .on_press(|app: &mut App| app.retry_profile())
                        .build(p.k)
                        .key(format!("profile-retry-{name}")),
                );
            }
            if !current {
                let use_name = name.to_string();
                actions = actions.child(
                    Button::new("Use")
                        .variant(ButtonVariant::Neutral)
                        .control_size(ControlSize::Small)
                        .on_press(move |app: &mut App| {
                            app.switch(crate::Backend::Controller);
                            app.use_profile(&use_name);
                        })
                        .build(p.k)
                        .key(format!("profile-use-{name}")),
                );
            }
            let (edit_name, edit_profile) = (name.to_string(), profile.clone());
            actions = actions.child(
                Button::new("Edit")
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .on_press(move |app: &mut App| {
                        let editing = configured.then_some(edit_name.as_str());
                        let mut form = Form::of(editing, &edit_profile);
                        if !configured {
                            form.name = TextEdit::new(&edit_name);
                        }
                        app.profile_form = Some(form);
                    })
                    .build(p.k)
                    .key(format!("profile-edit-{name}")),
            );
        }
        Err(why) => {
            about = about.child(text(why.clone()).font(p.k.ty.small).color(p.fail).wrap());
        }
    }
    if configured {
        let remove = name.to_string();
        actions = actions.child(
            Button::new("Remove")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(move |app: &mut App| app.remove_profile(&remove))
                .build(p.k)
                .key(format!("profile-remove-{name}")),
        );
    }
    row()
        .key(format!("profile-{name}"))
        .w_full()
        .py(10.0)
        .gap(16.0)
        .center_y()
        .child(about)
        .child(actions)
}

fn status(app: &App, p: &Paint) -> El {
    let (label, color) = match app.remote.target.readiness() {
        Readiness::Starting(what) => (what, p.c.text_muted),
        Readiness::Failed(_) => ("tunnel failed".to_string(), p.fail),
        Readiness::Ready => match &*app.remote.live {
            crate::live::LiveView::Ready { .. } => ("in use".to_string(), p.pass),
            crate::live::LiveView::Loading => ("connecting".to_string(), p.c.text_muted),
            crate::live::LiveView::Unavailable(_) => ("not connected".to_string(), p.fail),
        },
    };
    text(label).font(p.k.ty.small).color(color)
}

type Field = fn(&mut Form) -> &mut TextEdit;

fn field(label: &str, key: &str, edit: &TextEdit, hint: &str, pick: Field, p: &Paint) -> El {
    setting_row(
        label,
        TextInput::new(edit)
            .placeholder(hint)
            .width(Some(FIELD))
            .on_edit(move |app: &mut App| match &mut app.profile_form {
                Some(form) => pick(form),
                None => &mut app.scratch,
            })
            .on_submit(|app: &mut App, _: &mut Tx| app.save_profile_form())
            .build(p.k)
            .key(format!("profile-form-{key}")),
        p,
    )
}

fn form_rows(form: &Form, p: &Paint) -> Vec<El> {
    let mut rows = vec![
        field("Name", "name", &form.name, "wdc", |f| &mut f.name, p),
        field(
            "URL",
            "url",
            &form.url,
            "https://crucible-api.example.com",
            |f| &mut f.url,
            p,
        ),
        field(
            "Proxy",
            "proxy",
            &form.proxy,
            "socks5h://127.0.0.1:1080",
            |f| &mut f.proxy,
            p,
        ),
        field(
            "Tunnel",
            "tunnel",
            &form.tunnel,
            "ssh -N -D 1080 jumpbox",
            |f| &mut f.tunnel,
            p,
        ),
        field("Shell", "shell", &form.shell, "$SHELL", |f| &mut f.shell, p),
        field(
            "Resolve",
            "resolve",
            &form.resolve,
            "api.example.com = 10.0.0.7",
            |f| &mut f.resolve,
            p,
        ),
        setting_row(
            "Key",
            SegmentedControl::new(&["Own", "crux's"], Some((form.auth == Auth::Crux) as usize))
                .label("Key")
                .fit()
                .on_pick(|app: &mut App, i: usize, _: &mut Tx| {
                    if let Some(form) = &mut app.profile_form {
                        form.auth = if i == 1 { Auth::Crux } else { Auth::Own };
                    }
                })
                .build(p.k)
                .key("profile-form-auth"),
            p,
        ),
    ];
    if let Some(why) = &form.error {
        rows.push(
            text(why.clone())
                .font(p.k.ty.small)
                .color(p.fail)
                .wrap()
                .pb(6.0),
        );
    }
    rows.push(
        row()
            .py(10.0)
            .gap(8.0)
            .child(el().grow(1.0))
            .child(
                Button::new("Cancel")
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .on_press(|app: &mut App| app.profile_form = None)
                    .build(p.k)
                    .key("profile-form-cancel"),
            )
            .child(
                Button::new("Save")
                    .variant(ButtonVariant::Submit)
                    .control_size(ControlSize::Small)
                    .on_press(|app: &mut App| app.save_profile_form())
                    .build(p.k)
                    .key("profile-form-save"),
            ),
    );
    rows
}
