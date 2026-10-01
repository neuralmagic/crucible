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

/// The sandbox-facing hostnames the compute drivers hand the agent (podman bridge / openshell
/// cluster alias). rmcp's DNS-rebinding guard allowlists loopback only, and neither of these is
/// loopback, so without them every tool call dies at the transport with a 403.
const SANDBOX_HOSTS: [&str; 2] = ["host.containers.internal", "host.openshell.internal"];

/// The `Host` values rmcp's streamable-http transport will accept, layered on top of its
/// loopback default (`loopback`): the two driver hostnames, plus whatever a deployment adds
/// through `BROKER_ALLOWED_HOSTS` (comma-separated). An entry may be a bare `host`, which matches
/// that host on ANY port, or an exact `host:port`. A deployment needs the env only when it fronts
/// the broker under some other name, e.g. an in-cluster Service DNS name or an explicit
/// `[broker].url` override in the domain manifest.
pub fn allowed_hosts(loopback: Vec<String>) -> Vec<String> {
    extra_hosts(
        loopback,
        std::env::var("BROKER_ALLOWED_HOSTS").ok().as_deref(),
    )
}

/// The pure half of [`allowed_hosts`], with the env value passed in.
fn extra_hosts(mut hosts: Vec<String>, env: Option<&str>) -> Vec<String> {
    let extra = SANDBOX_HOSTS.iter().copied().chain(
        env.unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|h| !h.is_empty()),
    );
    for host in extra {
        // Order-preserving, so the loopback defaults stay first. `Vec::dedup` would only catch
        // adjacent repeats, and a deployment re-listing a driver host is not adjacent to it.
        if !hosts.iter().any(|h| h == host) {
            hosts.push(host.to_string());
        }
    }
    hosts
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

    /// rmcp's loopback-only default 403s the sandbox, which reaches us on the driver hostname.
    /// Both driver names must survive with no deployment config at all.
    #[test]
    fn sandbox_driver_hosts_are_allowed_without_any_env() {
        let hosts = extra_hosts(vec!["localhost".into(), "127.0.0.1".into()], None);
        assert!(hosts.iter().any(|h| h == "host.containers.internal"));
        assert!(hosts.iter().any(|h| h == "host.openshell.internal"));
        assert!(hosts.iter().any(|h| h == "localhost"), "keeps loopback");
    }

    #[test]
    fn env_adds_deployment_hosts_and_ignores_blanks() {
        let hosts = extra_hosts(
            vec!["localhost".into()],
            Some(" broker.crucible.svc , broker.crucible.svc:8849 ,, "),
        );
        assert!(hosts.iter().any(|h| h == "broker.crucible.svc"));
        assert!(hosts.iter().any(|h| h == "broker.crucible.svc:8849"));
        assert!(!hosts.iter().any(|h| h.is_empty()));
        assert!(hosts.iter().any(|h| h == "host.containers.internal"));
    }

    /// A deployment re-listing a host we already add must not double it up.
    #[test]
    fn repeated_hosts_collapse() {
        let hosts = extra_hosts(
            vec!["localhost".into()],
            Some("host.containers.internal,localhost"),
        );
        assert_eq!(
            hosts,
            vec![
                "localhost",
                "host.containers.internal",
                "host.openshell.internal"
            ]
        );
    }

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
