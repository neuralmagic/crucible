//! Tracker-agnostic experiment emission: file a human-review trail into the tracker.
//!
//! One **container** per experiment — identity = (issue key, frozen pack digest) — holding refs
//! to the scenario and the pack, plus one **task** per opened PR. Containers are created lazily
//! on the first kept PR, so an experiment that never keeps anything files nothing. The
//! `emissions` ledger (migration 0025) makes every step lookup-before-create: retries and
//! re-reconciles never file twice, and later runs of the same experiment reuse its container.

use crate::client::Db;
use crate::launches::tracker::{EmittedKind, IssueEmitter, NewTrackerIssue};
use crate::runs::ingest::PrLink;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Everything emission needs, resolved from config at the call site and passed explicitly (no
/// globals). `None` at the hook point ⇒ emission is off for this deploy.
#[derive(Clone)]
pub struct EmissionCtx {
    pub(crate) emitter: Arc<dyn IssueEmitter>,
    /// Stamped on every emitted issue (e.g. `agentops`), straight from config.
    pub(crate) labels: Vec<String>,
    /// The SPA base for deep-links; `None` ⇒ bodies omit links.
    pub(crate) public_url: Option<String>,
}

impl EmissionCtx {
    /// The experiment-emission context, or `None` when the Jira creds or any `jira_emission_*`
    /// field is unset — the completion hook then files nothing, silently and by design.
    pub fn from_cfg(cfg: &crate::config::ControllerCfg) -> Option<Self> {
        let jira = cfg.jira_config()?;
        let emission = crate::launches::jira::JiraEmissionCfg {
            project_key: cfg.jira_emission_project.clone()?,
            container_type_id: cfg.jira_emission_epic_type_id.clone()?,
            task_type_id: cfg.jira_emission_task_type_id.clone()?,
        };
        let tracker = match crate::launches::jira::JiraTracker::new(jira) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "emission: Jira client build failed"
                );
                return None;
            }
        };
        Some(EmissionCtx {
            emitter: std::sync::Arc::new(crate::launches::jira::JiraEmitter::new(
                tracker, emission,
            )),
            labels: cfg.emission_labels.clone(),
            public_url: cfg.public_url.clone(),
        })
    }
}

/// The experiment being emitted for: the adopted issue + the frozen pack that ran.
pub(crate) struct ExperimentRef<'a> {
    /// Stored `issues.key` (`jira:{site}:{PROJ-N}`, `owner/repo#N`, `scenario:{id}`).
    pub(crate) issue_key: &'a str,
    /// The issue's human title, for emitted summaries.
    pub(crate) issue_title: &'a str,
    /// The frozen pack digest the run executed — half of the experiment identity.
    pub(crate) pack_digest: &'a str,
}

/// What one emission pass did: issues newly filed vs. patched in place. A replay with
/// unchanged inputs is `updated == everything, created == 0` — never a skip, so corrected
/// titles/links always reach the tracker.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EmitOutcome {
    pub(crate) created: usize,
    pub(crate) updated: usize,
}

/// Emit the review trail for a finished run's opened PRs: create on first sight, PATCH in
/// place on re-emission (ledger hit), and pin the PR into each task's links panel as a web
/// link (URL-keyed, so replays refresh rather than stack).
pub(crate) async fn emit_pr_reviews(
    db: &Db,
    ctx: &EmissionCtx,
    exp: &ExperimentRef<'_>,
    run_id: &str,
    pr_links: &[PrLink],
) -> Result<EmitOutcome> {
    if pr_links.is_empty() {
        return Ok(EmitOutcome::default());
    }
    let mut out = EmitOutcome::default();
    // The session deep-link is only real once the run is ingested; a dead link in a filed
    // ticket reads as a bug, so an untracked run_id emits no link at all.
    let run_ingested = crate::runs::store::get_run(db.pool(), run_id)
        .await?
        .is_some();
    let container_artifact = format!("epic:{}", exp.pack_digest);
    let container_id =
        match crate::launches::store::emission_for(db.pool(), exp.issue_key, &container_artifact)
            .await?
        {
            Some(id) => {
                ctx.emitter
                    .update_issue(&id, container_issue(ctx, exp))
                    .await
                    .context("patching the experiment container issue")?;
                out.updated += 1;
                id
            }
            None => {
                let id = ctx
                    .emitter
                    .create_issue(container_issue(ctx, exp))
                    .await
                    .context("creating the experiment container issue")?;
                crate::launches::store::record_emission(
                    db.pool(),
                    exp.issue_key,
                    &container_artifact,
                    &id,
                )
                .await?;
                out.created += 1;
                id
            }
        };

    for pr in pr_links {
        let issue = task_issue(ctx, exp, run_id, pr, &container_id, run_ingested);
        let id =
            match crate::launches::store::emission_for(db.pool(), exp.issue_key, &pr.url).await? {
                Some(id) => {
                    ctx.emitter
                        .update_issue(&id, issue)
                        .await
                        .with_context(|| format!("patching the review task for {}", pr.url))?;
                    out.updated += 1;
                    id
                }
                None => {
                    let id = ctx
                        .emitter
                        .create_issue(issue)
                        .await
                        .with_context(|| format!("creating the review task for {}", pr.url))?;
                    crate::launches::store::record_emission(db.pool(), exp.issue_key, &pr.url, &id)
                        .await?;
                    out.created += 1;
                    id
                }
            };
        ctx.emitter
            .add_web_link(&id, &pr.url, &format!("PR: {}", pr.repo))
            .await
            .with_context(|| format!("linking {} on the review task", pr.url))?;
    }
    Ok(out)
}

/// The completion-edge entrypoint: resolve the experiment identity (issue title, the scope's
/// frozen pack digest) from the ledger, then [`emit_pr_reviews`]. Failures here must never fail
/// the run completion — the caller logs and continues.
pub(crate) async fn emit_for_completed_run(
    db: &Db,
    ctx: &EmissionCtx,
    issue_key: &str,
    scope_id: i64,
    run_id: &str,
    pr_links: &[PrLink],
) -> Result<EmitOutcome> {
    let scope = crate::issues::store::get_scope_by_id(db.pool(), scope_id)
        .await?
        .with_context(|| format!("emission: no scope row {scope_id}"))?;
    let pack_digest = scope.pack_digest.as_deref().unwrap_or("unpinned");
    let issue = crate::issues::store::get_issue(db.pool(), issue_key)
        .await?
        .with_context(|| format!("emission: no issue row {issue_key}"))?;
    let title = issue.title.as_deref().unwrap_or(issue_key);
    emit_pr_reviews(
        db,
        ctx,
        &ExperimentRef {
            issue_key,
            issue_title: title,
            pack_digest,
        },
        run_id,
        pr_links,
    )
    .await
}

/// Percent-encode a stored issue key for a path segment (`:` and `#` are the usual offenders).
/// Path-segment encoding that leaves RFC 3986 unreserved characters (`-`, `.`, `_`, `~`)
/// alone: NON_ALPHANUMERIC escaped hyphens too, mangling every link into `%2D` soup.
const PATH_SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'\\');

fn encoded(key: &str) -> String {
    percent_encoding::utf8_percent_encode(key, PATH_SEGMENT).to_string()
}

fn container_issue(ctx: &EmissionCtx, exp: &ExperimentRef<'_>) -> NewTrackerIssue {
    let mut body = format!(
        "Review trail for a crucible experiment.\n\n\
         - scenario: {}\n\
         - pack digest: {}",
        exp.issue_key, exp.pack_digest
    );
    if let Some(base) = &ctx.public_url {
        body.push_str(&format!(
            "\n- progress: {}/issues/{}",
            base.trim_end_matches('/'),
            encoded(exp.issue_key)
        ));
    }
    NewTrackerIssue {
        title: format!("experiment: {}", exp.issue_title),
        body,
        labels: ctx.labels.clone(),
        kind: EmittedKind::Container,
        parent: None,
        extra_fields: BTreeMap::new(),
    }
}

fn task_issue(
    ctx: &EmissionCtx,
    exp: &ExperimentRef<'_>,
    run_id: &str,
    pr: &PrLink,
    container_id: &str,
    run_ingested: bool,
) -> NewTrackerIssue {
    let component = if pr.name.is_empty() {
        String::new()
    } else {
        format!(" (component {})", pr.name)
    };
    let mut body = format!(
        "Human review of agent-generated code.\n\n\
         - PR: {}\n\
         - repo: {}{component}\n\
         - candidate branch: {}\n\
         - run: {run_id}",
        pr.url, pr.repo, pr.branch
    );
    if run_ingested && let Some(base) = &ctx.public_url {
        body.push_str(&format!(
            "\n- session: {}/runs/{}",
            base.trim_end_matches('/'),
            encoded(run_id)
        ));
    }
    NewTrackerIssue {
        title: format!("review: {} — {}", pr.branch, exp.issue_title),
        body,
        labels: ctx.labels.clone(),
        kind: EmittedKind::Task,
        parent: Some(container_id.to_string()),
        extra_fields: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use crate::daemon::queue::BoxFuture;
    use crate::launches::emission::{EmissionCtx, EmitOutcome, ExperimentRef, emit_pr_reviews};
    use crate::launches::tracker::{EmittedKind, IssueEmitter, NewTrackerIssue};
    use crate::runs::ingest::PrLink;
    use anyhow::Result;
    use sqlx::PgPool;
    use std::sync::{Arc, Mutex};

    /// A real in-memory emitter: records creates, in-place updates, and web links.
    #[derive(Default)]
    struct MemEmitter {
        created: Mutex<Vec<NewTrackerIssue>>,
        updated: Mutex<Vec<(String, NewTrackerIssue)>>,
        links: Mutex<Vec<(String, String)>>,
    }

    impl IssueEmitter for MemEmitter {
        fn create_issue(&self, issue: NewTrackerIssue) -> BoxFuture<Result<String>> {
            let mut created = self.created.lock().unwrap();
            created.push(issue);
            let id = format!("EMIT-{}", created.len());
            Box::pin(async move { Ok(id) })
        }

        fn update_issue(&self, id: &str, issue: NewTrackerIssue) -> BoxFuture<Result<()>> {
            self.updated.lock().unwrap().push((id.to_string(), issue));
            Box::pin(async move { Ok(()) })
        }

        fn add_web_link(&self, id: &str, url: &str, _title: &str) -> BoxFuture<Result<()>> {
            self.links
                .lock()
                .unwrap()
                .push((id.to_string(), url.to_string()));
            Box::pin(async move { Ok(()) })
        }
    }

    fn ctx(emitter: Arc<MemEmitter>, public_url: Option<&str>) -> EmissionCtx {
        EmissionCtx {
            emitter,
            labels: vec!["agentops".to_string()],
            public_url: public_url.map(str::to_string),
        }
    }

    fn pr(url: &str, branch: &str) -> PrLink {
        PrLink {
            url: url.to_string(),
            repo: "owner/repo".to_string(),
            name: String::new(),
            branch: branch.to_string(),
        }
    }

    fn exp<'a>() -> ExperimentRef<'a> {
        ExperimentRef {
            issue_key: "jira:example:ACME-9093",
            issue_title: "Triton bitcast error",
            pack_digest: "sha256:abc",
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn first_emission_files_container_then_tasks_under_it(pool: PgPool) -> Result<()> {
        let db = crate::client::Db::new(pool);
        let emitter = Arc::new(MemEmitter::default());
        let ctx = ctx(emitter.clone(), Some("https://crucible.example"));

        let n = emit_pr_reviews(
            &db,
            &ctx,
            &exp(),
            "run-1",
            &[
                pr("https://gh/pr/1", "autoresearch/run-1/a"),
                pr("https://gh/pr/2", "autoresearch/run-1/b"),
            ],
        )
        .await?;
        assert_eq!(
            n,
            EmitOutcome {
                created: 3,
                updated: 0
            }
        );

        let links = emitter.links.lock().unwrap();
        assert_eq!(
            *links,
            vec![
                ("EMIT-2".to_string(), "https://gh/pr/1".to_string()),
                ("EMIT-3".to_string(), "https://gh/pr/2".to_string()),
            ],
            "each task gets its PR pinned as a web link"
        );
        let created = emitter.created.lock().unwrap();
        assert_eq!(created.len(), 3, "container + 2 tasks");
        assert_eq!(created[0].kind, EmittedKind::Container);
        assert!(created[0].body.contains("sha256:abc"));
        assert!(created[0].body.contains("jira:example:ACME-9093"));
        assert!(
            created[0]
                .body
                .contains("https://crucible.example/issues/jira:example:ACME-9093"),
            "deep-link present and readable (colon and hyphen are legal in a path segment): {}",
            created[0].body
        );
        assert_eq!(created[1].kind, EmittedKind::Task);
        assert_eq!(created[1].parent.as_deref(), Some("EMIT-1"));
        assert!(created[1].body.contains("https://gh/pr/1"));
        assert_eq!(created[2].parent.as_deref(), Some("EMIT-1"));
        assert_eq!(created[1].labels, vec!["agentops"]);
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn re_emission_is_idempotent_and_later_prs_reuse_the_container(
        pool: PgPool,
    ) -> Result<()> {
        let db = crate::client::Db::new(pool);
        let emitter = Arc::new(MemEmitter::default());
        let ctx = ctx(emitter.clone(), None);

        let first = [pr("https://gh/pr/1", "autoresearch/run-1/a")];
        assert_eq!(
            emit_pr_reviews(&db, &ctx, &exp(), "run-1", &first).await?,
            EmitOutcome {
                created: 2,
                updated: 0
            }
        );
        // Same PR again (crash-replay / corrected re-upload): nothing NEW is filed, but both
        // issues are patched in place so refreshed content reaches the tracker.
        assert_eq!(
            emit_pr_reviews(&db, &ctx, &exp(), "run-1", &first).await?,
            EmitOutcome {
                created: 0,
                updated: 2
            }
        );
        assert_eq!(
            emitter.updated.lock().unwrap().len(),
            2,
            "container + task patched on replay"
        );
        // A later run of the same experiment: new task, SAME container (patched).
        let second = [pr("https://gh/pr/9", "autoresearch/run-2/a")];
        assert_eq!(
            emit_pr_reviews(&db, &ctx, &exp(), "run-2", &second).await?,
            EmitOutcome {
                created: 1,
                updated: 1
            }
        );

        let created = emitter.created.lock().unwrap();
        assert_eq!(created.len(), 3, "one container total across both runs");
        assert_eq!(
            created
                .iter()
                .filter(|i| i.kind == EmittedKind::Container)
                .count(),
            1
        );
        assert_eq!(created[2].parent.as_deref(), Some("EMIT-1"));
        // No public_url ⇒ no links in bodies.
        assert!(!created[0].body.contains("http://") || created[0].body.contains("https://gh"));
        assert!(!created[0].body.contains("/issues/"));
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn empty_pr_set_files_nothing(pool: PgPool) -> Result<()> {
        let db = crate::client::Db::new(pool);
        let emitter = Arc::new(MemEmitter::default());
        let ctx = ctx(emitter.clone(), None);
        assert_eq!(
            emit_pr_reviews(&db, &ctx, &exp(), "run-1", &[]).await?,
            EmitOutcome::default()
        );
        assert!(
            emitter.created.lock().unwrap().is_empty(),
            "lazy: no empty containers"
        );
        Ok(())
    }
}
