//! Tools and the tasks extension's methods over HTTP, against the router a deployment serves, with
//! a kube client whose API server never answers: a request the server refuses fails with its
//! refusal, one it lets through fails only when it reaches Kubernetes. Access checks go to a
//! reviewer that grants a fixed set of verbs in a fixed set of namespaces, standing in for the
//! launcher's RBAC.

use crucible_mcp_auth::access::{AccessCheck, AccessReview, ReviewFuture};
use crucible_mcp_auth::policy::ToolPolicy;
use crucible_mcp_auth::principal::Callers;
use crucible_mcp_auth::principal::Principal;
use crucible_mcp_auth::server::{ResourceServer, TrustedIssuer};
use crucible_mcp_auth::testing::{self, JwksServer};
use rmcp::model::{
    CallToolRequestParams, CancelTaskParams, ClientCapabilities, ClientConfig, GetTaskParams,
    Implementation, ProtocolVersion,
};
use rmcp::service::{ClientLifecycleMode, ClientServiceExt, RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

type Client = RunningService<RoleClient, ClientConfig>;

const PEOPLE: &str = "https://people.example/realms/crucible";
const OWN_TASK: &str = "team-e2e/Job/train/0b6f2c1e";
const FOREIGN_TASK: &str = "team-b/Job/train/0b6f2c1e";

/// Allows a check when its verb and namespace are both granted.
struct Grants {
    verbs: &'static [&'static str],
    namespaces: &'static [&'static str],
}

impl AccessReview for Grants {
    fn review<'a>(&'a self, _: &'a Principal, check: &'a AccessCheck) -> ReviewFuture<'a> {
        let allowed = self.verbs.contains(&check.verb.as_str())
            && self.namespaces.contains(&check.namespace.as_str());
        Box::pin(async move { Ok(allowed) })
    }
}

/// The launcher may read, update and patch workloads in team-e2e only.
const TEAM_E2E: Grants = Grants {
    verbs: &["get", "update", "patch"],
    namespaces: &["team-e2e"],
};

fn unreachable_kube() -> kube::Client {
    crucible_kueue::install_tls_provider();
    let config = kube::Config::new("http://127.0.0.1:1".parse().unwrap());
    kube::Client::try_from(config).unwrap()
}

async fn serve(jwks: &JwksServer, policy: ToolPolicy, grants: Option<Grants>) -> String {
    let mut config = testing::config(jwks.url.clone());
    config.issuers.push(TrustedIssuer {
        issuer: PEOPLE.into(),
        jwks_uri: jwks.url.clone(),
        callers: Callers::People,
    });
    let server = ResourceServer::with_jwks(config, |t| {
        crucible_mcp_auth::jwks::Jwks::new(reqwest::Client::new(), t.jwks_uri.clone())
            .with_timing(Duration::from_secs(300), Duration::from_secs(1))
    });
    let app = crucible_kueue::router(
        unreachable_kube(),
        crucible_kueue::Options {
            oidc: Arc::new(server),
            policy,
            reviewer: grants.map(|g| Arc::new(g) as Arc<dyn AccessReview>),
            kueue: crucible_kueue::kueue::KueueVersion::default(),
            allowed_hosts: Vec::new(),
            kinds: crucible_kueue::manifest::WorkloadKind::ALL.to_vec(),
            config: crucible_kueue::config::Config {
                namespaces: vec!["team-e2e".into(), "team-b".into()],
                ..crucible_kueue::config::Config::default()
            },
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}{}", crucible_kueue::MCP_PATH)
}

fn shipped_policy() -> ToolPolicy {
    ToolPolicy::from_toml(include_str!("../deploy/kueue-policy.toml")).unwrap()
}

async fn connect(url: &str, token: &str) -> Client {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url).auth_header(token),
    );
    ClientConfig::new(
        ClientCapabilities::builder().enable_tasks().build(),
        Implementation::from_build_env(),
    )
    .serve_with_lifecycle(
        transport,
        ClientLifecycleMode::Discover {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        },
    )
    .await
    .unwrap()
}

fn run_token(edit: impl FnOnce(&mut serde_json::Map<String, Value>)) -> String {
    let mut claims = testing::run_claims();
    claims["kube_user"] = json!("alice");
    if let Value::Object(map) = &mut claims {
        edit(map);
    }
    testing::sign("a", &claims)
}

async fn get(client: &Client, id: &str) -> String {
    client
        .get_task(GetTaskParams::new(id))
        .await
        .expect_err("no task can be read without the API server")
        .to_string()
}

async fn cancel(client: &Client, id: &str) -> String {
    client
        .cancel_task(CancelTaskParams::new(id))
        .await
        .expect_err("no task can be cancelled without the API server")
        .to_string()
}

fn reached_kubernetes(error: &str) -> bool {
    error.contains("-32603")
}

#[tokio::test]
async fn a_run_reaches_tasks_where_its_launcher_may_and_no_others() {
    let jwks = JwksServer::start(vec![testing::jwk("a")]).await;
    let url = serve(&jwks, shipped_policy(), Some(TEAM_E2E)).await;
    let client = connect(&url, &run_token(|_| {})).await;

    let own = get(&client, OWN_TASK).await;
    assert!(reached_kubernetes(&own), "{own}");
    let own = cancel(&client, OWN_TASK).await;
    assert!(reached_kubernetes(&own), "{own}");

    let foreign = get(&client, FOREIGN_TASK).await;
    assert!(foreign.contains("task-get"), "{foreign}");
    let foreign = cancel(&client, FOREIGN_TASK).await;
    assert!(foreign.contains("task-cancel"), "{foreign}");

    let granted_elsewhere = serve(
        &jwks,
        shipped_policy(),
        Some(Grants {
            verbs: &["get", "update"],
            namespaces: &["team-e2e", "elsewhere"],
        }),
    )
    .await;
    let client = connect(&granted_elsewhere, &run_token(|_| {})).await;
    let elsewhere = get(&client, "elsewhere/Job/train/0b6f2c1e").await;
    assert!(
        elsewhere.contains("this server is not installed there") && !reached_kubernetes(&elsewhere),
        "a namespace the server does not serve is refused whatever RBAC allows: {elsewhere}"
    );

    let read_only = serve(
        &jwks,
        shipped_policy(),
        Some(Grants {
            verbs: &["get"],
            namespaces: &["team-e2e"],
        }),
    )
    .await;
    let client = connect(&read_only, &run_token(|_| {})).await;
    let refused = cancel(&client, OWN_TASK).await;
    assert!(refused.contains("task-cancel"), "{refused}");

    let no_workload_patch = serve(
        &jwks,
        shipped_policy(),
        Some(Grants {
            verbs: &["get", "update"],
            namespaces: &["team-e2e"],
        }),
    )
    .await;
    let client = connect(&no_workload_patch, &run_token(|_| {})).await;
    let refused = cancel(&client, OWN_TASK).await;
    assert!(
        refused.contains("task-activate") && !reached_kubernetes(&refused),
        "cancelling patches the Job's Kueue Workload, so the launcher must be able to: {refused}"
    );
}

#[tokio::test]
async fn cancelling_needs_the_submit_scope_whatever_the_policy() {
    let jwks = JwksServer::start(vec![testing::jwk("a")]).await;
    let read_only = run_token(|c| {
        c.insert("scope".into(), json!("kueue.read"));
    });
    for policy in [shipped_policy(), ToolPolicy::default()] {
        let url = serve(&jwks, policy, Some(TEAM_E2E)).await;
        let client = connect(&url, &read_only).await;
        let refused = cancel(&client, OWN_TASK).await;
        assert!(refused.contains("Insufficient scope"), "{refused}");
        let read = get(&client, OWN_TASK).await;
        assert!(reached_kubernetes(&read), "{read}");
    }
}

#[tokio::test]
async fn without_a_reviewer_no_task_can_be_checked() {
    let jwks = JwksServer::start(vec![testing::jwk("a")]).await;
    let url = serve(&jwks, shipped_policy(), None).await;
    let client = connect(&url, &run_token(|_| {})).await;
    let refused = get(&client, OWN_TASK).await;
    assert!(refused.contains("access reviewer"), "{refused}");
    let refused = cancel(&client, OWN_TASK).await;
    assert!(refused.contains("access reviewer"), "{refused}");
}

#[tokio::test]
async fn a_person_is_refused_by_a_server_without_a_reviewer_whatever_their_claims() {
    let jwks = JwksServer::start(vec![testing::jwk("a")]).await;
    let url = serve(&jwks, shipped_policy(), None).await;
    for claims in [
        json!({"kube_user": "alice"}),
        json!({"kube_user": "alice", "run_id": "auth-spike", "namespace": "team-e2e", "role": "submitter"}),
    ] {
        let token = run_token(|c| {
            c.insert("iss".into(), json!(PEOPLE));
            for key in ["run_id", "namespace", "role", "queues", "act"] {
                c.remove(key);
            }
            if let Value::Object(extra) = &claims {
                c.extend(extra.clone());
            }
        });
        let response = reqwest::Client::new()
            .post(&url)
            .bearer_auth(&token)
            .header("accept", "application/json, text/event-stream")
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tasks/get", "params": {"taskId": OWN_TASK}}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "{claims}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"], "insufficient_scope", "{body}");
    }
}

/// Every verb any tool's access checks ask about.
const ALL_VERBS: &[&str] = &["get", "list", "create", "update", "patch", "delete", "use"];

/// A value `schema` accepts: its first enum or const value, or a placeholder of its type, with
/// `$ref`s resolved against `root`. Objects get a Job manifest, the only object a tool takes.
fn placeholder(schema: &Value, root: &Value) -> Value {
    let schema = match schema.get("$ref").and_then(Value::as_str) {
        Some(r) => root.pointer(r.trim_start_matches('#')).unwrap_or(schema),
        None => schema,
    };
    if let Some(first) = schema
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|e| e.first())
    {
        return first.clone();
    }
    if let Some(value) = schema.get("const") {
        return value.clone();
    }
    for key in ["oneOf", "anyOf"] {
        let branch = schema
            .get(key)
            .and_then(Value::as_array)
            .and_then(|branches| {
                branches
                    .iter()
                    .find(|b| b.get("type").and_then(Value::as_str) != Some("null"))
            });
        if let Some(branch) = branch {
            return placeholder(branch, root);
        }
    }
    let ty = match schema.get("type") {
        Some(Value::String(t)) => t.as_str(),
        Some(Value::Array(types)) => types
            .iter()
            .filter_map(Value::as_str)
            .find(|t| *t != "null")
            .unwrap_or("string"),
        _ => "object",
    };
    match ty {
        "string" => json!("x"),
        "integer" | "number" => json!(1),
        "boolean" => json!(false),
        "array" => json!([]),
        _ => json!({"apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": "j"}}),
    }
}

/// Arguments `tool` accepts, acting in `namespace`.
fn arguments(tool: &rmcp::model::Tool, namespace: &str) -> serde_json::Map<String, Value> {
    let root = Value::Object((*tool.input_schema).clone());
    let mut args = serde_json::Map::new();
    for name in root["required"].as_array().into_iter().flatten() {
        let name = name.as_str().unwrap();
        args.insert(name.into(), placeholder(&root["properties"][name], &root));
    }
    args.insert("namespace".into(), json!(namespace));
    args
}

/// A refused tool call's text, or what it returned instead.
async fn refusal(client: &Client, tool: &str, args: serde_json::Map<String, Value>) -> String {
    let result = client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(args))
        .await;
    match result {
        Ok(r) if r.is_error == Some(true) => {
            serde_json::to_value(&r).unwrap()["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        }
        other => format!("not refused: {other:?}"),
    }
}

/// Each tool, called in a namespace the server does not serve, answers `not_found` before it
/// reaches Kubernetes, however much RBAC allows there. A new tool that skips the namespace check
/// fails here.
#[tokio::test]
async fn every_tool_refuses_a_namespace_the_server_does_not_serve() {
    let jwks = JwksServer::start(vec![testing::jwk("a")]).await;
    let everything_elsewhere = Grants {
        verbs: ALL_VERBS,
        namespaces: &["elsewhere"],
    };
    let url = serve(&jwks, shipped_policy(), Some(everything_elsewhere)).await;
    let client = connect(&url, &run_token(|_| {})).await;
    let tools = client.list_all_tools().await.unwrap();
    assert!(tools.len() >= 13, "{}", tools.len());
    for tool in &tools {
        let schema = &tool.input_schema;
        assert!(
            schema["properties"].get("namespace").is_some(),
            "{} takes no namespace",
            tool.name
        );
        let text = refusal(&client, &tool.name, arguments(tool, "elsewhere")).await;
        assert!(
            text.contains("not_found") && text.contains("elsewhere"),
            "{}: {text}",
            tool.name
        );
    }
}

/// A token's `namespaces` claim confines the run: a served namespace outside the claim is
/// `not_found`, and so is everything when the claim and the served set share nothing.
#[tokio::test]
async fn the_namespaces_claim_confines_a_run_within_the_served_namespaces() {
    let jwks = JwksServer::start(vec![testing::jwk("a")]).await;
    let both = Grants {
        verbs: ALL_VERBS,
        namespaces: &["team-e2e", "team-b", "other"],
    };
    let url = serve(&jwks, shipped_policy(), Some(both)).await;
    let confined = run_token(|c| {
        c.insert("namespaces".into(), json!(["team-e2e"]));
    });
    let client = connect(&url, &confined).await;
    let args = |ns: &str| {
        let mut m = serde_json::Map::new();
        m.insert("namespace".into(), json!(ns));
        m
    };
    let text = refusal(&client, "queues_list", args("team-b")).await;
    assert!(
        text.contains("not_found") && text.contains("team-b"),
        "{text}"
    );
    let own = refusal(&client, "queues_list", args("team-e2e")).await;
    assert!(
        !own.contains("not_found"),
        "the claimed namespace reaches Kubernetes: {own}"
    );

    let disjoint = run_token(|c| {
        c.insert("namespace".into(), json!("other"));
        c.insert("namespaces".into(), json!(["other"]));
    });
    let client = connect(&url, &disjoint).await;
    for tool in ["queues_list", "workload_list"] {
        let text = refusal(&client, tool, serde_json::Map::new()).await;
        assert!(
            text.contains("not_found") && text.contains("other"),
            "{tool}: {text}"
        );
    }
}

/// Any caller who may submit to a queue can discover its hardware: `use` on the LocalQueue is
/// enough for `hardware_describe` and `queue_describe`, with no RBAC on nodes, pods or Kueue
/// Workloads. Without `get` or `use` on the queue, both are refused.
#[tokio::test]
async fn a_caller_who_may_use_a_queue_may_describe_it_and_its_hardware() {
    let jwks = JwksServer::start(vec![testing::jwk("a")]).await;
    let submitter = Grants {
        verbs: &["use", "create"],
        namespaces: &["team-e2e"],
    };
    let url = serve(&jwks, shipped_policy(), Some(submitter)).await;
    let client = connect(&url, &run_token(|_| {})).await;
    let mut args = serde_json::Map::new();
    args.insert("namespace".into(), json!("team-e2e"));
    args.insert("queue".into(), json!("gpu"));
    for tool in ["hardware_describe", "queue_describe"] {
        let text = refusal(&client, tool, args.clone()).await;
        assert!(
            !text.contains("refused"),
            "{tool} passes the guard and fails only at the unreachable API server: {text}"
        );
    }

    let creator = Grants {
        verbs: &["create"],
        namespaces: &["team-e2e"],
    };
    let url = serve(&jwks, shipped_policy(), Some(creator)).await;
    let client = connect(&url, &run_token(|_| {})).await;
    for tool in ["hardware_describe", "queue_describe"] {
        let text = refusal(&client, tool, args.clone()).await;
        assert!(
            text.contains("refused by access check queue-get"),
            "{tool}: {text}"
        );
    }
}
