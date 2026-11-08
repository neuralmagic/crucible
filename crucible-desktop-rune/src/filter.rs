//! The sidebar's search and switches over live launches.

use crate::live::LivePlaybook;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OwnerFilter {
    All,
    Mine,
}

impl OwnerFilter {
    pub const ALL: [OwnerFilter; 2] = [OwnerFilter::All, OwnerFilter::Mine];

    pub fn label(self) -> &'static str {
        match self {
            OwnerFilter::All => "All",
            OwnerFilter::Mine => "Just mine",
        }
    }

    pub fn keeps(self, created_by: &str, me: &str) -> bool {
        match self {
            OwnerFilter::All => true,
            OwnerFilter::Mine => {
                !me.is_empty() && principal(created_by).eq_ignore_ascii_case(principal(me))
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OriginFilter {
    All,
    Schedules,
    Drafts,
}

impl OriginFilter {
    pub const ALL: [OriginFilter; 3] = [
        OriginFilter::All,
        OriginFilter::Schedules,
        OriginFilter::Drafts,
    ];

    pub fn label(self) -> &'static str {
        match self {
            OriginFilter::All => "All",
            OriginFilter::Schedules => "Schedules",
            OriginFilter::Drafts => "Drafts",
        }
    }

    pub fn keeps(self, origin: &str) -> bool {
        match self {
            OriginFilter::All => true,
            OriginFilter::Schedules => origin == "schedule",
            OriginFilter::Drafts => origin == "draft",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LandedFilter {
    Any,
    Hour,
    Day,
    Week,
}

impl LandedFilter {
    pub const ALL: [LandedFilter; 4] = [
        LandedFilter::Any,
        LandedFilter::Hour,
        LandedFilter::Day,
        LandedFilter::Week,
    ];

    pub fn label(self) -> &'static str {
        match self {
            LandedFilter::Any => "All",
            LandedFilter::Hour => "1h",
            LandedFilter::Day => "24h",
            LandedFilter::Week => "7d",
        }
    }

    pub fn keeps(self, landed: i64, now: i64) -> bool {
        let window = match self {
            LandedFilter::Any => return true,
            LandedFilter::Hour => 3_600,
            LandedFilter::Day => 86_400,
            LandedFilter::Week => 604_800,
        };
        landed > 0 && now.saturating_sub(landed) <= window
    }
}

fn principal(name: &str) -> &str {
    name.strip_prefix("user:").unwrap_or(name).trim()
}

/// The search text and switches, as one predicate over launches.
pub struct LaunchFilter {
    /// Lowercased and trimmed.
    pub query: String,
    pub owner: OwnerFilter,
    pub origin: OriginFilter,
    pub landed: LandedFilter,
}

impl LaunchFilter {
    pub fn narrows(&self) -> bool {
        !self.query.is_empty()
            || self.owner != OwnerFilter::All
            || self.origin != OriginFilter::All
            || self.landed != LandedFilter::Any
    }

    pub fn keeps(&self, launch: &LivePlaybook, me: &str, now: i64) -> bool {
        (self.query.is_empty() || launch.matches(&self.query))
            && self.owner.keeps(&launch.by, me)
            && self.origin.keeps(&launch.origin)
            && self.landed.keeps(launch.landed, now)
    }
}

#[cfg(test)]
mod tests {
    use crate::filter::{LandedFilter, OriginFilter, OwnerFilter};

    #[test]
    fn the_owner_filter_keeps_only_the_signed_in_principal() {
        assert!(OwnerFilter::All.keeps("anyone", "weaton"));
        assert!(OwnerFilter::Mine.keeps("weaton", "weaton"));
        assert!(OwnerFilter::Mine.keeps("user:weaton", "weaton"));
        assert!(!OwnerFilter::Mine.keeps("lwilkins", "weaton"));
        assert!(!OwnerFilter::Mine.keeps("weaton", ""));
    }

    #[test]
    fn the_origin_filter_separates_schedules_from_drafts() {
        assert!(OriginFilter::All.keeps("schedule"));
        assert!(OriginFilter::Schedules.keeps("schedule"));
        assert!(!OriginFilter::Schedules.keeps("draft"));
        assert!(OriginFilter::Drafts.keeps("draft"));
        assert!(!OriginFilter::Drafts.keeps("schedule"));
    }

    #[test]
    fn the_landed_filter_counts_back_from_now() {
        let now = 1_800_000_000;
        assert!(LandedFilter::Any.keeps(0, now));
        assert!(LandedFilter::Hour.keeps(now - 3_000, now));
        assert!(!LandedFilter::Hour.keeps(now - 4_000, now));
        assert!(LandedFilter::Day.keeps(now - 80_000, now));
        assert!(!LandedFilter::Day.keeps(now - 90_000, now));
        assert!(LandedFilter::Week.keeps(now - 500_000, now));
        assert!(!LandedFilter::Week.keeps(now - 700_000, now));
        assert!(!LandedFilter::Hour.keeps(0, now));
    }
}
