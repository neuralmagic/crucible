//! People's access tokens: RFC 6749 authorization code with RFC 7636 PKCE (S256 only) for public
//! clients ([`clients`]: registered ones, and metadata document ones when enabled), on top of the
//! controller's own browser login.
//!
//! ```text
//!  MCP client ──GET /oauth/authorize──▶ session? ──no──▶ /auth/login?rd=… ──▶ upstream OIDC
//!                                         │yes ◀───────────────────────────────────┘
//!                                         ▼
//!                       303 /consent?id=… (SPA: links the cluster username when none is known)
//!  browser ──POST /api/oauth/consent──▶ code: single use, 60 s, bound to client, redirect,
//!                                        challenge, resource and login
//!          ◀── {redirect_url: <the client's registered redirect>?code&state&iss}
//!  MCP client ──POST /oauth/token (code, verifier)──▶ ES256: sub=login, aud=resource, kube_user
//!             ──POST /oauth/token (refresh_token)──▶ rotated; reuse, or another client presenting
//!                                                     it, revokes the approval
//! ```
//!
//! Codes and refresh tokens live in Postgres ([`grants`]), so any replica redeems what another
//! issued. Every refresh re-reads the person and their cluster username.

pub mod cimd;
pub mod clients;
pub mod directory;
pub mod dynamic;
pub mod grants;
pub mod registry;

use crate::agent_auth::AgentAuth;
use crate::agent_auth::issuer::{AUTHORIZE_PATH, AgentIssuer, MintError, token_id};
use crate::agent_auth::people::cimd::{CIMD_HOSTS_ENV, CimdConfigError, CimdPolicy};
use crate::agent_auth::people::clients::{
    CLIENTS_FILE_ENV, Client, ClientId, RedirectUri, RegistryError,
};
use crate::agent_auth::people::dynamic::{DCR_ENV, DcrConfigError, DcrPolicy, TRUSTED_PROXIES_ENV};
use crate::agent_auth::people::registry::Clients;
use crate::identity::api_key::constant_time_eq;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tower_sessions::Session;
use url::Url;

pub const CODE_TTL_SECS: u64 = 60;
/// An access token's lifetime unless `CONTROLLER_PEOPLE_ACCESS_TTL_SECS` says otherwise.
pub const ACCESS_TTL_SECS: u64 = 900;
pub(crate) const PENDING_KEY: &str = "oauth.pending";
/// The SPA page a parked authorization request is shown on.
pub const CONSENT_PATH: &str = "/consent";
const MAX_STATE_LEN: usize = 1024;
/// OIDC scopes a client may ask for out of habit; they grant nothing here.
const IGNORED_SCOPES: &[&str] = &["openid", "profile", "email", "offline_access"];

/// An MCP resource people may get tokens for, served on one cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonAudience {
    pub resource: String,
    pub cluster: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PeopleConfigError {
    #[error("CONTROLLER_PEOPLE_AUDIENCES entry {0:?} is not <cluster>:<resource url>=<scope> ...")]
    Shape(String),
    #[error("CONTROLLER_PEOPLE_AUDIENCES resource {0:?} is not an http(s) URL without a fragment")]
    Resource(String),
    #[error("CONTROLLER_PEOPLE_AUDIENCES names resource {0:?} twice")]
    Duplicate(String),
    #[error("CONTROLLER_PEOPLE_AUDIENCES is set but lists no audience")]
    Empty,
    #[error(
        "CONTROLLER_PEOPLE_ACCESS_TTL_SECS {0:?} is not a whole number of seconds from 1 to 3600"
    )]
    AccessTtl(String),
    #[error(transparent)]
    Clients(#[from] RegistryError),
    #[error(transparent)]
    Cimd(#[from] CimdConfigError),
    #[error(transparent)]
    Dcr(#[from] DcrConfigError),
}

/// `<cluster>:<resource url>=<scope> <scope> ...`
impl std::str::FromStr for PersonAudience {
    type Err = PeopleConfigError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let shape = || PeopleConfigError::Shape(raw.to_string());
        let (cluster, rest) = raw.trim().split_once(':').ok_or_else(shape)?;
        let label = !cluster.is_empty()
            && cluster
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !label {
            return Err(shape());
        }
        let (resource, scopes) = rest.split_once('=').unwrap_or((rest, ""));
        let resource = resource.trim();
        let ok = Url::parse(resource).is_ok_and(|u| {
            matches!(u.scheme(), "http" | "https") && u.host().is_some() && u.fragment().is_none()
        });
        if !ok {
            return Err(PeopleConfigError::Resource(resource.to_string()));
        }
        Ok(Self {
            resource: resource.to_string(),
            cluster: cluster.to_string(),
            scopes: scopes.split_whitespace().map(str::to_string).collect(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeopleConfig {
    pub audiences: Vec<PersonAudience>,
    pub access_ttl_secs: u64,
    /// The registered clients: `CONTROLLER_PEOPLE_CLIENTS_FILE`, or Claude Code, pi and Codex.
    pub clients: Vec<Client>,
    /// Metadata document clients, off unless `CONTROLLER_PEOPLE_CIMD_HOSTS` names hosts.
    pub cimd: Option<CimdPolicy>,
    /// Dynamic registration of loopback public clients, on unless `CONTROLLER_PEOPLE_DCR=false`.
    pub dcr: Option<DcrPolicy>,
}

impl PeopleConfig {
    /// Every scope a person's token may carry, for the issuer's metadata.
    pub fn scopes(&self) -> Vec<String> {
        let mut scopes: Vec<String> = Vec::new();
        for scope in self.audiences.iter().flat_map(|a| a.scopes.iter()) {
            if !scopes.contains(scope) {
                scopes.push(scope.clone());
            }
        }
        scopes
    }

    /// `CONTROLLER_PEOPLE_AUDIENCES`, comma separated; `None` when unset.
    pub fn from_env() -> Result<Option<Self>, PeopleConfigError> {
        let Some(mut config) =
            Self::from_lookup(std::env::var("CONTROLLER_PEOPLE_AUDIENCES").ok())?
        else {
            return Ok(None);
        };
        if let Ok(raw) = std::env::var("CONTROLLER_PEOPLE_ACCESS_TTL_SECS") {
            config.access_ttl_secs = raw
                .trim()
                .parse()
                .ok()
                .filter(|s| (1..=3600).contains(s))
                .ok_or(PeopleConfigError::AccessTtl(raw))?;
        }
        config.dcr = DcrPolicy::from_lookup(std::env::var(DCR_ENV).ok())?;
        if let Some(dcr) = config.dcr.as_mut() {
            dcr.trusted_proxies =
                DcrPolicy::trusted_proxies(std::env::var(TRUSTED_PROXIES_ENV).ok())?;
        }
        let file = std::env::var(CLIENTS_FILE_ENV)
            .ok()
            .filter(|p| !p.trim().is_empty());
        config
            .with_clients(
                clients::from_file(file.as_deref().map(str::trim))?,
                CimdPolicy::from_lookup(std::env::var(CIMD_HOSTS_ENV).ok())?,
            )
            .map(Some)
    }

    /// This configuration with `clients` registered and `cimd` deciding metadata document
    /// clients, refusing a client restricted to a resource that is no audience.
    pub fn with_clients(
        mut self,
        clients: Vec<Client>,
        cimd: Option<CimdPolicy>,
    ) -> Result<Self, PeopleConfigError> {
        for client in &clients {
            for resource in client.resources.iter().flatten() {
                if self.audience(resource).is_none() {
                    return Err(RegistryError::Resource {
                        client: client.id.to_string(),
                        resource: resource.clone(),
                    }
                    .into());
                }
            }
        }
        self.clients = clients;
        self.cimd = cimd;
        Ok(self)
    }

    pub fn from_lookup(raw: Option<String>) -> Result<Option<Self>, PeopleConfigError> {
        let Some(raw) = raw.filter(|r| !r.trim().is_empty()) else {
            return Ok(None);
        };
        let mut audiences: Vec<PersonAudience> = Vec::new();
        for entry in raw.split(',').filter(|e| !e.trim().is_empty()) {
            let audience: PersonAudience = entry.parse()?;
            if audiences.iter().any(|a| a.resource == audience.resource) {
                return Err(PeopleConfigError::Duplicate(audience.resource));
            }
            audiences.push(audience);
        }
        if audiences.is_empty() {
            return Err(PeopleConfigError::Empty);
        }
        Ok(Some(Self {
            audiences,
            access_ttl_secs: ACCESS_TTL_SECS,
            clients: clients::builtin(),
            cimd: None,
            dcr: Some(DcrPolicy::default()),
        }))
    }

    pub fn audience(&self, resource: &str) -> Option<&PersonAudience> {
        self.audiences.iter().find(|a| a.resource == resource)
    }
}

/// An S256 code challenge: the unpadded base64url SHA-256 of the verifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeChallenge(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PkceError {
    #[error("code_challenge is not 43 base64url characters")]
    Challenge,
    #[error("code_verifier is not 43 to 128 unreserved characters")]
    Verifier,
    #[error("code_verifier does not match the code_challenge")]
    Mismatch,
}

impl CodeChallenge {
    pub fn parse(raw: &str) -> Result<Self, PkceError> {
        let ok = raw.len() == 43
            && raw
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        match ok {
            true => Ok(Self(raw.to_string())),
            false => Err(PkceError::Challenge),
        }
    }

    pub fn verify(&self, verifier: &str) -> Result<(), PkceError> {
        let well_formed = (43..=128).contains(&verifier.len())
            && verifier
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'));
        if !well_formed {
            return Err(PkceError::Verifier);
        }
        let derived = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        match constant_time_eq(derived.as_bytes(), self.0.as_bytes()) {
            true => Ok(()),
            false => Err(PkceError::Mismatch),
        }
    }
}

/// What a person approved: who, through which client, for which resource on which cluster, with
/// which scopes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approved {
    pub sub: String,
    pub client_id: ClientId,
    /// The client's name as the person saw it on the consent page.
    pub client_name: String,
    pub login: String,
    /// The email the login recorded, for clusters whose usernames are emails.
    pub email: Option<String>,
    pub resource: String,
    pub cluster: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeGrant {
    pub approved: Approved,
    pub redirect: RedirectUri,
    pub challenge: CodeChallenge,
    pub expires_at: u64,
}

/// The people half of the issuer: its audiences and clients, where grants live, and what it asks
/// its host.
pub struct People {
    pub config: PeopleConfig,
    pub clients: Clients,
    pub grants: Arc<dyn grants::GrantStore>,
    pub subjects: Arc<dyn directory::SubjectDirectory>,
    pub links: Arc<dyn directory::LinkDirectory>,
}

/// A person's access token for `approved`, acting as `kube_user`.
pub fn mint(
    issuer: &AgentIssuer,
    approved: &Approved,
    kube_user: &str,
    now: u64,
    ttl_secs: u64,
) -> Result<String, MintError> {
    issuer.sign(&json!({
        "iss": issuer.issuer().as_str(),
        "sub": approved.login,
        "aud": approved.resource,
        "azp": approved.client_id.as_str(),
        "iat": now,
        "nbf": now,
        "exp": now.saturating_add(ttl_secs),
        "jti": token_id()?,
        "scope": approved.scopes.join(" "),
        "kube_user": kube_user,
        "kube_cluster": approved.cluster,
    }))
}

/// Form or query pairs as one JSON object, refusing any parameter given twice (RFC 6749 §3.1).
pub(crate) fn single_valued(pairs: Vec<(String, String)>) -> Result<Map<String, Value>, String> {
    let mut map = Map::new();
    for (k, v) in pairs {
        if map.insert(k.clone(), Value::String(v)).is_some() {
            return Err(format!("parameter {k:?} is repeated"));
        }
    }
    Ok(map)
}

#[derive(Debug, Deserialize)]
struct AuthorizeQuery {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    state: Option<String>,
    resource: Option<String>,
    scope: Option<String>,
}

/// A validated authorization request, parked on the session until the person decides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizeRequest {
    pub client_id: ClientId,
    /// The registry's name for the client, or the `client_name` its metadata document gives.
    pub client_name: String,
    pub redirect: RedirectUri,
    pub state: String,
    pub challenge: CodeChallenge,
    pub resource: String,
    pub cluster: String,
    pub scopes: Vec<String>,
}

/// Why an authorization request was refused. Before the client and redirect check out the browser
/// is shown an error; after, the error goes back to the client (RFC 6749 §4.1.2.1).
#[derive(Debug, PartialEq, Eq)]
pub enum AuthorizeError {
    Page(String),
    Redirect {
        redirect: RedirectUri,
        state: Option<String>,
        error: &'static str,
        description: String,
    },
}

/// Check an authorization request against its client and the configured audiences. A client that
/// is not one people may use, or a redirect it did not register, is never redirected to.
pub async fn validate(
    config: &PeopleConfig,
    clients: &Clients,
    params: Map<String, Value>,
) -> Result<AuthorizeRequest, AuthorizeError> {
    let q: AuthorizeQuery = serde_json::from_value(Value::Object(params))
        .map_err(|e| AuthorizeError::Page(format!("malformed authorization request: {e}")))?;
    let client = clients
        .resolve(q.client_id.as_deref().unwrap_or_default())
        .await
        .map_err(|e| AuthorizeError::Page(e.to_string()))?;
    let redirect = client
        .redirect(q.redirect_uri.as_deref().unwrap_or_default())
        .map_err(|e| AuthorizeError::Page(e.to_string()))?;
    let state = q.state.clone();
    let back = |error: &'static str, description: String| AuthorizeError::Redirect {
        redirect: redirect.clone(),
        state: state.clone(),
        error,
        description,
    };
    if q.response_type.as_deref() != Some("code") {
        return Err(back(
            "unsupported_response_type",
            "response_type must be code".into(),
        ));
    }
    let state = match q.state.as_deref() {
        Some(s) if !s.is_empty() && s.len() <= MAX_STATE_LEN => s.to_string(),
        _ => {
            return Err(back(
                "invalid_request",
                format!("state is required, at most {MAX_STATE_LEN} characters"),
            ));
        }
    };
    if q.code_challenge_method.as_deref() != Some("S256") {
        return Err(back(
            "invalid_request",
            "code_challenge_method must be S256".into(),
        ));
    }
    let challenge = CodeChallenge::parse(q.code_challenge.as_deref().unwrap_or_default())
        .map_err(|e| back("invalid_request", e.to_string()))?;
    let resource = q.resource.unwrap_or_default();
    let audience = config
        .audience(&resource)
        .filter(|a| client.allows_resource(&a.resource))
        .ok_or_else(|| {
            back(
                "invalid_target",
                format!(
                    "resource {resource:?} is not one this issuer mints tokens for to {}",
                    client.id
                ),
            )
        })?;
    let unknown: Vec<&str> = q
        .scope
        .as_deref()
        .unwrap_or_default()
        .split_whitespace()
        .filter(|s| !IGNORED_SCOPES.contains(s) && !audience.scopes.iter().any(|a| a == s))
        .collect();
    if !unknown.is_empty() {
        return Err(back(
            "invalid_scope",
            format!("scopes {unknown:?} are not offered for {resource}"),
        ));
    }
    Ok(AuthorizeRequest {
        client_id: client.id,
        client_name: client.name,
        redirect,
        state,
        challenge,
        resource: audience.resource.clone(),
        cluster: audience.cluster.clone(),
        // Claude Code does not step up after a 403, so a person gets every scope the audience
        // offers; RBAC as their kube_user bounds what any of them reaches.
        scopes: audience.scopes.clone(),
    })
}

/// An authorization request waiting on the person's decision, parked on their session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Pending {
    pub(crate) id: String,
    pub(crate) sub: String,
    pub(crate) request: AuthorizeRequest,
}

fn escape(raw: &str) -> String {
    raw.chars()
        .fold(String::with_capacity(raw.len()), |mut s, c| {
            match c {
                '&' => s.push_str("&amp;"),
                '<' => s.push_str("&lt;"),
                '>' => s.push_str("&gt;"),
                '"' => s.push_str("&quot;"),
                '\'' => s.push_str("&#39;"),
                c => s.push(c),
            }
            s
        })
}

fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{t}</title>\
         <style>:root{{color-scheme:light dark;--ink:#151515;--ink-3:#6a6e73;--line:#d2d2d2;\
         --bg:#fff;--red:#c9190b}}@media (prefers-color-scheme:dark){{:root{{--ink:#e0e0e0;\
         --ink-3:#a3a3a3;--line:#3c3f42;--bg:#151515;--red:#f0561d}}}}\
         body{{background:var(--bg);color:var(--ink);font:15px/1.5 \"Red Hat Text\",system-ui,sans-serif;\
         max-width:40rem;margin:4rem auto;padding:0 1rem}}h1{{font:600 1.25rem \"Red Hat Display\",\
         system-ui,sans-serif;border-bottom:1px solid var(--line);padding-bottom:.5rem}}\
         .err{{color:var(--red)}}a{{color:inherit}}</style></head><body><h1>{t}</h1>{body}\
         <p style=\"color:var(--ink-3)\"><a href=\"/\">Back to the controller</a></p></body></html>",
        t = escape(title),
    );
    let mut response = (status, Html(html)).into_response();
    let headers = response.headers_mut();
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'none'; frame-ancestors 'none'",
        ),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn refused_page(why: &str) -> Response {
    page(
        StatusCode::BAD_REQUEST,
        "Authorization refused",
        &format!("<p class=\"err\">{}</p>", escape(why)),
    )
}

fn redirect_error(
    redirect: &RedirectUri,
    state: Option<&str>,
    issuer: &str,
    error: &str,
    description: &str,
) -> Response {
    let mut params = vec![("error", error), ("error_description", description)];
    if let Some(state) = state {
        params.push(("state", state));
    }
    params.push(("iss", issuer));
    Redirect::to(&redirect.with(&params)).into_response()
}

fn session_failure(e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "oauth authorize session store");
    page(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Authorization failed",
        "<p>session store error</p>",
    )
}

/// The signed-in person, `None` without a native session or with one whose groups refresh failed.
async fn signed_in(
    session: &Session,
) -> Result<Option<crate::identity::session::NativeClaims>, tower_sessions::session::Error> {
    crate::identity::session::native_claims(session)
        .await
        .map(|c| c.filter(|c| !c.downgraded))
}

pub fn router(state: Arc<AgentAuth>) -> axum::Router {
    axum::Router::new()
        .route(AUTHORIZE_PATH, axum::routing::get(authorize))
        .with_state(state)
}

async fn authorize(
    State(auth): State<Arc<AgentAuth>>,
    session: Session,
    uri: Uri,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    let Some(people) = auth.people.as_ref() else {
        return refused_page("this controller issues no tokens to people");
    };
    let issuer = auth.issuer.issuer().as_str();
    let validated = match single_valued(pairs) {
        Ok(params) => validate(&people.config, &people.clients, params).await,
        Err(why) => Err(AuthorizeError::Page(why)),
    };
    let request = match validated {
        Ok(request) => request,
        Err(AuthorizeError::Page(why)) => {
            tracing::info!(reason = why, "authorization request refused");
            return refused_page(&why);
        }
        Err(AuthorizeError::Redirect {
            redirect,
            state,
            error,
            description,
        }) => {
            tracing::info!(error, reason = description, "authorization request refused");
            return redirect_error(&redirect, state.as_deref(), issuer, error, &description);
        }
    };
    let claims = match signed_in(&session).await {
        Ok(Some(claims)) => claims,
        Ok(None) => {
            let here = uri
                .path_and_query()
                .map(|p| p.as_str())
                .unwrap_or(AUTHORIZE_PATH);
            let rd =
                percent_encoding::utf8_percent_encode(here, percent_encoding::NON_ALPHANUMERIC);
            return Redirect::to(&format!("/auth/login?rd={rd}")).into_response();
        }
        Err(e) => return session_failure(e),
    };
    let mut id = [0u8; 16];
    if aws_lc_rs::rand::fill(&mut id).is_err() {
        return session_failure("drawing random bytes failed");
    }
    let pending = Pending {
        id: crucible_contract::artifact::lower_hex(&id),
        sub: claims.sub.clone(),
        request,
    };
    if let Err(e) = session.insert(PENDING_KEY, &pending).await {
        return session_failure(e);
    }
    Redirect::to(&format!("{CONSENT_PATH}?id={}", pending.id)).into_response()
}

/// RFC 6749 §5.2 errors from the people grants. The detail is logged, never returned.
#[derive(Debug)]
pub(crate) enum TokenError {
    InvalidRequest(String),
    InvalidClient(String),
    InvalidGrant(String),
    InvalidTarget(String),
    Server(String),
}

impl TokenError {
    pub(crate) fn detail(&self) -> &str {
        match self {
            Self::InvalidRequest(d)
            | Self::InvalidClient(d)
            | Self::InvalidGrant(d)
            | Self::InvalidTarget(d)
            | Self::Server(d) => d,
        }
    }
}

impl IntoResponse for TokenError {
    fn into_response(self) -> Response {
        let (status, code, description) = match self {
            Self::InvalidRequest(_) => (
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "the token request is malformed",
            ),
            Self::InvalidClient(_) => {
                (StatusCode::UNAUTHORIZED, "invalid_client", "unknown client")
            }
            Self::InvalidGrant(_) => (
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "the grant is invalid, expired, or already used",
            ),
            Self::InvalidTarget(_) => (
                StatusCode::BAD_REQUEST,
                "invalid_target",
                "the requested resource does not match the grant",
            ),
            Self::Server(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "the token could not be issued",
            ),
        };
        crate::agent_auth::oauth_error(status, code, description)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodeRequest {
    #[allow(dead_code)]
    grant_type: String,
    code: String,
    redirect_uri: String,
    client_id: String,
    code_verifier: String,
    resource: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshRequest {
    #[allow(dead_code)]
    grant_type: String,
    refresh_token: String,
    client_id: String,
    resource: Option<String>,
    scope: Option<String>,
}

fn parse<T: serde::de::DeserializeOwned>(fields: Map<String, Value>) -> Result<T, TokenError> {
    serde_json::from_value(Value::Object(fields))
        .map_err(|e| TokenError::InvalidRequest(e.to_string()))
}

async fn client(people: &People, client_id: &str) -> Result<ClientId, TokenError> {
    people.clients.admits(client_id).await.map_err(|e| match e {
        registry::ClientError::Store(why) => TokenError::Server(why),
        e => TokenError::InvalidClient(e.to_string()),
    })
}

fn same_resource(requested: Option<&str>, granted: &str) -> Result<(), TokenError> {
    match requested {
        Some(r) if r != granted => Err(TokenError::InvalidTarget(format!(
            "resource {r:?} is not the approved {granted:?}"
        ))),
        _ => Ok(()),
    }
}

impl From<grants::GrantError> for TokenError {
    fn from(e: grants::GrantError) -> Self {
        use grants::GrantError as G;
        match e {
            G::Unknown | G::Expired | G::Reused | G::Revoked | G::WrongClient { .. } => {
                Self::InvalidGrant(e.to_string())
            }
            G::Overreach(grants::Overreach::Resource(why)) => Self::InvalidTarget(why),
            G::Overreach(grants::Overreach::Scope(why)) => Self::InvalidGrant(why),
            G::Random | G::Clock(_) | G::Store(_) => Self::Server(format!("{e:#}")),
        }
    }
}

/// `grant_type=authorization_code`.
pub(crate) async fn redeem_code(
    auth: &AgentAuth,
    people: &People,
    fields: Map<String, Value>,
    now: u64,
) -> Result<Response, TokenError> {
    let req: CodeRequest = parse(fields)?;
    let client = client(people, &req.client_id).await?;
    let grant = people.grants.redeem_code(&req.code, now).await?;
    if grant.approved.client_id != client {
        return Err(TokenError::InvalidGrant(format!(
            "the code was issued to client {}, not {client}",
            grant.approved.client_id
        )));
    }
    if grant.redirect.as_str() != req.redirect_uri {
        return Err(TokenError::InvalidGrant(format!(
            "redirect_uri {:?} is not the one the code was issued to",
            req.redirect_uri
        )));
    }
    grant
        .challenge
        .verify(&req.code_verifier)
        .map_err(|e| TokenError::InvalidGrant(e.to_string()))?;
    same_resource(req.resource.as_deref(), &grant.approved.resource)?;
    let approved = grant.approved;
    let (access_token, kube_user) = access(auth, people, &approved, now).await?;
    let refresh_token = people.grants.issue_refresh(&approved, now).await?;
    Ok(token_response(
        "authorization_code",
        &approved,
        &kube_user,
        access_token,
        refresh_token,
        people.config.access_ttl_secs,
    ))
}

/// Whether a refresh asks only for what its approval holds: the approved resource, if it names one,
/// and no scope beyond the approval's.
fn refresh_fits(req: &RefreshRequest, family: &grants::Family) -> Result<(), grants::Overreach> {
    if let Some(r) = req.resource.as_deref()
        && r != family.resource
    {
        return Err(grants::Overreach::Resource(format!(
            "resource {r:?} is not the approved {:?}",
            family.resource
        )));
    }
    let wider: Vec<&str> = req
        .scope
        .as_deref()
        .unwrap_or_default()
        .split_whitespace()
        .filter(|s| !IGNORED_SCOPES.contains(s) && !family.scopes.iter().any(|g| g == s))
        .collect();
    match wider.is_empty() {
        true => Ok(()),
        false => Err(grants::Overreach::Scope(format!(
            "refresh asks for scopes {wider:?} beyond the approval"
        ))),
    }
}

/// `grant_type=refresh_token`, rotating: the presented token is spent and the next one returned.
/// A token presented by a client other than the one it was issued to revokes the approval. The
/// person is read again, so a deleted or refused account, a narrowed approval, or a changed cluster
/// username takes effect here; any of those revokes the approval.
pub(crate) async fn redeem_refresh(
    auth: &AgentAuth,
    people: &People,
    fields: Map<String, Value>,
    now: u64,
) -> Result<Response, TokenError> {
    let req: RefreshRequest = parse(fields)?;
    let client = client(people, &req.client_id).await?;
    let fits = |family: &grants::Family| refresh_fits(&req, family);
    let (family, next) = people
        .grants
        .rotate_refresh_if(&req.refresh_token, &client, now, &fits)
        .await?;
    let refused = |why: String| async move {
        if let Err(e) = people.grants.revoke_family(&family.id, now).await {
            tracing::warn!(error = %e, "revoking a refused approval");
        }
        Err(TokenError::InvalidGrant(why))
    };
    if !people.clients.allows_resource(&client, &family.resource) {
        return refused(format!(
            "client {client} may no longer hold tokens for {}",
            family.resource
        ))
        .await;
    }
    let person = people
        .subjects
        .allowed(&family.sub)
        .await
        .map_err(|e| TokenError::Server(format!("{e:#}")))?;
    let Some(directory::Allowed { login, email }) = person else {
        return refused("the person behind this approval may no longer sign in".into()).await;
    };
    let approved = Approved {
        sub: family.sub.clone(),
        client_id: family.client_id.clone(),
        client_name: family.client_name.clone(),
        login,
        email,
        resource: family.resource.clone(),
        cluster: family.cluster.clone(),
        scopes: family.scopes.clone(),
    };
    let (access_token, kube_user) = match access(auth, people, &approved, now).await {
        Ok(minted) => minted,
        Err(TokenError::InvalidGrant(why) | TokenError::InvalidTarget(why)) => {
            return refused(why).await;
        }
        Err(e) => return Err(e),
    };
    Ok(token_response(
        "refresh_token",
        &approved,
        &kube_user,
        access_token,
        next,
        people.config.access_ttl_secs,
    ))
}

/// An access token for `approved`, with the cluster username it carries.
async fn access(
    auth: &AgentAuth,
    people: &People,
    approved: &Approved,
    now: u64,
) -> Result<(String, String), TokenError> {
    if people.config.audience(&approved.resource).is_none() {
        return Err(TokenError::InvalidTarget(format!(
            "resource {:?} is no longer configured",
            approved.resource
        )));
    }
    let kube_user = match people
        .links
        .kube_user(approved)
        .await
        .map_err(|e| TokenError::Server(format!("{e:#}")))?
    {
        directory::ClusterUser::Known(user) => user,
        directory::ClusterUser::Unlinked => {
            return Err(TokenError::InvalidGrant(format!(
                "{} has no kubernetes username on {}",
                approved.login, approved.cluster
            )));
        }
        directory::ClusterUser::Unavailable(why) => return Err(TokenError::InvalidGrant(why)),
    };
    let access_token = mint(
        &auth.issuer,
        approved,
        &kube_user,
        now,
        people.config.access_ttl_secs,
    )
    .map_err(|e| TokenError::Server(e.to_string()))?;
    Ok((access_token, kube_user))
}

fn token_response(
    grant: &'static str,
    approved: &Approved,
    kube_user: &str,
    access_token: String,
    refresh_token: String,
    ttl_secs: u64,
) -> Response {
    tracing::info!(
        grant,
        client_id = approved.client_id.as_str(),
        sub = approved.login,
        kube_user,
        cluster = approved.cluster,
        aud = approved.resource,
        "person token issued"
    );
    let mut response = axum::Json(json!({
        "access_token": access_token,
        "token_type": "Bearer",
        "expires_in": ttl_secs,
        "scope": approved.scopes.join(" "),
        "refresh_token": refresh_token,
    }))
    .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use crate::agent_auth::people::cimd::testing::{DOC_HOST, DocServer, OTHER_HOST, document};
    use crate::agent_auth::people::cimd::{Cimd, CimdLimits, CimdPolicy};
    use crate::agent_auth::people::clients::{CLAUDE_CODE, CODEX, PI, builtin};
    use crate::agent_auth::people::dynamic::Dcr;
    use crate::agent_auth::people::*;
    use crate::agent_auth::testing::{self, ISSUER, Spire};
    use crate::identity::kube_links::LinkSource;
    use crate::identity::kube_links::testing::Clusters;
    use crate::identity::session::NativeClaims;
    use axum::body::Body;
    use axum::http::Request;
    use sqlx::PgPool;
    use std::collections::HashMap;
    use tower::ServiceExt;

    const PIRATE_KUEUE: &str = "https://crucible-kueue.apps.pirate.example/mcp";
    const EAST_KUEUE: &str = "https://crucible-kueue.apps.east.example/mcp";
    const WALDORF_KUEUE: &str = "http://localhost:8852/mcp";
    const CLIENT_ID: &str = CLAUDE_CODE;
    const REDIRECT: &str = "http://localhost:8765/callback";
    const PI_REDIRECT: &str = "http://127.0.0.1:8765/callback";
    const CODEX_REDIRECT: &str = "http://127.0.0.1:51234/callback/3q2-7wAAAAAA";
    /// RFC 7636 appendix B.
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    const SUB: &str = "f:sso:jordan";

    fn config() -> PeopleConfig {
        PeopleConfig::from_lookup(Some(format!(
            "pirate:{PIRATE_KUEUE}=kueue.read kueue.submit, east:{EAST_KUEUE}=kueue.read, \
             waldorf:{WALDORF_KUEUE}=kueue.read, nowhere:https://unlisted.example/mcp=kueue.read"
        )))
        .unwrap()
        .unwrap()
    }

    fn clients() -> Clients {
        Clients::new(builtin(), None, None)
    }

    fn params(pairs: &[(&str, &str)]) -> Map<String, Value> {
        single_valued(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
        .unwrap()
    }

    fn request(edit: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut fields: Vec<(String, String)> = [
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT),
            ("code_challenge", CHALLENGE),
            ("code_challenge_method", "S256"),
            ("state", "st-1"),
            ("resource", PIRATE_KUEUE),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        for (k, v) in edit {
            fields.retain(|(key, _)| key != k);
            if !v.is_empty() {
                fields.push((k.to_string(), v.to_string()));
            }
        }
        fields
    }

    fn encode(fields: &[(String, String)]) -> String {
        fields
            .iter()
            .map(|(k, v)| {
                format!(
                    "{k}={}",
                    percent_encoding::utf8_percent_encode(v, percent_encoding::NON_ALPHANUMERIC)
                )
            })
            .collect::<Vec<_>>()
            .join("&")
    }

    #[test]
    fn pkce_accepts_only_the_verifier_behind_the_challenge() {
        let challenge = CodeChallenge::parse(CHALLENGE).unwrap();
        assert_eq!(challenge.verify(VERIFIER), Ok(()));
        let other = "x".repeat(43);
        assert_eq!(challenge.verify(&other), Err(PkceError::Mismatch));
        assert_eq!(challenge.verify("short"), Err(PkceError::Verifier));
        assert_eq!(challenge.verify(&"a".repeat(129)), Err(PkceError::Verifier));
        assert_eq!(
            challenge.verify(&format!("{} ", &VERIFIER[..42])),
            Err(PkceError::Verifier)
        );
        for bad in [
            &CHALLENGE[..42],
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw+cM",
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM=",
            "",
        ] {
            assert_eq!(
                CodeChallenge::parse(bad),
                Err(PkceError::Challenge),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn audiences_parse_as_cluster_resource_and_scopes() {
        let cfg = config();
        assert_eq!(
            cfg.audience(PIRATE_KUEUE),
            Some(&PersonAudience {
                resource: PIRATE_KUEUE.into(),
                cluster: "pirate".into(),
                scopes: vec!["kueue.read".into(), "kueue.submit".into()],
            })
        );
        assert_eq!(PeopleConfig::from_lookup(None), Ok(None));
        assert_eq!(
            PeopleConfig::from_lookup(Some(" , ".into())),
            Err(PeopleConfigError::Empty)
        );
        for (raw, want) in [
            (PIRATE_KUEUE.to_string(), "Resource"),
            ("Pirate:https://a.example/mcp".to_string(), "Shape"),
            ("pirate:ftp://a.example/mcp".to_string(), "Resource"),
            ("pirate:https://a.example/mcp#x".to_string(), "Resource"),
            (format!("a:{PIRATE_KUEUE},b:{PIRATE_KUEUE}"), "Duplicate"),
        ] {
            let err = PeopleConfig::from_lookup(Some(raw.clone())).unwrap_err();
            assert!(format!("{err:?}").starts_with(want), "{raw}: {err:?}");
        }
    }

    #[tokio::test]
    async fn a_valid_request_names_its_client_audience_cluster_and_every_scope_it_offers() {
        let r = validate(
            &config(),
            &clients(),
            params(&[
                ("response_type", "code"),
                ("client_id", CLIENT_ID),
                ("redirect_uri", REDIRECT),
                ("code_challenge", CHALLENGE),
                ("code_challenge_method", "S256"),
                ("state", "st-1"),
                ("resource", PIRATE_KUEUE),
                ("scope", "openid kueue.read offline_access"),
                ("prompt", "consent"),
            ]),
        )
        .await
        .unwrap();
        assert_eq!(r.client_id.as_str(), CLAUDE_CODE);
        assert_eq!(r.client_name, "Claude Code");
        assert_eq!(r.cluster, "pirate");
        assert_eq!(r.resource, PIRATE_KUEUE);
        assert_eq!(r.scopes, ["kueue.read", "kueue.submit"]);
        assert_eq!(r.state, "st-1");
    }

    #[tokio::test]
    async fn bad_clients_and_redirects_are_shown_to_the_browser_never_redirected() {
        for edit in [
            ("client_id", "someone-else"),
            ("client_id", ""),
            ("client_id", "https://chatgpt.com/oauth/codex/client.json"),
            ("redirect_uri", "https://evil.example/callback"),
            ("redirect_uri", "http://localhost:8765/other"),
            ("redirect_uri", PI_REDIRECT),
            ("redirect_uri", "http://localhost:8765/callback#frag"),
            ("redirect_uri", ""),
        ] {
            let pairs = request(&[edit]);
            let got = validate(&config(), &clients(), single_valued(pairs).unwrap()).await;
            assert!(
                matches!(got, Err(AuthorizeError::Page(_))),
                "{edit:?}: {got:?}"
            );
        }
    }

    /// Each built-in client is held to the callback it actually sends, and borrowing another
    /// client's id does not borrow its redirect.
    #[tokio::test]
    async fn each_builtin_client_is_held_to_its_own_redirect() {
        for (client, good, bad) in [
            (CLAUDE_CODE, REDIRECT, PI_REDIRECT),
            (PI, PI_REDIRECT, REDIRECT),
            (
                CODEX,
                CODEX_REDIRECT,
                "http://localhost:51234/callback/3q2-7wAAAAAA",
            ),
            (CODEX, PI_REDIRECT, "http://127.0.0.1:51234/callback/a/b"),
        ] {
            let ok = validate(
                &config(),
                &clients(),
                single_valued(request(&[("client_id", client), ("redirect_uri", good)])).unwrap(),
            )
            .await
            .unwrap();
            assert_eq!(
                (ok.client_id.as_str(), ok.redirect.as_str()),
                (client, good)
            );
            let refused = validate(
                &config(),
                &clients(),
                single_valued(request(&[("client_id", client), ("redirect_uri", bad)])).unwrap(),
            )
            .await;
            assert!(
                matches!(refused, Err(AuthorizeError::Page(ref why)) if why.contains(client)),
                "{client} {bad}: {refused:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_client_restricted_to_some_resources_is_refused_the_others() {
        let mut registry = builtin();
        registry[1].resources = Some(vec![EAST_KUEUE.into()]);
        let cfg = config().with_clients(registry.clone(), None).unwrap();
        let restricted = Clients::new(registry, None, None);
        let ask = |resource| {
            single_valued(request(&[
                ("client_id", PI),
                ("redirect_uri", PI_REDIRECT),
                ("resource", resource),
            ]))
            .unwrap()
        };
        assert!(validate(&cfg, &restricted, ask(EAST_KUEUE)).await.is_ok());
        assert!(matches!(
            validate(&cfg, &restricted, ask(PIRATE_KUEUE)).await,
            Err(AuthorizeError::Redirect {
                error: "invalid_target",
                ..
            })
        ));
        let mut bad = builtin();
        bad[0].resources = Some(vec!["https://nowhere.example/mcp".into()]);
        assert!(matches!(
            config().with_clients(bad, None),
            Err(PeopleConfigError::Clients(
                crate::agent_auth::people::clients::RegistryError::Resource { .. }
            ))
        ));
    }

    #[tokio::test]
    async fn other_refusals_go_back_to_the_client_with_its_state() {
        for (edit, want) in [
            (("resource", "https://other.example/mcp"), "invalid_target"),
            (("resource", ""), "invalid_target"),
            (
                ("resource", EAST_KUEUE.trim_end_matches("/mcp")),
                "invalid_target",
            ),
            (("response_type", "token"), "unsupported_response_type"),
            (("code_challenge_method", "plain"), "invalid_request"),
            (("code_challenge_method", ""), "invalid_request"),
            (("code_challenge", "short"), "invalid_request"),
            (("scope", "kueue.read buildit.build"), "invalid_scope"),
        ] {
            match validate(
                &config(),
                &clients(),
                single_valued(request(&[edit])).unwrap(),
            )
            .await
            {
                Err(AuthorizeError::Redirect {
                    error,
                    state,
                    redirect,
                    ..
                }) => {
                    assert_eq!(error, want, "{edit:?}");
                    assert_eq!(state.as_deref(), Some("st-1"));
                    assert_eq!(redirect.as_str(), REDIRECT);
                }
                other => panic!("{edit:?}: {other:?}"),
            }
        }
        let long = "s".repeat(MAX_STATE_LEN + 1);
        for state in ["", long.as_str()] {
            assert!(matches!(
                validate(
                    &config(),
                    &clients(),
                    single_valued(request(&[("state", state)])).unwrap()
                )
                .await,
                Err(AuthorizeError::Redirect {
                    error: "invalid_request",
                    ..
                })
            ));
        }
        assert!(
            single_valued(vec![
                ("state".into(), "a".into()),
                ("state".into(), "b".into())
            ])
            .is_err()
        );
    }

    fn approved(resource: &str, cluster: &str) -> Approved {
        Approved {
            sub: SUB.into(),
            client_id: ClientId::parse(CLIENT_ID).unwrap(),
            client_name: "Claude Code".into(),
            login: "jordan".into(),
            email: Some("casey@example.com".into()),
            resource: resource.into(),
            cluster: cluster.into(),
            scopes: vec!["kueue.read".into()],
        }
    }

    fn code_grant(expires_at: u64) -> CodeGrant {
        CodeGrant {
            approved: approved(PIRATE_KUEUE, "pirate"),
            redirect: builtin()[0].redirect(REDIRECT).unwrap(),
            challenge: CodeChallenge::parse(CHALLENGE).unwrap(),
            expires_at,
        }
    }

    struct Harness {
        auth: Arc<AgentAuth>,
        app: axum::Router,
        _profile: tempfile::TempDir,
    }

    async fn test_login(session: Session) -> StatusCode {
        let claims = NativeClaims {
            sub: SUB.into(),
            login: "jordan".into(),
            email: Some("casey@example.com".into()),
            groups: Vec::new(),
            groups_at: String::new(),
            downgraded: false,
        };
        crate::identity::session::establish_native(&session, &claims, None)
            .await
            .unwrap();
        StatusCode::NO_CONTENT
    }

    async fn harness(pool: &PgPool) -> Harness {
        harness_with(pool, None, true).await
    }

    /// The harness, resolving metadata document clients with `cimd` when given, and taking
    /// dynamic registrations.
    async fn harness_with(pool: &PgPool, cimd: Option<Cimd>, dcr: bool) -> Harness {
        let spire = Spire::start().await;
        let profile = format!(
            r#"{}
[clusters.pirate]
kubeconfig_secret = "pirate-kubeconfig"
[clusters.east]
kubeconfig_secret = "east-kubeconfig"
identity = {{ claim = "login" }}
[clusters.waldorf]
kubeconfig_secret = "waldorf-kubeconfig"
identity = {{ claim = "email", prefix = "oidc:" }}
"#,
            testing::PROFILE
        );
        let (base, _) = testing::agent_auth(pool, &spire, &profile, &[]);
        let Ok(mut auth) = Arc::try_unwrap(base) else {
            panic!("the harness holds the only reference");
        };
        let loaded: crucible::deploy::DeployProfile = toml::from_str(&profile).unwrap();
        let mut people =
            crate::identity::people_directory::people(config(), pool.clone(), &loaded).unwrap();
        people.clients = Clients::new(
            builtin(),
            cimd,
            dcr.then(|| Arc::new(Dcr::new(pool.clone(), DcrPolicy::default()))),
        );
        auth.people = Some(people);
        let auth = Arc::new(auth);
        crate::identity::oidc::users::record_login(
            pool,
            SUB,
            "jordan",
            Some("casey@example.com"),
            jiff::Timestamp::now(),
        )
        .await
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.toml");
        std::fs::write(&path, &profile).unwrap();
        let api_state = crate::api::state::ApiState {
            deploy_profile: Some(path),
            people_issuer: Some(auth.clone()),
            identities: true,
            ..crate::api::state::ApiState::test(crate::Db::new(pool.clone()), Arc::new(NoSink))
        };
        let signed_in = crate::identity::session::Resolved {
            user: Some("jordan".into()),
            groups: Vec::new(),
            sub: Some(SUB.into()),
            source: crate::identity::session::IdentitySource::Native,
        };
        let api = crate::api::router(api_state).layer(axum::middleware::from_fn(
            move |mut req: axum::extract::Request, next: axum::middleware::Next| {
                let signed_in = signed_in.clone();
                async move {
                    req.extensions_mut()
                        .insert(crate::identity::auth::AuthPath::Session);
                    req.extensions_mut().insert(signed_in);
                    next.run(req).await
                }
            },
        ));
        let store = crate::identity::session::store(pool).await.unwrap();
        let app = router(auth.clone())
            .merge(crate::agent_auth::router(auth.clone()))
            .merge(api)
            .route("/test/login", axum::routing::get(test_login))
            .layer(crate::identity::session::layer(store, false));
        Harness {
            auth,
            app,
            _profile: dir,
        }
    }

    struct NoSink;
    impl crate::daemon::queue::OverrideSink for NoSink {
        fn submit(&self, _ov: crate::daemon::queue::Override) {}
    }

    struct Reply {
        status: StatusCode,
        location: Option<String>,
        cookie: Option<String>,
        headers: axum::http::HeaderMap,
        body: String,
    }

    async fn send(app: &axum::Router, req: Request<Body>) -> Reply {
        let res = app.clone().oneshot(req).await.unwrap();
        let header = |name| {
            res.headers()
                .get(name)
                .map(|v: &HeaderValue| v.to_str().unwrap().to_string())
        };
        let location = header(header::LOCATION);
        let cookie = header(header::SET_COOKIE).map(|c| c.split(';').next().unwrap().to_string());
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        Reply {
            status,
            location,
            cookie,
            headers,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    fn get(uri: &str, cookie: Option<&str>) -> Request<Body> {
        let mut req = Request::get(uri);
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        req.body(Body::empty()).unwrap()
    }

    fn post(uri: &str, cookie: Option<&str>, fields: &[(String, String)]) -> Request<Body> {
        let mut req =
            Request::post(uri).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        req.body(Body::from(encode(fields))).unwrap()
    }

    fn pairs(raw: &[(&str, &str)]) -> Vec<(String, String)> {
        raw.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn query_of(location: &str) -> HashMap<String, String> {
        Url::parse(location)
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    /// Follow an authorization request to the consent page it parks, returning the pending id.
    async fn consent_id(app: &axum::Router, uri: &str, cookie: &str) -> String {
        let parked = send(app, get(uri, Some(cookie))).await;
        assert_eq!(parked.status, StatusCode::SEE_OTHER, "{}", parked.body);
        let location = parked.location.unwrap();
        location
            .strip_prefix(&format!("{CONSENT_PATH}?id="))
            .unwrap_or_else(|| panic!("not the consent page: {location}"))
            .to_string()
    }

    fn post_json(uri: &str, cookie: Option<&str>, body: Value) -> Request<Body> {
        let mut req = Request::post(uri).header(header::CONTENT_TYPE, "application/json");
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        req.body(Body::from(body.to_string())).unwrap()
    }

    async fn decide_as(app: &axum::Router, cookie: Option<&str>, id: &str, verdict: &str) -> Reply {
        send(
            app,
            post_json(
                "/api/oauth/consent",
                cookie,
                json!({"id": id, "decision": verdict}),
            ),
        )
        .await
    }

    /// The loopback URL a decision sends the browser to.
    fn redirect_of(reply: &Reply) -> String {
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        let body: Value = serde_json::from_str(&reply.body).unwrap();
        body["redirect_url"].as_str().unwrap().to_string()
    }

    fn link_pirate() -> Clusters {
        Clusters::with(&[("pirate", "sha256~pirate", "casey")])
    }

    fn claims_of(auth: &AgentAuth, token: &str, aud: &str) -> Map<String, Value> {
        let jwk = auth.issuer.jwks().keys.remove(0);
        let key = jsonwebtoken::DecodingKey::from_jwk(&jwk).unwrap();
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::ES256);
        validation.set_audience(&[aud]);
        validation.set_issuer(&[ISSUER]);
        jsonwebtoken::decode::<Map<String, Value>>(token, &key, &validation)
            .unwrap()
            .claims
    }

    async fn token(app: &axum::Router, fields: &[(&str, &str)]) -> (StatusCode, Value) {
        let reply = send(
            app,
            post(crate::agent_auth::issuer::TOKEN_PATH, None, &pairs(fields)),
        )
        .await;
        (reply.status, serde_json::from_str(&reply.body).unwrap())
    }

    fn authorize_uri(edit: &[(&str, &str)]) -> String {
        format!("{AUTHORIZE_PATH}?{}", encode(&request(edit)))
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn claude_code_signs_in_links_its_cluster_user_and_gets_a_token_for_the_resource(
        pool: PgPool,
    ) {
        let h = harness(&pool).await;
        let uri = authorize_uri(&[]);

        let anon = send(&h.app, get(&uri, None)).await;
        assert_eq!(anon.status, StatusCode::SEE_OTHER);
        let login = anon.location.unwrap();
        let rd = login.strip_prefix("/auth/login?rd=").unwrap();
        assert_eq!(
            percent_encoding::percent_decode_str(rd)
                .decode_utf8()
                .unwrap(),
            uri,
            "the login comes back to this authorization request"
        );

        let cookie = send(&h.app, get("/test/login", None)).await.cookie.unwrap();
        let id = consent_id(&h.app, &uri, &cookie).await;

        let unlinked = decide_as(&h.app, Some(&cookie), &id, "approve").await;
        assert_eq!(unlinked.status, StatusCode::CONFLICT);
        assert!(
            unlinked.body.contains("link your username on pirate first"),
            "{}",
            unlinked.body
        );
        crate::identity::kube_links::link(
            &pool,
            &link_pirate(),
            SUB,
            "pirate",
            "sha256~pirate",
            LinkSource::OpenshiftOauth,
        )
        .await
        .unwrap();

        let approved = decide_as(&h.app, Some(&cookie), &id, "approve").await;
        assert_eq!(
            approved
                .headers
                .get(header::CACHE_CONTROL)
                .map(|v| v.to_str().unwrap()),
            Some("no-store")
        );
        let location = redirect_of(&approved);
        assert!(location.starts_with(&format!("{REDIRECT}?")), "{location}");
        let q = query_of(&location);
        assert_eq!(q["state"], "st-1");
        assert_eq!(q["iss"], ISSUER);
        let code = q["code"].clone();

        let replayed = decide_as(&h.app, Some(&cookie), &id, "approve").await;
        assert_eq!(
            replayed.status,
            StatusCode::CONFLICT,
            "an approval is spent once"
        );

        let (status, body) = token(
            &h.app,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT),
                ("client_id", CLIENT_ID),
                ("code_verifier", VERIFIER),
                ("resource", PIRATE_KUEUE),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["token_type"], "Bearer");
        assert_eq!(body["expires_in"], ACCESS_TTL_SECS);
        let claims = claims_of(
            &h.auth,
            body["access_token"].as_str().unwrap(),
            PIRATE_KUEUE,
        );
        assert_eq!(claims["sub"], "jordan");
        assert_eq!(claims["aud"], PIRATE_KUEUE);
        assert_eq!(claims["kube_user"], "casey");
        assert_eq!(claims["kube_cluster"], "pirate");
        assert_eq!(claims["azp"], CLIENT_ID);
        assert_eq!(claims["scope"], "kueue.read kueue.submit");
        assert_eq!(
            claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap(),
            ACCESS_TTL_SECS
        );
        for absent in ["run_id", "act", "namespace", "role"] {
            assert!(!claims.contains_key(absent), "{absent} is a run's claim");
        }

        let (status, body) = token(
            &h.app,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT),
                ("client_id", CLIENT_ID),
                ("code_verifier", VERIFIER),
            ],
        )
        .await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_grant"))
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_cluster_whose_users_are_logins_needs_no_link(pool: PgPool) {
        let h = harness(&pool).await;
        let cookie = send(&h.app, get("/test/login", None)).await.cookie.unwrap();
        let id = consent_id(&h.app, &authorize_uri(&[("resource", EAST_KUEUE)]), &cookie).await;
        let approved = decide_as(&h.app, Some(&cookie), &id, "approve").await;
        let code = query_of(&redirect_of(&approved))["code"].clone();
        let (status, body) = token(
            &h.app,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT),
                ("client_id", CLIENT_ID),
                ("code_verifier", VERIFIER),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let claims = claims_of(&h.auth, body["access_token"].as_str().unwrap(), EAST_KUEUE);
        assert_eq!(claims["kube_user"], "jordan");
        assert_eq!(claims["scope"], "kueue.read");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_email_cluster_takes_the_recorded_email_and_an_unlisted_cluster_needs_a_link(
        pool: PgPool,
    ) {
        let h = harness(&pool).await;
        let cookie = send(&h.app, get("/test/login", None)).await.cookie.unwrap();
        let id = consent_id(
            &h.app,
            &authorize_uri(&[("resource", WALDORF_KUEUE)]),
            &cookie,
        )
        .await;
        let approved = decide_as(&h.app, Some(&cookie), &id, "approve").await;
        let code = query_of(&redirect_of(&approved))["code"].clone();
        let (status, body) = token(
            &h.app,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT),
                ("client_id", CLIENT_ID),
                ("code_verifier", VERIFIER),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let claims = claims_of(
            &h.auth,
            body["access_token"].as_str().unwrap(),
            WALDORF_KUEUE,
        );
        assert_eq!(claims["kube_user"], "oidc:casey@example.com");
        assert_eq!(claims["kube_cluster"], "waldorf");

        let id = consent_id(
            &h.app,
            &authorize_uri(&[("resource", "https://unlisted.example/mcp")]),
            &cookie,
        )
        .await;
        let unlisted = decide_as(&h.app, Some(&cookie), &id, "approve").await;
        assert_eq!(
            unlisted.status,
            StatusCode::CONFLICT,
            "a cluster the profile does not list is link: {}",
            unlisted.body
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_unlinked_person_cannot_approve_and_a_denial_goes_back_to_the_client(pool: PgPool) {
        let h = harness(&pool).await;
        let cookie = send(&h.app, get("/test/login", None)).await.cookie.unwrap();
        let id = consent_id(&h.app, &authorize_uri(&[]), &cookie).await;
        let unlinked = decide_as(&h.app, Some(&cookie), &id, "approve").await;
        assert_eq!(unlinked.status, StatusCode::CONFLICT);
        let forged = decide_as(&h.app, Some(&cookie), "0000", "approve").await;
        assert_eq!(
            forged.status,
            StatusCode::CONFLICT,
            "a forged id spends the approval"
        );
        let spent = decide_as(&h.app, Some(&cookie), &id, "deny").await;
        assert_eq!(spent.status, StatusCode::CONFLICT);

        let id = consent_id(&h.app, &authorize_uri(&[]), &cookie).await;
        let q = query_of(&redirect_of(
            &decide_as(&h.app, Some(&cookie), &id, "deny").await,
        ));
        assert_eq!(q["error"], "access_denied");
        assert_eq!(q["state"], "st-1");
        assert!(!q.contains_key("code"));

        let id = consent_id(&h.app, &authorize_uri(&[]), &cookie).await;
        let other_browser = decide_as(&h.app, None, &id, "approve").await;
        assert_eq!(
            other_browser.status,
            StatusCode::CONFLICT,
            "the request is parked on the browser that started it"
        );
        let as_form = send(
            &h.app,
            post(
                "/api/oauth/consent",
                Some(&cookie),
                &pairs(&[("id", &id), ("decision", "approve")]),
            ),
        )
        .await;
        assert_eq!(
            as_form.status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "a cross-site form cannot decide"
        );
    }

    /// The consent page reads what the browser parked and who the person would be on the
    /// resource's cluster, with the link it needs when there is nobody yet.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn the_consent_page_reads_the_parked_request_and_the_cluster_identity(pool: PgPool) {
        let h = harness(&pool).await;
        let cookie = send(&h.app, get("/test/login", None)).await.cookie.unwrap();
        let read = |id: String| {
            let app = h.app.clone();
            let cookie = cookie.clone();
            async move {
                send(
                    &app,
                    get(&format!("/api/oauth/consent?id={id}"), Some(&cookie)),
                )
                .await
            }
        };
        let id = consent_id(&h.app, &authorize_uri(&[]), &cookie).await;
        let consent = read(id.clone()).await;
        assert_eq!(consent.status, StatusCode::OK, "{}", consent.body);
        let body: Value = serde_json::from_str(&consent.body).unwrap();
        assert_eq!(body["client_id"], CLIENT_ID);
        assert_eq!(body["resource"], PIRATE_KUEUE);
        assert_eq!(body["scopes"], json!(["kueue.read", "kueue.submit"]));
        assert_eq!(body["login"], "jordan");
        assert_eq!(
            body["identity"],
            json!({"cluster": "pirate", "rule": {"kind": "link", "oauth": false},
                   "kube_user": null, "source": null, "linked_at": null})
        );
        assert_eq!(read("0000".into()).await.status, StatusCode::NOT_FOUND);

        let id = consent_id(
            &h.app,
            &authorize_uri(&[("resource", WALDORF_KUEUE)]),
            &cookie,
        )
        .await;
        let body: Value = serde_json::from_str(&read(id).await.body).unwrap();
        assert_eq!(body["identity"]["kube_user"], "oidc:casey@example.com");
        assert_eq!(body["identity"]["source"], "rule");
    }

    /// Error pages carry no form, so nothing they render can post anywhere.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_refused_request_renders_a_page_that_posts_nowhere(pool: PgPool) {
        let h = harness(&pool).await;
        let refused = send(&h.app, get(&authorize_uri(&[("client_id", "other")]), None)).await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
        let csp = refused
            .headers
            .get(header::CONTENT_SECURITY_POLICY)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(csp.contains("form-action 'none'"), "{csp}");
        assert!(!refused.body.contains("<form"), "{}", refused.body);
    }

    async fn issued_code(h: &Harness, pool: &PgPool) -> String {
        crate::identity::kube_links::link(
            pool,
            &Clusters::with(&[("pirate", "t", "casey")]),
            SUB,
            "pirate",
            "t",
            LinkSource::TokenReview,
        )
        .await
        .unwrap();
        let people = h.auth.people.as_ref().unwrap();
        let now = crate::clock::unix_secs();
        people
            .grants
            .issue_code(&code_grant(now + CODE_TTL_SECS))
            .await
            .unwrap()
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_code_redeems_only_with_its_verifier_redirect_resource_and_client(pool: PgPool) {
        let h = harness(&pool).await;
        let other_verifier = "o".repeat(43);
        for (edit, status, error) in [
            (
                ("code_verifier", other_verifier.as_str()),
                StatusCode::BAD_REQUEST,
                "invalid_grant",
            ),
            (
                ("code_verifier", ""),
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
            (
                ("redirect_uri", "http://localhost:9999/callback"),
                StatusCode::BAD_REQUEST,
                "invalid_grant",
            ),
            (
                ("resource", EAST_KUEUE),
                StatusCode::BAD_REQUEST,
                "invalid_target",
            ),
            (
                ("client_id", "someone-else"),
                StatusCode::UNAUTHORIZED,
                "invalid_client",
            ),
            (
                ("client_secret", "x"),
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
        ] {
            let code = issued_code(&h, &pool).await;
            let mut fields = vec![
                ("grant_type", "authorization_code"),
                ("code", code.as_str()),
                ("redirect_uri", REDIRECT),
                ("client_id", CLIENT_ID),
                ("code_verifier", VERIFIER),
            ];
            fields.retain(|(k, _)| *k != edit.0);
            if !edit.1.is_empty() {
                fields.push(edit);
            }
            let (got, body) = token(&h.app, &fields).await;
            assert_eq!(
                (got, body["error"].as_str()),
                (status, Some(error)),
                "{edit:?}"
            );
        }
        let code = issued_code(&h, &pool).await;
        let (status, _) = token(
            &h.app,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT),
                ("client_id", CLIENT_ID),
                ("code_verifier", &other_verifier),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, body) = token(
            &h.app,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT),
                ("client_id", CLIENT_ID),
                ("code_verifier", VERIFIER),
            ],
        )
        .await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_grant")),
            "a failed redemption spends the code"
        );
        let (status, body) = token(
            &h.app,
            &[
                ("grant_type", "authorization_code"),
                ("grant_type", "authorization_code"),
            ],
        )
        .await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_request"))
        );
    }

    async fn code_exchange(app: &axum::Router, code: &str) -> (StatusCode, Value) {
        token(
            app,
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", REDIRECT),
                ("client_id", CLIENT_ID),
                ("code_verifier", VERIFIER),
            ],
        )
        .await
    }

    async fn refreshed(app: &axum::Router, refresh: &Value) -> (StatusCode, Value) {
        token(
            app,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh.as_str().unwrap()),
                ("client_id", CLIENT_ID),
            ],
        )
        .await
    }

    fn refused(reply: &(StatusCode, Value)) -> bool {
        reply.0 == StatusCode::BAD_REQUEST && reply.1["error"] == "invalid_grant"
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn refresh_tokens_rotate_and_never_widen(pool: PgPool) {
        let h = harness(&pool).await;
        let code = issued_code(&h, &pool).await;
        let (_, first) = code_exchange(&h.app, &code).await;
        let (status, second) = token(
            &h.app,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", first["refresh_token"].as_str().unwrap()),
                ("client_id", CLIENT_ID),
                ("resource", PIRATE_KUEUE),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert_ne!(second["refresh_token"], first["refresh_token"]);
        let claims = claims_of(
            &h.auth,
            second["access_token"].as_str().unwrap(),
            PIRATE_KUEUE,
        );
        assert_eq!(claims["kube_user"], "casey");
        let wider = token(
            &h.app,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", second["refresh_token"].as_str().unwrap()),
                ("client_id", CLIENT_ID),
                ("scope", "kueue.read kueue.admin"),
            ],
        )
        .await;
        assert!(refused(&wider), "{wider:?}");
    }

    /// A refresh refused for asking another resource or a wider scope spends nothing: the token
    /// it presented still refreshes, and is not taken for a reuse.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_refused_refresh_leaves_its_token_live(pool: PgPool) {
        let h = harness(&pool).await;
        let code = issued_code(&h, &pool).await;
        let (_, first) = code_exchange(&h.app, &code).await;
        let refresh = first["refresh_token"].as_str().unwrap();
        let elsewhere = token(
            &h.app,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh),
                ("client_id", CLIENT_ID),
                ("resource", "https://unlisted.example/mcp"),
            ],
        )
        .await;
        assert_eq!(elsewhere.0, StatusCode::BAD_REQUEST, "{elsewhere:?}");
        assert_eq!(elsewhere.1["error"], "invalid_target");
        let wider = token(
            &h.app,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh),
                ("client_id", CLIENT_ID),
                ("scope", "kueue.read kueue.admin"),
            ],
        )
        .await;
        assert!(refused(&wider), "{wider:?}");
        let (status, second) = refreshed(&h.app, &first["refresh_token"]).await;
        assert_eq!(status, StatusCode::OK, "{second}");
        let (status, third) = refreshed(&h.app, &second["refresh_token"]).await;
        assert_eq!(status, StatusCode::OK, "the approval survived: {third}");
    }

    /// A spent refresh token presented again revokes its approval: the latest token dies too.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_reused_refresh_token_revokes_the_whole_approval(pool: PgPool) {
        let h = harness(&pool).await;
        let code = issued_code(&h, &pool).await;
        let (_, first) = code_exchange(&h.app, &code).await;
        let (status, second) = refreshed(&h.app, &first["refresh_token"]).await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert!(refused(&refreshed(&h.app, &first["refresh_token"]).await));
        assert!(
            refused(&refreshed(&h.app, &second["refresh_token"]).await),
            "the live token went with the approval"
        );
    }

    /// Two replicas share nothing but the database: a code from one redeems on the other, and the
    /// refresh token that returns rotates on the first.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_code_or_refresh_token_from_one_replica_redeems_on_another(pool: PgPool) {
        let a = harness(&pool).await;
        let b = harness(&pool).await;
        let code = issued_code(&a, &pool).await;
        let (status, on_b) = code_exchange(&b.app, &code).await;
        assert_eq!(status, StatusCode::OK, "{on_b}");
        let claims = claims_of(
            &b.auth,
            on_b["access_token"].as_str().unwrap(),
            PIRATE_KUEUE,
        );
        assert_eq!(claims["kube_user"], "casey");
        let (status, on_a) = refreshed(&a.app, &on_b["refresh_token"]).await;
        assert_eq!(status, StatusCode::OK, "{on_a}");
        assert!(
            refused(&code_exchange(&a.app, &code).await),
            "a code works once across replicas"
        );
    }

    /// The username is read again on every refresh: a relink shows up in the next token, and an
    /// unlink refuses the refresh and ends the approval.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_relink_or_unlink_takes_effect_on_the_next_refresh(pool: PgPool) {
        let h = harness(&pool).await;
        let code = issued_code(&h, &pool).await;
        let (_, first) = code_exchange(&h.app, &code).await;
        crate::identity::kube_links::link(
            &pool,
            &Clusters::with(&[("pirate", "t2", "casey-next")]),
            SUB,
            "pirate",
            "t2",
            LinkSource::OpenshiftOauth,
        )
        .await
        .unwrap();
        let (status, second) = refreshed(&h.app, &first["refresh_token"]).await;
        assert_eq!(status, StatusCode::OK, "{second}");
        let claims = claims_of(
            &h.auth,
            second["access_token"].as_str().unwrap(),
            PIRATE_KUEUE,
        );
        assert_eq!(claims["kube_user"], "casey-next");
        assert!(
            crate::identity::kube_links::unlink(&pool, SUB, "pirate")
                .await
                .unwrap()
        );
        let unlinked = refreshed(&h.app, &second["refresh_token"]).await;
        assert!(refused(&unlinked), "{unlinked:?}");
        let live: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM people_grants WHERE sub = $1 AND revoked_at IS NULL",
        )
        .bind(SUB)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(live, 0, "the approval is revoked");
    }

    /// A person the identity provider has refused, or one who is gone, refreshes nothing.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_refused_or_deleted_person_refreshes_nothing(pool: PgPool) {
        let h = harness(&pool).await;
        let code = issued_code(&h, &pool).await;
        let (_, first) = code_exchange(&h.app, &code).await;
        sqlx::query(
            "INSERT INTO user_credentials (sub, token_cipher, key_id, last_error, failures,
                                           created_at, updated_at)
             VALUES ($1, 'sealed', 'k1', 'invalid_grant: account disabled', 1, '', '')",
        )
        .bind(SUB)
        .execute(&pool)
        .await
        .unwrap();
        assert!(refused(&refreshed(&h.app, &first["refresh_token"]).await));

        let code = issued_code(&h, &pool).await;
        let (_, other) = code_exchange(&h.app, &code).await;
        sqlx::query("DELETE FROM users WHERE sub = $1")
            .bind(SUB)
            .execute(&pool)
            .await
            .unwrap();
        assert!(refused(&refreshed(&h.app, &other["refresh_token"]).await));
    }

    /// Settings lists the person's live approvals and signs every client out at once.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn signing_out_mcp_clients_ends_every_approval(pool: PgPool) {
        let h = harness(&pool).await;
        let code = issued_code(&h, &pool).await;
        let (_, first) = code_exchange(&h.app, &code).await;
        let listed = send(&h.app, get("/api/me/mcp-sign-ins", None)).await;
        assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
        let listed: Value = serde_json::from_str(&listed.body).unwrap();
        assert_eq!(listed["enabled"], true);
        assert_eq!(listed["sign_ins"][0]["resource"], PIRATE_KUEUE);
        assert_eq!(listed["sign_ins"][0]["cluster"], "pirate");
        let out = send(
            &h.app,
            Request::delete("/api/me/mcp-sign-ins")
                .header("sec-fetch-site", "same-origin")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(out.status, StatusCode::OK, "{}", out.body);
        assert_eq!(
            serde_json::from_str::<Value>(&out.body).unwrap()["revoked"],
            1
        );
        assert!(refused(&refreshed(&h.app, &first["refresh_token"]).await));
        let listed: Value =
            serde_json::from_str(&send(&h.app, get("/api/me/mcp-sign-ins", None)).await.body)
                .unwrap();
        assert_eq!(listed["sign_ins"], json!([]));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn the_metadata_advertises_the_code_grant_only_when_people_are_served(pool: PgPool) {
        let h = harness(&pool).await;
        let doc = send(&h.app, get(crate::agent_auth::issuer::METADATA_PATH, None)).await;
        let doc: Value = serde_json::from_str(&doc.body).unwrap();
        assert_eq!(
            doc["authorization_endpoint"],
            format!("{ISSUER}{AUTHORIZE_PATH}")
        );
        assert_eq!(
            doc["grant_types_supported"],
            json!(["client_credentials", "authorization_code", "refresh_token"])
        );
        assert_eq!(doc["response_types_supported"], json!(["code"]));
        assert_eq!(doc["code_challenge_methods_supported"], json!(["S256"]));
        assert_eq!(
            doc["token_endpoint_auth_methods_supported"],
            json!(["private_key_jwt", "none"])
        );
        assert_eq!(doc["authorization_response_iss_parameter_supported"], true);
        assert_eq!(
            doc["registration_endpoint"],
            format!("{ISSUER}{}", crate::agent_auth::issuer::REGISTER_PATH)
        );
        assert!(doc.get("client_id_metadata_document_supported").is_none());
        let agents_only = serde_json::to_value(h.auth.issuer.metadata(None)).unwrap();
        for absent in [
            "authorization_endpoint",
            "code_challenge_methods_supported",
            "authorization_response_iss_parameter_supported",
            "client_id_metadata_document_supported",
            "registration_endpoint",
        ] {
            assert!(agents_only.get(absent).is_none(), "{absent}");
        }
    }

    /// With dynamic registration off, nothing advertises or answers it.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn without_dynamic_registration_nothing_registers(pool: PgPool) {
        let h = harness_with(&pool, None, false).await;
        let doc: Value = serde_json::from_str(
            &send(&h.app, get(crate::agent_auth::issuer::METADATA_PATH, None))
                .await
                .body,
        )
        .unwrap();
        assert!(doc.get("registration_endpoint").is_none(), "{doc}");
        let (status, _) = register(&h.app, json!({"redirect_uris": [PI_REDIRECT]})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    async fn register(app: &axum::Router, body: Value) -> (StatusCode, Value) {
        let reply = send(
            app,
            post_json(crate::agent_auth::issuer::REGISTER_PATH, None, body),
        )
        .await;
        if reply.status == StatusCode::CREATED {
            assert_eq!(
                reply
                    .headers
                    .get(header::CACHE_CONTROL)
                    .map(|v| v.to_str().unwrap()),
                Some("no-store")
            );
        }
        (reply.status, serde_json::from_str(&reply.body).unwrap())
    }

    /// The registration endpoint takes public clients with loopback redirects only.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn registration_accepts_only_public_loopback_clients(pool: PgPool) {
        let h = harness(&pool).await;
        for uri in [
            "http://127.0.0.1:53120/callback",
            "http://localhost:8765/callback",
            "http://[::1]:40000/callback",
            "http://127.0.0.1/callback",
        ] {
            let (status, body) = register(
                &h.app,
                json!({"client_name": "pi", "redirect_uris": [uri],
                       "grant_types": ["authorization_code", "refresh_token"],
                       "response_types": ["code"], "token_endpoint_auth_method": "none",
                       "logo_uri": "https://ignored.example/logo.png"}),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{uri}: {body}");
            assert!(body["client_id"].as_str().unwrap().starts_with("dyn-"));
            assert_eq!(body["token_endpoint_auth_method"], "none");
            assert_eq!(body["redirect_uris"], json!([uri]));
            for absent in [
                "client_secret",
                "registration_access_token",
                "registration_client_uri",
            ] {
                assert!(body.get(absent).is_none(), "{absent}");
            }
        }
        for (body, error) in [
            (
                json!({"redirect_uris": ["https://app.example/callback"]}),
                "invalid_redirect_uri",
            ),
            (
                json!({"redirect_uris": ["com.example.app:/callback"]}),
                "invalid_redirect_uri",
            ),
            (
                json!({"redirect_uris": ["http://127.0.0.1:5000/callback#f"]}),
                "invalid_redirect_uri",
            ),
            (json!({"redirect_uris": []}), "invalid_redirect_uri"),
            (json!({"client_name": "x"}), "invalid_redirect_uri"),
            (
                json!({"redirect_uris": [PI_REDIRECT], "token_endpoint_auth_method": "client_secret_post"}),
                "invalid_client_metadata",
            ),
            (
                json!({"redirect_uris": [PI_REDIRECT], "grant_types": ["client_credentials"]}),
                "invalid_client_metadata",
            ),
            (json!(["not", "an", "object"]), "invalid_client_metadata"),
        ] {
            let (status, reply) = register(&h.app, body.clone()).await;
            assert_eq!(
                (status, reply["error"].as_str()),
                (StatusCode::BAD_REQUEST, Some(error)),
                "{body}: {reply}"
            );
        }
        let (status, body) = register(
            &h.app,
            json!({"client_name": format!("pi\n{}", "x".repeat(400)), "redirect_uris": [PI_REDIRECT]}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let name = body["client_name"].as_str().unwrap();
        assert_eq!(name.chars().count(), 100);
        assert!(name.starts_with("pi x"), "{name}");
    }

    /// A self-registered client signs in like any other: any port on its redirect, its own path
    /// only, consent marks it self-registered with where the code goes, and its grants are bound to
    /// its id.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_self_registered_client_signs_in_bound_to_its_issued_id(pool: PgPool) {
        let h = harness(&pool).await;
        let (_, reg) = register(
            &h.app,
            json!({"client_name": "Claude Code", "redirect_uris": ["http://127.0.0.1:1/callback"]}),
        )
        .await;
        let id = reg["client_id"].as_str().unwrap().to_string();
        let cookie = send(&h.app, get("/test/login", None)).await.cookie.unwrap();
        let consent = consent_of(
            &h,
            &cookie,
            &authorize_uri(&[
                ("client_id", &id),
                ("redirect_uri", "http://127.0.0.1:53120/callback"),
                ("resource", EAST_KUEUE),
            ]),
        )
        .await;
        assert_eq!(consent["client_id"], id.as_str());
        assert_eq!(consent["client_name"], "Claude Code");
        assert_eq!(consent["client_kind"], "dynamic");
        assert_eq!(consent["redirect_origin"], "127.0.0.1:53120");
        for wrong in [
            "http://127.0.0.1:53120/other",
            "http://localhost:53120/callback",
            "https://127.0.0.1:53120/callback",
        ] {
            let page = send(
                &h.app,
                get(
                    &authorize_uri(&[("client_id", &id), ("redirect_uri", wrong)]),
                    None,
                ),
            )
            .await;
            assert_eq!(page.status, StatusCode::BAD_REQUEST, "{wrong}");
        }

        let (status, body) = sign_in_as(&h, &cookie, &id, "http://127.0.0.1:41000/callback").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let claims = claims_of(&h.auth, body["access_token"].as_str().unwrap(), EAST_KUEUE);
        assert_eq!(claims["azp"], id.as_str());
        let (status, next) = refresh_as(&h.app, &id, &body["refresh_token"]).await;
        assert_eq!(status, StatusCode::OK, "{next}");
        let listed: Value =
            serde_json::from_str(&send(&h.app, get("/api/me/mcp-sign-ins", None)).await.body)
                .unwrap();
        assert_eq!(listed["sign_ins"][0]["client_kind"], "dynamic");
        assert_eq!(listed["sign_ins"][0]["client_name"], "Claude Code");

        assert!(
            refused(&refresh_as(&h.app, CLAUDE_CODE, &next["refresh_token"]).await),
            "a registered client cannot redeem the self-registered one's token"
        );
        assert!(refused(
            &refresh_as(&h.app, &id, &next["refresh_token"]).await
        ));

        let (_, other) = register(&h.app, json!({"redirect_uris": [PI_REDIRECT]})).await;
        let other = other["client_id"].as_str().unwrap().to_string();
        let parked = consent_id(
            &h.app,
            &authorize_uri(&[
                ("client_id", &id),
                ("redirect_uri", PI_REDIRECT),
                ("resource", EAST_KUEUE),
            ]),
            &cookie,
        )
        .await;
        let approved = decide_as(&h.app, Some(&cookie), &parked, "approve").await;
        let code = query_of(&redirect_of(&approved))["code"].clone();
        let (status, body) = token(
            &h.app,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", PI_REDIRECT),
                ("client_id", &other),
                ("code_verifier", VERIFIER),
            ],
        )
        .await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_grant")),
            "another registration cannot redeem its code: {body}"
        );
    }

    /// A registration left unused past the idle limit stops refreshing and authorizing.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_expired_registration_stops_refreshing(pool: PgPool) {
        let h = harness(&pool).await;
        let (_, reg) = register(&h.app, json!({"redirect_uris": [PI_REDIRECT]})).await;
        let id = reg["client_id"].as_str().unwrap().to_string();
        let cookie = send(&h.app, get("/test/login", None)).await.cookie.unwrap();
        let (status, body) = sign_in_as(&h, &cookie, &id, PI_REDIRECT).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        sqlx::query("UPDATE people_dynamic_clients SET last_used_at = 0 WHERE client_id = $1")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        let (status, reply) = refresh_as(&h.app, &id, &body["refresh_token"]).await;
        assert_eq!(
            (status, reply["error"].as_str()),
            (StatusCode::UNAUTHORIZED, Some("invalid_client"))
        );
        let page = send(
            &h.app,
            get(
                &authorize_uri(&[("client_id", &id), ("redirect_uri", PI_REDIRECT)]),
                None,
            ),
        )
        .await;
        assert_eq!(page.status, StatusCode::BAD_REQUEST);
        assert!(
            page.body.contains("register the client again"),
            "{}",
            page.body
        );
    }

    async fn sign_in_as(
        h: &Harness,
        cookie: &str,
        client: &str,
        redirect: &str,
    ) -> (StatusCode, Value) {
        let id = consent_id(
            &h.app,
            &authorize_uri(&[
                ("client_id", client),
                ("redirect_uri", redirect),
                ("resource", EAST_KUEUE),
            ]),
            cookie,
        )
        .await;
        let location = redirect_of(&decide_as(&h.app, Some(cookie), &id, "approve").await);
        assert!(location.starts_with(&format!("{redirect}?")), "{location}");
        let code = query_of(&location)["code"].clone();
        token(
            &h.app,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", redirect),
                ("client_id", client),
                ("code_verifier", VERIFIER),
            ],
        )
        .await
    }

    async fn refresh_as(app: &axum::Router, client: &str, refresh: &Value) -> (StatusCode, Value) {
        token(
            app,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh.as_str().unwrap()),
                ("client_id", client),
            ],
        )
        .await
    }

    async fn consent_of(h: &Harness, cookie: &str, uri: &str) -> Value {
        let id = consent_id(&h.app, uri, cookie).await;
        let read = send(
            &h.app,
            get(&format!("/api/oauth/consent?id={id}"), Some(cookie)),
        )
        .await;
        assert_eq!(read.status, StatusCode::OK, "{}", read.body);
        serde_json::from_str(&read.body).unwrap()
    }

    /// Claude Code, pi and Codex each sign in under their own id, with the callback each really
    /// sends; the consent page names them, the token's `azp` is theirs, and each refreshes as itself.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn each_builtin_client_signs_in_as_itself(pool: PgPool) {
        let h = harness(&pool).await;
        let cookie = send(&h.app, get("/test/login", None)).await.cookie.unwrap();
        for (client, name, redirect) in [
            (CLAUDE_CODE, "Claude Code", REDIRECT),
            (PI, "pi", PI_REDIRECT),
            (CODEX, "Codex", CODEX_REDIRECT),
        ] {
            let consent = consent_of(
                &h,
                &cookie,
                &authorize_uri(&[
                    ("client_id", client),
                    ("redirect_uri", redirect),
                    ("resource", EAST_KUEUE),
                ]),
            )
            .await;
            assert_eq!(consent["client_id"], client);
            assert_eq!(consent["client_name"], name);
            assert_eq!(consent["client_host"], Value::Null);

            let (status, body) = sign_in_as(&h, &cookie, client, redirect).await;
            assert_eq!(status, StatusCode::OK, "{client}: {body}");
            let claims = claims_of(&h.auth, body["access_token"].as_str().unwrap(), EAST_KUEUE);
            assert_eq!(claims["azp"], client);
            let (status, next) = refresh_as(&h.app, client, &body["refresh_token"]).await;
            assert_eq!(status, StatusCode::OK, "{client}: {next}");
            let claims = claims_of(&h.auth, next["access_token"].as_str().unwrap(), EAST_KUEUE);
            assert_eq!(claims["azp"], client);
        }
        let listed: Value =
            serde_json::from_str(&send(&h.app, get("/api/me/mcp-sign-ins", None)).await.body)
                .unwrap();
        let mut names: Vec<&str> = listed["sign_ins"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["client_name"].as_str().unwrap())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["Claude Code", "Codex", "pi"]);
    }

    /// A code is bound to the client it was issued to: another registered client redeeming it is
    /// refused, and the attempt spends the code.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_code_redeems_only_for_the_client_it_was_issued_to(pool: PgPool) {
        let h = harness(&pool).await;
        let code = issued_code(&h, &pool).await;
        let redeem = |client: &'static str| {
            let app = h.app.clone();
            let code = code.clone();
            async move {
                token(
                    &app,
                    &[
                        ("grant_type", "authorization_code"),
                        ("code", &code),
                        ("redirect_uri", REDIRECT),
                        ("client_id", client),
                        ("code_verifier", VERIFIER),
                    ],
                )
                .await
            }
        };
        let (status, body) = redeem(PI).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_grant")),
            "{body}"
        );
        assert!(
            refused(&redeem(CLAUDE_CODE).await),
            "the cross-client try spent it"
        );
    }

    /// A refresh token presented by another client revokes the approval: the rightful client's
    /// token dies with it.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_refresh_token_presented_by_another_client_revokes_the_approval(pool: PgPool) {
        let h = harness(&pool).await;
        let code = issued_code(&h, &pool).await;
        let (_, first) = code_exchange(&h.app, &code).await;
        let (status, second) = refreshed(&h.app, &first["refresh_token"]).await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert!(refused(
            &refresh_as(&h.app, CODEX, &second["refresh_token"]).await
        ));
        assert!(
            refused(&refreshed(&h.app, &second["refresh_token"]).await),
            "the approval is gone for its own client too"
        );
        let unknown = refresh_as(&h.app, "someone-else", &second["refresh_token"]).await;
        assert_eq!(
            (unknown.0, unknown.1["error"].as_str()),
            (StatusCode::UNAUTHORIZED, Some("invalid_client"))
        );
    }

    /// Settings signs one client out and leaves the others signed in.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn signing_one_client_out_leaves_the_others(pool: PgPool) {
        let h = harness(&pool).await;
        let cookie = send(&h.app, get("/test/login", None)).await.cookie.unwrap();
        let (_, claude) = sign_in_as(&h, &cookie, CLAUDE_CODE, REDIRECT).await;
        let (_, codex) = sign_in_as(&h, &cookie, CODEX, CODEX_REDIRECT).await;
        let (_, codex_again) = sign_in_as(&h, &cookie, CODEX, PI_REDIRECT).await;
        let delete = |query: &str| {
            Request::delete(format!("/api/me/mcp-sign-ins{query}"))
                .header("sec-fetch-site", "same-origin")
                .body(Body::empty())
                .unwrap()
        };
        let out = send(&h.app, delete("?client_id=codex")).await;
        assert_eq!(out.status, StatusCode::OK, "{}", out.body);
        assert_eq!(
            serde_json::from_str::<Value>(&out.body).unwrap()["revoked"],
            2
        );
        assert!(refused(
            &refresh_as(&h.app, CODEX, &codex["refresh_token"]).await
        ));
        assert!(refused(
            &refresh_as(&h.app, CODEX, &codex_again["refresh_token"]).await
        ));
        let (status, _) = refresh_as(&h.app, CLAUDE_CODE, &claude["refresh_token"]).await;
        assert_eq!(status, StatusCode::OK, "claude-code stays signed in");
        let listed: Value =
            serde_json::from_str(&send(&h.app, get("/api/me/mcp-sign-ins", None)).await.body)
                .unwrap();
        let clients: Vec<&str> = listed["sign_ins"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["client_id"].as_str().unwrap())
            .collect();
        assert_eq!(clients, [CLAUDE_CODE]);
        let again = send(&h.app, delete("?client_id=codex")).await;
        assert_eq!(
            serde_json::from_str::<Value>(&again.body).unwrap()["revoked"],
            0
        );
        let bad = send(&h.app, delete("?client_id=not%20an%20id")).await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{}", bad.body);
    }

    fn cimd_policy(hosts: &[&str]) -> CimdPolicy {
        CimdPolicy {
            hosts: hosts.iter().map(|h| h.to_string()).collect(),
            limits: CimdLimits {
                timeout: std::time::Duration::from_millis(500),
                ..CimdLimits::default()
            },
        }
    }

    /// Publishes `/desk.json`, a public client of its own URL, and `/liar.json`, which names
    /// `/desk.json` as its client_id.
    async fn doc_server() -> DocServer {
        let port = Arc::new(std::sync::OnceLock::<u16>::new());
        let serve = |named: &'static str| {
            let port = port.clone();
            axum::routing::get(move || {
                let port = port.get().copied().unwrap_or_default();
                async move {
                    let url = format!("https://{DOC_HOST}:{port}{named}");
                    axum::Json(document(&url, "Desk Agent")).into_response()
                }
            })
        };
        let app = axum::Router::new()
            .route("/desk.json", serve("/desk.json"))
            .route("/liar.json", serve("/desk.json"));
        let server = DocServer::start(app).await;
        port.set(server.addr.port()).unwrap();
        server
    }

    /// With metadata documents on, an allowlisted URL client signs in under its URL: the consent
    /// page names it and the host that published it, the token's `azp` is the URL, and it refreshes
    /// as itself. Another host, or a document naming another URL, is refused before any consent.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_metadata_document_client_signs_in_under_its_url(pool: PgPool) {
        let docs = doc_server().await;
        let cimd = Cimd::new(cimd_policy(&[DOC_HOST]), docs.builder()).unwrap();
        let h = harness_with(&pool, Some(cimd), true).await;
        let meta: Value = serde_json::from_str(
            &send(&h.app, get(crate::agent_auth::issuer::METADATA_PATH, None))
                .await
                .body,
        )
        .unwrap();
        assert_eq!(meta["client_id_metadata_document_supported"], true);
        assert!(meta.get("registration_endpoint").is_some());

        let cookie = send(&h.app, get("/test/login", None)).await.cookie.unwrap();
        let desk = docs.url(DOC_HOST, "/desk.json");
        let consent = consent_of(
            &h,
            &cookie,
            &authorize_uri(&[
                ("client_id", &desk),
                ("redirect_uri", PI_REDIRECT),
                ("resource", EAST_KUEUE),
            ]),
        )
        .await;
        assert_eq!(consent["client_id"], desk.as_str());
        assert_eq!(consent["client_name"], "Desk Agent");
        assert_eq!(consent["client_host"], DOC_HOST);

        let (status, body) =
            sign_in_as(&h, &cookie, &desk, "http://localhost:40001/callback").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let claims = claims_of(&h.auth, body["access_token"].as_str().unwrap(), EAST_KUEUE);
        assert_eq!(claims["azp"], desk.as_str());
        let (status, next) = refresh_as(&h.app, &desk, &body["refresh_token"]).await;
        assert_eq!(status, StatusCode::OK, "{next}");
        assert!(refused(
            &refresh_as(&h.app, CLAUDE_CODE, &next["refresh_token"]).await
        ));

        for (client, why) in [
            (docs.url(OTHER_HOST, "/desk.json"), "is not on"),
            (docs.url(DOC_HOST, "/liar.json"), "not its own URL"),
            (docs.url(DOC_HOST, "/gone.json"), "answered 404"),
        ] {
            let page = send(
                &h.app,
                get(
                    &authorize_uri(&[("client_id", &client), ("redirect_uri", PI_REDIRECT)]),
                    Some(&cookie),
                ),
            )
            .await;
            assert_eq!(page.status, StatusCode::BAD_REQUEST, "{client}");
            assert!(page.body.contains(why), "{client}: {}", page.body);
        }
        let wrong_redirect = send(
            &h.app,
            get(
                &authorize_uri(&[("client_id", &desk), ("redirect_uri", CODEX_REDIRECT)]),
                Some(&cookie),
            ),
        )
        .await;
        assert_eq!(wrong_redirect.status, StatusCode::BAD_REQUEST);
    }

    /// With metadata documents off (the default) a URL client_id is refused at authorize and at
    /// the token endpoint, and nothing is fetched.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn without_metadata_documents_a_url_client_is_refused(pool: PgPool) {
        let h = harness(&pool).await;
        let url = "https://chatgpt.com/oauth/codex/client.json";
        let page = send(
            &h.app,
            get(
                &authorize_uri(&[("client_id", url), ("redirect_uri", PI_REDIRECT)]),
                None,
            ),
        )
        .await;
        assert_eq!(page.status, StatusCode::BAD_REQUEST);
        assert!(page.body.contains("accepts none"), "{}", page.body);
        let (status, body) = token(
            &h.app,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", "anything"),
                ("client_id", url),
            ],
        )
        .await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::UNAUTHORIZED, Some("invalid_client"))
        );
    }
}
