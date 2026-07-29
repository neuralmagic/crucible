//! Bearer-token guard for the broker's http endpoint. The broker binds `0.0.0.0` so the sandbox
//! reaches it over the podman bridge. On a cluster pod that also exposes the port to any pod
//! that can route to it, and the tools behind it roll deployments and comment on JIRA with the broker's
//! credentials. crucible mints a per-run token, hands it to the broker as `BROKER_TOKEN`, and seeds
//! it into the sandbox's `.mcp.json` headers; this layer rejects any request that doesn't carry it.
//! No token in the env = guard off (an operator-run broker, the pre-token behavior).

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

/// The expected token from `BROKER_TOKEN` (`None`/empty = guard off). The binaries pass this to
/// [`require_bearer`] via `middleware::from_fn_with_state`.
pub fn expected_token() -> Option<String> {
    std::env::var("BROKER_TOKEN").ok().filter(|t| !t.is_empty())
}

/// Middleware: 401 any request that doesn't carry the expected `Authorization: Bearer <token>`.
/// With no expected token, every request passes.
pub async fn require_bearer(
    State(expected): State<Arc<Option<String>>>,
    req: Request,
    next: Next,
) -> Response {
    let got = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if authorized(got, expected.as_deref()) {
        return next.run(req).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"status":"error","error":"missing or wrong broker bearer token"}"#,
    )
        .into_response()
}

/// The pure check: pass when no token is expected, else require an exact `Bearer <token>` match.
fn authorized(header: Option<&str>, expected: Option<&str>) -> bool {
    let Some(want) = expected else { return true };
    header
        .and_then(|h| h.strip_prefix("Bearer "))
        .is_some_and(|got| constant_time_eq(got.as_bytes(), want.as_bytes()))
}

/// Length-gated constant-time byte compare, so the token check leaks no early-exit timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_expected_token_means_open() {
        assert!(authorized(None, None));
        assert!(authorized(Some("Bearer whatever"), None));
    }

    #[test]
    fn expected_token_requires_exact_bearer_match() {
        let want = Some("s3cr3t");
        assert!(authorized(Some("Bearer s3cr3t"), want));
        assert!(!authorized(None, want), "missing header");
        assert!(!authorized(Some("s3cr3t"), want), "no Bearer prefix");
        assert!(!authorized(Some("Bearer wrong"), want));
        assert!(
            !authorized(Some("Bearer s3cr3t2"), want),
            "prefix match is not a match"
        );
        assert!(!authorized(Some("Bearer "), want), "empty credential");
    }

    #[test]
    fn constant_time_eq_matches_plain_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }
}
