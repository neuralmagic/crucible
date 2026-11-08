//! The engine's console, one typed row per line.

use rune::{Color, El, ScrollSpec, TextStyle, Tx, col, el, icon, row, text};
use rune_kit::{SegmentedControl, TextInput};
use rune_theme_opencode::icons as oc;

use crate::logline::{Body, Level, LogLine, LogView};
use crate::settings::LogFont;
use crate::ui::tasks::{count, status_color};
use crate::ui::{Paint, mono};
use crate::{App, LogSource};
use crucible_contract::session::TaskStatus;

/// Rows drawn at once; the newest are kept.
const SHOWN: usize = 500;

pub(crate) fn log_pane(
    transcript: &[LogLine],
    console: &[LogLine],
    font: LogFont,
    app: &App,
    p: &Paint,
) -> El {
    let reading = if transcript.is_empty() {
        LogSource::Console
    } else {
        app.log_source
    };
    let log = match reading {
        LogSource::Transcript => transcript,
        LogSource::Console => console,
    };
    let query = app.log_search.text.trim().to_ascii_lowercase();
    let in_scope = |line: &&LogLine| {
        app.log_task.as_deref().is_none_or(|task| line.is_for(task))
            && (query.is_empty() || line.raw.to_ascii_lowercase().contains(&query))
    };
    let scoped: Vec<&LogLine> = log.iter().filter(in_scope).collect();
    let matched: Vec<&LogLine> = scoped
        .iter()
        .copied()
        .filter(|line| app.log_view.keeps(line, app.log_chatter))
        .collect();

    let style = TextStyle::new(11.5, 400.0).lh(17.0).family(font.family());
    let mut lines = col()
        .key("log-lines")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(1.0)
        .px(10.0)
        .py(8.0)
        .scroll(ScrollSpec {
            stick_bottom: app.log_task.is_none() && query.is_empty(),
        })
        .scroll_edge_fade(8.0);
    if matched.is_empty() {
        lines = lines.child(
            text(if log.is_empty() {
                "No console output for this run yet."
            } else {
                "No lines match."
            })
            .font(p.k.ty.ui)
            .color(p.c.text_muted)
            .py(4.0),
        );
    }
    for line in matched.iter().skip(matched.len().saturating_sub(SHOWN)) {
        lines = lines.child(line_row(line, style, app.log_task.is_some(), p));
    }

    col()
        .key("log")
        .w_full()
        .grow(1.0)
        .min_w(0.0)
        .min_h(0.0)
        .gap(8.0)
        .child(
            row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(scope_row(app, p).grow(1.0).min_w(0.0))
                .children((!transcript.is_empty()).then(|| sources(reading, p))),
        )
        .children((!log.is_empty()).then(|| views(&scoped, app, p)))
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

/// Transcript or console, offered only when the run recorded a transcript.
fn sources(reading: LogSource, p: &Paint) -> El {
    const SOURCES: [LogSource; 2] = [LogSource::Transcript, LogSource::Console];
    SegmentedControl::new(
        &["Transcript", "Console"],
        SOURCES.iter().position(|source| *source == reading),
    )
    .label("Log source")
    .width(Some(180.0))
    .on_pick(|app: &mut App, i: usize, _: &mut Tx| app.log_source = SOURCES[i])
    .build(p.k)
    .key("log-sources")
}

fn views(scoped: &[&LogLine], app: &App, p: &Paint) -> El {
    let mut bar = row().w_full().flex_wrap().gap(2.0).center_y();
    for view in LogView::ALL {
        let n = scoped
            .iter()
            .filter(|line| view.keeps(line, app.log_chatter))
            .count();
        let selected = app.log_view == view;
        let ink = if view == LogView::Problems && n > 0 {
            p.c.danger.fg
        } else if selected {
            p.c.text_base
        } else {
            p.c.text_muted
        };
        bar = bar.child(toggle(
            &format!("log-view-{}", view.label().to_lowercase()),
            &format!("{} {n}", view.label()),
            selected,
            ink,
            p,
            move |app: &mut App| app.log_view = view,
        ));
    }
    let chatter = app.log_chatter;
    bar.child(el().grow(1.0)).child(toggle(
        "log-chatter",
        "Library info",
        chatter,
        if chatter {
            p.c.text_base
        } else {
            p.c.text_muted
        },
        p,
        move |app: &mut App| app.log_chatter = !chatter,
    ))
}

fn scope_row(app: &App, p: &Paint) -> El {
    let mut bar = row().w_full().gap(6.0).center_y();
    if let Some(task) = &app.log_task {
        bar = bar.child(
            row()
                .key("log-task")
                .role("button")
                .label(format!("Stop showing only {task}"))
                .h(28.0)
                .pl(8.0)
                .pr(4.0)
                .gap(4.0)
                .center_y()
                .radius(p.k.radius.md)
                .fill(p.c.layer_03)
                .hover(p.c.hover)
                .focusable(true)
                .on_press(|app: &mut App| app.filter_log(None))
                .child(
                    text(task.clone())
                        .font(mono(11.0, 500.0))
                        .color(p.c.text_base),
                )
                .child(icon(&oc::XMARK_SMALL, 16.0).color(p.c.icon_muted)),
        );
    }
    let mut input = TextInput::new(&app.log_search)
        .placeholder("Filter log")
        .leading_icon(&oc::MAGNIFYING_GLASS)
        .on_edit(|app: &mut App| &mut app.log_search);
    if !app.log_search.text.is_empty() {
        input = input.clear_button();
    }
    bar.child(input.build(p.k).key("log-search").grow(1.0).min_w(0.0))
}

fn toggle(
    key: &str,
    label: &str,
    selected: bool,
    ink: Color,
    p: &Paint,
    on_press: impl Fn(&mut App) + 'static,
) -> El {
    el().key(key.to_string())
        .role("button")
        .label(label.to_string())
        .checked(selected)
        .h(22.0)
        .px(8.0)
        .center()
        .radius(p.k.radius.md)
        .fill(if selected {
            p.c.layer_03
        } else {
            Color::TRANSPARENT
        })
        .hover(p.c.hover)
        .press(p.c.pressed)
        .focusable(true)
        .on_press(on_press)
        .child(text(label.to_string()).font(p.k.ty.small_medium).color(ink))
}

fn line_row(line: &LogLine, style: TextStyle, scoped: bool, p: &Paint) -> El {
    let mut lead = row().gap(5.0).center_y();
    if let Some(at) = &line.at {
        lead = lead.child(
            text(at.clone())
                .font(mono(10.5, 400.0))
                .color(p.c.text_faint),
        );
    }
    if let (Some(task), false) = (&line.task, scoped) {
        let name = task.clone();
        lead = lead.child(
            el().role("button")
                .label(format!("Show only {task}"))
                .h(16.0)
                .px(5.0)
                .center()
                .radius(p.k.radius.sm)
                .fill(p.c.layer_03)
                .hover(p.c.hover)
                .on_press(move |app: &mut App| app.filter_log(Some(&name)))
                .child(
                    text(short(task, 18))
                        .font(mono(10.5, 500.0))
                        .color(p.c.text_muted),
                ),
        );
    }
    let (tag, tag_ink, content, ink) = parts(line, p);
    if let Some(tag) = tag {
        lead = lead.child(
            text(tag)
                .font(TextStyle::new(9.5, 600.0).lh(12.0).tracking(0.4))
                .color(tag_ink)
                .w(52.0),
        );
    }
    let content = text(content)
        .font(style)
        .color(ink)
        .grow(1.0)
        .min_w(0.0)
        .selectable(true)
        .selection_color(p.k.selection());
    let content = if matches!(line.body, Body::TaskRow { .. }) {
        content.truncate()
    } else {
        content.wrap()
    };
    row().w_full().gap(6.0).py(1.0).child(lead).child(content)
}

/// The tag, its colour, the text and its colour for one line.
fn parts(line: &LogLine, p: &Paint) -> (Option<&'static str>, Color, String, Color) {
    let dim = p.c.text_muted;
    match &line.body {
        Body::Tool {
            name,
            detail,
            failed,
            subagent,
        } => {
            let tag = if *subagent { "AGENT" } else { "TOOL" };
            let ink = if *failed { p.fail } else { p.accent };
            let verb = if *failed { " failed" } else { "" };
            (
                Some(tag),
                ink,
                format!("{name}{verb}  {detail}"),
                if *failed { p.fail } else { p.c.text_base },
            )
        }
        Body::Thinking {
            chars: Some(chars), ..
        } => (Some("THINK"), dim, format!("{} chars", count(*chars)), dim),
        Body::Thinking { text, .. } => (Some("THINK"), dim, text.clone(), dim),
        Body::Tokens {
            input,
            output,
            cache_read,
            total,
        } => (
            Some("TOKENS"),
            dim,
            format!(
                "{} total \u{b7} in {} \u{b7} out {} \u{b7} cache {}",
                count(*total),
                count(*input),
                count(*output),
                count(*cache_read)
            ),
            dim,
        ),
        Body::Retry(text) => (Some("RETRY"), p.live, text.clone(), p.live),
        Body::Error(text) => (Some("ERROR"), p.fail, text.clone(), p.fail),
        Body::TurnResult { text, failed } => {
            let ink = if *failed { p.fail } else { p.pass };
            (
                Some("RESULT"),
                ink,
                text.clone(),
                if *failed { p.fail } else { p.c.text_base },
            )
        }
        Body::Waiting(text) => (Some("WAIT"), p.live, text.clone(), dim),
        Body::Sandbox(text) => (Some("SANDBOX"), dim, text.clone(), dim),
        Body::Say(text) => (None, dim, text.clone(), p.c.text_base),
        Body::TaskRow {
            name,
            status,
            attempts,
            cost,
            rest,
        } => {
            let ink = status
                .parse::<TaskStatus>()
                .map_or(dim, |status| status_color(status, p));
            let tries = if *attempts > 1 {
                format!(" \u{b7} {attempts} tries")
            } else {
                String::new()
            };
            (
                Some("TASK"),
                ink,
                format!("{name} {status}{tries} \u{b7} {cost}  {rest}"),
                p.c.text_base,
            )
        }
        Body::Trace {
            level,
            target,
            text,
        } => {
            let ink = match level {
                Level::Error => p.fail,
                Level::Warn => p.live,
                _ => dim,
            };
            let content = if target.is_empty() {
                text.clone()
            } else {
                format!("{target}: {text}")
            };
            (
                Some(level.label()),
                ink,
                content,
                if *level <= Level::Warn { ink } else { dim },
            )
        }
        Body::Notice { text, warning } => {
            let ink = if *warning { p.live } else { dim };
            (
                Some("ENGINE"),
                ink,
                text.clone(),
                if *warning { p.live } else { dim },
            )
        }
        Body::Summary(text) => (Some("RUN"), p.accent, text.clone(), p.c.text_base),
        Body::Note(text) => (Some("TURN"), dim, text.clone(), dim),
        Body::Plain(text) => (None, dim, text.clone(), dim),
    }
}

fn short(text: &str, chars: usize) -> String {
    match text.char_indices().nth(chars) {
        Some((cut, _)) => format!("{}\u{2026}", &text[..cut]),
        None => text.to_string(),
    }
}
