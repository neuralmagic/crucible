//! One forge account's sign-in as the app shows it, and what its threads report.

use crate::forge::device::{DeviceCode, Tokens};
use crate::forge::link::LinkError;

/// What a sign-in or link thread reports back.
pub enum Event {
    Code(DeviceCode),
    SignedIn(Result<Tokens, String>),
    Linked {
        profile: String,
        answer: Result<String, LinkError>,
        renew_at: Option<i64>,
    },
}

/// One forge account's sign-in as the app shows it.
#[derive(Default)]
pub struct Session {
    pub loaded: bool,
    pub login: Option<String>,
    pub code: Option<DeviceCode>,
    pub busy: bool,
    pub error: Option<String>,
    pub linked: Option<(String, String)>,
    pub next: Option<(String, i64)>,
    pub events: Option<std::sync::mpsc::Receiver<Event>>,
    pub cancel: Option<crate::signin::Cancel>,
}

impl Session {
    pub fn hold(&mut self, profile: &str) {
        self.next = Some((profile.to_string(), i64::MAX));
    }

    pub fn due(&self, profile: &str, now: i64) -> bool {
        if self.login.is_none() || self.busy {
            return false;
        }
        match &self.next {
            Some((at_profile, at)) if at_profile == profile => *at <= now,
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::forge::session::*;

    #[test]
    fn a_link_is_due_once_per_profile_until_its_renewal_or_retry() {
        let mut session = Session::default();
        assert!(!session.due("local", 0));
        session.login = Some("wren".into());
        assert!(session.due("local", 0));
        session.next = Some(("local".into(), 100));
        assert!(!session.due("local", 99));
        assert!(session.due("local", 100));
        assert!(session.due("cluster", 0));
        session.busy = true;
        assert!(!session.due("cluster", 0));
        session.busy = false;
        session.hold("cluster");
        assert!(!session.due("cluster", i64::MAX - 1));
        assert!(session.due("local", 100));
    }
}
