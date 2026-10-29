//! The item store over HTTP: `POST /api/items/runs/{run_id}/{store}/{query,claim,set,release}`.
//! Only the run's own pod token authenticates, and the store is always one of the playbook the run
//! was launched from, so a run never names another playbook's items.

use crate::items::{Outcome, Scope, SetError};
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use crucible_contract::items::{
    ClaimRequest, ItemsReply, QueryRequest, Refused, ReleaseRequest, SetRequest, StoreName,
};

#[derive(Clone)]
pub(crate) struct ItemsState {
    pub(crate) db: crate::Db,
}

pub(crate) fn router(state: ItemsState) -> Router {
    Router::new()
        .route("/api/items/runs/{run_id}/{store}/query", post(query))
        .route("/api/items/runs/{run_id}/{store}/claim", post(claim))
        .route("/api/items/runs/{run_id}/{store}/set", post(set))
        .route("/api/items/runs/{run_id}/{store}/release", post(release))
        .with_state(state)
}

fn refuse(status: StatusCode, error: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": error.into() }))).into_response()
}

fn failed(error: &anyhow::Error, what: &str) -> Response {
    tracing::error!(
        error = format!("{error:#}"),
        what,
        "item store request failed"
    );
    refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("{what} failed"))
}

/// The scope `run_id` acts on, or the refusal for a request without that run's own live token.
#[allow(clippy::result_large_err)]
async fn authorize(
    state: &ItemsState,
    headers: &HeaderMap,
    run_id: &str,
    store: String,
) -> Result<Scope, Response> {
    let Some(presented) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .filter(|t| crate::runs::pod_token::looks_like_token(t))
    else {
        return Err(refuse(
            StatusCode::UNAUTHORIZED,
            "only a run's pod token reaches its playbook's items",
        ));
    };
    let pool = state.db.pool();
    match crate::runs::pod_token::verify(pool, presented).await {
        Ok(grant) if grant.may_write_run(run_id) => {}
        Ok(_) => {
            return Err(refuse(
                StatusCode::FORBIDDEN,
                "this token reaches only its own run's items",
            ));
        }
        Err(e) => return Err(refuse(StatusCode::UNAUTHORIZED, e.to_string())),
    }
    let store =
        StoreName::new(store).map_err(|e| refuse(StatusCode::BAD_REQUEST, e.to_string()))?;
    match crate::items::scope_of_run(pool, run_id, store).await {
        Ok(Some(scope)) => Ok(scope),
        Ok(None) => Err(refuse(
            StatusCode::NOT_FOUND,
            format!("run {run_id} was not launched from a playbook"),
        )),
        Err(e) => Err(failed(&e, "resolving the run's playbook")),
    }
}

/// [`authorize`], then the request body.
#[allow(clippy::result_large_err)]
async fn admit<T>(
    state: &ItemsState,
    headers: &HeaderMap,
    run_id: &str,
    store: String,
    body: Result<Json<T>, JsonRejection>,
) -> Result<(Scope, T), Response> {
    let scope = authorize(state, headers, run_id, store).await?;
    let Json(request) = body.map_err(|e| refuse(StatusCode::BAD_REQUEST, e.body_text()))?;
    Ok((scope, request))
}

fn outcome(outcome: Outcome) -> Response {
    match outcome {
        Outcome::Done(items) => Json(ItemsReply { items }).into_response(),
        Outcome::Refused(conflicts) => {
            (StatusCode::CONFLICT, Json(Refused { conflicts })).into_response()
        }
    }
}

async fn query(
    State(state): State<ItemsState>,
    Path((run_id, store)): Path<(String, String)>,
    headers: HeaderMap,
    request: Result<Json<QueryRequest>, JsonRejection>,
) -> Response {
    let (scope, request) = match admit(&state, &headers, &run_id, store, request).await {
        Ok(admitted) => admitted,
        Err(refusal) => return refusal,
    };
    match crate::items::query(state.db.pool(), &scope, &request).await {
        Ok(items) => Json(ItemsReply { items }).into_response(),
        Err(e) => failed(&e, "reading items"),
    }
}

async fn claim(
    State(state): State<ItemsState>,
    Path((run_id, store)): Path<(String, String)>,
    headers: HeaderMap,
    request: Result<Json<ClaimRequest>, JsonRejection>,
) -> Response {
    let (scope, request) = match admit(&state, &headers, &run_id, store, request).await {
        Ok(admitted) => admitted,
        Err(refusal) => return refusal,
    };
    match crate::items::claim(state.db.pool(), &scope, &run_id, &request).await {
        Ok(done) => {
            if let Outcome::Done(items) = &done {
                tracing::info!(run_id, playbook = scope.playbook, store = %scope.store, items = items.len(), "items claimed");
            }
            outcome(done)
        }
        Err(e) => failed(&e, "claiming items"),
    }
}

async fn set(
    State(state): State<ItemsState>,
    Path((run_id, store)): Path<(String, String)>,
    headers: HeaderMap,
    request: Result<Json<SetRequest>, JsonRejection>,
) -> Response {
    let (scope, request) = match admit(&state, &headers, &run_id, store, request).await {
        Ok(admitted) => admitted,
        Err(refusal) => return refusal,
    };
    match crate::items::set(state.db.pool(), &scope, &run_id, &request).await {
        Ok(done) => {
            if let Outcome::Done(items) = &done {
                tracing::info!(run_id, playbook = scope.playbook, store = %scope.store, items = items.len(), "item labels set");
            }
            outcome(done)
        }
        Err(e @ SetError::Invalid { .. }) => {
            refuse(StatusCode::UNPROCESSABLE_ENTITY, e.to_string())
        }
        Err(SetError::Store(e)) => failed(&e, "setting item labels"),
    }
}

async fn release(
    State(state): State<ItemsState>,
    Path((run_id, store)): Path<(String, String)>,
    headers: HeaderMap,
    request: Result<Json<ReleaseRequest>, JsonRejection>,
) -> Response {
    let (scope, request) = match admit(&state, &headers, &run_id, store, request).await {
        Ok(admitted) => admitted,
        Err(refusal) => return refusal,
    };
    match crate::items::release(state.db.pool(), &scope, &run_id, &request).await {
        Ok(items) => Json(ItemsReply { items }).into_response(),
        Err(e) => failed(&e, "releasing items"),
    }
}

#[cfg(test)]
mod tests {
    use crate::items::api::*;
    use crate::items::tests::playbook_run;
    use axum::body::Body;
    use axum::http::Request;
    use sqlx::PgPool;
    use tower::ServiceExt;

    async fn post(
        router: &Router,
        path: &str,
        bearer: Option<&str>,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let mut request = Request::post(path).header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = bearer {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    fn app(pool: &PgPool) -> Router {
        router(ItemsState {
            db: crate::Db::new(pool.clone()),
        })
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn only_the_runs_own_live_pod_token_reaches_its_items(pool: PgPool) {
        let a = playbook_run(&pool, "triage", "run-a").await;
        let b = playbook_run(&pool, "triage", "run-b").await;
        let app = app(&pool);
        let path = "/api/items/runs/run-a/tickets/query";
        let body = r#"{"keys": ["K-1"]}"#;
        for (bearer, want) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some("crk_id_secret"), StatusCode::UNAUTHORIZED),
            (Some("crr_nosuch_secret"), StatusCode::UNAUTHORIZED),
            (Some(b.as_str()), StatusCode::FORBIDDEN),
            (Some(a.as_str()), StatusCode::OK),
        ] {
            assert_eq!(post(&app, path, bearer, body).await.0, want, "{bearer:?}");
        }
        crate::runs::pod_token::revoke(
            &pool,
            &crate::runs::pod_token::Subject::Run("run-a".into()),
        )
        .await
        .unwrap();
        assert_eq!(
            post(&app, path, Some(&a), body).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn bad_stores_bodies_and_non_playbook_runs_are_refused(pool: PgPool) {
        let a = playbook_run(&pool, "triage", "run-a").await;
        sqlx::query("INSERT INTO runs (run_id, status) VALUES ('loose', 'running')")
            .execute(&pool)
            .await
            .unwrap();
        let loose = crate::runs::pod_token::mint(
            &pool,
            &crate::runs::pod_token::Subject::Run("loose".into()),
            None,
            std::time::Duration::from_secs(3600),
        )
        .await
        .unwrap();
        let app = app(&pool);
        let (status, body) = post(
            &app,
            "/api/items/runs/run-a/Tickets/claim",
            Some(&a),
            r#"{"keys": ["K-1"]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"].as_str().unwrap().contains("Tickets"),
            "{body}"
        );
        for bad in [
            r#"{"keys": []}"#,
            r#"{"keys": ["a b"]}"#,
            r#"{"key": ["K"]}"#,
            "nope",
        ] {
            let (status, _) =
                post(&app, "/api/items/runs/run-a/tickets/claim", Some(&a), bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        }
        let (status, body) = post(
            &app,
            "/api/items/runs/loose/tickets/claim",
            Some(&loose),
            r#"{"keys": ["K-1"]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap().contains("loose"), "{body}");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_claim_set_release_round_trip_answers_items_and_conflicts(pool: PgPool) {
        let a = playbook_run(&pool, "triage", "run-a").await;
        let b = playbook_run(&pool, "triage", "run-b").await;
        let app = app(&pool);
        let (status, body) = post(
            &app,
            "/api/items/runs/run-a/tickets/claim",
            Some(&a),
            r#"{"keys": ["K-1", "K-2"], "unless": "triaged"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body,
            serde_json::json!({"items": [
                {"key": "K-1", "labels": {}, "held_by": "run-a"},
                {"key": "K-2", "labels": {}, "held_by": "run-a"},
            ]})
        );
        let (status, body) = post(
            &app,
            "/api/items/runs/run-b/tickets/claim",
            Some(&b),
            r#"{"keys": ["K-2"]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body,
            serde_json::json!({"conflicts": [{"reason": "held", "key": "K-2", "run": "run-a"}]})
        );
        let (status, body) = post(
            &app,
            "/api/items/runs/run-b/tickets/set",
            Some(&b),
            r#"{"keys": ["K-1"], "labels": {"triaged": ""}}"#,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["conflicts"][0]["reason"], "not_held");
        let (status, body) = post(
            &app,
            "/api/items/runs/run-a/tickets/set",
            Some(&a),
            r#"{"keys": ["K-1"], "labels": {"triaged": "", "verdict": "not_affected"}}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["items"][0]["updated_by"], "run-a");
        let (status, body) = post(
            &app,
            "/api/items/runs/run-a/tickets/release",
            Some(&a),
            r#"{"keys": ["K-1", "K-2"]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"].as_array().unwrap().len(), 2);
        let (status, body) = post(
            &app,
            "/api/items/runs/run-b/tickets/claim",
            Some(&b),
            r#"{"keys": ["K-1", "K-2"], "unless": "triaged"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body,
            serde_json::json!({"conflicts": [{
                "reason": "matched",
                "key": "K-1",
                "labels": {"triaged": "", "verdict": "not_affected"},
            }]})
        );
        let (status, body) = post(
            &app,
            "/api/items/runs/run-b/tickets/query",
            Some(&b),
            r#"{"keys": ["K-1", "K-2", "K-3"], "selector": "!triaged"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!({"items": [{"key": "K-2", "labels": {}}]})
        );
    }
}
