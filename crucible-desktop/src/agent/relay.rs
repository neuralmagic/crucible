//! The controller's `/mcp` on a local port, for an agent in a sandbox.
//!
//! ```text
//! sandbox ──Bearer <session token>──▶ relay ──▶ controller /mcp
//!                                       │        as the profile's user: its key,
//!                                       └─ Passed  team, proxy and resolver pins
//! ```

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::net::{Handle, Task};
use crate::profile::Target;

const BODY_LIMIT: usize = 32 * 1024 * 1024;
const PROTOCOL_VERSION: &str = "mcp-protocol-version";
const SAVE: &str = "crucible_draft_save";
const LAUNCH: &str = "crucible_draft_launch";

/// What the relay saw that the session's owner should know.
#[derive(Clone, Debug, PartialEq)]
pub enum Passed {
    /// The session's draft was saved.
    Saved,
    Launched,
    /// A request that did not get an answer from the controller's tools.
    Trouble(String),
}

/// Where calls go, and what the profile sends with each.
pub struct Upstream {
    http: reqwest::Client,
    url: String,
    headers: Vec<(&'static str, String)>,
}

impl Upstream {
    /// `target`'s controller. Blocking: a profile's resolver pins are looked up here.
    pub fn of(target: &Target) -> Result<Upstream, String> {
        target.ready()?;
        let config = target.config()?;
        let http = target
            .http()?
            .build()
            .map_err(|e| format!("building the relay's client: {e}"))?;
        let mut headers = config.auth.headers();
        if let Some(act_as) = target.act_as() {
            headers.push((crux::client::ACT_AS_HEADER, act_as.team().to_string()));
        }
        Ok(Upstream {
            http,
            url: format!("{}/mcp", config.url),
            headers,
        })
    }
}

struct Shared {
    upstream: Upstream,
    /// The draft the session edits; a save of it is reported.
    draft: String,
    bearer: String,
    passed: UnboundedSender<Passed>,
}

impl Shared {
    fn trouble(&self, what: impl Into<String>) {
        let _ = self.passed.send(Passed::Trouble(what.into()));
    }
}

/// A listening relay. Dropping it stops it.
pub struct Relay {
    pub port: u16,
    pub token: String,
    _task: Task,
}

/// Serve `upstream` on a loopback port to whoever holds the token it returns.
pub async fn bind(
    runtime: &Handle,
    upstream: Upstream,
    draft: &str,
    passed: UnboundedSender<Passed>,
) -> Result<Relay, String> {
    let token = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| format!("binding the relay: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("reading the relay's port: {e}"))?
        .port();
    let app = router(Shared {
        upstream,
        draft: draft.to_string(),
        bearer: format!("Bearer {token}"),
        passed,
    });
    let task = Task::spawn(runtime, async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(Relay {
        port,
        token,
        _task: task,
    })
}

fn router(shared: Shared) -> Router {
    Router::new()
        .route("/mcp", post(relay).fallback(unserved))
        .fallback(unserved)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(Arc::new(shared))
}

async fn unserved(State(shared): State<Arc<Shared>>, request: Request) -> StatusCode {
    let status = if request.uri().path() == "/mcp" {
        StatusCode::METHOD_NOT_ALLOWED
    } else {
        StatusCode::NOT_FOUND
    };
    shared.trouble(format!(
        "The relay answered {} to {} {}",
        status.as_u16(),
        request.method(),
        request.uri().path()
    ));
    status
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Whether a `tools/call` answer says the tool did its work.
fn succeeded(answer: &Value) -> bool {
    let Some(result) = answer.get("result") else {
        return false;
    };
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    let text = result
        .pointer("/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    !text.starts_with("error:")
}

fn first_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let line = text.lines().next().unwrap_or_default();
    line.chars().take(200).collect()
}

async fn relay(State(shared): State<Arc<Shared>>, headers: HeaderMap, body: Bytes) -> Response {
    let presented = headers.get(AUTHORIZATION).map(|v| v.as_bytes());
    if !presented.is_some_and(|presented| same(presented, shared.bearer.as_bytes())) {
        shared.trouble("The relay refused a request without the session's token");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let message = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    let method = message.get("method").and_then(Value::as_str);
    let call = (method == Some("tools/call"))
        .then(|| message.pointer("/params/name").and_then(Value::as_str))
        .flatten();
    let own_draft = message
        .pointer("/params/arguments/draft_id")
        .and_then(Value::as_str)
        == Some(shared.draft.as_str());

    let mut request = shared
        .upstream
        .http
        .post(&shared.upstream.url)
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/event-stream")
        .body(body);
    for (name, value) in &shared.upstream.headers {
        request = request.header(*name, value);
    }
    if let Some(version) = headers.get(PROTOCOL_VERSION) {
        request = request.header(PROTOCOL_VERSION, version);
    }
    let answer = match request.send().await {
        Ok(answer) => answer,
        Err(error) => {
            let why = format!("The controller did not answer: {error}");
            shared.trouble(why.clone());
            return (StatusCode::BAD_GATEWAY, why).into_response();
        }
    };
    let status = StatusCode::from_u16(answer.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = answer
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let bytes = match answer.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            let why = format!("Reading the controller's answer: {error}");
            shared.trouble(why.clone());
            return (StatusCode::BAD_GATEWAY, why).into_response();
        }
    };
    if !status.is_success() {
        shared.trouble(format!(
            "The controller answered {} to {}: {}",
            status.as_u16(),
            method.unwrap_or("a request"),
            first_line(&bytes)
        ));
    } else if let Some(tool) = call
        && serde_json::from_slice::<Value>(&bytes).is_ok_and(|answer| succeeded(&answer))
    {
        let passed = match tool {
            SAVE if own_draft => Some(Passed::Saved),
            LAUNCH => Some(Passed::Launched),
            _ => None,
        };
        if let Some(passed) = passed {
            let _ = shared.passed.send(passed);
        }
    }
    (status, [(CONTENT_TYPE, content_type)], bytes).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::Bytes;
    use axum::http::HeaderMap;
    use axum::routing::post;
    use serde_json::{Value, json};
    use tokio::sync::mpsc::unbounded_channel;

    use crate::agent::relay::{Passed, Shared, Upstream, router};

    type Asked = Arc<Mutex<Vec<(Option<String>, Option<String>, Value)>>>;

    /// A stand-in controller: records each call's bearer, act-as header and body, and answers
    /// like the hosted tool surface does.
    async fn controller() -> (String, Asked) {
        let asked = Asked::default();
        let seen = Arc::clone(&asked);
        let app = Router::new().route(
            "/mcp",
            post(move |headers: HeaderMap, body: Bytes| {
                let seen = Arc::clone(&seen);
                async move {
                    let header = |name: &str| {
                        headers
                            .get(name)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string)
                    };
                    let message: Value = serde_json::from_slice(&body).unwrap();
                    seen.lock().unwrap().push((
                        header("authorization"),
                        header(crux::client::ACT_AS_HEADER),
                        message.clone(),
                    ));
                    let id = message["id"].clone();
                    let result = match message["method"].as_str() {
                        Some("tools/list") => json!({"tools": [
                            {"name": "crucible_draft_save"},
                            {"name": "crucible_draft_delete"},
                            {"name": "crucible_turns"},
                            {"name": "crucible_secret_bind"},
                        ]}),
                        Some("tools/call") => {
                            let text = match message["params"]["arguments"]["base_version"].as_i64()
                            {
                                Some(1) => "error: version 2 was saved since",
                                _ => "saved",
                            };
                            json!({"content": [{"type": "text", "text": text}]})
                        }
                        _ => json!({}),
                    };
                    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, asked)
    }

    struct Rig {
        url: String,
        asked: Asked,
        passed: tokio::sync::mpsc::UnboundedReceiver<Passed>,
        http: reqwest::Client,
    }

    impl Rig {
        async fn start() -> Rig {
            let (controller, asked) = controller().await;
            let (sender, passed) = unbounded_channel();
            let app = router(Shared {
                upstream: Upstream {
                    http: reqwest::Client::new(),
                    url: format!("{controller}/mcp"),
                    headers: vec![
                        ("authorization", "Bearer crk_profile".into()),
                        (crux::client::ACT_AS_HEADER, "team:paxos".into()),
                    ],
                },
                draft: "triage-wren".into(),
                bearer: "Bearer session".into(),
                passed: sender,
            });
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let url = format!("http://{}/mcp", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            Rig {
                url,
                asked,
                passed,
                http: reqwest::Client::new(),
            }
        }

        async fn send(&self, bearer: &str, body: &Value) -> (u16, String) {
            let answer = self
                .http
                .post(&self.url)
                .header("authorization", bearer)
                .body(body.to_string())
                .send()
                .await
                .unwrap();
            (answer.status().as_u16(), answer.text().await.unwrap())
        }

        async fn call(&self, tool: &str, arguments: Value) -> Value {
            let body = json!({
                "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                "params": {"name": tool, "arguments": arguments},
            });
            let (status, text) = self.send("Bearer session", &body).await;
            assert_eq!(status, 200, "{text}");
            serde_json::from_str(&text).unwrap()
        }

        fn asked(&self) -> usize {
            self.asked.lock().unwrap().len()
        }
    }

    #[tokio::test]
    async fn only_the_session_token_gets_through_and_it_carries_the_profiles_identity() {
        let mut rig = Rig::start().await;
        let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        assert_eq!(rig.send("Bearer crk_profile", &list).await.0, 401);
        assert_eq!(rig.send("Bearer sessio", &list).await.0, 401);
        assert_eq!(rig.asked(), 0);
        assert_eq!(
            rig.passed.try_recv(),
            Ok(Passed::Trouble(
                "The relay refused a request without the session's token".into()
            ))
        );
        assert_eq!(rig.send("Bearer session", &list).await.0, 200);
        let asked = rig.asked.lock().unwrap();
        assert_eq!(asked[0].0.as_deref(), Some("Bearer crk_profile"));
        assert_eq!(asked[0].1.as_deref(), Some("team:paxos"));
    }

    #[tokio::test]
    async fn every_tool_the_user_has_is_listed_and_callable() {
        let rig = Rig::start().await;
        let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        let (_, text) = rig.send("Bearer session", &list).await;
        let answer: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(answer["result"]["tools"].as_array().unwrap().len(), 4);
        let other = rig
            .call("crucible_draft_delete", json!({"draft_id": "billing"}))
            .await;
        assert_eq!(other["result"]["content"][0]["text"], "saved");
        assert_eq!(rig.asked(), 2);
    }

    #[tokio::test]
    async fn a_save_of_the_sessions_draft_is_reported_once_it_lands() {
        let mut rig = Rig::start().await;
        rig.call(
            "crucible_draft_save",
            json!({"draft_id": "triage-wren", "base_version": 1, "files": {}}),
        )
        .await;
        rig.call(
            "crucible_draft_save",
            json!({"draft_id": "billing", "base_version": 2, "files": {}}),
        )
        .await;
        assert!(rig.passed.try_recv().is_err());

        rig.call(
            "crucible_draft_save",
            json!({"draft_id": "triage-wren", "base_version": 2, "files": {}}),
        )
        .await;
        assert_eq!(rig.passed.try_recv(), Ok(Passed::Saved));
        rig.call(
            "crucible_draft_launch",
            json!({"draft_id": "triage-wren", "max_cost": 0.5, "max_time": "10m"}),
        )
        .await;
        assert_eq!(rig.passed.try_recv(), Ok(Passed::Launched));
        rig.call("crucible_turns", json!({"key": "x"})).await;
        assert!(rig.passed.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_request_the_relay_does_not_serve_is_reported() {
        let mut rig = Rig::start().await;
        let get = rig.http.get(&rig.url).send().await.unwrap();
        assert_eq!(get.status().as_u16(), 405);
        assert_eq!(
            rig.passed.try_recv(),
            Ok(Passed::Trouble("The relay answered 405 to GET /mcp".into()))
        );
        let elsewhere = rig
            .url
            .replace("/mcp", "/.well-known/oauth-protected-resource");
        assert_eq!(rig.http.get(&elsewhere).send().await.unwrap().status(), 404);
        assert_eq!(
            rig.passed.try_recv(),
            Ok(Passed::Trouble(
                "The relay answered 404 to GET /.well-known/oauth-protected-resource".into()
            ))
        );
        assert_eq!(rig.asked(), 0);
    }
}
