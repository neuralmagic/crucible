//! The forge hosts each profile's controller links accounts on, and the OAuth client it names
//! for each, read once a profile is ready.

use std::collections::BTreeMap;
use std::sync::mpsc::Receiver;

use serde::Deserialize;

use crate::forge::link::controller;
use crate::forge::{Account, GITHUB_HOST, gitlab_host};
use crate::net::Handle;
use crate::profile::Target;

const APPS_PATH: &str = "/api/me/forge-apps";
const RETRY_SECS: i64 = 300;

type Answer = Result<Vec<Offered>, String>;

/// One forge host a controller links accounts on.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Offered {
    pub forge: String,
    pub host: String,
    #[serde(default)]
    pub client_id: Option<String>,
}

impl Offered {
    pub fn account(&self) -> Option<Account> {
        match self.forge.as_str() {
            "github" if self.host == GITHUB_HOST => Some(Account::Github),
            "gitlab" => gitlab_host(&self.host).map(Account::Gitlab),
            _ => None,
        }
    }
}

pub fn offered(runtime: &Handle, target: &Target) -> Result<Vec<Offered>, String> {
    let client = controller(target)?;
    let answer = runtime.block_on(crate::net::bounded(
        "reading the controller's forge apps",
        client.get::<Vec<Offered>>(APPS_PATH),
    ))?;
    match answer {
        Ok(apps) => Ok(apps),
        Err(e)
            if e.downcast_ref::<crux::client::HttpStatus>()
                .is_some_and(|s| s.status == reqwest::StatusCode::NOT_FOUND) =>
        {
            Ok(Vec::new())
        }
        Err(e) => Err(format!("{e:#}")),
    }
}

/// What each profile's controller offers, and the read in flight.
#[derive(Default)]
pub struct Offers {
    by_profile: BTreeMap<String, Vec<Offered>>,
    next: BTreeMap<String, i64>,
    asked: Option<(String, Receiver<Answer>)>,
}

impl Offers {
    pub fn of(&self, profile: &str) -> &[Offered] {
        self.by_profile.get(profile).map_or(&[], Vec::as_slice)
    }

    pub fn due(&self, profile: &str, now: i64) -> bool {
        self.asked.is_none() && self.next.get(profile).is_none_or(|at| *at <= now)
    }

    pub fn asking(&mut self, profile: &str, rx: Receiver<Answer>) {
        self.asked = Some((profile.to_string(), rx));
    }

    pub fn heard(&mut self, now: i64) -> bool {
        let Some((profile, rx)) = &self.asked else {
            return false;
        };
        let Ok(answer) = rx.try_recv() else {
            return false;
        };
        let profile = profile.clone();
        self.asked = None;
        match answer {
            Ok(apps) => {
                self.by_profile.insert(profile.clone(), apps);
                self.next.insert(profile, i64::MAX);
            }
            Err(_) => {
                self.next.insert(profile, now + RETRY_SECS);
            }
        }
        true
    }

    pub fn forget(&mut self, profile: &str) {
        self.by_profile.remove(profile);
        self.next.remove(profile);
    }
}

#[cfg(test)]
mod tests {
    use crate::forge::apps::*;

    fn offer(forge: &str, host: &str, client_id: Option<&str>) -> Offered {
        Offered {
            forge: forge.into(),
            host: host.into(),
            client_id: client_id.map(str::to_owned),
        }
    }

    #[test]
    fn an_offer_names_an_account_only_on_a_real_forge_host() {
        assert_eq!(
            offer("github", "github.com", None).account(),
            Some(Account::Github)
        );
        assert_eq!(
            offer("gitlab", "GitLab.Example.com", Some("x")).account(),
            Some(Account::Gitlab("gitlab.example.com".into()))
        );
        assert_eq!(offer("github", "ghe.example.com", None).account(), None);
        assert_eq!(offer("gitlab", "a b", None).account(), None);
        assert_eq!(offer("gitea", "git.example.com", None).account(), None);
        let read: Vec<Offered> = serde_json::from_value(serde_json::json!([
            {"forge": "github", "host": "github.com", "client_id": "Iv23", "keeps_tokens": false},
            {"forge": "gitlab", "host": "gitlab.example.com", "client_id": null, "keeps_tokens": true}
        ]))
        .unwrap();
        assert_eq!(read[0].client_id.as_deref(), Some("Iv23"));
        assert_eq!(read[1].client_id, None);
    }

    #[test]
    fn a_profile_is_read_once_and_a_failed_read_retried_later() {
        let mut offers = Offers::default();
        assert!(offers.due("cluster", 0));
        let (tx, rx) = std::sync::mpsc::channel();
        offers.asking("cluster", rx);
        assert!(!offers.due("cluster", 0));
        assert!(!offers.heard(0));
        tx.send(Err("down".into())).unwrap();
        assert!(offers.heard(10));
        assert!(!offers.due("cluster", 10 + RETRY_SECS - 1));
        assert!(offers.due("cluster", 10 + RETRY_SECS));
        let (tx, rx) = std::sync::mpsc::channel();
        offers.asking("cluster", rx);
        tx.send(Ok(vec![offer("github", "github.com", Some("Iv23"))]))
            .unwrap();
        assert!(offers.heard(400));
        assert_eq!(offers.of("cluster").len(), 1);
        assert!(!offers.due("cluster", i64::MAX - 1));
        assert!(offers.of("local").is_empty());
        offers.forget("cluster");
        assert!(offers.due("cluster", 0));
    }
}
