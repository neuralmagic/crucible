//! The controller as OAuth authorization server for sandboxed agents: RFC 6749 client
//! credentials authenticated by an RFC 7523 JWT-SVID assertion, exactly what OpenShell's provider
//! `token_grant` sends (no `client_id`; the SVID's `sub` identifies the sandbox).
//!
//! ```text
//!  run pod ──POST /api/sandbox-bindings/runs/{run} (the run's pod token)──▶ run_sandboxes
//!  supervisor ──POST /oauth/token, client_assertion=<JWT-SVID>──▶ verify SVID (its cluster's
//!                                                                   SPIRE, via the API server)
//!                                                                   resolve sandbox → live run
//!                                                                   on that same cluster
//!                                                    ◀── ES256 token: sub=launcher, act.sub=SVID
//!  MCP server ──GET /.well-known/jwks.json──▶ verifies the token
//! ```
//!
//! These routes sit outside the human auth stack: the token endpoint authenticates with the SVID
//! alone, and the JWKS and metadata documents are public.

pub mod bindings;
pub mod directory;
pub mod issuer;
pub mod people;
pub mod placement;
pub mod sandbox;
pub mod spire;
#[cfg(test)]
mod testing;

use crate::agent_auth::issuer::{AgentGrant, AgentIssuer, AssertionError, GrantError};
use axum::Json;
use axum::extract::State;
use axum::extract::rejection::FormRejection;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use sqlx::PgPool;
use std::sync::Arc;

pub struct AgentAuth {
    pub issuer: AgentIssuer,
    pub pool: PgPool,
    /// The deploy profile naming the sandbox namespace and, in `[kueue]`, a run's default
    /// namespace, read once at startup.
    pub profile: Arc<crucible::deploy::DeployProfile>,
    pub directory: Arc<dyn directory::SandboxDirectory>,
    /// The authorization code grant for people, when `CONTROLLER_PEOPLE_AUDIENCES` is set.
    pub people: Option<people::People>,
}

impl AgentAuth {
    /// The issuer `CONTROLLER_AGENT_*` configures, `None` when `CONTROLLER_AGENT_ISSUER` is unset.
    /// `people` builds the people grant from its configuration, over the controller's tables.
    pub async fn from_env(
        pool: PgPool,
        cfg: &crate::config::ControllerCfg,
        clusters: Arc<crate::runs::clusters::ClusterClients>,
        people: impl FnOnce(
            people::PeopleConfig,
            PgPool,
            &crucible::deploy::DeployProfile,
        ) -> anyhow::Result<people::People>,
    ) -> anyhow::Result<Option<Arc<Self>>> {
        use anyhow::Context;
        let Some(issuer_cfg) = issuer::AgentIssuerConfig::from_env()? else {
            return Ok(None);
        };
        let profile = cfg
            .profile()
            .await
            .context("reading the deploy profile for the agent issuer")?
            .ok_or(placement::PlacementError::NoProfile)?;
        let namespace = placement::controller_namespace();
        match issuer_cfg.spire.is_empty() {
            false => placement::check_key_placement(namespace.as_deref(), &profile)?,
            true => placement::check_key_exposure(namespace.as_deref(), &profile)?,
        }
        let make_people = people;
        let people = match people::PeopleConfig::from_env()? {
            None => None,
            Some(config) => {
                tracing::info!(
                    audiences = ?config.audiences.iter().map(|a| (a.resource.as_str(), a.cluster.as_str())).collect::<Vec<_>>(),
                    clients = ?config.clients.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
                    cimd_hosts = ?config.cimd.as_ref().map(|c| c.hosts.as_slice()),
                    dynamic_registration = config.dcr.is_some(),
                    access_ttl_secs = config.access_ttl_secs,
                    "issuing access tokens to people"
                );
                let people = make_people(config, pool.clone(), &profile)
                    .context("building the people grant")?;
                people::grants::spawn_sweep(people.grants.clone(), people.clients.dynamic());
                Some(people)
            }
        };
        if issuer_cfg.spire.is_empty() && people.is_none() {
            return Err(issuer::ConfigError::NoRecipients.into());
        }
        tracing::info!(
            issuer = issuer_cfg.issuer.as_str(),
            kid = issuer_cfg.signing_key.kid(),
            spire = ?issuer_cfg.spire.iter().map(|s| (s.cluster.as_str(), s.svid_issuer.as_str())).collect::<Vec<_>>(),
            audiences = ?issuer_cfg.audiences.iter().map(|a| a.resource.as_str()).collect::<Vec<_>>(),
            "issuing agent access tokens"
        );
        let keys = |entry: &issuer::ClusterSpire| {
            crucible_mcp_auth::jwks::Jwks::from_source(Arc::new(spire::ProxyKeys::new(
                clusters.clone(),
                entry,
            )))
        };
        Ok(Some(Arc::new(Self {
            issuer: AgentIssuer::new(issuer_cfg, keys),
            pool,
            profile,
            directory: Arc::new(directory::KubeDirectory(clusters)),
            people,
        })))
    }
}

impl AgentAuth {
    /// What the sandbox bind route needs from the issuer's configuration.
    pub(crate) fn bind_state(&self, db: crate::Db) -> bindings::BindState {
        bindings::BindState {
            db,
            directory: self.directory.clone(),
            sandbox_namespace: placement::sandbox_namespace(Some(&self.profile))
                .map(ToString::to_string)
                .map_err(|e| e.to_string()),
        }
    }
}

pub fn router(state: Arc<AgentAuth>) -> axum::Router {
    axum::Router::new()
        .route(issuer::TOKEN_PATH, axum::routing::post(token))
        .route(issuer::JWKS_PATH, axum::routing::get(jwks))
        .route(issuer::METADATA_PATH, axum::routing::get(metadata))
        .route(
            issuer::REGISTER_PATH,
            axum::routing::post(people::dynamic::register),
        )
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

/// RFC 6749 §5.2 and RFC 8707 §2 token endpoint errors. The detail each carries is logged, never
/// returned: a caller holding a stolen or forged assertion learns only the error code.
#[derive(Debug)]
enum OAuthError {
    InvalidRequest(String),
    InvalidClient(String),
    InvalidGrant(String),
    UnauthorizedClient(String),
    UnsupportedGrantType(String),
    InvalidScope(String),
    InvalidTarget(String),
    Server(String),
}

impl OAuthError {
    fn detail(&self) -> &str {
        match self {
            Self::InvalidRequest(d)
            | Self::InvalidClient(d)
            | Self::InvalidGrant(d)
            | Self::UnauthorizedClient(d)
            | Self::UnsupportedGrantType(d)
            | Self::InvalidScope(d)
            | Self::InvalidTarget(d)
            | Self::Server(d) => d,
        }
    }
}

impl IntoResponse for OAuthError {
    fn into_response(self) -> Response {
        let (status, code, description) = match self {
            Self::InvalidRequest(_) => (
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "the token request is malformed",
            ),
            Self::InvalidClient(_) => (
                StatusCode::UNAUTHORIZED,
                "invalid_client",
                "the client assertion was not accepted",
            ),
            Self::InvalidGrant(_) => (
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "no token can be issued for this sandbox",
            ),
            Self::UnauthorizedClient(_) => (
                StatusCode::BAD_REQUEST,
                "unauthorized_client",
                "this deployment issues no agent tokens",
            ),
            Self::UnsupportedGrantType(_) => (
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
                "only client_credentials is supported",
            ),
            Self::InvalidScope(_) => (
                StatusCode::BAD_REQUEST,
                "invalid_scope",
                "a requested scope is not available",
            ),
            Self::InvalidTarget(_) => (
                StatusCode::BAD_REQUEST,
                "invalid_target",
                "the requested audience is not available",
            ),
            Self::Server(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "the token could not be issued",
            ),
        };
        oauth_error(status, code, description)
    }
}

/// An RFC 6749 §5.2 error body, never cached.
pub(crate) fn oauth_error(status: StatusCode, code: &str, description: &str) -> Response {
    no_store(
        (
            status,
            Json(serde_json::json!({"error": code, "error_description": description})),
        )
            .into_response(),
    )
}

impl From<GrantError> for OAuthError {
    fn from(e: GrantError) -> Self {
        match e {
            GrantError::NoAudience { .. } | GrantError::Audience { .. } => {
                Self::InvalidTarget(e.to_string())
            }
            GrantError::Scope { .. } => Self::InvalidScope(e.to_string()),
            GrantError::NoLauncher { .. }
            | GrantError::NoKubeUser { .. }
            | GrantError::WrongCluster { .. } => Self::InvalidGrant(e.to_string()),
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
    headers: axum::http::HeaderMap,
    form: Result<axum::Form<Vec<(String, String)>>, FormRejection>,
) -> Response {
    let fields = match form
        .map_err(|e| e.body_text())
        .and_then(|axum::Form(pairs)| people::single_valued(pairs))
    {
        Ok(fields) => fields,
        Err(why) => {
            tracing::info!(reason = why, "token request refused");
            return OAuthError::InvalidRequest(why).into_response();
        }
    };
    let grant_type = fields.get("grant_type").and_then(serde_json::Value::as_str);
    let people_grant = match (grant_type, state.people.as_ref()) {
        (Some("authorization_code"), Some(p)) => Some((p, true)),
        (Some("refresh_token"), Some(p)) => Some((p, false)),
        _ => None,
    };
    if let Some((people, code)) = people_grant {
        let now = crate::clock::unix_secs();
        let result = match code {
            true => people::redeem_code(&state, people, fields, now).await,
            false => people::redeem_refresh(&state, people, fields, now).await,
        };
        return match result {
            Ok(response) => response,
            Err(error) => {
                tracing::info!(reason = error.detail(), "person token refused");
                error.into_response()
            }
        };
    }
    match issue(&state, fields).await {
        Ok(response) => response,
        Err(error) => {
            let user_agent = headers
                .get(header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            tracing::info!(reason = error.detail(), user_agent, "agent token refused");
            error.into_response()
        }
    }
}

async fn issue(
    state: &AgentAuth,
    fields: serde_json::Map<String, serde_json::Value>,
) -> Result<Response, OAuthError> {
    let req: TokenRequest = serde_json::from_value(serde_json::Value::Object(fields))
        .map_err(|e| OAuthError::InvalidRequest(e.to_string()))?;
    if !state.issuer.serves_agents() {
        return Err(OAuthError::UnsupportedGrantType(format!(
            "grant_type {:?}: this controller issues tokens to people only",
            req.grant_type
        )));
    }
    let profile = &state.profile;
    placement::sandbox_namespace(Some(profile))
        .map_err(|e| OAuthError::UnauthorizedClient(e.to_string()))?;
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
    let run = bindings::resolve(&state.pool, &sandbox.id)
        .await
        .map_err(|e| match e {
            bindings::ResolveError::Db(_) => OAuthError::Server(format!("{e:#}")),
            e => OAuthError::InvalidGrant(e.to_string()),
        })?;
    if run.cluster != sandbox.cluster {
        return Err(GrantError::WrongCluster {
            sandbox: sandbox.id.to_string(),
            spire: sandbox.cluster.clone(),
            run: run.cluster.clone(),
        }
        .into());
    }
    let expected = issuer::GatewayId::of_run_pod(&run.pod_name);
    if sandbox.gateway != expected {
        return Err(OAuthError::InvalidGrant(format!(
            "sandbox {} was created by gateway {}, not {expected}, the gateway of run pod {}",
            sandbox.id, sandbox.gateway, run.pod_name
        )));
    }
    let audience = state
        .issuer
        .audience(req.audience.as_deref(), &run.cluster)?;
    let launcher = issuer::launcher(&run)?;
    let kueue = profile.kueue.as_ref();
    let namespace = kueue.and_then(|k| issuer::default_namespace(k, &run.cluster));
    let scopes = issuer::granted_scopes(req.scope.as_deref(), audience)?;
    let resolved =
        crate::identity::cluster_identity::for_login(&state.pool, profile, &run.cluster, &launcher)
            .await
            .map_err(|e| OAuthError::Server(format!("{e:#}")))?;
    let kube_user = match resolved {
        crate::identity::cluster_identity::Resolution::Known { user, .. } => Some(user),
        unknown => {
            let why = match unknown {
                crate::identity::cluster_identity::Resolution::Unavailable(why) => why,
                _ => format!("{launcher} has not linked a username on {}", run.cluster),
            };
            if scopes.iter().any(|s| issuer::is_kueue_scope(s)) {
                return Err(GrantError::NoKubeUser {
                    launcher: launcher.clone(),
                    cluster: run.cluster.clone(),
                    why,
                }
                .into());
            }
            tracing::warn!(
                launcher,
                sandbox = %sandbox.id,
                cluster = run.cluster,
                reason = why,
                "agent token minted without kube_user"
            );
            None
        }
    };
    let grant = AgentGrant {
        sandbox,
        launcher,
        kube_user: kube_user.map(|k| k.as_str().to_string()),
        run,
        audience: audience.resource.clone(),
        scopes,
        namespace,
    };
    let minted = state
        .issuer
        .mint(&grant, crate::clock::unix_secs())
        .map_err(|e| OAuthError::Server(e.to_string()))?;
    tracing::info!(
        sandbox = %grant.sandbox.id,
        spiffe_id = grant.sandbox.spiffe_id,
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
    let people = state.people.as_ref().map(|p| issuer::PeopleMetadata {
        scopes: p.config.scopes(),
        client_id_metadata_documents: p.clients.metadata_documents(),
        registration: p.clients.dynamic().is_some(),
    });
    Json(state.issuer.metadata(people.as_ref())).into_response()
}

#[cfg(test)]
mod tests {
    use crate::agent_auth::testing::{
        self, EcKey, ISSUER, KUEUE, POD, PREFIX, PROFILE, Spire, now, running_run, serve_jwks,
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
    }

    async fn harness(pool: PgPool) -> Harness {
        harness_with(pool, PROFILE).await
    }

    async fn harness_with(pool: PgPool, profile_toml: &str) -> Harness {
        let spire = Spire::start().await;
        let (state, _) = testing::agent_auth(&pool, &spire, profile_toml, &[]);
        Harness {
            spire,
            app: router(state.clone()),
            state,
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

    /// A running run with `sandbox` bound, launched by alice, whose Kubernetes username is known.
    async fn bound(pool: &PgPool, sandbox: &str) {
        bound_unlinked(pool, sandbox).await;
        link(pool, "alice-k8s").await;
    }

    async fn bound_unlinked(pool: &PgPool, sandbox: &str) {
        running_run(
            pool,
            "crucible-run-brave-otter",
            "run-1",
            Some("user:alice"),
        )
        .await;
        bindings::bind(
            pool,
            &bindings::run_pod(pool, "run-1").await.unwrap(),
            &testing::binding(sandbox),
        )
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
        assert_eq!(body["expires_in"], issuer::MIN_TOKEN_TTL_SECS);
        assert_eq!(body["scope"], "kueue.read kueue.submit");

        let jwks = serve_jwks(h.state.issuer.jwks()).await;
        let rs = crucible_mcp_auth::server::ResourceServer::with_jwks(
            crucible_mcp_auth::server::ResourceServerConfig {
                issuers: vec![crucible_mcp_auth::server::TrustedIssuer {
                    issuer: ISSUER.into(),
                    jwks_uri: jwks,
                    callers: crucible_mcp_auth::principal::Callers::Runs,
                }],
                resources: vec![url::Url::parse(KUEUE).unwrap()],
                required_scopes: vec!["kueue.read".into()],
                scopes_supported: Vec::new(),
                resource_name: None,
                cluster: None,
            },
            |t| crucible_mcp_auth::jwks::Jwks::new(reqwest::Client::new(), t.jwks_uri.clone()),
        );
        let principal = rs
            .validate(body["access_token"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(principal.subject, "alice");
        assert_eq!(
            principal.presenter(),
            format!("{PREFIX}{POD}/sandbox/sbx-1")
        );
        assert_eq!(principal.namespace.as_deref(), Some("crucible-kueue-e2e"));
        assert_eq!(principal.claims["run_id"], "crucible-run-brave-otter");
        for gone in ["queues", "role"] {
            assert!(
                principal.claims.get(gone).is_none(),
                "{gone}: claims name the caller, never what it may do"
            );
        }
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

    /// alice signs in and links `kube_user` as her username on the run's cluster.
    async fn link(pool: &PgPool, kube_user: &str) {
        link_as(pool, "sub-alice", "alice", kube_user).await;
    }

    async fn link_as(pool: &PgPool, sub: &str, login: &str, kube_user: &str) {
        crate::identity::oidc::users::record_login(pool, sub, login, None, jiff::Timestamp::now())
            .await
            .unwrap();
        let cluster = crate::identity::kube_links::testing::Clusters::with(&[(
            testing::CLUSTER,
            "t",
            kube_user,
        )]);
        crate::identity::kube_links::link(
            pool,
            &cluster,
            sub,
            testing::CLUSTER,
            "t",
            crate::identity::kube_links::LinkSource::TokenReview,
        )
        .await
        .unwrap();
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_launcher_with_a_known_kubernetes_username_gets_kube_user(pool: PgPool) {
        bound(&pool, "sbx-1").await;
        link(&pool, "alice-k8s").await;
        let h = harness(pool).await;
        let svid = h.spire.svid(&Spire::claims("sbx-1"));
        let (status, _, body) = post(&h.app, &form(&svid, &[])).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let claims = claims_of(&body);
        assert_eq!(
            (claims["sub"].as_str(), claims["kube_user"].as_str()),
            (Some("alice"), Some("alice-k8s"))
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_unknown_kubernetes_username_refuses_kueue_tokens_and_only_kueue_tokens(
        pool: PgPool,
    ) {
        bound_unlinked(&pool, "sbx-1").await;
        let strict = harness(pool.clone()).await;
        let svid = strict.spire.svid(&Spire::claims("sbx-1"));
        let (status, _, body) = post(&strict.app, &form(&svid, &[])).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_grant")),
            "{body}"
        );
        let (status, _, body) =
            post(&strict.app, &form(&svid, &[("audience", testing::BUILDIT)])).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a buildit token runs no SubjectAccessReview: {body}"
        );
        assert!(claims_of(&body).get("kube_user").is_none());

        link(&pool, "alice-k8s").await;
        let svid = strict.spire.svid(&Spire::claims("sbx-1"));
        let (status, _, body) = post(&strict.app, &form(&svid, &[])).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(claims_of(&body)["kube_user"], "alice-k8s");
    }

    /// A spoke checks RBAC as the launcher's username there; the hub's is no stand-in for it.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_token_carries_the_launchers_username_on_the_runs_cluster(pool: PgPool) {
        bound_unlinked(&pool, "sbx-1").await;
        crate::identity::oidc::users::record_login(
            &pool,
            "sub-alice",
            "alice",
            Some("alice@example.com"),
            jiff::Timestamp::now(),
        )
        .await
        .unwrap();
        crate::identity::kube_identity::set_override(
            &pool,
            "alice",
            Some(&crate::identity::kube_identity::KubeUser::parse("alice-hub").unwrap()),
        )
        .await
        .unwrap();
        let h = harness(pool.clone()).await;
        let svid = h.spire.svid(&Spire::claims("sbx-1"));
        let (status, _, body) = post(&h.app, &form(&svid, &[])).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_grant")),
            "alice's hub username is not hers on the run's cluster: {body}"
        );

        let claim = harness_with(
            pool.clone(),
            &format!(
                "{PROFILE}[clusters.{}]\nkubeconfig_secret = \"dev-kubeconfig\"\n\
                 identity = {{ claim = \"email\", prefix = \"oidc:\" }}\n",
                testing::CLUSTER
            ),
        )
        .await;
        let svid = claim.spire.svid(&Spire::claims("sbx-1"));
        let (status, _, body) = post(&claim.app, &form(&svid, &[])).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(claims_of(&body)["kube_user"], "oidc:alice@example.com");

        link(&pool, "alice-dev").await;
        let svid = h.spire.svid(&Spire::claims("sbx-1"));
        let (status, _, body) = post(&h.app, &form(&svid, &[])).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(claims_of(&body)["kube_user"], "alice-dev");
    }

    /// alice launched the issue; bob unparks it. The run that follows was dispatched for bob, so
    /// its token names bob and carries bob's Kubernetes username, never alice's.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_run_another_user_redispatched_acts_for_that_user(pool: PgPool) {
        use crate::daemon::queue::{IssueKey, Override, OverrideKind, OverrideSink, WorkQueue};
        let key = "owner/repo#1";
        let db = crate::Db::new(pool.clone());
        crate::issues::store::upsert_issue(
            &pool,
            &crate::NewIssue {
                key: key.to_string(),
                repo: "owner/repo".to_string(),
                priority: 0,
                evidence_url: None,
                title: None,
                author: None,
                body: None,
                labels: Vec::new(),
                upstream_updated_at: None,
            },
        )
        .await
        .unwrap();
        crate::issues::transitions::park_and_purge(
            &pool,
            key,
            "stuck",
            crate::model::ParkedBy::Human,
        )
        .await
        .unwrap();
        let store = Arc::new(crate::daemon::overrides::OverrideStore::new());
        crate::daemon::overrides::QueueOverrideSink::new(store.clone(), WorkQueue::new()).submit(
            Override {
                key: IssueKey(key.into()),
                kind: OverrideKind::Unpark,
                reason: None,
                priority: None,
                actor: Some("Bob".into()),
            },
        );
        crate::daemon::overrides::apply_pending(&db, &store, key, true)
            .await
            .unwrap();

        crate::runs::work_pods::insert_work_pod(
            &pool,
            &crate::runs::workpod::NewWorkPod {
                pod_name: "crucible-run-again".into(),
                kind: "run".into(),
                issue_key: Some(key.into()),
                state: WorkPodState::Running,
                cost_tag: "run".into(),
                cluster: testing::CLUSTER.into(),
            },
        )
        .await
        .unwrap();
        crate::runs::store::insert_run(
            &pool,
            &crate::NewRun {
                run_id: "run-again".into(),
                scope: None,
                issue: Some(key.into()),
                identity_digest: None,
                status: "running".into(),
                pod: Some("crucible-run-again".into()),
                session_uri: None,
                best_score: None,
                cost_usd: None,
            },
        )
        .await
        .unwrap();
        sqlx::query("UPDATE runs SET attributed_to = 'user:alice' WHERE run_id = 'run-again'")
            .execute(&pool)
            .await
            .unwrap();
        crate::runs::store::stamp_dispatcher(&pool, "run-again")
            .await
            .unwrap();
        bindings::bind(
            &pool,
            &bindings::run_pod(&pool, "run-again").await.unwrap(),
            &testing::binding("sbx-1"),
        )
        .await
        .unwrap();
        link(&pool, "alice-k8s").await;
        link_as(&pool, "sub-bob", "bob", "bob-k8s").await;

        let h = harness(pool).await;
        let svid = h
            .spire
            .svid(&Spire::claims_from("crucible-run-again", "sbx-1"));
        let (status, _, body) = post(&h.app, &form(&svid, &[])).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let claims = claims_of(&body);
        assert_eq!(
            (claims["sub"].as_str(), claims["kube_user"].as_str()),
            (Some("bob"), Some("bob-k8s"))
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_deployment_without_a_sandbox_namespace_issues_nothing(pool: PgPool) {
        bound(&pool, "sbx-1").await;
        let unplaced = harness_with(
            pool,
            &PROFILE.replace("sandbox_namespace = \"sandboxes\"\n", ""),
        )
        .await;
        let svid = unplaced.spire.svid(&Spire::claims("sbx-1"));
        let (status, _, body) = post(&unplaced.app, &form(&svid, &[])).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("unauthorized_client")),
            "{body}"
        );
        assert!(body.get("access_token").is_none());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn refusals_describe_nothing_beyond_the_error_code(pool: PgPool) {
        bound(&pool, "sbx-1").await;
        let h = harness(pool).await;
        let forged = testing::sign(&EcKey::generate(), "spire", &Spire::claims("sbx-1"));
        let (_, _, body) = post(&h.app, &form(&forged, &[])).await;
        assert_eq!(
            body,
            json!({"error": "invalid_client", "error_description": "the client assertion was not accepted"})
        );
        let unbound = h.spire.svid(&Spire::claims("sbx-unbound"));
        let (_, _, body) = post(&h.app, &form(&unbound, &[])).await;
        assert_eq!(
            body,
            json!({"error": "invalid_grant", "error_description": "no token can be issued for this sandbox"})
        );
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
        bindings::bind(
            &pool,
            &bindings::run_pod(&pool, "run-team").await.unwrap(),
            &testing::binding("sbx-team"),
        )
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
                form(
                    &h.spire
                        .svid(&Spire::claims_from("crucible-run-team", "sbx-team")),
                    &[],
                ),
                StatusCode::BAD_REQUEST,
                "invalid_grant",
            ),
            (
                "a bound sandbox id claimed by a supervisor another run's gateway created",
                form(
                    &h.spire
                        .svid(&Spire::claims_from("crucible-run-team", "sbx-1")),
                    &[],
                ),
                StatusCode::BAD_REQUEST,
                "invalid_grant",
            ),
            (
                "an SVID without a gateway segment",
                form(
                    &svid(&|c| {
                        c.insert(
                            "sub".into(),
                            json!("spiffe://example.crucible/openshell/sandbox/sbx-1"),
                        );
                    }),
                    &[],
                ),
                StatusCode::UNAUTHORIZED,
                "invalid_client",
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
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_run_on_another_cluster_than_the_svids_spire_gets_no_token(pool: PgPool) {
        bound(&pool, "sbx-1").await;
        let h = harness(pool.clone()).await;
        let svid = h.spire.svid(&Spire::claims("sbx-1"));
        assert_eq!(post(&h.app, &form(&svid, &[])).await.0, StatusCode::OK);
        sqlx::query("UPDATE work_pods SET cluster = 'elsewhere' WHERE pod_name = $1")
            .bind(POD)
            .execute(&pool)
            .await
            .unwrap();
        let (status, _, body) = post(&h.app, &form(&svid, &[])).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_grant")),
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
            serde_json::to_value(h.state.issuer.metadata(None)).unwrap()
        );
    }
}
