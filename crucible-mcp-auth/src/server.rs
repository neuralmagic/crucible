//! Bearer-token validation and the MCP authorization challenges (MCP 2026-07-28, RFC 6750,
//! RFC 9728).
//!
//! | Outcome                         | Status | `WWW-Authenticate`                                   |
//! |---------------------------------|--------|------------------------------------------------------|
//! | no `Authorization: Bearer`      | 401    | `Bearer resource_metadata=".." , scope=".."`          |
//! | bad signature / iss / aud / exp | 401    | `Bearer error="invalid_token", resource_metadata=".."` |
//! | valid but missing a scope       | 403    | `Bearer error="insufficient_scope", scope=".."`       |

use crate::jwks::{Jwks, JwksError};
use crate::principal::{ClaimsError, Principal};
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http::{HeaderMap, HeaderValue, StatusCode, header};
use jsonwebtoken::{Algorithm, Validation};
use serde_json::{Map, Value};
use std::sync::Arc;
use url::Url;

/// Clock skew tolerated on `exp` and `nbf`.
const LEEWAY_SECS: u64 = 30;

#[derive(Debug, Clone)]
pub struct ResourceServerConfig {
    /// The authorization server's issuer identifier; tokens must carry exactly this `iss`.
    pub issuer: Url,
    /// Where to fetch the issuer's signing keys.
    pub jwks_uri: Url,
    /// This server's canonical URIs (RFC 8707 resource indicators), one per way clients reach
    /// it (Service DNS, Route). A token must name at least one in `aud`.
    pub resources: Vec<Url>,
    /// Scopes every request needs, before any per-tool requirement.
    pub required_scopes: Vec<String>,
    /// Advertised in the protected-resource metadata.
    pub scopes_supported: Vec<String>,
    pub resource_name: Option<String>,
}

pub struct ResourceServer {
    config: ResourceServerConfig,
    jwks: Jwks,
}

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("malformed token: {0}")]
    Malformed(jsonwebtoken::errors::Error),
    #[error("token header has no kid")]
    NoKid,
    #[error("algorithm {0:?} is not accepted")]
    Algorithm(Algorithm),
    #[error("token algorithm {token:?} differs from its key's {key:?}")]
    AlgorithmMismatch { token: Algorithm, key: Algorithm },
    #[error(transparent)]
    Key(#[from] JwksError),
    #[error("token rejected: {0}")]
    Rejected(jsonwebtoken::errors::Error),
    #[error(transparent)]
    Claims(#[from] ClaimsError),
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("no bearer token")]
    Missing,
    #[error("bearer token not recognized")]
    Unrecognized,
    #[error(transparent)]
    Invalid(#[from] TokenError),
    #[error("token lacks scope {}", missing.join(" "))]
    InsufficientScope { missing: Vec<String> },
}

const ACCEPTED_ALGORITHMS: &[Algorithm] = &[
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

impl ResourceServer {
    /// A server fetching keys from `config.jwks_uri` without following redirects.
    pub fn new(config: ResourceServerConfig) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let jwks = Jwks::new(http, config.jwks_uri.clone());
        Ok(Self { config, jwks })
    }

    pub fn with_jwks(config: ResourceServerConfig, jwks: Jwks) -> Self {
        Self { config, jwks }
    }

    pub fn config(&self) -> &ResourceServerConfig {
        &self.config
    }

    /// Validate a compact JWS access token and return its principal.
    pub async fn validate(&self, token: &str) -> Result<Principal, TokenError> {
        let header = jsonwebtoken::decode_header(token).map_err(TokenError::Malformed)?;
        if !ACCEPTED_ALGORITHMS.contains(&header.alg) {
            return Err(TokenError::Algorithm(header.alg));
        }
        let kid = header.kid.ok_or(TokenError::NoKid)?;
        let key = self.jwks.key(&kid).await?;
        if let Some(key_alg) = key.algorithm
            && key_alg != header.alg
        {
            return Err(TokenError::AlgorithmMismatch {
                token: header.alg,
                key: key_alg,
            });
        }
        let mut validation = Validation::new(header.alg);
        validation.leeway = LEEWAY_SECS;
        validation.validate_nbf = true;
        validation.set_issuer(&[self.config.issuer.as_str()]);
        let audiences: Vec<&str> = self.config.resources.iter().map(Url::as_str).collect();
        validation.set_audience(&audiences);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        let data = jsonwebtoken::decode::<Map<String, Value>>(token, &key.key, &validation)
            .map_err(TokenError::Rejected)?;
        Ok(Principal::from_claims(data.claims)?)
    }

    /// The bearer token's principal, with the server-wide scopes checked.
    pub async fn authenticate(&self, headers: &HeaderMap) -> Result<Principal, AuthError> {
        let token = bearer_token(headers).ok_or(AuthError::Missing)?;
        let principal = self.validate(token).await?;
        self.require_scopes(&principal, &self.config.required_scopes)?;
        Ok(principal)
    }

    pub fn require_scopes(
        &self,
        principal: &Principal,
        required: &[String],
    ) -> Result<(), AuthError> {
        let missing = principal.missing_scopes(required);
        if missing.is_empty() {
            return Ok(());
        }
        Err(AuthError::InsufficientScope {
            missing: missing.into_iter().map(str::to_string).collect(),
        })
    }

    /// The configured resource the request addressed, matched on its `Host` header. Never
    /// derived from the header itself, so a forged Host cannot steer clients elsewhere.
    pub fn resource_for(&self, headers: &HeaderMap) -> Option<&Url> {
        let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
        let by_host = host.and_then(|host| {
            self.config
                .resources
                .iter()
                .find(|r| authority(r).eq_ignore_ascii_case(host))
        });
        by_host.or_else(|| self.config.resources.first())
    }

    /// The RFC 9728 metadata URL for `resource`: the well-known path inserted before the
    /// resource's own path.
    pub fn metadata_url(resource: &Url) -> Url {
        let mut url = resource.clone();
        let path = resource.path().trim_end_matches('/');
        url.set_path(&format!("/.well-known/oauth-protected-resource{path}"));
        url.set_query(None);
        url.set_fragment(None);
        url
    }

    /// The 401/403 response for `error`, its challenge pointing at this server's metadata.
    pub fn challenge(&self, headers: &HeaderMap, error: &AuthError) -> Response {
        let metadata = self.resource_for(headers).map(Self::metadata_url);
        let mut params: Vec<(&str, String)> = Vec::new();
        let (status, code) = match error {
            AuthError::Missing => (StatusCode::UNAUTHORIZED, None),
            AuthError::Invalid(_) | AuthError::Unrecognized => {
                (StatusCode::UNAUTHORIZED, Some("invalid_token"))
            }
            AuthError::InsufficientScope { .. } => {
                (StatusCode::FORBIDDEN, Some("insufficient_scope"))
            }
        };
        if let Some(code) = code {
            params.push(("error", code.to_string()));
            params.push(("error_description", error.to_string()));
        }
        if let Some(url) = &metadata {
            params.push(("resource_metadata", url.to_string()));
        }
        let scope = match error {
            AuthError::InsufficientScope { missing } => missing.join(" "),
            _ => self.config.required_scopes.join(" "),
        };
        if !scope.is_empty() {
            params.push(("scope", scope));
        }
        let challenge = format!(
            "Bearer {}",
            params
                .iter()
                .map(|(k, v)| format!("{k}=\"{}\"", quoted(v)))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let body = serde_json::json!({
            "error": code.unwrap_or("unauthorized"),
            "error_description": error.to_string(),
        });
        let mut response = (status, axum::Json(body)).into_response();
        if let Ok(value) = HeaderValue::from_str(&challenge) {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, value);
        }
        response
    }
}

/// Middleware: refuse requests without a valid bearer token, otherwise put the [`Principal`] in
/// the request extensions, where rmcp hands it to tool handlers inside `http::request::Parts`.
pub async fn require_bearer(
    State(server): State<Arc<ResourceServer>>,
    mut req: Request,
    next: Next,
) -> Response {
    match server.authenticate(req.headers()).await {
        Ok(principal) => {
            tracing::debug!(sub = %principal.subject, presenter = principal.presenter(), "bearer accepted");
            req.extensions_mut().insert(principal);
            next.run(req).await
        }
        Err(error) => {
            tracing::info!(%error, "bearer refused");
            server.challenge(req.headers(), &error)
        }
    }
}

/// The token in `Authorization: Bearer <token>`, scheme matched case-insensitively.
pub fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

fn authority(url: &Url) -> String {
    match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        _ => String::new(),
    }
}

/// RFC 7230 quoted-string content: backslash-escape `"` and `\`, drop control characters.
fn quoted(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .flat_map(|c| match c {
            '"' | '\\' => vec!['\\', c],
            c => vec![c],
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::server::*;
    use crate::testing::{self, JwksServer, jwk, run_claims, sign};
    use axum::body::{Body, to_bytes};
    use jsonwebtoken::{EncodingKey, Header};
    use serde_json::json;
    use std::time::Duration;
    use tower::ServiceExt;

    const LONG: Duration = Duration::from_secs(600);

    fn bearer(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        h
    }

    async fn server_with(keys: Vec<Value>) -> (JwksServer, ResourceServer) {
        let jwks = JwksServer::start(keys).await;
        let rs = testing::resource_server(&jwks, LONG, Duration::ZERO);
        (jwks, rs)
    }

    fn with(claims: Value, edit: impl FnOnce(&mut Map<String, Value>)) -> Value {
        let mut claims = claims;
        edit(claims.as_object_mut().unwrap());
        claims
    }

    #[tokio::test]
    async fn a_realm_token_yields_the_delegated_principal() {
        let (_jwks, rs) = server_with(vec![jwk("a")]).await;
        let p = rs
            .authenticate(&bearer(&sign("a", &run_claims())))
            .await
            .unwrap();
        assert_eq!(p.subject, "weaton");
        assert_eq!(
            p.presenter(),
            "spiffe://pirate.crucible/openshell/sandbox/auth-spike"
        );
        assert_eq!(p.namespace.as_deref(), Some("crucible-kueue-e2e"));
        assert_eq!(p.claims["queues"], json!(["crucible-e2e"]));
    }

    #[tokio::test]
    async fn one_matching_audience_is_enough() {
        let (_jwks, rs) = server_with(vec![jwk("a")]).await;
        let claims = with(run_claims(), |c| {
            c.insert("aud".into(), json!(testing::RESOURCE_ROUTE));
        });
        rs.validate(&sign("a", &claims)).await.unwrap();
    }

    #[tokio::test]
    async fn tokens_failing_any_registered_claim_are_invalid() {
        let (_jwks, rs) = server_with(vec![jwk("a")]).await;
        let cases: Vec<(&str, Value)> = vec![
            (
                "other issuer",
                with(run_claims(), |c| {
                    c.insert("iss".into(), json!("https://evil.example/realms/crucible"));
                }),
            ),
            (
                "other audience",
                with(run_claims(), |c| {
                    c.insert("aud".into(), json!("https://other.example/mcp"));
                }),
            ),
            (
                "no audience",
                with(run_claims(), |c| {
                    c.remove("aud");
                }),
            ),
            (
                "expired",
                with(run_claims(), |c| {
                    c.insert("exp".into(), json!(testing::now() - 120));
                }),
            ),
            (
                "no exp",
                with(run_claims(), |c| {
                    c.remove("exp");
                }),
            ),
            (
                "not yet valid",
                with(run_claims(), |c| {
                    c.insert("nbf".into(), json!(testing::now() + 600));
                }),
            ),
            (
                "no sub",
                with(run_claims(), |c| {
                    c.remove("sub");
                }),
            ),
            (
                "bad act",
                with(run_claims(), |c| {
                    c.insert("act".into(), json!({"iss": "x"}));
                }),
            ),
        ];
        for (what, claims) in cases {
            let err = rs
                .authenticate(&bearer(&sign("a", &claims)))
                .await
                .unwrap_err();
            assert!(matches!(err, AuthError::Invalid(_)), "{what}: {err}");
        }
    }

    #[tokio::test]
    async fn a_token_signed_by_an_unpublished_key_is_refused() {
        let (_jwks, rs) = server_with(vec![jwk("a")]).await;
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("key-a".into());
        let forged =
            jsonwebtoken::encode(&header, &run_claims(), &testing::signing_key("b")).unwrap();
        let err = rs.validate(&forged).await.unwrap_err();
        assert!(matches!(err, TokenError::Rejected(_)), "{err}");
    }

    #[tokio::test]
    async fn symmetric_and_unsigned_algorithms_are_refused_before_key_lookup() {
        let (jwks, rs) = server_with(vec![jwk("a")]).await;
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("key-a".into());
        let n = jwk("a")["n"].as_str().unwrap().to_string();
        let confused = jsonwebtoken::encode(
            &header,
            &run_claims(),
            &EncodingKey::from_secret(n.as_bytes()),
        )
        .unwrap();
        assert!(matches!(
            rs.validate(&confused).await,
            Err(TokenError::Algorithm(Algorithm::HS256))
        ));
        let none = format!(
            "{}.{}.",
            b64(&json!({"alg": "none", "kid": "key-a"})),
            b64(&run_claims())
        );
        assert!(matches!(
            rs.validate(&none).await,
            Err(TokenError::Malformed(_))
        ));
        assert_eq!(jwks.fetches(), 0);
    }

    fn b64(v: &Value) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
    }

    #[tokio::test]
    async fn a_token_without_kid_is_refused() {
        let (_jwks, rs) = server_with(vec![jwk("a")]).await;
        let token = jsonwebtoken::encode(
            &Header::new(Algorithm::RS256),
            &run_claims(),
            &testing::signing_key("a"),
        )
        .unwrap();
        assert!(matches!(rs.validate(&token).await, Err(TokenError::NoKid)));
    }

    #[tokio::test]
    async fn a_key_pinned_to_one_algorithm_refuses_another() {
        let mut pinned = jwk("a");
        pinned["alg"] = json!("RS512");
        let (_jwks, rs) = server_with(vec![pinned]).await;
        let err = rs.validate(&sign("a", &run_claims())).await.unwrap_err();
        assert!(
            matches!(
                err,
                TokenError::AlgorithmMismatch {
                    token: Algorithm::RS256,
                    key: Algorithm::RS512
                }
            ),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_rotated_key_is_picked_up_on_first_sight() {
        let (jwks, rs) = server_with(vec![jwk("a")]).await;
        rs.validate(&sign("a", &run_claims())).await.unwrap();
        *jwks.keys.write().await = vec![jwk("a"), jwk("b")];
        rs.validate(&sign("b", &run_claims())).await.unwrap();
        assert_eq!(jwks.fetches(), 2);
        rs.validate(&sign("b", &run_claims())).await.unwrap();
        assert_eq!(jwks.fetches(), 2, "known kids are served from cache");
    }

    #[tokio::test]
    async fn unknown_kids_refetch_at_most_once_per_interval() {
        let jwks = JwksServer::start(vec![jwk("a")]).await;
        let rs = testing::resource_server(&jwks, LONG, Duration::from_secs(60));
        rs.validate(&sign("a", &run_claims())).await.unwrap();
        for _ in 0..5 {
            let err = rs.validate(&sign("b", &run_claims())).await.unwrap_err();
            assert!(
                matches!(err, TokenError::Key(JwksError::UnknownKid(_))),
                "{err}"
            );
        }
        assert_eq!(jwks.fetches(), 1);
    }

    #[tokio::test]
    async fn an_issuer_outage_keeps_cached_keys_and_fails_closed_on_new_ones() {
        let jwks = JwksServer::start(vec![jwk("a")]).await;
        let rs = testing::resource_server(&jwks, Duration::ZERO, Duration::ZERO);
        rs.validate(&sign("a", &run_claims())).await.unwrap();
        jwks.down.store(true, std::sync::atomic::Ordering::SeqCst);
        rs.validate(&sign("a", &run_claims())).await.unwrap();
        let err = rs.validate(&sign("b", &run_claims())).await.unwrap_err();
        assert!(
            matches!(err, TokenError::Key(JwksError::Fetch { .. })),
            "{err}"
        );
    }

    #[tokio::test]
    async fn server_wide_scopes_are_enforced() {
        let (_jwks, rs) = server_with(vec![jwk("a")]).await;
        let claims = with(run_claims(), |c| {
            c.insert("scope".into(), json!("kueue.submit"));
        });
        match rs.authenticate(&bearer(&sign("a", &claims))).await {
            Err(AuthError::InsufficientScope { missing }) => assert_eq!(missing, ["kueue.read"]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bearer_tokens_parse_case_insensitively_and_reject_other_schemes() {
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bearer abc"),
        );
        assert_eq!(bearer_token(&h), Some("abc"));
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        assert_eq!(bearer_token(&h), None);
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer "));
        assert_eq!(bearer_token(&h), None);
    }

    #[test]
    fn metadata_urls_insert_the_well_known_path_before_the_resource_path() {
        let cases = [
            (
                "https://kueue.apps.example/mcp",
                "https://kueue.apps.example/.well-known/oauth-protected-resource/mcp",
            ),
            (
                "http://h.svc:8850/mcp/",
                "http://h.svc:8850/.well-known/oauth-protected-resource/mcp",
            ),
            (
                "https://h/",
                "https://h/.well-known/oauth-protected-resource",
            ),
        ];
        for (resource, want) in cases {
            assert_eq!(
                ResourceServer::metadata_url(&Url::parse(resource).unwrap()).as_str(),
                want
            );
        }
    }

    #[tokio::test]
    async fn the_resource_is_chosen_by_host_and_never_taken_from_it() {
        let (_jwks, rs) = server_with(vec![]).await;
        let host = |h: &'static str| {
            let mut m = HeaderMap::new();
            m.insert(header::HOST, HeaderValue::from_static(h));
            m
        };
        assert_eq!(
            rs.resource_for(&host("kueue.apps.example"))
                .unwrap()
                .as_str(),
            testing::RESOURCE_ROUTE
        );
        assert_eq!(
            rs.resource_for(&host("KUEUE.crucible-system.svc.cluster.local:8850"))
                .unwrap()
                .as_str(),
            testing::RESOURCE_SVC
        );
        assert_eq!(
            rs.resource_for(&host("evil.example")).unwrap().as_str(),
            testing::RESOURCE_SVC
        );
    }

    async fn call(rs: Arc<ResourceServer>, headers: HeaderMap) -> Response {
        let app = axum::Router::new()
            .route(
                "/mcp",
                axum::routing::post(|req: Request| async move {
                    let p = req
                        .extensions()
                        .get::<crate::principal::Principal>()
                        .unwrap();
                    p.subject.clone()
                }),
            )
            .layer(axum::middleware::from_fn_with_state(rs, require_bearer));
        let mut req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .body(Body::empty())
            .unwrap();
        *req.headers_mut() = headers;
        app.oneshot(req).await.unwrap()
    }

    fn challenge(r: &Response) -> String {
        r.headers()
            .get(header::WWW_AUTHENTICATE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn the_middleware_challenges_per_mcp_authorization() {
        let (_jwks, rs) = server_with(vec![jwk("a")]).await;
        let rs = Arc::new(rs);
        let mut route = HeaderMap::new();
        route.insert(header::HOST, HeaderValue::from_static("kueue.apps.example"));

        let missing = call(rs.clone(), route.clone()).await;
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            challenge(&missing),
            r#"Bearer resource_metadata="https://kueue.apps.example/.well-known/oauth-protected-resource/mcp", scope="kueue.read""#
        );

        let mut bad = route.clone();
        bad.extend(bearer("not.a.jwt"));
        let invalid = call(rs.clone(), bad).await;
        assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED);
        assert!(
            challenge(&invalid).starts_with(r#"Bearer error="invalid_token", error_description=""#),
            "{}",
            challenge(&invalid)
        );

        let narrow = with(run_claims(), |c| {
            c.insert("scope".into(), json!("kueue.submit"));
        });
        let mut scoped = route.clone();
        scoped.extend(bearer(&sign("a", &narrow)));
        let forbidden = call(rs.clone(), scoped).await;
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
        assert!(challenge(&forbidden).contains(r#"error="insufficient_scope""#));
        assert!(
            challenge(&forbidden).ends_with(r#"scope="kueue.read""#),
            "{}",
            challenge(&forbidden)
        );

        let mut ok = route;
        ok.extend(bearer(&sign("a", &run_claims())));
        let accepted = call(rs, ok).await;
        assert_eq!(accepted.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(accepted.into_body(), 1024).await.unwrap(),
            "weaton"
        );
    }

    #[tokio::test]
    async fn protected_resource_metadata_is_served_at_both_well_known_forms() {
        let (_jwks, rs) = server_with(vec![]).await;
        let app = crate::metadata::router(Arc::new(rs));
        let get = |path: &'static str, host: &'static str| {
            Request::builder()
                .uri(path)
                .header(header::HOST, host)
                .body(Body::empty())
                .unwrap()
        };
        let body = |r: Response| async move {
            serde_json::from_slice::<Value>(&to_bytes(r.into_body(), 4096).await.unwrap()).unwrap()
        };
        let inserted = app
            .clone()
            .oneshot(get(
                "/.well-known/oauth-protected-resource/mcp",
                "kueue.apps.example",
            ))
            .await
            .unwrap();
        assert_eq!(
            body(inserted).await,
            json!({
                "resource": testing::RESOURCE_ROUTE,
                "authorization_servers": [testing::ISSUER],
                "scopes_supported": ["kueue.read", "kueue.submit"],
                "bearer_methods_supported": ["header"],
                "resource_name": "crucible-kueue",
            })
        );
        let root = app
            .clone()
            .oneshot(get(
                "/.well-known/oauth-protected-resource",
                "kueue.crucible-system.svc.cluster.local:8850",
            ))
            .await
            .unwrap();
        assert_eq!(body(root).await["resource"], testing::RESOURCE_SVC);
        let unknown = app
            .oneshot(get(
                "/.well-known/oauth-protected-resource/other",
                "kueue.apps.example",
            ))
            .await
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn challenge_values_are_quoted_strings() {
        assert_eq!(quoted(r#"a "b" \c"#), r#"a \"b\" \\c"#);
        assert_eq!(quoted("line\nbreak"), "linebreak");
    }
}
