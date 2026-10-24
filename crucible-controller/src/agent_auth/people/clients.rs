//! The public clients people may sign in with: the ones the operator registers (by default Claude
//! Code, pi and Codex), ones that publish a Client ID Metadata Document on an allowlisted host, and
//! native ones that registered themselves ([`crate::agent_auth::people::dynamic`]). A client is
//! trusted only with the redirect URIs its registration or document names.

use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, Ipv6Addr};
use url::Url;

pub const CLAUDE_CODE: &str = "claude-code";
pub const PI: &str = "pi";
pub const CODEX: &str = "codex";
/// Where the registry comes from when the built-in clients are not the ones wanted.
pub const CLIENTS_FILE_ENV: &str = "CONTROLLER_PEOPLE_CLIENTS_FILE";
const MAX_REGISTERED_ID: usize = 64;
const MAX_NAME: usize = 100;
const MAX_SERVER_ID: usize = 64;
const MAX_URL: usize = 2048;

/// A client identifier: a name the operator registered, the HTTPS URL of the client's metadata
/// document, or the id this controller issued to a client that registered itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum ClientId {
    Registered(RegisteredId),
    Metadata(MetadataUrl),
    Dynamic(DynamicId),
}

/// Who vouches for a client: the operator, the host publishing its document, or nobody.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Registered,
    Metadata,
    Dynamic,
}

/// A registered client's name: 1 to 64 of `A-Z a-z 0-9 . _ -`, not starting `dyn-`, so it can
/// never look like a metadata document URL or an issued id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RegisteredId(String);

/// An id issued at dynamic registration: `dyn-` and 16 to 64 base64url characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DynamicId(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ClientIdError {
    #[error(
        "client_id {0:?} is neither a registered name (1 to 64 of A-Z a-z 0-9 . _ -), an issued dyn- id, nor an https URL"
    )]
    Shape(String),
    #[error(transparent)]
    Metadata(#[from] MetadataUrlError),
}

/// The HTTPS URL of a client's metadata document, in canonical form: a host name, a path, and no
/// credentials, query or fragment. Compared character for character.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MetadataUrl(Url);

impl MetadataUrl {
    pub fn parse(raw: &str) -> Result<Self, MetadataUrlError> {
        let bad = || MetadataUrlError(raw.to_string());
        let url = Url::parse(raw).map_err(|_| bad())?;
        let ok = raw.len() <= MAX_URL
            && url.scheme() == "https"
            && matches!(url.host(), Some(url::Host::Domain(_)))
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() != "/"
            && url.as_str() == raw;
        match ok {
            true => Ok(Self(url)),
            false => Err(bad()),
        }
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn host(&self) -> &str {
        self.0.host_str().unwrap_or_default()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "client_id {0:?} is not a canonical https URL with a host name and a path, without credentials, query or fragment"
)]
pub struct MetadataUrlError(pub String);

impl RegisteredId {
    pub fn parse(raw: &str) -> Result<Self, ClientIdError> {
        let ok = (1..=MAX_REGISTERED_ID).contains(&raw.len())
            && !raw.starts_with(DynamicId::PREFIX)
            && raw
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        match ok {
            true => Ok(Self(raw.to_string())),
            false => Err(ClientIdError::Shape(raw.to_string())),
        }
    }
}

impl DynamicId {
    pub const PREFIX: &'static str = "dyn-";

    pub fn parse(raw: &str) -> Result<Self, ClientIdError> {
        let ok = raw.strip_prefix(Self::PREFIX).is_some_and(|rest| {
            (16..=64).contains(&rest.len())
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        });
        match ok {
            true => Ok(Self(raw.to_string())),
            false => Err(ClientIdError::Shape(raw.to_string())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ClientId {
    pub fn parse(raw: &str) -> Result<Self, ClientIdError> {
        if raw.starts_with("https://") {
            return Ok(Self::Metadata(MetadataUrl::parse(raw)?));
        }
        if raw.starts_with(DynamicId::PREFIX) {
            return Ok(Self::Dynamic(DynamicId::parse(raw)?));
        }
        Ok(Self::Registered(RegisteredId::parse(raw)?))
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Registered(RegisteredId(id)) => id,
            Self::Metadata(url) => url.as_str(),
            Self::Dynamic(id) => id.as_str(),
        }
    }

    pub fn kind(&self) -> ClientKind {
        match self {
            Self::Registered(_) => ClientKind::Registered,
            Self::Metadata(_) => ClientKind::Metadata,
            Self::Dynamic(_) => ClientKind::Dynamic,
        }
    }

    /// The host that publishes a metadata document client, `None` for any other.
    pub fn host(&self) -> Option<&str> {
        match self {
            Self::Metadata(url) => Some(url.host()),
            Self::Registered(_) | Self::Dynamic(_) => None,
        }
    }
}

impl std::fmt::Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<String> for ClientId {
    type Error = ClientIdError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

impl From<ClientId> for String {
    fn from(id: ClientId) -> Self {
        id.as_str().to_string()
    }
}

/// A loopback interface a native client listens on. `localhost` is matched by name and never
/// stands in for an address, nor an address for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopbackHost {
    Localhost,
    Ipv4,
    Ipv6,
}

impl LoopbackHost {
    fn of(url: &Url) -> Option<Self> {
        match url.host()? {
            url::Host::Domain("localhost") => Some(Self::Localhost),
            url::Host::Ipv4(ip) if ip == Ipv4Addr::LOCALHOST => Some(Self::Ipv4),
            url::Host::Ipv6(ip) if ip == Ipv6Addr::LOCALHOST => Some(Self::Ipv6),
            _ => None,
        }
    }
}

/// The path of a loopback redirect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackPath {
    Exact(String),
    /// The path followed by one segment of 1 to 64 base64url characters naming the MCP server,
    /// as Codex appends to a pre-registered client's callback.
    WithServerId(String),
}

impl CallbackPath {
    fn matches(&self, path: &str) -> bool {
        match self {
            Self::Exact(want) => path == want,
            Self::WithServerId(base) => path
                .strip_prefix(base.as_str())
                .and_then(|rest| rest.strip_prefix('/'))
                .is_some_and(|id| {
                    (1..=MAX_SERVER_ID).contains(&id.len())
                        && id
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                }),
        }
    }
}

/// A registered redirect URI. A loopback `http` URI admits any port on that interface with the
/// same path (RFC 8252 §7.3); anything else must be `https` and is matched character for character.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectRule {
    Loopback {
        host: LoopbackHost,
        path: CallbackPath,
    },
    Exact(Url),
}

/// Where a redirect rule was written: only the operator may use the `/*` server id suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleSource {
    Registry,
    Document,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "redirect URI {0:?} is neither http on localhost, 127.0.0.1 or [::1] nor an https URL, without credentials or fragment"
)]
pub struct BadRedirectRule(pub String);

impl RedirectRule {
    pub fn parse(raw: &str, source: RuleSource) -> Result<Self, BadRedirectRule> {
        let bad = || BadRedirectRule(raw.to_string());
        let url = Url::parse(raw).map_err(|_| bad())?;
        if url.fragment().is_some() || !url.username().is_empty() || url.password().is_some() {
            return Err(bad());
        }
        match (url.scheme(), LoopbackHost::of(&url)) {
            ("http", Some(host)) => {
                if url.query().is_some() {
                    return Err(bad());
                }
                let path = match (source, url.path().strip_suffix("/*")) {
                    (RuleSource::Registry, Some(base)) => CallbackPath::WithServerId(base.into()),
                    _ => CallbackPath::Exact(url.path().to_string()),
                };
                Ok(Self::Loopback { host, path })
            }
            ("https", _) if url.host().is_some() && !url.path().contains('*') => {
                Ok(Self::Exact(url))
            }
            _ => Err(bad()),
        }
    }

    fn admits(&self, url: &Url, raw: &str) -> bool {
        match self {
            Self::Loopback { host, path } => {
                url.scheme() == "http"
                    && LoopbackHost::of(url) == Some(*host)
                    && url.port().is_some_and(|p| p != 0)
                    && url.query().is_none()
                    && path.matches(url.path())
            }
            Self::Exact(want) => want.as_str() == raw,
        }
    }
}

/// A redirect URI an authorization request named and its client registered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedirectUri(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("redirect_uri {uri:?} is not one client {client} registered")]
pub struct RedirectRefused {
    pub uri: String,
    pub client: String,
}

impl RedirectUri {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `host:port` the code is sent to, for the consent page.
    pub fn origin(&self) -> String {
        match Url::parse(&self.0) {
            Ok(url) => match url.port_or_known_default() {
                Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
                None => url.host_str().unwrap_or_default().to_string(),
            },
            Err(_) => self.0.clone(),
        }
    }

    pub(crate) fn with(&self, params: &[(&str, &str)]) -> String {
        match Url::parse(&self.0) {
            Ok(mut url) => {
                url.query_pairs_mut().extend_pairs(params);
                url.to_string()
            }
            Err(_) => self.0.clone(),
        }
    }
}

/// A client people may approve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    pub id: ClientId,
    /// What the consent page and Settings call it.
    pub name: String,
    pub redirects: Vec<RedirectRule>,
    /// The resources it may get tokens for; `None` is every configured audience.
    pub resources: Option<Vec<String>>,
}

impl Client {
    /// `raw` when it is canonical, carries no fragment or credentials, and a rule admits it.
    pub fn redirect(&self, raw: &str) -> Result<RedirectUri, RedirectRefused> {
        let refused = || RedirectRefused {
            uri: raw.to_string(),
            client: self.id.to_string(),
        };
        let url = Url::parse(raw).map_err(|_| refused())?;
        let clean = url.as_str() == raw
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none();
        match clean && self.redirects.iter().any(|r| r.admits(&url, raw)) {
            true => Ok(RedirectUri(raw.to_string())),
            false => Err(refused()),
        }
    }

    pub fn allows_resource(&self, resource: &str) -> bool {
        self.resources
            .as_ref()
            .is_none_or(|r| r.iter().any(|r| r == resource))
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegistryError {
    #[error("reading {CLIENTS_FILE_ENV} {path}: {reason}")]
    Read { path: String, reason: String },
    #[error("{CLIENTS_FILE_ENV} {path} is not a [[clients]] list: {reason}")]
    Toml { path: String, reason: String },
    #[error("{CLIENTS_FILE_ENV} lists no client")]
    Empty,
    #[error("registered client_id {0:?} is not 1 to 64 of A-Z a-z 0-9 . _ -")]
    Id(String),
    #[error("client_id {0:?} is registered twice")]
    Duplicate(String),
    #[error("client {client} needs a name of 1 to {MAX_NAME} printable characters")]
    Name { client: String },
    #[error("client {client} registers no redirect URI")]
    NoRedirects { client: String },
    #[error("client {client}: {source}")]
    Redirect {
        client: String,
        source: BadRedirectRule,
    },
    #[error(
        "client {client} names resource {resource:?}, which CONTROLLER_PEOPLE_AUDIENCES does not"
    )]
    Resource { client: String, resource: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryFile {
    clients: Vec<RegistryEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryEntry {
    client_id: String,
    name: String,
    redirect_uris: Vec<String>,
    resources: Option<Vec<String>>,
}

pub(crate) fn display_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    let ok =
        !name.is_empty() && name.chars().count() <= MAX_NAME && !name.chars().any(char::is_control);
    ok.then(|| name.to_string())
}

fn registered(entry: RegistryEntry) -> Result<Client, RegistryError> {
    let id = RegisteredId::parse(&entry.client_id)
        .map_err(|_| RegistryError::Id(entry.client_id.clone()))?;
    let client = entry.client_id;
    let name = display_name(&entry.name).ok_or_else(|| RegistryError::Name {
        client: client.clone(),
    })?;
    if entry.redirect_uris.is_empty() {
        return Err(RegistryError::NoRedirects { client });
    }
    let redirects = entry
        .redirect_uris
        .iter()
        .map(|raw| RedirectRule::parse(raw, RuleSource::Registry))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| RegistryError::Redirect {
            client: client.clone(),
            source,
        })?;
    Ok(Client {
        id: ClientId::Registered(id),
        name,
        redirects,
        resources: entry.resources,
    })
}

/// Claude Code, pi and Codex, each with the loopback callback it actually sends.
pub fn builtin() -> Vec<Client> {
    let loopback = |host, path| RedirectRule::Loopback { host, path };
    let callback = || CallbackPath::Exact("/callback".into());
    let client = |id: &str, name: &str, redirects| Client {
        id: ClientId::Registered(RegisteredId(id.to_string())),
        name: name.to_string(),
        redirects,
        resources: None,
    };
    vec![
        client(
            CLAUDE_CODE,
            "Claude Code",
            vec![loopback(LoopbackHost::Localhost, callback())],
        ),
        client(PI, "pi", vec![loopback(LoopbackHost::Ipv4, callback())]),
        client(
            CODEX,
            "Codex",
            vec![
                loopback(LoopbackHost::Ipv4, callback()),
                loopback(
                    LoopbackHost::Ipv4,
                    CallbackPath::WithServerId("/callback".into()),
                ),
            ],
        ),
    ]
}

/// The registry in a TOML file of `[[clients]]` tables, in place of the built-in clients.
pub fn from_toml(path: &str, raw: &str) -> Result<Vec<Client>, RegistryError> {
    let file: RegistryFile = toml::from_str(raw).map_err(|e| RegistryError::Toml {
        path: path.to_string(),
        reason: e.message().to_string(),
    })?;
    if file.clients.is_empty() {
        return Err(RegistryError::Empty);
    }
    let mut clients: Vec<Client> = Vec::new();
    for entry in file.clients {
        let client = registered(entry)?;
        if clients.iter().any(|c| c.id == client.id) {
            return Err(RegistryError::Duplicate(client.id.to_string()));
        }
        clients.push(client);
    }
    Ok(clients)
}

/// The registry `path` names, or the built-in clients without one.
pub fn from_file(path: Option<&str>) -> Result<Vec<Client>, RegistryError> {
    let Some(path) = path else {
        return Ok(builtin());
    };
    let raw = std::fs::read_to_string(path).map_err(|e| RegistryError::Read {
        path: path.to_string(),
        reason: e.to_string(),
    })?;
    from_toml(path, &raw)
}

#[cfg(test)]
mod tests {
    use crate::agent_auth::people::clients::*;

    fn client(id: &str) -> Client {
        builtin().into_iter().find(|c| c.id.as_str() == id).unwrap()
    }

    fn admits(id: &str, uri: &str) -> bool {
        client(id).redirect(uri).is_ok()
    }

    const ANY: &[&str] = &[
        "http://localhost/callback",
        "http://localhost:0/callback",
        "https://localhost:8765/callback",
        "http://localhost:8765/callback#f",
        "http://localhost:8765/callback?x=1",
        "http://user@localhost:8765/callback",
        "http://LOCALHOST:8765/callback",
        "http://localhost.evil.example:8765/callback",
        "http://127.0.0.2:8765/callback",
        "http://0.0.0.0:8765/callback",
        "http://[::ffff:127.0.0.1]:8765/callback",
        "http://evil.example:8765/callback",
        "https://evil.example/callback",
        "http://127.0.0.1:8765/x/../callback",
        "http://127.0.0.1:8765/callback#",
        "custom-scheme://callback",
        "",
    ];

    #[test]
    fn claude_code_gets_any_localhost_port_on_callback_and_nothing_else() {
        for good in [
            "http://localhost:8765/callback",
            "http://localhost:49152/callback",
            "http://localhost:1/callback",
        ] {
            assert!(admits(CLAUDE_CODE, good), "{good}");
        }
        for bad in ANY.iter().copied().chain([
            "http://127.0.0.1:8765/callback",
            "http://[::1]:8765/callback",
            "http://localhost:8765/",
            "http://localhost:8765/callback/",
            "http://localhost:8765/Callback",
            "http://localhost:8765/callback/abcdefghijkl",
        ]) {
            assert!(!admits(CLAUDE_CODE, bad), "{bad:?} must be refused");
        }
    }

    #[test]
    fn pi_gets_any_ipv4_loopback_port_on_callback_and_nothing_else() {
        for good in [
            "http://127.0.0.1:8765/callback",
            "http://127.0.0.1:65535/callback",
        ] {
            assert!(admits(PI, good), "{good}");
        }
        for bad in ANY.iter().copied().chain([
            "http://localhost:8765/callback",
            "http://[::1]:8765/callback",
            "http://127.0.0.1:8765/other",
            "http://127.0.0.1:8765/callback/abcdefghijkl",
        ]) {
            assert!(!admits(PI, bad), "{bad:?} must be refused");
        }
    }

    /// Codex appends a 12 character id of the MCP server to a pre-registered client's callback.
    #[test]
    fn codex_gets_the_ipv4_loopback_callback_with_or_without_its_server_id() {
        for good in [
            "http://127.0.0.1:8765/callback",
            "http://127.0.0.1:51234/callback/3q2-7wAAAAAA",
            "http://127.0.0.1:51234/callback/a_b",
        ] {
            assert!(admits(CODEX, good), "{good}");
        }
        let long = format!("http://127.0.0.1:51234/callback/{}", "a".repeat(65));
        for bad in ANY.iter().copied().chain([
            "http://localhost:51234/callback/3q2-7wAAAAAA",
            "http://127.0.0.1:51234/callback/",
            "http://127.0.0.1:51234/callback/a/b",
            "http://127.0.0.1:51234/callback/a.b",
            "http://127.0.0.1:51234/callback/3q2-7wAAAAAA#f",
            "http://127.0.0.1:51234/callbackx",
            "http://127.0.0.1:51234/other/3q2-7wAAAAAA",
            "http://evil.example:51234/callback/3q2-7wAAAAAA",
            long.as_str(),
        ]) {
            assert!(!admits(CODEX, bad), "{bad:?} must be refused");
        }
    }

    #[test]
    fn rules_parse_loopback_as_any_port_and_https_as_exact() {
        assert_eq!(
            RedirectRule::parse("http://127.0.0.1:3000/callback", RuleSource::Document),
            Ok(RedirectRule::Loopback {
                host: LoopbackHost::Ipv4,
                path: CallbackPath::Exact("/callback".into())
            }),
            "a document's loopback port is not a restriction"
        );
        assert_eq!(
            RedirectRule::parse("http://[::1]/cb/*", RuleSource::Registry),
            Ok(RedirectRule::Loopback {
                host: LoopbackHost::Ipv6,
                path: CallbackPath::WithServerId("/cb".into())
            })
        );
        assert_eq!(
            RedirectRule::parse("http://[::1]/cb/*", RuleSource::Document),
            Ok(RedirectRule::Loopback {
                host: LoopbackHost::Ipv6,
                path: CallbackPath::Exact("/cb/*".into())
            }),
            "only the operator may register a server id suffix"
        );
        let web =
            RedirectRule::parse("https://app.example/oauth/cb", RuleSource::Registry).unwrap();
        let c = Client {
            id: ClientId::parse("web").unwrap(),
            name: "Web".into(),
            redirects: vec![web],
            resources: None,
        };
        assert!(c.redirect("https://app.example/oauth/cb").is_ok());
        for bad in [
            "https://app.example/oauth/cb/",
            "https://app.example:8443/oauth/cb",
            "https://app.example/oauth/cb?x=1",
            "http://app.example/oauth/cb",
        ] {
            assert!(c.redirect(bad).is_err(), "{bad}");
        }
        for bad in [
            "http://app.example/cb",
            "http://localhost/cb?x=1",
            "http://localhost/cb#f",
            "https://user@app.example/cb",
            "https://app.example/*",
            "com.example.app:/cb",
            "not a url",
        ] {
            assert!(
                RedirectRule::parse(bad, RuleSource::Registry).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn client_ids_are_registered_names_or_https_urls() {
        assert!(matches!(
            ClientId::parse("claude-code"),
            Ok(ClientId::Registered(_))
        ));
        assert!(matches!(
            ClientId::parse("https://chatgpt.com/oauth/codex/client.json"),
            Ok(ClientId::Metadata(_))
        ));
        for bad in [
            "",
            "has space",
            "a/b",
            "http://x.example/c",
            &"a".repeat(65),
            "dyn-short",
            "dyn-0123456789abcdef!",
        ] {
            assert!(ClientId::parse(bad).is_err(), "{bad:?}");
        }
        assert!(matches!(
            ClientId::parse("dyn-0123456789abcdef_-"),
            Ok(ClientId::Dynamic(_))
        ));
        assert!(
            RegisteredId::parse("dyn-0123456789abcdef").is_err(),
            "a registry name cannot pose as an issued id"
        );
        let id = ClientId::parse("https://pi.dev/client.json").unwrap();
        assert_eq!(id.host(), Some("pi.dev"));
        assert_eq!(
            serde_json::to_value(&id).unwrap(),
            "https://pi.dev/client.json"
        );
        assert_eq!(
            serde_json::from_value::<ClientId>(serde_json::json!("pi")).unwrap(),
            ClientId::parse("pi").unwrap()
        );
        assert!(serde_json::from_value::<ClientId>(serde_json::json!("a b")).is_err());
    }

    #[test]
    fn a_registry_file_replaces_the_builtins_and_is_checked_whole() {
        let clients = from_toml(
            "clients.toml",
            r#"
[[clients]]
client_id = "claude-code"
name = "Claude Code"
redirect_uris = ["http://localhost/callback"]

[[clients]]
client_id = "lab-cli"
name = "Lab CLI"
redirect_uris = ["http://127.0.0.1/oauth/done", "https://lab.example/cb"]
resources = ["https://kueue.example/mcp"]
"#,
        )
        .unwrap();
        assert_eq!(clients.len(), 2);
        let lab = &clients[1];
        assert_eq!(lab.name, "Lab CLI");
        assert!(lab.redirect("http://127.0.0.1:9000/oauth/done").is_ok());
        assert!(lab.redirect("https://lab.example/cb").is_ok());
        assert!(lab.allows_resource("https://kueue.example/mcp"));
        assert!(!lab.allows_resource("https://other.example/mcp"));
        assert!(clients[0].allows_resource("https://other.example/mcp"));

        let entry = |id: &str, name: &str, uris: &str| {
            format!("[[clients]]\nclient_id = {id:?}\nname = {name:?}\nredirect_uris = {uris}\n")
        };
        for (raw, want) in [
            ("clients = []".to_string(), "Empty"),
            ("[[clients]]\nclient_id = \"a\"".to_string(), "Toml"),
            (
                format!(
                    "{}extra = 1\n",
                    entry("a", "A", r#"["http://localhost/cb"]"#)
                ),
                "Toml",
            ),
            (
                entry("https://x.example/c", "X", r#"["http://localhost/cb"]"#),
                "Id",
            ),
            (entry("a", " ", r#"["http://localhost/cb"]"#), "Name"),
            (entry("a", "A", "[]"), "NoRedirects"),
            (entry("a", "A", r#"["http://a.example/cb"]"#), "Redirect"),
            (
                format!(
                    "{}{}",
                    entry("a", "A", r#"["http://localhost/cb"]"#),
                    entry("a", "B", r#"["http://localhost/cb"]"#)
                ),
                "Duplicate",
            ),
        ] {
            let err = from_toml("clients.toml", &raw).unwrap_err();
            assert!(format!("{err:?}").starts_with(want), "{raw}: {err:?}");
        }
        assert_eq!(from_file(None).unwrap(), builtin());
        assert!(matches!(
            from_file(Some("/nonexistent/clients.toml")),
            Err(RegistryError::Read { .. })
        ));
    }
}
