use super::*;
#[cfg(feature = "autoresearch")]
use crate::api::dto::*;
#[cfg(feature = "autoresearch")]
use crate::builds::model::NewBuild;
#[cfg(feature = "autoresearch")]
use crate::builds::model::{BuildBackendKind, BuildState};
use crate::client::Db;
#[cfg(feature = "autoresearch")]
use crate::daemon::queue::OverrideKind;
use crate::daemon::queue::{Override, OverrideSink};
#[cfg(feature = "autoresearch")]
use crate::issues::model::IssueQuery;
#[cfg(feature = "autoresearch")]
use crate::issues::model::{InputKind, NewIssue};
#[cfg(feature = "autoresearch")]
use crate::model::Status;
use crate::runs::model::{NewCandidate, NewRun};
use anyhow::Result;
use axum::Router;
use axum::body::Body;
use axum::http::Request as HttpRequest;
use axum::http::{StatusCode, header};
use axum::response::Response;
use sqlx::PgPool;
use std::sync::Arc;
use std::sync::Mutex;
use tower::ServiceExt;

/// A recording `OverrideSink` — a plain struct capturing calls, not a mock framework — for
/// asserting that POSTs enqueue overrides without ever touching the DB.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<Override>>,
}

impl OverrideSink for Recorder {
    fn submit(&self, ov: Override) {
        self.calls.lock().expect("lock").push(ov);
    }
}

fn db_with(pool: PgPool) -> (Db, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::new(pool);
    (db, dir)
}

#[cfg(feature = "autoresearch")]
fn sample_issue(key: &str) -> NewIssue {
    NewIssue {
        key: key.to_string(),
        repo: "owner/repo".to_string(),
        priority: 5,
        evidence_url: Some("https://github.com/owner/repo/issues/1".to_string()),
        title: Some("a sample issue".to_string()),
        author: Some("octocat".to_string()),
        body: None,
        labels: Vec::new(),
        upstream_updated_at: None,
    }
}

fn app(db: Db, sink: Arc<dyn OverrideSink>) -> Router {
    router(ApiState::test(db, sink))
}

fn app_with_caps(db: Db, caps: Option<Caps>) -> Router {
    router(ApiState {
        caps,
        ..ApiState::test(db, Arc::new(Recorder::default()))
    })
}

fn app_with_admins(db: Db, admins: Vec<String>) -> Router {
    router(ApiState {
        roles: crate::identity::auth::Roles::new(admins, vec![], vec![]),
        ..ApiState::test(db, Arc::new(Recorder::default()))
    })
}

/// The broker-contract rig: an admin plus a configured contract set, so the adopt tests can drive
/// both the accepted and the unknown-name branches without touching any other knob.
fn app_with_contracts(
    db: Db,
    admins: Vec<String>,
    contracts: crate::config::BrokerContracts,
) -> Router {
    router(ApiState {
        roles: crate::identity::auth::Roles::new(admins, vec![], vec![]),
        broker_contracts: contracts,
        ..ApiState::test(db, Arc::new(Recorder::default()))
    })
}

/// One configured contract, `deepgemm`, with a body small enough to assert on verbatim.
#[cfg(feature = "autoresearch")]
fn test_contracts() -> crate::config::BrokerContracts {
    crate::config::BrokerContracts::from_map(std::collections::BTreeMap::from([(
        "deepgemm".to_string(),
        r#"{"gpus":1,"build":{"src_dir":"/opt/deepgemm"}}"#.to_string(),
    )]))
}

fn app_with_admins_and_jira(
    db: Db,
    admins: Vec<String>,
    jira: Option<crate::launches::jira::JiraConfig>,
) -> Router {
    router(ApiState {
        roles: crate::identity::auth::Roles::new(admins, vec![], vec![]),
        jira,
        ..ApiState::test(db, Arc::new(Recorder::default()))
    })
}

/// Spawn a one-shot local HTTP listener that answers any request with a canned
/// `/rest/api/2/issue/{KEY}` payload — the real reqwest path, no mocked client. Returns its base URL.
#[cfg(feature = "autoresearch")]
async fn spawn_mock_jira(
    summary: &str,
    description: &str,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock jira");
    let addr = listener.local_addr().expect("mock jira addr");
    let payload =
        serde_json::json!({"fields": {"summary": summary, "description": description}}).to_string();
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = vec![0u8; 4096];
        let _ = socket.read(&mut buf).await;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let _ = socket.write_all(resp.as_bytes()).await;
        let _ = socket.flush().await;
    });
    (format!("http://{addr}"), handle)
}

fn app_with_roles(
    db: Db,
    sink: Arc<dyn OverrideSink>,
    admins: Vec<String>,
    operators: Vec<String>,
) -> Router {
    router(ApiState {
        roles: crate::identity::auth::Roles::new(admins, operators, vec![]),
        ..ApiState::test(db, sink)
    })
}

/// The repo-watch-set (Lane O3) test rig: an admin (and, optionally, an operator) plus an
/// explicit whitelist, so `POST /api/repos` tests control both the org whitelist and the
/// caller's role independently.
#[cfg(feature = "autoresearch")]
fn app_with_repo_whitelist(
    db: Db,
    admins: Vec<String>,
    operators: Vec<String>,
    whitelist: crate::issues::repo_ref::RepoWhitelist,
) -> Router {
    router(ApiState {
        roles: crate::identity::auth::Roles::new(admins, operators, vec![]),
        repo_whitelist: Arc::new(whitelist),
        ..ApiState::test(db, Arc::new(Recorder::default()))
    })
}

/// Same as [`app`] but with a real `scratch_dir` for the flow-cache endpoints.
fn app_with_scratch_dir(db: Db, scratch_dir: std::path::PathBuf) -> Router {
    router(ApiState {
        scratch_dir,
        ..ApiState::test(db, Arc::new(Recorder::default()))
    })
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn healthz_reports_ok(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app(db, Arc::new(Recorder::default()));
    let res = app
        .oneshot(HttpRequest::get("/healthz").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["status"], "ok");
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn issue_facets_count_each_axis_without_its_own_filter(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    for key in ["owner/repo#1", "owner/repo#2", "owner/repo#3"] {
        crate::issues::store::upsert_issue(db.pool(), &sample_issue(key)).await?;
    }
    assert!(
        crate::issues::store::claim_issue(db.pool(), "owner/repo#2", Status::New, Status::Scoped)
            .await?
    );
    assert!(
        crate::issues::store::claim_issue(db.pool(), "owner/repo#3", Status::New, Status::Scoped)
            .await?
    );
    let app = app(db, Arc::new(Recorder::default()));

    let (st, v) = get_json_object(&app, "/api/issues/facets?status=scoped").await;
    assert_eq!(st, StatusCode::OK);

    // `total` honours every filter, so it sees only the two scoped rows.
    assert_eq!(v["total"], 2);

    // The status facet drops its own filter, so it still reports the unscoped row — that count is
    // what the user would get by switching to it, which is the whole point of showing it.
    let status = v["status"].as_array().expect("status facet");
    let count_for = |field: &serde_json::Value, want: &str| -> i64 {
        field
            .as_array()
            .expect("facet array")
            .iter()
            .find(|f| f["value"] == want)
            .map(|f| f["count"].as_i64().unwrap_or_default())
            .unwrap_or_default()
    };
    assert_eq!(status.len(), 2);
    assert_eq!(count_for(&v["status"], "scoped"), 2);
    assert_eq!(count_for(&v["status"], "new"), 1);

    // Every other facet stays narrowed by the active status filter.
    assert_eq!(count_for(&v["repo"], "owner/repo"), 2);
    assert_eq!(count_for(&v["kind"], "github"), 2);

    let (st, _) = get_json_object(&app, "/api/issues/facets?status=bogus").await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn list_issues_filters_by_status(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#2")).await?;
    assert!(
        crate::issues::store::claim_issue(db.pool(), "owner/repo#2", Status::New, Status::Scoped)
            .await?
    );
    let app = app(db, Arc::new(Recorder::default()));

    let res = app
        .clone()
        .oneshot(HttpRequest::get("/api/issues?status=scoped").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
    assert_eq!(v.len(), 1);
    assert_eq!(v[0]["key"], "owner/repo#2");

    let res = app
        .oneshot(HttpRequest::get("/api/issues?status=bogus").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn list_issues_upstream_filters_validate_and_apply(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let mut iss = sample_issue("owner/repo#1");
    iss.upstream_updated_at = Some("2026-06-01T00:00:00Z".to_string());
    crate::issues::store::upsert_issue(db.pool(), &iss).await?;
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#2")).await?;
    crate::issues::transitions::park_and_purge(
        db.pool(),
        "owner/repo#2",
        &crate::model::ParkReason::UpstreamClosed.to_string(),
        crate::model::ParkedBy::Machine,
    )
    .await?;
    let app = app(db, Arc::new(Recorder::default()));

    for bad in [
        "/api/issues?upstream=bogus",
        "/api/issues?upstream_since=notadate",
    ] {
        let res = app
            .clone()
            .oneshot(HttpRequest::get(bad).body(Body::empty())?)
            .await?;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{bad}");
    }

    let res = app
        .clone()
        .oneshot(
            HttpRequest::get(
                "/api/issues?upstream=open&upstream_since=2026-01-01T00:00:00Z&sort=upstream",
            )
            .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
    assert_eq!(v.len(), 1, "the retired row and the NULL relic are out");
    assert_eq!(v[0]["key"], "owner/repo#1");
    assert_eq!(v[0]["upstream_updated_at"], "2026-06-01T00:00:00Z");

    let res = app
        .oneshot(HttpRequest::get("/api/issues?upstream=closed").body(Body::empty())?)
        .await?;
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
    assert_eq!(v.len(), 1);
    assert_eq!(v[0]["key"], "owner/repo#2");
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn get_issue_detail_carries_body_and_comments(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let mut iss = sample_issue("owner/repo#1");
    iss.body = Some("## Steps\n\n1. run it\n2. watch it fail".to_string());
    crate::issues::store::upsert_issue(db.pool(), &iss).await?;
    crate::issues::store::replace_issue_comments(
        db.pool(),
        "owner/repo#1",
        &[crate::issues::model::IssueComment {
            id: 42,
            issue_key: "owner/repo#1".to_string(),
            author: Some("commenter".to_string()),
            created_at: "2026-07-01T00:00:00Z".to_string(),
            updated_at: "2026-07-01T00:00:00Z".to_string(),
            body: "same here on v0.9".to_string(),
        }],
    )
    .await?;
    let app = app(db, Arc::new(Recorder::default()));

    let res = app
        .oneshot(HttpRequest::get("/api/issues/owner%2Frepo%231").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["body"], "## Steps\n\n1. run it\n2. watch it fail");
    let comments = v["comments"].as_array().expect("comments array");
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["id"], 42);
    assert_eq!(comments[0]["author"], "commenter");
    assert_eq!(comments[0]["body"], "same here on v0.9");
    assert_eq!(comments[0]["created_at"], "2026-07-01T00:00:00Z");
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn get_issue_returns_full_provenance_or_404(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    crate::runs::store::insert_run(
        db.pool(),
        &NewRun {
            run_id: "run-1".to_string(),
            scope: None,
            issue: None,
            identity_digest: None,
            status: "done".to_string(),
            pod: None,
            session_uri: None,
            best_score: Some(240.0),
            cost_usd: Some(1.0),
        },
    )
    .await?;
    crate::runs::store::insert_candidate(
        db.pool(),
        &NewCandidate {
            run_id: "run-1".to_string(),
            kind: Some("wide".to_string()),
            lane: Some(0),
            iter: Some(1),
            score: Some(240.0),
            decision: Some("keep".to_string()),
            worktree: None,
            sandbox: None,
            pr_url: None,
            branch: None,
        },
    )
    .await?;
    let app = app(db, Arc::new(Recorder::default()));

    let res = app
        .clone()
        .oneshot(HttpRequest::get("/api/issues/owner/repo#1").body(Body::empty())?)
        .await?;
    // The key contains a literal `/`, so it must be percent-encoded to route correctly.
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    let res = app
        .clone()
        .oneshot(HttpRequest::get("/api/issues/owner%2Frepo%231").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["issue"]["key"], "owner/repo#1");
    assert_eq!(v["scopes"].as_array().unwrap().len(), 0);

    let res = app
        .oneshot(HttpRequest::get("/api/issues/nope%2399").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn list_runs_carries_the_score_series_in_iteration_order(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    for (run_id, best) in [("run-a", 234.0), ("run-empty", 0.0)] {
        crate::runs::store::insert_run(
            db.pool(),
            &NewRun {
                run_id: run_id.to_string(),
                scope: None,
                issue: None,
                identity_digest: None,
                status: "done".to_string(),
                pod: None,
                session_uri: None,
                best_score: Some(best),
                cost_usd: Some(1.0),
            },
        )
        .await?;
    }
    // Inserted out of order, and one candidate never measured, so the endpoint has to sort by
    // iteration and drop the null rather than echo insertion order.
    for (iter, score) in [
        (2, Some(295.0)),
        (0, Some(358.0)),
        (3, None),
        (1, Some(340.0)),
    ] {
        crate::runs::store::insert_candidate(
            db.pool(),
            &NewCandidate {
                run_id: "run-a".to_string(),
                kind: Some("deep".to_string()),
                lane: Some(0),
                iter: Some(iter),
                score,
                decision: Some("keep".to_string()),
                worktree: None,
                sandbox: None,
                pr_url: None,
                branch: None,
            },
        )
        .await?;
    }

    let app = app(db, Arc::new(Recorder::default()));
    let (st, v) = get_json(&app, "/api/runs").await;
    assert_eq!(st, StatusCode::OK);

    let by_id = |id: &str| {
        v.iter()
            .find(|r| r["run_id"] == id)
            .expect("run present")
            .clone()
    };

    assert_eq!(
        by_id("run-a")["score_series"],
        serde_json::json!([358.0, 340.0, 295.0])
    );
    assert_eq!(by_id("run-empty")["score_series"], serde_json::json!([]));
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn list_runs_returns_most_recent_with_default_and_explicit_limit(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::runs::store::insert_run(
        db.pool(),
        &NewRun {
            run_id: "run-1".to_string(),
            scope: None,
            issue: None,
            identity_digest: None,
            status: "done".to_string(),
            pod: Some("loop-1".to_string()),
            session_uri: None,
            best_score: None,
            cost_usd: None,
        },
    )
    .await?;
    crate::runs::store::insert_run(
        db.pool(),
        &NewRun {
            run_id: "run-2".to_string(),
            scope: None,
            issue: None,
            identity_digest: None,
            status: "done".to_string(),
            pod: Some("loop-2".to_string()),
            session_uri: None,
            best_score: None,
            cost_usd: None,
        },
    )
    .await?;
    let app = app(db, Arc::new(Recorder::default()));

    let res = app
        .clone()
        .oneshot(HttpRequest::get("/api/runs").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
    assert_eq!(v.len(), 2);
    assert_eq!(v[0]["run_id"], "run-2", "newest first");
    assert_eq!(v[1]["run_id"], "run-1");

    let res = app
        .oneshot(HttpRequest::get("/api/runs?limit=1").body(Body::empty())?)
        .await?;
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
    assert_eq!(v.len(), 1);
    assert_eq!(v[0]["run_id"], "run-2");
    Ok(())
}

/// Seed a run under a repo (issue → scope → run), so the leaderboard join resolves issue_key + repo.
#[cfg(feature = "autoresearch")]
async fn seed_scoped_run(
    db: &Db,
    repo: &str,
    issue_key: &str,
    run_id: &str,
    best: Option<f64>,
    cost: Option<f64>,
) -> Result<()> {
    crate::issues::store::upsert_issue(
        db.pool(),
        &NewIssue {
            key: issue_key.to_string(),
            repo: repo.to_string(),
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
    crate::issues::store::claim_issue(db.pool(), issue_key, Status::New, Status::Scoped).await?;
    let scope_id = crate::issues::store::insert_scope(
        db.pool(),
        &crate::issues::model::NewScope {
            issue: issue_key.to_string(),
            pack_digest: None,
            check_outcome: None,
        },
    )
    .await?;
    crate::runs::store::insert_run(
        db.pool(),
        &NewRun {
            run_id: run_id.to_string(),
            scope: Some(scope_id),
            issue: None,
            identity_digest: None,
            status: "finished".to_string(),
            pod: None,
            session_uri: None,
            best_score: best,
            cost_usd: cost,
        },
    )
    .await?;
    Ok(())
}

/// [`get_json_as`] the same administrator the write helpers act as.
async fn get_json(app: &Router, uri: &str) -> (StatusCode, Vec<serde_json::Value>) {
    get_json_as(app, uri, "wren").await
}

/// A read as a named user; a non-200 answer reads as an empty list.
async fn get_json_as(app: &Router, uri: &str, user: &str) -> (StatusCode, Vec<serde_json::Value>) {
    let (status, body) = send(app, "GET", uri, user, None).await;
    let v = if status == StatusCode::OK {
        serde_json::from_slice(&body).expect("json")
    } else {
        Vec::new()
    };
    (status, v)
}

/// One request through the router as `user`: a JSON body when `body` is `Some`, the raw response
/// bytes back. Every other request helper decodes on top of this.
async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    user: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, axum::body::Bytes) {
    let mut req = HttpRequest::builder().method(method).uri(uri);
    if body.is_some() {
        req = req.header(header::CONTENT_TYPE, "application/json");
    }
    let req = req
        .header("x-auth-request-user", user)
        .body(body.map_or_else(Body::empty, |v| Body::from(v.to_string())))
        .expect("req");
    crate::testing::oneshot_bytes(app, req).await
}

/// [`get_json`] for the endpoints that answer with one object rather than an array.
async fn get_json_object(app: &Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let (status, body) = send(app, "GET", uri, "wren", None).await;
    let v = if status == StatusCode::OK {
        serde_json::from_slice(&body).expect("json")
    } else {
        serde_json::Value::Null
    };
    (status, v)
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn list_runs_page_sorts_filters_and_pages(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    // Time-first run ids so `created` (== run_id) order is meaningful; two repos for the filter.
    seed_scoped_run(
        &db,
        "o/a",
        "o/a#1",
        "20260704T100000Z-a",
        Some(300.0),
        Some(1.0),
    )
    .await?;
    seed_scoped_run(
        &db,
        "o/a",
        "o/a#2",
        "20260704T110000Z-b",
        Some(100.0),
        Some(3.0),
    )
    .await?;
    seed_scoped_run(
        &db,
        "o/b",
        "o/b#1",
        "20260704T120000Z-c",
        Some(200.0),
        Some(2.0),
    )
    .await?;
    // The newest run kept a PR; the leaderboard row should carry it, others stay null.
    crate::runs::store::insert_candidate(
        db.pool(),
        &NewCandidate {
            run_id: "20260704T120000Z-c".to_string(),
            kind: None,
            lane: None,
            iter: None,
            score: None,
            decision: Some("keep".to_string()),
            worktree: None,
            sandbox: None,
            pr_url: Some("https://github.com/o/b/pull/7".to_string()),
            branch: None,
        },
    )
    .await?;
    let app = app(db.clone(), Arc::new(Recorder::default()));

    // Default: sort=created desc → newest run_id first, and the join carries issue_key + repo.
    let (st, v) = get_json(&app, "/api/runs").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v.len(), 3);
    assert_eq!(v[0]["run_id"], "20260704T120000Z-c");
    assert_eq!(v[0]["repo"], "o/b");
    assert_eq!(v[0]["issue_key"], "o/b#1");
    assert_eq!(v[0]["created"], "2026-07-04T12:00:00Z");
    assert_eq!(v[0]["pr_url"], "https://github.com/o/b/pull/7");
    // A run with no kept PR reports a null pr_url.
    assert!(v[1]["pr_url"].is_null());

    // sort=best_score asc → 100, 200, 300.
    let (_st, v) = get_json(&app, "/api/runs?sort=best_score&dir=asc").await;
    let scores: Vec<f64> = v
        .iter()
        .map(|r| r["best_score"].as_f64().unwrap())
        .collect();
    assert_eq!(scores, vec![100.0, 200.0, 300.0]);

    // sort=cost desc → 3, 2, 1.
    let (_st, v) = get_json(&app, "/api/runs?sort=cost&dir=desc").await;
    let costs: Vec<f64> = v.iter().map(|r| r["cost_usd"].as_f64().unwrap()).collect();
    assert_eq!(costs, vec![3.0, 2.0, 1.0]);

    // repo filter.
    let (_st, v) = get_json(&app, "/api/runs?repo=o/a").await;
    assert_eq!(v.len(), 2);
    assert!(v.iter().all(|r| r["repo"] == "o/a"));

    crate::runs::store::set_run_location(
        db.pool(),
        "20260704T110000Z-b",
        &crate::runs::model::RunLocation::new("wharf", Some("runs".to_string())),
    )
    .await?;
    let (_st, v) = get_json(&app, "/api/runs?dispatch_target=wharf").await;
    assert_eq!(v.len(), 1);
    assert_eq!(v[0]["run_id"], "20260704T110000Z-b");

    // paging: limit + offset over created-desc.
    let (_st, v) = get_json(&app, "/api/runs?limit=1&offset=1").await;
    assert_eq!(v.len(), 1);
    assert_eq!(v[0]["run_id"], "20260704T110000Z-b");

    // bad sort key → 400.
    let (st, _v) = get_json(&app, "/api/runs?sort=bogus").await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    // Kind scoping: every seeded run is scoped (autoresearch), so the playbook view is empty,
    // the autoresearch view is the full set, and a bogus kind is the caller's error.
    let (st, v) = get_json(&app, "/api/runs?kind=autoresearch").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v.len(), 3);
    let (st, v) = get_json(&app, "/api/runs?kind=playbook").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v.len(), 0);
    let (st, _) = get_json(&app, "/api/runs?kind=bogus").await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    Ok(())
}

/// Seed a build under a fresh issue+scope, returning the build id (so the caller can pin it).
#[cfg(feature = "autoresearch")]
async fn seed_scoped_build(
    db: &Db,
    repo: &str,
    issue_key: &str,
    name: &str,
    backend: BuildBackendKind,
) -> Result<i64> {
    crate::issues::store::upsert_issue(
        db.pool(),
        &NewIssue {
            key: issue_key.to_string(),
            repo: repo.to_string(),
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
    let scope_id = crate::issues::store::insert_scope(
        db.pool(),
        &crate::issues::model::NewScope {
            issue: issue_key.to_string(),
            pack_digest: None,
            check_outcome: None,
        },
    )
    .await?;
    crate::builds::store::insert_build(
        db.pool(),
        &crate::builds::model::NewBuild {
            scope: Some(scope_id),
            name: name.to_string(),
            image: format!("quay.io/x/{name}"),
            tag: "e2e".to_string(),
            context_digest: format!("ctx-{name}"),
            backend,
            timeout_secs: 1800,
        },
    )
    .await
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn list_builds_filters_and_pages_and_per_issue(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    // Two issues; a cluster build that succeeded (pinned digest) and a github build that failed
    // (evidence pointer), plus a still-pending cluster build on the second issue.
    let ok = seed_scoped_build(&db, "o/a", "o/a#1", "loop", BuildBackendKind::Cluster).await?;
    crate::builds::store::set_build_succeeded(db.pool(), ok, "quay.io/x/loop@sha256:dead").await?;
    let bad =
        seed_scoped_build(&db, "o/a", "o/a#2", "web", BuildBackendKind::GithubActions).await?;
    crate::builds::store::set_build_failed(
        db.pool(),
        bad,
        BuildState::Failed,
        Some("https://github.com/o/a/actions/runs/9"),
    )
    .await?;
    let _pending =
        seed_scoped_build(&db, "o/a", "o/a#2", "cache", BuildBackendKind::Cluster).await?;
    let app = app(db, Arc::new(Recorder::default()));

    // No filter → all three, newest first (id desc): cache, web, loop.
    let (st, v) = get_json(&app, "/api/builds").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v.len(), 3);
    assert_eq!(v[0]["name"], "cache");
    assert_eq!(v[0]["issue_key"], "o/a#2");
    assert_eq!(v[0]["repo"], "o/a");
    assert_eq!(v[2]["name"], "loop");
    assert_eq!(v[2]["digest_ref"], "quay.io/x/loop@sha256:dead");
    assert_eq!(v[2]["state"], "succeeded");

    // state filter.
    let (_st, v) = get_json(&app, "/api/builds?state=failed").await;
    assert_eq!(v.len(), 1);
    assert_eq!(v[0]["name"], "web");
    assert_eq!(
        v[0]["evidence_url"],
        "https://github.com/o/a/actions/runs/9"
    );

    // backend filter.
    let (_st, v) = get_json(&app, "/api/builds?backend=cluster").await;
    assert_eq!(v.len(), 2);
    assert!(v.iter().all(|b| b["backend"] == "cluster"));

    // issue filter (query param form).
    let (_st, v) = get_json(&app, "/api/builds?issue=o/a%232").await;
    assert_eq!(v.len(), 2);
    assert!(v.iter().all(|b| b["issue_key"] == "o/a#2"));

    // paging.
    let (_st, v) = get_json(&app, "/api/builds?limit=1&offset=1").await;
    assert_eq!(v.len(), 1);
    assert_eq!(v[0]["name"], "web");

    // bad state → 400.
    let (st, _v) = get_json(&app, "/api/builds?state=bogus").await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // per-issue path endpoint, oldest first.
    let (st, v) = get_json(&app, "/api/issues/o%2Fa%232/builds").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v.len(), 2);
    assert_eq!(v[0]["name"], "web", "oldest first within the issue");
    assert_eq!(v[1]["name"], "cache");

    // per-issue on an unknown issue → 404.
    let (st, _v) = get_json(&app, "/api/issues/o%2Fa%23404/builds").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn rebuild_build_requires_admin(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let id = seed_scoped_build(&db, "o/a", "o/a#1", "loop", BuildBackendKind::Cluster).await?;
    crate::builds::store::set_build_failed(
        db.pool(),
        id,
        BuildState::Failed,
        Some("https://x/log"),
    )
    .await?;
    for user in [None, Some("bob")] {
        let app = app_with_admins(db.clone(), vec!["alice".to_string()]);
        let mut req = HttpRequest::post(format!("/api/builds/{id}/rebuild"));
        if let Some(u) = user {
            req = req.header("x-auth-request-user", u);
        }
        let res = app.oneshot(req.body(Body::empty())?).await?;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "user={user:?}");
    }
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn rebuild_build_404_on_unknown_id(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db, vec!["alice".to_string()]);
    let res = app
        .oneshot(
            HttpRequest::post("/api/builds/999/rebuild")
                .header("x-auth-request-user", "alice")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn rebuild_build_409_when_in_flight(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    // A freshly-inserted build starts `pending` — in flight, not terminal.
    let id = seed_scoped_build(&db, "o/a", "o/a#1", "loop", BuildBackendKind::Cluster).await?;
    let app = app_with_admins(db.clone(), vec!["alice".to_string()]);
    let res = app
        .clone()
        .oneshot(
            HttpRequest::post(format!("/api/builds/{id}/rebuild"))
                .header("x-auth-request-user", "alice")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::CONFLICT);

    // Same for `dispatched`.
    crate::builds::store::set_build_dispatched(db.pool(), id, "job-1").await?;
    let res = app
        .oneshot(
            HttpRequest::post(format!("/api/builds/{id}/rebuild"))
                .header("x-auth-request-user", "alice")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn rebuild_build_resets_row_and_unparks_the_issue_it_had_parked(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let id = seed_scoped_build(&db, "o/a", "o/a#1", "loop", BuildBackendKind::Cluster).await?;
    crate::builds::store::set_build_failed(
        db.pool(),
        id,
        BuildState::Failed,
        Some("https://x/log/9"),
    )
    .await?;
    // The issue was `building`, then the reconcile driver parked it on this exact build's
    // failure (the wording `reconcile_building` parks with).
    crate::issues::store::claim_issue(db.pool(), "o/a#1", Status::New, Status::Building).await?;
    crate::issues::transitions::park(
        db.pool(),
        db.events(),
        "o/a#1",
        Status::Building,
        &crate::model::ParkReason::ImageBuildFailed {
            evidence: Some("https://x/log/9".to_string()),
        },
        crate::model::ParkedBy::Machine,
    )
    .await?;

    let notify = Arc::new(tokio::sync::Notify::new());
    let app = app_with_reconcile(db.clone(), vec!["alice".to_string()], notify.clone());
    let res = app
        .oneshot(
            HttpRequest::post(format!("/api/builds/{id}/rebuild"))
                .header("x-auth-request-user", "alice")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["name"], "loop");
    assert_eq!(v["issue_key"], "o/a#1");
    assert_eq!(v["actor"], "alice");

    // The row is back to a never-dispatched `pending`.
    let row = crate::builds::store::get_build(db.pool(), id)
        .await?
        .expect("build still exists");
    assert_eq!(row.state, BuildState::Pending);
    assert!(row.dispatch_id.is_none());
    assert!(row.digest_ref.is_none());
    assert!(row.evidence_url.is_none());
    assert_eq!(row.dispatch_attempts, 0);

    // The issue is unparked back to `building`, the exact state reconcile drives builds from.
    let issue = crate::issues::store::get_issue(db.pool(), "o/a#1")
        .await?
        .expect("issue exists");
    assert_eq!(issue.status, Status::Building);

    // The daemon's manual-reconcile trigger fired.
    tokio::time::timeout(std::time::Duration::from_secs(1), notify.notified())
        .await
        .expect("rebuild must kick the reconcile pass");
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn rebuild_build_leaves_a_still_building_issue_alone(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    // A scope with two builds: one already succeeded (pinned); the issue is still `building`
    // (as if a sibling build were still in flight), not parked.
    let id = seed_scoped_build(&db, "o/a", "o/a#1", "loop", BuildBackendKind::Cluster).await?;
    crate::builds::store::set_build_succeeded(db.pool(), id, "quay.io/x/loop@sha256:dead").await?;
    crate::issues::store::claim_issue(db.pool(), "o/a#1", Status::New, Status::Building).await?;

    let app = app_with_admins(db.clone(), vec!["alice".to_string()]);
    let res = app
        .oneshot(
            HttpRequest::post(format!("/api/builds/{id}/rebuild"))
                .header("x-auth-request-user", "alice")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::ACCEPTED);

    let row = crate::builds::store::get_build(db.pool(), id)
        .await?
        .expect("build still exists");
    assert_eq!(row.state, BuildState::Pending);
    assert!(row.digest_ref.is_none());

    // Still `building` — no unpark to do, this call just reset the row.
    let issue = crate::issues::store::get_issue(db.pool(), "o/a#1")
        .await?
        .expect("issue exists");
    assert_eq!(issue.status, Status::Building);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn rebuild_build_409_when_issue_has_moved_past_building(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    // The build succeeded and its issue advanced all the way to `done` — `reconcile_building`
    // never runs for a `done` issue, so a reset row here would never be re-driven.
    let id = seed_scoped_build(&db, "o/a", "o/a#1", "loop", BuildBackendKind::Cluster).await?;
    crate::builds::store::set_build_succeeded(db.pool(), id, "quay.io/x/loop@sha256:dead").await?;
    crate::issues::store::claim_issue(db.pool(), "o/a#1", Status::New, Status::Building).await?;
    crate::issues::store::claim_issue(db.pool(), "o/a#1", Status::Building, Status::Running)
        .await?;
    crate::issues::store::claim_issue(db.pool(), "o/a#1", Status::Running, Status::Done).await?;

    let app = app_with_admins(db.clone(), vec!["alice".to_string()]);
    let res = app
        .oneshot(
            HttpRequest::post(format!("/api/builds/{id}/rebuild"))
                .header("x-auth-request-user", "alice")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::CONFLICT);

    // The row was NOT reset — no stranded `pending` row consuming a build_pod_cap slot.
    let row = crate::builds::store::get_build(db.pool(), id)
        .await?
        .expect("build still exists");
    assert_eq!(row.state, BuildState::Succeeded);
    let issue = crate::issues::store::get_issue(db.pool(), "o/a#1")
        .await?
        .expect("issue exists");
    assert_eq!(issue.status, Status::Done);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn rebuild_build_409_when_issue_parked_for_another_reason(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    // The build succeeded, but the issue is `parked` for a reason unrelated to this build (the
    // `parked_on_this_build` narrate check must not treat this as safe to reset).
    let id = seed_scoped_build(&db, "o/a", "o/a#1", "loop", BuildBackendKind::Cluster).await?;
    crate::builds::store::set_build_succeeded(db.pool(), id, "quay.io/x/loop@sha256:dead").await?;
    crate::issues::store::claim_issue(db.pool(), "o/a#1", Status::New, Status::Building).await?;
    crate::issues::store::claim_issue(db.pool(), "o/a#1", Status::Building, Status::Running)
        .await?;
    crate::issues::transitions::park(
        db.pool(),
        db.events(),
        "o/a#1",
        Status::Running,
        &crate::model::ParkReason::Legacy("unrelated: run crashed".to_string()),
        crate::model::ParkedBy::Machine,
    )
    .await?;

    let app = app_with_admins(db.clone(), vec!["alice".to_string()]);
    let res = app
        .oneshot(
            HttpRequest::post(format!("/api/builds/{id}/rebuild"))
                .header("x-auth-request-user", "alice")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::CONFLICT);

    let row = crate::builds::store::get_build(db.pool(), id)
        .await?
        .expect("build still exists");
    assert_eq!(row.state, BuildState::Succeeded);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn rebuild_build_409_when_no_scope(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    // A CLI-invoked build the controller recorded without an issue: no `building` reconcile
    // will ever pick a reset row for it back up.
    let id = crate::builds::store::insert_build(
        db.pool(),
        &NewBuild {
            scope: None,
            name: "loop".to_string(),
            image: "quay.io/x/loop".to_string(),
            tag: "latest".to_string(),
            context_digest: "sha256:abc".to_string(),
            backend: BuildBackendKind::Cluster,
            timeout_secs: 600,
        },
    )
    .await?;
    crate::builds::store::set_build_succeeded(db.pool(), id, "quay.io/x/loop@sha256:dead").await?;

    let app = app_with_admins(db.clone(), vec!["alice".to_string()]);
    let res = app
        .oneshot(
            HttpRequest::post(format!("/api/builds/{id}/rebuild"))
                .header("x-auth-request-user", "alice")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::CONFLICT);

    let row = crate::builds::store::get_build(db.pool(), id)
        .await?
        .expect("build still exists");
    assert_eq!(row.state, BuildState::Succeeded);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn run_iterations_ordered_by_iter_or_404(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::runs::store::insert_run(
        db.pool(),
        &NewRun {
            run_id: "run-1".to_string(),
            scope: None,
            issue: None,
            identity_digest: None,
            status: "finished".to_string(),
            pod: None,
            session_uri: None,
            best_score: None,
            cost_usd: None,
        },
    )
    .await?;
    // Insert out of iteration order; the endpoint must return them ordered by iter.
    for iter in [2_i64, 0, 1] {
        crate::runs::store::insert_candidate(
            db.pool(),
            &NewCandidate {
                run_id: "run-1".to_string(),
                kind: Some("deep".to_string()),
                lane: None,
                iter: Some(iter),
                score: Some(iter as f64),
                decision: Some("keep".to_string()),
                worktree: None,
                sandbox: None,
                pr_url: None,
                branch: None,
            },
        )
        .await?;
    }
    let app = app(db, Arc::new(Recorder::default()));

    let (st, v) = get_json(&app, "/api/runs/run-1/iterations").await;
    assert_eq!(st, StatusCode::OK);
    let iters: Vec<i64> = v.iter().map(|c| c["iter"].as_i64().unwrap()).collect();
    assert_eq!(iters, vec![0, 1, 2], "ordered by iter");

    let (st, _v) = get_json(&app, "/api/runs/nope/iterations").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn run_graph_returns_newest_plan_or_404(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let graph = r#"[{"name":"propose","kind":"agent","depends_on":[],"session":"solver","needs":"all","required":true},
                    {"name":"measure","kind":"command","depends_on":["propose"],"session":"","needs":"all","required":true}]"#;
    crate::runs::task_results::upsert_run_plan(db.pool(), "run-graph", 1, "[]").await?;
    crate::runs::task_results::upsert_run_plan(db.pool(), "run-graph", 2, graph).await?;
    for (iter, task, status) in [(0_i64, "propose", "pass"), (1, "measure", "fail")] {
        crate::runs::task_results::upsert_task_result(
            db.pool(),
            "run-graph",
            &crate::runs::model::TaskResult {
                iter,
                task: task.to_string(),
                status: status.to_string(),
                note: "n".to_string(),
                cost_usd: Some(0.25),
                secs: Some(3.0),
                blocked: None,
            },
        )
        .await?;
    }
    let app = app(db, Arc::new(Recorder::default()));

    let res = app
        .clone()
        .oneshot(HttpRequest::get("/api/runs/run-graph/graph").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["plan_version"], 2, "the replan wins");
    assert_eq!(v["tasks"][0]["name"], "propose");
    assert_eq!(v["tasks"][0]["session"], "solver");
    assert_eq!(v["tasks"][1]["depends_on"][0], "propose");
    assert_eq!(v["results"][0]["task"], "propose");
    assert_eq!(v["results"][1]["status"], "fail");
    assert_eq!(v["results"][1]["cost_usd"], 0.25);
    assert!(
        v.get("outputs").is_none(),
        "no exposure was stored for this run's revision, which is not the same wire shape as a \
         pack that declares no outputs: {v}"
    );

    // A legacy run that admitted no plan is indistinguishable from an unknown run here: both 404,
    // which is what lets the SPA hide the panel.
    let res = app
        .oneshot(HttpRequest::get("/api/runs/run-legacy/graph").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn get_run_returns_candidates_or_404(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    // A scope-less run: issue_key/repo resolve to null.
    crate::runs::store::insert_run(
        db.pool(),
        &NewRun {
            run_id: "run-1".to_string(),
            scope: None,
            issue: None,
            identity_digest: None,
            status: "running".to_string(),
            pod: Some("loop-abc".to_string()),
            session_uri: None,
            best_score: None,
            cost_usd: None,
        },
    )
    .await?;
    // A scoped run: the join resolves its issue key + repo onto the DTO.
    seed_scoped_run(&db, "o/x", "o/x#7", "run-2", Some(200.0), Some(1.5)).await?;
    let app = app(db, Arc::new(Recorder::default()));

    let res = app
        .clone()
        .oneshot(HttpRequest::get("/api/runs/run-1").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["run"]["pod"], "loop-abc");
    assert!(v["run"]["issue_key"].is_null());
    assert!(v["run"]["repo"].is_null());

    let res = app
        .clone()
        .oneshot(HttpRequest::get("/api/runs/run-2").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["run"]["issue_key"], "o/x#7");
    assert_eq!(v["run"]["repo"], "o/x");

    let res = app
        .oneshot(HttpRequest::get("/api/runs/nope").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// A run detail echoes the pair its issue pinned, in that order, so the run page says which
/// provider and model the work was committed to rather than leaving a reader to guess.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn get_run_echoes_the_issues_pinned_pair(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    seed_scoped_run(&db, "o/x", "o/x#7", "run-2", None, None).await?;
    crate::issues::store::set_agent_dispatch(
        db.pool(),
        "o/x#7",
        Some("plat-openai"),
        Some("gpt-5.6-sol"),
    )
    .await?;
    let app = app(db, Arc::new(Recorder::default()));

    let res = app
        .oneshot(HttpRequest::get("/api/runs/run-2").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["run"]["agent_provider"], "plat-openai");
    assert_eq!(v["run"]["agent_model"], "gpt-5.6-sol");
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn live_run_404s_unknown_and_ends_non_running(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    // A finished (non-running) run: nothing live to stream.
    crate::runs::store::insert_run(
        db.pool(),
        &NewRun {
            run_id: "run-done".to_string(),
            scope: None,
            issue: None,
            identity_digest: None,
            status: "done".to_string(),
            pod: Some("loop-done".to_string()),
            session_uri: None,
            best_score: Some(240.0),
            cost_usd: Some(1.0),
        },
    )
    .await?;
    let app = app(db, Arc::new(Recorder::default()));

    // Unknown run → 404 (distinguishable from a known-but-non-streamable run).
    let res = app
        .clone()
        .oneshot(HttpRequest::get("/api/runs/nope/live").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // Known non-running run → 200 SSE with a single terminal `end{run-not-running}` event.
    let res = app
        .oneshot(HttpRequest::get("/api/runs/run-done/live").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let text = String::from_utf8(body.to_vec())?;
    assert!(text.contains("event: end"), "{text}");
    assert!(text.contains("data: run-not-running"), "{text}");
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn ledger_summary_sums_by_day(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    db.ledger_append(Some("run-1"), "scope", 0.5).await?;
    db.ledger_append(Some("run-1"), "run", 1.25).await?;
    let app = app(db, Arc::new(Recorder::default()));

    let res = app
        .oneshot(HttpRequest::get("/api/ledger/summary").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    let days = v["days"].as_array().unwrap();
    assert_eq!(days.len(), 1);
    assert!((days[0]["total_usd"].as_f64().unwrap() - 1.75).abs() < 1e-9);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn ledger_by_tag_groups_today_and_carries_the_ceiling(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    db.ledger_append(None, "rank-grounded", 0.52).await?;
    db.ledger_append(None, "rank-grounded", 0.48).await?;
    db.ledger_append(Some("run-1"), "run", 2.0).await?;
    let app = app_with_caps(
        db,
        Some(Caps {
            max_concurrent_pods: 2,
            max_scopes_per_day: 3,
            daily_cost_ceiling: 25.0,
        }),
    );

    let res = app
        .oneshot(HttpRequest::get("/api/ledger/by-tag").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["ceiling"], 25.0);
    assert_eq!(v["day"].as_str().unwrap().len(), "2026-07-04".len());
    let tags = v["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 2);
    // Biggest spender first.
    assert_eq!(tags[0]["tag"], "run");
    assert!((tags[0]["total_usd"].as_f64().unwrap() - 2.0).abs() < 1e-9);
    assert_eq!(tags[1]["tag"], "rank-grounded");
    assert!((tags[1]["total_usd"].as_f64().unwrap() - 1.0).abs() < 1e-9);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn ledger_by_tag_without_caps_has_null_ceiling(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app(db, Arc::new(Recorder::default()));

    let res = app
        .oneshot(HttpRequest::get("/api/ledger/by-tag").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert!(v["ceiling"].is_null());
    assert_eq!(v["tags"].as_array().unwrap().len(), 0);
    Ok(())
}

/// Seed one `work_pods` row in `state` (routed through the insert + a state advance, exactly
/// as dispatch does), carrying `error` when failed.
#[cfg(feature = "autoresearch")]
async fn seed_work_pod(
    db: &Db,
    pod_name: &str,
    kind: &str,
    issue_key: &str,
    state: crate::runs::workpod::WorkPodState,
    error: Option<&str>,
) -> Result<()> {
    crate::runs::work_pods::insert_work_pod(
        db.pool(),
        &crate::runs::workpod::NewWorkPod {
            pod_name: pod_name.to_string(),
            kind: kind.to_string(),
            issue_key: Some(issue_key.to_string()),
            state: crate::runs::workpod::WorkPodState::Running,
            cost_tag: kind.to_string(),
            cluster: "hub".to_string(),
        },
    )
    .await?;
    if state != crate::runs::workpod::WorkPodState::Running {
        crate::runs::work_pods::set_work_pod_state(db.pool(), pod_name, state, None, error).await?;
    }
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn list_turns_filters_orders_and_limits(pool: PgPool) -> Result<()> {
    use crate::runs::workpod::WorkPodState as S;
    let (db, _d) = db_with(pool);
    // Same-second created_at stamps tie-break on pod_name DESC, so insertion order and name
    // order agree here: c is newest either way.
    seed_work_pod(
        &db,
        "turn-a",
        "grounded-rank",
        "o/r#1",
        S::Failed,
        Some("no verdict"),
    )
    .await?;
    seed_work_pod(&db, "turn-b", "scope", "o/r#2", S::Collected, None).await?;
    seed_work_pod(&db, "turn-c", "grounded-rank", "o/r#3", S::Running, None).await?;
    let app = app(db, Arc::new(Recorder::default()));

    // Unfiltered: every row, newest first, full shape.
    let (st, v) = get_json(&app, "/api/turns").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v.len(), 3);
    assert_eq!(v[0]["pod_name"], "turn-c");
    assert_eq!(v[0]["state"], "running");
    assert_eq!(v[0]["kind"], "grounded-rank");
    assert_eq!(v[0]["issue_key"], "o/r#3");
    assert_eq!(v[0]["cost_tag"], "grounded-rank");
    assert!(v[0]["terminal_at"].is_null());
    assert!(v[0]["created_at"].as_str().is_some());

    // state filter: the failed row carries its failure reason + terminal stamp.
    let (st, v) = get_json(&app, "/api/turns?state=failed").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0]["pod_name"], "turn-a");
    // The list wire carries `error` as a LongText; a short reason rides through whole.
    assert_eq!(v[0]["error"]["text"], "no verdict");
    assert_eq!(v[0]["error"]["truncated"], false);
    assert!(v[0]["terminal_at"].as_str().is_some());

    // kind filter.
    let (_st, v) = get_json(&app, "/api/turns?kind=scope").await;
    assert_eq!(v.len(), 1);
    assert_eq!(v[0]["pod_name"], "turn-b");

    // Combined filters intersect.
    let (_st, v) = get_json(&app, "/api/turns?state=failed&kind=scope").await;
    assert_eq!(v.len(), 0);

    // limit caps the page.
    let (_st, v) = get_json(&app, "/api/turns?limit=2").await;
    assert_eq!(v.len(), 2);

    // Out-of-vocabulary values 400 loudly instead of matching nothing.
    let (st, _v) = get_json(&app, "/api/turns?state=exploded").await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _v) = get_json(&app, "/api/turns?kind=bogus").await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    Ok(())
}

/// The list/detail split for long free text: `GET /api/turns` serves a flagged preview, and the
/// row expansion's `GET /api/turns/{pod_name}` serves the whole thing.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn turns_list_previews_the_error_and_the_detail_serves_it_whole(pool: PgPool) -> Result<()> {
    use crate::runs::workpod::WorkPodState as S;
    let (db, _d) = db_with(pool);
    // A realistic failure: an anyhow chain wrapping a pod log tail, far past the preview cut.
    let long_error = format!(
        "run failed: the wrapper exited non-zero. Last pod log lines:\n{}",
        (0..10)
            .map(|i| format!("2026-01-15T10:15:0{i}Z crucible-wrapper: step {i} of the loop"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    seed_work_pod(&db, "turn-a", "run", "o/r#1", S::Failed, Some(&long_error)).await?;
    let app = app(db, Arc::new(Recorder::default()));

    let (st, v) = get_json(&app, "/api/turns").await;
    assert_eq!(st, StatusCode::OK);
    let preview = v[0]["error"]["text"].as_str().expect("error text");
    assert_eq!(v[0]["error"]["truncated"], true);
    assert!(preview.chars().count() <= LIST_TRUNCATE_CHARS + 1);
    assert!(preview.ends_with('…'));
    assert!(long_error.starts_with(preview.trim_end_matches('…')));

    let (st, v) = get_json_object(&app, "/api/turns/turn-a").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["error"]["text"], long_error);
    assert_eq!(v["error"]["truncated"], false);
    assert_eq!(v["pod_name"], "turn-a");

    let (st, _v) = get_json_object(&app, "/api/turns/no-such-pod").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    Ok(())
}

/// The same split for `GET /api/issues`: a parked reason embeds a pod log tail, and the whole
/// backlog ships on one unpaginated response. The list previews it; the detail keeps it whole.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn issues_list_previews_the_parked_reason_and_the_detail_serves_it_whole(
    pool: PgPool,
) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    let long_reason = crate::model::ParkReason::NoSessionEmpty {
        run_id: "20260115T101500Z-owner-repo-1".to_string(),
        rc: Some(1),
        tail: (0..10)
            .map(|i| format!("2026-01-15T10:15:0{i}Z crucible-wrapper: step {i} of the loop"))
            .collect::<Vec<_>>()
            .join("\n"),
    }
    .to_string();
    crate::issues::store::park_issue(
        db.pool(),
        "owner/repo#1",
        &long_reason,
        crate::model::ParkedBy::Machine,
        "2026-01-15T10:15:00Z",
    )
    .await?;
    let app = app(db, Arc::new(Recorder::default()));

    let (st, v) = get_json(&app, "/api/issues").await;
    assert_eq!(st, StatusCode::OK);
    let preview = v[0]["parked_reason"]["text"].as_str().expect("reason text");
    assert_eq!(v[0]["parked_reason"]["truncated"], true);
    assert!(preview.chars().count() <= LIST_TRUNCATE_CHARS + 1);
    assert!(long_reason.starts_with(preview.trim_end_matches('…')));

    let (st, v) = get_json_object(&app, "/api/issues/owner%2Frepo%231").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["issue"]["parked_reason"]["text"], long_reason);
    assert_eq!(v["issue"]["parked_reason"]["truncated"], false);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn list_events_returns_the_tail_newest_first_with_keys(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    crate::issues::transitions::transition(
        db.pool(),
        db.events(),
        "owner/repo#1",
        Status::New,
        Status::Scoped,
        Some("older"),
        None,
    )
    .await?;
    crate::issues::transitions::transition(
        db.pool(),
        db.events(),
        "owner/repo#1",
        Status::Scoped,
        Status::Parked,
        Some("newer"),
        None,
    )
    .await?;
    let app = app(db, Arc::new(Recorder::default()));

    let res = app
        .clone()
        .oneshot(HttpRequest::get("/api/events").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
    assert_eq!(v.len(), 2);
    assert_eq!(v[0]["reason"]["text"], "newer", "newest first");
    assert_eq!(v[0]["key"], "owner/repo#1", "feed rows carry the key");

    let res = app
        .oneshot(HttpRequest::get("/api/events?limit=1").body(Body::empty())?)
        .await?;
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
    assert_eq!(v.len(), 1);
    assert_eq!(v[0]["reason"]["text"], "newer");
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn events_stream_emits_a_transition_as_sse_json(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    let app = app(db.clone(), Arc::new(Recorder::default()));

    let res = app
        .oneshot(HttpRequest::get("/api/events/stream").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        res.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("text/event-stream"))
    );

    // The subscription is live once the response exists; a transition now must arrive as one
    // SSE frame whose data line is the EventDto JSON.
    crate::issues::transitions::transition(
        db.pool(),
        db.events(),
        "owner/repo#1",
        Status::New,
        Status::Scoped,
        Some("pack passed"),
        None,
    )
    .await?;
    let mut body = res.into_body().into_data_stream();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        use futures_util::StreamExt;
        loop {
            let chunk = body
                .next()
                .await
                .expect("stream stays open")
                .expect("chunk ok");
            let text = String::from_utf8(chunk.to_vec()).expect("utf8");
            if text.contains("data:") {
                return text;
            }
        }
    })
    .await
    .expect("an event frame arrives");
    let json_line = frame
        .lines()
        .find_map(|l| l.strip_prefix("data: "))
        .expect("a data line");
    let v: serde_json::Value = serde_json::from_str(json_line)?;
    assert_eq!(v["key"], "owner/repo#1");
    assert_eq!(v["to"], "scoped");
    assert_eq!(v["reason"]["text"], "pack passed");
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn park_json_enqueues_override_without_touching_the_db(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    let recorder = Arc::new(Recorder::default());
    let app = app_with_roles(
        db.clone(),
        recorder.clone(),
        vec![],
        vec!["operator".to_string()],
    );

    let res = app
        .oneshot(
            HttpRequest::post("/api/issues/owner%2Frepo%231/park")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "operator")
                .body(Body::from(r#"{"reason":"no repro"}"#))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["action"], "park");
    assert_eq!(v["reason"], "no repro");

    {
        let calls = recorder.calls.lock().expect("lock");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].key.0, "owner/repo#1");
        assert_eq!(calls[0].kind, OverrideKind::Park);
        assert_eq!(calls[0].reason.as_deref(), Some("no repro"));
    }

    // The POST must never write the DB: the issue is still `new`, not `parked`.
    let issue = crate::issues::store::get_issue(db.pool(), "owner/repo#1")
        .await?
        .expect("issue");
    assert_eq!(issue.status, Status::New, "park POST does not write the DB");
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn park_carries_the_proxy_identity_as_the_actor(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    let recorder = Arc::new(Recorder::default());
    let app = app_with_roles(db, recorder.clone(), vec![], vec!["wren".to_string()]);

    let res = app
        .oneshot(
            HttpRequest::post("/api/issues/owner%2Frepo%231/park")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(r#"{"reason":"no repro"}"#))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["actor"], "wren", "ack echoes the actor");

    let calls = recorder.calls.lock().expect("lock");
    assert_eq!(calls[0].actor.as_deref(), Some("wren"));
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn park_form_redirects_and_enqueues(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    let recorder = Arc::new(Recorder::default());
    let app = app_with_roles(
        db.clone(),
        recorder.clone(),
        vec![],
        vec!["operator".to_string()],
    );

    let res = app
        .oneshot(
            HttpRequest::post("/api/issues/owner%2Frepo%231/park")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header("x-auth-request-user", "operator")
                .body(Body::from("reason=no+repro"))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/inbox");

    {
        let calls = recorder.calls.lock().expect("lock");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].reason.as_deref(), Some("no repro"));
    }
    let issue = crate::issues::store::get_issue(db.pool(), "owner/repo#1")
        .await?
        .expect("issue");
    assert_eq!(issue.status, Status::New, "park POST does not write the DB");
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn unpark_and_bump_enqueue_without_a_body(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    let recorder = Arc::new(Recorder::default());
    let app = app_with_roles(db, recorder.clone(), vec![], vec!["operator".to_string()]);

    let res = app
        .clone()
        .oneshot(
            HttpRequest::post("/api/issues/owner%2Frepo%231/unpark")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header("x-auth-request-user", "operator")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);

    let res = app
        .oneshot(
            HttpRequest::post("/api/issues/owner%2Frepo%231/bump")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "operator")
                .body(Body::from(r#"{"priority":9}"#))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::ACCEPTED);

    let calls = recorder.calls.lock().expect("lock");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].kind, OverrideKind::Unpark);
    assert_eq!(calls[1].kind, OverrideKind::Bump);
    assert_eq!(calls[1].priority, Some(9));
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn overview_returns_counts_without_caps(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#2")).await?;
    crate::issues::store::claim_issue(db.pool(), "owner/repo#2", Status::New, Status::Running)
        .await?;
    db.ledger_append(None, "test", 1.5).await?;

    let app = app_with_caps(db, None);
    let res = app
        .oneshot(HttpRequest::get("/api/overview").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let overview: Overview = serde_json::from_slice(&body)?;

    assert_eq!(overview.statuses.len(), 2);
    assert_eq!(overview.running.current, 1);
    assert!(overview.running.cap.is_none());
    assert!(overview.scopes_today.cap.is_none());
    assert!(overview.cost_today.ceiling.is_none());
    assert_eq!(overview.cost_today.current, 1.5);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn overview_includes_caps_when_present(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;

    let app = app_with_caps(
        db,
        Some(Caps {
            max_concurrent_pods: 5,
            max_scopes_per_day: 20,
            daily_cost_ceiling: 50.0,
        }),
    );
    let res = app
        .oneshot(HttpRequest::get("/api/overview").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let overview: Overview = serde_json::from_slice(&body)?;

    assert_eq!(overview.running.cap, Some(5));
    assert_eq!(overview.scopes_today.cap, Some(20));
    assert_eq!(overview.cost_today.ceiling, Some(50.0));
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn funnel_returns_a_stage_for_every_pipeline_bucket(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?; // discovered
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#2")).await?;
    crate::issues::store::claim_issue(db.pool(), "owner/repo#2", Status::New, Status::Running)
        .await?; // running

    let app = app_with_caps(db, None);
    let res = app
        .oneshot(HttpRequest::get("/api/funnel").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let funnel: FunnelDto = serde_json::from_slice(&body)?;

    let keys: Vec<&str> = funnel.stages.iter().map(|s| s.key.as_str()).collect();
    assert_eq!(
        keys,
        vec![
            "discovered",
            "ranked",
            "awaiting_approval",
            "running",
            "pr_open",
            "done",
            "parked",
            "stale",
        ]
    );
    let by_key: std::collections::HashMap<_, _> =
        funnel.stages.iter().map(|s| (s.key.as_str(), s)).collect();
    assert_eq!(by_key["discovered"].count, 1);
    assert_eq!(by_key["running"].count, 1);
    assert_eq!(by_key["ranked"].count, 0);
    assert_eq!(by_key["parked"].count, 0);
    assert_eq!(by_key["stale"].count, 0);
    Ok(())
}

#[tokio::test]
async fn auth_guard_rejects_without_bearer_and_allows_with_it() -> Result<()> {
    let pool = crate::connect(&crate::test_ledger_url()).await?;
    let (db, _d) = db_with(pool);
    let app = app(db, Arc::new(Recorder::default())).layer(axum::middleware::from_fn_with_state(
        Arc::new(crate::identity::auth::BearerGuard {
            expected: crate::identity::auth::SharedToken::new("s3cr3t"),
            ..crate::identity::auth::BearerGuard::default()
        }),
        crate::identity::auth::require_auth,
    ));

    let res = app
        .clone()
        .oneshot(HttpRequest::get("/healthz").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    let res = app
        .oneshot(
            HttpRequest::get("/healthz")
                .header(header::AUTHORIZATION, "Bearer s3cr3t")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    Ok(())
}

/// The machine path end to end: the static bearer names nobody of its own, so the guard stamps the
/// configured identity and `/api/whoami` reports it with the role that identity's whitelist entry
/// gives. A spoofed user or group on the same request buys nothing — 401 once an identity is
/// pinned, anonymous when it is not.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn the_static_token_is_whoami_as_the_configured_identity(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let guarded = |state: ApiState, identity: Option<&str>| -> Result<Router> {
        let guard = crate::identity::auth::BearerGuard::new(
            crate::identity::auth::SharedToken::new("s3cr3t"),
            crate::identity::auth::SharedToken::new("edge"),
            identity.map(str::to_string),
            None,
            crate::identity::auth::AuthMode::Proxy,
        )?;
        Ok(router(state).layer(axum::middleware::from_fn_with_state(
            Arc::new(guard),
            crate::identity::auth::require_auth,
        )))
    };
    let state = |admins: Vec<String>| ApiState {
        roles: crate::identity::auth::Roles::new(admins, vec![], vec!["team-x".to_string()]),
        ..ApiState::test(db.clone(), Arc::new(Recorder::default()))
    };
    let probe = |app: Router, spoof: bool| async move {
        let mut req =
            HttpRequest::get("/api/whoami").header(header::AUTHORIZATION, "Bearer s3cr3t");
        if spoof {
            req = req
                .header("x-auth-request-user", "mallory")
                .header("x-auth-request-groups", "/groups/team-x");
        }
        let res = app.oneshot(req.body(Body::empty())?).await?;
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
        anyhow::Ok((status, serde_json::from_slice::<serde_json::Value>(&body)?))
    };

    let admin_state = || state(vec!["crucible-cd".to_string()]);
    let (status, v) = probe(guarded(admin_state(), Some("crucible-cd"))?, false).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["user"], "crucible-cd");
    assert_eq!(v["role"], "admin");
    assert_eq!(v["admin"], true);
    assert_eq!(v["groups"], serde_json::json!([]));

    // The same bearer wearing the edge's headers is an edge on the wrong token, not the CD job.
    let (status, _) = probe(guarded(admin_state(), Some("crucible-cd"))?, true).await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // No configured identity: anonymous, and the asserted operator group does not rescue it.
    let (status, v) = probe(guarded(state(vec![]), None)?, true).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["user"], serde_json::Value::Null);
    assert_eq!(v["role"], "viewer");
    assert_eq!(v["groups"], serde_json::json!([]));

    // The edge's own bearer: its group assertion survives, is reported, and grants operator. This
    // is the probe an operator runs after an IdP change to see whether the groups claim arrives.
    let app = guarded(state(vec![]), Some("crucible-cd"))?;
    let res = app
        .oneshot(
            HttpRequest::get("/api/whoami")
                .header(header::AUTHORIZATION, "Bearer edge")
                .header("x-auth-request-user", "alice")
                .header("x-auth-request-groups", "/groups/Team-X, other")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["user"], "alice");
    assert_eq!(v["role"], "operator");
    assert_eq!(v["groups"], serde_json::json!(["/groups/team-x", "other"]));
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn get_approvals_returns_awaiting_approval_and_kept_prs(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    assert!(
        crate::issues::store::claim_issue(
            db.pool(),
            "owner/repo#1",
            Status::New,
            Status::AwaitingApproval
        )
        .await?
    );
    let scope_id_1 = crate::issues::store::insert_scope(
        db.pool(),
        &crate::issues::model::NewScope {
            issue: "owner/repo#1".to_string(),
            pack_digest: None,
            check_outcome: None,
        },
    )
    .await?;
    crate::issues::store::set_scope_approval_pr(
        db.pool(),
        scope_id_1,
        "https://github.com/owner/repo/pull/42",
    )
    .await?;

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#2")).await?;
    let scope_id_2 = crate::issues::store::insert_scope(
        db.pool(),
        &crate::issues::model::NewScope {
            issue: "owner/repo#2".to_string(),
            pack_digest: None,
            check_outcome: None,
        },
    )
    .await?;
    crate::runs::store::insert_run(
        db.pool(),
        &NewRun {
            run_id: "run-1".to_string(),
            scope: Some(scope_id_2),
            issue: None,
            identity_digest: None,
            status: "done".to_string(),
            pod: None,
            session_uri: None,
            best_score: None,
            cost_usd: None,
        },
    )
    .await?;
    crate::runs::store::insert_candidate(
        db.pool(),
        &NewCandidate {
            run_id: "run-1".to_string(),
            kind: None,
            lane: None,
            iter: None,
            score: None,
            decision: Some("keep".to_string()),
            worktree: None,
            sandbox: None,
            pr_url: Some("https://github.com/owner/repo/pull/99".to_string()),
            branch: None,
        },
    )
    .await?;

    let on = app(db.clone(), Arc::new(Recorder::default()));
    let (status, v) = get_json_object(&on, "/api/approvals").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["awaiting_approval"].as_array().unwrap().len(), 1);
    assert_eq!(v["kept_prs"].as_array().unwrap().len(), 1);
    assert_eq!(v["awaiting_approval"][0]["key"], "owner/repo#1");
    assert_eq!(v["kept_prs"][0]["issue"], "owner/repo#2");

    let off = router(ApiState {
        autoresearch: false,
        ..ApiState::test(db, Arc::new(Recorder::default()))
    });
    let (status, v) = get_json_object(&off, "/api/approvals").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["awaiting_approval"], serde_json::json!([]));
    assert_eq!(v["kept_prs"], serde_json::json!([]));
    Ok(())
}

/// With the lane off, its routes are not served and the version says so; the playbook surface
/// is untouched.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn the_autoresearch_routes_are_served_only_with_the_lane_on(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let state = ApiState::test(db.clone(), Arc::new(Recorder::default()));
    #[cfg(feature = "autoresearch")]
    let state = ApiState {
        autoresearch: false,
        ..state
    };
    let off = router(state);
    for uri in [
        "/api/issues",
        "/api/autopilot",
        "/api/turns",
        "/api/repos",
        "/api/builds",
    ] {
        let (status, _) = get_json_object(&off, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    let (status, version) = get_json_object(&off, "/api/version").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(version["autoresearch"], false);
    let (_, spec) = get_json_object(&off, "/api/openapi.json").await;
    assert!(spec["paths"].get("/api/issues").is_none());
    assert!(spec["paths"].get("/api/playbooks").is_some());
    let (status, _) = get_json_object(&off, "/api/playbooks").await;
    assert_eq!(status, StatusCode::OK);

    let on = app(db, Arc::new(Recorder::default()));
    let (_, version) = get_json_object(&on, "/api/version").await;
    assert_eq!(version["autoresearch"], cfg!(feature = "autoresearch"));
    let (status, _) = get_json_object(&on, "/api/issues").await;
    let expected = if cfg!(feature = "autoresearch") {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };
    assert_eq!(status, expected);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn issue_surfaces_carry_the_latest_kept_pr_url(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#2")).await?;
    let scope_id = crate::issues::store::insert_scope(
        db.pool(),
        &crate::issues::model::NewScope {
            issue: "owner/repo#1".to_string(),
            pack_digest: None,
            check_outcome: None,
        },
    )
    .await?;
    // Two runs, each keeping a PR: the newer run's PR must win. Run ids sort lexically.
    for (run_id, pr, decision) in [
        (
            "20260701T000000Z-a",
            "https://github.com/owner/repo/pull/7",
            "keep",
        ),
        (
            "20260702T000000Z-b",
            "https://github.com/owner/repo/pull/9",
            "keep",
        ),
        // A discarded candidate's PR never surfaces.
        (
            "20260703T000000Z-c",
            "https://github.com/owner/repo/pull/11",
            "discard",
        ),
    ] {
        crate::runs::store::insert_run(
            db.pool(),
            &NewRun {
                run_id: run_id.to_string(),
                scope: Some(scope_id),
                issue: None,
                identity_digest: None,
                status: "done".to_string(),
                pod: None,
                session_uri: None,
                best_score: None,
                cost_usd: None,
            },
        )
        .await?;
        crate::runs::store::insert_candidate(
            db.pool(),
            &NewCandidate {
                run_id: run_id.to_string(),
                kind: None,
                lane: None,
                iter: None,
                score: None,
                decision: Some(decision.to_string()),
                worktree: None,
                sandbox: None,
                pr_url: Some(pr.to_string()),
                branch: None,
            },
        )
        .await?;
    }

    let app = app(db, Arc::new(Recorder::default()));
    let res = app
        .clone()
        .oneshot(HttpRequest::get("/api/issues").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    let rows = v.as_array().expect("issue list");
    let by_key = |k: &str| {
        rows.iter()
            .find(|r| r["key"] == k)
            .unwrap_or_else(|| panic!("{k} in the list"))
    };
    assert_eq!(
        by_key("owner/repo#1")["pr_url"],
        "https://github.com/owner/repo/pull/9",
        "the newest kept run's PR wins"
    );
    assert!(by_key("owner/repo#2")["pr_url"].is_null());

    let res = app
        .oneshot(HttpRequest::get("/api/issues/owner%2Frepo%231").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(
        v["issue"]["pr_url"], "https://github.com/owner/repo/pull/9",
        "the detail DTO carries the same kept PR"
    );
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn get_scope_evidence_parses_the_refine_trail_off_the_stored_pack(
    pool: PgPool,
) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    let scope_id = crate::issues::store::insert_scope(
        db.pool(),
        &crate::issues::model::NewScope {
            issue: "owner/repo#1".to_string(),
            pack_digest: None,
            check_outcome: Some("PASS".to_string()),
        },
    )
    .await?;

    let pack_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        pack_dir.path().join("SCOPE.md"),
        "# SCOPE.md\n\n## Refine loop\n\n```json\n[\
         {\"round\":1,\"kind\":\"propose\",\"judge_block\":\"[judge]\",\"cost\":0.0,\
         \"outcome\":{\"result\":\"passed\"}}]\n```\n",
    )?;
    crate::playbooks::packs::store_pack_tree(db.pool(), "owner/repo#1", pack_dir.path())
        .await
        .expect("store pack");

    let app = app(db, Arc::new(Recorder::default()));
    let res = app
        .oneshot(
            HttpRequest::get(format!("/api/approvals/{scope_id}/evidence")).body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["scope_id"], scope_id);
    assert_eq!(v["check_outcome"], "PASS");
    assert_eq!(v["rounds"].as_array().unwrap().len(), 1);
    assert_eq!(v["rounds"][0]["kind"], "propose");
    assert_eq!(v["rounds"][0]["outcome"]["result"], "passed");
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn get_scope_evidence_is_empty_trail_for_a_pack_with_no_refine_section(
    pool: PgPool,
) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    let scope_id = crate::issues::store::insert_scope(
        db.pool(),
        &crate::issues::model::NewScope {
            issue: "owner/repo#1".to_string(),
            pack_digest: None,
            check_outcome: Some("PASS".to_string()),
        },
    )
    .await?;

    let pack_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        pack_dir.path().join("SCOPE.md"),
        "# SCOPE.md\n\nHand-authored, no trail.\n",
    )?;
    crate::playbooks::packs::store_pack_tree(db.pool(), "owner/repo#1", pack_dir.path())
        .await
        .expect("store pack");

    let app = app(db, Arc::new(Recorder::default()));
    let res = app
        .oneshot(
            HttpRequest::get(format!("/api/approvals/{scope_id}/evidence")).body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["rounds"].as_array().unwrap().len(), 0);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn get_scope_evidence_404s_an_unknown_scope(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app(db, Arc::new(Recorder::default()));
    let res = app
        .oneshot(HttpRequest::get("/api/approvals/999/evidence").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn get_scope_report_serves_the_latest_structured_report(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    crate::issues::store::insert_scope_report(
        db.pool(),
        &crate::issues::model::NewScopeReport {
            issue_key: "owner/repo#1".to_string(),
            pod_name: None,
            survived: true,
            report_json: r#"{"stages":[],"digest":"v1:old","cost":0.1}"#.to_string(),
        },
    )
    .await?;
    crate::issues::store::insert_scope_report(
        db.pool(),
        &crate::issues::model::NewScopeReport {
            issue_key: "owner/repo#1".to_string(),
            pod_name: Some("crucible-scope-owner-repo-1".to_string()),
            survived: false,
            report_json: r#"{
            "stages": [
                {"name": "ingest", "passed": true, "detail": "goal from issue"},
                {"name": "propose", "passed": false, "detail": "refine exhausted"}
            ],
            "digest": null,
            "cost": 0.42,
            "rounds": [
                {"round": 1, "kind": "propose", "judge_block": "", "cost": 0.42,
                 "outcome": {"result": "failed",
                             "evidence": {"stage": "structure", "detail": "no crucible.toml"}}}
            ]
        }"#
            .to_string(),
        },
    )
    .await?;

    let app = app(db, Arc::new(Recorder::default()));
    let res = app
        .oneshot(HttpRequest::get("/api/issues/owner%2Frepo%231/scope-report").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["survived"], false, "the LATEST report wins");
    assert_eq!(v["pod_name"], "crucible-scope-owner-repo-1");
    assert_eq!(v["stages"].as_array().unwrap().len(), 2);
    assert_eq!(v["stages"][1]["passed"], false);
    assert_eq!(v["rounds"][0]["outcome"]["evidence"]["stage"], "structure");
    assert!((v["cost"].as_f64().unwrap() - 0.42).abs() < 1e-9);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn get_scope_report_404s_an_issue_with_no_report(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    let app = app(db, Arc::new(Recorder::default()));
    let res = app
        .oneshot(HttpRequest::get("/api/issues/owner%2Frepo%231/scope-report").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn get_scope_transcript_serves_the_stored_ndjson_decompressed(pool: PgPool) -> Result<()> {
    use std::io::Write as _;
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    let report_id = crate::issues::store::insert_scope_report(
        db.pool(),
        &crate::issues::model::NewScopeReport {
            issue_key: "owner/repo#1".to_string(),
            pod_name: None,
            survived: true,
            report_json: r#"{"stages":[],"digest":"v1:abc","cost":0.1}"#.to_string(),
        },
    )
    .await?;
    let ndjson = "{\"kind\":\"note\",\"msg\":\"round 1: propose turn\"}\n{\"kind\":\"agent\",\"event\":{\"kind\":\"text\",\"delta\":\"hello\"}}\n";
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(ndjson.as_bytes())?;
    crate::issues::store::insert_scope_transcript(
        db.pool(),
        &crate::issues::model::NewScopeTranscript {
            scope_report_id: report_id,
            issue_key: "owner/repo#1".to_string(),
            transcript_gz: enc.finish()?,
        },
    )
    .await?;

    let app = app(db, Arc::new(Recorder::default()));
    let res = app
        .oneshot(
            HttpRequest::get("/api/issues/owner%2Frepo%231/scope-transcript").body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/x-ndjson")
    );
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    assert_eq!(std::str::from_utf8(&body)?, ndjson, "served decompressed");
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn get_scope_transcript_404s_an_issue_with_no_transcript(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    let app = app(db, Arc::new(Recorder::default()));
    let res = app
        .oneshot(
            HttpRequest::get("/api/issues/owner%2Frepo%231/scope-transcript").body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn get_repos_returns_repo_health(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#2")).await?;
    assert!(
        crate::issues::store::claim_issue(db.pool(), "owner/repo#1", Status::New, Status::Scoped)
            .await?
    );
    crate::issues::store::set_watermark(db.pool(), "owner/repo", "2025-01-05T12:00:00Z").await?;

    let app = app(db, Arc::new(Recorder::default()));
    let res = app
        .oneshot(HttpRequest::get("/api/repos").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
    assert_eq!(v.len(), 1);
    assert_eq!(v[0]["repo"], "owner/repo");
    assert_eq!(v[0]["total"], 2);
    assert_eq!(v[0]["watermark"], "2025-01-05T12:00:00Z");
    Ok(())
}

// --- repo watch-set (Lane O3) ------------------------------------------------------------

/// Points `GITHUB_API_URL` at `server` for the body of `f`, holding the crate-wide env lock
/// (the same discipline `triage`/`reconcile` tests use for this shared global).
#[cfg(feature = "autoresearch")]
async fn with_github_env<F, Fut, T>(server_uri: &str, f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let _g = crate::ENV_LOCK.lock().await;
    unsafe {
        std::env::set_var("GITHUB_API_URL", server_uri);
    }
    let out = f().await;
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }
    out
}

#[cfg(feature = "autoresearch")]
async fn mount_repo_exists(server: &wiremock::MockServer, repo: &str) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    Mock::given(method("GET"))
        .and(path(format!("/repos/{repo}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 1})))
        .mount(server)
        .await;
}

#[cfg(feature = "autoresearch")]
async fn mount_repo_missing(server: &wiremock::MockServer, repo: &str) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    Mock::given(method("GET"))
        .and(path(format!("/repos/{repo}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn add_repo_requires_admin(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let whitelist =
        crate::issues::repo_ref::RepoWhitelist::new(vec!["neuralmagic".to_string()], vec![]);
    // Neither a viewer (no role at all) nor an operator may add a repo — only admin.
    for (admins, operators) in [(vec![], vec![]), (vec![], vec!["bob".to_string()])] {
        let app = app_with_repo_whitelist(db.clone(), admins, operators, whitelist.clone());
        let res = app
            .oneshot(
                HttpRequest::post("/api/repos")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({"repo": "neuralmagic/llm-d", "justification": "need it"})
                            .to_string(),
                    ))?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn add_repo_rejects_malformed_repo_with_422(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let whitelist =
        crate::issues::repo_ref::RepoWhitelist::new(vec!["neuralmagic".to_string()], vec![]);
    let app = app_with_repo_whitelist(db, vec!["alice".to_string()], vec![], whitelist);
    let res = app
        .oneshot(
            HttpRequest::post("/api/repos")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "alice")
                .body(Body::from(
                    serde_json::json!({"repo": "not-a-valid-repo", "justification": "need it"})
                        .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert!(
        v["error"]
            .as_str()
            .unwrap_or_default()
            .contains("invalid repo")
    );
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn add_repo_rejects_org_not_on_the_whitelist_with_422(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let whitelist =
        crate::issues::repo_ref::RepoWhitelist::new(vec!["neuralmagic".to_string()], vec![]);
    let app = app_with_repo_whitelist(db, vec!["alice".to_string()], vec![], whitelist);
    let res = app
        .oneshot(
            HttpRequest::post("/api/repos")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "alice")
                .body(Body::from(
                    serde_json::json!({"repo": "someoneelse/repo", "justification": "need it"})
                        .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert!(
        v["error"]
            .as_str()
            .unwrap_or_default()
            .contains("whitelist")
    );
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn add_repo_rejects_a_repo_that_does_not_exist_on_github(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let whitelist =
        crate::issues::repo_ref::RepoWhitelist::new(vec!["neuralmagic".to_string()], vec![]);
    let app = app_with_repo_whitelist(db, vec!["alice".to_string()], vec![], whitelist);

    let server = wiremock::MockServer::start().await;
    mount_repo_missing(&server, "neuralmagic/ghost").await;
    let req = HttpRequest::post("/api/repos")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-auth-request-user", "alice")
        .body(Body::from(
            serde_json::json!({"repo": "neuralmagic/ghost", "justification": "need it"})
                .to_string(),
        ))?;
    let res = with_github_env(&server.uri(), || app.oneshot(req)).await?;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert!(
        v["error"]
            .as_str()
            .unwrap_or_default()
            .contains("does not exist on GitHub")
    );
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn add_repo_requires_a_non_empty_justification(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let whitelist =
        crate::issues::repo_ref::RepoWhitelist::new(vec!["neuralmagic".to_string()], vec![]);
    let app = app_with_repo_whitelist(db, vec!["alice".to_string()], vec![], whitelist);
    let res = app
        .oneshot(
            HttpRequest::post("/api/repos")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "alice")
                .body(Body::from(
                    serde_json::json!({"repo": "neuralmagic/llm-d", "justification": "   "})
                        .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn add_repo_succeeds_and_audits_then_conflicts_on_a_repeat(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let whitelist =
        crate::issues::repo_ref::RepoWhitelist::new(vec!["neuralmagic".to_string()], vec![]);
    let app = app_with_repo_whitelist(db.clone(), vec!["alice".to_string()], vec![], whitelist);

    let server = wiremock::MockServer::start().await;
    mount_repo_exists(&server, "neuralmagic/llm-d").await;
    let make_req = || {
        HttpRequest::post("/api/repos")
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-auth-request-user", "alice")
            .body(Body::from(
                serde_json::json!({"repo": "neuralmagic/llm-d", "justification": "onboarding"})
                    .to_string(),
            ))
            .expect("request")
    };

    let res = with_github_env(&server.uri(), || app.clone().oneshot(make_req())).await?;
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["repo"], "neuralmagic/llm-d");
    assert_eq!(v["watched"], true);
    assert_eq!(v["paused"], false);
    assert_eq!(v["added_by"], "alice");

    let watch = crate::issues::repo_watch::get_repo_watch(db.pool(), "neuralmagic/llm-d")
        .await?
        .expect("row");
    assert!(watch.watched);
    assert_eq!(watch.added_by.as_deref(), Some("alice"));
    let events = db.events().read_for_key("neuralmagic/llm-d").await?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].to, "watched");
    assert_eq!(events[0].reason.as_deref(), Some("onboarding"));
    assert_eq!(events[0].actor.as_deref(), Some("alice"));

    // A second add of the same repo conflicts.
    let res = with_github_env(&server.uri(), || app.clone().oneshot(make_req())).await?;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn add_repo_is_exempt_from_the_whitelist_when_env_seeded(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    // An empty org whitelist (locked closed) but the exact repo was env-seeded at boot.
    let whitelist =
        crate::issues::repo_ref::RepoWhitelist::new(vec![], vec!["someoneelse/repo".to_string()]);
    let app = app_with_repo_whitelist(db, vec!["alice".to_string()], vec![], whitelist);

    let server = wiremock::MockServer::start().await;
    mount_repo_exists(&server, "someoneelse/repo").await;
    let res = with_github_env(&server.uri(), || {
        app.oneshot(
            HttpRequest::post("/api/repos")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "alice")
                .body(Body::from(
                    serde_json::json!({"repo": "someoneelse/repo", "justification": "re-add"})
                        .to_string(),
                ))
                .expect("request"),
        )
    })
    .await?;
    assert_eq!(res.status(), StatusCode::CREATED);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn pause_resume_unwatch_require_admin_and_transition_correctly(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::repo_watch::insert_watched_repo(db.pool(), "owner/repo", Some("env")).await?;
    let whitelist = crate::issues::repo_ref::RepoWhitelist::default();

    // Viewer and operator both get 403 on all three mutating routes.
    for (admins, operators) in [(vec![], vec![]), (vec![], vec!["bob".to_string()])] {
        let app = app_with_repo_whitelist(db.clone(), admins, operators, whitelist.clone());
        for (method_str, path) in [
            ("POST", "/api/repos/owner%2Frepo/pause"),
            ("POST", "/api/repos/owner%2Frepo/resume"),
            ("DELETE", "/api/repos/owner%2Frepo"),
        ] {
            let req = HttpRequest::builder()
                .method(method_str)
                .uri(path)
                .body(Body::empty())?;
            let res = app.clone().oneshot(req).await?;
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{method_str} {path}");
        }
    }

    let app = app_with_repo_whitelist(db.clone(), vec!["alice".to_string()], vec![], whitelist);

    // 404 for an unknown repo.
    let res = app
        .clone()
        .oneshot(
            HttpRequest::post("/api/repos/no%2Fsuch/pause")
                .header("x-auth-request-user", "alice")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // Pause: watched stays true, paused flips true, an audit event lands.
    let res = app
        .clone()
        .oneshot(
            HttpRequest::post("/api/repos/owner%2Frepo/pause")
                .header("x-auth-request-user", "alice")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let watch = crate::issues::repo_watch::get_repo_watch(db.pool(), "owner/repo")
        .await?
        .expect("row");
    assert!(watch.watched && watch.paused);
    assert!(
        crate::issues::repo_watch::watched_repos(db.pool())
            .await?
            .is_empty(),
        "paused excludes it"
    );

    // Resume: paused flips back false.
    let res = app
        .clone()
        .oneshot(
            HttpRequest::post("/api/repos/owner%2Frepo/resume")
                .header("x-auth-request-user", "alice")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        !crate::issues::repo_watch::get_repo_watch(db.pool(), "owner/repo")
            .await?
            .expect("row")
            .paused
    );
    assert_eq!(
        crate::issues::repo_watch::watched_repos(db.pool()).await?,
        vec!["owner/repo".to_string()]
    );

    // Unwatch: the row (and any issues on it) stay, but watched flips false.
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    let res = app
        .clone()
        .oneshot(
            HttpRequest::delete("/api/repos/owner%2Frepo")
                .header("x-auth-request-user", "alice")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let watch = crate::issues::repo_watch::get_repo_watch(db.pool(), "owner/repo")
        .await?
        .expect("row kept");
    assert!(!watch.watched);
    assert!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#1")
            .await?
            .is_some(),
        "issue kept"
    );

    let events = db.events().read_for_key("owner/repo").await?;
    assert_eq!(
        events.iter().map(|e| e.to.as_str()).collect::<Vec<_>>(),
        vec!["paused", "watching", "unwatched"]
    );
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn scope_now_post_enqueues_override_with_justification(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    let recorder = Arc::new(Recorder::default());
    let app = router(ApiState {
        roles: crate::identity::auth::Roles::new(vec!["wren".to_string()], vec![], vec![]),
        ..ApiState::test(db, recorder.clone())
    });

    // The trigger is admin-gated: a non-admin identity is refused before the sink.
    let res = app
        .clone()
        .oneshot(
            HttpRequest::post("/api/issues/owner%2Frepo%231/scope")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "mallory")
                .body(Body::from(r#"{"justification":"nope"}"#))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    let res = app
        .oneshot(
            HttpRequest::post("/api/issues/owner%2Frepo%231/scope")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(
                    r#"{"justification":"customer escalation","max_cost":5.0}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["action"], "scope_now");
    assert_eq!(v["reason"], "customer escalation");
    assert_eq!(v["actor"], "wren");

    let calls = recorder.calls.lock().expect("lock");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].key.0, "owner/repo#1");
    assert!(
        matches!(
            &calls[0].kind,
            OverrideKind::ScopeNow { justification, max_cost }
                if justification == "customer escalation" && *max_cost == Some(5.0)
        ),
        "override kind carries justification + max_cost"
    );
    assert_eq!(calls[0].actor.as_deref(), Some("wren"));
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn redispatch_post_enqueues_override_and_is_admin_gated(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    let recorder = Arc::new(Recorder::default());
    let app = router(ApiState {
        roles: crate::identity::auth::Roles::new(vec!["wren".to_string()], vec![], vec![]),
        ..ApiState::test(db, recorder.clone())
    });

    // Admin-gated like scope_now: a non-admin identity is refused before the sink.
    let res = app
        .clone()
        .oneshot(
            HttpRequest::post("/api/issues/owner%2Frepo%231/redispatch")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "mallory")
                .body(Body::from(r#"{"justification":"nope"}"#))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    let res = app
        .oneshot(
            HttpRequest::post("/api/issues/owner%2Frepo%231/redispatch")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(
                    r#"{"justification":"re-run the approved pack"}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["action"], "redispatch");
    assert_eq!(v["reason"], "re-run the approved pack");
    assert_eq!(v["actor"], "wren");

    // The POST only enqueued an intent — it never wrote the DB (reconcile is the only writer).
    let calls = recorder.calls.lock().expect("lock");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].key.0, "owner/repo#1");
    assert!(
        matches!(
            &calls[0].kind,
            OverrideKind::Redispatch { justification }
                if justification == "re-run the approved pack"
        ),
        "override kind carries the justification"
    );
    assert_eq!(calls[0].actor.as_deref(), Some("wren"));
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn artifact_manifest_lists_local_files_and_derives_for_s3(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("session.jsonl"), "{}\n")?;
    std::fs::write(dir.path().join("RESULTS.md"), "# hi")?;
    std::fs::create_dir(dir.path().join("codegen-out"))?;
    std::fs::write(dir.path().join("codegen-out").join("plot.png"), [0u8; 3])?;
    let local_uri = dir
        .path()
        .join("session.jsonl")
        .to_string_lossy()
        .to_string();

    for (run_id, session_uri) in [
        ("run-local", Some(local_uri)),
        (
            "run-s3",
            Some("s3://bucket/runs/goal/run-s3/session.jsonl".to_string()),
        ),
        ("run-bare", None),
    ] {
        crate::runs::store::insert_run(
            db.pool(),
            &NewRun {
                run_id: run_id.to_string(),
                scope: None,
                issue: None,
                identity_digest: None,
                status: "done".to_string(),
                pod: None,
                session_uri,
                best_score: None,
                cost_usd: None,
            },
        )
        .await?;
    }
    crate::runs::store::insert_candidate(
        db.pool(),
        &NewCandidate {
            run_id: "run-s3".to_string(),
            kind: Some("deep".to_string()),
            lane: None,
            iter: Some(2),
            score: Some(1.0),
            decision: Some("keep".to_string()),
            worktree: None,
            sandbox: None,
            pr_url: None,
            branch: None,
        },
    )
    .await?;
    let app = app(db, Arc::new(Recorder::default()));

    let res = app
        .clone()
        .oneshot(HttpRequest::get("/api/runs/run-local/artifacts").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    let entries = v["entries"].as_array().expect("entries");
    let paths: Vec<&str> = entries.iter().filter_map(|e| e["path"].as_str()).collect();
    assert_eq!(
        paths,
        ["RESULTS.md", "codegen-out/plot.png", "session.jsonl"]
    );
    assert!(entries.iter().all(|e| e["size_bytes"].is_u64()));

    let res = app
        .clone()
        .oneshot(HttpRequest::get("/api/runs/run-s3/artifacts").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    let entries = v["entries"].as_array().expect("entries");
    let paths: Vec<&str> = entries.iter().filter_map(|e| e["path"].as_str()).collect();
    assert_eq!(
        paths,
        [
            "RESULTS.md",
            "diffs/iter-2.patch",
            "flow.html",
            "flow.json",
            "session.jsonl",
            "summary.json",
        ]
    );
    assert!(entries.iter().all(|e| e["size_bytes"].is_null()));

    // No session evidence: an empty manifest, not a 404.
    let res = app
        .clone()
        .oneshot(HttpRequest::get("/api/runs/run-bare/artifacts").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["entries"].as_array().expect("entries").len(), 0);

    let res = app
        .oneshot(HttpRequest::get("/api/runs/nope/artifacts").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn openapi_spec_contains_all_api_routes(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app(db, Arc::new(Recorder::default()));

    let res = app
        .oneshot(HttpRequest::get("/api/openapi.json").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let spec: serde_json::Value = serde_json::from_slice(&body)?;

    let paths = spec["paths"].as_object().expect("paths object");
    #[allow(unused_mut)]
    let mut expected_paths = vec![
        "/healthz",
        "/api/whoami",
        "/api/teams",
        "/api/teams/{slug}",
        "/api/teams/{slug}/members",
        "/api/teams/{slug}/audit",
        "/api/authz/actions",
        "/api/authz/schema",
        "/api/authz/policy-sets",
        "/api/authz/policy-sets/{digest}",
        "/api/authz/policy-sets/{digest}/activate",
        "/api/playbooks/{id}/owner",
        "/api/playbook-drafts/{id}/owner",
        "/api/playbooks/imports/{id}/owner",
        "/api/providers/{id}/owner",
        "/api/playbooks/{id}/shares",
        "/api/playbooks/{id}/shares/{grantee}",
        "/api/playbook-drafts/{id}/shares",
        "/api/playbook-drafts/{id}/shares/{grantee}",
        "/api/playbooks/imports/{id}/shares",
        "/api/playbooks/imports/{id}/shares/{grantee}",
        "/api/providers/{id}/shares",
        "/api/providers/{id}/shares/{grantee}",
        "/api/credentials/me",
        "/api/access",
        "/api/prefs",
        "/api/prefs/editor",
        "/api/prefs/pickers",
        "/api/overview",
        "/api/funnel",
        "/api/runs",
        "/api/clusters",
        "/api/runs/{run_id}",
        "/api/runs/{run_id}/iterations",
        "/api/runs/{run_id}/graph",
        "/api/runs/{run_id}/tasks/{task}/evidence",
        "/api/runs/{run_id}/log",
        "/api/runs/{run_id}/files",
        "/api/runs/{run_id}/files/{key}",
        "/api/runs/{run_id}/artifacts",
        "/api/runs/{run_id}/artifacts/{path}",
        "/api/runs/{run_id}/flow-enriched",
        "/api/export/runs.parquet",
        "/api/export/iterations.parquet",
        "/api/runs/{run_id}/live",
        "/api/ledger/summary",
        "/api/ledger/by-tag",
        "/api/events",
        "/api/events/stream",
        "/api/approvals",
        "/api/issues/{key}/park",
        "/api/issues/{key}/unpark",
        "/api/reconcile",
        "/api/emissions/run",
        "/api/runs/{run_id}/session",
        "/api/config",
        "/api/config/overrides",
        "/api/config/broker-contracts",
        "/api/config/playbook-caps",
        "/api/playbooks",
        "/api/playbooks/import/candidates",
        "/api/playbooks/imports",
        "/api/playbooks/imports/{id}",
        "/api/playbooks/imports/{id}/compile",
        "/api/playbooks/imports/{id}/register",
        "/api/playbooks/imports/{id}/discard",
        "/api/playbooks/imports/{id}/draft",
        "/api/playbooks/drafts/skill",
        "/api/playbooks/drafts/co-draft",
        "/api/playbooks/{id}",
        "/api/playbooks/{id}/schema",
        "/api/playbooks/{id}/launch",
        "/api/playbook-drafts",
        "/api/playbook-drafts/from-git",
        "/api/playbook-drafts/{id}",
        "/api/playbook-drafts/{id}/files",
        "/api/playbook-drafts/{id}/preview",
        "/api/playbook-drafts/{id}/origin/files",
        "/api/playbook-drafts/{id}/tarball",
        "/api/playbook-drafts/{id}/versions",
        "/api/playbook-drafts/{id}/launch",
        "/api/playbook-drafts/{id}/graduate",
        "/api/playbook-drafts/{id}/publish",
        "/api/playbook-runs",
        "/api/playbook-runs/{key}",
        "/api/one-shots",
        "/api/one-shots/{id}",
        "/api/schedules",
        "/api/schedules/preview",
        "/api/schedules/{id}",
        "/api/watches",
        "/api/watches/trackers",
        "/api/watches/preview",
        "/api/watches/{id}",
        "/api/watches/{id}/enabled",
        "/api/watches/{id}/hits",
        "/api/watches/{id}/hits/{item}",
        "/api/secrets",
        "/api/secrets/{id}",
        "/api/secrets/{id}/rotate",
        "/api/secrets/{id}/owner",
        "/api/secrets/{id}/bindings",
        "/api/secrets/{id}/bindings/{binding_id}",
        "/api/secrets/{id}/audit",
        "/api/dispatch-targets",
        "/api/config/providers",
        "/api/config/dispatch-defaults",
        "/api/providers",
        "/api/providers/{id}",
        "/api/keys",
        "/api/keys/{id}",
        "/api/version",
        "/api/images",
        "/api/images/refresh",
        "/api/images/rank",
    ];
    #[cfg(feature = "autoresearch")]
    expected_paths.extend([
        "/api/issues",
        "/api/issues/facets",
        "/api/issues/{key}",
        "/api/issues/{key}/journey",
        "/api/issues/{key}/builds",
        "/api/builds",
        "/api/builds/{id}/rebuild",
        "/api/turns/{pod_name}/live",
        "/api/turns",
        "/api/turns/{pod_name}",
        "/api/approvals/{scope_id}/evidence",
        "/api/issues/{key}/scope-report",
        "/api/issues/{key}/scope-transcript",
        "/api/repos",
        "/api/repos/{repo}/pause",
        "/api/repos/{repo}/resume",
        "/api/repos/{repo}",
        "/api/issues/{key}/bump",
        "/api/issues/{key}/redispatch",
        "/api/issues/{key}/rerank",
        "/api/issues/rerank",
        "/api/autopilot",
        "/api/issues/{key}/scope",
        "/api/scenarios",
        "/api/scenarios/{key}/approve",
        "/api/jira",
        "/api/packs/launch",
    ]);

    for path in &expected_paths {
        assert!(
            paths.contains_key(*path),
            "OpenAPI spec missing route: {path}"
        );
    }

    assert_eq!(
        paths.len(),
        expected_paths.len(),
        "OpenAPI spec has unexpected extra paths"
    );

    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn admin_guard_403s_non_admin_on_autopilot_set(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db, vec!["alice".to_string()]);

    let res = app
        .oneshot(
            HttpRequest::post("/api/autopilot")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "bob")
                .body(Body::from(r#"{"enabled":false,"reason":"nope"}"#))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert!(v["error"].as_str().unwrap().contains("bob"));
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn admin_guard_allows_whitelisted_user(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db, vec!["alice".to_string()]);

    let res = app
        .oneshot(
            HttpRequest::post("/api/autopilot")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "Alice")
                .body(Body::from(r#"{"enabled":false,"reason":"cost runaway"}"#))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["enabled"], false);
    assert_eq!(v["reason"], "cost runaway");
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn rerank_endpoints_are_admin_only(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    // Neither anonymous, a viewer, nor an operator may force a re-rank — only admin.
    for (admins, operators) in [(vec![], vec![]), (vec![], vec!["bob".to_string()])] {
        let app = app_with_roles(db.clone(), Arc::new(Recorder::default()), admins, operators);
        for (path, body) in [
            ("/api/issues/owner%2Frepo%231/rerank", ""),
            ("/api/issues/rerank", r#"{"scope":"all"}"#),
        ] {
            let res = app
                .clone()
                .oneshot(
                    HttpRequest::post(path)
                        .header(header::CONTENT_TYPE, "application/json")
                        .header("x-auth-request-user", "bob")
                        .body(Body::from(body))?,
                )
                .await?;
            assert_eq!(
                res.status(),
                StatusCode::FORBIDDEN,
                "{path} must be admin-gated"
            );
        }
    }
    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#1")
        .await?
        .expect("issue");
    assert!(iss.tier.is_none(), "no gated call may have touched the row");
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn rerank_single_clears_the_cache_keeps_the_tier_and_events(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    crate::issues::store::set_ranked_tier(db.pool(), "owner/repo#1", "T1", "perf", "hash-1")
        .await?;
    let app = app_with_admins(db.clone(), vec!["alice".to_string()]);

    let res = app
        .clone()
        .oneshot(
            HttpRequest::post("/api/issues/owner%2Frepo%231/rerank")
                .header("x-auth-request-user", "alice")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["key"], "owner/repo#1");
    assert_eq!(v["actor"], "alice");

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#1")
        .await?
        .expect("issue");
    assert!(iss.ranked_content_hash.is_none(), "cache cleared");
    assert_eq!(
        iss.tier.as_deref(),
        Some("T1"),
        "the standing tier keeps gating until the fresh verdict lands"
    );

    let events = crate::event_log::export_string(db.pool()).await?;
    let line: serde_json::Value = serde_json::from_str(events.lines().next().expect("event"))?;
    assert_eq!(line["key"], "owner/repo#1");
    assert_eq!(line["actor"], "alice");
    assert!(
        line["reason"]
            .as_str()
            .expect("reason")
            .contains("re-ranks next sweep")
    );

    // An untracked key is a 404, and no event is appended for it.
    let res = app
        .oneshot(
            HttpRequest::post("/api/issues/nope%2399/rerank")
                .header("x-auth-request-user", "alice")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn rerank_bulk_filters_count_affected_rows_and_logs_one_summary_event(
    pool: PgPool,
) -> Result<()> {
    let (db, _d) = db_with(pool);
    // Two ranked `new` rows (T0, T1), one unranked `new` row, and a ranked-but-scoped row the
    // sweep would never re-rank.
    for key in [
        "owner/repo#1",
        "owner/repo#2",
        "owner/repo#3",
        "owner/repo#4",
    ] {
        crate::issues::store::upsert_issue(db.pool(), &sample_issue(key)).await?;
    }
    crate::issues::store::set_ranked_tier(db.pool(), "owner/repo#1", "T0", "perf", "hash-1")
        .await?;
    crate::issues::store::set_ranked_tier(db.pool(), "owner/repo#2", "T1", "perf", "hash-2")
        .await?;
    crate::issues::store::set_ranked_tier(db.pool(), "owner/repo#4", "T1", "perf", "hash-4")
        .await?;
    assert!(
        crate::issues::store::claim_issue(db.pool(), "owner/repo#4", Status::New, Status::Scoped)
            .await?
    );
    let app = app_with_admins(db.clone(), vec!["alice".to_string()]);

    let post = |body: &'static str| {
        HttpRequest::post("/api/issues/rerank")
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-auth-request-user", "alice")
            .body(Body::from(body))
    };
    for (body, affected) in [
        (r#"{"scope":"tier","tier":"T1"}"#, 1u64),
        (r#"{"scope":"unranked"}"#, 1),
        (r#"{"scope":"all"}"#, 3),
    ] {
        let res = app.clone().oneshot(post(body)?).await?;
        assert_eq!(res.status(), StatusCode::OK, "{body}");
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
        let v: serde_json::Value = serde_json::from_slice(&bytes)?;
        assert_eq!(v["affected"], affected, "{body}");
        assert_eq!(v["actor"], "alice");
    }

    let scoped = crate::issues::store::get_issue(db.pool(), "owner/repo#4")
        .await?
        .expect("issue");
    assert!(
        scoped.ranked_content_hash.is_some(),
        "bulk only touches `new` rows"
    );

    // One summary line per bulk call on the synthetic `rerank` key — not one per issue.
    let events = crate::event_log::export_string(db.pool()).await?;
    let rerank_lines: Vec<serde_json::Value> = events
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let summaries: Vec<_> = rerank_lines
        .iter()
        .filter(|l| l["key"] == "rerank")
        .collect();
    assert_eq!(summaries.len(), 3);
    assert_eq!(summaries[0]["actor"], "alice");
    assert!(
        summaries[0]["reason"]
            .as_str()
            .expect("reason")
            .contains("tier T1")
    );

    // A garbage tier is a 400, not a silent no-op.
    let res = app
        .oneshot(post(r#"{"scope":"tier","tier":"T9"}"#)?)
        .await?;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// The manual-reconcile test rig: an admin whitelist plus the caller-held notify, so a test
/// can assert the endpoint actually stored a trigger permit.
fn app_with_reconcile(db: Db, admins: Vec<String>, notify: Arc<tokio::sync::Notify>) -> Router {
    router(ApiState {
        roles: crate::identity::auth::Roles::new(admins, vec![], vec![]),
        reconcile_now: notify,
        ..ApiState::test(db, Arc::new(Recorder::default()))
    })
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn trigger_reconcile_is_admin_only(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let notify = Arc::new(tokio::sync::Notify::new());
    // Neither anonymous nor a non-admin login may kick the daemon.
    for user in [None, Some("bob")] {
        let app = app_with_reconcile(db.clone(), vec!["alice".to_string()], notify.clone());
        let mut req = HttpRequest::post("/api/reconcile");
        if let Some(u) = user {
            req = req.header("x-auth-request-user", u);
        }
        let res = app.oneshot(req.body(Body::empty())?).await?;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "user={user:?}");
    }
    // No gated call may have stored a permit.
    let fired = tokio::time::timeout(std::time::Duration::from_millis(50), notify.notified()).await;
    assert!(fired.is_err(), "a 403 must not fire the trigger");
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn trigger_reconcile_acks_fires_the_notify_and_audits(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let notify = Arc::new(tokio::sync::Notify::new());
    let app = app_with_reconcile(db.clone(), vec!["alice".to_string()], notify.clone());

    let res = app
        .oneshot(
            HttpRequest::post("/api/reconcile")
                .header("x-auth-request-user", "alice")
                .body(Body::empty())?,
        )
        .await?;
    // 202: the pass runs async in the daemon — the request never waits for the sweep.
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["actor"], "alice");

    // The permit landed (notify_one stores it even with no waiter yet).
    tokio::time::timeout(std::time::Duration::from_secs(1), notify.notified())
        .await
        .expect("the trigger permit must be stored");

    // One audit line on the synthetic `reconcile` key, attributed to the admin.
    let events = crate::event_log::export_string(db.pool()).await?;
    let lines: Vec<serde_json::Value> = events
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let audit: Vec<_> = lines.iter().filter(|l| l["key"] == "reconcile").collect();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["actor"], "alice");
    assert_eq!(audit[0]["from"], "requested");
    assert_eq!(audit[0]["to"], "triggered");
    assert!(
        audit[0]["reason"]
            .as_str()
            .expect("reason")
            .contains("manual reconcile")
    );
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn admin_guard_403s_anonymous(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db, vec!["alice".to_string()]);

    let res = app
        .oneshot(
            HttpRequest::post("/api/autopilot")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"enabled":true,"reason":"re-enable"}"#))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn empty_admin_list_locks_closed(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db, vec![]);

    let res = app
        .oneshot(
            HttpRequest::post("/api/autopilot")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "alice")
                .body(Body::from(r#"{"enabled":false,"reason":"test"}"#))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn get_autopilot_returns_default_enabled(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app(db, Arc::new(Recorder::default()));

    let res = app
        .oneshot(HttpRequest::get("/api/autopilot").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["enabled"], true);
    assert!(v["changed_by"].is_null());
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn set_autopilot_appends_audit_event(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);

    let res = app
        .oneshot(
            HttpRequest::post("/api/autopilot")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(r#"{"enabled":false,"reason":"cost runaway"}"#))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);

    let events = db.events().read_for_key("autopilot").await?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].from, "enabled");
    assert_eq!(events[0].to, "disabled");
    assert_eq!(events[0].reason.as_deref(), Some("cost runaway"));
    assert_eq!(events[0].actor.as_deref(), Some("wren"));
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn whoami_reports_admin_status(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db, vec!["wren".to_string()]);

    let res = app
        .clone()
        .oneshot(
            HttpRequest::get("/api/whoami")
                .header("x-auth-request-user", "wren")
                .body(Body::empty())?,
        )
        .await?;
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["admin"], true);

    let res = app
        .oneshot(
            HttpRequest::get("/api/whoami")
                .header("x-auth-request-user", "bob")
                .body(Body::empty())?,
        )
        .await?;
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["admin"], false);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn whoami_reports_role_for_admin_operator_and_viewer(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_roles(
        db,
        Arc::new(Recorder::default()),
        vec!["alice".to_string()],
        vec!["bob".to_string()],
    );

    let whoami_as = |app: Router, user: &str| {
        let user = user.to_string();
        async move {
            let res = app
                .oneshot(
                    HttpRequest::get("/api/whoami")
                        .header("x-auth-request-user", user)
                        .body(Body::empty())?,
                )
                .await?;
            let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
            anyhow::Ok(serde_json::from_slice::<serde_json::Value>(&body)?)
        }
    };

    let v = whoami_as(app.clone(), "alice").await?;
    assert_eq!(v["role"], "admin");
    assert_eq!(v["admin"], true);

    let v = whoami_as(app.clone(), "bob").await?;
    assert_eq!(v["role"], "operator");
    assert_eq!(v["admin"], false);

    let v = whoami_as(app, "mallory").await?;
    assert_eq!(v["role"], "viewer");
    assert_eq!(v["admin"], false);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn get_access_lists_admins_operators_and_allowed_orgs(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    // Normalization (trim + lowercase) is applied by Roles/RepoWhitelist, so the response
    // reflects the canonical forms, not the raw config strings.
    let app = app_with_repo_whitelist(
        db,
        vec!["Alice".to_string()],
        vec!["Bob".to_string(), "carol".to_string()],
        crate::issues::repo_ref::RepoWhitelist::new(vec!["NeuralMagic".to_string()], vec![]),
    );

    let res = app
        .oneshot(HttpRequest::get("/api/access").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["admins"], serde_json::json!(["alice"]));
    assert_eq!(v["operators"], serde_json::json!(["bob", "carol"]));
    assert_eq!(v["allowed_orgs"], serde_json::json!(["neuralmagic"]));
    Ok(())
}

/// The complete 403 matrix for every operator-gated route (park/unpark/bump): a viewer is
/// refused, an operator passes, and an admin passes too (admin implies operator).
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn operator_guard_covers_park_unpark_bump_for_every_tier(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;

    let routes: [(&str, &str); 3] = [
        ("/api/issues/owner%2Frepo%231/park", r#"{"reason":"r"}"#),
        ("/api/issues/owner%2Frepo%231/unpark", r#"{}"#),
        ("/api/issues/owner%2Frepo%231/bump", r#"{"priority":1}"#),
    ];

    for (path, body) in routes {
        // Viewer (unknown login): 403.
        let app = app_with_roles(
            db.clone(),
            Arc::new(Recorder::default()),
            vec!["alice".to_string()],
            vec!["bob".to_string()],
        );
        let res = app
            .oneshot(
                HttpRequest::post(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("x-auth-request-user", "mallory")
                    .body(Body::from(body))?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "viewer on {path}");

        // Operator: passes.
        let app = app_with_roles(
            db.clone(),
            Arc::new(Recorder::default()),
            vec!["alice".to_string()],
            vec!["bob".to_string()],
        );
        let res = app
            .oneshot(
                HttpRequest::post(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("x-auth-request-user", "bob")
                    .body(Body::from(body))?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::ACCEPTED, "operator on {path}");

        // Admin: passes too (admin implies operator).
        let app = app_with_roles(
            db.clone(),
            Arc::new(Recorder::default()),
            vec!["alice".to_string()],
            vec!["bob".to_string()],
        );
        let res = app
            .oneshot(
                HttpRequest::post(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("x-auth-request-user", "alice")
                    .body(Body::from(body))?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::ACCEPTED, "admin on {path}");
    }
    Ok(())
}

/// Empty operator + admin lists lock every operator-gated route closed, even for a caller
/// with a plausible-looking identity.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn empty_operator_list_locks_closed(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    let app = app_with_roles(db, Arc::new(Recorder::default()), vec![], vec![]);

    let res = app
        .oneshot(
            HttpRequest::post("/api/issues/owner%2Frepo%231/park")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "alice")
                .body(Body::from(r#"{"reason":"r"}"#))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    Ok(())
}

/// ScopeNow and the autopilot kill switch stay admin-only: an operator who isn't an admin
/// still gets 403.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn operator_alone_cannot_scope_now_or_set_autopilot(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    let app = app_with_roles(
        db,
        Arc::new(Recorder::default()),
        vec![],
        vec!["bob".to_string()],
    );

    let res = app
        .clone()
        .oneshot(
            HttpRequest::post("/api/issues/owner%2Frepo%231/scope")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "bob")
                .body(Body::from(r#"{"justification":"nope"}"#))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    let res = app
        .oneshot(
            HttpRequest::post("/api/autopilot")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "bob")
                .body(Body::from(r#"{"enabled":false,"reason":"nope"}"#))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    Ok(())
}

// --- config overrides (Lane O2) -------------------------------------------

use crate::daemon::overrides_store::{
    CmSnapshot, ConfigMapApi, ConfigStore, DEFAULT_CONFIGMAP_NAME, ReplaceOutcome,
};

/// An in-memory `ConfigMapApi` for the API tests (bumps rv on write, like the store's own fake).
#[derive(Default)]
struct FakeCm {
    state: std::sync::Mutex<Option<(u64, String)>>,
}

#[async_trait::async_trait]
impl ConfigMapApi for FakeCm {
    async fn get(&self, _ns: &str, _name: &str) -> anyhow::Result<Option<CmSnapshot>> {
        Ok(self
            .state
            .lock()
            .expect("lock")
            .as_ref()
            .map(|(rv, p)| CmSnapshot {
                resource_version: rv.to_string(),
                payload: Some(p.clone()),
            }))
    }
    async fn create(&self, _ns: &str, _name: &str, payload: &str) -> anyhow::Result<()> {
        *self.state.lock().expect("lock") = Some((1, payload.to_string()));
        Ok(())
    }
    async fn replace(
        &self,
        _ns: &str,
        _name: &str,
        payload: &str,
        rv: &str,
    ) -> anyhow::Result<ReplaceOutcome> {
        let mut g = self.state.lock().expect("lock");
        if g.as_ref().map(|(v, _)| v.to_string()).as_deref() != Some(rv) {
            return Ok(ReplaceOutcome::Conflict);
        }
        let next_rv = g.as_ref().map(|(v, _)| v + 1).unwrap_or(1);
        *g = Some((next_rv, payload.to_string()));
        Ok(ReplaceOutcome::Applied)
    }
}

fn test_store() -> ConfigStore {
    let cfg = crate::testing::cfg_from_args(["ctl"]);
    ConfigStore::new(
        cfg.base_config(),
        DEFAULT_CONFIGMAP_NAME.to_string(),
        "ns".to_string(),
        Arc::new(FakeCm::default()),
        None,
        None,
    )
}

fn app_with_config(db: Db, admins: Vec<String>, store: ConfigStore) -> Router {
    router(ApiState {
        roles: crate::identity::auth::Roles::new(admins, vec![], vec![]),
        config: Some(store),
        ..ApiState::test(db, Arc::new(Recorder::default()))
    })
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn get_config_lists_every_knob_with_source(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_config(db, vec![], test_store());
    let res = app
        .oneshot(HttpRequest::get("/api/config").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    let knobs = v["knobs"].as_array().expect("knobs array");
    assert_eq!(knobs.len(), crate::daemon::overrides_store::Knob::ALL.len());
    // Every knob is overridable, and a fresh store shows the defaults.
    assert!(knobs.iter().all(|k| k["overridable"] == true));
    let dcc = knobs
        .iter()
        .find(|k| k["name"] == "daily_cost_ceiling")
        .expect("daily_cost_ceiling");
    assert_eq!(dcc["source"], "default");
    assert_eq!(dcc["value"], 50.0);
    // allowed_tiers is exposed too, defaulting to t0,t1.
    let at = knobs
        .iter()
        .find(|k| k["name"] == "allowed_tiers")
        .expect("allowed_tiers");
    assert_eq!(at["source"], "default");
    assert_eq!(at["value"], serde_json::json!(["t0", "t1"]));
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn get_config_reports_every_dispatch_target_contract(pool: PgPool) -> Result<()> {
    use crate::runs::contract::{
        CONTROLLER_CONTRACT_VERSION, ContractReadError, ContractRegistry, DispatchTarget,
        TableReader,
    };
    let (db, _d) = db_with(pool);
    let registry = Arc::new(ContractRegistry::new(Arc::new(TableReader::new([
        (
            "quay.io/x/loop:1".to_string(),
            Ok(Some(CONTROLLER_CONTRACT_VERSION.to_string())),
        ),
        ("quay.io/x/sandbox:1".to_string(), Ok(None)),
        (
            "quay.io/x/private:1".to_string(),
            Err(ContractReadError("401 unauthorized".to_string())),
        ),
    ]))));
    registry
        .check_all([
            DispatchTarget::image("quay.io/x/loop:1"),
            DispatchTarget::image("quay.io/x/sandbox:1"),
            DispatchTarget::image("quay.io/x/private:1"),
        ])
        .await;
    let app = router(ApiState {
        config: Some(test_store()),
        contracts: registry,
        ..ApiState::test(db, Arc::new(Recorder::default()))
    });
    let res = app
        .oneshot(HttpRequest::get("/api/config").body(Body::empty())?)
        .await?;
    // A registry failure is reported, never a reason to stop serving.
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert!(v["knobs"].is_array(), "the knobs still ride alongside");
    let contract = &v["contract"];
    assert_eq!(contract["controller_version"], CONTROLLER_CONTRACT_VERSION);
    let images = contract["images"].as_array().expect("images array");
    assert_eq!(images.len(), 3);
    let by_ref = |r: &str| {
        images
            .iter()
            .find(|i| i["reference"] == r)
            .unwrap_or_else(|| panic!("{r} reported"))
            .clone()
    };
    let ok = by_ref("quay.io/x/loop:1");
    assert_eq!(ok["engine_version"], CONTROLLER_CONTRACT_VERSION);
    assert_eq!(ok["controller_version"], CONTROLLER_CONTRACT_VERSION);
    assert_eq!(ok["match"], true);
    assert_eq!(ok["error"], serde_json::Value::Null);
    assert!(ok["checked_at"].as_str().is_some_and(|t| t.ends_with('Z')));
    let unlabeled = by_ref("quay.io/x/sandbox:1");
    assert_eq!(unlabeled["engine_version"], "unknown");
    assert_eq!(unlabeled["match"], false);
    assert_eq!(unlabeled["error"], serde_json::Value::Null);
    let broken = by_ref("quay.io/x/private:1");
    assert_eq!(broken["engine_version"], "unknown");
    assert_eq!(broken["match"], false);
    assert_eq!(broken["error"], "401 unauthorized");
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn get_config_503_without_a_store(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app(db, Arc::new(Recorder::default()));
    let res = app
        .oneshot(HttpRequest::get("/api/config").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn put_config_overrides_requires_admin(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    // Whitelist has wren; a request without the header is anonymous → 403.
    let app = app_with_config(db, vec!["wren".to_string()], test_store());
    let res = app
        .oneshot(
            HttpRequest::put("/api/config/overrides")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"max_concurrent_pods":4,"justification":"x"}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn put_config_overrides_rejects_bad_values(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_config(db, vec!["wren".to_string()], test_store());
    let res = app
        .oneshot(
            HttpRequest::put("/api/config/overrides")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(
                    r#"{"daily_cost_ceiling":-5.0,"justification":"oops"}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn put_config_overrides_rejects_a_garbage_tier(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_config(db, vec!["wren".to_string()], test_store());
    let res = app
        .oneshot(
            HttpRequest::put("/api/config/overrides")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(
                    r#"{"allowed_tiers":["t0","t9"],"justification":"oops"}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn put_config_overrides_requires_justification(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_config(db, vec!["wren".to_string()], test_store());
    let res = app
        .oneshot(
            HttpRequest::put("/api/config/overrides")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(
                    r#"{"max_concurrent_pods":4,"justification":"   "}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn put_config_overrides_writes_and_audits(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let store = test_store();
    let app = app_with_config(db.clone(), vec!["wren".to_string()], store.clone());
    let res = app
        .oneshot(
            HttpRequest::put("/api/config/overrides")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(
                    r#"{"max_concurrent_pods":4,"allow_t3":true,"justification":"scale up for the sprint"}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    // The response is the new effective config; the override took effect immediately.
    let mcp = v["knobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["name"] == "max_concurrent_pods")
        .unwrap();
    assert_eq!(mcp["value"], 4);
    assert_eq!(mcp["source"], "override");
    // The store's effective config reflects the write.
    assert_eq!(store.effective().max_concurrent_pods, 4);
    assert!(store.effective().allow_t3);

    // The audit event names the actor, justification, and changed keys.
    let events = db.events().read_for_key("config").await?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].actor.as_deref(), Some("wren"));
    let reason = events[0].reason.as_deref().unwrap_or_default();
    assert!(reason.contains("scale up for the sprint"), "{reason}");
    assert!(reason.contains("allow_t3"), "{reason}");
    assert!(reason.contains("max_concurrent_pods"), "{reason}");
    Ok(())
}

// --- scenario adopt + approve (Phase 4) ---------------------------------------

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn adopt_scenario_requires_admin(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db, vec!["wren".to_string()]);

    let res = app
        .oneshot(
            HttpRequest::post("/api/scenarios")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "mallory")
                .body(Body::from(
                    r#"{"title":"t","body":"b","affected_repos":["owner/repo"],"justification":"j"}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn adopt_scenario_rejects_blank_fields(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db, vec!["wren".to_string()]);

    let res = app
        .oneshot(
            HttpRequest::post("/api/scenarios")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(
                    r#"{"title":"","body":"b","affected_repos":["owner/repo"],"justification":"j"}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn adopt_scenario_rejects_empty_repo_list(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db, vec!["wren".to_string()]);

    let res = app
        .oneshot(
            HttpRequest::post("/api/scenarios")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(
                    r#"{"title":"t","body":"b","affected_repos":[],"justification":"j"}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn adopt_scenario_rejects_blank_repo_entry(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db, vec!["wren".to_string()]);

    let res = app
        .oneshot(
            HttpRequest::post("/api/scenarios")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(
                    r#"{"title":"t","body":"b","affected_repos":["owner/repo","   "],"justification":"j"}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    Ok(())
}

/// The happy path: an admin's adopt POST mints an `issues` row (`input_kind='scenario'`, `new`,
/// a preset tier) plus its `scenarios` sidecar in one transaction, and a ledger event names the
/// actor + justification.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn adopt_scenario_creates_issue_scenario_row_and_ledger_event(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);

    let res = app
        .oneshot(
            HttpRequest::post("/api/scenarios")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(
                    r#"{"title":"faster p99","body":"cut p99 latency under load","affected_repos":["owner/repo"],"justification":"customer ask"}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    let key = v["key"].as_str().expect("key").to_string();
    assert!(key.starts_with("scenario:"), "{key}");
    assert_eq!(v["affected_repos"], serde_json::json!(["owner/repo"]));
    assert_eq!(v["actor"], "wren");

    let issue = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .expect("issue row created");
    assert_eq!(issue.status, Status::New);
    assert_eq!(issue.repo, "owner/repo");
    assert!(
        matches!(&issue.kind, InputKind::Scenario { id } if key == format!("scenario:{id}")),
        "input_kind decodes back to Scenario with the minted id"
    );
    assert!(
        issue.tier.is_some(),
        "a preset tier, not NULL like a GitHub row awaiting the ranker"
    );

    let scenario = crate::issues::store::get_scenario(db.pool(), &key)
        .await?
        .expect("scenarios sidecar row created");
    assert_eq!(scenario.body, "cut p99 latency under load");
    assert!(
        !scenario.authoritative,
        "an omitted authoritative field defaults to false"
    );

    let events = db.events().read_for_key(&key).await?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].actor.as_deref(), Some("wren"));
    assert!(
        events[0]
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("customer ask")
    );
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn direct_pack_launch_freezes_an_approved_autoresearch_scope(pool: PgPool) -> Result<()> {
    let dir = tempfile::tempdir()?;
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("examples/selfhost"))?;
    std::fs::write(
        repo.join("examples/selfhost/crucible.toml"),
        "[repo]\nurl = \"https://github.com/owner/repo\"\n\n[agent]\nbackend = \"openshell\"\n\n[judge]\nmeasure_cmd = \"bench\"\ndirection = \"higher\"\nobjective = \"score\"\n\n[workflow]\ntype = \"autoresearch\"\nfile = \"workflow.star\"\n",
    )?;
    std::fs::write(
        repo.join("examples/selfhost/workflow.star"),
        concat!(
            "params = {}\n",
            "solver = session(name = \"solver\")\n",
            "candidate = propose(name = \"propose\", session = solver)\n",
            "applied = apply(name = \"apply\", depends_on = [candidate])\n",
            "score = evaluate(name = \"score\", run = \"./measure.sh\", depends_on = [applied], isolated = True, emits = [\"score\", \"pass\"])\n",
            "measurement = grade(name = \"grade\", evidence = [score], score = score)\n",
            "decision = decide(name = \"decide\", measurement = measurement)\n",
            "workflow(type = \"autoresearch\", tasks = [candidate, applied, score, measurement, decision], result = decision)\n",
        ),
    )?;
    for args in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "pack",
        ],
    ] {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .status()?;
        assert!(status.success());
    }
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    let repo_text = repo.to_string_lossy().to_string();

    let (status, ack) = post_admin(
        &app,
        "/api/packs/launch",
        serde_json::json!({
            "repo": repo_text,
            "path": "examples/selfhost",
            "justification": "known reviewed pack"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{ack}");
    let key = ack["key"].as_str().expect("launch key");
    assert_eq!(ack["status"], "awaiting-approval");

    let issue = crate::issues::store::get_issue(db.pool(), key)
        .await?
        .expect("direct launch issue");
    assert_eq!(issue.status, Status::AwaitingApproval);
    let scope = crate::issues::store::latest_scope_for_issue(db.pool(), key)
        .await?
        .expect("approved frozen scope");
    assert_eq!(scope.check_outcome.as_deref(), Some("DIRECT"));
    assert_eq!(scope.approved_by.as_deref(), Some("wren"));
    assert!(scope.is_approved());
    assert!(
        crate::playbooks::packs::materialize_pack(db.pool(), key)
            .await?
            .is_some()
    );
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn adopt_authoritative_scenario_persists_the_flag(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);

    let res = app
        .oneshot(
            HttpRequest::post("/api/scenarios")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(
                    r#"{"title":"trace-derived plan","body":"pooled arena for the hot path","affected_repos":["owner/repo"],"justification":"profiler brief","authoritative":true}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    let key = v["key"].as_str().expect("key").to_string();

    let scenario = crate::issues::store::get_scenario(db.pool(), &key)
        .await?
        .expect("scenarios sidecar row created");
    assert!(
        scenario.authoritative,
        "authoritative:true must persist through the adopt endpoint"
    );

    // The detail DTO carries the flag out to the SPA.
    let detail = crate::api::dto::issue_detail(&db, &key)
        .await?
        .expect("issue detail");
    assert!(
        detail.scenario.expect("scenario detail").authoritative,
        "the detail DTO must expose the authoritative flag"
    );
    Ok(())
}

/// A valid `git_ref` lands on the issue row (where the turn dispatch reads it) and is echoed on the
/// ack. The `scenarios` sidecar does NOT carry it: the ref pins the CLONE, and the clone target is
/// an `issues` column.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn adopt_scenario_persists_a_valid_git_ref(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);

    let res = app
        .oneshot(
            HttpRequest::post("/api/scenarios")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(
                    r#"{"title":"t","body":"b","affected_repos":["owner/repo"],"justification":"j","git_ref":"  nv_dev  "}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["git_ref"], "nv_dev", "the ack echoes the trimmed ref");
    let key = v["key"].as_str().expect("key").to_string();

    let issue = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .expect("issue row created");
    assert_eq!(issue.git_ref.as_deref(), Some("nv_dev"));

    // …and it is readable afterwards. The ack used to be the only place the ref was ever visible,
    // which left no way to check what an adopted issue is actually pointed at.
    let res = app_with_admins(db.clone(), vec!["wren".to_string()])
        .oneshot(HttpRequest::get("/api/issues").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let rows: serde_json::Value = serde_json::from_slice(&body)?;
    let row = rows
        .as_array()
        .expect("a list")
        .iter()
        .find(|r| r["key"] == key.as_str())
        .expect("the adopted issue is listed");
    assert_eq!(row["git_ref"], "nv_dev");
    assert_eq!(
        row["codegen_contract"],
        serde_json::Value::Null,
        "omitted at adopt, so null rather than absent"
    );
    Ok(())
}

/// An omitted `git_ref` leaves the column NULL — the default branch, exactly as before this existed.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn adopt_scenario_without_a_git_ref_leaves_it_null(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);

    let res = app
        .oneshot(
            HttpRequest::post("/api/scenarios")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(
                    r#"{"title":"t","body":"b","affected_repos":["owner/repo"],"justification":"j"}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert!(v["git_ref"].is_null(), "no ref asked for, none reported");
    let key = v["key"].as_str().expect("key").to_string();

    let issue = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .expect("issue row created");
    assert!(issue.git_ref.is_none());
    Ok(())
}

/// The ref ends up in a turn pod's argv, so the grammar is deliberately narrow: flag injection, git
/// rev-range/traversal syntax, shell metacharacters, whitespace, and a present-but-blank value are
/// all 422s, not sanitized-and-accepted.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn adopt_scenario_rejects_a_malformed_git_ref(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let long = "a".repeat(129);
    for bad in [
        "",
        "   ",
        "--upload-pack=evil",
        "-nv_dev",
        "refs/../../etc/passwd",
        "main..dev",
        "release branch",
        "main;rm -rf /",
        "main^{}",
        "feat/été",
        long.as_str(),
    ] {
        let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
        let payload = serde_json::json!({
            "title": "t",
            "body": "b",
            "affected_repos": ["owner/repo"],
            "justification": "j",
            "git_ref": bad,
        });
        let res = app
            .oneshot(
                HttpRequest::post("/api/scenarios")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("x-auth-request-user", "wren")
                    .body(Body::from(serde_json::to_vec(&payload)?))?,
            )
            .await?;
        assert_eq!(
            res.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "git_ref {bad:?} must be rejected"
        );
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
        let v: serde_json::Value = serde_json::from_slice(&body)?;
        assert!(
            v["error"].as_str().unwrap_or_default().contains("git_ref"),
            "the 422 must name the offending field: {v}"
        );
    }
    Ok(())
}

/// A configured contract name lands on the issue row (where both dispatches read it) and is echoed
/// on the ack. The NAME is what persists — the JSON is resolved from config at each dispatch.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn adopt_scenario_persists_a_configured_codegen_contract(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_contracts(db.clone(), vec!["wren".to_string()], test_contracts());

    let res = app
        .oneshot(
            HttpRequest::post("/api/scenarios")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(
                    r#"{"title":"t","body":"b","affected_repos":["owner/repo"],"justification":"j","codegen_contract":"  deepgemm  "}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(
        v["codegen_contract"], "deepgemm",
        "the ack echoes the trimmed name"
    );
    let key = v["key"].as_str().expect("key").to_string();

    let issue = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .expect("issue row created");
    assert_eq!(issue.codegen_contract.as_deref(), Some("deepgemm"));
    Ok(())
}

/// An omitted contract leaves the column NULL — local measure, exactly as before this existed.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn adopt_scenario_without_a_codegen_contract_leaves_it_null(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_contracts(db.clone(), vec!["wren".to_string()], test_contracts());

    let res = app
        .oneshot(
            HttpRequest::post("/api/scenarios")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(
                    r#"{"title":"t","body":"b","affected_repos":["owner/repo"],"justification":"j"}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert!(v["codegen_contract"].is_null());
    let key = v["key"].as_str().expect("key").to_string();
    let issue = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .expect("issue row created");
    assert!(issue.codegen_contract.is_none());
    Ok(())
}

/// A name the deploy does not configure is a 422 at adoption, not a row that parks hours later at
/// its first run dispatch. Present-but-blank is a caller mistake, not "local measure".
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn adopt_scenario_rejects_an_unconfigured_codegen_contract(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    for (contracts, bad) in [
        (test_contracts(), "vllm"),
        (test_contracts(), "DeepGEMM"),
        (test_contracts(), "   "),
        // Nothing configured at all: every name is unknown, and the error says so.
        (crate::config::BrokerContracts::default(), "deepgemm"),
    ] {
        let app = app_with_contracts(db.clone(), vec!["wren".to_string()], contracts);
        let payload = serde_json::json!({
            "title": "t",
            "body": "b",
            "affected_repos": ["owner/repo"],
            "justification": "j",
            "codegen_contract": bad,
        });
        let res = app
            .oneshot(
                HttpRequest::post("/api/scenarios")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("x-auth-request-user", "wren")
                    .body(Body::from(serde_json::to_vec(&payload)?))?,
            )
            .await?;
        assert_eq!(
            res.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "codegen_contract {bad:?} must be rejected"
        );
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
        let v: serde_json::Value = serde_json::from_slice(&body)?;
        assert!(
            v["error"]
                .as_str()
                .unwrap_or_default()
                .contains("codegen_contract"),
            "the 422 must name the offending field: {v}"
        );
    }
    Ok(())
}

/// The form's select source: names only, sorted, and an empty list when nothing is configured (the
/// form then offers local measure alone). Contract BODIES are never served — they carry base images
/// and benchmark command lines the picker has no use for.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn broker_contracts_endpoint_lists_the_configured_names(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let contracts = crate::config::BrokerContracts::from_map(std::collections::BTreeMap::from([
        ("vllm".to_string(), r#"{"gpus":2}"#.to_string()),
        ("deepgemm".to_string(), r#"{"gpus":1}"#.to_string()),
    ]));
    let app = app_with_contracts(db.clone(), vec![], contracts);
    let res = app
        .oneshot(HttpRequest::get("/api/config/broker-contracts").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["names"], serde_json::json!(["deepgemm", "vllm"]));
    assert!(v.get("contracts").is_none(), "names only: {v}");

    let empty = app_with_contracts(db, vec![], crate::config::BrokerContracts::default());
    let res = empty
        .oneshot(HttpRequest::get("/api/config/broker-contracts").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["names"], serde_json::json!([]));
    Ok(())
}

/// Unconfigured Jira: the adopt endpoint answers a clean 503 instead of issuing a broken fetch.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn adopt_jira_unconfigured_returns_503(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins_and_jira(db, vec!["wren".to_string()], None);

    let res = app
        .oneshot(
            HttpRequest::post("/api/jira")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(
                    r#"{"issue_key":"ACME-1234","affected_repos":["owner/repo"],"justification":"customer ask"}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    Ok(())
}

/// Non-admins can't adopt a Jira issue even when Jira is configured.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn adopt_jira_requires_admin(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let jira = crate::launches::jira::JiraConfig {
        base_url: "http://127.0.0.1:1".to_string(),
        email: "e".to_string(),
        api_token: "t".to_string(),
    };
    let app = app_with_admins_and_jira(db, vec!["wren".to_string()], Some(jira));

    let res = app
        .oneshot(
            HttpRequest::post("/api/jira")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "mallory")
                .body(Body::from(
                    r#"{"issue_key":"ACME-1234","affected_repos":["owner/repo"],"justification":"j"}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    Ok(())
}

/// A malformed issue key is a 422 (request error), never a silent `Unknown` row.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn adopt_jira_rejects_malformed_key(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let jira = crate::launches::jira::JiraConfig {
        base_url: "http://127.0.0.1:1".to_string(),
        email: "e".to_string(),
        api_token: "t".to_string(),
    };
    let app = app_with_admins_and_jira(db, vec!["wren".to_string()], Some(jira));

    let res = app
        .oneshot(
            HttpRequest::post("/api/jira")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(
                    r#"{"issue_key":"NOHYPHEN","affected_repos":["owner/repo"],"justification":"j"}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    Ok(())
}

/// The happy path: an admin adopts a Jira issue by key; the controller fetches its title/body from
/// (a mock) Jira Cloud and mints an `issues` row (`input_kind='jira'`, key `jira:{site}:{PROJ-N}`,
/// `new`, preset tier) plus its body sidecar, with a ledger event naming the actor + justification.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn adopt_jira_fetches_and_creates_issue_row_and_ledger_event(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let (base_url, server) = spawn_mock_jira("Fix the router", "steps to repro").await;
    let jira = crate::launches::jira::JiraConfig {
        base_url,
        email: "me@example.com".to_string(),
        api_token: "tok".to_string(),
    };
    let app = app_with_admins_and_jira(db.clone(), vec!["wren".to_string()], Some(jira));

    let res = app
        .oneshot(
            HttpRequest::post("/api/jira")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(
                    r#"{"issue_key":"ACME-1234","affected_repos":["owner/repo"],"justification":"customer escalation"}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    let key = v["key"].as_str().expect("key").to_string();
    // Site defaults to the base URL host label (a 127.0.0.1 mock -> "127").
    assert_eq!(key, "jira:127:ACME-1234");
    assert_eq!(v["issue_key"], "ACME-1234");
    assert_eq!(v["title"], "Fix the router");
    assert_eq!(v["actor"], "wren");

    let issue = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .expect("issue row created");
    assert_eq!(issue.status, Status::New);
    assert_eq!(issue.repo, "owner/repo");
    assert!(
        matches!(
            &issue.kind,
            InputKind::Jira { project, number, .. } if project == "ACME" && *number == 1234
        ),
        "input_kind decodes back to Jira"
    );
    assert!(issue.tier.is_some(), "a preset tier, not NULL");

    // The fetched body is stored like a scenario's, so the scope path picks it up unchanged.
    let stored = crate::issues::store::get_scenario(db.pool(), &key)
        .await?
        .expect("body sidecar row created");
    assert_eq!(stored.title, "Fix the router");
    assert_eq!(stored.body, "steps to repro");

    let events = db.events().read_for_key(&key).await?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].actor.as_deref(), Some("wren"));
    assert!(
        events[0]
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("customer escalation")
    );
    server.await.expect("mock jira server task");
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn approve_scenario_requires_admin(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db, vec!["wren".to_string()]);

    let res = app
        .oneshot(
            HttpRequest::post("/api/scenarios/scenario%3Afoo/approve")
                .header("x-auth-request-user", "mallory")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn approve_scenario_404s_with_no_pending_scope(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("scenario:no-scope")).await?;
    sqlx::query("UPDATE issues SET input_kind = 'scenario' WHERE key = $1")
        .bind("scenario:no-scope")
        .execute(db.pool())
        .await?;
    let app = app_with_admins(db, vec!["wren".to_string()]);

    let res = app
        .oneshot(
            HttpRequest::post("/api/scenarios/scenario%3Ano-scope/approve")
                .header("x-auth-request-user", "wren")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// The approve endpoint stamps `approved_at` on the scenario's latest scope — the same
/// `is_approved()` signal the draft-PR poll flips for a GitHub issue — fed from the UI instead.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn approve_scenario_stamps_approved_at(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("scenario:approve-1")).await?;
    sqlx::query("UPDATE issues SET input_kind = 'scenario' WHERE key = $1")
        .bind("scenario:approve-1")
        .execute(db.pool())
        .await?;
    let scope_id = crate::issues::store::insert_scope(
        db.pool(),
        &crate::issues::model::NewScope {
            issue: "scenario:approve-1".to_string(),
            pack_digest: Some("v1:deadbeef".to_string()),
            check_outcome: Some("OK".to_string()),
        },
    )
    .await?;
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);

    let res = app
        .oneshot(
            HttpRequest::post("/api/scenarios/scenario%3Aapprove-1/approve")
                .header("x-auth-request-user", "wren")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["scope_id"], scope_id);
    assert_eq!(v["approved_by"], "wren");
    assert!(v["approved_at"].as_str().is_some());

    let scope = crate::issues::store::latest_scope_for_issue(db.pool(), "scenario:approve-1")
        .await?
        .expect("scope");
    assert!(scope.is_approved());
    assert_eq!(scope.approved_by.as_deref(), Some("wren"));

    let events = db.events().read_for_key("scenario:approve-1").await?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].actor.as_deref(), Some("wren"));
    Ok(())
}

/// A second approve on an already-approved scope must not overwrite the ledgered stamp or emit a
/// second audit event; it reports the stamp that actually landed with a 409.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn approve_scenario_conflicts_on_already_approved_scope(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("scenario:approve-2")).await?;
    sqlx::query("UPDATE issues SET input_kind = 'scenario' WHERE key = $1")
        .bind("scenario:approve-2")
        .execute(db.pool())
        .await?;
    let scope_id = crate::issues::store::insert_scope(
        db.pool(),
        &crate::issues::model::NewScope {
            issue: "scenario:approve-2".to_string(),
            pack_digest: Some("v1:deadbeef".to_string()),
            check_outcome: Some("OK".to_string()),
        },
    )
    .await?;
    crate::issues::store::record_approval(db.pool(), scope_id, "admin-a", "2026-01-01T00:00:00Z")
        .await?;
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);

    let res = app
        .oneshot(
            HttpRequest::post("/api/scenarios/scenario%3Aapprove-2/approve")
                .header("x-auth-request-user", "wren")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["approved_by"], "admin-a");
    assert_eq!(v["approved_at"], "2026-01-01T00:00:00Z");

    let scope = crate::issues::store::latest_scope_for_issue(db.pool(), "scenario:approve-2")
        .await?
        .expect("scope");
    assert_eq!(
        scope.approved_by.as_deref(),
        Some("admin-a"),
        "the original stamp survives the second caller's attempt"
    );

    let events = db.events().read_for_key("scenario:approve-2").await?;
    assert!(
        events.iter().all(|e| e.actor.as_deref() != Some("wren")),
        "the rejected re-approve must not append a misleading audit event"
    );
    Ok(())
}

// --- external-run emission (`POST /api/emissions/run`) ---------------------------------------

/// A real in-memory emitter for the endpoint tests: records creates, mints sequential ids.
#[derive(Default)]
struct MemEmitter {
    created: std::sync::Mutex<Vec<crate::launches::tracker::NewTrackerIssue>>,
}

impl crate::launches::tracker::IssueEmitter for MemEmitter {
    fn create_issue(
        &self,
        issue: crate::launches::tracker::NewTrackerIssue,
    ) -> crate::daemon::queue::BoxFuture<anyhow::Result<String>> {
        let mut created = self.created.lock().unwrap();
        created.push(issue);
        let id = format!("EMIT-{}", created.len());
        Box::pin(async move { Ok(id) })
    }

    fn update_issue(
        &self,
        _id: &str,
        _issue: crate::launches::tracker::NewTrackerIssue,
    ) -> crate::daemon::queue::BoxFuture<anyhow::Result<()>> {
        Box::pin(async move { Ok(()) })
    }

    fn add_web_link(
        &self,
        _id: &str,
        _url: &str,
        _title: &str,
    ) -> crate::daemon::queue::BoxFuture<anyhow::Result<()>> {
        Box::pin(async move { Ok(()) })
    }
}

fn app_with_emission(db: Db, admins: Vec<String>, emitter: Option<Arc<MemEmitter>>) -> Router {
    router(ApiState {
        roles: crate::identity::auth::Roles::new(admins, vec![], vec![]),
        emission: emitter.map(|e| crate::launches::emission::EmissionCtx {
            emitter: e,
            labels: vec!["agentops".to_string()],
            public_url: None,
        }),
        ..ApiState::test(db, Arc::new(Recorder::default()))
    })
}

const EMIT_BODY: &str = r#"{
    "issue_key": "jira:example:ACME-9093",
    "pack_digest": "sha256:abc",
    "run_id": "run-42",
    "prs": [{"url": "https://gh/pr/1", "repo": "o/r", "branch": "autoresearch/run-42/a"}],
    "justification": "deepgemm harness run"
}"#;

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn emit_run_unconfigured_returns_503(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_emission(db, vec!["wren".to_string()], None);
    let res = app
        .oneshot(
            HttpRequest::post("/api/emissions/run")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(EMIT_BODY))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn emit_run_requires_admin(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let emitter = Arc::new(MemEmitter::default());
    let app = app_with_emission(db, vec!["wren".to_string()], Some(emitter.clone()));
    let res = app
        .oneshot(
            HttpRequest::post("/api/emissions/run")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "mallory")
                .body(Body::from(EMIT_BODY))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert!(emitter.created.lock().unwrap().is_empty());
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn emit_run_files_trail_idempotently_and_ledgers_the_actor(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let emitter = Arc::new(MemEmitter::default());
    let app = app_with_emission(db.clone(), vec!["wren".to_string()], Some(emitter.clone()));

    let res = app
        .clone()
        .oneshot(
            HttpRequest::post("/api/emissions/run")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(EMIT_BODY))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::CREATED);
    let v: serde_json::Value =
        serde_json::from_slice(&axum::body::to_bytes(res.into_body(), usize::MAX).await?)?;
    assert_eq!(v["issues_created"], 2, "container + task");
    assert_eq!(v["issues_updated"], 0);
    {
        let created = emitter.created.lock().unwrap();
        assert_eq!(created.len(), 2, "container + task");
        assert_eq!(created[1].parent.as_deref(), Some("EMIT-1"));
        assert!(created[1].body.contains("https://gh/pr/1"));
    }

    // Replay: nothing new filed, both issues patched in place, still 201.
    let res = app
        .oneshot(
            HttpRequest::post("/api/emissions/run")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(EMIT_BODY))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::CREATED);
    let v: serde_json::Value =
        serde_json::from_slice(&axum::body::to_bytes(res.into_body(), usize::MAX).await?)?;
    assert_eq!(v["issues_created"], 0, "replay files nothing new");
    assert_eq!(v["issues_updated"], 2, "replay patches both in place");
    assert_eq!(emitter.created.lock().unwrap().len(), 2);

    let events = db.events().read_for_key("jira:example:ACME-9093").await?;
    assert!(
        events.iter().any(|e| e.actor.as_deref() == Some("wren")
            && e.reason.as_deref().unwrap_or_default().contains("run-42")),
        "audit event names the actor + run"
    );
    Ok(())
}

const EXTERNAL_SESSION: &str = r#"{"v":1,"kind":"identity","identity":{"digest":"v1:feed"}}
{"v":1,"kind":"row","row":{"iter":0,"decision":"baseline","score":300.0},"solved":false}
{"v":1,"kind":"row","row":{"iter":1,"decision":"keep","score":250.0},"solved":false}
{"v":1,"kind":"budget","spent":1.5,"elapsed_secs":200}
{"v":1,"kind":"summary","rows":[],"gate":"bench","best_score":250.0}
{"v":1,"kind":"pr_links","links":[{"url":"https://gh/pr/7","repo":"o/r","name":"","branch":"autoresearch/x/0"}]}
{"v":1,"kind":"shutdown","outcome":"finished","reason":"done"}"#;

/// The external-run approval: PUT ingests a scope-less run so the SPA renders it; a re-PUT REPLACES
/// the row (patch-not-skip, same stance as emissions); a dispatched (scoped) run is refused.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn external_run_session_ingests_replaces_and_refuses_scoped(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_emission(db.clone(), vec!["wren".to_string()], None);

    let put = |body: &'static str| {
        HttpRequest::put("/api/runs/run-ext-1/session?justification=integration+test")
            .header(header::CONTENT_TYPE, "application/x-ndjson")
            .header("x-auth-request-user", "wren")
            .body(Body::from(body))
    };

    let res = app.clone().oneshot(put(EXTERNAL_SESSION)?).await?;
    assert_eq!(res.status(), StatusCode::CREATED);
    let v: serde_json::Value =
        serde_json::from_slice(&axum::body::to_bytes(res.into_body(), usize::MAX).await?)?;
    assert_eq!(v["replaced"], false);
    assert_eq!(v["status"], "finished");
    assert_eq!(v["pr_links"], 1);
    let run = crate::runs::store::get_run(db.pool(), "run-ext-1")
        .await?
        .expect("runs row folded");
    assert_eq!(run.scope, None, "external runs carry no scope");
    assert_eq!(run.best_score, Some(250.0));

    // Re-upload: replaced in place, not duplicated, not skipped.
    let res = app.clone().oneshot(put(EXTERNAL_SESSION)?).await?;
    assert_eq!(res.status(), StatusCode::CREATED);
    let v: serde_json::Value =
        serde_json::from_slice(&axum::body::to_bytes(res.into_body(), usize::MAX).await?)?;
    assert_eq!(v["replaced"], true);

    // A scoped (dispatched) run refuses external replace.
    crate::issues::store::upsert_issue(
        db.pool(),
        &crate::issues::model::NewIssue {
            key: "o/r#1".into(),
            repo: "o/r".into(),
            priority: 0,
            evidence_url: None,
            title: Some("t".into()),
            author: None,
            body: None,
            labels: vec![],
            upstream_updated_at: None,
        },
    )
    .await?;
    let scope_id = crate::issues::store::insert_scope(
        db.pool(),
        &crate::issues::model::NewScope {
            issue: "o/r#1".into(),
            pack_digest: None,
            check_outcome: None,
        },
    )
    .await?;
    crate::runs::store::insert_run(
        db.pool(),
        &crate::runs::model::NewRun {
            run_id: "run-scoped-1".into(),
            scope: Some(scope_id),
            issue: None,
            identity_digest: None,
            status: "running".into(),
            pod: None,
            session_uri: None,
            best_score: None,
            cost_usd: None,
        },
    )
    .await?;
    let res = app
        .oneshot(
            HttpRequest::put("/api/runs/run-scoped-1/session?justification=x")
                .header(header::CONTENT_TYPE, "application/x-ndjson")
                .header("x-auth-request-user", "wren")
                .body(Body::from(EXTERNAL_SESSION))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    Ok(())
}

// --- span-enriched flow ------------------------------------------------------

/// A run whose `session_uri` points at a real `session.jsonl` in `dir`.
async fn insert_flow_run(db: &Db, run_id: &str, dir: &std::path::Path) -> Result<()> {
    std::fs::write(dir.join("session.jsonl"), "{\"turn\":1}\n")?;
    crate::runs::store::insert_run(
        db.pool(),
        &NewRun {
            run_id: run_id.to_string(),
            scope: None,
            issue: None,
            identity_digest: None,
            status: "done".to_string(),
            pod: None,
            session_uri: Some(dir.join("session.jsonl").to_string_lossy().to_string()),
            best_score: None,
            cost_usd: None,
        },
    )
    .await?;
    Ok(())
}

/// A Datadog spans API that answers every search with one page of `spans`, at `DD_API_URL`.
async fn mount_datadog(spans: serde_json::Value) -> wiremock::MockServer {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/api/v2/spans/events/search"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": spans})),
        )
        .mount(&server)
        .await;
    server
}

/// Point the flow render at `server` with dummy DD creds. No restore: every flow test sets its
/// own values and holds the crate-wide `ENV_LOCK` for its whole body.
fn set_flow_env(server: &wiremock::MockServer) {
    unsafe {
        std::env::set_var("DD_API_URL", server.uri());
        std::env::set_var("DD_API_KEY", "dummy-api");
        std::env::set_var("DD_APP_KEY", "dummy-app");
    }
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn flow_enriched_renders_caches_and_serves(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, _d) = db_with(pool);
    let dir = tempfile::tempdir()?;
    insert_flow_run(&db, "run-1", dir.path()).await?;
    let server = mount_datadog(serde_json::json!([{"attributes": {"service": "crucible"}}])).await;
    set_flow_env(&server);
    let state_dir = dir.path().join("state");
    let app = app_with_scratch_dir(db, state_dir.clone());

    let res = app
        .clone()
        .oneshot(
            HttpRequest::get("/api/runs/run-1/flow-enriched?trace_id=deadbeef")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/html; charset=utf-8"
    );
    assert_eq!(
        res.headers().get(header::CONTENT_SECURITY_POLICY).unwrap(),
        "sandbox allow-scripts"
    );
    assert_eq!(
        res.headers().get(header::CACHE_CONTROL).unwrap(),
        "public, max-age=31536000, immutable"
    );
    let etag = res.headers().get(header::ETAG).unwrap().clone();
    assert_eq!(etag, "\"run-1/flow-enriched/deadbeef\"");
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    assert!(
        body.starts_with(b"<!"),
        "a rendered page: {:.60}",
        String::from_utf8_lossy(&body)
    );
    let cached = state_dir.join("flow-cache").join("run-1--deadbeef.html");
    assert!(cached.is_file(), "render published into the cache");
    assert_eq!(
        std::fs::read(&cached)?,
        body,
        "the cache holds the served page"
    );
    assert_eq!(
        server.received_requests().await.map(|r| r.len()),
        Some(1),
        "one span search per render"
    );

    // Second GET: served off the cache, Datadog is never asked again.
    let res = app
        .clone()
        .oneshot(
            HttpRequest::get("/api/runs/run-1/flow-enriched?trace_id=deadbeef")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let again = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    assert_eq!(again, body);
    assert_eq!(
        server.received_requests().await.map(|r| r.len()),
        Some(1),
        "cache hit must not re-fetch the spans"
    );

    // If-None-Match on the returned ETag short-circuits to 304.
    let res = app
        .oneshot(
            HttpRequest::get("/api/runs/run-1/flow-enriched?trace_id=deadbeef")
                .header(header::IF_NONE_MATCH, etag)
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn flow_enriched_rejects_a_bad_trace_id(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app(db, Arc::new(Recorder::default()));
    for uri in [
        "/api/runs/run-1/flow-enriched?trace_id=abc%2F..%2Fx",
        "/api/runs/run-1/flow-enriched?trace_id=",
        "/api/runs/run-1/flow-enriched",
    ] {
        let res = app
            .clone()
            .oneshot(HttpRequest::get(uri).body(Body::empty())?)
            .await?;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "for {uri}");
    }
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn flow_enriched_404s_without_run_or_evidence_or_session(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, _d) = db_with(pool);
    let dir = tempfile::tempdir()?;
    let server = mount_datadog(serde_json::json!([])).await;
    set_flow_env(&server);

    // A run with no session evidence, and one whose session.jsonl is gone from the prefix.
    crate::runs::store::insert_run(
        db.pool(),
        &NewRun {
            run_id: "run-noev".to_string(),
            scope: None,
            issue: None,
            identity_digest: None,
            status: "done".to_string(),
            pod: None,
            session_uri: None,
            best_score: None,
            cost_usd: None,
        },
    )
    .await?;
    let gone = tempfile::tempdir()?;
    insert_flow_run(&db, "run-gone", gone.path()).await?;
    std::fs::remove_file(gone.path().join("session.jsonl"))?;
    let app = app_with_scratch_dir(db, dir.path().join("state"));

    for run in ["nope", "run-noev", "run-gone"] {
        let res = app
            .clone()
            .oneshot(
                HttpRequest::get(format!("/api/runs/{run}/flow-enriched?trace_id=abc"))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "for run {run}");
    }
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn flow_enriched_502_hides_the_dd_error_and_leaves_no_cache(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, _d) = db_with(pool);
    let dir = tempfile::tempdir()?;
    insert_flow_run(&db, "run-1", dir.path()).await?;
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(403).set_body_string("{\"errors\": [\"Forbidden\"]}"),
        )
        .mount(&server)
        .await;
    set_flow_env(&server);
    let state_dir = dir.path().join("state");
    let app = app_with_scratch_dir(db, state_dir.clone());

    let res = app
        .oneshot(
            HttpRequest::get("/api/runs/run-1/flow-enriched?trace_id=deadbeef")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    // Generic body only: the DD error (which can echo the request) stays server-side.
    assert_eq!(v["error"], "flow render failed");
    assert!(
        !state_dir
            .join("flow-cache")
            .join("run-1--deadbeef.html")
            .exists(),
        "failed render must not publish a cache entry"
    );
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn flow_enriched_503s_without_dd_creds_and_never_fetches(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, _d) = db_with(pool);
    let dir = tempfile::tempdir()?;
    insert_flow_run(&db, "run-1", dir.path()).await?;
    let server = mount_datadog(serde_json::json!([])).await;
    unsafe {
        std::env::set_var("DD_API_URL", server.uri());
        std::env::remove_var("DD_API_KEY");
        std::env::set_var("DD_APP_KEY", "dummy-app");
    }
    let app = app_with_scratch_dir(db, dir.path().join("state"));

    let res = app
        .oneshot(
            HttpRequest::get("/api/runs/run-1/flow-enriched?trace_id=abc").body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert!(v["error"].as_str().unwrap().contains("DD_API_KEY"));
    assert_eq!(
        server.received_requests().await.map(|r| r.len()),
        Some(0),
        "Datadog must never have been asked"
    );
    Ok(())
}

// --- session-backed prefs ----------------------------------------------------------------------

/// The human surface exactly as `serve` mounts it (minus the bearer guard, which wraps outside),
/// with the prod `secure` cookie flag on.
async fn app_with_sessions(db: Db, pool: PgPool) -> Router {
    let store = crate::identity::session::store(&pool)
        .await
        .expect("session store");
    crate::human_router(
        ApiState::test(db, Arc::new(Recorder::default())),
        store,
        true,
        crate::HumanAuth {
            guard: Arc::new(crate::identity::auth::BearerGuard::default()),
            routes: crate::identity::oidc::routes::AuthState {
                mode: crate::identity::auth::AuthMode::Proxy,
                oidc: None,
                pool,
                credential_keys: None,
                proxy_prefix: "/oauth2".to_string(),
            },
        },
        crate::spa::Source::Embedded,
    )
}

fn session_cookie(res: &axum::response::Response) -> Option<String> {
    res.headers()
        .get(axum::http::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn cookie_pair(set_cookie: &str) -> &str {
    set_cookie.split(';').next().unwrap_or_default()
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn prefs_roundtrip_across_requests(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool.clone());
    let app = app_with_sessions(db, pool).await;

    let res = app
        .clone()
        .oneshot(
            HttpRequest::put("/api/prefs")
                .header("x-auth-request-user", "wren")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"prefs":{"theme":"dark"}}"#))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let set_cookie = session_cookie(&res).expect("a session write must set the cookie");
    assert!(set_cookie.starts_with("crucible_session="));
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("SameSite=Lax"));
    assert!(set_cookie.contains("Secure"));

    let res = app
        .oneshot(
            HttpRequest::get("/api/prefs")
                .header("x-auth-request-user", "wren")
                .header("cookie", cookie_pair(&set_cookie))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["prefs"]["theme"], "dark");
    Ok(())
}

/// Editor prefs are not session state: a second browser, with no cookie of its own, reads back
/// what the first one wrote, and one user's document is invisible to another.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn editor_prefs_survive_a_fresh_browser_and_stay_per_user(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool.clone());
    let app = app_with_sessions(db, pool).await;

    let res = app
        .clone()
        .oneshot(
            HttpRequest::put("/api/prefs/editor")
                .header("x-auth-request-user", "wren")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"prefs":{"fontSize":13,"wordWrap":true}}"#))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);

    let res = app
        .clone()
        .oneshot(
            HttpRequest::get("/api/prefs/editor")
                .header("x-auth-request-user", "wren")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["prefs"]["fontSize"], 13);
    assert_eq!(v["prefs"]["wordWrap"], true);

    let res = app
        .clone()
        .oneshot(
            HttpRequest::get("/api/prefs/editor")
                .header("x-auth-request-user", "mallory")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(
        v["prefs"],
        serde_json::json!({}),
        "another user's prefs leaked"
    );

    let res = app
        .oneshot(
            HttpRequest::put("/api/prefs/editor")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"prefs":{"fontSize":20}}"#))?,
        )
        .await?;
    assert_eq!(
        res.status(),
        StatusCode::FORBIDDEN,
        "an anonymous request has no document to write"
    );
    Ok(())
}

/// Picker prefs share the editor's contract: identity-bound, so a fresh browser reads them back,
/// and invisible to another user.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn picker_prefs_are_identity_bound_and_separate_from_the_editors(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool.clone());
    let app = app_with_sessions(db, pool).await;

    let res = app
        .clone()
        .oneshot(
            HttpRequest::put("/api/prefs/pickers")
                .header("x-auth-request-user", "wren")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"prefs":{"ownerFavorites":["group:/groups/team-x"],"ownerFilter":"team"}}"#,
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);

    let read = |path: &'static str, user: Option<&'static str>| {
        let app = app.clone();
        async move {
            let mut req = HttpRequest::get(path);
            if let Some(user) = user {
                req = req.header("x-auth-request-user", user);
            }
            let res = app.oneshot(req.body(Body::empty())?).await?;
            assert_eq!(res.status(), StatusCode::OK);
            let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
            Ok::<serde_json::Value, anyhow::Error>(serde_json::from_slice(&body)?)
        }
    };
    let v = read("/api/prefs/pickers", Some("wren")).await?;
    assert_eq!(v["prefs"]["ownerFavorites"][0], "group:/groups/team-x");
    assert_eq!(v["prefs"]["ownerFilter"], "team");
    assert_eq!(
        read("/api/prefs/editor", Some("wren")).await?["prefs"],
        serde_json::json!({}),
        "the pickers' document is not the editor's"
    );
    assert_eq!(
        read("/api/prefs/pickers", Some("mallory")).await?["prefs"],
        serde_json::json!({}),
        "another user's prefs leaked"
    );
    assert_eq!(
        read("/api/prefs/pickers", None).await?["prefs"],
        serde_json::json!({})
    );

    let res = app
        .oneshot(
            HttpRequest::put("/api/prefs/pickers")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"prefs":{"ownerFilter":"x"}}"#))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn non_session_routes_never_set_a_cookie(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool.clone());
    let app = app_with_sessions(db, pool).await;

    let res = app
        .oneshot(
            HttpRequest::get("/api/whoami")
                .header("x-auth-request-user", "wren")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        session_cookie(&res).is_none(),
        "a route that never writes the session must not set a cookie"
    );
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn identity_change_flushes_and_cycles_the_session(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool.clone());
    let app = app_with_sessions(db, pool.clone()).await;

    let res = app
        .clone()
        .oneshot(
            HttpRequest::put("/api/prefs")
                .header("x-auth-request-user", "alice")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"prefs":{"theme":"dark"}}"#))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let alice_cookie = session_cookie(&res).expect("cookie");

    let res = app
        .oneshot(
            HttpRequest::get("/api/prefs")
                .header("x-auth-request-user", "mallory")
                .header("cookie", cookie_pair(&alice_cookie))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let mallory_cookie = session_cookie(&res).expect("the rebind must cycle the session id");
    assert_ne!(
        cookie_pair(&alice_cookie),
        cookie_pair(&mallory_cookie),
        "a rebound session kept its old id"
    );
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["prefs"], serde_json::json!({}), "alice's prefs leaked");

    let rows: i64 = sqlx::query_scalar("select count(*) from sessions")
        .fetch_one(&pool)
        .await?;
    assert_eq!(rows, 1, "the flushed session row must be deleted");
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn identified_get_without_cookie_stays_stateless(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool.clone());
    let app = app_with_sessions(db, pool.clone()).await;

    let res = app
        .oneshot(
            HttpRequest::get("/api/prefs")
                .header("x-auth-request-user", "wren")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        session_cookie(&res).is_none(),
        "an identified read must not mint a session"
    );
    let rows: i64 = sqlx::query_scalar("select count(*) from sessions")
        .fetch_one(&pool)
        .await?;
    assert_eq!(rows, 0);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn anonymous_put_prefs_is_forbidden(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool.clone());
    let app = app_with_sessions(db, pool.clone()).await;

    let res = app
        .oneshot(
            HttpRequest::put("/api/prefs")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"prefs":{"theme":"dark"}}"#))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let rows: i64 = sqlx::query_scalar("select count(*) from sessions")
        .fetch_one(&pool)
        .await?;
    assert_eq!(rows, 0, "an anonymous write must not mint a session");
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn anonymous_prefs_read_stays_stateless(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool.clone());
    let app = app_with_sessions(db, pool.clone()).await;

    let res = app
        .oneshot(HttpRequest::get("/api/prefs").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(session_cookie(&res).is_none());
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["prefs"], serde_json::json!({}));

    let rows: i64 = sqlx::query_scalar("select count(*) from sessions")
        .fetch_one(&pool)
        .await?;
    assert_eq!(rows, 0);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn oversized_prefs_are_rejected(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool.clone());
    let app = app_with_sessions(db, pool).await;

    let blob = serde_json::json!({"prefs": {"pad": "x".repeat(17 * 1024)}});
    let res = app
        .oneshot(
            HttpRequest::put("/api/prefs")
                .header("x-auth-request-user", "wren")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&blob)?))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    Ok(())
}

// --- playbook registry --------------------------------------------------------

/// A git repo holding a one-file playbook pack whose workflow is `source`. Returns the repo path.
fn playbook_fixture(dir: &std::path::Path, source: &str) -> String {
    crate::testing::fixtures::git_pack_repo(
        dir,
        crate::testing::fixtures::PLAYBOOK_REPO_MANIFEST,
        source,
    )
}

fn register_body(id: &str, repo: &str) -> String {
    serde_json::json!({
        "id": id,
        "description": "reads a paper and files a spec",
        "repo": repo,
        "git_ref": "main",
    })
    .to_string()
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn register_playbook_requires_admin(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db, vec!["wren".to_string()]);

    let res = app
        .oneshot(
            HttpRequest::post("/api/playbooks")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "mallory")
                .body(Body::from(register_body("survey", "owner/repo")))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn register_playbook_rejects_a_malformed_id(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db, vec!["wren".to_string()]);

    let res = app
        .oneshot(
            HttpRequest::post("/api/playbooks")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(register_body("Survey:One", "owner/repo")))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    Ok(())
}

/// The whole registry surface end to end: an admin's POST pins the pack and stores the form, the
/// list carries id/description/schema digest, and the schema endpoint serves the engine's own
/// document verbatim.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn registering_a_playbook_lists_it_and_serves_its_schema(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let source = crate::testing::fixtures::WORKFLOW_TOPIC;
    let repo = playbook_fixture(dir.path(), source);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);

    let res = app
        .clone()
        .oneshot(
            HttpRequest::post("/api/playbooks")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(register_body("survey", &repo)))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let ack: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(ack["id"], "survey");
    assert_eq!(ack["schema_changed"], false);
    assert_eq!(ack["rev"].as_str().unwrap_or_default().len(), 40);

    let (status, rows) = get_json(&app, "/api/playbooks").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "survey");
    assert_eq!(rows[0]["description"], "reads a paper and files a spec");
    assert_eq!(rows[0]["schema_digest"], ack["schema_digest"]);

    let (status, inspected) = get_json_object(&app, "/api/playbooks/survey").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(inspected["id"], "survey");
    assert_eq!(inspected["rev"], ack["rev"]);
    assert_eq!(inspected["files"]["workflow.star"], source);
    assert!(
        inspected["files"]["crucible.toml"]
            .as_str()
            .is_some_and(|manifest| manifest.contains("type = \"playbook\"")),
        "the inspector serves the exact pinned pack files: {inspected}"
    );

    let (status, served) = get_json_object(&app, "/api/playbooks/survey/schema").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        served,
        crate::testing::fixtures::schema_of(source),
        "the engine's document is served verbatim"
    );

    let res = app
        .oneshot(HttpRequest::get("/api/playbooks/nope/schema").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// A pack whose workflow source does not compile fails registration with the engine's own
/// `file:line:col` error — the form never renders a 500 later because it never registered.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_pack_that_does_not_compile_is_422_with_the_engine_error(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let repo = playbook_fixture(dir.path(), crate::testing::fixtures::WORKFLOW_UNPARSABLE);
    let app = app_with_admins(db, vec!["wren".to_string()]);

    let res = app
        .clone()
        .oneshot(
            HttpRequest::post("/api/playbooks")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(register_body("survey", &repo)))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let err: serde_json::Value = serde_json::from_slice(&body)?;
    assert!(
        err["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Parse error"),
        "{err}"
    );

    let (status, rows) = get_json(&app, "/api/playbooks").await;
    assert_eq!(status, StatusCode::OK);
    assert!(rows.is_empty(), "nothing half-registers");
    Ok(())
}

// --- pack import wizard -------------------------------------------------------

/// A repo holding two playbook packs in nested directories (their workflow is `source`), a
/// non-playbook pack, and a manifest with no source. Returns the repo path.
fn import_fixture(dir: &std::path::Path, source: &str) -> String {
    let repo = dir.join("importrepo");
    let d = repo.to_string_lossy().to_string();
    let git = crate::testing::fixtures::run_git;
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&["init", "--quiet", "-b", "main", &d]);
    git(&["-C", &d, "config", "user.email", "t@example.com"]);
    git(&["-C", &d, "config", "user.name", "t"]);
    for (rel, workflow_type, source) in [
        ("packs/survey", "playbook", Some(source)),
        ("packs/nested/audit", "playbook", Some(source)),
        ("packs/loop", "autoresearch", Some(source)),
        ("packs/headless", "playbook", None),
    ] {
        let root = repo.join(rel);
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::write(
            root.join("crucible.toml"),
            format!(
                "[repo]\npath = \".\"\n\n[agent]\n\n[workflow]\ntype = \"{workflow_type}\"\nfile = \"workflow.star\"\n"
            ),
        )
        .expect("manifest");
        if let Some(source) = source {
            std::fs::write(root.join("workflow.star"), source).expect("source");
        }
    }
    git(&["-C", &d, "add", "-A"]);
    git(&["-C", &d, "commit", "--quiet", "-m", "packs"]);
    d
}

/// The import wizard's fixture workflow: a defaulted `topic` (so the gate compiles it unvalued),
/// an agent that emits `paper`, and a command fanned out over it — the graph shape the gate draws.
const IMPORT_WORKFLOW: &str = concat!(
    "params = {\"topic\": {\"type\": \"string\", \"default\": \"attention\"}}\n",
    "\n",
    "read = agent(name = \"read\", prompt = \"read the paper\", model = \"opus\", emits = [\"paper\"])\n",
    "file = command(\n",
    "    name = \"file\",\n",
    "    run = \"true\",\n",
    "    depends_on = [read],\n",
    "    join = \"passed\",\n",
    "    over = read.paper,\n",
    "    max_fanout = 3,\n",
    ")\n",
    "\n",
    "workflow(type = \"playbook\", tasks = [read, file], result = file)\n",
);

/// [`IMPORT_WORKFLOW`] with a second parameter: the form a re-import bumps to.
const IMPORT_WORKFLOW_BUMPED: &str = concat!(
    "params = {\n",
    "    \"topic\": {\"type\": \"string\", \"default\": \"attention\"},\n",
    "    \"depth\": {\"type\": \"string\", \"default\": \"shallow\"},\n",
    "}\n",
    "\n",
    "read = agent(name = \"read\", prompt = \"read the paper\", model = \"opus\", emits = [\"paper\"])\n",
    "file = command(name = \"file\", run = \"true\", depends_on = [read])\n",
    "\n",
    "workflow(type = \"playbook\", tasks = [read, file], result = file)\n",
);

async fn post_json(
    app: &Router,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    post_json_as(app, uri, "wren", body).await
}

async fn post_json_as(
    app: &Router,
    uri: &str,
    user: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    send_as(app, "POST", uri, user, body).await
}

/// One import proposed by an operator: the wizard lists the pack dirs at a ref, proposes the one
/// it picked, and an admin registers the row. What registers is what was proposed, and the row a
/// shared link opens says so.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn an_operator_proposes_an_import_and_an_admin_registers_it(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let repo = import_fixture(dir.path(), IMPORT_WORKFLOW);
    let app = app_with_roles(
        db,
        Arc::new(Recorder::default()),
        vec!["wren".to_string()],
        vec!["dana".to_string()],
    );

    let (status, listed) = post_json_as(
        &app,
        "/api/playbooks/import/candidates",
        "dana",
        serde_json::json!({"repo": &repo, "git_ref": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let rev = listed["rev"].as_str().unwrap_or_default().to_string();
    let paths: Vec<&str> = listed["candidates"]
        .as_array()
        .expect("candidates")
        .iter()
        .map(|c| c["path"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        paths,
        vec!["packs/survey", "packs/nested/audit"],
        "only registrable playbook packs: {listed}"
    );

    let (status, import) = post_json_as(
        &app,
        "/api/playbooks/imports",
        "dana",
        serde_json::json!({"repo": &repo, "git_ref": "main", "path": "packs/survey"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{import}");
    let id = import["id"].as_str().unwrap_or_default().to_string();
    assert!(!id.is_empty(), "{import}");
    assert_eq!(import["rev"], serde_json::Value::String(rev.clone()));
    assert_eq!(import["status"], "pending");
    assert_eq!(import["proposed_by"], "dana");
    assert_eq!(
        import["params_schema"],
        crate::testing::fixtures::schema_of(IMPORT_WORKFLOW),
        "the form the gate renders is the engine's own document"
    );
    assert_eq!(import["graph"]["result"], "file");
    assert_eq!(
        import["graph"]["nodes"][1]["fanout"],
        serde_json::json!({"over_task": "read", "over_field": "paper", "max_fanout": 3}),
        "the mapped task carries what it maps over: {import}"
    );
    assert_eq!(import["diagnostics"], serde_json::json!([]));

    // The link, opened cold: the same gate, without re-fetching anything.
    let (status, reread) = get_json_object(&app, &format!("/api/playbooks/imports/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reread, import, "a shared link rehydrates the proposal");

    let (status, approvals) = get_json_object(&app, "/api/approvals").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(approvals["pending_imports"][0]["id"], import["id"]);
    assert_eq!(approvals["pending_imports"][0]["proposed_by"], "dana");
    assert_eq!(approvals["pending_imports"][0]["compiles"], true);

    let registration =
        serde_json::json!({"id": "survey", "description": "reads a paper and files a spec"});
    let (status, _) = post_json_as(
        &app,
        &format!("/api/playbooks/imports/{id}/register"),
        "dana",
        registration.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "an operator proposes only");

    let (status, ack) = post_json_as(
        &app,
        &format!("/api/playbooks/imports/{id}/register"),
        "wren",
        registration,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{ack}");
    assert_eq!(ack["rev"], serde_json::Value::String(rev.clone()));
    assert_eq!(ack["schema_digest"], import["schema_digest"]);

    let (status, resolved) = get_json_object(&app, &format!("/api/playbooks/imports/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resolved["status"], "registered");
    assert_eq!(resolved["playbook"], "survey");
    assert_eq!(resolved["resolved_by"], "wren");

    let (status, rows) = get_json(&app, "/api/playbooks").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["source"]["path"], "packs/survey");
    assert_eq!(rows[0]["rev"], serde_json::Value::String(rev));

    let (status, approvals) = get_json_object(&app, "/api/approvals").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        approvals["pending_imports"],
        serde_json::json!([]),
        "a resolved import is off the rail"
    );
    Ok(())
}

/// A ref that moved after the proposal answers 409: the preview still shows what was proposed,
/// and nothing pins bytes nobody looked at.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn registering_a_frozen_import_behind_a_moved_ref_is_409(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let repo = import_fixture(dir.path(), IMPORT_WORKFLOW);
    let app = app_with_admins(db, vec!["wren".to_string()]);

    let (status, import) = post_json(
        &app,
        "/api/playbooks/imports",
        serde_json::json!({"repo": &repo, "git_ref": "main", "path": "packs/survey"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{import}");
    let id = import["id"].as_str().unwrap_or_default().to_string();

    std::fs::write(
        std::path::Path::new(&repo).join("packs/survey/workflow.star"),
        format!("{IMPORT_WORKFLOW}# more\n"),
    )?;
    for args in [
        vec!["-C", repo.as_str(), "add", "-A"],
        vec!["-C", repo.as_str(), "commit", "--quiet", "-m", "move"],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(&args)
                .status()?
                .success()
        );
    }

    let (status, err) = post_json(
        &app,
        &format!("/api/playbooks/imports/{id}/register"),
        serde_json::json!({"id": "survey", "description": "reads a paper"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{err}");

    let (status, frozen) = get_json_object(&app, &format!("/api/playbooks/imports/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        frozen["rev"], import["rev"],
        "the preview is still the proposal"
    );
    assert_eq!(frozen["status"], "pending");
    let (status, rows) = get_json(&app, "/api/playbooks").await;
    assert_eq!(status, StatusCode::OK);
    assert!(rows.is_empty(), "nothing registers behind a moved ref");
    Ok(())
}

/// A discarded proposal is closed: off the rail, and no path back to a registration.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn discarding_an_import_closes_it(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let repo = import_fixture(dir.path(), IMPORT_WORKFLOW);
    let app = app_with_admins(db, vec!["wren".to_string()]);

    let (status, import) = post_json(
        &app,
        "/api/playbooks/imports",
        serde_json::json!({"repo": &repo, "git_ref": "main", "path": "packs/survey"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{import}");
    let id = import["id"].as_str().unwrap_or_default().to_string();

    let (status, discarded) = post_json(
        &app,
        &format!("/api/playbooks/imports/{id}/discard"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{discarded}");
    assert_eq!(discarded["status"], "discarded");
    assert_eq!(discarded["resolved_by"], "wren");

    let (status, err) = post_json(
        &app,
        &format!("/api/playbooks/imports/{id}/register"),
        serde_json::json!({"id": "survey", "description": "reads a paper"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{err}");

    let (status, approvals) = get_json_object(&app, "/api/approvals").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(approvals["pending_imports"], serde_json::json!([]));
    let (status, rows) = get_json(&app, "/api/playbooks").await;
    assert_eq!(status, StatusCode::OK);
    assert!(rows.is_empty());
    Ok(())
}

/// The other half of a proposal: a human opens it as a draft and edits the frozen bytes on the
/// authoring surface instead of registering them as they are.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_pending_import_opens_as_a_draft(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let repo = import_fixture(dir.path(), IMPORT_WORKFLOW);
    let app = app_with_roles(
        db,
        Arc::new(Recorder::default()),
        vec!["wren".to_string()],
        vec!["dana".to_string()],
    );

    let (status, import) = post_json_as(
        &app,
        "/api/playbooks/imports",
        "dana",
        serde_json::json!({"repo": &repo, "git_ref": "main", "path": "packs/survey"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{import}");
    let id = import["id"].as_str().unwrap_or_default().to_string();

    let (status, opened) = post_json_as(
        &app,
        &format!("/api/playbooks/imports/{id}/draft"),
        "dana",
        serde_json::json!({"id": "survey-draft", "description": "an agent's proposal, edited"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{opened}");
    assert_eq!(opened["version"], 1);

    let (status, files) = get_json_object(&app, "/api/playbook-drafts/survey-draft/files").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(files["files"]["workflow.star"], IMPORT_WORKFLOW);
    assert!(files["files"]["crucible.toml"].is_string(), "{files}");

    let (status, row) = get_json_object(&app, &format!("/api/playbooks/imports/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(row["draft_id"], "survey-draft");
    assert_eq!(row["status"], "pending", "a fork is not a resolution");
    Ok(())
}

/// The one-motion entry: a repo, a ref and a path go in, a draft comes out, and the pending import
/// it minted is what says where those bytes came from. The draft's origin names that import, and
/// its rebase source is the frozen pack.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_draft_opens_straight_from_git_carrying_its_import(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let repo = import_fixture(dir.path(), IMPORT_WORKFLOW);
    let app = app_with_roles(
        db,
        Arc::new(Recorder::default()),
        vec!["wren".to_string()],
        vec!["dana".to_string()],
    );

    let from_git = serde_json::json!({
        "id": "survey-draft",
        "description": "a pack pulled in to edit",
        "repo": &repo,
        "git_ref": "main",
        "path": "packs/survey",
    });
    let (status, opened) = post_json_as(
        &app,
        "/api/playbook-drafts/from-git",
        "dana",
        from_git.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{opened}");
    assert_eq!(opened["draft"]["version"], 1);
    let import_id = opened["import_id"].as_str().unwrap_or_default().to_string();
    let rev = opened["rev"].as_str().unwrap_or_default().to_string();
    assert!(!import_id.is_empty(), "{opened}");
    assert!(!rev.is_empty(), "{opened}");

    let (status, files) = get_json_object(&app, "/api/playbook-drafts/survey-draft/files").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(files["files"]["workflow.star"], IMPORT_WORKFLOW);

    let (status, row) = get_json_object(&app, &format!("/api/playbooks/imports/{import_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(row["draft_id"], "survey-draft");
    assert_eq!(row["status"], "pending", "a fork is not a resolution");

    let (status, draft) = get_json_object(&app, "/api/playbook-drafts/survey-draft").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(draft["origin"]["kind"], "import");
    assert_eq!(draft["origin"]["import_id"], serde_json::json!(import_id));
    assert_eq!(draft["origin"]["repo"], serde_json::json!(repo));
    assert_eq!(draft["origin"]["path"], "packs/survey");
    assert_eq!(draft["origin"]["rev"], serde_json::json!(rev));
    assert_eq!(draft["origin"]["moved"], false, "frozen bytes never move");

    let (status, origin) =
        get_json_object(&app, "/api/playbook-drafts/survey-draft/origin/files").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(origin["kind"], "import");
    assert_eq!(origin["files"]["workflow.star"], IMPORT_WORKFLOW);

    // A name already taken is refused before anything is fetched, so it mints no second import.
    let (status, clash) =
        post_json_as(&app, "/api/playbook-drafts/from-git", "dana", from_git).await;
    assert_eq!(status, StatusCode::CONFLICT, "{clash}");
    let (status, approvals) = get_json_object(&app, "/api/approvals").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        approvals["pending_imports"].as_array().map(Vec::len),
        Some(1),
        "the refused create proposed nothing: {approvals}"
    );
    Ok(())
}

/// A draft with no origin says so rather than inventing one.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_skeleton_draft_has_no_origin_to_rebase_onto(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db, vec!["wren".to_string()]);
    let (status, created) = {
        post_admin(
            &app,
            "/api/playbook-drafts",
            serde_json::json!({"id": "studio", "description": "a drafted pack"}),
        )
        .await
    };
    assert_eq!(status, StatusCode::CREATED, "{created}");

    let (status, draft) = get_json_object(&app, "/api/playbook-drafts/studio").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(draft["origin"], serde_json::Value::Null);

    let (status, _) = get_json_object(&app, "/api/playbook-drafts/studio/origin/files").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

/// Any save downloads as the bytes it stored, named for the version it is.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn any_draft_version_downloads_as_a_tarball(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db, vec!["wren".to_string()]);
    let (status, created) = {
        post_admin(
            &app,
            "/api/playbook-drafts",
            serde_json::json!({"id": "studio", "description": "a drafted pack"}),
        )
        .await
    };
    assert_eq!(status, StatusCode::CREATED, "{created}");

    let res = app
        .clone()
        .oneshot(
            HttpRequest::get("/api/playbook-drafts/studio/tarball")
                .header("x-auth-request-user", "wren")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("resp");
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/gzip")
    );
    assert_eq!(
        res.headers()
            .get(header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok()),
        Some("attachment; filename=\"studio-v1.tar.gz\"")
    );
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let pack =
        crate::playbooks::packs::unpack_to_scratch(&bytes).expect("the download is a pack tarball");
    assert!(
        pack.path().join("crucible.toml").is_file(),
        "the download unpacks to the save's own files"
    );

    let (status, _) = get_json_object(&app, "/api/playbook-drafts/studio/tarball?version=9").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get_json_object(&app, "/api/playbook-drafts/nope/tarball").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

/// The skill file a local agent is handed is generated per deployment: the configured public URL
/// is the one baked into it, and it downloads as a markdown attachment.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn the_co_draft_skill_downloads_with_this_deployments_url(pool: PgPool) -> Result<()> {
    let (db, _dir) = db_with(pool);
    let app = router(ApiState {
        public_url: Some("https://crucible.example.com".to_string()),
        ..ApiState::test(db, Arc::new(Recorder::default()))
    });

    let res = app
        .clone()
        .oneshot(
            HttpRequest::get("/api/playbooks/drafts/skill")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("resp");
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/markdown; charset=utf-8")
    );
    assert_eq!(
        res.headers()
            .get(header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok()),
        Some("attachment; filename=\"crucible-co-draft-SKILL.md\"")
    );
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let skill = String::from_utf8(bytes.to_vec()).expect("utf8");
    assert!(skill.contains("https://crucible.example.com/playbooks/drafts/<draft_id>"));
    assert!(skill.contains("crucible_draft_save"));
    assert!(skill.contains("base_version"));
    Ok(())
}

/// With no configured public URL the handoff still has to reach this controller, so it renders
/// against the host the request arrived on — and only a host, never whatever a caller asserts.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn the_co_draft_setup_falls_back_to_the_forwarded_host(pool: PgPool) -> Result<()> {
    let (db, _dir) = db_with(pool);
    let app = router(ApiState {
        dispatch: crate::playbooks::dispatch::DispatchCapability::new(
            crate::config::PlaybookExecutor::Local,
            false,
        ),
        ..ApiState::test(db, Arc::new(Recorder::default()))
    });

    let body = |req: HttpRequest<Body>| {
        let app = app.clone();
        async move {
            let res = app.oneshot(req).await.expect("resp");
            assert_eq!(res.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .expect("body");
            serde_json::from_slice::<serde_json::Value>(&bytes).expect("json")
        }
    };

    let json = body(
        HttpRequest::get("/api/playbooks/drafts/co-draft")
            .header("x-forwarded-proto", "https")
            .header("x-forwarded-host", "crucible.apps.example.com")
            .body(Body::empty())
            .expect("req"),
    )
    .await;
    assert_eq!(json["url"], "https://crucible.apps.example.com");
    let commands = json["steps"].to_string();
    assert!(
        commands.contains("https://crucible.apps.example.com/mcp"),
        "the forwarded host reaches the MCP URL too: {commands}"
    );

    let json = body(
        HttpRequest::get("/api/playbooks/drafts/co-draft")
            .header("host", "evil host; rm -rf /")
            .body(Body::empty())
            .expect("req"),
    )
    .await;
    assert_eq!(json["url"], "http://localhost");
    Ok(())
}

/// The MCP surface takes only a minted key, so the setup a human is handed mints one and points
/// the client at that surface; nothing in it asserts an identity.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn the_co_draft_setup_mints_a_key_and_points_at_the_mcp_surface(pool: PgPool) -> Result<()> {
    let (db, _dir) = db_with(pool);
    let app = router(ApiState {
        public_url: Some("https://crucible.example.com".to_string()),
        ..ApiState::test(db, Arc::new(Recorder::default()))
    });
    let (status, json) = get_json_object(&app, "/api/playbooks/drafts/co-draft").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json["skill_url"],
        "https://crucible.example.com/api/playbooks/drafts/skill"
    );
    let commands = json["steps"].to_string();
    assert!(!commands.contains("BREAK_GLASS"), "{commands}");
    assert!(
        commands.contains("export CONTROLLER_API_TOKEN=crk_"),
        "{commands}"
    );
    assert!(
        commands
            .contains("claude mcp add --transport http crucible https://crucible.example.com/mcp"),
        "{commands}"
    );
    Ok(())
}

/// A pack that does not compile still proposes: the diagnostics are what was proposed. A path
/// that is not a pack at all is the caller's mistake, and neither registers anything.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_pack_that_does_not_compile_proposes_its_error(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let repo = import_fixture(dir.path(), crate::testing::fixtures::WORKFLOW_BROKEN);
    let app = app_with_admins(db, vec!["wren".to_string()]);

    let (status, import) = post_json(
        &app,
        "/api/playbooks/imports",
        serde_json::json!({"repo": &repo, "git_ref": "main", "path": "packs/survey"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a refused compile is a preview"
    );
    assert_eq!(import["graph"], serde_json::Value::Null);
    assert!(
        import["diagnostics"][0]
            .as_str()
            .unwrap_or_default()
            .contains("workflow.star:3:39"),
        "the engine's file:line:col rides verbatim: {import}"
    );

    let (status, err) = post_json(
        &app,
        "/api/playbooks/imports",
        serde_json::json!({"repo": &repo, "git_ref": "main", "path": "packs/loop"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");

    let (status, rows) = get_json(&app, "/api/playbooks").await;
    assert_eq!(status, StatusCode::OK);
    assert!(rows.is_empty(), "a proposal never registers");
    Ok(())
}

/// Re-importing a registered pack is a pin bump: the pending import carries the new form while
/// the registry still serves the old one, so the wizard can show the diff before it is accepted.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_re_import_carries_the_new_form_while_the_registry_serves_the_old(
    pool: PgPool,
) -> Result<()> {
    let (db, dir) = db_with(pool);
    let repo = import_fixture(dir.path(), IMPORT_WORKFLOW);
    let app = app_with_admins(db, vec!["wren".to_string()]);

    let (status, ack) = post_json(
        &app,
        "/api/playbooks",
        serde_json::json!({
            "id": "survey",
            "description": "reads a paper",
            "repo": &repo,
            "git_ref": "main",
            "path": "packs/survey",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{ack}");

    // The pack's form changes at the next ref.
    std::fs::write(
        std::path::Path::new(&repo).join("packs/survey/workflow.star"),
        IMPORT_WORKFLOW_BUMPED,
    )?;
    for args in [
        vec!["-C", repo.as_str(), "add", "-A"],
        vec!["-C", repo.as_str(), "commit", "--quiet", "-m", "bump"],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(&args)
                .status()
                .expect("git runs")
                .success(),
            "git {args:?}"
        );
    }

    let (status, import) = post_json(
        &app,
        "/api/playbooks/imports",
        serde_json::json!({"repo": &repo, "git_ref": "main", "path": "packs/survey"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{import}");
    assert_eq!(
        import["params_schema"],
        crate::testing::fixtures::schema_of(IMPORT_WORKFLOW_BUMPED),
        "the proposal is the new form"
    );

    let (status, served) = get_json_object(&app, "/api/playbooks/survey/schema").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        served,
        crate::testing::fixtures::schema_of(IMPORT_WORKFLOW),
        "the registry still serves the old form until the bump is accepted"
    );

    let id = import["id"].as_str().unwrap_or_default().to_string();
    let (status, bump) = post_json(
        &app,
        &format!("/api/playbooks/imports/{id}/register"),
        serde_json::json!({"id": "survey", "description": "reads a paper"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{bump}");
    assert_eq!(bump["schema_changed"], true, "the form changed: {bump}");
    assert_eq!(bump["schema_digest"], import["schema_digest"]);
    Ok(())
}

/// Proposing is operator-level, registering is admin-only, and a viewer does neither.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn import_endpoints_hold_their_roles(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_roles(
        db,
        Arc::new(Recorder::default()),
        vec!["wren".to_string()],
        vec!["dana".to_string()],
    );

    for uri in ["/api/playbooks/import/candidates", "/api/playbooks/imports"] {
        let (status, _) = post_json_as(
            &app,
            uri,
            "mallory",
            serde_json::json!({"repo": "owner/repo"}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
    }

    // Admin-only, whatever the row: the guard runs before the lookup.
    let (status, _) = post_json_as(
        &app,
        "/api/playbooks/imports/nope/register",
        "dana",
        serde_json::json!({"id": "survey", "description": "d"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = post_json_as(
        &app,
        "/api/playbooks/imports/nope/register",
        "wren",
        serde_json::json!({"id": "survey", "description": "d"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

// --- playbook launch ----------------------------------------------------------

/// A workflow whose params exercise the closed subset the launch form renders: a required
/// patterned string and a second required field, with nothing undeclared allowed through.
const LAUNCH_WORKFLOW: &str = concat!(
    "params = {\n",
    "    \"topic\": {\"type\": \"string\", \"required\": True, \"pattern\": \"^[a-z ]+$\"},\n",
    "    \"depth\": {\"type\": \"string\", \"required\": True},\n",
    "}\n",
    "\n",
    "hello = command(name = \"hello\", run = \"echo hello\")\n",
    "\n",
    "workflow(type = \"playbook\", tasks = [hello], result = hello)\n",
);

fn app_with_playbook_caps(
    db: Db,
    admins: Vec<String>,
    caps: crate::config::PlaybookCaps,
) -> Router {
    router(ApiState {
        roles: crate::identity::auth::Roles::new(admins, vec![], vec![]),
        playbook_caps: caps,
        ..ApiState::test(db, Arc::new(Recorder::default()))
    })
}

/// The launch form reads its ceiling bounds from the deploy, so the caps endpoint serves the same
/// numbers `check_ceilings` refuses against.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn playbook_caps_endpoint_serves_the_deploy_bounds(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let caps = crate::config::PlaybookCaps {
        max_cost: 12.5,
        max_time: crate::model::MaxTime::parse("90m").expect("cap"),
    };
    let app = app_with_playbook_caps(db, vec![], caps);
    let res = app
        .oneshot(HttpRequest::get("/api/config/playbook-caps").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["max_cost"], serde_json::json!(12.5));
    assert_eq!(v["max_time"], serde_json::json!("90m"));
    Ok(())
}

/// Register `survey` through the real registration path (a git pack + a stub engine that prints
/// `schema`), and return the stored schema digest the launch form would have been rendered with.
async fn register_survey(app: &Router, dir: &std::path::Path, schema: &str) -> String {
    register_survey_with(
        app,
        dir,
        schema,
        "[repo]\npath = \".\"\n\n[agent]\n\n[workflow]\ntype = \"playbook\"\nfile = \"workflow.star\"\n",
    )
    .await
}

async fn register_survey_with(
    app: &Router,
    dir: &std::path::Path,
    schema: &str,
    manifest: &str,
) -> String {
    let repo = crate::testing::fixtures::git_pack_repo(dir, manifest, schema);
    let res = app
        .clone()
        .oneshot(
            HttpRequest::post("/api/playbooks")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(register_body("survey", &repo)))
                .expect("req"),
        )
        .await
        .expect("resp");
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let ack: serde_json::Value = serde_json::from_slice(&body).expect("json");
    ack["schema_digest"].as_str().expect("digest").to_string()
}

async fn post_launch(
    app: &Router,
    id: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let (status, bytes) = send(
        app,
        "POST",
        &format!("/api/playbooks/{id}/launch"),
        "wren",
        Some(body),
    )
    .await;
    (status, serde_json::from_slice(&bytes).expect("json"))
}

/// The endpoint is the enforcement the SPA form is a convenience over: values that miss the pack's
/// schema come back as a field-level list, addressed by the input that produced them.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn launching_with_bad_values_is_422_naming_each_field(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    let (status, body) = post_launch(
        &app,
        "survey",
        serde_json::json!({
            "params": {"topic": "ATTENTION"},
            "max_cost": 3.0,
            "max_time": "30m",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let fields: Vec<&str> = body["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .map(|f| f["field"].as_str().unwrap_or_default())
        .collect();
    assert!(fields.contains(&"topic"), "{body}");
    assert!(fields.contains(&"depth"), "{body}");
    assert!(
        crate::issues::store::get_issue(db.pool(), "playbook:survey:x")
            .await?
            .is_none(),
        "a refused launch adopts nothing"
    );
    Ok(())
}

/// The manifest a pack that needs an OpenShell sandbox declares.
const OPENSHELL_MANIFEST: &str = "[repo]\npath = \".\"\n\n[workflow]\ntype = \"playbook\"\nfile = \"workflow.star\"\n\n\
     [agent]\nbackend = \"openshell\"\nsandbox_image = \"quay.io/x/sandbox:dev\"\n";

/// A pod deployment with no deploy profile cannot give an OpenShell pack its sandbox, and says so
/// at launch — where a person is watching — instead of at the bottom of a failed reconcile.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn launching_a_backend_this_deployment_cannot_dispatch_is_refused(
    pool: PgPool,
) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = router(ApiState {
        roles: crate::identity::auth::Roles::new(vec!["wren".to_string()], vec![], vec![]),
        dispatch: crate::playbooks::dispatch::DispatchCapability::new(
            crate::config::PlaybookExecutor::Pod,
            false,
        ),
        ..ApiState::test(db.clone(), Arc::new(Recorder::default()))
    });
    let digest = register_survey_with(&app, dir.path(), LAUNCH_WORKFLOW, OPENSHELL_MANIFEST).await;

    // The registry says what the pack needs and that this deployment cannot give it, before anyone
    // fills the form in.
    let res = app
        .clone()
        .oneshot(
            HttpRequest::get("/api/playbooks")
                .header("x-auth-request-user", "wren")
                .body(Body::empty())?,
        )
        .await?;
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let listed: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(listed[0]["dispatch"]["backend"], "openshell");
    assert_eq!(
        listed[0]["dispatch"]["sandbox_image"],
        "quay.io/x/sandbox:dev"
    );
    assert_eq!(listed[0]["dispatch"]["dispatchable"], false);
    assert_eq!(listed[0]["dispatch"]["local_mode"], false);

    let (status, body) = post_launch(
        &app,
        "survey",
        serde_json::json!({
            "params": {"topic": "attention sinks", "depth": "deep"},
            "max_cost": 3.5,
            "max_time": "30m",
            "schema_digest": digest,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let message = body["fields"][0]["message"].as_str().unwrap_or_default();
    assert!(message.contains("CONTROLLER_DEPLOY_PROFILE"), "{body}");
    assert_eq!(
        body["error"], "this deployment cannot dispatch the pack's agent backend",
        "the headline names the refusal; the parameters were fine"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM playbook_launches")
            .fetch_one(db.pool())
            .await?,
        0,
        "a refused launch adopts nothing"
    );
    Ok(())
}

/// The same pack on a deployment that can reach a cluster launches: the capability is the
/// deployment's, not the pack's.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn the_same_backend_launches_where_a_cluster_is_reachable(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = router(ApiState {
        roles: crate::identity::auth::Roles::new(vec!["wren".to_string()], vec![], vec![]),
        dispatch: crate::playbooks::dispatch::DispatchCapability::new(
            crate::config::PlaybookExecutor::Pod,
            true,
        ),
        ..ApiState::test(db.clone(), Arc::new(Recorder::default()))
    });
    let digest = register_survey_with(&app, dir.path(), LAUNCH_WORKFLOW, OPENSHELL_MANIFEST).await;
    let (status, ack) = post_launch(
        &app,
        "survey",
        serde_json::json!({
            "params": {"topic": "attention sinks", "depth": "deep"},
            "max_cost": 3.5,
            "max_time": "30m",
            "schema_digest": digest,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{ack}");
    Ok(())
}

/// A valid launch lands the whole authorization: the issue row at `new`, the values and ceilings
/// the dispatch will re-read, and the launch's own copy of the registered pack.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_valid_launch_adopts_the_issue_the_row_and_the_pack(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    let digest = register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    let (status, ack) = post_launch(
        &app,
        "survey",
        serde_json::json!({
            "params": {"topic": "attention sinks", "depth": "deep"},
            "max_cost": 3.5,
            "max_time": "30m",
            "schema_digest": digest,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{ack}");
    let key = ack["key"].as_str().expect("key").to_string();
    assert!(key.starts_with("playbook:survey:"), "{key}");
    assert_eq!(ack["max_time"], "30m");
    assert_eq!(ack["actor"], "wren");

    let issue = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .expect("issue");
    assert_eq!(issue.status, crate::model::Status::New);
    assert_eq!(issue.kind.tag(), "playbook");
    let registered = crate::playbooks::registry::get(db.pool(), "survey")
        .await?
        .expect("registry row");
    assert_eq!(
        Some(issue.repo.as_str()),
        registered.source.repo(),
        "a launch groups under the pack's source repo"
    );

    let launch = crate::launches::store::get_playbook_launch(db.pool(), &key)
        .await?
        .expect("launch row");
    assert_eq!(launch.playbook, "survey");
    assert_eq!(
        launch.params,
        vec![
            ("depth".to_string(), "deep".to_string()),
            ("topic".to_string(), "attention sinks".to_string()),
        ],
        "values read back sorted, so a launch always renders the same argv"
    );
    assert_eq!(launch.schema_digest, digest);
    assert!((launch.max_cost - 3.5).abs() < 1e-9);
    assert_eq!(launch.max_time.as_str(), "30m");

    // The launch owns a copy of the pack, so a later re-pin of the registry cannot change what
    // this run executes.
    assert!(
        crate::playbooks::packs::materialize_pack(db.pool(), &key)
            .await?
            .is_some(),
        "the launch key materializes its own pack"
    );
    Ok(())
}

/// Ceilings are the launcher's, and the admin caps are the only bound on them.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn ceilings_above_the_admin_caps_are_refused(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let caps = crate::config::PlaybookCaps {
        max_cost: 5.0,
        max_time: crate::model::MaxTime::parse("1h").expect("duration"),
    };
    let app = app_with_playbook_caps(db, vec!["wren".to_string()], caps);
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    let good_params = serde_json::json!({"topic": "attention sinks", "depth": "deep"});
    for (max_cost, max_time) in [(50.0, "30m"), (3.0, "2h"), (0.0, "30m"), (3.0, "forever")] {
        let (status, body) = post_launch(
            &app,
            "survey",
            serde_json::json!({
                "params": good_params,
                "max_cost": max_cost,
                "max_time": max_time,
            }),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "max_cost {max_cost} / max_time {max_time}: {body}"
        );
        let field = body["fields"][0]["field"].as_str().unwrap_or_default();
        assert!(
            field == "max_cost" || field == "max_time",
            "a refused ceiling names its own input: {body}"
        );
    }
    Ok(())
}

/// A form rendered before a pin bump is refused rather than launched against values the current
/// schema never validated.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_stale_schema_digest_is_a_conflict(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db, vec!["wren".to_string()]);
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    let (status, _) = post_launch(
        &app,
        "survey",
        serde_json::json!({
            "params": {"topic": "attention sinks", "depth": "deep"},
            "max_cost": 3.0,
            "max_time": "30m",
            "schema_digest": "sha256:stale",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn launching_an_unregistered_playbook_is_404(pool: PgPool) -> Result<()> {
    let (db, _dir) = db_with(pool);
    let app = app_with_admins(db, vec!["wren".to_string()]);
    let (status, _) = post_launch(
        &app,
        "nope",
        serde_json::json!({"params": {}, "max_cost": 3.0, "max_time": "30m"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn dispatch_targets_for_an_unregistered_playbook_is_404(pool: PgPool) -> Result<()> {
    let (db, _dir) = db_with(pool);
    let app = app(db, Arc::new(Recorder::default()));
    let res = app
        .oneshot(HttpRequest::get("/api/dispatch-targets?playbook=nope").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// A playbook somebody else owns does not exist to a caller with no role in it or share on it,
/// whatever else is wrong with the request: nothing about the pack is checked before the decision.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn launching_a_playbook_the_caller_may_not_read_is_not_found(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;
    for body in [
        serde_json::json!({
            "params": {"topic": "attention", "depth": "deep"},
            "max_cost": 1.0,
            "max_time": "5m",
        }),
        serde_json::json!({"params": {"topic": "NOT VALID"}, "max_cost": 1.0, "max_time": "5m"}),
        serde_json::json!({
            "params": {"topic": "attention", "depth": "deep"},
            "max_cost": 1.0,
            "max_time": "5m",
            "schema_digest": "stale",
        }),
        serde_json::json!({"params": {}, "max_cost": 1e9, "max_time": "5m"}),
    ] {
        let (status, refused) = send_as(
            &app,
            "POST",
            "/api/playbooks/survey/launch",
            "mallory",
            body.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}: {refused}");
    }
    let launches: i64 = sqlx::query_scalar("SELECT count(*) FROM playbook_launches")
        .fetch_one(db.pool())
        .await?;
    assert_eq!(launches, 0);
    Ok(())
}

// --- one-shot runs surface ----------------------------------------------------

async fn get_json_value(app: &Router, path: &str) -> (StatusCode, serde_json::Value) {
    let (status, bytes) = send(app, "GET", path, "wren", None).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn post_one_shot(app: &Router, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let (status, bytes) = send(app, "POST", "/api/one-shots", "wren", Some(body)).await;
    (status, serde_json::from_slice(&bytes).expect("json"))
}

async fn delete_one_shot(app: &Router, id: &str) -> (StatusCode, serde_json::Value) {
    let (status, bytes) = send(app, "DELETE", &format!("/api/one-shots/{id}"), "wren", None).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

fn one_shot_body(fire_at: &str) -> serde_json::Value {
    serde_json::json!({
        "playbook": "survey",
        "params": {"topic": "attention sinks", "depth": "deep"},
        "max_cost": 3.5,
        "max_time": "30m",
        "fire_at": fire_at,
    })
}

/// The runs section is the relaunch source: every ad-hoc launch with the values and ceilings it
/// froze, its outcome, and its cost — and a schedule's firing is not one of them.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn the_runs_list_carries_the_snapshot_and_leaves_schedules_out(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    let (status, ack) = post_launch(
        &app,
        "survey",
        serde_json::json!({
            "params": {"topic": "attention sinks", "depth": "deep"},
            "max_cost": 3.5,
            "max_time": "30m",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{ack}");
    let key = ack["key"].as_str().expect("key").to_string();

    // A schedule's firing lands the same row with a different origin (WI-005 writes these); the
    // one-shot surface must not claim it.
    let scheduled = "playbook:survey:scheduled-1";
    sqlx::query(
        "INSERT INTO issues (key, repo, tier, status, priority, input_kind, title, updated_at) \
         VALUES ($1, 'owner/repo', 'T1', 'new', 0, 'playbook', 'sched', '2026-08-23T00:00:00Z')",
    )
    .bind(scheduled)
    .execute(db.pool())
    .await?;
    sqlx::query(
        "INSERT INTO playbook_launches (key, playbook, params, schema_digest, max_cost, max_time, \
         advance_dedupe, origin, created_at) \
         VALUES ($1, 'survey', '{}'::jsonb, 'sha256:x', 1.0, '5m', TRUE, 'schedule', \
         '2026-08-23T00:00:00Z')",
    )
    .bind(scheduled)
    .execute(db.pool())
    .await?;

    let (status, rows) = get_json_value(&app, "/api/playbook-runs").await;
    assert_eq!(status, StatusCode::OK);
    let rows = rows.as_array().expect("array");
    assert_eq!(
        rows.len(),
        1,
        "the scheduled firing stays off this list: {rows:?}"
    );
    let row = &rows[0];
    assert_eq!(row["key"], key.as_str());
    assert_eq!(row["origin"], "manual");
    assert_eq!(row["status"], "new");
    assert_eq!(row["advance_dedupe"], false);
    assert_eq!(row["max_time"], "30m");
    assert_eq!(row["runs"], 0);
    assert!(row["cost_usd"].is_null(), "no run has booked spend yet");
    assert!(
        row["agent_provider"].is_null() && row["agent_model"].is_null(),
        "a launch that pinned nothing resolves the defaults at dispatch: {row}"
    );
    assert_eq!(
        row["params"],
        serde_json::json!({"topic": "attention sinks", "depth": "deep"}),
        "the snapshot is what a relaunch prefills from"
    );

    // The single-launch read is the prefill endpoint, and it reads a launch of any origin.
    let (status, one) = get_json_value(
        &app,
        &format!("/api/playbook-runs/{}", urlencoding_encode(&key)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{one}");
    assert_eq!(one["launch"]["params"], row["params"]);
    assert_eq!(one["launch"]["max_cost"], 3.5);
    assert_eq!(one["launch"]["schema_drifted"], false);
    assert_eq!(one["dispatch"]["state"], "pending");
    assert_eq!(one["runs"], serde_json::json!([]));
    assert_eq!(one["source_exists"], true);

    let (status, scheduled_row) = get_json_value(
        &app,
        &format!("/api/playbook-runs/{}", urlencoding_encode(scheduled)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{scheduled_row}");
    assert_eq!(scheduled_row["launch"]["origin"], "schedule");

    let (status, _) = get_json_value(&app, "/api/playbook-runs/playbook:survey:nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

/// The launch view's half of the by-key read: dispatch is a state of its own, a failed attempt
/// carries the error that stopped it, and a dispatched launch lists the runs it produced.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_launch_reads_back_its_dispatch_state_and_runs(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    let (status, ack) = post_launch(
        &app,
        "survey",
        serde_json::json!({
            "params": {"topic": "attention sinks", "depth": "deep"},
            "max_cost": 3.5,
            "max_time": "30m",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{ack}");
    let key = ack["key"].as_str().expect("key").to_string();
    let path = format!("/api/playbook-runs/{}", urlencoding_encode(&key));

    db.events()
        .append(&crate::event_log::Event::now(
            &key,
            "new",
            "new",
            Some(crate::runs::launch::PLAYBOOK_DISPATCH_FAILED),
            Some("dispatch_run needs a deploy profile"),
        ))
        .await?;

    let (status, failed) = get_json_value(&app, &path).await;
    assert_eq!(status, StatusCode::OK, "{failed}");
    assert_eq!(failed["dispatch"]["state"], "failed");
    assert_eq!(
        failed["dispatch"]["failure"], "dispatch_run needs a deploy profile",
        "the reason a person looks for is the error chain, not the headline: {failed}"
    );
    assert_eq!(failed["dispatch"]["failures"], 1);

    let run_id = format!("{}-1787517814", crate::model::sanitize_key(&key));
    crate::runs::store::insert_run(
        db.pool(),
        &NewRun {
            run_id: run_id.clone(),
            scope: None,
            issue: Some(key.clone()),
            identity_digest: None,
            status: "running".to_string(),
            pod: None,
            session_uri: None,
            best_score: None,
            cost_usd: Some(0.67),
        },
    )
    .await?;
    crate::runs::store::set_run_dispatch(
        db.pool(),
        &run_id,
        crate::runs::model::RunDispatch::Local,
    )
    .await?;

    let (status, dispatched) = get_json_value(&app, &path).await;
    assert_eq!(status, StatusCode::OK, "{dispatched}");
    assert_eq!(dispatched["dispatch"]["state"], "dispatched");
    assert_eq!(
        dispatched["dispatch"]["failures"], 1,
        "a run that took two tries still says the first one failed"
    );
    let runs = dispatched["runs"].as_array().expect("runs");
    assert_eq!(runs.len(), 1, "{dispatched}");
    assert_eq!(runs[0]["run_id"], run_id.as_str());
    assert_eq!(runs[0]["dispatch"], "local");
    assert_eq!(runs[0]["cost_usd"], 0.67);

    // The run reads its way back to the launch, which has no scope row to join through.
    let (status, run) = get_json_value(&app, &format!("/api/runs/{run_id}")).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["run"]["issue_key"], key.as_str());
    Ok(())
}

/// Percent-encode a launch key for a path segment, the way the SPA's `encodeURIComponent` does.
fn urlencoding_encode(key: &str) -> String {
    key.replace(':', "%3A")
}

/// The dedupe opt-in is the launcher's, and it defaults to off: an ad-hoc experiment must not eat
/// the next scheduled sweep's inputs.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn dedupe_advancement_is_off_unless_opted_in(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;
    let params = serde_json::json!({"topic": "attention sinks", "depth": "deep"});

    let (status, plain) = post_launch(
        &app,
        "survey",
        serde_json::json!({"params": params, "max_cost": 3.0, "max_time": "30m"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{plain}");
    assert_eq!(plain["advance_dedupe"], false);

    let (status, opted) = post_launch(
        &app,
        "survey",
        serde_json::json!({
            "params": params,
            "max_cost": 3.0,
            "max_time": "30m",
            "advance_dedupe": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{opted}");
    assert_eq!(opted["advance_dedupe"], true);

    let stored: Vec<(String, bool)> =
        sqlx::query_as("SELECT key, advance_dedupe FROM playbook_launches ORDER BY key")
            .fetch_all(db.pool())
            .await?;
    let plain_key = plain["key"].as_str().expect("key");
    let opted_key = opted["key"].as_str().expect("key");
    assert_eq!(
        stored.iter().find(|(k, _)| k == plain_key).map(|(_, f)| *f),
        Some(false)
    );
    assert_eq!(
        stored.iter().find(|(k, _)| k == opted_key).map(|(_, f)| *f),
        Some(true)
    );
    Ok(())
}

/// A deferred one-shot is validated the moment it is created, against the same schema and the same
/// caps an immediate launch answers to — the deferral is the only difference.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_one_shot_is_validated_at_creation_and_frozen(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    let digest = register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    let (status, refused) = post_one_shot(
        &app,
        serde_json::json!({
            "playbook": "survey",
            "params": {"topic": "ATTENTION"},
            "max_cost": 3.0,
            "max_time": "30m",
            "fire_at": "2099-01-01T00:00:00Z",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let fields: Vec<&str> = refused["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .map(|f| f["field"].as_str().unwrap_or_default())
        .collect();
    assert!(fields.contains(&"topic"), "{refused}");
    assert!(fields.contains(&"depth"), "{refused}");

    // A ceiling over the cap is refused here exactly as it is on the launch endpoint.
    let (status, _) = post_one_shot(
        &app,
        serde_json::json!({
            "playbook": "survey",
            "params": {"topic": "attention sinks", "depth": "deep"},
            "max_cost": 3.0,
            "max_time": "forever",
            "fire_at": "2099-01-01T00:00:00Z",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // A non-UTC offset is stored as UTC, so the sweep's lexicographic `fire_at <= now` is sound.
    let (status, created) = post_one_shot(&app, one_shot_body("2099-01-01T02:00:00+02:00")).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["fire_at"], "2099-01-01T00:00:00Z");
    assert_eq!(created["status"], "pending");
    assert_eq!(created["advance_dedupe"], false);
    assert_eq!(created["schema_digest"], digest.as_str());
    assert!(created["fired_key"].is_null());
    assert_eq!(
        created["params"],
        serde_json::json!({"topic": "attention sinks", "depth": "deep"})
    );

    // Nothing is adopted until it fires.
    let (_, runs) = get_json_value(&app, "/api/playbook-runs").await;
    assert!(runs.as_array().expect("array").is_empty());

    let (status, listed) = get_json_value(&app, "/api/one-shots").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.as_array().expect("array").len(), 1);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_one_shot_in_the_past_or_unparseable_is_refused(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db, vec!["wren".to_string()]);
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    for fire_at in [
        "2020-01-01T00:00:00Z",
        "tomorrow",
        "",
        "2026-13-01T00:00:00Z",
    ] {
        let (status, body) = post_one_shot(&app, one_shot_body(fire_at)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{fire_at:?}");
        assert_eq!(body["fields"][0]["field"], "fire_at", "{body}");
    }
    Ok(())
}

/// Cancelling is a CAS on `pending`: it succeeds once, and a one-shot that already fired is a run
/// in flight, not a pending intent.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn cancelling_a_one_shot_is_a_cas_on_pending(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    let (_, created) = post_one_shot(&app, one_shot_body("2099-01-01T00:00:00Z")).await;
    let id = created["id"].as_str().expect("id").to_string();

    let (status, canceled) = delete_one_shot(&app, &id).await;
    assert_eq!(status, StatusCode::OK, "{canceled}");
    assert_eq!(canceled["status"], "canceled");

    let (status, _) = delete_one_shot(&app, &id).await;
    assert_eq!(status, StatusCode::CONFLICT, "a cancel does not repeat");

    let (status, _) = delete_one_shot(&app, "0199c0de-0000-7000-8000-000000000000").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A canceled one-shot never fires, however long the sweep runs.
    let fired = crate::launches::one_shots::fire_due(&db, "2099-06-01T00:00:00Z", 8).await?;
    assert!(fired.is_empty(), "{fired:?}");
    Ok(())
}

/// The one-shot opt-in has to name the schedule it may move, and that schedule has to run the same
/// pack. Opting in without naming one advances nothing, which is the default an ad-hoc experiment
/// wants.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_one_shot_advances_dedupe_only_for_the_schedule_it_names(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    let mut body = schedule_body("0 6 * * *", "UTC");
    body["cursor"] = serde_json::json!({"from": "$.scan.newest_created_at", "param": "since"});
    let (status, scheduled) = send_json(&app, HttpRequest::post("/api/schedules"), body).await;
    assert_eq!(status, StatusCode::CREATED, "{scheduled}");
    let schedule_id = scheduled["id"].as_str().expect("id").to_string();

    let one_shot = |advance: bool, target: Option<&str>| {
        let mut body = serde_json::json!({
            "playbook": "survey",
            "params": {"topic": "attention sinks", "depth": "deep"},
            "max_cost": 3.5,
            "max_time": "30m",
            "fire_at": "2099-01-01T00:00:00Z",
            "advance_dedupe": advance,
        });
        if let Some(target) = target {
            body["dedupe_schedule"] = serde_json::json!(target);
        }
        body
    };

    let (status, plain) = post_one_shot(&app, one_shot(true, None)).await;
    assert_eq!(status, StatusCode::CREATED, "{plain}");
    assert_eq!(
        plain["dedupe_schedule"],
        serde_json::Value::Null,
        "opted in, pointed at nothing"
    );

    let (status, refused) = post_one_shot(&app, one_shot(true, Some("no-such-schedule"))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(refused["fields"][0]["field"], "dedupe_schedule");

    let (status, opted) = post_one_shot(&app, one_shot(true, Some(&schedule_id))).await;
    assert_eq!(status, StatusCode::CREATED, "{opted}");
    assert_eq!(opted["dedupe_schedule"], schedule_id.as_str());

    // The firing carries the target onto the launch row, which is the only thing completion reads.
    let fired = crate::launches::one_shots::fire_due(&db, "2099-01-01T00:00:00Z", 8).await?;
    assert_eq!(fired.len(), 2, "{fired:?}");
    let targets: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT key, dedupe_schedule FROM playbook_launches ORDER BY dedupe_schedule NULLS FIRST",
    )
    .fetch_all(db.pool())
    .await?;
    assert_eq!(targets[0].1, None);
    assert_eq!(targets[1].1.as_deref(), Some(schedule_id.as_str()));
    Ok(())
}

/// A schedule of another pack is refused: advancing its cursor from this pack's result would
/// poison a recurrence nobody asked to touch.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_launch_cannot_advance_another_playbooks_cursor(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;
    sqlx::query(
        r#"INSERT INTO playbooks (id, description, repo, git_ref, rev, path, tar_gz, tar_digest,
                                  tar_bytes, params_schema, schema_digest, core_rev, created_by,
                                  created_at, updated_at)
           VALUES ('triage', 'sorts issues', 'owner/packs', 'main', 'abc123', '', $1,
                   'sha256:tar', 3, '{"type":"object"}'::jsonb, 'sha256:other', 'core1',
                   'wren', '2026-08-23T00:00:00Z', '2026-08-23T00:00:00Z')"#,
    )
    .bind(vec![1u8, 2, 3])
    .execute(db.pool())
    .await?;
    let max_time = crate::model::MaxTime::parse("30m").expect("duration");
    let spec = crate::launches::schedules::cron::CronSpec::parse("0 6 * * *", "UTC").expect("expr");
    let other = crate::launches::schedules::ScheduleStore::new(db.clone())
        .create(
            &crate::launches::schedules::NewSchedule {
                standing: crate::launches::standing::NewStanding {
                    playbook: "triage",
                    target_kind: "adopted",
                    eligible_draft_version: None,
                    params: &serde_json::json!({}),
                    schema_digest: "sha256:other",
                    max_cost: 1.0,
                    max_time: &max_time,
                    advance_dedupe: true,
                    enabled: true,
                    created_by: Some("wren"),
                    owner_principal: None,
                    owner_groups: None,
                    dispatch_target: None,
                    agent_provider: None,
                    agent_model: None,
                },
                cursor: None,
                spec: &spec,
            },
            jiff::Timestamp::now(),
        )
        .await?;

    let (status, refused) = send_json(
        &app,
        HttpRequest::post("/api/playbooks/survey/launch"),
        serde_json::json!({
            "params": {"topic": "attention sinks", "depth": "deep"},
            "max_cost": 3.5,
            "max_time": "30m",
            "advance_dedupe": true,
            "dedupe_schedule": other.id,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(refused["fields"][0]["field"], "dedupe_schedule");
    let launched: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM playbook_launches")
        .fetch_one(db.pool())
        .await?;
    assert_eq!(launched, 0);
    Ok(())
}

/// A one-shot is a launch: deferring one on a playbook the caller may not launch is refused, and
/// somebody else's one-shot is not theirs to cancel.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn one_shot_writes_are_decided_against_the_playbook_and_the_one_shot(
    pool: PgPool,
) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;
    let body = one_shot_body("2099-01-01T00:00:00Z");
    let (status, refused) = send_as(&app, "POST", "/api/one-shots", "mallory", body.clone()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{refused}");
    let (status, created) = send_as(&app, "POST", "/api/one-shots", "wren", body).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let uri = format!("/api/one-shots/{}", created["id"].as_str().expect("id"));
    let (status, listed) = get_json_as(&app, "/api/one-shots", "mallory").await;
    assert_eq!(status, StatusCode::OK);
    assert!(listed.is_empty(), "somebody else's one-shot is not listed");
    let (status, listed) = get_json_as(&app, "/api/one-shots", "wren").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed[0]["id"], created["id"]);
    assert!(
        listed[0]["actions"]
            .as_array()
            .expect("actions")
            .contains(&serde_json::json!("delete")),
        "{listed:?}"
    );
    let (status, refused) = send_as(&app, "DELETE", &uri, "mallory", serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{refused}");
    let (status, body) = send_as(&app, "DELETE", &uri, "wren", serde_json::Value::Null).await;
    assert!(status.is_success(), "{status} {body}");
    Ok(())
}

/// End to end through the surface a person uses: defer a launch, let the sweep claim it, and read
/// the resulting run back with the values the one-shot froze.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_fired_one_shot_becomes_a_deferred_run_with_the_frozen_snapshot(
    pool: PgPool,
) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    let (_, created) = post_one_shot(
        &app,
        serde_json::json!({
            "playbook": "survey",
            "params": {"topic": "attention sinks", "depth": "deep"},
            "max_cost": 3.5,
            "max_time": "30m",
            "fire_at": "2099-01-01T00:00:00Z",
            "advance_dedupe": true,
        }),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_string();

    let fired = crate::launches::one_shots::fire_due(&db, "2099-01-01T00:00:00Z", 8).await?;
    assert_eq!(fired.len(), 1, "{fired:?}");

    let (status, rows) = get_json_value(&app, "/api/playbook-runs").await;
    assert_eq!(status, StatusCode::OK);
    let rows = rows.as_array().expect("array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["key"], fired[0].as_str());
    assert_eq!(rows[0]["origin"], "deferred");
    assert_eq!(rows[0]["advance_dedupe"], true);
    assert_eq!(rows[0]["max_cost"], 3.5);
    assert_eq!(
        rows[0]["params"], created["params"],
        "the firing launches the frozen values, not today's form"
    );

    let (_, one_shots) = get_json_value(&app, "/api/one-shots").await;
    let row = &one_shots.as_array().expect("array")[0];
    assert_eq!(row["id"], id.as_str());
    assert_eq!(row["status"], "fired");
    assert_eq!(row["fired_key"], fired[0].as_str());

    // A fired one-shot is terminal: cancelling it is a conflict, not an undo.
    let (status, _) = delete_one_shot(&app, &id).await;
    assert_eq!(status, StatusCode::CONFLICT);
    Ok(())
}

// --- schedules ----------------------------------------------------------------

async fn send_json(
    app: &Router,
    req: axum::http::request::Builder,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(
            req.header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", "wren")
                .body(Body::from(body.to_string()))
                .expect("req"),
        )
        .await
        .expect("resp");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

fn schedule_body(cron_expr: &str, tz: &str) -> serde_json::Value {
    serde_json::json!({
        "playbook": "survey",
        "params": {"topic": "attention sinks", "depth": "deep"},
        "max_cost": 3.5,
        "max_time": "30m",
        "cron_expr": cron_expr,
        "tz": tz,
    })
}

/// The schedule surface round-trips: create, read back, edit, delete. A created schedule carries
/// the values it will launch and a due window in the future.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_schedule_round_trips_through_its_crud_surface(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    let digest = register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    let (status, created) = send_json(
        &app,
        HttpRequest::post("/api/schedules"),
        schedule_body("30 6 * * MON-FRI", "America/New_York"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("id").to_string();
    assert_eq!(created["playbook"], "survey");
    assert_eq!(created["cron_expr"], "30 6 * * MON-FRI");
    assert_eq!(created["tz"], "America/New_York");
    assert_eq!(created["schema_digest"], digest.as_str());
    assert_eq!(created["enabled"], true);
    assert_eq!(
        created["advance_dedupe"], true,
        "a schedule is the surface dedupe state belongs to"
    );
    assert_eq!(created["consecutive_failures"], 0);
    assert_eq!(created["params"]["topic"], "attention sinks");
    let due = created["next_due_at"].as_str().expect("next_due_at");
    assert!(
        due.parse::<jiff::Timestamp>()? > jiff::Timestamp::now(),
        "{due} is ahead of now"
    );

    let (status, listed) = get_json_value(&app, "/api/schedules").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.as_array().expect("array").len(), 1);
    let (status, fetched) = get_json_value(&app, &format!("/api/schedules/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["id"], id.as_str());

    let mut edit = schedule_body("0 0 * * *", "UTC");
    edit["enabled"] = serde_json::Value::Bool(false);
    let (status, updated) =
        send_json(&app, HttpRequest::put(format!("/api/schedules/{id}")), edit).await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["cron_expr"], "0 0 * * *");
    assert_eq!(updated["tz"], "UTC");
    assert_eq!(updated["enabled"], false);
    assert_eq!(
        updated["next_due_at"],
        serde_json::Value::Null,
        "a disabled schedule has no due window"
    );

    let res = app
        .clone()
        .oneshot(
            HttpRequest::delete(format!("/api/schedules/{id}"))
                .header("x-auth-request-user", "wren")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let (status, _) = get_json_value(&app, &format!("/api/schedules/{id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

/// A schedule is validated exactly like an immediate launch, plus its recurrence: bad values, bad
/// ceilings, a bad expression, and an unknown zone all come back addressed to their form field.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_schedule_is_refused_field_by_field(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    for (body, field) in [
        (schedule_body("every morning", "UTC"), "cron_expr"),
        (schedule_body("0 6 * * *", "Mars/Olympus"), "tz"),
    ] {
        let (status, refused) = send_json(&app, HttpRequest::post("/api/schedules"), body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
        assert_eq!(refused["fields"][0]["field"], field, "{refused}");
    }

    let mut bad_params = schedule_body("0 6 * * *", "UTC");
    bad_params["params"] = serde_json::json!({"topic": "ATTENTION"});
    let (status, refused) = send_json(&app, HttpRequest::post("/api/schedules"), bad_params).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    let fields: Vec<&str> = refused["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .map(|f| f["field"].as_str().unwrap_or_default())
        .collect();
    assert!(
        fields.contains(&"topic") && fields.contains(&"depth"),
        "{refused}"
    );

    let mut over_cap = schedule_body("0 6 * * *", "UTC");
    over_cap["max_time"] = serde_json::json!("40h");
    let (status, refused) = send_json(&app, HttpRequest::post("/api/schedules"), over_cap).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(refused["fields"][0]["field"], "max_time", "{refused}");

    let mut unknown = schedule_body("0 6 * * *", "UTC");
    unknown["playbook"] = serde_json::json!("nope");
    let (status, _) = send_json(&app, HttpRequest::post("/api/schedules"), unknown).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM playbook_standing_launches")
        .fetch_one(db.pool())
        .await?;
    assert_eq!(stored, 0, "a refused schedule stores nothing");
    Ok(())
}

/// The cursor is schedule config: a launcher writes the mapping, and only a completed run writes
/// the value. A PATCH-shaped attempt to set the value is ignored because the body has no such
/// field, and an edit that repoints the cursor drops the value the old one collected.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_schedules_cursor_is_writable_config_and_a_read_only_value(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    let mut body = schedule_body("0 6 * * *", "UTC");
    body["cursor"] = serde_json::json!({"from": "$.scan.newest_created_at", "param": "since"});
    body["cursor_value"] = serde_json::json!("2026-01-01T00:00:00Z");
    let (status, created) = send_json(&app, HttpRequest::post("/api/schedules"), body).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("id").to_string();
    assert_eq!(created["cursor"]["from"], "$.scan.newest_created_at");
    assert_eq!(created["cursor"]["param"], "since");
    assert_eq!(
        created["cursor_value"],
        serde_json::Value::Null,
        "only a completed run writes the value"
    );

    sqlx::query("UPDATE playbook_schedules SET cursor_value = $2 WHERE id = $1")
        .bind(&id)
        .bind("2026-08-23T00:00:00Z")
        .execute(db.pool())
        .await?;
    let (_, fetched) = get_json_value(&app, &format!("/api/schedules/{id}")).await;
    assert_eq!(fetched["cursor_value"], "2026-08-23T00:00:00Z");

    let mut repoint = schedule_body("0 6 * * *", "UTC");
    repoint["cursor"] = serde_json::json!({"from": "$.scan.oldest", "param": "since"});
    let (status, updated) = send_json(
        &app,
        HttpRequest::put(format!("/api/schedules/{id}")),
        repoint,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["cursor"]["from"], "$.scan.oldest");
    assert_eq!(
        updated["cursor_value"],
        serde_json::Value::Null,
        "a different field is a different cursor"
    );

    let mut dropped = schedule_body("0 6 * * *", "UTC");
    dropped["cursor"] = serde_json::Value::Null;
    let (status, updated) = send_json(
        &app,
        HttpRequest::put(format!("/api/schedules/{id}")),
        dropped,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["cursor"], serde_json::Value::Null);
    Ok(())
}

/// A run-file cursor is written and read back as `from` plus `path`, and its stored body is
/// described on the wire by size and digest rather than shipped with every listing.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_file_cursor_round_trips_and_describes_its_body(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    let mut body = schedule_body("0 6 * * *", "UTC");
    body["cursor"] = serde_json::json!({"from": "rollup/STATE.json", "path": "state/cursor.json"});
    let (status, created) = send_json(&app, HttpRequest::post("/api/schedules"), body).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("id").to_string();
    assert_eq!(created["cursor"]["from"], "rollup/STATE.json");
    assert_eq!(created["cursor"]["path"], "state/cursor.json");
    assert_eq!(created["cursor"]["param"], serde_json::Value::Null);
    assert_eq!(created["cursor_value"], serde_json::Value::Null);
    assert_eq!(created["cursor_file"], serde_json::Value::Null);

    let stored = "{\"seen\": [1, 2, 3]}";
    sqlx::query("UPDATE playbook_schedules SET cursor_value = $2 WHERE id = $1")
        .bind(&id)
        .bind(stored)
        .execute(db.pool())
        .await?;
    let (_, fetched) = get_json_value(&app, &format!("/api/schedules/{id}")).await;
    assert_eq!(
        fetched["cursor_value"],
        serde_json::Value::Null,
        "a file body is not a wire value"
    );
    assert_eq!(fetched["cursor_file"]["bytes"], stored.len());
    let digest = fetched["cursor_file"]["digest"].as_str().expect("digest");
    assert!(
        digest.starts_with("sha256:") && digest.len() == 71,
        "{digest}"
    );
    let (_, listed) = get_json_value(&app, "/api/schedules").await;
    assert_eq!(listed[0]["cursor_file"]["digest"], digest);
    Ok(())
}

/// A cursor path outside the closed grammar is refused against the form field that wrote it,
/// rather than stored as a mapping that would never resolve.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_malformed_cursor_is_refused_by_field(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    for (cursor, field) in [
        (
            serde_json::json!({"from": "newest", "param": "since"}),
            "cursor.from",
        ),
        (
            serde_json::json!({"from": "$.scan.newest", "param": ""}),
            "cursor.param",
        ),
        (
            serde_json::json!({"from": "$.scan.newest", "param": "since", "path": "state.json"}),
            "cursor.path",
        ),
        (
            serde_json::json!({"from": "rollup/STATE.json"}),
            "cursor.path",
        ),
        (
            serde_json::json!({"from": "rollup/STATE.json", "param": "since", "path": "state.json"}),
            "cursor.param",
        ),
        (
            serde_json::json!({"from": "rollup/STATE.json", "path": "../state.json"}),
            "cursor.path",
        ),
        (
            serde_json::json!({"from": "STATE.json", "path": "state.json"}),
            "cursor.from",
        ),
    ] {
        let mut body = schedule_body("0 6 * * *", "UTC");
        body["cursor"] = cursor;
        let (status, refused) = send_json(&app, HttpRequest::post("/api/schedules"), body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
        assert_eq!(refused["fields"][0]["field"], field, "{refused}");
    }
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM playbook_standing_launches")
        .fetch_one(db.pool())
        .await?;
    assert_eq!(stored, 0);
    Ok(())
}

/// The preview is computed server-side, so the firings a launcher reads are the instants the sweep
/// will fire at — one interpretation of the zone, not two.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn the_preview_serves_the_next_firings_the_sweep_would_use(pool: PgPool) -> Result<()> {
    let (db, _dir) = db_with(pool);
    let app = app_with_admins(db, vec!["wren".to_string()]);

    let (status, preview) = send_json(
        &app,
        HttpRequest::post("/api/schedules/preview"),
        serde_json::json!({"cron_expr": "0 * * * *", "tz": "UTC", "count": 3}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let firings: Vec<String> = preview["firings"]
        .as_array()
        .expect("firings")
        .iter()
        .map(|f| f.as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(firings.len(), 3, "{preview}");
    let spec = crate::launches::schedules::cron::CronSpec::parse("0 * * * *", "UTC").expect("spec");
    let mut previous = jiff::Timestamp::now();
    for firing in &firings {
        assert!(firing.ends_with('Z'), "{firing}");
        let at: jiff::Timestamp = firing.parse()?;
        assert!(at > previous, "{firings:?} is ordered");
        previous = at;
        assert_eq!(
            spec.next_after(at - jiff::SignedDuration::from_secs(1)),
            Some(at),
            "the preview and the sweep read the same expression"
        );
    }

    let (status, defaulted) = send_json(
        &app,
        HttpRequest::post("/api/schedules/preview"),
        serde_json::json!({"cron_expr": "0 6 * * *"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(defaulted["tz"], "UTC", "absent zone reads as UTC");
    assert_eq!(defaulted["firings"].as_array().expect("firings").len(), 5);

    let (status, refused) = send_json(
        &app,
        HttpRequest::post("/api/schedules/preview"),
        serde_json::json!({"cron_expr": "not a cron", "tz": "UTC"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(refused["fields"][0]["field"], "cron_expr", "{refused}");
    Ok(())
}

/// A schedule is a standing launch: creating one on a playbook the caller may not launch is
/// refused, and somebody else's schedule is not theirs to change or delete.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn schedule_writes_are_decided_against_the_playbook_and_the_schedule(
    pool: PgPool,
) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;
    let body = schedule_body("0 6 * * *", "UTC");
    let (status, refused) = send_as(&app, "POST", "/api/schedules", "mallory", body.clone()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{refused}");
    let (status, created) = send_as(&app, "POST", "/api/schedules", "wren", body.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let uri = format!("/api/schedules/{}", created["id"].as_str().expect("id"));
    let (status, refused) = send_as(&app, "PUT", &uri, "mallory", body).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{refused}");
    let (status, refused) = send_as(&app, "DELETE", &uri, "mallory", serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{refused}");
    let (status, body) = send_as(&app, "DELETE", &uri, "wren", serde_json::Value::Null).await;
    assert!(status.is_success(), "{status} {body}");
    Ok(())
}

/// End to end through the surface a person uses: schedule a playbook, let the sweep claim its due
/// window, and read the firing back as an ordinary run of the schedule's values.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_fired_schedule_becomes_an_ordinary_launch(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    let (status, created) = send_json(
        &app,
        HttpRequest::post("/api/schedules"),
        schedule_body("0 * * * *", "UTC"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("id").to_string();
    let due: jiff::Timestamp = created["next_due_at"].as_str().expect("due").parse()?;

    let fired =
        crate::launches::schedules::fire_due(&db, due, 8, 5, std::time::Duration::from_secs(3600))
            .await?;
    assert_eq!(fired.len(), 1, "{fired:?}");

    let (status, run) = get_json_value(&app, &format!("/api/playbook-runs/{}", fired[0])).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["launch"]["origin"], "schedule");
    assert_eq!(run["launch"]["schedule"], id.as_str());
    assert_eq!(run["launch"]["params"], created["params"]);
    assert_eq!(run["launch"]["max_cost"], 3.5);
    assert_eq!(run["launch"]["status"], "new");

    let (_, listed) = get_json_value(&app, "/api/schedules").await;
    let row = &listed.as_array().expect("array")[0];
    assert_eq!(row["id"], id.as_str());
    assert_eq!(row["last_fired_at"], crate::clock::stamp(due));
    assert!(
        row["next_due_at"].as_str().expect("next") > row["last_fired_at"].as_str().expect("last"),
        "{row}"
    );
    Ok(())
}

// --- draft packs (the authoring studio) ---------------------------------------

async fn send_as(
    app: &Router,
    method: &str,
    uri: &str,
    user: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let (status, bytes) = send(app, method, uri, user, Some(body)).await;
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn post_admin(
    app: &Router,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    post_json(app, uri, body).await
}

fn draft_files() -> serde_json::Value {
    serde_json::json!({
        "crucible.toml": "[repo]\npath = \".\"\n\n[agent]\n\n[workflow]\ntype = \"playbook\"\nfile = \"workflow.star\"\n",
        "workflow.star": "params = {}\nhello = command(name = \"hello\", run = \"echo hello\")\nworkflow(type = \"playbook\", tasks = [hello], result = hello)\n",
    })
}

/// A draft is not a registered pack: it lists on its own rail, with its version history, and never
/// shows up where the registry says what can be launched from a pin.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn drafts_list_separately_from_the_registry_with_their_versions(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db, vec!["wren".to_string()]);
    register_survey(&app, &dir.path().join("registry"), LAUNCH_WORKFLOW).await;

    let created = {
        let (status, body) = post_admin(
            &app,
            "/api/playbook-drafts",
            serde_json::json!({"id": "studio", "description": "a drafted pack"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        post_admin(
            &app,
            "/api/playbook-drafts/studio/versions",
            serde_json::json!({"files": draft_files()}),
        )
        .await
    };
    assert_eq!(created.0, StatusCode::OK, "{}", created.1);
    assert_eq!(created.1["version"], 2);

    let (status, registry) = get_json(&app, "/api/playbooks").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        registry.iter().map(|p| p["id"].clone()).collect::<Vec<_>>(),
        vec![serde_json::json!("survey")],
        "the registry never lists a draft"
    );

    let (status, drafts) = get_json(&app, "/api/playbook-drafts").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0]["id"], "studio");
    assert_eq!(drafts[0]["latest_version"], 2);
    assert_eq!(drafts[0]["compiles"], serde_json::json!(true));
    assert_eq!(drafts[0]["retired_at"], serde_json::Value::Null);

    let (status, detail) = get_json_object(&app, "/api/playbook-drafts/studio").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        detail["versions"]
            .as_array()
            .expect("versions")
            .iter()
            .map(|v| v["version"].clone())
            .collect::<Vec<_>>(),
        vec![serde_json::json!(2), serde_json::json!(1)],
        "newest save first"
    );

    let (status, files) = get_json_object(&app, "/api/playbook-drafts/studio/files").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(files["version"], 2);
    assert_eq!(
        files["files"]["workflow.star"],
        draft_files()["workflow.star"]
    );
    Ok(())
}

/// Compile-on-save: the version stores either way, and a refused compile answers 200 carrying the
/// anchor the editor pins the message to.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn saving_a_draft_returns_the_form_the_graph_or_the_anchored_diagnostic(
    pool: PgPool,
) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db, vec!["wren".to_string()]);

    let good = {
        post_admin(
            &app,
            "/api/playbook-drafts",
            serde_json::json!({"id": "studio", "description": "a drafted pack"}),
        )
        .await
    };
    assert_eq!(good.0, StatusCode::CREATED, "{}", good.1);
    assert!(good.1["schema_digest"].is_string(), "{}", good.1);
    assert_eq!(good.1["graph"]["result"], "hello");
    assert_eq!(good.1["graph"]["nodes"][0]["name"], "hello");
    assert_eq!(good.1["diagnostics"], serde_json::json!([]));

    let mut files = draft_files();
    files["workflow.star"] = serde_json::json!(crate::testing::fixtures::WORKFLOW_BROKEN);
    let bad = {
        post_admin(
            &app,
            "/api/playbook-drafts/studio/versions",
            serde_json::json!({"files": files}),
        )
        .await
    };
    assert_eq!(
        bad.0,
        StatusCode::OK,
        "diagnostics are the payload: {}",
        bad.1
    );
    assert_eq!(bad.1["version"], 2);
    assert_eq!(bad.1["graph"], serde_json::Value::Null);
    assert_eq!(bad.1["diagnostics"][0]["file"], "workflow.star");
    assert_eq!(bad.1["diagnostics"][0]["line"], 3);
    assert_eq!(bad.1["diagnostics"][0]["col"], 39);

    // A path that leaves the pack is a structural refusal, not a diagnostic.
    let escaped = {
        post_admin(
            &app,
            "/api/playbook-drafts/studio/versions",
            serde_json::json!({"files": {"../escape.star": "x"}}),
        )
        .await
    };
    assert_eq!(escaped.0, StatusCode::UNPROCESSABLE_ENTITY, "{}", escaped.1);
    Ok(())
}

/// The shared editing surface: a save names the version it edited from, the save that lands first
/// wins, and the refusal carries who overtook it so the loser re-reads that version and merges.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_stale_base_is_refused_with_the_version_that_overtook_it(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db, vec!["wren".to_string(), "author".to_string()]);

    let created = {
        post_admin(
            &app,
            "/api/playbook-drafts",
            serde_json::json!({"id": "studio", "description": "a drafted pack"}),
        )
        .await
    };
    assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);

    // What the editor loads is the base it saves against: the version, its saver, its diagnostics.
    let (status, loaded) = get_json_object(&app, "/api/playbook-drafts/studio/files").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(loaded["version"], 1);
    assert_eq!(loaded["saved_by"], "wren");
    assert_eq!(loaded["diagnostics"], serde_json::json!([]));
    assert!(loaded["saved_at"].is_string(), "{loaded}");

    let (agent, stale) = {
        let agent = post_json_as(
            &app,
            "/api/playbook-drafts/studio/versions",
            "author",
            serde_json::json!({"files": draft_files(), "base_version": 1}),
        )
        .await;
        let stale = post_json_as(
            &app,
            "/api/playbook-drafts/studio/versions",
            "wren",
            serde_json::json!({"files": draft_files(), "base_version": 1}),
        )
        .await;
        (agent, stale)
    };
    assert_eq!(agent.0, StatusCode::OK, "{}", agent.1);
    assert_eq!(agent.1["version"], 2);
    assert_eq!(agent.1["saved_by"], "author");

    assert_eq!(stale.0, StatusCode::CONFLICT, "{}", stale.1);
    assert_eq!(stale.1["base_version"], 1);
    assert_eq!(stale.1["current_version"], 2);
    assert_eq!(stale.1["saved_by"], "author");
    assert_eq!(stale.1["saved_at"], agent.1["saved_at"]);
    assert!(
        stale.1["error"]
            .as_str()
            .is_some_and(|e| e.contains("merge")),
        "{}",
        stale.1
    );

    // The refused save changed nothing: the agent's version is still what a reader picks up.
    let (_, head) = get_json_object(&app, "/api/playbook-drafts/studio/files").await;
    assert_eq!(head["version"], 2);
    assert_eq!(head["saved_by"], "author");

    let merged = {
        post_json_as(
            &app,
            "/api/playbook-drafts/studio/versions",
            "wren",
            serde_json::json!({"files": draft_files(), "base_version": 2}),
        )
        .await
    };
    assert_eq!(merged.0, StatusCode::OK, "{}", merged.1);
    assert_eq!(merged.1["version"], 3);

    let (status, detail) = get_json_object(&app, "/api/playbook-drafts/studio").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        detail["versions"]
            .as_array()
            .expect("versions")
            .iter()
            .map(|v| (v["version"].clone(), v["created_by"].clone()))
            .collect::<Vec<_>>(),
        vec![
            (serde_json::json!(3), serde_json::json!("wren")),
            (serde_json::json!(2), serde_json::json!("author")),
            (serde_json::json!(1), serde_json::json!("wren")),
        ],
        "the history interleaves the human's saves with the agent's"
    );
    Ok(())
}

/// New-from-template: version 1 is the registered pack's files, and an unknown template is a 404.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_draft_seeds_from_a_registered_pack(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db, vec!["wren".to_string()]);
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    let (created, missing) = {
        let created = post_admin(
            &app,
            "/api/playbook-drafts",
            serde_json::json!({"id": "studio", "description": "forked", "template": "survey", "template_rev": "deadbeef"}),
        )
        .await;
        let missing = post_admin(
            &app,
            "/api/playbook-drafts",
            serde_json::json!({"id": "other", "description": "forked", "template": "nope"}),
        )
        .await;
        (created, missing)
    };
    assert_eq!(created.0, StatusCode::CONFLICT, "{}", created.1);
    assert_eq!(missing.0, StatusCode::NOT_FOUND, "{}", missing.1);

    let (_, inspected) = get_json_object(&app, "/api/playbooks/survey").await;
    let created = post_admin(
        &app,
        "/api/playbook-drafts",
        serde_json::json!({"id": "studio", "description": "forked", "template": "survey", "template_rev": inspected["rev"]}),
    )
    .await;
    assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);

    let (status, files) = get_json_object(&app, "/api/playbook-drafts/studio/files").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        files["files"]
            .as_object()
            .expect("files")
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["crucible.toml".to_string(), "workflow.star".to_string()],
        "the registered pack's own files"
    );
    assert_eq!(files["files"]["workflow.star"], LAUNCH_WORKFLOW);

    let (status, draft) = get_json_object(&app, "/api/playbook-drafts/studio").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(draft["origin"]["kind"], "playbook");
    assert_eq!(draft["origin"]["playbook"], "survey");
    assert_eq!(
        draft["origin"]["rev"], draft["origin"]["current_rev"],
        "the template was copied at the rev the registry serves"
    );
    assert_eq!(draft["origin"]["moved"], false);
    assert!(
        draft["origin"]["repo"].is_string(),
        "graduation pre-fills its target from here: {draft}"
    );
    Ok(())
}

/// A draft launches like a registered pack: the same ceilings, the same field-level refusals, and
/// a launch row carrying the draft version and its own copy of that version's pack.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_draft_launches_with_ceilings_and_freezes_its_version(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_playbook_caps(
        db.clone(),
        vec!["wren".to_string()],
        crate::config::PlaybookCaps {
            max_cost: 10.0,
            max_time: crate::model::MaxTime::parse("2h").expect("cap"),
        },
    );
    let created = post_admin(
        &app,
        "/api/playbook-drafts",
        serde_json::json!({"id": "studio", "description": "a drafted pack"}),
    )
    .await;
    assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);
    // The skeleton declares no params; the form the launcher fills is the saved workflow's.
    let mut files = draft_files();
    files["workflow.star"] = serde_json::json!(LAUNCH_WORKFLOW);
    let saved = post_admin(
        &app,
        "/api/playbook-drafts/studio/versions",
        serde_json::json!({"files": files}),
    )
    .await;
    assert_eq!(saved.0, StatusCode::OK, "{}", saved.1);
    assert_eq!(saved.1["version"], 2);
    let digest = saved.1["schema_digest"]
        .as_str()
        .expect("digest")
        .to_string();

    // The launcher's ceilings are bounded by the same admin caps the registered path is held to.
    for (max_cost, max_time, field) in [(99.0, "30m", "max_cost"), (1.0, "9h", "max_time")] {
        let (status, body) = post_admin(
            &app,
            "/api/playbook-drafts/studio/launch",
            serde_json::json!({
                "params": {"topic": "attention", "depth": "deep"},
                "max_cost": max_cost,
                "max_time": max_time,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["fields"][0]["field"], field, "{body}");
    }

    // Values are checked against the draft's own stored schema.
    let (status, body) = post_admin(
        &app,
        "/api/playbook-drafts/studio/launch",
        serde_json::json!({"params": {"topic": "ATTENTION"}, "max_cost": 1.0, "max_time": "30m"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let (status, ack) = post_admin(
        &app,
        "/api/playbook-drafts/studio/launch",
        serde_json::json!({
            "params": {"topic": "attention", "depth": "deep"},
            "max_cost": 2.5,
            "max_time": "30m",
            "schema_digest": digest,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{ack}");
    let key = ack["key"].as_str().expect("key").to_string();
    assert!(key.starts_with("playbook:studio:"), "{key}");

    let issue = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .expect("issue");
    assert_eq!(issue.status, crate::model::Status::New);
    assert_eq!(issue.kind.tag(), "playbook");

    let launch = crate::launches::store::get_playbook_launch(db.pool(), &key)
        .await?
        .expect("launch row");
    assert_eq!(launch.playbook, "studio");
    assert_eq!(launch.max_time.as_str(), "30m");
    let row = sqlx::query!(
        "SELECT origin, draft_version FROM playbook_launches WHERE key = $1",
        key
    )
    .fetch_one(db.pool())
    .await?;
    assert_eq!(row.origin, "draft");
    assert_eq!(row.draft_version, Some(2));

    // The launch owns the draft version's bytes, so a later save cannot change what it runs.
    let slug = crate::model::sanitize_key(&key);
    let copied = sqlx::query!(
        "SELECT digest FROM pack_tarballs WHERE issue_slug = $1",
        slug
    )
    .fetch_one(db.pool())
    .await?;
    let stored = sqlx::query!(
        "SELECT tar_digest FROM playbook_draft_versions WHERE draft_id = 'studio' AND version = 2"
    )
    .fetch_one(db.pool())
    .await?;
    assert_eq!(copied.digest, stored.tar_digest);

    // A save that lands after the form was rendered is a 409, not a launch of bytes nobody saw.
    let (status, _) = post_admin(
        &app,
        "/api/playbook-drafts/studio/versions",
        serde_json::json!({"files": draft_files()}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    sqlx::query!(
        "UPDATE playbook_draft_versions SET schema_digest = 'sha256:moved' WHERE version = 3"
    )
    .execute(db.pool())
    .await?;
    let (status, body) = post_admin(
        &app,
        "/api/playbook-drafts/studio/launch",
        serde_json::json!({
            "params": {"topic": "attention", "depth": "deep"},
            "max_cost": 2.5,
            "max_time": "30m",
            "schema_digest": digest,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // A newest save that never compiled has no form to launch with.
    sqlx::query!("UPDATE playbook_draft_versions SET schema_digest = NULL, params_schema = NULL WHERE version = 3")
        .execute(db.pool())
        .await?;
    let (status, body) = post_admin(
        &app,
        "/api/playbook-drafts/studio/launch",
        serde_json::json!({"params": {}, "max_cost": 2.5, "max_time": "30m"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    Ok(())
}

/// Put a recording `git` and `gh` on PATH for the duration of a body: graduation's push and PR
/// open are the two shells, and this is what proves which refs it pushed. The `git` stub delegates
/// everything but `push` to the real binary, so a test running beside this one still gets git.
async fn with_forge_stubs<F, Fut, T>(dir: &std::path::Path, f: F) -> (T, String)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let bin = dir.join("forge-bin");
    std::fs::create_dir_all(&bin).expect("mkdir");
    let log = bin.join("calls.log");
    let write = |name: &str, body: String| {
        let path = bin.join(name);
        crate::testing::write_exec(&path, &body);
    };
    let log_path = log.to_string_lossy().to_string();
    let real_git = String::from_utf8(
        std::process::Command::new("/usr/bin/env")
            .args(["which", "git"])
            .output()
            .expect("which git")
            .stdout,
    )
    .expect("utf8")
    .trim()
    .to_string();
    write(
        "git",
        format!(
            "#!/bin/sh\ncase \"$*\" in\n  *push*) echo \"git $*\" >> '{log_path}'; exit 0 ;;\nesac\nexec '{real_git}' \"$@\"\n"
        ),
    );
    write(
        "gh",
        format!(
            "#!/bin/sh\necho \"gh $*\" >> '{log_path}'\ncase \"$2\" in\n  create) echo https://github.com/owner/packs/pull/7 ;;\nesac\nexit 0\n"
        ),
    );

    let _g = crate::ENV_LOCK.lock().await;
    let original = std::env::var("PATH").unwrap_or_default();
    unsafe {
        std::env::set_var("PATH", format!("{}:{original}", bin.display()));
    }
    let out = f().await;
    unsafe {
        std::env::set_var("PATH", &original);
    }
    (out, std::fs::read_to_string(&log).unwrap_or_default())
}

/// Graduation exports the draft as a PR over the pack-PR push, is idempotent against a second
/// attempt, and the import of the merged pack is what retires the draft.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn graduation_opens_a_pr_and_the_import_retires_the_draft(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    let created = {
        post_admin(
            &app,
            "/api/playbook-drafts",
            serde_json::json!({"id": "studio", "description": "a drafted pack"}),
        )
        .await
    };
    assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);

    let ((first, second), log) = with_forge_stubs(dir.path(), || async {
        let first = post_admin(
            &app,
            "/api/playbook-drafts/studio/graduate",
            serde_json::json!({"repo": "owner/packs", "path": "packs/studio"}),
        )
        .await;
        let second = post_admin(
            &app,
            "/api/playbook-drafts/studio/graduate",
            serde_json::json!({"repo": "owner/packs", "path": "packs/studio"}),
        )
        .await;
        (first, second)
    })
    .await;
    assert_eq!(first.0, StatusCode::OK, "{}", first.1);
    assert_eq!(first.1["pr_url"], "https://github.com/owner/packs/pull/7");
    assert!(
        log.contains("crucible-pack/draft_studio")
            && log.contains("crucible-pack/draft_studio-base"),
        "the branch pair is pushed: {log}"
    );
    assert_eq!(
        second.0,
        StatusCode::CONFLICT,
        "a graduated draft does not open a second PR"
    );
    assert!(
        second.1["error"]
            .as_str()
            .is_some_and(|e| e.contains("pull/7")),
        "the standing PR rides the refusal: {}",
        second.1
    );

    // The other half of the loop: registering the graduated repo/path retires the draft.
    let repo = {
        let repo = playbook_fixture(&dir.path().join("merged"), LAUNCH_WORKFLOW);
        let (status, _) = {
            post_admin(
                &app,
                "/api/playbooks",
                serde_json::json!({
                    "id": "studio-pack",
                    "description": "the merged pack",
                    "repo": repo,
                    "git_ref": "main",
                }),
            )
            .await
        };
        assert_eq!(status, StatusCode::CREATED);
        let registered = crate::playbooks::registry::get(db.pool(), "studio-pack")
            .await?
            .expect("row");
        let crate::playbooks::registry::PlaybookSource::Git { repo, path, .. } = registered.source
        else {
            panic!("a git registration has a git source");
        };
        sqlx::query!(
            "UPDATE playbook_drafts SET graduation_repo = $1, graduation_path = $2 WHERE id = 'studio'",
            repo,
            path,
        )
        .execute(db.pool())
        .await?;
        repo
    };
    assert!(
        crate::playbooks::drafts::get(db.pool(), "studio")
            .await?
            .expect("draft")
            .retired_at
            .is_none(),
        "nothing retires until the graduated pack is imported"
    );

    let repo_for_body = repo.clone();
    let (status, _) = {
        post_admin(
            &app,
            "/api/playbooks",
            serde_json::json!({
                "id": "studio-pack",
                "description": "the merged pack",
                "repo": repo_for_body,
                "git_ref": "main",
            }),
        )
        .await
    };
    assert_eq!(status, StatusCode::CREATED);
    let retired = crate::playbooks::drafts::get(db.pool(), "studio")
        .await?
        .expect("draft");
    assert!(
        retired.retired_at.is_some(),
        "the merged import retires the draft"
    );

    // A retired draft is read-only.
    let (status, _) = {
        post_admin(
            &app,
            "/api/playbook-drafts/studio/versions",
            serde_json::json!({"files": draft_files()}),
        )
        .await
    };
    assert_eq!(status, StatusCode::CONFLICT);
    Ok(())
}

/// A team holding no platform role runs its own draft: a maintainer authors it, a member launches
/// it, only the owner graduates or deletes it, and a caller outside the team is told it does not
/// exist.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_team_with_no_platform_role_authors_and_launches_its_draft(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_playbook_caps(
        db.clone(),
        vec![],
        crate::config::PlaybookCaps {
            max_cost: 10.0,
            max_time: crate::model::MaxTime::parse("2h").expect("cap"),
        },
    );
    let (status, body) = send_as(
        &app,
        "POST",
        "/api/teams",
        "reed",
        serde_json::json!({
            "slug": "mlr",
            "display_name": "MLR",
            "members": [
                {"kind": "user", "member": "reed", "role": "owner"},
                {"kind": "user", "member": "dana", "role": "maintainer"},
                {"kind": "user", "member": "kim", "role": "member"},
            ],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let draft = serde_json::json!({"id": "studio", "description": "d", "owner": "team:mlr"});
    let (status, body) = send_as(
        &app,
        "POST",
        "/api/playbook-drafts",
        "mallory",
        draft.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = send_as(&app, "POST", "/api/playbook-drafts", "kim", draft.clone()).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a member does not create: {body}"
    );
    assert_eq!(body["rule"], "no-rule");
    let (status, body) = send_as(&app, "POST", "/api/playbook-drafts", "dana", draft).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let mut files = draft_files();
    files["workflow.star"] = serde_json::json!(LAUNCH_WORKFLOW);
    let save = serde_json::json!({"files": files});
    let versions = "/api/playbook-drafts/studio/versions";
    let (status, body) = send_as(&app, "POST", versions, "kim", save.clone()).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a member does not save: {body}"
    );
    let (status, body) = send_as(&app, "POST", versions, "mallory", save.clone()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = send_as(&app, "POST", versions, "dana", save).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The controls a page enables come from the same decision the writes are held to.
    for (user, expected) in [
        ("kim", serde_json::json!(["read", "launch"])),
        (
            "dana",
            serde_json::json!(["read", "create", "update", "launch"]),
        ),
        (
            "reed",
            serde_json::json!([
                "read", "create", "update", "delete", "transfer", "share", "launch", "approve"
            ]),
        ),
    ] {
        let (status, detail) = send_as(
            &app,
            "GET",
            "/api/playbook-drafts/studio",
            user,
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{user}");
        assert_eq!(detail["actions"], expected, "{user}");
        let (status, listed) = get_json_as(&app, "/api/playbook-drafts", user).await;
        assert_eq!(status, StatusCode::OK, "{user}");
        assert_eq!(listed[0]["actions"], expected, "{user}");
    }

    let launch = serde_json::json!({
        "params": {"topic": "attention", "depth": "deep"},
        "max_cost": 1.0,
        "max_time": "30m",
    });
    let launch_uri = "/api/playbook-drafts/studio/launch";
    let (status, body) = send_as(&app, "POST", launch_uri, "mallory", launch.clone()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, ack) = send_as(&app, "POST", launch_uri, "kim", launch).await;
    assert_eq!(status, StatusCode::CREATED, "a member launches: {ack}");
    let key = ack["key"].as_str().expect("key");
    let created_by: Option<String> =
        sqlx::query_scalar("SELECT created_by FROM playbook_launches WHERE key = $1")
            .bind(key)
            .fetch_one(db.pool())
            .await?;
    assert_eq!(created_by.as_deref(), Some("kim"));

    let graduate_uri = "/api/playbook-drafts/studio/graduate";
    let target = serde_json::json!({"repo": "owner/packs", "path": "packs/studio"});
    let (refusals, log) = with_forge_stubs(dir.path(), || async {
        let mut refusals = Vec::new();
        for user in ["kim", "dana", "mallory"] {
            refusals.push((
                user,
                send_as(&app, "POST", graduate_uri, user, target.clone()).await,
            ));
        }
        refusals
    })
    .await;
    for (user, (status, body)) in refusals {
        let expected = if user == "mallory" {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::FORBIDDEN
        };
        assert_eq!(status, expected, "{user}: {body}");
        if user != "mallory" {
            assert!(
                body["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("playbook_draft:approve"),
                "{user}: {body}"
            );
        }
    }
    assert!(log.is_empty(), "a refused graduation pushes nothing: {log}");
    let ((status, body), log) = with_forge_stubs(dir.path(), || {
        send_as(&app, "POST", graduate_uri, "reed", target.clone())
    })
    .await;
    assert_eq!(status, StatusCode::OK, "the owner graduates: {body}");
    assert_eq!(body["pr_url"], "https://github.com/owner/packs/pull/7");
    assert!(log.contains("crucible-pack/draft_studio"), "{log}");

    let uri = "/api/playbook-drafts/studio";
    for user in ["kim", "dana"] {
        let (status, body) = send_as(&app, "DELETE", uri, user, serde_json::Value::Null).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{user}: {body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("playbook_draft:delete"),
            "{body}"
        );
    }
    let (status, body) = send_as(&app, "DELETE", uri, "mallory", serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = send_as(&app, "DELETE", uri, "reed", serde_json::Value::Null).await;
    assert!(status.is_success(), "the owner deletes: {status} {body}");
    Ok(())
}

/// Publishing a draft skips review, so owning it is not enough: the default policy refuses the
/// team's owner until a rule names the team, and from then on the draft stays live and each publish
/// re-pins the same playbook, which launches and schedules like any other.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_draft_publishes_straight_to_the_registry_only_under_a_rule_naming_its_owner(
    pool: PgPool,
) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_playbook_caps(
        db.clone(),
        vec!["root".to_string()],
        crate::config::PlaybookCaps {
            max_cost: 10.0,
            max_time: crate::model::MaxTime::parse("2h").expect("cap"),
        },
    );
    let (status, body) = send_as(
        &app,
        "POST",
        "/api/teams",
        "reed",
        serde_json::json!({
            "slug": "mlr",
            "display_name": "MLR",
            "members": [
                {"kind": "user", "member": "reed", "role": "owner"},
                {"kind": "user", "member": "kim", "role": "member"},
            ],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let draft =
        serde_json::json!({"id": "studio", "description": "mlr sweep", "owner": "team:mlr"});
    let (status, body) = send_as(&app, "POST", "/api/playbook-drafts", "reed", draft).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let mut files = draft_files();
    files["workflow.star"] = serde_json::json!(LAUNCH_WORKFLOW);
    let versions = "/api/playbook-drafts/studio/versions";
    let (status, body) = send_as(
        &app,
        "POST",
        versions,
        "reed",
        serde_json::json!({"files": files.clone()}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let publish = "/api/playbook-drafts/studio/publish";
    let into = |playbook: &str| serde_json::json!({"playbook": playbook});
    let (status, body) = send_as(&app, "POST", publish, "reed", into("mlr-pack")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("playbook_draft:publish"),
        "{body}"
    );
    let actions = |user: &'static str| {
        let app = app.clone();
        async move {
            let (status, detail) = send_as(
                &app,
                "GET",
                "/api/playbook-drafts/studio",
                user,
                serde_json::Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{user}: {detail}");
            detail
        }
    };
    let detail = actions("reed").await;
    assert!(
        !detail["actions"]
            .as_array()
            .expect("actions")
            .contains(&serde_json::json!("publish")),
        "{detail}"
    );

    let granted = format!(
        "{}\n@id(\"mlr-owners-publish\") permit(principal, action == Action::\"playbook_draft:publish\", resource) when {{ principal.hasTag(\"team:mlr\") && resource has owner_role && resource.owner_role == \"owner\" }};",
        crate::authz::policy::DEFAULT_POLICY
    );
    let (status, set) = send_as(
        &app,
        "POST",
        "/api/authz/policy-sets",
        "root",
        serde_json::json!({"text": granted}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{set}");
    let activate = format!(
        "/api/authz/policy-sets/{}/activate",
        set["digest"].as_str().expect("digest")
    );
    let (status, body) = send_as(&app, "POST", &activate, "root", serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let detail = actions("reed").await;
    assert!(
        detail["actions"]
            .as_array()
            .expect("actions")
            .contains(&serde_json::json!("publish")),
        "{detail}"
    );

    let (status, body) = send_as(&app, "POST", publish, "kim", into("mlr-pack")).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a member is outside the rule: {body}"
    );
    let (status, body) = send_as(&app, "POST", publish, "mallory", into("mlr-pack")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = send_as(&app, "POST", publish, "reed", serde_json::json!({})).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "no target yet: {body}"
    );
    let (status, body) = send_as(&app, "POST", publish, "reed", into("studio")).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the draft keeps its own id: {body}"
    );

    let (status, first) = send_as(&app, "POST", publish, "reed", into("mlr-pack")).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    assert_eq!(first["id"], "mlr-pack");
    assert_eq!(
        first["rev"], first["tar_digest"],
        "a draft-sourced pack pins its bytes"
    );
    let (status, pack) = send_as(
        &app,
        "GET",
        "/api/playbooks/mlr-pack",
        "kim",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{pack}");
    assert_eq!(
        pack["source"],
        serde_json::json!({"kind": "draft", "draft": "studio", "version": 2})
    );
    assert_eq!(pack["owner"], "team:mlr");
    let detail = actions("reed").await;
    assert_eq!(detail["published_playbook"], "mlr-pack");
    assert!(detail["retired_at"].is_null(), "the draft stays live");

    files["workflow.star"] = serde_json::json!(format!("{LAUNCH_WORKFLOW}\n# second pass\n"));
    let (status, body) = send_as(
        &app,
        "POST",
        versions,
        "reed",
        serde_json::json!({"files": files}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a published draft still saves: {body}"
    );
    let (status, second) = send_as(&app, "POST", publish, "reed", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    assert_eq!(
        second["id"], "mlr-pack",
        "a re-publish re-pins the same playbook"
    );
    assert_ne!(second["rev"], first["rev"]);
    let (_, pack) = send_as(
        &app,
        "GET",
        "/api/playbooks/mlr-pack",
        "reed",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(pack["source"]["version"], 3);
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM playbooks WHERE source_draft = 'studio'")
            .fetch_one(db.pool())
            .await?;
    assert_eq!(rows, 1);

    let launch = serde_json::json!({
        "params": {"topic": "attention", "depth": "deep"},
        "max_cost": 1.0,
        "max_time": "30m",
    });
    let (status, ack) = send_as(
        &app,
        "POST",
        "/api/playbooks/mlr-pack/launch",
        "kim",
        launch,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a member launches the published pack: {ack}"
    );
    let launched_repo: String = sqlx::query_scalar("SELECT repo FROM issues WHERE key = $1")
        .bind(ack["key"].as_str().expect("key"))
        .fetch_one(db.pool())
        .await?;
    assert_eq!(launched_repo, crate::playbooks::registry::DRAFT_LAUNCH_REPO);
    let mut schedule = schedule_body("0 6 * * *", "UTC");
    schedule["playbook"] = serde_json::json!("mlr-pack");
    schedule["params"] = serde_json::json!({"topic": "attention", "depth": "deep"});
    let (status, body) = send_as(&app, "POST", "/api/schedules", "reed", schedule).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a schedule adopts the published pack: {body}"
    );

    let (status, body) = send_as(
        &app,
        "POST",
        "/api/playbook-drafts",
        "root",
        serde_json::json!({"id": "roots", "description": "root's"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = send_as(
        &app,
        "POST",
        "/api/playbook-drafts/roots/versions",
        "root",
        serde_json::json!({"files": draft_files()}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send_as(
        &app,
        "POST",
        "/api/playbook-drafts/roots/publish",
        "root",
        into("root-pack"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "an admin publishes: {body}");
    let (status, body) = send_as(&app, "POST", publish, "reed", into("root-pack")).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "re-pinning takes playbook:update on the target: {body}"
    );
    let (status, body) = send_as(&app, "POST", publish, "reed", into("roots")).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a live draft holds the launch key: {body}"
    );
    Ok(())
}

/// Turning publishing on for someone is a teams-page edit: an administrator lists them in the
/// seeded publishers team, and from then on they publish the drafts they own, and only those.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_member_of_the_publishers_team_publishes_the_drafts_it_owns(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    crate::authz::bootstrap::seed_playbook_publishers(db.pool(), &[]).await?;
    let app = app_with_admins(db.clone(), vec!["root".to_string()]);
    let (status, body) = send_as(
        &app,
        "POST",
        "/api/teams",
        "reed",
        serde_json::json!({
            "slug": "mlr",
            "display_name": "MLR",
            "members": [
                {"kind": "user", "member": "reed", "role": "owner"},
                {"kind": "user", "member": "kim", "role": "member"},
            ],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let draft =
        serde_json::json!({"id": "studio", "description": "mlr sweep", "owner": "team:mlr"});
    let (status, body) = send_as(&app, "POST", "/api/playbook-drafts", "reed", draft).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let mut files = draft_files();
    files["workflow.star"] = serde_json::json!(LAUNCH_WORKFLOW);
    let (status, body) = send_as(
        &app,
        "POST",
        "/api/playbook-drafts/studio/versions",
        "reed",
        serde_json::json!({"files": files}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let publish = "/api/playbook-drafts/studio/publish";
    let into = serde_json::json!({"playbook": "mlr-pack"});
    let (status, body) = send_as(&app, "POST", publish, "reed", into.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "not yet a publisher: {body}");

    let members = serde_json::json!({"members": [
        {"kind": "user", "member": "root", "role": "owner"},
        {"kind": "user", "member": "reed", "role": "member"},
        {"kind": "user", "member": "kim", "role": "member"},
    ]});
    let uri = "/api/teams/playbook-publishers/members";
    let (status, body) = send_as(&app, "PUT", uri, "reed", members.clone()).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "only its owners manage it: {body}"
    );
    let (status, body) = send_as(&app, "PUT", uri, "root", members).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = send_as(&app, "POST", publish, "kim", into.clone()).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a publisher who is only a member of the owning team does not publish: {body}"
    );
    let (status, detail) = send_as(
        &app,
        "GET",
        "/api/playbook-drafts/studio",
        "reed",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert!(
        detail["actions"]
            .as_array()
            .expect("actions")
            .contains(&serde_json::json!("publish")),
        "{detail}"
    );
    let (status, ack) = send_as(&app, "POST", publish, "reed", into).await;
    assert_eq!(status, StatusCode::CREATED, "{ack}");
    assert_eq!(ack["id"], "mlr-pack");
    Ok(())
}

/// The task-evidence rig: a finished local run with a stored session, one declared task, one
/// fanned-out instance, and a captured file on the run's own state dir.
async fn evidence_rig(db: &Db, scratch: &std::path::Path) -> Result<()> {
    crate::runs::store::insert_run(
        db.pool(),
        &NewRun {
            run_id: "run-ev".to_string(),
            scope: None,
            issue: None,
            identity_digest: None,
            status: "succeeded".to_string(),
            pod: None,
            session_uri: None,
            best_score: None,
            cost_usd: Some(1.5),
        },
    )
    .await?;
    crate::runs::store::set_run_dispatch(
        db.pool(),
        "run-ev",
        crate::runs::model::RunDispatch::Local,
    )
    .await?;
    let plan = r#"[{"name":"scan","kind":"agent","depends_on":[],"session":"","needs":"any","required":true},
                   {"name":"triage","kind":"agent","depends_on":["scan"],"session":"","needs":"any","required":false}]"#;
    crate::runs::task_results::upsert_run_plan(db.pool(), "run-ev", 1, plan).await?;
    for (iter, task, cost) in [(0_i64, "triage[1027]", 0.25_f64), (1, "triage[1027]", 0.75)] {
        crate::runs::task_results::upsert_task_result(
            db.pool(),
            "run-ev",
            &crate::runs::model::TaskResult {
                iter,
                task: task.to_string(),
                status: "pass".to_string(),
                note: "classified as a feature".to_string(),
                cost_usd: Some(cost),
                secs: Some(4.0),
                blocked: None,
            },
        )
        .await?;
    }
    let session = concat!(
        "{\"v\":1,\"kind\":\"task_result\",\"task\":\"triage[1027]\",\"status\":\"pass\",",
        "\"iter\":1,\"attempts\":2,\"output\":{\"classification\":\"feature\"}}\n",
    );
    crate::runs::blob_store::put_run_session(db.pool(), "run-ev", session.as_bytes()).await?;

    let files = scratch
        .join("local-runs")
        .join("run-ev")
        .join("pack")
        .join("state")
        .join("files")
        .join("triage[1027]");
    std::fs::create_dir_all(&files)?;
    std::fs::write(
        files.join("TRIAGE.md"),
        "# 1027\n\nclassified as a feature\n",
    )?;
    Ok(())
}

/// One task's evidence is what the run left behind: the payload off its session line, the recorded
/// note and summed spend, and the files it captured under its own state dir.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn task_evidence_serves_the_payload_note_and_captured_files(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    evidence_rig(&db, dir.path()).await?;
    let app = router(ApiState {
        scratch_dir: dir.path().to_path_buf(),
        ..ApiState::test(db, Arc::new(Recorder::default()))
    });

    let (status, ev) =
        get_json_value(&app, "/api/runs/run-ev/tasks/triage%5B1027%5D/evidence").await;
    assert_eq!(status, StatusCode::OK, "{ev}");
    assert_eq!(ev["task"], "triage[1027]");
    assert_eq!(ev["status"], "pass");
    assert_eq!(ev["iter"], 1, "the newest attempt wins");
    assert_eq!(ev["note"], "classified as a feature");
    assert_eq!(ev["attempts"], 2, "the session's own count");
    assert_eq!(ev["cost_usd"], 1.0, "summed across attempts");
    assert_eq!(ev["payload"]["classification"], "feature");
    assert!(!ev["running"].as_bool().expect("running"), "{ev}");
    let files = ev["files"].as_array().expect("files");
    assert_eq!(files.len(), 1, "{ev}");
    assert_eq!(files[0]["name"], "TRIAGE.md");
    assert_eq!(files[0]["size_bytes"], 32);
    assert!(
        files[0]["content"]
            .as_str()
            .expect("content")
            .contains("classified as a feature"),
        "{ev}"
    );

    // A declared task nothing reported on: no result, no files, and a finished run says so rather
    // than 404ing a task the plan names.
    let (status, planned) = get_json_value(&app, "/api/runs/run-ev/tasks/scan/evidence").await;
    assert_eq!(status, StatusCode::OK, "{planned}");
    assert_eq!(planned["status"], serde_json::Value::Null);
    assert_eq!(planned["attempts"], serde_json::Value::Null);
    assert!(!planned["running"].as_bool().expect("running"));

    let (status, missing) = get_json_value(&app, "/api/runs/run-ev/tasks/nope/evidence").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
    let (status, no_run) = get_json_value(&app, "/api/runs/nobody/tasks/scan/evidence").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{no_run}");
    Ok(())
}

/// A task name or run id that would walk out of the run's directory is refused before any path is
/// joined, whether the traversal is spelled with slashes or percent-encoded ones.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn task_evidence_refuses_a_traversal(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    evidence_rig(&db, dir.path()).await?;
    // The file a traversal would be reaching for, one level above the run's files root.
    std::fs::write(
        dir.path().join("local-runs").join("SECRET.md"),
        "not yours\n",
    )?;
    let app = router(ApiState {
        scratch_dir: dir.path().to_path_buf(),
        ..ApiState::test(db, Arc::new(Recorder::default()))
    });

    for path in [
        "/api/runs/run-ev/tasks/..%2F..%2F..%2FSECRET.md/evidence",
        "/api/runs/run-ev/tasks/%2E%2E/evidence",
        "/api/runs/%2E%2E/tasks/triage%5B1027%5D/evidence",
        "/api/runs/run-ev%2F..%2F..%2Fetc/tasks/scan/evidence",
    ] {
        let (status, body) = get_json_value(&app, path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
    }
    let (status, body) = get_json_value(&app, "/api/runs/%2E%2E/log").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    Ok(())
}

/// The raw body of a GET, for a route that serves bytes rather than JSON.
async fn get_bytes(app: &Router, path: &str) -> (StatusCode, Vec<u8>) {
    let (status, bytes) = send(app, "GET", path, "wren", None).await;
    (status, bytes.to_vec())
}

/// A gzipped tar shaped like the one the loop POSTs: `<task>/<declared path>`.
fn run_files_bundle(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, body) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, name, *body)
            .expect("append");
    }
    let tar = builder.into_inner().expect("tar");
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut enc, &tar).expect("gzip");
    enc.finish().expect("gzip")
}

/// A run's product is what its tasks wrote. A local run answers off its own state dir and a pod run
/// off the bundle adopted at completion, both under the same keys, and each file's content comes
/// back byte for byte.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_run_lists_and_serves_the_files_its_tasks_captured(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool.clone());
    evidence_rig(&db, dir.path()).await?;
    crate::runs::store::insert_run(
        db.pool(),
        &NewRun {
            run_id: "run-pod".to_string(),
            scope: None,
            issue: None,
            identity_digest: None,
            status: "done".to_string(),
            pod: Some("loop-abc".to_string()),
            session_uri: None,
            best_score: None,
            cost_usd: None,
        },
    )
    .await?;
    let app = router(ApiState {
        scratch_dir: dir.path().to_path_buf(),
        ..ApiState::test(db, Arc::new(Recorder::default()))
    });

    // The local run: `evidence_rig` wrote one capture under a fan-out instance's own directory.
    let (status, local) = get_json_value(&app, "/api/runs/run-ev/files").await;
    assert_eq!(status, StatusCode::OK, "{local}");
    assert_eq!(local["files"].as_array().expect("files").len(), 1);
    assert_eq!(local["files"][0]["task"], "triage");
    assert_eq!(
        local["files"][0]["instance"], "1027",
        "a fan-out capture is listed under its instance's key"
    );
    assert_eq!(local["files"][0]["path"], "TRIAGE.md");
    assert_eq!(local["files"][0]["key"], "triage[1027]/TRIAGE.md");
    let (status, body) = get_bytes(&app, "/api/runs/run-ev/files/triage%5B1027%5D/TRIAGE.md").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"# 1027\n\nclassified as a feature\n");

    // The pod run: before the bundle is adopted its files are nowhere, and the listing says so
    // rather than inventing entries.
    let (status, empty) = get_json_value(&app, "/api/runs/run-pod/files").await;
    assert_eq!(status, StatusCode::OK, "{empty}");
    assert!(empty["files"].as_array().expect("files").is_empty());

    crate::runs::blob_store::put_run_files(
        &pool,
        "run-pod",
        run_files_bundle(&[
            ("roundup/FINDINGS.json", b"{\"broken\":21}"),
            ("propose/fix.patch", b"diff --git a/x b/x\n"),
        ]),
    )
    .await?;
    let (status, listed) = get_json_value(&app, "/api/runs/run-pod/files").await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let keys: Vec<&str> = listed["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| f["key"].as_str().expect("key"))
        .collect();
    assert_eq!(keys, ["propose/fix.patch", "roundup/FINDINGS.json"]);
    assert_eq!(listed["files"][1]["size_bytes"], 13);

    let (status, findings) = get_bytes(&app, "/api/runs/run-pod/files/roundup/FINDINGS.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(findings, b"{\"broken\":21}");

    // A file the task declared and never produced was never captured, so it is a 404 and not an
    // empty body pretending the run delivered it.
    let (status, missing) = get_bytes(&app, "/api/runs/run-pod/files/propose/PROPOSAL.md").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing:?}");

    let (status, unknown) = get_json_value(&app, "/api/runs/nobody/files").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{unknown}");
    let (status, traversal) = get_bytes(&app, "/api/runs/%2E%2E/files/roundup/x").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{traversal:?}");
    Ok(())
}

/// The run log is the local supervisor's engine output. A pod run's output lives in its pod, so it
/// says where instead of 404ing on a run that exists.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn run_log_serves_local_output_and_points_a_pod_run_at_its_pod(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool.clone());
    evidence_rig(&db, dir.path()).await?;
    crate::runs::store::insert_run(
        db.pool(),
        &NewRun {
            run_id: "run-pod".to_string(),
            scope: None,
            issue: None,
            identity_digest: None,
            status: "running".to_string(),
            pod: Some("loop-abc".to_string()),
            session_uri: None,
            best_score: None,
            cost_usd: None,
        },
    )
    .await?;
    let app = router(ApiState {
        scratch_dir: dir.path().to_path_buf(),
        ..ApiState::test(db, Arc::new(Recorder::default()))
    });

    // Before the supervisor writes one, a local run says where its log would be.
    let (status, none) = get_json_value(&app, "/api/runs/run-ev/log").await;
    assert_eq!(status, StatusCode::OK, "{none}");
    assert_eq!(none["text"], serde_json::Value::Null);
    assert!(
        none["location"]
            .as_str()
            .expect("location")
            .contains("run-ev"),
        "{none}"
    );

    std::fs::write(
        dir.path()
            .join("local-runs")
            .join("run-ev")
            .join("engine.log"),
        "plan run: admitted 3 tasks\nplan run: done\n",
    )?;
    let (status, log) = get_json_value(&app, "/api/runs/run-ev/log").await;
    assert_eq!(status, StatusCode::OK, "{log}");
    assert_eq!(log["dispatch"], "local");
    assert!(
        log["text"]
            .as_str()
            .expect("text")
            .contains("admitted 3 tasks")
    );
    assert!(!log["truncated"].as_bool().expect("truncated"));

    // No cluster to reach and nothing ingested yet: the pod run says where its output lives, and
    // names the cluster as well as the namespace so the hint is runnable.
    let (status, pod) = get_json_value(&app, "/api/runs/run-pod/log").await;
    assert_eq!(status, StatusCode::OK, "{pod}");
    assert_eq!(pod["dispatch"], "pod");
    assert_eq!(pod["text"], serde_json::Value::Null);
    assert_eq!(
        pod["location"],
        "pod loop-abc in namespace autoresearch on cluster hub"
    );

    // Once the run's session is ingested it outlives the pod, so the log is served from the store.
    crate::runs::blob_store::put_run_session(
        &pool,
        "run-pod",
        b"task scan: pass\ntask roundup: pass\n",
    )
    .await?;
    let (status, stored) = get_json_value(&app, "/api/runs/run-pod/log").await;
    assert_eq!(status, StatusCode::OK, "{stored}");
    assert!(
        stored["text"]
            .as_str()
            .expect("text")
            .contains("task roundup: pass"),
        "{stored}"
    );
    assert_eq!(stored["location"], serde_json::Value::Null);

    // The engine log kept at completion is the pod's own output and wins over the session.
    crate::runs::blob_store::put_run_engine_log(
        &pool,
        "run-pod",
        "gateway_boot: driver=kubernetes\ntriage[a]: transport retries exhausted\n",
    )
    .await?;
    let (status, engine) = get_json_value(&app, "/api/runs/run-pod/log").await;
    assert_eq!(status, StatusCode::OK, "{engine}");
    let text = engine["text"].as_str().expect("text");
    assert!(text.contains("gateway_boot: driver=kubernetes"), "{engine}");
    assert!(!text.contains("task roundup: pass"), "{engine}");

    let (status, unknown) = get_json_value(&app, "/api/runs/nobody/log").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{unknown}");
    Ok(())
}

/// A key mints, authenticates, and names its owner — and the three ways it can be wrong are told
/// apart, because each one needs the caller to do something different about it.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn an_api_key_authenticates_as_the_person_who_minted_it(pool: PgPool) -> Result<()> {
    let now = jiff::Timestamp::now();
    crate::identity::oidc::users::record_login(&pool, "sub-alice", "alice", None, now).await?;
    let mut conn = pool.acquire().await?;
    crate::identity::oidc::users::record_groups_on(
        &mut conn,
        "sub-alice",
        &["/groups/team-x".to_string()],
        now,
    )
    .await?;
    drop(conn);

    let minted = crate::identity::api_key::mint(&pool, "sub-alice", "laptop", None).await?;
    let who = crate::identity::api_key::verify(&pool, &minted.secret).await;
    let who = who.expect("the freshly minted key authenticates");
    assert_eq!(who.login, "alice");
    assert_eq!(who.sub, "sub-alice");
    assert_eq!(who.groups, vec!["/groups/team-x".to_string()]);

    // A wrong secret against a real id reads the same as an id that never existed: telling them
    // apart would confirm which ids are real.
    let (id, _) = minted
        .secret
        .trim_start_matches(crate::identity::api_key::PREFIX)
        .split_once('_')
        .expect("a minted key has both halves");
    let forged = format!("{}{id}_not-the-secret", crate::identity::api_key::PREFIX);
    assert_eq!(
        crate::identity::api_key::verify(&pool, &forged)
            .await
            .unwrap_err(),
        crate::identity::api_key::KeyRefusal::Unknown
    );
    assert_eq!(
        crate::identity::api_key::verify(&pool, "not-a-key-at-all")
            .await
            .unwrap_err(),
        crate::identity::api_key::KeyRefusal::Malformed
    );

    // An expiry already past is refused as expired, which is what tells its owner to mint another.
    let stale =
        crate::identity::api_key::mint(&pool, "sub-alice", "old", Some("2020-01-01T00:00:00Z"))
            .await?;
    assert!(matches!(
        crate::identity::api_key::verify(&pool, &stale.secret)
            .await
            .unwrap_err(),
        crate::identity::api_key::KeyRefusal::Expired { .. }
    ));

    // Revocation is immediate and says so by name.
    assert!(crate::identity::api_key::revoke(&pool, "sub-alice", &minted.key.id).await?);
    assert!(matches!(
        crate::identity::api_key::verify(&pool, &minted.secret)
            .await
            .unwrap_err(),
        crate::identity::api_key::KeyRefusal::Revoked { .. }
    ));
    // Revoking twice is not an error the second time, it is simply nothing left to revoke.
    assert!(!crate::identity::api_key::revoke(&pool, "sub-alice", &minted.key.id).await?);
    Ok(())
}

/// One caller's key may not revoke another's, however well they guess the id.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_key_is_revocable_only_by_its_owner(pool: PgPool) -> Result<()> {
    let now = jiff::Timestamp::now();
    crate::identity::oidc::users::record_login(&pool, "sub-alice", "alice", None, now).await?;
    crate::identity::oidc::users::record_login(&pool, "sub-bob", "bob", None, now).await?;
    let alices = crate::identity::api_key::mint(&pool, "sub-alice", "laptop", None).await?;

    assert!(!crate::identity::api_key::revoke(&pool, "sub-bob", &alices.key.id).await?);
    assert!(
        crate::identity::api_key::verify(&pool, &alices.secret)
            .await
            .is_ok(),
        "bob's revoke must not have touched alice's key"
    );
    assert!(
        crate::identity::api_key::list(&pool, "sub-bob")
            .await?
            .is_empty()
    );
    assert_eq!(
        crate::identity::api_key::list(&pool, "sub-alice")
            .await?
            .len(),
        1
    );
    Ok(())
}

/// The hosted MCP surface, end to end over its own guard: an unauthenticated initialize is
/// refused, a key-carrying one is answered, and further requests — each standing alone, because
/// the surface is stateless — list the tools and call one as the key's owner.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn the_mcp_surface_speaks_mcp_to_a_key_and_nothing_to_anyone_else(
    pool: PgPool,
) -> Result<()> {
    let now = jiff::Timestamp::now();
    crate::identity::oidc::users::record_login(&pool, "sub-alice", "alice", None, now).await?;
    let minted = crate::identity::api_key::mint(&pool, "sub-alice", "agent", None).await?;

    let (db, _d) = db_with(pool.clone());
    let app = crate::mcp::router(
        app(db, Arc::new(Recorder::default())),
        pool,
        "https://crucible-api.example.com".to_string(),
    );

    let initialize = || {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"},
            },
        })
    };
    let post = |app: Router, bearer: Option<String>, body: serde_json::Value| async move {
        // rmcp checks Host as DNS-rebinding protection; a real client's request carries one.
        let mut req = HttpRequest::post("/mcp")
            .header(header::HOST, "crucible-api.example.com")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream");
        if let Some(bearer) = bearer {
            req = req.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        let res = app
            .oneshot(req.body(Body::from(serde_json::to_vec(&body)?))?)
            .await?;
        let status = res.status();
        let session = res
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
        anyhow::Ok((
            status,
            session,
            String::from_utf8_lossy(&bytes).into_owned(),
        ))
    };

    let (status, _, body) = post(app.clone(), None, initialize()).await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert!(body.contains("api key"), "{body}");

    let (status, _, body) = post(
        app.clone(),
        Some("crk_deadbeef_nope".to_string()),
        initialize(),
    )
    .await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    let (status, session, body) =
        post(app.clone(), Some(minted.secret.clone()), initialize()).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("serverInfo"), "{body}");
    assert!(
        session.is_none(),
        "a stateless surface hands out no session to resume: {body}"
    );

    // Stateless, so nothing carries over: the next request stands on its own, authenticated by the
    // same key and naming the protocol version the initialize above settled.
    let listed = app
        .clone()
        .oneshot(
            HttpRequest::post("/mcp")
                .header(header::HOST, "crucible-api.example.com")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream")
                .header(header::AUTHORIZATION, format!("Bearer {}", minted.secret))
                .header("mcp-protocol-version", "2025-06-18")
                .body(Body::from(serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/list",
                }))?))?,
        )
        .await?;
    assert_eq!(listed.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(listed.into_body(), usize::MAX).await?;
    let tools = String::from_utf8_lossy(&bytes);
    assert!(tools.contains("crucible_issues"), "{tools}");
    assert!(tools.contains("crucible_whoami"), "{tools}");
    // The launching half: an agent that can only read is not one that can drive the controller.
    assert!(tools.contains("crucible_launch"), "{tools}");
    assert!(tools.contains("crucible_runs"), "{tools}");
    assert!(tools.contains("crucible_schedules"), "{tools}");

    // The proof that the in-process transport carries the caller: whoami is answered by the very
    // handler a network request would have reached, and it names the key's owner rather than the
    // anonymous caller a missing identity would produce.
    let called = app
        .oneshot(
            HttpRequest::post("/mcp")
                .header(header::HOST, "crucible-api.example.com")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream")
                .header(header::AUTHORIZATION, format!("Bearer {}", minted.secret))
                .header("mcp-protocol-version", "2025-06-18")
                .body(Body::from(serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools/call",
                    "params": {"name": "crucible_whoami", "arguments": {}},
                }))?))?,
        )
        .await?;
    assert_eq!(called.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(called.into_body(), usize::MAX).await?;
    let answer = String::from_utf8_lossy(&bytes);
    assert!(answer.contains("alice"), "{answer}");
    Ok(())
}

// --- the model-provider registry ---------------------------------------------

/// One registration, straight into the store, for the tests that care about what a launch does
/// with a provider rather than about how it got registered.
async fn seed_provider(
    pool: &PgPool,
    id: &str,
    kind: crate::playbooks::providers::ProviderKind,
    enabled: bool,
) {
    crate::playbooks::providers::upsert(
        pool,
        &crate::playbooks::providers::NewProvider {
            owner: crate::authz::model::Principal::platform(),
            id,
            display_name: "Platform OpenAI",
            kind,
            models: &[],
            default_model: None,
            secret: None,
            endpoint: None,
            harness: None,
            enabled,
            created_by: "wren",
        },
    )
    .await
    .expect("register a provider");
}

/// One registry row of the given kind under `name`, owned by alice. No Vault: the registration
/// checks read the metadata, never the bytes.
async fn seed_secret(pool: &PgPool, id: &str, name: &str, kind: crate::secrets::SecretKind) {
    let owner = crate::authz::model::Principal::parse("user:alice").expect("principal");
    let name = crate::secrets::SecretName::parse(name).expect("name");
    let mut conn = pool.acquire().await.expect("conn");
    crate::secrets::store::insert(
        &mut conn,
        &crate::secrets::store::NewSecret {
            id,
            name: &name,
            owner: &owner,
            kind,
            visibility: crate::secrets::Visibility::BrokerOnly,
            consumer: crate::secrets::ConsumerClass::Run,
            mode: crate::secrets::SecretMode::Managed,
            vault_path: "user:alice/key",
            current_version: Some(1),
            created_by: Some("user:alice"),
        },
    )
    .await
    .expect("insert a secret");
}

fn admin_json(method: &str, path: &str, body: serde_json::Value) -> Result<HttpRequest<Body>> {
    Ok(HttpRequest::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-auth-request-user", "wren")
        .body(Body::from(serde_json::to_vec(&body)?))?)
}

async fn json_of(res: Response) -> Result<serde_json::Value> {
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// The zero-config shape: nothing registered, so the pickers get two empty lists and the forms
/// show no choice at all.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn config_providers_is_empty_until_something_is_registered(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db, vec!["wren".to_string()]);
    let res = app
        .oneshot(HttpRequest::get("/api/config/providers").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let v = json_of(res).await?;
    assert_eq!(v["providers"].as_array().map(Vec::len), Some(0));
    assert_eq!(v["defaults"].as_array().map(Vec::len), Some(0));
    Ok(())
}

/// The picker read is open to any authenticated caller, lists only what a launch may actually
/// pick, and never names the credential a provider spends.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn config_providers_lists_enabled_providers_and_the_defaults(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    seed_secret(
        db.pool(),
        "sec-1",
        "openai-key",
        crate::secrets::SecretKind::InferenceApiKey,
    )
    .await;
    crate::playbooks::providers::upsert(
        db.pool(),
        &crate::playbooks::providers::NewProvider {
            owner: crate::authz::model::Principal::platform(),
            id: "plat-openai",
            display_name: "Platform OpenAI",
            kind: crate::playbooks::providers::ProviderKind::OpenAi,
            models: &[],
            default_model: None,
            secret: Some(&crate::playbooks::providers::ProviderSecretRef {
                name: "openai-key".to_string(),
                owner: crate::authz::model::Principal::parse("user:alice").expect("a principal"),
            }),
            endpoint: None,
            harness: None,
            enabled: true,
            created_by: "wren",
        },
    )
    .await?;
    seed_provider(
        db.pool(),
        "retired",
        crate::playbooks::providers::ProviderKind::Anthropic,
        false,
    )
    .await;
    crate::playbooks::providers::set_default(
        db.pool(),
        &crate::playbooks::providers::DispatchDefault {
            scope_kind: crate::playbooks::providers::DefaultScope::Platform,
            scope_ref: String::new(),
            workload_class: crate::playbooks::providers::WorkloadClass::Autoresearch,
            provider_id: "plat-openai".to_string(),
            model: Some("gpt-5.6-sol".to_string()),
        },
    )
    .await?;

    let app = app_with_admins(db, vec![]);
    let res = app
        .oneshot(
            HttpRequest::get("/api/config/providers")
                .header("x-auth-request-user", "nobody")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "any authenticated caller may read the pickers"
    );
    let v = json_of(res).await?;
    let providers = v["providers"].as_array().expect("providers");
    assert_eq!(
        providers.len(),
        1,
        "a disabled provider is not offered: {v}"
    );
    assert_eq!(providers[0]["id"], "plat-openai");
    assert_eq!(providers[0]["kind"], "openai");
    assert_eq!(providers[0]["default_model"], "gpt-5.6-luna");
    assert_eq!(providers[0]["models"][1], "gpt-5.6-sol");
    assert!(
        providers[0]["secret_name"].is_null(),
        "the picker read never names the credential: {v}"
    );
    let defaults = v["defaults"].as_array().expect("defaults");
    assert_eq!(defaults.len(), 1);
    assert_eq!(defaults[0]["scope_kind"], "platform");
    assert_eq!(defaults[0]["workload_class"], "autoresearch");
    assert_eq!(defaults[0]["provider"], "plat-openai");
    assert_eq!(defaults[0]["model"], "gpt-5.6-sol");
    Ok(())
}

/// A provider somebody owns is offered to, and pinnable by, only the callers who may read it; to
/// everyone else it was never registered, enabled or not.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn an_owned_provider_is_offered_and_pinned_only_by_those_who_may_read_it(
    pool: PgPool,
) -> Result<()> {
    let (db, _d) = db_with(pool);
    seed_provider(
        db.pool(),
        "plat-openai",
        crate::playbooks::providers::ProviderKind::OpenAi,
        true,
    )
    .await;
    for (id, enabled) in [("reeds", true), ("reeds-retired", false)] {
        crate::playbooks::providers::upsert(
            db.pool(),
            &crate::playbooks::providers::NewProvider {
                owner: crate::authz::model::Principal::parse("user:reed").expect("a principal"),
                id,
                display_name: "Reed's",
                kind: crate::playbooks::providers::ProviderKind::Anthropic,
                models: &[],
                default_model: None,
                secret: None,
                endpoint: None,
                harness: None,
                enabled,
                created_by: "reed",
            },
        )
        .await?;
    }
    let app = app_with_playbook_caps(
        db.clone(),
        vec![],
        crate::config::PlaybookCaps {
            max_cost: 10.0,
            max_time: crate::model::MaxTime::parse("2h").expect("cap"),
        },
    );

    let offered = |v: &serde_json::Value| -> Vec<String> {
        let mut ids: Vec<String> = v["providers"]
            .as_array()
            .expect("providers")
            .iter()
            .map(|p| p["id"].as_str().expect("id").to_string())
            .collect();
        ids.sort();
        ids
    };
    let picker = |user: &'static str| {
        let app = app.clone();
        async move {
            let res = app
                .oneshot(
                    HttpRequest::get("/api/config/providers")
                        .header("x-auth-request-user", user)
                        .body(Body::empty())
                        .expect("req"),
                )
                .await
                .expect("resp");
            json_of(res).await.expect("json")
        }
    };
    assert_eq!(offered(&picker("reed").await), vec!["plat-openai", "reeds"]);
    assert_eq!(offered(&picker("mallory").await), vec!["plat-openai"]);

    for user in ["reed", "mallory"] {
        let (status, body) = send_as(
            &app,
            "POST",
            "/api/playbook-drafts",
            user,
            serde_json::json!({"id": format!("{user}-studio"), "description": "d"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let mut files = draft_files();
        files["workflow.star"] = serde_json::json!(LAUNCH_WORKFLOW);
        let (status, body) = send_as(
            &app,
            "POST",
            &format!("/api/playbook-drafts/{user}-studio/versions"),
            user,
            serde_json::json!({"files": files}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let launch = |provider: &str| {
        serde_json::json!({
            "params": {"topic": "attention", "depth": "deep"},
            "max_cost": 1.0,
            "max_time": "30m",
            "provider": provider,
        })
    };
    for (user, provider, expected) in [
        ("mallory", "reeds", "is not registered"),
        ("mallory", "reeds-retired", "is not registered"),
        ("reed", "reeds-retired", "is disabled"),
    ] {
        let (status, body) = send_as(
            &app,
            "POST",
            &format!("/api/playbook-drafts/{user}-studio/launch"),
            user,
            launch(provider),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{user} {provider}: {body}"
        );
        let message = body["fields"][0]["message"].as_str().unwrap_or_default();
        assert!(message.contains(expected), "{user} {provider}: {body}");
        let enabled = message.rsplit("(enabled: ").next().unwrap_or_default();
        assert_eq!(
            enabled.contains("reeds"),
            user == "reed",
            "the enabled list names only what the caller may read: {body}"
        );
    }
    let (status, body) = send_as(
        &app,
        "POST",
        "/api/playbook-drafts/mallory-studio/launch",
        "mallory",
        launch("plat-openai"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a platform provider is everyone's: {body}"
    );
    Ok(())
}

/// The registry round trip: register, read back with the credential an administrator may see,
/// re-register to disable, and deregister.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn an_admin_registers_edits_and_deregisters_a_provider(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    seed_secret(
        db.pool(),
        "sec-1",
        "openai-key",
        crate::secrets::SecretKind::InferenceApiKey,
    )
    .await;
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);

    let res = app
        .clone()
        .oneshot(admin_json(
            "POST",
            "/api/providers",
            serde_json::json!({
                "id": "  plat-openai  ",
                "display_name": "Platform OpenAI",
                "kind": "openai",
                "secret_name": "openai-key",
            }),
        )?)
        .await?;
    assert_eq!(res.status(), StatusCode::CREATED);
    let v = json_of(res).await?;
    assert_eq!(v["id"], "plat-openai");
    assert_eq!(v["secret_name"], "openai-key");
    assert_eq!(v["enabled"], true);
    assert_eq!(
        v["models"][0], "gpt-5.6-luna",
        "an empty list takes the kind's curated one"
    );
    assert_eq!(v["created_by"], "wren");

    let res = app
        .clone()
        .oneshot(admin_json(
            "POST",
            "/api/providers",
            serde_json::json!({"id": "plat-openai", "display_name": "again", "kind": "openai"}),
        )?)
        .await?;
    assert_eq!(
        res.status(),
        StatusCode::CONFLICT,
        "a POST never replaces a registration"
    );

    let res = app
        .clone()
        .oneshot(admin_json(
            "PUT",
            "/api/providers/plat-openai",
            serde_json::json!({
                "display_name": "Platform OpenAI (retiring)",
                "kind": "openai",
                "models": ["gpt-5.6-sol"],
                "default_model": "gpt-5.6-sol",
                "enabled": false,
            }),
        )?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let v = json_of(res).await?;
    assert_eq!(v["enabled"], false);
    assert_eq!(v["models"], serde_json::json!(["gpt-5.6-sol"]));
    assert_eq!(v["default_model"], "gpt-5.6-sol");
    assert!(
        v["secret_name"].is_null(),
        "a PUT is the whole registration, not a patch"
    );

    let res = app
        .clone()
        .oneshot(
            HttpRequest::get("/api/providers")
                .header("x-auth-request-user", "wren")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let v = json_of(res).await?;
    assert_eq!(
        v.as_array().map(Vec::len),
        Some(1),
        "the admin list shows disabled rows too"
    );

    let res = app
        .clone()
        .oneshot(
            HttpRequest::delete("/api/providers/plat-openai")
                .header("x-auth-request-user", "wren")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert!(
        crate::playbooks::providers::get(db.pool(), "plat-openai")
            .await?
            .is_none()
    );
    let res = app
        .oneshot(
            HttpRequest::delete("/api/providers/plat-openai")
                .header("x-auth-request-user", "wren")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// Every write to the registry is an administrator's, and a caller who is not one changes nothing.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_non_admin_may_not_touch_the_registry(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    seed_provider(
        db.pool(),
        "plat-openai",
        crate::playbooks::providers::ProviderKind::OpenAi,
        true,
    )
    .await;
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);

    let body = serde_json::json!({"id": "sneaky", "display_name": "Mine", "kind": "openai"});
    let requests: Vec<HttpRequest<Body>> = vec![
        HttpRequest::get("/api/providers")
            .header("x-auth-request-user", "mallory")
            .body(Body::empty())?,
        HttpRequest::builder()
            .method("POST")
            .uri("/api/providers")
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-auth-request-user", "mallory")
            .body(Body::from(serde_json::to_vec(&body)?))?,
        HttpRequest::builder()
            .method("PUT")
            .uri("/api/providers/plat-openai")
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-auth-request-user", "mallory")
            .body(Body::from(serde_json::to_vec(
                &serde_json::json!({"display_name": "Mine", "kind": "openai"}),
            )?))?,
        HttpRequest::delete("/api/providers/plat-openai")
            .header("x-auth-request-user", "mallory")
            .body(Body::empty())?,
        HttpRequest::builder()
            .method("PUT")
            .uri("/api/config/dispatch-defaults")
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-auth-request-user", "mallory")
            .body(Body::from(serde_json::to_vec(&serde_json::json!({
                "scope_kind": "platform",
                "workload_class": "playbook",
                "provider": "plat-openai",
            }))?))?,
        HttpRequest::delete(
            "/api/config/dispatch-defaults?scope_kind=platform&workload_class=playbook",
        )
        .header("x-auth-request-user", "mallory")
        .body(Body::empty())?,
    ];
    for req in requests {
        let (method, uri) = (req.method().clone(), req.uri().clone());
        let res = app.clone().oneshot(req).await?;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{method} {uri}");
    }
    assert_eq!(
        crate::playbooks::providers::list(db.pool(), false)
            .await?
            .len(),
        1
    );
    assert!(
        crate::playbooks::providers::list_defaults(db.pool())
            .await?
            .is_empty()
    );
    Ok(())
}

/// Vertex runs on the deploy profile's ambient credentials, so a key attached to one has nowhere
/// to go. Refused at registration rather than at the first dispatch that needed it.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_vertex_provider_may_not_name_a_secret(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    seed_secret(
        db.pool(),
        "sec-1",
        "vertex-key",
        crate::secrets::SecretKind::InferenceApiKey,
    )
    .await;
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    let res = app
        .oneshot(admin_json(
            "POST",
            "/api/providers",
            serde_json::json!({
                "id": "vertex",
                "display_name": "Vertex",
                "kind": "vertex",
                "secret_name": "vertex-key",
            }),
        )?)
        .await?;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let v = json_of(res).await?;
    assert!(
        v["error"].as_str().unwrap_or_default().contains("ambient"),
        "the 422 must say why a vertex provider takes no secret: {v}"
    );
    assert!(
        crate::playbooks::providers::list(db.pool(), false)
            .await?
            .is_empty()
    );
    Ok(())
}

/// A named credential must be a registered secret, and it must be an inference key: any other kind
/// would be refused at dispatch by the projection, which is far too late to learn it.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_provider_key_must_exist_and_be_an_inference_key(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    seed_secret(
        db.pool(),
        "sec-1",
        "pr-token",
        crate::secrets::SecretKind::Opaque,
    )
    .await;
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);

    for (secret_name, expected) in [
        ("nothing-here", "names no secret"),
        ("pr-token", "which is a opaque secret"),
    ] {
        let res = app
            .clone()
            .oneshot(admin_json(
                "POST",
                "/api/providers",
                serde_json::json!({
                    "id": "plat-openai",
                    "display_name": "Platform OpenAI",
                    "kind": "openai",
                    "secret_name": secret_name,
                }),
            )?)
            .await?;
        assert_eq!(
            res.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{secret_name}"
        );
        let v = json_of(res).await?;
        assert!(
            v["error"].as_str().unwrap_or_default().contains(expected),
            "{secret_name}: {v}"
        );
    }
    assert!(
        crate::playbooks::providers::list(db.pool(), false)
            .await?
            .is_empty()
    );

    seed_secret(
        db.pool(),
        "sec-2",
        "openai-key",
        crate::secrets::SecretKind::InferenceApiKey,
    )
    .await;
    let res = app
        .oneshot(admin_json(
            "POST",
            "/api/providers",
            serde_json::json!({
                "id": "plat-openai",
                "display_name": "Platform OpenAI",
                "kind": "openai",
                "secret_name": "openai-key",
            }),
        )?)
        .await?;
    assert_eq!(res.status(), StatusCode::CREATED);
    Ok(())
}

/// A default is set, read back through resolution, and cleared. It may only name a provider a
/// launch could pick: a default nobody may take would refuse every dispatch that inherited it.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn an_admin_sets_and_clears_a_dispatch_default(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    seed_provider(
        db.pool(),
        "plat-openai",
        crate::playbooks::providers::ProviderKind::OpenAi,
        true,
    )
    .await;
    seed_provider(
        db.pool(),
        "retired",
        crate::playbooks::providers::ProviderKind::Anthropic,
        false,
    )
    .await;
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);

    let res = app
        .clone()
        .oneshot(admin_json(
            "PUT",
            "/api/config/dispatch-defaults",
            serde_json::json!({
                "scope_kind": "domain",
                "scope_ref": "org/vllm",
                "workload_class": "autoresearch",
                "provider": "plat-openai",
                "model": "gpt-5.6-sol",
            }),
        )?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let v = json_of(res).await?;
    assert_eq!(v["scope_ref"], "org/vllm");
    assert_eq!(v["provider"], "plat-openai");
    assert_eq!(
        crate::playbooks::providers::resolve_dispatch(
            db.pool(),
            None,
            Some("org/vllm"),
            crate::playbooks::providers::WorkloadClass::Autoresearch,
        )
        .await?
        .expect("the default resolves")
        .model,
        "gpt-5.6-sol"
    );

    for bad in [
        serde_json::json!({"scope_kind": "domain", "workload_class": "playbook", "provider": "plat-openai"}),
        serde_json::json!({"scope_kind": "platform", "scope_ref": "org/vllm", "workload_class": "playbook", "provider": "plat-openai"}),
        serde_json::json!({"scope_kind": "platform", "workload_class": "playbook", "provider": "retired"}),
        serde_json::json!({"scope_kind": "platform", "workload_class": "playbook", "provider": "never-registered"}),
    ] {
        let res = app
            .clone()
            .oneshot(admin_json(
                "PUT",
                "/api/config/dispatch-defaults",
                bad.clone(),
            )?)
            .await?;
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
    assert_eq!(
        crate::playbooks::providers::list_defaults(db.pool())
            .await?
            .len(),
        1
    );

    let res = app
        .clone()
        .oneshot(
            HttpRequest::delete(
                "/api/config/dispatch-defaults?scope_kind=domain&scope_ref=org%2Fvllm&workload_class=autoresearch",
            )
            .header("x-auth-request-user", "wren")
            .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert!(
        crate::playbooks::providers::list_defaults(db.pool())
            .await?
            .is_empty()
    );

    let res = app
        .oneshot(
            HttpRequest::delete(
                "/api/config/dispatch-defaults?scope_kind=domain&scope_ref=org%2Fvllm&workload_class=autoresearch",
            )
            .header("x-auth-request-user", "wren")
            .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// A launch that picks a provider pins the pair on its issue row, where every later dispatch reads
/// it, and echoes what it pinned.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn adopt_scenario_pins_the_provider_it_picked(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    seed_provider(
        db.pool(),
        "plat-openai",
        crate::playbooks::providers::ProviderKind::OpenAi,
        true,
    )
    .await;
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);

    let res = app
        .oneshot(admin_json(
            "POST",
            "/api/scenarios",
            serde_json::json!({
                "title": "t",
                "body": "b",
                "affected_repos": ["owner/repo"],
                "justification": "j",
                "provider": " plat-openai ",
                "model": " gpt-5.6-sol ",
            }),
        )?)
        .await?;
    assert_eq!(res.status(), StatusCode::CREATED);
    let v = json_of(res).await?;
    assert_eq!(
        v["provider"], "plat-openai",
        "the ack echoes the trimmed pair"
    );
    assert_eq!(v["model"], "gpt-5.6-sol");
    let key = v["key"].as_str().expect("key").to_string();

    let issue = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .expect("issue row");
    assert_eq!(issue.agent_provider.as_deref(), Some("plat-openai"));
    assert_eq!(issue.agent_model.as_deref(), Some("gpt-5.6-sol"));
    let resolved = crate::playbooks::providers::resolve_for_issue(
        db.pool(),
        &issue,
        crate::playbooks::providers::WorkloadClass::Autoresearch,
    )
    .await?
    .expect("the pin resolves");
    assert_eq!(resolved.model, "gpt-5.6-sol");
    Ok(())
}

/// Every launch surface that renders a picker pins the pair the same way: a playbook-class
/// launch stores it on its row exactly as an autoresearch one does, and the run reaches the pod as
/// `crucible plan run --harness/--model` replacing the pack manifest's `[agent]` table.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn every_launch_surface_agrees_on_what_a_provider_pin_means(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    seed_provider(
        db.pool(),
        "plat-openai",
        crate::playbooks::providers::ProviderKind::OpenAi,
        true,
    )
    .await;
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    let digest = register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    // The registered playbook launch.
    let (status, ack) = post_launch(
        &app,
        "survey",
        serde_json::json!({
            "params": {"topic": "attention sinks", "depth": "deep"},
            "max_cost": 3.5,
            "max_time": "30m",
            "schema_digest": digest,
            "provider": "plat-openai",
            "model": "gpt-5.6-luna",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{ack}");
    assert_eq!(ack["provider"], "plat-openai", "{ack}");
    assert_eq!(ack["model"], "gpt-5.6-luna", "{ack}");
    let issue = crate::issues::store::get_issue(db.pool(), ack["key"].as_str().expect("key"))
        .await?
        .expect("issue row");
    assert_eq!(issue.agent_provider.as_deref(), Some("plat-openai"));
    assert_eq!(issue.agent_model.as_deref(), Some("gpt-5.6-luna"));
    let resolved = crate::playbooks::providers::resolve_for_issue(
        db.pool(),
        &issue,
        crate::playbooks::providers::WorkloadClass::Playbook,
    )
    .await?
    .expect("the pin resolves");
    assert_eq!(resolved.harness, crucible::manifest::Harness::Codex);
    assert_eq!(resolved.model, "gpt-5.6-luna");
    let (status, view) = get_json_value(
        &app,
        &format!("/api/playbook-runs/{}", urlencoding_encode(&issue.key)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["launch"]["agent_provider"], "plat-openai", "{view}");
    assert_eq!(view["launch"]["agent_model"], "gpt-5.6-luna", "{view}");
    let (status, rows) = get_json_value(&app, "/api/playbook-runs").await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    assert_eq!(rows[0]["agent_provider"], "plat-openai", "{rows}");
    assert_eq!(rows[0]["agent_model"], "gpt-5.6-luna", "{rows}");

    // The draft test-fire.
    let created = post_admin(
        &app,
        "/api/playbook-drafts",
        serde_json::json!({"id": "studio", "description": "a drafted pack"}),
    )
    .await;
    assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);
    let mut files = draft_files();
    files["workflow.star"] = serde_json::json!(LAUNCH_WORKFLOW);
    let saved = post_admin(
        &app,
        "/api/playbook-drafts/studio/versions",
        serde_json::json!({"files": files}),
    )
    .await;
    assert_eq!(saved.0, StatusCode::OK, "{}", saved.1);
    let (status, ack) = post_admin(
        &app,
        "/api/playbook-drafts/studio/launch",
        serde_json::json!({
            "params": {"topic": "attention", "depth": "deep"},
            "max_cost": 2.5,
            "max_time": "30m",
            "schema_digest": saved.1["schema_digest"],
            "provider": "plat-openai",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{ack}");
    assert_eq!(ack["provider"], "plat-openai", "{ack}");
    assert!(
        ack["model"].is_null(),
        "no model pinned takes the provider's default: {ack}"
    );
    let issue = crate::issues::store::get_issue(db.pool(), ack["key"].as_str().expect("key"))
        .await?
        .expect("issue row");
    assert_eq!(issue.agent_provider.as_deref(), Some("plat-openai"));
    assert_eq!(issue.agent_model, None);
    let (status, view) = get_json_value(
        &app,
        &format!("/api/playbook-runs/{}", urlencoding_encode(&issue.key)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["launch"]["agent_provider"], "plat-openai", "{view}");
    assert!(view["launch"]["agent_model"].is_null(), "{view}");

    // The schedule, whose every firing mints a launch carrying the pair.
    let mut body = schedule_body("30 6 * * MON-FRI", "America/New_York");
    body["provider"] = serde_json::json!("plat-openai");
    let (status, ack) = send_json(&app, HttpRequest::post("/api/schedules"), body).await;
    assert_eq!(status, StatusCode::CREATED, "{ack}");
    let schedules = crate::launches::schedules::ScheduleStore::new(db.clone())
        .list(50)
        .await?;
    assert_eq!(schedules.len(), 1);
    assert_eq!(schedules[0].agent_provider.as_deref(), Some("plat-openai"));

    // The autoresearch surface.
    let (status, ack) = post_admin(
        &app,
        "/api/scenarios",
        serde_json::json!({
            "title": "t",
            "body": "b",
            "affected_repos": ["owner/repo"],
            "justification": "j",
            "provider": "plat-openai",
            "model": "gpt-5.6-sol",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{ack}");
    let issue = crate::issues::store::get_issue(db.pool(), ack["key"].as_str().expect("key"))
        .await?
        .expect("issue row");
    assert_eq!(issue.agent_provider.as_deref(), Some("plat-openai"));
    Ok(())
}

/// The same refusals on the playbook surfaces: a registered launch and a draft test-fire each
/// refuse a disabled or unregistered provider, a model with no provider, a blank field, and a
/// model name a shell would read, and each refusal adopts nothing.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_playbook_launch_refuses_a_pin_it_may_not_pick(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    seed_provider(
        db.pool(),
        "retired",
        crate::playbooks::providers::ProviderKind::Anthropic,
        false,
    )
    .await;
    seed_provider(
        db.pool(),
        "plat-openai",
        crate::playbooks::providers::ProviderKind::OpenAi,
        true,
    )
    .await;
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    let digest = register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    let created = post_admin(
        &app,
        "/api/playbook-drafts",
        serde_json::json!({"id": "studio", "description": "a drafted pack"}),
    )
    .await;
    assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);
    let mut files = draft_files();
    files["workflow.star"] = serde_json::json!(LAUNCH_WORKFLOW);
    let saved = post_admin(
        &app,
        "/api/playbook-drafts/studio/versions",
        serde_json::json!({"files": files}),
    )
    .await;
    assert_eq!(saved.0, StatusCode::OK, "{}", saved.1);

    let picks = [
        (
            serde_json::json!({"provider": "retired"}),
            "provider",
            "is disabled",
        ),
        (
            serde_json::json!({"provider": "never-registered"}),
            "provider",
            "is not registered",
        ),
        (
            serde_json::json!({"model": "gpt-5.6-sol"}),
            "provider",
            "no provider",
        ),
        (
            serde_json::json!({"provider": "  "}),
            "provider",
            "non-empty",
        ),
        (
            serde_json::json!({"provider": "plat-openai", "model": "  "}),
            "model",
            "non-empty",
        ),
        (
            serde_json::json!({"provider": "plat-openai", "model": "gpt$(id)"}),
            "model",
            "model",
        ),
    ];
    for (surface, path, mut payload) in [
        (
            "registered",
            "/api/playbooks/survey/launch",
            serde_json::json!({
                "params": {"topic": "attention sinks", "depth": "deep"},
                "max_cost": 3.5,
                "max_time": "30m",
                "schema_digest": digest,
            }),
        ),
        (
            "draft",
            "/api/playbook-drafts/studio/launch",
            serde_json::json!({
                "params": {"topic": "attention", "depth": "deep"},
                "max_cost": 2.5,
                "max_time": "30m",
                "schema_digest": saved.1["schema_digest"],
            }),
        ),
    ] {
        for (pick, field, expected) in &picks {
            for (k, v) in pick.as_object().expect("object") {
                payload[k] = v.clone();
            }
            let (status, refused) = post_admin(&app, path, payload.clone()).await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{surface} {pick}: {refused}"
            );
            let fields = refused["fields"].as_array().expect("field list");
            assert!(
                fields.iter().any(|f| f["field"] == *field
                    && f["message"].as_str().unwrap_or_default().contains(expected)),
                "{surface} {pick}: {refused}"
            );
            for k in pick.as_object().expect("object").keys() {
                payload.as_object_mut().expect("object").remove(k);
            }
        }
    }
    let issues =
        crate::issues::store::list_issues_filtered(db.pool(), &IssueQuery::default()).await?;
    assert!(issues.is_empty(), "a refused launch adopts nothing");
    Ok(())
}

/// A model name is one argv word in the loop pod's `/bin/sh -c` wrapper, so the launch refuses
/// anything a shell would read as syntax rather than as a model.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_launch_refuses_a_model_name_a_shell_would_read(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    seed_provider(
        db.pool(),
        "plat-openai",
        crate::playbooks::providers::ProviderKind::OpenAi,
        true,
    )
    .await;
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);

    for model in ["gpt-5.6-sol; touch /tmp/pwned", "gpt$(id)", "gpt luna"] {
        let (status, refused) = post_admin(
            &app,
            "/api/scenarios",
            serde_json::json!({
                "title": "t",
                "body": "b",
                "affected_repos": ["owner/repo"],
                "justification": "j",
                "provider": "plat-openai",
                "model": model,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{model:?}");
        assert!(
            refused["error"]
                .as_str()
                .unwrap_or_default()
                .contains("model name"),
            "{model:?}: {refused}"
        );
    }
    let issues =
        crate::issues::store::list_issues_filtered(db.pool(), &IssueQuery::default()).await?;
    assert!(issues.is_empty(), "a refused launch adopts nothing");
    Ok(())
}

/// A provider work still names cannot be deregistered: the pin would outlive the registration and
/// every dispatch reading it would fail with no API left to clear it. Disabling is the retirement.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn deregistering_a_pinned_provider_is_refused(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    seed_provider(
        db.pool(),
        "plat-openai",
        crate::playbooks::providers::ProviderKind::OpenAi,
        true,
    )
    .await;
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    let (status, ack) = post_admin(
        &app,
        "/api/scenarios",
        serde_json::json!({
            "title": "t",
            "body": "b",
            "affected_repos": ["owner/repo"],
            "justification": "j",
            "provider": "plat-openai",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{ack}");
    let key = ack["key"].as_str().expect("key").to_string();

    let res = app
        .clone()
        .oneshot(
            HttpRequest::delete("/api/providers/plat-openai")
                .header("x-auth-request-user", "wren")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let v = json_of(res).await?;
    assert!(
        v["error"].as_str().unwrap_or_default().contains(&key),
        "the refusal names what holds it: {v}"
    );
    assert!(
        crate::playbooks::providers::get(db.pool(), "plat-openai")
            .await?
            .is_some()
    );

    crate::issues::store::set_agent_dispatch(db.pool(), &key, None, None).await?;
    let res = app
        .oneshot(
            HttpRequest::delete("/api/providers/plat-openai")
                .header("x-auth-request-user", "wren")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert!(
        crate::playbooks::providers::get(db.pool(), "plat-openai")
            .await?
            .is_none()
    );
    Ok(())
}

/// An adoption that picks nothing leaves both columns null, which is what keeps a deployment with
/// no registry dispatching exactly as it did before one existed.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn adopt_scenario_without_a_provider_pins_nothing(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    let res = app
        .oneshot(admin_json(
            "POST",
            "/api/scenarios",
            serde_json::json!({
                "title": "t",
                "body": "b",
                "affected_repos": ["owner/repo"],
                "justification": "j",
            }),
        )?)
        .await?;
    assert_eq!(res.status(), StatusCode::CREATED);
    let v = json_of(res).await?;
    assert!(v["provider"].is_null());
    assert!(v["model"].is_null());
    let key = v["key"].as_str().expect("key").to_string();
    let issue = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .expect("issue row");
    assert!(issue.agent_provider.is_none());
    assert!(issue.agent_model.is_none());
    Ok(())
}

/// The ways a launch's provider choice is wrong: a provider nobody may pick, one that was never
/// registered, a model with no provider to serve it, and a field sent blank. Each is refused at
/// the launch, and nothing is adopted.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_launch_refuses_a_provider_it_may_not_pick(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    seed_provider(
        db.pool(),
        "retired",
        crate::playbooks::providers::ProviderKind::Anthropic,
        false,
    )
    .await;
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);

    for (pick, expected) in [
        (serde_json::json!({"provider": "retired"}), "is disabled"),
        (
            serde_json::json!({"provider": "never-registered"}),
            "is not registered",
        ),
        (serde_json::json!({"model": "gpt-5.6-sol"}), "no provider"),
        (serde_json::json!({"provider": "  "}), "non-empty"),
        (
            serde_json::json!({"provider": "retired", "model": "  "}),
            "non-empty",
        ),
    ] {
        let mut payload = serde_json::json!({
            "title": "t",
            "body": "b",
            "affected_repos": ["owner/repo"],
            "justification": "j",
        });
        for (k, v) in pick.as_object().expect("object") {
            payload[k] = v.clone();
        }
        let res = app
            .clone()
            .oneshot(admin_json("POST", "/api/scenarios", payload.clone())?)
            .await?;
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY, "{payload}");
        let v = json_of(res).await?;
        assert!(
            v["error"].as_str().unwrap_or_default().contains(expected),
            "{payload}: {v}"
        );
    }
    let issues =
        crate::issues::store::list_issues_filtered(db.pool(), &IssueQuery::default()).await?;
    assert!(issues.is_empty(), "a refused launch adopts nothing");
    Ok(())
}

/// A custom provider is the operator's own service: it needs the endpoint it is reached at, the
/// API it speaks, and a default model (its kind has none), and every other kind refuses an
/// endpoint because it is reached at its service's own address.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_custom_provider_needs_an_endpoint_a_protocol_and_a_default_model(
    pool: PgPool,
) -> Result<()> {
    let (db, _d) = db_with(pool);
    seed_secret(
        db.pool(),
        "sec-1",
        "vllm-key",
        crate::secrets::SecretKind::InferenceApiKey,
    )
    .await;
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);

    for (body, expected) in [
        (
            serde_json::json!({"id": "onprem", "display_name": "vLLM", "kind": "custom",
                "default_model": "gpt-oss-120b", "protocol": "responses"}),
            "needs an endpoint",
        ),
        (
            serde_json::json!({"id": "onprem", "display_name": "vLLM", "kind": "custom",
                "default_model": "gpt-oss-120b", "endpoint": "http://vllm.internal:8000/v1"}),
            "needs a protocol",
        ),
        (
            serde_json::json!({"id": "onprem", "display_name": "vLLM", "kind": "custom",
                "endpoint": "http://vllm.internal:8000/v1", "protocol": "responses"}),
            "default_model",
        ),
        (
            serde_json::json!({"id": "onprem", "display_name": "vLLM", "kind": "custom",
                "default_model": "gpt-oss-120b", "protocol": "responses",
                "endpoint": "vllm.internal:8000/v1"}),
            "endpoint",
        ),
        (
            serde_json::json!({"id": "plat-openai", "display_name": "OpenAI", "kind": "openai",
                "endpoint": "https://proxy.internal/v1", "protocol": "chat_completions"}),
            "takes no endpoint",
        ),
    ] {
        let (status, refused) = post_admin(&app, "/api/providers", body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
        assert!(
            refused["error"]
                .as_str()
                .unwrap_or_default()
                .contains(expected),
            "{expected}: {refused}"
        );
    }

    let (status, created) = post_admin(
        &app,
        "/api/providers",
        serde_json::json!({
            "id": "onprem", "display_name": "vLLM", "kind": "custom",
            "default_model": "gpt-oss-120b", "protocol": "responses",
            "endpoint": "http://vllm.internal:8000/v1/", "secret_name": "vllm-key",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["endpoint"], "http://vllm.internal:8000/v1");
    assert_eq!(created["protocol"], "responses");
    assert_eq!(created["secret_name"], "vllm-key");
    assert_eq!(created["models"], serde_json::json!([]));

    let (status, picker) = get_json_object(&app, "/api/config/providers").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        picker["providers"][0]["endpoint"],
        "http://vllm.internal:8000/v1"
    );
    assert_eq!(picker["providers"][0]["protocol"], "responses");
    Ok(())
}

/// A registration may name the agent CLI its models run under, from the harnesses that speak to
/// its service: a chat-completions endpoint runs opencode unless it names pi, and never codex.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_provider_names_its_harness_from_those_that_speak_to_it(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    let custom = |id: &str, harness: Option<&str>| {
        let mut body = serde_json::json!({
            "id": id, "display_name": id, "kind": "custom", "default_model": "qwen-3-8-27b",
            "protocol": "chat_completions",
            "endpoint": "https://inference.example.com/llm/qwen-3-8-27b/v1",
        });
        if let Some(h) = harness {
            body["harness"] = serde_json::json!(h);
        }
        body
    };

    for (body, expected) in [
        (custom("bay", Some("codex")), "opencode, pi"),
        (custom("bay", Some("gemini")), "not a harness"),
        (
            serde_json::json!({"id": "plat-vertex", "display_name": "Vertex", "kind": "vertex",
                "harness": "pi"}),
            "claude, hermes",
        ),
    ] {
        let (status, refused) = post_admin(&app, "/api/providers", body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
        assert!(
            refused["error"]
                .as_str()
                .unwrap_or_default()
                .contains(expected),
            "{expected}: {refused}"
        );
    }

    let (status, created) = post_admin(&app, "/api/providers", custom("bay", None)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["harness"], "opencode");
    assert_eq!(created["harness_override"], serde_json::Value::Null);

    let (status, created) = post_admin(&app, "/api/providers", custom("bay-pi", Some("pi"))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["harness"], "pi");
    assert_eq!(created["harness_override"], "pi");

    let (status, picker) = get_json_object(&app, "/api/config/providers").await;
    assert_eq!(status, StatusCode::OK);
    let harnesses: Vec<(&str, &str)> = picker["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .map(|p| {
            (
                p["id"].as_str().unwrap_or_default(),
                p["harness"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(harnesses, vec![("bay", "opencode"), ("bay-pi", "pi")]);
    Ok(())
}

/// An inference key is read back as a credentials map at every dispatch, so a value that would
/// not parse then is refused at registration and at rotation instead.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn an_inference_key_value_has_to_be_a_key_or_a_credentials_map(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    for (value, expected) in [
        (r#"{"openai_api_key": "sk"}"#, "environment variable name"),
        (r#"{"OPENAI_API_KEY": ""}"#, "empty"),
        (r#"{}"#, "no variable"),
    ] {
        let (status, refused) = post_admin(
            &app,
            "/api/secrets",
            serde_json::json!({"name": "openai-key", "kind": "inference_api_key", "value": value}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{value}: {refused}"
        );
        assert!(
            refused["error"]
                .as_str()
                .unwrap_or_default()
                .contains(expected),
            "{expected}: {refused}"
        );
    }
    Ok(())
}

fn watch_body(query: &str) -> serde_json::Value {
    serde_json::json!({
        "playbook": "survey",
        "tracker": "jira",
        "query": query,
        "key_param": "topic",
        "params": {"depth": "deep"},
        "max_cost": 3.5,
        "max_time": "30m",
    })
}

fn jira_at(base: &str) -> crate::launches::jira::JiraConfig {
    crate::launches::jira::JiraConfig::from_parts(
        Some(base.to_string()),
        Some("watch@example.com".to_string()),
        Some("token".to_string()),
    )
    .expect("jira config")
}

/// A one-connection Jira search responder: any request gets the canned hit list back.
async fn spawn_mock_jira_search(keys: &[&str]) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock jira");
    let addr = listener.local_addr().expect("mock jira addr");
    let issues: Vec<serde_json::Value> = keys
        .iter()
        .map(|k| serde_json::json!({"key": k, "fields": {"updated": "2026-08-26T12:00:00.000+0000"}}))
        .collect();
    let payload = serde_json::json!({"issues": issues}).to_string();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = vec![0u8; 8192];
        let _ = socket.read(&mut buf).await;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            payload.len(),
            payload
        );
        let _ = socket.write_all(resp.as_bytes()).await;
        let _ = socket.flush().await;
    });
    format!("http://{addr}")
}

/// The watch surface round-trips: create, read back, edit, toggle, hits, delete. A created watch
/// starts at its creation time unless the save names an earlier bound.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_watch_round_trips_through_its_crud_surface(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins_and_jira(
        db.clone(),
        vec!["wren".to_string()],
        Some(jira_at("http://127.0.0.1:1")),
    );
    let digest = register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    let before = jiff::Timestamp::now();
    let (status, created) = send_json(
        &app,
        HttpRequest::post("/api/watches"),
        watch_body("project = ACME AND labels = backport-request"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("id").to_string();
    assert_eq!(created["playbook"], "survey");
    assert_eq!(created["tracker"], "jira");
    assert_eq!(created["key_param"], "topic");
    assert_eq!(created["schema_digest"], digest.as_str());
    assert_eq!(created["enabled"], true);
    assert_eq!(created["params"], serde_json::json!({"depth": "deep"}));
    assert_eq!(created["owner_principal"], "user:wren");
    let watermark: jiff::Timestamp = created["watermark"].as_str().expect("watermark").parse()?;
    assert!(watermark >= before, "starts at creation");

    let (status, listed) = get_json_value(&app, "/api/watches").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.as_array().expect("array").len(), 1);
    let (status, trackers) = get_json_value(&app, "/api/watches/trackers").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(trackers["trackers"], serde_json::json!(["jira"]));

    let mut edit = watch_body("labels = backport-request");
    edit["since"] = serde_json::Value::String("2026-08-01T00:00:00Z".to_string());
    let (status, updated) =
        send_json(&app, HttpRequest::put(format!("/api/watches/{id}")), edit).await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["query"], "labels = backport-request");
    assert_eq!(updated["watermark"], "2026-08-01T00:00:00Z");

    let (status, toggled) = send_json(
        &app,
        HttpRequest::post(format!("/api/watches/{id}/enabled")),
        serde_json::json!({"enabled": false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{toggled}");
    assert_eq!(toggled["enabled"], false);

    let (status, hits) = get_json_value(&app, &format!("/api/watches/{id}/hits")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hits, serde_json::json!([]));
    let res = app
        .clone()
        .oneshot(
            HttpRequest::delete(format!("/api/watches/{id}/hits/ACME-1"))
                .header("x-auth-request-user", "wren")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::NOT_FOUND, "never launched");

    let res = app
        .clone()
        .oneshot(
            HttpRequest::delete(format!("/api/watches/{id}"))
                .header("x-auth-request-user", "wren")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let (status, _) = get_json_value(&app, &format!("/api/watches/{id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

/// A watch is validated like an immediate launch plus its trigger: an unconfigured tracker, a
/// query the tracker refuses, a key param the pack does not declare, a key param also fixed in
/// params, and a bad bound all come back addressed to their field.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_watch_is_refused_field_by_field(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins_and_jira(
        db.clone(),
        vec!["wren".to_string()],
        Some(jira_at("http://127.0.0.1:1")),
    );
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;

    let field_of = |refused: &serde_json::Value| {
        refused["fields"][0]["field"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };

    let mut body = watch_body("labels = x");
    body["tracker"] = serde_json::json!("github");
    let (status, refused) = send_json(&app, HttpRequest::post("/api/watches"), body).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(field_of(&refused), "tracker");

    let (status, refused) = send_json(
        &app,
        HttpRequest::post("/api/watches"),
        watch_body("labels = x ORDER BY created"),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(field_of(&refused), "query");

    let mut body = watch_body("labels = x");
    body["key_param"] = serde_json::json!("nope");
    let (status, refused) = send_json(&app, HttpRequest::post("/api/watches"), body).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(
        field_of(&refused),
        "key_param",
        "an undeclared key param is refused by name"
    );

    let mut body = watch_body("labels = x");
    body["params"]["topic"] = serde_json::json!("fixed");
    let (status, refused) = send_json(&app, HttpRequest::post("/api/watches"), body).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(field_of(&refused), "key_param");

    let mut body = watch_body("labels = x");
    body["since"] = serde_json::json!("yesterday");
    let (status, refused) = send_json(&app, HttpRequest::post("/api/watches"), body).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(field_of(&refused), "since");

    let mut body = watch_body("labels = x");
    body["max_time"] = serde_json::json!("forever");
    let (status, refused) = send_json(&app, HttpRequest::post("/api/watches"), body).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(field_of(&refused), "max_time");

    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM playbook_watches")
        .fetch_one(db.pool())
        .await?;
    assert_eq!(stored, 0, "a refused watch stores nothing");

    // No Jira credentials at all: the tracker is refused, and the list of trackers is empty.
    let bare = app_with_admins(db.clone(), vec!["wren".to_string()]);
    let (status, refused) = send_json(
        &bare,
        HttpRequest::post("/api/watches"),
        watch_body("labels = x"),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(field_of(&refused), "tracker");
    let (_, trackers) = get_json_value(&bare, "/api/watches/trackers").await;
    assert_eq!(trackers["trackers"], serde_json::json!([]));
    Ok(())
}

/// The preview asks the real tracker, so a creator sees what the query matches before saving.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_watch_preview_counts_what_the_query_matches(pool: PgPool) -> Result<()> {
    let (db, _dir) = db_with(pool);
    let base = spawn_mock_jira_search(&["ACME-1", "ACME-2", "ACME-3"]).await;
    let app = app_with_admins_and_jira(db, vec!["wren".to_string()], Some(jira_at(&base)));
    let (status, preview) = send_json(
        &app,
        HttpRequest::post("/api/watches/preview"),
        serde_json::json!({"tracker": "jira", "query": "labels = backport-request"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["matched"], 3);
    assert_eq!(
        preview["sample"],
        serde_json::json!(["ACME-1", "ACME-2", "ACME-3"])
    );
    Ok(())
}

/// A watch is a standing launch: creating one on a playbook the caller may not launch is refused.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_watch_on_a_playbook_the_caller_may_not_read_is_not_found(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let app = app_with_admins_and_jira(
        db.clone(),
        vec!["wren".to_string()],
        Some(jira_at("http://127.0.0.1:1")),
    );
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;
    let (status, body) =
        send_as(&app, "POST", "/api/watches", "mallory", watch_body("x = y")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let watches: i64 = sqlx::query_scalar("SELECT count(*) FROM playbook_watches")
        .fetch_one(db.pool())
        .await?;
    assert_eq!(watches, 0);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The image catalog: `GET /api/images` and the admin-only refresh.
// ---------------------------------------------------------------------------------------------

fn catalog_image(
    repository: &str,
    digest: &str,
    verified: bool,
) -> crate::images::model::CatalogImage {
    crate::images::model::CatalogImage {
        repository: repository.to_string(),
        digest: digest.to_string(),
        tags: vec!["latest".to_string()],
        arches: vec!["amd64".to_string()],
        created_at: Some("2026-09-12T06:40:00Z".to_string()),
        capabilities: verified.then(|| crucible_capability::CapabilityDoc {
            features: vec!["base".into(), "go".into()],
            image: "sandbox-go-cc".into(),
            predicates: [("toolchain.go".to_string(), "1.25.11".to_string())]
                .into_iter()
                .collect(),
            schema: crucible_capability::CAPABILITIES_SCHEMA.into(),
        }),
        capability_digest: verified.then(|| "sha256:cap".to_string()),
        intro_digest: None,
        first_seen: "2026-09-12T07:00:00Z".to_string(),
        last_seen: "2026-09-12T07:00:00Z".to_string(),
    }
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn list_images_serves_the_cached_catalog_with_verification_state(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let app = app(db.clone(), Arc::new(Recorder::default()));

    // Nothing configured yet: an empty catalog, not an error.
    let (st, v) = get_json_object(&app, "/api/images").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["images"].as_array().map(Vec::len), Some(0));
    assert_eq!(v["repositories"].as_array().map(Vec::len), Some(0));

    crate::images::store::upsert_image(
        db.pool(),
        &catalog_image("ghcr.io/acme/sandbox-go-cc", "sha256:aaaa", true),
    )
    .await?;
    crate::images::store::upsert_image(
        db.pool(),
        &catalog_image("ghcr.io/acme/custom", "sha256:bbbb", false),
    )
    .await?;
    crate::images::store::upsert_repository(
        db.pool(),
        &crate::images::model::RepositoryStatus {
            repository: "ghcr.io/acme/custom".into(),
            last_polled: "2026-09-12T07:00:00Z".into(),
            last_ok: Some("2026-09-12T06:00:00Z".into()),
            last_error: Some("502 from ghcr.io".into()),
        },
    )
    .await?;

    let (st, v) = get_json_object(&app, "/api/images").await;
    assert_eq!(st, StatusCode::OK);
    let images = v["images"].as_array().expect("images");
    assert_eq!(images.len(), 2);
    let custom = &images[0];
    assert_eq!(custom["name"], "custom");
    assert_eq!(custom["verified"], false);
    assert!(custom["capabilities"].is_null());
    let go = &images[1];
    assert_eq!(go["name"], "sandbox-go-cc");
    assert_eq!(go["verified"], true);
    assert_eq!(go["capabilities"]["predicates"]["toolchain.go"], "1.25.11");
    assert_eq!(go["tags"][0], "latest");
    let repos = v["repositories"].as_array().expect("repositories");
    assert_eq!(repos[0]["last_error"], "502 from ghcr.io");
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn refresh_images_is_admin_only_and_wakes_the_watcher(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    let state = ApiState {
        roles: crate::identity::auth::Roles::new(vec!["alice".into()], vec![], vec![]),
        ..ApiState::test(db, Arc::new(Recorder::default()))
    };
    let refresh = state.images_refresh();
    let app = router(state);

    let res = app
        .clone()
        .oneshot(
            HttpRequest::post("/api/images/refresh")
                .header("x-auth-request-user", "bob")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    let res = app
        .clone()
        .oneshot(
            HttpRequest::post("/api/images/refresh")
                .header("x-auth-request-user", "alice")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    tokio::time::timeout(std::time::Duration::from_secs(1), refresh.notified())
        .await
        .expect("the refresh signal fired");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The capability preflight at launch: the pack's [agent.requires] against the image catalog.
// ---------------------------------------------------------------------------------------------

const PREFLIGHT_MANIFEST: &str = concat!(
    "[repo]\npath = \".\"\n\n",
    "[agent]\nbackend = \"local\"\nharness = \"claude\"\n",
    "sandbox_image = \"ghcr.io/acme/sandbox-go-cc:latest\"\n\n",
    "[agent.requires]\n\"toolchain.go\" = \">=1.25\"\n\"toolchain.cuda\" = \">=13\"\n\n",
    "[workflow]\ntype = \"playbook\"\nfile = \"workflow.star\"\n",
);

const SATISFIED_MANIFEST: &str = concat!(
    "[repo]\npath = \".\"\n\n",
    "[agent]\nbackend = \"local\"\nharness = \"claude\"\n",
    "sandbox_image = \"ghcr.io/acme/sandbox-go-cc:latest\"\n\n",
    "[agent.requires]\n\"toolchain.go\" = \">=1.25\"\n\n",
    "[workflow]\ntype = \"playbook\"\nfile = \"workflow.star\"\n",
);

const OVERRIDE_MANIFEST: &str = concat!(
    "[repo]\npath = \".\"\n\n",
    "[agent]\nbackend = \"local\"\nsandbox_image = \"quay.io/acme/custom:dev\"\n",
    "allow_unverified_image = true\n\n",
    "[workflow]\ntype = \"playbook\"\nfile = \"workflow.star\"\n",
);

const GO_CC_DIGEST: &str =
    "sha256:1111111111111111111111111111111111111111111111111111111111111111";

async fn seed_go_cc_image(db: &Db) -> Result<()> {
    crate::images::store::upsert_image(
        db.pool(),
        &crate::images::model::CatalogImage {
            repository: "ghcr.io/acme/sandbox-go-cc".into(),
            digest: GO_CC_DIGEST.into(),
            tags: vec!["latest".into()],
            arches: vec!["amd64".into()],
            created_at: None,
            capabilities: Some(crucible_capability::CapabilityDoc {
                features: vec!["base".into(), "go".into(), "claude-code".into()],
                image: "sandbox-go-cc".into(),
                predicates: [
                    ("toolchain.go".to_string(), "1.25.11".to_string()),
                    ("agent.claude-code".to_string(), "2.1.270".to_string()),
                ]
                .into_iter()
                .collect(),
                schema: crucible_capability::CAPABILITIES_SCHEMA.into(),
            }),
            capability_digest: Some("sha256:cap".into()),
            intro_digest: None,
            first_seen: "2026-09-12T07:00:00Z".into(),
            last_seen: "2026-09-12T07:00:00Z".into(),
        },
    )
    .await?;
    Ok(())
}

fn launch_body(digest: &str) -> serde_json::Value {
    serde_json::json!({
        "params": {"topic": "attention sinks", "depth": "deep"},
        "max_cost": 3.0,
        "max_time": "30m",
        "schema_digest": digest,
    })
}

/// The whole path a workflow owner sees: the registry names the failing predicates on the pack,
/// and a launch is refused before any pod with the same predicates, on the image field.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_launch_whose_image_fails_its_requires_is_refused_naming_the_predicates(
    pool: PgPool,
) -> Result<()> {
    let (db, dir) = db_with(pool);
    seed_go_cc_image(&db).await?;
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    let digest = register_survey_with(&app, dir.path(), LAUNCH_WORKFLOW, PREFLIGHT_MANIFEST).await;

    let (st, list) = get_json(&app, "/api/playbooks").await;
    assert_eq!(st, StatusCode::OK);
    let dispatch = &list[0]["dispatch"];
    assert_eq!(dispatch["requires"]["toolchain.cuda"], ">=13");
    assert_eq!(dispatch["harness"], "claude");
    assert_eq!(dispatch["image"]["checked"], true);
    assert_eq!(dispatch["image"]["catalogued"], true);
    assert_eq!(dispatch["image"]["digest"], GO_CC_DIGEST);
    let refusals = dispatch["image"]["refusals"].as_array().expect("refusals");
    assert_eq!(refusals.len(), 1, "{dispatch}");
    assert!(
        refusals[0]
            .as_str()
            .unwrap_or_default()
            .contains("lacks toolchain.cuda"),
        "{dispatch}"
    );

    let (status, body) = post_launch(&app, "survey", launch_body(&digest)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["fields"][0]["field"], "sandbox_image");
    assert!(
        body["fields"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("lacks toolchain.cuda"),
        "{body}"
    );
    let launches: i64 = sqlx::query_scalar("SELECT count(*) FROM playbook_launches")
        .fetch_one(db.pool())
        .await?;
    assert_eq!(launches, 0, "a refused launch writes no launch row");
    Ok(())
}

/// A satisfied pack launches, and the ack carries the digest and capability document the
/// preflight matched, which is what the run's provenance records.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_launch_whose_image_satisfies_its_requires_carries_the_resolved_digest(
    pool: PgPool,
) -> Result<()> {
    let (db, dir) = db_with(pool);
    seed_go_cc_image(&db).await?;
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    let digest = register_survey_with(&app, dir.path(), LAUNCH_WORKFLOW, SATISFIED_MANIFEST).await;

    let (status, ack) = post_launch(&app, "survey", launch_body(&digest)).await;
    assert_eq!(status, StatusCode::CREATED, "{ack}");
    assert_eq!(ack["image"]["digest"], GO_CC_DIGEST);
    assert_eq!(ack["image"]["capability_digest"], "sha256:cap");
    assert_eq!(ack["image"]["verified"], true);
    assert_eq!(ack["image"]["overridden"], false);
    assert!(
        ack["image"]["refusals"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    Ok(())
}

/// An image the catalog cannot vouch for launches only on the manifest's explicit override, and
/// the ack says so.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn an_uncatalogued_image_launches_only_on_the_override(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    seed_go_cc_image(&db).await?;
    let app = app_with_admins(db.clone(), vec!["wren".to_string()]);
    let digest = register_survey_with(&app, dir.path(), LAUNCH_WORKFLOW, OVERRIDE_MANIFEST).await;

    let (status, ack) = post_launch(&app, "survey", launch_body(&digest)).await;
    assert_eq!(status, StatusCode::CREATED, "{ack}");
    assert_eq!(ack["image"]["catalogued"], false);
    assert_eq!(ack["image"]["overridden"], true);
    assert!(
        ack["image"]["warnings"][0]
            .as_str()
            .unwrap_or_default()
            .contains("allow_unverified_image"),
        "{ack}"
    );

    // The same pack without the override is refused.
    let without = OVERRIDE_MANIFEST.replace("allow_unverified_image = true\n", "");
    let digest = register_survey_with(&app, dir.path(), LAUNCH_WORKFLOW, &without).await;
    let (status, body) = post_launch(&app, "survey", launch_body(&digest)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["fields"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("is not in the image catalog"),
        "{body}"
    );
    Ok(())
}

/// The picker's view of the catalog for one pack: the slim image ranks first and is the default,
/// the excluded image names the predicate that excluded it.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn rank_images_orders_compatible_images_and_names_exclusions(pool: PgPool) -> Result<()> {
    let (db, _d) = db_with(pool);
    seed_go_cc_image(&db).await?;
    crate::images::store::upsert_image(
        db.pool(),
        &crate::images::model::CatalogImage {
            repository: "ghcr.io/acme/sandbox-rust-cc".into(),
            digest: "sha256:2222222222222222222222222222222222222222222222222222222222222222"
                .into(),
            tags: vec!["latest".into()],
            arches: vec!["amd64".into()],
            created_at: None,
            capabilities: Some(crucible_capability::CapabilityDoc {
                features: vec!["base".into(), "rust".into(), "claude-code".into()],
                image: "sandbox-rust-cc".into(),
                predicates: [
                    ("toolchain.rust".to_string(), "1.90.0".to_string()),
                    ("agent.claude-code".to_string(), "2.1.270".to_string()),
                ]
                .into_iter()
                .collect(),
                schema: crucible_capability::CAPABILITIES_SCHEMA.into(),
            }),
            capability_digest: Some("sha256:cap2".into()),
            intro_digest: None,
            first_seen: "2026-09-12T07:00:00Z".into(),
            last_seen: "2026-09-12T07:00:00Z".into(),
        },
    )
    .await?;
    let app = app(db, Arc::new(Recorder::default()));
    let res = app
        .clone()
        .oneshot(
            HttpRequest::post("/api/images/rank")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"requires": {"toolchain.go": ">=1.25"}, "harness": "claude"})
                        .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(v["compatible"].as_array().map(Vec::len), Some(1));
    assert_eq!(v["compatible"][0]["image"]["name"], "sandbox-go-cc");
    assert_eq!(v["compatible"][0]["default"], true);
    assert_eq!(v["compatible"][0]["surplus"], 0);
    assert_eq!(v["excluded"][0]["image"]["name"], "sandbox-rust-cc");
    assert_eq!(
        v["excluded"][0]["unsatisfied"][0]["predicate"],
        "toolchain.go"
    );
    assert_eq!(
        v["excluded"][0]["unsatisfied"][0]["found"],
        serde_json::Value::Null
    );

    let res = app
        .oneshot(
            HttpRequest::post("/api/images/rank")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"harness": "gemini"}).to_string(),
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

async fn send_json_as(
    app: &Router,
    req: axum::http::request::Builder,
    user: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(
            req.header(header::CONTENT_TYPE, "application/json")
                .header("x-auth-request-user", user)
                .body(Body::from(body.to_string()))
                .expect("req"),
        )
        .await
        .expect("resp");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// A standing launch is owned to the principal the request names, an edit by someone else keeps
/// that owner, and the owner's team roles decide who may create against it (RFC-0003 C-OWNERSHIP,
/// C-DECISION).
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_schedule_is_owned_to_the_named_principal_and_kept_on_edit(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool.clone());
    let app = app_with_admins(db.clone(), vec!["wren".to_string(), "root".to_string()]);
    register_survey(&app, dir.path(), LAUNCH_WORKFLOW).await;
    let (status, team) = send_json(
        &app,
        HttpRequest::post("/api/teams"),
        serde_json::json!({"slug": "llm-d", "display_name": "LLM-D"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{team}");

    let mut body = schedule_body("30 6 * * MON-FRI", "UTC");
    body["owner"] = serde_json::json!("team:other");
    let (status, refused) = send_json(&app, HttpRequest::post("/api/schedules"), body).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");

    let mut body = schedule_body("30 6 * * MON-FRI", "UTC");
    body["owner"] = serde_json::json!("team:llm-d");
    let (status, created) = send_json(&app, HttpRequest::post("/api/schedules"), body).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("id").to_string();
    assert_eq!(created["owner_principal"], "team:llm-d");

    // root administers the platform but is not in the team: the edit keeps the team as owner.
    let mut edit = schedule_body("0 0 * * *", "UTC");
    edit["enabled"] = serde_json::Value::Bool(false);
    let (status, updated) = send_json_as(
        &app,
        HttpRequest::put(format!("/api/schedules/{id}")),
        "root",
        edit,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["owner_principal"], "team:llm-d");
    assert_eq!(updated["enabled"], false);
    let (status, _) = send_json_as(
        &app,
        HttpRequest::delete(format!("/api/schedules/{id}")),
        "root",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // A row that predates ownership is the platform's: a platform administrator still edits it.
    let (status, created) = send_json(
        &app,
        HttpRequest::post("/api/schedules"),
        schedule_body("30 6 * * MON-FRI", "UTC"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("id").to_string();
    sqlx::query("UPDATE playbook_standing_launches SET owner_principal = NULL WHERE id = $1")
        .bind(&id)
        .execute(&pool)
        .await?;
    let (status, _) = send_json_as(
        &app,
        HttpRequest::delete(format!("/api/schedules/{id}")),
        "root",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let trail =
        crucible_controller::authz::store::audit_for(&pool, "standing_launch", &id, 5).await?;
    assert!(
        trail.is_empty(),
        "an allowed delete leaves no denial: {trail:?}"
    );
    Ok(())
}

/// C-READ-SCOPING on the registry: an administrator registers one pack under a team and one under
/// themselves; a team member lists and reads the team's pack only, and every route under the other
/// answers not-found.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn the_registry_lists_only_what_the_caller_may_read(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let repo = playbook_fixture(dir.path(), crate::testing::fixtures::WORKFLOW_TOPIC);
    let app = app_with_admins(db, vec!["wren".to_string()]);
    let (status, team) = post_json(
        &app,
        "/api/teams",
        serde_json::json!({
            "slug": "llm-d",
            "display_name": "LLM-D",
            "members": [
                {"kind": "user", "member": "wren", "role": "owner"},
                {"kind": "user", "member": "bob", "role": "member"},
            ],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{team}");
    for (id, owner) in [("shared", Some("team:llm-d")), ("solo", None)] {
        let mut body: serde_json::Value = serde_json::from_str(&register_body(id, &repo))?;
        if let Some(owner) = owner {
            body["owner"] = serde_json::json!(owner);
        }
        let (status, ack) = post_json(&app, "/api/playbooks", body).await;
        assert_eq!(status, StatusCode::CREATED, "{ack}");
    }
    let ids = |rows: &[serde_json::Value]| -> Vec<String> {
        let mut ids: Vec<String> = rows
            .iter()
            .map(|r| r["id"].as_str().unwrap_or_default().to_string())
            .collect();
        ids.sort();
        ids
    };
    let (status, rows) = get_json_as(&app, "/api/playbooks", "wren").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&rows), vec!["shared", "solo"]);
    let (status, rows) = get_json_as(&app, "/api/playbooks", "bob").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&rows), vec!["shared"]);
    let (status, rows) = get_json_as(&app, "/api/playbooks", "carol").await;
    assert_eq!(status, StatusCode::OK);
    assert!(rows.is_empty());
    let res = app
        .clone()
        .oneshot(HttpRequest::get("/api/playbooks").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&axum::body::to_bytes(res.into_body(), usize::MAX).await?)?;
    assert!(rows.is_empty(), "anonymous reads nothing");

    for uri in ["/api/playbooks/solo", "/api/playbooks/solo/schema"] {
        let res = app
            .clone()
            .oneshot(
                HttpRequest::get(uri)
                    .header("x-auth-request-user", "bob")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{uri}");
    }
    for uri in ["/api/playbooks/shared", "/api/playbooks/shared/schema"] {
        let res = app
            .clone()
            .oneshot(
                HttpRequest::get(uri)
                    .header("x-auth-request-user", "bob")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::OK, "{uri}");
    }
    Ok(())
}

/// C-READ-SCOPING on imports: the proposer reads and drives their own proposal; another operator
/// gets not-found from the get and from every step, and each refused step is audited.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn an_import_is_readable_and_drivable_by_its_owner_alone(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool.clone());
    let repo = import_fixture(dir.path(), IMPORT_WORKFLOW);
    let app = app_with_roles(
        db,
        Arc::new(Recorder::default()),
        vec!["wren".to_string()],
        vec!["alice".to_string(), "bob".to_string()],
    );
    let (status, import) = post_json_as(
        &app,
        "/api/playbooks/imports",
        "alice",
        serde_json::json!({"repo": &repo, "git_ref": "main", "path": "packs/survey"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{import}");
    let id = import["id"].as_str().unwrap_or_default().to_string();
    assert_eq!(import["owner"], "user:alice");

    let get = |user: &str| {
        HttpRequest::get(format!("/api/playbooks/imports/{id}"))
            .header("x-auth-request-user", user)
            .body(Body::empty())
            .expect("req")
    };
    assert_eq!(
        app.clone().oneshot(get("alice")).await?.status(),
        StatusCode::OK
    );
    assert_eq!(
        app.clone().oneshot(get("wren")).await?.status(),
        StatusCode::OK
    );
    assert_eq!(
        app.clone().oneshot(get("bob")).await?.status(),
        StatusCode::NOT_FOUND
    );
    for step in ["compile", "discard"] {
        let (status, body) = post_json_as(
            &app,
            &format!("/api/playbooks/imports/{id}/{step}"),
            "bob",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{step}: {body}");
    }
    let (status, body) = post_json_as(
        &app,
        &format!("/api/playbooks/imports/{id}/draft"),
        "bob",
        serde_json::json!({"id": "stolen", "description": "d"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = post_json_as(
        &app,
        &format!("/api/playbooks/imports/{id}/compile"),
        "alice",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let denied: Vec<(String, String)> = sqlx::query_as(
        "SELECT actor, action FROM authz_audit
          WHERE decision = 'deny' AND resource_type = 'pack_import' AND resource_id = $1
          ORDER BY id",
    )
    .bind(&id)
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        denied,
        vec![
            ("user:bob".to_string(), "pack_import:update".to_string()),
            ("user:bob".to_string(), "pack_import:update".to_string()),
            ("user:bob".to_string(), "pack_import:update".to_string()),
        ]
    );
    Ok(())
}
