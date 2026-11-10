//! The Files tab: the files a run's tasks captured, listed by task, one shown at a time.

use rune::{Color, El, ScrollSpec, TextStyle, Tx, col, el, row, text};
use rune_kit::diff::DiffView;
use rune_kit::syntax::SyntaxTheme;
use rune_kit::{IconButton, IconButtonVariant, Markdown, SegmentedControl, Spinner};
use rune_theme_opencode::icons as oc;

use crate::App;
use crate::captured::{self, Body, Captured, Kind};
use crate::syntax;
use crate::ui::{Paint, Pane};

/// Lines drawn per file; a longer file says how much it left out.
const LINES: usize = 4000;
const FILE_LIST: f32 = 210.0;

pub(crate) fn files_view(pane: &Pane, app: &App, p: &Paint) -> El {
    let files = match &pane.files {
        None => return note("Listing the captured files\u{2026}", "", p),
        Some(Err(error)) => return note("Could not list the captured files.", error, p),
        Some(Ok([])) => {
            return note("No task in this run captured files.", "", p);
        }
        Some(Ok(files)) => files,
    };
    let open = captured::pick(files, app.run_file.as_deref());
    let mut list = col()
        .key("files-list")
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
    for (owner, owned) in captured::by_owner(files) {
        list = list.child(
            text(owner.clone())
                .font(p.mono(10.5, 600.0))
                .color(p.c.text_faint)
                .truncate()
                .px(6.0)
                .pt(6.0)
                .pb(2.0)
                .key(format!("files-task-{owner}")),
        );
        for file in owned {
            let selected = open.is_some_and(|open| open.key == file.key);
            list = list.child(file_row(file, selected, p));
        }
    }
    row()
        .key("files")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(10.0)
        .child(list)
        .child(match open {
            Some(file) => viewer(file, pane.run_id.as_deref(), app, p),
            None => el().grow(1.0),
        })
}

fn file_row(file: &Captured, selected: bool, p: &Paint) -> El {
    let key = file.key.clone();
    row()
        .key(format!("files-file-{}", file.key))
        .role("button")
        .label(format!("Show {}", file.key))
        .w_full()
        .h(26.0)
        .px(8.0)
        .gap(8.0)
        .center_y()
        .radius(p.k.radius.md)
        .fill(if selected {
            p.c.pressed
        } else {
            Color::TRANSPARENT
        })
        .hover(p.c.hover)
        .focusable(true)
        .on_press(move |app: &mut App| app.pick_file(&key))
        .child(
            text(file.path.clone())
                .font(p.mono(11.0, if selected { 500.0 } else { 400.0 }))
                .color(if selected {
                    p.c.text_base
                } else {
                    p.c.text_muted
                })
                .grow(1.0)
                .min_w(0.0)
                .truncate(),
        )
        .child(
            text(captured::size_label(file.size))
                .font(p.mono(10.0, 400.0))
                .color(p.c.text_faint),
        )
}

fn viewer(file: &Captured, run: Option<&str>, app: &App, p: &Paint) -> El {
    let kind = Kind::of(&file.path);
    let opened = app
        .remote
        .opened_file
        .as_ref()
        .filter(|opened| opened.key == file.key && Some(opened.run.as_str()) == run);
    let body = match (file.shown(), opened.map(|opened| &opened.body)) {
        (false, _) => note(
            &format!(
                "{} is {}, more than the pane draws.",
                file.key,
                captured::size_label(file.size)
            ),
            &format!(
                "crux run-file {} {} --out <path>",
                run.unwrap_or("<run>"),
                file.key
            ),
            p,
        ),
        (true, None) => col()
            .key("files-reading")
            .w_full()
            .grow(1.0)
            .center()
            .child(Spinner::new().size(16.0).build(p.k)),
        (true, Some(Err(error))) => note("Could not read the file.", error, p),
        (true, Some(Ok(Body::Binary(len)))) => note(
            &format!("{} is {len} bytes that are not text.", file.key),
            "",
            p,
        ),
        (true, Some(Ok(Body::Text(body)))) => content(kind, &file.key, body, app, p),
    };
    let disk = opened.and_then(|opened| opened.disk.as_ref());
    let head = row()
        .w_full()
        .h(22.0)
        .gap(8.0)
        .center_y()
        .child(
            text(file.key.clone())
                .font(p.mono(11.5, 500.0))
                .color(p.c.text_base)
                .selectable(true)
                .selection_color(p.k.selection())
                .grow(1.0)
                .min_w(0.0)
                .truncate()
                .help(file.key.clone()),
        )
        .children((kind == Kind::Markdown).then(|| {
            SegmentedControl::new(&["Preview", "Source"], Some(app.files_md_source as usize))
                .label("Show")
                .fit()
                .on_pick(|app: &mut App, i: usize, _: &mut Tx| app.files_md_source = i == 1)
                .build(p.k)
                .key("files-md-mode")
        }))
        .children(disk.map(|disk| {
            let path = disk.display().to_string();
            IconButton::new(&oc::COPY)
                .variant(IconButtonVariant::GhostMuted)
                .label("Copy path")
                .help(path.clone())
                .on_press_tx(move |_: &mut App, tx: &mut Tx| tx.copy(path.clone()))
                .build(p.k)
                .key("files-copy-path")
        }))
        .children(disk.map(|_| {
            IconButton::new(&oc::EDIT)
                .variant(IconButtonVariant::GhostMuted)
                .label("Open in editor")
                .help("Open in editor")
                .on_press(|app: &mut App| app.open_captured())
                .build(p.k)
                .key("files-open")
        }));
    col()
        .key(format!("files-view-{}", file.key))
        .grow(1.0)
        .min_w(0.0)
        .h_full()
        .min_h(0.0)
        .gap(6.0)
        .child(head)
        .children(
            app.editor_error
                .as_ref()
                .map(|error| text(error.clone()).font(p.k.ty.small).color(p.fail).wrap()),
        )
        .child(body)
}

fn content(kind: Kind, key: &str, body: &str, app: &App, p: &Paint) -> El {
    let frame = || {
        col()
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .radius(p.k.radius.lg)
            .fill(p.c.layer_01)
            .border(p.hair, p.c.border_muted)
            .clip_rounded(p.k.radius.lg)
    };
    match kind {
        Kind::Patch => frame().child(
            DiffView::new(
                |app: &App| &app.remote.file_diff,
                |app: &mut App| &mut app.remote.file_diff,
            )
            .key("files-diff")
            .build(app, p.k),
        ),
        Kind::Markdown if !app.files_md_source => frame().px(16.0).child(
            col()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .scroll_edge_fade(8.0)
                .child(
                    col().w_full().py(12.0).child(
                        Markdown::new(body.to_string())
                            .selectable(true)
                            .build(p.k)
                            .key(format!("files-markdown-{key}")),
                    ),
                ),
        ),
        Kind::Json | Kind::JsonLines => {
            frame().child(code(&captured::display(kind, body), Some("json"), app, p))
        }
        Kind::Markdown => frame().child(code(body, Some("markdown"), app, p)),
        Kind::Text => frame().child(code(body, syntax::language(key), app, p)),
    }
}

/// `source` as numbered lines, highlighted as `language`.
fn code(source: &str, language: Option<&str>, app: &App, p: &Paint) -> El {
    let lines = syntax::lines(source);
    let found = syntax::highlighter().highlight(language, source);
    let theme = SyntaxTheme::from_kit(p.k);
    let width = lines.len().max(1).to_string().len();
    let style = p.sized(
        TextStyle::new(11.5, 400.0)
            .lh(18.0)
            .family(app.settings.log_font.family()),
    );
    let mut body = col().gap(0.0).py(10.0);
    for (n, line) in lines.iter().take(LINES).enumerate() {
        body = body.child(
            row()
                .gap(14.0)
                .child(
                    text(format!("{:>width$}", n + 1))
                        .font(style)
                        .color(p.c.text_faint),
                )
                .child(
                    text(source[line.clone()].to_string())
                        .font(style)
                        .color(p.c.text_base)
                        .spans(theme.spans(&syntax::on_line(&found, line.clone()), 0))
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
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .px(12.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_xy()
        .scroll_edge_fade(8.0)
        .child(body)
}

fn note(headline: &str, detail: &str, p: &Paint) -> El {
    let mut card = col()
        .key("files-note")
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
                .font(p.mono(11.0, 400.0))
                .color(p.c.text_muted)
                .selectable(true)
                .selection_color(p.k.selection())
                .wrap(),
        );
    }
    card
}
