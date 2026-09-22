//! Leader election for the resident daemon (ADR-0049 §1): a `coordination.k8s.io/v1` Lease
//! elects the active reconciler, and the maintenance advisory lock fences it. The lease is the
//! coordination — observable with `kubectl get lease`, unaffected by a database blip. The lock is
//! the fence — a stale leader that stalls past its lease expiry still cannot write, because the
//! new leader cannot take the lock until the old session dies, and the old leader steps down the
//! moment its own renew or lock ping fails.
//!
//! Hand-rolled on the tree's existing kube client: the candidate crates each pin their own kube
//! generation, and a Lease is CRUD on one object. The decision half is pure ([`decide`]) so it is
//! unit-tested directly; the I/O loop around it stays thin.
//!
//! Without a reachable kube API ([`LeaderConfig::from_env`] finding election disabled, or local
//! dev outside a cluster), election degrades to fence-only: the caller leads immediately once it
//! holds the advisory lock, which is exactly the pre-election single-replica behavior.

use anyhow::{Context, Result};
use jiff::Timestamp;
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use kube::api::{Api, ObjectMeta, Patch, PatchParams, PostParams};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;

/// How long a holder's claim stands without renewal before another replica may take the lease.
const LEASE_DURATION_SECS: i32 = 15;
/// Renew cadence; three renew attempts fit inside one lease duration.
const RENEW_INTERVAL: Duration = Duration::from_secs(5);
/// How often the fence connection is pinged; a dead session is noticed within this.
const FENCE_PING_INTERVAL: Duration = Duration::from_secs(5);
/// Backoff between campaign polls while another replica holds a live lease, and between fence
/// attempts while the outgoing leader's lock session drains.
const CAMPAIGN_INTERVAL: Duration = Duration::from_secs(3);

/// What the election loop should do with the lease it just read.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// No lease object yet: create one naming us.
    Create,
    /// Expired, unheld, or already ours: write our claim (CAS via resourceVersion).
    Claim,
    /// Another replica holds a live lease: wait and poll again.
    Wait,
}

/// The pure election decision: what `identity` should do about `lease` at `now`.
fn decide(lease: Option<&Lease>, identity: &str, now: Timestamp) -> Action {
    let Some(lease) = lease else {
        return Action::Create;
    };
    let spec = lease.spec.as_ref();
    let holder = spec
        .and_then(|s| s.holder_identity.as_deref())
        .unwrap_or("");
    if holder.is_empty() || holder == identity {
        return Action::Claim;
    }
    let renewed = spec.and_then(|s| s.renew_time.as_ref()).map(|t| t.0);
    let duration = spec
        .and_then(|s| s.lease_duration_seconds)
        .unwrap_or(LEASE_DURATION_SECS);
    match renewed {
        None => Action::Claim,
        Some(renewed) => {
            let expiry = Timestamp::from_second(renewed.as_second() + i64::from(duration))
                .unwrap_or(Timestamp::MAX);
            if now >= expiry {
                Action::Claim
            } else {
                Action::Wait
            }
        }
    }
}

/// Where the lease lives and who we claim to be. `None` from [`LeaderConfig::from_env`] means
/// election is disabled and the caller should run fence-only.
#[derive(Debug, Clone)]
pub struct LeaderConfig {
    pub namespace: String,
    pub lease_name: String,
    pub identity: String,
}

impl LeaderConfig {
    /// Election is on when the chart projects `POD_NAME`/`POD_NAMESPACE` (the downward API is the
    /// signal we are a replica among replicas). `CONTROLLER_LEADER_ELECTION=off` forces it off.
    pub fn from_env() -> Option<LeaderConfig> {
        if std::env::var("CONTROLLER_LEADER_ELECTION").is_ok_and(|v| v == "off") {
            return None;
        }
        let identity = std::env::var("POD_NAME").ok()?;
        let namespace = std::env::var("POD_NAMESPACE").ok()?;
        Some(LeaderConfig {
            namespace,
            lease_name: std::env::var("CONTROLLER_LEASE_NAME")
                .unwrap_or_else(|_| "crucible-controller-leader".to_string()),
            identity,
        })
    }
}

/// Held by the one active reconciler: the advisory-lock fence, the lease renew task, and the
/// channel that fires when either is lost. Dropping it aborts the background tasks; the lock
/// connection closes with them and the server releases the lock.
pub struct Leadership {
    lost: tokio::sync::watch::Receiver<Option<&'static str>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Leadership {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Leadership {
    /// Resolves with the reason the moment leadership is lost — lease renewal failing past its
    /// duration, or the fence connection dying. The daemon treats it as an immediate step-down.
    pub async fn lost(&mut self) -> &'static str {
        loop {
            if let Some(reason) = *self.lost.borrow() {
                return reason;
            }
            if self.lost.changed().await.is_err() {
                return "leadership watch closed";
            }
        }
    }
}

/// Campaign until this process leads, then fence it. Returns when both the lease (when election
/// is on) and the advisory lock are held, or `Ok(None)` when `shutdown` fires first.
///
/// The fence is taken *after* the lease on purpose: while the outgoing leader's lock session
/// drains, the new lease holder waits on the lock rather than writing — the fence, not the
/// lease, is what makes a stale leader harmless.
pub async fn campaign(
    cfg: Option<LeaderConfig>,
    pool: &PgPool,
    shutdown: Arc<tokio::sync::Notify>,
) -> Result<Option<Leadership>> {
    let lease = match &cfg {
        Some(cfg) => {
            let client = kube::Client::try_default()
                .await
                .context("building the kube client for leader election")?;
            let api: Api<Lease> = Api::namespaced(client, &cfg.namespace);
            match win_lease(&api, cfg, &shutdown).await? {
                Won::Lease => Some(api),
                Won::Shutdown => return Ok(None),
            }
        }
        None => {
            tracing::info!("leader election disabled (no POD_NAME/POD_NAMESPACE); fence-only");
            None
        }
    };

    // The fence. A running maintenance command, or the outgoing leader's not-yet-dead session,
    // holds it; wait rather than fail — the lease already names us, so nothing else will lead.
    let fence = loop {
        match crate::client::try_maintenance_lock(pool).await? {
            Some(lock) => break lock,
            None => {
                tracing::info!(
                    "waiting for the maintenance advisory lock (outgoing leader or a \
                     maintenance command still holds it)"
                );
                tokio::select! {
                    _ = tokio::time::sleep(CAMPAIGN_INTERVAL) => {}
                    _ = shutdown.notified() => return Ok(None),
                }
            }
        }
    };

    let (tx, rx) = tokio::sync::watch::channel(None);
    let mut tasks = Vec::new();

    // The fence watchdog: a session lock lives exactly as long as its connection, so a failed
    // ping means the lock is gone (or about to be) and writing further would be unfenced.
    let fence_tx = tx.clone();
    tasks.push(tokio::spawn(async move {
        let mut fence = fence;
        loop {
            tokio::time::sleep(FENCE_PING_INTERVAL).await;
            if let Err(e) = fence.ping().await {
                tracing::error!(error = %format!("{e:#}"), "advisory-lock fence lost");
                let _ = fence_tx.send(Some("advisory-lock fence lost"));
                return;
            }
        }
    }));

    if let (Some(api), Some(cfg)) = (lease, cfg) {
        let renew_tx = tx;
        tasks.push(tokio::spawn(async move {
            let mut misses = 0u32;
            loop {
                tokio::time::sleep(RENEW_INTERVAL).await;
                match renew(&api, &cfg).await {
                    Ok(true) => misses = 0,
                    Ok(false) => {
                        tracing::error!("lease taken by another holder");
                        let _ = renew_tx.send(Some("lease taken by another holder"));
                        return;
                    }
                    Err(e) => {
                        misses += 1;
                        tracing::warn!(misses, error = %format!("{e:#}"), "lease renew failed");
                        // Give up only once the lease we last wrote has actually expired:
                        // until then no other replica can claim it.
                        if u64::from(misses).saturating_mul(RENEW_INTERVAL.as_secs())
                            >= u64::from(LEASE_DURATION_SECS.unsigned_abs())
                        {
                            let _ = renew_tx.send(Some("lease renewal failed past expiry"));
                            return;
                        }
                    }
                }
            }
        }));
    }

    Ok(Some(Leadership { lost: rx, tasks }))
}

enum Won {
    Lease,
    Shutdown,
}

/// Poll the lease until we hold it or shutdown fires. Conflicts (another replica writing the
/// same generation) and transient API errors both fall back to the campaign interval.
async fn win_lease(
    api: &Api<Lease>,
    cfg: &LeaderConfig,
    shutdown: &tokio::sync::Notify,
) -> Result<Won> {
    loop {
        let current = api
            .get_opt(&cfg.lease_name)
            .await
            .context("reading the leader lease")?;
        match decide(current.as_ref(), &cfg.identity, Timestamp::now()) {
            Action::Create => match api.create(&PostParams::default(), &claim(cfg, None)).await {
                Ok(_) => {
                    tracing::info!(lease = %cfg.lease_name, identity = %cfg.identity, "lease created; leading");
                    return Ok(Won::Lease);
                }
                Err(kube::Error::Api(e)) if e.code == 409 => {}
                Err(e) => return Err(e).context("creating the leader lease"),
            },
            Action::Claim => {
                let resource_version = current.and_then(|l| l.metadata.resource_version);
                match api
                    .replace(
                        &cfg.lease_name,
                        &PostParams::default(),
                        &claim(cfg, resource_version),
                    )
                    .await
                {
                    Ok(_) => {
                        tracing::info!(lease = %cfg.lease_name, identity = %cfg.identity, "lease claimed; leading");
                        return Ok(Won::Lease);
                    }
                    Err(kube::Error::Api(e)) if e.code == 409 => {}
                    Err(e) => return Err(e).context("claiming the leader lease"),
                }
            }
            Action::Wait => {}
        }
        tokio::select! {
            _ = tokio::time::sleep(CAMPAIGN_INTERVAL) => {}
            _ = shutdown.notified() => return Ok(Won::Shutdown),
        }
    }
}

/// One renew: patch our own renewTime. `Ok(false)` when the holder is no longer us.
async fn renew(api: &Api<Lease>, cfg: &LeaderConfig) -> Result<bool> {
    let current = api
        .get(&cfg.lease_name)
        .await
        .context("reading the lease to renew")?;
    let holder = current
        .spec
        .as_ref()
        .and_then(|s| s.holder_identity.as_deref())
        .unwrap_or("");
    if holder != cfg.identity {
        return Ok(false);
    }
    let patch = serde_json::json!({
        "spec": { "renewTime": MicroTime(Timestamp::now()) }
    });
    api.patch(
        &cfg.lease_name,
        &PatchParams::default(),
        &Patch::Merge(&patch),
    )
    .await
    .context("renewing the lease")?;
    Ok(true)
}

fn claim(cfg: &LeaderConfig, resource_version: Option<String>) -> Lease {
    Lease {
        metadata: ObjectMeta {
            name: Some(cfg.lease_name.clone()),
            resource_version,
            ..Default::default()
        },
        spec: Some(LeaseSpec {
            holder_identity: Some(cfg.identity.clone()),
            lease_duration_seconds: Some(LEASE_DURATION_SECS),
            acquire_time: Some(MicroTime(Timestamp::now())),
            renew_time: Some(MicroTime(Timestamp::now())),
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn lease(holder: &str, renewed_secs_ago: i64, duration: i32) -> Lease {
        Lease {
            metadata: ObjectMeta::default(),
            spec: Some(LeaseSpec {
                holder_identity: Some(holder.to_string()),
                lease_duration_seconds: Some(duration),
                renew_time: Some(MicroTime(
                    Timestamp::now() - jiff::Span::new().seconds(renewed_secs_ago),
                )),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn no_lease_creates() {
        assert_eq!(decide(None, "me", Timestamp::now()), Action::Create);
    }

    #[test]
    fn own_lease_reclaims() {
        let l = lease("me", 0, 15);
        assert_eq!(decide(Some(&l), "me", Timestamp::now()), Action::Claim);
    }

    #[test]
    fn live_foreign_lease_waits() {
        let l = lease("them", 2, 15);
        assert_eq!(decide(Some(&l), "me", Timestamp::now()), Action::Wait);
    }

    #[test]
    fn expired_foreign_lease_claims() {
        let l = lease("them", 60, 15);
        assert_eq!(decide(Some(&l), "me", Timestamp::now()), Action::Claim);
    }

    #[test]
    fn empty_holder_claims() {
        let l = lease("", 0, 15);
        assert_eq!(decide(Some(&l), "me", Timestamp::now()), Action::Claim);
    }

    /// The lease layer against a real cluster: A wins, B sees a live foreign lease, and once
    /// A stops renewing past the duration, B claims. Gated on `LEADER_E2E_CONTEXT` (the kind CI
    /// job); a run without it is a no-op pass.
    #[tokio::test]
    async fn leader_lease_kind_round_trip() {
        if std::env::var("LEADER_E2E_CONTEXT").is_err() {
            return;
        }
        crate::install_crypto_provider();
        let client = kube::Client::try_default().await.expect("kube client");
        let name = format!("crucible-leader-e2e-{}", std::process::id());
        let api: Api<Lease> = Api::namespaced(client, "default");
        let a = LeaderConfig {
            namespace: "default".to_string(),
            lease_name: name.clone(),
            identity: "replica-a".to_string(),
        };
        let b = LeaderConfig {
            identity: "replica-b".to_string(),
            ..a.clone()
        };

        let shutdown = tokio::sync::Notify::new();
        match win_lease(&api, &a, &shutdown).await.expect("A campaigns") {
            Won::Lease => {}
            Won::Shutdown => panic!("no shutdown was requested"),
        }
        assert!(
            renew(&api, &a).await.expect("A renews"),
            "A holds the lease"
        );

        // B reads a live foreign lease and must wait.
        let current = api.get(&name).await.expect("lease readable");
        assert_eq!(
            decide(Some(&current), &b.identity, Timestamp::now()),
            Action::Wait
        );

        // A stops renewing; past the lease duration B claims it.
        tokio::time::sleep(Duration::from_secs(
            u64::from(LEASE_DURATION_SECS.unsigned_abs()) + 2,
        ))
        .await;
        match win_lease(&api, &b, &shutdown).await.expect("B campaigns") {
            Won::Lease => {}
            Won::Shutdown => panic!("no shutdown was requested"),
        }
        let held = api.get(&name).await.expect("lease readable");
        assert_eq!(
            held.spec.and_then(|s| s.holder_identity),
            Some("replica-b".to_string())
        );
        assert!(
            !renew(&api, &a).await.expect("A's renew runs"),
            "A lost the lease"
        );
        let _ = api.delete(&name, &kube::api::DeleteParams::default()).await;
    }

    #[test]
    fn unrenewed_foreign_lease_claims() {
        let l = Lease {
            metadata: ObjectMeta::default(),
            spec: Some(LeaseSpec {
                holder_identity: Some("them".to_string()),
                lease_duration_seconds: Some(15),
                renew_time: None,
                ..Default::default()
            }),
        };
        assert_eq!(decide(Some(&l), "me", Timestamp::now()), Action::Claim);
    }
}
