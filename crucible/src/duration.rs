//! Wall-clock durations parsed at the CLI boundary (`--max-time 30m`) and in workflow sources
//! (`timeout = "10m"`).

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

/// One task attempt's wall-clock limit (`timeout = "10m"`): positive, parsed like `--max-time`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskTimeout(Duration);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BadTimeout {
    #[error("timeout {raw:?} is not a duration (try `90s`, `10m`, `2h`)")]
    NotADuration { raw: String },
    #[error("timeout must be positive, got {raw:?}")]
    NotPositive { raw: String },
}

impl TaskTimeout {
    pub fn get(self) -> Duration {
        self.0
    }
}

impl std::str::FromStr for TaskTimeout {
    type Err = BadTimeout;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let d = parse_duration(s).ok_or_else(|| BadTimeout::NotADuration { raw: s.to_string() })?;
        if d.is_zero() {
            return Err(BadTimeout::NotPositive { raw: s.to_string() });
        }
        Ok(TaskTimeout(d))
    }
}

impl std::fmt::Display for TaskTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Shown(self.0).fmt(f)
    }
}

/// A duration in the `--max-time` syntax, in the largest unit that states it exactly: `1.5h`
/// shows as `90m`, and whatever it shows as parses back to the same duration.
pub struct Shown(pub Duration);

impl std::fmt::Display for Shown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.subsec_nanos() != 0 {
            return write!(f, "{}s", self.0.as_secs_f64());
        }
        match self.0.as_secs() {
            s if s != 0 && s % 3600 == 0 => write!(f, "{}h", s / 3600),
            s if s != 0 && s % 60 == 0 => write!(f, "{}m", s / 60),
            s => write!(f, "{s}s"),
        }
    }
}

impl serde::Serialize for TaskTimeout {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for TaskTimeout {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use crate::duration::{BadMaxTime, BadTimeout, MaxTime, TaskTimeout, parse_duration};
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

    #[test]
    fn a_task_timeout_reads_back_in_the_largest_exact_unit() {
        for (written, canonical, secs) in [
            ("90s", "90s", 90.0),
            ("10m", "10m", 600.0),
            ("2h", "2h", 7200.0),
            ("1.5h", "90m", 5400.0),
            ("3600", "1h", 3600.0),
            ("0.25s", "0.25s", 0.25),
        ] {
            let t: TaskTimeout = written.parse().expect(written);
            assert_eq!(t.to_string(), canonical, "{written}");
            assert_eq!(t.get().as_secs_f64(), secs, "{written}");
            assert_eq!(canonical.parse::<TaskTimeout>(), Ok(t), "{written}");
        }
    }

    #[test]
    fn a_task_timeout_must_be_a_positive_duration() {
        for bad in ["garbage", "", "-5m", "10x", "nan"] {
            assert!(
                matches!(
                    bad.parse::<TaskTimeout>(),
                    Err(BadTimeout::NotADuration { .. })
                ),
                "{bad}"
            );
        }
        for zero in ["0", "0s", "0h"] {
            assert!(
                matches!(
                    zero.parse::<TaskTimeout>(),
                    Err(BadTimeout::NotPositive { .. })
                ),
                "{zero}"
            );
        }
    }

    #[test]
    fn a_task_timeout_travels_as_its_string() {
        let t: TaskTimeout = "10m".parse().unwrap();
        assert_eq!(serde_json::to_string(&t).unwrap(), "\"10m\"");
        assert_eq!(serde_json::from_str::<TaskTimeout>("\"600s\"").unwrap(), t);
        let refused = serde_json::from_str::<TaskTimeout>("\"0s\"").unwrap_err();
        assert!(refused.to_string().contains("positive"), "{refused}");
        assert!(serde_json::from_str::<TaskTimeout>("600").is_err());
    }
}
