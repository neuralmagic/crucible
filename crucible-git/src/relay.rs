//! One request to GitHub, from either entry point: the `/git` routes sandboxes reach with run
//! tokens, and the TLS listener the run's tasks reach as `github.com`. The run's App token is lent
//! only to a request that carries none of its own or carries the placeholder, and only as far as
//! [`crate::policy`] allows. A request with a credential of its own is relayed with it, untouched:
//! `crucible-git` never widens what a caller could do anyway.
//!
//! ```text
//!   Authorization          policy   on refusal
//!   own credential         skipped  -
//!   placeholder            applied  403
//!   none                   applied  401 challenge, so git retries with the caller's credentials
//! ```

use crate::allow::Allow;
use crate::github::GitHub;
use crate::policy::{self, Verdict};
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use crucible_mcp_auth::principal::Principal;
use futures_util::TryStreamExt;
use http_body_util::BodyExt;
use std::sync::Arc;

/// The largest request body buffered for a decision (a GraphQL document).
const DECIDED_BODY_LIMIT: usize = 1024 * 1024;

/// Request headers never forwarded: hop-by-hop, framing reqwest redoes, and the proxy's own.
const DROPPED_REQUEST: [HeaderName; 7] = [
    header::HOST,
    header::CONTENT_LENGTH,
    header::CONNECTION,
    header::PROXY_AUTHORIZATION,
    header::TRANSFER_ENCODING,
    header::TE,
    header::UPGRADE,
];

const DROPPED_RESPONSE: [HeaderName; 3] = [
    header::CONNECTION,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
];

/// Who sent the request.
pub enum Caller<'a> {
    /// A task in the loop container, through the TLS listener.
    Task,
    /// A sandbox or task authenticated as the run, through the `/git` routes; its scopes bound
    /// what it may borrow the token for.
    Run(&'a Principal),
}

pub struct Relay {
    pub gh: Arc<GitHub>,
    pub allow: Arc<Allow>,
    pub policy: policy::State,
}

/// Whether `value` is the placeholder token, in any of the forms `gh` and git send it.
fn is_placeholder(value: &HeaderValue) -> bool {
    let placeholder = crucible_contract::git::PLACEHOLDER_TOKEN;
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some((scheme, credential)) = value.split_once(' ') else {
        return false;
    };
    match scheme.to_ascii_lowercase().as_str() {
        "token" | "bearer" => credential.trim() == placeholder,
        "basic" => base64::engine::general_purpose::STANDARD
            .decode(credential.trim())
            .ok()
            .and_then(|raw| String::from_utf8(raw).ok())
            .and_then(|pair| pair.split_once(':').map(|(_, p)| p == placeholder))
            .unwrap_or(false),
        _ => false,
    }
}

fn refuse(status: StatusCode, why: impl Into<String>) -> Response {
    let why = why.into();
    tracing::info!(%status, %why, "refused");
    (status, why).into_response()
}

fn challenge(why: String) -> Response {
    tracing::info!(%why, "challenged for the caller's own credential");
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"GitHub\"")],
        why,
    )
        .into_response()
}

impl Relay {
    pub async fn relay(&self, host: &str, req: Request, caller: Caller<'_>) -> Response {
        let (parts, body) = req.into_parts();
        let path = parts
            .uri
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_else(|| "/".to_string());
        let presented = parts.headers.get(header::AUTHORIZATION);
        let own = matches!(caller, Caller::Task) && presented.is_some_and(|v| !is_placeholder(v));
        let graphql = !own && policy::needs_body(host, &parts.method, &path);
        let (decided_body, body) = match graphql {
            false => (None, Some(body)),
            true => match http_body_util::Limited::new(body, DECIDED_BODY_LIMIT)
                .collect()
                .await
            {
                Ok(collected) => (Some(collected.to_bytes()), None),
                Err(_) => return refuse(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
            },
        };

        let mut upstream = self
            .gh
            .http
            .request(parts.method.clone(), format!("https://{host}{path}"))
            .headers(forwarded(&parts.headers, own));
        if !own {
            let verdict = policy::decide(
                &self.policy,
                &self.gh,
                &self.allow,
                host,
                &parts.method,
                &path,
                decided_body.as_deref(),
            )
            .await;
            let scope = match verdict {
                Verdict::Allow { scope } => scope,
                Verdict::Deny(_, why) if presented.is_none() && matches!(caller, Caller::Task) => {
                    return challenge(why);
                }
                Verdict::Deny(status, why) => return refuse(status, why),
            };
            if let Caller::Run(principal) = caller
                && !principal.missing_scopes(&[scope.to_string()]).is_empty()
            {
                return refuse(StatusCode::FORBIDDEN, format!("this call needs {scope}"));
            }
            let token = match self.gh.token().await {
                Ok(t) => t,
                Err(e) => return refuse(e.status(), e.to_string()),
            };
            let auth = match host {
                "github.com" => GitHub::basic(&token),
                _ => format!("token {token}"),
            };
            upstream = upstream.header(header::AUTHORIZATION, auth);
        }
        upstream = match (decided_body, body) {
            (Some(bytes), _) => upstream.body(bytes),
            (None, _) if parts.method == Method::GET || parts.method == Method::HEAD => upstream,
            (None, Some(body)) => {
                upstream.body(reqwest::Body::wrap_stream(body.into_data_stream()))
            }
            (None, None) => upstream,
        };
        let resp = match upstream.send().await {
            Ok(r) => r,
            Err(e) => return refuse(StatusCode::BAD_GATEWAY, format!("reaching {host}: {e}")),
        };
        tracing::info!(
            %host,
            method = %parts.method,
            path = %path.split('?').next().unwrap_or(""),
            status = %resp.status(),
            credential = if own { "caller" } else { "run" },
            "relayed"
        );
        let mut out = Response::builder().status(resp.status());
        for (name, value) in resp.headers() {
            if !DROPPED_RESPONSE.contains(name) {
                out = out.header(name, value);
            }
        }
        if graphql {
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => return refuse(StatusCode::BAD_GATEWAY, format!("reading {host}: {e}")),
            };
            policy::record_created(&self.policy, &bytes);
            return out
                .body(Body::from(bytes))
                .unwrap_or_else(|e| refuse(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
        }
        let stream = resp.bytes_stream().map_err(std::io::Error::other);
        out.body(Body::from_stream(stream))
            .unwrap_or_else(|e| refuse(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
    }
}

/// The caller's headers as GitHub should see them: its own `Authorization` kept only when it is
/// relayed with its own credential.
fn forwarded(headers: &HeaderMap, own: bool) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        let dropped = DROPPED_REQUEST.contains(name)
            || name.as_str().starts_with("proxy-")
            || (!own && name == header::AUTHORIZATION);
        if !dropped {
            out.append(name.clone(), value.clone());
        }
    }
    out
}
