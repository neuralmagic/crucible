//! The controller's TLS listener: a second HTTPS surface serving the same router as the plaintext
//! one.
//!
//! It exists for the redemption Route. That Route re-encrypts to the pod, which means the router
//! opens a TLS connection to a controller port, and without a listener that terminates TLS the
//! backend answers nothing at all. The certificate is the cluster's service-serving certificate,
//! mounted from the Secret the service CA writes; the service CA rotates it in place well before
//! expiry, so the resolver re-reads both files on a timer and swaps the loaded pair rather than
//! serving a certificate that expired under a process that never restarts.

#![allow(clippy::disallowed_macros)]

use anyhow::{Context, Result, anyhow, bail};
use rustls::ServerConfig;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

/// How often the serving certificate is re-read from disk.
const RELOAD_INTERVAL: Duration = Duration::from_secs(300);

/// How long a client has to finish its handshake before its slot is given up.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The pause after an accept that failed, so a listener that cannot accept does not spin a core.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// How many finished handshakes may queue ahead of the server loop.
const BACKLOG: usize = 128;

/// What the TLS surface needs: where to listen and which mounted PEM pair to serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsFront {
    pub addr: SocketAddr,
    cert: PathBuf,
    key: PathBuf,
}

impl TlsFront {
    /// `CONTROLLER_TLS_ADDR` plus `CONTROLLER_TLS_CERT` / `CONTROLLER_TLS_KEY`. `None` when the
    /// address is unset — the deployment serves plaintext only. An address without a PEM pair is
    /// an error rather than a silent plaintext port on the TLS number.
    pub fn from_env() -> Result<Option<Self>> {
        let Some(addr) = env("CONTROLLER_TLS_ADDR") else {
            return Ok(None);
        };
        let addr: SocketAddr = addr
            .parse()
            .with_context(|| format!("CONTROLLER_TLS_ADDR `{addr}` is not a `host:port`"))?;
        let cert = env("CONTROLLER_TLS_CERT").ok_or_else(|| {
            anyhow!(
                "CONTROLLER_TLS_ADDR is set without CONTROLLER_TLS_CERT (the serving certificate)"
            )
        })?;
        let key = env("CONTROLLER_TLS_KEY").ok_or_else(|| {
            anyhow!("CONTROLLER_TLS_ADDR is set without CONTROLLER_TLS_KEY (the serving key)")
        })?;
        Ok(Some(TlsFront {
            addr,
            cert: PathBuf::from(cert),
            key: PathBuf::from(key),
        }))
    }

    /// The same front, moved onto `host`. The controller binds every surface to the same address
    /// so the loopback-only rule for an unauthenticated deployment covers all of them.
    pub fn with_host(mut self, host: std::net::IpAddr) -> Self {
        self.addr.set_ip(host);
        self
    }

    /// The same front on a caller-chosen address, for a test that needs an ephemeral port.
    #[cfg(test)]
    pub(crate) fn new(addr: SocketAddr, cert: PathBuf, key: PathBuf) -> Self {
        TlsFront { addr, cert, key }
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Bind the TLS surface and start accepting. The returned listener hands finished TLS streams to
/// `axum::serve`; handshakes and certificate reloads run in their own tasks.
pub async fn bind(front: &TlsFront) -> Result<TlsListener> {
    crate::install_crypto_provider();
    let serving = Arc::new(ServingCert::load(&front.cert, &front.key)?);
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(serving.clone());
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let tcp = TcpListener::bind(front.addr)
        .await
        .with_context(|| format!("binding the controller tls surface to {}", front.addr))?;
    let local = tcp
        .local_addr()
        .context("reading the controller tls surface's bound address")?;
    let (tx, incoming) = mpsc::channel(BACKLOG);
    tokio::spawn(accept_loop(tcp, acceptor, tx));
    tokio::spawn(reload_loop(serving.clone()));
    Ok(TlsListener {
        local,
        #[cfg(test)]
        serving,
        incoming,
    })
}

/// An [`axum::serve::Listener`] whose connections arrive already handshaken.
pub struct TlsListener {
    local: SocketAddr,
    #[cfg(test)]
    serving: Arc<ServingCert>,
    incoming: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
}

impl TlsListener {
    /// The address actually bound, which is what a test asks for after requesting port 0.
    pub fn addr(&self) -> SocketAddr {
        self.local
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.incoming.recv().await {
            Some(conn) => conn,
            None => {
                tracing::error!(
                    "the tls accept loop stopped; this surface will serve no further connections"
                );
                std::future::pending().await
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local)
    }
}

/// Accept TCP forever, handing each connection's handshake to its own task so one client that
/// opens a socket and says nothing cannot stall every other client.
async fn accept_loop(
    tcp: TcpListener,
    acceptor: TlsAcceptor,
    tx: mpsc::Sender<(TlsStream<TcpStream>, SocketAddr)>,
) {
    loop {
        let (stream, peer) = match tcp.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(error = %e, "tls surface: accepting a connection failed");
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                Ok(Ok(tls)) => {
                    if tx.send((tls, peer)).await.is_err() {
                        tracing::debug!(%peer, "tls surface: the server loop is gone");
                    }
                }
                Ok(Err(e)) => tracing::debug!(%peer, error = %e, "tls handshake failed"),
                Err(_) => tracing::debug!(%peer, "tls handshake timed out"),
            }
        });
    }
}

/// Re-read the mounted PEM pair on a timer. A read that fails keeps the pair already in memory,
/// because a half-written rotation must not take the surface down.
async fn reload_loop(serving: Arc<ServingCert>) {
    let mut ticks = tokio::time::interval(RELOAD_INTERVAL);
    ticks.tick().await;
    loop {
        ticks.tick().await;
        let serving = serving.clone();
        let done = tokio::task::spawn_blocking(move || serving.reload()).await;
        match done {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(
                error = format!("{e:#}"),
                "reloading the tls serving certificate failed; keeping the loaded one"
            ),
            Err(e) => tracing::warn!(error = %e, "the tls certificate reload task failed"),
        }
    }
}

/// The mounted certificate and key, and whichever pair is currently being served.
#[derive(Debug)]
struct ServingCert {
    cert: PathBuf,
    key: PathBuf,
    current: Mutex<Arc<CertifiedKey>>,
}

impl ServingCert {
    fn load(cert: &Path, key: &Path) -> Result<Self> {
        let current = Mutex::new(Self::read(cert, key)?);
        Ok(ServingCert {
            cert: cert.to_path_buf(),
            key: key.to_path_buf(),
            current,
        })
    }

    fn read(cert: &Path, key: &Path) -> Result<Arc<CertifiedKey>> {
        let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
            .with_context(|| format!("reading the tls certificate {}", cert.display()))?
            .collect::<Result<_, _>>()
            .with_context(|| format!("parsing the tls certificate {}", cert.display()))?;
        if chain.is_empty() {
            bail!(
                "the tls certificate {} holds no certificates",
                cert.display()
            );
        }
        let private = PrivateKeyDer::from_pem_file(key)
            .with_context(|| format!("reading the tls private key {}", key.display()))?;
        let provider =
            CryptoProvider::get_default().context("no rustls crypto provider is installed")?;
        let pair = CertifiedKey::from_der(chain, private, provider)
            .context("the tls certificate and key are not a usable pair")?;
        Ok(Arc::new(pair))
    }

    fn reload(&self) -> Result<()> {
        let next = Self::read(&self.cert, &self.key)?;
        let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        *current = next;
        Ok(())
    }
}

impl ResolvesServerCert for ServingCert {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        Some(Arc::clone(&current))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;
    use std::net::{IpAddr, Ipv4Addr};
    use std::process::Command;

    /// A self-signed serving certificate for 127.0.0.1, written to `dir` as `tls.crt`/`tls.key`.
    /// `None` where the local `openssl` cannot issue one — the same skip shape the Vault-backed
    /// tests use.
    fn issue(dir: &Path, cn: &str) -> Option<(PathBuf, PathBuf)> {
        let cert = dir.join("tls.crt");
        let key = dir.join("tls.key");
        let out = Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                &format!("/CN={cn}"),
                "-addext",
                "subjectAltName=IP:127.0.0.1",
                "-addext",
                "extendedKeyUsage=serverAuth",
                "-addext",
                "basicConstraints=critical,CA:false",
                "-keyout",
            ])
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .output()
            .ok()?;
        out.status.success().then_some((cert, key))
    }

    /// A client that trusts exactly the certificate at `cert` and pools nothing, so every request
    /// is a fresh handshake against whatever the surface is serving now.
    fn client(pem: &[u8]) -> reqwest::Client {
        reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(pem).expect("a root"))
            .pool_max_idle_per_host(0)
            .build()
            .expect("a client")
    }

    fn pem(path: &Path) -> Vec<u8> {
        std::fs::read(path).expect("an issued certificate")
    }

    /// The whole point of the module: a real client completes a real handshake against the mounted
    /// pair and gets the router's answer over TLS; and when the service CA rotates the mounted
    /// files in place, the reload serves the new pair without restarting the listener.
    #[tokio::test]
    async fn the_surface_serves_tls_and_picks_up_a_rotated_certificate() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let Some((cert, key)) = issue(dir.path(), "first.crucible.test") else {
            return;
        };
        let retired = pem(&cert);

        let front = TlsFront::new(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            cert.clone(),
            key.clone(),
        );
        let listener = bind(&front).await.expect("bind the tls surface");
        let addr = listener.addr();
        let reload = listener.serving.clone();
        let app = Router::new().route("/tls-probe", get(|| async { "bundle" }));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let url = format!("https://127.0.0.1:{}/tls-probe", addr.port());
        let body = client(&retired)
            .get(&url)
            .send()
            .await
            .expect("a tls response")
            .text()
            .await
            .expect("a body");
        assert_eq!(body, "bundle");

        let rotated = tempfile::tempdir().expect("a temp dir");
        let (next_cert, next_key) = issue(rotated.path(), "second.crucible.test").expect("reissue");
        std::fs::copy(&next_cert, &cert).expect("rotate the certificate");
        std::fs::copy(&next_key, &key).expect("rotate the key");
        let rotated_root = pem(&cert);

        // Before the reload the surface still presents the pair it loaded at boot.
        assert!(
            client(&rotated_root).get(&url).send().await.is_err(),
            "the new root does not yet validate what the surface is serving"
        );
        reload.reload().expect("reload the rotated pair");

        let body = client(&rotated_root)
            .get(&url)
            .send()
            .await
            .expect("the rotated pair is served")
            .text()
            .await
            .expect("a body");
        assert_eq!(body, "bundle");
        assert!(
            client(&retired).get(&url).send().await.is_err(),
            "and the retired certificate is no longer presented"
        );
    }

    /// A TLS address whose PEM pair is not there is a boot failure, not a plaintext port on the
    /// TLS number.
    #[test]
    fn a_tls_address_without_a_certificate_is_refused() {
        let err = ServingCert::load(
            &PathBuf::from("/nonexistent/tls.crt"),
            &PathBuf::from("/nonexistent/tls.key"),
        )
        .expect_err("a missing certificate cannot load");
        assert!(
            format!("{err:#}").contains("tls certificate"),
            "the error names what it could not read: {err:#}"
        );
    }
}
