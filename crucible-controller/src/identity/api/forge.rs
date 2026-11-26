//! Linking the caller's forge accounts (GitHub, GitLab hosts) by presenting one of their tokens.
//! The controller asks the forge whose token it is, records that account as the caller's, and
//! forgets the token, unless it acts with the person's own forge access, where it keeps the
//! token in memory for that.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::api::dto::{bad_request, conflict, forbidden, not_found, unavailable};
use crate::api::state::{ApiResult, ApiState, ErrorBody, Json};
use crate::authz::Caller;
use crate::authz::model::Principal;
use crate::identity::forge::links::{self, ForgeLink, LinkError};
use crate::identity::forge::{Forge, ForgeHost};

pub(crate) const FORGE_ACCOUNTS_PATH: &str = "/api/me/forge-accounts";
pub(crate) const FORGE_ACCOUNT_PATH: &str = "/api/me/forge-accounts/{forge}/{host}";

const FORGE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Deserialize, ToSchema)]
pub struct ForgeTokenBody {
    pub token: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ForgeAccountDto {
    pub forge: Forge,
    pub host: String,
    pub login: String,
    pub id: u64,
}

impl From<ForgeLink> for ForgeAccountDto {
    fn from(link: ForgeLink) -> Self {
        ForgeAccountDto {
            forge: link.forge,
            host: link.host.as_str().to_owned(),
            login: link.login,
            id: link.account_id,
        }
    }
}

fn person(caller: &Caller) -> Result<&str, &'static str> {
    match (&caller.acting_as, caller.principals.user()) {
        (None, Some(Principal::User(login))) => Ok(login),
        (Some(_), _) => Err("a forge account links to a person; stop acting as a team first"),
        _ => Err("this credential names no person to link"),
    }
}

fn keeps_tokens(state: &ApiState, forge: Forge) -> bool {
    match forge {
        Forge::Github => state.calls_github == Some(crate::calls::GithubToken::Person),
        Forge::Gitlab => state.calls_gitlab == Some(crate::calls::GitlabToken::Person),
    }
}

enum Unserved {
    NoForge(String),
    BadHost(String),
    NotLinked(Forge, ForgeHost),
}

impl Unserved {
    fn response(self) -> Response {
        match self {
            Unserved::NoForge(forge) => not_found(format!("no forge {forge:?}")),
            Unserved::BadHost(host) => bad_request(format!("{host:?} is not a host name")),
            Unserved::NotLinked(forge, host) => not_found(format!(
                "this controller links no {} accounts on {}",
                forge.as_str(),
                host.as_str()
            )),
        }
    }
}

fn account(state: &ApiState, forge: &str, host: &str) -> Result<(Forge, ForgeHost), Unserved> {
    let forge = match forge {
        "github" => Forge::Github,
        "gitlab" => Forge::Gitlab,
        other => return Err(Unserved::NoForge(other.to_owned())),
    };
    let Some(host) = ForgeHost::parse(host) else {
        return Err(Unserved::BadHost(host.to_owned()));
    };
    let served = match forge {
        Forge::Github => host == ForgeHost::github(),
        Forge::Gitlab => state.gitlab_hosts.contains(&host),
    };
    if !served {
        return Err(Unserved::NotLinked(forge, host));
    }
    Ok((forge, host))
}

enum Whoami {
    Refused(String),
    Unreachable(String),
}

async fn whoami(forge: Forge, host: &ForgeHost, token: &str) -> Result<(u64, String), Whoami> {
    match forge {
        Forge::Github => {
            let client =
                forge::github::Client::new().map_err(|e| Whoami::Unreachable(e.to_string()))?;
            match client.user(&forge::github::Token::new(token)).await {
                Ok(user) => Ok((user.id, user.login)),
                Err(e) if e.is_transient() => Err(Whoami::Unreachable(e.to_string())),
                Err(e) => Err(Whoami::Refused(e.to_string())),
            }
        }
        Forge::Gitlab => {
            match gitlab_api(host)?
                .user(&crucible_ci::token::BearerToken::new(token))
                .await
            {
                Ok(user) => Ok((user.id, user.username)),
                Err(e) if e.is_transient() => Err(Whoami::Unreachable(e.to_string())),
                Err(e) => Err(Whoami::Refused(e.to_string())),
            }
        }
    }
}

fn gitlab_api(host: &ForgeHost) -> Result<crucible_ci::gitlab::Gitlab, Whoami> {
    let http = reqwest::Client::builder()
        .timeout(FORGE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| Whoami::Unreachable(e.to_string()))?;
    crucible_ci::gitlab::Gitlab::new(http, &format!("https://{}", host.as_str()))
        .map_err(|e| Whoami::Refused(e.to_string()))
}

async fn issued_here(
    state: &ApiState,
    forge: Forge,
    host: &ForgeHost,
    token: &str,
) -> Result<(), Whoami> {
    let not_ours = |what: &str| {
        Whoami::Refused(format!(
            "the token was not issued by {what}; sign in from the desktop app"
        ))
    };
    match forge {
        Forge::Github if state.calls_github == Some(crate::calls::GithubToken::AppInstallation) => {
            let ours = state
                .pack_pr_app
                .as_ref()
                .and_then(|app| app.app_id().parse::<u64>().ok())
                .ok_or_else(|| Whoami::Refused("this controller has no GitHub App".to_owned()))?;
            let client =
                forge::github::Client::new().map_err(|e| Whoami::Unreachable(e.to_string()))?;
            match client
                .user_installations(&forge::github::Token::new(token))
                .await
            {
                Ok(installs) if installs.iter().any(|i| i.app_id == ours) => Ok(()),
                Ok(_) => Err(not_ours(
                    "this controller's GitHub App on an installation the account can reach",
                )),
                Err(e) if e.is_transient() => Err(Whoami::Unreachable(e.to_string())),
                Err(_) => Err(not_ours("this controller's GitHub App")),
            }
        }
        Forge::Gitlab if state.calls_gitlab == Some(crate::calls::GitlabToken::Bot) => {
            let ours = state.gitlab_client_ids.get(host).ok_or_else(|| {
                Whoami::Refused(format!(
                    "this controller names no OAuth application for {}",
                    host.as_str()
                ))
            })?;
            match gitlab_api(host)?
                .token_info(&crucible_ci::token::BearerToken::new(token))
                .await
            {
                Ok(info) if info.application.as_ref().is_some_and(|a| &a.uid == ours) => Ok(()),
                Ok(_) => Err(not_ours("this controller's GitLab application")),
                Err(e) if e.is_transient() => Err(Whoami::Unreachable(e.to_string())),
                Err(_) => Err(not_ours("this controller's GitLab application")),
            }
        }
        Forge::Github | Forge::Gitlab => Ok(()),
    }
}

#[utoipa::path(
    get,
    path = "/api/me/forge-accounts",
    responses(
        (status = 200, description = "The caller's linked forge accounts", body = [ForgeAccountDto]),
        (status = 403, description = "The caller is not a person, or acts as a team", body = ErrorBody)
    )
)]
pub(crate) async fn list_forge_accounts(
    State(state): State<ApiState>,
    caller: Caller,
) -> ApiResult<Response> {
    let owner = match person(&caller) {
        Ok(owner) => owner,
        Err(why) => return Ok(forbidden(why)),
    };
    let links = links::list(state.db.pool(), owner).await?;
    let out: Vec<ForgeAccountDto> = links.into_iter().map(ForgeAccountDto::from).collect();
    Ok(Json(out).into_response())
}

#[utoipa::path(
    put,
    path = "/api/me/forge-accounts/{forge}/{host}",
    request_body = ForgeTokenBody,
    params(
        ("forge" = String, Path, description = "github or gitlab"),
        ("host" = String, Path, description = "The forge host, such as github.com")
    ),
    responses(
        (status = 200, description = "The forge named the token's account; it is now the caller's", body = ForgeAccountDto),
        (status = 400, description = "The forge refused the token, or it was not issued to this controller's forge application", body = ErrorBody),
        (status = 403, description = "The caller is not a person, or acts as a team", body = ErrorBody),
        (status = 404, description = "This controller links no accounts on that forge host", body = ErrorBody),
        (status = 409, description = "The account is linked to another person", body = ErrorBody),
        (status = 503, description = "The forge could not be asked", body = ErrorBody)
    )
)]
pub(crate) async fn put_forge_account(
    State(state): State<ApiState>,
    caller: Caller,
    Path((forge, host)): Path<(String, String)>,
    Json(body): Json<ForgeTokenBody>,
) -> ApiResult<Response> {
    let owner = match person(&caller) {
        Ok(owner) => owner,
        Err(why) => return Ok(forbidden(why)),
    };
    let (forge, host) = match account(&state, &forge, &host) {
        Ok(account) => account,
        Err(unserved) => return Ok(unserved.response()),
    };
    let secret = body.token.trim();
    if secret.is_empty() || secret.contains(char::is_whitespace) {
        return Ok(bad_request("token is one forge token"));
    }
    let asked = match issued_here(&state, forge, &host, secret).await {
        Ok(()) => whoami(forge, &host, secret).await,
        Err(e) => Err(e),
    };
    let (account_id, login) = match asked {
        Ok(who) => who,
        Err(Whoami::Unreachable(e)) => {
            tracing::warn!(
                forge = forge.as_str(),
                host = host.as_str(),
                error = e,
                "asking a forge whose token it is failed"
            );
            return Ok(unavailable(format!(
                "{} could not be asked; retry",
                host.as_str()
            )));
        }
        Err(Whoami::Refused(e)) => {
            return Ok(bad_request(format!(
                "{} refused the token: {e}",
                host.as_str()
            )));
        }
    };
    let link = ForgeLink {
        forge,
        host,
        account_id,
        login,
    };
    match links::link(state.db.pool(), owner, &link).await {
        Ok(()) => {}
        Err(e @ LinkError::Taken { .. }) => return Ok(conflict(e.to_string())),
        Err(LinkError::Store(e)) => return Err(e.into()),
    }
    if keeps_tokens(&state, link.forge) {
        state
            .forge_signed_in
            .set(link.forge, &link.host, Some(secret.to_owned()));
    }
    Ok(Json(ForgeAccountDto::from(link)).into_response())
}

#[utoipa::path(
    delete,
    path = "/api/me/forge-accounts/{forge}/{host}",
    params(
        ("forge" = String, Path, description = "github or gitlab"),
        ("host" = String, Path, description = "The forge host, such as github.com")
    ),
    responses(
        (status = 204, description = "The account is no longer linked"),
        (status = 404, description = "The caller linked none there", body = ErrorBody)
    )
)]
pub(crate) async fn delete_forge_account(
    State(state): State<ApiState>,
    caller: Caller,
    Path((forge, host)): Path<(String, String)>,
) -> ApiResult<Response> {
    let owner = match person(&caller) {
        Ok(owner) => owner,
        Err(why) => return Ok(forbidden(why)),
    };
    let (forge, host) = match account(&state, &forge, &host) {
        Ok(account) => account,
        Err(unserved) => return Ok(unserved.response()),
    };
    if keeps_tokens(&state, forge) {
        state.forge_signed_in.set(forge, &host, None);
    }
    Ok(
        match links::unlink(state.db.pool(), owner, forge, &host).await? {
            true => axum::http::StatusCode::NO_CONTENT.into_response(),
            false => not_found("no account is linked there"),
        },
    )
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use sqlx::PgPool;

    use crate::api::state::ApiState;
    use crate::client::Db;
    use crate::identity::api::forge::*;

    async fn send(
        app: &axum::Router,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, String) {
        let req = Request::builder()
            .method(method)
            .uri(path)
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
            calls_gitlab: Some(crate::calls::GitlabToken::Person),
            gitlab_hosts: std::sync::Arc::new(vec![
                ForgeHost::parse("gitlab.example.com").unwrap(),
            ]),
            ..ApiState::test(Db::new(pool))
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_controller_acting_for_people_links_only_its_own_apps_tokens(pool: PgPool) {
        let state = ApiState {
            calls_github: Some(crate::calls::GithubToken::AppInstallation),
            calls_gitlab: Some(crate::calls::GitlabToken::Bot),
            gitlab_hosts: std::sync::Arc::new(vec![
                ForgeHost::parse("gitlab.example.com").unwrap(),
            ]),
            ..ApiState::test(Db::new(pool.clone()))
        };
        let app = crate::api::router(state);
        let token = Some(serde_json::json!({"token": "gho_anything"}));
        let (status, body) = send(
            &app,
            "PUT",
            "/api/me/forge-accounts/github/github.com",
            token.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("no GitHub App"), "{body}");
        let (status, body) = send(
            &app,
            "PUT",
            "/api/me/forge-accounts/gitlab/gitlab.example.com",
            token,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("names no OAuth application"), "{body}");
        assert!(links::list(&pool, "wren").await.unwrap().is_empty());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn linked_accounts_are_listed_and_unlinked_by_their_person(pool: PgPool) {
        let state = person_state(pool.clone());
        let gl = ForgeHost::parse("gitlab.example.com").unwrap();
        state
            .forge_signed_in
            .set(Forge::Gitlab, &gl, Some("glpat_kept".into()));
        let app = crate::api::router(state.clone());

        let (status, body) = send(&app, "GET", FORGE_ACCOUNTS_PATH, None).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "[]"));

        links::link(
            &pool,
            "wren",
            &ForgeLink {
                forge: Forge::Gitlab,
                host: gl.clone(),
                account_id: 7,
                login: "wren-gl".into(),
            },
        )
        .await
        .unwrap();
        let (status, body) = send(&app, "GET", FORGE_ACCOUNTS_PATH, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let listed: Vec<ForgeAccountDto> = serde_json::from_str(&body).unwrap();
        assert_eq!(
            listed,
            vec![ForgeAccountDto {
                forge: Forge::Gitlab,
                host: "gitlab.example.com".into(),
                login: "wren-gl".into(),
                id: 7,
            }]
        );

        let path = "/api/me/forge-accounts/gitlab/gitlab.example.com";
        let (status, _) = send(&app, "DELETE", path, None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(state.forge_signed_in.token(Forge::Gitlab, &gl).is_none());
        let (status, _) = send(&app, "DELETE", path, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn only_served_forge_hosts_link_and_a_link_needs_one_token(pool: PgPool) {
        let state = person_state(pool);
        let app = crate::api::router(state.clone());
        let token = Some(serde_json::json!({"token": "x"}));
        for (path, want) in [
            (
                "/api/me/forge-accounts/gitlab/gitlab.com",
                StatusCode::NOT_FOUND,
            ),
            (
                "/api/me/forge-accounts/github/gitlab.example.com",
                StatusCode::NOT_FOUND,
            ),
            (
                "/api/me/forge-accounts/gitea/gitlab.example.com",
                StatusCode::NOT_FOUND,
            ),
            (
                "/api/me/forge-accounts/gitlab/bad%20host",
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let (status, body) = send(&app, "PUT", path, token.clone()).await;
            assert_eq!(status, want, "{path}: {body}");
        }
        for token in ["", "  ", "two tokens"] {
            let (status, body) = send(
                &app,
                "PUT",
                "/api/me/forge-accounts/github/github.com",
                Some(serde_json::json!({"token": token})),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{token:?}: {body}");
        }
        assert!(
            state
                .forge_signed_in
                .token(Forge::Github, &ForgeHost::github())
                .is_none()
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_controller_acting_with_the_app_never_keeps_a_token(pool: PgPool) {
        let mut state = person_state(pool);
        assert!(keeps_tokens(&state, Forge::Github));
        assert!(keeps_tokens(&state, Forge::Gitlab));
        state.calls_github = Some(crate::calls::GithubToken::AppInstallation);
        state.calls_gitlab = Some(crate::calls::GitlabToken::Bot);
        assert!(!keeps_tokens(&state, Forge::Github));
        assert!(!keeps_tokens(&state, Forge::Gitlab));
        state.calls_github = None;
        state.calls_gitlab = None;
        assert!(!keeps_tokens(&state, Forge::Github));
        assert!(!keeps_tokens(&state, Forge::Gitlab));
    }
}
