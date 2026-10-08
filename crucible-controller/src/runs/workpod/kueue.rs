//! The per-run crucible-kueue grant. The controller is its only authority: it mints a fresh bearer
//! token, records the token's digest and the profile's scope in the tenants namespace on the
//! dispatch cluster, and hands the token to the loop pod through the run Secret. Nothing a pack or
//! a loop pod supplies reaches the record.

use crate::secrets::deliver::Delivery;
use crucible::deploy::profile::KueueCfg;
use crucible_contract::kueue::{
    ENV_KUEUE_TOKEN, ENV_KUEUE_URL, TENANT_LABEL, TENANT_LABEL_VALUE, TENANT_RECORD_KEY,
    TenantRecord, record_name, token_digest,
};
use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use std::collections::BTreeMap;
use std::fmt;

const TOKEN_BYTES: usize = 32;

/// Extra time past a playbook's own wall-clock ceiling that its token still authenticates, so a
/// run collected late can still stop and delete what it submitted.
const EXPIRY_SLACK_SECS: u64 = 3600;

#[derive(Debug, thiserror::Error)]
pub enum GrantError {
    #[error("drawing random bytes for the crucible-kueue token failed")]
    Random,
    #[error("serializing the crucible-kueue tenant record: {0}")]
    Record(#[from] serde_json::Error),
}

/// A minted token. Never printed.
pub struct Token(String);

impl Token {
    pub fn mint() -> Result<Self, GrantError> {
        let mut bytes = [0u8; TOKEN_BYTES];
        aws_lc_rs::rand::fill(&mut bytes).map_err(|_| GrantError::Random)?;
        Ok(Self(crucible_contract::artifact::lower_hex(&bytes)))
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// What a dispatch creates for one run's grant.
#[derive(Debug)]
pub struct Grant {
    pub tenant_namespace: String,
    pub record: ConfigMap,
    /// The token and URL, folded into the run's Secret delivery. Never agent-visible: the sandbox
    /// reaches the token only as the gateway's placeholder.
    pub delivery: Delivery,
}

/// The run's identity and lifetime, as the grant needs them.
pub struct RunScope<'a> {
    pub cluster: &'a str,
    pub run_id: &'a str,
    /// The run pod's name, which the server labels the run's workloads with: a run id can exceed
    /// a label value's 63 characters, a pod name cannot.
    pub pod_name: &'a str,
    /// Unix seconds at dispatch.
    pub now: u64,
    /// The run's wall-clock ceiling, when it states one.
    pub max_time_secs: Option<u64>,
}

/// The deploy profile's `[kueue]` table, `None` when the controller has no profile or the profile
/// has no table.
pub async fn profile_cfg(cfg: &crate::config::ControllerCfg) -> anyhow::Result<Option<KueueCfg>> {
    load_cfg(cfg.deploy_profile.clone()).await
}

/// The `[kueue]` table of the deploy profile at `path`, `None` without a profile or table.
pub async fn load_cfg(path: Option<std::path::PathBuf>) -> anyhow::Result<Option<KueueCfg>> {
    use anyhow::Context;
    let Some(path) = path else {
        return Ok(None);
    };
    tokio::task::spawn_blocking(move || {
        crucible::deploy::DeployProfile::load(&path).map(|profile| profile.kueue)
    })
    .await
    .context("joining the deploy profile read")?
}

/// The grant for a run dispatched onto `run.cluster`, or `None` when the server runs elsewhere.
pub fn grant(
    cfg: &KueueCfg,
    run: &RunScope<'_>,
    token: Token,
) -> Result<Option<Grant>, GrantError> {
    if cfg.cluster != run.cluster {
        return Ok(None);
    }
    let lifetime = run.max_time_secs.map_or(cfg.token_ttl_secs, |secs| {
        secs.saturating_add(EXPIRY_SLACK_SECS)
    });
    let record = TenantRecord {
        run_id: run.pod_name.to_string(),
        token_sha256: token_digest(&token.0),
        namespace: cfg.workload_namespace.to_string(),
        role: cfg.role,
        queues: cfg.queues.iter().map(ToString::to_string).collect(),
        expires_at: Some(run.now.saturating_add(lifetime)),
    };
    let tenant_namespace = cfg.tenant_namespace.to_string();
    let record = ConfigMap {
        metadata: ObjectMeta {
            name: Some(record_name(run.pod_name)),
            namespace: Some(tenant_namespace.clone()),
            labels: Some(BTreeMap::from([(
                TENANT_LABEL.to_string(),
                TENANT_LABEL_VALUE.to_string(),
            )])),
            annotations: Some(BTreeMap::from([(
                crate::daemon::RUN_ID_ANNOTATION.to_string(),
                run.run_id.to_string(),
            )])),
            ..Default::default()
        },
        data: Some(BTreeMap::from([(
            TENANT_RECORD_KEY.to_string(),
            serde_json::to_string(&record)?,
        )])),
        ..Default::default()
    };
    let delivery = Delivery {
        data: BTreeMap::from([(ENV_KUEUE_TOKEN.to_string(), token.0)]),
        env: vec![(ENV_KUEUE_TOKEN.to_string(), ENV_KUEUE_TOKEN.to_string())],
        plain_env: vec![(ENV_KUEUE_URL.to_string(), cfg.url())],
        ..Delivery::default()
    };
    Ok(Some(Grant {
        tenant_namespace,
        record,
        delivery,
    }))
}

#[cfg(test)]
mod tests {
    use crate::runs::workpod::kueue::*;
    use crucible::deploy::profile::DeployProfile;
    use crucible_contract::kueue::TenantRole;

    const PROFILE: &str = r#"
        [cluster]
        loop_namespace = "crucible-system"
        rig_namespace = "rig"
        service_account = "crucible-spoke"
        supervisor_image = "registry.example.com/openshell-supervisor:latest"
        [image]
        loop = "registry.example.com/crucible-loop:latest"
        pull_secret = "quay-pull"
        [kueue]
        image = "registry.example.com/crucible-kueue:latest"
        cluster = "pirate"
        namespace = "crucible-system"
        tenant_namespace = "crucible-kueue-tenants"
        workload_namespace = "crucible-kueue-e2e"
        queues = ["crucible-e2e"]
        record_writers = ["crucible-system/crucible-hub-dispatch"]
        token_ttl_secs = 600
    "#;

    fn cfg() -> KueueCfg {
        toml::from_str::<DeployProfile>(PROFILE)
            .ok()
            .and_then(|p| p.kueue)
            .expect("profile parses")
    }

    fn run(cluster: &str, max_time_secs: Option<u64>) -> RunScope<'_> {
        RunScope {
            cluster,
            run_id: "playbook_kueue-e2e_01a11c37-256d-76b8-9837-e997d48e0485-1791474738",
            pod_name: "crucible-run-brave-otter",
            now: 1_000,
            max_time_secs,
        }
    }

    fn record(grant: &Grant) -> TenantRecord {
        let data = grant.record.data.as_ref().expect("data");
        serde_json::from_str(&data[TENANT_RECORD_KEY]).expect("record")
    }

    #[test]
    fn a_run_on_another_cluster_gets_no_grant() {
        assert!(
            grant(&cfg(), &run("hub", None), Token::mint().unwrap())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn the_record_holds_the_digest_and_the_profile_scope_and_never_the_token() {
        let token = Token("secret-token".to_string());
        let g = grant(&cfg(), &run("pirate", None), token).unwrap().unwrap();
        let r = record(&g);
        assert_eq!(r.token_sha256, token_digest("secret-token"));
        assert_eq!(r.run_id, "crucible-run-brave-otter");
        assert_eq!(
            g.record.metadata.annotations.as_ref().unwrap()[crate::daemon::RUN_ID_ANNOTATION],
            "playbook_kueue-e2e_01a11c37-256d-76b8-9837-e997d48e0485-1791474738"
        );
        assert_eq!(r.namespace, "crucible-kueue-e2e");
        assert_eq!(r.role, TenantRole::Submitter);
        assert_eq!(r.queues, ["crucible-e2e"]);
        let text = serde_json::to_string(&g.record).unwrap();
        assert!(!text.contains("secret-token"), "{text}");
        assert_eq!(g.tenant_namespace, "crucible-kueue-tenants");
        assert_eq!(
            g.record.metadata.namespace.as_deref(),
            Some("crucible-kueue-tenants")
        );
        assert_eq!(
            g.record.metadata.name.as_deref(),
            Some("kueue-tenant-crucible-run-brave-otter")
        );
        assert_eq!(
            g.record.metadata.labels.as_ref().unwrap()[TENANT_LABEL],
            TENANT_LABEL_VALUE
        );
    }

    #[test]
    fn the_token_reaches_the_pod_through_the_secret_and_is_never_agent_visible() {
        let g = grant(&cfg(), &run("pirate", None), Token("t".to_string()))
            .unwrap()
            .unwrap();
        assert_eq!(g.delivery.data[ENV_KUEUE_TOKEN], "t");
        assert_eq!(
            g.delivery.env,
            [(ENV_KUEUE_TOKEN.to_string(), ENV_KUEUE_TOKEN.to_string())]
        );
        assert_eq!(
            g.delivery.plain_env,
            [(
                ENV_KUEUE_URL.to_string(),
                "http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp".to_string()
            )]
        );
        assert!(g.delivery.agent_visible.is_empty());
    }

    #[test]
    fn expiry_follows_the_runs_ceiling_plus_slack_or_the_profile_ttl() {
        let with_ceiling = grant(&cfg(), &run("pirate", Some(1800)), Token("t".into()))
            .unwrap()
            .unwrap();
        assert_eq!(record(&with_ceiling).expires_at, Some(1_000 + 1800 + 3600));
        let without = grant(&cfg(), &run("pirate", None), Token("t".into()))
            .unwrap()
            .unwrap();
        assert_eq!(record(&without).expires_at, Some(1_000 + 600));
    }

    #[test]
    fn minted_tokens_are_distinct_and_never_debug_printed() {
        let a = Token::mint().unwrap();
        let b = Token::mint().unwrap();
        assert_ne!(a.0, b.0);
        assert_eq!(a.0.len(), 64);
        assert_eq!(format!("{a:?}"), "Token(<redacted>)");
    }
}
