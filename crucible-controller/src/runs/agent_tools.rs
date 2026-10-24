//! The MCP servers a run's agent is granted by default, from the deploy profile: crucible-kueue on
//! the `[kueue]` clusters and buildit on the `[buildit]` clusters, where the issuer mints their
//! tokens and with the scopes it mints. Dispatch records what a run got, the run's principal fills
//! in who kueue acts as once it is stamped, the run page shows it, and a revision of the
//! configured set tells people when it changes.

use crate::runs::workpod::kueue::AgentTokens;
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
    /// `granted kueue/pirate as casey (kueue.read kueue.submit)`, the run's event line.
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

fn kueue_tool(cfg: &KueueCfg, cluster: &str, scopes: &[String]) -> AgentTool {
    let mut limits =
        BTreeMap::from([("namespace".to_string(), cfg.workload_namespace.to_string())]);
    if !cfg.pull_secrets.is_empty() {
        limits.insert("pull_secrets".to_string(), cfg.pull_secrets.join(","));
    }
    AgentTool {
        server: AgentServer::Kueue,
        cluster: cluster.to_string(),
        url: cfg.url(),
        kube_user: None,
        scopes: scopes.to_vec(),
        limits,
    }
}

fn buildit_tool(cfg: &BuilditCfg, cluster: &str, scopes: &[String]) -> AgentTool {
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
        scopes: scopes.to_vec(),
        limits,
    }
}

/// What a run on `cluster` is granted: each server there whose tokens the issuer mints, with the
/// scopes it mints them with.
pub fn granted(
    kueue: Option<&KueueCfg>,
    buildit: Option<&BuilditCfg>,
    cluster: &str,
    tokens: &AgentTokens,
) -> Vec<AgentTool> {
    let kueue = kueue.filter(|k| k.serves(cluster)).and_then(|k| {
        tokens
            .audience(cluster, &k.url())
            .map(|a| kueue_tool(k, cluster, &a.scopes))
    });
    let buildit = buildit.filter(|b| b.serves(cluster)).and_then(|b| {
        tokens
            .audience(cluster, crucible_contract::buildit::URL)
            .map(|a| buildit_tool(b, cluster, &a.scopes))
    });
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
pub fn defaults(profile: Option<&DeployProfile>, tokens: Option<&AgentTokens>) -> Defaults {
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
    let tools: Vec<AgentTool> = match tokens {
        Some(tokens) => clusters
            .iter()
            .flat_map(|cluster| granted(kueue, buildit, cluster, tokens))
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
        let Some(server) = AgentServer::parse(&server) else {
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
        tools.sort_by_key(|t| t.server);
    }
    Ok(by_run)
}

/// Record what run `run_id`, dispatched onto `cluster`, is granted. Who kueue acts as is the run's
/// principal, stamped after dispatch: [`attribute`] fills it in.
pub async fn grant(
    db: &crate::Db,
    profile: &DeployProfile,
    run_id: &str,
    cluster: &str,
    tokens: &AgentTokens,
) -> Result<Vec<AgentTool>> {
    let tools = granted(
        profile.kueue.as_ref(),
        profile.buildit.as_ref(),
        cluster,
        tokens,
    );
    record(db.pool(), run_id, &tools).await?;
    Ok(tools)
}

/// Fill in who run `run_id`'s kueue tools act as, the username its tokens carry: the person it was
/// dispatched for (`runs.dispatched_by`), under the profile's identity rule for the tool's cluster.
/// Then say what the run got on the issue's event log, one line per server.
pub async fn attribute(
    db: &crate::Db,
    profile: &DeployProfile,
    issue_key: &str,
    run_id: &str,
) -> Result<()> {
    let mut tools = for_run(db.pool(), run_id).await?;
    if tools.is_empty() {
        return Ok(());
    }
    let dispatched_by: Option<String> =
        sqlx::query_scalar("SELECT dispatched_by FROM runs WHERE run_id = $1")
            .bind(run_id)
            .fetch_optional(db.pool())
            .await
            .context("reading who the run was dispatched for")?
            .flatten();
    let launcher = dispatched_by
        .as_deref()
        .and_then(|p| p.strip_prefix("user:"))
        .filter(|login| !login.is_empty());
    for tool in tools.iter_mut().filter(|t| t.server == AgentServer::Kueue) {
        let Some(login) = launcher else { break };
        if let crate::identity::cluster_identity::Resolution::Known { user, .. } =
            crate::identity::cluster_identity::for_login(db.pool(), profile, &tool.cluster, login)
                .await?
        {
            tool.kube_user = Some(user.as_str().to_string());
            sqlx::query(
                "UPDATE run_agent_tools SET kube_user = $3 WHERE run_id = $1 AND server = $2",
            )
            .bind(run_id)
            .bind(tool.server.as_str())
            .bind(&tool.kube_user)
            .execute(db.pool())
            .await
            .context("recording who a run's agent tool acts as")?;
        }
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
    let attributed = match cfg.profile().await {
        Ok(Some(profile)) => attribute(db, &profile, issue_key, run_id).await,
        Ok(None) => Ok(()),
        Err(e) => Err(e),
    };
    if let Err(e) = attributed {
        tracing::warn!(
            run_id,
            error = format!("{e:#}"),
            "recording who a run's agent tools act as failed"
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
        namespace = "crucible-builds"
        run_issuer = "https://crucible.hub.example"
        push_repo = "quay.io/org/smoke"
    "#;

    fn profile(text: &str) -> DeployProfile {
        toml::from_str(text).expect("profile parses")
    }

    const KUEUE_URL: &str = "http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp";

    fn tokens(clusters: &[&str], kueue_scopes: &str) -> AgentTokens {
        AgentTokens {
            svid_audience: "https://crucible.hub.example".into(),
            endpoint: "https://crucible.hub.example/oauth/token".into(),
            clusters: clusters.iter().map(|c| c.to_string()).collect(),
            audiences: vec![
                format!("{KUEUE_URL}={kueue_scopes}").parse().unwrap(),
                format!(
                    "{}=buildit.read buildit.build",
                    crucible_contract::buildit::URL
                )
                .parse()
                .unwrap(),
            ],
        }
    }

    #[test]
    fn a_run_gets_the_servers_its_cluster_runs_with_the_issuers_scopes_and_limits() {
        let p = profile(PROFILE);
        let all = tokens(&["pirate", "waldorf"], "kueue.read kueue.submit");
        let on_waldorf = granted(p.kueue.as_ref(), p.buildit.as_ref(), "waldorf", &all);
        assert_eq!(
            on_waldorf.iter().map(|t| t.server).collect::<Vec<_>>(),
            [AgentServer::Kueue, AgentServer::Buildit]
        );
        let kueue = &on_waldorf[0];
        assert_eq!(kueue.kube_user, None, "filled in from the run's principal");
        assert_eq!(kueue.scopes, ["kueue.read", "kueue.submit"]);
        assert_eq!(kueue.limits["namespace"], "crucible-kueue-e2e");
        assert_eq!(kueue.limits["pull_secrets"], "quay-pull");
        let buildit = &on_waldorf[1];
        assert_eq!(buildit.url, "http://host.openshell.internal:8848/mcp");
        assert_eq!(buildit.limits["push_repo"], "quay.io/org/smoke");
        assert_eq!(
            buildit.summary(),
            "granted buildit/waldorf (buildit.read buildit.build)"
        );

        let read_only = tokens(&["pirate", "waldorf"], "kueue.read");
        assert_eq!(
            granted(p.kueue.as_ref(), None, "waldorf", &read_only)[0].scopes,
            ["kueue.read"],
            "the scopes the issuer mints, not a fixed set"
        );
        let on_pirate = granted(p.kueue.as_ref(), p.buildit.as_ref(), "pirate", &all);
        assert_eq!(on_pirate.len(), 1, "buildit serves waldorf only");
        assert!(granted(p.kueue.as_ref(), p.buildit.as_ref(), "hub", &all).is_empty());
        assert!(
            granted(
                p.kueue.as_ref(),
                p.buildit.as_ref(),
                "waldorf",
                &tokens(&["pirate"], "kueue.read")
            )
            .is_empty(),
            "no SPIRE for waldorf, so nothing its sandboxes could get tokens for"
        );
    }

    #[test]
    fn the_defaults_revision_changes_exactly_when_the_granted_set_does() {
        let all = tokens(&["pirate", "waldorf"], "kueue.read kueue.submit");
        let base = defaults(Some(&profile(PROFILE)), Some(&all));
        assert_eq!(
            base,
            defaults(Some(&profile(PROFILE)), Some(&all)),
            "stable"
        );
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
            defaults(Some(&profile(&reworded)), Some(&all)).revision,
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
                defaults(Some(&profile(&changed)), Some(&all)).revision,
                base.revision,
                "{changed}"
            );
        }
        assert_ne!(
            defaults(
                Some(&profile(PROFILE)),
                Some(&tokens(&["pirate", "waldorf"], "kueue.read"))
            )
            .revision,
            base.revision,
            "fewer scopes is a different set"
        );
        for none in [
            defaults(None, Some(&all)),
            defaults(Some(&profile(PROFILE)), None),
        ] {
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

    /// The record names who kueue acts as from the run's principal, as its tokens do: after a
    /// redispatch by someone else, the new run acts for them.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_grant_acts_for_the_runs_principal_and_is_told_on_the_issue(pool: PgPool) {
        let db = crate::Db::new(pool.clone());
        for (sub, login) in [("sub-alice", "alice"), ("sub-bob", "bob")] {
            crate::identity::oidc::users::record_login(
                &pool,
                sub,
                login,
                Some(&format!("{login}@example.com")),
                jiff::Timestamp::now(),
            )
            .await
            .unwrap();
        }
        let p = profile(PROFILE);
        let all = tokens(&["pirate", "waldorf"], "kueue.read kueue.submit");
        let tools = grant(&db, &p, "run-1", "waldorf", &all).await.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(
            for_run(&pool, "run-1").await.unwrap(),
            tools,
            "read back as recorded"
        );

        run_dispatched_for(&pool, "run-1", "user:alice").await;
        attribute(&db, &p, "owner/repo#1", "run-1").await.unwrap();
        let recorded = for_run(&pool, "run-1").await.unwrap();
        assert_eq!(
            recorded[0].kube_user.as_deref(),
            Some("oidc:alice@example.com"),
            "the username waldorf's rule gives alice"
        );
        assert_eq!(
            recorded[1].kube_user, None,
            "buildit acts as no Kubernetes user"
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

        grant(&db, &p, "run-2", "waldorf", &all).await.unwrap();
        run_dispatched_for(&pool, "run-2", "user:bob").await;
        attribute(&db, &p, "owner/repo#1", "run-2").await.unwrap();
        assert_eq!(
            for_run(&pool, "run-2").await.unwrap()[0]
                .kube_user
                .as_deref(),
            Some("oidc:bob@example.com"),
            "a redispatch for bob acts as bob"
        );

        grant(&db, &p, "run-3", "pirate", &all).await.unwrap();
        run_dispatched_for(&pool, "run-3", "platform").await;
        attribute(&db, &p, "owner/repo#1", "run-3").await.unwrap();
        assert_eq!(
            for_run(&pool, "run-3").await.unwrap()[0].kube_user,
            None,
            "no person, no username"
        );
        assert!(for_run(&pool, "run-4").await.unwrap().is_empty());
        attribute(&db, &p, "owner/repo#1", "run-4").await.unwrap();
    }
}
