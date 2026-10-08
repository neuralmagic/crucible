//! Middleware that applies a [`ToolPolicy`] to MCP traffic. Runs after authentication: it needs
//! the [`Principal`] in the request extensions and refuses the request without one.
//!
//! `tools/call` bodies are parsed, rewritten by the policy, and re-serialized, so rmcp executes
//! exactly the arguments the rules saw. The access checks a call's authorize rules produce go to
//! the guard's [`AccessReview`]; every one must be allowed. `tools/list` responses (JSON or SSE)
//! have injected parameters removed from each tool's `inputSchema`.

use crate::policy::{AccessCheck, Refusal, ToolPolicy};
use crate::principal::Principal;
use crate::server::{AuthError, ResourceServer};
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http::{Method, StatusCode, header};
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Largest request or `tools/list` response body the guard buffers.
const BODY_LIMIT: usize = 4 * 1024 * 1024;

/// JSON-RPC error code for a call the policy refused (implementation-defined server error range).
pub const POLICY_REFUSED: i64 = -32001;

pub type ReviewFuture<'a> = Pin<
    Box<dyn Future<Output = Result<bool, Box<dyn std::error::Error + Send + Sync>>> + Send + 'a>,
>;

/// Decides an [`AccessCheck`] for a principal: `Ok(true)` allows, `Ok(false)` denies, an error
/// refuses the call too.
pub trait AccessReview: Send + Sync {
    fn review<'a>(&'a self, principal: &'a Principal, check: &'a AccessCheck) -> ReviewFuture<'a>;
}

pub struct PolicyGuard {
    pub policy: ToolPolicy,
    /// When set, a missing per-tool scope answers with this server's 403 step-up challenge.
    pub server: Option<Arc<ResourceServer>>,
    /// Reviews authorize rules' checks. A call that needs one without a reviewer is refused.
    pub reviewer: Option<Arc<dyn AccessReview>>,
}

pub async fn enforce(State(guard): State<Arc<PolicyGuard>>, req: Request, next: Next) -> Response {
    if req.method() != Method::POST {
        return next.run(req).await;
    }
    let Some(principal) = req.extensions().get::<Principal>().cloned() else {
        return plain(StatusCode::UNAUTHORIZED, "no authenticated principal");
    };
    let (mut parts, body) = req.into_parts();
    let bytes = match to_bytes(body, BODY_LIMIT).await {
        Ok(b) => b,
        Err(_) => return plain(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };
    let Ok(mut message) = serde_json::from_slice::<Value>(&bytes) else {
        return next
            .run(Request::from_parts(parts, Body::from(bytes)))
            .await;
    };
    if message.is_array() {
        return plain(
            StatusCode::BAD_REQUEST,
            "JSON-RPC batches are not supported",
        );
    }
    match message.get("method").and_then(Value::as_str) {
        Some("tools/call") => {
            let id = message.get("id").cloned().unwrap_or(Value::Null);
            let decided = match apply(&guard.policy, &mut message, &principal) {
                Ok(checks) => review(guard.reviewer.as_deref(), &principal, &checks).await,
                Err(refusal) => Err(refusal),
            };
            if let Err(refusal) = decided {
                tracing::info!(sub = %principal.subject, presenter = principal.presenter(), %refusal, "tool call refused");
                return refuse(&guard, &parts.headers, id, refusal);
            }
            let body = match serde_json::to_vec(&message) {
                Ok(b) => b,
                Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "re-encoding the call"),
            };
            parts.headers.remove(header::CONTENT_LENGTH);
            next.run(Request::from_parts(parts, Body::from(body))).await
        }
        Some("tools/list") if !guard.policy.hides_nothing() => {
            let response = next
                .run(Request::from_parts(parts, Body::from(bytes)))
                .await;
            hide_injected(&guard.policy, response).await
        }
        _ => {
            next.run(Request::from_parts(parts, Body::from(bytes)))
                .await
        }
    }
}

async fn review(
    reviewer: Option<&dyn AccessReview>,
    principal: &Principal,
    checks: &[AccessCheck],
) -> Result<(), Refusal> {
    for check in checks {
        let Some(reviewer) = reviewer else {
            return Err(Refusal::ReviewError {
                tool: check.tool.clone(),
                rule: check.rule.clone(),
                error: "this server has no access reviewer".to_string(),
            });
        };
        match reviewer.review(principal, check).await {
            Ok(true) => {}
            Ok(false) => {
                return Err(Refusal::Denied {
                    tool: check.tool.clone(),
                    rule: check.rule.clone(),
                    message: check.message.clone(),
                });
            }
            Err(error) => {
                return Err(Refusal::ReviewError {
                    tool: check.tool.clone(),
                    rule: check.rule.clone(),
                    error: error.to_string(),
                });
            }
        }
    }
    Ok(())
}

fn apply(
    policy: &ToolPolicy,
    message: &mut Value,
    principal: &Principal,
) -> Result<Vec<AccessCheck>, Refusal> {
    let params = message
        .get_mut("params")
        .and_then(Value::as_object_mut)
        .ok_or(Refusal::NoToolName)?;
    let tool = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or(Refusal::NoToolName)?
        .to_string();
    let arguments = params
        .entry("arguments")
        .or_insert_with(|| Value::Object(Default::default()));
    if arguments.is_null() {
        *arguments = Value::Object(Default::default());
    }
    let Some(args) = arguments.as_object_mut() else {
        return Err(Refusal::Arguments { tool });
    };
    policy.apply(&tool, args, &principal.claims, &principal.scopes)
}

fn refuse(guard: &PolicyGuard, headers: &http::HeaderMap, id: Value, refusal: Refusal) -> Response {
    if let (Refusal::Scope { missing, .. }, Some(server)) = (&refusal, &guard.server) {
        return server.challenge(
            headers,
            &AuthError::InsufficientScope {
                missing: missing.clone(),
            },
        );
    }
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": POLICY_REFUSED, "message": refusal.to_string()},
    });
    (StatusCode::OK, axum::Json(body)).into_response()
}

fn plain(status: StatusCode, message: &'static str) -> Response {
    (status, axum::Json(json!({"error": message}))).into_response()
}

async fn hide_injected(policy: &ToolPolicy, response: Response) -> Response {
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, BODY_LIMIT).await else {
        return plain(
            StatusCode::BAD_GATEWAY,
            "tools/list response too large to filter",
        );
    };
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let rewritten = if content_type.starts_with("text/event-stream") {
        rewrite_sse(policy, &bytes)
    } else {
        rewrite_json(policy, &bytes)
    };
    match rewritten {
        Some(body) => {
            parts.headers.remove(header::CONTENT_LENGTH);
            Response::from_parts(parts, Body::from(body))
        }
        None => Response::from_parts(parts, Body::from(bytes)),
    }
}

/// Hide injected parameters in a `tools/list` result; false if `message` is not one.
fn rewrite_message(policy: &ToolPolicy, message: &mut Value) -> bool {
    let Some(tools) = message
        .pointer_mut("/result/tools")
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    for tool in tools {
        let name = tool.get("name").and_then(Value::as_str).map(str::to_string);
        if let (Some(name), Some(schema)) = (
            name,
            tool.get_mut("inputSchema").and_then(Value::as_object_mut),
        ) {
            policy.hide_injected(&name, schema);
        }
    }
    true
}

fn rewrite_json(policy: &ToolPolicy, bytes: &Bytes) -> Option<Vec<u8>> {
    let mut message: Value = serde_json::from_slice(bytes).ok()?;
    if !rewrite_message(policy, &mut message) {
        return None;
    }
    serde_json::to_vec(&message).ok()
}

/// Rewrite every SSE event whose data is a tool list. Other events and fields pass through.
fn rewrite_sse(policy: &ToolPolicy, bytes: &Bytes) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let normalized = text.replace("\r\n", "\n");
    let mut out = String::with_capacity(normalized.len());
    let mut changed = false;
    for (i, event) in normalized.split("\n\n").enumerate() {
        if i > 0 {
            out.push_str("\n\n");
        }
        let data: Vec<&str> = event
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(|d| d.strip_prefix(' ').unwrap_or(d))
            .collect();
        let rewritten = (!data.is_empty())
            .then(|| serde_json::from_str::<Value>(&data.join("\n")).ok())
            .flatten()
            .and_then(|mut m| rewrite_message(policy, &mut m).then_some(m))
            .and_then(|m| serde_json::to_string(&m).ok());
        match rewritten {
            Some(json) => {
                changed = true;
                let mut wrote_data = false;
                let lines: Vec<String> = event
                    .lines()
                    .filter_map(|l| {
                        if l.starts_with("data:") {
                            if wrote_data {
                                return None;
                            }
                            wrote_data = true;
                            return Some(format!("data: {json}"));
                        }
                        Some(l.to_string())
                    })
                    .collect();
                out.push_str(&lines.join("\n"));
            }
            None => out.push_str(event),
        }
    }
    changed.then(|| out.into_bytes())
}

#[cfg(test)]
mod tests {
    use crate::guard::*;
    use crate::testing::{self, JwksServer};
    use axum::body::to_bytes;
    use std::time::Duration;
    use tower::ServiceExt;

    const POLICY: &str = r#"
        [tools.workload_plan]
        scopes = ["kueue.submit"]
        [[tools.workload_plan.mutate]]
        path = "/launched_by"
        value = "claims.sub"
        [[tools.workload_plan.validate]]
        name = "queue-allowed"
        rule = "args.queue in claims.queues"
        message = "queue is not in this run's allowlist"
        [tools.whoami]
    "#;

    const TOOLS: &str = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[
        {"name":"workload_plan","inputSchema":{"type":"object","properties":{"queue":{"type":"string"},"launched_by":{"type":"string"}},"required":["queue","launched_by"]}},
        {"name":"whoami","inputSchema":{"type":"object","properties":{"launched_by":{}}}}]}}"#;

    fn principal(scope: &str) -> Principal {
        let mut claims = testing::run_claims();
        claims["scope"] = json!(scope);
        match claims {
            Value::Object(m) => Principal::from_claims(m).unwrap(),
            _ => unreachable!(),
        }
    }

    /// The guard in front of a downstream that echoes the JSON-RPC body it received, or answers
    /// `tools/list` with TOOLS (as SSE when `sse`).
    fn app(guard: PolicyGuard, principal: Option<Principal>, sse: bool) -> axum::Router {
        let downstream = axum::routing::post(move |body: Bytes| async move {
            let message: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            if message["method"] == "tools/list" {
                let (ct, body) = if sse {
                    (
                        "text/event-stream",
                        format!(
                            "id: 1\nevent: message\ndata: {}\n\n",
                            TOOLS.replace('\n', "")
                        ),
                    )
                } else {
                    ("application/json", TOOLS.to_string())
                };
                return ([(header::CONTENT_TYPE, ct)], body).into_response();
            }
            axum::Json(json!({"received": message})).into_response()
        });
        axum::Router::new()
            .route("/mcp", downstream)
            .layer(axum::middleware::from_fn_with_state(
                Arc::new(guard),
                enforce,
            ))
            .layer(axum::middleware::from_fn(
                move |mut req: Request, next: Next| {
                    let principal = principal.clone();
                    async move {
                        if let Some(p) = principal {
                            req.extensions_mut().insert(p);
                        }
                        next.run(req).await
                    }
                },
            ))
    }

    fn guard(server: Option<Arc<ResourceServer>>) -> PolicyGuard {
        PolicyGuard {
            policy: ToolPolicy::from_toml(POLICY).unwrap(),
            server,
            reviewer: None,
        }
    }

    async fn post(app: axum::Router, body: Value) -> (StatusCode, http::HeaderMap, Value, String) {
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "kueue.apps.example")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let (parts, body) = res.into_parts();
        let bytes = to_bytes(body, BODY_LIMIT).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        let json = serde_json::from_str(&text).unwrap_or(Value::Null);
        (parts.status, parts.headers, json, text)
    }

    fn call(tool: &str, args: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": tool, "arguments": args}})
    }

    #[tokio::test]
    async fn the_downstream_sees_the_mutated_arguments() {
        let app = app(guard(None), Some(principal("kueue.submit")), false);
        let (status, _, body, _) = post(
            app,
            call(
                "workload_plan",
                json!({"queue": "crucible-e2e", "launched_by": "forged"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["received"]["params"]["arguments"],
            json!({"queue": "crucible-e2e", "launched_by": "alice"})
        );
    }

    #[tokio::test]
    async fn a_refused_call_never_reaches_the_downstream() {
        let app = app(guard(None), Some(principal("kueue.submit")), false);
        let (status, _, body, _) =
            post(app, call("workload_plan", json!({"queue": "other"}))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"jsonrpc": "2.0", "id": 7, "error": {"code": POLICY_REFUSED,
                "message": "tool workload_plan refused by policy rule queue-allowed: queue is not in this run's allowlist"}})
        );
    }

    #[tokio::test]
    async fn calls_without_a_name_or_with_non_object_arguments_are_refused() {
        let app = app(guard(None), Some(principal("kueue.submit")), false);
        let (_, _, nameless, _) = post(
            app.clone(),
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {}}),
        )
        .await;
        assert_eq!(nameless["error"]["message"], "tools/call names no tool");
        let (_, _, listy, _) = post(app.clone(), call("whoami", json!([1]))).await;
        assert_eq!(
            listy["error"]["message"],
            "tool whoami: arguments must be an object"
        );
        let (_, _, null_args, _) = post(app, call("whoami", Value::Null)).await;
        assert_eq!(null_args["received"]["params"]["arguments"], json!({}));
    }

    #[tokio::test]
    async fn a_missing_tool_scope_is_a_step_up_challenge_when_a_server_is_attached() {
        let jwks = JwksServer::start(vec![]).await;
        let rs = Arc::new(testing::resource_server(
            &jwks,
            Duration::from_secs(60),
            Duration::ZERO,
        ));
        let app = app(guard(Some(rs)), Some(principal("kueue.read")), false);
        let (status, headers, _, _) =
            post(app, call("workload_plan", json!({"queue": "crucible-e2e"}))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let challenge = headers[header::WWW_AUTHENTICATE].to_str().unwrap();
        assert!(
            challenge.contains(r#"error="insufficient_scope""#),
            "{challenge}"
        );
        assert!(challenge.contains(r#"resource_metadata="https://kueue.apps.example/.well-known/oauth-protected-resource/mcp""#), "{challenge}");
        assert!(
            challenge.ends_with(r#"scope="kueue.submit""#),
            "{challenge}"
        );

        let without_server = app_scope_error().await;
        assert_eq!(
            without_server,
            "tool workload_plan needs scope kueue.submit"
        );
    }

    async fn app_scope_error() -> Value {
        let app = app(guard(None), Some(principal("kueue.read")), false);
        let (_, _, body, _) =
            post(app, call("workload_plan", json!({"queue": "crucible-e2e"}))).await;
        body["error"]["message"].clone()
    }

    #[tokio::test]
    async fn requests_without_a_principal_are_refused() {
        let app = app(guard(None), None, false);
        let (status, _, _, _) = post(app, call("whoami", json!({}))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn batches_are_refused() {
        let app = app(guard(None), Some(principal("kueue.submit")), false);
        let (status, _, _, _) = post(app, json!([call("whoami", json!({}))])).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn other_methods_pass_through_unchanged() {
        let app = app(guard(None), Some(principal("")), false);
        let init = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"launched_by": "x"}});
        let (_, _, body, _) = post(app, init.clone()).await;
        assert_eq!(body["received"], init);
    }

    fn advertised(list: &Value) -> Value {
        list["result"]["tools"].clone()
    }

    #[tokio::test]
    async fn tools_list_hides_injected_parameters_in_json_responses() {
        let app = app(guard(None), Some(principal("")), false);
        let (_, headers, body, _) = post(
            app,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .await;
        assert!(
            headers.get(header::CONTENT_LENGTH).is_none_or(|l| l
                .to_str()
                .unwrap()
                .parse::<usize>()
                .unwrap()
                == serde_json::to_vec(&body).unwrap().len())
        );
        assert_eq!(
            advertised(&body),
            json!([
                {"name": "workload_plan", "inputSchema": {"type": "object", "properties": {"queue": {"type": "string"}}, "required": ["queue"]}},
                {"name": "whoami", "inputSchema": {"type": "object", "properties": {"launched_by": {}}}},
            ])
        );
    }

    #[tokio::test]
    async fn tools_list_hides_injected_parameters_in_sse_responses() {
        let app = app(guard(None), Some(principal("")), true);
        let (_, _, _, text) = post(
            app,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .await;
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("id: 1"));
        assert_eq!(lines.next(), Some("event: message"));
        let data: Value =
            serde_json::from_str(lines.next().unwrap().strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(
            advertised(&data)[0]["inputSchema"]["required"],
            json!(["queue"])
        );
        assert!(
            advertised(&data)[0]["inputSchema"]["properties"]
                .get("launched_by")
                .is_none()
        );
    }

    #[test]
    fn sse_events_that_are_not_tool_lists_are_left_alone() {
        let policy = ToolPolicy::from_toml(POLICY).unwrap();
        let stream = Bytes::from(
            "event: ping\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n: comment\n\n",
        );
        assert_eq!(rewrite_sse(&policy, &stream), None);
        let tools = TOOLS.replace('\n', "");
        let (head, tail) = tools.split_at(tools.find("\"result\":").unwrap() + "\"result\":".len());
        let split = Bytes::from(format!("data: {head}\ndata: {tail}\n\n"));
        let out = String::from_utf8(rewrite_sse(&policy, &split).unwrap()).unwrap();
        assert_eq!(out.matches("data:").count(), 1, "{out}");
        assert!(
            !out.contains("launched_by\":{\"type\":\"string\"}"),
            "{out}"
        );
    }
}
