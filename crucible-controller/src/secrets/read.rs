//! Read a registered secret's current bytes with the hub's own Vault identity.
//!
//! Both storage modes end here: a managed secret is a KV read under the registry's own mount, a
//! reference is a read of the path its owner granted the hub. The version is resolved at read time,
//! so a rotation reaches the next reader with nothing to invalidate.
//!
//! Every consumer of a `hub` secret goes through this — the run Secret's delivery and the
//! personal dispatch-target resolver — so there is one place a credential leaves Vault.

use crate::secrets::store::SecretRow;
use crate::secrets::vault::{VaultClient, VaultError, VaultPath};
use crate::secrets::{SecretMode, VALUE_KEY};

/// Why a secret's bytes could not be read. Every variant names the secret, because the caller
/// reports it to whoever registered it.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("secret {name} has an unusable {what}: {detail}")]
    Unusable {
        name: String,
        what: &'static str,
        detail: String,
    },
    #[error("reading secret {name}: {source}")]
    Vault {
        name: String,
        #[source]
        source: VaultError,
    },
    #[error("secret {name} has no {key} key in Vault")]
    Missing { name: String, key: String },
    #[error("secret {name} is minted at dispatch and has no stored bytes to read")]
    NotStored { name: String },
}

/// The secret's current value and the KV version it was read at.
pub async fn current_value(
    vault: &VaultClient,
    secret: &SecretRow,
) -> Result<(String, u64), ReadError> {
    let name = secret.name.to_string();
    match secret.mode {
        SecretMode::Managed => {
            let path = VaultPath::parse(&secret.vault_path).map_err(|e| ReadError::Unusable {
                name: name.clone(),
                what: "path",
                detail: e.to_string(),
            })?;
            let read = vault
                .get(&path, None)
                .await
                .map_err(|source| ReadError::Vault {
                    name: name.clone(),
                    source,
                })?;
            let value = read.data.get(VALUE_KEY).ok_or_else(|| ReadError::Missing {
                name: name.clone(),
                key: VALUE_KEY.to_string(),
            })?;
            Ok((value.to_string(), read.version.get()))
        }
        SecretMode::Reference => {
            let reference =
                vault
                    .reference(&secret.vault_path)
                    .map_err(|e| ReadError::Unusable {
                        name: name.clone(),
                        what: "reference",
                        detail: e.to_string(),
                    })?;
            let read = vault
                .read_reference_as_hub(&reference)
                .await
                .map_err(|source| ReadError::Vault {
                    name: name.clone(),
                    source,
                })?;
            let value = read
                .data
                .get(reference.key())
                .ok_or_else(|| ReadError::Missing {
                    name: name.clone(),
                    key: reference.key().to_string(),
                })?;
            Ok((value.to_string(), read.version.get()))
        }
        // The provider mints these; reaching Vault for one is asking the wrong backend.
        SecretMode::Minted => Err(ReadError::NotStored { name }),
    }
}
