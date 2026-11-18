//! Settings' About rows: the app's version, the controller's beside it, and the newest release.

use rune::{El, row, text};
use rune_kit::{Button, ButtonVariant, Switch};

use crate::ui::{Paint, rule, setting_row, tag};
use crate::{App, Backend, version};

fn value(key: &str, shown: String, note: Option<&str>, action: Option<El>, p: &Paint) -> El {
    row()
        .key(key.to_string())
        .gap(10.0)
        .center_y()
        .child(
            text(shown)
                .font(p.mono(12.0, 480.0))
                .color(p.c.text_base)
                .selectable(true),
        )
        .children(note.map(|note| tag(note, p)))
        .children(action)
}

pub(crate) fn rows(app: &App, p: &Paint) -> Vec<El> {
    let mut rows = vec![setting_row(
        "Crucible Playbooks",
        value("about-app", version::APP.to_string(), None, None, p),
        p,
    )];
    if app.backend == Backend::Controller {
        let (shown, note) = match &app.remote.admin.version {
            Some(Ok(controller)) => (
                [controller.version.as_str(), controller.git_sha.as_str()]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(" \u{b7} "),
                version::controller_note(version::APP, &controller.version),
            ),
            Some(Err(why)) => (why.clone(), None),
            None => ("\u{2026}".to_string(), None),
        };
        rows.push(rule("about-controller-sep", false, p));
        rows.push(setting_row(
            "Controller",
            value("about-controller", shown, note, None, p),
            p,
        ));
    }
    if cfg!(target_arch = "wasm32") {
        return rows;
    }
    let (shown, newer) = app.release().line(version::APP);
    let notes = newer.map(|tag| {
        let url = crate::update::notes(tag);
        Button::new("Release notes")
            .variant(ButtonVariant::Neutral)
            .on_press(move |app: &mut App| {
                app.editor_error = crate::editor::open_url(&url).err();
            })
            .build(p.k)
            .key("about-release-notes")
    });
    rows.push(rule("about-release-sep", false, p));
    rows.push(setting_row(
        "Latest release",
        value("about-release", shown, None, notes, p),
        p,
    ));
    rows.push(rule("about-check-sep", false, p));
    rows.push(setting_row(
        "Check for updates",
        Switch::new(app.settings.updates)
            .label("Check for updates")
            .hide_label()
            .on_toggle(|app: &mut App| {
                let on = !app.settings.updates;
                app.settings.set_updates(on);
                if on {
                    app.check_release();
                }
            })
            .build(p.k)
            .key("updates-switch"),
        p,
    ));
    rows
}
