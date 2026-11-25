//! The runs page: every launch of one playbook on a time axis and a calendar, and as a list.
//! Picking one opens its run.

use std::collections::HashSet;

use rune::{Color, El, ScrollSpec, col, el, icon, row, text};
use rune_theme_opencode::icons as oc;

use crate::live::{LivePlaybook, LiveView};
use crate::model::Verdict;
use crate::runs::{
    DAY, Day, Summary, axis, calendar, date, day_label, lanes, month_labels, ticks, wall,
    weeks_shown,
};
use crate::ui::Flight;
use crate::ui::desk;
use crate::ui::detail::{runs_note, stat};
use crate::ui::sidebar::{age, verdict_color, verdict_icon};
use crate::ui::{Paint, SECTION, TITLE, meter, pulsing};
use crate::{App, RunsTab, Selection};
use jiff::tz::TimeZone;
use rune_kit::Spinner;

const DOT: f32 = 12.0;
const LANE: f32 = 18.0;
const CELL: f32 = 14.0;
const CELL_GAP: f32 = 3.0;
const WEEKS: (usize, usize) = (5, 12);
/// How far along the axis a mark can sit and still have room for its label.
const LAST_LABEL: f32 = 0.94;

pub(crate) fn runs_page(
    launch: Option<El>,
    shares: Option<El>,
    playbook: &str,
    launches: &[&LivePlaybook],
    app: &App,
    view: Option<(RunsTab, Option<El>)>,
    p: &Paint,
) -> El {
    let (live, unfolded) = (&*app.remote.live, &app.runs_unfolded);
    let now = match live {
        LiveView::Ready { now, .. } => *now,
        _ => 0,
    };
    let summary = Summary::of(
        &launches
            .iter()
            .map(|launch| (launch.verdict(), launch.cost))
            .collect::<Vec<_>>(),
    );
    let newest = launches
        .iter()
        .map(|launch| launch.landed)
        .max()
        .unwrap_or(now);
    let description = launches
        .iter()
        .find_map(|launch| launch.description.clone())
        .unwrap_or_default();
    let zone = TimeZone::system();

    col()
        .key("runs-page")
        .grow(1.0)
        .h_full()
        .min_h(0.0)
        .min_w(0.0)
        .px(28.0)
        .pb(20.0)
        .gap(16.0)
        .child(
            col()
                .w_full()
                .child(
                    row()
                        .w_full()
                        .h(52.0)
                        .center_y()
                        .window_drag()
                        .child(
                            text(playbook.to_string())
                                .font(p.sized(TITLE))
                                .color(p.c.text_base)
                                .matched(crate::ui::title_flight(playbook)),
                        )
                        .child(el().grow(1.0))
                        .children(launch),
                )
                .children((!description.is_empty()).then(|| {
                    text(description)
                        .font(p.k.ty.ui)
                        .color(p.c.text_muted)
                        .max_lines(2)
                        .wrap()
                })),
        )
        .children(view.as_ref().map(|(tab, _)| tabs(*tab, p)))
        .children(match view {
            Some((RunsTab::Report, Some(drawn))) => vec![
                col()
                    .key("runs-report")
                    .w_full()
                    .grow(1.0)
                    .min_h(0.0)
                    .child(drawn),
            ],
            _ => vec![
                stats(&summary, newest, now, p),
                card("Timeline", timeline(launches, now, &zone, p), p),
                row()
                    .w_full()
                    .grow(1.0)
                    .min_h(0.0)
                    .gap(16.0)
                    .child(
                        col()
                            .gap(16.0)
                            .h_full()
                            .min_h(0.0)
                            .child(card("Calendar", heatmap(launches, now, &zone, p), p))
                            .children(shares.map(|shares| card("Shares", shares, p))),
                    )
                    .child(
                        card("All runs", list(launches, live, unfolded, now, p), p)
                            .grow(1.0)
                            .min_w(0.0)
                            .min_h(0.0),
                    ),
            ],
        })
}

/// The runs page's tabs: the launches, and the pack's view of them.
fn tabs(shown: RunsTab, p: &Paint) -> El {
    let options = [("runs", "Runs"), ("report", "Report")]
        .map(|(value, label)| (value.to_string(), label.to_string()));
    let picked = match shown {
        RunsTab::Runs => "runs",
        RunsTab::Report => "report",
    };
    desk::pills(
        "runs-tabs",
        &options,
        Some(picked),
        |app: &mut App, value: &str| {
            app.runs_tab = match value {
                "report" => RunsTab::Report,
                _ => RunsTab::Runs,
            };
        },
        p,
    )
}

fn stats(summary: &Summary, newest: i64, now: i64, p: &Paint) -> El {
    let rate = summary.pass_rate();
    row()
        .key("runs-stats")
        .w_full()
        .gap(10.0)
        .child(stat(
            "runs",
            "Runs",
            &summary.runs.to_string(),
            p.c.text_base,
            None,
            "",
            p,
        ))
        .child(stat(
            "pass-rate",
            "Pass rate",
            &rate.map_or("—".to_string(), |r| format!("{:.0}%", r * 100.0)),
            match rate {
                Some(r) if r < 0.5 => p.fail,
                Some(_) => p.pass,
                None => p.c.text_muted,
            },
            Some(meter(
                "pass-track",
                &[
                    (rate.unwrap_or(0.0), p.pass),
                    (rate.map_or(0.0, |r| 1.0 - r), p.fail),
                ],
                p,
            )),
            &if summary.failed > 0 {
                format!("{} failed", summary.failed)
            } else {
                String::new()
            },
            p,
        ))
        .child(stat(
            "spent",
            "Spend",
            &summary
                .spent
                .map_or("\u{2014}".to_string(), |spent| format!("${spent:.2}")),
            if summary.spent.is_some() {
                p.c.text_base
            } else {
                p.c.text_muted
            },
            None,
            "",
            p,
        ))
        .child(stat(
            "last",
            "Last run",
            &age(newest, now),
            if summary.running > 0 {
                p.live
            } else {
                p.c.text_base
            },
            None,
            &if summary.running > 0 {
                format!("{} running", summary.running)
            } else {
                String::new()
            },
            p,
        ))
}

fn card(title: &str, body: El, p: &Paint) -> El {
    col()
        .key(format!(
            "runs-{}",
            title.to_ascii_lowercase().replace(' ', "-")
        ))
        .gap(10.0)
        .padding(14.0)
        .radius(p.k.radius.lg)
        .fill(p.c.layer_01)
        .border(p.hair, p.c.border_muted)
        .child(
            text(title.to_string())
                .font(p.sized(SECTION))
                .color(p.c.text_base),
        )
        .child(body)
}

/// Opens `launch`, its run view flying out of `from`, the matched id of what was clicked.
fn open(launch: &LivePlaybook, from: Flight) -> impl Fn(&mut App) + 'static {
    let pin = Selection::launch(launch);
    let id = from.id(&launch.key);
    move |app: &mut App| {
        app.flight = Some(id.clone());
        app.select(pin.clone())
    }
}

fn dot(launch: &LivePlaybook, p: &Paint) -> El {
    let verdict = launch.verdict();
    let ink = verdict_color(verdict, p);
    let mark = el()
        .size(DOT, DOT)
        .radius(DOT / 2.0)
        .fill(ink)
        .border(p.hair, p.c.bg_base);
    let mark = if verdict == Verdict::Running {
        pulsing(p, mark, p.live, DOT)
    } else {
        mark
    };
    row()
        .key(format!("dot-{}", launch.key))
        .role("button")
        .label(format!("{} \u{2014} {}", launch.landed_at, launch.status))
        .size(DOT + 6.0, DOT + 6.0)
        .center()
        .radius(f32::midpoint(DOT, 6.0))
        .hover(p.c.hover)
        .focusable(true)
        .help(format!("{} \u{b7} {}", launch.landed_at, launch.status))
        .matched(Flight::Dot.id(&launch.key))
        .on_press(open(launch, Flight::Dot))
        .child(mark)
}

/// Each launch as a dot where it landed, from the first launch to the last, in lanes so none
/// overlap.
fn timeline(launches: &[&LivePlaybook], now: i64, zone: &TimeZone, p: &Paint) -> El {
    let landed: Vec<i64> = launches.iter().map(|l| l.landed).collect();
    let running = launches.iter().any(|l| l.status.is_running());
    let (start, end) = axis(&landed, running, now);
    let span = (end - start) as f32;
    let xs: Vec<f32> = launches
        .iter()
        .map(|l| ((l.landed - start) as f32 / span).clamp(0.0, 1.0))
        .collect();
    let lane_of = lanes(&xs, 0.018);
    let depth = lane_of.iter().max().map_or(1, |l| l + 1);

    let mut track = el().key("runs-track").w_full().h(depth as f32 * LANE + 8.0);
    for (i, launch) in launches.iter().enumerate() {
        track = track.child(
            row()
                .absolute(0.0, 4.0 + lane_of[i] as f32 * LANE - 3.0)
                .w_full()
                .child(el().w_frac(xs[i]))
                .child(dot(launch, p).offset(-(DOT + 6.0) / 2.0, 0.0)),
        );
    }

    let mut axis = el().key("runs-axis").w_full().h(16.0);
    for tick in ticks(start, end, zone) {
        let x = ((tick.at - start) as f32 / span).clamp(0.0, 1.0);
        axis = axis.child(
            row()
                .absolute(0.0, 0.0)
                .w_full()
                .h_full()
                .center_y()
                .child(el().w_frac(x))
                .child(el().w(1.0).h(6.0).fill(p.c.border_strong))
                .children((x < LAST_LABEL).then(|| {
                    text(tick.label)
                        .font(p.k.ty.small)
                        .color(p.c.text_faint)
                        .pl(4.0)
                })),
        );
    }

    col()
        .w_full()
        .gap(6.0)
        .child(
            col()
                .w_full()
                .radius(p.k.radius.md)
                .fill(p.c.bg_base)
                .px(12.0)
                .py(6.0)
                .child(track),
        )
        .child(row().w_full().px(12.0).child(axis))
}

/// The weeks back to the first launch, Monday at the top, each day coloured by how its launches
/// ended.
fn heatmap(launches: &[&LivePlaybook], now: i64, zone: &TimeZone, p: &Paint) -> El {
    let landed: Vec<(i64, &LivePlaybook)> = launches
        .iter()
        .map(|l| (wall(l.landed, zone), *l))
        .collect();
    let now = wall(now, zone);
    let first = landed.iter().map(|(at, _)| *at).min().unwrap_or(now);
    let days = calendar(
        &landed
            .iter()
            .map(|(at, l)| (*at, l.verdict()))
            .collect::<Vec<_>>(),
        now,
        weeks_shown(first, now, WEEKS.0, WEEKS.1),
    );
    let busiest = days.iter().map(|d| d.runs).max().unwrap_or(1).max(1);
    let today = now.div_euclid(DAY);

    let mut grid = row().key("calendar").gap(CELL_GAP);
    let labels = col().gap(CELL_GAP).pr(4.0).pt(16.0 + CELL_GAP).children(
        ["Mon", "", "Wed", "", "Fri", "", ""]
            .map(|day| text(day).font(p.k.ty.small).color(p.c.text_faint).h(CELL)),
    );
    grid = grid.child(labels);
    let labelled = month_labels(&days, 2);
    for (week, labelled) in days.chunks(7).zip(labelled) {
        let month = labelled.then(|| date(week[6].day).strftime("%b").to_string());
        let mut column = col().gap(CELL_GAP).child(
            text(month.unwrap_or_default())
                .font(p.k.ty.small)
                .color(p.c.text_faint)
                .h(16.0)
                .w(CELL),
        );
        for d in week {
            column = column.child(cell(d, busiest, today, &landed, p));
        }
        grid = grid.child(column);
    }
    grid
}

fn cell(d: &Day, busiest: usize, today: i64, landed: &[(i64, &LivePlaybook)], p: &Paint) -> El {
    let future = d.day > today;
    let fill = if d.runs == 0 {
        if future {
            Color::TRANSPARENT
        } else {
            p.c.layer_03
        }
    } else {
        let base = match d.fail_rate() {
            Some(failed) => p.pass.mix(p.fail, failed),
            None if d.running > 0 => p.live,
            None => p.c.text_faint,
        };
        base.alpha(0.35 + 0.65 * d.runs as f32 / busiest as f32)
    };
    let mut c = el()
        .key(format!("day-{}", d.day))
        .size(CELL, CELL)
        .radius(3.0)
        .fill(fill);
    if d.day == today {
        c = c.border(1.0, p.c.text_base.alpha(0.6));
    }
    if d.runs == 0 {
        return c;
    }
    let newest = landed
        .iter()
        .filter(|(at, _)| at.div_euclid(DAY) == d.day)
        .max_by_key(|(at, _)| *at)
        .map(|(_, launch)| *launch);
    let mut summary = format!("{} \u{b7} {}", day_label(d.day), runs_note(d.runs as i64));
    if d.failed > 0 {
        summary.push_str(&format!(" \u{b7} {} failed", d.failed));
    }
    match newest {
        Some(launch) => c
            .role("button")
            .label(summary.clone())
            .help(summary)
            .hover(p.c.hover)
            .focusable(true)
            .matched(Flight::Cell.id(&launch.key))
            .on_press(open(launch, Flight::Cell)),
        None => c,
    }
}

/// One row of the runs list: a launch, how deep under the launch whose call dispatched it, how
/// many launches its own calls dispatched, and whether those show.
#[derive(Clone, Copy)]
struct Nested<'a> {
    launch: &'a LivePlaybook,
    depth: usize,
    calls: usize,
    open: bool,
}

/// `launches` newest first, each followed by the launches its calls dispatched while it is in
/// `unfolded`. A launch whose caller is listed shows only under it.
fn nested<'a>(
    launches: &[&'a LivePlaybook],
    live: &'a LiveView,
    unfolded: &HashSet<String>,
) -> Vec<Nested<'a>> {
    let listed: HashSet<&str> = launches.iter().map(|l| l.key.as_str()).collect();
    let mut tops: Vec<&'a LivePlaybook> = launches
        .iter()
        .copied()
        .filter(|l| {
            l.called_by
                .as_ref()
                .is_none_or(|by| !listed.contains(by.launch.as_str()))
        })
        .collect();
    tops.sort_by_key(|l| std::cmp::Reverse(l.landed));
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    let mut stack: Vec<(&'a LivePlaybook, usize)> =
        tops.into_iter().rev().map(|launch| (launch, 0)).collect();
    while let Some((launch, depth)) = stack.pop() {
        if !seen.insert(launch.key.as_str()) {
            continue;
        }
        let called = live.called_from(&launch.key);
        let open = unfolded.contains(&launch.key) && depth < MAX_DEPTH;
        rows.push(Nested {
            launch,
            depth,
            calls: called.len(),
            open,
        });
        if open {
            stack.extend(called.into_iter().rev().map(|child| (child, depth + 1)));
        }
    }
    rows
}

/// The deepest a dispatched launch nests.
const MAX_DEPTH: usize = crucible_contract::calls::MAX_CALL_DEPTH as usize;

/// Every launch, newest first, under its column heads.
fn list(
    launches: &[&LivePlaybook],
    live: &LiveView,
    unfolded: &HashSet<String>,
    now: i64,
    p: &Paint,
) -> El {
    let columns =
        rune::memo::layout_with("runs-list", |rect| Columns::fit(rect[2])).unwrap_or(Columns::All);
    let mut rows = col()
        .key("runs-list")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_edge_fade(8.0);
    for (i, row) in nested(launches, live, unfolded).into_iter().enumerate() {
        if i > 0 && row.depth == 0 {
            rows = rows.child(el().h(p.hair).w_full().fill(p.c.border_muted));
        }
        rows = rows.child(list_row(row, now, columns, p));
    }
    col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(heads(columns, p))
        .child(el().h(p.hair).w_full().fill(p.c.border_muted))
        .child(p.k.scroll_bar(rows).w_full().grow(1.0).min_h(0.0))
}

/// Which of a run row's columns fit the list's width; origin goes first, then the result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Columns {
    Fewest,
    NoOrigin,
    All,
}

impl Columns {
    /// Every column needs about 440 points; without the origin, 360.
    fn fit(width: f32) -> Columns {
        match width {
            w if w >= 440.0 => Columns::All,
            w if w >= 360.0 => Columns::NoOrigin,
            _ => Columns::Fewest,
        }
    }
}

const LANDED: f32 = 110.0;
const ORIGIN: f32 = 84.0;
const COST: f32 = 64.0;
const AGE: f32 = 36.0;

fn heads(columns: Columns, p: &Paint) -> El {
    let head = |label: &'static str| text(label).font(p.k.ty.small).color(p.c.text_faint);
    row()
        .key("runs-heads")
        .w_full()
        .h(24.0)
        .px(6.0)
        .gap(10.0)
        .center_y()
        .child(el().w(16.0))
        .child(head("Landed").w(LANDED))
        .child(
            head(if columns >= Columns::NoOrigin {
                "Result"
            } else {
                ""
            })
            .grow(1.0)
            .min_w(0.0),
        )
        .children((columns == Columns::All).then(|| head("Origin").w(ORIGIN)))
        .child(head("Cost").w(COST))
        .child(head("Age").w(AGE))
}

/// What the Result column says: why a launch failed when the controller gave a reason, else its
/// status.
fn result(launch: &LivePlaybook) -> String {
    launch
        .why
        .clone()
        .unwrap_or_else(|| launch.status.to_string())
}

/// One row of the list, keyed by position so a script can pick "run-0"; a launch a call
/// dispatched sits under its caller's row as `run-<i>-<j>`, named `called` in place of its time.
/// How far each nesting level indents.
const INDENT: f32 = 16.0;

/// The count of launches a launch's calls dispatched; a press shows or folds them.
fn calls_badge(launch: &LivePlaybook, calls: usize, open: bool, p: &Paint) -> El {
    let key = launch.key.clone();
    row()
        .key(format!("run-calls-{}", launch.key))
        .role("button")
        .label(if open { "Fold calls" } else { "Show calls" })
        .checked(open)
        .h(20.0)
        .px(5.0)
        .gap(2.0)
        .center_y()
        .radius(p.k.radius.sm)
        .fill(p.c.layer_03)
        .hover(p.c.hover)
        .on_press(move |app: &mut App| {
            if !app.runs_unfolded.remove(&key) {
                app.runs_unfolded.insert(key.clone());
            }
        })
        .child(
            icon(
                if open {
                    &oc::CHEVRON_DOWN
                } else {
                    &oc::CHEVRON_RIGHT
                },
                10.0,
            )
            .color(p.c.icon_muted),
        )
        .child(
            text(calls.to_string())
                .font(p.mono(10.5, 500.0).features("'tnum' 1"))
                .color(p.c.text_muted),
        )
}

fn list_row(nest: Nested, now: i64, columns: Columns, p: &Paint) -> El {
    let launch = nest.launch;
    let verdict = launch.verdict();
    let glyph = match verdict_icon(verdict) {
        _ if verdict == Verdict::Running => Spinner::new().size(14.0).color(p.live).build(p.k),
        Some(glyph) => icon(glyph, 14.0).color(verdict_color(verdict, p)),
        None => el()
            .size(10.0, 10.0)
            .radius(5.0)
            .border(1.0, p.c.text_faint),
    };
    let glyph = if verdict == Verdict::Running {
        pulsing(p, glyph, p.live, 14.0)
    } else {
        glyph
    };
    let result = result(launch);
    let landed = match nest.depth {
        0 => launch.landed_at.clone(),
        _ => format!("\u{21b3} {}", launch.id),
    };
    let badge = (nest.calls > 0).then(|| calls_badge(launch, nest.calls, nest.open, p));
    row()
        .key(format!("run-{}", launch.key))
        .role("button")
        .label(format!("{} \u{2014} {}", launch.landed_at, launch.status))
        .w_full()
        .h(36.0)
        .px(6.0)
        .pl(6.0 + INDENT * nest.depth as f32)
        .gap(10.0)
        .center_y()
        .radius(p.k.radius.md)
        .hover(p.c.hover)
        .press(p.c.pressed)
        .focusable(true)
        .matched(Flight::Row.id(&launch.key))
        .on_press(open(launch, Flight::Row))
        .child(row().size(16.0, 16.0).center().child(glyph))
        .child(
            row()
                .w(LANDED)
                .gap(4.0)
                .center_y()
                .child(
                    text(landed)
                        .font(p.k.ty.ui_medium)
                        .color(p.c.text_base)
                        .shrink()
                        .min_w(0.0)
                        .truncate(),
                )
                .children(badge),
        )
        .child(
            text(if columns >= Columns::NoOrigin {
                result.clone()
            } else {
                String::new()
            })
            .font(p.k.ty.ui)
            .color(p.c.text_muted)
            .grow(1.0)
            .min_w(0.0)
            .truncate()
            .help_when_truncated(result),
        )
        .children((columns == Columns::All).then(|| {
            text(if launch.origin.is_empty() {
                "launch".to_string()
            } else {
                launch.origin.clone()
            })
            .font(p.k.ty.ui)
            .color(p.c.text_muted)
            .w(ORIGIN)
            .truncate()
        }))
        .child(
            text(
                launch
                    .cost
                    .map_or("\u{2014}".to_string(), |c| format!("${c:.2}")),
            )
            .font(p.mono(12.0, 400.0).features("'tnum' 1"))
            .color(p.c.text_muted)
            .w(COST),
        )
        .child(
            text(age(launch.landed, now))
                .font(p.k.ty.small)
                .color(p.c.text_faint)
                .w(AGE),
        )
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::live::{LaunchStatus, LivePlaybook, LiveView};
    use crate::ui::runs::{Columns, nested, result};

    fn launch(key: &str, landed: i64, by: Option<&str>) -> LivePlaybook {
        LivePlaybook {
            key: key.into(),
            landed,
            called_by: by.map(|launch| crux::dto::CalledBy {
                run: format!("run-{launch}"),
                launch: launch.into(),
                task: "call".into(),
                instance: None,
            }),
            ..LivePlaybook::default()
        }
    }

    fn ready(launches: Vec<LivePlaybook>) -> LiveView {
        LiveView::Ready {
            endpoint: String::new(),
            identity: Default::default(),
            now: 0,
            launches,
            drafts: Vec::new(),
            stale: None,
        }
    }

    #[test]
    fn a_dispatched_run_nests_folded_under_its_caller_with_a_count() {
        let live = ready(vec![
            launch("p:2", 20, None),
            launch("c:1", 12, Some("p:1")),
            launch("c:2", 11, Some("p:1")),
            launch("g:1", 13, Some("c:1")),
            launch("p:1", 10, None),
            launch("orphan", 5, Some("gone")),
        ]);
        let listed: Vec<&LivePlaybook> = live.launches().iter().collect();
        let rows = |open: &[&str]| -> Vec<(String, usize, usize, bool)> {
            let open: HashSet<String> = open.iter().map(ToString::to_string).collect();
            nested(&listed, &live, &open)
                .into_iter()
                .map(|row| (row.launch.key.clone(), row.depth, row.calls, row.open))
                .collect()
        };
        assert_eq!(
            rows(&[]),
            [
                ("p:2".into(), 0, 0, false),
                ("p:1".into(), 0, 2, false),
                ("orphan".into(), 0, 0, false),
            ]
        );
        assert_eq!(
            rows(&["p:1", "c:1"]),
            [
                ("p:2".into(), 0, 0, false),
                ("p:1".into(), 0, 2, true),
                ("c:2".into(), 1, 0, false),
                ("c:1".into(), 1, 1, true),
                ("g:1".into(), 2, 0, false),
                ("orphan".into(), 0, 0, false),
            ]
        );
        let alone: Vec<&LivePlaybook> = listed.iter().copied().filter(|l| l.key == "c:1").collect();
        let open = HashSet::new();
        let shown: Vec<String> = nested(&alone, &live, &open)
            .into_iter()
            .map(|row| row.launch.key.clone())
            .collect();
        assert_eq!(shown, ["c:1"], "a caller off the list leaves it on top");
    }

    #[test]
    fn a_narrow_list_drops_origin_then_the_result() {
        assert_eq!(Columns::fit(600.0), Columns::All);
        assert_eq!(Columns::fit(440.0), Columns::All);
        assert_eq!(Columns::fit(400.0), Columns::NoOrigin);
        assert_eq!(Columns::fit(300.0), Columns::Fewest);
    }

    #[test]
    fn the_result_is_why_a_launch_failed_when_that_is_known() {
        let mut launch = LivePlaybook {
            status: LaunchStatus::Error,
            ..LivePlaybook::default()
        };
        assert_eq!(result(&launch), "error");
        launch.why = Some("no such secret".into());
        assert_eq!(result(&launch), "no such secret");
    }
}
