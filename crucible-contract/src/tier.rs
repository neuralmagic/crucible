//! The judge-tier vocabulary shared by the engine and the controller: [`Tier`] (the ranker's
//! verdict) and [`Disposition`] (the grounded ranker's extended verdict). The engine's
//! `rank-grounded` prints these on the wire; the controller's reconcile acts on them. A single
//! spelling here prevents cross-crate drift.

/// The error every parse in this module returns: the offending input plus what was expected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierParseError(String);

impl std::fmt::Display for TierParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TierParseError {}

/// The judge-tier vocabulary: the ranker's verdict, and nothing else. A row's `tier` is `NULL`
/// until the ranker confirms it; there is no fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Existing-tests: a bug with a reproducible failing test, or a feature with clear acceptance
    /// tests. The judge is just running the suite.
    T0,
    /// New-metric-harness: a measurable quantity (latency/memory/throughput/rate) needing a new,
    /// locally-runnable benchmark.
    T1,
    /// Live-deployment: success can only be measured against one deployed component / real load
    /// test.
    T2,
    /// Multi-component live deployment required: success needs a *composite* live deployment
    /// (GPU-backed, multi-node, or cross-component) that the v1 autopilot cannot build. Scopeable
    /// with a real objective, unlike N; a T3 row stays `new` behind `ControllerCfg::allow_t3`.
    T3,
    /// Not-autoresearchable: no frozen objective (design/docs/discussion, "investigate X").
    N,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::T0 => "T0",
            Tier::T1 => "T1",
            Tier::T2 => "T2",
            Tier::T3 => "T3",
            Tier::N => "N",
        }
    }

    /// Parse a tier string (the DB spelling, and the ranker's verdict JSON spelling, both
    /// exactly `T0|T1|T2|T3|N`); errors on anything else rather than guessing.
    pub fn parse(s: &str) -> Result<Self, TierParseError> {
        Ok(match s {
            "T0" => Tier::T0,
            "T1" => Tier::T1,
            "T2" => Tier::T2,
            "T3" => Tier::T3,
            "N" => Tier::N,
            other => return Err(TierParseError(format!("unknown tier `{other}`"))),
        })
    }

    /// The `allowed_tiers` knob's CLI/env/ConfigMap spelling (`CONTROLLER_ALLOWED_TIERS=t0,t1`):
    /// lowercase, distinct from [`Tier::as_str`]'s uppercase DB/ranker vocabulary, never written
    /// to `issues.tier`.
    pub fn as_str_lower(self) -> &'static str {
        match self {
            Tier::T0 => "t0",
            Tier::T1 => "t1",
            Tier::T2 => "t2",
            Tier::T3 => "t3",
            Tier::N => "n",
        }
    }

    /// Parse the `allowed_tiers` knob's lowercase spelling (`t0|t1|t2|t3`, case-insensitive).
    /// Errors on anything else, including `n`, because [`Tier::N`] is never an allowable tier.
    pub fn parse_lower(s: &str) -> Result<Self, TierParseError> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "t0" => Tier::T0,
            "t1" => Tier::T1,
            "t2" => Tier::T2,
            "t3" => Tier::T3,
            other => {
                return Err(TierParseError(format!(
                    "unknown tier `{other}` (expected one of t0|t1|t2|t3)"
                )));
            }
        })
    }
}

impl std::str::FromStr for Tier {
    type Err = String;

    /// The CLI/env `--allowed-tier`/`CONTROLLER_ALLOWED_TIERS` value parser: [`Tier::parse_lower`]
    /// with a `String` error (`clap::value_parser!`'s bound), rather than [`Tier::parse`]'s DB
    /// vocabulary.
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Tier::parse_lower(s).map_err(|e| e.to_string())
    }
}

/// What a grounded (code-checked) ranking verdict decided: a tier to scope against, or that the
/// issue's ask is already implemented in the checkout (`stale`). `stale` is deliberately NOT a
/// [`Tier`] variant: it says a scope turn is pointless rather than describing what deployment the
/// issue would need, so it never enters `issues.tier`'s T0..N vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Tier(Tier),
    /// The requested change already exists in the checkout; supersedes any tier the text-only
    /// ranker assigned.
    Stale,
}

impl Disposition {
    /// The wire spelling: a [`Tier`]'s own spelling, or `stale`.
    pub fn as_str(self) -> &'static str {
        match self {
            Disposition::Tier(t) => t.as_str(),
            Disposition::Stale => "stale",
        }
    }

    /// Parse the grounded verdict's `tier` field, which carries the extended `T0|T1|T2|T3|N|stale`
    /// vocabulary (a superset of [`Tier::parse`]'s).
    pub fn parse(s: &str) -> Result<Self, TierParseError> {
        if s == "stale" {
            return Ok(Disposition::Stale);
        }
        Ok(Disposition::Tier(Tier::parse(s)?))
    }
}

impl serde::Serialize for Disposition {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for Disposition {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let spelling = String::deserialize(deserializer)?;
        Disposition::parse(&spelling).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use crate::tier::Tier;

    #[test]
    fn tier_lower_round_trips_and_is_case_insensitive() {
        for (t, s) in [
            (Tier::T0, "t0"),
            (Tier::T1, "t1"),
            (Tier::T2, "t2"),
            (Tier::T3, "t3"),
        ] {
            assert_eq!(t.as_str_lower(), s);
            assert_eq!(Tier::parse_lower(s).expect("parses"), t);
            assert_eq!(
                Tier::parse_lower(&s.to_ascii_uppercase()).expect("parses"),
                t
            );
            assert_eq!(s.parse::<Tier>().expect("FromStr parses"), t);
        }
    }

    #[test]
    fn tier_lower_rejects_garbage_and_n() {
        assert!(Tier::parse_lower("t9").is_err());
        assert!(
            Tier::parse_lower("n").is_err(),
            "N is never an allowed tier"
        );
        assert!(Tier::parse_lower("").is_err());
        assert!("bogus".parse::<Tier>().is_err());
    }
}
