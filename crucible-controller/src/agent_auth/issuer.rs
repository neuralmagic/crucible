//! Agent access tokens. The controller verifies a sandbox supervisor's SPIRE JWT-SVID, and for a
//! sandbox bound to a live run mints a short-lived ES256 token naming the run's launcher as `sub`
//! and the sandbox as `act.sub`. Nothing in the request beyond the verified SVID, the audience
//! (checked against an allowlist), and the scope (narrowed to what the audience lists) reaches the
//! claims.

use crate::agent_auth::bindings::BoundRun;
use crate::agent_auth::sandbox::{SandboxId, SandboxIdError};
use crucible::deploy::profile::{KubeName, check_cluster_name};
use crucible::deploy::servers::ServerSpec;
use crucible_mcp_auth::jwks::Jwks;
use crucible_mcp_auth::verify::{TokenError, Verifier, unverified_issuer};
use jsonwebtoken::jwk::{Jwk, JwkSet, PublicKeyUse, ThumbprintHash};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Serialize;
use serde_json::{Value, json};
use std::fmt;
use url::Url;

/// The bounds of a minted access token's lifetime: long enough that a sandbox fetches one per
/// task rather than per call, short enough that a leaked one expires within the task.
pub const MIN_TOKEN_TTL_SECS: u64 = 15 * 60;
pub const MAX_TOKEN_TTL_SECS: u64 = 30 * 60;

pub const TOKEN_PATH: &str = "/oauth/token";

/// People's authorization endpoint.
pub const AUTHORIZE_PATH: &str = "/oauth/authorize";
/// RFC 7591 registration, for loopback public clients only.
pub const REGISTER_PATH: &str = "/oauth/register";
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
    #[error(
        "gateway prefix {0:?} must be a spiffe:// URI ending in /, with at most one {{tenant}} path segment"
    )]
    Prefix(String),
    #[error("CONTROLLER_AGENT_AUDIENCES names no MCP resource")]
    NoAudiences,
    #[error("CONTROLLER_AGENT_SPIRE names no cluster's SPIRE")]
    NoSpire,
    #[error(
        "CONTROLLER_AGENT_ISSUER is set but neither CONTROLLER_AGENT_SPIRE (agent tokens) nor \
         CONTROLLER_PEOPLE_AUDIENCES (people's tokens) says whom it issues to"
    )]
    NoRecipients,
    #[error(
        "CONTROLLER_AGENT_SPIRE entry {0:?} is not \
         <cluster>=<svid issuer> <gateway prefix> [http://]<namespace>/<service>:<port>"
    )]
    SpireEntry(String),
    #[error("CONTROLLER_AGENT_SPIRE names {0:?} twice; each cluster and each SVID issuer once")]
    SpireTwice(String),
    #[error(
        "CONTROLLER_AGENT_TOKEN_TTL_SECS {0:?} is not a number of seconds from \
         {MIN_TOKEN_TTL_SECS} to {MAX_TOKEN_TTL_SECS}"
    )]
    Ttl(String),
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
    #[error(
        "CONTROLLER_AGENT_AUDIENCES entry {0:?} is neither <url>=<scopes> nor <cluster>:<url>=<scopes>"
    )]
    AudienceCluster(String),
    #[error("{var} is set, but CONTROLLER_AGENT_ISSUER is not; it does nothing without the issuer")]
    NoIssuer { var: &'static str },
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

    pub fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.0)
    }
}

/// The SPIFFE ID path under which every sandbox supervisor's ID sits, ending in `/`. The rest of
/// the ID is `<gateway id>/sandbox/<sandbox id>`, both read from labels the gateway that created
/// the supervisor pod stamped. One path segment may be `{tenant}`, matching a DNS-1123 label:
///
/// ```text
///   spiffe://td/openshell/gateway/            spiffe://td/openshell/gateway/<gw>/sandbox/<id>
///   spiffe://td/openshell/{tenant}/gateway/   spiffe://td/openshell/<tenant>/gateway/<gw>/sandbox/<id>
///   └──────── head ──────┘└ tail ─┘
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayPrefix {
    raw: String,
    /// Everything before `{tenant}`, or the whole prefix without one.
    head: String,
    /// Everything after `{tenant}/`, when the prefix has one.
    tail: Option<String>,
}

const TENANT_SEGMENT: &str = "/{tenant}/";

impl GatewayPrefix {
    pub fn parse(raw: &str) -> Result<Self, ConfigError> {
        let bad = || ConfigError::Prefix(raw.to_string());
        let path = raw.strip_prefix("spiffe://").ok_or_else(bad)?;
        if path.is_empty() || !raw.ends_with('/') {
            return Err(bad());
        }
        let (head, tail) = match raw.split_once(TENANT_SEGMENT) {
            Some((head, tail)) => (format!("{head}/"), Some(tail.to_string())),
            None => (raw.to_string(), None),
        };
        let trust_domain = head
            .strip_prefix("spiffe://")
            .and_then(|p| p.split('/').next());
        let braces = |v: &str| v.contains(['{', '}']);
        if trust_domain.is_none_or(str::is_empty)
            || braces(&head)
            || tail.as_deref().is_some_and(braces)
        {
            return Err(bad());
        }
        Ok(Self {
            raw: raw.to_string(),
            head,
            tail,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The tenant (when the prefix has a `{tenant}` segment) and the rest of `spiffe_id` after the
    /// prefix; `None` when the ID is not under it.
    pub fn split<'a>(&self, spiffe_id: &'a str) -> Option<(Option<&'a str>, &'a str)> {
        let rest = spiffe_id.strip_prefix(self.head.as_str())?;
        let Some(tail) = &self.tail else {
            return Some((None, rest));
        };
        let (tenant, rest) = rest.split_once('/')?;
        KubeName::try_from(tenant.to_string()).ok()?;
        Some((Some(tenant), rest.strip_prefix(tail.as_str())?))
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

/// A cluster's SPIRE OIDC discovery provider Service. Its key set is read through that cluster's
/// API server Service proxy with the controller's own client for the cluster, so it needs no
/// route of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryService {
    /// Whether the Service serves TLS: `http://` before the namespace says it does not.
    pub tls: bool,
    pub namespace: KubeName,
    pub name: KubeName,
    pub port: u16,
}

impl DiscoveryService {
    /// The API server path of the provider's JWKS.
    pub fn keys_path(&self) -> String {
        let scheme = match self.tls {
            true => "https",
            false => "http",
        };
        format!(
            "/api/v1/namespaces/{}/services/{scheme}:{}:{}/proxy/keys",
            self.namespace, self.name, self.port
        )
    }
}

/// One cluster's SPIRE, trusted for the SVIDs of sandboxes whose runs it hosts:
/// `<cluster>=<svid issuer> <gateway prefix> [http://]<namespace>/<service>:<port>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterSpire {
    pub cluster: String,
    /// The `iss` of its JWT-SVIDs, compared as an exact string.
    pub svid_issuer: String,
    /// `spiffe://<its trust domain>/openshell/gateway/`.
    pub gateway_prefix: GatewayPrefix,
    pub discovery: DiscoveryService,
}

impl std::str::FromStr for ClusterSpire {
    type Err = ConfigError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let bad = || ConfigError::SpireEntry(raw.to_string());
        let (cluster, rest) = raw.split_once('=').ok_or_else(bad)?;
        let [issuer, prefix, service] = rest.split_whitespace().collect::<Vec<_>>()[..] else {
            return Err(bad());
        };
        let (tls, service) = match service.strip_prefix("http://") {
            Some(rest) => (false, rest),
            None => (true, service),
        };
        let (namespace, name_port) = service.split_once('/').ok_or_else(bad)?;
        let (name, port) = name_port.split_once(':').ok_or_else(bad)?;
        let kube_name = |v: &str| KubeName::try_from(v.to_string()).map_err(|_| bad());
        let cluster = cluster.trim();
        check_cluster_name(cluster).map_err(|_| bad())?;
        if issuer.is_empty() {
            return Err(bad());
        }
        Ok(Self {
            cluster: cluster.to_string(),
            svid_issuer: issuer.to_string(),
            gateway_prefix: GatewayPrefix::parse(prefix)?,
            discovery: DiscoveryService {
                tls,
                namespace: kube_name(namespace)?,
                name: kube_name(name)?,
                port: port.parse().map_err(|_| bad())?,
            },
        })
    }
}

/// An MCP resource a token may be minted for, and the scopes a token for it may carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audience {
    /// The resource URL, compared to the requested `audience` as an exact string.
    pub resource: String,
    /// The only cluster whose runs get tokens for it; `None` for runs on any cluster.
    pub cluster: Option<String>,
    pub scopes: Vec<String>,
    /// Whether the server authorizes as the token's `kube_user`, so no token is minted without
    /// one.
    pub requires_kube_user: bool,
    /// The namespaces a token names, the first as the default.
    pub namespaces: Vec<String>,
}

impl Audience {
    /// This audience with what the server at its URL needs in a token, as the deploy profile
    /// describes the server. A URL no description names needs nothing.
    pub fn described_by(self, servers: &[ServerSpec]) -> Self {
        let described = servers.iter().find(|s| {
            s.url == self.resource && self.cluster.as_deref().is_none_or(|c| s.serves(c))
        });
        match described {
            Some(s) => Self {
                requires_kube_user: s.requires_kube_user,
                namespaces: s.namespaces.clone(),
                ..self
            },
            None => self,
        }
    }

    /// Whether a run on `cluster` may get tokens for this audience.
    pub fn serves(&self, cluster: &str) -> bool {
        self.cluster.as_deref().is_none_or(|c| c == cluster)
    }
}

/// `[<cluster>:]<resource url>=<scope> <scope> ...`; a bare URL grants no scopes. Without a cluster
/// the audience serves runs on every cluster.
impl std::str::FromStr for Audience {
    type Err = ConfigError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let raw = raw.trim();
        let (cluster, raw) = match raw.starts_with("http://") || raw.starts_with("https://") {
            true => (None, raw),
            false => {
                let (cluster, rest) = raw
                    .split_once(':')
                    .ok_or_else(|| ConfigError::AudienceCluster(raw.to_string()))?;
                check_cluster_name(cluster)
                    .map_err(|_| ConfigError::AudienceCluster(raw.to_string()))?;
                (Some(cluster.to_string()), rest)
            }
        };
        let (resource, scopes) = raw.split_once('=').unwrap_or((raw, ""));
        let resource = resource.trim();
        Url::parse(resource).map_err(|source| ConfigError::Url {
            var: "CONTROLLER_AGENT_AUDIENCES",
            value: resource.to_string(),
            source,
        })?;
        Ok(Self {
            resource: resource.to_string(),
            cluster,
            scopes: scopes.split_whitespace().map(str::to_string).collect(),
            requires_kube_user: false,
            namespaces: Vec::new(),
        })
    }
}

/// A minted token's lifetime, within [`MIN_TOKEN_TTL_SECS`]..=[`MAX_TOKEN_TTL_SECS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenTtl(u64);

impl TokenTtl {
    pub fn secs(self) -> u64 {
        self.0
    }
}

impl Default for TokenTtl {
    fn default() -> Self {
        Self(MIN_TOKEN_TTL_SECS)
    }
}

impl std::str::FromStr for TokenTtl {
    type Err = ConfigError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().parse::<u64>() {
            Ok(secs) if (MIN_TOKEN_TTL_SECS..=MAX_TOKEN_TTL_SECS).contains(&secs) => Ok(Self(secs)),
            _ => Err(ConfigError::Ttl(raw.to_string())),
        }
    }
}

#[derive(Debug)]
pub struct AgentIssuerConfig {
    pub issuer: IssuerUrl,
    /// The key tokens are signed with.
    pub signing_key: SigningKey,
    /// The key tokens were signed with before a rotation, still published in the JWKS so tokens
    /// it signed verify until they expire.
    pub previous_key: Option<SigningKey>,
    pub token_ttl: TokenTtl,
    /// Each cluster's SPIRE, by cluster and by SVID issuer unique.
    pub spire: Vec<ClusterSpire>,
    /// The MCP resources a token may be minted for.
    pub audiences: Vec<Audience>,
}

impl AgentIssuerConfig {
    /// The same settings, each audience with the token needs of the server `servers` describe at
    /// its URL.
    pub fn described_by(self, servers: &[ServerSpec]) -> Self {
        Self {
            audiences: self
                .audiences
                .into_iter()
                .map(|a| a.described_by(servers))
                .collect(),
            ..self
        }
    }
}

/// The settings that mean nothing without `CONTROLLER_AGENT_ISSUER`; one set without it is a
/// configuration error rather than a silently disabled feature.
const DEPENDENT_VARS: &[&str] = &[
    "CONTROLLER_AGENT_SIGNING_KEY",
    "CONTROLLER_AGENT_SIGNING_KEY_FILE",
    "CONTROLLER_AGENT_PREVIOUS_SIGNING_KEY_FILE",
    "CONTROLLER_AGENT_TOKEN_TTL_SECS",
    "CONTROLLER_AGENT_SPIRE",
    "CONTROLLER_AGENT_AUDIENCES",
    "CONTROLLER_AGENT_TOKEN_ENDPOINT",
    "CONTROLLER_PEOPLE_AUDIENCES",
    "CONTROLLER_PEOPLE_ACCESS_TTL_SECS",
    "CONTROLLER_PEOPLE_CLIENTS_FILE",
    "CONTROLLER_PEOPLE_CIMD_HOSTS",
    "CONTROLLER_PEOPLE_DCR",
    "CONTROLLER_PEOPLE_TRUSTED_PROXIES",
];

impl AgentIssuerConfig {
    /// The issuer's configuration from `CONTROLLER_AGENT_*`, `None` when
    /// `CONTROLLER_AGENT_ISSUER` and every setting that needs it are unset. A partial configuration is an error. Without
    /// `CONTROLLER_AGENT_SPIRE` the issuer mints no agent tokens and serves people only. The
    /// signing key is `CONTROLLER_AGENT_SIGNING_KEY` (PEM) or the file
    /// `CONTROLLER_AGENT_SIGNING_KEY_FILE` names.
    pub fn from_env() -> Result<Option<Self>, ConfigError> {
        Self::from_lookup(|var| std::env::var(var).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, ConfigError> {
        let get = |var: &str| get(var).filter(|v| !v.trim().is_empty());
        let Some(issuer) = get("CONTROLLER_AGENT_ISSUER") else {
            return match DEPENDENT_VARS.iter().find(|var| get(var).is_some()) {
                Some(var) => Err(ConfigError::NoIssuer { var }),
                None => Ok(None),
            };
        };
        let need = |var: &'static str| get(var).ok_or(ConfigError::Missing { var });
        let read = |path: String| {
            std::fs::read(&path).map_err(|source| ConfigError::Read { path, source })
        };
        let load_key = |path: String| {
            SigningKey::from_pem(&read(path.clone())?)
                .map_err(|source| ConfigError::Key { path, source })
        };
        let signing_key = match get("CONTROLLER_AGENT_SIGNING_KEY") {
            Some(pem) => {
                SigningKey::from_pem(pem.as_bytes()).map_err(|source| ConfigError::Key {
                    path: "CONTROLLER_AGENT_SIGNING_KEY".into(),
                    source,
                })?
            }
            None => load_key(need("CONTROLLER_AGENT_SIGNING_KEY_FILE")?)?,
        };
        let previous_key = get("CONTROLLER_AGENT_PREVIOUS_SIGNING_KEY_FILE")
            .map(load_key)
            .transpose()?;
        let token_ttl = get("CONTROLLER_AGENT_TOKEN_TTL_SECS")
            .map(|raw| raw.parse())
            .transpose()?
            .unwrap_or_default();
        let spire: Vec<ClusterSpire> = match get("CONTROLLER_AGENT_SPIRE") {
            None => Vec::new(),
            Some(raw) => {
                let spire: Vec<ClusterSpire> = parse_list(&raw)?;
                if spire.is_empty() {
                    return Err(ConfigError::NoSpire);
                }
                spire
            }
        };
        let mut seen = std::collections::BTreeSet::new();
        for entry in &spire {
            for key in [&entry.cluster, &entry.svid_issuer] {
                if !seen.insert(key.as_str()) {
                    return Err(ConfigError::SpireTwice(key.clone()));
                }
            }
        }
        let audiences: Vec<Audience> = match spire.is_empty() {
            true => Vec::new(),
            false => parse_list(&need("CONTROLLER_AGENT_AUDIENCES")?)?,
        };
        if !spire.is_empty() && audiences.is_empty() {
            return Err(ConfigError::NoAudiences);
        }
        Ok(Some(Self {
            issuer: IssuerUrl::parse(&issuer)?,
            signing_key,
            previous_key,
            token_ttl,
            spire,
            audiences,
        }))
    }
}

/// The comma-separated entries of a `CONTROLLER_AGENT_*` list.
pub fn parse_list<T: std::str::FromStr<Err = ConfigError>>(
    raw: &str,
) -> Result<Vec<T>, ConfigError> {
    raw.split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::parse)
        .collect()
}

/// A verified sandbox supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    pub spiffe_id: String,
    /// The cluster whose SPIRE issued the SVID.
    pub cluster: String,
    /// The tenant segment of its SPIFFE ID, under a prefix with `{tenant}`.
    pub tenant: Option<String>,
    /// The gateway that created the supervisor's pod.
    pub gateway: GatewayId,
    pub id: SandboxId,
}

/// An OpenShell gateway id: a DNS-1123 label, the `openshell.ai/gateway-id` label value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayId(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("gateway id {0:?} is not a DNS-1123 label")]
pub struct GatewayIdError(pub String);

impl GatewayId {
    pub fn parse(raw: &str) -> Result<Self, GatewayIdError> {
        let bytes = raw.as_bytes();
        let valid = !raw.is_empty()
            && raw.len() <= 63
            && bytes
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
            && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
            && bytes.last().is_some_and(u8::is_ascii_alphanumeric);
        match valid {
            true => Ok(Self(raw.to_string())),
            false => Err(GatewayIdError(raw.to_string())),
        }
    }

    /// The gateway the controller starts for the run whose pod is `run_pod`.
    pub fn of_run_pod(run_pod: &str) -> Self {
        Self(crucible::deploy::render::gateway::GatewayNames::for_run_pod(run_pod).gateway_id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GatewayId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AssertionError {
    #[error(transparent)]
    Token(#[from] TokenError),
    #[error("assertion iss {0:?} is no trusted cluster's SPIRE")]
    UnknownSpire(String),
    #[error("assertion aud must be exactly [{expected:?}], got {got}")]
    Audience { expected: String, got: Value },
    #[error("assertion sub {sub:?} is not <gateway>/sandbox/<id> under {prefix}")]
    NotASandbox { sub: String, prefix: String },
    #[error(transparent)]
    GatewayId(#[from] GatewayIdError),
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
    #[error("audience {requested:?} is not one of {allowed:?} for runs on cluster {cluster}")]
    Audience {
        requested: String,
        cluster: String,
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
        "launcher {launcher} has no known kubernetes username on cluster {cluster}, which {audience} \
         requires ({why}); link it under Settings > Cluster identities"
    )]
    NoKubeUser {
        launcher: String,
        cluster: String,
        audience: String,
        why: String,
    },
    #[error(
        "the SVID comes from cluster {spire}'s SPIRE, but sandbox {sandbox} is bound to a run on \
         {run}"
    )]
    WrongCluster {
        sandbox: String,
        spire: String,
        run: String,
    },
    #[error(
        "run {run_id} was dispatched for {dispatched_by:?}, not a person, so it has no launcher"
    )]
    NoLauncher {
        run_id: String,
        dispatched_by: Option<String>,
    },
}

/// The launcher a run's token acts for: the login of the person the run was dispatched for.
pub fn launcher(run: &BoundRun) -> Result<String, GrantError> {
    run.dispatched_by
        .as_deref()
        .and_then(|p| p.strip_prefix("user:"))
        .filter(|login| !login.is_empty())
        .map(str::to_string)
        .ok_or_else(|| GrantError::NoLauncher {
            run_id: run.run_id.clone(),
            dispatched_by: run.dispatched_by.clone(),
        })
}

/// The scopes to grant for `audience`: all it lists when none is requested, else exactly the
/// requested ones, each of which it must list. What a scope lets the caller do is the resource
/// server's to decide, from the token's identity.
pub fn granted_scopes(
    requested: Option<&str>,
    audience: &Audience,
) -> Result<Vec<String>, GrantError> {
    let grantable = audience.scopes.clone();
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
    /// The `namespaces` claim, the audience's namespaces; the first is also the `namespace`
    /// claim, the default.
    pub namespaces: Vec<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_endpoint: Option<String>,
    pub token_endpoint: String,
    pub jwks_uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    pub grant_types_supported: Vec<&'static str>,
    pub response_types_supported: Vec<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub code_challenge_methods_supported: Vec<&'static str>,
    pub token_endpoint_auth_methods_supported: Vec<&'static str>,
    pub token_endpoint_auth_signing_alg_values_supported: Vec<Algorithm>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub authorization_response_iss_parameter_supported: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub client_id_metadata_document_supported: bool,
    pub scopes_supported: Vec<String>,
}

/// What the people grant adds to the metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeopleMetadata {
    /// Every scope a person's token may carry.
    pub scopes: Vec<String>,
    /// Whether an https `client_id` is resolved as a Client ID Metadata Document.
    pub client_id_metadata_documents: bool,
    /// Whether loopback public clients may register themselves.
    pub registration: bool,
}

/// A fresh random `jti`.
pub fn token_id() -> Result<String, MintError> {
    let mut jti = [0u8; JTI_BYTES];
    aws_lc_rs::rand::fill(&mut jti).map_err(|_| MintError::Random)?;
    Ok(crucible_contract::artifact::lower_hex(&jti))
}

pub struct AgentIssuer {
    issuer: IssuerUrl,
    key: SigningKey,
    previous_key: Option<SigningKey>,
    ttl: TokenTtl,
    spire: Vec<TrustedSpire>,
    audiences: Vec<Audience>,
}

struct TrustedSpire {
    cluster: String,
    svid_issuer: String,
    gateway_prefix: GatewayPrefix,
    svids: Verifier,
}

impl AgentIssuer {
    /// `keys` gives each cluster's SPIRE key set.
    pub fn new(cfg: AgentIssuerConfig, keys: impl Fn(&ClusterSpire) -> Jwks) -> Self {
        let spire = cfg
            .spire
            .iter()
            .map(|entry| TrustedSpire {
                cluster: entry.cluster.clone(),
                svid_issuer: entry.svid_issuer.clone(),
                gateway_prefix: entry.gateway_prefix.clone(),
                svids: Verifier::new(entry.svid_issuer.clone(), keys(entry)),
            })
            .collect();
        Self {
            spire,
            issuer: cfg.issuer,
            key: cfg.signing_key,
            previous_key: cfg.previous_key,
            ttl: cfg.token_ttl,
            audiences: cfg.audiences,
        }
    }

    pub fn issuer(&self) -> &IssuerUrl {
        &self.issuer
    }

    /// Whether any cluster's SPIRE is trusted, and so whether agent tokens are minted at all.
    pub fn serves_agents(&self) -> bool {
        !self.spire.is_empty()
    }

    /// The sandbox a JWT-SVID client assertion identifies. The SVID must verify against the keys
    /// of the cluster SPIRE its `iss` names, carry exactly one `aud` equal to this issuer, and
    /// name a SPIFFE ID `<that cluster's gateway prefix><gateway id>/sandbox/<sandbox id>`.
    pub async fn sandbox(&self, assertion: &str) -> Result<Sandbox, AssertionError> {
        let issuer = self.issuer.as_str();
        let claimed = unverified_issuer(assertion)?;
        let spire = self
            .spire
            .iter()
            .find(|s| s.svid_issuer == claimed)
            .ok_or(AssertionError::UnknownSpire(claimed))?;
        let claims = spire.svids.verify(assertion, &[issuer]).await?;
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
            prefix: spire.gateway_prefix.as_str().to_string(),
        };
        let (tenant, rest) = spire.gateway_prefix.split(sub).ok_or_else(not_a_sandbox)?;
        let (gateway, id) = rest.split_once("/sandbox/").ok_or_else(not_a_sandbox)?;
        Ok(Sandbox {
            spiffe_id: sub.to_string(),
            cluster: spire.cluster.clone(),
            tenant: tenant.map(str::to_string),
            gateway: GatewayId::parse(gateway)?,
            id: SandboxId::parse(id)?,
        })
    }

    /// The requested audience, when it is an MCP resource allowlisted for runs on `cluster`.
    pub fn audience(
        &self,
        requested: Option<&str>,
        cluster: &str,
    ) -> Result<&Audience, GrantError> {
        let serving = || self.audiences.iter().filter(|a| a.serves(cluster));
        let allowed = || serving().map(|a| a.resource.clone()).collect();
        let requested = requested
            .filter(|r| !r.is_empty())
            .ok_or_else(|| GrantError::NoAudience { allowed: allowed() })?;
        serving()
            .find(|a| a.resource == requested)
            .ok_or_else(|| GrantError::Audience {
                requested: requested.to_string(),
                cluster: cluster.to_string(),
                allowed: allowed(),
            })
    }

    /// Sign `claims` with the issuer's key.
    pub fn sign(&self, claims: &Value) -> Result<String, MintError> {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(self.key.kid.clone());
        Ok(jsonwebtoken::encode(&header, claims, &self.key.key)?)
    }

    /// Sign `grant` as an access token issued at `now` (Unix seconds).
    pub fn mint(&self, grant: &AgentGrant, now: u64) -> Result<Minted, MintError> {
        let jti = token_id()?;
        let scope = grant.scopes.join(" ");
        let mut claims = json!({
            "iss": self.issuer.as_str(),
            "sub": grant.launcher,
            "aud": grant.audience,
            "iat": now,
            "nbf": now,
            "exp": now.saturating_add(self.ttl.secs()),
            "jti": jti,
            "act": {"sub": grant.sandbox.spiffe_id},
            "run_id": grant.run.pod_name,
            "kube_cluster": grant.run.cluster,
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
            if let Some(default) = grant.namespaces.first() {
                map.insert("namespace".into(), json!(default));
                map.insert("namespaces".into(), json!(grant.namespaces));
            }
        }
        let access_token = self.sign(&claims)?;
        Ok(Minted {
            access_token,
            expires_in: self.ttl.secs(),
            scope,
        })
    }

    /// The current key, then the previous one while it is configured.
    pub fn jwks(&self) -> JwkSet {
        JwkSet {
            keys: std::iter::once(&self.key)
                .chain(self.previous_key.iter().filter(|k| k.kid != self.key.kid))
                .map(|k| k.public.clone())
                .collect(),
        }
    }

    /// The metadata document; `people` adds the authorization code grant for people's clients and,
    /// when they may register themselves, the registration endpoint.
    pub fn metadata(&self, people: Option<&PeopleMetadata>) -> AuthorizationServerMetadata {
        let mut scopes_supported = Vec::new();
        let people_scopes = people.into_iter().flat_map(|p| p.scopes.iter());
        for scope in self
            .audiences
            .iter()
            .flat_map(|a| a.scopes.iter())
            .chain(people_scopes)
        {
            if !scopes_supported.contains(scope) {
                scopes_supported.push(scope.clone());
            }
        }
        let mut metadata = AuthorizationServerMetadata {
            issuer: self.issuer.as_str().to_string(),
            authorization_endpoint: None,
            token_endpoint: self.issuer.endpoint(TOKEN_PATH),
            jwks_uri: self.issuer.endpoint(JWKS_PATH),
            registration_endpoint: None,
            grant_types_supported: Vec::new(),
            response_types_supported: Vec::new(),
            code_challenge_methods_supported: Vec::new(),
            token_endpoint_auth_methods_supported: Vec::new(),
            token_endpoint_auth_signing_alg_values_supported: Vec::new(),
            authorization_response_iss_parameter_supported: false,
            client_id_metadata_document_supported: false,
            scopes_supported,
        };
        if self.serves_agents() {
            metadata.grant_types_supported.push("client_credentials");
            metadata
                .token_endpoint_auth_methods_supported
                .push("private_key_jwt");
            metadata.token_endpoint_auth_signing_alg_values_supported =
                crucible_mcp_auth::verify::ACCEPTED_ALGORITHMS.to_vec();
        }
        if let Some(people) = people {
            metadata.client_id_metadata_document_supported = people.client_id_metadata_documents;
            metadata.registration_endpoint = people
                .registration
                .then(|| self.issuer.endpoint(REGISTER_PATH));
            metadata.authorization_endpoint = Some(self.issuer.endpoint(AUTHORIZE_PATH));
            metadata
                .grant_types_supported
                .extend(["authorization_code", "refresh_token"]);
            metadata.response_types_supported.push("code");
            metadata.code_challenge_methods_supported.push("S256");
            metadata.token_endpoint_auth_methods_supported.push("none");
            metadata.authorization_response_iss_parameter_supported = true;
        }
        metadata
    }
}

#[cfg(test)]
mod tests {
    use crate::agent_auth::issuer::*;
    use crate::agent_auth::testing::{
        self, BUILDIT, CLUSTER, EcKey, ISSUER, KUEUE, POD, PREFIX, PROFILE, Spire, now, serve_jwks,
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
            assert_eq!(sandbox.gateway.as_str(), POD);
            assert_eq!(sandbox.spiffe_id, format!("{PREFIX}{POD}/sandbox/sbx-1"));
            assert_eq!(sandbox.cluster, CLUSTER);
            assert_eq!(sandbox.tenant, None);
        }
    }

    #[test]
    fn a_gateway_prefix_may_hold_one_tenant_segment() {
        let plain = GatewayPrefix::parse("spiffe://td/openshell/gateway/").unwrap();
        assert_eq!(
            plain.split("spiffe://td/openshell/gateway/gw/sandbox/s"),
            Some((None, "gw/sandbox/s"))
        );
        assert_eq!(
            plain.split("spiffe://td/openshell/team-a/gateway/gw/sandbox/s"),
            None
        );
        let tenanted = GatewayPrefix::parse("spiffe://td/openshell/{tenant}/gateway/").unwrap();
        assert_eq!(
            tenanted.split("spiffe://td/openshell/team-a/gateway/gw/sandbox/s"),
            Some((Some("team-a"), "gw/sandbox/s"))
        );
        for outside in [
            "spiffe://td/openshell/gateway/gw/sandbox/s",
            "spiffe://td/openshell/Team_A/gateway/gw/sandbox/s",
            "spiffe://td/openshell/team-a/other/gateway/gw/sandbox/s",
            "spiffe://other/openshell/team-a/gateway/gw/sandbox/s",
        ] {
            assert_eq!(tenanted.split(outside), None, "{outside}");
        }
        for bad in [
            "spiffe://td/openshell/gateway",
            "https://td/openshell/gateway/",
            "spiffe:///openshell/gateway/",
            "spiffe://td/openshell/{tenant}/{tenant}/gateway/",
            "spiffe://td/openshell/x{tenant}/gateway/",
            "spiffe://td/openshell/{team}/gateway/",
            "spiffe://{tenant}/gateway/",
        ] {
            assert!(GatewayPrefix::parse(bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn an_svid_under_a_tenant_prefix_names_its_tenant_and_sandbox() {
        let spire = Spire::start().await;
        let mut cfg = testing::config(&EcKey::generate());
        cfg.spire[0].gateway_prefix =
            GatewayPrefix::parse("spiffe://example.crucible/openshell/{tenant}/gateway/").unwrap();
        let issuer = AgentIssuer::new(cfg, |_| {
            Jwks::new(reqwest::Client::new(), spire.jwks_uri.clone())
        });
        let svid = |sub: String| {
            spire.svid(&with(Spire::claims("sbx-1"), |c| {
                c.insert("sub".into(), json!(sub));
            }))
        };
        let sandbox = issuer
            .sandbox(&svid(format!(
                "spiffe://example.crucible/openshell/team-a/gateway/{POD}/sandbox/sbx-1"
            )))
            .await
            .unwrap();
        assert_eq!(sandbox.tenant.as_deref(), Some("team-a"));
        assert_eq!(sandbox.gateway.as_str(), POD);
        assert_eq!(sandbox.id.as_str(), "sbx-1");
        assert!(matches!(
            issuer
                .sandbox(&svid(format!("{PREFIX}{POD}/sandbox/sbx-1")))
                .await,
            Err(AssertionError::NotASandbox { .. })
        ));
    }

    #[tokio::test]
    async fn each_svid_verifies_only_against_the_spire_its_issuer_names() {
        let hub = Spire::start().await;
        let spoke = Spire::start().await;
        let spoke_issuer = "https://oidc-discovery.spoke.example";
        let spoke_prefix = "spiffe://spoke.example/openshell/gateway/";
        let mut cfg = testing::config(&EcKey::generate());
        cfg.spire.push(
            format!("spoke={spoke_issuer} {spoke_prefix} spire/oidc-discovery:443")
                .parse()
                .unwrap(),
        );
        let issuer = AgentIssuer::new(cfg, |entry| {
            let uri = match entry.cluster.as_str() {
                "spoke" => spoke.jwks_uri.clone(),
                _ => hub.jwks_uri.clone(),
            };
            Jwks::new(reqwest::Client::new(), uri)
        });
        let from_spoke = |sub: String| {
            with(Spire::claims("sbx-1"), |c| {
                c.insert("iss".into(), json!(spoke_issuer));
                c.insert("sub".into(), json!(sub));
            })
        };
        let own = from_spoke(format!("{spoke_prefix}{POD}/sandbox/sbx-1"));
        let sandbox = issuer.sandbox(&spoke.svid(&own)).await.unwrap();
        assert_eq!(sandbox.cluster, "spoke");
        assert!(
            matches!(
                issuer.sandbox(&hub.svid(&own)).await,
                Err(AssertionError::Token(_))
            ),
            "the hub's key does not verify an SVID claiming the spoke's issuer"
        );
        let hub_domain = from_spoke(format!("{PREFIX}{POD}/sandbox/sbx-1"));
        assert!(
            matches!(
                issuer.sandbox(&spoke.svid(&hub_domain)).await,
                Err(AssertionError::NotASandbox { .. })
            ),
            "the spoke's SPIRE speaks only for its own trust domain"
        );
        let unknown = with(Spire::claims("sbx-1"), |c| {
            c.insert("iss".into(), json!("https://oidc-discovery.evil.example"));
        });
        assert!(matches!(
            issuer.sandbox(&hub.svid(&unknown)).await,
            Err(AssertionError::UnknownSpire(iss)) if iss == "https://oidc-discovery.evil.example"
        ));
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
                "a nested path under the sandbox",
                spire.svid(&with(base(), |c| {
                    c.insert(
                        "sub".into(),
                        json!(format!("{PREFIX}{POD}/sandbox/sbx-1/extra")),
                    );
                })),
            ),
            (
                "no gateway segment",
                spire.svid(&with(base(), |c| {
                    c.insert("sub".into(), json!(format!("{PREFIX}sbx-1")));
                })),
            ),
            (
                "a gateway id that is not a label",
                spire.svid(&with(base(), |c| {
                    c.insert(
                        "sub".into(),
                        json!(format!("{PREFIX}Gateway_A/sandbox/sbx-1")),
                    );
                })),
            ),
            (
                "an empty gateway id",
                spire.svid(&with(base(), |c| {
                    c.insert("sub".into(), json!(format!("{PREFIX}/sandbox/sbx-1")));
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
        assert_eq!(issuer.audience(Some(KUEUE), "hub").unwrap().resource, KUEUE);
        assert_eq!(
            issuer.audience(Some(BUILDIT), "hub").unwrap().scopes,
            ["buildit.read", "buildit.build"]
        );
        assert!(matches!(
            issuer.audience(None, "hub"),
            Err(GrantError::NoAudience { .. })
        ));
        assert!(matches!(
            issuer.audience(Some(""), "hub"),
            Err(GrantError::NoAudience { .. })
        ));
        assert!(matches!(
            issuer.audience(Some(&format!("{KUEUE}/")), "hub"),
            Err(GrantError::Audience { .. })
        ));
    }

    /// The issuer grants a cluster-bound audience only to runs on its cluster.
    #[tokio::test]
    async fn a_cluster_bound_audience_is_granted_on_its_cluster_only() {
        let spire = Spire::start().await;
        let mut cfg = testing::config(&EcKey::generate());
        cfg.audiences = parse_list(&format!(
            "spoke-a:{KUEUE}=kueue.read,{BUILDIT}=buildit.read"
        ))
        .unwrap();
        let issuer = AgentIssuer::new(cfg, |_| {
            crucible_mcp_auth::jwks::Jwks::new(reqwest::Client::new(), spire.jwks_uri.clone())
        });
        assert_eq!(
            issuer.audience(Some(KUEUE), "spoke-a").unwrap().scopes,
            ["kueue.read"]
        );
        match issuer.audience(Some(KUEUE), "spoke-b") {
            Err(GrantError::Audience {
                cluster, allowed, ..
            }) => {
                assert_eq!(cluster, "spoke-b");
                assert_eq!(allowed, [BUILDIT], "only what spoke-b's runs may get");
            }
            other => panic!("{other:?}"),
        }
        assert!(issuer.audience(Some(BUILDIT), "spoke-b").is_ok());
    }

    /// A cluster-bound audience serves runs on that cluster only, and the same URL can be bound
    /// to each cluster on its own; a bare one serves every cluster.
    #[test]
    fn audiences_bind_to_a_cluster_or_serve_all() {
        let url = "http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp";
        let entries: Vec<Audience> = parse_list(&format!(
            "spoke-a:{url}=kueue.read kueue.submit,spoke-b:{url}=kueue.read,{BUILDIT}=buildit.read"
        ))
        .unwrap();
        assert_eq!(entries[0].cluster.as_deref(), Some("spoke-a"));
        assert_eq!(entries[0].resource, url);
        assert_eq!(entries[2].cluster, None);
        let on = |cluster: &str| {
            entries
                .iter()
                .find(|a| a.resource == url && a.serves(cluster))
                .map(|a| a.scopes.clone())
        };
        assert_eq!(
            on("spoke-a"),
            Some(vec!["kueue.read".to_string(), "kueue.submit".to_string()])
        );
        assert_eq!(on("spoke-b"), Some(vec!["kueue.read".to_string()]));
        assert_eq!(on("spoke-c"), None);
        assert!(entries[2].serves("spoke-c"));
        for bad in [
            "Spoke:http://a.example/mcp",
            ":http://a.example/mcp",
            "spoke-a",
        ] {
            assert!(bad.parse::<Audience>().is_err(), "{bad}");
        }
    }

    /// Agent audiences, people audiences, SPIRE entries and the deploy profile accept the same
    /// cluster names.
    #[test]
    fn every_cluster_setting_accepts_the_names_the_profile_accepts() {
        for name in [
            "hub", "pirate", "spoke-a", "a1", "", "Pirate", "spoke_a", "-a", "a-", "a.b",
        ] {
            let profile = crucible::deploy::profile::check_cluster_name(name).is_ok();
            let agent = format!("{name}:http://a.example/mcp=s")
                .parse::<Audience>()
                .is_ok();
            let person = format!("{name}:http://a.example/mcp=s")
                .parse::<crate::agent_auth::people::PersonAudience>()
                .is_ok();
            let spire = format!(
                "{name}=https://oidc.example spiffe://td/openshell/gateway/ spire/oidc:443"
            )
            .parse::<ClusterSpire>()
            .is_ok();
            assert_eq!(
                (agent, person, spire),
                (profile, profile, profile),
                "{name:?}"
            );
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
    fn scopes_narrow_to_what_the_audience_lists() {
        assert_eq!(
            granted_scopes(None, &kueue()).unwrap(),
            [SCOPE_READ, SCOPE_SUBMIT]
        );
        assert_eq!(
            granted_scopes(Some(""), &kueue()).unwrap(),
            [SCOPE_READ, SCOPE_SUBMIT]
        );
        assert_eq!(
            granted_scopes(Some("kueue.read kueue.read"), &kueue()).unwrap(),
            [SCOPE_READ]
        );
        assert_eq!(
            granted_scopes(Some("kueue.admin"), &kueue()),
            Err(GrantError::Scope {
                requested: "kueue.admin".into(),
                grantable: vec![SCOPE_READ.into(), SCOPE_SUBMIT.into()]
            })
        );
        assert_eq!(
            granted_scopes(None, &buildit()).unwrap(),
            ["buildit.read", "buildit.build"]
        );
        assert!(
            granted_scopes(Some("kueue.read"), &buildit()).is_err(),
            "a scope another audience lists is not grantable here"
        );
        let bare: Audience = KUEUE.parse().unwrap();
        assert!(granted_scopes(None, &bare).unwrap().is_empty());
    }

    #[test]
    fn an_audience_takes_its_token_needs_from_the_server_the_profile_describes() {
        let servers = toml::from_str::<crucible::deploy::DeployProfile>(PROFILE)
            .unwrap()
            .agent_servers();
        let kueue = kueue().described_by(&servers);
        assert!(kueue.requires_kube_user);
        assert_eq!(kueue.namespaces, ["crucible-kueue-e2e"]);
        let elsewhere: Audience = format!("hub:{KUEUE}=kueue.read").parse().unwrap();
        let elsewhere = elsewhere.described_by(&servers);
        assert!(
            !elsewhere.requires_kube_user && elsewhere.namespaces.is_empty(),
            "the profile's server runs on {CLUSTER}, not hub"
        );
        for undescribed in [
            buildit(),
            "http://mcp.example/mcp=x.read".parse::<Audience>().unwrap(),
        ] {
            let undescribed = undescribed.described_by(&servers);
            assert!(!undescribed.requires_kube_user, "{}", undescribed.resource);
            assert!(
                undescribed.namespaces.is_empty(),
                "{}",
                undescribed.resource
            );
        }
    }

    fn run(dispatched_by: Option<&str>) -> BoundRun {
        BoundRun {
            run_id: "playbook_kueue-e2e_01a1-1791474738".into(),
            pod_name: "crucible-run-brave-otter".into(),
            cluster: CLUSTER.into(),
            dispatched_by: dispatched_by.map(str::to_string),
            sandbox_name: Some("ci-0123abcd".into()),
            workdir: Some("/sandbox/repo".into()),
        }
    }

    #[test]
    fn the_launcher_is_the_person_the_run_was_dispatched_for() {
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

    fn grant(namespaces: &[&str]) -> AgentGrant {
        AgentGrant {
            sandbox: Sandbox {
                spiffe_id: format!("{PREFIX}{POD}/sandbox/sbx-1"),
                cluster: CLUSTER.into(),
                tenant: None,
                gateway: GatewayId::of_run_pod(POD),
                id: crate::agent_auth::sandbox::SandboxId::parse("sbx-1").unwrap(),
            },
            launcher: "alice".into(),
            kube_user: Some("alice-k8s".into()),
            run: run(Some("user:alice")),
            audience: KUEUE.into(),
            scopes: vec![SCOPE_READ.into(), SCOPE_SUBMIT.into()],
            namespaces: namespaces.iter().map(|n| n.to_string()).collect(),
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
    async fn a_minted_token_carries_identity_claims_only_and_verifies() {
        let spire = Spire::start().await;
        let issuer = testing::issuer(&spire, &EcKey::generate());
        let at = now();
        let minted = issuer
            .mint(&grant(&["crucible-kueue-e2e", "team-b"]), at)
            .unwrap();
        assert_eq!(minted.expires_in, MIN_TOKEN_TTL_SECS);
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
                "exp": at + MIN_TOKEN_TTL_SECS,
                "act": {"sub": format!("{PREFIX}{POD}/sandbox/sbx-1")},
                "run_id": "crucible-run-brave-otter",
                "kube_cluster": "dev",
                "sandbox": "ci-0123abcd",
                "workdir": "/sandbox/repo",
                "scope": "kueue.read kueue.submit",
                "namespace": "crucible-kueue-e2e",
                "namespaces": ["crucible-kueue-e2e", "team-b"],
            })
        );
        let principal = crucible_mcp_auth::principal::Principal::from_claims(
            verified(&issuer, &minted.access_token).await,
            crucible_mcp_auth::principal::Callers::Runs,
        )
        .unwrap();
        assert_eq!(
            principal.presenter(),
            format!("{PREFIX}{POD}/sandbox/sbx-1")
        );
    }

    #[tokio::test]
    async fn every_token_gets_its_own_id_and_omits_the_namespace_and_kube_user_it_lacks() {
        let spire = Spire::start().await;
        let issuer = testing::issuer(&spire, &EcKey::generate());
        let unlinked = AgentGrant {
            kube_user: None,
            ..grant(&[])
        };
        let a = verified(
            &issuer,
            &issuer.mint(&unlinked, now()).unwrap().access_token,
        )
        .await;
        let b = verified(
            &issuer,
            &issuer.mint(&grant(&[]), now()).unwrap().access_token,
        )
        .await;
        assert_ne!(a["jti"], b["jti"]);
        for key in ["namespace", "namespaces", "queues", "role", "kube_user"] {
            assert!(!a.contains_key(key), "{key}");
        }
    }

    #[tokio::test]
    async fn a_rotation_keeps_tokens_from_the_previous_key_verifying_until_they_expire() {
        let spire = Spire::start().await;
        let old = EcKey::generate();
        let new = EcKey::generate();
        let before = testing::issuer(&spire, &old);
        let token = before.mint(&grant(&[]), now()).unwrap().access_token;
        let mut cfg = testing::config(&new);
        cfg.previous_key = Some(SigningKey::from_pem(old.pem().as_bytes()).unwrap());
        let after = AgentIssuer::new(cfg, |_| {
            Jwks::new(reqwest::Client::new(), spire.jwks_uri.clone())
        });
        let kids: Vec<_> = after
            .jwks()
            .keys
            .iter()
            .filter_map(|k| k.common.key_id.clone())
            .collect();
        assert_eq!(
            kids,
            [after.key.kid().to_string(), before.key.kid().to_string()]
        );
        let issued = verified(&after, &token).await;
        assert_eq!(
            issued["sub"], "alice",
            "signed by the previous key, published still"
        );
        let fresh = after.mint(&grant(&[]), now()).unwrap().access_token;
        assert_eq!(
            jsonwebtoken::decode_header(&fresh).unwrap().kid.as_deref(),
            Some(after.key.kid()),
            "new tokens are signed with the current key"
        );
        assert_eq!(after.jwks().keys.len(), 2);
        let mut same = testing::config(&new);
        same.previous_key = Some(SigningKey::from_pem(new.pem().as_bytes()).unwrap());
        let unrotated = AgentIssuer::new(same, |_| {
            Jwks::new(reqwest::Client::new(), spire.jwks_uri.clone())
        });
        assert_eq!(unrotated.jwks().keys.len(), 1, "one key is published once");
    }

    #[test]
    fn the_token_lifetime_is_fifteen_to_thirty_minutes() {
        assert_eq!(TokenTtl::default().secs(), 900);
        assert_eq!("1800".parse::<TokenTtl>().unwrap().secs(), 1800);
        for bad in ["899", "1801", "300", "soon", ""] {
            assert!(
                matches!(bad.parse::<TokenTtl>(), Err(ConfigError::Ttl(_))),
                "{bad}"
            );
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
        let doc = serde_json::to_value(issuer.metadata(None)).unwrap();
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
            (
                "CONTROLLER_AGENT_SPIRE",
                format!(
                    "{CLUSTER}={} {PREFIX} spire/oidc-discovery:443, \
                     spoke=https://oidc.spoke.example spiffe://spoke.example/openshell/gateway/ \
                     spire-system/spire-oidc:8443,",
                    testing::SVID_ISSUER
                ),
            ),
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
                    cluster: None,
                    scopes: vec!["kueue.read".into(), "kueue.submit".into()],
                    requires_kube_user: false,
                    namespaces: vec![],
                },
                Audience {
                    resource: BUILDIT.into(),
                    cluster: None,
                    scopes: vec!["buildit.read".into(), "buildit.build".into()],
                    requires_kube_user: false,
                    namespaces: vec![],
                },
            ]
        );
        assert_eq!(
            cfg.spire,
            [
                testing::cluster_spire(),
                ClusterSpire {
                    cluster: "spoke".into(),
                    svid_issuer: "https://oidc.spoke.example".into(),
                    gateway_prefix: GatewayPrefix::parse(
                        "spiffe://spoke.example/openshell/gateway/"
                    )
                    .unwrap(),
                    discovery: DiscoveryService {
                        tls: true,
                        namespace: KubeName::try_from("spire-system".to_string()).unwrap(),
                        name: KubeName::try_from("spire-oidc".to_string()).unwrap(),
                        port: 8443,
                    },
                },
            ]
        );
        assert_eq!(
            cfg.spire[1].discovery.keys_path(),
            "/api/v1/namespaces/spire-system/services/https:spire-oidc:8443/proxy/keys"
        );
        let plain: ClusterSpire = "waldorf=https://oidc-discovery.waldorf.crucible \
             spiffe://waldorf.crucible/openshell/gateway/ \
             http://spire-server/spire-spiffe-oidc-discovery-provider:80"
            .parse()
            .unwrap();
        assert_eq!(
            plain.discovery.keys_path(),
            "/api/v1/namespaces/spire-server/services/http:spire-spiffe-oidc-discovery-provider:80/proxy/keys",
            "a discovery provider without TLS is read over http"
        );

        assert!(
            AgentIssuerConfig::from_lookup(|_| None).unwrap().is_none(),
            "no issuer, no endpoint"
        );
        for var in DEPENDENT_VARS {
            let mut orphaned = vars.clone();
            orphaned.remove("CONTROLLER_AGENT_ISSUER");
            orphaned.insert(*var, "x".to_string());
            assert!(
                matches!(
                    AgentIssuerConfig::from_lookup(lookup(&orphaned)),
                    Err(ConfigError::NoIssuer { .. })
                ),
                "{var} without the issuer starts nothing silently"
            );
        }

        for missing in [
            "CONTROLLER_AGENT_SIGNING_KEY_FILE",
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

        let spire = |entry: &str| format!("{CLUSTER}={entry}");
        let twice = format!(
            "{},{}",
            spire(&format!("{} {PREFIX} spire/oidc:443", testing::SVID_ISSUER)),
            spire(&format!("https://other.example {PREFIX} spire/oidc:443"))
        );
        let same_issuer = format!(
            "{},other={} {PREFIX} spire/oidc:443",
            spire(&format!("{} {PREFIX} spire/oidc:443", testing::SVID_ISSUER)),
            testing::SVID_ISSUER
        );
        let bad: Vec<(&'static str, String)> = vec![
            (
                "CONTROLLER_AGENT_SPIRE",
                spire(
                    "https://oidc.example spiffe://example.crucible/openshell/gateway spire/oidc:443",
                ),
            ),
            (
                "CONTROLLER_AGENT_SPIRE",
                spire(
                    "https://oidc.example https://example.crucible/openshell/gateway/ spire/oidc:443",
                ),
            ),
            (
                "CONTROLLER_AGENT_SPIRE",
                spire(&format!("https://oidc.example {PREFIX}")),
            ),
            (
                "CONTROLLER_AGENT_SPIRE",
                spire(&format!("https://oidc.example {PREFIX} spire/oidc")),
            ),
            (
                "CONTROLLER_AGENT_SPIRE",
                spire(&format!("https://oidc.example {PREFIX} spire/oidc:https")),
            ),
            (
                "CONTROLLER_AGENT_SPIRE",
                spire(&format!("https://oidc.example {PREFIX} Spire_NS/oidc:443")),
            ),
            (
                "CONTROLLER_AGENT_SPIRE",
                format!("https://oidc.example {PREFIX} spire/oidc:443"),
            ),
            ("CONTROLLER_AGENT_SPIRE", " , ".to_string()),
            ("CONTROLLER_AGENT_SPIRE", twice),
            ("CONTROLLER_AGENT_SPIRE", same_issuer),
        ];
        let bad = bad.iter().map(|(v, s)| (*v, s.as_str())).chain([
            (
                "CONTROLLER_AGENT_ISSUER",
                "http://controller.svc:8080/oauth",
            ),
            ("CONTROLLER_AGENT_ISSUER", "http://controller.svc:8080/?x=1"),
            ("CONTROLLER_AGENT_ISSUER", "spiffe://example.crucible"),
            ("CONTROLLER_AGENT_AUDIENCES", " , "),
            ("CONTROLLER_AGENT_AUDIENCES", "not a url=kueue.read"),
            (
                "CONTROLLER_AGENT_SIGNING_KEY_FILE",
                "/nonexistent/signing.pem",
            ),
        ]);
        for (var, value) in bad {
            let mut broken = vars.clone();
            broken.insert(var, value.to_string());
            assert!(
                AgentIssuerConfig::from_lookup(lookup(&broken)).is_err(),
                "{var}={value}"
            );
        }
    }

    /// Without SPIRE the issuer serves people only: no agent audiences are needed, the key may come
    /// inline, and the metadata advertises no client_credentials grant.
    #[test]
    fn an_issuer_without_spire_serves_people_only() {
        let key = EcKey::generate();
        let vars = HashMap::from([
            ("CONTROLLER_AGENT_ISSUER", ISSUER.to_string()),
            ("CONTROLLER_AGENT_SIGNING_KEY", key.pem()),
        ]);
        let cfg = AgentIssuerConfig::from_lookup(lookup(&vars))
            .unwrap()
            .unwrap();
        assert!(cfg.spire.is_empty() && cfg.audiences.is_empty());
        let issuer = AgentIssuer::new(cfg, |_: &ClusterSpire| -> crucible_mcp_auth::jwks::Jwks {
            panic!("no SPIRE to read keys from")
        });
        assert!(!issuer.serves_agents());
        let people = crate::agent_auth::people::PeopleConfig::from_lookup(Some(format!(
            "pirate:{KUEUE}=kueue.read"
        )))
        .unwrap()
        .unwrap();
        let doc = issuer.metadata(Some(&PeopleMetadata {
            scopes: people.scopes(),
            client_id_metadata_documents: false,
            registration: false,
        }));
        assert_eq!(
            doc.grant_types_supported,
            ["authorization_code", "refresh_token"]
        );
        assert_eq!(doc.token_endpoint_auth_methods_supported, ["none"]);
        assert!(issuer.metadata(None).grant_types_supported.is_empty());

        let mut bad = vars.clone();
        bad.insert("CONTROLLER_AGENT_SIGNING_KEY", "not a key".into());
        assert!(matches!(
            AgentIssuerConfig::from_lookup(lookup(&bad)),
            Err(ConfigError::Key { .. })
        ));
    }
}
