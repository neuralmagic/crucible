//! A cluster-free controller http surface over a throwaway seeded ledger — the Vite dev proxy
//! target for frontend work, and a quick way to eyeball the SPA + API locally:
//!
//! ```sh
//! cargo run -p crucible-controller --example demo_server   # serves on 127.0.0.1:8899
//! ```

use crucible_controller::api::state::ApiState;
use crucible_controller::issues::model::{NewIssue, NewScope};
use crucible_controller::model::{ParkReason, ParkedBy, Status};
use crucible_controller::runs::model::{NewCandidate, NewRun};
use crucible_controller::{ControllerCfg, Db, Event, OverrideStore, QueueOverrideSink, WorkQueue};
use std::net::SocketAddr;
use std::sync::Arc;

/// `ControllerCfg` derives `clap::Args`, not `Parser`; flatten it under a `Parser` wrapper so the
/// example can build one off defaults and then override the few fields it cares about.
#[derive(clap::Parser)]
struct DemoCfg {
    #[command(flatten)]
    cfg: ControllerCfg,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    use clap::Parser;

    let dir = tempfile::tempdir()?;
    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://localhost:5432/crucible_demo".to_string());
    let db = Db::open(&db_url).await?;
    seed(&db).await?;
    seed_task_graph(&db).await?;

    let queue = WorkQueue::new();
    let sink = Arc::new(QueueOverrideSink::new(
        Arc::new(OverrideStore::new()),
        queue.clone(),
    ));
    let addr: SocketAddr = "127.0.0.1:8899".parse()?;

    let mut cfg = DemoCfg::parse_from(["demo_server"]).cfg;
    cfg.state_dir = dir.path().to_path_buf();
    cfg.admins = vec!["demo".to_string()];
    cfg.profile.max_concurrent_pods = 4;
    cfg.profile.max_scopes_per_day = 10;
    cfg.profile.daily_cost_ceiling = 50.0;

    let clusters = Arc::new(crucible_controller::runs::clusters::ClusterClients::new(
        None,
    ));
    let reconcile_now = Arc::new(tokio::sync::Notify::new());
    let contracts = Arc::new(crucible_controller::runs::contract::ContractRegistry::new(
        Arc::new(crucible_controller::runs::contract::LiveContractReader::new(None)),
    ));
    let state = ApiState::new(
        db,
        sink,
        Arc::new(queue),
        clusters,
        None,
        reconcile_now,
        contracts,
        crucible_controller::authz::policy::ActivePolicy::default_set()
            .expect("the shipped default policy set loads"),
        &cfg,
    );
    crucible_controller::serve(
        state,
        addr,
        crucible_controller::config::TurnAccounts::default(),
        cfg.session_secure_cookies,
    )
    .await
}

async fn seed(db: &Db) -> anyhow::Result<()> {
    for (n, title) in [
        (101, "EPP filter panics on empty pool"),
        (102, "prefix-cache scorer misses on chunked prefill"),
        (103, "router drops streaming responses under load"),
        (104, "flaky e2e: leader election times out"),
        (105, "memory leak in connection pooling"),
        (106, "incorrect timeout handling in retry logic"),
        (107, "duplicate report of the empty-pool panic"),
    ] {
        let key = format!("llm-d/llm-d-router#{n}");
        crucible_controller::issues::store::upsert_issue(
            db.pool(),
            &NewIssue {
                key: key.clone(),
                repo: "llm-d/llm-d-router".to_string(),
                priority: 5,
                evidence_url: Some(format!("https://github.com/llm-d/llm-d-router/issues/{n}")),
                title: Some(title.to_string()),
                author: Some("octocat".to_string()),
                body: Some(format!(
                    "Demo body for issue #{n}: steps to reproduce, expected vs actual."
                )),
                labels: vec!["kind/bug".to_string()],
                upstream_updated_at: Some("2026-07-01T12:00:00Z".to_string()),
            },
        )
        .await?;
    }

    // Status changes go through the event-logging verbs so /activity has a real tail:
    // transition() for machine progress, park() for machine parks (reason + authority + event),
    // unpark(actor) for the one human action so the feed shows an actor badge.
    crucible_controller::issues::transitions::transition(
        db.pool(),
        db.events(),
        "llm-d/llm-d-router#101",
        Status::New,
        Status::Running,
        Some("picked up by loop daemon"),
        None,
    )
    .await?;
    crucible_controller::issues::transitions::transition(
        db.pool(),
        db.events(),
        "llm-d/llm-d-router#102",
        Status::New,
        Status::AwaitingApproval,
        Some("agent produced candidate"),
        None,
    )
    .await?;
    for (n, reason) in [
        (103, "no upstream activity in 30 days"),
        (105, "unscopeable per ranker"),
        (106, "stale: already fixed in src/retry.rs:142"),
        (107, "duplicate of llm-d/llm-d-router#101"),
    ] {
        crucible_controller::issues::transitions::park(
            db.pool(),
            db.events(),
            &format!("llm-d/llm-d-router#{n}"),
            Status::New,
            &ParkReason::Legacy(reason.to_string()),
            ParkedBy::Machine,
        )
        .await?;
    }
    crucible_controller::issues::transitions::unpark(
        db.pool(),
        db.events(),
        "llm-d/llm-d-router#107",
        Some("re-evaluated priority"),
        Some("alice"),
    )
    .await?;

    // Ranking rationale: reconcile records the ranker's verdict as a `new -> new` event whose
    // reason carries the file:line evidence — the shape the issues page's expanded row surfaces.
    db.events().append(&Event::now(
        "llm-d/llm-d-router#101",
        "new",
        "new",
        Some("src/filter.rs:78 assumes pool.len() > 0 without checking; src/pool.rs:42 can return empty Vec; panics in low-traffic windows"),
        Some("sha256:abc123"),
    )).await?;
    db.events().append(&Event::now(
        "llm-d/llm-d-router#102",
        "new",
        "new",
        Some("src/scorer.rs:156 chunked prefill path misses cache check; full prefill path (scorer.rs:120) hits correctly"),
        Some("sha256:def456"),
    )).await?;

    let scope = crucible_controller::issues::store::insert_scope(
        db.pool(),
        &NewScope {
            issue: "llm-d/llm-d-router#102".to_string(),
            pack_digest: Some("sha256:demo".to_string()),
            check_outcome: None,
        },
    )
    .await?;
    crucible_controller::issues::store::set_scope_approval_pr(
        db.pool(),
        scope,
        "https://github.com/llm-d/llm-d-router/pull/900",
    )
    .await?;

    let run_scope = crucible_controller::issues::store::insert_scope(
        db.pool(),
        &NewScope {
            issue: "llm-d/llm-d-router#101".to_string(),
            pack_digest: Some("sha256:demo2".to_string()),
            check_outcome: None,
        },
    )
    .await?;
    crucible_controller::runs::store::insert_run(
        db.pool(),
        &NewRun {
            run_id: "run-demo-1".to_string(),
            scope: Some(run_scope),
            issue: None,
            identity_digest: None,
            status: "done".to_string(),
            pod: Some("loop-demo-1".to_string()),
            session_uri: None,
            best_score: Some(234.0),
            cost_usd: Some(12.5),
        },
    )
    .await?;
    crucible_controller::runs::store::insert_candidate(
        db.pool(),
        &NewCandidate {
            run_id: "run-demo-1".to_string(),
            kind: Some("deep".to_string()),
            lane: Some(0),
            iter: Some(3),
            score: Some(234.0),
            decision: Some("keep".to_string()),
            worktree: None,
            sandbox: None,
            pr_url: Some("https://github.com/llm-d/llm-d-router/pull/901".to_string()),
            branch: Some("autoresearch/run-demo-1/0".to_string()),
        },
    )
    .await?;
    Ok(())
}

/// Feed `run-demo-1` a session log through the real ingest path so the run page's task-graph
/// panel has an engine work graph to draw: a session-bound proposer, two iterations, a fail and
/// a skip for status-color variety.
async fn seed_task_graph(db: &Db) -> anyhow::Result<()> {
    let log = [
        r#"{"v":1,"kind":"plan_admitted","plan_version":1,"reason":"","budget_usd":25.0,"tasks":[{"name":"propose","kind":"engine_propose","depends_on":[],"session":"solver","needs":"any","required":true},{"name":"apply","kind":"engine_apply","depends_on":["propose"],"session":"","needs":"any","required":true},{"name":"refcheck","kind":"evaluate","depends_on":["apply"],"session":"","needs":"any","required":true},{"name":"calc-diff","kind":"evaluate","depends_on":["refcheck"],"session":"","needs":"any","required":true},{"name":"tensor-pipe","kind":"evaluate","depends_on":["calc-diff"],"session":"","needs":"any","required":false},{"name":"racecheck","kind":"evaluate","depends_on":["calc-diff"],"session":"","needs":"any","required":false},{"name":"grade","kind":"engine_grade","depends_on":["refcheck","calc-diff","tensor-pipe","racecheck"],"session":"","needs":"any","required":true},{"name":"decide","kind":"engine_decide","depends_on":["grade"],"session":"","needs":"any","required":true}]}"#,
        r#"{"v":1,"kind":"task_result","task":"propose","status":"pass","plan_version":1,"task_kind":"engine_propose","iter":1,"attempts":1,"cost_usd":1.4,"note":"","secs":210.0}"#,
        r#"{"v":1,"kind":"task_result","task":"apply","status":"pass","plan_version":1,"task_kind":"engine_apply","iter":1,"attempts":1,"cost_usd":0.0,"note":"","secs":2.0}"#,
        r#"{"v":1,"kind":"task_result","task":"refcheck","status":"pass","plan_version":1,"task_kind":"evaluate","iter":1,"attempts":1,"cost_usd":0.0,"note":"reference self-check","secs":45.0}"#,
        r#"{"v":1,"kind":"task_result","task":"calc-diff","status":"pass","plan_version":1,"task_kind":"evaluate","iter":1,"attempts":1,"cost_usd":0.0,"note":"calc_diff 0.0004","secs":480.0}"#,
        r#"{"v":1,"kind":"task_result","task":"tensor-pipe","status":"pass","plan_version":1,"task_kind":"evaluate","iter":1,"attempts":1,"cost_usd":0.0,"note":"tensor-pipe util 71%","secs":610.0}"#,
        r#"{"v":1,"kind":"task_result","task":"racecheck","status":"pass","plan_version":1,"task_kind":"evaluate","iter":1,"attempts":1,"cost_usd":0.0,"note":"0 hazards","secs":540.0}"#,
        r#"{"v":1,"kind":"task_result","task":"grade","status":"pass","plan_version":1,"task_kind":"engine_grade","iter":1,"attempts":1,"cost_usd":0.0,"note":"evidence 4/4, score from calc-diff","secs":0.1}"#,
        r#"{"v":1,"kind":"task_result","task":"decide","status":"pass","plan_version":1,"task_kind":"engine_decide","iter":1,"attempts":1,"cost_usd":0.0,"note":"keep","secs":0.1}"#,
        r#"{"v":1,"kind":"task_result","task":"propose","status":"pass","plan_version":1,"task_kind":"engine_propose","iter":2,"attempts":1,"cost_usd":0.9,"note":"","secs":150.0}"#,
        r#"{"v":1,"kind":"task_result","task":"apply","status":"pass","plan_version":1,"task_kind":"engine_apply","iter":2,"attempts":1,"cost_usd":0.0,"note":"","secs":2.0}"#,
        r#"{"v":1,"kind":"task_result","task":"refcheck","status":"pass","plan_version":1,"task_kind":"evaluate","iter":2,"attempts":1,"cost_usd":0.0,"note":"","secs":44.0}"#,
        r#"{"v":1,"kind":"task_result","task":"calc-diff","status":"fail","plan_version":1,"task_kind":"evaluate","iter":2,"attempts":1,"cost_usd":0.0,"note":"calc_diff 0.004 over threshold","secs":455.0}"#,
        r#"{"v":1,"kind":"task_result","task":"grade","status":"fail","plan_version":1,"task_kind":"engine_grade","iter":2,"attempts":1,"cost_usd":0.0,"note":"terminal rung failed","secs":0.1}"#,
        r#"{"v":1,"kind":"task_result","task":"decide","status":"skipped","plan_version":1,"task_kind":"engine_decide","iter":2,"attempts":0,"cost_usd":0.0,"note":"measurement failed","secs":0.0}"#,
    ]
    .join("\n");
    crucible_controller::runs::blob_store::put_run_session(db.pool(), "run-demo-1", log.as_bytes())
        .await?;
    crucible_controller::runs::ingest::ingest_session(
        db,
        &crucible_controller::runs::ingest::IngestTarget {
            run_id: "run-demo-1",
            scope_id: None,
            issue: None,
            pod: Some("loop-demo-1"),
            session_uri: &crucible_controller::runs::blob_store::run_session_uri("run-demo-1"),
        },
        &log,
    )
    .await?;
    Ok(())
}
