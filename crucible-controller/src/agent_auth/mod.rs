//! The controller as OAuth authorization server for sandboxed agents: RFC 6749 client
//! credentials authenticated by an RFC 7523 JWT-SVID assertion, exactly what OpenShell's provider
//! `token_grant` sends (no `client_id`; the SVID's `sub` identifies the sandbox).
//!
//! ```text
//!  run pod ──POST /api/pods/{pod}/sandboxes (pod-bound SA token)──▶ run_sandboxes
//!  supervisor ──POST /oauth/token, client_assertion=<JWT-SVID>──▶ verify SVID (SPIRE JWKS)
//!                                                                   resolve sandbox → live run
//!                                                    ◀── ES256 token: sub=launcher, act.sub=SVID
//!  MCP server ──GET /.well-known/jwks.json──▶ verifies the token
//! ```
//!
//! These routes sit outside the human auth stack: the token endpoint authenticates with the SVID
//! alone, and the JWKS and metadata documents are public.

pub mod bindings;
pub mod issuer;
#[cfg(test)]
mod testing;

use crate::agent_auth::issuer::{AgentGrant, AgentIssuer, AssertionError, GrantError, TenantScope};
use axum::Json;
use axum::extract::State;
use axum::extract::rejection::FormRejection;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use sqlx::PgPool;
use std::path::PathBuf;
use std::sync::Arc;

pub struct AgentAuth {
    pub issuer: AgentIssuer,
    pub pool: PgPool,
    /// The deploy profile whose `[kueue]` table scopes a run's tenant claims, read per request
    /// like the dispatch path reads it.
    pub deploy_profile: Option<PathBuf>,
}

impl AgentAuth {
    /// The issuer `CONTROLLER_AGENT_*` configures, `None` when `CONTROLLER_AGENT_ISSUER` is unset.
    pub fn from_env(
        pool: PgPool,
        deploy_profile: Option<PathBuf>,
    ) -> anyhow::Result<Option<Arc<Self>>> {
        use anyhow::Context;
        let Some(cfg) = issuer::AgentIssuerConfig::from_env()? else {
            return Ok(None);
        };
        let http = cfg.svid_http().context("building the SPIRE JWKS client")?;
        let jwks = crucible_mcp_auth::jwks::Jwks::new(http, cfg.svid_jwks_uri.clone());
        tracing::info!(
            issuer = cfg.issuer.as_str(),
            kid = cfg.signing_key.kid(),
            svid_issuer = cfg.svid_issuer,
            audiences = ?cfg.audiences.iter().map(|a| a.resource.as_str()).collect::<Vec<_>>(),
            "issuing agent access tokens"
        );
        Ok(Some(Arc::new(Self {
            issuer: AgentIssuer::new(cfg, jwks),
            pool,
            deploy_profile,
        })))
    }
}

pub fn router(state: Arc<AgentAuth>) -> axum::Router {
    axum::Router::new()
        .route(issuer::TOKEN_PATH, axum::routing::post(token))
        .route(issuer::JWKS_PATH, axum::routing::get(jwks))
        .route(issuer::METADATA_PATH, axum::routing::get(metadata))
        .with_state(state)
}

/// The token request, field for field what OpenShell's `token_grant` posts. Anything else
/// (`client_id`, `client_secret`, `resource`) is refused rather than ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenRequest {
    grant_type: String,
    client_assertion_type: String,
    client_assertion: String,
    audience: Option<String>,
    scope: Option<String>,
}

/// RFC 6749 §5.2 and RFC 8707 §2 token endpoint errors.
#[derive(Debug)]
enum OAuthError {
    InvalidRequest(String),
    InvalidClient(String),
    InvalidGrant(String),
    UnsupportedGrantType(String),
    InvalidScope(String),
    InvalidTarget(String),
    Server(String),
}

impl IntoResponse for OAuthError {
    fn into_response(self) -> Response {
        let (status, code, description) = match self {
            Self::InvalidRequest(d) => (StatusCode::BAD_REQUEST, "invalid_request", d),
            Self::InvalidClient(d) => (StatusCode::UNAUTHORIZED, "invalid_client", d),
            Self::InvalidGrant(d) => (StatusCode::BAD_REQUEST, "invalid_grant", d),
            Self::UnsupportedGrantType(d) => (StatusCode::BAD_REQUEST, "unsupported_grant_type", d),
            Self::InvalidScope(d) => (StatusCode::BAD_REQUEST, "invalid_scope", d),
            Self::InvalidTarget(d) => (StatusCode::BAD_REQUEST, "invalid_target", d),
            Self::Server(d) => (StatusCode::INTERNAL_SERVER_ERROR, "server_error", d),
        };
        no_store(
            (
                status,
                Json(serde_json::json!({"error": code, "error_description": description})),
            )
                .into_response(),
        )
    }
}

impl From<GrantError> for OAuthError {
    fn from(e: GrantError) -> Self {
        match e {
            GrantError::NoAudience { .. } | GrantError::Audience { .. } => {
                Self::InvalidTarget(e.to_string())
            }
            GrantError::Scope { .. } => Self::InvalidScope(e.to_string()),
            GrantError::NoLauncher { .. } | GrantError::NoKubeUser { .. } => {
                Self::InvalidGrant(e.to_string())
            }
        }
    }
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn token(
    State(state): State<Arc<AgentAuth>>,
    form: Result<axum::Form<TokenRequest>, FormRejection>,
) -> Response {
    match issue(&state, form).await {
        Ok(response) => response,
        Err(error) => {
            tracing::info!(?error, "agent token refused");
            error.into_response()
        }
    }
}

async fn issue(
    state: &AgentAuth,
    form: Result<axum::Form<TokenRequest>, FormRejection>,
) -> Result<Response, OAuthError> {
    let axum::Form(req) = form.map_err(|e| OAuthError::InvalidRequest(e.body_text()))?;
    if req.grant_type != "client_credentials" {
        return Err(OAuthError::UnsupportedGrantType(format!(
            "grant_type {:?} is not client_credentials",
            req.grant_type
        )));
    }
    if req.client_assertion_type != issuer::JWT_BEARER {
        return Err(OAuthError::InvalidClient(format!(
            "client_assertion_type {:?} is not {}",
            req.client_assertion_type,
            issuer::JWT_BEARER
        )));
    }
    let sandbox = state
        .issuer
        .sandbox(&req.client_assertion)
        .await
        .map_err(|e| match e {
            AssertionError::Token(crucible_mcp_auth::verify::TokenError::Key(
                crucible_mcp_auth::jwks::JwksError::Fetch { .. },
            )) => OAuthError::Server(format!("SPIRE keys unavailable: {e}")),
            e => OAuthError::InvalidClient(e.to_string()),
        })?;
    let audience = state.issuer.audience(req.audience.as_deref())?;
    let run = bindings::resolve(&state.pool, &sandbox.id)
        .await
        .map_err(|e| match e {
            bindings::ResolveError::Db(_) => OAuthError::Server(format!("{e:#}")),
            e => OAuthError::InvalidGrant(e.to_string()),
        })?;
    let launcher = issuer::launcher(&run)?;
    let kueue = crate::runs::workpod::kueue::load_cfg(state.deploy_profile.clone())
        .await
        .map_err(|e| OAuthError::Server(format!("reading the deploy profile: {e:#}")))?;
    let tenant = kueue
        .as_ref()
        .and_then(|k| TenantScope::for_run(k, &run.cluster));
    let scopes = issuer::granted_scopes(req.scope.as_deref(), audience, tenant.as_ref())?;
    let kube_user = crate::identity::kube_identity::effective(&state.pool, &launcher)
        .await
        .map_err(|e| OAuthError::Server(format!("{e:#}")))?;
    let required = kueue.as_ref().is_some_and(|k| k.require_kube_user);
    match (&kube_user, required) {
        (None, true) => {
            return Err(GrantError::NoKubeUser {
                launcher: launcher.clone(),
            }
            .into());
        }
        (None, false) => tracing::warn!(
            launcher,
            sandbox = %sandbox.id,
            reason = "launcher has no known kubernetes username",
            "agent token minted without kube_user; the run is limited by its claims alone"
        ),
        (Some(_), _) => {}
    }
    let grant = AgentGrant {
        sandbox,
        launcher,
        kube_user: kube_user.map(|k| k.as_str().to_string()),
        run,
        audience: audience.resource.clone(),
        scopes,
        tenant,
    };
    let minted = state
        .issuer
        .mint(&grant, crate::clock::unix_secs())
        .map_err(|e| OAuthError::Server(e.to_string()))?;
    tracing::info!(
        sandbox = %grant.sandbox.id,
        run_id = grant.run.run_id,
        sub = grant.launcher,
        kube_user = grant.kube_user.as_deref(),
        aud = grant.audience,
        scope = minted.scope,
        "agent token issued"
    );
    Ok(no_store(
        Json(serde_json::json!({
            "access_token": minted.access_token,
            "token_type": "Bearer",
            "expires_in": minted.expires_in,
            "scope": minted.scope,
        }))
        .into_response(),
    ))
}

async fn jwks(State(state): State<Arc<AgentAuth>>) -> Response {
    Json(state.issuer.jwks()).into_response()
}

async fn metadata(State(state): State<Arc<AgentAuth>>) -> Response {
    Json(state.issuer.metadata()).into_response()
}

#[cfg(test)]
mod tests {
    use crate::agent_auth::testing::{
        self, EcKey, ISSUER, KUEUE, PREFIX, PROFILE, Spire, now, running_run, serve_jwks,
        set_pod_state,
    };
    use crate::agent_auth::*;
    use crate::runs::workpod::WorkPodState;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    struct Harness {
        spire: Spire,
        app: axum::Router,
        state: Arc<AgentAuth>,
        _profile: tempfile::TempDir,
    }

    async fn harness(pool: PgPool) -> Harness {
        harness_with(pool, PROFILE).await
    }

    async fn harness_with(pool: PgPool, profile_toml: &str) -> Harness {
        let spire = Spire::start().await;
        let profile = tempfile::tempdir().unwrap();
        let path = profile.path().join("profile.toml");
        std::fs::write(&path, profile_toml).unwrap();
        let state = Arc::new(AgentAuth {
            issuer: testing::issuer(&spire, &EcKey::generate()),
            pool,
            deploy_profile: Some(path),
        });
        Harness {
            spire,
            app: router(state.clone()),
            state,
            _profile: profile,
        }
    }

    fn form(svid: &str, extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut fields = vec![
            ("grant_type".to_string(), "client_credentials".to_string()),
            (
                "client_assertion_type".to_string(),
                issuer::JWT_BEARER.to_string(),
            ),
            ("client_assertion".to_string(), svid.to_string()),
            ("audience".to_string(), KUEUE.to_string()),
        ];
        for (k, v) in extra {
            fields.retain(|(key, _)| key != k);
            if !v.is_empty() {
                fields.push((k.to_string(), v.to_string()));
            }
        }
        fields
    }

    async fn post(
        app: &axum::Router,
        fields: &[(String, String)],
    ) -> (StatusCode, Option<String>, Value) {
        let body = fields
            .iter()
            .map(|(k, v)| {
                format!(
                    "{k}={}",
                    percent_encoding::utf8_percent_encode(v, percent_encoding::NON_ALPHANUMERIC)
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        let req = axum::http::Request::post(issuer::TOKEN_PATH)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(axum::body::Body::from(body))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let cache = res
            .headers()
            .get(header::CACHE_CONTROL)
            .map(|v| v.to_str().unwrap().to_string());
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        (status, cache, serde_json::from_slice(&bytes).unwrap())
    }

    async fn bound(pool: &PgPool, sandbox: &str) {
        running_run(
            pool,
            "crucible-run-brave-otter",
            "run-1",
            Some("user:weaton"),
        )
        .await;
        bindings::bind(pool, "crucible-run-brave-otter", &testing::binding(sandbox))
            .await
            .unwrap();
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_bound_sandboxs_svid_is_exchanged_for_a_delegated_token(pool: PgPool) {
        bound(&pool, "sbx-1").await;
        let h = harness(pool).await;
        let svid = h.spire.svid(&Spire::claims("sbx-1"));
        let (status, cache, body) = post(
            &h.app,
            &form(&svid, &[("scope", "kueue.read kueue.submit")]),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(cache.as_deref(), Some("no-store"));
        assert_eq!(body["token_type"], "Bearer");
        assert_eq!(body["expires_in"], issuer::TOKEN_TTL_SECS);
        assert_eq!(body["scope"], "kueue.read kueue.submit");

        let jwks = serve_jwks(h.state.issuer.jwks()).await;
        let rs = crucible_mcp_auth::server::ResourceServer::with_jwks(
            crucible_mcp_auth::server::ResourceServerConfig {
                issuers: vec![crucible_mcp_auth::server::TrustedIssuer {
                    issuer: ISSUER.into(),
                    jwks_uri: jwks,
                }],
                resources: vec![url::Url::parse(KUEUE).unwrap()],
                required_scopes: vec!["kueue.read".into()],
                scopes_supported: Vec::new(),
                resource_name: None,
            },
            |t| crucible_mcp_auth::jwks::Jwks::new(reqwest::Client::new(), t.jwks_uri.clone()),
        );
        let principal = rs
            .validate(body["access_token"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(principal.subject, "weaton");
        assert_eq!(principal.presenter(), format!("{PREFIX}sbx-1"));
        assert_eq!(principal.namespace.as_deref(), Some("crucible-kueue-e2e"));
        assert_eq!(principal.claims["run_id"], "crucible-run-brave-otter");
        assert_eq!(principal.claims["queues"], json!(["crucible-e2e"]));
        assert_eq!(principal.claims["role"], "submitter");
        assert_eq!(principal.claims["sandbox"], "ci-sbx-1");
        assert_eq!(principal.claims["workdir"], "/sandbox/repo");
    }

    fn claims_of(body: &Value) -> Value {
        use base64::Engine;
        let payload = body["access_token"]
            .as_str()
            .unwrap()
            .split('.')
            .nth(1)
            .unwrap();
        serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(payload)
                .unwrap(),
        )
        .unwrap()
    }

    async fn link(pool: &PgPool, kube_user: &str) {
        crate::identity::oidc::users::record_login(
            pool,
            "sub-weaton",
            "weaton",
            None,
            jiff::Timestamp::now(),
        )
        .await
        .unwrap();
        crate::identity::kube_identity::learn(
            pool,
            "weaton",
            &crate::identity::kube_identity::KubeUser::parse(kube_user).unwrap(),
        )
        .await
        .unwrap();
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_launcher_with_a_known_kubernetes_username_gets_kube_user(pool: PgPool) {
        bound(&pool, "sbx-1").await;
        link(&pool, "wseaton").await;
        let h = harness(pool).await;
        let svid = h.spire.svid(&Spire::claims("sbx-1"));
        let (status, _, body) = post(&h.app, &form(&svid, &[])).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let claims = claims_of(&body);
        assert_eq!(
            (claims["sub"].as_str(), claims["kube_user"].as_str()),
            (Some("weaton"), Some("wseaton"))
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_unknown_kubernetes_username_is_omitted_or_refused_as_the_profile_says(
        pool: PgPool,
    ) {
        bound(&pool, "sbx-1").await;
        let lenient = harness(pool.clone()).await;
        let svid = lenient.spire.svid(&Spire::claims("sbx-1"));
        let (status, _, body) = post(&lenient.app, &form(&svid, &[])).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(claims_of(&body).get("kube_user").is_none());

        let strict = harness_with(
            pool.clone(),
            &format!("{PROFILE}require_kube_user = true\n"),
        )
        .await;
        let svid = strict.spire.svid(&Spire::claims("sbx-1"));
        let (status, _, body) = post(&strict.app, &form(&svid, &[])).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_grant")),
            "{body}"
        );
        assert!(
            body["error_description"]
                .as_str()
                .unwrap()
                .contains("kubernetes username"),
            "{body}"
        );

        link(&pool, "wseaton").await;
        let (status, _, body) = post(&strict.app, &form(&svid, &[])).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(claims_of(&body)["kube_user"], "wseaton");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_buildit_token_carries_buildits_scopes_and_the_sandbox(pool: PgPool) {
        bound(&pool, "sbx-1").await;
        let h = harness(pool).await;
        let svid = h.spire.svid(&Spire::claims("sbx-1"));
        let (status, _, body) = post(&h.app, &form(&svid, &[("audience", testing::BUILDIT)])).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["scope"], "buildit.read buildit.build");
        let payload = body["access_token"]
            .as_str()
            .unwrap()
            .split('.')
            .nth(1)
            .unwrap();
        use base64::Engine;
        let claims: Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(payload)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(claims["aud"], testing::BUILDIT);
        assert_eq!(claims["sandbox"], "ci-sbx-1");
        assert_eq!(claims["workdir"], "/sandbox/repo");

        let (status, _, body) = post(
            &h.app,
            &form(
                &svid,
                &[("audience", testing::BUILDIT), ("scope", "kueue.read")],
            ),
        )
        .await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_scope")),
            "a kueue scope is not grantable for buildit"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn no_scope_requested_grants_what_the_role_allows(pool: PgPool) {
        bound(&pool, "sbx-1").await;
        let h = harness(pool).await;
        let svid = h.spire.svid(&Spire::claims("sbx-1"));
        let (status, _, body) = post(&h.app, &form(&svid, &[])).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["scope"], "kueue.read kueue.submit");
    }

    fn with(claims: Value, edit: impl FnOnce(&mut serde_json::Map<String, Value>)) -> Value {
        let mut claims = claims;
        edit(claims.as_object_mut().unwrap());
        claims
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn refusals_carry_the_oauth_error_code(pool: PgPool) {
        bound(&pool, "sbx-1").await;
        running_run(
            &pool,
            "crucible-run-team",
            "run-team",
            Some("team:platform-administrators"),
        )
        .await;
        bindings::bind(&pool, "crucible-run-team", &testing::binding("sbx-team"))
            .await
            .unwrap();
        let h = harness(pool).await;
        let good = h.spire.svid(&Spire::claims("sbx-1"));
        let svid = |edit: &dyn Fn(&mut serde_json::Map<String, Value>)| {
            h.spire.svid(&with(Spire::claims("sbx-1"), edit))
        };
        type Case = (
            &'static str,
            Vec<(String, String)>,
            StatusCode,
            &'static str,
        );
        let cases: Vec<Case> = vec![
            (
                "an SVID for another audience",
                form(
                    &svid(&|c| {
                        c.insert(
                            "aud".into(),
                            json!(["https://keycloak.example/realms/crucible"]),
                        );
                    }),
                    &[],
                ),
                StatusCode::UNAUTHORIZED,
                "invalid_client",
            ),
            (
                "an SVID from another trust domain",
                form(
                    &svid(&|c| {
                        c.insert(
                            "sub".into(),
                            json!("spiffe://evil.example/openshell/sandbox/sbx-1"),
                        );
                    }),
                    &[],
                ),
                StatusCode::UNAUTHORIZED,
                "invalid_client",
            ),
            (
                "an expired SVID",
                form(
                    &svid(&|c| {
                        c.insert("exp".into(), json!(now() - 120));
                    }),
                    &[],
                ),
                StatusCode::UNAUTHORIZED,
                "invalid_client",
            ),
            (
                "a forged SVID",
                form(
                    &testing::sign(&EcKey::generate(), "spire", &Spire::claims("sbx-1")),
                    &[],
                ),
                StatusCode::UNAUTHORIZED,
                "invalid_client",
            ),
            (
                "an unbound sandbox",
                form(&h.spire.svid(&Spire::claims("sbx-unbound")), &[]),
                StatusCode::BAD_REQUEST,
                "invalid_grant",
            ),
            (
                "a run with no person behind it",
                form(&h.spire.svid(&Spire::claims("sbx-team")), &[]),
                StatusCode::BAD_REQUEST,
                "invalid_grant",
            ),
            (
                "an audience off the allowlist",
                form(&good, &[("audience", "https://other-mcp.example/mcp")]),
                StatusCode::BAD_REQUEST,
                "invalid_target",
            ),
            (
                "no audience",
                form(&good, &[("audience", "")]),
                StatusCode::BAD_REQUEST,
                "invalid_target",
            ),
            (
                "a scope the role does not grant",
                form(&good, &[("scope", "kueue.read kueue.admin")]),
                StatusCode::BAD_REQUEST,
                "invalid_scope",
            ),
            (
                "another grant type",
                form(&good, &[("grant_type", "password")]),
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
            ),
            (
                "another assertion type",
                form(
                    &good,
                    &[(
                        "client_assertion_type",
                        "urn:ietf:params:oauth:client-assertion-type:jwt-spiffe",
                    )],
                ),
                StatusCode::UNAUTHORIZED,
                "invalid_client",
            ),
            (
                "a client_id",
                form(&good, &[("client_id", "run-1")]),
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
            (
                "no assertion",
                form(&good, &[("client_assertion", "")]),
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
        ];
        for (what, fields, want_status, want_error) in cases {
            let (status, cache, body) = post(&h.app, &fields).await;
            assert_eq!(
                (status, body["error"].as_str()),
                (want_status, Some(want_error)),
                "{what}: {body}"
            );
            assert_eq!(cache.as_deref(), Some("no-store"), "{what}");
            assert!(body.get("access_token").is_none(), "{what}");
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_collected_runs_sandbox_gets_no_token(pool: PgPool) {
        bound(&pool, "sbx-1").await;
        let h = harness(pool.clone()).await;
        let svid = h.spire.svid(&Spire::claims("sbx-1"));
        assert_eq!(post(&h.app, &form(&svid, &[])).await.0, StatusCode::OK);
        set_pod_state(&pool, "crucible-run-brave-otter", WorkPodState::Collected).await;
        let (status, _, body) = post(&h.app, &form(&svid, &[])).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_grant"))
        );
        assert!(
            body["error_description"]
                .as_str()
                .unwrap()
                .contains("collected"),
            "{body}"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn the_jwks_and_metadata_are_served(pool: PgPool) {
        let h = harness(pool).await;
        let get = |path: &'static str| {
            let app = h.app.clone();
            async move {
                let res = app
                    .oneshot(
                        axum::http::Request::get(path)
                            .body(axum::body::Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(res.status(), StatusCode::OK, "{path}");
                let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
                    .await
                    .unwrap();
                serde_json::from_slice::<Value>(&bytes).unwrap()
            }
        };
        assert_eq!(
            get(issuer::JWKS_PATH).await,
            serde_json::to_value(h.state.issuer.jwks()).unwrap()
        );
        assert_eq!(
            get(issuer::METADATA_PATH).await,
            serde_json::to_value(h.state.issuer.metadata()).unwrap()
        );
    }
}
