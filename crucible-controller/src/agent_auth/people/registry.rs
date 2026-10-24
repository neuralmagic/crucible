//! The clients people may sign in with: the operator's registry and, when enabled, metadata
//! documents and dynamic registrations, resolved from a request's `client_id`.

use crate::agent_auth::people::cimd::{Cimd, CimdError};
use crate::agent_auth::people::clients::{Client, ClientId, ClientIdError, DynamicId};
use crate::agent_auth::people::dynamic::Dcr;
use std::sync::Arc;

/// Why a client is not one people may sign in with.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ClientError {
    #[error(transparent)]
    Id(#[from] ClientIdError),
    #[error("client_id {0:?} is not registered with this controller")]
    Unknown(String),
    #[error("client_id {0:?} is a metadata document URL, and this controller accepts none")]
    MetadataOff(String),
    #[error(transparent)]
    Metadata(#[from] CimdError),
    #[error("client_id {0:?} is not a live registration; register the client again")]
    Expired(String),
    #[error("reading client registrations: {0}")]
    Store(String),
}

/// The registry and, when enabled, the metadata document resolver and dynamic registrations.
pub struct Clients {
    registered: Vec<Client>,
    cimd: Option<Cimd>,
    dynamic: Option<Arc<Dcr>>,
}

impl Clients {
    pub fn new(registered: Vec<Client>, cimd: Option<Cimd>, dynamic: Option<Arc<Dcr>>) -> Self {
        Self {
            registered,
            cimd,
            dynamic,
        }
    }

    pub fn metadata_documents(&self) -> bool {
        self.cimd.is_some()
    }

    pub fn dynamic(&self) -> Option<Arc<Dcr>> {
        self.dynamic.clone()
    }

    async fn registration(&self, raw: &str, id: &DynamicId) -> Result<Client, ClientError> {
        let dcr = self
            .dynamic
            .as_ref()
            .ok_or_else(|| ClientError::Expired(raw.to_string()))?;
        dcr.client(id, crate::clock::unix_secs())
            .await
            .map_err(|e| ClientError::Store(format!("{e:#}")))?
            .ok_or_else(|| ClientError::Expired(raw.to_string()))
    }

    fn registered(&self, id: &ClientId) -> Option<&Client> {
        self.registered.iter().find(|c| &c.id == id)
    }

    /// The client an authorization request names, fetching its metadata document when it is one.
    pub async fn resolve(&self, raw: &str) -> Result<Client, ClientError> {
        let id = ClientId::parse(raw)?;
        match &id {
            ClientId::Registered(_) => self
                .registered(&id)
                .cloned()
                .ok_or_else(|| ClientError::Unknown(raw.to_string())),
            ClientId::Metadata(url) => {
                let cimd = self
                    .cimd
                    .as_ref()
                    .ok_or_else(|| ClientError::MetadataOff(raw.to_string()))?;
                Ok(cimd.client(url).await?)
            }
            ClientId::Dynamic(dynamic) => self.registration(raw, dynamic).await,
        }
    }

    /// The client a token request names, when it is still one people may use. Nothing is fetched:
    /// the grant the request redeems is bound to the client it was issued to. A registration's use
    /// is stamped.
    pub async fn admits(&self, raw: &str) -> Result<ClientId, ClientError> {
        let id = ClientId::parse(raw)?;
        match &id {
            ClientId::Registered(_) if self.registered(&id).is_none() => {
                Err(ClientError::Unknown(raw.to_string()))
            }
            ClientId::Registered(_) => Ok(id),
            ClientId::Metadata(url) => {
                let cimd = self
                    .cimd
                    .as_ref()
                    .ok_or_else(|| ClientError::MetadataOff(raw.to_string()))?;
                cimd.admits(url)?;
                Ok(id)
            }
            ClientId::Dynamic(dynamic) => {
                self.registration(raw, dynamic).await?;
                Ok(id)
            }
        }
    }

    /// Whether `id` may still hold a token for `resource`. A metadata document client may hold one
    /// for any configured audience.
    pub fn allows_resource(&self, id: &ClientId, resource: &str) -> bool {
        match id {
            ClientId::Registered(_) => self
                .registered(id)
                .is_some_and(|c| c.allows_resource(resource)),
            ClientId::Metadata(_) | ClientId::Dynamic(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::agent_auth::people::clients::{CODEX, PI, builtin};
    use crate::agent_auth::people::registry::*;

    #[tokio::test]
    async fn without_metadata_documents_a_url_client_is_refused() {
        let clients = Clients::new(builtin(), None, None);
        assert!(!clients.metadata_documents());
        assert_eq!(
            clients.resolve(CODEX).await.map(|c| c.name),
            Ok("Codex".into())
        );
        let url = "https://chatgpt.com/oauth/codex/client.json";
        assert!(matches!(
            clients.resolve(url).await,
            Err(ClientError::MetadataOff(_))
        ));
        assert!(matches!(
            clients.admits(url).await,
            Err(ClientError::MetadataOff(_))
        ));
        assert!(matches!(
            clients.resolve("someone-else").await,
            Err(ClientError::Unknown(_))
        ));
        assert!(matches!(
            clients.admits("someone-else").await,
            Err(ClientError::Unknown(_))
        ));
        assert!(clients.admits(PI).await.is_ok());
        let issued = "dyn-AAAAAAAAAAAAAAAAAAAAAA";
        assert!(matches!(
            clients.resolve(issued).await,
            Err(ClientError::Expired(_))
        ));
        assert!(matches!(
            clients.admits(issued).await,
            Err(ClientError::Expired(_))
        ));
    }
}
