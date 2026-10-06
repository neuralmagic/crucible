//! The public delivery surface (RFC-0003:C-WEBHOOK-DELIVERY): `POST /hooks/{id}` and
//! `POST /hooks/{id}/{token}`, served on their own listener.

use crate::identity::oidc::credentials::CredentialKeys;
use crate::launches::webhooks as store;
use crate::launches::webhooks::verify::{self, Presented};
use axum::Router;
use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use sqlx::PgPool;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// The largest delivery body the surface reads.
pub const MAX_BODY: usize = 512 * 1024;

/// Deliveries handled at once; one past it is shed with a 503 before any lookup.
const CONCURRENCY: usize = 64;

#[derive(Clone)]
pub struct HooksState {
    pool: PgPool,
    keys: Option<Arc<CredentialKeys>>,
    permits: Arc<Semaphore>,
}

impl HooksState {
    pub fn new(pool: PgPool, keys: Option<Arc<CredentialKeys>>) -> Self {
        HooksState {
            pool,
            keys,
            permits: Arc::new(Semaphore::new(CONCURRENCY)),
        }
    }
}

pub fn router(state: HooksState) -> Router {
    Router::new()
        .route("/hooks/{id}", post(untokened).fallback(refused))
        .route("/hooks/{id}/{token}", post(tokened).fallback(refused))
        .fallback(refused)
        .with_state(state)
}

/// The one response to every delivery the surface does not record.
async fn refused() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"error":"not found"}"#,
    )
        .into_response()
}

fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"error":"try again"}"#,
    )
        .into_response()
}

async fn untokened(
    State(state): State<HooksState>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    match path {
        Ok(Path(id)) => deliver(&state, &id, None, &headers, body).await,
        Err(_) => deliver(&state, "", None, &headers, body).await,
    }
}

async fn tokened(
    State(state): State<HooksState>,
    path: Result<Path<(String, String)>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    match path {
        Ok(Path((id, token))) => deliver(&state, &id, Some(&token), &headers, body).await,
        Err(_) => deliver(&state, "", Some(""), &headers, body).await,
    }
}

async fn deliver(
    state: &HooksState,
    id: &str,
    path_token: Option<&str>,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    let Ok(_permit) = state.permits.clone().try_acquire_owned() else {
        return unavailable();
    };
    let Ok(body) = axum::body::to_bytes(body, MAX_BODY).await else {
        return refused().await;
    };
    if id.is_empty() || id.contains('\0') || path_token.is_some_and(|t| t.contains('\0')) {
        return refused().await;
    }
    let webhook = match store::receivable(&state.pool, id).await {
        Ok(Some(webhook)) => webhook,
        Ok(None) => return refused().await,
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "webhooks: lookup failed");
            return unavailable();
        }
    };
    let verifier = webhook.verifier();
    let Some(material) = webhook.stored().and_then(|stored| {
        verify::material(&webhook.core.id, &stored, state.keys.as_deref())
            .map_err(|e| {
                tracing::error!(webhook = %webhook.core.id, error = format!("{e:#}"), "webhooks: secret unusable");
            })
            .ok()
    }) else {
        return refused().await;
    };
    let presented = Presented {
        path_token,
        headers,
        body: &body,
    };
    if !verify::verify(&verifier, &material, &presented) {
        return refused().await;
    }
    let recorded = verify::recorded_headers(&verifier, headers);
    match store::record_delivery(&state.pool, &webhook.core.id, &recorded, &body).await {
        Ok(delivery) => (
            StatusCode::ACCEPTED,
            axum::Json(serde_json::json!({ "delivery": delivery })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(webhook = %webhook.core.id, error = format!("{e:#}"), "webhooks: record failed");
            unavailable()
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::identity::oidc::credentials::CredentialKeys;
    use crate::launches::standing::NewStanding;
    use crate::launches::webhooks::receive::{HooksState, MAX_BODY, router};
    use crate::launches::webhooks::verify::{Verifier, VerifierKind};
    use crate::launches::webhooks::{self as store, NewWebhook};
    use crate::model::MaxTime;
    use aws_lc_rs::hmac;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use sqlx::{PgPool, Row};
    use std::sync::Arc;
    use tower::ServiceExt;

    const BODY: &str = r#"{"repository":"org/img","updated_tags":["latest"]}"#;

    fn keys() -> Arc<CredentialKeys> {
        Arc::new(CredentialKeys::new(vec![vec![7u8; 32]]).expect("keys"))
    }

    async fn register(pool: &PgPool) {
        sqlx::query(
            r#"INSERT INTO playbooks (id, description, repo, git_ref, rev, path, tar_gz,
                                      tar_digest, tar_bytes, params_schema, schema_digest,
                                      core_rev, created_by, created_at, updated_at)
               VALUES ('rebuild', 'Rebuild on push', 'neuralmagic/crucible', 'main', 'abc123',
                       'domains/rebuild', $1, 'sha256:tar', 3, $2::jsonb, 'sha256:schema',
                       'core1', 'tms', '2026-09-29T00:00:00Z', '2026-09-29T00:00:00Z')"#,
        )
        .bind(vec![1u8, 2, 3])
        .bind(
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {"image": {"type": "string"}}
            })
            .to_string(),
        )
        .execute(pool)
        .await
        .expect("register");
    }

    async fn webhook(
        pool: &PgPool,
        keys: &CredentialKeys,
        kind: VerifierKind,
        header: Option<&str>,
    ) -> store::Created {
        let max_time = MaxTime::parse("1h").expect("duration");
        let params = serde_json::json!({});
        let derive = serde_json::json!({"image": "body.repository"});
        store::create(
            pool,
            Some(keys),
            &NewWebhook {
                standing: NewStanding {
                    playbook: "rebuild",
                    target: crate::launches::standing::StandingTarget::Adopted(
                        crate::playbooks::registry::PackRevision::Bytes("sha256:tar"),
                    ),
                    eligible_draft_version: None,
                    params: &params,
                    schema_digest: "sha256:schema",
                    max_cost: 5.0,
                    max_time: &max_time,
                    advance_dedupe: false,
                    enabled: true,
                    created_by: Some("tms"),
                    owner_principal: None,
                    owner_groups: None,
                    dispatch_target: None,
                    agent_provider: None,
                    agent_model: None,
                },
                verifier: &Verifier {
                    kind,
                    header: header.map(str::to_string),
                },
                filter: "true",
                dedupe: "body.repository",
                derive: derive.as_object().expect("object"),
                max_launches_per_hour: 10,
                retention_days: 7,
            },
        )
        .await
        .expect("create webhook")
    }

    async fn post(
        state: &HooksState,
        uri: &str,
        headers: &[(&str, &str)],
        body: impl Into<Body>,
    ) -> (StatusCode, Option<String>, String) {
        let mut req = Request::post(uri);
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let res = router(state.clone())
            .oneshot(req.body(body.into()).expect("request"))
            .await
            .expect("response");
        let status = res.status();
        let content_type = res
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body");
        (
            status,
            content_type,
            String::from_utf8(bytes.to_vec()).expect("utf-8"),
        )
    }

    async fn deliveries(pool: &PgPool) -> Vec<(String, serde_json::Value, Vec<u8>)> {
        sqlx::query("SELECT webhook_id, headers, body FROM playbook_webhook_deliveries ORDER BY id")
            .fetch_all(pool)
            .await
            .expect("deliveries")
            .into_iter()
            .map(|r| (r.get("webhook_id"), r.get("headers"), r.get("body")))
            .collect()
    }

    fn signature(secret: &str, body: &str) -> String {
        let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
        let tag = hmac::sign(&key, body.as_bytes());
        format!(
            "sha256={}",
            tag.as_ref()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        )
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_path_token_delivery_is_recorded_before_the_202(pool: PgPool) {
        register(&pool).await;
        let keys = keys();
        let created = webhook(&pool, &keys, VerifierKind::PathToken, None).await;
        let state = HooksState::new(pool.clone(), Some(keys));

        let (status, _, body) = post(
            &state,
            &format!("/hooks/{}/{}", created.webhook.core.id, created.secret),
            &[
                ("content-type", "application/json"),
                ("x-quay-event", "push"),
            ],
            BODY,
        )
        .await;

        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let recorded = deliveries(&pool).await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, created.webhook.core.id);
        assert_eq!(recorded[0].2, BODY.as_bytes());
        assert_eq!(recorded[0].1["x-quay-event"], "push");
        let stamped = store::get(&pool, &created.webhook.core.id)
            .await
            .expect("get")
            .expect("row");
        assert!(stamped.last_delivery_at.is_some());
        let ack: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert!(ack["delivery"].as_str().is_some_and(|d| !d.is_empty()));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_hmac_delivery_is_recorded_without_its_signature(pool: PgPool) {
        register(&pool).await;
        let keys = keys();
        let created = webhook(
            &pool,
            &keys,
            VerifierKind::HmacSha256,
            Some("x-hub-signature-256"),
        )
        .await;
        let state = HooksState::new(pool.clone(), Some(keys));
        let signed = signature(&created.secret, BODY);

        let (status, _, _) = post(
            &state,
            &format!("/hooks/{}", created.webhook.core.id),
            &[
                ("x-hub-signature-256", &signed),
                ("x-github-event", "release"),
            ],
            BODY,
        )
        .await;

        assert_eq!(status, StatusCode::ACCEPTED);
        let recorded = deliveries(&pool).await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].1["x-github-event"], "release");
        assert!(recorded[0].1.get("x-hub-signature-256").is_none());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn every_unrecorded_delivery_gets_the_same_response(pool: PgPool) {
        register(&pool).await;
        let keys = keys();
        let token = webhook(&pool, &keys, VerifierKind::PathToken, None).await;
        let signed = webhook(
            &pool,
            &keys,
            VerifierKind::HmacSha256,
            Some("x-hub-signature-256"),
        )
        .await;
        let disabled = webhook(&pool, &keys, VerifierKind::PathToken, None).await;
        crate::launches::standing::set_enabled(&pool, &disabled.webhook.core.id, false)
            .await
            .expect("disable");
        let deleted = webhook(&pool, &keys, VerifierKind::PathToken, None).await;
        crate::launches::standing::delete(&pool, &deleted.webhook.core.id)
            .await
            .expect("delete");
        let state = HooksState::new(pool.clone(), Some(keys));
        let token_id = &token.webhook.core.id;
        let signed_id = &signed.webhook.core.id;
        let oversized = vec![b'x'; MAX_BODY + 1];
        let good_signature = signature(&signed.secret, BODY);

        let mut responses = vec![
            post(&state, "/hooks/no-such-webhook", &[], BODY).await,
            post(&state, "/hooks/no-such-webhook/token", &[], BODY).await,
            post(&state, &format!("/hooks/{token_id}/wrong"), &[], BODY).await,
            post(&state, &format!("/hooks/{token_id}"), &[], BODY).await,
            post(
                &state,
                &format!("/hooks/{token_id}/{}", token.secret),
                &[],
                oversized.clone(),
            )
            .await,
            post(
                &state,
                &format!("/hooks/{}/{}", disabled.webhook.core.id, disabled.secret),
                &[],
                BODY,
            )
            .await,
            post(
                &state,
                &format!("/hooks/{}/{}", deleted.webhook.core.id, deleted.secret),
                &[],
                BODY,
            )
            .await,
            post(
                &state,
                &format!("/hooks/{signed_id}"),
                &[("x-hub-signature-256", "sha256=00")],
                BODY,
            )
            .await,
            post(
                &state,
                &format!("/hooks/{signed_id}/{}", signed.secret),
                &[("x-hub-signature-256", &good_signature)],
                BODY,
            )
            .await,
            post(&state, "/elsewhere", &[], BODY).await,
        ];
        let get = router(state.clone())
            .oneshot(
                Request::get(format!("/hooks/{token_id}/{}", token.secret))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let get_status = get.status();
        let get_type = get
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let get_body = axum::body::to_bytes(get.into_body(), usize::MAX)
            .await
            .expect("body");
        responses.push((
            get_status,
            get_type,
            String::from_utf8(get_body.to_vec()).expect("utf-8"),
        ));

        let first = responses[0].clone();
        assert_eq!(first.0, StatusCode::NOT_FOUND);
        for (i, response) in responses.iter().enumerate() {
            assert_eq!(response, &first, "case {i} differs");
        }
        assert!(deliveries(&pool).await.is_empty());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_delivery_that_cannot_be_recorded_is_retryable(pool: PgPool) {
        register(&pool).await;
        let keys = keys();
        let created = webhook(&pool, &keys, VerifierKind::PathToken, None).await;
        let state = HooksState::new(pool.clone(), Some(keys));
        pool.close().await;

        let (status, _, _) = post(
            &state,
            &format!("/hooks/{}/{}", created.webhook.core.id, created.secret),
            &[],
            BODY,
        )
        .await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_rotated_secret_retires_the_old_one(pool: PgPool) {
        register(&pool).await;
        let keys = keys();
        let created = webhook(&pool, &keys, VerifierKind::PathToken, None).await;
        let id = created.webhook.core.id.clone();
        let rotated = store::rotate_secret(&pool, Some(&keys), &id)
            .await
            .expect("rotate")
            .expect("webhook");
        let state = HooksState::new(pool.clone(), Some(keys));

        let old = post(
            &state,
            &format!("/hooks/{id}/{}", created.secret),
            &[],
            BODY,
        )
        .await;
        let new = post(&state, &format!("/hooks/{id}/{rotated}"), &[], BODY).await;

        assert_eq!(old.0, StatusCode::NOT_FOUND);
        assert_eq!(new.0, StatusCode::ACCEPTED);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn deleting_the_standing_launch_removes_everything_under_it(pool: PgPool) {
        register(&pool).await;
        let keys = keys();
        let created = webhook(&pool, &keys, VerifierKind::PathToken, None).await;
        let id = created.webhook.core.id.clone();
        let state = HooksState::new(pool.clone(), Some(keys));
        post(
            &state,
            &format!("/hooks/{id}/{}", created.secret),
            &[],
            BODY,
        )
        .await;
        sqlx::query(
            "INSERT INTO playbook_webhook_keys (webhook_id, dedupe_key, delivery_id, launch_key,
                 consumed_at) VALUES ($1, 'k', 'd', 'playbook:rebuild:x', '2026-09-29T00:00:00Z')",
        )
        .bind(&id)
        .execute(&pool)
        .await
        .expect("key");

        assert!(
            crate::launches::standing::delete(&pool, &id)
                .await
                .expect("delete")
        );

        let left: (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM playbook_webhooks),
                    (SELECT count(*) FROM playbook_webhook_deliveries),
                    (SELECT count(*) FROM playbook_webhook_keys)",
        )
        .fetch_one(&pool)
        .await
        .expect("counts");
        assert_eq!(left, (0, 0, 0));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_malformed_address_gets_the_same_response(pool: PgPool) {
        register(&pool).await;
        let keys = keys();
        let created = webhook(&pool, &keys, VerifierKind::PathToken, None).await;
        let id = created.webhook.core.id.clone();
        let state = HooksState::new(pool.clone(), Some(keys));
        let reference = post(&state, "/hooks/no-such-webhook", &[], BODY).await;
        for uri in [
            "/hooks/%FF".to_string(),
            "/hooks/%FF/%FE".to_string(),
            "/hooks/%00/token".to_string(),
            format!("/hooks/{id}/%00"),
            format!("/hooks/{id}%00/{}", created.secret),
        ] {
            assert_eq!(post(&state, &uri, &[], BODY).await, reference, "{uri}");
        }
    }
}
