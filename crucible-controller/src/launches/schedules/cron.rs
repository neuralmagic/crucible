//! The cron half of a schedule: one parsed expression, the zone it is read in, and the instants it
//! fires at. Every caller that asks "when next" — the create/update endpoints, the preview
//! endpoint, the sweep's recompute, the missed-window count — comes through [`CronSpec`], so a
//! timezone is interpreted in exactly one place.
//!
//! croner owns the pattern grammar (ranges, steps, lists, `L`, `#`, day-of-month/day-of-week
//! matching); jiff owns the calendar walk and the DST edges. The two meet at integers: croner
//! matches civil fields, jiff turns a matched civil datetime into an instant.

use crate::playbooks::registry::FieldError;
use jiff::Timestamp;
use jiff::civil;
use jiff::tz::{AmbiguousOffset, TimeZone};

/// How far ahead the walk looks before giving up on an expression. Five years covers the sparsest
/// real pattern (Feb 29) with room to spare; past that a pattern matches nothing and the schedule
/// has no next firing.
const SEARCH_DAYS: u32 = 366 * 5;

/// jiff's civil calendar fields are `i8`; croner's matchers take `u32`. Every value passed through
/// here is a month, day, hour, or minute jiff has already bounded.
fn field(v: i8) -> u32 {
    u32::try_from(v).unwrap_or(0)
}

/// A validated schedule: a five-field cron expression plus the IANA zone it is read in.
#[derive(Debug, Clone)]
pub struct CronSpec {
    expr: String,
    tz_name: String,
    cron: croner::Cron,
    tz: TimeZone,
    /// The hours and minutes the pattern matches, resolved once at parse time so the walk skips
    /// straight to candidate times instead of testing every minute of every day.
    hours: Vec<i8>,
    minutes: Vec<i8>,
}

impl CronSpec {
    /// Parse an expression and its zone. `Err` addresses the form field that was wrong, so the
    /// endpoints answer 422 the same way a bad param value does.
    pub fn parse(expr: &str, tz_name: &str) -> Result<CronSpec, FieldError> {
        let expr = expr.trim();
        let tz_name = tz_name.trim();
        let tz = TimeZone::get(tz_name).map_err(|e| FieldError {
            field: "tz".to_string(),
            message: format!("{tz_name:?} is not an IANA time zone (try `UTC`): {e}"),
        })?;
        let cron = croner::parser::CronParser::builder()
            .seconds(croner::parser::Seconds::Disallowed)
            .year(croner::parser::Year::Disallowed)
            .dom_and_dow(false)
            .build()
            .parse(expr)
            .map_err(|e| FieldError {
                field: "cron_expr".to_string(),
                message: format!("{expr:?} is not a cron expression (try `0 * * * *`): {e}"),
            })?;
        let hours = (0..24)
            .filter(|h| cron.pattern.hour_match(field(*h)).unwrap_or(false))
            .collect();
        let minutes = (0..60)
            .filter(|m| cron.pattern.minute_match(field(*m)).unwrap_or(false))
            .collect();
        Ok(CronSpec {
            expr: expr.to_string(),
            tz_name: tz_name.to_string(),
            cron,
            tz,
            hours,
            minutes,
        })
    }

    pub fn expr(&self) -> &str {
        &self.expr
    }

    pub fn tz_name(&self) -> &str {
        &self.tz_name
    }

    /// The first firing strictly after `after`, or `None` when the expression matches nothing
    /// within the search horizon.
    pub fn next_after(&self, after: Timestamp) -> Option<Timestamp> {
        self.firings_after(after).next()
    }

    /// The next `n` firings after `after` — what the preview endpoint serves.
    pub fn next_firings(&self, after: Timestamp, n: usize) -> Vec<Timestamp> {
        self.firings_after(after).take(n).collect()
    }

    /// How many firings fall in `(after, through]`, counting no further than `cap`. This is the
    /// missed-window count: windows a stopped controller slept through, recorded and skipped.
    pub fn firings_between(&self, after: Timestamp, through: Timestamp, cap: usize) -> usize {
        self.firings_after(after)
            .take_while(|ts| *ts <= through)
            .take(cap)
            .count()
    }

    fn firings_after(&self, after: Timestamp) -> Firings<'_> {
        Firings {
            spec: self,
            after,
            date: after.to_zoned(self.tz.clone()).date(),
            slot: 0,
            days: 0,
        }
    }

    fn day_matches(&self, date: civil::Date) -> bool {
        self.cron
            .pattern
            .day_match(
                i32::from(date.year()),
                field(date.month()),
                field(date.day()),
            )
            .unwrap_or(false)
            && self
                .cron
                .pattern
                .month_match(field(date.month()))
                .unwrap_or(false)
    }

    /// The instant a matched civil time happens at. `None` when that local time does not exist —
    /// the hour a spring-forward skipped — so a schedule pinned inside the gap does not fire that
    /// day. A local time that happens twice fires on the first of the two.
    fn instant_at(&self, date: civil::Date, hour: i8, minute: i8) -> Option<Timestamp> {
        let dt = civil::DateTime::from_parts(date, civil::Time::new(hour, minute, 0, 0).ok()?);
        let ambiguous = self.tz.to_ambiguous_timestamp(dt);
        match ambiguous.offset() {
            AmbiguousOffset::Unambiguous { .. } => ambiguous.unambiguous().ok(),
            AmbiguousOffset::Fold { .. } => ambiguous.earlier().ok(),
            AmbiguousOffset::Gap { .. } => None,
        }
    }
}

/// The walk: days that match the date fields, and within each the matching times in order.
struct Firings<'a> {
    spec: &'a CronSpec,
    after: Timestamp,
    date: civil::Date,
    slot: usize,
    days: u32,
}

impl Iterator for Firings<'_> {
    type Item = Timestamp;

    fn next(&mut self) -> Option<Timestamp> {
        let per_day = self.spec.hours.len() * self.spec.minutes.len();
        loop {
            if self.days > SEARCH_DAYS || per_day == 0 {
                return None;
            }
            if self.slot >= per_day || !self.spec.day_matches(self.date) {
                self.date = self.date.tomorrow().ok()?;
                self.days += 1;
                self.slot = 0;
                continue;
            }
            let hour = self.spec.hours[self.slot / self.spec.minutes.len()];
            let minute = self.spec.minutes[self.slot % self.spec.minutes.len()];
            self.slot += 1;
            if let Some(ts) = self.spec.instant_at(self.date, hour, minute)
                && ts > self.after
            {
                return Some(ts);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::stamp;

    fn ts(raw: &str) -> Timestamp {
        raw.parse().expect(raw)
    }

    fn firings(expr: &str, tz: &str, from: &str, n: usize) -> Vec<String> {
        CronSpec::parse(expr, tz)
            .expect(expr)
            .next_firings(ts(from), n)
            .into_iter()
            .map(stamp)
            .collect()
    }

    #[test]
    fn a_firing_is_strictly_after_the_instant_asked_about() {
        assert_eq!(
            firings("0 * * * *", "UTC", "2026-08-23T12:00:00Z", 3),
            [
                "2026-08-23T13:00:00Z",
                "2026-08-23T14:00:00Z",
                "2026-08-23T15:00:00Z"
            ]
        );
        assert_eq!(
            firings("0 * * * *", "UTC", "2026-08-23T11:59:59Z", 1),
            ["2026-08-23T12:00:00Z"]
        );
    }

    /// The expression is read in the schedule's zone, and what is stored is the UTC instant: a
    /// weekday 06:30 in New York is 10:30Z in summer and 11:30Z in winter.
    #[test]
    fn an_expression_is_read_in_its_own_zone() {
        assert_eq!(
            firings(
                "30 6 * * MON-FRI",
                "America/New_York",
                "2026-08-21T12:00:00Z",
                3
            ),
            [
                "2026-08-24T10:30:00Z",
                "2026-08-25T10:30:00Z",
                "2026-08-26T10:30:00Z"
            ],
            "Friday 21st is past 06:30 local, so the next three are Mon-Wed"
        );
        assert_eq!(
            firings(
                "30 6 * * MON-FRI",
                "America/New_York",
                "2026-12-01T12:00:00Z",
                1
            ),
            ["2026-12-02T11:30:00Z"],
            "the same local time, an hour later in UTC once EST returns"
        );
    }

    /// A local time the spring-forward skipped never happened, so the schedule does not fire that
    /// day. 2026-03-08 in New York has no 02:30.
    #[test]
    fn a_local_time_inside_a_dst_gap_does_not_fire() {
        assert_eq!(
            firings("30 2 * * *", "America/New_York", "2026-03-06T12:00:00Z", 3),
            [
                "2026-03-07T07:30:00Z",
                "2026-03-09T06:30:00Z",
                "2026-03-10T06:30:00Z"
            ],
            "the 8th is skipped; the 9th is already on EDT"
        );
    }

    /// A local time the fall-back repeats happens twice; the schedule fires on the first of them,
    /// so a daily job stays daily. 2026-11-01 in New York has two 01:30s.
    #[test]
    fn a_repeated_local_time_fires_once_on_the_earlier_offset() {
        assert_eq!(
            firings("30 1 * * *", "America/New_York", "2026-10-31T12:00:00Z", 3),
            [
                "2026-11-01T05:30:00Z",
                "2026-11-02T06:30:00Z",
                "2026-11-03T06:30:00Z"
            ],
            "05:30Z is 01:30 EDT, the first of the two"
        );
    }

    /// Day-of-month and day-of-week are OR'd, the way vixie cron reads them: a pattern naming both
    /// fires on either. Pinned because croner can be built to AND them instead.
    #[test]
    fn day_of_month_and_day_of_week_are_ored() {
        assert_eq!(
            firings("0 0 13 * FRI", "UTC", "2026-08-01T00:00:00Z", 5),
            [
                "2026-08-07T00:00:00Z",
                "2026-08-13T00:00:00Z",
                "2026-08-14T00:00:00Z",
                "2026-08-21T00:00:00Z",
                "2026-08-28T00:00:00Z"
            ],
            "every Friday, plus the 13th whatever day it is"
        );
    }

    #[test]
    fn missed_windows_are_counted_between_two_instants() {
        let spec = CronSpec::parse("0 * * * *", "UTC").expect("hourly");
        // The window at 12:00 is the one being fired; nothing else fell in before 12:59.
        assert_eq!(
            spec.firings_between(ts("2026-08-23T12:00:00Z"), ts("2026-08-23T12:59:00Z"), 100),
            0
        );
        assert_eq!(
            spec.firings_between(ts("2026-08-23T12:00:00Z"), ts("2026-08-23T13:00:00Z"), 100),
            1
        );
        assert_eq!(
            spec.firings_between(ts("2026-08-23T12:00:00Z"), ts("2026-08-24T00:00:00Z"), 100),
            12
        );
        assert_eq!(
            spec.firings_between(ts("2026-08-23T12:00:00Z"), ts("2026-08-24T00:00:00Z"), 5),
            5,
            "the count stops at the cap"
        );
    }

    /// A sparse pattern still resolves inside the search horizon, and one that can never match
    /// resolves to nothing rather than looping.
    #[test]
    fn a_sparse_pattern_resolves_and_an_impossible_one_does_not() {
        assert_eq!(
            firings("0 0 29 2 *", "UTC", "2026-08-23T00:00:00Z", 1),
            ["2028-02-29T00:00:00Z"]
        );
        assert!(
            CronSpec::parse("0 0 30 2 *", "UTC")
                .expect("parses")
                .next_after(ts("2026-08-23T00:00:00Z"))
                .is_none(),
            "February 30th never comes"
        );
    }

    #[test]
    fn parse_refuses_what_the_sweep_could_not_fire() {
        for (expr, tz, field) in [
            ("", "UTC", "cron_expr"),
            ("every hour", "UTC", "cron_expr"),
            ("0 0 * *", "UTC", "cron_expr"),
            ("0 0 0 * * *", "UTC", "cron_expr"),
            ("99 * * * *", "UTC", "cron_expr"),
            ("0 * * * *", "Mars/Olympus", "tz"),
            ("0 * * * *", "", "tz"),
        ] {
            let err = CronSpec::parse(expr, tz).expect_err("{expr:?} {tz:?}");
            assert_eq!(err.field, field, "{expr:?} {tz:?}: {}", err.message);
        }
        let spec = CronSpec::parse("  0 6 * * *  ", " UTC ").expect("trimmed");
        assert_eq!(spec.expr(), "0 6 * * *");
        assert_eq!(spec.tz_name(), "UTC");
    }

    /// Every instant this module stores is fixed-width UTC, which is what makes the sweep's
    /// `next_due_at <= now` TEXT comparison order the same way the instants do.
    #[test]
    fn stamps_are_fixed_width_utc_and_sort_lexicographically() {
        let spec = CronSpec::parse("0 * * * *", "America/New_York").expect("hourly");
        let stamps: Vec<String> = spec
            .next_firings(ts("2026-11-01T00:00:00Z"), 6)
            .into_iter()
            .map(stamp)
            .collect();
        let mut sorted = stamps.clone();
        sorted.sort();
        assert_eq!(stamps, sorted, "{stamps:?}");
        for s in &stamps {
            assert_eq!(s.len(), 20, "{s}");
            assert!(s.ends_with('Z'), "{s}");
            assert_eq!(stamp(ts(s)), *s, "{s} round-trips");
        }
    }
}
