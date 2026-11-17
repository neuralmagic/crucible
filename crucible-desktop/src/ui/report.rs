//! The Report tab: the view a pack's plugin built from the run's captured files.

mod axis;
mod canvas;
mod flow;
mod hue;
mod plot;

use std::borrow::Cow;

use crucible_view::{Cell, Column, ColumnKind, Node, Row, Stat, Tab, Tone, Width};
use rune::{Align, Color, El, Tx, col, el, icon, row, text};
use rune_kit::{IconButton, IconButtonVariant, Kit, Markdown, Table, TableColumn, TextInput};
use rune_theme_opencode::icons as oc;

use crate::App;
use crate::ui::report::hue::dot;
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
        .child(if app.report_shown.fills(&view.body) {
            node(&view.body, true, app, p)
        } else {
            desk::scrolled("report-scroll", node(&view.body, false, app, p), p)
        })
}

/// The narrowest a child of `columns` is drawn before the next one wraps under it.
const COLUMN: f32 = 320.0;
/// What a card adds to the width of what it holds: its padding on both sides.
const CARD: f32 = 24.0;

/// The width `node` draws at whatever room it is given: its widest canvas or flow.
fn least_width(node: &Node) -> f32 {
    let widest =
        |nodes: &mut dyn Iterator<Item = &Node>| nodes.map(least_width).fold(0.0, f32::max);
    match node {
        Node::Stack { children } | Node::Columns { children } => widest(&mut children.iter()),
        Node::Section { body, .. } => least_width(body),
        Node::Tabs { tabs, .. } => widest(&mut tabs.iter().map(|tab| &tab.body)),
        Node::Canvas { width, .. } => *width,
        Node::Flow { .. } => flow::WIDTH + CARD,
        Node::Stats { .. }
        | Node::Bars { .. }
        | Node::Plot { .. }
        | Node::Histogram { .. }
        | Node::Heatmap { .. }
        | Node::Grid { .. }
        | Node::Table { .. }
        | Node::Text { .. }
        | Node::Markdown { .. }
        | Node::Mermaid { .. } => 0.0,
    }
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
            for (i, child) in children.iter().enumerate() {
                let drawn = node(child, fills && app.report_shown.fills(child), app, p);
                stack = stack.child(match child {
                    Node::Tabs { .. } | Node::Table { .. } | Node::Section { .. } => drawn,
                    _ => drawn.key(format!("report-part-{i}")),
                });
            }
            stack
        }
        Node::Columns { children } => {
            let mut line = row().w_full().gap(12.0).flex_wrap().items_start();
            for (i, child) in children.iter().enumerate() {
                line = line.child(
                    col()
                        .key(format!("report-column-{i}"))
                        .grow(1.0)
                        .min_w(COLUMN.max(least_width(child)))
                        .child(node(child, false, app, p)),
                );
            }
            line
        }
        Node::Section {
            id,
            title,
            open,
            body,
        } => section(id, title, *open, body, fills, app, p),
        Node::Tabs { id, tabs } => tabs_view(id, tabs, fills, app, p),
        Node::Stats { items } => stats(items, p),
        Node::Bars { bars } => plot::bars(bars, p),
        Node::Plot {
            kind,
            x,
            y,
            series,
            x_scale,
            y_scale,
            categories,
        } => plot::plot(
            &plot::Plotted {
                kind: *kind,
                x,
                y,
                series,
                x_scale: *x_scale,
                y_scale: *y_scale,
                categories,
            },
            p,
        ),
        Node::Histogram {
            x,
            values,
            bins,
            tone,
        } => plot::histogram(x, values, *bins, *tone, p),
        Node::Heatmap { x, y, values, tone } => plot::heatmap(x, y, values, *tone, p),
        Node::Flow { nodes, links } => match flow::drawing(nodes, links, &flow_palette(p)) {
            Some((height, commands)) => col()
                .w_full()
                .max_w(720.0)
                .padding(CARD / 2.0)
                .radius(p.k.radius.lg)
                .fill(p.c.layer_01)
                .border(p.hair, p.c.border_muted)
                .child(canvas::canvas_view(flow::WIDTH, height, &commands, p)),
            None => el(),
        },
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
        Node::Markdown { text: body } => markdown(body.clone(), "report-markdown", p),
        Node::Mermaid { source } => markdown(fenced(source), "report-mermaid", p),
    }
}

/// The palette's colours as a flow's stages are drawn in them.
fn flow_palette(p: &Paint) -> Vec<crucible_view::Style> {
    (0..hue::palette(p.dark).len())
        .map(|i| hue::style(hue::Hue::Palette(i), p.k))
        .collect()
}

/// Markdown whose links lead to the web or into the run.
fn markdown(body: String, key: &str, p: &Paint) -> El {
    Markdown::new(body)
        .selectable(true)
        .on_link_tx(|app: &mut App, href: &str, tx: &mut Tx| app.follow_report_link(href, tx))
        .build(p.k)
        .key(key.to_string())
        .w_full()
        .max_w(720.0)
}

/// `source` as Markdown holding one `mermaid` fence, longer than any run of backticks in it.
fn fenced(source: &str) -> String {
    let longest = source.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}mermaid\n{source}\n{fence}\n")
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

/// A title the reader clicks to fold or unfold `body` under it.
fn section(
    id: &str,
    title: &str,
    open: bool,
    body: &Node,
    fills: bool,
    app: &App,
    p: &Paint,
) -> El {
    let unfolded = app.report_shown.open(id, open);
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
        .on_press(move |app: &mut App| app.toggle_report_section(&set, open))
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
        shown = shown.child(node(body, fills, app, p));
    }
    shown
}

fn tabs_view(id: &str, tabs: &[Tab], fills: bool, app: &App, p: &Paint) -> El {
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
        .gap(10.0)
        .child(bar);
    if fills {
        shown = shown.grow(1.0).min_h(0.0);
    }
    if let Some(tab) = tabs.get(picked) {
        let filled = app.report_shown.fills(&tab.body);
        let body = node(&tab.body, fills && filled, app, p);
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
        |app: &App| app.report_rows(),
        |app: &App, i| {
            app.report_row(i)
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
            let Some(cell) = app.report_row(i).and_then(|row| row.cells.get(at)) else {
                return rune_kit::Cell::text("");
            };
            let kind = kinds.get(at).map_or(ColumnKind::Text, |(_, kind)| *kind);
            let leads = cell
                .href
                .as_deref()
                .is_some_and(|href| app.report_leads(href));
            drawn(cell, kind, leads.then_some(link), k)
        })
        .on_sort(|app: &mut App, column, dir, _| app.sort_report(column, dir))
        .on_open(|app: &mut App, id, tx: &mut Tx| {
            if let Some(href) = app.report_link(id) {
                app.follow_report_link(&href, tx);
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
        .child(filter(app, p))
        .child(table)
        .children(app.report_detail().map(|shown| detail(shown, p)))
}

/// The box that filters the shown table, and how many rows pass while it holds something.
fn filter(app: &App, p: &Paint) -> El {
    let edit = &app.report_shown.filter;
    let mut input = TextInput::new(edit)
        .placeholder("Filter rows")
        .leading_icon(&oc::MAGNIFYING_GLASS)
        .on_edit(|app: &mut App| &mut app.report_shown.filter)
        .on_change(|app: &mut App, _: &mut Tx| app.filter_report());
    if !edit.text.is_empty() {
        input = input.clear_button();
    }
    let total = app.report_table().map_or(0, |table| table.rows.len());
    row()
        .w_full()
        .gap(10.0)
        .center_y()
        .child(input.build(p.k).key("report-filter").w(280.0).min_w(0.0))
        .children((!edit.text.trim().is_empty()).then(|| {
            text(format!("{} of {total}", app.report_rows()))
                .key("report-filter-count")
                .font(p.mono(11.0, 440.0))
                .color(p.c.text_muted)
        }))
}

/// The selected row's detail, under the table.
fn detail(shown: &Row, p: &Paint) -> El {
    let body = shown.detail.clone().unwrap_or_default();
    col()
        .key("report-detail")
        .w_full()
        .h(260.0)
        .radius(p.k.radius.lg)
        .fill(p.c.layer_01)
        .border(p.hair, p.c.border_muted)
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
                        .on_press(|app: &mut App| app.report_shown.table.clear_selection())
                        .build(p.k)
                        .key("report-detail-close"),
                ),
        )
        .child(el().h(p.hair).w_full().fill(p.c.border_muted))
        .child(desk::scrolled(
            "report-detail-scroll",
            col().w_full().padding(12.0).child(markdown(
                body,
                &format!("report-detail-{}", shown.id),
                p,
            )),
            p,
        ))
}

/// `link` is the colour of a cell whose address leads somewhere.
fn drawn(cell: &Cell, kind: ColumnKind, link: Option<Color>, k: &Kit) -> rune_kit::Cell {
    if let (Some(url), Some(link)) = (cell.href.clone(), link) {
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
                .on_press_tx(move |app: &mut App, tx| app.follow_report_link(&url, tx)),
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
        ColumnKind::Text | ColumnKind::Mono | ColumnKind::Number => {
            let body = rune_kit::Cell::text(cell.text.clone());
            let body = if kind == ColumnKind::Text {
                body
            } else {
                body.mono()
            };
            match ink(cell.tone, k) {
                Some(color) => body.color(color),
                None => body,
            }
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
