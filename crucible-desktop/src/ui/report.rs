//! The Report tab: the view a pack's plugin built from the run's captured files.

mod axis;
mod canvas;
mod controls;
mod figures;
mod flow;
mod hue;
mod plot;

use std::borrow::Cow;
use std::sync::Arc;

use crucible_view::{Cell, Column, ColumnKind, Node, Row, Stat, Tab, Tone, Width};
use rune::{Align, Color, El, Tx, col, el, icon, row, text};
use rune_kit::{IconButton, IconButtonVariant, Kit, Markdown, Table, TableColumn, TextInput};
use rune_theme_opencode::icons as oc;

use crate::App;
use crate::report::Report;
use crate::report::shown::Shown;
use crate::ui::report::axis::unit;
use crate::ui::report::figures::figure;
use crate::ui::report::hue::{dot, ink};
use crate::ui::{Paint, desk};

/// Where a drawn report and its reader state live on the app, and what its links lead to.
pub(crate) trait Slot: 'static {
    fn shown(app: &App) -> &Shown;
    fn shown_mut(app: &mut App) -> &mut Shown;
    /// Whether an address the report holds leads somewhere.
    fn leads(app: &App, href: &str) -> bool;
    /// Opens an address the report holds.
    fn follow(app: &mut App, href: &str, tx: &mut Tx);
    /// Builds the report again with what its controls now hold.
    fn ask(app: &mut App);
}

/// The Report pane's report: the selected local pack's, or the one built for the selected
/// live run.
struct RunReport;

impl Slot for RunReport {
    fn shown(app: &App) -> &Shown {
        &app.report_shown.shown
    }

    fn shown_mut(app: &mut App) -> &mut Shown {
        &mut app.report_shown.shown
    }

    fn leads(app: &App, href: &str) -> bool {
        app.report_leads(href)
    }

    fn follow(app: &mut App, href: &str, tx: &mut Tx) {
        app.follow_report_link(href, tx);
    }

    fn ask(app: &mut App) {
        app.sync_report();
    }
}

pub(crate) fn report_view(app: &App, p: &Paint, layer: &mut Vec<El>) -> El {
    let followed = |report: &&Arc<Report>| RunReport::shown(app).holds(Some(report));
    let Some(report) = app.shown_report().filter(followed) else {
        let why = if app.awaits_report() {
            "Drawing the view\u{2026}"
        } else {
            "This pack has no view."
        };
        return desk::note("report-note", why, "", p);
    };
    view::<RunReport>(report, app, p, layer)
}

/// `report`'s title, controls and body.
fn view<S: Slot>(report: &Report, app: &App, p: &Paint, layer: &mut Vec<El>) -> El {
    let controls = controls::controls::<S>(S::shown(app), p, layer);
    let view = match &report.view {
        Ok(view) => view,
        Err(error) => {
            let note = desk::note("report-note", "The view could not be drawn.", error, p);
            return match controls {
                Some(controls) => col()
                    .key("report")
                    .w_full()
                    .gap(12.0)
                    .child(controls)
                    .child(note),
                None => note,
            };
        }
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
        .children(controls)
        .child(if S::shown(app).fills(&view.body) {
            node::<S>(&view.body, true, "0", app, p)
        } else {
            desk::scrolled(
                "report-scroll",
                node::<S>(&view.body, false, "0", app, p),
                p,
            )
        })
}

/// The narrowest a child of `columns` is drawn before the next one wraps under it.
const COLUMN: f32 = 320.0;
/// A card's padding on each side.
const PAD: f32 = 12.0;
/// What a card adds to the width of what it holds: its padding on both sides.
const CARD: f32 = 2.0 * PAD;
/// The widest a card or a block of text is drawn.
const MEASURE: f32 = 720.0;

/// A card [`MEASURE`] wide at most, holding a chart.
fn card(p: &Paint) -> El {
    desk::surface(p)
        .w_full()
        .max_w(MEASURE)
        .padding(PAD)
        .gap(8.0)
}

/// The width `node` draws at when it has the room: its widest canvas or flow.
fn least_width(node: &Node) -> f32 {
    match node {
        Node::Canvas { width, .. } => *width,
        Node::Flow { .. } => flow::WIDTH + CARD,
        _ => node.children().map(least_width).fold(0.0, f32::max),
    }
}

/// `fills` when the node takes the height its parent has left, as a table's ancestors do;
/// otherwise it is as tall as its content, inside a scroll area. `at` is the node's place in
/// the view, unique among the nodes drawn.
fn node<S: Slot>(node_: &Node, fills: bool, at: &str, app: &App, p: &Paint) -> El {
    match node_ {
        Node::Stack { children } => {
            let mut stack = col().w_full().gap(12.0);
            if fills {
                stack = stack.grow(1.0).min_h(0.0);
            }
            for (i, child) in children.iter().enumerate() {
                let place = format!("{at}-{i}");
                let drawn = node::<S>(child, fills && S::shown(app).fills(child), &place, app, p);
                stack = stack.child(match child {
                    Node::Tabs { .. } | Node::Table { .. } | Node::Section { .. } => drawn,
                    _ => drawn.key(format!("report-part-{i}")),
                });
            }
            stack
        }
        Node::Columns { children } => {
            let key = format!("report-columns-{at}");
            let widest = least_width(node_).max(COLUMN);
            let room = rune::memo::layout_with(&key, move |rect| rect[2].min(widest).floor())
                .unwrap_or(widest);
            let mut line = row().key(key).w_full().gap(12.0).flex_wrap().items_start();
            for (i, child) in children.iter().enumerate() {
                line = line.child(
                    col()
                        .key(format!("report-column-{i}"))
                        .grow(1.0)
                        .min_w(COLUMN.max(least_width(child)).min(room))
                        .child(node::<S>(child, false, &format!("{at}-{i}"), app, p)),
                );
            }
            col().w_full().child(line)
        }
        Node::Section {
            id,
            title,
            open,
            body,
        } => section::<S>(id, title, *open, body, fills, app, p),
        Node::Tabs { id, tabs } => tabs_view::<S>(id, tabs, fills, app, p),
        Node::Stats { items } => stats(items, p),
        Node::Meter {
            label,
            value,
            max,
            tone,
        } => meter(at, label, *value, *max, *tone, p),
        Node::Bars { bars } => plot::bars(bars, p),
        Node::Plot {
            kind,
            x,
            y,
            series,
            x_scale,
            y_scale,
            categories,
        } => plot::plot(*kind, [x, y], series, [*x_scale, *y_scale], categories, p),
        Node::Histogram {
            x,
            values,
            bins,
            tone,
        } => plot::histogram(x, values, *bins, *tone, p),
        Node::Heatmap { x, y, values, tone } => plot::heatmap(x, y, values, *tone, p),
        Node::Flow { nodes, links } => {
            match flow::drawing(nodes, links, &|hue| hue::style(hue, p.k)) {
                Some((height, commands)) => card(p).child(canvas::fitted(
                    format!("report-canvas-{at}"),
                    flow::WIDTH,
                    height,
                    &commands,
                    p,
                )),
                None => el(),
            }
        }
        Node::Canvas {
            width,
            height,
            commands,
        } => canvas::fitted(format!("report-canvas-{at}"), *width, *height, commands, p),
        Node::Grid { columns, rows } => grid(columns, rows, p),
        Node::Table { columns, .. } => table::<S>(columns, app, p),
        Node::Text { text: body, tone } => text(body.clone())
            .font(p.k.ty.ui)
            .color(ink(*tone, p.k))
            .selectable(true)
            .selection_color(p.k.selection())
            .wrap(),
        Node::Note {
            headline,
            detail,
            tone,
        } => desk::toned_note(
            &format!("report-note-{at}"),
            headline,
            detail,
            ink(*tone, p.k),
            p,
        ),
        Node::Markdown { text: body } => markdown::<S>(body.clone(), "report-markdown", p),
        Node::Mermaid { source } => markdown::<S>(fenced(source), "report-mermaid", p),
    }
}

/// Markdown whose links lead to the web or into the run.
fn markdown<S: Slot>(body: String, key: &str, p: &Paint) -> El {
    Markdown::new(body)
        .selectable(true)
        .on_link_tx(|app: &mut App, href: &str, tx: &mut Tx| S::follow(app, href, tx))
        .build(p.k)
        .key(key.to_string())
        .w_full()
        .max_w(MEASURE)
}

/// `source` as Markdown holding one `mermaid` fence, longer than any run of backticks in it.
fn fenced(source: &str) -> String {
    let longest = source.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}mermaid\n{source}\n{fence}\n")
}

/// A title the reader clicks to fold or unfold `body` under it.
fn section<S: Slot>(
    id: &str,
    title: &str,
    open: bool,
    body: &Node,
    fills: bool,
    app: &App,
    p: &Paint,
) -> El {
    let unfolded = S::shown(app).open(id, open);
    let glyph = if unfolded {
        p.k.glyph.triangle_down
    } else {
        p.k.glyph.triangle_right
    };
    let set = id.to_string();
    let header = row()
        .key(format!("report-section-{id}"))
        .role("button")
        .label(title.to_string())
        .expanded(unfolded)
        .focusable(true)
        .focus_ring(2.0, 0.0, p.c.border_focus)
        .w_full()
        .h(28.0)
        .gap(6.0)
        .center_y()
        .on_press(move |app: &mut App| S::shown_mut(app).toggle(&set, open))
        .child(icon(glyph, 14.0).color(p.c.icon_weaker))
        .child(
            text(title.to_string())
                .font(p.k.ty.small_medium)
                .color(p.c.text_base)
                .truncate(),
        );
    let mut shown = col()
        .key(format!("report-sectioned-{id}"))
        .w_full()
        .gap(8.0)
        .child(header);
    if fills {
        shown = shown.grow(1.0).min_h(0.0);
    }
    if unfolded {
        shown = shown.child(node::<S>(
            body,
            fills,
            &format!("s{}:{id}", id.len()),
            app,
            p,
        ));
    }
    shown
}

fn tabs_view<S: Slot>(id: &str, tabs: &[Tab], fills: bool, app: &App, p: &Paint) -> El {
    let picked = S::shown(app).tab(id, tabs.len());
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
                S::shown_mut(app).pick(&set, tab);
            }
        },
        p,
    );
    let mut shown = col()
        .key(format!("report-tabbed-{id}"))
        .w_full()
        .gap(10.0)
        .child(bar);
    if fills {
        shown = shown.grow(1.0).min_h(0.0);
    }
    if let Some(tab) = tabs.get(picked) {
        let filled = S::shown(app).fills(&tab.body);
        let body = node::<S>(
            &tab.body,
            fills && filled,
            &format!("t{}:{id}", id.len()),
            app,
            p,
        );
        shown = shown.child(if !fills || filled {
            body
        } else {
            desk::scrolled(&format!("report-scroll-{id}"), body, p)
        });
    }
    shown
}

fn stats(items: &[Stat], p: &Paint) -> El {
    let mut tiles = row().key("report-stats").w_full().gap(10.0).flex_wrap();
    for (i, item) in items.iter().enumerate() {
        tiles = tiles.child(
            desk::surface(p)
                .key(format!("report-stat-{i}"))
                .min_w(120.0)
                .padding(12.0)
                .gap(2.0)
                .child(
                    text(item.label.clone())
                        .font(p.k.ty.small_medium)
                        .color(p.c.text_muted),
                )
                .child(
                    text(item.value.clone())
                        .font(p.mono(20.0, 560.0))
                        .color(ink(item.tone, p.k)),
                ),
        );
    }
    tiles
}

/// `value` of `max` as a bar, full at `max` and empty at 0 or below.
fn meter(at: &str, label: &str, value: f64, max: f64, tone: Tone, p: &Paint) -> El {
    let share = unit(value / max, 0.0);
    col()
        .key(format!("report-meter-{at}"))
        .w_full()
        .max_w(MEASURE)
        .gap(6.0)
        .child(
            row()
                .w_full()
                .gap(10.0)
                .center_y()
                .child(
                    text(label.to_string())
                        .font(p.k.ty.small_medium)
                        .color(p.c.text_muted)
                        .grow(1.0)
                        .min_w(0.0)
                        .truncate(),
                )
                .child(
                    text(format!("{} / {}", figure(value), figure(max)))
                        .font(p.mono(12.0, 480.0))
                        .color(ink(tone, p.k)),
                ),
        )
        .child(track(share, 6.0, dot(tone, p.k), p.c.border_muted))
}

/// A bar `tall` points high on `under`, filled in `color` for `share` of its width.
fn track(share: f32, tall: f32, color: Color, under: Color) -> El {
    row()
        .w_full()
        .h(tall)
        .radius(tall / 2.0)
        .fill(under)
        .children((share > 0.0).then(|| el().grow(share).h_full().radius(tall / 2.0).fill(color)))
        .children((share < 1.0).then(|| el().grow(1.0 - share).h_full()))
}

/// `body` over a thin bar of `cell`'s share in its tone, when it has one.
fn with_bar(body: El, cell: &Cell, k: &Kit) -> El {
    match cell.bar {
        Some(bar) => col().grow(1.0).min_w(0.0).gap(3.0).child(body).child(track(
            unit(bar, 0.0),
            3.0,
            dot(cell.tone, k),
            k.c.border_muted,
        )),
        None => body,
    }
}

/// `body` cut to its width, with the whole text on hover when cut.
fn cut(body: &str, font: rune::TextStyle, color: Color, align: Align) -> El {
    text(body.to_string())
        .font(font)
        .color(color)
        .align(align)
        .truncate()
        .w_full()
        .min_w(0.0)
        .help_when_truncated(body.to_string())
}

fn grid(columns: &[String], rows: &[Vec<Cell>], p: &Paint) -> El {
    let cell = |i: usize, body: El| {
        if i == 0 {
            row().grow(1.0).min_w(0.0).child(body)
        } else {
            row().w(96.0).justify_end().child(body)
        }
    };
    let align = |i: usize| if i == 0 { Align::Start } else { Align::End };
    let mut head = row().w_full().h(26.0).px(10.0).center_y();
    for (i, title) in columns.iter().enumerate() {
        head = head.child(cell(
            i,
            cut(title, p.k.ty.small_medium, p.c.text_muted, align(i)),
        ));
    }
    let mut lines = vec![head];
    for cells in rows {
        let mut line = row().w_full().h(28.0).px(10.0).center_y();
        for (i, value) in cells.iter().enumerate() {
            let font = p.mono(12.0, 440.0);
            let body = cut(&value.text, font, ink(value.tone, p.k), align(i));
            line = line.child(cell(i, with_bar(body, value, p.k)));
        }
        lines.push(line);
    }
    desk::surface(p)
        .key("report-grid")
        .w_full()
        .max_w(MEASURE)
        .child(desk::stacked("report-grid-rows", lines, p))
}

fn table<S: Slot>(columns: &[Column], app: &App, p: &Paint) -> El {
    let mut table = Table::new(
        |app: &App| &S::shown(app).table,
        |app: &mut App| &mut S::shown_mut(app).table,
        |app: &App| S::shown(app).rows(),
        |app: &App, i| {
            S::shown(app)
                .row(i)
                .map_or(Cow::Borrowed(""), |row| Cow::Borrowed(row.id.as_str()))
        },
    );
    for column in columns {
        let sized = TableColumn::new(column.id.clone(), column.title.clone()).sortable();
        let sized = if column.kind == ColumnKind::Number {
            sized.align(Align::End)
        } else {
            sized
        };
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
    let table = table
        .name("Report")
        .cell(move |app: &App, i, column| {
            let Some(at) = kinds.iter().position(|(id, _)| id == column) else {
                return rune_kit::Cell::text("");
            };
            let Some(cell) = S::shown(app).row(i).and_then(|row| row.cells.get(at)) else {
                return rune_kit::Cell::text("");
            };
            let kind = kinds.get(at).map_or(ColumnKind::Text, |(_, kind)| *kind);
            let leads = cell.href.as_deref().is_some_and(|href| S::leads(app, href));
            drawn::<S>(cell, kind, leads.then_some(link), k)
        })
        .on_sort(|app: &mut App, column, dir, _| S::shown_mut(app).sort(column, dir))
        .on_open(|app: &mut App, id, tx: &mut Tx| {
            if let Some(href) = S::shown(app).link(id) {
                S::follow(app, &href, tx);
            }
        })
        .zebra(true)
        .build(app, k)
        .key("report-table")
        .grow(1.0)
        .min_h(0.0);
    col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(10.0)
        .child(filter::<S>(app, p))
        .child(table)
        .children(S::shown(app).detail().map(|shown| detail::<S>(shown, p)))
}

/// The box that filters the shown table, a button that copies the rows it shows, and how many
/// rows pass while it holds something.
fn filter<S: Slot>(app: &App, p: &Paint) -> El {
    let shown = S::shown(app);
    let edit = &shown.filter;
    let mut input = TextInput::new(edit)
        .placeholder("Filter rows")
        .leading_icon(&oc::MAGNIFYING_GLASS)
        .on_edit(|app: &mut App| &mut S::shown_mut(app).filter)
        .on_change(|app: &mut App, _: &mut Tx| S::shown_mut(app).arrange());
    if !edit.text.is_empty() {
        input = input.clear_button();
    }
    let total = shown.table().map_or(0, |table| table.rows.len());
    row()
        .w_full()
        .gap(10.0)
        .center_y()
        .child(input.build(p.k).key("report-filter").w(280.0).min_w(0.0))
        .child(
            IconButton::new(&oc::COPY)
                .variant(IconButtonVariant::GhostMuted)
                .label("Copy as CSV")
                .help("Copy as CSV".to_string())
                .on_press_tx(|app: &mut App, tx: &mut Tx| {
                    if let Some(csv) = S::shown(app).csv() {
                        tx.copy(csv);
                    }
                })
                .build(p.k)
                .key("report-copy-csv"),
        )
        .children((!edit.text.trim().is_empty()).then(|| {
            text(format!("{} of {total}", shown.rows()))
                .key("report-filter-count")
                .font(p.mono(11.0, 440.0))
                .color(p.c.text_muted)
        }))
}

/// The selected row's detail, under the table.
fn detail<S: Slot>(shown: &Row, p: &Paint) -> El {
    let body = shown.detail.clone().unwrap_or_default();
    desk::surface(p)
        .key("report-detail")
        .w_full()
        .h(260.0)
        .child(
            row()
                .w_full()
                .h(32.0)
                .pl(12.0)
                .pr(6.0)
                .center_y()
                .child(
                    text(shown.id.clone())
                        .font(p.mono(11.5, 520.0))
                        .color(p.c.text_muted)
                        .truncate(),
                )
                .child(el().grow(1.0))
                .child(
                    IconButton::new(&oc::XMARK_SMALL)
                        .variant(IconButtonVariant::GhostMuted)
                        .label("Close")
                        .on_press(|app: &mut App| S::shown_mut(app).table.clear_selection())
                        .build(p.k)
                        .key("report-detail-close"),
                ),
        )
        .child(el().h(p.hair).w_full().fill(p.c.border_muted))
        .child(desk::scrolled(
            "report-detail-scroll",
            col().w_full().padding(12.0).child(markdown::<S>(
                body,
                &format!("report-detail-{}", shown.id),
                p,
            )),
            p,
        ))
}

/// `link` is the colour of a cell whose address leads somewhere.
fn drawn<S: Slot>(cell: &Cell, kind: ColumnKind, link: Option<Color>, k: &Kit) -> rune_kit::Cell {
    if let (Some(url), Some(link)) = (cell.href.clone(), link) {
        let style = if kind == ColumnKind::Mono {
            k.ty.mono
        } else {
            k.ty.text_small
        };
        return rune_kit::Cell::el(with_bar(
            text(cell.text.clone())
                .font(style)
                .color(link)
                .role("link")
                .label(format!("Open {url}"))
                .help(url.clone())
                .on_press_tx(move |app: &mut App, tx| S::follow(app, &url, tx)),
            cell,
            k,
        ));
    }
    match (kind, cell.bar) {
        (ColumnKind::Badge, _) => rune_kit::Cell::el(with_bar(
            row()
                .gap(6.0)
                .center_y()
                .child(el().size(6.0, 6.0).radius(3.0).fill(dot(cell.tone, k)))
                .child(
                    text(cell.text.clone())
                        .font(k.ty.text_small)
                        .color(k.c.text_base),
                ),
            cell,
            k,
        )),
        (ColumnKind::Text | ColumnKind::Mono | ColumnKind::Number, Some(_)) => {
            let font = if kind == ColumnKind::Text {
                k.ty.text_small
            } else {
                k.ty.mono.features("'tnum' 1")
            };
            let align = if kind == ColumnKind::Number {
                Align::End
            } else {
                Align::Start
            };
            rune_kit::Cell::el(with_bar(
                cut(&cell.text, font, ink(cell.tone, k), align),
                cell,
                k,
            ))
        }
        (ColumnKind::Text | ColumnKind::Mono | ColumnKind::Number, None) => {
            let body = rune_kit::Cell::text(cell.text.clone());
            let body = if kind == ColumnKind::Text {
                body
            } else {
                body.mono()
            };
            body.color(ink(cell.tone, k))
        }
    }
}

#[cfg(test)]
mod tests {
    use crucible_view::{Context2d, Node, Tab};

    use crate::ui::report::{CARD, fenced, flow, least_width};

    #[test]
    fn a_column_is_as_wide_as_its_widest_canvas_or_flow() {
        let canvas = |width: f32| Context2d::new(width, 10.0).finish();
        assert_eq!(least_width(&Node::Markdown { text: "x".into() }), 0.0);
        assert_eq!(least_width(&Node::Stack { children: vec![] }), 0.0);
        assert_eq!(least_width(&canvas(4_000.0)), 4_000.0);
        let flowing = Node::Flow {
            nodes: vec![],
            links: vec![],
        };
        assert_eq!(least_width(&flowing), flow::WIDTH + CARD);
        let nested = Node::Section {
            id: "s".into(),
            title: "S".into(),
            open: false,
            body: Box::new(Node::Tabs {
                id: "t".into(),
                tabs: vec![
                    Tab {
                        label: "a".into(),
                        count: None,
                        body: canvas(300.0),
                    },
                    Tab {
                        label: "b".into(),
                        count: None,
                        body: Node::Columns {
                            children: vec![canvas(120.0), canvas(700.0)],
                        },
                    },
                ],
            }),
        };
        assert_eq!(least_width(&nested), 700.0);
    }

    #[test]
    fn a_diagram_s_fence_is_longer_than_any_run_of_backticks_in_it() {
        assert_eq!(
            fenced("graph TD\n  a --> b"),
            "```mermaid\ngraph TD\n  a --> b\n```\n"
        );
        assert_eq!(
            fenced("graph TD\n```\n# not a heading"),
            "````mermaid\ngraph TD\n```\n# not a heading\n````\n"
        );
        assert!(fenced("a ````` b").starts_with("``````mermaid\n"));
        assert_eq!(fenced(""), "```mermaid\n\n```\n");
    }
}
