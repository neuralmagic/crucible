//! Who is signed in, at the foot of the sidebar, and the menu it opens.

use crate::live::LiveView;
use crate::ui::Paint;
use crate::{App, Backend};
#[cfg(target_arch = "wasm32")]
use rune::Tx;
use rune::{El, col, el, icon, row, text};
use rune_kit::{IconButton, IconButtonVariant};
use rune_theme_opencode::icons as oc;

/// Who is signed in, bottom left, with the settings button beside it. Pressing it opens a menu of
/// the profiles and the local packs, to switch between them.
pub(crate) fn account(app: &App, source: String, p: &Paint, layer: &mut Vec<El>) -> El {
    let (name, about) = match (app.backend, &*app.remote.live) {
        (Backend::Local, _) => ("Local packs".to_string(), source),
        (Backend::Controller, LiveView::Ready { identity, .. }) if !identity.user.is_empty() => {
            let role = if identity.role.is_empty() {
                String::new()
            } else {
                format!(" ({})", identity.role)
            };
            (
                identity.user.clone(),
                format!("{}{role}\n{source}", identity.user),
            )
        }
        (Backend::Controller, LiveView::Unavailable(_)) => ("Not connected".to_string(), source),
        (Backend::Controller, _) => ("Connecting\u{2026}".to_string(), source),
    };
    let initial = name
        .chars()
        .find(|c| c.is_alphanumeric())
        .map_or("?".to_string(), |c| c.to_uppercase().to_string());
    let acting = app
        .remote
        .target
        .act_as()
        .map(|act_as| act_as.team().to_string());
    let place = match (app.backend, &acting) {
        (Backend::Local, _) => "Local".to_string(),
        (Backend::Controller, Some(team)) => format!(
            "{} \u{b7} as {}",
            app.remote.target.name,
            team.trim_start_matches("team:")
        ),
        (Backend::Controller, None) => app.remote.target.name.clone(),
    };
    let trigger = row()
        .key("source-menu-button")
        .role("button")
        .label("Switch profile")
        .w(p.tune.sidebar - 76.0)
        .min_w(0.0)
        .gap(10.0)
        .px(6.0)
        .py(5.0)
        .radius(p.k.radius.md)
        .center_y()
        .hover(p.c.hover)
        .on_press(|app: &mut App| app.source_menu = !app.source_menu)
        .help(about)
        .child(
            row()
                .size(26.0, 26.0)
                .radius(13.0)
                .fill(p.c.layer_03)
                .border(p.hair, p.c.border_base)
                .center()
                .child(text(initial).font(p.k.ty.small_medium).color(p.c.text_base)),
        )
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .child(
                    text(name)
                        .font(p.k.ty.ui_medium)
                        .color(p.c.text_base)
                        .truncate(),
                )
                .child(
                    text(place)
                        .font(p.k.ty.small)
                        .color(p.c.text_muted)
                        .truncate(),
                ),
        )
        .child(icon(&rune_kit::glyphs::CHEVRON_UP, 12.0).color(p.c.text_faint));

    let mut menu = rune_kit::Menu::new("source-menu")
        .trigger(trigger)
        .placement(rune_kit::Placement::TopStart)
        .min_width(240.0)
        .group_label("Controllers");
    for (i, name) in crate::profile::usable(&app.settings)
        .into_keys()
        .enumerate()
    {
        let current = app.backend == Backend::Controller && app.remote.target.name == name;
        let mut item = rune_kit::MenuItem::new(name.clone()).checked(current);
        if i < 9 {
            item = item.shortcut(format!("\u{2318}\u{2325}{}", i + 1));
        }
        menu = menu.item(item.on_select(move |app: &mut App| {
            app.source_menu = false;
            app.switch(Backend::Controller);
            app.use_profile(&name);
        }));
    }
    let teams: Vec<String> = app
        .owners()
        .into_iter()
        .filter(|owner| owner.starts_with("team:"))
        .collect();
    if app.backend == Backend::Controller && (!teams.is_empty() || acting.is_some()) {
        menu = menu.separator().group_label("Act as").item(
            rune_kit::MenuItem::new("Yourself")
                .checked(acting.is_none())
                .on_select(|app: &mut App| {
                    app.source_menu = false;
                    app.act_as(None);
                }),
        );
        for team in teams {
            let label = team.trim_start_matches("team:").to_string();
            let current = acting.as_deref() == Some(team.as_str());
            menu = menu.item(rune_kit::MenuItem::new(label).checked(current).on_select(
                move |app: &mut App| {
                    app.source_menu = false;
                    app.act_as(Some(team.clone()));
                },
            ));
        }
    }
    if crate::LOCAL_PACKS {
        menu = menu.separator().item(
            rune_kit::MenuItem::new("Local packs")
                .checked(app.backend == Backend::Local)
                .on_select(|app: &mut App| {
                    app.source_menu = false;
                    app.switch(Backend::Local);
                }),
        );
    }
    let menu = menu
        .separator()
        .item(
            rune_kit::MenuItem::new("Edit profiles\u{2026}").on_select(|app: &mut App| {
                app.source_menu = false;
                app.page = crate::Page::Settings;
            }),
        );
    #[cfg(target_arch = "wasm32")]
    let menu = menu.item(rune_kit::MenuItem::new("Classic UI").on_select_tx(
        |app: &mut App, tx: &mut Tx| {
            app.source_menu = false;
            let _ = tx.open_url_same_tab("/");
        },
    ));
    let menu = menu
        .open(app.source_menu)
        .on_close(|app: &mut App| app.source_menu = false)
        .build(p.k, layer);

    row()
        .key("account")
        .w_full()
        .h(52.0)
        .px(6.0)
        .gap(4.0)
        .center_y()
        .child(menu)
        .child(el().grow(1.0))
        .child(
            IconButton::new(&oc::SETTINGS_GEAR)
                .variant(if app.page.group() == Some(crate::Group::Settings) {
                    IconButtonVariant::Neutral
                } else {
                    IconButtonVariant::GhostMuted
                })
                .label("Settings")
                .on_press(|app: &mut App| app.toggle_group(crate::Group::Settings))
                .build(p.k)
                .key("settings-open"),
        )
}
