//! A playbook's launches laid out for the runs page: lanes on a time axis, days on a calendar,
//! and the counts across them. Times are unix seconds; days are UTC days since the epoch, the
//! same calendar the launch labels use.

use crate::model::Verdict;

pub const DAY: i64 = 86_400;

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
    /// UTC days since 1970-01-01.
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
            Verdict::Unfinished | Verdict::NotRun => {}
        }
    }
    days
}

/// 0 for Monday through 6 for Sunday. 1970-01-01 was a Thursday.
pub fn weekday(day: i64) -> i64 {
    (day + 3).rem_euclid(7)
}

/// The UTC calendar date of a day number: (year, month 1-12, day 1-31).
pub fn date(day: i64) -> (i64, u32, u32) {
    // Howard Hinnant's days-to-civil algorithm.
    let z = day + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

pub const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// "Nov 7" for a day number.
pub fn day_label(day: i64) -> String {
    let (_, month, d) = date(day);
    format!("{} {d}", MONTHS[(month as usize).saturating_sub(1).min(11)])
}

/// Counts across a playbook's launches.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Summary {
    pub runs: usize,
    pub passed: usize,
    pub failed: usize,
    pub running: usize,
    pub spent: f64,
}

impl Summary {
    pub fn of(launches: &[(Verdict, Option<f64>)]) -> Summary {
        let mut s = Summary::default();
        for (verdict, cost) in launches {
            s.runs += 1;
            s.spent += cost.unwrap_or(0.0);
            match verdict {
                Verdict::Valid => s.passed += 1,
                Verdict::Invalid | Verdict::Error => s.failed += 1,
                Verdict::Running => s.running += 1,
                Verdict::Unfinished | Verdict::NotRun => {}
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

#[cfg(test)]
mod tests {
    use crate::model::Verdict;
    use crate::runs::{DAY, Summary, calendar, date, day_label, lanes, weekday};

    #[test]
    fn close_points_take_new_lanes_and_far_ones_reuse_them() {
        assert_eq!(lanes(&[0.0, 0.01, 0.02, 0.5, 0.51], 0.05), [0, 1, 2, 0, 1]);
        assert_eq!(lanes(&[0.5, 0.0, 0.01], 0.05), [0, 0, 1]);
        assert_eq!(lanes(&[], 0.05), Vec::<usize>::new());
        assert_eq!(lanes(&[0.2, 0.2], 0.0), [0, 0]);
    }

    #[test]
    fn days_convert_to_dates_and_weekdays() {
        assert_eq!(date(0), (1970, 1, 1));
        assert_eq!(weekday(0), 3);
        assert_eq!(date(20_399), (2025, 11, 7));
        assert_eq!(weekday(20_399), 4);
        assert_eq!(date(-1), (1969, 12, 31));
        assert_eq!(date(11_016), (2000, 2, 29));
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
        assert!((s.spent - 3.0).abs() < 1e-9);
        assert!((s.pass_rate().unwrap() - 2.0 / 3.0).abs() < 1e-6);
        assert_eq!(Summary::of(&[(Verdict::Running, None)]).pass_rate(), None);
    }
}
