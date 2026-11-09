//! The Timeline pane: a Gantt chart of the run's tasks over a strip of how many ran at once.

use rune::{Color, El, ScrollSpec, col, el, icon, row, text};
use rune_theme_opencode::icons as oc;

use crate::gfx;
use crate::session::TaskState;
use crate::timeline::{Bar, RoundSpan, Schedule, concurrency, schedule, tick};
use crate::ui::tasks::{duration, status_color};
use crate::ui::{Paint, Pane, fx};

const LANE: f32 = 5.0;
const STRIP: f32 = 30.0;

pub(crate) fn timeline_view(pane: &Pane, p: &Paint) -> El {
    let plan = schedule(&pane.tasks);
    if plan.bars.is_empty() || plan.span <= 0.0 {
        return col()
            .key("timeline-empty")
            .w_full()
            .grow(1.0)
            .center()
            .gap(6.0)
            .child(icon(&oc::STATUS, 22.0).color(p.c.icon_faint))
            .child(
                text(if plan.bars.is_empty() {
                    "Nothing has started yet."
                } else {
                    "This run recorded no task durations."
                })
                .font(p.k.ty.ui_medium)
                .color(p.c.text_base),
            )
            .child(
                text(if plan.bars.is_empty() {
                    "Bars appear as tasks run."
                } else {
                    "Its session has results but no timings to place them by."
                })
                .font(p.k.ty.ui)
                .color(p.c.text_muted),
            );
    }
    let steps = concurrency(&plan);
    let peak = steps.iter().map(|(_, _, n)| *n).max().unwrap_or(0);

    let mut rows = col()
        .key("timeline-rows")
        .w_full()
        .gap(2.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_edge_fade(8.0)
        .grow(1.0)
        .min_h(0.0);
    for (i, bar) in plan.bars.iter().enumerate() {
        rows = rows.child(bar_row(i, bar, plan.span, p));
    }

    col()
        .key("timeline")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(8.0)
        .child(summary(&plan, peak, p))
        .child(lane(
            "strip",
            text("At once").font(p.k.ty.small).color(p.c.text_faint),
            strip(&steps, plan.span, peak, p),
            p,
        ))
        .child(lane("axis", el(), axis(plan.span, p), p))
        .child(p.k.scroll_bar(rows).w_full().grow(1.0).min_h(0.0))
}

/// The label column, then the track.
fn lane(key: &str, label: El, track: El, p: &Paint) -> El {
    row()
        .key(format!("lane-{key}"))
        .w_full()
        .center_y()
        .child(row().w(p.tune.timeline_label).pr(10.0).child(label))
        .child(track.grow(1.0).min_w(0.0))
}

fn summary(plan: &Schedule, peak: usize, p: &Paint) -> El {
    let critical: f64 = plan
        .bars
        .iter()
        .filter(|bar| bar.critical)
        .filter_map(|bar| bar.end.map(|end| end - bar.start))
        .sum();
    let fact = |label: &str, value: String| {
        row()
            .gap(5.0)
            .center_y()
            .child(
                text(label.to_string())
                    .font(p.k.ty.small)
                    .color(p.c.text_faint),
            )
            .child(
                text(value)
                    .font(p.k.ty.small_medium.features("'tnum' 1"))
                    .color(p.c.text_base),
            )
    };
    row()
        .key("timeline-summary")
        .w_full()
        .gap(16.0)
        .center_y()
        .child(fact("Span", duration(plan.span)))
        .child(fact("Peak", format!("{peak} at once")))
        .child(fact("Critical path", duration(critical)))
        .child(el().grow(1.0))
        .child(
            icon(&oc::INFO, 13.0)
                .color(p.c.icon_faint)
                .help("Placed from durations: each task starts when its dependencies settle."),
        )
}

/// How many tasks ran at once, as steps across the span.
fn strip(steps: &[(f64, f64, usize)], span: f64, peak: usize, p: &Paint) -> El {
    let mut track = row()
        .key("strip")
        .h(STRIP)
        .items_end()
        .radius(p.k.radius.sm)
        .fill(p.c.layer_01)
        .clip_rounded(p.k.radius.sm);
    let mut at = 0.0;
    for (from, to, n) in steps {
        if *from > at {
            track = track.child(el().w_frac(((from - at) / span) as f32));
        }
        let height = STRIP * *n as f32 / peak.max(1) as f32;
        track = track.child(
            el().w_frac(((to - from) / span) as f32)
                .h(height)
                .fill(p.c.text_accent.alpha(if *n == peak { 0.55 } else { 0.3 })),
        );
        at = *to;
    }
    track
}

fn axis(span: f64, p: &Paint) -> El {
    let step = tick(span);
    let mut track = row().key("axis").h(16.0).center_y();
    let mut t = 0.0;
    while t < span {
        let width = (step.min(span - t) / span) as f32;
        track = track.child(
            row()
                .w_frac(width)
                .h_full()
                .center_y()
                .child(el().w(1.0).h(6.0).fill(p.c.border_strong))
                .child(
                    text(duration(t))
                        .font(p.mono(10.5, 400.0))
                        .color(p.c.text_faint)
                        .pl(4.0)
                        .truncate(),
                ),
        );
        t += step;
    }
    track
}

fn ink(state: TaskState, p: &Paint) -> Color {
    match state {
        TaskState::Pending => p.c.text_faint,
        TaskState::Running(_) => p.live,
        TaskState::Settled(status) => status_color(status, p),
    }
}

fn bar_row(i: usize, bar: &Bar, span: f64, p: &Paint) -> El {
    let end = bar.end.unwrap_or(span).max(bar.start);
    let fill = ink(bar.state, p);
    let own = if bar.rounds.is_empty() {
        span_bar(
            bar.start,
            end,
            span,
            fill,
            bar.critical,
            bar.end.is_none(),
            p,
        )
    } else {
        round_bars(&bar.name, &bar.rounds, span, bar.critical, p)
    };
    let mut track = col().w_full().gap(2.0).child(own);
    for (start, lane_end, state) in &bar.lanes {
        let lane_end = lane_end.unwrap_or(span).max(*start);
        track = track.child(
            row()
                .w_full()
                .h(LANE)
                .child(el().w_frac((start / span) as f32))
                .child(
                    el().w_frac(((lane_end - start) / span) as f32)
                        .min_w(2.0)
                        .h(LANE - 1.0)
                        .radius(1.5)
                        .fill(ink(*state, p).alpha(0.7)),
                ),
        );
    }
    let label = row()
        .gap(6.0)
        .center_y()
        .w_full()
        .child(
            text(bar.name.clone())
                .font(p.mono(11.5, if bar.critical { 500.0 } else { 400.0 }))
                .color(if bar.critical {
                    p.c.text_base
                } else {
                    p.c.text_muted
                })
                .grow(1.0)
                .min_w(0.0)
                .truncate()
                .help_when_truncated(bar.name.clone()),
        )
        .child(
            text(match bar.end {
                Some(end) => duration(end - bar.start),
                None => "running".to_string(),
            })
            .font(p.k.ty.small.features("'tnum' 1"))
            .color(p.c.text_faint),
        );
    row()
        .key(format!("bar-{}", bar.name))
        .w_full()
        .min_h(p.tune.timeline_row)
        .py(4.0)
        .radius(p.k.radius.sm)
        .fill(if i % 2 == 1 {
            p.c.layer_01
        } else {
            Color::TRANSPARENT
        })
        .hover(p.c.hover)
        .center_y()
        .child(row().w(p.tune.timeline_label).px(8.0).child(label))
        .child(track.grow(1.0).min_w(0.0).pr(8.0))
}

/// A revise loop's rounds on one track, each where it ran and numbered.
fn round_bars(name: &str, rounds: &[RoundSpan], span: f64, critical: bool, p: &Paint) -> El {
    let mut track = row().w_full().h(12.0);
    let mut at = 0.0;
    for round in rounds {
        let start = round.start.max(at);
        let end = round.end.unwrap_or(span).max(start);
        if start > at {
            track = track.child(el().w_frac(((start - at) / span) as f32));
        }
        let mut bar = row()
            .key(format!("bar-{name}[round-{}]", round.n))
            .help(format!("{name} round {}", round.n))
            .w_frac(((end - start) / span) as f32)
            .min_w(3.0)
            .h(12.0)
            .radius(3.0)
            .center()
            .clip_rounded(3.0)
            .fill(ink(round.state, p).alpha(if critical { 0.95 } else { 0.6 }))
            .child(
                text(round.n.to_string())
                    .font(p.mono(9.0, 600.0))
                    .color(p.c.text_base.alpha(0.85)),
            );
        if round.end.is_none() {
            bar = bar.child(
                fx(
                    gfx::SHIMMER,
                    gfx::data(Color::WHITE, p.tune.bar_shimmer),
                    0.0,
                )
                .radius(3.0),
            );
        }
        track = track.child(bar);
        at = end;
    }
    track
}

fn span_bar(
    start: f64,
    end: f64,
    span: f64,
    fill: Color,
    critical: bool,
    running: bool,
    p: &Paint,
) -> El {
    let mut bar = el()
        .w_frac(((end - start) / span) as f32)
        .min_w(3.0)
        .h(12.0)
        .radius(3.0)
        .fill(fill.alpha(if critical { 0.95 } else { 0.6 }));
    if critical {
        bar = bar.border(1.0, p.c.text_base.alpha(0.35));
    }
    if running {
        bar = bar.clip_rounded(3.0).child(
            fx(
                gfx::SHIMMER,
                gfx::data(Color::WHITE, p.tune.bar_shimmer),
                0.0,
            )
            .radius(3.0),
        );
    }
    row()
        .w_full()
        .h(12.0)
        .child(el().w_frac((start / span) as f32))
        .child(bar)
}
