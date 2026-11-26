//! The Settings page.

use crate::App;
use crate::editor::Editor;
use crate::settings::{Appearance, LogFont, TextSize, Theme};
use crate::ui::{Paint, SECTION, TITLE, about, desk, profiles, rule};
use rune::{El, TextStyle, Tx, col, el, row, text};
use rune_kit::{Button, ButtonVariant, SegmentedControl, Switch, TextInput};

pub(crate) fn settings_pane(app: &App, p: &Paint) -> El {
    let mut rows = Vec::new();
    for (i, font) in LogFont::ALL.into_iter().enumerate() {
        if i > 0 {
            rows.push(rule(&format!("font-sep-{i}"), false, p));
        }
        rows.push(font_option(font, app, p));
    }

    col()
        .key("detail")
        .grow(1.0)
        .h_full()
        .min_h(0.0)
        .min_w(0.0)
        .px(40.0)
        .pt(32.0)
        .pb(32.0)
        .gap(24.0)
        .scroll(rune::ScrollSpec {
            stick_bottom: false,
        })
        .scroll_edge_fade(8.0)
        .child(
            row()
                .w_full()
                .center_y()
                .gap(10.0)
                .child(
                    text("Settings")
                        .font(p.sized(TITLE))
                        .color(p.c.text_base)
                        .grow(1.0),
                )
                .child(
                    Button::new("Done")
                        .variant(ButtonVariant::Neutral)
                        .on_press(|app: &mut App| app.page = crate::Page::Runs)
                        .build(p.k)
                        .key("settings-done"),
                ),
        )
        .children(desk::group_tabs(app, p))
        .child(settings_section("about", "About", about::rows(app, p), p))
        .child(settings_section(
            "profiles",
            "Profiles",
            profiles::rows(app, p),
            p,
        ))
        .children(accounts_section(app, p))
        .child(settings_section(
            "appearance",
            "Appearance",
            vec![
                setting_row(
                    "Theme",
                    SegmentedControl::new(
                        &Theme::ALL.map(|theme| theme.label()),
                        Theme::ALL.iter().position(|t| *t == app.settings.theme),
                    )
                    .label("Theme")
                    .width(Some(360.0))
                    .on_pick(|app: &mut App, i: usize, tx: &mut Tx| {
                        app.settings.set_theme(Theme::ALL[i]);
                        tx.color_motion(rune::Spring::new(0.35, 0.0));
                    })
                    .build(p.k)
                    .key("theme"),
                    p,
                ),
                rule("appearance-sep", false, p),
                setting_row(
                    "Mode",
                    SegmentedControl::new(
                        &Appearance::ALL.map(|a| a.label()),
                        Appearance::ALL
                            .iter()
                            .position(|a| *a == app.settings.appearance),
                    )
                    .label("Mode")
                    .width(Some(240.0))
                    .on_pick(|app: &mut App, i: usize, tx: &mut Tx| {
                        app.settings.set_appearance(Appearance::ALL[i]);
                        tx.color_motion(rune::Spring::new(0.35, 0.0));
                    })
                    .build(p.k)
                    .key("mode"),
                    p,
                ),
                rule("zoom-sep", false, p),
                setting_row(
                    "Zoom",
                    SegmentedControl::new(
                        &crate::settings::ZOOMS.map(|z| format!("{:.0}%", z * 100.0)),
                        crate::settings::ZOOMS
                            .iter()
                            .position(|z| (z - app.settings.zoom).abs() < 0.01),
                    )
                    .label("Zoom")
                    .width(Some(420.0))
                    .on_pick(|app: &mut App, i: usize, _: &mut Tx| {
                        app.set_zoom(crate::settings::ZOOMS[i]);
                    })
                    .build(p.k)
                    .key("zoom"),
                    p,
                ),
                rule("text-size-sep", false, p),
                setting_row(
                    "Text size",
                    SegmentedControl::new(
                        &TextSize::ALL.map(|s| s.label()),
                        TextSize::ALL
                            .iter()
                            .position(|s| *s == app.settings.text_size),
                    )
                    .label("Text size")
                    .width(Some(360.0))
                    .on_pick(|app: &mut App, i: usize, _: &mut Tx| {
                        app.settings.set_text_size(TextSize::ALL[i]);
                    })
                    .build(p.k)
                    .key("text-size"),
                    p,
                ),
                rule("compact-graph-sep", false, p),
                setting_row(
                    "Compact graph",
                    Switch::new(app.settings.compact_graph)
                        .label("Compact graph")
                        .hide_label()
                        .on_toggle(|app: &mut App| {
                            let on = !app.settings.compact_graph;
                            app.settings.set_compact_graph(on);
                        })
                        .build(p.k)
                        .key("compact-graph-switch"),
                    p,
                ),
            ],
            p,
        ))
        .child(settings_section(
            "effects",
            "Effects",
            vec![setting_row(
                "CRT charts",
                Switch::new(app.settings.crt)
                    .label("CRT charts")
                    .hide_label()
                    .on_toggle(|app: &mut App| {
                        let on = !app.settings.crt;
                        app.settings.set_crt(on);
                    })
                    .build(p.k)
                    .key("crt-switch"),
                p,
            )],
            p,
        ))
        .child(settings_section(
            "checks",
            "Checks",
            vec![setting_row(
                "Check pack files",
                Switch::new(app.settings.checks)
                    .label("Check pack files")
                    .hide_label()
                    .on_toggle(|app: &mut App| {
                        let on = !app.settings.checks;
                        app.set_checks(on);
                    })
                    .build(p.k)
                    .key("checks-switch"),
                p,
            )],
            p,
        ))
        .child(settings_section(
            "views",
            "Views",
            vec![setting_row(
                "Draw pack views",
                Switch::new(app.settings.view_plugins)
                    .label("Draw pack views")
                    .hide_label()
                    .on_toggle(|app: &mut App| {
                        let on = !app.settings.view_plugins;
                        app.settings.set_view_plugins(on);
                    })
                    .build(p.k)
                    .key("views-switch"),
                p,
            )],
            p,
        ))
        .child(settings_section(
            "notifications",
            "Notifications",
            vec![
                setting_row(
                    "When my runs finish",
                    Switch::new(app.settings.notify)
                        .label("When my runs finish")
                        .hide_label()
                        .on_toggle(|app: &mut App| {
                            let on = !app.settings.notify;
                            app.settings.set_notify(on);
                        })
                        .build(p.k)
                        .key("notify-switch"),
                    p,
                ),
                rule("notifications-sep", false, p),
                setting_row(
                    "When a decision waits on me",
                    Switch::new(app.settings.ask)
                        .label("When a decision waits on me")
                        .hide_label()
                        .on_toggle(|app: &mut App| {
                            let on = !app.settings.ask;
                            app.settings.set_ask(on);
                        })
                        .build(p.k)
                        .key("ask-switch"),
                    p,
                ),
            ],
            p,
        ))
        .child(settings_section("packs", "Packs", pack_rows(app, p), p))
        .child(settings_section(
            "dropped",
            "Dropped on the Dock",
            drop_rows(app, p),
            p,
        ))
        .child(settings_section("log-viewer", "Log font", rows, p))
        .child(settings_section("keys", "Keys", key_rows(app, p), p))
}

/// A row per command: its first key in a recorder, any others beside it, and a reset when the
/// config file binds it; the bindings the config file could not use come first.
fn key_rows(app: &App, p: &Paint) -> Vec<El> {
    let mut rows: Vec<El> = app
        .keys_refused
        .iter()
        .map(|refused| {
            text(format!(
                "{} {}: {}",
                refused.command, refused.key, refused.why
            ))
            .font(p.k.ty.small)
            .color(p.fail)
            .py(6.0)
        })
        .collect();
    for (i, command) in crate::keymap::Command::ALL.into_iter().enumerate() {
        if i > 0 {
            rows.push(rule(&format!("keys-sep-{i}"), false, p));
        }
        let state = app.recorders.get(&command).cloned().unwrap_or_else(|| {
            rune_kit::ShortcutRecorderState::new(app.keymap.keys(command).first().cloned())
        });
        let others: Vec<String> = app
            .keymap
            .keys(command)
            .iter()
            .skip(1)
            .map(|key| key.keycaps().join(""))
            .collect();
        let id = command.id();
        let recorder =
            rune_kit::ShortcutRecorder::new(format!("key-{id}"), &state, move |app: &mut App| {
                app.recorder(command)
            })
            .label(command.title())
            .allow_unmodified()
            .width(200.0)
            .on_change(move |app: &mut App, shortcut, _: &mut Tx| app.rebind(command, shortcut))
            .build(p.k);
        let mut control = row().gap(8.0).center_y();
        if !others.is_empty() {
            control = control.child(
                text(others.join("  "))
                    .font(p.mono(11.0, 400.0))
                    .color(p.c.text_faint),
            );
        }
        control = control.child(recorder);
        if app.settings.keys.contains_key(&id) {
            control = control.child(
                Button::new("Reset")
                    .variant(ButtonVariant::Ghost)
                    .control_size(rune_kit::ControlSize::Small)
                    .on_press(move |app: &mut App| app.reset_keys(command))
                    .build(p.k)
                    .key(format!("key-reset-{id}")),
            );
        }
        rows.push(setting_row(&command.title(), control, p));
    }
    rows
}

#[cfg(target_arch = "wasm32")]
fn accounts_section(_app: &App, _p: &Paint) -> Option<El> {
    None
}

#[cfg(not(target_arch = "wasm32"))]
fn account_control(app: &App, account: &crate::forge::Account, p: &Paint) -> El {
    let key = account.store_key();
    let Some(session) = app.forge.get(account) else {
        return el();
    };
    if let Some(code) = &session.code {
        let page = code.page().to_string();
        let open = account.clone();
        let cancel = account.clone();
        return row()
            .gap(8.0)
            .center_y()
            .child(
                text(code.user_code.clone())
                    .font(p.mono(15.0, 600.0))
                    .color(p.c.text_base)
                    .selectable(true),
            )
            .child(
                Button::new("Open")
                    .variant(ButtonVariant::Neutral)
                    .control_size(rune_kit::ControlSize::Small)
                    .on_press(move |app: &mut App| {
                        if let Err(why) = crate::editor::open_url(&page) {
                            app.forge.entry(open.clone()).or_default().error = Some(why);
                        }
                    })
                    .build(p.k)
                    .key(format!("forge-open-{key}")),
            )
            .child(
                Button::new("Cancel")
                    .variant(ButtonVariant::Ghost)
                    .control_size(rune_kit::ControlSize::Small)
                    .on_press(move |app: &mut App| app.forge_cancel(cancel.clone()))
                    .build(p.k)
                    .key(format!("forge-cancel-{key}")),
            );
    }
    if let Some(login) = &session.login {
        let here = session
            .linked
            .as_ref()
            .is_some_and(|(profile, _)| *profile == app.remote.target.name);
        let out = account.clone();
        let link = account.clone();
        let offered = !session.busy && !app.settings.forge_linked(&app.remote.target.name, account);
        return row()
            .gap(8.0)
            .center_y()
            .child(text(login.clone()).font(p.k.ty.ui).color(p.c.text_base))
            .children(here.then(|| {
                text(format!("linked to {}", app.remote.target.name))
                    .font(p.k.ty.small)
                    .color(p.c.text_muted)
            }))
            .children(offered.then(|| {
                Button::new(format!("Link to {}", app.remote.target.name))
                    .variant(ButtonVariant::Neutral)
                    .control_size(rune_kit::ControlSize::Small)
                    .on_press(move |app: &mut App| app.forge_link_here(link.clone()))
                    .build(p.k)
                    .key(format!("forge-link-{key}"))
            }))
            .children(
                session
                    .busy
                    .then(|| rune_kit::Spinner::new().size(12.0).build(p.k)),
            )
            .child(
                Button::new("Sign out")
                    .variant(ButtonVariant::Ghost)
                    .control_size(rune_kit::ControlSize::Small)
                    .on_press(move |app: &mut App| app.forge_sign_out(out.clone()))
                    .build(p.k)
                    .key(format!("forge-sign-out-{key}")),
            );
    }
    if session.busy {
        return row().child(rune_kit::Spinner::new().size(12.0).build(p.k));
    }
    let start = account.clone();
    row().child(
        Button::new("Sign in")
            .variant(ButtonVariant::Submit)
            .control_size(rune_kit::ControlSize::Small)
            .on_press(move |app: &mut App| app.forge_sign_in(start.clone()))
            .build(p.k)
            .key(format!("forge-sign-in-{key}")),
    )
}

#[cfg(not(target_arch = "wasm32"))]
fn accounts_section(app: &App, p: &Paint) -> Option<El> {
    use crate::forge::Account;
    let offered: Vec<(Account, Option<String>)> = app
        .forge_offered()
        .iter()
        .filter_map(|o| o.account().map(|a| (a, o.client_id.clone())))
        .collect();
    let named_here = |account: &Account| offered.iter().any(|(a, id)| a == account && id.is_some());
    let mut rows = Vec::new();
    if !named_here(&Account::Github) {
        rows.push(setting_row(
            "GitHub App client ID",
            TextInput::new(&app.github_client_id)
                .placeholder("Iv23\u{2026}")
                .width(Some(240.0))
                .on_edit(|app: &mut App| &mut app.github_client_id)
                .on_change(|app: &mut App, _: &mut Tx| {
                    let id = app.github_client_id.text.clone();
                    app.settings.set_github_client_id(&id);
                })
                .build(p.k)
                .key("github-client-id"),
            p,
        ));
    }
    rows.push(setting_row(
        "GitHub",
        account_control(app, &Account::Github, p),
        p,
    ));
    for account in app.forge_accounts() {
        let Account::Gitlab(host) = &account else {
            continue;
        };
        let own = app.settings.gitlab_apps.contains_key(host);
        let remove = host.clone();
        rows.push(rule(&format!("gitlab-sep-{host}"), false, p));
        rows.push(setting_row(
            &account.name(),
            row()
                .gap(8.0)
                .center_y()
                .child(account_control(app, &account, p))
                .child(
                    text(app.forge_client_id(&account))
                        .font(p.mono(11.0, 400.0))
                        .color(p.c.text_faint)
                        .truncate(),
                )
                .children((own && !named_here(&account)).then(|| {
                    Button::new("Remove")
                        .variant(ButtonVariant::Ghost)
                        .control_size(rune_kit::ControlSize::Small)
                        .on_press(move |app: &mut App| app.remove_gitlab_host(remove.clone()))
                        .build(p.k)
                        .key(format!("gitlab-remove-{host}"))
                })),
            p,
        ));
    }
    let host_ok = crate::forge::gitlab_host(&app.gitlab_host.text).is_some();
    rows.push(rule("gitlab-add-sep", false, p));
    rows.push(setting_row(
        "Add GitLab",
        row()
            .gap(8.0)
            .center_y()
            .child(
                TextInput::new(&app.gitlab_host)
                    .placeholder("gitlab.com")
                    .width(Some(180.0))
                    .invalid(!app.gitlab_host.text.trim().is_empty() && !host_ok)
                    .on_edit(|app: &mut App| &mut app.gitlab_host)
                    .build(p.k)
                    .key("gitlab-host"),
            )
            .child(
                TextInput::new(&app.gitlab_client_id)
                    .placeholder("client ID")
                    .width(Some(180.0))
                    .on_edit(|app: &mut App| &mut app.gitlab_client_id)
                    .on_submit(|app: &mut App, _: &mut Tx| app.add_gitlab_host())
                    .build(p.k)
                    .key("gitlab-client-id"),
            )
            .child(
                Button::new("Add")
                    .variant(ButtonVariant::Neutral)
                    .control_size(rune_kit::ControlSize::Small)
                    .disabled(!host_ok || app.gitlab_client_id.text.trim().is_empty())
                    .on_press(|app: &mut App| app.add_gitlab_host())
                    .build(p.k)
                    .key("gitlab-add"),
            ),
        p,
    ));
    for account in app.forge_accounts() {
        if let Some(why) = app.forge.get(&account).and_then(|s| s.error.as_ref()) {
            rows.push(
                text(format!("{}: {why}", account.name()))
                    .font(p.k.ty.small)
                    .color(p.fail)
                    .wrap()
                    .py(6.0),
            );
        }
    }
    Some(settings_section("accounts", "Accounts", rows, p))
}

/// The ceilings a pack folder dropped on the Dock icon is test-fired under.
fn drop_rows(app: &App, p: &Paint) -> Vec<El> {
    let save = |app: &mut App, _: &mut Tx| {
        let (cost, time) = (app.drop_cost.text.clone(), app.drop_time.text.clone());
        app.drop_error = app.settings.set_drop(&cost, &time).err();
    };
    let mut rows = vec![
        setting_row(
            "Cost ceiling",
            TextInput::new(&app.drop_cost)
                .placeholder("1.00")
                .width(Some(160.0))
                .on_edit(|app: &mut App| &mut app.drop_cost)
                .on_change(save)
                .build(p.k)
                .key("drop-cost"),
            p,
        ),
        rule("drop-sep", false, p),
        setting_row(
            "Time ceiling",
            TextInput::new(&app.drop_time)
                .placeholder("30m")
                .width(Some(160.0))
                .on_edit(|app: &mut App| &mut app.drop_time)
                .on_change(save)
                .build(p.k)
                .key("drop-time"),
            p,
        ),
    ];
    if let Some(why) = &app.drop_error {
        rows.push(text(why.clone()).font(p.k.ty.small).color(p.fail).pb(10.0));
    }
    rows
}

/// The editor rows, then the folder draft working copies are kept in.
fn pack_rows(app: &App, p: &Paint) -> Vec<El> {
    let mut rows = editor_rows(app, p);
    rows.push(rule("drafts-sep", false, p));
    rows.push(setting_row(
        "Drafts folder",
        TextInput::new(&app.drafts_dir)
            .placeholder(
                crate::draft::default_root()
                    .map(|dir| dir.display().to_string())
                    .unwrap_or_default(),
            )
            .width(Some(400.0))
            .invalid(app.drafts_error.is_some())
            .on_edit(|app: &mut App| &mut app.drafts_dir)
            .on_change(|app: &mut App, _: &mut Tx| app.set_drafts_dir())
            .build(p.k)
            .key("drafts-dir"),
        p,
    ));
    if let Some(why) = &app.drafts_error {
        rows.push(text(why.clone()).font(p.k.ty.small).color(p.fail).pb(10.0));
    }
    rows
}

/// The editor pick, and the command field while a command is picked.
fn editor_rows(app: &App, p: &Paint) -> Vec<El> {
    let picked = if app.settings.editor_custom {
        Editor::ALL.len() + 1
    } else {
        app.settings
            .editor
            .and_then(|e| Editor::ALL.iter().position(|x| *x == e))
            .map_or(0, |i| i + 1)
    };
    let mut rows = vec![setting_row(
        "Editor",
        SegmentedControl::new(
            &["Auto", "VS Code", "Cursor", "Zed", "Command"],
            Some(picked),
        )
        .label("Editor")
        .width(Some(400.0))
        .on_pick(|app: &mut App, i: usize, _: &mut Tx| {
            if i == Editor::ALL.len() + 1 {
                let command = app.editor_command.text.clone();
                app.settings.set_editor_command(true, &command);
            } else {
                app.settings
                    .set_editor(i.checked_sub(1).and_then(|i| Editor::ALL.get(i).copied()));
            }
        })
        .build(p.k)
        .key("editor"),
        p,
    )];
    if app.settings.editor_custom {
        rows.push(rule("editor-sep", false, p));
        rows.push(setting_row(
            "Command",
            TextInput::new(&app.editor_command)
                .placeholder(crate::editor::EXAMPLE)
                .width(Some(400.0))
                .invalid(
                    !app.editor_command.text.trim().is_empty()
                        && !app.editor_command.text.contains("{file}"),
                )
                .on_edit(|app: &mut App| &mut app.editor_command)
                .on_change(|app: &mut App, _: &mut Tx| {
                    let command = app.editor_command.text.clone();
                    app.settings.set_editor_command(true, &command);
                })
                .build(p.k)
                .key("editor-command"),
            p,
        ));
    }
    rows
}

/// A titled card of rows, as a settings page lays them out.
fn settings_section(key: &str, title: &str, rows: Vec<El>, p: &Paint) -> El {
    col()
        .key(key.to_string())
        .w_full()
        .max_w(720.0)
        .gap(10.0)
        .child(
            text(title.to_string())
                .font(p.sized(SECTION))
                .color(p.c.text_base),
        )
        .child(
            col()
                .key(format!("{key}-card"))
                .w_full()
                .px(16.0)
                .radius(p.k.radius.lg)
                .fill(p.c.layer_01)
                .border(p.hair, p.c.border_muted)
                .children(rows),
        )
}

pub(crate) fn setting_row(label: &str, control: El, p: &Paint) -> El {
    row()
        .w_full()
        .py(12.0)
        .gap(16.0)
        .center_y()
        .child(
            text(label.to_string())
                .font(p.k.ty.ui_medium)
                .color(p.c.text_base)
                .grow(1.0),
        )
        .child(control)
}

fn font_option(font: LogFont, app: &App, p: &Paint) -> El {
    let available = font != LogFont::Tx02 || app.settings.tx02.is_some();
    let selected = app.settings.log_font == font;
    let mut option = row()
        .key(format!(
            "font-{}",
            font.label().to_lowercase().replace(' ', "-")
        ))
        .role("radio")
        .label(font.label())
        .checked(selected)
        .w_full()
        .py(12.0)
        .gap(12.0)
        .center_y();
    if available {
        option = option
            .focusable(true)
            .on_press(move |app: &mut App| app.set_log_font(font));
    }

    let mut radio = el().size(16.0, 16.0).radius(8.0).center().border(
        1.0,
        if selected {
            p.c.bg_accent
        } else {
            p.c.border_strong
        },
    );
    if selected {
        radio = radio
            .fill(p.c.bg_accent)
            .child(el().size(6.0, 6.0).radius(3.0).fill(p.c.text_contrast));
    }

    let about = match (&app.settings.tx02, font, available) {
        (_, _, false) => "Not installed on this machine.".to_string(),
        (Some(path), LogFont::Tx02, true) => format!("{}\n{}", font.detail(), path.display()),
        _ => font.detail().to_string(),
    };
    option
        .help(about)
        .child(radio)
        .child(
            text(font.label())
                .font(p.k.ty.ui_medium)
                .color(if available {
                    p.c.text_base
                } else {
                    p.c.text_faint
                })
                .grow(1.0)
                .min_w(0.0),
        )
        .child(
            text("Aa 0O 1lI {}")
                .font(TextStyle::new(13.0, 400.0).family(font.family()))
                .color(if available {
                    p.c.text_muted
                } else {
                    p.c.text_faint
                }),
        )
}
