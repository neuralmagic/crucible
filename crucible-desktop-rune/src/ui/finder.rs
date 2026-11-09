//! The run finder: a field over every line of a pack's runs that matches, grouped by launch, each
//! opening its run at the line's task.

use rune::{El, ScrollSpec, Span, Tx, col, el, row, text};
use rune_kit::{SegmentedControl, TextInput};
use rune_theme_opencode::icons as oc;

use crate::App;
use crate::search::Source;
use crate::ui::Paint;

const WIDTH: f32 = 780.0;
const ROW: f32 = 28.0;

pub(crate) fn finder(app: &App, p: &Paint, layer: &mut Vec<El>) {
    let Some(open) = &app.finder else {
        return;
    };
    let query = open.query.text.trim().to_string();
    let input = TextInput::new(&open.query)
        .placeholder("Search runs: tickets, errors, outputs")
        .leading_icon(&oc::MAGNIFYING_GLASS)
        .on_edit(|app: &mut App| match &mut app.finder {
            Some(finder) => &mut finder.query,
            None => &mut app.scratch,
        })
        .on_change(|app: &mut App, _: &mut Tx| app.ask_finder())
        .on_key(crate::commands::finder_key)
        .on_submit(|app: &mut App, _: &mut Tx| app.open_hit(None))
        .build(p.k)
        .key("finder-query")
        .w_full();

    let selected_playbook = match &app.selection {
        Some(crate::Selection::Live { playbook, .. }) => playbook.clone(),
        _ => String::new(),
    };
    let scope = (!selected_playbook.is_empty()).then(|| {
        let labels = [selected_playbook.clone(), "All playbooks".to_string()];
        let all = open.playbook.is_empty();
        let playbook = selected_playbook.clone();
        SegmentedControl::new(&labels, Some(all as usize))
            .label("Search in")
            .fit()
            .on_pick(move |app: &mut App, i: usize, _: &mut Tx| {
                if let Some(finder) = &mut app.finder {
                    finder.playbook = if i == 0 {
                        playbook.clone()
                    } else {
                        String::new()
                    };
                }
                app.ask_finder();
            })
            .build(p.k)
            .key("finder-scope")
    });
    let found = &open.found;
    let status = match (&found.error, found.indexed < found.total) {
        (Some(why), _) => why.clone(),
        (None, true) => format!("Indexing {} of {} runs\u{2026}", found.indexed, found.total),
        (None, false) if found.total > 0 => format!("{} runs indexed", found.total),
        _ => String::new(),
    };
    let head = row()
        .w_full()
        .gap(10.0)
        .center_y()
        .children(scope)
        .child(el().grow(1.0))
        .child(
            text(status)
                .font(p.k.ty.small)
                .color(p.c.text_faint)
                .key("finder-status"),
        );

    let mut list = col()
        .key("finder-list")
        .w_full()
        .max_h(ROW * 18.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_edge_fade(6.0);
    if query.is_empty() {
        list = list.child(note("Type a ticket, an error or a value", p));
    } else if found.hits.is_empty() {
        list = list.child(note("Nothing matches", p));
    }
    let mut last_launch: Option<&str> = None;
    for (i, hit) in found.hits.iter().enumerate() {
        if last_launch != Some(hit.launch.as_str()) {
            last_launch = Some(&hit.launch);
            let count = found
                .matched
                .iter()
                .find(|(launch, _)| *launch == hit.launch)
                .map_or(0, |(_, n)| *n);
            list = list.child(launch_head(app, &hit.launch, count, p));
        }
        list = list.child(hit_row(i, hit, i == open.selected, &query, p));
    }

    let panel = col()
        .key("finder-panel")
        .role("dialog")
        .label("Search runs")
        .w_full()
        .max_w(WIDTH)
        .padding(8.0)
        .gap(8.0)
        .radius(p.k.radius.lg)
        .fill(p.c.layer_01)
        .border(p.hair, p.c.border_muted)
        .modal(true, |app: &mut App| app.finder = None)
        .child(input)
        .child(head)
        .child(list);
    layer.push(
        el().key("finder-scrim")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .fill(p.k.c.scrim)
            .a11y_hidden(),
    );
    layer.push(
        row()
            .key("finder-layer")
            .absolute(0.0, 0.0)
            .w_full()
            .pt(72.0)
            .px(24.0)
            .justify_center()
            .child(panel),
    );
}

fn note(label: &str, p: &Paint) -> El {
    text(label.to_string())
        .font(p.k.ty.ui)
        .color(p.c.text_muted)
        .padding(10.0)
}

/// The launch a group of hits came from: when it landed, how it ended, how many lines matched.
fn launch_head(app: &App, key: &str, count: usize, p: &Paint) -> El {
    let (title, status) = match app.live.launch(key) {
        Some(launch) => (
            format!("{} \u{b7} {}", launch.id, launch.landed_at),
            launch.status.clone(),
        ),
        None => (key.to_string(), String::new()),
    };
    row()
        .key(format!("finder-launch-{key}"))
        .w_full()
        .pt(8.0)
        .pb(2.0)
        .px(10.0)
        .gap(8.0)
        .center_y()
        .child(
            text(title)
                .font(p.k.ty.small_medium)
                .color(p.c.text_base)
                .truncate(),
        )
        .child(text(status).font(p.k.ty.small).color(p.c.text_muted))
        .child(el().grow(1.0))
        .child(
            text(format!("{count} line{}", if count == 1 { "" } else { "s" }))
                .font(p.k.ty.small)
                .color(p.c.text_faint),
        )
}

fn hit_row(i: usize, hit: &crate::search::Hit, on: bool, query: &str, p: &Paint) -> El {
    let source = match hit.source {
        Source::Output => "output",
        Source::Transcript => "session",
        Source::Console => "log",
        Source::Params => "params",
    };
    let spans = matched(&hit.text, query)
        .map(|(start, end)| {
            vec![
                Span::new(start, end)
                    .bg(p.c.warning.bg, 1.0)
                    .color(p.c.text_base),
            ]
        })
        .unwrap_or_default();
    row()
        .key(format!("finder-hit-{i}"))
        .role("option")
        .label(hit.text.clone())
        .checked(on)
        .w_full()
        .h(ROW)
        .px(10.0)
        .gap(10.0)
        .center_y()
        .radius(p.k.radius.md)
        .fill(if on {
            p.c.layer_03
        } else {
            rune::Color::TRANSPARENT
        })
        .hover(p.c.hover)
        .on_press(move |app: &mut App| app.open_hit(Some(i)))
        .child(
            text(source.to_string())
                .font(p.mono(10.5, 500.0))
                .color(if hit.exact {
                    p.c.text_muted
                } else {
                    p.c.text_faint
                })
                .w(52.0),
        )
        .child(
            text(hit.task.clone())
                .font(p.mono(11.0, 500.0))
                .color(p.c.text_muted)
                .w(150.0)
                .truncate(),
        )
        .child(
            text(hit.text.clone())
                .font(p.mono(11.5, 400.0))
                .color(if hit.exact {
                    p.c.text_base
                } else {
                    p.c.text_muted
                })
                .spans(spans)
                .grow(1.0)
                .min_w(0.0)
                .truncate(),
        )
}

/// Where `query` sits in `text`, ignoring ASCII case.
fn matched(text: &str, query: &str) -> Option<(usize, usize)> {
    if query.is_empty() {
        return None;
    }
    let start = text
        .to_ascii_lowercase()
        .find(&query.to_ascii_lowercase())?;
    Some((start, start + query.len()))
}

#[cfg(test)]
mod tests {
    use crate::ui::finder::matched;

    #[test]
    fn the_match_is_found_whatever_its_case() {
        assert_eq!(matched("tickets[0]: XYZ-69", "xyz-69"), Some((12, 18)));
        assert_eq!(matched("nothing", "xyz"), None);
        assert_eq!(matched("x", ""), None);
    }
}
