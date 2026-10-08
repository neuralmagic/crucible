//! The authorization server's signing keys, fetched from its JWKS URI and cached by `kid`.
//!
//! A cached key is served until `max_age`; after that the next lookup refetches, and keeps the
//! old set if the fetch fails. An unknown `kid` (key rotation) refetches at most once per
//! `min_refetch`, so a flood of tokens with made-up kids cannot hammer the issuer.

use jsonwebtoken::jwk::{AlgorithmParameters, JwkSet, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum JwksError {
    #[error("no signing key with kid {0:?}")]
    UnknownKid(String),
    #[error("fetching JWKS from {uri}: {source}")]
    Fetch {
        uri: Url,
        #[source]
        source: reqwest::Error,
    },
}

/// One verification key. `algorithm` is the JWK's `alg` when it declares one; a token signed with
/// any other algorithm is refused.
#[derive(Clone)]
pub struct VerificationKey {
    pub key: DecodingKey,
    pub algorithm: Option<Algorithm>,
}

#[derive(Default)]
struct Cache {
    keys: HashMap<String, VerificationKey>,
    fetched_at: Option<Instant>,
}

pub struct Jwks {
    http: reqwest::Client,
    uri: Url,
    max_age: Duration,
    min_refetch: Duration,
    cache: RwLock<Cache>,
    fetching: Mutex<()>,
}

impl Jwks {
    pub fn new(http: reqwest::Client, uri: Url) -> Self {
        Self {
            http,
            uri,
            max_age: Duration::from_secs(600),
            min_refetch: Duration::from_secs(30),
            cache: RwLock::new(Cache::default()),
            fetching: Mutex::new(()),
        }
    }

    pub fn with_timing(mut self, max_age: Duration, min_refetch: Duration) -> Self {
        self.max_age = max_age;
        self.min_refetch = min_refetch;
        self
    }

    pub fn uri(&self) -> &Url {
        &self.uri
    }

    pub async fn key(&self, kid: &str) -> Result<VerificationKey, JwksError> {
        if let Some(key) = self.fresh(kid).await {
            return Ok(key);
        }
        let _fetching = self.fetching.lock().await;
        if let Some(key) = self.fresh(kid).await {
            return Ok(key);
        }
        let (stale, fetched_at) = {
            let cache = self.cache.read().await;
            (cache.keys.get(kid).cloned(), cache.fetched_at)
        };
        let may_fetch =
            stale.is_some() || fetched_at.is_none_or(|at| at.elapsed() >= self.min_refetch);
        if may_fetch {
            match self.fetch().await {
                Ok(keys) => {
                    let found = keys.get(kid).cloned();
                    *self.cache.write().await = Cache {
                        keys,
                        fetched_at: Some(Instant::now()),
                    };
                    if let Some(key) = found {
                        return Ok(key);
                    }
                }
                Err(error) if stale.is_some() => {
                    tracing::warn!(%error, kid, "JWKS refresh failed; keeping the cached key");
                }
                Err(error) => return Err(error),
            }
        }
        stale.ok_or_else(|| JwksError::UnknownKid(kid.to_string()))
    }

    async fn fresh(&self, kid: &str) -> Option<VerificationKey> {
        let cache = self.cache.read().await;
        let at = cache.fetched_at?;
        if at.elapsed() >= self.max_age {
            return None;
        }
        cache.keys.get(kid).cloned()
    }

    async fn fetch(&self) -> Result<HashMap<String, VerificationKey>, JwksError> {
        let wrap = |source| JwksError::Fetch {
            uri: self.uri.clone(),
            source,
        };
        let set: JwkSet = self
            .http
            .get(self.uri.clone())
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(wrap)?
            .json()
            .await
            .map_err(wrap)?;
        Ok(verification_keys(&set))
    }
}

/// The asymmetric signing keys in `set`, by `kid`. Symmetric (`oct`) keys, encryption keys, keys
/// without a `kid`, and keys jsonwebtoken cannot load are left out.
pub fn verification_keys(set: &JwkSet) -> HashMap<String, VerificationKey> {
    set.keys
        .iter()
        .filter(|jwk| !matches!(jwk.algorithm, AlgorithmParameters::OctetKey(_)))
        .filter(|jwk| !matches!(jwk.common.public_key_use, Some(PublicKeyUse::Encryption)))
        .filter_map(|jwk| {
            let kid = jwk.common.key_id.clone()?;
            let algorithm = match jwk.common.key_algorithm {
                Some(alg) => Some(Algorithm::try_from(alg).ok()?),
                None => None,
            };
            let key = DecodingKey::from_jwk(jwk).ok()?;
            Some((kid, VerificationKey { key, algorithm }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::jwks::*;
    use serde_json::json;

    fn set(v: serde_json::Value) -> JwkSet {
        serde_json::from_value(v).unwrap()
    }

    const N: &str = "sXchDaQebHnPiGvyDOAT4saGEUetSyo9MKLOoWFsueri23bOdgWp4Dy1WlUzewbgBHod5pcM9H95GQRV3JDXboIRROSBigeC5yjU1hGzHHyXss8UDprecbAYxknTcQkhslANGRUZmdTOQ5qTRsLAt6BTYuyvVRdhS8exSZEy_c4gs_7svlJJQ4H9_NxsiIoLwAEk7-Q3UXERGYw_75IDrGA84-lA_-Ct4eTlXHBIY2EaV7t7LjJaynVJCpkv4LKjTTAumiGUIuQhrNhZLuF_RJLqHpM2kgWFLU7-VTdL1VbC2tejvcI2BlMkEpk1BzBZI0KQB0GaDWFLN-aEAw3vRw";

    #[test]
    fn only_asymmetric_signing_keys_with_a_kid_are_kept() {
        let keys = verification_keys(&set(json!({"keys": [
            {"kty": "RSA", "kid": "rsa", "alg": "RS256", "use": "sig", "n": N, "e": "AQAB"},
            {"kty": "RSA", "kid": "rsa-any-alg", "n": N, "e": "AQAB"},
            {"kty": "RSA", "kid": "enc", "use": "enc", "alg": "RSA-OAEP", "n": N, "e": "AQAB"},
            {"kty": "RSA", "n": N, "e": "AQAB"},
            {"kty": "oct", "kid": "hmac", "alg": "HS256", "k": "c2VjcmV0"},
        ]})));
        let mut kids: Vec<_> = keys.keys().map(String::as_str).collect();
        kids.sort();
        assert_eq!(kids, ["rsa", "rsa-any-alg"]);
        assert_eq!(keys["rsa"].algorithm, Some(Algorithm::RS256));
        assert_eq!(keys["rsa-any-alg"].algorithm, None);
    }
}
