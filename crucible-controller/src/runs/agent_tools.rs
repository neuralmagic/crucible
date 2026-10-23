//! The MCP servers a run's agent is granted by default, from the deploy profile: crucible-kueue on
//! the `[kueue]` clusters and buildit on the `[buildit]` clusters. Dispatch records what a run got,
//! the run page shows it, and a revision of the configured set tells people when it changes.

use anyhow::{Context, Result};
use crucible::deploy::profile::{BuilditCfg, DeployProfile, KueueCfg};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::collections::BTreeMap;

/// A server the agent was granted.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum AgentServer {
    Kueue,
    Buildit,
}

impl AgentServer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kueue => "kueue",
            Self::Buildit => "buildit",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "kueue" => Some(Self::Kueue),
            "buildit" => Some(Self::Buildit),
            _ => None,
        }
    }

    fn scopes(self) -> [&'static str; 2] {
        match self {
            Self::Kueue => [
                crucible_contract::kueue::SCOPE_READ,
                crucible_contract::kueue::SCOPE_SUBMIT,
            ],
            Self::Buildit => [
                crucible_contract::buildit::SCOPE_READ,
                crucible_contract::buildit::SCOPE_BUILD,
            ],
        }
    }
}

/// One server a run's agent can use: where, as whom, with what, within what.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AgentTool {
    pub server: AgentServer,
    pub cluster: String,
    /// The server's MCP URL, the audience of the run's tokens for it.
    pub url: String,
    /// The Kubernetes username the server acts as, when it acts as one and it is known.
    pub kube_user: Option<String>,
    pub scopes: Vec<String>,
    /// What bounds it beyond its scopes: a namespace, a push repository, pull secrets.
    pub limits: BTreeMap<String, String>,
}

impl AgentTool {
    /// `granted kueue/pirate as wseaton (kueue.read kueue.submit)`, the run's event line.
    pub fn summary(&self) -> String {
        let user = match &self.kube_user {
            Some(user) => format!(" as {user}"),
            None => String::new(),
        };
        format!(
            "granted {}/{}{user} ({})",
            self.server.as_str(),
            self.cluster,
            self.scopes.join(" ")
        )
    }
}

fn kueue_tool(cfg: &KueueCfg, cluster: &str, kube_user: Option<&str>) -> AgentTool {
    let mut limits =
        BTreeMap::from([("namespace".to_string(), cfg.workload_namespace.to_string())]);
    if !cfg.pull_secrets.is_empty() {
        limits.insert("pull_secrets".to_string(), cfg.pull_secrets.join(","));
    }
    AgentTool {
        server: AgentServer::Kueue,
        cluster: cluster.to_string(),
        url: cfg.url(),
        kube_user: kube_user.map(str::to_string),
        scopes: AgentServer::Kueue.scopes().map(String::from).to_vec(),
        limits,
    }
}

fn buildit_tool(cfg: &BuilditCfg, cluster: &str) -> AgentTool {
    let limits = cfg
        .push_repo
        .iter()
        .map(|repo| ("push_repo".to_string(), repo.clone()))
        .collect();
    AgentTool {
        server: AgentServer::Buildit,
        cluster: cluster.to_string(),
        url: crucible_contract::buildit::URL.to_string(),
        kube_user: None,
        scopes: AgentServer::Buildit.scopes().map(String::from).to_vec(),
        limits,
    }
}

/// What a run on `cluster` is granted, the launcher acting there as `kube_user`.
pub fn granted(
    kueue: Option<&KueueCfg>,
    buildit: Option<&BuilditCfg>,
    cluster: &str,
    kube_user: Option<&str>,
) -> Vec<AgentTool> {
    let kueue = kueue
        .filter(|k| k.serves(cluster))
        .map(|k| kueue_tool(k, cluster, kube_user));
    let buildit = buildit
        .filter(|b| b.serves(cluster))
        .map(|b| buildit_tool(b, cluster));
    kueue.into_iter().chain(buildit).collect()
}

/// The configured default set, as runs on each cluster get it, and a revision naming it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct Defaults {
    /// A digest of the servers, clusters, scopes and limits; it changes exactly when they do.
    pub revision: String,
    /// One line on what runs now get, for the notice shown when the revision is new.
    pub changelog: String,
    pub tools: Vec<AgentTool>,
}

/// The default set the profile configures; no `kube_user`, which is per person.
pub fn defaults(profile: Option<&DeployProfile>) -> Defaults {
    let kueue = profile.and_then(|p| p.kueue.as_ref());
    let buildit = profile.and_then(|p| p.buildit.as_ref());
    let mut clusters: Vec<&str> = kueue
        .iter()
        .flat_map(|k| k.clusters.iter())
        .chain(buildit.iter().flat_map(|b| b.clusters.iter()))
        .map(String::as_str)
        .collect();
    clusters.sort_unstable();
    clusters.dedup();
    let tools: Vec<AgentTool> = clusters
        .iter()
        .flat_map(|cluster| granted(kueue, buildit, cluster, None))
        .collect();
    let canonical = serde_json::to_string(&tools).unwrap_or_default();
    let revision = format!("{:x}", Sha256::digest(canonical.as_bytes()))[..16].to_string();
    Defaults {
        revision,
        changelog: changelog(&tools),
        tools,
    }
}

fn changelog(tools: &[AgentTool]) -> String {
    let mut by_server: BTreeMap<AgentServer, Vec<&str>> = BTreeMap::new();
    for t in tools {
        by_server.entry(t.server).or_default().push(&t.cluster);
    }
    if by_server.is_empty() {
        return "Runs get no MCP servers by default.".to_string();
    }
    let parts: Vec<String> = by_server
        .iter()
        .map(|(server, clusters)| format!("{} on {}", server.as_str(), clusters.join(", ")))
        .collect();
    format!("Runs now include {} tools.", parts.join(" and "))
}

/// Record what run `run_id` was granted, replacing any earlier record of it.
pub async fn record(pool: &PgPool, run_id: &str, tools: &[AgentTool]) -> Result<()> {
    let now = crate::clock::now_rfc3339();
    let mut tx = pool
        .begin()
        .await
        .context("opening the agent tools write")?;
    sqlx::query("DELETE FROM run_agent_tools WHERE run_id = $1")
        .bind(run_id)
        .execute(&mut *tx)
        .await
        .context("clearing the run's agent tools")?;
    for t in tools {
        sqlx::query(
            "INSERT INTO run_agent_tools
                 (run_id, server, cluster, url, kube_user, scopes, limits, granted_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(run_id)
        .bind(t.server.as_str())
        .bind(&t.cluster)
        .bind(&t.url)
        .bind(t.kube_user.as_deref())
        .bind(sqlx::types::Json(&t.scopes))
        .bind(sqlx::types::Json(&t.limits))
        .bind(&now)
        .execute(&mut *tx)
        .await
        .context("recording a run's agent tool")?;
    }
    tx.commit()
        .await
        .context("committing the run's agent tools")
}

/// What run `run_id` was granted, by server.
pub async fn for_run(pool: &PgPool, run_id: &str) -> Result<Vec<AgentTool>> {
    type Row = (
        String,
        String,
        String,
        Option<String>,
        sqlx::types::Json<Vec<String>>,
        sqlx::types::Json<BTreeMap<String, String>>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT server, cluster, url, kube_user, scopes, limits FROM run_agent_tools
          WHERE run_id = $1",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await
    .context("reading the run's agent tools")?;
    let mut tools: Vec<AgentTool> = rows
        .into_iter()
        .filter_map(|(server, cluster, url, kube_user, scopes, limits)| {
            Some(AgentTool {
                server: AgentServer::parse(&server)?,
                cluster,
                url,
                kube_user,
                scopes: scopes.0,
                limits: limits.0,
            })
        })
        .collect();
    tools.sort_by_key(|t| t.server);
    Ok(tools)
}

/// Record what run `run_id`, launched by `launcher` onto `cluster`, is granted, and say so on the
/// issue's event log, one line per server. The launcher's username on the cluster is resolved by
/// the profile's rule for it, as the run's tokens will carry it.
pub async fn grant(
    db: &crate::Db,
    profile: &DeployProfile,
    issue_key: &str,
    run_id: &str,
    cluster: &str,
    launcher: Option<&str>,
) -> Result<Vec<AgentTool>> {
    let kube_user = match launcher {
        Some(login) => {
            match crate::identity::cluster_identity::for_login(db.pool(), profile, cluster, login)
                .await?
            {
                crate::identity::cluster_identity::Resolution::Known { user, .. } => {
                    Some(user.as_str().to_string())
                }
                _ => None,
            }
        }
        None => None,
    };
    let tools = granted(
        profile.kueue.as_ref(),
        profile.buildit.as_ref(),
        cluster,
        kube_user.as_deref(),
    );
    record(db.pool(), run_id, &tools).await?;
    let status = crate::issues::store::get_issue(db.pool(), issue_key)
        .await?
        .map(|i| i.status.as_str().to_string())
        .unwrap_or_else(|| "new".to_string());
    for tool in &tools {
        let reason = tool.summary();
        db.events()
            .append(&crate::event_log::Event::now(
                issue_key,
                &status,
                &status,
                Some(&reason),
                Some(run_id),
            ))
            .await?;
    }
    Ok(tools)
}

#[cfg(test)]
mod tests {
    use crate::runs::agent_tools::*;

    const PROFILE: &str = r#"
        [cluster]
        loop_namespace = "crucible-system"
        rig_namespace = "rig"
        service_account = "crucible-spoke"
        supervisor_image = "registry.example.com/openshell-supervisor:latest"
        [image]
        loop = "registry.example.com/crucible-loop:latest"
        pull_secret = "quay-pull"
        [clusters.waldorf]
        kubeconfig_secret = "waldorf-kubeconfig"
        identity = { claim = "email", prefix = "oidc:" }
        [kueue]
        image = "registry.example.com/crucible-kueue:latest"
        clusters = ["pirate", "waldorf"]
        namespace = "crucible-system"
        workload_namespace = "crucible-kueue-e2e"
        run_issuer = "https://crucible.hub.example"
        kube_authz = "openshift"
        pull_secrets = ["quay-pull"]
        [buildit]
        clusters = ["waldorf"]
        run_issuer = "https://crucible.hub.example"
        push_repo = "quay.io/org/smoke"
    "#;

    fn profile(text: &str) -> DeployProfile {
        toml::from_str(text).expect("profile parses")
    }

    #[test]
    fn a_run_gets_the_servers_its_cluster_runs_with_their_scopes_and_limits() {
        let p = profile(PROFILE);
        let on_waldorf = granted(
            p.kueue.as_ref(),
            p.buildit.as_ref(),
            "waldorf",
            Some("oidc:alice@example.com"),
        );
        assert_eq!(
            on_waldorf.iter().map(|t| t.server).collect::<Vec<_>>(),
            [AgentServer::Kueue, AgentServer::Buildit]
        );
        let kueue = &on_waldorf[0];
        assert_eq!(kueue.kube_user.as_deref(), Some("oidc:alice@example.com"));
        assert_eq!(kueue.scopes, ["kueue.read", "kueue.submit"]);
        assert_eq!(kueue.limits["namespace"], "crucible-kueue-e2e");
        assert_eq!(kueue.limits["pull_secrets"], "quay-pull");
        let buildit = &on_waldorf[1];
        assert_eq!(buildit.url, "http://host.openshell.internal:8848/mcp");
        assert_eq!(
            buildit.kube_user, None,
            "buildit acts as no Kubernetes user"
        );
        assert_eq!(buildit.limits["push_repo"], "quay.io/org/smoke");
        assert_eq!(
            kueue.summary(),
            "granted kueue/waldorf as oidc:alice@example.com (kueue.read kueue.submit)"
        );
        assert_eq!(
            buildit.summary(),
            "granted buildit/waldorf (buildit.read buildit.build)"
        );

        let on_pirate = granted(p.kueue.as_ref(), p.buildit.as_ref(), "pirate", None);
        assert_eq!(on_pirate.len(), 1, "buildit serves waldorf only");
        assert!(granted(p.kueue.as_ref(), p.buildit.as_ref(), "hub", None).is_empty());
    }

    #[test]
    fn the_defaults_revision_changes_exactly_when_the_granted_set_does() {
        let base = defaults(Some(&profile(PROFILE)));
        assert_eq!(base, defaults(Some(&profile(PROFILE))), "stable");
        assert_eq!(base.revision.len(), 16);
        assert_eq!(
            base.changelog,
            "Runs now include kueue on pirate, waldorf and buildit on waldorf tools."
        );
        let reworded = PROFILE.replace(
            "image = \"registry.example.com/crucible-kueue:latest\"",
            "image = \"registry.example.com/crucible-kueue:v2\"",
        );
        assert_eq!(
            defaults(Some(&profile(&reworded))).revision,
            base.revision,
            "a new server image grants nothing new"
        );
        for changed in [
            PROFILE.replace(
                "clusters = [\"waldorf\"]",
                "clusters = [\"pirate\", \"waldorf\"]",
            ),
            PROFILE.replace(
                "push_repo = \"quay.io/org/smoke\"",
                "push_repo = \"quay.io/org/other\"",
            ),
            PROFILE.replace("pull_secrets = [\"quay-pull\"]", ""),
        ] {
            assert_ne!(
                defaults(Some(&profile(&changed))).revision,
                base.revision,
                "{changed}"
            );
        }
        let none = defaults(None);
        assert!(none.tools.is_empty());
        assert_eq!(none.changelog, "Runs get no MCP servers by default.");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_grant_is_recorded_for_the_run_and_told_on_the_issue(pool: PgPool) {
        let db = crate::Db::new(pool.clone());
        crate::identity::oidc::users::record_login(
            &pool,
            "sub-alice",
            "alice",
            Some("alice@example.com"),
            jiff::Timestamp::now(),
        )
        .await
        .unwrap();
        let p = profile(PROFILE);
        let tools = grant(&db, &p, "owner/repo#1", "run-1", "waldorf", Some("alice"))
            .await
            .unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(
            for_run(&pool, "run-1").await.unwrap(),
            tools,
            "read back as recorded"
        );
        assert_eq!(
            tools[0].kube_user.as_deref(),
            Some("oidc:alice@example.com"),
            "the username waldorf's rule gives alice"
        );
        let events = db.events().read_for_key("owner/repo#1").await.unwrap();
        let reasons: Vec<&str> = events.iter().filter_map(|e| e.reason.as_deref()).collect();
        assert_eq!(
            reasons,
            [
                "granted kueue/waldorf as oidc:alice@example.com (kueue.read kueue.submit)",
                "granted buildit/waldorf (buildit.read buildit.build)"
            ]
        );
        assert!(
            events
                .iter()
                .all(|e| e.evidence.as_deref() == Some("run-1"))
        );

        let again = grant(&db, &p, "owner/repo#1", "run-1", "pirate", None)
            .await
            .unwrap();
        assert_eq!(
            for_run(&pool, "run-1").await.unwrap(),
            again,
            "a redispatch replaces it"
        );
        assert_eq!(again[0].kube_user, None, "no launcher, no username");
        assert!(for_run(&pool, "run-2").await.unwrap().is_empty());
    }
}
