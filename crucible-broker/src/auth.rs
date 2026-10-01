//! Bearer-token guard for the broker's http endpoint. The broker binds `0.0.0.0` so the sandbox
//! reaches it over the podman bridge. On a cluster pod that also exposes the port to any pod
//! that can route to it, and the tools behind it roll deployments and comment on JIRA with the broker's
//! credentials. crucible either mints one per-run token and hands it over as `BROKER_TOKEN`, or
//! mints one token per sandbox and names the file mapping each to its sandbox in
//! `MCP_TOKENS_FILE` (see [`crucible_contract::mcp`]); this layer rejects any request that doesn't
//! carry a token it knows. No token in the env = guard off (an operator-run broker, the pre-token
//! behavior).

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use crucible_contract::mcp::{ENV_TOKENS_FILE, TokenMap};
use std::path::PathBuf;
use std::sync::Arc;

/// How the broker authenticates a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Guard {
    /// No token configured: every request passes.
    Open,
    /// One token for every caller (`BROKER_TOKEN`).
    Token(String),
    /// One token per sandbox, re-read from this file on every request (`MCP_TOKENS_FILE`).
    PerSandbox(PathBuf),
}

/// The sandbox a per-sandbox token belongs to, attached to the request for the tools behind the
/// guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox(pub String);

/// The guard the env configures: `MCP_TOKENS_FILE` over `BROKER_TOKEN` over open.
pub fn guard() -> Guard {
    if let Some(path) = std::env::var_os(ENV_TOKENS_FILE).filter(|p| !p.is_empty()) {
        return Guard::PerSandbox(PathBuf::from(path));
    }
    match expected_token() {
        Some(token) => Guard::Token(token),
        None => Guard::Open,
    }
}

/// Middleware over [`guard`]: 401 any request whose bearer the guard does not know. A per-sandbox
/// token also attaches its [`Sandbox`]. An unreadable token file refuses everything.
pub async fn require_guard(
    State(guard): State<Arc<Guard>>,
    mut req: Request,
    next: Next,
) -> Response {
    let got = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let pass = match guard.as_ref() {
        Guard::Open => true,
        Guard::Token(want) => authorized(got.as_deref(), Some(want)),
        Guard::PerSandbox(path) => match sandbox_for(path, got.as_deref()).await {
            Some(sandbox) => {
                req.extensions_mut().insert(Sandbox(sandbox));
                true
            }
            None => false,
        },
    };
    if pass {
        return next.run(req).await;
    }
    unauthorized()
}

/// The sandbox the bearer in `header` belongs to, per the token file at `path`.
async fn sandbox_for(path: &std::path::Path, header: Option<&str>) -> Option<String> {
    let token = header?.strip_prefix("Bearer ")?;
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(e) => {
            tracing::error!("reading the token file {}: {e}", path.display());
            return None;
        }
    };
    match TokenMap::parse(&text) {
        Ok(map) => map.sandbox_for(token).map(str::to_owned),
        Err(e) => {
            tracing::error!("parsing the token file {}: {e}", path.display());
            None
        }
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"status":"error","error":"missing or wrong broker bearer token"}"#,
    )
        .into_response()
}

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
    unauthorized()
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
    use crate::auth::*;

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

    /// One request against a live server: the status code and the body.
    fn get(addr: std::net::SocketAddr, bearer: Option<&str>) -> (u16, String) {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        let auth = bearer
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        write!(
            stream,
            "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{auth}\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response[9..12].parse().unwrap();
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, b)| b.to_string())
            .unwrap_or_default();
        (status, body)
    }

    /// The guard in front of a real server whose one route echoes the sandbox the guard attached.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_per_sandbox_token_acts_only_as_its_own_sandbox() {
        let dir = std::env::temp_dir().join(format!("broker-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("tokens");
        let mut map = TokenMap::default();
        map.grant("ci-a", "tok-a");
        map.grant("ci-b", "tok-b");
        std::fs::write(&file, map.render()).unwrap();

        async fn who(req: Request) -> String {
            req.extensions()
                .get::<Sandbox>()
                .map(|s| s.0.clone())
                .unwrap_or_default()
        }
        let app = axum::Router::new()
            .route("/", axum::routing::get(who))
            .layer(axum::middleware::from_fn_with_state(
                Arc::new(Guard::PerSandbox(file.clone())),
                require_guard,
            ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });

        let call = move |bearer: Option<&'static str>| {
            tokio::task::spawn_blocking(move || get(addr, bearer))
        };
        assert_eq!(call(Some("tok-a")).await.unwrap(), (200, "ci-a".into()));
        assert_eq!(call(Some("tok-b")).await.unwrap(), (200, "ci-b".into()));
        assert_eq!(call(None).await.unwrap().0, 401);
        assert_eq!(call(Some("tok-c")).await.unwrap().0, 401);
        assert_eq!(
            call(Some("ci-a")).await.unwrap().0,
            401,
            "a sandbox name is no token"
        );

        map.revoke("ci-a");
        std::fs::write(&file, map.render()).unwrap();
        assert_eq!(
            call(Some("tok-a")).await.unwrap().0,
            401,
            "the file is re-read, so a revoked token stops working"
        );
        assert_eq!(call(Some("tok-b")).await.unwrap(), (200, "ci-b".into()));

        std::fs::remove_file(&file).unwrap();
        assert_eq!(
            call(Some("tok-b")).await.unwrap().0,
            401,
            "a missing token file refuses everything"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
