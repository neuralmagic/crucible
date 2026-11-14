//! The engine's console, one typed row per line.

use rune::{Color, El, ScrollSpec, TextStyle, Tx, col, el, icon, row, text};
use rune_kit::{IconButton, IconButtonVariant, SegmentedControl, TextInput};
use rune_theme_opencode::icons as oc;

use crate::logline::{self, Body, Level, LogLine, LogView};
use crate::session::{TaskState, TaskView, split_name};
use crate::settings::LogFont;
use crate::ui::Paint;
use crate::ui::tasks::{count, state_mark, status_color};
use crate::unfurl::{self, Block, LogMode};
use crate::{App, LogSource};
use crucible_contract::session::TaskStatus;

/// Rows drawn at once; the newest are kept.
const SHOWN: usize = 500;
/// Lines an open block draws; its newest are kept.
const BLOCK_SHOWN: usize = 300;
/// Lines a running task's folded block shows.
const TAIL: usize = 3;
const INDENT: f32 = 16.0;

pub(crate) fn log_pane(
    transcript: &[LogLine],
    console: &[LogLine],
    tasks: &[TaskView],
    font: LogFont,
    app: &App,
    p: &Paint,
) -> El {
    let Shown {
        reading,
        log,
        scoped,
        matched,
    } = shown(transcript, console, app);
    let query = app.log_search.text.trim();

    let style = p.sized(TextStyle::new(11.5, 400.0).lh(17.0).family(font.family()));
    let mode = app
        .log_mode
        .unwrap_or_else(|| unfurl::default_mode(&matched));
    let mut lines = col()
        .key("log-lines")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(1.0)
        .px(10.0)
        .py(8.0)
        .scroll(ScrollSpec {
            stick_bottom: app.run.log_task.is_none() && query.is_empty() && mode == LogMode::Stream,
        })
        .scroll_anchor()
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
    match mode {
        LogMode::Stream => {
            for line in matched.iter().skip(matched.len().saturating_sub(SHOWN)) {
                let row = line_row(line, style, app.run.log_task.is_some(), p);
                lines = lines.child(keyed(row, log, line));
            }
        }
        LogMode::ByTask => {
            let blocks = Blocks {
                log,
                tasks,
                app,
                style,
                p,
            };
            let plan: Vec<&str> = tasks.iter().map(|task| task.name.as_str()).collect();
            for block in unfurl::blocks(&matched, &plan) {
                lines = lines.children(blocks.rows(&block, 0));
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
        .child(
            row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(scope_row(app, p).grow(1.0).min_w(0.0))
                .child(modes(mode, p))
                .children((!transcript.is_empty()).then(|| sources(reading, p)))
                .child(copy_button(p)),
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

/// The lines the pane reads from, and the ones its task, filter and view keep.
struct Shown<'a> {
    reading: LogSource,
    log: &'a [LogLine],
    scoped: Vec<&'a LogLine>,
    matched: Vec<&'a LogLine>,
}

fn shown<'a>(transcript: &'a [LogLine], console: &'a [LogLine], app: &App) -> Shown<'a> {
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
    let scoped: Vec<&LogLine> = log
        .iter()
        .filter(|line| {
            app.run
                .log_task
                .as_deref()
                .is_none_or(|task| line.is_for(task))
                && (query.is_empty() || line.raw.to_ascii_lowercase().contains(&query))
        })
        .collect();
    let matched = scoped
        .iter()
        .copied()
        .filter(|line| app.log_view.keeps(line, app.log_chatter))
        .collect();
    Shown {
        reading,
        log,
        scoped,
        matched,
    }
}

/// Copies every line the pane's task, filter and view keep, drawn or not.
fn copy_button(p: &Paint) -> El {
    IconButton::new(&oc::COPY)
        .variant(IconButtonVariant::GhostMuted)
        .label("Copy log")
        .on_press_tx(|app: &mut App, tx: &mut Tx| {
            let (transcript, console) = app.log_lines();
            tx.copy(logline::copy_text(shown(transcript, console, app).matched));
        })
        .build(p.k)
        .key("log-copy")
}

/// One stream, or a block per task.
fn modes(mode: LogMode, p: &Paint) -> El {
    const MODES: [LogMode; 2] = [LogMode::Stream, LogMode::ByTask];
    SegmentedControl::new(
        &["Stream", "By task"],
        MODES.iter().position(|m| *m == mode),
    )
    .label("Log layout")
    .width(Some(150.0))
    .on_pick(|app: &mut App, i: usize, _: &mut Tx| app.log_mode = Some(MODES[i]))
    .build(p.k)
    .key("log-modes")
}

/// How a block shows unless opened or folded by hand.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fold {
    Folded,
    /// Folded, with its newest lines under the header.
    Tail,
    Open,
}

/// The pieces every block draws with.
struct Blocks<'a> {
    log: &'a [LogLine],
    tasks: &'a [TaskView],
    app: &'a App,
    style: TextStyle,
    p: &'a Paint,
}

impl Blocks<'_> {
    /// Where the task, instance or round `name` stands; `None` for the lines no task owns.
    fn state(&self, name: &str) -> Option<TaskState> {
        let (task, key) = split_name(name);
        let task = self.tasks.iter().find(|t| t.name == task)?;
        match key {
            None => Some(task.state()),
            Some(key) => task.part_state(key),
        }
    }

    /// A running task shows its tail, a failed one opens, the rest fold.
    fn fold(&self, block: &Block) -> Fold {
        if let Some(open) = self.app.run.log_folds.get(&block.name) {
            return if *open { Fold::Open } else { Fold::Folded };
        }
        match self.state(&block.name) {
            Some(TaskState::Running(_)) => Fold::Tail,
            Some(TaskState::Settled(status))
                if !status.passed()
                    && !matches!(status, TaskStatus::Skipped | TaskStatus::NotTaken) =>
            {
                Fold::Open
            }
            _ => Fold::Folded,
        }
    }

    fn rows(&self, block: &Block, depth: usize) -> Vec<El> {
        let fold = self.fold(block);
        let indent = INDENT * depth as f32;
        let mut rows = vec![self.header(block, fold, indent)];
        let line = |line: &LogLine, scoped: bool| {
            let row = line_row(line, self.style, scoped, self.p).pl(indent + INDENT);
            keyed(row, self.log, line)
        };
        match fold {
            Fold::Folded => {}
            Fold::Tail => rows.extend(block.tail(TAIL).iter().map(|l| line(l, false))),
            Fold::Open => {
                let own = &block.lines;
                rows.extend(
                    own.iter()
                        .skip(own.len().saturating_sub(BLOCK_SHOWN))
                        .map(|l| line(l, true)),
                );
                for instance in &block.instances {
                    rows.extend(self.rows(instance, depth + 1));
                }
            }
        }
        rows
    }

    fn header(&self, block: &Block, fold: Fold, indent: f32) -> El {
        let p = self.p;
        let open = fold == Fold::Open;
        let name = block.name.clone();
        let label = if block.name.is_empty() {
            "engine".to_string()
        } else {
            block.name.clone()
        };
        let problems = block.problems();
        let preview = block
            .all
            .last()
            .map(|line| parts(line, p).2)
            .unwrap_or_default();
        let mut head = row()
            .key(format!("log-block-{}", block.name))
            .role("button")
            .label(format!("{} {label}", if open { "Fold" } else { "Open" }))
            .w_full()
            .h(24.0)
            .pl(indent + 2.0)
            .pr(4.0)
            .gap(7.0)
            .center_y()
            .radius(p.k.radius.sm)
            .hover(p.c.hover)
            .focusable(true)
            .on_press(move |app: &mut App| {
                app.run.log_folds.insert(name.clone(), !open);
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
            );
        if let Some(state) = self.state(&block.name) {
            head = head.child(state_mark(state, p).scale(0.8));
        }
        head = head
            .child(
                text(label)
                    .font(p.mono(11.0, 600.0))
                    .color(p.c.text_base)
                    .max_w(260.0)
                    .truncate(),
            )
            .child(
                text(count(block.all.len() as u64))
                    .font(p.mono(10.5, 400.0))
                    .color(p.c.text_faint),
            );
        if problems > 0 {
            head = head.child(
                text(format!("{problems} problems"))
                    .font(p.mono(10.5, 500.0))
                    .color(p.c.danger.fg),
            );
        }
        head.child(
            text(if fold == Fold::Folded {
                preview
            } else {
                String::new()
            })
            .font(p.mono(10.5, 400.0))
            .color(p.c.text_faint)
            .grow(1.0)
            .min_w(0.0)
            .truncate(),
        )
    }
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
    if let Some(task) = &app.run.log_task {
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
                        .font(p.mono(11.0, 500.0))
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

/// Keys a line's row by where the line sits in `log`, so it stays the same row while the lines
/// drawn around it change.
fn keyed(row: El, log: &[LogLine], line: &LogLine) -> El {
    match log.element_offset(line) {
        Some(at) => row.key(format!("log-line-{at}")),
        None => row,
    }
}

fn line_row(line: &LogLine, style: TextStyle, scoped: bool, p: &Paint) -> El {
    let mut lead = row().gap(5.0).center_y();
    if let Some(at) = &line.at {
        lead = lead.child(
            text(at.clone())
                .font(p.mono(10.5, 400.0))
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
                        .font(p.mono(10.5, 500.0))
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
pub(crate) fn parts(line: &LogLine, p: &Paint) -> (Option<&'static str>, Color, String, Color) {
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
        Body::Output {
            stderr: false,
            text,
        } => (Some("OUT"), dim, text.clone(), p.c.text_base),
        Body::Output { stderr: true, text } => (Some("ERR"), p.live, text.clone(), p.c.text_base),
        Body::Plain(text) => (None, dim, text.clone(), dim),
    }
}

fn short(text: &str, chars: usize) -> String {
    match text.char_indices().nth(chars) {
        Some((cut, _)) => format!("{}\u{2026}", &text[..cut]),
        None => text.to_string(),
    }
}
