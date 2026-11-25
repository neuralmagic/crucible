//! The Log pane on one node of a call's target graph: a GitHub job's log, or a target task's.

use rune::{El, ScrollSpec, TextStyle, Tx, col, icon, row, text};
use rune_kit::{IconButton, IconButtonVariant};
use rune_theme_opencode::icons as oc;

use crate::App;
use crate::live::calls::{CallNode, Jump, NodeLog};
use crate::logline::LogLine;
use crate::settings::LogFont;
use crate::ui::Paint;
use crate::ui::log::{SHOWN, line_row};

pub(in crate::ui::log) fn node_log(
    node: &CallNode,
    log: Option<&NodeLog>,
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
    match log.map(|log| &log.text) {
        None => lines = lines.child(muted("Loading".to_string())),
        Some(Err(why)) => lines = lines.child(muted(why.clone())),
        Some(Ok(body)) if body.trim().is_empty() => {
            lines = lines.child(muted("No log yet.".to_string()))
        }
        Some(Ok(body)) => {
            let shown: Vec<&str> = body.lines().collect();
            for (n, raw) in shown
                .iter()
                .enumerate()
                .skip(shown.len().saturating_sub(SHOWN))
            {
                let line = LogLine::parse(raw);
                lines = lines.child(line_row(&line, style, true, p).key(format!("node-line-{n}")));
            }
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

/// `frame › node` with a press that goes back to the run's log, then where the node opens and
/// a copy of its log.
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
        .and_then(|log| log.text.as_ref().ok())
        .cloned()
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
