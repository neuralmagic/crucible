//! Linking the caller's GitHub account by presenting one of its tokens. The controller asks
//! GitHub whose token it is, records that account as the caller's, and forgets the token, unless
//! it dispatches GitHub workflows with the person's own access, where it keeps the token in
//! memory for that.

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::api::dto::{bad_request, conflict, forbidden, not_found, unavailable};
use crate::api::state::{ApiResult, ApiState, ErrorBody, Json};
use crate::authz::Caller;
use crate::authz::model::Principal;
use crate::identity::github_links::{self, GithubLink, LinkError};

pub(crate) const GITHUB_LINK_PATH: &str = "/api/me/github";

#[derive(Debug, Deserialize, ToSchema)]
pub struct GithubLinkBody {
    pub token: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct GithubLinkDto {
    pub login: String,
    pub id: u64,
}

fn person(caller: &Caller) -> Result<&str, &'static str> {
    match (&caller.acting_as, caller.principals.user()) {
        (None, Some(Principal::User(login))) => Ok(login),
        (Some(_), _) => Err("a GitHub account links to a person; stop acting as a team first"),
        _ => Err("this credential names no person to link"),
    }
}

fn keeps_token(state: &ApiState) -> bool {
    state.calls_github == Some(crate::calls::GithubToken::Person)
}

#[utoipa::path(
    get,
    path = "/api/me/github",
    responses(
        (status = 200, description = "The caller's linked GitHub account", body = GithubLinkDto),
        (status = 404, description = "The caller linked none", body = ErrorBody)
    )
)]
pub(crate) async fn get_github_link(
    State(state): State<ApiState>,
    caller: Caller,
) -> ApiResult<Response> {
    let owner = match person(&caller) {
        Ok(owner) => owner,
        Err(why) => return Ok(forbidden(why)),
    };
    Ok(match github_links::get(state.db.pool(), owner).await? {
        Some(link) => Json(GithubLinkDto {
            login: link.github_login,
            id: link.github_id,
        })
        .into_response(),
        None => not_found("no GitHub account is linked"),
    })
}

#[utoipa::path(
    put,
    path = "/api/me/github",
    request_body = GithubLinkBody,
    responses(
        (status = 200, description = "GitHub named the token's account; it is now the caller's", body = GithubLinkDto),
        (status = 400, description = "GitHub refused the token", body = ErrorBody),
        (status = 403, description = "The caller is not a person, or acts as a team", body = ErrorBody),
        (status = 409, description = "The account is linked to another person", body = ErrorBody),
        (status = 503, description = "GitHub could not be asked", body = ErrorBody)
    )
)]
pub(crate) async fn put_github_link(
    State(state): State<ApiState>,
    caller: Caller,
    Json(body): Json<GithubLinkBody>,
) -> ApiResult<Response> {
    let owner = match person(&caller) {
        Ok(owner) => owner,
        Err(why) => return Ok(forbidden(why)),
    };
    let secret = body.token.trim();
    if secret.is_empty() || secret.contains(char::is_whitespace) {
        return Ok(bad_request("token is one GitHub token"));
    }
    let client = forge::github::Client::new().map_err(anyhow::Error::from)?;
    let user = match client.user(&forge::github::Token::new(secret)).await {
        Ok(user) => user,
        Err(e) if e.is_transient() => {
            tracing::warn!(error = %e, "reading a GitHub token's account failed");
            return Ok(unavailable("GitHub could not be asked; retry"));
        }
        Err(e) => return Ok(bad_request(format!("GitHub refused the token: {e}"))),
    };
    let link = GithubLink {
        github_id: user.id,
        github_login: user.login,
    };
    match github_links::link(state.db.pool(), owner, &link).await {
        Ok(()) => {}
        Err(e @ LinkError::Taken { .. }) => return Ok(conflict(e.to_string())),
        Err(LinkError::Store(e)) => return Err(e.into()),
    }
    if keeps_token(&state) {
        state.github_signed_in.set(Some(secret.to_owned()));
    }
    Ok(Json(GithubLinkDto {
        login: link.github_login,
        id: link.github_id,
    })
    .into_response())
}

#[utoipa::path(
    delete,
    path = "/api/me/github",
    responses(
        (status = 204, description = "The caller's GitHub account is no longer linked"),
        (status = 404, description = "The caller linked none", body = ErrorBody)
    )
)]
pub(crate) async fn delete_github_link(
    State(state): State<ApiState>,
    caller: Caller,
) -> ApiResult<Response> {
    let owner = match person(&caller) {
        Ok(owner) => owner,
        Err(why) => return Ok(forbidden(why)),
    };
    if keeps_token(&state) {
        state.github_signed_in.set(None);
    }
    Ok(match github_links::unlink(state.db.pool(), owner).await? {
        true => axum::http::StatusCode::NO_CONTENT.into_response(),
        false => not_found("no GitHub account is linked"),
    })
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use sqlx::PgPool;

    use crate::api::state::ApiState;
    use crate::client::Db;
    use crate::identity::api::github_link::*;

    async fn send(
        app: &axum::Router,
        method: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, String) {
        let req = Request::builder()
            .method(method)
            .uri(GITHUB_LINK_PATH)
            .header("x-auth-request-user", "wren")
            .header("content-type", "application/json");
        let req = match body {
            Some(body) => req.body(Body::from(body.to_string())),
            None => req.body(Body::empty()),
        }
        .expect("request");
        let (status, bytes) = crate::testing::oneshot_bytes(app, req).await;
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    fn person_state(pool: PgPool) -> ApiState {
        ApiState {
            calls_github: Some(crate::calls::GithubToken::Person),
            ..ApiState::test(Db::new(pool))
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_linked_account_is_read_and_unlinked_by_its_person(pool: PgPool) {
        let state = person_state(pool.clone());
        state.github_signed_in.set(Some("ghu_kept".into()));
        let app = crate::api::router(state.clone());

        let (status, _) = send(&app, "GET", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        github_links::link(
            &pool,
            "wren",
            &GithubLink {
                github_id: 7,
                github_login: "wren-gh".into(),
            },
        )
        .await
        .unwrap();
        let (status, body) = send(&app, "GET", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let dto: GithubLinkDto = serde_json::from_str(&body).unwrap();
        assert_eq!(
            dto,
            GithubLinkDto {
                login: "wren-gh".into(),
                id: 7
            }
        );

        let (status, _) = send(&app, "DELETE", None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(state.github_signed_in.token().is_none());
        let (status, _) = send(&app, "DELETE", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_link_needs_one_token(pool: PgPool) {
        let state = person_state(pool);
        let app = crate::api::router(state.clone());
        for token in ["", "  ", "two tokens"] {
            let (status, body) = send(&app, "PUT", Some(serde_json::json!({"token": token}))).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{token:?}: {body}");
        }
        assert!(state.github_signed_in.token().is_none());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_controller_dispatching_with_the_app_never_keeps_a_token(pool: PgPool) {
        let mut state = person_state(pool);
        assert!(keeps_token(&state));
        state.calls_github = Some(crate::calls::GithubToken::AppInstallation);
        assert!(!keeps_token(&state));
        state.calls_github = None;
        assert!(!keeps_token(&state));
    }
}
