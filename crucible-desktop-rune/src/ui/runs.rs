//! The runs page: every launch of one playbook on a time axis and a calendar, and as a list.
//! Picking one opens its run.

use rune::{Color, El, ScrollSpec, col, el, icon, row, text};

use crate::live::LivePlaybook;
use crate::model::Verdict;
use crate::runs::{DAY, Day, Summary, calendar, date, day_label, lanes};
use crate::ui::{
    Paint, SECTION, TITLE, age, meter, mono, pulsing, runs_note, stat, verdict_color, verdict_tone,
};
use crate::{App, Selection};
use rune_kit::Spinner;

const DOT: f32 = 12.0;
const LANE: f32 = 18.0;
const CELL: f32 = 14.0;
const CELL_GAP: f32 = 3.0;
const WEEKS: usize = 12;

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
                        .child(text(playbook.to_string()).font(TITLE).color(p.c.text_base))
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
        .child(card("Timeline", timeline(launches, now, p), p))
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
                        .child(card("Calendar", heatmap(launches, now, p), p))
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
            &format!("${:.2}", summary.spent),
            p.c.text_base,
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
        .child(text(title.to_string()).font(SECTION).color(p.c.text_base))
        .child(body)
}

fn open(launch: &LivePlaybook) -> impl Fn(&mut App) + 'static {
    let pin = Selection::launch(launch);
    move |app: &mut App| app.select(pin.clone())
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
        pulsing(mark, p.live, DOT)
    } else {
        mark
    };
    row()
        .key(format!("dot-{}", launch.key))
        .role("button")
        .label(format!("{} \u{2014} {}", launch.landed_at, launch.status))
        .size(DOT + 6.0, DOT + 6.0)
        .center()
        .radius((DOT + 6.0) / 2.0)
        .hover(p.c.hover)
        .focusable(true)
        .help(format!("{} \u{b7} {}", launch.landed_at, launch.status))
        .on_press(open(launch))
        .child(mark)
}

/// Each launch as a dot where it landed, from the first launch to now, in lanes so none overlap.
fn timeline(launches: &[&LivePlaybook], now: i64, p: &Paint) -> El {
    let start = launches.iter().map(|l| l.landed).min().unwrap_or(now);
    let span = (now - start).max(3_600) as f32;
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

    // Day boundaries along the axis.
    let mut axis = el().key("runs-axis").w_full().h(16.0);
    let first_day = start.div_euclid(DAY) + 1;
    let days = ((now - start) / DAY + 1).max(1);
    let every = match days {
        0..=8 => 1,
        9..=21 => 3,
        22..=60 => 7,
        _ => 30,
    };
    let mut day = first_day;
    while day * DAY < now {
        let x = ((day * DAY - start) as f32 / span).clamp(0.0, 1.0);
        axis = axis.child(
            row()
                .absolute(0.0, 0.0)
                .w_full()
                .h_full()
                .center_y()
                .child(el().w_frac(x))
                .child(el().w(1.0).h(6.0).fill(p.c.border_strong))
                .child(
                    text(day_label(day))
                        .font(p.k.ty.small)
                        .color(p.c.text_faint)
                        .pl(4.0),
                ),
        );
        day += every;
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

/// Twelve weeks of days, Monday at the top, coloured by how each day's launches ended.
fn heatmap(launches: &[&LivePlaybook], now: i64, p: &Paint) -> El {
    let days = calendar(
        &launches
            .iter()
            .map(|l| (l.landed, l.verdict()))
            .collect::<Vec<_>>(),
        now,
        WEEKS,
    );
    let busiest = days.iter().map(|d| d.runs).max().unwrap_or(1).max(1);
    let today = now.div_euclid(DAY);

    let mut grid = row().key("calendar").gap(CELL_GAP);
    let labels = col().gap(CELL_GAP).pr(4.0).pt(16.0 + CELL_GAP).children(
        ["Mon", "", "Wed", "", "Fri", "", ""]
            .map(|day| text(day).font(p.k.ty.small).color(p.c.text_faint).h(CELL)),
    );
    grid = grid.child(labels);
    for week in days.chunks(7) {
        let starts_month =
            week.iter().any(|d| date(d.day).day() == 1) || week[0].day == days[0].day;
        let month = starts_month.then(|| date(week[6].day).strftime("%b").to_string());
        let mut column = col().gap(CELL_GAP).child(
            text(month.unwrap_or_default())
                .font(p.k.ty.small)
                .color(p.c.text_faint)
                .h(16.0)
                .w(CELL),
        );
        for d in week {
            column = column.child(cell(d, busiest, today, launches, p));
        }
        grid = grid.child(column);
    }
    grid
}

fn cell(d: &Day, busiest: usize, today: i64, launches: &[&LivePlaybook], p: &Paint) -> El {
    let future = d.day > today;
    let fill = if d.runs == 0 {
        if future {
            Color::TRANSPARENT
        } else {
            p.c.layer_03
        }
    } else {
        let base = if d.failed > 0 && d.failed >= d.passed {
            p.fail
        } else if d.running > 0 && d.passed == 0 {
            p.live
        } else {
            p.pass
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
    let newest = launches
        .iter()
        .filter(|l| l.landed.div_euclid(DAY) == d.day)
        .max_by_key(|l| l.landed)
        .copied();
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
            .on_press(open(launch)),
        None => c,
    }
}

/// Every launch, newest first.
fn list(launches: &[&LivePlaybook], now: i64, p: &Paint) -> El {
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
        rows = rows.child(list_row(i, launch, now, p));
    }
    p.k.scroll_bar(rows).w_full().grow(1.0).min_h(0.0)
}

/// Row `i` of the list, newest first; keyed by position so a script can pick "run-0".
fn list_row(i: usize, launch: &LivePlaybook, now: i64, p: &Paint) -> El {
    let verdict = launch.verdict();
    let glyph = match verdict_tone(verdict) {
        _ if verdict == Verdict::Running => Spinner::new().size(14.0).color(p.live).build(p.k),
        Some((_, glyph)) => icon(glyph, 14.0).color(verdict_color(verdict, p)),
        None => el()
            .size(10.0, 10.0)
            .radius(5.0)
            .border(1.0, p.c.text_faint),
    };
    let glyph = if verdict == Verdict::Running {
        pulsing(glyph, p.live, 14.0)
    } else {
        glyph
    };
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
        .on_press(open(launch))
        .child(row().size(16.0, 16.0).center().child(glyph))
        .child(
            text(launch.landed_at.clone())
                .font(p.k.ty.ui_medium)
                .color(p.c.text_base)
                .w(110.0),
        )
        .child(
            text(launch.status.clone())
                .font(p.k.ty.ui)
                .color(p.c.text_muted)
                .w(80.0),
        )
        .child(
            text(
                if launch.origin.is_empty() {
                    "launch"
                } else {
                    launch.origin.as_str()
                }
                .to_string(),
            )
            .font(p.k.ty.ui)
            .color(p.c.text_muted)
            .grow(1.0)
            .min_w(0.0)
            .truncate(),
        )
        .child(
            text(launch.cost.map_or("—".to_string(), |c| format!("${c:.2}")))
                .font(mono(12.0, 400.0).features("'tnum' 1"))
                .color(p.c.text_muted)
                .w(64.0),
        )
        .child(
            text(age(launch.landed, now))
                .font(p.k.ty.small)
                .color(p.c.text_faint)
                .w(36.0),
        )
}
