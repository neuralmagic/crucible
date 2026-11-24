//! The registry credentials a run brings: its pack's `[buildit]` push repository and `[kueue]`
//! pull secrets, resolved against the secrets its scope bound. The push authfile is a file the
//! binding already projects into the loop pod. Each pull secret is a
//! `kubernetes.io/dockerconfigjson` Secret the controller writes into crucible-kueue's own
//! namespace at dispatch, records in `run_pull_secrets`, and deletes once the run's pod is
//! collected. The run's tokens name them in their `pull_secrets` claim, read back from that
//! table, and crucible-kueue copies one beside each workload that pulls with it.

use crate::runs::workpod::dispatcher::PodDispatcher;
use crate::secrets::deliver::Delivery;
use crate::secrets::grant::GrantMint;
use crate::secrets::{ProjectionKind, SecretName};
use anyhow::{Context, Result};
use crucible::deploy::servers::{Hosting, RunPullSecret, RunPush, RunRegistries, ServerSpec};
use crucible::manifest::{BuilditPushCfg, KueuePullCfg};
use k8s_openapi::api::core::v1 as core;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// How often the leader looks for pull Secrets whose run has ended.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(300);

/// The run pod a pull Secret was written for.
pub const PULL_SECRET_FOR_LABEL: &str = "crucible.dev/pull-secret-for";

const DOCKER_CONFIG_TYPE: &str = "kubernetes.io/dockerconfigjson";
const DOCKER_CONFIG_KEY: &str = ".dockerconfigjson";

/// What a pack's manifest says about registries.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct PackRegistries {
    #[serde(default)]
    buildit: BuilditPushCfg,
    #[serde(default)]
    kueue: KueuePullCfg,
}

/// Read a pack tree's `[buildit]` and `[kueue]` tables. A tree with no manifest has neither.
pub fn of_pack(
    pack_root: &Path,
    variant: Option<&crucible::knobs::VariantName>,
) -> Result<PackRegistries> {
    if !pack_root.join(crate::playbooks::packs::MANIFEST).is_file() {
        return Ok(PackRegistries::default());
    }
    let text = crate::playbooks::packs::manifest_text(pack_root, variant)?;
    toml::from_str(&text).context("parsing the pack's [buildit] and [kueue]")
}

/// Why a run cannot have the registry credentials its pack names.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Refusal {
    #[error("the pack's {table} names secret {name:?}, and this launch's scope binds none for it")]
    Unbound { table: &'static str, name: String },
    #[error(
        "the pack's [buildit] push_secret {name} is bound as an environment variable; buildit \
         reads the authfile from a file, so bind it to a path"
    )]
    NotAFile { name: SecretName },
    #[error("the pack names a push repository, and no buildit serves runs on cluster {cluster}")]
    NoBuildit { cluster: String },
    #[error(
        "the pack brings pull secrets, and no crucible-kueue on cluster {cluster} takes them: \
         set [kueue] run_pull_secrets in the deploy profile"
    )]
    NoPullSecrets { cluster: String },
}

/// What a pack's tables resolved to for one launch, before its run has a pod to name things
/// after.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    push: Option<RunPush>,
    pulls: Vec<Pull>,
}

/// One pull credential: the declared secret that holds it and the images it pulls.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pull {
    declared: SecretName,
    registry: String,
}

/// One pull Secret to write, and where.
#[derive(Debug, Clone, PartialEq)]
pub struct PullSecretObject {
    pub namespace: String,
    pub secret: RunPullSecret,
    pub object: core::Secret,
}

#[derive(Debug, thiserror::Error)]
#[error("the run's Secret holds no value for {0}, which its pull secret is made from")]
pub struct NoValue(SecretName);

fn bound<'a>(
    table: &'static str,
    name: &str,
    mints: &'a [GrantMint],
) -> Result<&'a GrantMint, Refusal> {
    let unbound = || Refusal::Unbound {
        table,
        name: name.to_string(),
    };
    let declared = SecretName::parse(name).map_err(|_| unbound())?;
    mints
        .iter()
        .find(|m| m.item.declared_name == declared)
        .ok_or_else(unbound)
}

impl PackRegistries {
    /// What a launch of the pack brings, given the bindings it resolved.
    pub fn resolve(&self, mints: &[GrantMint]) -> Result<Resolved, Refusal> {
        let push = match (&self.buildit.push_repo, &self.buildit.push_secret) {
            (Some(repo), Some(secret)) => {
                let mint = bound("[buildit] push_secret", secret, mints)?;
                if mint.item.projection_kind != ProjectionKind::File {
                    return Err(Refusal::NotAFile {
                        name: mint.item.declared_name.clone(),
                    });
                }
                Some(RunPush {
                    repo: repo.to_string(),
                    authfile: mint.item.projection.trim().to_string(),
                })
            }
            _ => None,
        };
        let pulls = self
            .kueue
            .pull_secrets
            .iter()
            .map(|pull| {
                let mint = bound("[kueue] pull_secrets", &pull.secret, mints)?;
                Ok(Pull {
                    declared: mint.item.declared_name.clone(),
                    registry: pull.registry.to_string(),
                })
            })
            .collect::<Result<_, Refusal>>()?;
        Ok(Resolved { push, pulls })
    }
}

impl Resolved {
    /// Refuse unless `granted`, the servers the run gets on `cluster`, can use what it brings.
    pub fn check_served(&self, granted: &[&ServerSpec], cluster: &str) -> Result<(), Refusal> {
        let builds = granted
            .iter()
            .any(|s| matches!(s.hosting, Hosting::Buildit { .. }));
        if self.push.is_some() && !builds {
            return Err(Refusal::NoBuildit {
                cluster: cluster.to_string(),
            });
        }
        let pulls = granted.iter().any(|s| s.pull_secret_namespace.is_some());
        if !self.pulls.is_empty() && !pulls {
            return Err(Refusal::NoPullSecrets {
                cluster: cluster.to_string(),
            });
        }
        Ok(())
    }

    /// What the run on `pod_name` brings: its pull Secrets are named after its pod.
    pub fn for_pod(&self, pod_name: &str) -> RunRegistries {
        RunRegistries {
            push: self.push.clone(),
            pull_secrets: self
                .pulls
                .iter()
                .enumerate()
                .map(|(i, pull)| RunPullSecret {
                    name: format!("{pod_name}-pull-{i}"),
                    registry: pull.registry.clone(),
                })
                .collect(),
        }
    }

    /// The pull Secrets of the run on `pod_name`, one in the pull secret namespace of each
    /// server in `granted` that takes them, holding the values `delivery` read.
    pub fn pull_secret_objects(
        &self,
        granted: &[&ServerSpec],
        delivery: &Delivery,
        pod_name: &str,
    ) -> Result<Vec<PullSecretObject>, NoValue> {
        let mut namespaces: Vec<&String> = granted
            .iter()
            .filter_map(|s| s.pull_secret_namespace.as_ref())
            .collect();
        namespaces.sort();
        namespaces.dedup();
        let mut objects = Vec::new();
        let secrets = self.for_pod(pod_name).pull_secrets;
        for (secret, pull) in secrets.iter().zip(&self.pulls) {
            let value = delivery
                .data
                .get(pull.declared.as_str())
                .ok_or_else(|| NoValue(pull.declared.clone()))?;
            for namespace in &namespaces {
                objects.push(PullSecretObject {
                    namespace: namespace.to_string(),
                    secret: secret.clone(),
                    object: core::Secret {
                        metadata: ObjectMeta {
                            name: Some(secret.name.clone()),
                            namespace: Some(namespace.to_string()),
                            labels: Some(BTreeMap::from([(
                                PULL_SECRET_FOR_LABEL.to_string(),
                                pod_name.to_string(),
                            )])),
                            ..Default::default()
                        },
                        string_data: Some(BTreeMap::from([(
                            DOCKER_CONFIG_KEY.to_string(),
                            value.clone(),
                        )])),
                        type_: Some(DOCKER_CONFIG_TYPE.to_string()),
                        ..Default::default()
                    },
                });
            }
        }
        Ok(objects)
    }
}

/// Record each of `objects` for the run on `pod_name`, then create it on `cluster`.
pub async fn write(
    pool: &sqlx::PgPool,
    dispatcher: &dyn PodDispatcher,
    cluster: &str,
    pod_name: &str,
    objects: Vec<PullSecretObject>,
) -> Result<()> {
    for PullSecretObject {
        namespace,
        secret,
        object,
    } in objects
    {
        sqlx::query(
            "INSERT INTO run_pull_secrets (pod_name, cluster, namespace, name, registry, created_at)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (pod_name, namespace, name) DO UPDATE SET registry = EXCLUDED.registry",
        )
        .bind(pod_name)
        .bind(cluster)
        .bind(&namespace)
        .bind(&secret.name)
        .bind(&secret.registry)
        .bind(crate::clock::now_rfc3339())
        .execute(pool)
        .await
        .context("recording a run's pull secret")?;
        dispatcher
            .create_secret(cluster, &namespace, object)
            .await
            .with_context(|| format!("creating pull Secret {namespace}/{}", secret.name))?;
    }
    Ok(())
}

/// The `pull_secrets` claim of a token the run on `pod_name` presents to the server whose pull
/// secret namespace is `namespace`: each pull Secret the run has there, as `name=registry`.
pub async fn claim(pool: &sqlx::PgPool, pod_name: &str, namespace: &str) -> Result<Vec<String>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT name, registry FROM run_pull_secrets
          WHERE pod_name = $1 AND namespace = $2
          ORDER BY name",
    )
    .bind(pod_name)
    .bind(namespace)
    .fetch_all(pool)
    .await
    .context("reading a run's pull secrets")?;
    Ok(rows
        .into_iter()
        .map(|(name, registry)| RunPullSecret { name, registry }.to_string())
        .collect())
}

/// Delete the pull Secrets of the run on `pod_name` and forget each one deleted; the number left
/// behind, which the sweep retries.
pub async fn remove(
    pool: &sqlx::PgPool,
    dispatcher: &dyn PodDispatcher,
    pod_name: &str,
) -> Result<usize> {
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT cluster, namespace, name FROM run_pull_secrets WHERE pod_name = $1")
            .bind(pod_name)
            .fetch_all(pool)
            .await
            .context("reading a run's pull secrets")?;
    let mut left = 0;
    for (cluster, namespace, name) in rows {
        if let Err(e) = dispatcher.delete_secret(&cluster, &namespace, &name).await {
            tracing::warn!(
                error = format!("{e:#}"),
                pod_name,
                cluster,
                namespace,
                name,
                "deleting a run's pull Secret failed; the sweep retries it"
            );
            left += 1;
            continue;
        }
        sqlx::query(
            "DELETE FROM run_pull_secrets WHERE pod_name = $1 AND namespace = $2 AND name = $3",
        )
        .bind(pod_name)
        .bind(&namespace)
        .bind(&name)
        .execute(pool)
        .await
        .context("forgetting a run's pull secret")?;
    }
    Ok(left)
}

/// [`remove`], logging a failure.
pub async fn remove_logged(pool: &sqlx::PgPool, dispatcher: &dyn PodDispatcher, pod_name: &str) {
    if let Err(e) = remove(pool, dispatcher, pod_name).await {
        tracing::warn!(
            error = format!("{e:#}"),
            pod_name,
            "removing a run's pull Secrets failed; the sweep retries it"
        );
    }
}

/// [`remove`] the pull Secrets of every run whose pod is no longer running; the number left
/// behind.
pub async fn sweep(pool: &sqlx::PgPool, dispatcher: &dyn PodDispatcher) -> Result<usize> {
    let ended: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT s.pod_name FROM run_pull_secrets s
          WHERE NOT EXISTS (SELECT 1 FROM work_pods w
                             WHERE w.pod_name = s.pod_name AND w.state = 'running')",
    )
    .fetch_all(pool)
    .await
    .context("listing ended runs' pull secrets")?;
    let mut left = 0;
    for pod_name in ended {
        left += remove(pool, dispatcher, &pod_name).await?;
    }
    Ok(left)
}

pub async fn sweep_loop(db: crate::client::Db, dispatcher: std::sync::Arc<dyn PodDispatcher>) {
    let mut timer = tokio::time::interval(SWEEP_INTERVAL);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        timer.tick().await;
        if let Err(e) = sweep(db.pool(), dispatcher.as_ref()).await {
            tracing::warn!(error = %format!("{e:#}"), "run pull secret sweep failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::runs::workpod::dispatcher::{TerminalState, TurnPhase};
    use crate::runs::workpod::registries::*;
    use crucible::deploy::profile::DeployProfile;
    use std::sync::Mutex;

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
        clusters = ["ocp"]
        workload_namespaces = ["team-b", "team-a"]
        run_pull_secrets = true
        [buildit]
        clusters = ["ocp"]
        push_repo = "quay.io/shared/smoke"
    "#;

    const PACK: &str = r#"
        [agent]
        backend = "openshell"

        [[secret]]
        name = "Registry"
        kind = "registry_authfile"
        path = "/etc/registry/auth.json"

        [[secret]]
        name = "registry-ro"
        kind = "registry_authfile"

        [buildit]
        push_repo = "quay.io/mine/smoke"
        push_secret = "Registry"

        [kueue]
        pull_secrets = [
            { secret = "registry-ro", registry = "quay.io/mine" },
            { secret = "registry", registry = "ghcr.io/mine" },
        ]
    "#;

    const POD: &str = "crucible-run-brave-otter";

    fn servers(profile: &str) -> Vec<ServerSpec> {
        toml::from_str::<DeployProfile>(profile)
            .expect("profile parses")
            .agent_servers()
            .expect("servers")
    }

    fn pack(text: &str) -> PackRegistries {
        toml::from_str(text).expect("pack parses")
    }

    fn mint(name: &str, kind: ProjectionKind, projection: &str) -> GrantMint {
        GrantMint {
            item: crate::secrets::grant::GrantItem {
                binding_id: format!("binding-{name}"),
                secret_id: format!("id-{name}"),
                declared_name: SecretName::parse(name).unwrap(),
                projection_kind: kind,
                projection: projection.to_string(),
            },
            secret_name: SecretName::parse(name).unwrap(),
            owner: crate::authz::model::Principal::parse("user:wynn").unwrap(),
        }
    }

    fn mints() -> Vec<GrantMint> {
        vec![
            mint("registry", ProjectionKind::File, "/etc/registry/auth.json"),
            mint("registry-ro", ProjectionKind::File, "/etc/registry/ro.json"),
        ]
    }

    fn delivery() -> Delivery {
        Delivery {
            data: BTreeMap::from([
                ("registry".to_string(), r#"{"auths":{"rw":{}}}"#.to_string()),
                (
                    "registry-ro".to_string(),
                    r#"{"auths":{"ro":{}}}"#.to_string(),
                ),
            ]),
            ..Delivery::default()
        }
    }

    #[test]
    fn a_pack_tree_is_read_for_its_tables_and_one_without_a_manifest_has_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            of_pack(dir.path(), None).unwrap(),
            PackRegistries::default()
        );
        std::fs::write(dir.path().join("crucible.toml"), PACK).unwrap();
        assert_eq!(of_pack(dir.path(), None).unwrap(), pack(PACK));
        std::fs::write(
            dir.path().join("crucible.toml"),
            "[buildit]\npush_repo = \"https://quay.io/mine\"",
        )
        .unwrap();
        assert!(of_pack(dir.path(), None).is_err());
    }

    #[test]
    fn a_pack_without_the_tables_brings_nothing_and_needs_no_server() {
        let resolved = pack("[agent]\nbackend = \"openshell\"")
            .resolve(&[])
            .unwrap();
        assert_eq!(resolved.for_pod(POD), RunRegistries::default());
        resolved.check_served(&[], "ocp").unwrap();
        assert!(
            resolved
                .pull_secret_objects(&[], &Delivery::default(), POD)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_packs_tables_resolve_to_its_bound_authfile_and_pull_secrets_named_after_the_pod() {
        let resolved = pack(PACK).resolve(&mints()).unwrap();
        assert_eq!(
            resolved.for_pod(POD),
            RunRegistries {
                push: Some(RunPush {
                    repo: "quay.io/mine/smoke".into(),
                    authfile: "/etc/registry/auth.json".into(),
                }),
                pull_secrets: vec![
                    RunPullSecret {
                        name: format!("{POD}-pull-0"),
                        registry: "quay.io/mine".into(),
                    },
                    RunPullSecret {
                        name: format!("{POD}-pull-1"),
                        registry: "ghcr.io/mine".into(),
                    },
                ],
            }
        );
    }

    #[test]
    fn a_secret_the_scope_did_not_bind_as_a_file_is_refused() {
        let only_push = [mint("registry", ProjectionKind::File, "/a")];
        assert_eq!(
            pack(PACK).resolve(&only_push),
            Err(Refusal::Unbound {
                table: "[kueue] pull_secrets",
                name: "registry-ro".into(),
            })
        );
        assert_eq!(
            pack(PACK).resolve(&[]),
            Err(Refusal::Unbound {
                table: "[buildit] push_secret",
                name: "Registry".into(),
            })
        );
        let as_env = [
            mint("registry", ProjectionKind::Env, "REGISTRY_AUTH"),
            mint("registry-ro", ProjectionKind::File, "/b"),
        ];
        assert_eq!(
            pack(PACK).resolve(&as_env),
            Err(Refusal::NotAFile {
                name: SecretName::parse("registry").unwrap(),
            })
        );
    }

    #[test]
    fn what_a_run_brings_needs_a_server_on_its_cluster_that_uses_it() {
        let resolved = pack(PACK).resolve(&mints()).unwrap();
        let all = servers(PROFILE);
        let granted: Vec<&ServerSpec> = all.iter().collect();
        resolved.check_served(&granted, "ocp").unwrap();
        assert_eq!(
            resolved.check_served(&granted[..1], "k8s"),
            Err(Refusal::NoBuildit {
                cluster: "k8s".into()
            })
        );
        let not_taking = servers(&PROFILE.replace("run_pull_secrets = true", ""));
        let refused = resolved
            .check_served(&not_taking.iter().collect::<Vec<_>>(), "ocp")
            .unwrap_err();
        assert_eq!(
            refused,
            Refusal::NoPullSecrets {
                cluster: "ocp".into()
            }
        );
        assert!(
            refused.to_string().contains("[kueue] run_pull_secrets"),
            "{refused}"
        );
    }

    #[test]
    fn each_pull_secret_is_a_docker_config_secret_in_the_servers_namespace() {
        let resolved = pack(PACK).resolve(&mints()).unwrap();
        let all = servers(PROFILE);
        let granted: Vec<&ServerSpec> = all.iter().collect();
        let objects = resolved
            .pull_secret_objects(&granted, &delivery(), POD)
            .unwrap();
        let placed: Vec<(&str, &str)> = objects
            .iter()
            .map(|o| (o.namespace.as_str(), o.secret.name.as_str()))
            .collect();
        assert_eq!(
            placed,
            [
                ("crucible-kueue", "crucible-run-brave-otter-pull-0"),
                ("crucible-kueue", "crucible-run-brave-otter-pull-1"),
            ]
        );
        let first = &objects[0].object;
        assert_eq!(first.metadata.namespace.as_deref(), Some("crucible-kueue"));
        assert_eq!(
            first.metadata.name.as_deref(),
            Some("crucible-run-brave-otter-pull-0")
        );
        assert_eq!(
            first.type_.as_deref(),
            Some("kubernetes.io/dockerconfigjson")
        );
        assert_eq!(
            first.metadata.labels.as_ref().unwrap()[PULL_SECRET_FOR_LABEL],
            POD
        );
        assert!(first.metadata.owner_references.is_none());
        assert_eq!(
            first.string_data.as_ref().unwrap()[".dockerconfigjson"],
            r#"{"auths":{"ro":{}}}"#
        );
        assert_eq!(
            objects[1].object.string_data.as_ref().unwrap()[".dockerconfigjson"],
            r#"{"auths":{"rw":{}}}"#
        );

        let unread = resolved
            .pull_secret_objects(&granted, &Delivery::default(), POD)
            .unwrap_err();
        assert!(unread.to_string().contains("registry-ro"), "{unread}");
    }

    type Placed = (String, String, String);

    #[derive(Default)]
    struct Cluster {
        secrets: Mutex<Vec<Placed>>,
        refuse_delete: Mutex<Option<String>>,
    }

    impl Cluster {
        fn secrets(&self) -> Vec<Placed> {
            self.secrets.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl PodDispatcher for Cluster {
        async fn create(&self, _cluster: &str, _ns: &str, pod: core::Pod) -> Result<core::Pod> {
            Ok(pod)
        }
        async fn await_terminal(
            &self,
            _cluster: &str,
            _ns: &str,
            _name: &str,
            _timeout: Duration,
        ) -> Result<TerminalState> {
            Ok(TerminalState {
                phase: TurnPhase::Succeeded,
                message: None,
            })
        }
        async fn logs(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<String> {
            Ok(String::new())
        }
        async fn delete(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<()> {
            Ok(())
        }
        async fn create_secret(&self, cluster: &str, ns: &str, secret: core::Secret) -> Result<()> {
            self.secrets.lock().unwrap().push((
                cluster.to_string(),
                ns.to_string(),
                secret.metadata.name.unwrap_or_default(),
            ));
            Ok(())
        }
        async fn delete_secret(&self, cluster: &str, ns: &str, name: &str) -> Result<()> {
            if self.refuse_delete.lock().unwrap().as_deref() == Some(name) {
                return Err(crate::runs::workpod::dispatcher::Unsupported::SecretValues.into());
            }
            let gone = (cluster.to_string(), ns.to_string(), name.to_string());
            self.secrets.lock().unwrap().retain(|s| *s != gone);
            Ok(())
        }
    }

    async fn work_pod(pool: &sqlx::PgPool, pod: &str, state: &str) {
        sqlx::query(
            "INSERT INTO work_pods (pod_name, kind, state, cost_tag, created_at, updated_at)
             VALUES ($1, 'run', $2, 'test', 'now', 'now')",
        )
        .bind(pod)
        .bind(state)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn bring(pool: &sqlx::PgPool, cluster: &Cluster, pod: &str) {
        let resolved = pack(PACK).resolve(&mints()).unwrap();
        let all = servers(PROFILE);
        let granted: Vec<&ServerSpec> = all.iter().collect();
        let objects = resolved
            .pull_secret_objects(&granted, &delivery(), pod)
            .unwrap();
        write(pool, cluster, "ocp", pod, objects).await.unwrap();
    }

    async fn recorded(pool: &sqlx::PgPool) -> Vec<(String, String, String)> {
        sqlx::query_as(
            "SELECT pod_name, namespace, name FROM run_pull_secrets
              ORDER BY pod_name, namespace, name",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_runs_tokens_name_its_pull_secrets_to_the_server_that_copies_them(
        pool: sqlx::PgPool,
    ) {
        let cluster = Cluster::default();
        work_pod(&pool, POD, "running").await;
        bring(&pool, &cluster, POD).await;
        assert_eq!(
            cluster.secrets(),
            [
                (
                    "ocp".to_string(),
                    "crucible-kueue".to_string(),
                    format!("{POD}-pull-0")
                ),
                (
                    "ocp".to_string(),
                    "crucible-kueue".to_string(),
                    format!("{POD}-pull-1")
                ),
            ]
        );
        assert_eq!(recorded(&pool).await.len(), 2);

        assert_eq!(
            claim(&pool, POD, "crucible-kueue").await.unwrap(),
            [
                format!("{POD}-pull-0=quay.io/mine"),
                format!("{POD}-pull-1=ghcr.io/mine"),
            ]
        );
        assert!(claim(&pool, POD, "team-a").await.unwrap().is_empty());
        assert!(
            claim(&pool, "crucible-run-other", "crucible-kueue")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn removing_a_runs_pull_secrets_deletes_and_forgets_them(pool: sqlx::PgPool) {
        let cluster = Cluster::default();
        work_pod(&pool, POD, "running").await;
        bring(&pool, &cluster, POD).await;
        assert_eq!(remove(&pool, &cluster, POD).await.unwrap(), 0);
        assert!(cluster.secrets().is_empty());
        assert!(recorded(&pool).await.is_empty());
        assert_eq!(remove(&pool, &cluster, POD).await.unwrap(), 0);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_secret_that_would_not_delete_stays_recorded_for_the_next_sweep(pool: sqlx::PgPool) {
        let cluster = Cluster::default();
        work_pod(&pool, POD, "collected").await;
        bring(&pool, &cluster, POD).await;
        *cluster.refuse_delete.lock().unwrap() = Some(format!("{POD}-pull-1"));
        assert_eq!(sweep(&pool, &cluster).await.unwrap(), 1);
        let left: Vec<String> = recorded(&pool)
            .await
            .into_iter()
            .map(|(_, _, name)| name)
            .collect();
        assert_eq!(left, [format!("{POD}-pull-1")]);
        *cluster.refuse_delete.lock().unwrap() = None;
        assert_eq!(sweep(&pool, &cluster).await.unwrap(), 0);
        assert!(cluster.secrets().is_empty());
        assert!(recorded(&pool).await.is_empty());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn the_sweep_takes_only_the_secrets_of_runs_that_ended(pool: sqlx::PgPool) {
        let cluster = Cluster::default();
        work_pod(&pool, "crucible-run-live", "running").await;
        work_pod(&pool, "crucible-run-done", "collected").await;
        for pod in [
            "crucible-run-live",
            "crucible-run-done",
            "crucible-run-rowless",
        ] {
            bring(&pool, &cluster, pod).await;
        }
        assert_eq!(sweep(&pool, &cluster).await.unwrap(), 0);
        let pods: Vec<String> = recorded(&pool).await.into_iter().map(|r| r.0).collect();
        assert_eq!(pods, ["crucible-run-live"; 2]);
        assert!(
            cluster
                .secrets()
                .iter()
                .all(|(_, _, name)| name.starts_with("crucible-run-live-pull-")),
            "{:?}",
            cluster.secrets()
        );
        assert_eq!(cluster.secrets().len(), 2);
    }
}
