//! Cluster-token bearer auth: resolve a presented bearer to an OpenShift username.
//!
//! The bearer guard's second path (after the static `CONTROLLER_API_TOKEN`): a caller presents
//! their own cluster token (`oc whoami -t`) and this module asks the own-cluster API who that
//! token belongs to, via `GET /apis/user.openshift.io/v1/users/~` — the self-view every
//! authenticated OpenShift user can read. The call authenticates AS the presented token, so no
//! extra RBAC is granted to the controller's service account, and a revoked or expired token
//! stops working the moment the API server says so (modulo the short cache below).
//!
//! The returned name feeds the same `X-Auth-Request-User` identity the oauth2-proxy edge injects,
//! so `CONTROLLER_ADMINS` / `CONTROLLER_OPERATORS` match it with no new role plumbing. Unlike the
//! proxy path, any client-supplied identity headers are STRIPPED first — on this path the API
//! server is the authority, not the caller.
//!
//! Fail-closed: a token the API server rejects is 401, an API server we cannot reach is 503
//! (same split as the ingest drop-box's TokenReview) — never a pass-through.

#![allow(clippy::disallowed_macros)]

use std::time::Duration;

/// How long a resolved (token → username) is trusted before the API server is asked again.
/// Short on purpose: this is the revocation window.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// How long a definitive rejection is remembered, so a client retry-looping on a dead token
/// costs one API call per TTL instead of one per request.
const NEGATIVE_CACHE_TTL: Duration = Duration::from_secs(30);

/// Cache size bound; at the cap, expired entries are dropped and new ones are not cached.
const CACHE_MAX: usize = 4096;

/// Why a token did not resolve to a user. `Unauthorized` is definitive (401 to the caller);
/// `Unavailable` means the API server could not be asked (503, fail closed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LookupError {
    Unauthorized,
    Unavailable(String),
}

#[derive(Clone)]
enum Cached {
    User(String),
    Rejected,
}

/// Per-entry expiry: an accepted user lives [`CACHE_TTL`] (the revocation window), a rejection only
/// [`NEGATIVE_CACHE_TTL`] so a token that becomes valid isn't held rejected for long.
struct VerdictExpiry;

impl moka::Expiry<[u8; 32], Cached> for VerdictExpiry {
    fn expire_after_create(
        &self,
        _key: &[u8; 32],
        value: &Cached,
        _created_at: std::time::Instant,
    ) -> Option<Duration> {
        Some(Self::ttl(value))
    }

    // lookup() re-inserts a verdict onto an already-present (expired) entry, so moka takes the
    // UPDATE path. The default clears the per-entry expiration, which would hold a revoked token
    // valid past its window; keep the same per-variant TTL so the revocation window still applies.
    fn expire_after_update(
        &self,
        _key: &[u8; 32],
        value: &Cached,
        _updated_at: std::time::Instant,
        _duration_until_expiry: Option<Duration>,
    ) -> Option<Duration> {
        Some(Self::ttl(value))
    }
}

impl VerdictExpiry {
    fn ttl(value: &Cached) -> Duration {
        match value {
            Cached::User(_) => CACHE_TTL,
            Cached::Rejected => NEGATIVE_CACHE_TTL,
        }
    }
}

pub(crate) struct KubeUserAuth {
    http: reqwest::Client,
    /// The `users/~` URL, fully formed.
    url: String,
    /// sha256(token) → verdict. Raw tokens never sit in memory beyond the request that carried them.
    cache: moka::sync::Cache<[u8; 32], Cached>,
}

/// The in-cluster API server, resolvable from any pod without env plumbing.
const IN_CLUSTER_API: &str = "https://kubernetes.default.svc";
/// The mounted CA that signs the in-cluster API endpoint's cert.
const IN_CLUSTER_CA: &str = "/var/run/secrets/kubernetes.io/serviceaccount/ca.crt";

impl KubeUserAuth {
    /// Build from the environment, `None` when the feature is off (`CONTROLLER_KUBE_USER_AUTH`
    /// unset/falsy). `CONTROLLER_KUBE_USER_AUTH_URL` / `_CA` override the in-cluster defaults —
    /// the URL override is what the tests point at a local server.
    pub(crate) fn from_env() -> anyhow::Result<Option<Self>> {
        let enabled = std::env::var("CONTROLLER_KUBE_USER_AUTH")
            .is_ok_and(|v| matches!(v.trim(), "1" | "true" | "yes"));
        if !enabled {
            return Ok(None);
        }
        let base = std::env::var("CONTROLLER_KUBE_USER_AUTH_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| IN_CLUSTER_API.to_string());
        let ca = std::env::var("CONTROLLER_KUBE_USER_AUTH_CA")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| IN_CLUSTER_CA.to_string());
        Ok(Some(Self::new(&base, std::path::Path::new(&ca))?))
    }

    /// `ca` is loaded when it exists; absent is fine for a plain-http test URL or a public CA.
    pub(crate) fn new(base: &str, ca: &std::path::Path) -> anyhow::Result<Self> {
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(10));
        if ca.exists() {
            let pem =
                std::fs::read(ca).map_err(|e| anyhow::anyhow!("reading {}: {e}", ca.display()))?;
            builder = builder.add_root_certificate(reqwest::Certificate::from_pem(&pem)?);
        }
        Ok(KubeUserAuth {
            http: builder.build()?,
            url: format!(
                "{}/apis/user.openshift.io/v1/users/~",
                base.trim_end_matches('/')
            ),
            cache: moka::sync::Cache::builder()
                .max_capacity(CACHE_MAX as u64)
                .expire_after(VerdictExpiry)
                .build(),
        })
    }

    /// Resolve a bearer to the username the API server vouches for.
    pub(crate) async fn lookup(&self, token: &str) -> Result<String, LookupError> {
        let key: [u8; 32] = {
            use sha2::Digest;
            sha2::Sha256::digest(token.as_bytes()).into()
        };
        if let Some(hit) = self.cached(&key) {
            return hit;
        }
        let verdict = self.ask(token).await;
        match &verdict {
            Ok(user) => self.remember(key, Cached::User(user.clone())),
            Err(LookupError::Unauthorized) => self.remember(key, Cached::Rejected),
            // An unreachable API server is a moment, not a fact about the token.
            Err(LookupError::Unavailable(_)) => {}
        }
        verdict
    }

    fn cached(&self, key: &[u8; 32]) -> Option<Result<String, LookupError>> {
        match self.cache.get(key)? {
            Cached::User(u) => Some(Ok(u)),
            Cached::Rejected => Some(Err(LookupError::Unauthorized)),
        }
    }

    fn remember(&self, key: [u8; 32], verdict: Cached) {
        self.cache.insert(key, verdict);
    }

    async fn ask(&self, token: &str) -> Result<String, LookupError> {
        let resp = self
            .http
            .get(&self.url)
            .bearer_auth(token)
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| LookupError::Unavailable(format!("GET users/~: {e}")))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(LookupError::Unauthorized);
        }
        if !status.is_success() {
            // A 404 here means no user.openshift.io API (vanilla k8s) — misconfiguration, and one
            // that must not quietly turn into "every token is invalid".
            return Err(LookupError::Unavailable(format!(
                "users/~ returned {status}"
            )));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| LookupError::Unavailable(format!("parsing users/~: {e}")))?;
        let name = body
            .get("metadata")
            .and_then(|m| m.get("name"))
            .and_then(|n| n.as_str())
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .ok_or_else(|| LookupError::Unavailable("users/~ answered without a name".into()))?;
        Ok(name.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A real HTTP server standing in for the API server: known tokens resolve, everything else
    /// 401s, and it counts hits so the cache tests measure actual traffic.
    async fn api_server(hits: Arc<AtomicUsize>) -> String {
        async fn users_me(
            State(hits): State<Arc<AtomicUsize>>,
            headers: HeaderMap,
        ) -> (StatusCode, Json<serde_json::Value>) {
            hits.fetch_add(1, Ordering::SeqCst);
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            match auth.strip_prefix("Bearer ") {
                Some("sha256~good") => (
                    StatusCode::OK,
                    Json(serde_json::json!({"metadata": {"name": "wynn"}})),
                ),
                Some("sha256~nameless") => (StatusCode::OK, Json(serde_json::json!({}))),
                _ => (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({"kind": "Status", "code": 401})),
                ),
            }
        }
        let app = axum::Router::new()
            .route("/apis/user.openshift.io/v1/users/~", get(users_me))
            .with_state(hits);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        format!("http://{addr}")
    }

    fn auth_against(url: &str) -> KubeUserAuth {
        KubeUserAuth::new(url, std::path::Path::new("/nonexistent-ca")).expect("client")
    }

    #[tokio::test]
    async fn a_valid_token_resolves_to_the_api_servers_name() {
        let hits = Arc::new(AtomicUsize::new(0));
        let auth = auth_against(&api_server(hits).await);
        assert_eq!(auth.lookup("sha256~good").await.expect("user"), "wynn");
    }

    #[tokio::test]
    async fn a_rejected_token_is_unauthorized_not_unavailable() {
        let hits = Arc::new(AtomicUsize::new(0));
        let auth = auth_against(&api_server(hits).await);
        assert_eq!(
            auth.lookup("sha256~stolen").await.expect_err("rejected"),
            LookupError::Unauthorized
        );
    }

    #[tokio::test]
    async fn repeat_lookups_within_the_ttl_hit_the_cache() {
        let hits = Arc::new(AtomicUsize::new(0));
        let auth = auth_against(&api_server(hits.clone()).await);
        for _ in 0..5 {
            assert_eq!(auth.lookup("sha256~good").await.expect("user"), "wynn");
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1, "one real call, four hits");
    }

    #[tokio::test]
    async fn rejections_are_negative_cached() {
        let hits = Arc::new(AtomicUsize::new(0));
        let auth = auth_against(&api_server(hits.clone()).await);
        for _ in 0..5 {
            assert!(auth.lookup("sha256~stolen").await.is_err());
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn distinct_tokens_do_not_share_cache_entries() {
        let hits = Arc::new(AtomicUsize::new(0));
        let auth = auth_against(&api_server(hits.clone()).await);
        assert!(auth.lookup("sha256~good").await.is_ok());
        assert!(auth.lookup("sha256~stolen").await.is_err());
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    /// vanilla k8s (no user API) or a proxy mangling the body must fail closed as UNAVAILABLE —
    /// treating it as "invalid token" would 401 every caller and read like a credential problem.
    #[tokio::test]
    async fn a_nameless_answer_is_unavailable() {
        let hits = Arc::new(AtomicUsize::new(0));
        let auth = auth_against(&api_server(hits).await);
        assert!(matches!(
            auth.lookup("sha256~nameless").await.expect_err("no name"),
            LookupError::Unavailable(_)
        ));
    }

    #[tokio::test]
    async fn an_unreachable_api_server_is_unavailable_and_uncached() {
        // A port nothing listens on: connection refused, immediately.
        let auth = auth_against("http://127.0.0.1:9");
        for _ in 0..2 {
            assert!(matches!(
                auth.lookup("sha256~good").await.expect_err("unreachable"),
                LookupError::Unavailable(_)
            ));
        }
        auth.cache.run_pending_tasks();
        assert_eq!(
            auth.cache.entry_count(),
            0,
            "an outage verdict must not stick to the token"
        );
    }

    #[test]
    fn from_env_is_off_by_default() {
        // Env-dependent construction is covered here only for the off case; the on case needs
        // process-global env and is exercised by the live deployment.
        if std::env::var("CONTROLLER_KUBE_USER_AUTH").is_err() {
            assert!(KubeUserAuth::from_env().expect("builds").is_none());
        }
    }

    // lookup()'s re-insert on an expired entry goes through moka's UPDATE path; the default there
    // clears the per-entry TTL, so a revoked token would authenticate forever. Both paths must
    // return the same per-variant duration.
    #[test]
    fn the_update_path_keeps_the_per_variant_ttl() {
        use moka::Expiry;
        let now = std::time::Instant::now();
        let key = [0u8; 32];
        for (verdict, ttl) in [
            (Cached::User("wynn".into()), CACHE_TTL),
            (Cached::Rejected, NEGATIVE_CACHE_TTL),
        ] {
            assert_eq!(
                VerdictExpiry.expire_after_create(&key, &verdict, now),
                Some(ttl),
            );
            assert_eq!(
                VerdictExpiry.expire_after_update(&key, &verdict, now, Some(ttl)),
                Some(ttl),
                "re-insert must not clear the expiration"
            );
        }
    }
}
