//! git's smart-HTTP protocol at `/git/<owner>/<repo>.git/...`, for callers that authenticate as
//! the run (sandboxes): rewritten to the github.com path and handed to the [`Relay`].

use crate::relay::{Caller, Relay};
use crate::{SCOPE_READ, SCOPE_WRITE};
use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use crucible_mcp_auth::principal::Principal;
use std::sync::Arc;

pub fn router(relay: Arc<Relay>) -> Router {
    Router::new()
        .route("/git/{owner}/{repo}/{*rest}", any(proxy))
        .with_state(relay)
}

/// Which of the three endpoints, and whether it writes.
pub fn endpoint(rest: &str, query: Option<&str>) -> Option<&'static str> {
    let service = query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|kv| kv.strip_prefix("service="));
    match (rest, service) {
        ("info/refs", Some("git-upload-pack")) | ("git-upload-pack", None) => Some(SCOPE_READ),
        ("info/refs", Some("git-receive-pack")) | ("git-receive-pack", None) => Some(SCOPE_WRITE),
        _ => None,
    }
}

pub fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn refuse(status: StatusCode, why: impl Into<String>) -> Response {
    (status, why.into()).into_response()
}

async fn proxy(
    State(relay): State<Arc<Relay>>,
    Path((owner, repo, rest)): Path<(String, String, String)>,
    mut req: Request,
) -> Response {
    let repo = repo.strip_suffix(".git").unwrap_or(&repo).to_string();
    if !is_name(&owner) || !is_name(&repo) {
        return refuse(StatusCode::BAD_REQUEST, "not a repository path");
    }
    let Some(principal) = req.extensions().get::<Principal>().cloned() else {
        return refuse(StatusCode::UNAUTHORIZED, "no authenticated caller");
    };
    let path = match req.uri().query() {
        Some(q) => format!("/{owner}/{repo}.git/{rest}?{q}"),
        None => format!("/{owner}/{repo}.git/{rest}"),
    };
    match path.parse() {
        Ok(uri) => *req.uri_mut() = uri,
        Err(_) => return refuse(StatusCode::BAD_REQUEST, "not a repository path"),
    }
    relay
        .relay("github.com", req, Caller::Run(&principal))
        .await
}
