//! People's access tokens: RFC 6749 authorization code with RFC 7636 PKCE (S256 only) for one
//! pre-registered public client, `claude-code`, on top of the controller's own browser login.
//!
//! ```text
//!  Claude Code ──GET /oauth/authorize──▶ session? ──no──▶ /auth/login?rd=… ──▶ upstream OIDC
//!                                          │yes ◀───────────────────────────────────┘
//!                                          ▼
//!                       consent page (links the cluster username when none is known)
//!  browser ──POST /oauth/authorize──▶ code: single use, 60 s, bound to client, redirect,
//!                                      challenge, resource and login
//!          ◀── 302 http://localhost:<port>/callback?code&state&iss
//!  Claude Code ──POST /oauth/token (code, verifier)──▶ ES256: sub=login, aud=resource, kube_user
//! ```
//!
//! Codes and refresh tokens live in this process's memory: a restart signs every client out, and a
//! second replica would not honour the first's codes.

use crate::agent_auth::AgentAuth;
use crate::agent_auth::issuer::{AgentIssuer, MintError, token_id};
use crate::identity::api_key::constant_time_eq;
use crate::identity::cluster_identity::{self, Person, Resolution};
use crate::identity::kube_identity::KubeUser;
use crate::identity::kube_links::{self, ClusterReviewer, LinkError, LinkSource};
use crate::identity::session::NativeClaims;
use axum::extract::{Form, Query, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_sessions::Session;
use url::Url;

pub const CLIENT_ID: &str = "claude-code";
pub const AUTHORIZE_PATH: &str = "/oauth/authorize";
pub const CODE_TTL_SECS: u64 = 60;
pub const ACCESS_TTL_SECS: u64 = 900;
/// A refresh token's lifetime, counted from the approval and not extended by rotation.
pub const REFRESH_TTL_SECS: u64 = 8 * 3600;
const MAX_GRANTS: usize = 10_000;
const PENDING_KEY: &str = "oauth.pending";
const SECRET_BYTES: usize = 32;
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
}

impl PeopleConfig {
    /// `CONTROLLER_PEOPLE_AUDIENCES`, comma separated; `None` when unset.
    pub fn from_env() -> Result<Option<Self>, PeopleConfigError> {
        Self::from_lookup(std::env::var("CONTROLLER_PEOPLE_AUDIENCES").ok())
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
        Ok(Some(Self { audiences }))
    }

    pub fn audience(&self, resource: &str) -> Option<&PersonAudience> {
        self.audiences.iter().find(|a| a.resource == resource)
    }
}

/// A native app's loopback redirect (RFC 8252 §7.3): `http`, host `localhost`, `127.0.0.1` or
/// `[::1]`, an explicit port, path `/callback`, nothing else, in canonical form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopbackRedirect(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "redirect_uri {0:?} is not http://localhost, 127.0.0.1 or [::1] with an explicit port and path /callback"
)]
pub struct NotLoopback(String);

impl LoopbackRedirect {
    pub fn parse(raw: &str) -> Result<Self, NotLoopback> {
        let refused = || NotLoopback(raw.to_string());
        let url = Url::parse(raw).map_err(|_| refused())?;
        let loopback = match url.host() {
            Some(url::Host::Domain(d)) => d == "localhost",
            Some(url::Host::Ipv4(ip)) => ip == Ipv4Addr::LOCALHOST,
            Some(url::Host::Ipv6(ip)) => ip == Ipv6Addr::LOCALHOST,
            None => false,
        };
        let ok = url.scheme() == "http"
            && loopback
            && url.port().is_some_and(|p| p != 0)
            && url.path() == "/callback"
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && url.as_str() == raw;
        match ok {
            true => Ok(Self(raw.to_string())),
            false => Err(refused()),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn with(&self, params: &[(&str, &str)]) -> String {
        match Url::parse(&self.0) {
            Ok(mut url) => {
                url.query_pairs_mut().extend_pairs(params);
                url.to_string()
            }
            Err(_) => self.0.clone(),
        }
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

/// What a person approved: who, for which resource on which cluster, with which scopes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approved {
    pub sub: String,
    pub login: String,
    /// The email the login recorded, for clusters whose usernames are emails.
    pub email: Option<String>,
    pub resource: String,
    pub cluster: String,
    pub scopes: Vec<String>,
}

impl Approved {
    pub fn person(&self) -> Person<'_> {
        Person {
            sub: &self.sub,
            login: &self.login,
            email: self.email.as_deref(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CodeGrant {
    pub approved: Approved,
    pub redirect: LoopbackRedirect,
    pub challenge: CodeChallenge,
    pub expires_at: u64,
}

#[derive(Debug, Clone)]
struct RefreshGrant {
    approved: Approved,
    expires_at: u64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GrantError {
    #[error("the code or refresh token is unknown or already used")]
    Unknown,
    #[error("the code or refresh token expired")]
    Expired,
    #[error("too many outstanding grants")]
    Full,
    #[error("drawing random bytes failed")]
    Random,
}

/// Outstanding codes and refresh tokens, keyed by the SHA-256 of the secret so the secrets
/// themselves are never held.
#[derive(Default)]
pub struct Grants {
    codes: Mutex<HashMap<[u8; 32], CodeGrant>>,
    refresh: Mutex<HashMap<[u8; 32], RefreshGrant>>,
}

fn new_secret() -> Result<(String, [u8; 32]), GrantError> {
    let mut bytes = [0u8; SECRET_BYTES];
    aws_lc_rs::rand::fill(&mut bytes).map_err(|_| GrantError::Random)?;
    let secret = URL_SAFE_NO_PAD.encode(bytes);
    let key = Sha256::digest(secret.as_bytes()).into();
    Ok((secret, key))
}

fn key_of(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

impl Grants {
    pub async fn issue_code(&self, grant: CodeGrant, now: u64) -> Result<String, GrantError> {
        let (code, key) = new_secret()?;
        let mut codes = self.codes.lock().await;
        codes.retain(|_, g| g.expires_at > now);
        if codes.len() >= MAX_GRANTS {
            return Err(GrantError::Full);
        }
        codes.insert(key, grant);
        Ok(code)
    }

    /// The code's grant, removed whether or not it is still valid: a code works at most once.
    pub async fn take_code(&self, code: &str, now: u64) -> Result<CodeGrant, GrantError> {
        let grant = self
            .codes
            .lock()
            .await
            .remove(&key_of(code))
            .ok_or(GrantError::Unknown)?;
        match grant.expires_at > now {
            true => Ok(grant),
            false => Err(GrantError::Expired),
        }
    }

    async fn issue_refresh(&self, grant: RefreshGrant, now: u64) -> Result<String, GrantError> {
        let (token, key) = new_secret()?;
        let mut refresh = self.refresh.lock().await;
        refresh.retain(|_, g| g.expires_at > now);
        if refresh.len() >= MAX_GRANTS {
            return Err(GrantError::Full);
        }
        refresh.insert(key, grant);
        Ok(token)
    }

    async fn take_refresh(&self, token: &str, now: u64) -> Result<RefreshGrant, GrantError> {
        let grant = self
            .refresh
            .lock()
            .await
            .remove(&key_of(token))
            .ok_or(GrantError::Unknown)?;
        match grant.expires_at > now {
            true => Ok(grant),
            false => Err(GrantError::Expired),
        }
    }
}

/// The people half of the issuer: its audiences, outstanding grants, and the clusters that review
/// the tokens people link their usernames with.
pub struct People {
    pub config: PeopleConfig,
    pub grants: Grants,
    pub reviewer: Arc<dyn ClusterReviewer>,
}

impl People {
    pub fn new(config: PeopleConfig, reviewer: Arc<dyn ClusterReviewer>) -> Self {
        Self {
            config,
            grants: Grants::default(),
            reviewer,
        }
    }
}

/// Who a person is on a cluster, or why that is not known yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClusterUser {
    Known(KubeUser),
    /// The cluster's rule is `link` and the person has not linked.
    Unlinked,
    /// The cluster's rule names a claim this login did not carry, or one that is no username.
    Unavailable(String),
}

/// The username `approved` acts as on its cluster, by the rule the deploy profile declares for it.
/// A cluster the profile does not list is `link`.
pub async fn kube_user(auth: &AgentAuth, approved: &Approved) -> anyhow::Result<ClusterUser> {
    let rule = auth
        .profile
        .cluster_identity(&approved.cluster)
        .cloned()
        .unwrap_or_default();
    let resolved =
        cluster_identity::resolve(&auth.pool, &approved.cluster, &rule, approved.person()).await?;
    Ok(ClusterUser::from(resolved))
}

impl From<Resolution> for ClusterUser {
    fn from(resolved: Resolution) -> Self {
        match resolved {
            Resolution::Known { user, .. } => Self::Known(user),
            Resolution::Unlinked => Self::Unlinked,
            Resolution::Unavailable(why) => Self::Unavailable(why),
        }
    }
}

/// A person's access token for `approved`, acting as `kube_user`.
pub fn mint(
    issuer: &AgentIssuer,
    approved: &Approved,
    kube_user: &KubeUser,
    now: u64,
) -> Result<String, MintError> {
    issuer.sign(&json!({
        "iss": issuer.issuer().as_str(),
        "sub": approved.login,
        "aud": approved.resource,
        "azp": CLIENT_ID,
        "iat": now,
        "nbf": now,
        "exp": now.saturating_add(ACCESS_TTL_SECS),
        "jti": token_id()?,
        "scope": approved.scopes.join(" "),
        "kube_user": kube_user.as_str(),
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
    pub redirect: LoopbackRedirect,
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
        redirect: LoopbackRedirect,
        state: Option<String>,
        error: &'static str,
        description: String,
    },
}

/// Check an authorization request against the one client and the configured audiences.
pub fn validate(
    config: &PeopleConfig,
    params: Map<String, Value>,
) -> Result<AuthorizeRequest, AuthorizeError> {
    let q: AuthorizeQuery = serde_json::from_value(Value::Object(params))
        .map_err(|e| AuthorizeError::Page(format!("malformed authorization request: {e}")))?;
    if q.client_id.as_deref() != Some(CLIENT_ID) {
        return Err(AuthorizeError::Page(format!(
            "unknown client_id {:?}; the only client is {CLIENT_ID}",
            q.client_id.unwrap_or_default()
        )));
    }
    let redirect = LoopbackRedirect::parse(q.redirect_uri.as_deref().unwrap_or_default())
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
    let audience = config.audience(&resource).ok_or_else(|| {
        back(
            "invalid_target",
            format!("resource {resource:?} is not one this issuer mints tokens for"),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pending {
    id: String,
    sub: String,
    request: AuthorizeRequest,
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
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{t}</title>\
         <style>body{{font-family:system-ui,sans-serif;max-width:40rem;margin:3rem auto;padding:0 1rem}}\
         textarea{{width:100%;font-family:monospace}}button{{margin-right:.5rem}}\
         .err{{color:#b00020}}</style></head><body><h1>{t}</h1>{body}</body></html>",
        t = escape(title),
    );
    let mut response = (status, Html(html)).into_response();
    let headers = response.headers_mut();
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'"),
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
    redirect: &LoopbackRedirect,
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

fn consent_page(
    pending: &Pending,
    login: &str,
    kube_user: &ClusterUser,
    error: Option<&str>,
) -> Response {
    let r = &pending.request;
    let identity = match kube_user {
        ClusterUser::Unavailable(why) => format!("<p class=\"err\">{}</p>", escape(why)),
        ClusterUser::Known(user) => format!(
            "<p>On cluster <b>{}</b> it acts as Kubernetes user <b>{}</b>, limited by that user's RBAC.</p>",
            escape(&r.cluster),
            escape(user.as_str())
        ),
        ClusterUser::Unlinked => format!(
            "<p>No Kubernetes username is known for you on cluster <b>{c}</b>. Link it from \
             <a href=\"/settings\" target=\"_blank\">Settings</a> by signing in to {c}, then \
             retry; or paste a token for {c} (<code>oc whoami -t</code>, or the ID token your \
             kubeconfig's OIDC login uses). The cluster names its owner and the token is not \
             kept.</p><textarea name=\"cluster_token\" rows=\"2\" autocomplete=\"off\"></textarea>",
            c = escape(&r.cluster)
        ),
    };
    let error = error
        .map(|e| format!("<p class=\"err\">{}</p>", escape(e)))
        .unwrap_or_default();
    let body = format!(
        "{error}<p>Claude Code asks to call <b>{resource}</b> as <b>{login}</b> with scopes \
         <code>{scopes}</code>.</p><form method=\"post\" action=\"{AUTHORIZE_PATH}\">{identity}\
         <input type=\"hidden\" name=\"id\" value=\"{id}\">\
         <p><button name=\"decision\" value=\"approve\">Allow</button>\
         <button name=\"decision\" value=\"deny\">Deny</button></p></form>",
        resource = escape(&r.resource),
        login = escape(login),
        scopes = escape(&r.scopes.join(" ")),
        id = escape(&pending.id),
    );
    page(StatusCode::OK, "Allow Claude Code?", &body)
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
        .route(AUTHORIZE_PATH, axum::routing::get(authorize).post(decide))
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
    let request = match single_valued(pairs)
        .map_err(AuthorizeError::Page)
        .and_then(|params| validate(&people.config, params))
    {
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
    show_consent(&auth, &session, pending, &claims, None).await
}

async fn show_consent(
    auth: &AgentAuth,
    session: &Session,
    pending: Pending,
    claims: &NativeClaims,
    error: Option<&str>,
) -> Response {
    let approved = approved(&pending, claims);
    let kube_user = match kube_user(auth, &approved).await {
        Ok(user) => user,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "resolving a person's kubernetes username");
            return refused_page("the controller could not resolve your kubernetes username");
        }
    };
    if let Err(e) = session.insert(PENDING_KEY, &pending).await {
        return session_failure(e);
    }
    consent_page(&pending, &claims.login, &kube_user, error)
}

fn approved(pending: &Pending, claims: &NativeClaims) -> Approved {
    Approved {
        sub: pending.sub.clone(),
        login: claims.login.clone(),
        email: claims.email.clone(),
        resource: pending.request.resource.clone(),
        cluster: pending.request.cluster.clone(),
        scopes: pending.request.scopes.clone(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    id: String,
    decision: String,
    cluster_token: Option<String>,
}

async fn decide(
    State(auth): State<Arc<AgentAuth>>,
    session: Session,
    Form(form): Form<Decision>,
) -> Response {
    let Some(people) = auth.people.as_ref() else {
        return refused_page("this controller issues no tokens to people");
    };
    let issuer = auth.issuer.issuer().as_str();
    let claims = match signed_in(&session).await {
        Ok(Some(claims)) => claims,
        Ok(None) => return refused_page("sign in again, then retry from Claude Code"),
        Err(e) => return session_failure(e),
    };
    let pending: Option<Pending> = match session.remove(PENDING_KEY).await {
        Ok(p) => p,
        Err(e) => return session_failure(e),
    };
    let Some(pending) = pending
        .filter(|p| constant_time_eq(p.id.as_bytes(), form.id.as_bytes()) && p.sub == claims.sub)
    else {
        return refused_page("this approval is no longer pending; retry from Claude Code");
    };
    let r = pending.request.clone();
    if form.decision != "approve" {
        return redirect_error(
            &r.redirect,
            Some(&r.state),
            issuer,
            "access_denied",
            "the person declined",
        );
    }
    if let Some(token) = form
        .cluster_token
        .as_deref()
        .filter(|t| !t.trim().is_empty())
    {
        match kube_links::link(
            &auth.pool,
            people.reviewer.as_ref(),
            &claims.sub,
            &r.cluster,
            token,
            LinkSource::TokenReview,
        )
        .await
        {
            Ok(_) => {}
            Err(LinkError::Store(e)) => {
                tracing::error!(error = %format!("{e:#}"), "storing a kubernetes identity link");
                return refused_page("the link could not be stored");
            }
            Err(e) => {
                tracing::info!(reason = %e, cluster = r.cluster, "kubernetes identity link refused");
                return show_consent(&auth, &session, pending, &claims, Some(&e.to_string())).await;
            }
        }
    }
    let approved = approved(&pending, &claims);
    match kube_user(&auth, &approved).await {
        Ok(ClusterUser::Known(_)) => {}
        Ok(ClusterUser::Unlinked) => {
            let why = format!("link your username on {} first", r.cluster);
            return show_consent(&auth, &session, pending, &claims, Some(&why)).await;
        }
        Ok(ClusterUser::Unavailable(why)) => return refused_page(&why),
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "resolving a person's kubernetes username");
            return refused_page("the controller could not resolve your kubernetes username");
        }
    }
    let now = crate::clock::unix_secs();
    let grant = CodeGrant {
        approved,
        redirect: r.redirect.clone(),
        challenge: r.challenge.clone(),
        expires_at: now.saturating_add(CODE_TTL_SECS),
    };
    match people.grants.issue_code(grant, now).await {
        Ok(code) => {
            tracing::info!(
                login = claims.login,
                resource = r.resource,
                "authorization code issued"
            );
            Redirect::to(
                &r.redirect
                    .with(&[("code", &code), ("state", &r.state), ("iss", issuer)]),
            )
            .into_response()
        }
        Err(e) => {
            tracing::warn!(reason = %e, "authorization code refused");
            redirect_error(
                &r.redirect,
                Some(&r.state),
                issuer,
                "temporarily_unavailable",
                "retry shortly",
            )
        }
    }
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

fn client(client_id: &str) -> Result<(), TokenError> {
    match client_id == CLIENT_ID {
        true => Ok(()),
        false => Err(TokenError::InvalidClient(format!(
            "client_id {client_id:?} is not {CLIENT_ID}"
        ))),
    }
}

fn same_resource(requested: Option<&str>, granted: &str) -> Result<(), TokenError> {
    match requested {
        Some(r) if r != granted => Err(TokenError::InvalidTarget(format!(
            "resource {r:?} is not the approved {granted:?}"
        ))),
        _ => Ok(()),
    }
}

impl From<GrantError> for TokenError {
    fn from(e: GrantError) -> Self {
        match e {
            GrantError::Unknown | GrantError::Expired => Self::InvalidGrant(e.to_string()),
            GrantError::Full | GrantError::Random => Self::Server(e.to_string()),
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
    client(&req.client_id)?;
    let grant = people.grants.take_code(&req.code, now).await?;
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
    issue(
        auth,
        people,
        grant.approved,
        now,
        now.saturating_add(REFRESH_TTL_SECS),
    )
    .await
}

/// `grant_type=refresh_token`, rotating: the presented token is spent and a new one returned.
pub(crate) async fn redeem_refresh(
    auth: &AgentAuth,
    people: &People,
    fields: Map<String, Value>,
    now: u64,
) -> Result<Response, TokenError> {
    let req: RefreshRequest = parse(fields)?;
    client(&req.client_id)?;
    let grant = people.grants.take_refresh(&req.refresh_token, now).await?;
    same_resource(req.resource.as_deref(), &grant.approved.resource)?;
    if let Some(scope) = req.scope.as_deref() {
        let wider: Vec<&str> = scope
            .split_whitespace()
            .filter(|s| {
                !IGNORED_SCOPES.contains(s) && !grant.approved.scopes.iter().any(|g| g == s)
            })
            .collect();
        if !wider.is_empty() {
            return Err(TokenError::InvalidGrant(format!(
                "refresh asks for scopes {wider:?} beyond the approval"
            )));
        }
    }
    issue(auth, people, grant.approved, now, grant.expires_at).await
}

async fn issue(
    auth: &AgentAuth,
    people: &People,
    approved: Approved,
    now: u64,
    refresh_expires_at: u64,
) -> Result<Response, TokenError> {
    if people.config.audience(&approved.resource).is_none() {
        return Err(TokenError::InvalidTarget(format!(
            "resource {:?} is no longer configured",
            approved.resource
        )));
    }
    let kube_user = match kube_user(auth, &approved)
        .await
        .map_err(|e| TokenError::Server(format!("{e:#}")))?
    {
        ClusterUser::Known(user) => user,
        ClusterUser::Unlinked => {
            return Err(TokenError::InvalidGrant(format!(
                "{} has no kubernetes username on {}",
                approved.login, approved.cluster
            )));
        }
        ClusterUser::Unavailable(why) => return Err(TokenError::InvalidGrant(why)),
    };
    let access_token = mint(&auth.issuer, &approved, &kube_user, now)
        .map_err(|e| TokenError::Server(e.to_string()))?;
    let refresh_token = people
        .grants
        .issue_refresh(
            RefreshGrant {
                approved: approved.clone(),
                expires_at: refresh_expires_at,
            },
            now,
        )
        .await?;
    tracing::info!(
        sub = approved.login,
        kube_user = kube_user.as_str(),
        cluster = approved.cluster,
        aud = approved.resource,
        "person token issued"
    );
    let mut response = axum::Json(json!({
        "access_token": access_token,
        "token_type": "Bearer",
        "expires_in": ACCESS_TTL_SECS,
        "scope": approved.scopes.join(" "),
        "refresh_token": refresh_token,
    }))
    .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

#[cfg(test)]
mod tests {
    use crate::agent_auth::people::*;
    use crate::agent_auth::testing::{self, ISSUER, Spire};
    use crate::identity::kube_links::testing::Clusters;
    use axum::body::Body;
    use axum::http::Request;
    use sqlx::PgPool;
    use tower::ServiceExt;

    const PIRATE_KUEUE: &str = "https://crucible-kueue.apps.pirate.example/mcp";
    const MPP_KUEUE: &str = "https://crucible-kueue.apps.mpp.example/mcp";
    const WALDORF_KUEUE: &str = "http://localhost:8852/mcp";
    const REDIRECT: &str = "http://localhost:8765/callback";
    /// RFC 7636 appendix B.
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    const SUB: &str = "f:sso:weaton";

    fn config() -> PeopleConfig {
        PeopleConfig::from_lookup(Some(format!(
            "pirate:{PIRATE_KUEUE}=kueue.read kueue.submit, mpp:{MPP_KUEUE}=kueue.read, \
             waldorf:{WALDORF_KUEUE}=kueue.read, nowhere:https://unlisted.example/mcp=kueue.read"
        )))
        .unwrap()
        .unwrap()
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
    fn only_canonical_loopback_callbacks_are_redirect_targets() {
        for good in [
            "http://localhost:8765/callback",
            "http://127.0.0.1:49152/callback",
            "http://[::1]:8765/callback",
        ] {
            assert_eq!(
                LoopbackRedirect::parse(good).map(|r| r.as_str().to_string()),
                Ok(good.to_string())
            );
        }
        for bad in [
            "https://localhost:8765/callback",
            "http://localhost/callback",
            "http://localhost:80/callback",
            "http://localhost:0/callback",
            "http://localhost:8765/",
            "http://localhost:8765/callback/",
            "http://localhost:8765/Callback",
            "http://localhost:8765/x/../callback",
            "http://localhost:8765/callback?x=1",
            "http://localhost:8765/callback#f",
            "http://user@localhost:8765/callback",
            "http://LOCALHOST:8765/callback",
            "http://localhost.evil.example:8765/callback",
            "http://127.0.0.2:8765/callback",
            "http://0.0.0.0:8765/callback",
            "http://[::ffff:127.0.0.1]:8765/callback",
            "http://evil.example/callback",
            "custom-scheme://callback",
            "",
        ] {
            assert!(
                LoopbackRedirect::parse(bad).is_err(),
                "{bad:?} must be refused"
            );
        }
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

    #[test]
    fn a_valid_request_names_its_audience_cluster_and_every_scope_it_offers() {
        let r = validate(
            &config(),
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
        .unwrap();
        assert_eq!(r.cluster, "pirate");
        assert_eq!(r.resource, PIRATE_KUEUE);
        assert_eq!(r.scopes, ["kueue.read", "kueue.submit"]);
        assert_eq!(r.state, "st-1");
    }

    #[test]
    fn bad_clients_and_redirects_are_shown_to_the_browser_never_redirected() {
        for edit in [
            ("client_id", "someone-else"),
            ("client_id", ""),
            ("redirect_uri", "https://evil.example/callback"),
            ("redirect_uri", "http://localhost:8765/other"),
            ("redirect_uri", ""),
        ] {
            let pairs = request(&[edit]);
            let got = validate(&config(), single_valued(pairs).unwrap());
            assert!(
                matches!(got, Err(AuthorizeError::Page(_))),
                "{edit:?}: {got:?}"
            );
        }
    }

    #[test]
    fn other_refusals_go_back_to_the_client_with_its_state() {
        for (edit, want) in [
            (("resource", "https://other.example/mcp"), "invalid_target"),
            (("resource", ""), "invalid_target"),
            (
                ("resource", MPP_KUEUE.trim_end_matches("/mcp")),
                "invalid_target",
            ),
            (("response_type", "token"), "unsupported_response_type"),
            (("code_challenge_method", "plain"), "invalid_request"),
            (("code_challenge_method", ""), "invalid_request"),
            (("code_challenge", "short"), "invalid_request"),
            (("scope", "kueue.read buildit.build"), "invalid_scope"),
        ] {
            match validate(&config(), single_valued(request(&[edit])).unwrap()) {
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
                    single_valued(request(&[("state", state)])).unwrap()
                ),
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
            login: "weaton".into(),
            email: Some("weaton@redhat.com".into()),
            resource: resource.into(),
            cluster: cluster.into(),
            scopes: vec!["kueue.read".into()],
        }
    }

    fn code_grant(expires_at: u64) -> CodeGrant {
        CodeGrant {
            approved: approved(PIRATE_KUEUE, "pirate"),
            redirect: LoopbackRedirect::parse(REDIRECT).unwrap(),
            challenge: CodeChallenge::parse(CHALLENGE).unwrap(),
            expires_at,
        }
    }

    #[tokio::test]
    async fn a_code_works_once_and_not_after_it_expires() {
        let grants = Grants::default();
        let code = grants.issue_code(code_grant(160), 100).await.unwrap();
        assert!(code.len() >= 43);
        assert_eq!(
            grants.take_code(&code, 110).await.unwrap().approved.login,
            "weaton"
        );
        assert_eq!(
            grants.take_code(&code, 110).await.unwrap_err(),
            GrantError::Unknown
        );
        let code = grants.issue_code(code_grant(160), 100).await.unwrap();
        assert_eq!(
            grants.take_code(&code, 160).await.unwrap_err(),
            GrantError::Expired
        );
        assert_eq!(
            grants.take_code(&code, 110).await.unwrap_err(),
            GrantError::Unknown,
            "an expired code is spent too"
        );
        assert_eq!(
            grants.take_code("nope", 110).await.unwrap_err(),
            GrantError::Unknown
        );
    }

    #[tokio::test]
    async fn expired_codes_are_pruned_and_the_store_is_bounded() {
        let grants = Grants::default();
        for _ in 0..MAX_GRANTS {
            grants.issue_code(code_grant(150), 100).await.unwrap();
        }
        assert_eq!(
            grants.issue_code(code_grant(150), 100).await.unwrap_err(),
            GrantError::Full
        );
        grants.issue_code(code_grant(300), 200).await.unwrap();
        assert_eq!(grants.codes.lock().await.len(), 1);
    }

    #[test]
    fn a_claim_rule_takes_the_login_or_email_with_its_prefix() {
        let person = approved(PIRATE_KUEUE, "c1");
        let rule = |claim, prefix: &str| forge::fleet::ClaimIdentity {
            claim,
            prefix: prefix.into(),
        };
        let known = |u: &str| ClusterUser::Known(KubeUser::parse(u).unwrap());
        let from_claim = |rule: &forge::fleet::ClaimIdentity, a: &Approved| {
            ClusterUser::from(cluster_identity::from_claim(rule, &a.cluster, a.person()))
        };
        assert_eq!(
            from_claim(&rule(forge::fleet::SsoClaim::Login, ""), &person),
            known("weaton")
        );
        assert_eq!(
            from_claim(&rule(forge::fleet::SsoClaim::Email, ""), &person),
            known("weaton@redhat.com")
        );
        assert_eq!(
            from_claim(&rule(forge::fleet::SsoClaim::Email, "oidc:"), &person),
            known("oidc:weaton@redhat.com")
        );
        assert_eq!(
            from_claim(&rule(forge::fleet::SsoClaim::Login, "sso#"), &person),
            known("sso#weaton")
        );
        for email in [None, Some(String::new())] {
            let mut no_email = person.clone();
            no_email.email = email;
            assert!(matches!(
                from_claim(&rule(forge::fleet::SsoClaim::Email, ""), &no_email),
                ClusterUser::Unavailable(why) if why.contains("email claim")
            ));
        }
        assert!(
            matches!(
                from_claim(&rule(forge::fleet::SsoClaim::Login, "system:"), &person),
                ClusterUser::Unavailable(_)
            ),
            "a prefix cannot turn a login into a system identity"
        );
        assert!(matches!(
            from_claim(&rule(forge::fleet::SsoClaim::Login, "a b:"), &person),
            ClusterUser::Unavailable(_)
        ));
    }

    struct Harness {
        auth: Arc<AgentAuth>,
        app: axum::Router,
    }

    async fn test_login(session: Session) -> StatusCode {
        let claims = NativeClaims {
            sub: SUB.into(),
            login: "weaton".into(),
            email: Some("weaton@redhat.com".into()),
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
        let spire = Spire::start().await;
        let profile = format!(
            r#"{}
[clusters.pirate]
kubeconfig_secret = "pirate-kubeconfig"
[clusters.mpp]
kubeconfig_secret = "mpp-kubeconfig"
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
        auth.people = Some(People::new(
            config(),
            Arc::new(Clusters::with(&[
                ("pirate", "sha256~pirate", "wseaton"),
                ("mpp", "sha256~mpp", "weaton"),
            ])),
        ));
        let auth = Arc::new(auth);
        let store = crate::identity::session::store(pool).await.unwrap();
        let app = router(auth.clone())
            .merge(crate::agent_auth::router(auth.clone()))
            .route("/test/login", axum::routing::get(test_login))
            .layer(crate::identity::session::layer(store, false));
        Harness { auth, app }
    }

    struct Reply {
        status: StatusCode,
        location: Option<String>,
        cookie: Option<String>,
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
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        Reply {
            status,
            location,
            cookie,
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

    fn pending_id(page: &str) -> String {
        let at = page.find("name=\"id\" value=\"").unwrap() + "name=\"id\" value=\"".len();
        page[at..].split('"').next().unwrap().to_string()
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
        let consent = send(&h.app, get(&uri, Some(&cookie))).await;
        assert_eq!(consent.status, StatusCode::OK);
        assert!(
            consent.body.contains("name=\"cluster_token\""),
            "no link yet: {}",
            consent.body
        );
        assert!(consent.body.contains(PIRATE_KUEUE));
        let id = pending_id(&consent.body);

        let wrong_cluster = send(
            &h.app,
            post(
                AUTHORIZE_PATH,
                Some(&cookie),
                &pairs(&[
                    ("id", &id),
                    ("decision", "approve"),
                    ("cluster_token", "sha256~mpp"),
                ]),
            ),
        )
        .await;
        assert_eq!(wrong_cluster.status, StatusCode::OK);
        assert!(
            wrong_cluster.body.contains("did not accept"),
            "{}",
            wrong_cluster.body
        );
        assert_eq!(
            crate::identity::kube_links::get(&pool, SUB, "pirate")
                .await
                .unwrap(),
            None
        );

        let approved = send(
            &h.app,
            post(
                AUTHORIZE_PATH,
                Some(&cookie),
                &pairs(&[
                    ("id", &id),
                    ("decision", "approve"),
                    ("cluster_token", "sha256~pirate"),
                ]),
            ),
        )
        .await;
        assert_eq!(approved.status, StatusCode::SEE_OTHER, "{}", approved.body);
        let location = approved.location.unwrap();
        assert!(location.starts_with(&format!("{REDIRECT}?")), "{location}");
        let q = query_of(&location);
        assert_eq!(q["state"], "st-1");
        assert_eq!(q["iss"], ISSUER);
        let code = q["code"].clone();

        let replayed = send(
            &h.app,
            post(
                AUTHORIZE_PATH,
                Some(&cookie),
                &pairs(&[("id", &id), ("decision", "approve")]),
            ),
        )
        .await;
        assert_eq!(
            replayed.status,
            StatusCode::BAD_REQUEST,
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
        assert_eq!(claims["sub"], "weaton");
        assert_eq!(claims["aud"], PIRATE_KUEUE);
        assert_eq!(claims["kube_user"], "wseaton");
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
        let consent = send(
            &h.app,
            get(&authorize_uri(&[("resource", MPP_KUEUE)]), Some(&cookie)),
        )
        .await;
        assert!(!consent.body.contains("cluster_token"), "{}", consent.body);
        assert!(consent.body.contains("Kubernetes user <b>weaton</b>"));
        let approved = send(
            &h.app,
            post(
                AUTHORIZE_PATH,
                Some(&cookie),
                &pairs(&[("id", &pending_id(&consent.body)), ("decision", "approve")]),
            ),
        )
        .await;
        let code = query_of(&approved.location.unwrap())["code"].clone();
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
        let claims = claims_of(&h.auth, body["access_token"].as_str().unwrap(), MPP_KUEUE);
        assert_eq!(claims["kube_user"], "weaton");
        assert_eq!(claims["scope"], "kueue.read");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_email_cluster_takes_the_recorded_email_and_an_unlisted_cluster_needs_a_link(
        pool: PgPool,
    ) {
        let h = harness(&pool).await;
        let cookie = send(&h.app, get("/test/login", None)).await.cookie.unwrap();
        let consent = send(
            &h.app,
            get(
                &authorize_uri(&[("resource", WALDORF_KUEUE)]),
                Some(&cookie),
            ),
        )
        .await;
        assert!(
            consent
                .body
                .contains("Kubernetes user <b>oidc:weaton@redhat.com</b>"),
            "{}",
            consent.body
        );
        let approved = send(
            &h.app,
            post(
                AUTHORIZE_PATH,
                Some(&cookie),
                &pairs(&[("id", &pending_id(&consent.body)), ("decision", "approve")]),
            ),
        )
        .await;
        let code = query_of(&approved.location.unwrap())["code"].clone();
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
        assert_eq!(claims["kube_user"], "oidc:weaton@redhat.com");
        assert_eq!(claims["kube_cluster"], "waldorf");

        let unlisted = send(
            &h.app,
            get(
                &authorize_uri(&[("resource", "https://unlisted.example/mcp")]),
                Some(&cookie),
            ),
        )
        .await;
        assert!(
            unlisted.body.contains("name=\"cluster_token\""),
            "{}",
            unlisted.body
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_unlinked_person_cannot_approve_and_a_denial_goes_back_to_the_client(pool: PgPool) {
        let h = harness(&pool).await;
        let cookie = send(&h.app, get("/test/login", None)).await.cookie.unwrap();
        let consent = send(&h.app, get(&authorize_uri(&[]), Some(&cookie))).await;
        let id = pending_id(&consent.body);
        let unlinked = send(
            &h.app,
            post(
                AUTHORIZE_PATH,
                Some(&cookie),
                &pairs(&[("id", &id), ("decision", "approve")]),
            ),
        )
        .await;
        assert_eq!(unlinked.status, StatusCode::OK);
        assert!(unlinked.body.contains("link your username on pirate first"));
        let forged = send(
            &h.app,
            post(
                AUTHORIZE_PATH,
                Some(&cookie),
                &pairs(&[("id", "0000"), ("decision", "approve")]),
            ),
        )
        .await;
        assert_eq!(
            forged.status,
            StatusCode::BAD_REQUEST,
            "a forged id spends the approval"
        );
        let consent = send(&h.app, get(&authorize_uri(&[]), Some(&cookie))).await;
        let denied = send(
            &h.app,
            post(
                AUTHORIZE_PATH,
                Some(&cookie),
                &pairs(&[("id", &pending_id(&consent.body)), ("decision", "deny")]),
            ),
        )
        .await;
        let q = query_of(&denied.location.unwrap());
        assert_eq!(q["error"], "access_denied");
        assert_eq!(q["state"], "st-1");
        let no_session = send(
            &h.app,
            post(
                AUTHORIZE_PATH,
                None,
                &pairs(&[("id", &id), ("decision", "approve")]),
            ),
        )
        .await;
        assert_eq!(no_session.status, StatusCode::BAD_REQUEST);
    }

    async fn issued_code(h: &Harness, pool: &PgPool) -> String {
        crate::identity::kube_links::link(
            pool,
            &Clusters::with(&[("pirate", "t", "wseaton")]),
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
            .issue_code(code_grant(now + CODE_TTL_SECS), now)
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
                ("resource", MPP_KUEUE),
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

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn refresh_tokens_rotate_and_never_widen(pool: PgPool) {
        let h = harness(&pool).await;
        let code = issued_code(&h, &pool).await;
        let (_, first) = token(
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
        let refresh = first["refresh_token"].as_str().unwrap().to_string();
        let (status, second) = token(
            &h.app,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh),
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
        assert_eq!(claims["kube_user"], "wseaton");
        let (status, body) = token(
            &h.app,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh),
                ("client_id", CLIENT_ID),
            ],
        )
        .await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_grant"))
        );
        let (status, body) = token(
            &h.app,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", second["refresh_token"].as_str().unwrap()),
                ("client_id", CLIENT_ID),
                ("scope", "kueue.read kueue.admin"),
            ],
        )
        .await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_grant"))
        );
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
        let agents_only = serde_json::to_value(h.auth.issuer.metadata(None)).unwrap();
        for absent in [
            "authorization_endpoint",
            "code_challenge_methods_supported",
            "authorization_response_iss_parameter_supported",
        ] {
            assert!(agents_only.get(absent).is_none(), "{absent}");
        }
    }
}
