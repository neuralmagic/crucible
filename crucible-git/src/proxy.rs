//! git's smart-HTTP protocol, passed through to GitHub with the run's token added. The three
//! endpoints are plain HTTP, so bodies stream through unread.

use crate::github::GitHub;
use crate::{SCOPE_READ, SCOPE_WRITE};
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use crucible_mcp_auth::principal::Principal;
use futures_util::TryStreamExt;
use std::sync::Arc;

/// Request headers git sends that GitHub needs to see.
const FORWARD_REQUEST: [HeaderName; 4] = [
    header::CONTENT_TYPE,
    header::CONTENT_ENCODING,
    header::ACCEPT,
    HeaderName::from_static("git-protocol"),
];

/// Response headers git reads.
const FORWARD_RESPONSE: [HeaderName; 3] = [
    header::CONTENT_TYPE,
    header::CONTENT_ENCODING,
    header::CACHE_CONTROL,
];

pub fn router(gh: Arc<GitHub>) -> Router {
    Router::new()
        .route("/git/{owner}/{repo}/{*rest}", any(proxy))
        .with_state(gh)
}

/// Which of the three endpoints, and whether it writes.
fn endpoint(rest: &str, query: Option<&str>) -> Option<&'static str> {
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

fn is_name(s: &str) -> bool {
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
    State(gh): State<Arc<GitHub>>,
    Path((owner, repo, rest)): Path<(String, String, String)>,
    req: Request,
) -> Response {
    let repo = repo.strip_suffix(".git").unwrap_or(&repo).to_string();
    if !is_name(&owner) || !is_name(&repo) {
        return refuse(StatusCode::BAD_REQUEST, "not a repository path");
    }
    let query = req.uri().query().map(str::to_string);
    let Some(scope) = endpoint(&rest, query.as_deref()) else {
        return refuse(
            StatusCode::NOT_FOUND,
            "only git's smart-HTTP endpoints are served",
        );
    };
    let Some(principal) = req.extensions().get::<Principal>() else {
        return refuse(StatusCode::UNAUTHORIZED, "no authenticated caller");
    };
    if !principal.missing_scopes(&[scope.to_string()]).is_empty() {
        return refuse(StatusCode::FORBIDDEN, format!("this call needs {scope}"));
    }
    let token = match gh.token().await {
        Ok(t) => t,
        Err(e) => return refuse(e.status(), e.to_string()),
    };
    let mut url = match gh.web.join(&format!("{owner}/{repo}.git/{rest}")) {
        Ok(u) => u,
        Err(e) => return refuse(StatusCode::BAD_REQUEST, e.to_string()),
    };
    url.set_query(query.as_deref());

    let (parts, body) = req.into_parts();
    let mut upstream = gh
        .http
        .request(parts.method, url)
        .header(header::AUTHORIZATION, GitHub::basic(&token));
    for name in FORWARD_REQUEST {
        if let Some(v) = parts.headers.get(&name) {
            upstream = upstream.header(name, v);
        }
    }
    let upstream = upstream.body(reqwest::Body::wrap_stream(body.into_data_stream()));
    let resp = match upstream.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(%owner, %repo, %rest, error = %e, "GitHub unreachable");
            return refuse(StatusCode::BAD_GATEWAY, format!("reaching GitHub: {e}"));
        }
    };
    tracing::info!(%owner, %repo, %rest, status = %resp.status(), "proxied");
    let mut out = Response::builder().status(resp.status());
    for name in FORWARD_RESPONSE {
        if let Some(v) = resp.headers().get(&name) {
            out = out.header(name, v);
        }
    }
    let stream = resp.bytes_stream().map_err(std::io::Error::other);
    out.body(Body::from_stream(stream))
        .unwrap_or_else(|e| refuse(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}
