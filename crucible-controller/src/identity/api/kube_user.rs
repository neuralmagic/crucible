//! A platform administrator's override of the hub username agent tokens carry as `kube_user` for
//! runs a person launches. People link their own per cluster through the cluster identity routes.

use crate::api::dto::{bad_request, forbidden, not_found};
use crate::api::state::{ApiState, AppError, ErrorBody, Json};
use crate::identity::kube_identity::{self, KubeUser};
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The hub username override a platform administrator set for a person.
#[derive(Debug, Serialize, ToSchema, PartialEq)]
pub struct KubeUserDto {
    /// The override, `null` once cleared.
    pub r#override: Option<String>,
}

/// The override to set, or `null` to clear it.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OverrideBody {
    pub kube_user: Option<String>,
}

#[utoipa::path(
    put,
    path = "/api/users/{login}/kube-user",
    request_body = OverrideBody,
    params(("login" = String, Path, description = "The person's login")),
    responses(
        (status = 200, description = "The override is set (or cleared with null)", body = KubeUserDto),
        (status = 400, description = "Not a username a person may act as", body = ErrorBody),
        (status = 403, description = "Caller is not a platform administrator, or names themselves", body = ErrorBody),
        (status = 404, description = "No user has this login", body = ErrorBody)
    )
)]
pub(crate) async fn override_kube_user(
    State(state): State<ApiState>,
    identity: crate::identity::session::Identity,
    _admin: crate::authz::guard::AdminGuard,
    Path(login): Path<String>,
    Json(body): Json<OverrideBody>,
) -> Result<Response, AppError> {
    let caller = identity.as_deref().map(|c| c.trim().to_lowercase());
    if caller.as_deref() == Some(login.trim().to_lowercase().as_str()) {
        return Ok(forbidden(
            "an administrator cannot set their own kubernetes username; another administrator must",
        ));
    }
    let user = match body.kube_user.as_deref().map(KubeUser::parse).transpose() {
        Ok(u) => u,
        Err(e) => return Ok(bad_request(e.to_string())),
    };
    if !kube_identity::set_override(state.db.pool(), &login, user.as_ref()).await? {
        return Ok(not_found(format!("no user has login {login}")));
    }
    let note = match &user {
        Some(u) => format!("kubernetes username override set to {}", u.as_str()),
        None => "kubernetes username override cleared".to_string(),
    };
    state
        .db
        .events()
        .append(
            &crate::event_log::Event::now(
                &format!("user:{}", login.trim().to_lowercase()),
                "kube-user",
                "kube-user",
                Some(&note),
                None,
            )
            .by(identity.as_deref()),
        )
        .await?;
    tracing::info!(
        login,
        kube_user = user.as_ref().map(KubeUser::as_str),
        by = identity.as_deref(),
        "kubernetes username override set"
    );
    Ok(Json(KubeUserDto {
        r#override: user.map(|u| u.as_str().to_string()),
    })
    .into_response())
}

#[cfg(test)]
mod tests {
    use crate::identity::auth::{AuthPath, Roles};
    use crate::identity::session::{IdentitySource, Resolved};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use std::sync::Arc;
    use tower::ServiceExt;

    struct NoSink;
    impl crate::daemon::queue::OverrideSink for NoSink {
        fn submit(&self, _ov: crate::daemon::queue::Override) {}
    }

    /// The API router, with the identity routes on, as `require_auth` admitted `login` on `path`
    /// with issuer subject `sub`.
    async fn app(pool: &PgPool, login: &str, sub: Option<&str>, path: AuthPath) -> axum::Router {
        let state = crate::api::state::ApiState {
            roles: Roles::new(vec!["admin".to_string()], vec![], vec![]),
            identities: true,
            ..crate::api::state::ApiState::test(crate::Db::new(pool.clone()), Arc::new(NoSink))
        };
        let resolved = Resolved {
            user: Some(login.to_string()),
            groups: Vec::new(),
            sub: sub.map(str::to_string),
            source: IdentitySource::Native,
        };
        crate::api::router(state).layer(axum::middleware::from_fn(
            move |mut req: axum::extract::Request, next: axum::middleware::Next| {
                let resolved = resolved.clone();
                async move {
                    req.extensions_mut().insert(path);
                    req.extensions_mut().insert(resolved);
                    next.run(req).await
                }
            },
        ))
    }

    async fn send(
        app: axum::Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn signed_in(pool: &PgPool, sub: &str, login: &str) {
        crate::identity::oidc::users::record_login(pool, sub, login, None, jiff::Timestamp::now())
            .await
            .unwrap();
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn only_an_administrator_overrides_and_the_override_wins(pool: PgPool) {
        signed_in(&pool, "sub-alice", "alice").await;
        signed_in(&pool, "sub-admin", "admin").await;
        let body = Some(json!({"kube_user": "alice-k8s"}));
        let (status, _) = send(
            app(&pool, "alice", Some("sub-alice"), AuthPath::Session).await,
            "PUT",
            "/api/users/alice/kube-user",
            body.clone(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a person cannot override their own"
        );

        let admin = || app(&pool, "admin", Some("sub-admin"), AuthPath::Session);
        let (status, got) = send(
            admin().await,
            "PUT",
            "/api/users/alice/kube-user",
            body.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{got}");
        assert_eq!(got, json!({"override": "alice-k8s"}));

        let (status, _) = send(
            admin().await,
            "PUT",
            "/api/users/admin/kube-user",
            Some(json!({"kube_user": "admin-k8s"})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "not even an administrator overrides themselves"
        );
        let audit = crate::event_log::export_string(&pool).await.unwrap();
        assert!(
            audit.contains("user:alice")
                && audit.contains("kubernetes username override set to alice-k8s"),
            "{audit}"
        );

        let (status, _) = send(admin().await, "PUT", "/api/users/nobody/kube-user", body).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send(
            admin().await,
            "PUT",
            "/api/users/alice/kube-user",
            Some(json!({"kube_user": "system:serviceaccount:x:y"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, got) = send(
            admin().await,
            "PUT",
            "/api/users/alice/kube-user",
            Some(json!({"kube_user": null})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(got, json!({"override": null}));
    }
}
