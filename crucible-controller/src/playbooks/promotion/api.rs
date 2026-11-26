//! `POST /api/playbook-drafts/{id}/promote` and `GET /api/playbook-drafts/{id}/promotion`.

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use utoipa::ToSchema;

use crate::api::dto::{bad_request, forbidden, not_found, unavailable, unprocessable};
use crate::api::state::{ApiError, ApiState, ErrorBody, Json};
use crate::authz::Caller;
use crate::authz::action::Verb;
use crate::identity::api::forge::{Whoami, issued_here, person, whoami};
use crate::identity::forge::{Forge, ForgeHost};
use crate::playbooks::promotion::forge::{ForgeError, Target, Write, propose};
use crate::playbooks::promotion::{Promotion, PromotionDto, PromotionState, branch_for, store};

const FIRST_LOOK: jiff::SignedDuration = jiff::SignedDuration::from_secs(10);

#[derive(Debug, Deserialize, ToSchema)]
pub struct PromoteBody {
    pub forge: Forge,
    pub host: String,
    pub project: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub base: Option<String>,
    pub token: String,
}

enum Unserved {
    BadHost(String),
    Elsewhere(String),
}

impl Unserved {
    fn response(self) -> Response {
        match self {
            Unserved::BadHost(msg) => bad_request(msg),
            Unserved::Elsewhere(msg) => not_found(msg),
        }
    }
}

fn served(state: &ApiState, forge: Forge, host: &str) -> Result<ForgeHost, Unserved> {
    let parsed = ForgeHost::parse(host)
        .ok_or_else(|| Unserved::BadHost(format!("{host:?} is not a host name")))?;
    let ok = match forge {
        Forge::Github => parsed == ForgeHost::github(),
        Forge::Gitlab => state.gitlab_hosts.contains(&parsed),
    };
    match ok {
        true => Ok(parsed),
        false => Err(Unserved::Elsewhere(format!(
            "this controller promotes to no {} host {}",
            forge.as_str(),
            parsed.as_str()
        ))),
    }
}

fn forge_failure(e: ForgeError) -> Response {
    match e {
        ForgeError::Refused(msg) => forbidden(msg),
        ForgeError::Unreachable(msg) => {
            (StatusCode::BAD_GATEWAY, Json(ErrorBody::new(msg))).into_response()
        }
    }
}

#[allow(clippy::result_large_err)]
async fn linked_account(
    state: &ApiState,
    owner: &str,
    forge: Forge,
    host: &ForgeHost,
    token: &str,
) -> Result<(u64, String), Response> {
    let linked = crate::identity::forge::links::get(state.db.pool(), owner, forge, host)
        .await
        .map_err(|e| ApiError::from(e).into_response())?
        .ok_or_else(|| {
            forbidden(format!(
                "you have no {} account on {} linked; sign in to it in the desktop app",
                forge.as_str(),
                host.as_str()
            ))
        })?;
    let asked = match issued_here(state, forge, host, token).await {
        Ok(()) => whoami(forge, host, token).await,
        Err(e) => Err(e),
    };
    match asked {
        Ok((account_id, login)) if account_id == linked.account_id => Ok((account_id, login)),
        Ok((_, login)) => Err(forbidden(format!(
            "the token is {login}'s, but your linked {} account on {} is {}",
            forge.as_str(),
            host.as_str(),
            linked.login
        ))),
        Err(Whoami::Unreachable(e)) => Err(unavailable(format!(
            "{} could not be asked: {e}",
            host.as_str()
        ))),
        Err(Whoami::Refused(e)) => Err(forbidden(e)),
    }
}

#[utoipa::path(
    post,
    path = "/api/playbook-drafts/{id}/promote",
    params(("id" = String, Path, description = "Draft id")),
    request_body = PromoteBody,
    responses(
        (status = 200, description = "The pull or merge request the promotion opened or updated", body = PromotionDto),
        (status = 400, description = "A malformed host", body = ErrorBody),
        (status = 403, description = "The caller may not approve the draft, the token is not their linked account, or the account may not push to the target", body = ErrorBody),
        (status = 404, description = "No draft, or a forge host this controller does not promote to", body = ErrorBody),
        (status = 409, description = "The draft retired", body = ErrorBody),
        (status = 422, description = "A bad path, or nothing that compiled to promote", body = ErrorBody),
        (status = 502, description = "The forge could not be reached", body = ErrorBody)
    )
)]
pub(crate) async fn promote_playbook_draft(
    State(state): State<ApiState>,
    caller: Caller,
    Path(id): Path<String>,
    Json(body): Json<PromoteBody>,
) -> Response {
    let draft =
        match crate::playbooks::api::drafts::readable_draft(&state, &caller, &id, Verb::Approve)
            .await
        {
            Ok(draft) => draft,
            Err(refused) => return refused,
        };
    let owner = match person(&caller) {
        Ok(owner) => owner.to_owned(),
        Err(why) => return forbidden(why),
    };
    if draft.retired_at.is_some() {
        return (
            StatusCode::CONFLICT,
            Json(ErrorBody::new(format!(
                "draft {id} retired when its promoted pack was registered"
            ))),
        )
            .into_response();
    }
    let host = match served(&state, body.forge, &body.host) {
        Ok(host) => host,
        Err(unserved) => return unserved.response(),
    };
    let token = body.token.trim();
    if token.is_empty() || token.contains(char::is_whitespace) {
        return bad_request("token is one forge token");
    }
    let path = body.path.trim().trim_matches('/').to_owned();
    if let Err(why) = crate::playbooks::registry::validate_path(&path) {
        return unprocessable(why);
    }
    let (account_id, login) = match linked_account(&state, &owner, body.forge, &host, token).await {
        Ok(who) => who,
        Err(refused) => return refused,
    };
    let (version, tree) =
        match crate::playbooks::drafts::newest_compiling(state.db.pool(), &id).await {
            Ok(newest) => newest,
            Err(e) => return crate::playbooks::api::drafts::draft_refusal(e),
        };
    let files: BTreeMap<String, Vec<u8>> = tree
        .files()
        .iter()
        .map(|(path, bytes)| (path.as_str().to_owned(), bytes.clone()))
        .collect();
    let digest = tree.digest().to_string();
    let target = match Target::new(body.forge, &host, &body.project) {
        Ok(target) => target,
        Err(e) => return forge_failure(e),
    };
    let branch = branch_for(&id);
    let title = format!("Add pack {id}");
    let message = format!("Add pack {id} (draft version {version})");
    let note = format!(
        "Promoted from draft `{id}` version {version} (`{digest}`). Merging registers the pack \
         from `{}` and retires the draft.\n",
        match path.as_str() {
            "" => "the repository root",
            path => path,
        }
    );
    let write = Write {
        base: body
            .base
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty()),
        branch: &branch,
        path: &path,
        files: &files,
        message: &message,
        title: &title,
        body: &note,
    };
    let opened = match propose(&target, account_id, token, &write).await {
        Ok(opened) => opened,
        Err(e) => return forge_failure(e),
    };
    let now = jiff::Timestamp::now();
    let first_look = match now.checked_add(FIRST_LOOK) {
        Ok(at) => crate::clock::stamp(at),
        Err(e) => return ApiError::from(anyhow::Error::new(e)).into_response(),
    };
    let promotion = Promotion {
        draft: id.clone(),
        forge: body.forge,
        host,
        project: body.project.trim().to_owned(),
        path,
        base: opened.base,
        branch,
        number: opened.number,
        url: opened.url,
        version,
        digest,
        author: login,
        promoted_by: owner.clone(),
        state: PromotionState::Open,
        note: String::new(),
        etag: None,
        created_at: crate::clock::stamp(now),
        updated_at: crate::clock::stamp(now),
    };
    if let Err(e) = store::record(state.db.pool(), &promotion, &first_look).await {
        return ApiError::from(e).into_response();
    }
    state
        .audit(
            crate::event_log::Event::now(
                &format!("draft:{id}"),
                "promoted",
                "promoted",
                Some(&format!(
                    "draft {id} version {} promoted as {}",
                    promotion.version, promotion.url
                )),
                None,
            )
            .by(Some(&owner)),
            "promote_playbook_draft",
        )
        .await;
    Json(PromotionDto::from(promotion)).into_response()
}

#[utoipa::path(
    get,
    path = "/api/playbook-drafts/{id}/promotion",
    params(("id" = String, Path, description = "Draft id")),
    responses(
        (status = 200, description = "The draft's promotion", body = PromotionDto),
        (status = 404, description = "No draft, or it was never promoted", body = ErrorBody)
    )
)]
pub(crate) async fn get_playbook_draft_promotion(
    State(state): State<ApiState>,
    caller: Caller,
    Path(id): Path<String>,
) -> Response {
    if let Err(refused) =
        crate::playbooks::api::drafts::readable_draft(&state, &caller, &id, Verb::Read).await
    {
        return refused;
    }
    match store::get(state.db.pool(), &id).await {
        Ok(Some(promotion)) => Json(PromotionDto::from(promotion)).into_response(),
        Ok(None) => not_found(format!("draft {id} was never promoted")),
        Err(e) => ApiError::from(e).into_response(),
    }
}
