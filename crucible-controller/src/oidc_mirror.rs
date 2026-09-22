//! The read-only OIDC discovery mirror (hub-spoke pod dispatch, trust bootstrap): the hub
//! cluster's SA issuer is not publicly discoverable and its API is unreachable from the spoke,
//! so the spoke's authn webhook points its `discovery_url` here instead. The mirror is a STRICT
//! two-path proxy of the controller's own cluster API, fetched with its in-cluster SA:
//!
//!   * `GET /.well-known/openid-configuration` — the discovery document, with `jwks_uri`
//!     rewritten to the mirror's own external base URL. The `issuer` field is deliberately NOT
//!     rewritten: tokens carry `iss = https://kubernetes.default.svc` and the webhook validates
//!     the claim against its configured issuer string; `discovery_url` only decouples where the
//!     document is FETCHED from, never what the issuer IS.
//!   * `GET /openid/v1/jwks` — the public signing keys, passed through verbatim.
//!
//! Nothing else is proxied — the upstream paths are hardcoded constants, never derived from the
//! request, and any other path 404s. Mounted by [`crate::serve`] OUTSIDE the bearer layer
//! (public read-only, like `/metrics`). `CONTROLLER_EXTERNAL_URL` unset/empty disables the
//! mirror entirely: the routes stay mounted and answer 404.
//!
//! Successful upstream responses are cached for 60s so an internet scrape can't hammer the API
//! server; an upstream failure past the TTL serves 503 (spoke-side JWKS caching tolerates short
//! mirror outages).

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use std::sync::Arc;
use std::time::Duration;

/// The mirror's public discovery path — also the upstream path fetched from the cluster API.
pub(crate) const DISCOVERY_PATH: &str = "/.well-known/openid-configuration";
/// The mirror's public JWKS path — also the upstream path fetched from the cluster API.
pub(crate) const JWKS_PATH: &str = "/openid/v1/jwks";

/// How long a successful upstream response is served from cache.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// The external base URL knob: `CONTROLLER_EXTERNAL_URL`, trailing slashes trimmed.
/// `None` (unset or empty) disables the mirror.
pub(crate) fn external_url_from_env() -> Option<String> {
    normalize_external_url(std::env::var("CONTROLLER_EXTERNAL_URL").ok())
}

/// The pure half of [`external_url_from_env`]: trim trailing slashes, empty disables.
fn normalize_external_url(raw: Option<String>) -> Option<String> {
    raw.map(|u| u.trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty())
}

/// Fetching one of the two hardcoded paths from the upstream cluster API. The `&'static str`
/// path is the strictness boundary: an implementation can only ever be handed one of the two
/// module constants, never request-derived input.
pub(crate) trait DiscoveryUpstream: Send + Sync {
    fn fetch(&self, path: &'static str) -> crate::daemon::queue::BoxFuture<Result<String, String>>;
}

/// The production upstream: the controller's own in-cluster kube client (its SA authenticates
/// the request; `system:service-account-issuer-discovery` grants every SA these two GETs).
pub(crate) struct KubeUpstream(pub(crate) kube::Client);

impl DiscoveryUpstream for KubeUpstream {
    fn fetch(&self, path: &'static str) -> crate::daemon::queue::BoxFuture<Result<String, String>> {
        let client = self.0.clone();
        Box::pin(async move {
            let req = axum::http::Request::builder()
                .uri(path)
                .body(Vec::new())
                .map_err(|e| format!("building the upstream request: {e}"))?;
            client
                .request_text(req)
                .await
                .map_err(|e| format!("fetching {path} from the cluster API: {e}"))
        })
    }
}

/// The upstream used when no kube client could be built at startup: every fetch fails, so the
/// mirror serves 503 until the controller restarts with a reachable API (mirrors the ingest
/// drop-box's fail-closed posture).
pub(crate) struct UnavailableUpstream;

impl DiscoveryUpstream for UnavailableUpstream {
    fn fetch(
        &self,
        _path: &'static str,
    ) -> crate::daemon::queue::BoxFuture<Result<String, String>> {
        Box::pin(async { Err("no kube client for the discovery mirror".to_string()) })
    }
}

/// The enabled mirror: the upstream fetcher, the external base URL the discovery document is
/// rewritten to, and one cached body per endpoint (keyed by the two hardcoded paths). The cache's
/// `try_get_with` coalesces concurrent cold-cache requests into a single API-server call and never
/// caches a failed fetch, so an outage retries upstream rather than pinning a dead entry.
pub(crate) struct Mirror {
    upstream: Arc<dyn DiscoveryUpstream>,
    external_base: String,
    cache: moka::future::Cache<&'static str, String>,
}

impl Mirror {
    /// `external_base` is the mirror's own externally reachable base URL (scheme + host, no
    /// trailing slash — [`external_url_from_env`] normalizes).
    pub(crate) fn new(upstream: Arc<dyn DiscoveryUpstream>, external_base: String) -> Self {
        Mirror {
            upstream,
            external_base: external_base.trim_end_matches('/').to_string(),
            cache: moka::future::Cache::builder()
                .time_to_live(CACHE_TTL)
                .build(),
        }
    }

    /// Serve one endpoint: fresh cache wins, else fetch + transform + cache, else 503.
    async fn serve(&self, path: &'static str) -> Response {
        let upstream = self.upstream.clone();
        let external_base = self.external_base.clone();
        // `&'static str` error carries the client-facing 503 message; a failed init is not cached.
        let result: Result<String, Arc<&'static str>> = self
            .cache
            .try_get_with(path, async move {
                let fetched = upstream.fetch(path).await.map_err(|e| {
                    tracing::warn!(path, error = %e, "discovery mirror upstream fetch failed");
                    "upstream cluster API unavailable"
                })?;
                if path == DISCOVERY_PATH {
                    rewrite_discovery(&fetched, &external_base).map_err(|e| {
                        tracing::warn!(path, error = %e, "discovery mirror got an unusable upstream document");
                        "upstream discovery document unusable"
                    })
                } else {
                    Ok(fetched)
                }
            })
            .await;
        match result {
            Ok(body) => json_ok(body),
            Err(msg) => err(StatusCode::SERVICE_UNAVAILABLE, *msg),
        }
    }
}

fn json_ok(body: String) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "public, max-age=60"),
        ],
        body,
    )
        .into_response()
}

fn err(status: StatusCode, msg: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        format!(r#"{{"status":"error","error":"{msg}"}}"#),
    )
        .into_response()
}

/// Rewrite the discovery document's `jwks_uri` to `<external_base>/openid/v1/jwks`. The
/// `issuer` (and every other field) passes through untouched — see the module docs for why the
/// issuer must keep matching the tokens' `iss` claim. The Kubernetes discovery document carries
/// no other endpoint URLs; if one ever appears it is passed through, pointing at the (spoke-
/// unreachable) upstream rather than silently proxying a path the mirror doesn't serve.
fn rewrite_discovery(body: &str, external_base: &str) -> Result<String, String> {
    let mut doc: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("upstream document is not JSON: {e}"))?;
    let obj = doc
        .as_object_mut()
        .ok_or_else(|| "upstream document is not a JSON object".to_string())?;
    obj.insert(
        "jwks_uri".to_string(),
        serde_json::Value::String(format!("{external_base}{JWKS_PATH}")),
    );
    serde_json::to_string(&doc).map_err(|e| format!("re-serializing the document: {e}"))
}

/// The mirror router: exactly the two GET routes. `None` (no `CONTROLLER_EXTERNAL_URL`) keeps
/// the routes mounted but disabled — both answer 404, and nothing upstream is ever fetched.
pub(crate) fn router(mirror: Option<Mirror>) -> Router {
    let state = Arc::new(mirror);
    Router::new()
        .route(DISCOVERY_PATH, get(serve_discovery))
        .route(JWKS_PATH, get(serve_jwks))
        .with_state(state)
}

async fn serve_discovery(State(mirror): State<Arc<Option<Mirror>>>) -> Response {
    match mirror.as_ref() {
        Some(m) => m.serve(DISCOVERY_PATH).await,
        None => err(StatusCode::NOT_FOUND, "discovery mirror disabled"),
    }
}

async fn serve_jwks(State(mirror): State<Arc<Option<Mirror>>>) -> Response {
    match mirror.as_ref() {
        Some(m) => m.serve(JWKS_PATH).await,
        None => err(StatusCode::NOT_FOUND, "discovery mirror disabled"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    /// A canned in-process upstream (unit-test boundary for the network edge, counting fetches).
    struct CannedUpstream {
        discovery: Result<String, String>,
        jwks: Result<String, String>,
        fetches: AtomicUsize,
    }

    impl CannedUpstream {
        fn ok(discovery: &str, jwks: &str) -> Self {
            CannedUpstream {
                discovery: Ok(discovery.to_string()),
                jwks: Ok(jwks.to_string()),
                fetches: AtomicUsize::new(0),
            }
        }

        fn down() -> Self {
            CannedUpstream {
                discovery: Err("connection refused".to_string()),
                jwks: Err("connection refused".to_string()),
                fetches: AtomicUsize::new(0),
            }
        }
    }

    impl DiscoveryUpstream for CannedUpstream {
        fn fetch(
            &self,
            path: &'static str,
        ) -> crate::daemon::queue::BoxFuture<Result<String, String>> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            let resp = if path == DISCOVERY_PATH {
                self.discovery.clone()
            } else {
                self.jwks.clone()
            };
            Box::pin(async move { resp })
        }
    }

    /// The shape the Kubernetes API actually serves at /.well-known/openid-configuration.
    const K8S_DISCOVERY: &str = r#"{
        "issuer": "https://kubernetes.default.svc",
        "jwks_uri": "https://172.30.0.1:443/openid/v1/jwks",
        "response_types_supported": ["id_token"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"]
    }"#;

    const JWKS: &str = r#"{"keys":[{"kty":"RSA","kid":"abc","use":"sig"}]}"#;

    async fn get_path(app: Router, path: &str) -> (StatusCode, String) {
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, String::from_utf8(bytes.to_vec()).expect("utf8"))
    }

    fn enabled_router(upstream: Arc<CannedUpstream>) -> Router {
        router(Some(Mirror::new(
            upstream,
            "https://crucible-ext.example.com".to_string(),
        )))
    }

    #[test]
    fn rewrite_replaces_jwks_uri_and_preserves_issuer() {
        let out =
            rewrite_discovery(K8S_DISCOVERY, "https://crucible-ext.example.com").expect("rewrite");
        let doc: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert_eq!(
            doc["jwks_uri"],
            "https://crucible-ext.example.com/openid/v1/jwks"
        );
        assert_eq!(
            doc["issuer"], "https://kubernetes.default.svc",
            "the issuer must keep matching the tokens' iss claim"
        );
        assert_eq!(doc["response_types_supported"][0], "id_token");
        assert_eq!(doc["id_token_signing_alg_values_supported"][0], "RS256");
    }

    #[test]
    fn rewrite_rejects_non_json_and_non_object() {
        assert!(rewrite_discovery("not json", "https://x").is_err());
        assert!(rewrite_discovery("[1,2]", "https://x").is_err());
    }

    #[tokio::test]
    async fn discovery_serves_rewritten_document() {
        let app = enabled_router(Arc::new(CannedUpstream::ok(K8S_DISCOVERY, JWKS)));
        let (status, body) = get_path(app, DISCOVERY_PATH).await;
        assert_eq!(status, StatusCode::OK);
        let doc: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(
            doc["jwks_uri"],
            "https://crucible-ext.example.com/openid/v1/jwks"
        );
        assert_eq!(doc["issuer"], "https://kubernetes.default.svc");
    }

    #[tokio::test]
    async fn jwks_passes_through_verbatim() {
        let app = enabled_router(Arc::new(CannedUpstream::ok(K8S_DISCOVERY, JWKS)));
        let (status, body) = get_path(app, JWKS_PATH).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, JWKS);
    }

    #[tokio::test]
    async fn non_mirror_paths_404() {
        let upstream = Arc::new(CannedUpstream::ok(K8S_DISCOVERY, JWKS));
        for path in [
            "/.well-known/other",
            "/.well-known/openid-configuration/extra",
            "/openid/v1/other",
            "/openid/v1/jwks/extra",
            "/openid",
            "/api/v1/pods",
        ] {
            let app = enabled_router(upstream.clone());
            let (status, _) = get_path(app, path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "path {path} must 404");
        }
        assert_eq!(
            upstream.fetches.load(Ordering::SeqCst),
            0,
            "no non-mirror path may ever reach the upstream"
        );
    }

    #[tokio::test]
    async fn disabled_mirror_404s_both_endpoints() {
        for path in [DISCOVERY_PATH, JWKS_PATH] {
            let (status, _) = get_path(router(None), path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "disabled {path} must 404");
        }
    }

    #[tokio::test]
    async fn upstream_failure_serves_503() {
        for path in [DISCOVERY_PATH, JWKS_PATH] {
            let app = enabled_router(Arc::new(CannedUpstream::down()));
            let (status, _) = get_path(app, path).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        }
    }

    #[tokio::test]
    async fn garbage_discovery_document_serves_503() {
        let app = enabled_router(Arc::new(CannedUpstream::ok("not json", JWKS)));
        let (status, _) = get_path(app, DISCOVERY_PATH).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn repeat_requests_within_ttl_hit_the_cache() {
        let upstream = Arc::new(CannedUpstream::ok(K8S_DISCOVERY, JWKS));
        let mirror = Mirror::new(
            upstream.clone(),
            "https://crucible-ext.example.com".to_string(),
        );
        for _ in 0..3 {
            let resp = mirror.serve(DISCOVERY_PATH).await;
            assert_eq!(resp.status(), StatusCode::OK);
        }
        assert_eq!(
            upstream.fetches.load(Ordering::SeqCst),
            1,
            "one upstream fetch serves every request inside the TTL"
        );
    }

    #[tokio::test]
    async fn upstream_failure_is_not_cached() {
        let upstream = Arc::new(CannedUpstream::down());
        let mirror = Mirror::new(upstream.clone(), "https://x".to_string());
        for _ in 0..2 {
            let resp = mirror.serve(JWKS_PATH).await;
            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
        assert_eq!(
            upstream.fetches.load(Ordering::SeqCst),
            2,
            "failures retry upstream instead of pinning a dead cache"
        );
    }

    #[test]
    fn external_url_normalizes_and_gates() {
        let norm = |s: &str| normalize_external_url(Some(s.to_string()));
        assert_eq!(
            norm("https://x.example.com/"),
            Some("https://x.example.com".to_string())
        );
        assert_eq!(norm(""), None, "empty disables the mirror");
        assert_eq!(norm("///"), None, "slash-only is empty after trimming");
        assert_eq!(normalize_external_url(None), None, "unset disables");
    }
}
