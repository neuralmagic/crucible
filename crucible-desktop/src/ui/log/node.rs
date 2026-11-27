//! The Log pane on one node of a call's target graph: a CI job's log, or a target task's. A
//! GitLab job's sections fold under their headers.

use std::collections::HashMap;

use crucible_contract::console::gitlab::{Item, Line, Section};
use rune::{El, ScrollSpec, TextStyle, Tx, col, icon, row, text};
use rune_kit::{IconButton, IconButtonVariant};
use rune_theme_opencode::icons as oc;

use crate::App;
use crate::live::calls::{CallNode, Jump, NodeLog};
use crate::logline::LogLine;
use crate::settings::LogFont;
use crate::ui::Paint;
use crate::ui::log::{INDENT, SHOWN, ink, line_row};
use crate::ui::tasks::duration;

pub(in crate::ui::log) fn node_log(
    node: &CallNode,
    log: Option<&NodeLog>,
    folds: &HashMap<usize, bool>,
    font: LogFont,
    p: &Paint,
) -> El {
    let style = p.sized(TextStyle::new(11.5, 400.0).lh(17.0).family(font.family()));
    let mut lines = col()
        .key("node-log-lines")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(1.0)
        .px(10.0)
        .py(8.0)
        .scroll(ScrollSpec { stick_bottom: true })
        .scroll_anchor()
        .scroll_edge_fade(8.0);
    let muted = |said: String| {
        text(said)
            .font(p.k.ty.ui)
            .color(p.c.text_muted)
            .py(4.0)
            .wrap()
    };
    match log {
        None => lines = lines.child(muted("Loading".to_string())),
        Some(NodeLog { text: Err(why), .. }) => lines = lines.child(muted(why.clone())),
        Some(NodeLog { text: Ok(body), .. }) if body.trim().is_empty() => {
            lines = lines.child(muted("No log yet.".to_string()))
        }
        Some(NodeLog { trace, .. }) => {
            let mut rows = Vec::new();
            flatten(trace, 0, folds, true, &mut 0, &mut rows);
            let skip = rows.len().saturating_sub(SHOWN);
            lines = lines.children(rows.into_iter().skip(skip).map(|row| draw(row, style, p)));
        }
    }
    col()
        .key("log")
        .w_full()
        .grow(1.0)
        .min_w(0.0)
        .min_h(0.0)
        .gap(8.0)
        .child(head(node, log, p))
        .child(
            p.k.scroll_bar(lines)
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .radius(p.k.radius.md)
                .fill(p.c.layer_01)
                .border(p.hair, p.c.border_muted)
                .clip_rounded(p.k.radius.md),
        )
}

fn head(node: &CallNode, log: Option<&NodeLog>, p: &Paint) -> El {
    let chip = row()
        .key("log-node")
        .role("button")
        .label(format!("Stop showing {}", node.label))
        .h(28.0)
        .pl(8.0)
        .pr(4.0)
        .gap(4.0)
        .center_y()
        .radius(p.k.radius.md)
        .fill(p.c.layer_03)
        .hover(p.c.hover)
        .focusable(true)
        .on_press(|app: &mut App| app.run.call_node = None)
        .child(
            text(format!("{} \u{203a} {}", node.frame, node.label))
                .font(p.mono(11.0, 500.0))
                .color(p.c.text_base)
                .truncate(),
        )
        .child(icon(&oc::XMARK_SMALL, 16.0).color(p.c.icon_muted));
    let open = node.jump.clone().map(|jump| {
        let says = match &jump {
            Jump::Web(_) => "Open on GitHub",
            Jump::Launch { .. } => "Open run",
        };
        IconButton::new(&oc::ARROW_UP_RIGHT)
            .variant(IconButtonVariant::GhostMuted)
            .label(says)
            .help(says)
            .on_press_tx(move |app: &mut App, tx: &mut Tx| app.jump(&jump, tx))
            .build(p.k)
            .key("log-node-open")
    });
    let copy = log
        .filter(|log| log.text.is_ok())
        .map(|log| copy_text(&log.trace))
        .map(|body| {
            IconButton::new(&oc::COPY)
                .variant(IconButtonVariant::GhostMuted)
                .label("Copy log")
                .on_press_tx(move |_: &mut App, tx: &mut Tx| tx.copy(body.clone()))
                .build(p.k)
                .key("log-copy")
        });
    row()
        .w_full()
        .gap(8.0)
        .center_y()
        .child(chip.shrink().min_w(0.0))
        .child(rune::el().grow(1.0))
        .children(open)
        .children(copy)
}

/// A row the pane draws: a section's header, or a line. `at` is where it sits among every row
/// of the log, drawn or folded away.
#[derive(Debug, PartialEq)]
enum Row<'a> {
    Head {
        section: &'a Section,
        open: bool,
        depth: usize,
        at: usize,
    },
    Line {
        line: &'a Line,
        depth: usize,
        at: usize,
    },
}

fn flatten<'a>(
    items: &'a [Item],
    depth: usize,
    folds: &HashMap<usize, bool>,
    shown: bool,
    seen: &mut usize,
    rows: &mut Vec<Row<'a>>,
) {
    for item in items {
        let at = *seen;
        *seen += 1;
        match item {
            Item::Line(line) => {
                if shown {
                    rows.push(Row::Line { line, depth, at });
                }
            }
            Item::Section(section) => {
                let open = folds.get(&at).copied().unwrap_or(!section.collapsed);
                if shown {
                    rows.push(Row::Head {
                        section,
                        open,
                        depth,
                        at,
                    });
                }
                flatten(&section.items, depth + 1, folds, shown && open, seen, rows);
            }
        }
    }
}

fn draw(row: Row<'_>, style: TextStyle, p: &Paint) -> El {
    match row {
        Row::Line { line, depth, at } => line_row(&LogLine::job(line.text.clone()), style, true, p)
            .pl(INDENT * depth as f32)
            .key(format!("node-line-{at}")),
        Row::Head {
            section,
            open,
            depth,
            at,
        } => head_row(section, open, depth, at, style, p),
    }
}

fn head_row(
    section: &Section,
    open: bool,
    depth: usize,
    at: usize,
    style: TextStyle,
    p: &Paint,
) -> El {
    let header = &section.header.text;
    let ink = p.c.text_base;
    rune::row()
        .key(format!("node-line-{at}"))
        .role("button")
        .label(format!(
            "{} {}",
            if open { "Fold" } else { "Open" },
            header.text
        ))
        .w_full()
        .pl(INDENT * depth as f32)
        .pr(4.0)
        .py(1.0)
        .gap(4.0)
        .center_y()
        .radius(p.k.radius.sm)
        .hover(p.c.hover)
        .focusable(true)
        .on_press(move |app: &mut App| {
            app.run.node_folds.insert(at, !open);
        })
        .child(
            icon(
                if open {
                    &oc::CHEVRON_DOWN
                } else {
                    &oc::CHEVRON_RIGHT
                },
                12.0,
            )
            .color(p.c.icon_faint),
        )
        .child(
            text(header.text.clone())
                .font(style)
                .color(ink)
                .spans(ink::spans(&header.runs, ink, p.dark))
                .grow(1.0)
                .min_w(0.0)
                .truncate(),
        )
        .children(section.duration.map(|secs| {
            text(duration(secs as f64))
                .font(p.mono(10.5, 400.0))
                .color(p.c.text_faint)
        }))
}

fn copy_text(items: &[Item]) -> String {
    fn walk(items: &[Item], out: &mut Vec<String>) {
        for item in items {
            match item {
                Item::Line(line) => out.push(line.text.text.clone()),
                Item::Section(section) => {
                    out.push(section.header.text.text.clone());
                    walk(&section.items, out);
                }
            }
        }
    }
    let mut lines = Vec::new();
    walk(items, &mut lines);
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crucible_contract::console::gitlab::parse;

    use crate::ui::log::node::{Row, copy_text, flatten};

    const TRACE: &str = "\u{1b}[0KRunning with gitlab-runner 19.5.1\u{1b}[0;m
section_start:1:step_script\r\u{1b}[0K\u{1b}[0K\u{1b}[36;1mExecuting \"step_script\" stage\u{1b}[0;m
\u{1b}[32;1m$ make\u{1b}[0;m
section_start:2:deps[collapsed=true]\r\u{1b}[0KInstalling deps
pip install -r requirements.txt
section_end:5:deps\r\u{1b}[0K
built
section_end:9:step_script\r\u{1b}[0K
\u{1b}[32;1mJob succeeded\u{1b}[0;m
";

    fn shown(folds: &HashMap<usize, bool>) -> Vec<(String, usize, usize)> {
        let trace = parse(TRACE);
        let mut rows = Vec::new();
        flatten(&trace, 0, folds, true, &mut 0, &mut rows);
        rows.iter()
            .map(|row| match row {
                Row::Head {
                    section, depth, at, ..
                } => (format!("> {}", section.header.text.text), *depth, *at),
                Row::Line { line, depth, at } => (line.text.text.clone(), *depth, *at),
            })
            .collect()
    }

    #[test]
    fn collapsed_sections_start_folded_and_rows_keep_their_place_when_folded() {
        let row = |text: &str, depth, at| (text.to_string(), depth, at);
        assert_eq!(
            shown(&HashMap::new()),
            [
                row("Running with gitlab-runner 19.5.1", 0, 0),
                row("> Executing \"step_script\" stage", 0, 1),
                row("$ make", 1, 2),
                row("> Installing deps", 1, 3),
                row("built", 1, 5),
                row("Job succeeded", 0, 6),
            ]
        );
        let opened = shown(&HashMap::from([(3, true)]));
        assert_eq!(opened[4], row("pip install -r requirements.txt", 2, 4));
        assert_eq!(
            shown(&HashMap::from([(1, false), (3, true)])),
            [
                row("Running with gitlab-runner 19.5.1", 0, 0),
                row("> Executing \"step_script\" stage", 0, 1),
                row("Job succeeded", 0, 6),
            ]
        );
    }

    #[test]
    fn copy_takes_every_line_as_plain_text() {
        assert_eq!(
            copy_text(&parse(TRACE)),
            "Running with gitlab-runner 19.5.1\nExecuting \"step_script\" stage\n$ make\nInstalling deps\npip install -r requirements.txt\nbuilt\nJob succeeded"
        );
        assert_eq!(copy_text(&parse("")), "");
    }
}
