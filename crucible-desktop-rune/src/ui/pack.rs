//! The Pack tab: the workflow pack a run came from, file by file.

use rune::{Color, El, ScrollSpec, TextStyle, col, el, icon, row, text};
use rune_kit::syntax::SyntaxTheme;
use rune_theme_opencode::icons as oc;

use crate::App;
use crate::pack::{PackFile, PackFiles};
use crate::settings::LogFont;
use crate::syntax;
use crate::ui::{Paint, mono};

/// Lines drawn per file; a longer file says how much it left out.
const LINES: usize = 4000;
const FILE_LIST: f32 = 190.0;

pub(crate) fn pack_view(
    pack: &Result<PackFiles, String>,
    font: LogFont,
    app: &App,
    p: &Paint,
) -> El {
    let pack = match pack {
        Ok(pack) if !pack.files.is_empty() => pack,
        Ok(_) => return note("This pack has no files to show.", "", p),
        Err(error) => return note("Could not read the pack.", error, p),
    };
    let open = app
        .pack_file
        .as_deref()
        .and_then(|path| pack.file(path))
        .or_else(|| pack.files.first());

    let mut list = col()
        .key("pack-files")
        .w(FILE_LIST)
        .h_full()
        .min_h(0.0)
        .gap(1.0)
        .padding(6.0)
        .radius(p.k.radius.lg)
        .fill(p.c.layer_01)
        .border(p.hair, p.c.border_muted)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_edge_fade(8.0);
    for file in &pack.files {
        let selected = open.is_some_and(|open| open.path == file.path);
        list = list.child(file_row(file, selected, p));
    }

    col()
        .key("pack")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(8.0)
        .child(
            text(pack.origin.clone())
                .font(mono(11.0, 400.0))
                .color(p.c.text_faint)
                .selectable(true)
                .selection_color(p.k.selection())
                .truncate(),
        )
        .child(
            row()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .gap(10.0)
                .child(list)
                .child(match open {
                    Some(file) => source(file, font, p),
                    None => el().grow(1.0),
                }),
        )
}

fn file_row(file: &PackFile, selected: bool, p: &Paint) -> El {
    let path = file.path.clone();
    let (dir, name) = match file.path.rsplit_once('/') {
        Some((dir, name)) => (format!("{dir}/"), name.to_string()),
        None => (String::new(), file.path.clone()),
    };
    row()
        .key(format!("pack-file-{}", file.path))
        .role("button")
        .label(format!("Show {}", file.path))
        .w_full()
        .h(26.0)
        .px(6.0)
        .gap(6.0)
        .center_y()
        .radius(p.k.radius.md)
        .fill(if selected {
            p.c.pressed
        } else {
            Color::TRANSPARENT
        })
        .hover(p.c.hover)
        .focusable(true)
        .on_press(move |app: &mut App| app.pack_file = Some(path.clone()))
        .child(icon(&oc::CODE_LINES, 14.0).min_w(14.0).color(if selected {
            p.c.icon_base
        } else {
            p.c.icon_faint
        }))
        .child(
            row()
                .grow(1.0)
                .min_w(0.0)
                .children(
                    (!dir.is_empty())
                        .then(|| text(dir).font(mono(11.0, 400.0)).color(p.c.text_faint)),
                )
                .child(
                    text(name)
                        .font(mono(11.0, if selected { 500.0 } else { 400.0 }))
                        .color(if selected {
                            p.c.text_base
                        } else {
                            p.c.text_muted
                        })
                        .truncate(),
                ),
        )
}

fn source(file: &PackFile, font: LogFont, p: &Paint) -> El {
    let Some(text_body) = &file.text else {
        return note(
            &format!("{} is not text, or too big to show.", file.path),
            "",
            p,
        );
    };
    let lines = syntax::lines(text_body);
    let found = syntax::highlighter().highlight(syntax::language(&file.path), text_body);
    let theme = SyntaxTheme::from_kit(p.k);
    let width = lines.len().max(1).to_string().len();
    let style = TextStyle::new(11.5, 400.0).lh(18.0).family(font.family());
    let mut body = col().gap(0.0).py(10.0);
    for (n, line) in lines.iter().take(LINES).enumerate() {
        let spans = theme.spans(&syntax::on_line(&found, line.clone()), 0);
        body = body.child(
            row()
                .gap(14.0)
                .child(
                    text(format!("{:>width$}", n + 1))
                        .font(style)
                        .color(p.c.text_faint),
                )
                .child(
                    text(text_body[line.clone()].to_string())
                        .font(style)
                        .color(p.c.text_base)
                        .spans(spans)
                        .selectable(true)
                        .selection_color(p.k.selection()),
                ),
        );
    }
    if lines.len() > LINES {
        body = body.child(
            text(format!("\u{2026} {} more lines", lines.len() - LINES))
                .font(style)
                .color(p.c.text_muted),
        );
    }
    col()
        .key(format!("pack-source-{}", file.path))
        .grow(1.0)
        .min_w(0.0)
        .h_full()
        .min_h(0.0)
        .gap(6.0)
        .child(
            row()
                .w_full()
                .h(20.0)
                .gap(8.0)
                .center_y()
                .child(
                    text(file.path.clone())
                        .font(mono(11.5, 500.0))
                        .color(p.c.text_base)
                        .selectable(true)
                        .selection_color(p.k.selection()),
                )
                .child(
                    text(format!("{} lines", lines.len()))
                        .font(p.k.ty.small)
                        .color(p.c.text_faint),
                ),
        )
        .child(
            col()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .px(12.0)
                .radius(p.k.radius.lg)
                .fill(p.c.layer_01)
                .border(p.hair, p.c.border_muted)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .scroll_xy()
                .scroll_edge_fade(8.0)
                .child(body),
        )
}

fn note(headline: &str, detail: &str, p: &Paint) -> El {
    let mut card = col()
        .key("pack-note")
        .w_full()
        .padding(24.0)
        .gap(6.0)
        .radius(p.k.radius.lg)
        .fill(p.c.layer_01)
        .border(p.hair, p.c.border_muted)
        .child(
            text(headline.to_string())
                .font(p.k.ty.ui_medium)
                .color(p.c.text_base)
                .wrap(),
        );
    if !detail.is_empty() {
        card = card.child(
            text(detail.to_string())
                .font(mono(11.0, 400.0))
                .color(p.c.text_muted)
                .selectable(true)
                .selection_color(p.k.selection())
                .wrap(),
        );
    }
    card
}
