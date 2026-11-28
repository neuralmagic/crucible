//! Promoting a draft to a pack repository: a pull request (GitHub) or merge request (GitLab) the
//! person authors with their own forge token, which the controller follows by polling until it
//! merges and the pack is registered from it.
//!
//! ```text
//! desktop ── POST /promote {account, project, path, token} ──▶ check approve, link, push access
//!                                                             write one commit, open PR / MR
//! follow loop ── GET pull / merge request (If-None-Match) ──▶ open │ closed │ merged ─▶ register
//! ```

pub mod api;
pub mod follow;
mod forge;
pub mod store;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::identity::forge::{Forge, ForgeHost};

/// Where a promotion stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PromotionState {
    Open,
    Merged,
    Closed,
    Imported,
    ImportFailed,
}

impl PromotionState {
    pub fn as_str(self) -> &'static str {
        match self {
            PromotionState::Open => "open",
            PromotionState::Merged => "merged",
            PromotionState::Closed => "closed",
            PromotionState::Imported => "imported",
            PromotionState::ImportFailed => "import_failed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "open" => Some(PromotionState::Open),
            "merged" => Some(PromotionState::Merged),
            "closed" => Some(PromotionState::Closed),
            "imported" => Some(PromotionState::Imported),
            "import_failed" => Some(PromotionState::ImportFailed),
            _ => None,
        }
    }

    pub fn followed(self) -> bool {
        matches!(self, PromotionState::Open | PromotionState::Merged)
    }
}

/// One draft's promotion, as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Promotion {
    pub draft: String,
    pub forge: Forge,
    pub host: ForgeHost,
    pub project: String,
    pub path: String,
    pub base: String,
    pub branch: String,
    pub number: u64,
    pub url: String,
    pub version: i64,
    pub digest: String,
    pub author: String,
    pub promoted_by: String,
    pub state: PromotionState,
    pub note: String,
    pub etag: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl Promotion {
    pub fn registry_repo(&self) -> String {
        registry_repo(self.forge, &self.host, &self.project)
    }
}

pub fn registry_repo(forge: Forge, host: &ForgeHost, project: &str) -> String {
    match forge {
        Forge::Github => project.to_owned(),
        Forge::Gitlab => format!("https://{}/{project}.git", host.as_str()),
    }
}

pub fn branch_for(draft: &str) -> String {
    format!("crucible/draft-{draft}")
}

/// A promotion as the API shows it.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct PromotionDto {
    pub forge: Forge,
    pub host: String,
    pub project: String,
    pub path: String,
    pub base: String,
    pub number: u64,
    pub url: String,
    pub version: i64,
    pub digest: String,
    pub author: String,
    pub promoted_by: String,
    pub state: PromotionState,
    pub note: String,
    pub updated_at: String,
}

impl From<Promotion> for PromotionDto {
    fn from(p: Promotion) -> Self {
        PromotionDto {
            forge: p.forge,
            host: p.host.as_str().to_owned(),
            project: p.project,
            path: p.path,
            base: p.base,
            number: p.number,
            url: p.url,
            version: p.version,
            digest: p.digest,
            author: p.author,
            promoted_by: p.promoted_by,
            state: p.state,
            note: p.note,
            updated_at: p.updated_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::playbooks::promotion::*;

    #[test]
    fn states_round_trip_and_only_open_or_merged_are_followed() {
        for state in [
            PromotionState::Open,
            PromotionState::Merged,
            PromotionState::Closed,
            PromotionState::Imported,
            PromotionState::ImportFailed,
        ] {
            assert_eq!(PromotionState::parse(state.as_str()), Some(state));
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                serde_json::json!(state.as_str())
            );
        }
        assert_eq!(PromotionState::parse("draft"), None);
        assert!(PromotionState::Open.followed() && PromotionState::Merged.followed());
        assert!(!PromotionState::Imported.followed());
    }

    #[test]
    fn a_promotion_registers_from_the_forges_clone_address() {
        let gl = ForgeHost::parse("gitlab.example.com").unwrap();
        assert_eq!(
            registry_repo(Forge::Gitlab, &gl, "group/packs"),
            "https://gitlab.example.com/group/packs.git"
        );
        assert_eq!(
            registry_repo(Forge::Github, &ForgeHost::github(), "acme/packs"),
            "acme/packs"
        );
        assert_eq!(branch_for("triage"), "crucible/draft-triage");
    }
}
