//! The app's own version beside a controller's or a release's.

use std::cmp::Ordering;

pub const APP: &str = env!("CARGO_PKG_VERSION");

/// `major.minor.patch` of a version or a `v`-prefixed tag, any pre-release or build suffix left
/// off.
pub fn parse(version: &str) -> Option<[u64; 3]> {
    let core = version.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
    let parsed = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some(parsed)
}

/// How `version` stands beside `other`; nothing when either does not parse.
pub fn beside(version: &str, other: &str) -> Option<Ordering> {
    Some(parse(version)?.cmp(&parse(other)?))
}

/// What to say of a controller on another release than the app; nothing when they match or
/// either does not parse.
pub fn controller_note(app: &str, controller: &str) -> Option<&'static str> {
    match beside(controller, app)? {
        Ordering::Less => Some("older than the app"),
        Ordering::Greater => Some("newer than the app"),
        Ordering::Equal => None,
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use crate::version::{APP, beside, controller_note, parse};

    #[test]
    fn a_version_or_a_tag_parses_to_its_three_numbers() {
        assert_eq!(parse("0.6.2"), Some([0, 6, 2]));
        assert_eq!(parse("v1.10.0"), Some([1, 10, 0]));
        assert_eq!(parse(" v0.7.0-rc.1+abc "), Some([0, 7, 0]));
        for unreadable in ["", "0.6", "0.6.2.1", "v0.x.2", "latest"] {
            assert_eq!(parse(unreadable), None, "{unreadable}");
        }
        assert!(parse(APP).is_some(), "the app's own version parses");
    }

    #[test]
    fn versions_order_by_number_not_by_text() {
        assert_eq!(beside("0.6.10", "v0.6.9"), Some(Ordering::Greater));
        assert_eq!(beside("v0.6.2", "0.6.2"), Some(Ordering::Equal));
        assert_eq!(beside("0.6.2", "1.0.0"), Some(Ordering::Less));
        assert_eq!(beside("0.6.2", "unknown"), None);
    }

    #[test]
    fn a_controller_on_another_release_is_said_to_be_older_or_newer() {
        assert_eq!(
            controller_note("0.6.2", "0.6.1"),
            Some("older than the app")
        );
        assert_eq!(
            controller_note("0.6.2", "0.7.0"),
            Some("newer than the app")
        );
        assert_eq!(controller_note("0.6.2", "0.6.2"), None);
        assert_eq!(controller_note("0.6.2", ""), None);
    }
}
