//! A run's OpenShell gateway in its own pod in the sandbox namespace, for a profile that isolates
//! gateways ([`render::isolated`]). The controller owns its lifecycle:
//!
//! ```text
//!  dispatch_run                                          collect_run_pod / sweep
//!  ───────────────────────────────────────────────────   ─────────────────────────────
//!  reserve   gateway Service → ClusterIP → mint TLS      teardown, in order:
//!            (loop client material joins the run          Sandboxes and supervisors the
//!             Secret before the loop pod renders)         gateway created (gateway-id),
//!  start     loop Service, NetworkPolicy, gateway pod,    the loop Service, then the
//!            TLS Secrets owned by the gateway pod         gateway's own objects last
//!  then      the loop pod
//! ```
//!
//! Nothing in the sandbox namespace can be owner-referenced to the loop pod across namespaces, so
//! collection deletes the run's gateway by label and a sweep on the discovery tick removes any
//! whose run pod is no longer running. The gateway's own objects go last, so a teardown that fails
//! halfway leaves the label the sweep finds it by.

use crate::runs::workpod::*;
use anyhow::{Context, Result};
use crucible::deploy::DeployProfile;
use crucible::deploy::render::gateway::{
    self as render, GatewayAddresses, GatewayNames, RUN_LABEL, TlsMaterial,
};
use k8s_openapi::api::core::v1::{Pod, Service};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType,
};
use std::net::IpAddr;

/// How long a run's gateway certificates stay valid past its deadline, from the day before they
/// are minted.
const VALIDITY_SLACK_DAYS: i64 = 1;
/// The validity of a run that states no deadline.
const UNBOUNDED_VALIDITY_DAYS: i64 = 30;

/// The run Secret keys the loop pod's gateway client material is delivered under.
const CA_KEY: &str = "openshell-gateway-ca.crt";
const CERT_KEY: &str = "openshell-gateway-tls.crt";
const KEY_KEY: &str = "openshell-gateway-tls.key";

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error(
        "cluster {cluster} reports to the hub by push, so the hub cannot reach its API to start \
         the run's gateway; a profile that isolates gateways can only dispatch to clusters the \
         hub reaches directly"
    )]
    PushOnlyCluster { cluster: String },
}

#[derive(Debug, thiserror::Error)]
pub enum GatewayTlsError {
    #[error("minting the run's gateway certificates: {0}")]
    Mint(#[from] rcgen::Error),
    #[error("computing the gateway certificates' validity: {0}")]
    Validity(#[from] jiff::Error),
}

/// A certificate and its private key, PEM-encoded.
pub(crate) struct Leaf {
    pub cert: String,
    pub key: String,
}

/// One run's gateway mTLS: a CA nobody else trusts, the gateway's server certificate for its
/// Service IP, and client certificates for the loop pod and the run's supervisors. The CA key is
/// dropped once these are issued.
pub(crate) struct GatewayTls {
    pub ca: String,
    pub server: Leaf,
    pub loop_client: Leaf,
    pub supervisor_client: Leaf,
}

/// The days a run's certificates must outlive minting: its deadline rounded up plus slack, or a
/// fixed span for a run with none.
pub(crate) fn validity_days(max_time_secs: Option<u64>) -> i64 {
    match max_time_secs {
        Some(secs) => i64::try_from(secs.div_ceil(86_400))
            .unwrap_or(UNBOUNDED_VALIDITY_DAYS)
            .saturating_add(VALIDITY_SLACK_DAYS),
        None => UNBOUNDED_VALIDITY_DAYS,
    }
}

impl GatewayTls {
    pub(crate) fn mint(
        gateway: IpAddr,
        today: jiff::civil::Date,
        valid_days: i64,
    ) -> Result<Self, GatewayTlsError> {
        let not_before = today.checked_sub(jiff::Span::new().days(1))?;
        let not_after = today.checked_add(jiff::Span::new().days(valid_days))?;
        let midnight = |d: jiff::civil::Date| {
            rcgen::date_time_ymd(
                i32::from(d.year()),
                d.month().unsigned_abs(),
                d.day().unsigned_abs(),
            )
        };
        let stamp = |params: &mut CertificateParams, cn: &str| {
            let mut dn = DistinguishedName::new();
            dn.push(DnType::CommonName, cn);
            params.distinguished_name = dn;
            params.not_before = midnight(not_before);
            params.not_after = midnight(not_after);
        };

        let mut ca = CertificateParams::default();
        stamp(&mut ca, "crucible run gateway CA");
        ca.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate()?)?;

        let leaf = |cn: &str, sans: Vec<SanType>, usage: ExtendedKeyUsagePurpose| {
            let mut params = CertificateParams::default();
            stamp(&mut params, cn);
            params.subject_alt_names = sans;
            params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            params.extended_key_usages = vec![usage];
            let key = KeyPair::generate()?;
            let cert = params.signed_by(&key, &ca)?;
            Ok::<_, rcgen::Error>(Leaf {
                cert: cert.pem(),
                key: key.serialize_pem(),
            })
        };
        let server = leaf(
            "openshell-gateway",
            vec![SanType::IpAddress(gateway)],
            ExtendedKeyUsagePurpose::ServerAuth,
        )?;
        let loop_client = leaf(
            "crucible-loop",
            Vec::new(),
            ExtendedKeyUsagePurpose::ClientAuth,
        )?;
        let supervisor_client = leaf(
            "openshell-supervisor",
            Vec::new(),
            ExtendedKeyUsagePurpose::ClientAuth,
        )?;
        Ok(Self {
            ca: ca.pem(),
            server,
            loop_client,
            supervisor_client,
        })
    }
}

fn today() -> jiff::civil::Date {
    jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date()
}

fn cluster_ip(service: &Service) -> Result<IpAddr> {
    let name = service.metadata.name.as_deref().unwrap_or_default();
    let raw = service
        .spec
        .as_ref()
        .and_then(|s| s.cluster_ip.as_deref())
        .filter(|ip| !ip.is_empty() && *ip != "None")
        .with_context(|| format!("Service {name} came back without a ClusterIP"))?;
    raw.parse()
        .with_context(|| format!("Service {name} has an unparseable ClusterIP {raw:?}"))
}

/// A run's gateway between its Service's creation and its pod's.
pub struct RunGateway<'a> {
    profile: &'a DeployProfile,
    names: GatewayNames,
    gateway_ip: IpAddr,
    tls: GatewayTls,
}

impl<'a> RunGateway<'a> {
    /// Create the gateway Service for `run_pod` and mint the run's TLS for its address, valid for
    /// the run's `max_time_secs` plus slack.
    pub async fn reserve(
        dispatcher: &dyn PodDispatcher,
        cluster: &str,
        profile: &'a DeployProfile,
        run_pod: &str,
        max_time_secs: Option<u64>,
    ) -> Result<Self> {
        let names = GatewayNames::for_run_pod(run_pod);
        let service = dispatcher
            .create_service(
                cluster,
                profile.cluster.sandbox_namespace(),
                render::gateway_service(profile, &names),
            )
            .await
            .context("creating the run's gateway Service")?;
        let gateway_ip = cluster_ip(&service)?;
        let tls = GatewayTls::mint(gateway_ip, today(), validity_days(max_time_secs))?;
        Ok(Self {
            profile,
            names,
            gateway_ip,
            tls,
        })
    }

    /// The URL the loop pod dials.
    pub fn url(&self) -> String {
        render::gateway_url(self.gateway_ip)
    }

    /// The loop pod's client material: the run CA, its certificate, and its key.
    pub fn loop_client_tls(&self) -> TlsMaterial<'_> {
        TlsMaterial {
            ca: &self.tls.ca,
            cert: &self.tls.loop_client.cert,
            key: &self.tls.loop_client.key,
        }
    }

    /// The loop pod's share: its client material as files under [`render::CLIENT_TLS_DIR`] and the
    /// env pointing the engine at the gateway.
    pub(crate) fn loop_delivery(&self) -> crate::secrets::deliver::Delivery {
        let file = |name: &str, key: &str| {
            (
                format!("{}/{name}", render::CLIENT_TLS_DIR),
                key.to_string(),
            )
        };
        crate::secrets::deliver::Delivery {
            data: [
                (CA_KEY.to_string(), self.tls.ca.clone()),
                (CERT_KEY.to_string(), self.tls.loop_client.cert.clone()),
                (KEY_KEY.to_string(), self.tls.loop_client.key.clone()),
            ]
            .into(),
            files: vec![
                file("ca.crt", CA_KEY),
                file("tls.crt", CERT_KEY),
                file("tls.key", KEY_KEY),
            ],
            plain_env: render::loop_pod_env(self.gateway_ip)
                .into_iter()
                .filter_map(|e| Some((e.name, e.value?)))
                .collect(),
            ..Default::default()
        }
    }

    /// Start the gateway for the rendered `loop_pod`, ahead of creating it so the gateway boots
    /// while the loop pod schedules: the loop Service sandboxes reach the loop pod through on
    /// `sandbox_ports`, the NetworkPolicy, the gateway pod, then the TLS Secrets owned by it.
    pub async fn start(
        &self,
        dispatcher: &dyn PodDispatcher,
        cluster: &str,
        loop_namespace: &str,
        loop_pod: &Pod,
        sandbox_ports: &[u16],
    ) -> Result<()> {
        let run = loop_pod
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get(RUN_LABEL))
            .with_context(|| format!("the loop pod carries no {RUN_LABEL} label"))?;
        let mut loop_service = render::loop_service(self.profile, &self.names, run, sandbox_ports);
        loop_service.metadata.namespace = Some(loop_namespace.to_string());
        let loop_service = dispatcher
            .create_service(cluster, loop_namespace, loop_service)
            .await
            .context("creating the run's loop Service")?;
        let addresses = GatewayAddresses {
            gateway: self.gateway_ip,
            loop_service: cluster_ip(&loop_service)?,
        };

        let ns = self.profile.cluster.sandbox_namespace().to_string();
        dispatcher
            .create_network_policy(
                cluster,
                &ns,
                render::gateway_network_policy(self.profile, &self.names, run),
            )
            .await
            .context("creating the gateway's NetworkPolicy")?;
        let gateway_pod = dispatcher
            .create(
                cluster,
                &ns,
                render::gateway_pod(self.profile, &self.names, loop_pod, addresses),
            )
            .await
            .context("creating the gateway pod")?;
        let owner = owner_reference_to_pod(&gateway_pod)
            .context("the gateway pod create response carried no UID to own its Secrets")?;
        let secret = |name: &str, leaf: &Leaf| {
            let mut secret = render::gateway_tls_secret(
                self.profile,
                &self.names,
                name,
                TlsMaterial {
                    ca: &self.tls.ca,
                    cert: &leaf.cert,
                    key: &leaf.key,
                },
            );
            secret.metadata.owner_references = Some(vec![owner.clone()]);
            secret
        };
        dispatcher
            .create_secret(
                cluster,
                &ns,
                secret(&self.names.server_secret, &self.tls.server),
            )
            .await
            .context("creating the gateway's server TLS Secret")?;
        dispatcher
            .create_secret(
                cluster,
                &ns,
                secret(&self.names.client_secret, &self.tls.supervisor_client),
            )
            .await
            .context("creating the supervisors' gateway client Secret")?;
        Ok(())
    }
}

/// Delete everything the gateway of the run whose pod is `run_pod` created and was created with:
/// its Sandboxes and their supervisors, the loop Service in `loop_namespace`, then its own pod,
/// Service, Secrets, and NetworkPolicy, which the sweep finds a half-finished teardown by.
pub async fn teardown(
    dispatcher: &dyn PodDispatcher,
    cluster: &str,
    profile: &DeployProfile,
    run_pod: &str,
    loop_namespace: &str,
) -> Result<()> {
    let names = GatewayNames::for_run_pod(run_pod);
    let sandbox_ns = profile.cluster.sandbox_namespace();
    let created = render::gateway_created_selector(&names);
    let own = render::run_gateway_selector(run_pod);
    let context = || format!("deleting the gateway of {run_pod}");
    dispatcher
        .delete_sandboxes(cluster, sandbox_ns, &created)
        .await
        .with_context(context)?;
    dispatcher
        .delete_selected(cluster, sandbox_ns, &created)
        .await
        .with_context(context)?;
    dispatcher
        .delete_services(cluster, loop_namespace, &own)
        .await
        .with_context(context)?;
    dispatcher
        .delete_selected(cluster, sandbox_ns, &own)
        .await
        .with_context(context)
}

/// A [`crate::daemon::queue::DiscoverySource`] that runs [`sweep_orphan_gateways`] on the daemon's
/// discovery tick, so a run that failed without being collected, or a dispatch the controller died
/// in the middle of, does not leave its gateway running.
pub(crate) struct GatewaySweep {
    db: crate::client::Db,
    profile: Option<std::path::PathBuf>,
    hub_namespace: String,
}

impl GatewaySweep {
    pub(crate) fn new(
        db: crate::client::Db,
        profile: Option<std::path::PathBuf>,
        hub_namespace: String,
    ) -> Self {
        Self {
            db,
            profile,
            hub_namespace,
        }
    }
}

impl crate::daemon::queue::DiscoverySource for GatewaySweep {
    fn poll(
        &self,
        _enqueue: std::sync::Arc<dyn crate::daemon::queue::Enqueue>,
    ) -> crate::daemon::queue::BoxFuture<Result<()>> {
        let db = self.db.clone();
        let profile = self.profile.clone();
        let hub_namespace = self.hub_namespace.clone();
        Box::pin(async move {
            sweep_orphan_gateways(
                &db,
                active_dispatcher().as_ref(),
                profile.as_deref(),
                &hub_namespace,
            )
            .await;
            Ok(())
        })
    }
}

/// Delete the gateway objects in each cluster's sandbox namespace whose run pod's `work_pods` row is
/// missing or no longer `running`. A no-op unless the profile at `profile_path` isolates gateways.
/// Best-effort: a cluster or run that fails is logged and the sweep moves on.
pub(crate) async fn sweep_orphan_gateways(
    db: &crate::client::Db,
    dispatcher: &dyn PodDispatcher,
    profile_path: Option<&std::path::Path>,
    hub_namespace: &str,
) {
    let profile = match crate::agent_auth::placement::load_profile(profile_path).await {
        Ok(Some(profile)) if render::isolated(&profile) => profile,
        Ok(_) => return,
        Err(e) => {
            tracing::warn!(
                error = format!("{e:#}"),
                "gateway sweep: reading the deploy profile failed"
            );
            return;
        }
    };
    let clusters = match crate::runs::work_pods::work_pod_clusters(db.pool()).await {
        Ok(clusters) => clusters,
        Err(e) => {
            tracing::warn!(
                error = format!("{e:#}"),
                "gateway sweep: reading clusters failed"
            );
            return;
        }
    };
    let ns = profile.cluster.sandbox_namespace().to_string();
    for cluster in clusters {
        let Some(loop_namespace) =
            crate::runs::workpod::row_namespace(dispatcher, &cluster, hub_namespace).await
        else {
            continue;
        };
        let runs = match dispatcher
            .selected_label_values(
                &cluster,
                &ns,
                &render::gateway_selector(),
                render::GATEWAY_FOR_LABEL,
            )
            .await
        {
            Ok(runs) => runs,
            Err(e) => {
                tracing::warn!(
                    cluster,
                    error = format!("{e:#}"),
                    "gateway sweep: listing gateways failed"
                );
                continue;
            }
        };
        for run_pod in runs {
            let live = match crate::runs::work_pods::get_work_pod(db.pool(), &run_pod).await {
                Ok(row) => row.is_some_and(|r| r.state == WorkPodState::Running),
                Err(e) => {
                    tracing::warn!(
                        run_pod,
                        error = format!("{e:#}"),
                        "gateway sweep: reading the run pod row failed"
                    );
                    continue;
                }
            };
            if live {
                continue;
            }
            match teardown(dispatcher, &cluster, &profile, &run_pod, &loop_namespace).await {
                Ok(()) => tracing::info!(
                    cluster,
                    run_pod,
                    "gateway sweep: removed an orphaned gateway"
                ),
                Err(e) => tracing::warn!(
                    cluster,
                    run_pod,
                    error = format!("{e:#}"),
                    "gateway sweep: teardown failed"
                ),
            }
        }
    }
}

/// An ownerReference pointing at a just-created pod (its UID populated by the API server), for
/// cascade-GC'ing the pack ConfigMap with the pod. `None` when the pod has no name/UID yet (a fake
/// dispatcher that doesn't stamp a UID) — the CM is still created, just not cascade-owned, and the
/// managed-by selector keeps it sweepable.
pub(crate) fn owner_reference_to_pod(
    pod: &Pod,
) -> Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference> {
    let name = pod.metadata.name.clone().filter(|s| !s.is_empty())?;
    let uid = pod.metadata.uid.clone().filter(|s| !s.is_empty())?;
    Some(
        k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
            api_version: "v1".to_string(),
            kind: "Pod".to_string(),
            name,
            uid,
            controller: Some(true),
            block_owner_deletion: Some(true),
        },
    )
}

#[cfg(test)]
mod tests {
    use crate::runs::workpod::gateway::*;

    #[test]
    fn minted_material_is_pem_and_each_certificate_has_its_own_key() {
        let tls = GatewayTls::mint(
            "172.30.4.2".parse().expect("ip"),
            jiff::civil::date(2026, 10, 8),
            validity_days(None),
        )
        .expect("mint");
        assert!(tls.ca.starts_with("-----BEGIN CERTIFICATE-----"));
        for leaf in [&tls.server, &tls.loop_client, &tls.supervisor_client] {
            assert!(leaf.cert.starts_with("-----BEGIN CERTIFICATE-----"));
            assert!(leaf.key.starts_with("-----BEGIN PRIVATE KEY-----"));
        }
        assert_ne!(tls.server.key, tls.loop_client.key);
        assert_ne!(tls.loop_client.key, tls.supervisor_client.key);
    }

    #[test]
    fn certificates_outlive_the_runs_deadline_by_a_day() {
        assert_eq!(validity_days(Some(3_600)), 2, "an hour rounds up to a day");
        assert_eq!(validity_days(Some(86_400)), 2);
        assert_eq!(validity_days(Some(86_401)), 3);
        assert_eq!(validity_days(None), UNBOUNDED_VALIDITY_DAYS);
    }

    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn roots(ca: &str) -> rustls::RootCertStore {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(ca.as_bytes()).expect("ca pem"))
            .expect("add root");
        roots
    }

    fn chain(leaf: &Leaf) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        (
            vec![CertificateDer::from_pem_slice(leaf.cert.as_bytes()).expect("cert pem")],
            PrivateKeyDer::from_pem_slice(leaf.key.as_bytes()).expect("key pem"),
        )
    }

    /// Serve one handshake as `server` and dial it as `client` at `dial`; whether both ends agree.
    async fn handshake(server: &GatewayTls, client: (&GatewayTls, &Leaf), dial: IpAddr) -> bool {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots(&server.ca)))
            .build()
            .expect("client verifier");
        let (certs, key) = chain(&server.server);
        let server_cfg = rustls::ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key)
            .expect("server config");
        let (certs, key) = chain(client.1);
        let client_cfg = rustls::ClientConfig::builder()
            .with_root_certificates(roots(&client.0.ca))
            .with_client_auth_cert(certs, key)
            .expect("client config");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_cfg));
        let served = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            let Ok(mut stream) = acceptor.accept(tcp).await else {
                return false;
            };
            let mut buf = [0u8; 1];
            stream.read_exact(&mut buf).await.is_ok() && stream.write_all(&buf).await.is_ok()
        });
        let tcp = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let dialed = match tokio_rustls::TlsConnector::from(Arc::new(client_cfg))
            .connect(ServerName::IpAddress(dial.into()), tcp)
            .await
        {
            Ok(mut stream) => {
                let mut buf = [0u8; 1];
                stream.write_all(b"x").await.is_ok()
                    && stream.read_exact(&mut buf).await.is_ok()
                    && &buf == b"x"
            }
            Err(_) => false,
        };
        let served = served.await.expect("join");
        dialed && served
    }

    fn mint_for(ip: &str) -> GatewayTls {
        GatewayTls::mint(ip.parse().expect("ip"), today(), validity_days(None)).expect("mint")
    }

    #[tokio::test]
    async fn the_loop_and_supervisors_complete_mtls_with_their_own_runs_gateway() {
        let run = mint_for("127.0.0.1");
        let local = "127.0.0.1".parse().expect("ip");
        assert!(handshake(&run, (&run, &run.loop_client), local).await);
        assert!(handshake(&run, (&run, &run.supervisor_client), local).await);
    }

    #[tokio::test]
    async fn another_runs_client_certificate_is_refused() {
        let run = mint_for("127.0.0.1");
        let other = mint_for("127.0.0.1");
        let local = "127.0.0.1".parse().expect("ip");
        assert!(!handshake(&run, (&run, &other.loop_client), local).await);
    }

    #[tokio::test]
    async fn the_server_certificate_names_only_the_gateway_address() {
        let run = mint_for("10.0.0.9");
        let local = "127.0.0.1".parse().expect("ip");
        assert!(!handshake(&run, (&run, &run.loop_client), local).await);
    }

    #[tokio::test]
    async fn a_server_certificate_cannot_authenticate_a_client() {
        let run = mint_for("127.0.0.1");
        let local = "127.0.0.1".parse().expect("ip");
        assert!(!handshake(&run, (&run, &run.server), local).await);
    }
}
