//! Middleware that decides MCP requests. Runs after authentication: it needs the [`Principal`]
//! in the request extensions and refuses the request without one.
//!
//! Deny by default: a POST passes as protocol plumbing (`initialize`, `ping`, `server/discover`,
//! `notifications/*`, a response to the server), as `tools/list`, or as a tool call or method the
//! server's [`AccessRules`] serve. For those: the rules' scopes, then (tools only) the
//! [`ToolPolicy`]'s mutate and validate rules, then the rules' access checks, each of which the
//! [`AccessReview`] must allow. The request is forwarded as the guard parsed and rewrote it, so the
//! server executes exactly what was decided on. `tools/list` responses (JSON or SSE) have injected
//! parameters removed from each tool's `inputSchema`.

use crate::access::{AccessCheck, AccessReview, AccessRules, Call};
use crate::policy::{Refusal, ToolPolicy};
use crate::principal::Principal;
use crate::server::{AuthError, ResourceServer};
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http::{Method, StatusCode, header};
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// Largest request or `tools/list` response body the guard buffers.
const BODY_LIMIT: usize = 4 * 1024 * 1024;

/// JSON-RPC error code for a refused method other than a tool call (implementation-defined server
/// error range).
pub const POLICY_REFUSED: i64 = -32001;

const PROTOCOL_METHODS: [&str; 3] = ["initialize", "ping", "server/discover"];

pub struct PolicyGuard {
    /// The server's tools and methods, their scopes and access checks.
    pub rules: Arc<dyn AccessRules>,
    /// Mutate and validate rules for tool arguments.
    pub policy: ToolPolicy,
    /// When set, a missing scope answers with this server's 403 step-up challenge.
    pub server: Option<Arc<ResourceServer>>,
    /// Reviews access checks. A call that needs one without a reviewer is refused.
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
        return plain(StatusCode::BAD_REQUEST, "the body is not JSON");
    };
    if message.is_array() {
        return plain(
            StatusCode::BAD_REQUEST,
            "JSON-RPC batches are not supported",
        );
    }
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_string);
    let hide = match method.as_deref() {
        Some("tools/list") => !guard.policy.hides_nothing(),
        Some(method)
            if PROTOCOL_METHODS.contains(&method) || method.starts_with("notifications/") =>
        {
            false
        }
        Some(method) => {
            let id = message.get("id").cloned().unwrap_or(Value::Null);
            if let Err(refusal) = decide(&guard, &mut message, &principal, method).await {
                tracing::info!(sub = %principal.subject, presenter = principal.presenter(), %refusal, "request refused");
                return refuse(&guard, &parts.headers, id, method, refusal);
            }
            false
        }
        None if is_response(&message) => false,
        None => return plain(StatusCode::BAD_REQUEST, "not a JSON-RPC request"),
    };
    let body = match serde_json::to_vec(&message) {
        Ok(b) => b,
        Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "re-encoding the request"),
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    let response = next.run(Request::from_parts(parts, Body::from(body))).await;
    if hide {
        return hide_injected(&guard.policy, response).await;
    }
    response
}

/// A JSON-RPC response from the client to a server request: an `id` and a `result` or `error`.
fn is_response(message: &Value) -> bool {
    message.get("id").is_some()
        && (message.get("result").is_some() || message.get("error").is_some())
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
                return Err(Refusal::ReviewFailed {
                    tool: check.tool.clone(),
                    rule: check.rule.clone(),
                    error: error.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Scopes, then (for a tool call) the policy's rules, then the access checks.
async fn decide(
    guard: &PolicyGuard,
    message: &mut Value,
    principal: &Principal,
    method: &str,
) -> Result<(), Refusal> {
    let checks = if method == "tools/call" {
        let (tool, args) = tool_call(message)?;
        require(
            principal,
            &tool,
            guard.rules.scopes(&Call::Tool { name: &tool, args })?,
        )?;
        guard.policy.apply(&tool, args, &principal.claims)?;
        guard
            .rules
            .checks(&Call::Tool { name: &tool, args }, principal)?
    } else {
        let params = message.get("params").unwrap_or(&Value::Null);
        let call = Call::Method {
            name: method,
            params,
        };
        require(principal, method, guard.rules.scopes(&call)?)?;
        guard.rules.checks(&call, principal)?
    };
    review(guard.reviewer.as_deref(), principal, &checks).await
}

fn require(principal: &Principal, call: &str, scopes: &[&str]) -> Result<(), Refusal> {
    let missing: Vec<String> = scopes
        .iter()
        .filter(|s| !principal.scopes.contains(**s))
        .map(|s| s.to_string())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(Refusal::Scope {
        tool: call.to_string(),
        missing,
    })
}

/// The tool a `tools/call` names and its arguments, an empty object when absent.
fn tool_call(message: &mut Value) -> Result<(String, &mut Map<String, Value>), Refusal> {
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
        .or_insert_with(|| Value::Object(Map::new()));
    if arguments.is_null() {
        *arguments = Value::Object(Map::new());
    }
    match arguments.as_object_mut() {
        Some(args) => Ok((tool, args)),
        None => Err(Refusal::Arguments { tool }),
    }
}

/// A missing scope is a step-up challenge when a server is attached. A refused tool call is a tool
/// result with `isError`, so the model reads why; a call that names no tool and any other method
/// get a JSON-RPC error.
fn refuse(
    guard: &PolicyGuard,
    headers: &http::HeaderMap,
    id: Value,
    method: &str,
    refusal: Refusal,
) -> Response {
    if let (Refusal::Scope { missing, .. }, Some(server)) = (&refusal, &guard.server) {
        return server.challenge(
            headers,
            &AuthError::InsufficientScope {
                missing: missing.clone(),
            },
        );
    }
    let body = if method == "tools/call" && refusal != Refusal::NoToolName {
        json!({"jsonrpc": "2.0", "id": id, "result": refused_tool_result(&refusal)})
    } else {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": POLICY_REFUSED, "message": refusal.to_string()},
        })
    };
    (StatusCode::OK, axum::Json(body)).into_response()
}

/// A `CallToolResult` with `isError` whose text is `{"status": "refused", "error", "retryable"}`.
fn refused_tool_result(refusal: &Refusal) -> Value {
    let text = json!({
        "status": "refused",
        "error": refusal.to_string(),
        "retryable": refusal.retryable(),
    });
    json!({"content": [{"type": "text", "text": text.to_string()}], "isError": true})
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
    use crate::access::ReviewFuture;
    use crate::guard::*;
    use crate::principal::Callers;
    use crate::testing::{self, JwksServer};
    use axum::body::to_bytes;
    use std::time::Duration;
    use tower::ServiceExt;

    const POLICY: &str = r#"
        [[tools.workload_plan.mutate]]
        path = "/launched_by"
        value = "claims.sub"
        [[tools.workload_plan.validate]]
        name = "queue-allowed"
        rule = "args.queue in claims.queues"
        message = "queue is not in this run's allowlist"
    "#;

    /// Serves workload_plan (kueue.submit), whoami, tasks/get (kueue.read) and tasks/cancel
    /// (kueue.submit). A caller with kube_user needs `use` on the planned queue, in the namespace
    /// the arguments name, and `update` on a cancelled task.
    struct Rules;

    impl AccessRules for Rules {
        fn scopes(&self, call: &Call<'_>) -> Result<&'static [&'static str], Refusal> {
            match call {
                Call::Tool {
                    name: "workload_plan",
                    ..
                }
                | Call::Method {
                    name: "tasks/cancel",
                    ..
                } => Ok(&["kueue.submit"]),
                Call::Tool { name: "whoami", .. } => Ok(&[]),
                Call::Method {
                    name: "tasks/get", ..
                } => Ok(&["kueue.read"]),
                other => Err(other.unserved()),
            }
        }

        fn checks(
            &self,
            call: &Call<'_>,
            principal: &Principal,
        ) -> Result<Vec<AccessCheck>, Refusal> {
            if !principal.claims.contains_key("kube_user") {
                return Ok(Vec::new());
            }
            let check = |verb: &str, resource: &str, namespace: &str, name: &str| AccessCheck {
                tool: call.name().to_string(),
                rule: format!("{resource}-{verb}"),
                message: format!("you may not {verb} this"),
                group: "kueue.x-k8s.io".into(),
                resource: resource.into(),
                subresource: String::new(),
                verb: verb.into(),
                namespace: namespace.into(),
                name: name.into(),
            };
            let text = |v: &Value, key: &str| v[key].as_str().unwrap_or_default().to_string();
            Ok(match call {
                Call::Tool {
                    name: "workload_plan",
                    args,
                } => {
                    let args = Value::Object((*args).clone());
                    vec![check(
                        "use",
                        "localqueues",
                        &text(&args, "namespace"),
                        &text(&args, "queue"),
                    )]
                }
                Call::Method {
                    name: "tasks/cancel",
                    params,
                } => vec![check("update", "workloads", "", &text(params, "taskId"))],
                _ => Vec::new(),
            })
        }
    }

    /// Allows exactly the checks whose verb is in `verbs`; any other check fails to be made when
    /// `broken`, and is denied otherwise.
    struct Verbs {
        verbs: &'static [&'static str],
        broken: bool,
    }

    impl AccessReview for Verbs {
        fn review<'a>(&'a self, _: &'a Principal, check: &'a AccessCheck) -> ReviewFuture<'a> {
            let allowed = self.verbs.contains(&check.verb.as_str());
            let broken = self.broken;
            Box::pin(async move {
                match (allowed, broken) {
                    (true, _) => Ok(true),
                    (false, true) => Err("the API server is unreachable".into()),
                    (false, false) => Ok(false),
                }
            })
        }
    }

    const TOOLS: &str = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[
        {"name":"workload_plan","inputSchema":{"type":"object","properties":{"queue":{"type":"string"},"launched_by":{"type":"string"}},"required":["queue","launched_by"]}},
        {"name":"whoami","inputSchema":{"type":"object","properties":{"launched_by":{}}}}]}}"#;

    fn principal(scope: &str) -> Principal {
        let mut claims = testing::run_claims();
        claims["scope"] = json!(scope);
        match claims {
            Value::Object(m) => Principal::from_claims(m, Callers::Runs).unwrap(),
            _ => unreachable!(),
        }
    }

    /// The guard in front of a downstream that echoes the JSON-RPC body it received (parsed, and
    /// as the raw text in `raw`), or answers
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
            let raw = String::from_utf8_lossy(&body).into_owned();
            axum::Json(json!({"received": message, "raw": raw})).into_response()
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
            rules: Arc::new(Rules),
            policy: ToolPolicy::from_toml(POLICY).unwrap(),
            server,
            reviewer: None,
        }
    }

    async fn post(app: axum::Router, body: Value) -> (StatusCode, http::HeaderMap, Value, String) {
        post_raw(app, &serde_json::to_string(&body).unwrap()).await
    }

    async fn post_raw(
        app: axum::Router,
        body: &str,
    ) -> (StatusCode, http::HeaderMap, Value, String) {
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "kueue.apps.example")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
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
        assert_eq!(body["id"], 7);
        assert_eq!(
            refusal(&body),
            json!({"status": "refused", "retryable": false,
                "error": "tool workload_plan refused by policy rule queue-allowed: queue is not in this run's allowlist"})
        );
    }

    /// The refusal a tool call was answered with: a tool result with `isError` whose text is JSON.
    fn refusal(body: &Value) -> Value {
        assert!(body.get("error").is_none(), "not a JSON-RPC error: {body}");
        assert_eq!(body["result"]["isError"], true, "{body}");
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
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
            refusal(&listy)["error"],
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
        assert_eq!(without_server, "workload_plan needs scope kueue.submit");
    }

    async fn app_scope_error() -> Value {
        let app = app(guard(None), Some(principal("kueue.read")), false);
        let (_, _, body, _) =
            post(app, call("workload_plan", json!({"queue": "crucible-e2e"}))).await;
        refusal(&body)["error"].clone()
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

    fn rpc(method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 9, "method": method, "params": params})
    }

    #[tokio::test]
    async fn methods_the_server_does_not_serve_are_refused() {
        for method in [
            "resources/read",
            "prompts/get",
            "completion/complete",
            "tasks/list",
            "logging/setLevel",
        ] {
            let app = app(
                guard(None),
                Some(principal("kueue.read kueue.submit")),
                false,
            );
            let (status, _, body, _) = post(app, rpc(method, json!({}))).await;
            assert_eq!(status, StatusCode::OK, "{method}");
            assert_eq!(body["error"]["code"], POLICY_REFUSED, "{method}: {body}");
            assert_eq!(
                body["error"]["message"],
                format!("method {method} is not permitted")
            );
            assert!(
                body.get("received").is_none(),
                "{method} reached the server"
            );
        }
    }

    #[tokio::test]
    async fn the_server_receives_the_request_the_guard_decided_on_whatever_its_duplicate_keys() {
        let read_only = || app(guard(None), Some(principal("kueue.read")), false);
        let smuggled = r#"{"jsonrpc":"2.0","id":9,"method":"tasks/cancel","method":"tasks/get","params":{"taskId":"t"}}"#;
        let (status, _, body, _) = post_raw(read_only(), smuggled).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let raw = body["raw"].as_str().unwrap();
        assert!(raw.contains("tasks/get"), "{raw}");
        assert!(!raw.contains("tasks/cancel"), "{raw}");

        let refused_tool = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"workload_plan","name":"whoami","arguments":{}}}"#;
        let (_, _, body, _) = post_raw(read_only(), refused_tool).await;
        let raw = body["raw"].as_str().unwrap();
        assert!(raw.contains("whoami"), "{raw}");
        assert!(!raw.contains("workload_plan"), "{raw}");

        let response = r#"{"jsonrpc":"2.0","id":3,"result":{},"result":{"ok":true}}"#;
        let (_, _, body, _) = post_raw(read_only(), response).await;
        assert_eq!(
            serde_json::from_str::<Value>(body["raw"].as_str().unwrap()).unwrap(),
            json!({"jsonrpc": "2.0", "id": 3, "result": {"ok": true}})
        );
    }

    #[tokio::test]
    async fn listed_methods_need_their_scopes() {
        let read_only = app(guard(None), Some(principal("kueue.read")), false);
        let (_, _, got, _) =
            post(read_only.clone(), rpc("tasks/get", json!({"taskId": "t"}))).await;
        assert_eq!(got["received"]["method"], "tasks/get");
        let (_, _, cancel, _) = post(read_only, rpc("tasks/cancel", json!({"taskId": "t"}))).await;
        assert_eq!(
            cancel["error"]["message"],
            "tasks/cancel needs scope kueue.submit"
        );

        let jwks = JwksServer::start(vec![]).await;
        let rs = Arc::new(testing::resource_server(
            &jwks,
            Duration::from_secs(60),
            Duration::ZERO,
        ));
        let challenged = app(guard(Some(rs)), Some(principal("kueue.read")), false);
        let (status, headers, _, _) =
            post(challenged, rpc("tasks/cancel", json!({"taskId": "t"}))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let challenge = headers[header::WWW_AUTHENTICATE].to_str().unwrap();
        assert!(
            challenge.ends_with(r#"scope="kueue.submit""#),
            "{challenge}"
        );
    }

    #[tokio::test]
    async fn protocol_plumbing_and_client_responses_pass() {
        for message in [
            rpc("initialize", json!({})),
            rpc("ping", json!({})),
            rpc("server/discover", json!({})),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 3, "result": {}}),
        ] {
            let app = app(guard(None), Some(principal("")), false);
            let (_, _, body, _) = post(app, message.clone()).await;
            assert_eq!(body["received"], message);
        }
    }

    #[tokio::test]
    async fn bodies_that_are_not_json_rpc_requests_are_refused() {
        let app = app(guard(None), Some(principal("")), false);
        let (status, _, _, _) = post(app.clone(), json!({"jsonrpc": "2.0", "id": 1})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("not json"))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn tools_come_from_the_servers_rules_whatever_the_policy() {
        let open = PolicyGuard {
            rules: Arc::new(Rules),
            policy: ToolPolicy::default(),
            server: None,
            reviewer: None,
        };
        let app = app(open, Some(principal("kueue.submit")), false);
        let (_, _, body, _) = post(app.clone(), call("workload_delete", json!({}))).await;
        assert_eq!(
            refusal(&body)["error"],
            "tool workload_delete is not permitted",
            "{body}"
        );
        let (_, _, body, _) = post(app, call("workload_plan", json!({"queue": "q"}))).await;
        assert_eq!(
            body["received"]["params"]["arguments"],
            json!({"queue": "q"}),
            "no policy, no mutation: {body}"
        );
    }

    fn kube_user(scope: &str) -> Principal {
        let mut claims = testing::run_claims();
        claims["scope"] = json!(scope);
        claims["kube_user"] = json!("alice");
        match claims {
            Value::Object(m) => Principal::from_claims(m, Callers::Runs).unwrap(),
            _ => unreachable!(),
        }
    }

    fn reviewed(verbs: &'static [&'static str], broken: bool) -> PolicyGuard {
        PolicyGuard {
            reviewer: Some(Arc::new(Verbs { verbs, broken })),
            ..guard(None)
        }
    }

    #[tokio::test]
    async fn every_access_check_must_be_allowed_by_the_reviewer() {
        let plan = || {
            call(
                "workload_plan",
                json!({"queue": "crucible-e2e", "namespace": "team-a"}),
            )
        };
        let allowed = app(
            reviewed(&["use"], false),
            Some(kube_user("kueue.submit")),
            false,
        );
        let (_, _, body, _) = post(allowed, plan()).await;
        assert_eq!(
            body["received"]["params"]["name"], "workload_plan",
            "{body}"
        );

        let denied = app(reviewed(&[], false), Some(kube_user("kueue.submit")), false);
        let (_, _, body, _) = post(denied, plan()).await;
        assert_eq!(
            refusal(&body),
            json!({"status": "refused", "retryable": false,
                "error": "workload_plan refused by access check localqueues-use: you may not use this"})
        );

        let broken = app(reviewed(&[], true), Some(kube_user("kueue.submit")), false);
        let (_, _, body, _) = post(broken, plan()).await;
        assert_eq!(
            refusal(&body),
            json!({"status": "refused", "retryable": true,
                "error": "workload_plan refused: access check localqueues-use failed: the API server is unreachable"}),
            "a review that could not complete may succeed later"
        );

        let cancel = app(
            reviewed(&["use"], false),
            Some(kube_user("kueue.submit")),
            false,
        );
        let (_, _, body, _) = post(cancel, rpc("tasks/cancel", json!({"taskId": "t"}))).await;
        assert_eq!(
            body["error"]["message"],
            "tasks/cancel refused by access check workloads-update: you may not update this"
        );
    }

    #[tokio::test]
    async fn a_call_needing_checks_without_a_reviewer_is_refused() {
        let app = app(guard(None), Some(kube_user("kueue.submit")), false);
        let (_, _, body, _) =
            post(app, call("workload_plan", json!({"queue": "crucible-e2e"}))).await;
        assert_eq!(
            refusal(&body),
            json!({"status": "refused", "retryable": false,
                "error": "workload_plan refused: access check localqueues-use could not be made: this server has no access reviewer"})
        );
    }

    #[tokio::test]
    async fn checks_see_the_arguments_after_the_policy_ran() {
        let policy = r#"
            [[tools.workload_plan.mutate]]
            path = "/namespace"
            value = "claims.namespace"
        "#;
        struct Recorder(std::sync::Mutex<Vec<String>>);
        impl AccessReview for Recorder {
            fn review<'a>(&'a self, _: &'a Principal, check: &'a AccessCheck) -> ReviewFuture<'a> {
                if let Ok(mut seen) = self.0.lock() {
                    seen.push(check.namespace.clone());
                }
                Box::pin(async { Ok(true) })
            }
        }
        let recorder = Arc::new(Recorder(std::sync::Mutex::new(Vec::new())));
        let pinned = PolicyGuard {
            rules: Arc::new(Rules),
            policy: ToolPolicy::from_toml(policy).unwrap(),
            server: None,
            reviewer: Some(recorder.clone()),
        };
        let app = app(pinned, Some(kube_user("kueue.submit")), false);
        post(
            app,
            call(
                "workload_plan",
                json!({"queue": "q", "namespace": "elsewhere"}),
            ),
        )
        .await;
        assert_eq!(*recorder.0.lock().unwrap(), ["team-e2e"]);
    }
}
