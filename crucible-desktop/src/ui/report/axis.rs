//! A chart axis: where a value falls along it, the ticks it is labelled at, and how a value
//! on it is written.

use std::borrow::Cow;

use crucible_view::Scale;
use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;

/// The most ticks an axis is labelled at.
const MOST_TICKS: usize = 7;
/// The least step between ticks, as a share of the largest value on the axis.
const FINEST: f64 = 1e-12;
/// The most category names written under an axis.
const MOST_NAMES: usize = 12;

/// A labelled place along an axis, `at` from 0 at its start to 1 at its end.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Tick {
    pub at: f64,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq)]
enum Kind<'a> {
    Numbers,
    /// `lo` and `hi` are powers of ten.
    Log,
    Time,
    Names(&'a [String]),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Axis<'a> {
    kind: Kind<'a>,
    /// The ends, in the axis's own units: powers of ten on a log axis, indexes on a named one.
    lo: f64,
    hi: f64,
    ticks: Vec<Tick>,
}

impl<'a> Axis<'a> {
    /// An axis for `values` on `scale`; numbers start at zero when `zero` and none is below it.
    pub fn new(scale: Scale, values: &[f64], zero: bool) -> Axis<'a> {
        match scale {
            Scale::Linear => Axis::numbers(values, zero, 0.0),
            Scale::Log => Axis::log(values),
            Scale::Time => Axis::time(values),
        }
    }

    /// An axis for counts from zero to `most`, ticked at whole numbers.
    pub fn counts(most: f64) -> Axis<'a> {
        Axis::numbers(&[0.0, most.max(1.0)], true, 1.0)
    }

    /// An axis for the indexes of `names`, each name centred on its own.
    pub fn names(names: &'a [String]) -> Axis<'a> {
        let every = names.len().div_ceil(MOST_NAMES).max(1);
        let hi = names.len().max(1) as f64 - 0.5;
        let ticks = names
            .iter()
            .enumerate()
            .step_by(every)
            .map(|(i, name)| Tick {
                at: (i as f64 + 0.5) / (hi + 0.5),
                label: name.clone(),
            })
            .collect();
        Axis {
            kind: Kind::Names(names),
            lo: -0.5,
            hi,
            ticks,
        }
    }

    fn numbers(values: &[f64], zero: bool, least_step: f64) -> Axis<'a> {
        let (lo, hi) = extent(values);
        let (lo, hi) = if zero {
            (lo.min(0.0), hi.max(0.0))
        } else {
            (lo, hi)
        };
        let (lo, hi) = widen(lo, hi);
        let finest = nice(lo.abs().max(hi.abs()) * FINEST, true).unwrap_or(0.0);
        let Some((first, last, step)) = steps(lo, hi, least_step.max(finest)) else {
            return Axis {
                kind: Kind::Numbers,
                lo,
                hi,
                ticks: [lo, hi]
                    .map(|end| Tick {
                        at: if end == lo { 0.0 } else { 1.0 },
                        label: figure(end),
                    })
                    .to_vec(),
            };
        };
        let count = ((last - first) / step).round() as usize;
        let ticks = (0..=count)
            .map(|i| {
                let value = first + step * i as f64;
                let value = if value.abs() < step * 1e-9 {
                    0.0
                } else {
                    value
                };
                Tick {
                    at: i as f64 / count.max(1) as f64,
                    label: number(value, step),
                }
            })
            .collect();
        Axis {
            kind: Kind::Numbers,
            lo: first,
            hi: last,
            ticks,
        }
    }

    fn log(values: &[f64]) -> Axis<'a> {
        let above: Vec<f64> = values.iter().copied().filter(|v| *v > 0.0).collect();
        let (low, high) = extent(&above);
        let (low, high) = if above.is_empty() {
            (1.0, 10.0)
        } else {
            (low, high)
        };
        let lo = low.log10().floor();
        let hi = high.log10().ceil().max(lo + 1.0);
        let decades = hi - lo;
        if decades <= 2.0 {
            let mut ticks = Vec::new();
            for e in lo as i32..=hi as i32 {
                for m in [1, 2, 5] {
                    let at = (f64::from(e) + f64::from(m).log10() - lo) / decades;
                    if at <= 1.0 + 1e-9 {
                        ticks.push(Tick {
                            at,
                            label: power(m, e),
                        });
                    }
                }
            }
            return Axis {
                kind: Kind::Log,
                lo,
                hi,
                ticks,
            };
        }
        let every = [1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0]
            .into_iter()
            .find(|every| decades / every <= (MOST_TICKS - 1) as f64)
            .unwrap_or(100.0);
        let (lo, hi) = ((lo / every).floor() * every, (hi / every).ceil() * every);
        let count = ((hi - lo) / every).round() as usize;
        let ticks = (0..=count)
            .map(|i| Tick {
                at: i as f64 / count.max(1) as f64,
                label: power(1, (lo + every * i as f64) as i32),
            })
            .collect();
        Axis {
            kind: Kind::Log,
            lo,
            hi,
            ticks,
        }
    }

    fn time(values: &[f64]) -> Axis<'a> {
        let (lo, hi) = extent(values);
        let middle = lo / 2.0 + hi / 2.0;
        let (lo, hi) = if hi - lo >= 10.0 {
            (lo, hi)
        } else {
            (middle - 5.0, middle + 5.0)
        };
        let mut ticks: Vec<Tick> = time_ticks(lo, hi)
            .into_iter()
            .map(|(second, label)| Tick {
                at: (second - lo) / (hi - lo),
                label,
            })
            .collect();
        if ticks.len() < 2 {
            ticks = [(0.0, lo), (1.0, hi)]
                .map(|(at, second)| Tick {
                    at,
                    label: moment(second, "%Y-%m-%d %H:%M:%S"),
                })
                .to_vec();
        }
        Axis {
            kind: Kind::Time,
            lo,
            hi,
            ticks,
        }
    }

    pub fn ticks(&self) -> &[Tick] {
        &self.ticks
    }

    /// Where `value` falls, from 0 at the axis's start to 1 at its end, and never outside.
    pub fn place(&self, value: f64) -> f32 {
        let value = match self.kind {
            Kind::Log if value > 0.0 => value.log10(),
            Kind::Log => self.lo,
            _ => value,
        };
        let at = (value - self.lo) / (self.hi - self.lo);
        if at.is_finite() {
            at.clamp(0.0, 1.0) as f32
        } else {
            0.0
        }
    }

    /// `value` as a reader is shown it on hover.
    pub fn show(&self, value: f64) -> Cow<'a, str> {
        match self.kind {
            Kind::Numbers | Kind::Log => figure(value).into(),
            Kind::Time => moment(value, "%Y-%m-%d %H:%M:%S UTC").into(),
            Kind::Names(names) => match names.get(value as usize) {
                Some(name) => name.as_str().into(),
                None => figure(value).into(),
            },
        }
    }
}

/// The least and greatest of `values`, or 0 and 1 when there are none.
fn extent(values: &[f64]) -> (f64, f64) {
    let lo = values.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if lo.is_finite() && hi.is_finite() {
        (lo, hi)
    } else {
        (0.0, 1.0)
    }
}

/// `lo` to `hi`, spread a tenth either side of a single value, and by one where that cannot be
/// told from it.
pub(crate) fn widen(lo: f64, hi: f64) -> (f64, f64) {
    if hi > lo {
        return (lo, hi);
    }
    let half = lo.abs() * 0.1;
    if lo - half < lo + half {
        (lo - half, lo + half)
    } else {
        (lo - 1.0, lo + 1.0)
    }
}

/// The nearest of 1, 2 and 5 times a power of ten to `raw`; with `up`, the least of them at
/// or above it.
fn nice(raw: f64, up: bool) -> Option<f64> {
    if !(raw > 0.0 && raw.is_finite()) {
        return None;
    }
    let power = 10_f64.powf(raw.log10().floor());
    if !(power > 0.0 && power.is_finite()) {
        return None;
    }
    let norm = raw / power;
    let [one, two, five] = if up { [1.0, 2.0, 5.0] } else { [1.5, 3.0, 7.0] };
    let m = if norm <= one {
        1.0
    } else if norm <= two {
        2.0
    } else if norm <= five {
        5.0
    } else {
        10.0
    };
    Some(m * power)
}

/// The first and last tick at or beyond `lo` and `hi` and the step between them: a nice
/// step of at least `least_step` with at most [`MOST_TICKS`] ticks.
fn steps(lo: f64, hi: f64, least_step: f64) -> Option<(f64, f64, f64)> {
    let mut step = nice((hi - lo) / 5.0, false)?.max(least_step);
    for _ in 0..4 {
        let first = (lo / step + 1e-9).floor() * step;
        let last = (hi / step - 1e-9).ceil() * step;
        let count = ((last - first) / step).round();
        if !(count.is_finite() && first.is_finite() && last.is_finite()) {
            return None;
        }
        if count >= 1.0 && count < MOST_TICKS as f64 {
            return Some((first, last, step));
        }
        step = nice(step * 1.01, true)?;
    }
    None
}

/// `text`, a fixed-point number, without the zeros that end its fraction.
fn trimmed(text: String) -> String {
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text
    }
}

/// A tick's label: `value` with as many decimals as `step` needs, thousands as k, M, B or T.
pub(crate) fn number(value: f64, step: f64) -> String {
    if value == 0.0 || !value.is_finite() {
        return "0".to_string();
    }
    let (unit, suffix) = [(1e12, "T"), (1e9, "B"), (1e6, "M"), (1e3, "k")]
        .into_iter()
        .find(|(unit, _)| value.abs() >= *unit)
        .unwrap_or((1.0, ""));
    let decimals = -(step.abs() / unit).log10().floor();
    let decimals = if decimals.is_finite() {
        decimals.max(0.0) as usize
    } else {
        0
    };
    if decimals > 4 && value.abs() < 1e-3 {
        let digits = (value.abs().log10().floor() - step.abs().log10().floor()).clamp(0.0, 17.0);
        return scientific(value, digits as usize);
    }
    format!(
        "{}{suffix}",
        trimmed(format!("{:.*}", decimals, value / unit))
    )
}

/// `value` to four significant figures, written as a tick is.
pub(crate) fn figure(value: f64) -> String {
    if value == 0.0 || !value.is_finite() {
        return "0".to_string();
    }
    let step = 10_f64.powf(value.abs().log10().floor() - 3.0);
    if step > 0.0 && step.is_finite() {
        number(value, step)
    } else {
        scientific(value, 3)
    }
}

fn scientific(value: f64, digits: usize) -> String {
    let text = format!("{:.*e}", digits, value);
    match text.split_once('e') {
        Some((mantissa, exponent)) => format!("{}e{exponent}", trimmed(mantissa.to_string())),
        None => text,
    }
}

/// `m` times ten to the `e`.
fn power(m: i32, e: i32) -> String {
    if (-4..=15).contains(&e) {
        let value = f64::from(m) * 10_f64.powi(e);
        number(value, value)
    } else {
        format!("{m}e{e}")
    }
}

/// The second `second` in UTC, written with `format`.
fn moment(second: f64, format: &str) -> String {
    match Timestamp::from_second(second.floor() as i64) {
        Ok(at) => at.strftime(format).to_string(),
        Err(_) => figure(second),
    }
}

/// A step between a time axis's ticks.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Every {
    Seconds(i64),
    Months(i32),
    Years(i32),
}

const MINUTE: i64 = 60;
const HOUR: i64 = 3_600;
const DAY: i64 = 86_400;

const EVERY: [Every; 37] = [
    Every::Seconds(1),
    Every::Seconds(2),
    Every::Seconds(5),
    Every::Seconds(10),
    Every::Seconds(15),
    Every::Seconds(30),
    Every::Seconds(MINUTE),
    Every::Seconds(2 * MINUTE),
    Every::Seconds(5 * MINUTE),
    Every::Seconds(10 * MINUTE),
    Every::Seconds(15 * MINUTE),
    Every::Seconds(30 * MINUTE),
    Every::Seconds(HOUR),
    Every::Seconds(2 * HOUR),
    Every::Seconds(3 * HOUR),
    Every::Seconds(6 * HOUR),
    Every::Seconds(12 * HOUR),
    Every::Seconds(DAY),
    Every::Seconds(2 * DAY),
    Every::Seconds(7 * DAY),
    Every::Seconds(14 * DAY),
    Every::Months(1),
    Every::Months(3),
    Every::Months(6),
    Every::Years(1),
    Every::Years(2),
    Every::Years(5),
    Every::Years(10),
    Every::Years(20),
    Every::Years(50),
    Every::Years(100),
    Every::Years(200),
    Every::Years(500),
    Every::Years(1_000),
    Every::Years(2_000),
    Every::Years(5_000),
    Every::Years(10_000),
];

impl Every {
    /// About how many seconds it is.
    fn seconds(self) -> f64 {
        match self {
            Every::Seconds(n) => n as f64,
            Every::Months(n) => f64::from(n) * 30.44 * DAY as f64,
            Every::Years(n) => f64::from(n) * 365.25 * DAY as f64,
        }
    }

    fn format(self, span: f64) -> &'static str {
        match self {
            Every::Seconds(n) if n < MINUTE => "%H:%M:%S",
            Every::Seconds(n) if n < DAY && span <= DAY as f64 => "%H:%M",
            Every::Seconds(n) if n < DAY => "%b %-d %H:%M",
            Every::Seconds(_) => "%b %-d",
            Every::Months(_) => "%b %Y",
            Every::Years(_) => "%Y",
        }
    }
}

/// The ticks from `lo` to `hi` seconds, each at its second with its label: the shortest step
/// that leaves at most [`MOST_TICKS`] of them.
fn time_ticks(lo: f64, hi: f64) -> Vec<(f64, String)> {
    let span = hi - lo;
    let every = EVERY
        .into_iter()
        .find(|every| span / every.seconds() <= (MOST_TICKS - 1) as f64)
        .unwrap_or(Every::Years(10_000));
    let format = every.format(span);
    let seconds: Vec<i64> = match every {
        Every::Seconds(n) => {
            let first = (lo / n as f64).ceil() as i64;
            (0..=MOST_TICKS as i64)
                .map(|i| (first + i) * n)
                .take_while(|second| *second as f64 <= hi)
                .collect()
        }
        Every::Months(n) => calendar(lo, hi, true, n),
        Every::Years(n) => calendar(lo, hi, false, n),
    };
    seconds
        .into_iter()
        .take(MOST_TICKS)
        .filter_map(|second| {
            let at = Timestamp::from_second(second).ok()?;
            Some((second as f64, at.strftime(format).to_string()))
        })
        .collect()
}

/// The starts of every `step` months, or years when not `months`, from `lo` to `hi`.
fn calendar(lo: f64, hi: f64, months: bool, step: i32) -> Vec<i64> {
    let Ok(first) = Timestamp::from_second(lo.floor() as i64) else {
        return Vec::new();
    };
    let first = first.to_zoned(TimeZone::UTC).date();
    let year = i32::from(first.year());
    let at = if months {
        year * 12 + i32::from(first.month()) - 1
    } else {
        year
    };
    let mut found = Vec::new();
    let mut index = at.div_euclid(step) * step;
    for _ in 0..=MOST_TICKS {
        let date = if months {
            Date::new(
                index.div_euclid(12) as i16,
                (index.rem_euclid(12) + 1) as i8,
                1,
            )
        } else {
            Date::new(index as i16, 1, 1)
        };
        let Some(second) = date
            .ok()
            .and_then(|date| date.to_zoned(TimeZone::UTC).ok())
            .map(|zoned| zoned.timestamp().as_second())
        else {
            break;
        };
        if second as f64 > hi {
            break;
        }
        if second as f64 >= lo {
            found.push(second);
        }
        index += step;
    }
    found
}

#[cfg(test)]
mod tests {
    use crucible_view::{MAGNITUDE, Scale, TIME_SPAN};

    use crate::ui::report::axis::{Axis, figure, nice, number, steps, widen};

    fn labels<'a>(axis: &'a Axis<'_>) -> Vec<&'a str> {
        axis.ticks()
            .iter()
            .map(|tick| tick.label.as_str())
            .collect()
    }

    fn sane(axis: &Axis<'_>) {
        let ticks = axis.ticks();
        assert!(ticks.len() >= 2 && ticks.len() <= 7, "{ticks:?}");
        assert!(
            ticks
                .windows(2)
                .all(|pair| pair[0].at < pair[1].at && pair[0].label != pair[1].label),
            "{ticks:?}"
        );
        assert!(
            ticks
                .iter()
                .all(|tick| tick.at.is_finite() && (-1e-9..=1.0 + 1e-9).contains(&tick.at)),
            "{ticks:?}"
        );
    }

    #[test]
    fn a_nice_step_is_one_two_or_five_times_a_power_of_ten() {
        assert_eq!(nice(0.2, false), Some(0.2));
        assert_eq!(nice(1.4, false), Some(1.0));
        assert_eq!(nice(1.6, false), Some(2.0));
        assert_eq!(nice(4.0, false), Some(5.0));
        assert_eq!(nice(8.0, false), Some(10.0));
        assert_eq!(nice(1.01, true), Some(2.0));
        assert_eq!(nice(2.02, true), Some(5.0));
        assert_eq!(nice(5.05, true), Some(10.0));
        for off in [0.0, -1.0, f64::NAN, f64::INFINITY, 5e-324 / 5.0] {
            assert_eq!(nice(off, false), None, "{off}");
        }
        assert_eq!(steps(0.0, 7.0, 0.0), Some((0.0, 8.0, 2.0)));
        assert_eq!(steps(0.0, 1.0, 0.0), Some((0.0, 1.0, 0.2)));
        assert_eq!(steps(0.0, 3.0, 1.0), Some((0.0, 3.0, 1.0)));
        assert_eq!(steps(0.0, 0.0, 0.0), None);
    }

    #[test]
    fn linear_ticks_cover_the_values_at_nice_steps() {
        let axis = Axis::new(Scale::Linear, &[3.3, 3.5], false);
        assert_eq!(labels(&axis), ["3.3", "3.35", "3.4", "3.45", "3.5"]);
        sane(&axis);
        let axis = Axis::new(Scale::Linear, &[0.0, 64.0], true);
        assert_eq!(labels(&axis), ["0", "20", "40", "60", "80"]);
        let axis = Axis::new(Scale::Linear, &[-7.0, 12.0], true);
        assert_eq!(labels(&axis), ["-10", "-5", "0", "5", "10", "15"]);
        assert_eq!(axis.place(-10.0), 0.0);
        assert_eq!(axis.place(15.0), 1.0);
        assert_eq!(axis.place(2.5), 0.5);
        assert_eq!(axis.place(1e300), 1.0, "never off the axis");
        assert_eq!(axis.place(f64::NAN), 0.0);
        let axis = Axis::new(Scale::Linear, &[1200.0, 3400.0], false);
        assert_eq!(labels(&axis), ["1k", "1.5k", "2k", "2.5k", "3k", "3.5k"]);
        sane(&axis);
    }

    #[test]
    fn tiny_huge_and_single_values_still_get_ticks() {
        for values in [
            vec![],
            vec![0.0],
            vec![5.0],
            vec![-3.0],
            vec![1e-300, 2e-300],
            vec![1e-300],
            vec![5e-324],
            vec![5e-324, 1e-323],
            vec![-MAGNITUDE, MAGNITUDE],
            vec![MAGNITUDE],
            vec![-MAGNITUDE],
            vec![MAGNITUDE - 1.0, MAGNITUDE],
            vec![0.1, 0.1 + 1e-12],
        ] {
            for zero in [false, true] {
                let axis = Axis::new(Scale::Linear, &values, zero);
                sane(&axis);
                for value in &values {
                    let at = axis.place(*value);
                    assert!((0.0..=1.0).contains(&at), "{values:?} {at}");
                }
            }
        }
        assert_eq!(
            labels(&Axis::new(Scale::Linear, &[-MAGNITUDE, MAGNITUDE], false)),
            ["-1000T", "-500T", "0", "500T", "1000T"]
        );
        assert_eq!(
            labels(&Axis::new(Scale::Linear, &[1e-7, 3e-7], false)),
            ["1e-7", "1.5e-7", "2e-7", "2.5e-7", "3e-7"]
        );
        assert_eq!(widen(5.0, 5.0), (4.5, 5.5));
        assert_eq!(widen(0.0, 0.0), (-1.0, 1.0));
        assert_eq!(widen(5e-324, 5e-324), (5e-324 - 1.0, 5e-324 + 1.0));
        assert_eq!(widen(1.0, 2.0), (1.0, 2.0));
    }

    #[test]
    fn narrow_spans_anywhere_up_to_the_bound_get_distinct_ticks() {
        for base in [
            -MAGNITUDE,
            -1e12,
            -3.7,
            -1e-9,
            0.0,
            1e-300,
            0.1,
            7.0,
            123_456.0,
            1e9,
            MAGNITUDE - 1.0,
        ] {
            for width in [1e-300, 1e-12, 1e-3, 0.1, 1.0, 37.0, 1e6, 1e14] {
                let hi = (base + width).min(MAGNITUDE);
                for zero in [false, true] {
                    let axis = Axis::new(Scale::Linear, &[base, hi], zero);
                    sane(&axis);
                    for value in [base, hi] {
                        assert!((0.0..=1.0).contains(&axis.place(value)));
                    }
                }
            }
        }
    }

    #[test]
    fn counts_are_ticked_at_whole_numbers() {
        assert_eq!(labels(&Axis::counts(1.0)), ["0", "1"]);
        assert_eq!(labels(&Axis::counts(0.0)), ["0", "1"]);
        assert_eq!(labels(&Axis::counts(3.0)), ["0", "1", "2", "3"]);
        assert_eq!(
            labels(&Axis::counts(12.0)),
            ["0", "2", "4", "6", "8", "10", "12"]
        );
        sane(&Axis::counts(50_000.0));
    }

    #[test]
    fn numbers_are_written_compactly() {
        assert_eq!(number(1234.0, 100.0), "1.2k");
        assert_eq!(number(3_400_000.0, 100_000.0), "3.4M");
        assert_eq!(number(2e9, 1e9), "2B");
        assert_eq!(number(-1500.0, 500.0), "-1.5k");
        assert_eq!(number(500.0, 500.0), "500");
        assert_eq!(number(0.25, 0.05), "0.25");
        assert_eq!(number(0.0, 0.05), "0");
        assert_eq!(number(f64::NAN, 1.0), "0");
        assert_eq!(figure(57.0), "57");
        assert_eq!(figure(0.1 + 0.2), "0.3");
        assert_eq!(figure(1_234_567.0), "1.235M");
        assert_eq!(figure(-0.000_012_34), "-1.234e-5");
        assert_eq!(figure(5e-324), "4.941e-324");
        assert_eq!(figure(MAGNITUDE), "1000T");
        assert_eq!(figure(7.5), "7.5");
    }

    #[test]
    fn a_log_axis_ticks_powers_of_ten() {
        let axis = Axis::new(Scale::Log, &[2.0, 80.0], false);
        assert_eq!(labels(&axis), ["1", "2", "5", "10", "20", "50", "100"]);
        sane(&axis);
        assert_eq!(axis.place(1.0), 0.0);
        assert_eq!(axis.place(10.0), 0.5);
        assert_eq!(axis.place(100.0), 1.0);
        assert_eq!(axis.place(0.0), 0.0, "off a log axis is its start");
        assert_eq!(axis.place(-5.0), 0.0);
        let axis = Axis::new(Scale::Log, &[3.0, 4.0], false);
        assert_eq!(labels(&axis), ["1", "2", "5", "10"]);
        let axis = Axis::new(Scale::Log, &[5e-324, MAGNITUDE], false);
        assert_eq!(
            labels(&axis),
            ["1e-400", "1e-300", "1e-200", "1e-100", "1", "1e100"]
        );
        sane(&axis);
        assert!((0.0..=1.0).contains(&axis.place(5e-324)));
        let axis = Axis::new(Scale::Log, &[0.002, 5e5], false);
        assert_eq!(labels(&axis), ["0.0001", "0.01", "1", "100", "10k", "1M"]);
        sane(&Axis::new(Scale::Log, &[], false));
        sane(&Axis::new(Scale::Log, &[1.0], false));
        sane(&Axis::new(Scale::Log, &[MAGNITUDE], false));
    }

    #[test]
    fn a_time_axis_ticks_dates_and_times_in_utc_at_steps_for_its_span() {
        let day = 86_400.0;
        let nov = 1_761_955_200.0;
        let axis = Axis::new(Scale::Time, &[nov, nov + 30.0 * day], false);
        assert_eq!(
            labels(&axis),
            ["Nov 6", "Nov 13", "Nov 20", "Nov 27"],
            "weeks from the epoch's Thursday"
        );
        let axis = Axis::new(Scale::Time, &[nov, nov + 6.0 * 3600.0], false);
        assert_eq!(
            labels(&axis),
            [
                "00:00", "01:00", "02:00", "03:00", "04:00", "05:00", "06:00"
            ]
        );
        let axis = Axis::new(Scale::Time, &[nov, nov + 3.0 * day], false);
        assert_eq!(
            labels(&axis),
            [
                "Nov 1 00:00",
                "Nov 1 12:00",
                "Nov 2 00:00",
                "Nov 2 12:00",
                "Nov 3 00:00",
                "Nov 3 12:00",
                "Nov 4 00:00"
            ]
        );
        let axis = Axis::new(Scale::Time, &[nov, nov + 400.0 * day], false);
        assert_eq!(
            labels(&axis),
            ["Jan 2026", "Apr 2026", "Jul 2026", "Oct 2026"]
        );
        let axis = Axis::new(Scale::Time, &[0.0, 30.0 * 365.25 * day], false);
        assert_eq!(
            labels(&axis),
            ["1970", "1975", "1980", "1985", "1990", "1995", "2000"]
        );
        let axis = Axis::new(Scale::Time, &[nov, nov + 20.0], false);
        assert_eq!(
            labels(&axis),
            ["00:00:00", "00:00:05", "00:00:10", "00:00:15", "00:00:20"]
        );
        assert_eq!(axis.show(nov + 7.9), "2025-11-01 00:00:07 UTC");
        for values in [
            vec![],
            vec![nov],
            vec![nov, nov + 0.5],
            vec![-0.5, 0.5],
            vec![TIME_SPAN[0], TIME_SPAN[1]],
            vec![TIME_SPAN[0]],
            vec![TIME_SPAN[1]],
            vec![TIME_SPAN[1] - 1.0, TIME_SPAN[1]],
        ] {
            let axis = Axis::new(Scale::Time, &values, false);
            sane(&axis);
            for value in &values {
                assert!((0.0..=1.0).contains(&axis.place(*value)));
            }
        }
        assert_eq!(
            labels(&Axis::new(
                Scale::Time,
                &[TIME_SPAN[0], TIME_SPAN[1]],
                false
            )),
            ["2000", "4000", "6000", "8000"]
        );
    }

    #[test]
    fn a_named_axis_centres_each_name_and_thins_long_lists() {
        let names: Vec<String> = ["3.3", "3.4", "3.5"].map(String::from).to_vec();
        let axis = Axis::names(&names);
        assert_eq!(labels(&axis), ["3.3", "3.4", "3.5"]);
        let at: Vec<f64> = axis.ticks().iter().map(|tick| tick.at).collect();
        assert_eq!(at, [1.0 / 6.0, 0.5, 5.0 / 6.0]);
        assert_eq!(axis.place(1.0), 0.5);
        assert_eq!(axis.show(2.0), "3.5");
        assert_eq!(axis.show(7.0), "7", "an index past the names is its number");
        let many: Vec<String> = (0..20_000).map(|i| i.to_string()).collect();
        let axis = Axis::names(&many);
        assert!(axis.ticks().len() <= 12, "{}", axis.ticks().len());
        assert_eq!(axis.ticks()[1].label, "1667");
        let none = Axis::names(&[]);
        assert!(none.ticks().is_empty());
        assert_eq!(none.place(0.0), 0.5);
    }
}
