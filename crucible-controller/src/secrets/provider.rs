//! Where a bound secret's bytes come from, as an injectable dependency.
//!
//! Dispatch needs values, not a Vault client. Naming that as a provider keeps the custody rule of
//! ADR-0028 (the hub is the only Vault client) inside one implementation, lets a deployment without
//! Vault refuse cleanly instead of half-working, and lets a dispatch test run without a Vault.

use crate::secrets::store::SecretRow;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Why a provider could not answer for a secret.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("no value for {name} at {path}")]
    NotFound { name: String, path: String },
    #[error("reading {name}: {source}")]
    Backend {
        name: String,
        #[source]
        source: anyhow::Error,
    },
    #[error("{name} needs {backend}, which this deployment has not configured")]
    NoBackend { name: String, backend: &'static str },
}

/// Reads the current value of a registered secret. Implementations hold whatever credential their
/// backend needs; callers hold only this.
///
/// `Debug` is required and must name the provider without naming anything it read: a launch is
/// formatted into logs, and a provider that printed its cache would put values in them.
#[async_trait::async_trait]
pub trait SecretProvider: Send + Sync + std::fmt::Debug {
    async fn value_of(&self, row: &SecretRow) -> Result<String, ProviderError>;
}

/// The production provider: stored modes read from Vault, minted ones are issued here.
pub struct RegistryProvider {
    vault: Option<Arc<crate::secrets::vault::VaultClient>>,
    github_app: Option<crate::secrets::github_app::GithubAppTokenSource>,
}

impl RegistryProvider {
    /// `None` when this deployment can back neither mode.
    pub fn installed(
        vault: Option<Arc<crate::secrets::vault::VaultClient>>,
        github_app: Option<crate::secrets::github_app::GithubAppTokenSource>,
    ) -> Option<Arc<dyn SecretProvider>> {
        if vault.is_none() && github_app.is_none() {
            return None;
        }
        Some(Arc::new(Self { vault, github_app }))
    }

    /// Issue a minted secret's bytes: one mint for this dispatch, never the controller's own.
    async fn mint(
        &self,
        row: &SecretRow,
        minter: crate::secrets::minter::Minter,
    ) -> Result<String, ProviderError> {
        match minter {
            crate::secrets::minter::Minter::GithubApp => {
                let app = self
                    .github_app
                    .as_ref()
                    .ok_or_else(|| ProviderError::NoBackend {
                        name: row.name.to_string(),
                        backend: "a configured GitHub App",
                    })?;
                app.token_for_one_consumer()
                    .await
                    .map_err(|source| ProviderError::Backend {
                        name: row.name.to_string(),
                        source,
                    })
            }
        }
    }
}

/// Names its backends, never anything it read.
impl std::fmt::Debug for RegistryProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryProvider")
            .field("vault", &self.vault.is_some())
            .field("github_app", &self.github_app.is_some())
            .finish()
    }
}

#[async_trait::async_trait]
impl SecretProvider for RegistryProvider {
    async fn value_of(&self, row: &SecretRow) -> Result<String, ProviderError> {
        let minter =
            crate::secrets::minter::Minter::of(row.mode, &row.vault_path).map_err(|source| {
                ProviderError::Backend {
                    name: row.name.to_string(),
                    source: source.into(),
                }
            })?;
        if let Some(minter) = minter {
            return self.mint(row, minter).await;
        }
        let vault = self
            .vault
            .as_ref()
            .ok_or_else(|| ProviderError::NoBackend {
                name: row.name.to_string(),
                backend: "a Vault client",
            })?;
        crate::secrets::read::current_value(vault, row)
            .await
            .map(|(value, _version)| value)
            .map_err(|source| ProviderError::Backend {
                name: row.name.to_string(),
                source: source.into(),
            })
    }
}

/// A provider over a fixed map, keyed by the secret's registered name. For tests, and for a local
/// run whose values come from the operator's own environment rather than a backend.
#[derive(Default)]
pub struct MapProvider(BTreeMap<String, String>);

/// The names it can answer for, never their values.
impl std::fmt::Debug for MapProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("MapProvider")
            .field(&self.0.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl MapProvider {
    pub fn new(values: impl IntoIterator<Item = (String, String)>) -> Self {
        Self(values.into_iter().collect())
    }
}

#[async_trait::async_trait]
impl SecretProvider for MapProvider {
    async fn value_of(&self, row: &SecretRow) -> Result<String, ProviderError> {
        self.0
            .get(row.name.as_str())
            .cloned()
            .ok_or_else(|| ProviderError::NotFound {
                name: row.name.to_string(),
                path: row.vault_path.clone(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::SecretName;

    fn row(name: &str) -> SecretRow {
        SecretRow {
            id: "id".to_string(),
            name: SecretName::parse(name).unwrap(),
            owner: crate::authz::model::Principal::parse("user:wynn").unwrap(),
            kind: crate::secrets::SecretKind::Opaque,
            visibility: crate::secrets::Visibility::AgentVisible,
            consumer: crate::secrets::ConsumerClass::Run,
            mode: crate::secrets::SecretMode::Managed,
            vault_path: format!("crucible/data/user/wynn/{name}"),
            current_version: Some(1),
            created_by: None,
            created_at: "2026-08-29T00:00:00Z".to_string(),
            updated_at: "2026-08-29T00:00:00Z".to_string(),
        }
    }

    #[tokio::test]
    async fn a_map_provider_answers_by_registered_name() {
        let provider = MapProvider::new([("pr_token".to_string(), "t".to_string())]);
        assert_eq!(provider.value_of(&row("pr_token")).await.unwrap(), "t");
    }

    #[tokio::test]
    async fn a_missing_value_names_the_secret_and_not_a_guess() {
        let provider = MapProvider::default();
        let err = provider.value_of(&row("pr_token")).await.unwrap_err();
        assert!(matches!(err, ProviderError::NotFound { .. }), "{err:?}");
        assert!(err.to_string().contains("pr_token"));
    }

    /// A provider is formatted into launch logs, so its `Debug` must be a name and nothing else.
    #[tokio::test]
    async fn debug_names_the_provider_without_its_values() {
        let provider = MapProvider::new([("pr_token".to_string(), "super-secret".to_string())]);
        assert!(!format!("{provider:?}").contains("super-secret"));
    }

    fn minted_row(name: &str) -> SecretRow {
        SecretRow {
            mode: crate::secrets::SecretMode::Minted,
            vault_path: crate::secrets::minter::Minter::GithubApp.uri(),
            current_version: None,
            ..row(name)
        }
    }

    /// The provider holds no Vault client, so a read that went looking for one could not pass.
    #[tokio::test]
    async fn a_minted_secret_is_issued_by_the_app_and_never_read_from_vault() {
        let dir = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/app/installations/99/access_tokens",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "token": "ghs_for_this_run",
                    "expires_at": jiff::Timestamp::from_second(
                        jiff::Timestamp::now().as_second() + 3600,
                    )
                    .unwrap()
                    .to_string(),
                })),
            )
            .mount(&server)
            .await;
        let app = crate::secrets::github_app::GithubAppTokenSource::new(
            "4210340",
            "99",
            crate::secrets::github_app::testkey::write_test_key(dir.path()),
            server.uri(),
        );
        let provider = RegistryProvider::installed(None, Some(app)).expect("an app is a backend");

        assert_eq!(
            provider.value_of(&minted_row("pr_token")).await.unwrap(),
            "ghs_for_this_run"
        );
    }

    /// Two runs must not share a credential, and must not be served the controller's cached one.
    #[tokio::test]
    async fn two_dispatches_are_issued_their_own_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;
        let in_an_hour =
            jiff::Timestamp::from_second(jiff::Timestamp::now().as_second() + 3600).unwrap();
        // Two mints, two different tokens, in order.
        for token in ["ghs_run_one", "ghs_run_two"] {
            wiremock::Mock::given(wiremock::matchers::method("POST"))
                .and(wiremock::matchers::path(
                    "/app/installations/99/access_tokens",
                ))
                .respond_with(wiremock::ResponseTemplate::new(201).set_body_json(
                    serde_json::json!({
                        "token": token,
                        "expires_at": in_an_hour.to_string(),
                    }),
                ))
                .up_to_n_times(1)
                .expect(1)
                .mount(&server)
                .await;
        }
        let app = crate::secrets::github_app::GithubAppTokenSource::new(
            "4210340",
            "99",
            crate::secrets::github_app::testkey::write_test_key(dir.path()),
            server.uri(),
        );
        // Warm the shared cache the way a pack-PR open would.
        assert_eq!(app.token().await.unwrap(), "ghs_run_one");

        let provider = RegistryProvider::installed(None, Some(app)).expect("an app is a backend");
        assert_eq!(
            provider.value_of(&minted_row("pr_token")).await.unwrap(),
            "ghs_run_two",
            "a dispatch must mint its own, not serve the controller's cached token"
        );
    }

    /// A deploy with no App must name what is missing, not hand the run an empty credential.
    #[tokio::test]
    async fn a_minted_secret_without_its_minter_refuses_by_name() {
        let provider = RegistryProvider {
            vault: None,
            github_app: None,
        };
        let err = provider
            .value_of(&minted_row("pr_token"))
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::NoBackend { .. }), "{err:?}");
        assert!(err.to_string().contains("pr_token"), "{err}");
        assert!(err.to_string().contains("GitHub App"), "{err}");
    }

    #[tokio::test]
    async fn a_deployment_with_neither_backend_installs_no_provider() {
        assert!(RegistryProvider::installed(None, None).is_none());
    }

    /// The `Debug` a launch logs names its backends, not what they hold.
    #[tokio::test]
    async fn the_registry_provider_debug_names_its_backends() {
        let provider = RegistryProvider {
            vault: None,
            github_app: None,
        };
        assert_eq!(
            format!("{provider:?}"),
            "RegistryProvider { vault: false, github_app: false }"
        );
    }
}
