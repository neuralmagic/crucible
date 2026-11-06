//! The MCP servers a run's agent is granted by default, from the deploy profile's server
//! descriptions ([`DeployProfile::agent_servers`]), where the issuer mints their tokens and with
//! the scopes it mints. Dispatch records what a run got, as the run's Kubernetes identity, the run
//! page shows it, and a revision of the configured set tells people when it changes.

use crate::runs::workpod::agent_servers::AgentTokens;
use anyhow::{Context, Result};
use crucible::deploy::servers::ServerSpec;
use crucible_contract::agent_servers::ServerName;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::collections::BTreeMap;

/// One server a run's agent can use: where, as whom, with what, within what.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AgentTool {
    /// The server's name, its `.mcp.json` key.
    #[schema(value_type = String)]
    pub server: ServerName,
    pub cluster: String,
    /// The server's MCP URL, the audience of the run's tokens for it.
    pub url: String,
    /// The Kubernetes username the run's tokens for the server carry, `crucible:run:<pod>`.
    pub kube_user: Option<String>,
    pub scopes: Vec<String>,
    /// What bounds it beyond its scopes: a namespace, a push repository, pull secrets.
    pub limits: BTreeMap<String, String>,
}

impl AgentTool {
    /// `granted kueue/ocp as casey (kueue.read kueue.submit)`, the run's event line.
    pub fn summary(&self) -> String {
        let user = match &self.kube_user {
            Some(user) => format!(" as {user}"),
            None => String::new(),
        };
        format!(
            "granted {}/{}{user} ({})",
            self.server,
            self.cluster,
            self.scopes.join(" ")
        )
    }
}

/// What a run on `cluster` is granted, by server name: each server the profile describes there
/// whose tokens the issuer mints, with the scopes it mints them with.
pub fn granted(servers: &[ServerSpec], cluster: &str, tokens: &AgentTokens) -> Vec<AgentTool> {
    let mut tools: Vec<AgentTool> =
        crate::runs::workpod::agent_servers::granted(servers, cluster, tokens)
            .into_iter()
            .map(|s| AgentTool {
                server: s.name.clone(),
                cluster: cluster.to_string(),
                url: s.url.clone(),
                kube_user: None,
                scopes: s.scopes.clone(),
                limits: s.limits.clone(),
            })
            .collect();
    tools.sort_by(|a, b| a.server.cmp(&b.server));
    tools
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
pub fn defaults(servers: &[ServerSpec], tokens: Option<&AgentTokens>) -> Defaults {
    let mut clusters: Vec<&str> = servers
        .iter()
        .flat_map(|s| s.clusters.iter())
        .map(String::as_str)
        .collect();
    clusters.sort_unstable();
    clusters.dedup();
    let tools: Vec<AgentTool> = match tokens {
        Some(tokens) => clusters
            .iter()
            .flat_map(|cluster| granted(servers, cluster, tokens))
            .collect(),
        None => Vec::new(),
    };
    let canonical = serde_json::to_string(&tools).unwrap_or_default();
    let revision = format!("{:x}", Sha256::digest(canonical.as_bytes()))[..16].to_string();
    Defaults {
        revision,
        changelog: changelog(&tools),
        tools,
    }
}

fn changelog(tools: &[AgentTool]) -> String {
    let mut by_server: BTreeMap<&ServerName, Vec<&str>> = BTreeMap::new();
    for t in tools {
        by_server.entry(&t.server).or_default().push(&t.cluster);
    }
    if by_server.is_empty() {
        return "Runs get no MCP servers by default.".to_string();
    }
    let parts: Vec<String> = by_server
        .iter()
        .map(|(server, clusters)| format!("{server} on {}", clusters.join(", ")))
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
    let mut by_run = for_runs(pool, &[run_id.to_string()]).await?;
    Ok(by_run.remove(run_id).unwrap_or_default())
}

/// What each of `run_ids` was granted, by server; a run granted nothing is absent.
pub async fn for_runs(
    pool: &PgPool,
    run_ids: &[String],
) -> Result<BTreeMap<String, Vec<AgentTool>>> {
    type Row = (
        String,
        String,
        String,
        String,
        Option<String>,
        sqlx::types::Json<Vec<String>>,
        sqlx::types::Json<BTreeMap<String, String>>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT run_id, server, cluster, url, kube_user, scopes, limits FROM run_agent_tools
          WHERE run_id = ANY($1)",
    )
    .bind(run_ids)
    .fetch_all(pool)
    .await
    .context("reading runs' agent tools")?;
    let mut by_run: BTreeMap<String, Vec<AgentTool>> = BTreeMap::new();
    for (run_id, server, cluster, url, kube_user, scopes, limits) in rows {
        let Ok(server) = ServerName::parse(&server) else {
            continue;
        };
        by_run.entry(run_id).or_default().push(AgentTool {
            server,
            cluster,
            url,
            kube_user,
            scopes: scopes.0,
            limits: limits.0,
        });
    }
    for tools in by_run.values_mut() {
        tools.sort_by(|a, b| a.server.cmp(&b.server));
    }
    Ok(by_run)
}

/// Record what run `run_id`, dispatched onto `cluster`, is granted. Each tool carries the
/// Kubernetes identity its tokens will hold: the run's own pod user, known at dispatch.
pub async fn grant(
    db: &crate::Db,
    servers: &[ServerSpec],
    run_id: &str,
    cluster: &str,
    tokens: &AgentTokens,
) -> Result<Vec<AgentTool>> {
    let mut tools = granted(servers, cluster, tokens);
    let kube_user = format!(
        "crucible:run:{}",
        crate::runs::workpod::run_pod_name(run_id)
    );
    for tool in &mut tools {
        tool.kube_user = Some(kube_user.clone());
    }
    record(db.pool(), run_id, &tools).await?;
    Ok(tools)
}

/// Say what run `run_id` got on the issue's event log, one line per server.
pub async fn attribute(db: &crate::Db, issue_key: &str, run_id: &str) -> Result<()> {
    let tools = for_run(db.pool(), run_id).await?;
    if tools.is_empty() {
        return Ok(());
    }
    let status = crate::issues::store::get_issue(db.pool(), issue_key)
        .await?
        .map(|i| i.status.as_str().to_string())
        .unwrap_or_else(|| "new".to_string());
    for tool in &tools {
        db.events()
            .append(&crate::event_log::Event::now(
                issue_key,
                &status,
                &status,
                Some(&tool.summary()),
                Some(run_id),
            ))
            .await?;
    }
    Ok(())
}

/// [`attribute`] after a launch committed its run row, when runs get agent tools. A failure is
/// logged: the run is already launched, and the record is what the run page shows.
pub async fn attribute_launched(
    db: &crate::Db,
    cfg: &crate::config::ControllerCfg,
    issue_key: &str,
    run_id: &str,
) {
    if !cfg.issues_agent_tokens() {
        return;
    }
    if let Err(e) = attribute(db, issue_key, run_id).await {
        tracing::warn!(
            run_id,
            error = format!("{e:#}"),
            "recording a run's agent tools on the event log failed"
        );
    }
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
        [clusters.k8s]
        kubeconfig_secret = "k8s-kubeconfig"
        identity = { claim = "email", prefix = "oidc:" }
        [kueue]
        image = "registry.example.com/crucible-kueue:latest"
        clusters = ["ocp", "k8s"]
        namespace = "crucible-system"
        workload_namespace = "crucible-kueue-e2e"
        kube_authz = "openshift"
        pull_secrets = ["quay-pull"]
        [buildit]
        clusters = ["k8s"]
        namespace = "crucible-builds"
        push_repo = "quay.io/org/smoke"
    "#;

    fn profile(text: &str) -> crucible::deploy::profile::DeployProfile {
        toml::from_str(text).expect("profile parses")
    }

    fn servers(text: &str) -> Vec<ServerSpec> {
        profile(text).agent_servers().expect("servers")
    }

    fn tokens(clusters: &[&str]) -> AgentTokens {
        AgentTokens {
            svid_audience: "https://crucible.hub.example".into(),
            endpoint: "https://crucible.hub.example/oauth/token".into(),
            clusters: clusters.iter().map(|c| c.to_string()).collect(),
            spire: Default::default(),
        }
    }

    #[test]
    fn a_run_gets_the_servers_its_cluster_runs_with_the_issuers_scopes_and_limits() {
        let p = servers(PROFILE);
        let all = tokens(&["ocp", "k8s"]);
        let on_k8s = granted(&p, "k8s", &all);
        assert_eq!(
            on_k8s.iter().map(|t| t.server.as_str()).collect::<Vec<_>>(),
            ["buildit", "kueue"],
            "by name"
        );
        let kueue = &on_k8s[1];
        assert_eq!(kueue.kube_user, None, "filled in from the run's principal");
        assert_eq!(kueue.scopes, ["kueue.read", "kueue.submit"]);
        assert_eq!(kueue.limits["namespace"], "crucible-kueue-e2e");
        assert_eq!(kueue.limits["pull_secrets"], "quay-pull");
        let buildit = &on_k8s[0];
        assert_eq!(buildit.url, "http://host.openshell.internal:8848/mcp");
        assert_eq!(buildit.limits["push_repo"], "quay.io/org/smoke");
        assert_eq!(
            buildit.summary(),
            "granted buildit/k8s (buildit.read buildit.build)"
        );

        let on_ocp = granted(&p, "ocp", &all);
        assert_eq!(on_ocp.len(), 1, "buildit serves k8s only");
        assert!(granted(&p, "hub", &all).is_empty());
        assert!(
            granted(&p, "k8s", &tokens(&["ocp"])).is_empty(),
            "no SPIRE for k8s, so nothing its sandboxes could get tokens for"
        );
    }

    #[test]
    fn the_defaults_revision_changes_exactly_when_the_granted_set_does() {
        let all = tokens(&["ocp", "k8s"]);
        let base = defaults(&servers(PROFILE), Some(&all));
        assert_eq!(base, defaults(&servers(PROFILE), Some(&all)), "stable");
        assert_eq!(base.revision.len(), 16);
        assert_eq!(
            base.changelog,
            "Runs now include buildit on k8s and kueue on k8s, ocp tools."
        );
        let reworded = PROFILE.replace(
            "image = \"registry.example.com/crucible-kueue:latest\"",
            "image = \"registry.example.com/crucible-kueue:v2\"",
        );
        assert_eq!(
            defaults(&servers(&reworded), Some(&all)).revision,
            base.revision,
            "a new server image grants nothing new"
        );
        for changed in [
            PROFILE.replace("clusters = [\"k8s\"]", "clusters = [\"ocp\", \"k8s\"]"),
            PROFILE.replace(
                "push_repo = \"quay.io/org/smoke\"",
                "push_repo = \"quay.io/org/other\"",
            ),
            PROFILE.replace("pull_secrets = [\"quay-pull\"]", ""),
        ] {
            assert_ne!(
                defaults(&servers(&changed), Some(&all)).revision,
                base.revision,
                "{changed}"
            );
        }
        for none in [defaults(&[], Some(&all)), defaults(&servers(PROFILE), None)] {
            assert!(none.tools.is_empty());
            assert_eq!(none.changelog, "Runs get no MCP servers by default.");
        }
    }

    async fn run_dispatched_for(pool: &PgPool, run_id: &str, principal: &str) {
        crate::issues::store::upsert_issue(
            pool,
            &crate::issues::model::NewIssue {
                key: "owner/repo#1".into(),
                repo: "owner/repo".into(),
                priority: 5,
                evidence_url: None,
                title: None,
                author: None,
                body: None,
                labels: Vec::new(),
                upstream_updated_at: None,
            },
        )
        .await
        .unwrap();
        crate::runs::store::insert_run(
            pool,
            &crate::NewRun {
                run_id: run_id.into(),
                scope: None,
                issue: Some("owner/repo#1".into()),
                identity_digest: None,
                status: "running".into(),
                pod: None,
                session_uri: None,
                best_score: None,
                cost_usd: None,
            },
        )
        .await
        .unwrap();
        sqlx::query("UPDATE runs SET dispatched_by = $2 WHERE run_id = $1")
            .bind(run_id)
            .bind(principal)
            .execute(pool)
            .await
            .unwrap();
    }

    /// The record carries the run's Kubernetes identity, as its tokens will, and the issue's
    /// event log says what the run got, one line per server.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_grant_records_the_runs_identity_and_is_told_on_the_issue(pool: PgPool) {
        let db = crate::Db::new(pool.clone());
        run_dispatched_for(&pool, "run-1", "user:alice").await;
        let all = tokens(&["ocp", "k8s"]);
        let tools = grant(&db, &servers(PROFILE), "run-1", "k8s", &all)
            .await
            .unwrap();
        assert_eq!(tools.len(), 2);
        for tool in &tools {
            assert_eq!(
                tool.kube_user.as_deref(),
                Some("crucible:run:crucible-run-run-1"),
                "the identity its tokens carry, known at dispatch"
            );
        }
        assert_eq!(
            for_run(&pool, "run-1").await.unwrap(),
            tools,
            "read back as recorded"
        );
        attribute(&db, "owner/repo#1", "run-1").await.unwrap();
        let events = db.events().read_for_key("owner/repo#1").await.unwrap();
        let reasons: Vec<&str> = events.iter().filter_map(|e| e.reason.as_deref()).collect();
        assert_eq!(
            reasons,
            [
                "granted buildit/k8s as crucible:run:crucible-run-run-1 (buildit.read buildit.build)",
                "granted kueue/k8s as crucible:run:crucible-run-run-1 (kueue.read kueue.submit)"
            ]
        );
        assert!(
            events
                .iter()
                .all(|e| e.evidence.as_deref() == Some("run-1"))
        );
        assert!(for_run(&pool, "run-2").await.unwrap().is_empty());
        attribute(&db, "owner/repo#1", "run-2").await.unwrap();
    }
}
