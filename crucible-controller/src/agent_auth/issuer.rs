//! Agent access tokens. The controller verifies a sandbox supervisor's SPIRE JWT-SVID, and for a
//! sandbox bound to a live run mints a short-lived ES256 token naming the run's launcher as `sub`
//! and the sandbox as `act.sub`. Nothing in the request beyond the verified SVID, the audience
//! (checked against an allowlist), and the scope (narrowed to what the run's role grants) reaches
//! the claims.

use crate::agent_auth::bindings::{BoundRun, SandboxId, SandboxIdError};
use crucible::deploy::profile::KueueCfg;
use crucible_contract::kueue::TenantRole;
use crucible_mcp_auth::jwks::Jwks;
use crucible_mcp_auth::verify::{TokenError, Verifier};
use jsonwebtoken::jwk::{Jwk, JwkSet, PublicKeyUse, ThumbprintHash};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Serialize;
use serde_json::{Value, json};
use std::fmt;
use url::Url;

/// Lifetime of a minted access token.
pub const TOKEN_TTL_SECS: u64 = 300;

pub const TOKEN_PATH: &str = "/oauth/token";
pub const JWKS_PATH: &str = "/.well-known/jwks.json";
pub const METADATA_PATH: &str = "/.well-known/oauth-authorization-server";

/// RFC 7523 §2.2: the only `client_assertion_type` accepted.
pub const JWT_BEARER: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";

const JTI_BYTES: usize = 16;

/// The longest a JWT-SVID may live, `exp` minus `iat`. SPIRE issues them for five minutes by
/// default; a longer-lived assertion is a replayable credential.
pub const MAX_SVID_LIFETIME_SECS: i64 = 900;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{var} is required when CONTROLLER_AGENT_ISSUER is set")]
    Missing { var: &'static str },
    #[error("{var} {value:?} is not a URL: {source}")]
    Url {
        var: &'static str,
        value: String,
        source: url::ParseError,
    },
    #[error(
        "CONTROLLER_AGENT_ISSUER {0:?} must be an http(s) origin with no path, query, or fragment, \
         so its metadata sits at /.well-known/oauth-authorization-server"
    )]
    IssuerShape(String),
    #[error("CONTROLLER_AGENT_SANDBOX_ID_PREFIX {0:?} must be a spiffe:// URI ending in /")]
    Prefix(String),
    #[error("CONTROLLER_AGENT_AUDIENCES names no MCP resource")]
    NoAudiences,
    #[error("reading {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("loading the agent signing key {path}: {source}")]
    Key {
        path: String,
        source: jsonwebtoken::errors::Error,
    },
}

/// The controller's issuer identifier: an http(s) origin, compared to `iss` and `aud` as an exact
/// string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuerUrl(String);

impl IssuerUrl {
    pub fn parse(raw: &str) -> Result<Self, ConfigError> {
        let url = Url::parse(raw).map_err(|source| ConfigError::Url {
            var: "CONTROLLER_AGENT_ISSUER",
            value: raw.to_string(),
            source,
        })?;
        let origin_only = matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty();
        match origin_only {
            true => Ok(Self(raw.trim_end_matches('/').to_string())),
            false => Err(ConfigError::IssuerShape(raw.to_string())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.0)
    }
}

/// The SPIFFE ID path under which every sandbox supervisor's ID sits, ending in `/`; the rest of
/// the ID is the sandbox id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxIdPrefix(String);

impl SandboxIdPrefix {
    pub fn parse(raw: &str) -> Result<Self, ConfigError> {
        match raw.starts_with("spiffe://") && raw.ends_with('/') && raw.len() > "spiffe://".len() {
            true => Ok(Self(raw.to_string())),
            false => Err(ConfigError::Prefix(raw.to_string())),
        }
    }
}

/// The ES256 key minted tokens are signed with, and its public half as published.
pub struct SigningKey {
    kid: String,
    key: EncodingKey,
    public: Jwk,
}

impl SigningKey {
    /// A PKCS#8 P-256 private key. Its `kid` is the RFC 7638 thumbprint of the public key, so a
    /// rotated key gets a new `kid` without configuration.
    pub fn from_pem(pem: &[u8]) -> Result<Self, jsonwebtoken::errors::Error> {
        let _ = jsonwebtoken::crypto::aws_lc::DEFAULT_PROVIDER.install_default();
        let key = EncodingKey::from_ec_pem(pem)?;
        let mut public = Jwk::from_encoding_key(&key, Algorithm::ES256)?;
        let kid = public.thumbprint(ThumbprintHash::SHA256)?;
        public.common.key_id = Some(kid.clone());
        public.common.public_key_use = Some(PublicKeyUse::Signature);
        Ok(Self { kid, key, public })
    }

    pub fn kid(&self) -> &str {
        &self.kid
    }
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SigningKey")
            .field("kid", &self.kid)
            .finish_non_exhaustive()
    }
}

/// An MCP resource a token may be minted for, and the scopes a token for it may carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audience {
    /// The resource URL, compared to the requested `audience` as an exact string.
    pub resource: String,
    pub scopes: Vec<String>,
}

/// `<resource url>=<scope> <scope> ...`; a bare URL grants no scopes.
impl std::str::FromStr for Audience {
    type Err = ConfigError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let (resource, scopes) = raw.split_once('=').unwrap_or((raw, ""));
        let resource = resource.trim();
        Url::parse(resource).map_err(|source| ConfigError::Url {
            var: "CONTROLLER_AGENT_AUDIENCES",
            value: resource.to_string(),
            source,
        })?;
        Ok(Self {
            resource: resource.to_string(),
            scopes: scopes.split_whitespace().map(str::to_string).collect(),
        })
    }
}

/// Scopes crucible-kueue authorizes by tenant role: granted only as far as the run's role allows.
pub(crate) fn governed_by_role(scope: &str) -> bool {
    TenantRole::Submitter.scopes().contains(&scope)
}

#[derive(Debug)]
pub struct AgentIssuerConfig {
    pub issuer: IssuerUrl,
    pub signing_key: SigningKey,
    /// SPIRE's OIDC discovery issuer: the `iss` of every JWT-SVID.
    pub svid_issuer: String,
    pub svid_jwks_uri: Url,
    /// A PEM bundle trusted for the SVID JWKS fetch on top of the system roots (OpenShift's
    /// service CA for an in-cluster `https://*.svc` endpoint).
    pub svid_ca: Option<Vec<u8>>,
    pub sandbox_prefix: SandboxIdPrefix,
    /// The MCP resources a token may be minted for.
    pub audiences: Vec<Audience>,
}

impl AgentIssuerConfig {
    /// The issuer's configuration from `CONTROLLER_AGENT_*`, `None` when
    /// `CONTROLLER_AGENT_ISSUER` is unset. A partial configuration is an error.
    pub fn from_env() -> Result<Option<Self>, ConfigError> {
        Self::from_lookup(|var| std::env::var(var).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, ConfigError> {
        let get = |var: &str| get(var).filter(|v| !v.trim().is_empty());
        let Some(issuer) = get("CONTROLLER_AGENT_ISSUER") else {
            return Ok(None);
        };
        let need = |var: &'static str| get(var).ok_or(ConfigError::Missing { var });
        let read = |path: String| {
            std::fs::read(&path).map_err(|source| ConfigError::Read { path, source })
        };
        let key_path = need("CONTROLLER_AGENT_SIGNING_KEY_FILE")?;
        let signing_key =
            SigningKey::from_pem(&read(key_path.clone())?).map_err(|source| ConfigError::Key {
                path: key_path,
                source,
            })?;
        let jwks_raw = need("CONTROLLER_AGENT_SVID_JWKS_URI")?;
        let svid_jwks_uri = Url::parse(&jwks_raw).map_err(|source| ConfigError::Url {
            var: "CONTROLLER_AGENT_SVID_JWKS_URI",
            value: jwks_raw,
            source,
        })?;
        let audiences: Vec<Audience> = need("CONTROLLER_AGENT_AUDIENCES")?
            .split(',')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::parse)
            .collect::<Result<_, _>>()?;
        if audiences.is_empty() {
            return Err(ConfigError::NoAudiences);
        }
        Ok(Some(Self {
            issuer: IssuerUrl::parse(&issuer)?,
            signing_key,
            svid_issuer: need("CONTROLLER_AGENT_SVID_ISSUER")?,
            svid_jwks_uri,
            svid_ca: get("CONTROLLER_AGENT_SVID_CA_FILE").map(read).transpose()?,
            sandbox_prefix: SandboxIdPrefix::parse(&need("CONTROLLER_AGENT_SANDBOX_ID_PREFIX")?)?,
            audiences,
        }))
    }

    /// The client the SVID JWKS is fetched with: bounded timeouts, no redirects, plus the
    /// configured CA bundle.
    pub fn svid_http(&self) -> Result<reqwest::Client, reqwest::Error> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none());
        if let Some(pem) = &self.svid_ca {
            for cert in reqwest::Certificate::from_pem_bundle(pem)? {
                builder = builder.add_root_certificate(cert);
            }
        }
        builder.build()
    }
}

/// A verified sandbox supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    pub spiffe_id: String,
    pub id: SandboxId,
}

#[derive(Debug, thiserror::Error)]
pub enum AssertionError {
    #[error(transparent)]
    Token(#[from] TokenError),
    #[error("assertion aud must be exactly [{expected:?}], got {got}")]
    Audience { expected: String, got: Value },
    #[error("assertion sub {sub:?} is not a sandbox under {prefix}")]
    NotASandbox { sub: String, prefix: String },
    #[error("assertion has no numeric iat")]
    NoIssuedAt,
    #[error("assertion iat {iat} is in the future")]
    IssuedInFuture { iat: i64 },
    #[error("assertion lives {lifetime}s, past the {MAX_SVID_LIFETIME_SECS}s maximum")]
    Lifetime { lifetime: i64 },
    #[error(transparent)]
    SandboxId(#[from] SandboxIdError),
}

/// Why a verified sandbox gets no token, each mapped to an RFC 6749 §5.2 error code.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GrantError {
    #[error("no audience requested; ask for one of {allowed:?}")]
    NoAudience { allowed: Vec<String> },
    #[error("audience {requested:?} is not one of {allowed:?}")]
    Audience {
        requested: String,
        allowed: Vec<String>,
    },
    #[error(
        "scope {requested:?} is not grantable for this audience; the run may hold {grantable:?}"
    )]
    Scope {
        requested: String,
        grantable: Vec<String>,
    },
    #[error(
        "launcher {launcher} has no known kubernetes username and this deployment requires one; \
         link it with PUT /api/me/kube-user or have an administrator set it"
    )]
    NoKubeUser { launcher: String },
    #[error("run {run_id} is attributed to {attributed_to:?}, not a person, so it has no launcher")]
    NoLauncher {
        run_id: String,
        attributed_to: Option<String>,
    },
}

/// The tenant claims crucible-kueue authorizes on, from the deploy profile's `[kueue]` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantScope {
    pub namespace: String,
    pub queues: Vec<String>,
    pub role: TenantRole,
}

impl TenantScope {
    /// The scope a run dispatched onto `cluster` holds, `None` when crucible-kueue runs elsewhere.
    pub fn for_run(kueue: &KueueCfg, cluster: &str) -> Option<Self> {
        (kueue.cluster == cluster).then(|| Self {
            namespace: kueue.workload_namespace.to_string(),
            queues: kueue.queues.iter().map(ToString::to_string).collect(),
            role: kueue.role,
        })
    }
}

/// The launcher a run's token acts for: the login of the person the run is attributed to.
pub fn launcher(run: &BoundRun) -> Result<String, GrantError> {
    run.attributed_to
        .as_deref()
        .and_then(|p| p.strip_prefix("user:"))
        .filter(|login| !login.is_empty())
        .map(str::to_string)
        .ok_or_else(|| GrantError::NoLauncher {
            run_id: run.run_id.clone(),
            attributed_to: run.attributed_to.clone(),
        })
}

/// The scopes to grant for `audience`: those it lists, with crucible-kueue's narrowed to what the
/// run's role allows. All of them when none is requested, else exactly the requested ones, each of
/// which must be grantable.
pub fn granted_scopes(
    requested: Option<&str>,
    audience: &Audience,
    tenant: Option<&TenantScope>,
) -> Result<Vec<String>, GrantError> {
    let grantable: Vec<String> = audience
        .scopes
        .iter()
        .filter(|s| {
            !governed_by_role(s) || tenant.is_some_and(|t| t.role.scopes().contains(&s.as_str()))
        })
        .cloned()
        .collect();
    let Some(requested) = requested.filter(|r| !r.trim().is_empty()) else {
        return Ok(grantable);
    };
    let mut granted: Vec<String> = Vec::new();
    for scope in requested.split_whitespace() {
        if !grantable.iter().any(|g| g == scope) {
            return Err(GrantError::Scope {
                requested: scope.to_string(),
                grantable,
            });
        }
        if !granted.iter().any(|g| g == scope) {
            granted.push(scope.to_string());
        }
    }
    Ok(granted)
}

/// Everything a minted token says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentGrant {
    pub sandbox: Sandbox,
    pub launcher: String,
    /// The launcher's Kubernetes username, for the `kube_user` claim.
    pub kube_user: Option<String>,
    pub run: BoundRun,
    pub audience: String,
    pub scopes: Vec<String>,
    pub tenant: Option<TenantScope>,
}

/// A minted token. Never printed.
pub struct Minted {
    pub access_token: String,
    pub expires_in: u64,
    pub scope: String,
}

impl fmt::Debug for Minted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Minted")
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MintError {
    #[error("drawing random bytes for the token id failed")]
    Random,
    #[error("signing the access token: {0}")]
    Sign(#[from] jsonwebtoken::errors::Error),
}

/// RFC 8414 authorization server metadata.
#[derive(Debug, Serialize, PartialEq)]
pub struct AuthorizationServerMetadata {
    pub issuer: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    pub grant_types_supported: Vec<&'static str>,
    pub response_types_supported: Vec<&'static str>,
    pub token_endpoint_auth_methods_supported: Vec<&'static str>,
    pub token_endpoint_auth_signing_alg_values_supported: Vec<Algorithm>,
    pub scopes_supported: Vec<String>,
}

pub struct AgentIssuer {
    issuer: IssuerUrl,
    key: SigningKey,
    svids: Verifier,
    sandbox_prefix: SandboxIdPrefix,
    audiences: Vec<Audience>,
}

impl AgentIssuer {
    pub fn new(cfg: AgentIssuerConfig, svid_jwks: Jwks) -> Self {
        Self {
            svids: Verifier::new(cfg.svid_issuer, svid_jwks),
            issuer: cfg.issuer,
            key: cfg.signing_key,
            sandbox_prefix: cfg.sandbox_prefix,
            audiences: cfg.audiences,
        }
    }

    pub fn issuer(&self) -> &IssuerUrl {
        &self.issuer
    }

    /// The sandbox a JWT-SVID client assertion identifies. The SVID must verify against SPIRE's
    /// keys, carry exactly one `aud` equal to this issuer, and name a SPIFFE ID under the
    /// sandbox prefix.
    pub async fn sandbox(&self, assertion: &str) -> Result<Sandbox, AssertionError> {
        let issuer = self.issuer.as_str();
        let claims = self.svids.verify(assertion, &[issuer]).await?;
        let aud = claims.get("aud").cloned().unwrap_or(Value::Null);
        let single = match &aud {
            Value::String(a) => a == issuer,
            Value::Array(items) => matches!(items.as_slice(), [Value::String(a)] if a == issuer),
            _ => false,
        };
        if !single {
            return Err(AssertionError::Audience {
                expected: issuer.to_string(),
                got: aud,
            });
        }
        let iat = claims
            .get("iat")
            .and_then(Value::as_i64)
            .ok_or(AssertionError::NoIssuedAt)?;
        let exp = claims
            .get("exp")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MAX);
        let now = i64::try_from(jsonwebtoken::get_current_timestamp()).unwrap_or(i64::MAX);
        let leeway = i64::try_from(crucible_mcp_auth::verify::LEEWAY_SECS).unwrap_or(0);
        if iat > now.saturating_add(leeway) {
            return Err(AssertionError::IssuedInFuture { iat });
        }
        let lifetime = exp.saturating_sub(iat);
        if lifetime > MAX_SVID_LIFETIME_SECS {
            return Err(AssertionError::Lifetime { lifetime });
        }
        let sub = claims
            .get("sub")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let not_a_sandbox = || AssertionError::NotASandbox {
            sub: sub.to_string(),
            prefix: self.sandbox_prefix.0.clone(),
        };
        let rest = sub
            .strip_prefix(self.sandbox_prefix.0.as_str())
            .ok_or_else(not_a_sandbox)?;
        Ok(Sandbox {
            spiffe_id: sub.to_string(),
            id: SandboxId::parse(rest)?,
        })
    }

    /// The requested audience, when it is an allowlisted MCP resource.
    pub fn audience(&self, requested: Option<&str>) -> Result<&Audience, GrantError> {
        let allowed = || self.audiences.iter().map(|a| a.resource.clone()).collect();
        let requested = requested
            .filter(|r| !r.is_empty())
            .ok_or_else(|| GrantError::NoAudience { allowed: allowed() })?;
        self.audiences
            .iter()
            .find(|a| a.resource == requested)
            .ok_or_else(|| GrantError::Audience {
                requested: requested.to_string(),
                allowed: allowed(),
            })
    }

    /// Sign `grant` as an access token issued at `now` (Unix seconds).
    pub fn mint(&self, grant: &AgentGrant, now: u64) -> Result<Minted, MintError> {
        let mut jti = [0u8; JTI_BYTES];
        aws_lc_rs::rand::fill(&mut jti).map_err(|_| MintError::Random)?;
        let scope = grant.scopes.join(" ");
        let mut claims = json!({
            "iss": self.issuer.as_str(),
            "sub": grant.launcher,
            "aud": grant.audience,
            "iat": now,
            "nbf": now,
            "exp": now.saturating_add(TOKEN_TTL_SECS),
            "jti": crucible_contract::artifact::lower_hex(&jti),
            "act": {"sub": grant.sandbox.spiffe_id},
            "run_id": grant.run.pod_name,
            "scope": scope,
        });
        if let Some(map) = claims.as_object_mut() {
            if let Some(name) = &grant.run.sandbox_name {
                map.insert("sandbox".into(), json!(name));
            }
            if let Some(kube_user) = &grant.kube_user {
                map.insert("kube_user".into(), json!(kube_user));
            }
            if let Some(workdir) = &grant.run.workdir {
                map.insert("workdir".into(), json!(workdir));
            }
            if let Some(tenant) = &grant.tenant {
                map.insert("namespace".into(), json!(tenant.namespace));
                map.insert("queues".into(), json!(tenant.queues));
                map.insert("role".into(), json!(tenant.role));
            }
        }
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(self.key.kid.clone());
        let access_token = jsonwebtoken::encode(&header, &claims, &self.key.key)?;
        Ok(Minted {
            access_token,
            expires_in: TOKEN_TTL_SECS,
            scope,
        })
    }

    pub fn jwks(&self) -> JwkSet {
        JwkSet {
            keys: vec![self.key.public.clone()],
        }
    }

    pub fn metadata(&self) -> AuthorizationServerMetadata {
        AuthorizationServerMetadata {
            issuer: self.issuer.as_str().to_string(),
            token_endpoint: self.issuer.endpoint(TOKEN_PATH),
            jwks_uri: self.issuer.endpoint(JWKS_PATH),
            grant_types_supported: vec!["client_credentials"],
            response_types_supported: Vec::new(),
            token_endpoint_auth_methods_supported: vec!["private_key_jwt"],
            token_endpoint_auth_signing_alg_values_supported:
                crucible_mcp_auth::verify::ACCEPTED_ALGORITHMS.to_vec(),
            scopes_supported: self.audiences.iter().fold(Vec::new(), |mut all, a| {
                for scope in &a.scopes {
                    if !all.contains(scope) {
                        all.push(scope.clone());
                    }
                }
                all
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::agent_auth::issuer::*;
    use crate::agent_auth::testing::{
        self, BUILDIT, CLUSTER, EcKey, ISSUER, KUEUE, PREFIX, PROFILE, Spire, now, serve_jwks,
    };
    use crucible_contract::kueue::{SCOPE_READ, SCOPE_SUBMIT};
    use std::collections::HashMap;

    fn with(claims: Value, edit: impl FnOnce(&mut serde_json::Map<String, Value>)) -> Value {
        let mut claims = claims;
        edit(claims.as_object_mut().unwrap());
        claims
    }

    #[tokio::test]
    async fn an_svid_for_the_controller_names_its_sandbox() {
        let spire = Spire::start().await;
        let issuer = testing::issuer(&spire, &EcKey::generate());
        for aud in [json!([ISSUER]), json!(ISSUER)] {
            let svid = spire.svid(&with(Spire::claims("sbx-1"), |c| {
                c.insert("aud".into(), aud.clone());
            }));
            let sandbox = issuer.sandbox(&svid).await.unwrap();
            assert_eq!(sandbox.id.as_str(), "sbx-1", "aud {aud}");
            assert_eq!(sandbox.spiffe_id, format!("{PREFIX}sbx-1"));
        }
    }

    #[tokio::test]
    async fn svids_failing_any_check_are_refused() {
        let spire = Spire::start().await;
        let issuer = testing::issuer(&spire, &EcKey::generate());
        let base = || Spire::claims("sbx-1");
        let cases: Vec<(&str, String)> = vec![
            (
                "another audience",
                spire.svid(&with(base(), |c| {
                    c.insert(
                        "aud".into(),
                        json!(["https://keycloak.example/realms/crucible"]),
                    );
                })),
            ),
            (
                "a second audience beside the controller",
                spire.svid(&with(base(), |c| {
                    c.insert("aud".into(), json!([ISSUER, KUEUE]));
                })),
            ),
            (
                "the issuer with a trailing slash",
                spire.svid(&with(base(), |c| {
                    c.insert("aud".into(), json!([format!("{ISSUER}/")]));
                })),
            ),
            (
                "no audience",
                spire.svid(&with(base(), |c| {
                    c.remove("aud");
                })),
            ),
            (
                "another trust domain",
                spire.svid(&with(base(), |c| {
                    c.insert(
                        "sub".into(),
                        json!("spiffe://evil.example/openshell/sandbox/sbx-1"),
                    );
                })),
            ),
            (
                "another workload in the trust domain",
                spire.svid(&with(base(), |c| {
                    c.insert(
                        "sub".into(),
                        json!("spiffe://example.crucible/ns/team-e2e/sa/default"),
                    );
                })),
            ),
            (
                "a nested path under the prefix",
                spire.svid(&with(base(), |c| {
                    c.insert("sub".into(), json!(format!("{PREFIX}sbx-1/extra")));
                })),
            ),
            (
                "the bare prefix",
                spire.svid(&with(base(), |c| {
                    c.insert("sub".into(), json!(PREFIX));
                })),
            ),
            (
                "expired",
                spire.svid(&with(base(), |c| {
                    c.insert("exp".into(), json!(now() - 120));
                })),
            ),
            (
                "no iat",
                spire.svid(&with(base(), |c| {
                    c.remove("iat");
                })),
            ),
            (
                "an iat in the future",
                spire.svid(&with(base(), |c| {
                    c.insert("iat".into(), json!(now() + 600));
                    c.insert("exp".into(), json!(now() + 900));
                })),
            ),
            (
                "a lifetime past the maximum",
                spire.svid(&with(base(), |c| {
                    c.insert("iat".into(), json!(now()));
                    c.insert("exp".into(), json!(now() + 3600));
                })),
            ),
            (
                "another issuer",
                spire.svid(&with(base(), |c| {
                    c.insert("iss".into(), json!("https://oidc-discovery.evil.example"));
                })),
            ),
            (
                "signed by a key SPIRE never published",
                testing::sign(&EcKey::generate(), "spire", &base()),
            ),
        ];
        for (what, svid) in cases {
            if let Ok(sandbox) = issuer.sandbox(&svid).await {
                panic!("{what}: accepted as {sandbox:?}");
            }
        }
        let expired = spire.svid(&with(base(), |c| {
            c.insert("exp".into(), json!(now() - 120));
        }));
        assert!(matches!(
            issuer.sandbox(&expired).await,
            Err(AssertionError::Token(_))
        ));
        let foreign = spire.svid(&with(base(), |c| {
            c.insert(
                "sub".into(),
                json!("spiffe://evil.example/openshell/sandbox/sbx-1"),
            );
        }));
        assert!(matches!(
            issuer.sandbox(&foreign).await,
            Err(AssertionError::NotASandbox { .. })
        ));
        let doubled = spire.svid(&with(base(), |c| {
            c.insert("aud".into(), json!([ISSUER, KUEUE]));
        }));
        assert!(matches!(
            issuer.sandbox(&doubled).await,
            Err(AssertionError::Audience { .. })
        ));
    }

    #[tokio::test]
    async fn only_an_allowlisted_audience_is_granted() {
        let spire = Spire::start().await;
        let issuer = testing::issuer(&spire, &EcKey::generate());
        assert_eq!(issuer.audience(Some(KUEUE)).unwrap().resource, KUEUE);
        assert_eq!(
            issuer.audience(Some(BUILDIT)).unwrap().scopes,
            ["buildit.read", "buildit.build"]
        );
        assert!(matches!(
            issuer.audience(None),
            Err(GrantError::NoAudience { .. })
        ));
        assert!(matches!(
            issuer.audience(Some("")),
            Err(GrantError::NoAudience { .. })
        ));
        assert!(matches!(
            issuer.audience(Some(&format!("{KUEUE}/"))),
            Err(GrantError::Audience { .. })
        ));
    }

    fn tenant(role: TenantRole) -> TenantScope {
        TenantScope {
            namespace: "crucible-kueue-e2e".into(),
            queues: vec!["crucible-e2e".into()],
            role,
        }
    }

    fn kueue() -> Audience {
        format!("{KUEUE}=kueue.read kueue.submit").parse().unwrap()
    }

    fn buildit() -> Audience {
        format!("{BUILDIT}=buildit.read buildit.build")
            .parse()
            .unwrap()
    }

    #[test]
    fn scopes_narrow_to_the_audience_and_kueue_scopes_to_the_runs_role() {
        let submitter = tenant(TenantRole::Submitter);
        let viewer = tenant(TenantRole::Viewer);
        assert_eq!(
            granted_scopes(None, &kueue(), Some(&submitter)).unwrap(),
            [SCOPE_READ, SCOPE_SUBMIT]
        );
        assert_eq!(
            granted_scopes(Some(""), &kueue(), Some(&viewer)).unwrap(),
            [SCOPE_READ]
        );
        assert_eq!(
            granted_scopes(Some("kueue.read kueue.read"), &kueue(), Some(&submitter)).unwrap(),
            [SCOPE_READ]
        );
        assert_eq!(
            granted_scopes(Some("kueue.submit"), &kueue(), Some(&viewer)),
            Err(GrantError::Scope {
                requested: SCOPE_SUBMIT.into(),
                grantable: vec![SCOPE_READ.into()]
            })
        );
        assert!(granted_scopes(Some("kueue.admin"), &kueue(), Some(&submitter)).is_err());
        assert!(granted_scopes(None, &kueue(), None).unwrap().is_empty());
        assert!(granted_scopes(Some("kueue.read"), &kueue(), None).is_err());
        assert_eq!(
            granted_scopes(None, &buildit(), None).unwrap(),
            ["buildit.read", "buildit.build"],
            "scopes outside crucible-kueue's need no tenant"
        );
        assert!(
            granted_scopes(Some("kueue.read"), &buildit(), Some(&submitter)).is_err(),
            "a scope another audience lists is not grantable here"
        );
        let bare: Audience = KUEUE.parse().unwrap();
        assert!(bare.scopes.is_empty());
        assert!(
            granted_scopes(None, &bare, Some(&submitter))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn tenant_scope_comes_from_the_profile_only_on_its_cluster() {
        let kueue = toml::from_str::<crucible::deploy::DeployProfile>(PROFILE)
            .unwrap()
            .kueue
            .unwrap();
        assert_eq!(
            TenantScope::for_run(&kueue, CLUSTER),
            Some(tenant(TenantRole::Submitter))
        );
        assert_eq!(TenantScope::for_run(&kueue, "hub"), None);
    }

    fn run(attributed_to: Option<&str>) -> BoundRun {
        BoundRun {
            run_id: "playbook_kueue-e2e_01a1-1791474738".into(),
            pod_name: "crucible-run-brave-otter".into(),
            cluster: CLUSTER.into(),
            attributed_to: attributed_to.map(str::to_string),
            sandbox_name: Some("ci-0123abcd".into()),
            workdir: Some("/sandbox/repo".into()),
        }
    }

    #[test]
    fn the_launcher_is_the_person_the_run_is_attributed_to() {
        assert_eq!(launcher(&run(Some("user:alice"))).unwrap(), "alice");
        for unattributed in [
            None,
            Some("team:platform-administrators"),
            Some("user:"),
            Some("alice"),
        ] {
            assert!(
                matches!(
                    launcher(&run(unattributed)),
                    Err(GrantError::NoLauncher { .. })
                ),
                "{unattributed:?}"
            );
        }
    }

    fn grant(tenant: Option<TenantScope>) -> AgentGrant {
        AgentGrant {
            sandbox: Sandbox {
                spiffe_id: format!("{PREFIX}sbx-1"),
                id: crate::agent_auth::bindings::SandboxId::parse("sbx-1").unwrap(),
            },
            launcher: "alice".into(),
            kube_user: Some("alice-k8s".into()),
            run: run(Some("user:alice")),
            audience: KUEUE.into(),
            scopes: vec![SCOPE_READ.into(), SCOPE_SUBMIT.into()],
            tenant,
        }
    }

    /// Verify `token` the way a resource server does: against the published JWKS.
    async fn verified(issuer: &AgentIssuer, token: &str) -> serde_json::Map<String, Value> {
        let jwks = serve_jwks(issuer.jwks()).await;
        crucible_mcp_auth::verify::Verifier::new(
            ISSUER,
            crucible_mcp_auth::jwks::Jwks::new(reqwest::Client::new(), jwks),
        )
        .verify(token, &[KUEUE])
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_minted_token_carries_the_delegation_and_tenant_claims_and_verifies() {
        let spire = Spire::start().await;
        let issuer = testing::issuer(&spire, &EcKey::generate());
        let at = now();
        let minted = issuer
            .mint(&grant(Some(tenant(TenantRole::Submitter))), at)
            .unwrap();
        assert_eq!(minted.expires_in, TOKEN_TTL_SECS);
        assert_eq!(minted.scope, "kueue.read kueue.submit");
        let header = jsonwebtoken::decode_header(&minted.access_token).unwrap();
        assert_eq!(header.alg, Algorithm::ES256);
        assert_eq!(header.kid.as_deref(), Some(issuer.key.kid()));
        let mut claims = verified(&issuer, &minted.access_token).await;
        let jti = claims.remove("jti").unwrap();
        assert_eq!(jti.as_str().unwrap().len(), 2 * JTI_BYTES);
        assert_eq!(
            Value::Object(claims),
            json!({
                "iss": ISSUER,
                "sub": "alice",
                "kube_user": "alice-k8s",
                "aud": KUEUE,
                "iat": at,
                "nbf": at,
                "exp": at + TOKEN_TTL_SECS,
                "act": {"sub": format!("{PREFIX}sbx-1")},
                "run_id": "crucible-run-brave-otter",
                "sandbox": "ci-0123abcd",
                "workdir": "/sandbox/repo",
                "scope": "kueue.read kueue.submit",
                "namespace": "crucible-kueue-e2e",
                "queues": ["crucible-e2e"],
                "role": "submitter",
            })
        );
        let principal = crucible_mcp_auth::principal::Principal::from_claims(
            verified(&issuer, &minted.access_token).await,
        )
        .unwrap();
        assert_eq!(principal.presenter(), format!("{PREFIX}sbx-1"));
    }

    #[tokio::test]
    async fn every_token_gets_its_own_id_and_omits_tenant_and_kube_user_claims_it_lacks() {
        let spire = Spire::start().await;
        let issuer = testing::issuer(&spire, &EcKey::generate());
        let unlinked = AgentGrant {
            kube_user: None,
            ..grant(None)
        };
        let a = verified(
            &issuer,
            &issuer.mint(&unlinked, now()).unwrap().access_token,
        )
        .await;
        let b = verified(
            &issuer,
            &issuer.mint(&grant(None), now()).unwrap().access_token,
        )
        .await;
        assert_ne!(a["jti"], b["jti"]);
        for key in ["namespace", "queues", "role", "kube_user"] {
            assert!(!a.contains_key(key), "{key}");
        }
    }

    #[test]
    fn the_kid_is_the_public_key_thumbprint_and_debug_hides_the_key() {
        let key = EcKey::generate();
        let a = SigningKey::from_pem(key.pem().as_bytes()).unwrap();
        let b = SigningKey::from_pem(key.pem().as_bytes()).unwrap();
        let other = SigningKey::from_pem(EcKey::generate().pem().as_bytes()).unwrap();
        assert_eq!(a.kid(), b.kid());
        assert_ne!(a.kid(), other.kid());
        assert_eq!(a.public.common.key_id.as_deref(), Some(a.kid()));
        assert_eq!(
            format!("{a:?}"),
            format!("SigningKey {{ kid: {:?}, .. }}", a.kid())
        );
        assert!(
            SigningKey::from_pem(b"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n")
                .is_err()
        );
    }

    #[tokio::test]
    async fn the_metadata_points_at_the_issuers_own_endpoints() {
        let spire = Spire::start().await;
        let issuer = testing::issuer(&spire, &EcKey::generate());
        let doc = serde_json::to_value(issuer.metadata()).unwrap();
        assert_eq!(doc["issuer"], ISSUER);
        assert_eq!(doc["token_endpoint"], format!("{ISSUER}/oauth/token"));
        assert_eq!(doc["jwks_uri"], format!("{ISSUER}/.well-known/jwks.json"));
        assert_eq!(doc["grant_types_supported"], json!(["client_credentials"]));
        assert_eq!(
            doc["scopes_supported"],
            json!([
                "kueue.read",
                "kueue.submit",
                "buildit.read",
                "buildit.build"
            ])
        );
        assert_eq!(
            doc["token_endpoint_auth_methods_supported"],
            json!(["private_key_jwt"])
        );
        assert!(
            doc["token_endpoint_auth_signing_alg_values_supported"]
                .as_array()
                .unwrap()
                .contains(&json!("ES256"))
        );
        let jwks = serde_json::to_value(issuer.jwks()).unwrap();
        assert_eq!(jwks["keys"][0]["kid"], issuer.key.kid());
        assert_eq!(jwks["keys"][0]["use"], "sig");
        assert_eq!(jwks["keys"][0]["alg"], "ES256");
        assert!(
            jwks["keys"][0].get("d").is_none(),
            "no private key material"
        );
    }

    fn lookup<'a>(vars: &'a HashMap<&'static str, String>) -> impl Fn(&str) -> Option<String> + 'a {
        |var| vars.get(var).cloned()
    }

    fn env(key_file: &std::path::Path) -> HashMap<&'static str, String> {
        HashMap::from([
            ("CONTROLLER_AGENT_ISSUER", format!("{ISSUER}/")),
            (
                "CONTROLLER_AGENT_SIGNING_KEY_FILE",
                key_file.display().to_string(),
            ),
            ("CONTROLLER_AGENT_SVID_ISSUER", testing::SVID_ISSUER.into()),
            (
                "CONTROLLER_AGENT_SVID_JWKS_URI",
                "https://spire-spiffe-oidc-discovery-provider.ztwim.svc/keys".into(),
            ),
            ("CONTROLLER_AGENT_SANDBOX_ID_PREFIX", PREFIX.into()),
            (
                "CONTROLLER_AGENT_AUDIENCES",
                format!("{KUEUE}=kueue.read kueue.submit, {BUILDIT}=buildit.read buildit.build,"),
            ),
        ])
    }

    #[test]
    fn config_loads_from_env_and_refuses_a_partial_or_malformed_one() {
        let dir = tempfile::tempdir().unwrap();
        let key_file = dir.path().join("signing.pem");
        std::fs::write(&key_file, EcKey::generate().pem()).unwrap();
        let vars = env(&key_file);

        let cfg = AgentIssuerConfig::from_lookup(lookup(&vars))
            .unwrap()
            .unwrap();
        assert_eq!(cfg.issuer.as_str(), ISSUER, "the trailing slash is dropped");
        assert_eq!(
            cfg.audiences,
            [
                Audience {
                    resource: KUEUE.into(),
                    scopes: vec!["kueue.read".into(), "kueue.submit".into()],
                },
                Audience {
                    resource: BUILDIT.into(),
                    scopes: vec!["buildit.read".into(), "buildit.build".into()],
                },
            ]
        );
        assert!(cfg.svid_ca.is_none());

        assert!(
            AgentIssuerConfig::from_lookup(|_| None).unwrap().is_none(),
            "no issuer, no endpoint"
        );

        for missing in [
            "CONTROLLER_AGENT_SIGNING_KEY_FILE",
            "CONTROLLER_AGENT_SVID_ISSUER",
            "CONTROLLER_AGENT_SVID_JWKS_URI",
            "CONTROLLER_AGENT_SANDBOX_ID_PREFIX",
            "CONTROLLER_AGENT_AUDIENCES",
        ] {
            let mut partial = vars.clone();
            partial.remove(missing);
            assert!(
                matches!(
                    AgentIssuerConfig::from_lookup(lookup(&partial)),
                    Err(ConfigError::Missing { var }) if var == missing
                ),
                "{missing}"
            );
        }

        let bad: [(&'static str, &str); 8] = [
            (
                "CONTROLLER_AGENT_ISSUER",
                "http://controller.svc:8080/oauth",
            ),
            ("CONTROLLER_AGENT_ISSUER", "http://controller.svc:8080/?x=1"),
            ("CONTROLLER_AGENT_ISSUER", "spiffe://example.crucible"),
            (
                "CONTROLLER_AGENT_SANDBOX_ID_PREFIX",
                "spiffe://example.crucible/openshell/sandbox",
            ),
            (
                "CONTROLLER_AGENT_SANDBOX_ID_PREFIX",
                "https://example.crucible/openshell/sandbox/",
            ),
            ("CONTROLLER_AGENT_AUDIENCES", " , "),
            ("CONTROLLER_AGENT_AUDIENCES", "not a url=kueue.read"),
            (
                "CONTROLLER_AGENT_SIGNING_KEY_FILE",
                "/nonexistent/signing.pem",
            ),
        ];
        for (var, value) in bad {
            let mut broken = vars.clone();
            broken.insert(var, value.to_string());
            assert!(
                AgentIssuerConfig::from_lookup(lookup(&broken)).is_err(),
                "{var}={value}"
            );
        }
    }
}
