use crate::api::state::ApiState;
use crate::client::Db;
use crate::daemon::queue::{Override, OverrideSink};
use crate::provenance::*;
use anyhow::Result;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use crucible_contract::provenance::{CommitSha, Expect, VerifyStep, verify};
use serde_json::{Value, json};
use tower::ServiceExt;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const REPO: &str = "https://github.com/acme/widget";

struct NoSink;

impl OverrideSink for NoSink {
    fn submit(&self, _ov: Override) {}
}

fn signer() -> Arc<Signer> {
    Arc::new(Signer::from_seed(&[11; 32]))
}

fn graph() -> &'static str {
    r#"[{"name":"fetch","kind":"command","depends_on":[]},{"name":"author","kind":"agent","depends_on":["fetch"]},{"name":"build","kind":"command","depends_on":["author"]},{"name":"lint","kind":"evaluate","depends_on":["author"]},{"name":"push","kind":"command","depends_on":["build"]},{"name":"notify","kind":"command","depends_on":["push"]}]"#
}

/// A playbook run whose `push` task reported pushing `sha`, with the checks and the agent that
/// came before it, a check beside it that is not upstream, and a task after it.
fn session(push_status: &str, commit_url: &str) -> String {
    [
        format!(
            r#"{{"v":1,"kind":"plan_admitted","plan_version":1,"reason":"","budget_usd":5.0,"tasks":{}}}"#,
            graph()
        ),
        r#"{"v":1,"kind":"task_result","task":"fetch","status":"pass","plan_version":1,"task_kind":"command","iter":0,"attempts":1,"cost_usd":0.0,"note":"","secs":1.0}"#.to_string(),
        r#"{"v":1,"kind":"task_result","task":"author","status":"pass","plan_version":1,"task_kind":"agent","iter":0,"attempts":1,"cost_usd":0.4,"note":"","secs":30.0,"agent":{"harness":"claude","model":"m-1","effort":"low"}}"#.to_string(),
        r#"{"v":1,"kind":"task_result","task":"build","status":"pass","plan_version":1,"task_kind":"command","iter":0,"attempts":1,"cost_usd":0.0,"note":"","secs":5.0}"#.to_string(),
        r#"{"v":1,"kind":"task_result","task":"lint","status":"fail","plan_version":1,"task_kind":"evaluate","iter":0,"attempts":1,"cost_usd":0.0,"note":"","secs":5.0}"#.to_string(),
        format!(
            r##"{{"v":1,"kind":"task_result","task":"push","status":"{push_status}","plan_version":1,"task_kind":"command","iter":0,"attempts":1,"cost_usd":0.0,"note":"","secs":2.0,"links":[{{"url":"{commit_url}","provider":"github","kind":"commit","label":"0123456"}},{{"url":"https://github.com/acme/widget/pull/7","provider":"github","kind":"pull_request","label":"#7"}},{{"url":"https://github.com/acme/other/pull/9","provider":"github","kind":"pull_request","label":"#9"}}]}}"##
        ),
        r#"{"v":1,"kind":"task_result","task":"notify","status":"pass","plan_version":1,"task_kind":"command","iter":0,"attempts":1,"cost_usd":0.0,"note":"","secs":1.0}"#.to_string(),
        r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":"done"}"#.to_string(),
    ]
    .join("\n")
}

fn commit_url() -> String {
    format!("{REPO}/commit/{SHA}")
}

async fn ingest(db: &Db, run_id: &str, log: &str) -> Result<()> {
    crate::runs::ingest::ingest_session(
        db,
        &crate::runs::ingest::IngestTarget {
            run_id,
            scope_id: None,
            issue: None,
            pod: None,
            session_uri: "db://run-session/x",
        },
        log,
    )
    .await?;
    Ok(())
}

fn dispatched() -> DispatchProvenance {
    DispatchProvenance {
        cluster: Some("east".into()),
        provider: Some("registry-provider".into()),
        sandbox_image: Some(format!(
            "registry.example/sandbox@sha256:{}",
            "b".repeat(64)
        )),
    }
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_pushed_commit_is_signed_from_the_controllers_records(pool: PgPool) -> Result<()> {
    let db = Db::new(pool);
    ingest(&db, "run-1", &session("pass", &commit_url())).await?;
    set_dispatch_provenance(db.pool(), "run-1", &dispatched()).await?;
    let signer = signer();

    let stored = attest_run(db.pool(), &signer, "run-1").await?;
    assert_eq!(stored.len(), 1);
    let bytes = envelope(db.pool(), &stored[0]).await?.expect("stored");
    let sha = CommitSha::parse(SHA)?;
    let ok = verify(
        &bytes,
        &signer.key_set(),
        Expect {
            digest: Some(&stored[0]),
            commit: Some(&sha),
            repository: Some(REPO),
        },
    )?;
    let p = ok.statement.predicate;
    assert_eq!(
        (p.run.as_str(), p.task.as_str(), p.iter),
        ("run-1", "push", 0)
    );
    let stored_plan = crate::runs::task_results::latest_run_plan(db.pool(), "run-1")
        .await?
        .expect("the admitted plan");
    assert_eq!(
        p.plan,
        Some(crucible_contract::content_digest(
            stored_plan.graph_json.as_bytes()
        )),
        "the plan digest is over the admitted graph as stored"
    );
    assert_eq!(
        p.checks
            .iter()
            .map(|c| (c.task.as_str(), c.status.as_str()))
            .collect::<Vec<_>>(),
        vec![("fetch", "pass"), ("build", "pass")],
        "only the command and evaluate tasks upstream of the push, not one beside or after it"
    );
    assert_eq!(
        p.agents
            .iter()
            .map(|a| (a.task.as_str(), a.harness.as_str(), a.model.as_str()))
            .collect::<Vec<_>>(),
        vec![("author", "claude", "m-1")]
    );
    assert_eq!(p.cluster.as_deref(), Some("east"));
    assert_eq!(p.provider.as_deref(), Some("registry-provider"));
    assert_eq!(p.sandbox_image, dispatched().sandbox_image);
    assert_eq!(
        p.pull_request.as_deref(),
        Some("https://github.com/acme/widget/pull/7"),
        "the pull request in the pushed commit's repository, not another one the task reported"
    );
    assert_eq!(p.playbook, None, "a run with no launch names no playbook");

    let by_commit = statements_for_commit(db.pool(), SHA).await?;
    assert_eq!(by_commit.len(), 1);
    assert_eq!(
        (
            by_commit[0].digest.clone(),
            by_commit[0].repository.as_str(),
            by_commit[0].task.as_str(),
            by_commit[0].keyid.as_str()
        ),
        (stored[0].clone(), REPO, "push", signer.keyid().as_str())
    );
    assert_eq!(statements_for_run(db.pool(), "run-1").await?, by_commit);
    Ok(())
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn attesting_a_run_again_signs_nothing_new(pool: PgPool) -> Result<()> {
    let db = Db::new(pool);
    ingest(&db, "run-1", &session("pass", &commit_url())).await?;
    let signer = signer();
    assert_eq!(attest_run(db.pool(), &signer, "run-1").await?.len(), 1);
    assert!(attest_run(db.pool(), &signer, "run-1").await?.is_empty());
    assert_eq!(statements_for_commit(db.pool(), SHA).await?.len(), 1);
    Ok(())
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_failed_push_an_abbreviated_sha_and_an_unknown_run_get_no_statement(
    pool: PgPool,
) -> Result<()> {
    let db = Db::new(pool);
    let signer = signer();
    ingest(&db, "failed", &session("fail", &commit_url())).await?;
    assert!(attest_run(db.pool(), &signer, "failed").await?.is_empty());
    ingest(
        &db,
        "short",
        &session("pass", &format!("{REPO}/commit/0123456")),
    )
    .await?;
    assert!(attest_run(db.pool(), &signer, "short").await?.is_empty());
    assert!(attest_run(db.pool(), &signer, "nope").await?.is_empty());
    assert!(statements_for_commit(db.pool(), SHA).await?.is_empty());
    Ok(())
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn every_run_that_pushes_the_same_commit_gets_its_own_statement(pool: PgPool) -> Result<()> {
    let db = Db::new(pool);
    let signer = signer();
    ingest(&db, "first", &session("pass", &commit_url())).await?;
    let first = attest_run(db.pool(), &signer, "first").await?;
    ingest(&db, "rerun", &session("pass", &commit_url())).await?;
    let rerun = attest_run(db.pool(), &signer, "rerun").await?;
    assert_ne!(first, rerun);
    let listed = statements_for_commit(db.pool(), SHA).await?;
    assert_eq!(
        listed
            .iter()
            .map(|s| s.run_id.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["first", "rerun"])
    );
    Ok(())
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn without_a_dispatch_record_the_run_row_names_the_cluster_and_image(
    pool: PgPool,
) -> Result<()> {
    let db = Db::new(pool);
    ingest(&db, "run-1", &session("pass", &commit_url())).await?;
    crate::runs::store::set_run_image(
        db.pool(),
        "run-1",
        &crate::runs::model::RunImage {
            reference: Some("registry.example/sandbox:v1".into()),
            digest: Some(format!("sha256:{}", "c".repeat(64))),
            capability_digest: None,
            overridden: false,
        },
    )
    .await?;
    let signer = signer();
    let digest = attest_run(db.pool(), &signer, "run-1").await?;
    let bytes = envelope(db.pool(), &digest[0]).await?.expect("stored");
    let p = verify(&bytes, &signer.key_set(), Expect::default())?
        .statement
        .predicate;
    assert_eq!(
        p.sandbox_image,
        Some(format!(
            "registry.example/sandbox:v1@sha256:{}",
            "c".repeat(64)
        ))
    );
    assert_eq!(
        p.cluster.as_deref(),
        Some(crate::runs::clusters::HUB_CLUSTER)
    );
    assert_eq!(
        p.provider, None,
        "a provider the controller did not record is absent"
    );
    Ok(())
}

#[test]
fn an_image_is_pinned_only_by_a_digest() {
    let d = format!("sha256:{}", "a".repeat(64));
    assert_eq!(
        pinned_image(Some("r/i:v1"), Some(&d)),
        Some(format!("r/i:v1@{d}"))
    );
    assert_eq!(
        pinned_image(Some(&format!("r/i@{d}")), None),
        Some(format!("r/i@{d}"))
    );
    assert_eq!(pinned_image(None, Some(&d)), Some(d.clone()));
    assert_eq!(pinned_image(Some("r/i:v1"), None), None);
    assert_eq!(pinned_image(None, None), None);
}

#[test]
fn a_key_file_loads_and_a_bad_one_names_its_path() -> Result<()> {
    use ed25519_dalek::pkcs8::EncodePrivateKey;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("provenance.pem");
    let pem = ed25519_dalek::SigningKey::from_bytes(&[11; 32])
        .to_pkcs8_pem(ed25519_dalek::pkcs8::spki::der::pem::LineEnding::LF)?;
    std::fs::write(&path, pem.as_bytes())?;
    assert_eq!(load_signer(&path)?.keyid(), signer().keyid());
    std::fs::write(&path, "not a key")?;
    let err = format!("{:#}", load_signer(&path).unwrap_err());
    assert!(err.contains("provenance.pem"), "{err}");
    Ok(())
}

fn app(db: Db, signer: Option<Arc<Signer>>) -> Router {
    let state = ApiState {
        provenance: signer,
        ..ApiState::test(db, Arc::new(NoSink))
    };
    crate::api::router(state.clone()).merge(crate::provenance::api::keys_router(state))
}

async fn send(app: &Router, req: Request<Body>) -> Result<(StatusCode, Vec<u8>)> {
    let resp = app.clone().oneshot(req).await?;
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await?;
    Ok((status, bytes.to_vec()))
}

async fn get(app: &Router, uri: &str) -> Result<(StatusCode, Vec<u8>)> {
    send(app, Request::get(uri).body(Body::empty())?).await
}

async fn post_verify(app: &Router, body: Value) -> Result<(StatusCode, Value)> {
    let (status, bytes) = send(
        app,
        Request::post("/api/provenance/verify")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))?,
    )
    .await?;
    Ok((status, serde_json::from_slice(&bytes)?))
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn the_api_serves_envelopes_statements_and_verification(pool: PgPool) -> Result<()> {
    let db = Db::new(pool);
    ingest(&db, "run-1", &session("pass", &commit_url())).await?;
    let signer = signer();
    let digest = attest_run(db.pool(), &signer, "run-1").await?.remove(0);
    let app = app(db.clone(), Some(signer.clone()));

    let (status, keys) = get(&app, "/api/provenance/keys").await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<crucible_contract::provenance::KeySet>(&keys)?,
        signer.key_set()
    );

    let (status, bytes) = get(&app, &format!("/api/provenance/envelopes/{digest}")).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        EnvelopeDigest::of(&bytes),
        digest,
        "served byte for byte as stored"
    );

    let (status, listed) = get(&app, &format!("/api/provenance/commits/{SHA}")).await?;
    assert_eq!(status, StatusCode::OK);
    let listed: Value = serde_json::from_slice(&listed)?;
    assert_eq!(listed[0]["digest"], json!(digest.to_string()));
    assert_eq!(listed[0]["run_id"], "run-1");
    let (_, by_run) = get(&app, "/api/runs/run-1/provenance").await?;
    assert_eq!(serde_json::from_slice::<Value>(&by_run)?, listed);

    let (status, checked) = post_verify(&app, json!({"target": SHA, "repository": REPO})).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(checked[0]["ok"], true, "{checked}");
    assert_eq!(checked[0]["run_id"], "run-1");
    let (_, wrong_repo) = post_verify(
        &app,
        json!({"target": digest.to_string(), "repository": "https://github.com/acme/other"}),
    )
    .await?;
    assert_eq!(
        (&wrong_repo[0]["ok"], &wrong_repo[0]["step"]),
        (&json!(false), &json!("subject"))
    );

    sqlx::query("UPDATE commit_provenance SET envelope = $2 WHERE envelope_digest = $1")
        .bind(digest.as_str())
        .bind(tampered(&bytes))
        .execute(db.pool())
        .await?;
    let (_, after) = post_verify(&app, json!({"target": SHA})).await?;
    assert_eq!(
        (&after[0]["ok"], &after[0]["step"]),
        (&json!(false), &json!("digest")),
        "{after}"
    );
    Ok(())
}

/// The envelope with its statement swapped for one claiming another model, signature kept.
fn tampered(bytes: &[u8]) -> Vec<u8> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut env: Value = serde_json::from_slice(bytes).expect("envelope");
    let mut statement: Value = serde_json::from_slice(
        &b64.decode(env["payload"].as_str().expect("payload"))
            .expect("b64"),
    )
    .expect("statement");
    statement["predicate"]["agents"][0]["model"] = json!("a model the run never used");
    env["payload"] = json!(b64.encode(serde_json::to_vec(&statement).expect("encode")));
    serde_json::to_vec(&env).expect("encode")
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn the_api_refuses_what_it_cannot_answer(pool: PgPool) -> Result<()> {
    let db = Db::new(pool);
    let unsigned = app(db.clone(), None);
    assert_eq!(
        get(&unsigned, "/api/provenance/keys").await?.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        post_verify(&unsigned, json!({"target": SHA})).await?.0,
        StatusCode::SERVICE_UNAVAILABLE
    );

    let app = app(db, Some(signer()));
    assert_eq!(
        get(&app, "/api/provenance/commits/0123abc").await?.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(&app, "/api/provenance/envelopes/sha256:nothex")
            .await?
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(
            &app,
            &format!("/api/provenance/envelopes/sha256:{}", "a".repeat(64))
        )
        .await?
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        post_verify(&app, json!({"target": "nope"})).await?.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post_verify(&app, json!({"target": SHA})).await?.0,
        StatusCode::NOT_FOUND
    );
    Ok(())
}

#[test]
fn the_verify_step_serializes_as_the_api_documents_it() {
    assert_eq!(
        serde_json::to_value(VerifyStep::PayloadType).ok(),
        Some(json!("payload_type"))
    );
}
