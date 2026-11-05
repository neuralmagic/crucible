//! GitHub, as the run's tasks see it, so they use plain `gh` and `git`. The pod maps GitHub's
//! hostnames to loopback, where this listener answers with a leaf certificate from a CA minted for
//! this run; the decrypted request is decided by [`crate::policy`] and leaves with the run's App
//! token in place of the placeholder the task holds. No other host reaches `crucible-git`.
//!
//! ```text
//!   task ──https://api.github.com──▶ 127.0.0.1:443 (run CA) ─▶ policy ─▶ token swapped ─▶ GitHub
//!   task ──https://pypi.org────────▶ pypi.org
//! ```

use crate::relay::{Caller, Relay};
use anyhow::{Context, Result};
use axum::body::Body;
use hyper_util::rt::TokioIo;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType,
};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crucible_contract::git::GITHUB_HOSTS as INTERCEPTED;

/// Where an image keeps the system's trusted roots, first match wins.
const SYSTEM_ROOTS: [&str; 3] = [
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/ssl/cert.pem",
];

#[derive(Debug, thiserror::Error)]
#[error("the certificate cache lock is poisoned")]
struct CachePoisoned;

/// The run's CA and the server configs it has issued, one per intercepted host.
pub struct Mitm {
    ca: CertifiedIssuer<'static, KeyPair>,
    provider: Arc<rustls::crypto::CryptoProvider>,
    configs: Mutex<HashMap<String, Arc<rustls::ServerConfig>>>,
    relay: Arc<Relay>,
}

impl Mitm {
    pub fn new(relay: Arc<Relay>) -> Result<Self> {
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
            relay,
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
        async move {
            Ok::<_, std::convert::Infallible>(
                mitm.relay
                    .relay(&host, req.map(Body::new), Caller::Task)
                    .await,
            )
        }
    });
    hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(tls), service)
        .await?;
    Ok(())
}
