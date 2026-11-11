//! The Activity page's grouping: transitions by the local day they happened on, and the launch
//! a transition's key names.

use crux::dto::Event;
use jiff::Timestamp;
use jiff::tz::TimeZone;

/// The day (`Nov 10`) and time (`13:05`) of `stamp` in `zone`.
pub fn when(stamp: &str, zone: &TimeZone) -> Option<(String, String)> {
    let at = stamp.parse::<Timestamp>().ok()?.to_zoned(zone.clone());
    Some((
        at.strftime("%b %-d").to_string(),
        at.strftime("%H:%M").to_string(),
    ))
}

/// `events` in the order given, split where the local day changes. One whose time does not read
/// goes under an empty day.
pub fn by_day<'a>(events: &'a [Event], zone: &TimeZone) -> Vec<(String, Vec<(&'a Event, String)>)> {
    let mut days: Vec<(String, Vec<(&Event, String)>)> = Vec::new();
    for event in events {
        let (day, time) = when(&event.ts, zone).unwrap_or_default();
        match days.last_mut() {
            Some((last, rows)) if *last == day => rows.push((event, time)),
            _ => days.push((day, vec![(event, time)])),
        }
    }
    days
}

/// The playbook of a launch key, `playbook:<id>:<launch>`.
pub fn playbook_of(key: &str) -> Option<&str> {
    let mut parts = key.splitn(3, ':');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("playbook"), Some(id), Some(launch)) if !id.is_empty() && !launch.is_empty() => {
            Some(id)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crux::dto::Event;
    use jiff::tz::TimeZone;

    use crate::activity::{by_day, playbook_of, when};

    fn event(ts: &str, key: &str) -> Event {
        Event {
            ts: ts.into(),
            key: key.into(),
            from: "running".into(),
            to: "done".into(),
            reason: None,
            actor: None,
        }
    }

    #[test]
    fn transitions_group_by_the_local_day_not_the_utc_one() {
        let eastern = TimeZone::get("America/New_York").unwrap();
        let events = [
            event("2026-11-10T18:01:39Z", "a"),
            event("2026-11-10T05:30:00Z", "b"),
            event("2026-11-10T04:59:00Z", "c"),
            event("not a time", "d"),
        ];
        let days = by_day(&events, &eastern);
        let shape: Vec<(&str, Vec<(&str, &str)>)> = days
            .iter()
            .map(|(day, rows)| {
                (
                    day.as_str(),
                    rows.iter()
                        .map(|(event, time)| (event.key.as_str(), time.as_str()))
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                ("Nov 10", vec![("a", "13:01"), ("b", "00:30")]),
                ("Nov 9", vec![("c", "23:59")]),
                ("", vec![("d", "")]),
            ]
        );
        assert_eq!(
            when("2026-11-10T18:01:39Z", &TimeZone::UTC),
            Some(("Nov 10".to_string(), "18:01".to_string()))
        );
    }

    #[test]
    fn a_launch_key_names_its_playbook() {
        assert_eq!(
            playbook_of("playbook:cve-triage:01a1c69d-e932"),
            Some("cve-triage")
        );
        assert_eq!(playbook_of("playbook:odd:id:with:colons"), Some("odd"));
        assert_eq!(playbook_of("NM-1234"), None);
        assert_eq!(playbook_of("playbook::x"), None);
        assert_eq!(playbook_of("playbook:only"), None);
    }
}
