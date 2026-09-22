//! What issues a [`SecretMode::Minted`] secret's bytes. The row names its minter as a `mint://`
//! URI in `vault_path`, the same column a reference spells a `vault://` URL in.

use crate::secrets::SecretMode;

/// The URI scheme a minted secret's source is spelled with.
const SCHEME: &str = "mint://";

/// A source that issues a fresh credential per dispatch instead of storing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Minter {
    /// An installation token for the controller's GitHub App: `ghs_…`, one hour.
    GithubApp,
}

/// Why a stored or requested minter could not be resolved.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MinterError {
    #[error("{0} is not a minter uri (expected mint://<minter>)")]
    NotAMinterUri(String),
    #[error("no minter named {0}")]
    Unknown(String),
}

impl Minter {
    /// The name a registration asks for.
    pub fn as_str(self) -> &'static str {
        match self {
            Minter::GithubApp => "github-app",
        }
    }

    /// What the row stores.
    pub fn uri(self) -> String {
        format!("{SCHEME}{}", self.as_str())
    }

    /// Resolve the name a registration named.
    pub fn parse_name(raw: &str) -> Result<Self, MinterError> {
        match raw.trim() {
            "github-app" => Ok(Minter::GithubApp),
            other => Err(MinterError::Unknown(other.to_string())),
        }
    }

    /// Resolve what a row stored.
    pub fn parse_uri(raw: &str) -> Result<Self, MinterError> {
        let raw = raw.trim();
        let name = raw
            .strip_prefix(SCHEME)
            .ok_or_else(|| MinterError::NotAMinterUri(raw.to_string()))?;
        Self::parse_name(name)
    }

    /// The minter a row names, or `None` when its bytes are stored.
    pub fn of(mode: SecretMode, vault_path: &str) -> Result<Option<Self>, MinterError> {
        match mode {
            SecretMode::Minted => Self::parse_uri(vault_path).map(Some),
            _ => Ok(None),
        }
    }
}

/// Whether any of a run's bound secrets is issued by `minter`. An unparseable source is nobody's;
/// the provider has already refused it by the time this runs.
pub fn any_bound(rows: &[crate::secrets::store::SecretRow], minter: Minter) -> bool {
    rows.iter()
        .any(|row| Minter::of(row.mode, &row.vault_path) == Ok(Some(minter)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::model::Principal;
    use crate::secrets::store::SecretRow;
    use crate::secrets::{ConsumerClass, SecretKind, SecretName, Visibility};

    fn row(mode: SecretMode, source: &str) -> SecretRow {
        SecretRow {
            id: "id".to_string(),
            name: SecretName::parse("pr-token").unwrap(),
            owner: Principal::parse("user:wynn").unwrap(),
            kind: SecretKind::Opaque,
            visibility: Visibility::BrokerOnly,
            consumer: ConsumerClass::Run,
            mode,
            vault_path: source.to_string(),
            current_version: None,
            created_by: None,
            created_at: "2026-08-29T00:00:00Z".to_string(),
            updated_at: "2026-08-29T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn a_run_pushes_as_the_app_only_when_one_of_its_secrets_is_minted_by_it() {
        let stored = row(SecretMode::Managed, "crucible/registry/user:wynn/pr-token");
        let minted = row(SecretMode::Minted, "mint://github-app");
        assert!(!any_bound(&[], Minter::GithubApp));
        assert!(!any_bound(std::slice::from_ref(&stored), Minter::GithubApp));
        assert!(any_bound(&[stored, minted], Minter::GithubApp));
    }

    /// A row whose source does not parse belongs to no minter.
    #[test]
    fn an_unreadable_source_is_not_credited_to_a_minter() {
        assert!(!any_bound(
            &[row(SecretMode::Minted, "mint://nonesuch")],
            Minter::GithubApp
        ));
    }

    #[test]
    fn a_minter_round_trips_through_the_uri_a_row_stores() {
        assert_eq!(Minter::GithubApp.uri(), "mint://github-app");
        assert_eq!(
            Minter::parse_uri("mint://github-app").unwrap(),
            Minter::GithubApp
        );
    }

    #[test]
    fn a_stored_vault_path_is_refused_as_a_minter() {
        let err = Minter::parse_uri("vault://kv/data/team/pr#token").unwrap_err();
        assert!(matches!(err, MinterError::NotAMinterUri(_)), "{err:?}");
    }

    #[test]
    fn an_unknown_minter_names_itself() {
        assert_eq!(
            Minter::parse_name("gitlab-app").unwrap_err(),
            MinterError::Unknown("gitlab-app".to_string())
        );
    }

    /// A stored secret has no minter, so a reader can ask without branching on the mode first.
    #[test]
    fn a_stored_mode_has_no_minter() {
        assert_eq!(
            Minter::of(SecretMode::Managed, "crucible/registry/user:w/pr").unwrap(),
            None
        );
        assert_eq!(
            Minter::of(SecretMode::Minted, "mint://github-app").unwrap(),
            Some(Minter::GithubApp)
        );
    }
}
