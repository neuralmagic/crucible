//! The Pack tab: the workflow pack a run came from, file by file.

use std::ops::Range;

use rune::{Color, El, ScrollSpec, Span, TextStyle, Tx, col, el, icon, row, text};
use rune_kit::syntax::SyntaxTheme;
use rune_kit::{
    Button, ButtonVariant, IconButton, IconButtonVariant, Markdown, SegmentedControl, Spinner,
};
use rune_theme_opencode::icons as oc;

use crate::lsp::{self, Problem, Severity};
use crate::pack::{PackFile, PackFiles};
use crate::settings::LogFont;
use crate::syntax;
use crate::ui::Paint;
use crate::{App, PackSource};

/// Lines drawn per file; a longer file says how much it left out.
const LINES: usize = 4000;
const FILE_LIST: f32 = 190.0;
/// Problems listed under a file before the list scrolls.
const LISTED: usize = 4;

pub(crate) fn pack_view(
    pack: &Result<PackFiles, String>,
    font: LogFont,
    app: &App,
    p: &Paint,
) -> El {
    let working = app.working_pack.as_ref().filter(|_| app.showing_working());
    let pack = match (working, pack) {
        (Some(pack), _) => pack,
        (None, Ok(pack)) if !pack.files.is_empty() => pack,
        (None, Ok(_)) => return note("This pack has no files to show.", "", p),
        (None, Err(error)) => return note("Could not read the pack.", error, p),
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
        let worst = problems(app, pack, &file.path)
            .iter()
            .map(|problem| problem.severity)
            .min();
        list = list.child(file_row(file, selected, worst, p));
    }

    col()
        .key("pack")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(8.0)
        .child(toolbar(app, pack, p))
        .children(app.draft_note.as_ref().map(|note| {
            let (message, color) = match note {
                Ok(message) => (message, p.c.text_muted),
                Err(error) => (error, p.fail),
            };
            text(message.clone())
                .font(p.k.ty.small)
                .color(color)
                .selectable(true)
                .selection_color(p.k.selection())
                .wrap()
                .key("pack-draft-note")
        }))
        .child(
            row()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .gap(10.0)
                .child(list)
                .child(match open {
                    Some(file) => source(file, problems(app, pack, &file.path), font, app, p),
                    None => el().grow(1.0),
                }),
        )
}

/// Where the pack came from, the switch between the run's pack and a working copy, and the
/// draft's pull, save and launch.
fn toolbar(app: &App, pack: &PackFiles, p: &Paint) -> El {
    let mut bar = row().w_full().gap(8.0).center_y().h(28.0);
    let draft = app.draft_target();
    if app.working_pack.is_some() {
        let showing = usize::from(app.showing_working());
        bar = bar.child(
            SegmentedControl::new(&["Run", "Draft"], Some(showing))
                .label("Pack")
                .width(Some(130.0))
                .on_pick(|app: &mut App, i: usize, _: &mut Tx| {
                    app.show_pack(if i == 1 {
                        PackSource::Working
                    } else {
                        PackSource::Run
                    })
                })
                .build(p.k)
                .key("pack-source"),
        );
    }
    bar = bar.child(
        text(pack.origin.clone())
            .font(p.mono(11.0, 400.0))
            .color(p.c.text_faint)
            .selectable(true)
            .selection_color(p.k.selection())
            .grow(1.0)
            .min_w(0.0)
            .truncate(),
    );
    if app.draft_busy {
        return bar.child(Spinner::new().size(14.0).build(p.k));
    }
    let Some((draft, template)) = draft else {
        return bar;
    };
    if !app.showing_working() {
        if app.working_pack.is_none() {
            let label = if template.is_some() { "Draft" } else { "Edit" };
            bar = bar.child(
                Button::new(label)
                    .variant(ButtonVariant::Neutral)
                    .on_press(|app: &mut App| app.pull_draft())
                    .build(p.k)
                    .help(match &template {
                        Some(template) => format!("Copy {template} into draft {draft}"),
                        None => format!("Pull draft {draft} into a local folder"),
                    })
                    .key("pack-draft"),
            );
        }
        return bar;
    }
    let edited = app.working_edited;
    bar.child(
        Button::new("Pull")
            .variant(ButtonVariant::Neutral)
            .disabled(edited)
            .on_press(|app: &mut App| app.pull_draft())
            .build(p.k)
            .help(if edited {
                "Save or undo your edits first".to_string()
            } else {
                format!("Replace with draft {draft}'s newest save")
            })
            .key("pack-pull"),
    )
    .child(
        Button::new("Save")
            .variant(if edited {
                ButtonVariant::Submit
            } else {
                ButtonVariant::Neutral
            })
            .disabled(!edited)
            .on_press(|app: &mut App| app.save_draft())
            .build(p.k)
            .key("pack-save"),
    )
    .child(
        Button::new("Launch")
            .variant(ButtonVariant::Neutral)
            .disabled(edited)
            .on_press(|app: &mut App| app.launch_working())
            .build(p.k)
            .key("pack-launch"),
    )
}

/// What the language server found in `file` of `pack`.
fn problems<'a>(app: &'a App, pack: &PackFiles, file: &str) -> &'a [Problem] {
    pack.path_of(file)
        .and_then(|path| app.checks.problems.get(&path))
        .map_or(&[], Vec::as_slice)
}

fn ink(severity: Severity, p: &Paint) -> Color {
    match severity {
        Severity::Error => p.fail,
        Severity::Warning => p.live,
        Severity::Info | Severity::Hint => p.c.text_muted,
    }
}

/// Each problem's byte range in `text`, at least one character wide so an empty range at a
/// line's end still marks something, in order and without overlap; the worse problem wins
/// where two meet.
pub(crate) fn marks(text: &str, problems: &[Problem]) -> Vec<(Range<usize>, Severity)> {
    let mut out: Vec<(Range<usize>, Severity)> = problems
        .iter()
        .filter(|problem| problem.severity <= Severity::Warning || problem.unnecessary)
        .map(|problem| {
            let start = lsp::offset(text, problem.start);
            let mut end = lsp::offset(text, problem.end).max(start);
            if end == start {
                end = text[start..]
                    .char_indices()
                    .nth(1)
                    .map_or(text.len(), |(at, _)| start + at);
            }
            (start..end, problem.severity)
        })
        .filter(|(range, _)| !range.is_empty())
        .collect();
    out.sort_by_key(|(range, severity)| (*severity, range.start));
    let mut taken: Vec<(Range<usize>, Severity)> = Vec::new();
    for (range, severity) in out {
        let mut pieces = vec![range];
        for (have, _) in &taken {
            pieces = pieces
                .into_iter()
                .flat_map(|piece| {
                    [
                        piece.start..piece.end.min(have.start),
                        piece.start.max(have.end)..piece.end,
                    ]
                })
                .filter(|piece| !piece.is_empty())
                .collect();
        }
        taken.extend(pieces.into_iter().map(|piece| (piece, severity)));
    }
    taken.sort_by_key(|(range, _)| range.start);
    taken
}

/// The marks on `line`, moved to start from the line's first byte. A mark that carries on
/// past the line skips its indentation and trailing space, so a call spanning lines underlines
/// its code rather than its margin.
fn marks_on(
    marks: &[(Range<usize>, Severity)],
    line: &Range<usize>,
    source: &str,
) -> Vec<(Range<usize>, Severity)> {
    let text = &source[line.clone()];
    let code = text.len() - text.trim_start().len()..text.trim_end().len();
    marks
        .iter()
        .filter_map(|(range, severity)| {
            let start = range.start.max(line.start) - line.start;
            let end = range.end.min(line.end).saturating_sub(line.start);
            if start >= end {
                return None;
            }
            let trimmed = start.max(code.start)..end.min(code.end);
            let spans_lines = range.start < line.start || range.end > line.end;
            Some(if spans_lines && !trimmed.is_empty() {
                (trimmed, *severity)
            } else {
                (start..end, *severity)
            })
        })
        .collect()
}

fn file_row(file: &PackFile, selected: bool, worst: Option<Severity>, p: &Paint) -> El {
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
                        .then(|| text(dir).font(p.mono(11.0, 400.0)).color(p.c.text_faint)),
                )
                .child(
                    text(name)
                        .font(p.mono(11.0, if selected { 500.0 } else { 400.0 }))
                        .color(if selected {
                            p.c.text_base
                        } else {
                            p.c.text_muted
                        })
                        .truncate(),
                ),
        )
        .children(
            worst
                .filter(|worst| *worst <= Severity::Warning)
                .map(|worst| {
                    el().size(6.0, 6.0)
                        .min_w(6.0)
                        .radius(3.0)
                        .fill(ink(worst, p))
                }),
        )
}

fn source(file: &PackFile, found_problems: &[Problem], font: LogFont, app: &App, p: &Paint) -> El {
    let Some(text_body) = &file.text else {
        return note(
            &format!("{} is not text, or too big to show.", file.path),
            "",
            p,
        );
    };
    let lines = syntax::lines(text_body);
    let markdown = is_markdown(&file.path);
    let previewing = markdown && !app.pack_md_source;
    let found = syntax::highlighter().highlight(syntax::language(&file.path), text_body);
    let theme = SyntaxTheme::from_kit(p.k);
    let width = lines.len().max(1).to_string().len();
    let style = p.sized(TextStyle::new(11.5, 400.0).lh(18.0).family(font.family()));
    let marks = marks(text_body, found_problems);
    let mut body = col().gap(0.0).py(10.0);
    let shown = if previewing { 0 } else { LINES };
    for (n, line) in lines.iter().take(shown).enumerate() {
        let mut spans = theme.spans(&syntax::on_line(&found, line.clone()), 0);
        let on_line = marks_on(&marks, line, text_body);
        spans.extend(on_line.iter().map(|(range, severity)| {
            Span::new(range.start, range.end).underline(ink(*severity, p))
        }));
        let here: Vec<&Problem> = found_problems
            .iter()
            .filter(|problem| (problem.start.line..=problem.end.line).contains(&(n as u32)))
            .filter(|problem| problem.start.line == n as u32 || !on_line.is_empty())
            .collect();
        let worst = here.iter().map(|problem| problem.severity).min();
        let mut number = row().gap(4.0).center_y().child(
            text(format!("{:>width$}", n + 1))
                .font(style)
                .color(worst.map_or(p.c.text_faint, |worst| ink(worst, p))),
        );
        if !here.is_empty() {
            number = number.help(
                here.iter()
                    .map(|problem| problem.message.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
        body = body.child(
            row().gap(14.0).child(number).child(
                text(text_body[line.clone()].to_string())
                    .font(style)
                    .color(p.c.text_base)
                    .spans(spans)
                    .selectable(true)
                    .selection_color(p.k.selection()),
            ),
        );
    }
    if previewing {
        body = body.child(
            Markdown::new(text_body.clone())
                .selectable(true)
                .build(p.k)
                .key(format!("pack-markdown-{}", file.path)),
        );
    } else if lines.len() > LINES {
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
                        .font(p.mono(11.5, 500.0))
                        .color(p.c.text_base)
                        .selectable(true)
                        .selection_color(p.k.selection()),
                )
                .child(
                    text(format!("{} lines", lines.len()))
                        .font(p.k.ty.small)
                        .color(p.c.text_faint),
                )
                .child(el().grow(1.0))
                .children(markdown.then(|| {
                    SegmentedControl::new(&["Preview", "Source"], Some(app.pack_md_source as usize))
                        .label("Show")
                        .fit()
                        .on_pick(|app: &mut App, i: usize, _: &mut Tx| app.pack_md_source = i == 1)
                        .build(p.k)
                        .key("pack-md-mode")
                }))
                .children(git_link(app, &file.path, found_problems, p))
                .children(copy_path(app, &file.path, p))
                .child(open_button(&file.path, found_problems, p)),
        )
        .children(
            app.editor_error
                .as_ref()
                .map(|error| text(error.clone()).font(p.k.ty.small).color(p.fail).wrap()),
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
        .children(problem_list(&file.path, found_problems, p))
}

fn is_markdown(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".markdown")
}

fn severity_icon(severity: Severity, p: &Paint) -> El {
    let glyph = match severity {
        Severity::Error => &oc::CIRCLE_X,
        Severity::Warning => &oc::WARNING,
        Severity::Info | Severity::Hint => &oc::INFO,
    };
    icon(glyph, 13.0).min_w(13.0).color(ink(severity, p))
}

/// The file's errors and warnings, each opening the editor at its line.
fn problem_list(file: &str, found: &[Problem], p: &Paint) -> Option<El> {
    let shown: Vec<&Problem> = found
        .iter()
        .filter(|problem| problem.severity <= Severity::Warning)
        .collect();
    if shown.is_empty() {
        return None;
    }
    let mut list = col()
        .key("pack-problems")
        .w_full()
        .max_h(LISTED as f32 * 26.0 + 8.0)
        .padding(4.0)
        .gap(0.0)
        .radius(p.k.radius.lg)
        .fill(p.c.layer_01)
        .border(p.hair, p.c.border_muted)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_edge_fade(6.0);
    for (i, problem) in shown.into_iter().enumerate() {
        let line = problem.start.line as usize + 1;
        let path = file.to_string();
        list = list.child(
            row()
                .key(format!("pack-problem-{i}"))
                .role("button")
                .label(format!("Open line {line} in the editor"))
                .w_full()
                .h(26.0)
                .px(8.0)
                .gap(8.0)
                .center_y()
                .radius(p.k.radius.md)
                .hover(p.c.hover)
                .focusable(true)
                .on_press(move |app: &mut App| app.open_in_editor(&path, line))
                .child(severity_icon(problem.severity, p))
                .child(
                    text(format!("{line}"))
                        .font(p.mono(11.0, 500.0))
                        .color(p.c.text_faint)
                        .min_w(28.0),
                )
                .child(
                    text(problem.message.clone())
                        .font(p.k.ty.small)
                        .color(p.c.text_base)
                        .grow(1.0)
                        .min_w(0.0)
                        .truncate(),
                )
                .help(problem.message.clone()),
        );
    }
    Some(list)
}

/// Opens the file in the user's editor, at its first error when it has one.
/// The first line with an error or warning, else the first.
fn first_line(found: &[Problem]) -> usize {
    found
        .iter()
        .filter(|problem| problem.severity <= Severity::Warning)
        .map(|problem| problem.start.line as usize + 1)
        .min()
        .unwrap_or(1)
}

/// Opens the file on the Git host the pack is pinned to.
fn git_link(app: &App, file: &str, found: &[Problem], p: &Paint) -> Option<El> {
    let line = first_line(found);
    let url = app.shown_pack()?.web_url(file, line)?;
    let path = file.to_string();
    Some(
        IconButton::new(&oc::BRANCH)
            .variant(IconButtonVariant::GhostMuted)
            .label("Open on Git")
            .help(url)
            .on_press(move |app: &mut App| app.open_on_git(&path, line))
            .build(p.k)
            .key("pack-git"),
    )
}

fn open_button(file: &str, found: &[Problem], p: &Paint) -> El {
    let line = first_line(found);
    let path = file.to_string();
    IconButton::new(&oc::EDIT)
        .variant(IconButtonVariant::GhostMuted)
        .label("Open in editor")
        .help("Open in editor")
        .on_press(move |app: &mut App| app.open_in_editor(&path, line))
        .build(p.k)
        .key("pack-open")
}

/// Copies the file's path on disk, to hand to an agent.
fn copy_path(app: &App, file: &str, p: &Paint) -> Option<El> {
    let path = app.shown_pack()?.path_of(file)?.display().to_string();
    Some(
        IconButton::new(&oc::COPY)
            .variant(IconButtonVariant::GhostMuted)
            .label("Copy path")
            .help(path.clone())
            .on_press_tx(move |_: &mut App, tx: &mut Tx| tx.copy(path.clone()))
            .build(p.k)
            .key("pack-copy-path"),
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
                .font(p.mono(11.0, 400.0))
                .color(p.c.text_muted)
                .selectable(true)
                .selection_color(p.k.selection())
                .wrap(),
        );
    }
    card
}

#[cfg(test)]
mod tests {
    use crate::lsp::{Pos, Problem, Severity};
    use crate::ui::pack::{marks, marks_on};

    fn problem(start: (u32, u32), end: (u32, u32), severity: Severity) -> Problem {
        Problem {
            start: Pos {
                line: start.0,
                character: start.1,
            },
            end: Pos {
                line: end.0,
                character: end.1,
            },
            severity,
            message: String::new(),
            unnecessary: false,
        }
    }

    #[test]
    fn marks_cover_problems_and_the_worse_one_wins_an_overlap() {
        let text = "abc = f(x\ny = 2\n";
        let found = marks(
            text,
            &[
                problem((0, 0), (0, 5), Severity::Warning),
                problem((0, 3), (1, 1), Severity::Error),
                problem((1, 5), (1, 5), Severity::Warning),
                problem((0, 0), (1, 0), Severity::Hint),
            ],
        );
        assert_eq!(
            found,
            vec![
                (0..3, Severity::Warning),
                (3..11, Severity::Error),
                (15..16, Severity::Warning),
            ]
        );
        let lines = crate::syntax::lines(text);
        assert_eq!(
            marks_on(&found, &lines[0], text),
            vec![(0..3, Severity::Warning), (3..9, Severity::Error)]
        );
        assert_eq!(
            marks_on(&found, &lines[1], text),
            vec![(0..1, Severity::Error)]
        );
        assert!(marks("", &[problem((0, 0), (0, 0), Severity::Error)]).is_empty());
    }

    #[test]
    fn a_mark_across_lines_skips_each_lines_margin() {
        let text = "f(\n    a = 1,  \n)";
        let found = marks(text, &[problem((0, 0), (2, 1), Severity::Error)]);
        let lines = crate::syntax::lines(text);
        assert_eq!(
            marks_on(&found, &lines[1], text),
            vec![(4..10, Severity::Error)]
        );
        assert_eq!(
            marks_on(&found, &lines[2], text),
            vec![(0..1, Severity::Error)]
        );
        let blank = "f(\n   \n)";
        let found = marks(blank, &[problem((0, 0), (2, 1), Severity::Error)]);
        let lines = crate::syntax::lines(blank);
        assert_eq!(
            marks_on(&found, &lines[1], blank),
            vec![(0..3, Severity::Error)]
        );
        let alone = "x = 1   ";
        let found = marks(alone, &[problem((0, 6), (0, 7), Severity::Warning)]);
        assert_eq!(
            marks_on(&found, &crate::syntax::lines(alone)[0], alone),
            vec![(6..7, Severity::Warning)]
        );
    }

    #[test]
    fn an_unnecessary_hint_is_marked() {
        let mut hint = problem((0, 0), (0, 1), Severity::Hint);
        hint.unnecessary = true;
        assert_eq!(marks("x", &[hint]), vec![(0..1, Severity::Hint)]);
    }
}
