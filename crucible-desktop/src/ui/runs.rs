//! The runs page: every launch of one playbook on a time axis and a calendar, and as a list.
//! Picking one opens its run.

use rune::{Color, El, ScrollSpec, col, el, icon, row, text};

use crate::live::LivePlaybook;
use crate::model::Verdict;
use crate::runs::{
    DAY, Day, Summary, axis, calendar, date, day_label, lanes, month_labels, ticks, wall,
    weeks_shown,
};
use crate::ui::Flight;
use crate::ui::{
    Paint, SECTION, TITLE, age, meter, pulsing, runs_note, stat, verdict_color, verdict_icon,
};
use crate::{App, Selection};
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
    now: i64,
    p: &Paint,
) -> El {
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
        .child(stats(&summary, newest, now, p))
        .child(card("Timeline", timeline(launches, now, &zone, p), p))
        .child(
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
                    card("All runs", list(launches, now, p), p)
                        .grow(1.0)
                        .min_w(0.0)
                        .min_h(0.0),
                ),
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

/// Every launch, newest first, under its column heads.
fn list(launches: &[&LivePlaybook], now: i64, p: &Paint) -> El {
    let columns =
        rune::memo::layout_with("runs-list", |rect| Columns::fit(rect[2])).unwrap_or(Columns::All);
    let mut sorted: Vec<&&LivePlaybook> = launches.iter().collect();
    sorted.sort_by_key(|l| std::cmp::Reverse(l.landed));
    let mut rows = col()
        .key("runs-list")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_edge_fade(8.0);
    for (i, launch) in sorted.into_iter().enumerate() {
        if i > 0 {
            rows = rows.child(el().h(p.hair).w_full().fill(p.c.border_muted));
        }
        rows = rows.child(list_row(i, launch, now, columns, p));
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

/// Row `i` of the list, newest first; keyed by position so a script can pick "run-0".
fn list_row(i: usize, launch: &LivePlaybook, now: i64, columns: Columns, p: &Paint) -> El {
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
    row()
        .key(format!("run-{i}"))
        .role("button")
        .label(format!("{} \u{2014} {}", launch.landed_at, launch.status))
        .w_full()
        .h(36.0)
        .px(6.0)
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
            text(launch.landed_at.clone())
                .font(p.k.ty.ui_medium)
                .color(p.c.text_base)
                .w(LANDED),
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
    use crate::live::{LaunchStatus, LivePlaybook};
    use crate::ui::runs::{Columns, result};

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
