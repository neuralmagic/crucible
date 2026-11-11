//! The controller on a local port, for an agent in a sandbox: its MCP tools at `/mcp` and its
//! API under `/api`, which is what `crux` speaks.
//!
//! ```text
//! sandbox ──Bearer <session token>──▶ relay ──▶ controller
//!   MCP client, crux                    │        as the profile's user: its key,
//!                                       └─ Passed  team, proxy and resolver pins
//! ```

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use crucible_contract::mcp_message::{self, TOOLS_CALL};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::net::{Handle, Task};
use crate::profile::Target;

const BODY_LIMIT: usize = 32 * 1024 * 1024;
/// Request headers the protocol defines, which the controller reads.
const PROTOCOL_HEADERS: &str = "mcp-";
/// How long one answer may stream: a watched run, not a request.
const STREAM_LIMIT: Duration = Duration::from_secs(24 * 60 * 60);
const CONNECT_WITHIN: Duration = Duration::from_secs(15);
const MCP: &str = "/mcp";
const LAST_EVENT_ID: &str = "last-event-id";
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
    /// The controller's base URL, with no trailing slash.
    base: String,
    headers: Vec<(&'static str, String)>,
}

impl Upstream {
    /// `target`'s controller. Blocking: a profile's resolver pins are looked up here.
    pub fn of(target: &Target) -> Result<Upstream, String> {
        target.ready()?;
        let config = target.config()?;
        let http = target
            .http()?
            .timeout(STREAM_LIMIT)
            .connect_timeout(CONNECT_WITHIN)
            .build()
            .map_err(|e| format!("building the relay's client: {e}"))?;
        let mut headers = config.auth.headers();
        if let Some(act_as) = target.act_as() {
            headers.push((crux::client::ACT_AS_HEADER, act_as.team().to_string()));
        }
        Ok(Upstream {
            http,
            base: config.url,
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
        .fallback(relay)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(Arc::new(shared))
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Whether a `tools/call` answer says the tool did its work. The controller's tools answer a
/// failure as text that starts `error:`, not as a result marked `isError`.
fn succeeded(answer: &Value) -> bool {
    !mcp_message::call_failed(answer)
        && !mcp_message::result_text(answer).is_some_and(|text| text.starts_with("error:"))
}

fn first_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let line = text.lines().next().unwrap_or_default();
    line.chars().take(200).collect()
}

/// What a request would change if the controller accepts it.
enum Asks {
    /// An MCP tool call, which answers 200 whether or not the tool worked.
    Tool(Passed),
    /// An API call, which answers 2xx only when it worked.
    Api(Passed),
    Nothing,
}

impl Shared {
    fn asks(&self, method: &Method, path: &str, body: &[u8]) -> (Asks, String) {
        if method == Method::POST && path == MCP {
            let mut message = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
            let named = mcp_message::method(&message).map(str::to_string);
            let label = named.clone().unwrap_or_else(|| "a request".to_string());
            if named.as_deref() != Some(TOOLS_CALL) {
                return (Asks::Nothing, label);
            }
            let asks = match mcp_message::tool_call(&mut message) {
                Ok((tool, arguments)) => {
                    let own = arguments.get("draft_id").and_then(Value::as_str)
                        == Some(self.draft.as_str());
                    match tool.as_str() {
                        SAVE if own => Asks::Tool(Passed::Saved),
                        LAUNCH => Asks::Tool(Passed::Launched),
                        _ => Asks::Nothing,
                    }
                }
                Err(_) => Asks::Nothing,
            };
            return (asks, label);
        }
        let label = format!("{method} {path}");
        let draft = format!("/api/playbook-drafts/{}", crux::client::encode(&self.draft));
        let asks = match path.strip_prefix(&draft) {
            Some("/versions") if method == Method::POST => Asks::Api(Passed::Saved),
            Some("/launch") if method == Method::POST => Asks::Api(Passed::Launched),
            _ => Asks::Nothing,
        };
        (asks, label)
    }
}

async fn relay(
    State(shared): State<Arc<Shared>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let presented = headers.get(AUTHORIZATION).map(|v| v.as_bytes());
    if !presented.is_some_and(|presented| same(presented, shared.bearer.as_bytes())) {
        shared.trouble("The relay refused a request without the session's token");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let (asks, label) = shared.asks(&method, uri.path(), &body);
    let to = uri.path_and_query().map_or(uri.path(), |all| all.as_str());
    let mut request = shared
        .upstream
        .http
        .request(method.clone(), format!("{}{to}", shared.upstream.base));
    for (name, value) in &headers {
        let passes = [CONTENT_TYPE.as_str(), ACCEPT.as_str(), LAST_EVENT_ID]
            .contains(&name.as_str())
            || name.as_str().starts_with(PROTOCOL_HEADERS);
        if passes {
            request = request.header(name, value);
        }
    }
    if uri.path() == MCP {
        request = request
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream");
    }
    for (name, value) in &shared.upstream.headers {
        request = request.header(*name, value);
    }
    let answer = match request.body(body).send().await {
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
    let refused = matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN);
    let read_whole = refused || status.is_server_error() || matches!(asks, Asks::Tool(_));
    if !read_whole {
        if let (Asks::Api(passed), true) = (asks, status.is_success()) {
            let _ = shared.passed.send(passed);
        }
        let chunks = futures_util::stream::unfold(Some(answer), |answer| async {
            let mut answer = answer?;
            match answer.chunk().await {
                Ok(Some(chunk)) => Some((Ok(chunk), Some(answer))),
                Ok(None) => None,
                Err(error) => Some((Err(error), None)),
            }
        });
        return (
            status,
            [(CONTENT_TYPE, content_type)],
            Body::from_stream(chunks),
        )
            .into_response();
    }
    let bytes = match answer.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            let why = format!("Reading the controller's answer: {error}");
            shared.trouble(why.clone());
            return (StatusCode::BAD_GATEWAY, why).into_response();
        }
    };
    if refused || status.is_server_error() {
        shared.trouble(format!(
            "The controller answered {} to {label}: {}",
            status.as_u16(),
            first_line(&bytes)
        ));
    } else if let Asks::Tool(passed) = asks
        && status.is_success()
        && serde_json::from_slice::<Value>(&bytes).is_ok_and(|answer| succeeded(&answer))
    {
        let _ = shared.passed.send(passed);
    }
    (status, [(CONTENT_TYPE, content_type)], bytes).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::Path;
    use axum::http::HeaderMap;
    use axum::routing::{get, post};
    use serde_json::{Value, json};
    use tokio::sync::mpsc::unbounded_channel;

    use crate::agent::relay::{Passed, Shared, Upstream, router};

    type Sent = std::collections::BTreeMap<String, String>;
    type Asked = Arc<Mutex<Vec<(Option<String>, Option<String>, Value, Sent)>>>;

    /// A stand-in controller: records each call's bearer, act-as header and body, and answers
    /// like the hosted tool surface does.
    type Feed = tokio::sync::mpsc::UnboundedSender<&'static str>;

    async fn controller() -> (String, Asked, Arc<Mutex<Vec<Option<String>>>>, Feed) {
        let asked = Asked::default();
        let seen = Arc::clone(&asked);
        let api: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
        let bearer = |headers: &HeaderMap| {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let (read, saved) = (Arc::clone(&api), Arc::clone(&api));
        let files = get(
            move |Path(draft): Path<String>, uri: axum::http::Uri, headers: HeaderMap| async move {
                read.lock().unwrap().push(bearer(&headers));
                format!("files of {draft} at {}", uri.query().unwrap_or_default())
            },
        );
        let versions = post(move |headers: HeaderMap, body: Bytes| async move {
            saved.lock().unwrap().push(bearer(&headers));
            let save: Value = serde_json::from_slice(&body).unwrap();
            match save["base_version"].as_i64() {
                Some(1) => (
                    axum::http::StatusCode::CONFLICT,
                    "version 2 was saved since",
                ),
                _ => (axum::http::StatusCode::OK, "{}"),
            }
        });
        let (feed, fed) = tokio::sync::mpsc::unbounded_channel::<&'static str>();
        let fed = Arc::new(tokio::sync::Mutex::new(Some(fed)));
        let live = get(move || async move {
            let fed = fed.lock().await.take().unwrap();
            let events = futures_util::stream::unfold(fed, |mut fed| async {
                let event = fed.recv().await?;
                Some((Ok::<_, std::convert::Infallible>(event), fed))
            });
            (
                [("content-type", "text/event-stream")],
                axum::body::Body::from_stream(events),
            )
        });
        let app = Router::new()
            .route("/api/runs/r1/live", live)
            .route("/api/playbook-drafts/{draft}/files", files)
            .route("/api/playbook-drafts/{draft}/versions", versions)
            .route(
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
                        let sent = headers
                            .iter()
                            .filter_map(|(name, value)| {
                                Some((name.to_string(), value.to_str().ok()?.to_string()))
                            })
                            .collect();
                        seen.lock().unwrap().push((
                            header("authorization"),
                            header(crux::client::ACT_AS_HEADER),
                            message.clone(),
                            sent,
                        ));
                        if message["method"] == "forbidden" {
                            return (axum::http::StatusCode::FORBIDDEN, "no".to_string());
                        }
                        let id = message["id"].clone();
                        let result = match message["method"].as_str() {
                            Some("tools/list") => json!({"tools": [
                                {"name": "crucible_draft_save"},
                                {"name": "crucible_draft_delete"},
                                {"name": "crucible_turns"},
                                {"name": "crucible_secret_bind"},
                            ]}),
                            Some("tools/call") => {
                                let text =
                                    match message["params"]["arguments"]["base_version"].as_i64() {
                                        Some(1) => "error: version 2 was saved since",
                                        _ => "saved",
                                    };
                                json!({"content": [{"type": "text", "text": text}]})
                            }
                            _ => json!({}),
                        };
                        let answer = json!({"jsonrpc": "2.0", "id": id, "result": result});
                        (axum::http::StatusCode::OK, answer.to_string())
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, asked, api, feed)
    }

    struct Rig {
        url: String,
        asked: Asked,
        /// The bearer of each request the stand-in controller's API got.
        api: Arc<Mutex<Vec<Option<String>>>>,
        /// Sends the next event of the stand-in controller's live stream.
        feed: Feed,
        passed: tokio::sync::mpsc::UnboundedReceiver<Passed>,
        http: reqwest::Client,
    }

    impl Rig {
        async fn start() -> Rig {
            let (controller, asked, api, feed) = controller().await;
            let (sender, passed) = unbounded_channel();
            let app = router(Shared {
                upstream: Upstream {
                    http: reqwest::Client::new(),
                    base: controller,
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
                api,
                feed,
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
    async fn protocol_headers_reach_the_controller_and_a_refusal_is_reported() {
        let mut rig = Rig::start().await;
        let probe = json!({"jsonrpc": "2.0", "id": 1, "method": "server/discover"});
        let answer = rig
            .http
            .post(&rig.url)
            .header("authorization", "Bearer session")
            .header("mcp-method", "server/discover")
            .header("mcp-protocol-version", "2025-06-18")
            .header("x-other", "kept out")
            .body(probe.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(answer.status().as_u16(), 200);
        {
            let asked = rig.asked.lock().unwrap();
            let sent = &asked[0].3;
            assert_eq!(
                sent.get("mcp-method").map(String::as_str),
                Some("server/discover")
            );
            assert_eq!(
                sent.get("mcp-protocol-version").map(String::as_str),
                Some("2025-06-18")
            );
            assert!(!sent.contains_key("x-other"));
        }
        assert!(rig.passed.try_recv().is_err());

        let (status, _) = rig
            .send(
                "Bearer session",
                &json!({"jsonrpc": "2.0", "id": 2, "method": "forbidden"}),
            )
            .await;
        assert_eq!(status, 403);
        assert_eq!(
            rig.passed.try_recv(),
            Ok(Passed::Trouble(
                "The controller answered 403 to forbidden: no".into()
            ))
        );
        let get = rig
            .http
            .get(&rig.url)
            .header("authorization", "Bearer session")
            .send()
            .await
            .unwrap();
        assert_eq!(get.status().as_u16(), 405);
        assert!(rig.passed.try_recv().is_err());
    }

    #[tokio::test]
    async fn the_api_is_served_to_the_token_holder_as_the_profiles_user() {
        let mut rig = Rig::start().await;
        let api = |path: &str| rig.url.replace("/mcp", path);
        let unsigned = rig
            .http
            .get(api("/api/playbook-drafts/triage-wren/files?version=3"))
            .send()
            .await
            .unwrap();
        assert_eq!(unsigned.status().as_u16(), 401);
        assert!(rig.api.lock().unwrap().is_empty());
        rig.passed.try_recv().unwrap();

        let read = rig
            .http
            .get(api("/api/playbook-drafts/triage-wren/files?version=3"))
            .header("authorization", "Bearer session")
            .send()
            .await
            .unwrap();
        assert_eq!(read.status().as_u16(), 200);
        assert_eq!(
            read.text().await.unwrap(),
            "files of triage-wren at version=3"
        );
        assert!(rig.passed.try_recv().is_err());

        let push = |draft: &str, base: i64| {
            rig.http
                .post(api(&format!("/api/playbook-drafts/{draft}/versions")))
                .header("authorization", "Bearer session")
                .json(&json!({"base_version": base, "files": {}}))
                .send()
        };
        assert_eq!(push("triage-wren", 1).await.unwrap().status().as_u16(), 409);
        assert_eq!(push("billing", 2).await.unwrap().status().as_u16(), 200);
        assert!(rig.passed.try_recv().is_err());
        assert_eq!(push("triage-wren", 2).await.unwrap().status().as_u16(), 200);
        assert_eq!(rig.passed.try_recv(), Ok(Passed::Saved));

        let asked = rig.api.lock().unwrap();
        assert_eq!(asked.len(), 4);
        assert!(
            asked
                .iter()
                .all(|bearer| bearer.as_deref() == Some("Bearer crk_profile"))
        );
    }

    #[tokio::test]
    async fn a_streamed_answer_arrives_event_by_event() {
        let rig = Rig::start().await;
        let mut live = rig
            .http
            .get(rig.url.replace("/mcp", "/api/runs/r1/live"))
            .header("authorization", "Bearer session")
            .header("accept", "text/event-stream")
            .send()
            .await
            .unwrap();
        assert_eq!(live.status().as_u16(), 200);
        assert_eq!(
            live.headers()["content-type"].to_str().unwrap(),
            "text/event-stream"
        );
        let limit = std::time::Duration::from_secs(5);

        rig.feed.send("data: one\n\n").unwrap();
        let first = tokio::time::timeout(limit, live.chunk()).await.unwrap();
        assert_eq!(first.unwrap().unwrap(), "data: one\n\n");
        rig.feed.send("data: two\n\n").unwrap();
        let second = tokio::time::timeout(limit, live.chunk()).await.unwrap();
        assert_eq!(second.unwrap().unwrap(), "data: two\n\n");
        drop(rig.feed);
        let end = tokio::time::timeout(limit, live.chunk()).await.unwrap();
        assert_eq!(end.unwrap(), None);
    }
}
