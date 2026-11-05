//! GitHub, as the run's tasks see it, so they use plain `gh` and `git`. The pod maps GitHub's
//! hostnames to loopback, where this listener answers with a leaf certificate from a CA minted for
//! this run; the decrypted request is decided by [`crate::policy`] and leaves with the run's App
//! token in place of the placeholder the task holds. No other host reaches `crucible-git`.
//!
//! ```text
//!   task ──https://api.github.com──▶ 127.0.0.1:443 (run CA) ─▶ policy ─▶ token swapped ─▶ GitHub
//!   task ──https://pypi.org────────▶ pypi.org
//! ```

use crate::allow::Allow;
use crate::github::GitHub;
use crate::policy::{self, Verdict};
use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::TryStreamExt;
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType,
};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crucible_contract::git::GITHUB_HOSTS as INTERCEPTED;

/// The largest request body buffered for a decision (a GraphQL document).
const DECIDED_BODY_LIMIT: usize = 1024 * 1024;

/// Where an image keeps the system's trusted roots, first match wins.
const SYSTEM_ROOTS: [&str; 3] = [
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/ssl/cert.pem",
];

/// Request headers never forwarded: hop-by-hop, the proxy's own, and the caller's credentials.
const DROPPED_REQUEST: [HeaderName; 8] = [
    header::HOST,
    header::CONTENT_LENGTH,
    header::AUTHORIZATION,
    header::CONNECTION,
    header::PROXY_AUTHORIZATION,
    header::TRANSFER_ENCODING,
    header::TE,
    header::UPGRADE,
];

const DROPPED_RESPONSE: [HeaderName; 3] = [
    header::CONNECTION,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
];

#[derive(Debug, thiserror::Error)]
#[error("the certificate cache lock is poisoned")]
struct CachePoisoned;

/// The run's CA and the server configs it has issued, one per intercepted host.
pub struct Mitm {
    ca: CertifiedIssuer<'static, KeyPair>,
    provider: Arc<rustls::crypto::CryptoProvider>,
    configs: Mutex<HashMap<String, Arc<rustls::ServerConfig>>>,
    gh: Arc<GitHub>,
    allow: Arc<Allow>,
    policy: policy::State,
}

impl Mitm {
    pub fn new(gh: Arc<GitHub>, allow: Arc<Allow>) -> Result<Self> {
        let mut params = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "crucible-git run CA");
        params.distinguished_name = dn;
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca = CertifiedIssuer::self_signed(params, KeyPair::generate()?)
            .context("minting the run CA")?;
        Ok(Self {
            ca,
            provider: Arc::new(rustls::crypto::ring::default_provider()),
            configs: Mutex::new(HashMap::new()),
            gh,
            allow,
            policy: policy::State::default(),
        })
    }

    /// Write the CA certificate, and a bundle of the system's roots plus it, into `dir`.
    pub fn write(&self, dir: &Path) -> Result<()> {
        let ca = self.ca.pem();
        std::fs::write(dir.join(crucible_contract::git::CA_FILE_NAME), &ca)
            .with_context(|| format!("writing the run CA into {}", dir.display()))?;
        let mut bundle = SYSTEM_ROOTS
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        if !bundle.ends_with('\n') && !bundle.is_empty() {
            bundle.push('\n');
        }
        bundle.push_str(&ca);
        std::fs::write(dir.join(crucible_contract::git::BUNDLE_FILE_NAME), bundle)
            .with_context(|| format!("writing the CA bundle into {}", dir.display()))
    }

    fn server_config(&self, host: &str) -> Result<Arc<rustls::ServerConfig>> {
        let mut configs = self.configs.lock().map_err(|_| CachePoisoned)?;
        if let Some(config) = configs.get(host) {
            return Ok(config.clone());
        }
        let mut params = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, host);
        params.distinguished_name = dn;
        params.subject_alt_names = vec![SanType::DnsName(host.try_into()?)];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let key = KeyPair::generate()?;
        let cert = params.signed_by(&key, &self.ca)?;
        let chain = vec![
            rustls::pki_types::CertificateDer::from(cert.der().to_vec()),
            rustls::pki_types::CertificateDer::from(self.ca.der().to_vec()),
        ];
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into());
        let mut config = rustls::ServerConfig::builder_with_provider(self.provider.clone())
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(chain, key)?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let config = Arc::new(config);
        configs.insert(host.to_string(), config.clone());
        Ok(config)
    }
}

/// Serve GitHub over TLS on `listener`, which the pod's `hostAliases` point GitHub's hostnames
/// at: the ClientHello's SNI picks the host, and so the certificate and the policy.
pub async fn serve(mitm: Arc<Mitm>, listener: tokio::net::TcpListener) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let mitm = mitm.clone();
        tokio::spawn(async move {
            if let Err(e) = accept(mitm, stream).await {
                tracing::debug!(error = %e, "TLS connection ended");
            }
        });
    }
}

async fn accept(mitm: Arc<Mitm>, stream: tokio::net::TcpStream) -> Result<()> {
    let start =
        tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), stream).await?;
    let host = start
        .client_hello()
        .server_name()
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if !INTERCEPTED.contains(&host.as_str()) {
        tracing::info!(%host, "refused TLS for a host crucible-git does not serve");
        return Ok(());
    }
    let tls = start.into_stream(mitm.server_config(&host)?).await?;
    let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let mitm = mitm.clone();
        let host = host.clone();
        async move { Ok::<_, std::convert::Infallible>(forward(&mitm, &host, req.map(Body::new)).await) }
    });
    hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(tls), service)
        .await?;
    Ok(())
}

fn refuse(status: StatusCode, why: impl Into<String>) -> Response {
    let why = why.into();
    tracing::info!(%status, %why, "refused");
    (status, why).into_response()
}

async fn forward(mitm: &Mitm, host: &str, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let graphql = policy::needs_body(host, &parts.method, &path);
    let (decided_body, body) = match graphql {
        false => (None, Some(body)),
        true => match http_body_util::Limited::new(body, DECIDED_BODY_LIMIT)
            .collect()
            .await
        {
            Ok(collected) => (Some(collected.to_bytes()), None),
            Err(_) => return refuse(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
        },
    };
    let verdict = policy::decide(
        &mitm.policy,
        &mitm.gh,
        &mitm.allow,
        host,
        &parts.method,
        &path,
        decided_body.as_deref(),
    )
    .await;
    if let Verdict::Deny(status, why) = verdict {
        return refuse(status, why);
    }
    let token = match mitm.gh.token().await {
        Ok(t) => t,
        Err(e) => return refuse(e.status(), e.to_string()),
    };
    let auth = match host {
        "github.com" => GitHub::basic(&token),
        _ => format!("token {token}"),
    };
    let mut upstream = mitm
        .gh
        .http
        .request(parts.method.clone(), format!("https://{host}{path}"))
        .headers(forwarded(&parts.headers))
        .header(header::AUTHORIZATION, auth);
    upstream = match (decided_body, body) {
        (Some(bytes), _) => upstream.body(bytes),
        (None, _) if parts.method == Method::GET || parts.method == Method::HEAD => upstream,
        (None, Some(body)) => upstream.body(reqwest::Body::wrap_stream(body.into_data_stream())),
        (None, None) => upstream,
    };
    let resp = match upstream.send().await {
        Ok(r) => r,
        Err(e) => return refuse(StatusCode::BAD_GATEWAY, format!("reaching {host}: {e}")),
    };
    tracing::info!(%host, method = %parts.method, path = %path.split('?').next().unwrap_or(""), status = %resp.status(), "forwarded");
    let mut out = Response::builder().status(resp.status());
    for (name, value) in resp.headers() {
        if !DROPPED_RESPONSE.contains(name) {
            out = out.header(name, value);
        }
    }
    if graphql {
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => return refuse(StatusCode::BAD_GATEWAY, format!("reading {host}: {e}")),
        };
        policy::record_created(&mitm.policy, &bytes);
        return out
            .body(Body::from(bytes))
            .unwrap_or_else(|e| refuse(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
    }
    let stream = resp.bytes_stream().map_err(std::io::Error::other);
    out.body(Body::from_stream(stream))
        .unwrap_or_else(|e| refuse(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn forwarded(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        if !DROPPED_REQUEST.contains(name) && !name.as_str().starts_with("proxy-") {
            out.append(name.clone(), value.clone());
        }
    }
    out
}
