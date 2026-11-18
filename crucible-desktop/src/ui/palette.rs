//! The command palette: a field over a ranked list of tasks, commands, playbooks and packs,
//! each command showing its key.

use rune::{El, ScrollSpec, Tx, col, el, row, text};
use rune_kit::{Keybind, KeybindVariant, TextInput};
use rune_theme_opencode::icons as oc;

use crate::App;
use crate::palette::{Item, rank};
use crate::ui::Paint;

const WIDTH: f32 = 600.0;
const ROW: f32 = 30.0;

pub(crate) fn palette(app: &App, p: &Paint, layer: &mut Vec<El>) {
    let Some(open) = &app.palette else {
        return;
    };
    let entries = app.palette_entries();
    let ranked = rank(&entries, &open.query.text);
    let selected = open.selected.min(ranked.len().saturating_sub(1));
    let input = TextInput::new(&open.query)
        .placeholder("Tasks, commands, playbooks")
        .leading_icon(&oc::MAGNIFYING_GLASS)
        .on_edit(|app: &mut App| &mut app.palette.get_or_insert_with(Default::default).query)
        .on_change(|app: &mut App, _: &mut Tx| {
            if let Some(open) = &mut app.palette {
                open.selected = 0;
            }
        })
        .on_key(crate::commands::palette_key)
        .on_submit(|app: &mut App, tx: &mut Tx| app.pick_entry(None, tx))
        .build(p.k)
        .key("palette-query")
        .w_full();
    let mut list = col()
        .key("palette-list")
        .w_full()
        .max_h(ROW * 12.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_edge_fade(6.0);
    if ranked.is_empty() {
        list = list.child(
            text("Nothing matches".to_string())
                .font(p.k.ty.ui)
                .color(p.c.text_muted)
                .padding(10.0),
        );
    }
    for (i, entry) in ranked.iter().enumerate() {
        let on = i == selected;
        let keys = match entry.item {
            Item::Run(command) => app.keymap.keys(command).first().map(|s| s.keycaps()),
            _ => None,
        };
        list = list.child(
            row()
                .key(format!("palette-{i}"))
                .role("option")
                .label(entry.title.clone())
                .checked(on)
                .w_full()
                .h(ROW)
                .px(10.0)
                .gap(10.0)
                .center_y()
                .radius(p.k.radius.md)
                .fill(if on {
                    p.c.bg_accent
                } else {
                    rune::Color::TRANSPARENT
                })
                .hover(p.c.hover)
                .on_press_tx(move |app: &mut App, tx: &mut Tx| app.pick_entry(Some(i), tx))
                .child(
                    text(entry.title.clone())
                        .font(p.k.ty.ui)
                        .color(p.c.text_base)
                        .grow(1.0)
                        .min_w(0.0)
                        .truncate(),
                )
                .children(keys.map(|caps| {
                    Keybind::new(&caps)
                        .variant(KeybindVariant::Ghost)
                        .build(p.k)
                }))
                .child(
                    text(entry.kind.to_string())
                        .font(p.mono(10.5, 400.0))
                        .color(p.c.text_faint)
                        .w(64.0),
                ),
        );
    }
    let panel = col()
        .key("palette-panel")
        .role("dialog")
        .label("Command palette")
        .w_full()
        .max_w(WIDTH)
        .padding(8.0)
        .gap(6.0)
        .radius(p.k.radius.lg)
        .fill(p.c.layer_01)
        .border(p.hair, p.c.border_muted)
        .modal(true, |app: &mut App| app.palette = None)
        .child(input)
        .child(list);
    layer.push(
        el().key("palette-scrim")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .fill(p.k.c.scrim)
            .a11y_hidden(),
    );
    layer.push(
        col()
            .key("palette-layer")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .padding(16.0)
            .pt(96.0)
            .items_center()
            .child(panel),
    );
}
