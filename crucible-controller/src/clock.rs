//! The UTC stamps the ledger stores.

/// The stamp format the whole schema stores instants in. Fixed width, always UTC, so a TEXT
/// comparison in SQL orders the same way the instants do.
pub(crate) fn stamp(ts: jiff::Timestamp) -> String {
    ts.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Current instant as RFC3339 UTC (`…Z`), the stamp format shared with the session log.
pub(crate) fn now_rfc3339() -> String {
    stamp(jiff::Timestamp::now())
}

/// Current UTC day as `YYYY-MM-DD` (the ledger's date prefix).
pub(crate) fn today_utc() -> String {
    jiff::Timestamp::now().strftime("%Y-%m-%d").to_string()
}
