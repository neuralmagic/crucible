//! The controller as an OIDC relying party against Keycloak.
//!
//! Two credentials are minted or checked here and nowhere else:
//!
//! ```text
//!   browser  -> /auth/login -> authorization code + PKCE + state + nonce -> /auth/callback
//!               -> ID token validated against the issuer's cached JWKS -> a native session
//!   CLI      -> a Keycloak ACCESS token presented as a bearer, validated against the same JWKS,
//!               required to be typ=Bearer with azp == the public device-flow client
//! ```
//!
//! Discovery, the code exchange, and ID token validation run on [`openidconnect`]; the access-token
//! path runs on `jsonwebtoken` over the issuer's JWKS, because it turns on two claims
//! (`typ`, `azp`) that no ID token verifier checks.
//!
//! Everything in here is lazy. The issuer is a separate deployment that may be down when the
//! controller boots, so discovery happens on first use behind a single-flight TTL cache and a
//! failure is a 503 for that one request, never a failed startup.

pub mod claims_probe;
pub mod credentials;
pub mod routes;
pub mod users;

#[cfg(test)]
mod keycloak_e2e;
#[cfg(test)]
mod tests;

use anyhow::Context;
use openidconnect::core::{
    CoreAuthDisplay, CoreAuthPrompt, CoreClaimName, CoreClaimType, CoreClientAuthMethod,
    CoreErrorResponseType, CoreGenderClaim, CoreGrantType, CoreJsonWebKey,
    CoreJweContentEncryptionAlgorithm, CoreJweKeyManagementAlgorithm, CoreJwsSigningAlgorithm,
    CoreResponseMode, CoreResponseType, CoreRevocableToken, CoreRevocationErrorResponse,
    CoreSubjectIdentifierType, CoreTokenIntrospectionResponse, CoreTokenType,
};
use openidconnect::{
    AdditionalClaims, AdditionalProviderMetadata, Client, ClientId, ClientSecret,
    EmptyExtraTokenFields, EndpointMaybeSet, EndpointNotSet, EndpointSet, HttpRequest,
    HttpResponse, IdTokenFields, IssuerUrl, ProviderMetadata, RedirectUrl, StandardErrorResponse,
    StandardTokenResponse,
};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a discovery document and a JWKS are reused before they are fetched again. Short enough
/// that a key rotation heals on its own, long enough that a login is not a discovery round trip.
const DISCOVERY_TTL: Duration = Duration::from_secs(600);

/// The shortest gap between two JWKS re-fetches forced by a JWT naming a key the cached set does
/// not hold. Presenting such a JWT needs no credential, so without this an anonymous caller turns
/// every request into a request against the issuer.
const JWKS_MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// How far past its `exp` a bearer JWT is still accepted, for clock skew between the issuer and
/// this process.
const EXPIRY_LEEWAY: Duration = Duration::from_secs(5);

/// The claim the controller reads group membership from, and the Keycloak-shaped fallback. A realm
/// that emits neither leaves every caller group-less, which is the difference between
/// `auth.operatorGroups` granting write access and granting nothing.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct GroupClaims {
    #[serde(default)]
    groups: Vec<String>,
    #[serde(default)]
    realm_access: Option<RealmAccess>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct RealmAccess {
    #[serde(default)]
    roles: Vec<String>,
}

impl AdditionalClaims for GroupClaims {}

impl GroupClaims {
    /// The asserted groups, normalized exactly like [`crate::identity::auth::Groups`]: trimmed, lowercased,
    /// full paths kept, so an ownership check still compares whole paths.
    fn normalized(&self) -> Vec<String> {
        let source = if self.groups.is_empty() {
            self.realm_access
                .as_ref()
                .map(|r| r.roles.as_slice())
                .unwrap_or_default()
        } else {
            self.groups.as_slice()
        };
        source
            .iter()
            .map(|g| g.trim().to_lowercase())
            .filter(|g| !g.is_empty())
            .collect()
    }
}

/// The provider metadata outside what the core type models: RP-initiated logout's endpoint, so
/// `/auth/logout` ends the session at the issuer and not only here, and the revocation endpoint a
/// revoked offline credential is reported dead to.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExtraMetadata {
    #[serde(default)]
    end_session_endpoint: Option<String>,
    #[serde(default)]
    revocation_endpoint: Option<String>,
}

impl AdditionalProviderMetadata for ExtraMetadata {}

type Metadata = ProviderMetadata<
    ExtraMetadata,
    CoreAuthDisplay,
    CoreClientAuthMethod,
    CoreClaimName,
    CoreClaimType,
    CoreGrantType,
    CoreJweContentEncryptionAlgorithm,
    CoreJweKeyManagementAlgorithm,
    CoreJsonWebKey,
    CoreResponseMode,
    CoreResponseType,
    CoreSubjectIdentifierType,
>;

type Fields = IdTokenFields<
    GroupClaims,
    EmptyExtraTokenFields,
    CoreGenderClaim,
    CoreJweContentEncryptionAlgorithm,
    CoreJwsSigningAlgorithm,
>;

type TokenResponse = StandardTokenResponse<Fields, CoreTokenType>;

/// The relying-party client, at the endpoint typestate [`Client::from_provider_metadata`] returns:
/// authorization set, token and userinfo maybe-set, everything else unset.
type RelyingParty = Client<
    GroupClaims,
    CoreAuthDisplay,
    CoreGenderClaim,
    CoreJweContentEncryptionAlgorithm,
    CoreJsonWebKey,
    CoreAuthPrompt,
    StandardErrorResponse<CoreErrorResponseType>,
    TokenResponse,
    CoreTokenIntrospectionResponse,
    CoreRevocableToken,
    CoreRevocationErrorResponse,
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

/// The relying party's registration, read off the environment.
#[derive(Debug, Clone)]
pub struct OidcCfg {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    /// The callback URL registered with the issuer. Must be this controller's own
    /// `<external base>/auth/callback`.
    pub redirect_url: String,
    /// The scopes the authorization request asks for, verbatim. `offline_access` belongs here so the
    /// refresh credential a scheduled launch needs survives logout; `groups` does NOT unless the
    /// realm registered it as a scope, because an unregistered scope is `invalid_scope` and breaks
    /// login outright.
    pub scopes: Vec<String>,
    /// The PUBLIC device-flow client id. A Keycloak access token is accepted as a bearer only when
    /// its `azp` is this; empty turns the JWT bearer path off.
    pub device_client_id: Option<String>,
    /// Where the issuer sends the browser after RP-initiated logout.
    pub post_logout_redirect: Option<String>,
}

/// The default scope set: what the relying party asks for when the deploy names none.
const DEFAULT_SCOPES: [&str; 4] = ["openid", "email", "profile", "offline_access"];

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl OidcCfg {
    /// Read the registration off the environment. `None` when no issuer is configured, which is
    /// what a proxy-mode or local deployment looks like.
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        let Some(issuer) = env_value("CONTROLLER_OIDC_ISSUER") else {
            return Ok(None);
        };
        let client_id = env_value("CONTROLLER_OIDC_CLIENT_ID").context(
            "CONTROLLER_OIDC_ISSUER is set without CONTROLLER_OIDC_CLIENT_ID, so the controller has no registration to run the flow with",
        )?;
        let redirect_url = env_value("CONTROLLER_OIDC_REDIRECT_URL").context(
            "CONTROLLER_OIDC_ISSUER is set without CONTROLLER_OIDC_REDIRECT_URL, and a code flow cannot guess its own callback",
        )?;
        let scopes = env_value("CONTROLLER_OIDC_SCOPES")
            .map(|raw| {
                raw.split([',', ' '])
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect());
        Ok(Some(OidcCfg {
            issuer,
            client_id,
            client_secret: env_value("CONTROLLER_OIDC_CLIENT_SECRET"),
            redirect_url,
            scopes,
            device_client_id: env_value("CONTROLLER_OIDC_DEVICE_CLIENT_ID"),
            post_logout_redirect: env_value("CONTROLLER_OIDC_POST_LOGOUT_REDIRECT"),
        }))
    }
}

/// What a validated credential says about the caller. The one shape both the browser callback and
/// the JWT bearer path produce, so a session and a token can never disagree about what identity is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedClaims {
    pub sub: String,
    pub login: String,
    pub email: Option<String>,
    pub groups: Vec<String>,
}

/// Why a credential was refused. Split so the callers can answer 401 for a bad credential and 503
/// for an issuer that could not be reached — never the other way around.
#[derive(Debug, Clone, thiserror::Error)]
pub enum OidcError {
    #[error("the identity provider could not be reached: {0}")]
    Unavailable(String),
    #[error("{0}")]
    Rejected(String),
}

impl OidcError {
    fn rejected(what: impl std::fmt::Display) -> Self {
        OidcError::Rejected(what.to_string())
    }
}

/// A discovered issuer: the client the flow runs on, plus the endpoints and key set the two
/// validation paths read.
struct Discovered {
    client: RelyingParty,
    end_session_endpoint: Option<String>,
    userinfo_endpoint: Option<String>,
    /// Where a revoked offline credential is told it is dead. `None` when the issuer publishes
    /// none, which makes a revoke local-only.
    revocation_endpoint: Option<String>,
    jwks_uri: String,
    issuer: String,
}

/// The relying party. One per process, shared by the auth middleware and the `/auth/*` routes.
pub struct OidcProvider {
    cfg: OidcCfg,
    http: reqwest::Client,
    discovery: moka::future::Cache<(), Arc<Discovered>>,
    jwks: moka::future::Cache<(), Arc<jsonwebtoken::jwk::JwkSet>>,
    forced_jwks_refresh: Mutex<Option<Instant>>,
}

/// The HTTP bridge's error. Distinct from [`OidcError`] because `openidconnect` needs a
/// `std::error::Error` of its own choosing on the client type.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("oidc http request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("oidc http response was malformed: {0}")]
    Response(#[from] axum::http::Error),
}

type BridgeFuture = Pin<Box<dyn Future<Output = Result<HttpResponse, BridgeError>> + Send>>;

/// `openidconnect` over this crate's own reqwest client. Redirects are never followed: an
/// authorization server that 302s a token request is not one whose answer may be trusted.
fn bridge(client: reqwest::Client) -> impl Fn(HttpRequest) -> BridgeFuture {
    move |req: HttpRequest| {
        let client = client.clone();
        Box::pin(async move {
            let (parts, body) = req.into_parts();
            let mut builder = client.request(parts.method, parts.uri.to_string());
            for (name, value) in parts.headers.iter() {
                builder = builder.header(name, value);
            }
            let res = builder.body(body).send().await?;
            let status = res.status();
            let headers = res.headers().clone();
            let bytes = res.bytes().await?;
            let mut out = axum::http::Response::builder().status(status);
            if let Some(existing) = out.headers_mut() {
                *existing = headers;
            }
            Ok(out.body(bytes.to_vec())?)
        })
    }
}

impl OidcProvider {
    pub fn new(cfg: OidcCfg) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .build()
            .context("building the oidc http client")?;
        Ok(OidcProvider {
            cfg,
            http,
            discovery: moka::future::Cache::builder()
                .max_capacity(1)
                .time_to_live(DISCOVERY_TTL)
                .build(),
            jwks: moka::future::Cache::builder()
                .max_capacity(1)
                .time_to_live(DISCOVERY_TTL)
                .build(),
            forced_jwks_refresh: Mutex::new(None),
        })
    }

    /// `None` when the configured redirect URL does not parse as an absolute URL with a path.
    /// The path component of the configured redirect URL: where the issuer sends the browser back,
    /// and therefore where this controller must answer.
    pub fn callback_path(&self) -> Option<String> {
        reqwest::Url::parse(&self.cfg.redirect_url)
            .ok()
            .map(|u| u.path().to_string())
            .filter(|p| p.starts_with('/'))
    }

    pub fn from_env() -> anyhow::Result<Option<Arc<Self>>> {
        OidcCfg::from_env()?
            .map(|cfg| OidcProvider::new(cfg).map(Arc::new))
            .transpose()
    }

    pub fn cfg(&self) -> &OidcCfg {
        &self.cfg
    }

    /// The discovery document, fetched at most once per [`DISCOVERY_TTL`] and single-flighted
    /// across concurrent logins.
    async fn discovered(&self) -> Result<Arc<Discovered>, OidcError> {
        self.discovery
            .try_get_with((), async {
                let issuer = IssuerUrl::new(self.cfg.issuer.clone())
                    .map_err(|e| OidcError::Rejected(format!("CONTROLLER_OIDC_ISSUER: {e}")))?;
                let metadata = Metadata::discover_async(issuer, &bridge(self.http.clone()))
                    .await
                    .map_err(|e| OidcError::Unavailable(format!("discovery: {e}")))?;
                let end_session_endpoint =
                    metadata.additional_metadata().end_session_endpoint.clone();
                let revocation_endpoint =
                    metadata.additional_metadata().revocation_endpoint.clone();
                let jwks_uri = metadata.jwks_uri().to_string();
                let issuer = metadata.issuer().to_string();
                let userinfo_endpoint = metadata.userinfo_endpoint().map(|u| u.to_string());
                let redirect = RedirectUrl::new(self.cfg.redirect_url.clone()).map_err(|e| {
                    OidcError::Rejected(format!("CONTROLLER_OIDC_REDIRECT_URL: {e}"))
                })?;
                let client = RelyingParty::from_provider_metadata(
                    metadata,
                    ClientId::new(self.cfg.client_id.clone()),
                    self.cfg.client_secret.clone().map(ClientSecret::new),
                )
                .set_redirect_uri(redirect);
                Ok::<_, OidcError>(Arc::new(Discovered {
                    client,
                    end_session_endpoint,
                    userinfo_endpoint,
                    revocation_endpoint,
                    jwks_uri,
                    issuer,
                }))
            })
            .await
            .map_err(|e: Arc<OidcError>| e.as_ref().clone())
    }

    /// The issuer's key set, by kid. `refresh` forces a re-fetch, which is what an unknown kid
    /// (a rotation inside the TTL) asks for, subject to [`Self::claim_forced_jwks_refresh`].
    async fn jwks(&self, refresh: bool) -> Result<Arc<jsonwebtoken::jwk::JwkSet>, OidcError> {
        if refresh {
            self.jwks.invalidate(&()).await;
        }
        let uri = self.discovered().await?.jwks_uri.clone();
        let http = self.http.clone();
        self.jwks
            .try_get_with((), async move {
                let res = http
                    .get(&uri)
                    .send()
                    .await
                    .map_err(|e| OidcError::Unavailable(format!("fetching the jwks: {e}")))?;
                if !res.status().is_success() {
                    return Err(OidcError::Unavailable(format!(
                        "the jwks endpoint answered {}",
                        res.status()
                    )));
                }
                let set: jsonwebtoken::jwk::JwkSet = res
                    .json()
                    .await
                    .map_err(|e| OidcError::Unavailable(format!("parsing the jwks: {e}")))?;
                Ok::<_, OidcError>(Arc::new(set))
            })
            .await
            .map_err(|e: Arc<OidcError>| e.as_ref().clone())
    }

    /// Take the right to force a JWKS re-fetch, granted at most once per
    /// [`JWKS_MIN_REFRESH_INTERVAL`]. Taking it stamps the clock whether or not the fetch that
    /// follows succeeds: the limit bounds requests sent, not requests answered.
    fn claim_forced_jwks_refresh(&self, now: Instant) -> bool {
        let mut last = self
            .forced_jwks_refresh
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match *last {
            Some(at) if now.duration_since(at) < JWKS_MIN_REFRESH_INTERVAL => false,
            _ => {
                *last = Some(now);
                true
            }
        }
    }

    /// The issuer's RP-initiated logout endpoint, when it publishes one.
    pub async fn end_session_endpoint(&self) -> Result<Option<String>, OidcError> {
        Ok(self.discovered().await?.end_session_endpoint.clone())
    }
}

/// The claims the JWT bearer path reads. Everything the spec-shaped verifier does not check
/// (`typ`, `azp`) is checked by hand below, because those two are what separate a CLI's access
/// token from an ID token or a token minted for a different application on the same realm.
#[derive(Debug, Deserialize)]
struct AccessTokenClaims {
    sub: String,
    #[serde(default)]
    typ: Option<String>,
    #[serde(default)]
    azp: Option<String>,
    #[serde(default)]
    preferred_username: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(flatten)]
    groups: GroupClaims,
    /// Present on an ID token and never on an access token; a second, independent reason to refuse.
    #[serde(default)]
    nonce: Option<String>,
}

impl OidcProvider {
    /// Validate a Keycloak ACCESS token presented as a bearer.
    ///
    /// Signature, issuer, and expiry come from the issuer's JWKS. On top of those: `typ` must be
    /// `Bearer`, `azp` must be the configured public device-flow client, and a token whose `azp` is
    /// this relying party (or that carries a `nonce`, so an ID token) is refused outright — those
    /// are credentials minted for the browser, not for a CLI.
    pub async fn verify_access_token(&self, token: &str) -> Result<VerifiedClaims, OidcError> {
        let Some(device_client) = self.cfg.device_client_id.as_deref() else {
            return Err(OidcError::rejected(
                "this deployment accepts no JWT bearers: no device-flow client is configured",
            ));
        };
        let claims = self.decode_access_token(token).await?;
        // Keycloak copies the authorization request's nonce onto the access token it mints for a
        // browser login, so a bearer carrying one came out of the web flow, not the device flow.
        if claims.nonce.is_some() {
            return Err(OidcError::rejected(
                "a token from a browser login is not accepted as a bearer",
            ));
        }
        let azp = claims
            .azp
            .as_deref()
            .ok_or_else(|| OidcError::rejected("the JWT names no authorized party (azp)"))?;
        if azp == self.cfg.client_id {
            return Err(OidcError::rejected(
                "a token minted for the web client is not accepted as a bearer",
            ));
        }
        if azp != device_client {
            return Err(OidcError::rejected(format!(
                "the JWT was minted for {azp:?}, not for this deployment's device-flow client"
            )));
        }
        Ok(VerifiedClaims {
            login: login_from(&claims.preferred_username, &claims.email, &claims.sub),
            sub: claims.sub,
            email: claims.email,
            groups: claims.groups.normalized(),
        })
    }

    /// The groups on the access token this relying party was just issued at the code exchange.
    /// Verified exactly like a bearer, and it must name this client as its authorized party.
    async fn own_access_token_groups(&self, token: &str) -> Result<Vec<String>, OidcError> {
        let claims = self.decode_access_token(token).await?;
        if claims.azp.as_deref() != Some(self.cfg.client_id.as_str()) {
            return Err(OidcError::rejected(format!(
                "the access token names {:?} as its authorized party, not this client",
                claims.azp
            )));
        }
        Ok(claims.groups.normalized())
    }

    /// Signature, issuer, and expiry against the issuer's JWKS, then the one claim that makes it
    /// an access token at all: `typ=Bearer`.
    async fn decode_access_token(&self, token: &str) -> Result<AccessTokenClaims, OidcError> {
        let header = jsonwebtoken::decode_header(token)
            .map_err(|e| OidcError::rejected(format!("not a JWT: {e}")))?;
        let kid = header
            .kid
            .clone()
            .ok_or_else(|| OidcError::rejected("the JWT names no signing key (kid)"))?;
        let issuer = self.discovered().await?.issuer.clone();

        // An unknown kid is usually the rotation case, not a forgery: re-fetch before refusing,
        // but no more often than the rate limit, since anyone can name a kid.
        let mut set = self.jwks(false).await?;
        if set.find(&kid).is_none() && self.claim_forced_jwks_refresh(Instant::now()) {
            set = self.jwks(true).await?;
        }
        let jwk = set.find(&kid).ok_or_else(|| {
            OidcError::rejected("the JWT is signed by a key the issuer does not publish")
        })?;
        let key = jsonwebtoken::DecodingKey::from_jwk(jwk)
            .map_err(|e| OidcError::rejected(format!("the issuer's key is unusable: {e}")))?;

        let mut validation = jsonwebtoken::Validation::new(header.alg);
        validation.set_issuer(&[issuer.as_str()]);
        // Explicit, and far below the library's 60-second default: the issuer and the controller
        // are NTP-synced, and a minute of accepting expired tokens is a minute the short access
        // token lifespan is not buying anything.
        validation.leeway = EXPIRY_LEEWAY.as_secs();
        // `aud` on a Keycloak access token is whatever the client's audience mapper puts there and
        // is not a trust boundary on its own; `azp` is.
        validation.validate_aud = false;
        let claims = jsonwebtoken::decode::<AccessTokenClaims>(token, &key, &validation)
            .map_err(|e| OidcError::rejected(format!("the JWT did not validate: {e}")))?
            .claims;
        if claims.typ.as_deref() != Some("Bearer") {
            return Err(OidcError::rejected(
                "only an access token (typ=Bearer) is accepted as a bearer",
            ));
        }
        Ok(claims)
    }
}

/// The name every role list, ownership check, and `user:` principal is spelled in:
/// `preferred_username`, falling back to `email`, falling back to the opaque subject. Normalized
/// like the role lists so a login is comparable however the issuer cased it.
fn login_from(preferred: &Option<String>, email: &Option<String>, sub: &str) -> String {
    preferred
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .or_else(|| email.as_deref().map(str::trim).filter(|v| !v.is_empty()))
        .unwrap_or(sub)
        .to_lowercase()
}

impl OidcProvider {
    /// Start an authorization-code flow: the URL to send the browser to, and the state the callback
    /// needs to prove the response belongs to this request.
    ///
    /// `redirect_to` is carried in the session, not in the authorization request, so the issuer
    /// never echoes an attacker-chosen destination back.
    pub async fn authorize(
        &self,
        redirect_to: String,
    ) -> Result<(String, crate::identity::session::LoginFlow), OidcError> {
        let discovered = self.discovered().await?;
        let (challenge, verifier) = openidconnect::PkceCodeChallenge::new_random_sha256();
        let mut request = discovered.client.authorize_url(
            openidconnect::AuthenticationFlow::<CoreResponseType>::AuthorizationCode,
            openidconnect::CsrfToken::new_random,
            openidconnect::Nonce::new_random,
        );
        // `openid` is added by the request itself; asking for it twice would send it twice.
        for scope in self.cfg.scopes.iter().filter(|s| s.as_str() != "openid") {
            request = request.add_scope(openidconnect::Scope::new(scope.clone()));
        }
        let (url, state, nonce) = request.set_pkce_challenge(challenge).url();
        Ok((
            url.to_string(),
            crate::identity::session::LoginFlow {
                state: state.secret().clone(),
                nonce: nonce.secret().clone(),
                pkce_verifier: verifier.secret().clone(),
                redirect_to,
            },
        ))
    }

    /// Finish the flow: exchange the code (proving the PKCE verifier) and validate the ID token that
    /// comes back — issuer, audience, expiry, signature, and the nonce this browser's request
    /// carried.
    ///
    /// The refresh token rides back with the claims. It is an OFFLINE token when the authorization
    /// request asked for `offline_access` and the realm granted it; `None` when it did not, which
    /// is the degraded shape where scheduled launches fall back to the schedule-row snapshot.
    pub async fn exchange(
        &self,
        code: String,
        flow: &crate::identity::session::LoginFlow,
    ) -> Result<Exchanged, OidcError> {
        let discovered = self.discovered().await?;
        let response = discovered
            .client
            .exchange_code(openidconnect::AuthorizationCode::new(code))
            .map_err(|e| {
                OidcError::rejected(format!("the issuer publishes no token endpoint: {e}"))
            })?
            .set_pkce_verifier(openidconnect::PkceCodeVerifier::new(
                flow.pkce_verifier.clone(),
            ))
            .request_async(&bridge(self.http.clone()))
            .await
            .map_err(|e| token_endpoint_failure("the code exchange", e))?;
        let claims = claims_from(
            &discovered,
            &response,
            &openidconnect::Nonce::new(flow.nonce.clone()),
        )?;
        let claims = self.supplement_groups(&discovered, &response, claims).await;
        Ok(Exchanged {
            claims,
            refresh_token: refresh_token_of(&response),
            id_token: id_token_of(&response),
        })
    }

    /// Where the issuer put group membership. RH SSO asserts it as realm roles on the ACCESS
    /// token (`realm_access.roles`), not on the ID token; other realms use a `groups` claim on
    /// either, or only answer it from userinfo. The ID token wins when it carries any, then the
    /// verified access token, then userinfo.
    async fn supplement_groups(
        &self,
        discovered: &Discovered,
        response: &TokenResponse,
        mut claims: VerifiedClaims,
    ) -> VerifiedClaims {
        use openidconnect::OAuth2TokenResponse;
        let access = response.access_token().secret();
        let mut source = "id_token";
        if claims.groups.is_empty() {
            match self.own_access_token_groups(access).await {
                Ok(groups) => {
                    claims.groups = groups;
                    source = "access_token";
                }
                Err(e) => {
                    tracing::warn!(error = %e, "oidc: the access token was not usable for groups")
                }
            }
        }
        if claims.groups.is_empty() {
            match self.userinfo(discovered, access).await {
                Ok(info) => {
                    claims.groups = info.groups.normalized();
                    source = "userinfo";
                }
                Err(e) => tracing::warn!(error = %e, "oidc: userinfo could not be read"),
            }
        }
        if claims.groups.is_empty() {
            source = "none";
        }
        log_claim_inventory(&claims, response, source);
        claims
    }

    async fn userinfo(
        &self,
        discovered: &Discovered,
        access_token: &str,
    ) -> Result<UserInfo, OidcError> {
        let endpoint = discovered
            .userinfo_endpoint
            .as_deref()
            .ok_or_else(|| OidcError::rejected("the issuer publishes no userinfo endpoint"))?;
        self.http
            .get(endpoint)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|e| OidcError::Unavailable(format!("userinfo: {e}")))?
            .error_for_status()
            .map_err(|e| OidcError::rejected(format!("userinfo: {e}")))?
            .json()
            .await
            .map_err(|e| OidcError::rejected(format!("userinfo: {e}")))
    }

    /// Spend a stored refresh token: a fresh ID token, validated the same way the callback's is,
    /// plus whatever refresh token the issuer rotated back.
    ///
    /// The error split is the whole point. Only an OAuth error document naming a dead grant is
    /// [`OidcError::Rejected`], which costs the user a re-login; an issuer that could not be
    /// reached, including one whose router answered for it, is [`OidcError::Unavailable`] and the
    /// caller retries. See [`token_endpoint_failure`].
    pub async fn refresh(&self, refresh_token: &str) -> Result<Refreshed, OidcError> {
        let discovered = self.discovered().await?;
        let response = discovered
            .client
            .exchange_refresh_token(&openidconnect::RefreshToken::new(refresh_token.to_string()))
            .map_err(|e| {
                OidcError::rejected(format!("the issuer publishes no token endpoint: {e}"))
            })?
            .request_async(&bridge(self.http.clone()))
            .await
            .map_err(|e| token_endpoint_failure("the refresh", e))?;
        // A refresh response's ID token carries no nonce: no browser request started it.
        let claims = claims_from(
            &discovered,
            &response,
            |_: Option<&openidconnect::Nonce>| Ok(()),
        )?;
        let claims = self.supplement_groups(&discovered, &response, claims).await;
        Ok(Refreshed {
            claims,
            refresh_token: refresh_token_of(&response),
        })
    }

    /// Tell the issuer a refresh token is dead, best effort. A user revoking their credential has
    /// already had the row deleted locally; an issuer that refuses or cannot be reached must not
    /// turn that into a failed revoke.
    pub async fn revoke(&self, refresh_token: &str) {
        let Ok(discovered) = self.discovered().await else {
            return;
        };
        let Some(endpoint) = discovered.revocation_endpoint.clone() else {
            return;
        };
        let mut form = vec![
            ("token", refresh_token),
            ("token_type_hint", "refresh_token"),
            ("client_id", self.cfg.client_id.as_str()),
        ];
        if let Some(secret) = self.cfg.client_secret.as_deref() {
            form.push(("client_secret", secret));
        }
        let body = form
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    percent_encoding::utf8_percent_encode(k, percent_encoding::NON_ALPHANUMERIC),
                    percent_encoding::utf8_percent_encode(v, percent_encoding::NON_ALPHANUMERIC)
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        if let Err(e) = self
            .http
            .post(&endpoint)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await
        {
            tracing::warn!(error = %e, "revoking the offline credential at the issuer");
        }
    }
}

/// What one authorization code produced.
#[derive(Debug, Clone)]
pub struct Exchanged {
    pub claims: VerifiedClaims,
    /// The offline credential, when the realm granted one.
    pub refresh_token: Option<String>,
    /// The serialized ID token, which RP-initiated logout sends back as `id_token_hint`.
    pub id_token: String,
}

/// What one spent refresh token produced.
#[derive(Debug, Clone)]
pub struct Refreshed {
    pub claims: VerifiedClaims,
    /// The token to store for next time. `None` means the issuer rotated nothing and the stored
    /// token stands.
    pub refresh_token: Option<String>,
}

/// Validate a token response's ID token and read the identity out of it.
fn claims_from(
    discovered: &Discovered,
    response: &TokenResponse,
    nonce: impl openidconnect::NonceVerifier,
) -> Result<VerifiedClaims, OidcError> {
    let id_token = response
        .extra_fields()
        .id_token()
        .ok_or_else(|| OidcError::rejected("the token response carried no ID token"))?;
    let claims = id_token
        .claims(&discovered.client.id_token_verifier(), nonce)
        .map_err(|e| OidcError::rejected(format!("the ID token did not validate: {e}")))?;
    let email = claims.email().map(|e| e.as_str().to_string());
    let preferred = claims.preferred_username().map(|u| u.as_str().to_string());
    let sub = claims.subject().as_str().to_string();
    Ok(VerifiedClaims {
        login: login_from(&preferred, &email, &sub),
        sub,
        email,
        groups: claims.additional_claims().normalized(),
    })
}

/// The userinfo document, of which only the group claims matter here.
#[derive(Debug, Deserialize)]
struct UserInfo {
    #[serde(flatten)]
    groups: GroupClaims,
}

/// One line per login naming which source the groups came from and the claim NAMES each token
/// carried, never a value: enough to tell "the realm sends no groups" from "we read the wrong
/// claim" off the pod log.
fn log_claim_inventory(claims: &VerifiedClaims, response: &TokenResponse, source: &str) {
    use openidconnect::OAuth2TokenResponse;
    let id_token = response
        .extra_fields()
        .id_token()
        .map(|t| claim_names(&t.to_string()))
        .unwrap_or_default();
    let access_token = claim_names(response.access_token().secret());
    tracing::info!(
        login = %claims.login,
        groups = claims.groups.len(),
        groups_source = source,
        ?id_token,
        ?access_token,
        "oidc: login claims"
    );
}

/// Top-level claim names of a JWT payload, unverified: this only feeds the inventory log.
fn claim_names(jwt: &str) -> Vec<String> {
    let mut names: Vec<String> = claims_probe::decode_jwt_payload(jwt)
        .ok()
        .and_then(|p| p.as_object().map(|o| o.keys().cloned().collect()))
        .unwrap_or_default();
    names.sort();
    names
}

/// The serialized ID token. `claims_from` has already refused a response without one.
fn id_token_of(response: &TokenResponse) -> String {
    response
        .extra_fields()
        .id_token()
        .map(|t| t.to_string())
        .unwrap_or_default()
}

fn refresh_token_of(response: &TokenResponse) -> Option<String> {
    use openidconnect::OAuth2TokenResponse;
    response
        .refresh_token()
        .map(|t| t.secret().clone())
        .filter(|t| !t.is_empty())
}

/// Which side a token-endpoint failure belongs to.
///
/// Definitive means the credential is dead and only a fresh sign-in brings it back, so the bar is
/// an OAuth error document the ISSUER authored, naming a code about the grant or the registration.
/// Everything else is the issuer being unreachable and retries: a transport failure, a non-200 with
/// an empty or non-JSON body (a router answering for a restarting issuer), a body that does not
/// parse, and any error code that may heal (`server_error`, `temporarily_unavailable`).
fn token_endpoint_failure(
    what: &str,
    e: openidconnect::RequestTokenError<BridgeError, StandardErrorResponse<CoreErrorResponseType>>,
) -> OidcError {
    let why = error_chain(&e);
    match &e {
        openidconnect::RequestTokenError::ServerResponse(response)
            if is_definitive(response.error()) =>
        {
            OidcError::rejected(format!("{what} was refused: {why}"))
        }
        _ => OidcError::Unavailable(format!("{what} failed: {why}")),
    }
}

/// The error codes that mean the grant will never be spendable again: revoked, expired, or issued
/// to a client this deployment no longer is. Nothing else is worth a user's re-login.
fn is_definitive(code: &CoreErrorResponseType) -> bool {
    matches!(
        code,
        CoreErrorResponseType::InvalidGrant
            | CoreErrorResponseType::InvalidClient
            | CoreErrorResponseType::UnauthorizedClient
    )
}

/// `RequestTokenError`'s own `Display` drops the cause for `Request` ("Request failed") and `Parse`
/// ("Failed to parse server response"), which is the whole diagnosis.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(next) = source {
        out.push_str(": ");
        out.push_str(&next.to_string());
        source = next.source();
    }
    out
}
