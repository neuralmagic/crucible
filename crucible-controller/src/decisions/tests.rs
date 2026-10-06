use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use crucible_contract::decision::{
    ChoiceOption, Label, PickSource, Question, QuestionId, QuestionKind,
};
use crucible_contract::decision_request::{
    Evidence, InputEvidence, OpenRequest, RequestState, RequestStatus, RunEvidence,
};
use sqlx::PgPool;

use crate::Db;
use crate::api::state::ApiState;
use crate::decisions::store::{self, Refused};
use crate::runs::ingest_auth::IngestValidator;
use crate::runs::ingest_drop::IngestState;

struct NoOverrides;

impl crate::daemon::queue::OverrideSink for NoOverrides {
    fn submit(&self, _ov: crate::daemon::queue::Override) {}
}

fn choice(labels: &[&str], multiple: bool) -> Question {
    Question {
        instructions: "launch the job?".into(),
        kind: QuestionKind::Choice {
            options: labels
                .iter()
                .map(|l| ChoiceOption {
                    label: Label::new(*l).unwrap(),
                    description: None,
                })
                .collect(),
            multiple,
        },
        drop: Vec::new(),
    }
}

fn question() -> Question {
    choice(&["approve", "deny"], false)
}

fn request(task: &str, gpus: u64, timeout_secs: u64) -> OpenRequest {
    let evidence = Evidence {
        inputs: BTreeMap::from([(
            "plan".to_string(),
            InputEvidence {
                status: "pass".into(),
                output: Some(serde_json::json!({"gpus": gpus})),
                files: Vec::new(),
            },
        )]),
        run: RunEvidence {
            spent_usd: 0.1,
            elapsed_secs: 5,
            max_cost_usd: 5.0,
            max_time_secs: Some(3600),
        },
        gated: Vec::new(),
        review: None,
        choices: BTreeMap::new(),
    };
    OpenRequest {
        task: task.into(),
        questions: BTreeMap::from([(QuestionId::new("go").unwrap(), question())]),
        evidence_digest: evidence.digest().unwrap(),
        evidence,
        timeout_secs,
    }
}

fn labels(go: &str) -> BTreeMap<QuestionId, Vec<String>> {
    BTreeMap::from([(QuestionId::new("go").unwrap(), vec![go.to_owned()])])
}

/// `go`, plus `checks` (several of lint/unit/e2e) and `nodes` (one or more picked from the
/// plan's node list).
fn multi_request() -> OpenRequest {
    let mut req = request("gate", 8, 3600);
    req.questions.insert(
        QuestionId::new("checks").unwrap(),
        choice(&["lint", "unit", "e2e"], true),
    );
    req.questions.insert(
        QuestionId::new("nodes").unwrap(),
        Question {
            instructions: "which nodes?".into(),
            kind: QuestionKind::Pick {
                source: PickSource {
                    task: "plan".into(),
                    field: "nodes".into(),
                },
                multiple: true,
            },
            drop: Vec::new(),
        },
    );
    req.evidence.choices = BTreeMap::from([(
        QuestionId::new("nodes").unwrap(),
        vec!["a1".to_owned(), "b2".to_owned()],
    )]);
    req.evidence_digest = req.evidence.digest().unwrap();
    req
}

/// A playbook launch by `wren`, its issue, and one running run attributed to her.
async fn seed_run(pool: &PgPool, run_id: &str) {
    let now = crate::clock::now_rfc3339();
    sqlx::query(
        "INSERT INTO issues (key, repo, tier, status, priority, input_kind, title, updated_at) \
         VALUES ('pb#1', 'o/r', 0, 'running', 0, 'playbook', 't', $1) ON CONFLICT DO NOTHING",
    )
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO playbook_launches (key, playbook, params, schema_digest, max_cost, max_time, \
                                        created_by, created_at) \
         VALUES ('pb#1', 'pb', '{}'::jsonb, 'sha256:2', 1.0, '1h', 'wren', $1) ON CONFLICT DO NOTHING",
    )
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO runs (run_id, issue, status) VALUES ($1, 'pb#1', 'running')")
        .bind(run_id)
        .execute(pool)
        .await
        .unwrap();
    crate::runs::store::attribute_run(pool, run_id)
        .await
        .unwrap();
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    auth: (&str, &str),
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(auth.0, auth.1);
    if body.is_some() {
        req = req.header(header::CONTENT_TYPE, "application/json");
    }
    let req = req
        .body(body.map_or_else(Body::empty, |v| Body::from(v.to_string())))
        .unwrap();
    let (status, bytes) = crate::testing::oneshot_bytes(app, req).await;
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

fn as_user(login: &str) -> (&'static str, String) {
    ("x-auth-request-user", login.to_string())
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_reopen_returns_the_first_request_with_its_expiry_and_evidence(pool: PgPool) {
    seed_run(&pool, "r1").await;
    let (first, opened) = store::open(&pool, "r1", Some("pb#1"), &request("gate", 8, 3600))
        .await
        .unwrap();
    assert!(opened);
    let (again, reopened) = store::open(&pool, "r1", Some("pb#1"), &request("gate", 9, 60))
        .await
        .unwrap();
    assert!(!reopened);
    assert_eq!(again.id, first.id);
    assert_eq!(again.expires_at, first.expires_at);
    assert_eq!(
        again.evidence, first.evidence,
        "a reopen never replaces the evidence"
    );
    let (other, opened) = store::open(&pool, "r1", Some("pb#1"), &request("gate-2", 8, 3600))
        .await
        .unwrap();
    assert!(opened);
    assert_ne!(other.id, first.id, "one request per run and task");
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn the_first_answer_wins_and_a_repeat_by_the_same_person_is_acknowledged(pool: PgPool) {
    seed_run(&pool, "r1").await;
    let req = request("gate", 8, 3600);
    let (d, _) = store::open(&pool, "r1", None, &req).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let digest = &req.evidence_digest;
    let first = store::answer(
        &mut conn,
        &d.id,
        &labels("approve"),
        digest,
        "user:wren",
        Some("go"),
    )
    .await
    .unwrap();
    assert_eq!(first, Ok(true));
    let repeat = store::answer(
        &mut conn,
        &d.id,
        &labels("approve"),
        digest,
        "user:wren",
        None,
    )
    .await
    .unwrap();
    assert_eq!(repeat, Ok(false));
    let other = store::answer(&mut conn, &d.id, &labels("deny"), digest, "user:wren", None)
        .await
        .unwrap();
    assert_eq!(other, Err(Refused::Closed("answered")));
    let someone_else = store::answer(
        &mut conn,
        &d.id,
        &labels("approve"),
        digest,
        "user:kai",
        None,
    )
    .await
    .unwrap();
    assert_eq!(someone_else, Err(Refused::Closed("answered")));
    let read = store::get(&pool, &d.id).await.unwrap().unwrap();
    let RequestStatus::Answered { answer } = read.status else {
        panic!("{:?}", read.status);
    };
    assert_eq!(answer.decided_by, "user:wren");
    assert_eq!(answer.note.as_deref(), Some("go"));
    assert_eq!(answer.labels, labels("approve"));
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn an_answer_naming_other_evidence_is_refused(pool: PgPool) {
    seed_run(&pool, "r1").await;
    let (d, _) = store::open(&pool, "r1", None, &request("gate", 8, 3600))
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let stale = request("gate", 9, 3600).evidence_digest;
    let refused = store::answer(
        &mut conn,
        &d.id,
        &labels("approve"),
        &stale,
        "user:wren",
        None,
    )
    .await
    .unwrap();
    assert_eq!(refused, Err(Refused::StaleEvidence));
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn an_expired_request_reads_expired_and_refuses_answers(pool: PgPool) {
    seed_run(&pool, "r1").await;
    let req = request("gate", 8, 1);
    let (d, _) = store::open(&pool, "r1", None, &req).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let read = store::get(&pool, &d.id).await.unwrap().unwrap();
    assert_eq!(read.status, RequestStatus::Expired);
    assert!(store::list_open(&pool).await.unwrap().is_empty());
    let mut conn = pool.acquire().await.unwrap();
    let refused = store::answer(
        &mut conn,
        &d.id,
        &labels("approve"),
        &req.evidence_digest,
        "user:wren",
        None,
    )
    .await
    .unwrap();
    assert_eq!(refused, Err(Refused::Closed("expired")));
    assert!(store::withdraw_run(&pool, "r1").await.unwrap().is_empty());
    assert_eq!(
        store::get(&pool, &d.id).await.unwrap().unwrap().status,
        RequestStatus::Expired,
        "a run ending after expiry leaves the request expired"
    );
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_run_ending_withdraws_its_open_requests_and_revokes_its_local_token(pool: PgPool) {
    seed_run(&pool, "r1").await;
    let db = Db::new(pool.clone());
    let (d, _) = store::open(&pool, "r1", None, &request("gate", 8, 3600))
        .await
        .unwrap();
    crate::runs::ingest_auth::mint_local_token(&pool, "r1")
        .await
        .unwrap();
    crate::decisions::run_ended(&db, "r1").await;
    assert_eq!(
        store::get(&pool, &d.id).await.unwrap().unwrap().status,
        RequestStatus::Withdrawn
    );
    let tokens: i64 = sqlx::query_scalar("SELECT count(*) FROM local_run_tokens")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tokens, 0);
}

fn ingest(pool: &PgPool) -> axum::Router {
    crate::decisions::ingest_routes().with_state(IngestState {
        db: Db::new(pool.clone()),
        validator: Arc::new(IngestValidator::local_only(pool.clone())),
    })
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_local_run_opens_and_polls_its_request_with_its_minted_token(pool: PgPool) {
    seed_run(&pool, "r1").await;
    seed_run(&pool, "r2").await;
    let (pod, token) = crate::runs::ingest_auth::mint_local_token(&pool, "r1")
        .await
        .unwrap();
    let app = ingest(&pool);
    let bearer = format!("Bearer {token}");
    let auth = ("authorization", bearer.as_str());
    let body = serde_json::to_value(request("gate", 8, 3600)).unwrap();

    let (status, opened) = send(
        &app,
        "POST",
        &format!("/api/pods/{pod}/decisions"),
        auth,
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{opened}");
    let opened: RequestState = serde_json::from_value(opened).unwrap();
    assert_eq!(opened.status, RequestStatus::Open);
    let (_, reopened) = send(
        &app,
        "POST",
        &format!("/api/pods/{pod}/decisions"),
        auth,
        Some(body),
    )
    .await;
    assert_eq!(reopened["id"], opened.id.as_str());
    let (status, polled) = send(
        &app,
        "GET",
        &format!("/api/pods/{pod}/decisions/{}", opened.id),
        auth,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(polled["state"], "open");

    let (status, _) = send(
        &app,
        "GET",
        &format!("/api/pods/{pod}/decisions/{}", opened.id),
        ("authorization", "Bearer nope"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (other_pod, other_token) = crate::runs::ingest_auth::mint_local_token(&pool, "r2")
        .await
        .unwrap();
    let other = format!("Bearer {other_token}");
    let (status, _) = send(
        &app,
        "GET",
        &format!("/api/pods/{other_pod}/decisions/{}", opened.id),
        ("authorization", other.as_str()),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a run reads only its own requests"
    );
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_request_whose_digest_does_not_match_its_evidence_is_refused(pool: PgPool) {
    seed_run(&pool, "r1").await;
    let (pod, token) = crate::runs::ingest_auth::mint_local_token(&pool, "r1")
        .await
        .unwrap();
    let mut req = request("gate", 8, 3600);
    req.evidence_digest = request("gate", 9, 3600).evidence_digest;
    let bearer = format!("Bearer {token}");
    let (status, _) = send(
        &ingest(&pool),
        "POST",
        &format!("/api/pods/{pod}/decisions"),
        ("authorization", bearer.as_str()),
        Some(serde_json::to_value(req).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

fn api(pool: &PgPool) -> axum::Router {
    crate::api::router(ApiState::test(Db::new(pool.clone()), Arc::new(NoOverrides)))
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn the_launcher_sees_and_answers_the_request_and_a_stranger_does_neither(pool: PgPool) {
    seed_run(&pool, "r1").await;
    let req = request("gate", 8, 3600);
    let (d, _) = store::open(&pool, "r1", Some("pb#1"), &req).await.unwrap();
    let app = api(&pool);
    let wren = as_user("wren");
    let mallory = as_user("mallory");

    let (status, listed) = send(&app, "GET", "/api/decisions", (wren.0, &wren.1), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["open"][0]["id"], d.id.as_str());
    let (_, listed) = send(&app, "GET", "/api/decisions", (mallory.0, &mallory.1), None).await;
    assert_eq!(listed["open"], serde_json::json!([]));

    let (status, shown) = send(
        &app,
        "GET",
        &format!("/api/decisions/{}", d.id),
        (wren.0, &wren.1),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(shown["can_answer"], true);
    assert_eq!(shown["inputs"][0]["output"]["gpus"], 8);
    assert_eq!(
        shown["questions"][0],
        serde_json::json!({
            "id": "go",
            "instructions": "launch the job?",
            "kind": "choice",
            "multiple": false,
            "options": ["approve", "deny"],
        })
    );

    let answer = |go: &str, digest: &str| serde_json::json!({"labels": {"go": [go]}, "evidence_digest": digest, "note": "ok"});
    let uri = format!("/api/decisions/{}/answer", d.id);
    let (status, _) = send(
        &app,
        "POST",
        &uri,
        (mallory.0, &mallory.1),
        Some(answer("approve", &req.evidence_digest)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a stranger cannot read the run, so cannot answer"
    );
    for bad in ["maybe", "uncertain"] {
        let (status, _) = send(
            &app,
            "POST",
            &uri,
            (wren.0, &wren.1),
            Some(answer(bad, &req.evidence_digest)),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
    let stale = request("gate", 9, 3600).evidence_digest;
    let (status, _) = send(
        &app,
        "POST",
        &uri,
        (wren.0, &wren.1),
        Some(answer("approve", &stale)),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, answered) = send(
        &app,
        "POST",
        &uri,
        (wren.0, &wren.1),
        Some(answer("approve", &req.evidence_digest)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answered}");
    assert_eq!(answered["state"], "answered");
    assert_eq!(answered["answer"]["decided_by"], "user:wren");
    assert_eq!(
        answered["answer"]["labels"]["go"],
        serde_json::json!(["approve"])
    );
    let (status, _) = send(
        &app,
        "POST",
        &uri,
        (wren.0, &wren.1),
        Some(answer("deny", &req.evidence_digest)),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM authz_audit WHERE action = 'run:approve' AND resource_id = 'r1' \
         AND decision = 'allow' AND actor = 'user:wren'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audited, 1, "one accepted answer, one audit row");
    let (_, listed) = send(&app, "GET", "/api/decisions", (wren.0, &wren.1), None).await;
    assert_eq!(listed["open"], serde_json::json!([]));
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_multiple_choice_and_a_pick_take_distinct_offered_values(pool: PgPool) {
    seed_run(&pool, "r1").await;
    let req = multi_request();
    let (d, _) = store::open(&pool, "r1", Some("pb#1"), &req).await.unwrap();
    let app = api(&pool);
    let wren = as_user("wren");

    let (_, shown) = send(
        &app,
        "GET",
        &format!("/api/decisions/{}", d.id),
        (wren.0, &wren.1),
        None,
    )
    .await;
    let question = |id: &str| {
        shown["questions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|q| q["id"] == id)
            .cloned()
            .unwrap()
    };
    assert_eq!(question("checks")["kind"], "choice");
    assert_eq!(question("checks")["multiple"], true);
    assert_eq!(
        question("nodes"),
        serde_json::json!({
            "id": "nodes",
            "instructions": "which nodes?",
            "kind": "pick",
            "multiple": true,
            "options": ["a1", "b2"],
        })
    );

    let uri = format!("/api/decisions/{}/answer", d.id);
    let answer = |go: serde_json::Value, checks: serde_json::Value, nodes: serde_json::Value| {
        serde_json::json!({
            "labels": {"go": go, "checks": checks, "nodes": nodes},
            "evidence_digest": req.evidence_digest,
        })
    };
    use serde_json::json;
    for (bad, why) in [
        (
            answer(json!(["approve", "deny"]), json!(["lint"]), json!(["a1"])),
            "two for a single choice",
        ),
        (
            answer(json!([]), json!(["lint"]), json!(["a1"])),
            "nothing for go",
        ),
        (
            answer(json!(["approve"]), json!([]), json!(["a1"])),
            "nothing for checks",
        ),
        (
            answer(json!(["approve"]), json!(["lint", "lint"]), json!(["a1"])),
            "a repeated label",
        ),
        (
            answer(
                json!(["approve"]),
                json!(["lint", "uncertain"]),
                json!(["a1"]),
            ),
            "uncertain",
        ),
        (
            answer(json!(["approve"]), json!(["lint"]), json!(["zz"])),
            "a value the pick did not offer",
        ),
        (
            answer(json!(["approve"]), json!(["lint"]), json!(["a1", "a1"])),
            "a repeated pick",
        ),
        (
            json!({"labels": {"go": ["approve"], "checks": ["lint"]}, "evidence_digest": req.evidence_digest}),
            "no answer for nodes",
        ),
        (
            json!({"labels": {"go": "approve", "checks": ["lint"], "nodes": ["a1"]}, "evidence_digest": req.evidence_digest}),
            "a bare string",
        ),
    ] {
        let (status, _) = send(&app, "POST", &uri, (wren.0, &wren.1), Some(bad)).await;
        assert!(
            status == StatusCode::UNPROCESSABLE_ENTITY || status == StatusCode::BAD_REQUEST,
            "{why}: {status}"
        );
    }

    let (status, answered) = send(
        &app,
        "POST",
        &uri,
        (wren.0, &wren.1),
        Some(answer(
            json!(["approve"]),
            json!(["unit", "lint"]),
            json!(["b2", "a1"]),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answered}");
    assert_eq!(
        answered["answer"]["labels"]["checks"],
        json!(["unit", "lint"])
    );
    assert_eq!(answered["answer"]["labels"]["nodes"], json!(["b2", "a1"]));
    let read = store::get(&pool, &d.id).await.unwrap().unwrap();
    let RequestStatus::Answered { answer: record } = read.status else {
        panic!("{:?}", read.status);
    };
    assert_eq!(
        record.labels[&QuestionId::new("nodes").unwrap()],
        ["b2", "a1"]
    );
    let (status, _) = send(
        &app,
        "POST",
        &uri,
        (wren.0, &wren.1),
        Some(answer(
            json!(["approve"]),
            json!(["unit", "lint"]),
            json!(["b2", "a1"]),
        )),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the same answer again is acknowledged"
    );
}

/// utoipa keeps the last schema registered under a name, so a decision DTO that took an existing
/// name would silently replace that schema in the typed client.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn the_decision_schemas_shadow_no_other_schema(pool: PgPool) {
    let app = api(&pool);
    let (status, spec) = send(
        &app,
        "GET",
        "/api/openapi.json",
        ("x-auth-request-user", "wren"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let schemas = &spec["components"]["schemas"];
    for name in [
        "DecisionDto",
        "DecisionsDto",
        "DecisionSummaryDto",
        "DecisionQuestionDto",
        "DecisionFileDto",
        "DecisionInputDto",
        "DecisionGatedDto",
        "DecisionAnswerDto",
        "DecisionAnswerBody",
    ] {
        assert!(schemas.get(name).is_some(), "missing schema {name}");
    }
    assert!(
        schemas["EvidenceFileDto"]["properties"]
            .get("name")
            .is_some(),
        "the task-evidence file schema is intact"
    );
}
