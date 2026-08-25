//! Wall-clock durations parsed at the CLI boundary (`--max-time 30m`).

use std::time::Duration;

/// Parse a short duration like `90s`, `30m`, `1h`. Empty, garbage, negative, and anything
/// `Duration` cannot hold all yield `None`. `Duration::from_secs_f64` panics on a negative or
/// non-finite argument, so both are refused before it is reached.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, unit) = s.split_at(s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len()));
    let n: f64 = num.trim().parse().ok()?;
    let secs = match unit.trim() {
        "" | "s" | "sec" => n,
        "m" | "min" => n * 60.0,
        "h" | "hr" => n * 3600.0,
        _ => return None,
    };
    Duration::try_from_secs_f64(secs).ok()
}

/// A positive wall-clock ceiling, parsed at the CLI boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaxTime(Duration);

#[derive(Debug, thiserror::Error)]
pub enum BadMaxTime {
    #[error("--max-time {raw:?} is not a duration (try `90s`, `30m`, `2h`)")]
    NotADuration { raw: String },
    #[error("--max-time must be positive, got {raw:?}")]
    NotPositive { raw: String },
}

impl std::str::FromStr for MaxTime {
    type Err = BadMaxTime;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let d = parse_duration(s).ok_or_else(|| BadMaxTime::NotADuration { raw: s.to_string() })?;
        if d.is_zero() {
            return Err(BadMaxTime::NotPositive { raw: s.to_string() });
        }
        Ok(MaxTime(d))
    }
}

impl std::fmt::Display for MaxTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}s", self.0.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use crate::duration::{BadMaxTime, MaxTime, parse_duration};
    use std::time::Duration;

    #[test]
    fn parse_duration_handles_suffixes() {
        assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("30m"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_duration("45"), Some(Duration::from_secs(45)));
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("garbage"), None);
        assert_eq!(parse_duration("10x"), None);
    }

    /// Every input reaches `Duration::try_from_secs_f64`, which refuses what
    /// `Duration::from_secs_f64` would have panicked on.
    #[test]
    fn parse_duration_refuses_what_a_duration_cannot_hold() {
        for hostile in [
            "-5",
            "-5s",
            "-0.001h",
            "99999999999999999999h",
            "1e400",
            "nan",
            "inf",
            "-inf",
        ] {
            assert_eq!(parse_duration(hostile), None, "{hostile}");
        }
        assert_eq!(parse_duration("0"), Some(Duration::ZERO));
    }

    #[test]
    fn max_time_parses_and_round_trips() {
        let t: MaxTime = "30m".parse().expect("30m parses");
        assert_eq!(t.to_string(), "1800s");
        assert_eq!(parse_duration(&t.to_string()), parse_duration("30m"));
        for bad in ["garbage", "", "-5m"] {
            assert!(
                matches!(bad.parse::<MaxTime>(), Err(BadMaxTime::NotADuration { .. })),
                "{bad}"
            );
        }
        for zero in ["0", "0s", "0m"] {
            assert!(
                matches!(zero.parse::<MaxTime>(), Err(BadMaxTime::NotPositive { .. })),
                "{zero}"
            );
        }
    }
}
