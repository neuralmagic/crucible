//! A view's charts: bars, plots on two axes, histograms and heatmaps.

use std::collections::HashSet;

use crucible_view::{Bar, PlotKind, Scale, Series, Tone};
use rune::{Color, El, LineCap, Shape, canvas, col, el, row, text};

use crate::ui::Paint;
use crate::ui::report::axis::{Axis, figure, widen};
use crate::ui::report::hue::{Hues, colour};

/// The most points a plot holds while each shows its series and values on hover.
const HOVER_POINTS: usize = 1_000;
/// The most cells a heatmap draws one by one, each with its value on hover.
const HOVER_CELLS: usize = 2_000;
/// The most columns a heatmap writes its values in.
const WRITTEN: usize = 16;
/// The bins a histogram is counted into when it does not say, at most.
const DEFAULT_BINS: u32 = 30;
/// A chart's own coordinates: as wide as its card lets it be, and this tall in points.
const PLOT: [f32; 2] = [560.0, 220.0];
/// Where a chart starts, past the vertical axis's numbers.
const GUTTER: f32 = 48.0;
/// Between the edge of a chart and its outermost marks.
const INSET: f32 = 6.0;
/// The side of the square a point is hovered in.
const TARGET: f32 = 10.0;
/// The most characters of a label a tooltip shows.
const BRIEF: usize = 80;
/// The width an x tick's label is centred in.
const LABEL: f32 = 80.0;
/// A heatmap row's height, and the width of its labels.
const ROW: f32 = 26.0;
const ROW_LABEL: f32 = 110.0;

fn card(key: String, p: &Paint) -> El {
    col()
        .key(key)
        .w_full()
        .max_w(720.0)
        .padding(12.0)
        .gap(8.0)
        .radius(p.k.radius.lg)
        .fill(p.c.layer_01)
        .border(p.hair, p.c.border_muted)
}

fn name(label: &str, p: &Paint) -> El {
    text(label.to_string())
        .font(p.k.ty.small_medium)
        .color(p.c.text_muted)
}

fn number(label: &str, p: &Paint) -> El {
    text(label.to_string())
        .font(p.mono(10.5, 400.0))
        .color(p.c.text_faint)
}

/// Each label once, in the order first seen, beside its colour.
fn legend<'a>(items: impl Iterator<Item = (&'a str, Color)>, p: &Paint) -> El {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut key = row().w_full().gap(14.0).flex_wrap();
    for (label, color) in items {
        if !seen.insert(label) {
            continue;
        }
        key = key.child(
            row()
                .gap(6.0)
                .center_y()
                .child(el().size(8.0, 8.0).radius(2.0).fill(color))
                .child(
                    text(label.to_string())
                        .font(p.k.ty.small)
                        .color(p.c.text_muted),
                ),
        );
    }
    key
}

/// `text` cut to [`BRIEF`] characters, an ellipsis marking the cut.
fn brief(text: &str) -> String {
    match text.char_indices().nth(BRIEF) {
        Some((at, _)) => format!("{}\u{2026}", &text[..at]),
        None => text.to_string(),
    }
}

fn short(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

pub(crate) fn bars(all: &[Bar], p: &Paint) -> El {
    let total = |bar: &Bar| bar.segments.iter().map(|s| s.value).sum::<f64>();
    let most = all.iter().map(total).fold(0.0, f64::max);
    let mut hues = Hues::default();
    let coloured: Vec<Vec<Color>> = all
        .iter()
        .map(|bar| {
            bar.segments
                .iter()
                .map(|segment| colour(hues.pick(&segment.label, segment.tone), p.k))
                .collect()
        })
        .collect();
    let key = legend(
        all.iter()
            .zip(&coloured)
            .flat_map(|(bar, colours)| bar.segments.iter().zip(colours))
            .map(|(segment, color)| (segment.label.as_str(), *color)),
        p,
    );
    let mut chart = card("report-bars".into(), p).child(key);
    for (bar, colours) in all.iter().zip(&coloured) {
        let mut track = row().grow(1.0).min_w(0.0).h(12.0).gap(1.0);
        for (segment, color) in bar.segments.iter().zip(colours) {
            if segment.value > 0.0 {
                track = track.child(
                    el().grow(segment.value as f32)
                        .h_full()
                        .radius(2.0)
                        .fill(*color)
                        .help(format!(
                            "{} {}",
                            brief(&segment.label),
                            figure(segment.value)
                        )),
                );
            }
        }
        let rest = most - total(bar);
        if rest > 0.0 {
            track = track.child(el().grow(rest as f32).h_full());
        }
        chart = chart.child(
            row()
                .key(format!("report-bar-{}", bar.label))
                .w_full()
                .h(20.0)
                .gap(10.0)
                .center_y()
                .child(
                    text(bar.label.clone())
                        .font(p.mono(11.5, 440.0))
                        .color(p.c.text_base)
                        .w(110.0)
                        .truncate(),
                )
                .child(track)
                .child(
                    row().w(44.0).justify_end().child(
                        text(short(total(bar)))
                            .font(p.mono(11.5, 440.0))
                            .color(p.c.text_muted),
                    ),
                ),
        );
    }
    chart
}

/// How far across a chart's width the place `at` along its x axis is, as a fraction.
fn across(at: f32) -> f32 {
    (INSET + at * (PLOT[0] - 2.0 * INSET)) / PLOT[0]
}

/// How many points down a chart the place `at` up its y axis is.
fn down(at: f32) -> f32 {
    INSET + (1.0 - at) * (PLOT[1] - 2.0 * INSET)
}

/// The point at the places `x` and `y` along the axes, in the chart's own coordinates.
fn spot(x: f32, y: f32) -> [f32; 2] {
    [across(x) * PLOT[0], down(y)]
}

/// `child`, `width` points wide, centred `at` across its parent and `top` points down.
fn centred(at: f32, top: f32, width: f32, height: f32, child: El) -> El {
    row()
        .absolute(-width / 2.0, top)
        .w_full()
        .h(height)
        .child(el().w_frac(at).h_full())
        .child(child.w(width).h(height))
}

/// `child` across its parent from the fraction `from` to `to`, `top` points down.
fn band(from: f32, to: f32, top: f32, height: f32, child: El) -> El {
    row()
        .absolute(0.0, top)
        .w_full()
        .h(height.max(0.0))
        .child(el().w_frac(from).h_full())
        .child(child.w_frac((to - from).max(0.0)).h_full())
}

/// A grid line at every tick of both axes.
fn grid(x: &Axis<'_>, y: &Axis<'_>, p: &Paint) -> Shape {
    let [w, h] = PLOT;
    let mut shape = Shape::new();
    for tick in y.ticks() {
        let at = down(tick.at as f32);
        shape = shape.line([0.0, at], [w, at], 1.0, LineCap::Butt, p.c.border_muted);
    }
    for tick in x.ticks() {
        let at = across(tick.at as f32) * w;
        shape = shape.line([at, 0.0], [at, h], 1.0, LineCap::Butt, p.c.border_muted);
    }
    shape
}

/// The axes' names and tick labels around `shape`, with `marks` laid over it.
fn framed(
    x: &Axis<'_>,
    y: &Axis<'_>,
    names: [&str; 2],
    shape: Shape,
    marks: Vec<El>,
    p: &Paint,
) -> El {
    let [w, h] = PLOT;
    let mut gutter = el().w(GUTTER).h(h);
    for tick in y.ticks() {
        gutter = gutter.child(
            row()
                .absolute(0.0, down(tick.at as f32) - 7.0)
                .w(GUTTER - 8.0)
                .h(14.0)
                .justify_end()
                .child(number(&tick.label, p)),
        );
    }
    let mut labels = el().grow(1.0).min_w(0.0).h(14.0);
    for tick in x.ticks() {
        labels = labels.child(centred(
            across(tick.at as f32),
            0.0,
            LABEL,
            14.0,
            row()
                .justify_center()
                .child(number(&tick.label, p).truncate()),
        ));
    }
    col()
        .w_full()
        .gap(6.0)
        .child(name(names[1], p))
        .child(
            row().w_full().child(gutter).child(
                el().grow(1.0)
                    .min_w(0.0)
                    .h(h)
                    .child(
                        canvas(shape.view_box(w, h))
                            .w_full()
                            .h(h)
                            .absolute(0.0, 0.0),
                    )
                    .children(marks),
            ),
        )
        .child(row().w_full().pl(GUTTER).child(labels))
        .child(
            row()
                .w_full()
                .pl(GUTTER)
                .justify_center()
                .child(name(names[0], p)),
        )
}

/// What a plot is drawn from.
pub(crate) struct Plotted<'a> {
    pub kind: PlotKind,
    pub x: &'a str,
    pub y: &'a str,
    pub series: &'a [Series],
    pub x_scale: Scale,
    pub y_scale: Scale,
    pub categories: &'a [String],
}

pub(crate) fn plot(plotted: &Plotted<'_>, p: &Paint) -> El {
    let Plotted {
        kind,
        x: x_name,
        y: y_name,
        series,
        x_scale,
        y_scale,
        categories,
    } = *plotted;
    let points = || series.iter().flat_map(|s| s.points.iter());
    let xs: Vec<f64> = points().map(|point| point[0]).collect();
    let ys: Vec<f64> = points().map(|point| point[1]).collect();
    let x = if categories.is_empty() {
        Axis::new(x_scale, &xs, false)
    } else {
        Axis::names(categories)
    };
    let y = Axis::new(y_scale, &ys, y_scale == Scale::Linear);
    let mut hues = Hues::default();
    let colours: Vec<Color> = series
        .iter()
        .map(|one| colour(hues.pick(&one.label, one.tone), p.k))
        .collect();
    let hover = xs.len() <= HOVER_POINTS;
    let mut shape = grid(&x, &y, p);
    let mut marks = Vec::new();
    for (one, color) in series.iter().zip(&colours) {
        let placed: Vec<[f32; 2]> = one
            .points
            .iter()
            .map(|[px, py]| spot(x.place(*px), y.place(*py)))
            .collect();
        if kind == PlotKind::Line && placed.len() > 1 {
            shape = shape.stroke(placed.clone(), 1.75, *color);
        }
        for [cx, cy] in &placed {
            let r = 3.0;
            shape = shape.fill(
                vec![[*cx, cy - r], [cx + r, *cy], [*cx, cy + r], [cx - r, *cy]],
                *color,
            );
        }
        if hover {
            for [px, py] in &one.points {
                marks.push(centred(
                    across(x.place(*px)),
                    down(y.place(*py)) - TARGET / 2.0,
                    TARGET,
                    TARGET,
                    el().help(brief(&one.label))
                        .help_row(format!("{} {}", brief(x_name), brief(&x.show(*px))))
                        .help_row(format!("{} {}", brief(y_name), brief(&y.show(*py)))),
                ));
            }
        }
    }
    card(format!("report-plot-{y_name}-{x_name}"), p)
        .child(legend(
            series
                .iter()
                .zip(&colours)
                .map(|(one, color)| (one.label.as_str(), *color)),
            p,
        ))
        .child(framed(&x, &y, [x_name, y_name], shape, marks, p))
}

/// Values counted into bins of equal width from `lo`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Binned {
    pub lo: f64,
    pub width: f64,
    pub counts: Vec<u64>,
}

/// `values` counted into `bins` bins, or about the square root of their number up to
/// [`DEFAULT_BINS`], from the least to the greatest; the greatest falls in the last bin.
pub(crate) fn binned(values: &[f64], bins: Option<u32>) -> Binned {
    let lo = values.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let (lo, hi) = if lo.is_finite() && hi.is_finite() {
        widen(lo, hi)
    } else {
        (0.0, 1.0)
    };
    let asked =
        bins.unwrap_or_else(|| ((values.len() as f64).sqrt().ceil() as u32).clamp(1, DEFAULT_BINS));
    let asked = asked.clamp(1, crucible_view::MAX_BINS);
    let width = (hi - lo) / f64::from(asked);
    let (count, width) = if width > 0.0 && width.is_finite() {
        (asked as usize, width)
    } else {
        (1, hi - lo)
    };
    let mut counts = vec![0_u64; count];
    for value in values {
        let at = ((value - lo) / width).floor();
        let at = if at.is_finite() && at > 0.0 {
            (at as usize).min(count - 1)
        } else {
            0
        };
        counts[at] += 1;
    }
    Binned { lo, width, counts }
}

pub(crate) fn histogram(
    x_name: &str,
    values: &[f64],
    bins: Option<u32>,
    tone: Tone,
    p: &Paint,
) -> El {
    let Binned { lo, width, counts } = binned(values, bins);
    let edge = |i: usize| lo + width * i as f64;
    let x = Axis::new(Scale::Linear, &[lo, edge(counts.len())], false);
    let most = counts.iter().copied().max().unwrap_or(0);
    let y = Axis::counts(most as f64);
    let color = colour(Hues::default().pick("", tone), p.k);
    let mut shape = grid(&x, &y, p);
    let mut marks = Vec::new();
    for (i, count) in counts.iter().enumerate() {
        let (from, to) = (x.place(edge(i)), x.place(edge(i + 1)));
        let top = down(y.place(*count as f64));
        let [left, _] = spot(from, 0.0);
        let [right, bottom] = spot(to, 0.0);
        let (left, right) = (left + 0.5, (right - 0.5).max(left + 0.5));
        if *count > 0 {
            shape = shape.fill(
                vec![[left, top], [right, top], [right, bottom], [left, bottom]],
                color,
            );
        }
        marks.push(band(
            across(from),
            across(to),
            INSET,
            PLOT[1] - 2.0 * INSET,
            el().help(format!("{} to {}", figure(edge(i)), figure(edge(i + 1))))
                .help_row(format!("count {count}")),
        ));
    }
    card(format!("report-histogram-{x_name}"), p).child(framed(
        &x,
        &y,
        [x_name, "count"],
        shape,
        marks,
        p,
    ))
}

/// Where `value` falls from `lo` to `hi`, from 0 to 1; halfway when they are one value.
fn share(value: f64, lo: f64, hi: f64) -> f32 {
    let at = (value - lo) / (hi - lo);
    if at.is_finite() {
        at.clamp(0.0, 1.0) as f32
    } else {
        0.5
    }
}

/// `color` as deep as the place `at` from 0 to 1 calls for.
fn shade(color: Color, at: f32) -> Color {
    color.alpha(0.1 + 0.85 * at)
}

pub(crate) fn heatmap(
    x: &[String],
    y: &[String],
    values: &[Vec<f64>],
    tone: Tone,
    p: &Paint,
) -> El {
    let all = || values.iter().flatten().copied();
    let lo = all().fold(f64::INFINITY, f64::min);
    let hi = all().fold(f64::NEG_INFINITY, f64::max);
    let color = colour(Hues::default().pick("", tone), p.k);
    let cells = x.len().saturating_mul(y.len());
    let label = |body: &str| {
        text(body.to_string())
            .font(p.mono(11.0, 440.0))
            .color(p.c.text_base)
            .truncate()
    };
    let mut chart = card("report-heatmap".into(), p);
    if cells <= HOVER_CELLS {
        let written = x.len() <= WRITTEN;
        for (row_label, row_values) in y.iter().zip(values) {
            let mut line = row().grow(1.0).min_w(0.0).h_full().gap(2.0);
            for (column, value) in x.iter().zip(row_values) {
                let at = share(*value, lo, hi);
                let ink = if at > 0.55 {
                    p.c.bg_base
                } else {
                    p.c.text_base
                };
                line = line.child(
                    row()
                        .grow(1.0)
                        .min_w(0.0)
                        .h_full()
                        .radius(3.0)
                        .fill(shade(color, at))
                        .center_y()
                        .justify_center()
                        .help(format!("{} {}", brief(row_label), brief(column)))
                        .help_row(figure(*value))
                        .children(written.then(|| {
                            text(figure(*value))
                                .font(p.mono(10.5, 460.0))
                                .color(ink)
                                .truncate()
                        })),
                );
            }
            chart = chart.child(
                row()
                    .w_full()
                    .h(ROW)
                    .gap(8.0)
                    .center_y()
                    .child(row().w(ROW_LABEL).child(label(row_label)))
                    .child(line),
            );
        }
    } else {
        let tall = (y.len() as f32 * 4.0).clamp(ROW, 520.0);
        let mut shape = Shape::new();
        for (j, row_values) in values.iter().enumerate() {
            for (i, value) in row_values.iter().enumerate() {
                let (left, top) = (i as f32, j as f32);
                shape = shape.fill(
                    vec![
                        [left, top],
                        [left + 1.0, top],
                        [left + 1.0, top + 1.0],
                        [left, top + 1.0],
                    ],
                    shade(color, share(*value, lo, hi)),
                );
            }
        }
        let rows = Axis::names(y);
        let mut labels = el().w(ROW_LABEL).h(tall);
        for tick in rows.ticks() {
            labels = labels.child(
                row()
                    .absolute(0.0, tick.at as f32 * tall - 7.0)
                    .w(ROW_LABEL)
                    .h(14.0)
                    .child(label(&tick.label)),
            );
        }
        chart = chart.child(
            row().w_full().gap(8.0).child(labels).child(
                canvas(shape.view_box(x.len().max(1) as f32, y.len().max(1) as f32))
                    .grow(1.0)
                    .min_w(0.0)
                    .h(tall),
            ),
        );
    }
    let mut under = el().grow(1.0).min_w(0.0).h(14.0);
    for tick in Axis::names(x).ticks() {
        under = under.child(centred(
            tick.at as f32,
            0.0,
            LABEL,
            14.0,
            row()
                .justify_center()
                .child(number(&tick.label, p).truncate()),
        ));
    }
    let (least, greatest) = if lo.is_finite() && hi.is_finite() {
        (figure(lo), figure(hi))
    } else {
        (String::new(), String::new())
    };
    let mut scale = row()
        .gap(2.0)
        .center_y()
        .child(number(&least, p))
        .child(el().w(4.0));
    for step in 0..5 {
        scale = scale.child(
            el().size(14.0, 8.0)
                .radius(2.0)
                .fill(shade(color, step as f32 / 4.0)),
        );
    }
    chart
        .child(row().w_full().pl(ROW_LABEL + 8.0).child(under))
        .child(
            row()
                .w_full()
                .justify_end()
                .child(scale.child(el().w(4.0)).child(number(&greatest, p))),
        )
}

#[cfg(test)]
mod tests {
    use crucible_view::MAGNITUDE;

    use crate::ui::report::plot::{BRIEF, Binned, across, binned, brief, down, share, spot};

    #[test]
    fn values_are_counted_into_equal_bins_with_the_greatest_in_the_last() {
        assert_eq!(
            binned(&[0.0, 1.0, 2.0, 3.0, 4.0], Some(2)),
            Binned {
                lo: 0.0,
                width: 2.0,
                counts: vec![2, 3]
            }
        );
        assert_eq!(
            binned(&[0.0, 1.0, 2.0, 3.0], None).counts,
            vec![2, 2],
            "the square root of four"
        );
        let many: Vec<f64> = (0..10_000).map(f64::from).collect();
        assert_eq!(binned(&many, None).counts.len(), 30);
        assert_eq!(binned(&many, Some(100)).counts.len(), 100);
        assert_eq!(binned(&many, Some(1)).counts, vec![10_000]);
        assert_eq!(binned(&many, Some(0)).counts.len(), 1, "never no bins");
        assert_eq!(binned(&many, Some(u32::MAX)).counts.len(), 100);
        assert_eq!(binned(&many, None).counts.iter().sum::<u64>(), 10_000);
    }

    #[test]
    fn odd_values_still_bin() {
        let empty = binned(&[], None);
        assert_eq!(
            empty,
            Binned {
                lo: 0.0,
                width: 1.0,
                counts: vec![0]
            }
        );
        let one = binned(&[5.0, 5.0], Some(4));
        assert_eq!(one.counts.iter().sum::<u64>(), 2);
        assert_eq!((one.lo, one.width), (4.5, 0.25));
        for values in [
            vec![-MAGNITUDE, MAGNITUDE],
            vec![MAGNITUDE; 3],
            vec![5e-324, 1e-323],
            vec![5e-324],
            vec![0.0],
            vec![-0.0, 0.0],
        ] {
            for bins in [None, Some(1), Some(100)] {
                let Binned { lo, width, counts } = binned(&values, bins);
                assert!(lo.is_finite() && width > 0.0 && width.is_finite());
                assert!(!counts.is_empty() && counts.len() <= 100);
                assert_eq!(
                    counts.iter().sum::<u64>(),
                    values.len() as u64,
                    "{values:?}"
                );
            }
        }
        assert_eq!(binned(&[-MAGNITUDE, MAGNITUDE], Some(2)).counts, vec![1, 1]);
    }

    #[test]
    fn a_tooltip_shows_a_brief_label() {
        assert_eq!(brief(""), "");
        let fits = "é".repeat(BRIEF);
        assert_eq!(brief(&fits), fits);
        let long = "é".repeat(BRIEF + 1);
        assert_eq!(brief(&long), format!("{}\u{2026}", "é".repeat(BRIEF)));
        assert_eq!(brief(&"x".repeat(20_000)).chars().count(), BRIEF + 1);
    }

    #[test]
    fn a_chart_s_places_stay_inside_it() {
        assert_eq!(spot(0.0, 0.0), [6.0, 214.0]);
        assert_eq!(spot(1.0, 1.0), [554.0, 6.0]);
        assert_eq!(across(0.5), 0.5);
        assert_eq!(down(0.5), 110.0);
        assert_eq!(share(5.0, 0.0, 10.0), 0.5);
        assert_eq!(share(5.0, 5.0, 5.0), 0.5, "one value is halfway");
        assert_eq!(share(-MAGNITUDE, -MAGNITUDE, MAGNITUDE), 0.0);
        assert_eq!(share(MAGNITUDE, -MAGNITUDE, MAGNITUDE), 1.0);
        assert_eq!(share(1.0, f64::INFINITY, f64::NEG_INFINITY), 0.5);
    }
}
