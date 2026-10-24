//! OAuth Client ID Metadata Documents (draft-ietf-oauth-client-id-metadata-document): a client whose
//! `client_id` is an HTTPS URL is whatever the JSON at that URL says, when the URL's host is one the
//! operator allowlisted. Only public clients are accepted. The fetch follows no redirect, reaches no
//! special-use address (RFC 6890), reads at most [`CimdLimits::max_bytes`], and a valid document is
//! cached for as long as its `Cache-Control` allows, within [`CimdLimits::max_ttl`].

use crate::agent_auth::people::clients::{
    Client, ClientId, MetadataUrl, MetadataUrlError, RedirectRule, RuleSource, display_name,
};
use reqwest::header::{AGE, CACHE_CONTROL, CONTENT_TYPE};
use serde::Deserialize;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use url::Url;

pub const CIMD_HOSTS_ENV: &str = "CONTROLLER_PEOPLE_CIMD_HOSTS";
const CACHE_CAPACITY: u64 = 256;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CimdError {
    #[error(transparent)]
    Url(#[from] MetadataUrlError),
    #[error("client metadata host {0:?} is not on {CIMD_HOSTS_ENV}")]
    Host(String),
    #[error("fetching client metadata {url}: {reason}")]
    Fetch { url: String, reason: String },
    #[error("fetching client metadata {url} took longer than {millis} ms")]
    Timeout { url: String, millis: u128 },
    #[error("client metadata {url} answered {status}, not 200")]
    Status { url: String, status: u16 },
    #[error("client metadata {url} is {content_type:?}, not JSON")]
    ContentType { url: String, content_type: String },
    #[error("client metadata {url} is larger than {limit} bytes")]
    TooLarge { url: String, limit: usize },
    #[error("client metadata {url} is not a JSON client metadata object: {reason}")]
    Json { url: String, reason: String },
    #[error("client metadata {url} names client_id {document:?}, not its own URL")]
    ClientId { url: String, document: String },
    #[error(
        "client metadata {url} has token_endpoint_auth_method {method:?}; only \"none\" is accepted"
    )]
    AuthMethod { url: String, method: Option<String> },
    #[error("client metadata {url} carries a client secret")]
    Secret { url: String },
    #[error(
        "client metadata {url} names no grant_types with authorization_code or response_types with code"
    )]
    GrantType { url: String },
    #[error("client metadata {url} needs a client_name of 1 to 100 printable characters")]
    Name { url: String },
    #[error("client metadata {url} lists no redirect_uris")]
    NoRedirects { url: String },
    #[error("client metadata {url}: {reason}")]
    Redirect { url: String, reason: String },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CimdConfigError {
    #[error("{CIMD_HOSTS_ENV} entry {0:?} is not a host name")]
    Host(String),
    #[error("{CIMD_HOSTS_ENV} is set but lists no host")]
    Empty,
}

/// How much a fetch may cost and how long its answer is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CimdLimits {
    pub max_bytes: usize,
    pub timeout: Duration,
    /// How long a document without `max-age` is kept.
    pub default_ttl: Duration,
    pub max_ttl: Duration,
}

impl Default for CimdLimits {
    fn default() -> Self {
        Self {
            max_bytes: 5 * 1024,
            timeout: Duration::from_secs(5),
            default_ttl: Duration::from_secs(300),
            max_ttl: Duration::from_secs(3600),
        }
    }
}

/// Which hosts may publish metadata documents, and the fetch limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CimdPolicy {
    pub hosts: Vec<String>,
    pub limits: CimdLimits,
}

impl CimdPolicy {
    /// `CONTROLLER_PEOPLE_CIMD_HOSTS`, comma separated host names; `None` when unset or blank.
    pub fn from_lookup(raw: Option<String>) -> Result<Option<Self>, CimdConfigError> {
        let Some(raw) = raw.filter(|r| !r.trim().is_empty()) else {
            return Ok(None);
        };
        let mut hosts: Vec<String> = Vec::new();
        for host in raw.split(',').map(str::trim).filter(|h| !h.is_empty()) {
            let parsed = Url::parse(&format!("https://{host}/")).ok();
            let ok = parsed.as_ref().is_some_and(|u| {
                matches!(u.host(), Some(url::Host::Domain(_)))
                    && u.host_str() == Some(host)
                    && u.port().is_none()
                    && u.path() == "/"
            });
            if !ok {
                return Err(CimdConfigError::Host(host.to_string()));
            }
            if !hosts.iter().any(|h| h == host) {
                hosts.push(host.to_string());
            }
        }
        if hosts.is_empty() {
            return Err(CimdConfigError::Empty);
        }
        Ok(Some(Self {
            hosts,
            limits: CimdLimits::default(),
        }))
    }
}

/// Not a special-use address of RFC 6890 (or its IPv6 successors): somewhere a public client
/// could publish a document.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public_v4(v4),
            None => is_public_v6(v6),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || a == 0
        || a >= 240
        || (a == 100 && (64..128).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (b == 18 || b == 19)))
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00
        || (s[0] & 0xffc0) == 0xfe80
        || (s[0] == 0x2001 && s[1] == 0x0db8)
        || (s[0] == 0x2001 && s[1] < 0x0200)
        || (s[0] == 0x0064 && s[1] == 0xff9b)
        || (s[0] == 0x0100 && s[1] == 0 && s[2] == 0 && s[3] == 0)
        || s[0] == 0)
}

/// The resolver every metadata fetch uses: the system's, minus special-use addresses.
struct PublicAddresses;

impl reqwest::dns::Resolve for PublicAddresses {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let public: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| is_public(a.ip()))
                .collect();
            if public.is_empty() {
                return Err(format!("{host} resolves to no public address").into());
            }
            let addrs: reqwest::dns::Addrs = Box::new(public.into_iter());
            Ok(addrs)
        })
    }
}

#[derive(Debug, Deserialize)]
struct Document {
    client_id: Option<String>,
    client_name: Option<String>,
    redirect_uris: Option<Vec<String>>,
    token_endpoint_auth_method: Option<String>,
    client_secret: Option<serde_json::Value>,
    client_secret_expires_at: Option<serde_json::Value>,
    grant_types: Option<Vec<String>>,
    response_types: Option<Vec<String>>,
}

/// The client a metadata document describes, when it is a public client of this URL.
fn client_of(url: &MetadataUrl, body: &[u8]) -> Result<Client, CimdError> {
    let at = || url.as_str().to_string();
    let doc: Document = serde_json::from_slice(body).map_err(|e| CimdError::Json {
        url: at(),
        reason: e.to_string(),
    })?;
    let named = doc.client_id.unwrap_or_default();
    if named != url.as_str() {
        return Err(CimdError::ClientId {
            url: at(),
            document: named,
        });
    }
    if doc.client_secret.is_some() || doc.client_secret_expires_at.is_some() {
        return Err(CimdError::Secret { url: at() });
    }
    if doc.token_endpoint_auth_method.as_deref() != Some("none") {
        return Err(CimdError::AuthMethod {
            url: at(),
            method: doc.token_endpoint_auth_method,
        });
    }
    let offers = |list: &Option<Vec<String>>, want: &str| {
        list.as_ref().is_none_or(|l| l.iter().any(|g| g == want))
    };
    if !offers(&doc.grant_types, "authorization_code") || !offers(&doc.response_types, "code") {
        return Err(CimdError::GrantType { url: at() });
    }
    let name = doc
        .client_name
        .as_deref()
        .and_then(display_name)
        .ok_or_else(|| CimdError::Name { url: at() })?;
    let uris = doc.redirect_uris.unwrap_or_default();
    if uris.is_empty() {
        return Err(CimdError::NoRedirects { url: at() });
    }
    let redirects = uris
        .iter()
        .map(|raw| RedirectRule::parse(raw, RuleSource::Document))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| CimdError::Redirect {
            url: at(),
            reason: e.to_string(),
        })?;
    Ok(Client {
        id: ClientId::Metadata(url.clone()),
        name,
        redirects,
        resources: None,
    })
}

fn is_json(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    essence == "application/json"
        || (essence.starts_with("application/") && essence.ends_with("+json"))
}

/// How long a response may be reused under RFC 9111 §5.2.2 and §5.1, within `limits`; `None` when
/// it must not be stored.
fn freshness(headers: &reqwest::header::HeaderMap, limits: &CimdLimits) -> Option<Duration> {
    let mut max_age: Option<u64> = None;
    for value in headers.get_all(CACHE_CONTROL) {
        let Ok(value) = value.to_str() else {
            return None;
        };
        for directive in value.split(',') {
            let (name, arg) = directive
                .split_once('=')
                .map_or((directive, None), |(n, a)| (n, Some(a)));
            match name.trim().to_ascii_lowercase().as_str() {
                "no-store" | "no-cache" => return None,
                "max-age" => {
                    let secs = arg
                        .map(|a| a.trim().trim_matches('"'))
                        .and_then(|a| a.parse().ok());
                    max_age = Some(secs?);
                }
                _ => {}
            }
        }
    }
    let age = headers
        .get(AGE)
        .and_then(|a| a.to_str().ok())
        .and_then(|a| a.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let lifetime = max_age.map_or(limits.default_ttl, Duration::from_secs);
    let ttl = lifetime
        .saturating_sub(Duration::from_secs(age))
        .min(limits.max_ttl);
    (!ttl.is_zero()).then_some(ttl)
}

#[derive(Clone)]
struct Cached {
    client: Client,
    ttl: Duration,
}

struct CachedExpiry;

impl moka::Expiry<MetadataUrl, Cached> for CachedExpiry {
    fn expire_after_create(
        &self,
        _key: &MetadataUrl,
        value: &Cached,
        _created_at: std::time::Instant,
    ) -> Option<Duration> {
        Some(value.ttl)
    }

    fn expire_after_update(
        &self,
        _key: &MetadataUrl,
        value: &Cached,
        _updated_at: std::time::Instant,
        _duration_until_expiry: Option<Duration>,
    ) -> Option<Duration> {
        Some(value.ttl)
    }
}

/// Resolves metadata document clients for the allowlisted hosts.
pub struct Cimd {
    policy: CimdPolicy,
    http: reqwest::Client,
    cache: moka::future::Cache<MetadataUrl, Cached>,
}

impl Cimd {
    /// `builder` is the HTTP client to start from; the fetch rules are applied on top of it.
    pub fn new(policy: CimdPolicy, builder: reqwest::ClientBuilder) -> anyhow::Result<Self> {
        use anyhow::Context;
        let http = builder
            .redirect(reqwest::redirect::Policy::none())
            .https_only(true)
            .timeout(policy.limits.timeout)
            .dns_resolver(std::sync::Arc::new(PublicAddresses))
            .build()
            .context("building the client metadata HTTP client")?;
        Ok(Self {
            policy,
            http,
            cache: moka::future::Cache::builder()
                .max_capacity(CACHE_CAPACITY)
                .expire_after(CachedExpiry)
                .build(),
        })
    }

    pub fn hosts(&self) -> &[String] {
        &self.policy.hosts
    }

    /// Whether `url`'s host may publish clients.
    pub fn admits(&self, url: &MetadataUrl) -> Result<(), CimdError> {
        match self.policy.hosts.iter().any(|h| h == url.host()) {
            true => Ok(()),
            false => Err(CimdError::Host(url.host().to_string())),
        }
    }

    /// The client `url` describes, from the cache while it is fresh.
    pub async fn client(&self, url: &MetadataUrl) -> Result<Client, CimdError> {
        self.admits(url)?;
        if let Some(hit) = self.cache.get(url).await {
            return Ok(hit.client);
        }
        let (client, ttl) = self.fetch(url).await?;
        if let Some(ttl) = ttl {
            self.cache
                .insert(
                    url.clone(),
                    Cached {
                        client: client.clone(),
                        ttl,
                    },
                )
                .await;
        }
        Ok(client)
    }

    async fn fetch(&self, url: &MetadataUrl) -> Result<(Client, Option<Duration>), CimdError> {
        let at = || url.as_str().to_string();
        let limits = &self.policy.limits;
        let failed = |e: reqwest::Error| match e.is_timeout() {
            true => CimdError::Timeout {
                url: at(),
                millis: limits.timeout.as_millis(),
            },
            false => CimdError::Fetch {
                url: at(),
                reason: format!("{:#}", anyhow::Error::from(e.without_url())),
            },
        };
        let mut response = self
            .http
            .get(url.as_str())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(failed)?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(CimdError::Status {
                url: at(),
                status: response.status().as_u16(),
            });
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if !is_json(&content_type) {
            return Err(CimdError::ContentType {
                url: at(),
                content_type,
            });
        }
        let too_large = || CimdError::TooLarge {
            url: at(),
            limit: limits.max_bytes,
        };
        let declared = response.content_length().unwrap_or(0);
        if usize::try_from(declared).map_or(true, |n| n > limits.max_bytes) {
            return Err(too_large());
        }
        let ttl = freshness(response.headers(), limits);
        let mut body: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(failed)? {
            if body.len().saturating_add(chunk.len()) > limits.max_bytes {
                return Err(too_large());
            }
            body.extend_from_slice(&chunk);
        }
        let client = client_of(url, &body)?;
        tracing::info!(
            client_id = url.as_str(),
            client_name = client.name,
            ttl_secs = ttl.map(|t| t.as_secs()),
            "client metadata document fetched"
        );
        Ok((client, ttl))
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    /// The host test documents are published on.
    pub const DOC_HOST: &str = "client.example";
    /// Another host the same server answers for, which no test allowlists.
    pub const OTHER_HOST: &str = "elsewhere.example";

    /// An HTTPS server for `DOC_HOST` and `OTHER_HOST` on 127.0.0.1, with its own self-signed
    /// certificate.
    pub struct DocServer {
        pub addr: SocketAddr,
        root: Vec<u8>,
        _dir: tempfile::TempDir,
    }

    impl DocServer {
        pub async fn start(app: axum::Router) -> Self {
            let issued =
                rcgen::generate_simple_self_signed(vec![DOC_HOST.into(), OTHER_HOST.into()])
                    .unwrap();
            let dir = tempfile::tempdir().unwrap();
            let cert = dir.path().join("tls.crt");
            let key = dir.path().join("tls.key");
            std::fs::write(&cert, issued.cert.pem()).unwrap();
            std::fs::write(&key, issued.signing_key.serialize_pem()).unwrap();
            let front = crate::tls::TlsFront::new(
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                cert,
                key,
            );
            let listener = crate::tls::bind(&front).await.unwrap();
            let addr = listener.addr();
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            Self {
                addr,
                root: issued.cert.pem().into_bytes(),
                _dir: dir,
            }
        }

        /// `https://<host>:<port><path>`.
        pub fn url(&self, host: &str, path: &str) -> String {
            format!("https://{host}:{}{path}", self.addr.port())
        }

        /// A client builder that trusts this server and finds both hosts on it.
        pub fn builder(&self) -> reqwest::ClientBuilder {
            reqwest::Client::builder()
                .no_proxy()
                .tls_certs_only([reqwest::Certificate::from_pem(&self.root).unwrap()])
                .resolve(DOC_HOST, self.addr)
                .resolve(OTHER_HOST, self.addr)
        }
    }

    /// A document for `url` as Codex publishes its own.
    pub fn document(url: &str, name: &str) -> serde_json::Value {
        serde_json::json!({
            "client_id": url,
            "client_name": name,
            "client_uri": "https://client.example/",
            "application_type": "native",
            "redirect_uris": ["http://127.0.0.1/callback", "http://localhost/callback"],
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::agent_auth::people::cimd::testing::{DOC_HOST, DocServer, OTHER_HOST, document};
    use crate::agent_auth::people::cimd::*;
    use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn policy(hosts: &[&str]) -> CimdPolicy {
        CimdPolicy {
            hosts: hosts.iter().map(|h| h.to_string()).collect(),
            limits: CimdLimits {
                timeout: Duration::from_millis(500),
                ..CimdLimits::default()
            },
        }
    }

    fn json_reply(body: Value, cache: Option<&'static str>) -> axum::response::Response {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        if let Some(cache) = cache {
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
        }
        (StatusCode::OK, headers, body.to_string()).into_response()
    }

    struct Fixture {
        server: DocServer,
        hits: Arc<AtomicUsize>,
    }

    impl Fixture {
        fn url(&self, path: &str) -> MetadataUrl {
            MetadataUrl::parse(&self.server.url(DOC_HOST, path)).unwrap()
        }

        fn cimd(&self, hosts: &[&str]) -> Cimd {
            Cimd::new(policy(hosts), self.server.builder()).unwrap()
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }

    /// One server publishing every document shape the tests need, counting every request.
    async fn fixture() -> Fixture {
        let hits = Arc::new(AtomicUsize::new(0));
        let port = Arc::new(std::sync::OnceLock::<u16>::new());
        let at = {
            let port = port.clone();
            move |path: &str| {
                format!(
                    "https://{DOC_HOST}:{}{path}",
                    port.get().copied().unwrap_or_default()
                )
            }
        };
        let doc = {
            let at = at.clone();
            move |path: &'static str, edit: fn(&mut Value)| {
                let at = at.clone();
                get(move || async move {
                    let mut body = document(&at(path), "Codex");
                    edit(&mut body);
                    json_reply(body, Some("max-age=60"))
                })
            }
        };
        let fresh = {
            let at = at.clone();
            get(move || async move { json_reply(document(&at("/fresh.json"), "Fresh"), None) })
        };
        let uncached = {
            let at = at.clone();
            get(move || async move {
                json_reply(
                    document(&at("/uncached.json"), "Uncached"),
                    Some("no-store"),
                )
            })
        };
        let app = axum::Router::new()
            .route("/codex.json", doc("/codex.json", |_| {}))
            .route("/fresh.json", fresh)
            .route("/uncached.json", uncached)
            .route(
                "/wrong-id.json",
                doc("/wrong-id.json", |d| {
                    d["client_id"] = json!("https://client.example/other.json");
                }),
            )
            .route(
                "/secret.json",
                doc("/secret.json", |d| {
                    d["token_endpoint_auth_method"] = json!("client_secret_basic");
                    d["client_secret"] = json!("s3cr3t");
                }),
            )
            .route(
                "/no-method.json",
                doc("/no-method.json", |d| {
                    d.as_object_mut()
                        .unwrap()
                        .remove("token_endpoint_auth_method");
                }),
            )
            .route(
                "/no-name.json",
                doc("/no-name.json", |d| {
                    d.as_object_mut().unwrap().remove("client_name");
                }),
            )
            .route(
                "/no-redirects.json",
                doc("/no-redirects.json", |d| d["redirect_uris"] = json!([])),
            )
            .route(
                "/web-redirect.json",
                doc("/web-redirect.json", |d| {
                    d["redirect_uris"] = json!(["http://evil.example/callback"]);
                }),
            )
            .route(
                "/implicit.json",
                doc("/implicit.json", |d| {
                    d["grant_types"] = json!(["implicit"]);
                }),
            )
            .route(
                "/oversized.json",
                doc("/oversized.json", |d| {
                    d["padding"] = json!("x".repeat(6 * 1024));
                }),
            )
            .route(
                "/html.json",
                get(|| async { ([(header::CONTENT_TYPE, "text/html")], "<html></html>") }),
            )
            .route(
                "/moved.json",
                get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/codex.json")]) }),
            )
            .route(
                "/slow.json",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    "never"
                }),
            )
            .route(
                "/not-json.json",
                get(|| async { json_reply(json!([1, 2]), None) }),
            )
            .route(
                "/streamed.json",
                get(|| async {
                    let chunks = (0..8).map(|_| {
                        Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(" ".repeat(1024)))
                    });
                    (
                        [(header::CONTENT_TYPE, "application/json")],
                        axum::body::Body::from_stream(futures_util::stream::iter(chunks)),
                    )
                }),
            )
            .layer(axum::middleware::from_fn({
                let hits = hits.clone();
                move |req: axum::extract::Request, next: axum::middleware::Next| {
                    hits.fetch_add(1, Ordering::SeqCst);
                    next.run(req)
                }
            }));
        let server = DocServer::start(app).await;
        port.set(server.addr.port()).unwrap();
        Fixture { server, hits }
    }

    #[tokio::test]
    async fn an_allowlisted_document_resolves_to_its_public_client() {
        let f = fixture().await;
        let cimd = f.cimd(&[DOC_HOST]);
        let url = f.url("/codex.json");
        let client = cimd.client(&url).await.unwrap();
        assert_eq!(client.id, ClientId::Metadata(url.clone()));
        assert_eq!(client.id.host(), Some(DOC_HOST));
        assert_eq!(client.name, "Codex");
        assert!(client.redirect("http://127.0.0.1:43210/callback").is_ok());
        assert!(client.redirect("http://localhost:43210/callback").is_ok());
        assert!(client.redirect("http://[::1]:43210/callback").is_err());
        assert!(
            client
                .redirect("http://127.0.0.1:43210/callback/abc")
                .is_err()
        );
        assert!(client.resources.is_none());
        assert_eq!(f.hits(), 1);
    }

    #[tokio::test]
    async fn a_document_is_cached_as_long_as_its_headers_allow() {
        let f = fixture().await;
        let cimd = f.cimd(&[DOC_HOST]);
        for _ in 0..3 {
            cimd.client(&f.url("/codex.json")).await.unwrap();
        }
        assert_eq!(
            f.hits(),
            1,
            "max-age=60 serves the second and third from cache"
        );
        for _ in 0..2 {
            cimd.client(&f.url("/fresh.json")).await.unwrap();
        }
        assert_eq!(f.hits(), 2, "no Cache-Control keeps the default lifetime");
        for _ in 0..3 {
            cimd.client(&f.url("/uncached.json")).await.unwrap();
        }
        assert_eq!(f.hits(), 5, "no-store is fetched every time");
    }

    #[tokio::test]
    async fn a_host_not_on_the_allowlist_is_refused_without_a_fetch() {
        let f = fixture().await;
        let cimd = f.cimd(&[DOC_HOST]);
        let other = MetadataUrl::parse(&f.server.url(OTHER_HOST, "/codex.json")).unwrap();
        assert_eq!(
            cimd.client(&other).await,
            Err(CimdError::Host(OTHER_HOST.into()))
        );
        assert_eq!(cimd.admits(&other), Err(CimdError::Host(OTHER_HOST.into())));
        assert!(cimd.admits(&f.url("/codex.json")).is_ok());
        assert_eq!(f.hits(), 0);
    }

    #[tokio::test]
    async fn documents_that_are_not_a_public_client_of_their_own_url_are_refused() {
        let f = fixture().await;
        let cimd = f.cimd(&[DOC_HOST]);
        for (path, want) in [
            ("/wrong-id.json", "ClientId"),
            ("/secret.json", "Secret"),
            ("/no-method.json", "AuthMethod"),
            ("/no-name.json", "Name"),
            ("/no-redirects.json", "NoRedirects"),
            ("/web-redirect.json", "Redirect"),
            ("/implicit.json", "GrantType"),
            ("/oversized.json", "TooLarge"),
            ("/streamed.json", "TooLarge"),
            ("/html.json", "ContentType"),
            ("/moved.json", "Status"),
            ("/missing.json", "Status"),
            ("/not-json.json", "Json"),
        ] {
            let got = cimd.client(&f.url(path)).await;
            assert!(
                format!("{got:?}").starts_with(&format!("Err({want}")),
                "{path}: {got:?}"
            );
        }
        let before = f.hits();
        let _ = cimd.client(&f.url("/wrong-id.json")).await;
        assert_eq!(f.hits(), before + 1, "a refused document is never cached");
    }

    #[tokio::test]
    async fn a_document_that_does_not_arrive_in_time_is_refused() {
        let f = fixture().await;
        let cimd = f.cimd(&[DOC_HOST]);
        let started = std::time::Instant::now();
        let got = cimd.client(&f.url("/slow.json")).await;
        assert!(
            matches!(got, Err(CimdError::Timeout { millis: 500, .. })),
            "{got:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// The SSRF guard: a host name that resolves to a loopback address is never fetched, even when
    /// it is allowlisted and something answers there.
    #[tokio::test]
    async fn an_allowlisted_name_that_resolves_to_a_special_use_address_is_never_fetched() {
        let f = fixture().await;
        let cimd = Cimd::new(
            policy(&["localhost"]),
            reqwest::Client::builder().no_proxy(),
        )
        .unwrap();
        let url = MetadataUrl::parse(&format!(
            "https://localhost:{}/codex.json",
            f.server.addr.port()
        ))
        .unwrap();
        let got = cimd.client(&url).await;
        assert!(
            matches!(&got, Err(CimdError::Fetch { reason, .. }) if reason.contains("no public address")),
            "{got:?}"
        );
        assert_eq!(f.hits(), 0);
    }

    #[test]
    fn special_use_addresses_are_not_public() {
        for private in [
            "0.0.0.0",
            "0.1.2.3",
            "10.1.2.3",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.0.0.8",
            "192.0.2.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
            "100::1",
            "2001::1",
            "2001:db8::1",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "ff02::1",
        ] {
            assert!(!is_public(private.parse().unwrap()), "{private}");
        }
        for public in [
            "1.1.1.1",
            "104.18.32.47",
            "2606:4700::6810:84e5",
            "::ffff:8.8.8.8",
        ] {
            assert!(is_public(public.parse().unwrap()), "{public}");
        }
    }

    #[test]
    fn metadata_urls_are_canonical_https_with_a_host_name_and_a_path() {
        for good in [
            "https://chatgpt.com/oauth/codex/client.json",
            "https://pi.dev/client.json",
            "https://client.example:8443/c",
        ] {
            assert_eq!(
                MetadataUrl::parse(good).map(|u| u.as_str().to_string()),
                Ok(good.into())
            );
        }
        for bad in [
            "http://pi.dev/client.json",
            "https://pi.dev",
            "https://pi.dev/",
            "https://pi.dev/client.json?v=1",
            "https://pi.dev/client.json#x",
            "https://user@pi.dev/client.json",
            "https://pi.dev/a/../client.json",
            "https://pi.dev/./client.json",
            "https://pi.dev:443/client.json",
            "https://PI.dev/client.json",
            "https://127.0.0.1/client.json",
            "https://[::1]/client.json",
            "https:pi.dev/client.json",
        ] {
            assert!(MetadataUrl::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_host_allowlist_takes_bare_host_names() {
        assert_eq!(CimdPolicy::from_lookup(None), Ok(None));
        assert_eq!(CimdPolicy::from_lookup(Some("  ".into())), Ok(None));
        let p = CimdPolicy::from_lookup(Some("chatgpt.com, pi.dev,chatgpt.com".into()))
            .unwrap()
            .unwrap();
        assert_eq!(p.hosts, ["chatgpt.com", "pi.dev"]);
        assert_eq!(p.limits, CimdLimits::default());
        assert_eq!(
            CimdPolicy::from_lookup(Some(",".into())),
            Err(CimdConfigError::Empty)
        );
        for bad in [
            "https://pi.dev",
            "pi.dev:443",
            "pi.dev/x",
            "127.0.0.1",
            "Pi.dev",
            "user@pi.dev",
        ] {
            assert_eq!(
                CimdPolicy::from_lookup(Some(bad.into())),
                Err(CimdConfigError::Host(bad.into())),
                "{bad}"
            );
        }
    }

    #[test]
    fn cache_lifetime_follows_cache_control_within_the_limits() {
        let limits = CimdLimits::default();
        let ttl = |pairs: &[(&'static str, &'static str)]| {
            let mut h = HeaderMap::new();
            for (k, v) in pairs {
                h.append(*k, HeaderValue::from_static(v));
            }
            freshness(&h, &limits)
        };
        assert_eq!(ttl(&[]), Some(limits.default_ttl));
        assert_eq!(
            ttl(&[("cache-control", "public, max-age=120")]),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            ttl(&[("cache-control", "max-age=120"), ("age", "100")]),
            Some(Duration::from_secs(20))
        );
        assert_eq!(
            ttl(&[("cache-control", "max-age=120"), ("age", "500")]),
            None
        );
        assert_eq!(
            ttl(&[("cache-control", "max-age=999999")]),
            Some(limits.max_ttl)
        );
        assert_eq!(ttl(&[("cache-control", "max-age=0")]), None);
        assert_eq!(ttl(&[("cache-control", "no-store")]), None);
        assert_eq!(ttl(&[("cache-control", "max-age=60, no-cache")]), None);
        assert_eq!(ttl(&[("cache-control", "max-age=soon")]), None);
    }

    #[test]
    fn only_json_content_types_are_documents() {
        for ok in [
            "application/json",
            "application/json; charset=utf-8",
            "Application/JSON",
            "application/client-metadata+json",
        ] {
            assert!(is_json(ok), "{ok}");
        }
        for bad in ["text/html", "text/json", "", "application/jsonp"] {
            assert!(!is_json(bad), "{bad}");
        }
    }
}
