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
const LOGIN_W: f32 = 140.0;
#[cfg(not(target_arch = "wasm32"))]
const LINKED_W: f32 = 140.0;
#[cfg(not(target_arch = "wasm32"))]
const ACTIONS_W: f32 = 230.0;

#[cfg(not(target_arch = "wasm32"))]
fn account_cells(name: El, login: El, linked: El, actions: El) -> El {
    row()
        .w_full()
        .py(10.0)
        .gap(12.0)
        .center_y()
        .child(row().grow(1.0).child(name))
        .child(row().w(LOGIN_W).child(login))
        .child(row().w(LINKED_W).child(linked))
        .child(
            row()
                .w(ACTIONS_W)
                .gap(8.0)
                .center_y()
                .justify_end()
                .child(actions),
        )
}

#[cfg(not(target_arch = "wasm32"))]
fn account_row(app: &App, account: &crate::forge::Account, p: &Paint) -> El {
    let key = account.store_key();
    let session = app.forge.get(account);
    let muted = |s: String| text(s).font(p.k.ty.ui).color(p.c.text_muted).truncate();
    let profiles = app.settings.forge_profiles(account);
    let linked = match profiles.is_empty() {
        true => muted("\u{2014}".into()),
        false => text(profiles.join(", "))
            .font(p.k.ty.ui)
            .color(p.c.text_base)
            .truncate(),
    };
    let name = text(account.name())
        .font(p.k.ty.ui_medium)
        .color(p.c.text_base)
        .truncate();
    let busy = session.is_some_and(|s| s.busy);
    let spinner = || rune_kit::Spinner::new().size(12.0).build(p.k);
    if let Some(code) = session.and_then(|s| s.code.as_ref()) {
        let page = code.page().to_string();
        let typed = code.user_code.clone();
        let open = account.clone();
        let cancel = account.clone();
        let login = text(code.user_code.clone())
            .font(p.mono(15.0, 600.0))
            .color(p.c.text_base)
            .selectable(true);
        let actions = row()
            .gap(8.0)
            .center_y()
            .child(
                Button::new("Copy and open")
                    .variant(ButtonVariant::Neutral)
                    .control_size(rune_kit::ControlSize::Small)
                    .on_press_tx(move |app: &mut App, tx: &mut Tx| {
                        tx.copy(typed.clone());
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
        return account_cells(name, login, linked, actions);
    }
    if let Some(login) = session.and_then(|s| s.login.as_ref()) {
        let out = account.clone();
        let link = account.clone();
        let here = &app.remote.target.name;
        let linkable = !busy && !app.settings.forge_linked(here, account);
        let actions = row()
            .gap(8.0)
            .center_y()
            .children(busy.then(spinner))
            .children(linkable.then(|| {
                Button::new(format!("Link to {here}"))
                    .variant(ButtonVariant::Neutral)
                    .control_size(rune_kit::ControlSize::Small)
                    .on_press(move |app: &mut App| app.forge_link_here(link.clone()))
                    .build(p.k)
                    .key(format!("forge-link-{key}"))
            }))
            .child(
                Button::new("Sign out")
                    .variant(ButtonVariant::Ghost)
                    .control_size(rune_kit::ControlSize::Small)
                    .on_press(move |app: &mut App| app.forge_sign_out(out.clone()))
                    .build(p.k)
                    .key(format!("forge-sign-out-{key}")),
            );
        let login = text(login.clone())
            .font(p.k.ty.ui)
            .color(p.c.text_base)
            .truncate();
        return account_cells(name, login, linked, actions);
    }
    let start = account.clone();
    let actions = match busy {
        true => spinner(),
        false => Button::new("Sign in")
            .variant(ButtonVariant::Submit)
            .control_size(rune_kit::ControlSize::Small)
            .on_press(move |app: &mut App| app.forge_sign_in(start.clone()))
            .build(p.k)
            .key(format!("forge-sign-in-{key}")),
    };
    account_cells(name, muted("\u{2014}".into()), linked, actions)
}

#[cfg(not(target_arch = "wasm32"))]
fn accounts_section(app: &App, p: &Paint) -> Option<El> {
    let head = |s: &str| text(s.to_string()).font(p.k.ty.small).color(p.c.text_muted);
    let mut rows = vec![account_cells(
        head("Account"),
        head("Login"),
        head("Controllers"),
        el(),
    )];
    for account in app.forge_accounts() {
        let session = app.forge.get(&account);
        let signed_in = session.is_some_and(|s| s.login.is_some());
        if !signed_in && app.forge_client_id(&account).is_none() {
            continue;
        }
        rows.push(rule(
            &format!("forge-sep-{}", account.store_key()),
            false,
            p,
        ));
        rows.push(account_row(app, &account, p));
        if let Some(why) = session.and_then(|s| s.error.as_ref()) {
            rows.push(
                text(why.clone())
                    .font(p.k.ty.small)
                    .color(p.fail)
                    .wrap()
                    .pb(10.0),
            );
        }
    }
    (rows.len() > 1).then(|| settings_section("accounts", "Accounts", rows, p))
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
