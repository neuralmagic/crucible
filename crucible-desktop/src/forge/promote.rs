//! Promoting a draft: the request that carries the person's own forge token to the controller, and
//! the promotion it answers with.

use serde::Deserialize;

use crate::forge::Account;
use crate::forge::device::Tokens;
use crate::forge::link::controller;
use crate::net::Handle;
use crate::profile::Target;

/// A draft's promotion, as the controller shows it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Promotion {
    pub forge: String,
    pub host: String,
    pub project: String,
    pub path: String,
    pub base: String,
    pub number: u64,
    pub url: String,
    pub version: i64,
    pub state: String,
    #[serde(default)]
    pub note: String,
    pub author: String,
}

impl Promotion {
    pub fn followed(&self) -> bool {
        matches!(self.state.as_str(), "open" | "merged")
    }

    pub fn label(&self) -> String {
        let mark = match self.forge.as_str() {
            "gitlab" => '!',
            _ => '#',
        };
        format!(
            "{}{mark}{} \u{b7} {}",
            self.project, self.number, self.state
        )
    }
}

/// Where to promote to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Destination {
    pub project: String,
    pub path: String,
    pub base: Option<String>,
}

fn promote_path(draft: &str) -> String {
    format!("/api/playbook-drafts/{draft}/promote")
}

fn promotion_path(draft: &str) -> String {
    format!("/api/playbook-drafts/{draft}/promotion")
}

pub fn promote(
    runtime: &Handle,
    target: &Target,
    account: &Account,
    tokens: &Tokens,
    draft: &str,
    to: &Destination,
) -> Result<Promotion, String> {
    let client = controller(target)?;
    let body = serde_json::json!({
        "forge": account.forge(),
        "host": account.host(),
        "project": to.project,
        "path": to.path,
        "base": to.base,
        "token": tokens.access(),
    });
    runtime.block_on(crate::net::within(
        &format!("promoting draft {draft}"),
        client.post(&promote_path(draft), Some(&body)),
    ))
}

pub fn promotion(
    runtime: &Handle,
    target: &Target,
    draft: &str,
) -> Result<Option<Promotion>, String> {
    let client = controller(target)?;
    let answer = runtime.block_on(crate::net::bounded(
        "reading the draft's promotion",
        client.get::<Promotion>(&promotion_path(draft)),
    ))?;
    match answer {
        Ok(promotion) => Ok(Some(promotion)),
        Err(e)
            if e.downcast_ref::<crux::client::HttpStatus>()
                .is_some_and(|s| s.status == reqwest::StatusCode::NOT_FOUND) =>
        {
            Ok(None)
        }
        Err(e) => Err(format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use crate::forge::promote::*;

    #[test]
    fn a_promotion_reads_the_controllers_answer_and_names_its_review() {
        let pr: Promotion = serde_json::from_value(serde_json::json!({
            "forge": "github", "host": "github.com", "project": "acme/packs",
            "path": "packs/triage", "base": "main", "number": 7,
            "url": "https://github.com/acme/packs/pull/7", "version": 3,
            "digest": "tree1:0", "author": "wren", "promoted_by": "wren",
            "state": "open", "note": "", "updated_at": "2026-11-26T12:00:00Z"
        }))
        .unwrap();
        assert_eq!(pr.label(), "acme/packs#7 \u{b7} open");
        assert!(pr.followed());
        let mr = Promotion {
            forge: "gitlab".into(),
            state: "imported".into(),
            ..pr
        };
        assert_eq!(mr.label(), "acme/packs!7 \u{b7} imported");
        assert!(!mr.followed());
        assert_eq!(
            promote_path("triage"),
            "/api/playbook-drafts/triage/promote"
        );
    }
}
