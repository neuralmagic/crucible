//! An issuer's signing keys, fetched from a [`KeySource`] (its JWKS URI over https, or anything
//! else that yields a JWK Set) and cached by `kid`.
//!
//! A cached key is served until `max_age`; after that the next lookup refetches, and keeps the
//! old set if the fetch fails. Any fetch, failed or not, waits `min_refetch` after the last
//! attempt, so a flood of tokens with made-up kids cannot hammer the issuer, and a response larger
//! than [`MAX_JWKS_BYTES`] is refused unread.

use jsonwebtoken::jwk::{AlgorithmParameters, JwkSet, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};
use url::Url;

/// Largest key set accepted.
pub const MAX_JWKS_BYTES: usize = 256 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum KeySourceError {
    #[error("the key set is larger than {limit} bytes")]
    TooLarge { limit: usize },
    #[error(transparent)]
    Fetch(Box<dyn std::error::Error + Send + Sync>),
}

pub type FetchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<u8>, KeySourceError>> + Send + 'a>>;

/// Where a JWK Set comes from. [`Jwks`] caches, throttles, size-checks and decodes what it
/// returns.
pub trait KeySource: Send + Sync {
    /// The key set's JSON. A source should stop reading past `limit` bytes and return
    /// [`KeySourceError::TooLarge`]; a longer body is refused either way.
    fn fetch(&self, limit: usize) -> FetchFuture<'_>;

    /// Where the keys come from, for errors and logs.
    fn describe(&self) -> String;
}

/// A JWKS URI fetched over HTTP.
pub struct HttpSource {
    http: reqwest::Client,
    uri: Url,
}

impl HttpSource {
    pub fn new(http: reqwest::Client, uri: Url) -> Self {
        Self { http, uri }
    }
}

impl KeySource for HttpSource {
    fn fetch(&self, limit: usize) -> FetchFuture<'_> {
        Box::pin(async move {
            let wrap = |e: reqwest::Error| KeySourceError::Fetch(Box::new(e));
            let mut response = self
                .http
                .get(self.uri.clone())
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .map_err(wrap)?;
            if response
                .content_length()
                .is_some_and(|n| usize::try_from(n).map_or(true, |n| n > limit))
            {
                return Err(KeySourceError::TooLarge { limit });
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(wrap)? {
                if body.len() + chunk.len() > limit {
                    return Err(KeySourceError::TooLarge { limit });
                }
                body.extend_from_slice(&chunk);
            }
            Ok(body)
        })
    }

    fn describe(&self) -> String {
        self.uri.to_string()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum JwksError {
    #[error("no signing key with kid {0:?}")]
    UnknownKid(String),
    #[error("fetching JWKS from {source_name}: {error}")]
    Fetch {
        source_name: String,
        #[source]
        error: KeySourceError,
    },
    #[error("decoding JWKS from {source_name}: {error}")]
    Decode {
        source_name: String,
        #[source]
        error: serde_json::Error,
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
    attempted_at: Option<Instant>,
}

pub struct Jwks {
    source: Arc<dyn KeySource>,
    max_age: Duration,
    min_refetch: Duration,
    cache: RwLock<Cache>,
    fetching: Mutex<()>,
}

impl Jwks {
    /// Keys from the JWKS at `uri`.
    pub fn new(http: reqwest::Client, uri: Url) -> Self {
        Self::from_source(Arc::new(HttpSource::new(http, uri)))
    }

    pub fn from_source(source: Arc<dyn KeySource>) -> Self {
        Self {
            source,
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

    pub async fn key(&self, kid: &str) -> Result<VerificationKey, JwksError> {
        if let Some(key) = self.fresh(kid).await {
            return Ok(key);
        }
        let _fetching = self.fetching.lock().await;
        if let Some(key) = self.fresh(kid).await {
            return Ok(key);
        }
        let (stale, attempted_at) = {
            let cache = self.cache.read().await;
            (cache.keys.get(kid).cloned(), cache.attempted_at)
        };
        if attempted_at.is_none_or(|at| at.elapsed() >= self.min_refetch) {
            let fetched = self.fetch().await;
            let mut cache = self.cache.write().await;
            cache.attempted_at = Some(Instant::now());
            match fetched {
                Ok(keys) => {
                    let found = keys.get(kid).cloned();
                    cache.keys = keys;
                    cache.fetched_at = cache.attempted_at;
                    return found.ok_or_else(|| JwksError::UnknownKid(kid.to_string()));
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
        let source_name = || self.source.describe();
        let fetched = self.source.fetch(MAX_JWKS_BYTES).await;
        let body = match fetched {
            Ok(body) if body.len() > MAX_JWKS_BYTES => Err(KeySourceError::TooLarge {
                limit: MAX_JWKS_BYTES,
            }),
            other => other,
        }
        .map_err(|error| JwksError::Fetch {
            source_name: source_name(),
            error,
        })?;
        let set: JwkSet = serde_json::from_slice(&body).map_err(|error| JwksError::Decode {
            source_name: source_name(),
            error,
        })?;
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

    async fn serve(
        body: Vec<u8>,
        hits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        status: u16,
    ) -> Url {
        let app = axum::Router::new().route(
            "/certs",
            axum::routing::get(move || {
                let body = body.clone();
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    (http::StatusCode::from_u16(status).unwrap(), body)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Url::parse(&format!("http://{addr}/certs")).unwrap()
    }

    #[tokio::test]
    async fn a_failed_fetch_also_waits_out_the_refetch_interval() {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let uri = serve(b"down".to_vec(), hits.clone(), 503).await;
        let jwks = Jwks::new(reqwest::Client::new(), uri)
            .with_timing(Duration::from_secs(600), Duration::from_secs(60));
        for kid in ["a", "b", "c", "d"] {
            assert!(jwks.key(kid).await.is_err());
        }
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_oversized_key_set_is_refused() {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let huge = format!(
            "{{\"keys\": [], \"pad\": \"{}\"}}",
            "x".repeat(MAX_JWKS_BYTES)
        );
        let uri = serve(huge.into_bytes(), hits, 200).await;
        let jwks = Jwks::new(reqwest::Client::new(), uri);
        assert!(matches!(
            jwks.key("a").await,
            Err(JwksError::Fetch {
                error: KeySourceError::TooLarge { .. },
                ..
            })
        ));
    }

    /// Serves `body` (or fails) from memory, counting fetches; ignores the size limit.
    struct Fixed {
        body: Option<Vec<u8>>,
        fetches: std::sync::atomic::AtomicUsize,
    }

    impl KeySource for Fixed {
        fn fetch(&self, _: usize) -> FetchFuture<'_> {
            self.fetches
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let body = self.body.clone();
            Box::pin(async move {
                body.ok_or_else(|| KeySourceError::Fetch("the proxy is unreachable".into()))
            })
        }

        fn describe(&self) -> String {
            "fixed".into()
        }
    }

    fn fixed(body: Option<Vec<u8>>) -> Arc<Fixed> {
        Arc::new(Fixed {
            body,
            fetches: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    #[tokio::test]
    async fn any_key_source_gets_the_same_cache_throttle_and_limits() {
        let key_set = json!({"keys": [crate::testing::jwk("a")]});
        let source = fixed(Some(serde_json::to_vec(&key_set).unwrap()));
        let jwks = Jwks::from_source(source.clone());
        assert!(jwks.key("key-a").await.is_ok());
        assert!(jwks.key("key-a").await.is_ok());
        assert!(matches!(
            jwks.key("made-up").await,
            Err(JwksError::UnknownKid(_))
        ));
        assert_eq!(
            source.fetches.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "cached keys and an unknown kid inside the refetch interval cost no fetch"
        );

        let failing = fixed(None);
        let Err(err) = Jwks::from_source(failing).key("key-a").await else {
            panic!("a failing source yields no key")
        };
        assert_eq!(
            err.to_string(),
            "fetching JWKS from fixed: the proxy is unreachable"
        );

        let huge = fixed(Some(vec![b' '; MAX_JWKS_BYTES + 1]));
        assert!(matches!(
            Jwks::from_source(huge).key("key-a").await,
            Err(JwksError::Fetch {
                error: KeySourceError::TooLarge { .. },
                ..
            })
        ));

        let garbage = fixed(Some(b"not json".to_vec()));
        assert!(matches!(
            Jwks::from_source(garbage).key("key-a").await,
            Err(JwksError::Decode { .. })
        ));
    }
}
