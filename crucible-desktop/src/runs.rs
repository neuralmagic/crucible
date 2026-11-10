//! A playbook's launches laid out for the runs page: lanes on a time axis, days on a calendar,
//! and the counts across them. Times are unix seconds. Days are counted on the viewer's clock:
//! [`wall`] shifts a time into their zone first, the same zone the launch labels use.

use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;

use crate::model::Verdict;

pub const DAY: i64 = 86_400;
const HOUR: i64 = 3_600;

/// `at` as seconds on the wall clock of `zone`, so day and hour arithmetic lands on local ones.
pub fn wall(at: i64, zone: &TimeZone) -> i64 {
    Timestamp::from_second(at).map_or(at, |stamp| at + i64::from(zone.to_offset(stamp).seconds()))
}

/// The stretch of time the axis covers: the first launch to the last, or to `now` while one
/// runs, widened to an hour around a shorter history.
pub fn axis(landed: &[i64], running: bool, now: i64) -> (i64, i64) {
    let first = landed.iter().copied().min().unwrap_or(now);
    let last = landed.iter().copied().max().unwrap_or(now);
    let last = if running { last.max(now) } else { last };
    let short = (HOUR - (last - first)).max(0);
    (first - short / 2, last + short - short / 2)
}

/// A mark on the time axis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tick {
    pub at: i64,
    pub label: String,
}

/// At most eight marks between `start` and `end`, on whole local hours or days. Marks within a
/// day read `14:00`, the first of them `Nov 9 14:00`; midnight and whole-day marks read `Nov 9`.
pub fn ticks(start: i64, end: i64, zone: &TimeZone) -> Vec<Tick> {
    const STEPS: [i64; 10] = [
        HOUR / 4,
        HOUR,
        3 * HOUR,
        6 * HOUR,
        12 * HOUR,
        DAY,
        2 * DAY,
        7 * DAY,
        30 * DAY,
        90 * DAY,
    ];
    let span = (end - start).max(1);
    let step = STEPS
        .into_iter()
        .find(|step| span / step < 8)
        .unwrap_or(365 * DAY);
    let shift = wall(start, zone) - start;
    let mut marks = Vec::new();
    let mut mark = (start + shift).div_euclid(step) * step;
    while mark <= end + shift {
        if mark >= start + shift {
            let label = if mark.rem_euclid(DAY) == 0 {
                day_label(mark.div_euclid(DAY))
            } else {
                clock(mark)
            };
            marks.push(Tick {
                at: mark - shift,
                label,
            });
        }
        mark += step;
    }
    if step < DAY
        && let Some(first) = marks.first_mut()
        && first.label.contains(':')
    {
        let day = day_label((first.at + shift).div_euclid(DAY));
        first.label = format!("{day} {}", first.label);
    }
    marks
}

/// `14:00` for wall-clock seconds.
fn clock(wall: i64) -> String {
    let minutes = wall.rem_euclid(DAY) / 60;
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// How many calendar weeks reach back from `today` to the first launch, between `least` and
/// `most`. Both times are wall-clock seconds.
pub fn weeks_shown(first: i64, today: i64, least: usize, most: usize) -> usize {
    let today = today.div_euclid(DAY);
    let monday = |day: i64| day - weekday(day);
    let back = (monday(today) - monday(first.div_euclid(DAY))).max(0) / 7 + 1;
    usize::try_from(back).map_or(most, |weeks| weeks.clamp(least, most))
}

/// The lane each point lands in so no two in a lane sit closer than `gap`. `xs` are positions
/// along the axis in `0..=1`, in any order; the result is in the same order.
pub fn lanes(xs: &[f32], gap: f32) -> Vec<usize> {
    let mut order: Vec<usize> = (0..xs.len()).collect();
    order.sort_by(|a, b| xs[*a].total_cmp(&xs[*b]));
    let mut ends: Vec<f32> = Vec::new();
    let mut lane = vec![0; xs.len()];
    for i in order {
        let x = xs[i];
        let free = ends.iter().position(|end| x - end >= gap);
        let l = free.unwrap_or_else(|| {
            ends.push(f32::NEG_INFINITY);
            ends.len() - 1
        });
        ends[l] = x;
        lane[i] = l;
    }
    lane
}

/// One calendar cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Day {
    /// Days since 1970-01-01.
    pub day: i64,
    pub runs: usize,
    pub passed: usize,
    pub failed: usize,
    pub running: usize,
}

/// `weeks` whole weeks, Monday first, ending with the week that holds `today`, with each day's
/// launches counted. Launches outside the grid are left out.
pub fn calendar(launches: &[(i64, Verdict)], today: i64, weeks: usize) -> Vec<Day> {
    let today = today.div_euclid(DAY);
    let last = today + 6 - weekday(today);
    let first = last - (weeks as i64) * 7 + 1;
    let mut days: Vec<Day> = (first..=last)
        .map(|day| Day {
            day,
            ..Day::default()
        })
        .collect();
    for (at, verdict) in launches {
        let day = at.div_euclid(DAY);
        if day < first || day > last {
            continue;
        }
        let cell = &mut days[(day - first) as usize];
        cell.runs += 1;
        match verdict {
            Verdict::Valid => cell.passed += 1,
            Verdict::Invalid | Verdict::Error => cell.failed += 1,
            Verdict::Running => cell.running += 1,
            Verdict::Unfinished | Verdict::NotRun | Verdict::Cancelled => {}
        }
    }
    days
}

/// Which week columns carry a month label: the week each month starts in, and the first week
/// when no month starts in the next `room` weeks, so no label runs into the next.
pub fn month_labels(days: &[Day], room: usize) -> Vec<bool> {
    let starts: Vec<bool> = days
        .chunks(7)
        .map(|week| week.iter().any(|d| date(d.day).day() == 1))
        .collect();
    let mut labels = starts.clone();
    if let Some(first) = labels.first_mut() {
        *first = *first || !starts.iter().skip(1).take(room).any(|s| *s);
    }
    labels
}

/// The calendar date of a day number.
pub fn date(day: i64) -> Date {
    Timestamp::from_second(day * DAY)
        .map(|at| at.to_zoned(TimeZone::UTC).date())
        .unwrap_or_default()
}

/// 0 for Monday through 6 for Sunday.
pub fn weekday(day: i64) -> i64 {
    i64::from(date(day).weekday().to_monday_zero_offset())
}

/// "Nov 7" for a day number.
pub fn day_label(day: i64) -> String {
    date(day).strftime("%b %-d").to_string()
}

/// Counts across a playbook's launches.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Summary {
    pub runs: usize,
    pub passed: usize,
    pub failed: usize,
    pub running: usize,
    /// What the launches cost; `None` when none of them says.
    pub spent: Option<f64>,
}

impl Summary {
    pub fn of(launches: &[(Verdict, Option<f64>)]) -> Summary {
        let mut s = Summary::default();
        for (verdict, cost) in launches {
            s.runs += 1;
            if let Some(cost) = cost {
                s.spent = Some(s.spent.unwrap_or(0.0) + cost);
            }
            match verdict {
                Verdict::Valid => s.passed += 1,
                Verdict::Invalid | Verdict::Error => s.failed += 1,
                Verdict::Running => s.running += 1,
                Verdict::Unfinished | Verdict::NotRun | Verdict::Cancelled => {}
            }
        }
        s
    }

    /// Passed over settled, or `None` before anything settled.
    pub fn pass_rate(&self) -> Option<f32> {
        let settled = self.passed + self.failed;
        (settled > 0).then(|| self.passed as f32 / settled as f32)
    }
}

impl Day {
    /// Failed over settled, or `None` when nothing that day settled.
    pub fn fail_rate(&self) -> Option<f32> {
        let settled = self.passed + self.failed;
        (settled > 0).then(|| self.failed as f32 / settled as f32)
    }
}

#[cfg(test)]
mod tests {
    use crate::model::Verdict;
    use crate::runs::{
        DAY, Day, Summary, Tick, axis, calendar, date, day_label, lanes, month_labels, ticks, wall,
        weekday, weeks_shown,
    };
    use jiff::tz::TimeZone;

    fn at(stamp: &str) -> i64 {
        stamp.parse::<jiff::Timestamp>().unwrap().as_second()
    }

    fn labels(marks: &[Tick]) -> Vec<&str> {
        marks.iter().map(|mark| mark.label.as_str()).collect()
    }

    #[test]
    fn wall_time_is_shifted_into_the_zone() {
        let new_york = TimeZone::get("America/New_York").unwrap();
        let noon = at("2026-11-09T12:00:00Z");
        assert_eq!(wall(noon, &TimeZone::UTC), noon);
        assert_eq!(wall(noon, &new_york), noon - 5 * 3_600);
        assert_eq!(wall(at("2026-07-09T12:00:00Z"), &new_york) % DAY, 8 * 3_600);
    }

    #[test]
    fn the_axis_fits_the_launches_and_reaches_now_only_while_one_runs() {
        let (first, last, now) = (1_000_000, 1_000_000 + 3 * DAY, 1_000_000 + 30 * DAY);
        assert_eq!(axis(&[last, first], false, now), (first, last));
        assert_eq!(axis(&[last, first], true, now), (first, now));
        assert_eq!(axis(&[first], false, now), (first - 1_800, first + 1_800));
        assert_eq!(
            axis(&[first, first + 600], false, now),
            (first - 1_500, first + 2_100)
        );
        assert_eq!(axis(&[], false, now), (now - 1_800, now + 1_800));
    }

    #[test]
    fn a_short_history_is_marked_in_hours_and_a_long_one_in_days() {
        let utc = TimeZone::UTC;
        let hours = ticks(at("2026-11-09T13:40:00Z"), at("2026-11-09T16:10:00Z"), &utc);
        assert_eq!(labels(&hours), ["Nov 9 14:00", "15:00", "16:00"]);
        assert_eq!(hours[0].at, at("2026-11-09T14:00:00Z"));

        let over_midnight = ticks(at("2026-11-08T17:00:00Z"), at("2026-11-09T08:00:00Z"), &utc);
        assert_eq!(
            labels(&over_midnight),
            ["Nov 8 18:00", "21:00", "Nov 9", "03:00", "06:00"]
        );

        let days = ticks(at("2026-11-02T09:00:00Z"), at("2026-11-07T09:00:00Z"), &utc);
        assert_eq!(labels(&days), ["Nov 3", "Nov 4", "Nov 5", "Nov 6", "Nov 7"]);

        let weeks = ticks(at("2026-09-01T00:00:00Z"), at("2026-10-20T00:00:00Z"), &utc);
        assert!(weeks.len() <= 8 && weeks.len() >= 6, "{weeks:?}");
        assert!(ticks(5, 5, &utc).is_empty());
    }

    #[test]
    fn marks_fall_on_local_hours_and_days() {
        let kolkata = TimeZone::get("Asia/Kolkata").unwrap();
        let hours = ticks(
            at("2026-11-09T08:10:00Z"),
            at("2026-11-09T10:40:00Z"),
            &kolkata,
        );
        assert_eq!(labels(&hours), ["Nov 9 14:00", "15:00", "16:00"]);
        assert_eq!(hours[0].at, at("2026-11-09T08:30:00Z"));
        let days = ticks(
            at("2026-11-02T09:00:00Z"),
            at("2026-11-07T09:00:00Z"),
            &kolkata,
        );
        assert_eq!(labels(&days), ["Nov 3", "Nov 4", "Nov 5", "Nov 6", "Nov 7"]);
        assert_eq!(days[0].at, at("2026-11-02T18:30:00Z"));
    }

    #[test]
    fn the_calendar_reaches_back_to_the_first_launch_within_its_bounds() {
        let today = 20_400 * DAY;
        assert_eq!(weekday(20_400), 5);
        assert_eq!(weeks_shown(today, today, 4, 12), 4);
        assert_eq!(weeks_shown(today - 6 * DAY, today, 1, 12), 2);
        assert_eq!(weeks_shown(today - 5 * DAY, today, 1, 12), 1);
        assert_eq!(weeks_shown(today - 40 * DAY, today, 4, 12), 6);
        assert_eq!(weeks_shown(today - 400 * DAY, today, 4, 12), 12);
        assert_eq!(weeks_shown(today + DAY, today, 4, 12), 4);
    }

    #[test]
    fn a_day_is_rated_by_what_settled() {
        let day = |passed, failed, running| Day {
            runs: passed + failed + running,
            passed,
            failed,
            running,
            ..Day::default()
        };
        assert_eq!(day(2, 0, 0).fail_rate(), Some(0.0));
        assert_eq!(day(3, 1, 0).fail_rate(), Some(0.25));
        assert_eq!(day(0, 2, 1).fail_rate(), Some(1.0));
        assert_eq!(day(0, 0, 2).fail_rate(), None);
    }

    #[test]
    fn close_points_take_new_lanes_and_far_ones_reuse_them() {
        assert_eq!(lanes(&[0.0, 0.01, 0.02, 0.5, 0.51], 0.05), [0, 1, 2, 0, 1]);
        assert_eq!(lanes(&[0.5, 0.0, 0.01], 0.05), [0, 0, 1]);
        assert_eq!(lanes(&[], 0.05), Vec::<usize>::new());
        assert_eq!(lanes(&[0.2, 0.2], 0.0), [0, 0]);
    }

    #[test]
    fn days_convert_to_dates_and_weekdays() {
        assert_eq!(date(0), jiff::civil::date(1970, 1, 1));
        assert_eq!(weekday(0), 3);
        assert_eq!(date(20_399), jiff::civil::date(2025, 11, 7));
        assert_eq!(weekday(20_399), 4);
        assert_eq!(date(-1), jiff::civil::date(1969, 12, 31));
        assert_eq!(date(11_016), jiff::civil::date(2000, 2, 29));
        assert_eq!(day_label(20_399), "Nov 7");
    }

    #[test]
    fn the_calendar_ends_on_the_sunday_of_this_week_and_counts_outcomes() {
        let today = 20_400 * DAY + 3_600;
        let launches = [
            (today, Verdict::Valid),
            (today + 60, Verdict::Error),
            (today - DAY, Verdict::Running),
            (today - 40 * DAY, Verdict::Valid),
        ];
        let days = calendar(&launches, today, 2);
        assert_eq!(days.len(), 14);
        assert_eq!(weekday(days[0].day), 0);
        assert_eq!(weekday(days[13].day), 6);
        let cell = days.iter().find(|d| d.day == 20_400).unwrap();
        assert_eq!((cell.runs, cell.passed, cell.failed), (2, 1, 1));
        let before = days.iter().find(|d| d.day == 20_399).unwrap();
        assert_eq!((before.runs, before.running), (1, 1));
        assert_eq!(days.iter().map(|d| d.runs).sum::<usize>(), 3);
    }

    #[test]
    fn the_summary_counts_and_rates_settled_runs() {
        let s = Summary::of(&[
            (Verdict::Valid, Some(1.5)),
            (Verdict::Error, None),
            (Verdict::Running, Some(0.5)),
            (Verdict::Valid, Some(1.0)),
        ]);
        assert_eq!((s.runs, s.passed, s.failed, s.running), (4, 2, 1, 1));
        assert!((s.spent.unwrap() - 3.0).abs() < 1e-9);
        assert_eq!(Summary::of(&[(Verdict::Valid, None)]).spent, None);
        assert_eq!(Summary::of(&[(Verdict::Valid, Some(0.0))]).spent, Some(0.0));
        assert!((s.pass_rate().unwrap() - 2.0 / 3.0).abs() < 1e-6);
        assert_eq!(Summary::of(&[(Verdict::Running, None)]).pass_rate(), None);
        let cancelled = Summary::of(&[(Verdict::Cancelled, None), (Verdict::Valid, None)]);
        assert_eq!((cancelled.runs, cancelled.failed), (2, 0));
        assert_eq!(cancelled.pass_rate(), Some(1.0));
    }

    fn weeks_from(monday: &str, weeks: i64) -> Vec<Day> {
        let start = monday
            .parse::<jiff::Timestamp>()
            .expect("stamp")
            .as_second()
            .div_euclid(DAY);
        (start..start + weeks * 7)
            .map(|day| Day {
                day,
                runs: 0,
                passed: 0,
                failed: 0,
                running: 0,
            })
            .collect()
    }

    #[test]
    fn the_first_week_gives_way_to_a_month_starting_close_behind_it() {
        let crowded = month_labels(&weeks_from("2026-08-24T00:00:00Z", 6), 2);
        assert_eq!(crowded, [false, true, false, false, false, true]);
        let roomy = month_labels(&weeks_from("2026-08-03T00:00:00Z", 6), 2);
        assert_eq!(roomy, [true, false, false, false, true, false]);
        assert!(month_labels(&[], 2).is_empty());
    }
}
