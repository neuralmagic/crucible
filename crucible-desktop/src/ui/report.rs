//! The Report tab: the view a pack's plugin built from the run's captured files.

mod canvas;

use std::borrow::Cow;

use crucible_view::{
    Bar, Cell, Column, ColumnKind, Node, PlotKind, Series, Stat, Tab, Tone, Width,
};
use rune::{Align, Color, El, LineCap, Shape, Tx, canvas, col, el, row, text};
use rune_kit::{Kit, Table, TableColumn};

use crate::App;
use crate::ui::{Paint, desk};

pub(crate) fn report_view(app: &App, p: &Paint) -> El {
    let Some(report) = app.shown_report() else {
        let why = if app.awaits_report() {
            "Drawing the view\u{2026}"
        } else {
            "This pack has no view."
        };
        return desk::note("report-note", why, "", p);
    };
    let view = match &report.view {
        Ok(view) => view,
        Err(error) => return desk::note("report-note", "The view could not be drawn.", error, p),
    };
    col()
        .key("report")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(12.0)
        .child(
            row()
                .w_full()
                .gap(10.0)
                .center_y()
                .child(
                    text(view.title.clone())
                        .font(p.k.ty.ui_medium)
                        .color(p.c.text_base),
                )
                .children(report.author.as_ref().map(|task| {
                    text(format!("written by {task}"))
                        .key("report-author")
                        .font(p.mono(10.5, 440.0))
                        .color(p.c.text_faint)
                })),
        )
        .child(node(&view.body, true, app, p))
}

/// `fills` when the node takes the height its parent has left, as a table's ancestors do;
/// otherwise it is as tall as its content, inside a scroll area.
fn node(node_: &Node, fills: bool, app: &App, p: &Paint) -> El {
    match node_ {
        Node::Stack { children } => {
            let mut stack = col().w_full().gap(12.0);
            if fills {
                stack = stack.grow(1.0).min_h(0.0);
            }
            for child in children {
                stack = stack.child(node(child, fills, app, p));
            }
            stack
        }
        Node::Tabs { id, tabs } => tabs_view(id, tabs, app, p),
        Node::Stats { items } => stats(items, p),
        Node::Bars { bars: all } => bars(all, p),
        Node::Plot { kind, x, y, series } => plot(*kind, x, y, series, p),
        Node::Canvas {
            width,
            height,
            commands,
        } => canvas::canvas_view(*width, *height, commands, p),
        Node::Grid { columns, rows } => grid(columns, rows, p),
        Node::Table { columns, .. } => table(columns, app, p),
        Node::Text { text: body, tone } => text(body.clone())
            .font(p.k.ty.ui)
            .color(ink(*tone, p.k).unwrap_or(p.c.text_base))
            .selectable(true)
            .selection_color(p.k.selection())
            .wrap(),
    }
}

fn ink(tone: Tone, k: &Kit) -> Option<Color> {
    match tone {
        Tone::Neutral => None,
        Tone::Muted => Some(k.c.text_muted),
        Tone::Good => Some(k.c.success.fg),
        Tone::Warn => Some(k.c.warning.fg),
        Tone::Bad => Some(k.c.danger.fg),
        Tone::Info => Some(k.c.info.fg),
    }
}

fn dot(tone: Tone, k: &Kit) -> Color {
    match tone {
        Tone::Neutral | Tone::Muted => k.c.icon_faint,
        Tone::Good => k.c.success.icon,
        Tone::Warn => k.c.warning.icon,
        Tone::Bad => k.c.danger.icon,
        Tone::Info => k.c.info.icon,
    }
}

fn tabs_view(id: &str, tabs: &[Tab], app: &App, p: &Paint) -> El {
    let picked = app.report_shown.tab(id, tabs.len());
    let options: Vec<(String, String)> = tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let label = match tab.count {
                Some(count) => format!("{} {count}", tab.label),
                None => tab.label.clone(),
            };
            (i.to_string(), label)
        })
        .collect();
    let set = id.to_string();
    let bar = desk::pills(
        &format!("report-tabs-{id}"),
        &options,
        Some(picked.to_string().as_str()),
        move |app: &mut App, value: &str| {
            if let Ok(tab) = value.parse() {
                app.report_shown.pick(&set, tab);
            }
        },
        p,
    );
    let mut shown = col()
        .key(format!("report-tabbed-{id}"))
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(10.0)
        .child(bar);
    if let Some(tab) = tabs.get(picked) {
        let fills = app.report_shown.shows_table(&tab.body);
        let body = node(&tab.body, fills, app, p);
        shown = shown.child(if fills {
            body
        } else {
            desk::scrolled(&format!("report-scroll-{id}"), body, p)
        });
    }
    shown
}

fn stats(items: &[Stat], p: &Paint) -> El {
    let mut tiles = row().key("report-stats").w_full().gap(10.0).flex_wrap();
    for item in items {
        tiles = tiles.child(
            col()
                .key(format!("report-stat-{}", item.label))
                .min_w(120.0)
                .padding(12.0)
                .gap(2.0)
                .radius(p.k.radius.lg)
                .fill(p.c.layer_01)
                .border(p.hair, p.c.border_muted)
                .child(
                    text(item.label.clone())
                        .font(p.k.ty.small_medium)
                        .color(p.c.text_muted),
                )
                .child(
                    text(item.value.clone())
                        .font(p.mono(20.0, 560.0))
                        .color(ink(item.tone, p.k).unwrap_or(p.c.text_base)),
                ),
        );
    }
    tiles
}

fn bars(all: &[Bar], p: &Paint) -> El {
    let total = |bar: &Bar| bar.segments.iter().map(|s| s.value).sum::<f64>();
    let most = all.iter().map(total).fold(0.0, f64::max);
    let key = legend(
        all.iter()
            .flat_map(|bar| &bar.segments)
            .map(|segment| (segment.label.as_str(), segment.tone)),
        p,
    );
    let mut chart = col()
        .key("report-bars")
        .w_full()
        .max_w(720.0)
        .padding(12.0)
        .gap(8.0)
        .radius(p.k.radius.lg)
        .fill(p.c.layer_01)
        .border(p.hair, p.c.border_muted)
        .child(key);
    for bar in all {
        let mut track = row().grow(1.0).min_w(0.0).h(12.0).gap(1.0);
        for segment in bar.segments.iter().filter(|s| s.value > 0.0) {
            track = track.child(
                el().grow(segment.value as f32)
                    .h_full()
                    .radius(2.0)
                    .fill(dot(segment.tone, p.k))
                    .help(format!("{} {}", segment.label, segment.value)),
            );
        }
        let rest = most - total(bar);
        if rest > 0.0 {
            track = track.child(el().grow(rest as f32).h_full());
        }
        chart = chart.child(
            row()
                .key(format!("report-bar-{}", bar.label))
                .w_full()
                .h(20.0)
                .gap(10.0)
                .center_y()
                .child(
                    text(bar.label.clone())
                        .font(p.mono(11.5, 440.0))
                        .color(p.c.text_base)
                        .w(110.0)
                        .truncate(),
                )
                .child(track)
                .child(
                    row().w(44.0).justify_end().child(
                        text(total(bar).to_string())
                            .font(p.mono(11.5, 440.0))
                            .color(p.c.text_muted),
                    ),
                ),
        );
    }
    chart
}

/// Each label once, in the order first seen, beside its colour.
fn legend<'a>(items: impl Iterator<Item = (&'a str, Tone)>, p: &Paint) -> El {
    let mut seen: Vec<&str> = Vec::new();
    let mut key = row().w_full().gap(14.0).flex_wrap();
    for (label, tone) in items {
        if seen.contains(&label) {
            continue;
        }
        seen.push(label);
        key = key.child(
            row()
                .gap(6.0)
                .center_y()
                .child(el().size(8.0, 8.0).radius(2.0).fill(dot(tone, p.k)))
                .child(
                    text(label.to_string())
                        .font(p.k.ty.small)
                        .color(p.c.text_muted),
                ),
        );
    }
    key
}

/// The plot's own coordinates: as wide as its card lets it be, and this tall in points.
const PLOT: [f32; 2] = [560.0, 220.0];
/// Where the plot starts, past the vertical axis's numbers.
const GUTTER: f32 = 48.0;

/// The least and greatest of `values`, widened to include zero when `zero`, and never empty.
fn span(values: impl Iterator<Item = f64> + Clone, zero: bool) -> (f64, f64) {
    let low = values.clone().fold(f64::INFINITY, f64::min);
    let high = values.fold(f64::NEG_INFINITY, f64::max);
    if !low.is_finite() || !high.is_finite() {
        return (0.0, 1.0);
    }
    let low = if zero { low.min(0.0) } else { low };
    if high > low {
        (low, high)
    } else {
        (low - 0.5, low + 0.5)
    }
}

fn short(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

fn plot(kind: PlotKind, x: &str, y: &str, series: &[Series], p: &Paint) -> El {
    let points = series.iter().flat_map(|s| s.points.iter());
    let (x0, x1) = span(points.clone().map(|point| point[0]), false);
    let (y0, y1) = span(points.map(|point| point[1]), true);
    let [w, h] = PLOT;
    let inset = 6.0;
    let at = |point: &[f64; 2]| {
        let across = ((point[0] - x0) / (x1 - x0)) as f32;
        let up = ((point[1] - y0) / (y1 - y0)) as f32;
        [
            inset + across * (w - 2.0 * inset),
            h - inset - up * (h - 2.0 * inset),
        ]
    };
    let mut shape = Shape::new();
    for line in 0..=4 {
        let down = inset + line as f32 / 4.0 * (h - 2.0 * inset);
        shape = shape.line([0.0, down], [w, down], 1.0, LineCap::Butt, p.c.border_muted);
    }
    for one in series {
        let color = dot(one.tone, p.k);
        let placed: Vec<[f32; 2]> = one.points.iter().map(at).collect();
        if kind == PlotKind::Line && placed.len() > 1 {
            shape = shape.stroke(placed.clone(), 1.75, color);
        }
        for [cx, cy] in placed {
            let r = 3.0;
            shape = shape.fill(
                vec![[cx, cy - r], [cx + r, cy], [cx, cy + r], [cx - r, cy]],
                color,
            );
        }
    }
    let number = |value: f64| {
        text(short(value))
            .font(p.mono(10.5, 400.0))
            .color(p.c.text_faint)
    };
    let name = |label: &str| {
        text(label.to_string())
            .font(p.k.ty.small_medium)
            .color(p.c.text_muted)
    };
    col()
        .key(format!("report-plot-{y}-{x}"))
        .w_full()
        .max_w(720.0)
        .padding(12.0)
        .gap(8.0)
        .radius(p.k.radius.lg)
        .fill(p.c.layer_01)
        .border(p.hair, p.c.border_muted)
        .child(legend(series.iter().map(|s| (s.label.as_str(), s.tone)), p))
        .child(name(y))
        .child(
            row()
                .w_full()
                .child(
                    col()
                        .w(GUTTER)
                        .h(h)
                        .child(number(y1))
                        .child(el().grow(1.0))
                        .child(number(y0)),
                )
                .child(canvas(shape.view_box(w, h)).grow(1.0).min_w(0.0).h(h)),
        )
        .child(
            row()
                .w_full()
                .pl(GUTTER)
                .child(number(x0))
                .child(el().grow(1.0))
                .child(name(x))
                .child(el().grow(1.0))
                .child(number(x1)),
        )
}

fn grid(columns: &[String], rows: &[Vec<Cell>], p: &Paint) -> El {
    let cell = |i: usize, body: El| {
        if i == 0 {
            row().grow(1.0).min_w(0.0).child(body)
        } else {
            row().w(96.0).justify_end().child(body)
        }
    };
    let mut head = row().w_full().h(26.0).px(10.0).center_y();
    for (i, title) in columns.iter().enumerate() {
        head = head.child(cell(
            i,
            text(title.clone())
                .font(p.k.ty.small_medium)
                .color(p.c.text_muted),
        ));
    }
    let mut lines = vec![head];
    for cells in rows {
        let mut line = row().w_full().h(28.0).px(10.0).center_y();
        for (i, value) in cells.iter().enumerate() {
            line = line.child(cell(
                i,
                text(value.text.clone())
                    .font(p.mono(12.0, 440.0))
                    .color(ink(value.tone, p.k).unwrap_or(p.c.text_base)),
            ));
        }
        lines.push(line);
    }
    col()
        .key("report-grid")
        .w_full()
        .max_w(720.0)
        .radius(p.k.radius.lg)
        .fill(p.c.layer_01)
        .border(p.hair, p.c.border_muted)
        .child(desk::stacked("report-grid-rows", lines, p))
}

fn table(columns: &[Column], app: &App, p: &Paint) -> El {
    let mut table = Table::new(
        |app: &App| &app.report_shown.table,
        |app: &mut App| &mut app.report_shown.table,
        |app: &App| app.report_table().map_or(0, |table| table.rows.len()),
        |app: &App, i| {
            app.report_row(i)
                .map_or(Cow::Borrowed(""), |row| Cow::Borrowed(row.id.as_str()))
        },
    );
    for column in columns {
        let sized = TableColumn::new(column.id.clone(), column.title.clone()).sortable();
        table = table.column(match column.width {
            Width::Narrow => sized.width(96.0).min(64.0),
            Width::Medium => sized.width(150.0).min(80.0),
            Width::Wide => sized.width(240.0).min(120.0),
            Width::Fill => sized.flex(1.0).min(200.0).align(Align::Start),
        });
    }
    let kinds: Vec<(String, ColumnKind)> = columns
        .iter()
        .map(|column| (column.id.clone(), column.kind))
        .collect();
    let k = p.k;
    let link = p.accent;
    table
        .name("Report")
        .cell(move |app: &App, i, column| {
            let Some(at) = kinds.iter().position(|(id, _)| id == column) else {
                return rune_kit::Cell::text("");
            };
            let Some(cell) = app.report_row(i).and_then(|row| row.cells.get(at)) else {
                return rune_kit::Cell::text("");
            };
            let kind = kinds.get(at).map_or(ColumnKind::Text, |(_, kind)| *kind);
            drawn(cell, kind, link, k)
        })
        .on_sort(|app: &mut App, column, dir, _| app.sort_report(column, dir))
        .on_open(|app: &mut App, id, tx: &mut Tx| {
            if let Some(url) = app.report_link(id) {
                let _ = tx.open_url(&url);
            }
        })
        .zebra(true)
        .build(app, k)
        .key("report-table")
        .grow(1.0)
        .min_h(0.0)
}

fn drawn(cell: &Cell, kind: ColumnKind, link: Color, k: &Kit) -> rune_kit::Cell {
    if let Some(url) = cell.href.clone() {
        let style = if kind == ColumnKind::Mono {
            k.ty.mono
        } else {
            k.ty.text_small
        };
        return rune_kit::Cell::el(
            text(cell.text.clone())
                .font(style)
                .color(link)
                .role("link")
                .label(format!("Open {url}"))
                .help(url.clone())
                .on_press_tx(move |_: &mut App, tx| {
                    let _ = tx.open_url(&url);
                }),
        );
    }
    match kind {
        ColumnKind::Badge => rune_kit::Cell::el(
            row()
                .gap(6.0)
                .center_y()
                .child(el().size(6.0, 6.0).radius(3.0).fill(dot(cell.tone, k)))
                .child(
                    text(cell.text.clone())
                        .font(k.ty.text_small)
                        .color(k.c.text_base),
                ),
        ),
        ColumnKind::Text | ColumnKind::Mono => {
            let body = rune_kit::Cell::text(cell.text.clone());
            let body = if kind == ColumnKind::Mono {
                body.mono()
            } else {
                body
            };
            match ink(cell.tone, k) {
                Some(color) => body.color(color),
                None => body,
            }
        }
    }
}
