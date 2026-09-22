//! `crucible-controller autopilot --once`: the reconcile core run to quiescence by hand (the
//! dev/test/break-glass mode). It enqueues every non-terminal row and drains it — here, since the
//! real queue + sources live in the resident daemon, "enqueue + drain" is just: read
//! `non_terminal_keys`, reconcile each once, sequentially, exit. The daemon being the ledger's only writer makes the
//! serial pass trivially safe.
//!
//! A per-key reconcile error is logged and the drain continues (one issue's failure must not strand
//! the rest); the resident daemon turns that same failure into a backoff-requeue then a
//! park. `--once` has no queue to requeue into, so it just reports what failed.

use crate::client::Db;
use crate::config::ControllerCfg;
use crate::issues::reconcile::reconcile;
use anyhow::{Context, Result};

/// Open the ledger from `cfg` and run one drain pass. The entry point `crucible-controller autopilot --once`
/// calls (via a `block_on` in the binary).
pub async fn run_once(cfg: &ControllerCfg) -> Result<()> {
    cfg.validate_grounded()
        .context("validating the grounded-rank executor configuration")?;
    cfg.validate_scope()
        .context("validating the scope executor configuration")?;
    let db = Db::open(cfg.db_url()).await.context("opening ledger")?;
    crate::issues::repo_watch::seed_watched_repos(db.pool(), &cfg.repos)
        .await
        .context("seeding the watched-repo set from CONTROLLER_WATCHED_REPOS")?;
    run_once_with(&db, cfg).await?;
    Ok(())
}

/// One drain pass over an already-open ledger (the seam tests drive). Returns the number of keys
/// reconciled; per-key errors are counted and logged, not propagated, so the pass is best-effort.
pub async fn run_once_with(db: &Db, cfg: &ControllerCfg) -> Result<usize> {
    let keys = crate::issues::store::non_terminal_keys(db.pool())
        .await
        .context("collecting non-terminal keys")?;
    let mut failures = 0usize;
    for key in &keys {
        if let Err(e) = reconcile(db, cfg, key).await {
            failures += 1;
            tracing::warn!(target: "autopilot_once", %key, error = format!("{e:#}"), "reconcile failed");
        }
    }
    if failures > 0 {
        tracing::warn!(target: "autopilot_once", drained = keys.len(), failures, "drain finished with failures");
    }
    Ok(keys.len())
}

#[cfg(test)]
// The crate-wide `ENV_LOCK` (an async mutex) is held across the async drain on purpose — see the
// note in `reconcile::tests`.
mod tests {
    use super::*;
    use crate::Db;
    use crate::issues::model::NewIssue;
    use crate::model::Status;
    use sqlx::PgPool;
    use std::path::{Path, PathBuf};

    /// A stand-in `crucible` binary that survives scope with a canned report + records `--out` (so
    /// the test can prove the pack dir was addressed) and asserts the pipeline flags are present.
    fn fake_crucible(dir: &Path, argfile: &Path) -> PathBuf {
        let json = r#"{"stages":[{"name":"validate","passed":true,"detail":"ok"}],"digest":"v1:beef","cost":0.33}"#;
        let path = dir.join("crucible");
        crate::testing::write_exec(
            &path,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = plan ] && [ \"$2\" = exposure ]; then\n printf '%s' '{{\"version\":1,\"outputs\":[],\"capabilities\":[]}}'\n exit 0\nfi\nif [ \"$1\" = scope ]; then\n echo \"$@\" >> '{}'\n \
                 prev=''\n pack=''\n \
                 for a in \"$@\"; do\n  if [ \"$prev\" = '--out' ]; then pack=\"$a\"; fi\n  prev=\"$a\"\n done\n \
                 if [ -n \"$pack\" ]; then mkdir -p \"$pack\"; printf '[repo]\\nurl = \"x\"\\n\\n[agent]\\n' > \"$pack/crucible.toml\"; fi\n \
                 printf '%s' '{json}'\n exit 0\nfi\nexit 0\n",
                argfile.display()
            ),
        );
        path
    }

    /// An OpenAI-chat-completions-shaped ranking response that confirms whatever tier it's told —
    /// `reconcile_new`'s first step runs for every drained `new` row.
    async fn mount_ranker_confirms(server: &wiremock::MockServer, tier: &str) {
        use wiremock::matchers::method;
        use wiremock::{Mock, ResponseTemplate};
        let text = format!(
            r#"{{"tier":"{tier}","affinity":"perf","rationale":"confirmed by test double","cost_usd":0.0}}"#
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": text}}],
                "usage": {"prompt_tokens": 100, "completion_tokens": 20}
            })))
            .mount(server)
            .await;
    }

    /// Mount a single-issue GET response — the GET `confirm_tier` makes per drained issue.
    async fn mount_issue(server: &wiremock::MockServer, repo: &str, number: u64) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        Mock::given(method("GET"))
            .and(path(format!("/repos/{repo}/issues/{number}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "number": number,
                "title": "an issue",
                "body": "body",
                "labels": [],
                "html_url": format!("https://github.com/{repo}/issues/{number}"),
                "updated_at": "2026-07-01T00:00:00Z",
                "state": "open",
            })))
            .mount(server)
            .await;
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn once_drains_new_rows_to_scoped_end_to_end(pool: PgPool) -> Result<()> {
        let _g = crate::ENV_LOCK.lock().await;
        let dir = tempfile::tempdir()?;
        let db = Db::new(pool);
        let argfile = dir.path().join("scope-args.txt");
        let bin = fake_crucible(dir.path(), &argfile);
        let gh = wiremock::MockServer::start().await;
        mount_issue(&gh, "owner/repo", 1).await;
        mount_issue(&gh, "owner/repo", 2).await;
        mount_ranker_confirms(&gh, "T1").await;
        unsafe {
            std::env::set_var("CRUCIBLE_BIN", &bin);
        }
        unsafe {
            std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
        }
        unsafe {
            std::env::set_var("GITHUB_API_URL", gh.uri());
        }

        let cfg = crate::testing::controller_cfg(dir.path(), vec!["owner/repo".into()]);

        for k in ["owner/repo#1", "owner/repo#2"] {
            crate::issues::store::upsert_issue(
                db.pool(),
                &NewIssue {
                    key: k.into(),
                    repo: "owner/repo".into(),
                    priority: 0,
                    evidence_url: None,
                    title: None,
                    author: None,
                    body: None,
                    labels: Vec::new(),
                    upstream_updated_at: None,
                },
            )
            .await?;
        }
        // A done row must be skipped by the drain (non_terminal_keys excludes it).
        crate::issues::store::upsert_issue(
            db.pool(),
            &NewIssue {
                key: "owner/repo#9".into(),
                repo: "owner/repo".into(),
                priority: 0,
                evidence_url: None,
                title: None,
                author: None,
                body: None,
                labels: Vec::new(),
                upstream_updated_at: None,
            },
        )
        .await?;
        assert!(
            crate::issues::store::claim_issue(db.pool(), "owner/repo#9", Status::New, Status::Done)
                .await?
        );

        let drained = run_once_with(&db, &cfg).await?;
        unsafe {
            std::env::remove_var("CRUCIBLE_BIN");
        }
        unsafe {
            std::env::remove_var("CONTROLLER_RANKER_API_URL");
        }
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }
        assert_eq!(drained, 2, "the done row is not drained");

        for k in ["owner/repo#1", "owner/repo#2"] {
            assert_eq!(
                crate::issues::store::get_issue(db.pool(), k)
                    .await?
                    .unwrap()
                    .status,
                Status::Scoped
            );
        }
        assert_eq!(
            crate::issues::store::get_issue(db.pool(), "owner/repo#9")
                .await?
                .unwrap()
                .status,
            Status::Done
        );

        // Two scope turns ledgered, two scope rows, and one rank-confirmation line plus one
        // transition line per issue (four event lines total).
        let today = jiff::Timestamp::now().strftime("%Y-%m-%d").to_string();
        assert_eq!(
            crate::issues::store::count_scopes_on_day(db.pool(), &today).await?,
            2
        );
        let events = crate::event_log::export_string(db.pool()).await?;
        assert_eq!(events.lines().count(), 4);

        // The scope subprocess was invoked with the pipeline flags + per-issue --out under the pack dir.
        let args = std::fs::read_to_string(&argfile)?;
        assert!(args.contains("--propose"), "args: {args}");
        assert!(args.contains("--issue owner/repo#1"), "args: {args}");
        assert!(
            args.contains("--max-cost 10"),
            "per-reconcile cost passed as --max-cost: {args}"
        );
        assert!(args.contains("--out"), "a scratch pack out dir: {args}");
        // The surviving trees were tarred into the durable pack store, keyed by sanitized key.
        for slug in ["owner_repo_1", "owner_repo_2"] {
            assert!(
                crate::runs::blob_store::get_pack_tarball(db.pool(), slug)
                    .await?
                    .is_some(),
                "pack tarball stored for {slug}"
            );
        }
        assert!(
            args.contains("--tier t1"),
            "the confirmed T1 tier is forwarded to the propose turn: {args}"
        );

        // Idempotent: a second --once pass is a no-op (both rows are now `scoped`; the approval is stubbed).
        let again = run_once_with(&db, &cfg).await?;
        assert_eq!(
            again, 2,
            "still non-terminal, but reconcile_scoped is a lane-E no-op"
        );
        assert_eq!(
            crate::issues::store::count_scopes_on_day(db.pool(), &today).await?,
            2,
            "no new scope turns"
        );
        Ok(())
    }
}
