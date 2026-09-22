//! What substrate a pack's agent needs, and what this deployment can actually give it.
//!
//! A pack declares its agent backend and sandbox image in `[agent]`; whether either can be
//! dispatched is the deployment's, not the pack's. The capability is derived from configuration
//! that already exists — `CONTROLLER_DEPLOY_PROFILE` is what a work-pod render needs, and
//! `CONTROLLER_PLAYBOOK_EXECUTOR` is the local-mode opt-in — so there is no second place to
//! configure where a run can go.
//!
//! [`DispatchCapability::refusal`] is the one verdict: the preview gate and the import review show
//! it as a warning, and the launch endpoints refuse on it.

use crate::config::{ControllerCfg, PlaybookExecutor};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// The agent substrate a pack declares. `backend` defaults to the engine's own default when the
/// manifest omits it, so what is stored is what the engine will do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackAgent {
    pub backend: String,
    pub sandbox_image: Option<String>,
    /// The `[agent]` harness as written (`claude`, `hermes`, `codex`), when the manifest names one.
    pub harness: Option<String>,
    /// `[agent.requires]`: predicate -> version range the sandbox image must satisfy.
    pub requires: BTreeMap<String, String>,
    /// `[agent.prefers]`: predicate -> version range that ranks compatible images.
    pub prefers: BTreeMap<String, String>,
    /// `[agent] allow_unverified_image = true`: launch on an image the catalog does not know or
    /// that carries no capability document, instead of being refused.
    pub allow_unverified_image: bool,
}

/// The `[agent]` fields beyond backend and image, as the `agent_requirements` column stores them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRequirements {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub requires: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub prefers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_unverified_image: bool,
}

impl PackAgent {
    /// A backend and image with nothing else declared.
    pub fn new(backend: impl Into<String>, sandbox_image: Option<String>) -> Self {
        Self::from_columns(backend.into(), sandbox_image, None)
    }

    /// Rebuild from the stored columns; a NULL `agent_requirements` is a row written before the
    /// column existed, which declared nothing.
    pub fn from_columns(
        backend: String,
        sandbox_image: Option<String>,
        requirements: Option<AgentRequirements>,
    ) -> Self {
        let r = requirements.unwrap_or_default();
        Self {
            backend,
            sandbox_image,
            harness: r.harness,
            requires: r.requires,
            prefers: r.prefers,
            allow_unverified_image: r.allow_unverified_image,
        }
    }

    /// The `agent_requirements` column value for this agent.
    fn requirements(&self) -> AgentRequirements {
        AgentRequirements {
            harness: self.harness.clone(),
            requires: self.requires.clone(),
            prefers: self.prefers.clone(),
            allow_unverified_image: self.allow_unverified_image,
        }
    }

    /// The `agent_requirements` column as JSON, for an insert.
    pub fn requirements_json(&self) -> serde_json::Value {
        serde_json::to_value(self.requirements()).unwrap_or(serde_json::Value::Null)
    }
}

/// The `[agent]` substrate stored on a row's `agent_*` columns, `None` when it declares no backend.
pub(crate) fn agent_from_row(row: &sqlx::postgres::PgRow) -> Result<Option<PackAgent>> {
    use sqlx::Row as _;
    Ok(match row.try_get::<Option<String>, _>("agent_backend")? {
        Some(backend) => Some(PackAgent::from_columns(
            backend,
            row.try_get("agent_sandbox_image")?,
            requirements_from_row(row)?,
        )),
        None => None,
    })
}

/// Decode the `agent_requirements` JSONB column of a row.
fn requirements_from_row(row: &sqlx::postgres::PgRow) -> Result<Option<AgentRequirements>> {
    use sqlx::Row as _;
    let value: Option<serde_json::Value> = row.try_get("agent_requirements")?;
    value
        .map(serde_json::from_value)
        .transpose()
        .context("decoding stored agent requirements")
}

/// The engine's default when `[agent]` declares no backend (`crucible::manifest::default_backend`).
const DEFAULT_BACKEND: &str = "local";

/// The backends the engine takes. A manifest naming anything else is refused by the engine at run
/// time, so it is refused here instead — at launch, where someone is watching.
const KNOWN_BACKENDS: [&str; 3] = ["local", "openshell", "command"];

/// The backend that runs each turn in an OpenShell sandbox, launched from `sandbox_image`.
const SANDBOX_BACKEND: &str = "openshell";

#[derive(Debug, Deserialize)]
struct ManifestAgent {
    agent: Option<AgentTable>,
}

#[derive(Debug, Default, Deserialize)]
struct AgentTable {
    backend: Option<String>,
    sandbox_image: Option<String>,
    harness: Option<String>,
    #[serde(default)]
    requires: BTreeMap<String, String>,
    #[serde(default)]
    prefers: BTreeMap<String, String>,
    #[serde(default)]
    allow_unverified_image: bool,
}

/// Read a pack tree's `[agent]` backend and sandbox image. `Err` is a manifest that is not there or
/// does not parse; a manifest with no `[agent]` table is the engine's default backend and no image.
pub fn pack_agent(pack_root: &Path) -> Result<PackAgent> {
    let manifest = pack_root.join("crucible.toml");
    let text = std::fs::read_to_string(&manifest)
        .with_context(|| format!("reading {}", manifest.display()))?;
    let parsed: ManifestAgent =
        toml::from_str(&text).with_context(|| format!("parsing {}", manifest.display()))?;
    let agent = parsed.agent.unwrap_or_default();
    Ok(PackAgent {
        backend: agent
            .backend
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| DEFAULT_BACKEND.to_string()),
        sandbox_image: agent
            .sandbox_image
            .map(|i| i.trim().to_string())
            .filter(|i| !i.is_empty()),
        harness: agent
            .harness
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty()),
        requires: agent.requires,
        prefers: agent.prefers,
        allow_unverified_image: agent.allow_unverified_image,
    })
}

/// What this deployment can dispatch a playbook onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchCapability {
    executor: PlaybookExecutor,
    /// Whether a work-pod render has the deploy profile it needs — the controller's own answer to
    /// "can we reach a cluster", since a dispatch without one fails at render.
    cluster: bool,
}

impl DispatchCapability {
    /// The capability spelled directly: `cluster` is whether a work-pod render has its deploy
    /// profile.
    pub fn new(executor: PlaybookExecutor, cluster: bool) -> Self {
        DispatchCapability { executor, cluster }
    }

    pub fn from_cfg(cfg: &ControllerCfg) -> Self {
        DispatchCapability {
            executor: cfg.playbook_executor,
            cluster: cfg.deploy_profile.is_some(),
        }
    }

    /// Whether launches run as a supervised subprocess on this machine.
    pub fn is_local(&self) -> bool {
        self.executor == PlaybookExecutor::Local
    }

    /// Why this deployment cannot dispatch `agent`, or `None` when it can. The message names the
    /// pack's declaration and the deployment's capability, because whoever reads it has to change
    /// one of the two.
    pub fn refusal(&self, agent: &PackAgent) -> Option<String> {
        let backend = agent.backend.as_str();
        if !KNOWN_BACKENDS.contains(&backend) {
            return Some(format!(
                "the pack declares [agent] backend {backend:?}, which the engine does not take \
                 (local, openshell or command)"
            ));
        }
        match self.executor {
            PlaybookExecutor::Local => None,
            PlaybookExecutor::Pod if !self.cluster => Some(
                "this deployment cannot dispatch a work pod (no CONTROLLER_DEPLOY_PROFILE) and \
                 local dispatch is off (CONTROLLER_PLAYBOOK_EXECUTOR=pod)"
                    .to_string(),
            ),
            PlaybookExecutor::Pod => None,
        }
    }

    /// Why an agent turn declared like this cannot be spawned where this deployment would dispatch
    /// it, or `None`. Asked where a pack is written; [`Self::refusal`] is the launch verdict.
    pub fn spawn_defect(&self, agent: &PackAgent) -> Option<SpawnDefect> {
        match agent.backend.as_str() {
            SANDBOX_BACKEND => agent
                .sandbox_image
                .is_none()
                .then_some(SpawnDefect::NoSandboxImage),
            DEFAULT_BACKEND if self.executor == PlaybookExecutor::Pod => {
                Some(SpawnDefect::InProcessBackend {
                    backend: agent.backend.clone(),
                })
            }
            _ => None,
        }
    }
}

/// Why an agent turn a pack declares cannot be spawned where this deployment would dispatch it.
/// Read at authoring time by [`DispatchCapability::spawn_defect`]; the text names the manifest
/// field whose absence produces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnDefect {
    /// A backend that runs the agent as a child of the loop process. The loop image carries no
    /// agent CLI, so the spawn fails inside a dispatched pod.
    InProcessBackend { backend: String },
    /// `openshell` with no image to launch the turn in.
    NoSandboxImage,
}

impl std::fmt::Display for SpawnDefect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnDefect::InProcessBackend { backend } => write!(
                f,
                "[agent] backend {backend:?} runs the agent as a child of the loop process, and \
                 the loop image this deployment dispatches carries no agent CLI; a work-pod \
                 launch needs backend = \"openshell\" and an [agent] sandbox_image to run the \
                 turn in"
            ),
            SpawnDefect::NoSandboxImage => write!(
                f,
                "the pack declares no [agent] sandbox_image, and backend \"openshell\" has no \
                 image to launch the agent turn in"
            ),
        }
    }
}

/// The tables holding a pack tarball and the substrate read off it, keyed for the backfill below.
const AGENT_TABLES: [(&str, &str); 3] = [
    ("playbooks", "id"),
    ("pack_imports", "id"),
    ("playbook_draft_versions", "draft_id, version"),
];

/// Read the `[agent]` substrate off every stored pack that has none recorded, and stamp it. Rows
/// written before the columns existed carry no substrate, and refusing to launch them for that
/// reason would be a lie about the pack. Runs once at startup, under the maintenance lock, beside
/// the schema re-derivation. Returns how many rows were filled.
pub async fn backfill_pack_agents(pool: &sqlx::PgPool) -> Result<usize> {
    use sqlx::Row as _;
    let mut filled = 0usize;
    for (table, key) in AGENT_TABLES {
        let rows = sqlx::query(&format!(
            "SELECT {key}, tar_gz FROM {table} WHERE agent_backend IS NULL OR agent_requirements IS NULL"
        ))
        .fetch_all(pool)
        .await
        .with_context(|| format!("listing {table} rows with no recorded agent"))?;
        for row in rows {
            let tar_gz: Vec<u8> = row.try_get("tar_gz")?;
            let agent = tokio::task::spawn_blocking(move || {
                let pack = crate::playbooks::packs::unpack_to_scratch(&tar_gz)?;
                pack_agent(pack.path())
            })
            .await
            .context("joining the pack agent backfill worker")?;
            let Ok(agent) = agent else {
                continue;
            };
            let sql = format!(
                "UPDATE {table} SET agent_backend = $1, agent_sandbox_image = $2, agent_requirements = $3 WHERE {}",
                if table == "playbook_draft_versions" {
                    "draft_id = $4 AND version = $5"
                } else {
                    "id = $4"
                }
            );
            let update = sqlx::query(&sql)
                .bind(&agent.backend)
                .bind(agent.sandbox_image.as_deref())
                .bind(agent.requirements_json());
            let update = if table == "playbook_draft_versions" {
                update
                    .bind(row.try_get::<String, _>("draft_id")?)
                    .bind(row.try_get::<i64, _>("version")?)
            } else {
                update.bind(row.try_get::<String, _>("id")?)
            };
            update
                .execute(pool)
                .await
                .with_context(|| format!("stamping the agent substrate on a {table} row"))?;
            filled += 1;
        }
    }
    Ok(filled)
}

/// Recover the dispatch location of runs written before `runs.cluster` existed. The work-pod
/// ledger already recorded the cluster of every dispatched run, so the join is the run's own pod
/// name; the namespace is asked of the cluster, because a spoke's comes from its kubeconfig
/// context and nothing in the database holds it.
///
/// Rows the ledger cannot answer for keep the `hub` default the migration gave them: a local-mode
/// run has no pod, and an externally uploaded run never had one. A cluster that will not answer
/// for its namespace leaves the cluster stamped and the namespace NULL rather than failing the
/// boot — the relay's fallback reads a namespace it can also resolve at request time.
///
/// Runs once at startup under the maintenance lock, beside [`backfill_pack_agents`]. Returns how
/// many runs were relocated.
pub async fn backfill_run_locations(
    pool: &sqlx::PgPool,
    clusters: &crate::runs::clusters::ClusterClients,
    hub_namespace: &str,
) -> Result<usize> {
    let stale: Vec<(String, String)> = sqlx::query_as(
        r#"
        SELECT r.run_id, w.cluster
        FROM runs r
        JOIN work_pods w ON w.pod_name = r.pod
        WHERE r.namespace IS NULL
        "#,
    )
    .fetch_all(pool)
    .await
    .context("listing runs whose dispatch location is unrecorded")?;

    // One namespace lookup per distinct cluster, not per run: a spoke lookup builds a kube client.
    let mut namespaces: HashMap<String, Option<String>> = HashMap::new();
    let mut filled = 0usize;
    for (run_id, cluster) in stale {
        let namespace = match namespaces.get(&cluster) {
            Some(ns) => ns.clone(),
            None => {
                let ns = clusters
                    .pod_namespace(&cluster, hub_namespace)
                    .await
                    .map_err(|e| {
                        tracing::warn!(
                            cluster,
                            error = %format!("{e:#}"),
                            "backfill: cluster would not answer for its namespace; recording the \
                             cluster alone"
                        );
                    })
                    .ok();
                namespaces.insert(cluster.clone(), ns.clone());
                ns
            }
        };
        sqlx::query("UPDATE runs SET cluster = $2, namespace = $3 WHERE run_id = $1")
            .bind(&run_id)
            .bind(&cluster)
            .bind(namespace.as_deref())
            .execute(pool)
            .await
            .with_context(|| format!("relocating run {run_id} onto cluster {cluster}"))?;
        filled += 1;
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use crate::playbooks::dispatch::*;

    fn agent(backend: &str) -> PackAgent {
        PackAgent::new(backend.to_string(), None)
    }

    fn capability(executor: PlaybookExecutor, cluster: bool) -> DispatchCapability {
        DispatchCapability::new(executor, cluster)
    }

    #[test]
    fn a_manifest_without_an_agent_table_reads_as_the_engines_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("crucible.toml"),
            "[workflow]\ntype = \"playbook\"\nfile = \"w.star\"\n",
        )
        .expect("manifest");
        assert_eq!(
            pack_agent(dir.path()).expect("parses"),
            PackAgent::new("local".to_string(), None)
        );
    }

    #[test]
    fn a_declared_backend_and_image_are_read_verbatim() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("crucible.toml"),
            "[agent]\nbackend = \"openshell\"\nsandbox_image = \"quay.io/x/y:tag\"\n",
        )
        .expect("manifest");
        assert_eq!(
            pack_agent(dir.path()).expect("parses"),
            PackAgent::new("openshell".to_string(), Some("quay.io/x/y:tag".to_string()))
        );
    }

    #[test]
    fn requires_prefers_harness_and_the_override_are_read_and_round_trip_the_column() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("crucible.toml"),
            concat!(
                "[agent]\nbackend = \"openshell\"\nharness = \"codex\"\n",
                "sandbox_image = \"quay.io/x/y:tag\"\nallow_unverified_image = true\n\n",
                "[agent.requires]\n\"toolchain.go\" = \">=1.25\"\n\n",
                "[agent.prefers]\n\"toolchain.go\" = \">=1.26\"\n",
            ),
        )
        .expect("manifest");
        let agent = pack_agent(dir.path()).expect("parses");
        assert_eq!(agent.harness.as_deref(), Some("codex"));
        assert_eq!(agent.requires["toolchain.go"], ">=1.25");
        assert_eq!(agent.prefers["toolchain.go"], ">=1.26");
        assert!(agent.allow_unverified_image);

        let json = agent.requirements_json();
        let back: AgentRequirements = serde_json::from_value(json).expect("decodes");
        assert_eq!(
            PackAgent::from_columns(
                agent.backend.clone(),
                agent.sandbox_image.clone(),
                Some(back)
            ),
            agent
        );
        // A row written before the column existed declares nothing.
        let legacy = PackAgent::from_columns("local".into(), None, None);
        assert_eq!(legacy, PackAgent::new("local", None));
        assert_eq!(legacy.requirements_json(), serde_json::json!({}));
    }

    #[test]
    fn a_missing_manifest_is_an_error_not_a_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(pack_agent(dir.path()).is_err());
    }

    /// The laptop case the whole surface exists for: local mode runs every backend the engine
    /// takes, `openshell` included, since the engine boots its sandbox on the host's podman.
    #[test]
    fn local_mode_takes_every_backend() {
        let cap = capability(PlaybookExecutor::Local, false);
        for backend in KNOWN_BACKENDS {
            assert_eq!(cap.refusal(&agent(backend)), None, "{backend}");
        }
    }

    /// Pod mode with no deploy profile dispatches nothing at all: the render it would need has no
    /// profile to read, which is exactly what the failed laptop launch discovered at reconcile.
    #[test]
    fn pod_mode_without_a_profile_refuses_every_backend() {
        let cap = capability(PlaybookExecutor::Pod, false);
        for backend in KNOWN_BACKENDS {
            let refusal = cap.refusal(&agent(backend)).expect("refused");
            assert!(refusal.contains("CONTROLLER_DEPLOY_PROFILE"), "{refusal}");
        }
        let cap = capability(PlaybookExecutor::Pod, true);
        for backend in KNOWN_BACKENDS {
            assert_eq!(cap.refusal(&agent(backend)), None, "{backend}");
        }
    }

    /// A pack stored before the substrate was recorded reads its own manifest at startup instead
    /// of being refused for want of a stamp.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn the_backfill_reads_the_substrate_off_a_stored_tarball(
        pool: sqlx::PgPool,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("crucible.toml"),
            "[workflow]\ntype = \"playbook\"\nfile = \"w.star\"\n\n\
             [agent]\nbackend = \"openshell\"\nsandbox_image = \"quay.io/x/y:tag\"\n",
        )?;
        std::fs::write(dir.path().join("w.star"), "params = {}\n")?;
        let tar_gz = crate::playbooks::packs::tar_pack_tree(dir.path())?;
        sqlx::query(
            r#"INSERT INTO playbooks (id, description, repo, git_ref, rev, path, tar_gz,
                                      tar_digest, tar_bytes, params_schema, schema_digest,
                                      core_rev, created_at, updated_at)
               VALUES ('old', 'stored before the columns', 'owner/repo', NULL, 'deadbeef', '', $1,
                       'sha256:tar', $2, '{}'::jsonb, 'sha256:form', 'deadbeef',
                       '2026-08-22T00:00:00Z', '2026-08-22T00:00:00Z')"#,
        )
        .bind(&tar_gz)
        .bind(tar_gz.len() as i64)
        .execute(&pool)
        .await?;

        assert_eq!(backfill_pack_agents(&pool).await?, 1);
        let row = crate::playbooks::registry::get(&pool, "old")
            .await?
            .expect("row");
        assert_eq!(
            row.agent,
            Some(PackAgent::new(
                "openshell".to_string(),
                Some("quay.io/x/y:tag".to_string())
            ))
        );
        assert_eq!(
            backfill_pack_agents(&pool).await?,
            0,
            "a stamped row is not re-read"
        );
        Ok(())
    }

    /// The relay's bug, at the database layer: a run dispatched to a spoke read back as `hub`
    /// because nothing recorded where it went. The work-pod ledger already knew, so the backfill
    /// recovers it rather than asking an operator to.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn the_backfill_relocates_a_run_onto_the_cluster_its_pod_ran_on(
        pool: sqlx::PgPool,
    ) -> anyhow::Result<()> {
        sqlx::query(
            r#"INSERT INTO runs (run_id, status, pod) VALUES
                 ('run-spoke', 'running', 'crucible-run-spoke'),
                 ('run-hub', 'running', 'crucible-run-hub'),
                 ('run-local', 'running', NULL)"#,
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"INSERT INTO work_pods (pod_name, kind, state, cost_tag, created_at, updated_at, cluster)
               VALUES ('crucible-run-spoke', 'run', 'running', 'run', '2026-08-26T00:00:00Z',
                       '2026-08-26T00:00:00Z', 'wharf'),
                      ('crucible-run-hub', 'run', 'running', 'run', '2026-08-26T00:00:00Z',
                       '2026-08-26T00:00:00Z', 'hub')"#,
        )
        .execute(&pool)
        .await?;

        // No clusters directory: the hub answers for its namespace, the spoke cannot be reached,
        // and the spoke's cluster is still recorded rather than the whole backfill failing.
        let clusters = crate::runs::clusters::ClusterClients::new(None);
        assert_eq!(
            backfill_run_locations(&pool, &clusters, "autoresearch").await?,
            2,
            "the local run has no pod, so the ledger cannot answer for it"
        );

        let row: (String, Option<String>) =
            sqlx::query_as("SELECT cluster, namespace FROM runs WHERE run_id = 'run-spoke'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(row.0, "wharf", "relocated onto the cluster its pod ran on");
        assert_eq!(
            row.1, None,
            "an unreachable spoke leaves the namespace unrecorded"
        );

        let row: (String, Option<String>) =
            sqlx::query_as("SELECT cluster, namespace FROM runs WHERE run_id = 'run-hub'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(row.0, "hub");
        assert_eq!(
            row.1.as_deref(),
            Some("autoresearch"),
            "the hub answers for its own namespace"
        );

        let row: (String, Option<String>) =
            sqlx::query_as("SELECT cluster, namespace FROM runs WHERE run_id = 'run-local'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            row.0, "hub",
            "a run with no pod keeps the migration default"
        );

        // The hub run is now stamped, so a second boot re-reads only what is still unrecorded.
        assert_eq!(
            backfill_run_locations(&pool, &clusters, "autoresearch").await?,
            1,
            "the spoke row stays unstamped while its cluster cannot answer"
        );
        Ok(())
    }

    /// The manifest the whole check exists for: an `[agent]` table declaring nothing reads as the
    /// engine's `local` default with no image, which a work-pod launch cannot spawn. The message
    /// has to name both fields the writer is missing.
    #[test]
    fn a_pack_declaring_no_substrate_cannot_spawn_a_turn_in_a_pod() {
        let cap = capability(PlaybookExecutor::Pod, true);
        let defect = cap.spawn_defect(&agent("local")).expect("defect");
        assert_eq!(
            defect,
            SpawnDefect::InProcessBackend {
                backend: "local".to_string()
            }
        );
        let message = defect.to_string();
        assert!(message.contains("sandbox_image"), "{message}");
        assert!(message.contains("openshell"), "{message}");
    }

    #[test]
    fn openshell_without_an_image_names_the_field_it_is_missing() {
        let cap = capability(PlaybookExecutor::Pod, true);
        let defect = cap.spawn_defect(&agent("openshell")).expect("defect");
        assert_eq!(defect, SpawnDefect::NoSandboxImage);
        assert!(defect.to_string().contains("sandbox_image"));

        let complete = PackAgent::new(
            "openshell".to_string(),
            Some("ghcr.io/org/sandbox:latest".to_string()),
        );
        assert_eq!(cap.spawn_defect(&complete), None);
    }

    /// A command backend brings its own process, and local mode spawns on the machine the
    /// controller runs on, where a `local` agent is the point. An `openshell` turn needs its image
    /// wherever it runs.
    #[test]
    fn a_command_backend_and_local_mode_earn_no_spawn_defect() {
        assert_eq!(
            capability(PlaybookExecutor::Pod, true).spawn_defect(&agent("command")),
            None
        );
        let local = capability(PlaybookExecutor::Local, false);
        assert_eq!(local.spawn_defect(&agent("local")), None);
        assert_eq!(local.spawn_defect(&agent("command")), None);
        assert_eq!(
            local.spawn_defect(&agent("openshell")),
            Some(SpawnDefect::NoSandboxImage)
        );
        let complete = PackAgent::new(
            "openshell".to_string(),
            Some("localhost/sandbox:dev".to_string()),
        );
        assert_eq!(local.spawn_defect(&complete), None);
    }

    /// The spawn check is the authoring path's alone. Every pack the launch path already accepts
    /// keeps being accepted, including the NULL-image rows a backfill stamped.
    #[test]
    fn a_spawn_defect_is_not_a_launch_refusal() {
        let cap = capability(PlaybookExecutor::Pod, true);
        for backend in KNOWN_BACKENDS {
            assert_eq!(cap.refusal(&agent(backend)), None, "{backend}");
        }
    }

    #[test]
    fn a_backend_the_engine_does_not_take_is_refused_before_it_is_dispatched() {
        for cap in [
            capability(PlaybookExecutor::Pod, true),
            capability(PlaybookExecutor::Local, false),
        ] {
            let refusal = cap.refusal(&agent("kubernetes")).expect("refused");
            assert!(refusal.contains("kubernetes"), "{refusal}");
        }
    }
}
