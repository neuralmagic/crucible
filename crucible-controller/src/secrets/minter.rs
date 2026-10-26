//! What issues a [`SecretMode::Minted`] secret's bytes. The row names its minter as a `mint://`
//! URI in `vault_path`, the same column a reference spells a `vault://` URL in.

use crate::secrets::SecretMode;
use crate::secrets::github_app::{GithubOwner, InvalidOwner};

/// The URI scheme a minted secret's source is spelled with.
const SCHEME: &str = "mint://";

const GITHUB_APP: &str = "github-app";

/// A source that issues a fresh credential per dispatch instead of storing one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Minter {
    /// An installation token for the controller's GitHub App: `ghs_…`, one hour. `None` is the
    /// home installation; an owner is the App's installation on that account.
    GithubApp(Option<GithubOwner>),
}

/// Why a stored or requested minter could not be resolved.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MinterError {
    #[error("{0} is not a minter uri (expected mint://<minter>)")]
    NotAMinterUri(String),
    #[error("no minter named {0}")]
    Unknown(String),
    #[error(transparent)]
    Owner(#[from] InvalidOwner),
}

impl Minter {
    /// The name a registration asks for: `github-app`, or `github-app/<owner>`.
    pub fn name(&self) -> String {
        match self {
            Minter::GithubApp(None) => GITHUB_APP.to_string(),
            Minter::GithubApp(Some(owner)) => format!("{GITHUB_APP}/{owner}"),
        }
    }

    /// What the row stores.
    pub fn uri(&self) -> String {
        format!("{SCHEME}{}", self.name())
    }

    /// Resolve the name a registration named.
    pub fn parse_name(raw: &str) -> Result<Self, MinterError> {
        let raw = raw.trim();
        let (minter, owner) = match raw.split_once('/') {
            Some((minter, owner)) => (minter, Some(owner)),
            None => (raw, None),
        };
        match minter {
            GITHUB_APP => Ok(Minter::GithubApp(
                owner.map(GithubOwner::parse).transpose()?,
            )),
            _ => Err(MinterError::Unknown(raw.to_string())),
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

/// Whether any of a run's bound secrets is a GitHub App token, for any installation. An
/// unparseable source is nobody's; the provider has already refused it by the time this runs.
pub fn any_github_app(rows: &[crate::secrets::store::SecretRow]) -> bool {
    rows.iter().any(|row| {
        matches!(
            Minter::of(row.mode, &row.vault_path),
            Ok(Some(Minter::GithubApp(_)))
        )
    })
}

#[cfg(test)]
mod tests {
    use crate::authz::model::Principal;
    use crate::secrets::minter::*;
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

    fn owner(login: &str) -> GithubOwner {
        GithubOwner::parse(login).unwrap()
    }

    #[test]
    fn a_run_pushes_as_the_app_when_any_of_its_secrets_is_an_app_token() {
        let stored = row(SecretMode::Managed, "crucible/registry/user:wynn/pr-token");
        let home = row(SecretMode::Minted, "mint://github-app");
        let other = row(
            SecretMode::Minted,
            "mint://github-app/red-hat-data-services",
        );
        assert!(!any_github_app(&[]));
        assert!(!any_github_app(std::slice::from_ref(&stored)));
        assert!(any_github_app(&[stored.clone(), home]));
        assert!(any_github_app(&[stored, other]));
    }

    /// A row whose source does not parse belongs to no minter.
    #[test]
    fn an_unreadable_source_is_not_credited_to_a_minter() {
        assert!(!any_github_app(&[row(
            SecretMode::Minted,
            "mint://nonesuch"
        )]));
        assert!(!any_github_app(&[row(
            SecretMode::Minted,
            "mint://github-app/not a login"
        )]));
    }

    #[test]
    fn a_minter_round_trips_through_the_uri_a_row_stores() {
        let home = Minter::GithubApp(None);
        assert_eq!(home.uri(), "mint://github-app");
        assert_eq!(Minter::parse_uri("mint://github-app").unwrap(), home);

        let other = Minter::GithubApp(Some(owner("red-hat-data-services")));
        assert_eq!(other.uri(), "mint://github-app/red-hat-data-services");
        assert_eq!(
            Minter::parse_uri("mint://github-app/red-hat-data-services").unwrap(),
            other
        );
    }

    /// GitHub logins are case-insensitive, so the stored form is lowercase.
    #[test]
    fn an_owner_is_stored_lowercase() {
        assert_eq!(
            Minter::parse_name("github-app/Red-Hat-Data-Services")
                .unwrap()
                .uri(),
            "mint://github-app/red-hat-data-services"
        );
    }

    #[test]
    fn a_malformed_owner_is_refused() {
        for bad in ["", "-lead", "trail-", "a/b", "has space", &"x".repeat(40)] {
            let err = Minter::parse_name(&format!("github-app/{bad}")).unwrap_err();
            assert!(matches!(err, MinterError::Owner(_)), "{bad:?}: {err:?}");
        }
        assert!(Minter::parse_name(&format!("github-app/{}", "x".repeat(39))).is_ok());
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
        assert_eq!(
            Minter::parse_name("gitlab-app/acme").unwrap_err(),
            MinterError::Unknown("gitlab-app/acme".to_string())
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
            Some(Minter::GithubApp(None))
        );
    }
}
