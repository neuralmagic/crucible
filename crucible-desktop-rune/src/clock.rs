//! RFC 3339 stamps as seconds since the Unix epoch.

/// Seconds since the Unix epoch of a stamp like `2026-11-08T13:30:00.25Z` or
/// `2026-11-08T08:30:00-05:00`.
pub fn secs(stamp: &str) -> Option<f64> {
    let (date, rest) = stamp.trim().split_once(['T', 't', ' '])?;
    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;
    if date.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let zone = rest.find(['Z', 'z', '+', '-'])?;
    let (time, offset) = rest.split_at(zone);
    let mut time = time.split(':');
    let hour: i64 = time.next()?.parse().ok()?;
    let minute: i64 = time.next()?.parse().ok()?;
    let second: f64 = time.next()?.parse().ok()?;
    if time.next().is_some() || hour > 23 || minute > 59 || !(0.0..61.0).contains(&second) {
        return None;
    }
    let offset = match offset {
        "Z" | "z" => 0,
        offset => {
            let sign = if offset.starts_with('-') { -1 } else { 1 };
            let (hours, minutes) = offset.get(1..)?.split_once(':')?;
            let hours: i64 = hours.parse().ok()?;
            let minutes: i64 = minutes.parse().ok()?;
            sign * (hours * 3_600 + minutes * 60)
        }
    };
    let whole = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 - offset;
    Some(whole as f64 + second)
}

/// Howard Hinnant's days-from-civil, with 1970-01-01 as day zero.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let month = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * month + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use crate::clock::secs;

    #[test]
    fn stamps_read_to_the_fraction_of_a_second_in_any_offset() {
        assert_eq!(secs("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(secs("2026-11-06T18:00:00Z"), Some(1_793_988_000.0));
        assert_eq!(secs("2026-11-06T18:00:00.25Z"), Some(1_793_988_000.25));
        assert_eq!(secs("2026-11-06T13:00:00-05:00"), Some(1_793_988_000.0));
        assert_eq!(secs("2026-11-06T19:30:00+01:30"), Some(1_793_988_000.0));
        assert_eq!(secs("2000-02-29T00:00:00Z"), Some(951_782_400.0));
        for bad in [
            "",
            "2026-11-06",
            "2026-11-06T18:00:00",
            "2026-13-06T18:00:00Z",
            "2026-11-06T24:00:00Z",
            "2026-11-06T18:00Z",
            "2026-11-06T18:00:00+0100",
            "soon",
        ] {
            assert_eq!(secs(bad), None, "{bad}");
        }
    }
}
