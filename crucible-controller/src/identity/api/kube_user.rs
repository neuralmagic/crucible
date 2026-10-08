//! The Kubernetes username agent tokens carry as `kube_user` for runs a person launches.
//!
//! A person links theirs by presenting a cluster token (`oc whoami -t`); the controller asks the
//! API server's `users/~` who it belongs to and records only that answer. A platform administrator
//! may override it. Neither route accepts an API key, for the reason the keys routes don't: a key
//! should not reshape the identity of the person who minted it.

use crate::api::dto::{bad_request, forbidden, not_found};
use crate::api::state::{ApiState, AppError, ErrorBody, Json};
use crate::identity::auth::{AuthPath, BearerGuard, Subject};
use crate::identity::kube_identity::{self, KubeIdentity, KubeUser};
use crate::identity::kube_user::LookupError;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::ToSchema;

/// A person's Kubernetes username, as learned and as overridden.
#[derive(Debug, Serialize, ToSchema, PartialEq)]
pub struct KubeUserDto {
    /// The username agent tokens carry: the override when set, else the learned one.
    pub kube_user: Option<String>,
    /// What the API server said the person's cluster token belongs to.
    pub learned: Option<String>,
    /// A platform administrator's override.
    pub r#override: Option<String>,
}

impl From<KubeIdentity> for KubeUserDto {
    fn from(id: KubeIdentity) -> Self {
        Self {
            kube_user: id.effective().map(str::to_string),
            learned: id.learned,
            r#override: id.r#override,
        }
    }
}

/// A cluster token whose owner the API server names.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LinkBody {
    pub cluster_token: String,
}

/// The override to set, or `null` to clear it.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OverrideBody {
    pub kube_user: Option<String>,
}

fn no_subject() -> Response {
    forbidden("this credential names no issuer subject, so it has no user row to link")
}

fn not_from_a_key() -> Response {
    forbidden("api keys cannot change their owner's kubernetes username; sign in to link one")
}

fn unavailable(why: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorBody::new(why.to_string())),
    )
        .into_response()
}

#[utoipa::path(
    get,
    path = "/api/me/kube-user",
    responses(
        (status = 200, description = "The caller's Kubernetes username, learned and overridden", body = KubeUserDto),
        (status = 403, description = "The caller has no issuer subject", body = ErrorBody)
    )
)]
pub(crate) async fn get_my_kube_user(
    State(state): State<ApiState>,
    subject: Subject,
) -> Result<Response, AppError> {
    let Some(sub) = subject.as_deref() else {
        return Ok(no_subject());
    };
    let identity = kube_identity::get_for_sub(state.db.pool(), sub)
        .await?
        .unwrap_or(KubeIdentity {
            learned: None,
            r#override: None,
        });
    Ok(Json(KubeUserDto::from(identity)).into_response())
}

#[utoipa::path(
    put,
    path = "/api/me/kube-user",
    request_body = LinkBody,
    responses(
        (status = 200, description = "The API server named the token's owner; it is now the caller's Kubernetes username", body = KubeUserDto),
        (status = 400, description = "The API server refused the token, or it belongs to a system identity", body = ErrorBody),
        (status = 403, description = "The caller has no issuer subject, or presented an api key", body = ErrorBody),
        (status = 503, description = "This deployment cannot ask the API server, or the API server did not answer", body = ErrorBody)
    )
)]
pub(crate) async fn link_my_kube_user(
    State(state): State<ApiState>,
    subject: Subject,
    path: AuthPath,
    guard: Option<axum::Extension<Arc<BearerGuard>>>,
    Json(body): Json<LinkBody>,
) -> Result<Response, AppError> {
    if path == AuthPath::ApiKey {
        return Ok(not_from_a_key());
    }
    let Some(sub) = subject.as_deref() else {
        return Ok(no_subject());
    };
    let Some(kube) = guard.as_ref().and_then(|g| g.kube.as_ref()) else {
        return Ok(unavailable(
            "this deployment does not resolve cluster tokens (CONTROLLER_KUBE_USER_AUTH is off)",
        ));
    };
    let user = match kube.lookup(body.cluster_token.trim()).await {
        Ok(user) => user,
        Err(LookupError::Unauthorized) => {
            return Ok(bad_request(
                "the API server did not accept that cluster token",
            ));
        }
        Err(LookupError::Unavailable(why)) => {
            tracing::warn!(error = %why, "linking a kubernetes username failed closed");
            return Ok(unavailable(
                "the cluster API could not verify the token; retry",
            ));
        }
    };
    let user = match KubeUser::parse(&user) {
        Ok(u) => u,
        Err(e) => return Ok(bad_request(e.to_string())),
    };
    if !kube_identity::learn_for_sub(state.db.pool(), sub, &user).await? {
        return Ok(no_subject());
    }
    tracing::info!(sub, kube_user = user.as_str(), "kubernetes username linked");
    let identity = kube_identity::get_for_sub(state.db.pool(), sub).await?;
    Ok(Json(KubeUserDto::from(identity.unwrap_or(KubeIdentity {
        learned: Some(user.as_str().to_string()),
        r#override: None,
    })))
    .into_response())
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
    let stored = kube_identity::get(state.db.pool(), &login).await?;
    Ok(Json(KubeUserDto::from(stored.unwrap_or(KubeIdentity {
        learned: None,
        r#override: user.map(|u| u.as_str().to_string()),
    })))
    .into_response())
}

#[cfg(test)]
mod tests {
    use crate::identity::api::kube_user::*;
    use crate::identity::auth::{AuthMode, Roles};
    use crate::identity::session::{IdentitySource, Resolved};
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tower::ServiceExt;

    struct NoSink;
    impl crate::daemon::queue::OverrideSink for NoSink {
        fn submit(&self, _ov: crate::daemon::queue::Override) {}
    }

    /// A stand-in API server: `sha256~mine` belongs to alice-k8s, `sha256~admin` to system:admin.
    async fn users_tilde() -> crate::identity::kube_user::KubeUserAuth {
        let app = axum::Router::new().route(
            "/apis/user.openshift.io/v1/users/~",
            axum::routing::get(|headers: axum::http::HeaderMap| async move {
                match headers.get("authorization").and_then(|v| v.to_str().ok()) {
                    Some("Bearer sha256~mine") => {
                        (StatusCode::OK, r#"{"metadata":{"name":"alice-k8s"}}"#)
                    }
                    Some("Bearer sha256~admin") => {
                        (StatusCode::OK, r#"{"metadata":{"name":"system:admin"}}"#)
                    }
                    _ => (StatusCode::UNAUTHORIZED, ""),
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        crate::identity::kube_user::KubeUserAuth::new(&url, std::path::Path::new("/nonexistent"))
            .unwrap()
    }

    /// The API router as `human_router` serves it to `login`, who `require_auth` admitted on
    /// `path` with issuer subject `sub`.
    async fn app(pool: &PgPool, login: &str, sub: Option<&str>, path: AuthPath) -> axum::Router {
        let state = crate::api::state::ApiState {
            roles: Roles::new(vec!["admin".to_string()], vec![], vec![]),
            ..crate::api::state::ApiState::test(crate::Db::new(pool.clone()), Arc::new(NoSink))
        };
        let guard = Arc::new(BearerGuard {
            kube: Some(users_tilde().await),
            ..BearerGuard::new(None, None, None, None, AuthMode::Native).unwrap()
        });
        let resolved = Resolved {
            user: Some(login.to_string()),
            groups: Vec::new(),
            sub: sub.map(str::to_string),
            source: IdentitySource::Native,
        };
        crate::api::router(state)
            .layer(axum::middleware::from_fn(
                move |mut req: axum::extract::Request, next: axum::middleware::Next| {
                    let resolved = resolved.clone();
                    async move {
                        req.extensions_mut().insert(path);
                        req.extensions_mut().insert(resolved);
                        next.run(req).await
                    }
                },
            ))
            .layer(axum::Extension(guard))
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
    async fn a_person_links_the_username_the_api_server_names(pool: PgPool) {
        signed_in(&pool, "sub-alice", "alice").await;
        let me = || app(&pool, "alice", Some("sub-alice"), AuthPath::Session);
        assert_eq!(
            send(me().await, "GET", "/api/me/kube-user", None).await,
            (
                StatusCode::OK,
                json!({"kube_user": null, "learned": null, "override": null})
            )
        );
        let (status, body) = send(
            me().await,
            "PUT",
            "/api/me/kube-user",
            Some(json!({"cluster_token": "sha256~mine"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body,
            json!({"kube_user": "alice-k8s", "learned": "alice-k8s", "override": null})
        );
        assert_eq!(
            crate::identity::kube_identity::effective(&pool, "alice")
                .await
                .unwrap()
                .unwrap()
                .as_str(),
            "alice-k8s"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn linking_refuses_bad_tokens_system_identities_keys_and_subjectless_callers(
        pool: PgPool,
    ) {
        signed_in(&pool, "sub-alice", "alice").await;
        let put = |token: &str| Some(json!({"cluster_token": token}));
        let session = || app(&pool, "alice", Some("sub-alice"), AuthPath::Session);
        let cases = [
            (
                "a token the API server refuses",
                session().await,
                put("sha256~nope"),
                StatusCode::BAD_REQUEST,
            ),
            (
                "a system identity's token",
                session().await,
                put("sha256~admin"),
                StatusCode::BAD_REQUEST,
            ),
            (
                "an api key",
                app(&pool, "alice", Some("sub-alice"), AuthPath::ApiKey).await,
                put("sha256~mine"),
                StatusCode::FORBIDDEN,
            ),
            (
                "no issuer subject",
                app(&pool, "alice", None, AuthPath::Edge).await,
                put("sha256~mine"),
                StatusCode::FORBIDDEN,
            ),
            (
                "an unknown field",
                session().await,
                Some(json!({"cluster_token": "sha256~mine", "kube_user": "root"})),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
        ];
        for (what, app, body, want) in cases {
            let (status, body) = send(app, "PUT", "/api/me/kube-user", body).await;
            assert_eq!(status, want, "{what}: {body}");
        }
        assert_eq!(
            crate::identity::kube_identity::effective(&pool, "alice")
                .await
                .unwrap(),
            None,
            "nothing was recorded"
        );
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
        assert_eq!(
            got,
            json!({"kube_user": "alice-k8s", "learned": null, "override": "alice-k8s"})
        );

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
        assert_eq!(got["kube_user"], Value::Null);
    }
}
