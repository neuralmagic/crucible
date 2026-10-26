//! End-to-end against a real cluster with Kueue: serves the MCP endpoint on loopback, drives it
//! with an rmcp client, and runs real Jobs through a LocalQueue.
//!
//! Run with:
//!   CRUCIBLE_KUEUE_E2E_CONTEXT=<kube context> CRUCIBLE_KUEUE_E2E_NAMESPACE=<ns> \
//!   CRUCIBLE_KUEUE_E2E_QUEUE=<local queue> cargo test --test cluster_e2e -- --ignored --nocapture
//!   CRUCIBLE_KUEUE_E2E_KUBE_USER=<user the tokens act as>
//!   CRUCIBLE_KUEUE_E2E_KUBE_GROUPS=<comma-separated groups of that user, optional>
//!   CRUCIBLE_KUEUE_E2E_MODE=crucible|tenant (default crucible)
//! `crucible` serves crucible's defaults to run tokens carrying kube_user. `tenant` serves the
//! generic setup: a people issuer whose tokens name the user in preferred_username and groups in
//! groups with no run claim, the tenant allowlist, rbac ownership and its own label domain.
//! The namespace must carry the label Kueue's ClusterQueue selects on and hold the LocalQueue. The
//! tokens act as CRUCIBLE_KUEUE_E2E_KUBE_USER, decided by SubjectAccessReview: it needs `use` on the
//! LocalQueue and the workload verbs the tests exercise in the namespace, and the context's identity
//! needs `create` on subjectaccessreviews.

use crucible_mcp_auth::testing::{self, JwksServer};
use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, CancelTaskParams, ClientCapabilities,
    ClientConfig, DetailedTask, GetTaskParams, Implementation, ProtocolVersion, TaskPayload,
    TaskStatus,
};
use rmcp::service::{ClientLifecycleMode, ClientServiceExt, RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

type Client = RunningService<RoleClient, ClientConfig>;

const IMAGE: &str = "registry.access.redhat.com/ubi9/ubi-minimal:latest";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Crucible,
    Tenant,
}

const TENANT_DOMAIN: &str = "kueue-mcp.e2e.example";

struct Env {
    context: String,
    namespace: String,
    queue: String,
    kube_user: String,
    kube_groups: Vec<String>,
    mode: Mode,
}

impl Env {
    fn config(&self) -> crucible_kueue::config::Config {
        use crucible_kueue::config::{ClaimMap, Config, Labels, Ownership};
        let served = vec![self.namespace.clone()];
        match self.mode {
            Mode::Crucible => Config {
                namespaces: served,
                ..Config::default()
            },
            Mode::Tenant => Config {
                namespaces: served,
                labels: Labels::new(TENANT_DOMAIN),
                ownership: Ownership::Rbac,
                profile: crucible_kueue::profile::Profile::tenant(),
                claims: ClaimMap {
                    user: "preferred_username".into(),
                    groups: "groups".into(),
                    run_id: None,
                    ..ClaimMap::default()
                },
                ..Config::default()
            },
        }
    }

    fn run_label(&self) -> String {
        format!(
            "{}/run-id",
            match self.mode {
                Mode::Crucible => "crucible.dev",
                Mode::Tenant => TENANT_DOMAIN,
            }
        )
    }

    /// The run id the server derives for a token minted for `run`.
    fn run_id(&self, run: &str) -> String {
        match self.mode {
            Mode::Crucible => run.to_string(),
            Mode::Tenant => crucible_kueue::config::label_safe(&subject(run)),
        }
    }
}

/// A tenant token's subject: an email-shaped value no label may hold as is.
fn subject(run: &str) -> String {
    format!("{run}@e2e.example")
}

fn env() -> Env {
    let var = |k: &str| {
        std::env::var(k).unwrap_or_else(|_| panic!("{k} must be set for the cluster e2e"))
    };
    Env {
        context: var("CRUCIBLE_KUEUE_E2E_CONTEXT"),
        namespace: var("CRUCIBLE_KUEUE_E2E_NAMESPACE"),
        queue: var("CRUCIBLE_KUEUE_E2E_QUEUE"),
        kube_user: var("CRUCIBLE_KUEUE_E2E_KUBE_USER"),
        kube_groups: std::env::var("CRUCIBLE_KUEUE_E2E_KUBE_GROUPS")
            .unwrap_or_default()
            .split(',')
            .filter(|g| !g.is_empty())
            .map(str::to_string)
            .collect(),
        mode: match std::env::var("CRUCIBLE_KUEUE_E2E_MODE").as_deref() {
            Ok("tenant") => Mode::Tenant,
            Ok("crucible") | Err(_) => Mode::Crucible,
            Ok(other) => panic!("CRUCIBLE_KUEUE_E2E_MODE {other:?} is not crucible or tenant"),
        },
    }
}

/// The server on loopback, trusting a loopback issuer whose run tokens [`submitter`] and
/// [`viewer`] mint.
async fn serve(env: &Env) -> String {
    let jwks = JwksServer::start(vec![testing::jwk("a")]).await;
    let mut rs = testing::config(jwks.url.clone());
    if env.mode == Mode::Tenant {
        rs.issuers[0].callers = crucible_mcp_auth::principal::Callers::People;
    }
    let oidc = crucible_mcp_auth::server::ResourceServer::with_jwks(rs, |t| {
        crucible_mcp_auth::jwks::Jwks::new(reqwest::Client::new(), t.jwks_uri.clone())
            .with_timing(Duration::from_secs(300), Duration::from_secs(1))
    });
    crucible_kueue::install_tls_provider();
    let options = kube::config::KubeConfigOptions {
        context: Some(env.context.clone()),
        ..Default::default()
    };
    let config = kube::Config::from_kubeconfig(&options).await.unwrap();
    let client = kube::Client::try_from(config).unwrap();
    let kueue_version = crucible_kueue::kueue::KueueVersion::discover(&client)
        .await
        .unwrap();
    let kinds = crucible_kueue::kinds::installed(&client).await.unwrap();
    let reviewer = Arc::new(crucible_mcp_auth::kube::SubjectAccessReviewer::new(
        client.clone(),
        crucible_mcp_auth::kube::GroupSource::Token,
    ));
    let app = crucible_kueue::router(
        client,
        crucible_kueue::Options {
            oidc: Arc::new(oidc),
            policy: crucible_mcp_auth::policy::ToolPolicy::default(),
            reviewer: Some(reviewer),
            kueue: kueue_version,
            allowed_hosts: Vec::new(),
            kinds,
            config: env.config(),
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}{}", crucible_kueue::MCP_PATH)
}

fn run_token(env: &Env, run: &str, scope: &str) -> String {
    testing::sign("a", &token_claims(env, run, scope))
}

/// The claims of a token for `run` in the server's mode.
fn token_claims(env: &Env, run: &str, scope: &str) -> Value {
    let mut claims = testing::run_claims();
    claims["namespace"] = json!(env.namespace);
    claims["scope"] = json!(scope);
    let map = claims.as_object_mut().unwrap();
    match env.mode {
        Mode::Crucible => {
            map.insert("run_id".into(), json!(run));
            map.insert("kube_user".into(), json!(env.kube_user));
            if !env.kube_groups.is_empty() {
                map.insert("kube_groups".into(), json!(env.kube_groups));
            }
        }
        Mode::Tenant => {
            for key in ["run_id", "kube_user", "kube_groups", "role", "queues"] {
                map.remove(key);
            }
            map.insert("sub".into(), json!(subject(run)));
            map.insert("preferred_username".into(), json!(env.kube_user));
            map.insert("groups".into(), json!(env.kube_groups));
        }
    }
    claims
}

/// A run's token that may submit, acting as the e2e user.
fn submitter(env: &Env, run: &str) -> String {
    run_token(env, run, "kueue.read kueue.submit")
}

/// A token that may only read, for another run in the same namespace.
fn viewer(env: &Env, run: &str) -> String {
    run_token(env, &format!("{run}-viewer"), "kueue.read")
}

async fn connect(url: &str, token: &str) -> Result<Client, String> {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url).auth_header(token),
    );
    ClientConfig::default()
        .serve(transport)
        .await
        .map_err(|e| e.to_string())
}

async fn connect_with_tasks(url: &str, token: &str) -> Client {
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

/// Plans and submits in task mode, returning the task id.
async fn submit_task(client: &Client, queue: &str, manifest: &Value) -> String {
    let plan = call(
        client,
        "workload_plan",
        json!({"queue": queue, "manifest": manifest}),
    )
    .await;
    assert_eq!(plan["status"], "ok", "{plan:#}");
    let Value::Object(args) =
        json!({"queue": queue, "manifest": manifest, "plan_hash": plan["plan_hash"]})
    else {
        unreachable!()
    };
    match client
        .call_tool_once(CallToolRequestParams::new("workload_submit").with_arguments(args))
        .await
        .unwrap()
    {
        CallToolResponse::Task(created) => {
            assert_eq!(created.task.status, TaskStatus::Working);
            assert!(created.task.poll_interval_ms.is_some());
            created.task.task_id
        }
        other => panic!("expected a task, got {other:?}"),
    }
}

async fn poll_task(client: &Client, id: &str, timeout: Duration) -> DetailedTask {
    let start = Instant::now();
    loop {
        let got = client
            .peer()
            .get_task(GetTaskParams::new(id.to_string()))
            .await
            .unwrap();
        eprintln!(
            "  task {id}: {:?} {}",
            got.task.status(),
            got.task.task.status_message.clone().unwrap_or_default()
        );
        if got.task.status().is_terminal() {
            return got.task;
        }
        assert!(start.elapsed() < timeout, "task {id} never finished");
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

fn completed_body(task: &DetailedTask) -> (bool, Value) {
    let TaskPayload::Completed { result } = &task.payload else {
        panic!("expected completed, got {:?}", task.payload)
    };
    let result: CallToolResult = serde_json::from_value(Value::Object(result.clone())).unwrap();
    let text = result.content[0].as_text().unwrap().text.clone();
    (
        result.is_error.unwrap_or(false),
        serde_json::from_str(&text).unwrap(),
    )
}

/// A tool's JSON reply. Every reply but `ok` must come back as a tool error with `retryable`.
async fn call(client: &Client, tool: &str, args: Value) -> Value {
    let Value::Object(map) = args else {
        panic!("args must be an object")
    };
    let result = client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(map))
        .await
        .unwrap_or_else(|e| panic!("{tool}: {e}"));
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_else(|| panic!("{tool}: no text content"));
    let body: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{tool}: {e}: {text}"));
    let failed = body["status"] != "ok";
    assert_eq!(
        result.is_error.unwrap_or(false),
        failed,
        "{tool}: isError must match the status: {body:#}"
    );
    if failed {
        assert!(body["retryable"].is_boolean(), "{tool}: {body:#}");
    }
    body
}

fn job(name: &str, script: &str) -> Value {
    json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {"name": name},
        "spec": {
            "backoffLimit": 0,
            "ttlSecondsAfterFinished": 600,
            "template": {"spec": {
                "restartPolicy": "Never",
                "containers": [{
                    "name": "main",
                    "image": IMAGE,
                    "command": ["/bin/sh", "-c", script],
                    "resources": {"requests": {"cpu": "100m", "memory": "64Mi"}, "limits": {"memory": "64Mi"}}
                }]
            }}
        }
    })
}

async fn wait_for_phase(client: &Client, name: &str, want: &[&str], timeout: Duration) -> Value {
    wait_until(client, "Job", name, timeout, |s| {
        want.contains(&s["phase"].as_str().unwrap_or_default())
    })
    .await
}

/// workload_status with a server-side wait, called again until `until` is reached.
async fn wait_on_server(
    client: &Client,
    kind: &str,
    name: &str,
    until: &str,
    timeout: Duration,
) -> Value {
    let start = Instant::now();
    loop {
        let s = call(
            client,
            "workload_status",
            json!({"kind": kind, "name": name, "until": until, "timeout_seconds": 20}),
        )
        .await;
        assert_eq!(s["status"], "ok", "{s:#}");
        assert_eq!(s["wait"]["until"], until, "{s:#}");
        assert!(
            s["wait"]["waited_seconds"].as_u64().unwrap() <= 22,
            "the wait is bounded: {s:#}"
        );
        eprintln!(
            "  {kind}/{name} until={until}: {} {}",
            s["wait"], s["phase"]
        );
        if s["wait"]["ended"] != "timeout" {
            return s;
        }
        assert!(
            start.elapsed() < timeout,
            "{kind}/{name} never reached {until}: {s:#}"
        );
    }
}

fn workloads_total(s: &Value) -> u64 {
    s["workload_counts"]
        .as_object()
        .map(|c| c.values().filter_map(Value::as_u64).sum())
        .unwrap_or(0)
}

async fn wait_until(
    client: &Client,
    kind: &str,
    name: &str,
    timeout: Duration,
    done: impl Fn(&Value) -> bool,
) -> Value {
    let start = Instant::now();
    loop {
        let s = call(
            client,
            "workload_status",
            json!({"kind": kind, "name": name}),
        )
        .await;
        eprintln!(
            "  {kind}/{name}: {} {} {}",
            s["phase"], s["workload_counts"], s["message"]
        );
        if done(&s) {
            return s;
        }
        assert!(
            start.elapsed() < timeout,
            "{kind}/{name} timed out; last status {s:#}"
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

fn admitted(s: &Value, n: u64) -> bool {
    s["phase"] == "admitted" && s["workload_counts"]["admitted"] == n
}

fn serving(kind: &str, name: &str, replicas: i64) -> Value {
    json!({
        "apiVersion": "apps/v1",
        "kind": kind,
        "metadata": {"name": name},
        "spec": {
            "replicas": replicas,
            "selector": {"matchLabels": {"app": name}},
            "template": {
                "metadata": {"labels": {"app": name}},
                "spec": {
                    "terminationGracePeriodSeconds": 1,
                    "containers": [{
                        "name": "main",
                        "image": IMAGE,
                        "command": ["/bin/sh", "-c", "sleep 3600"],
                        "resources": {"requests": {"cpu": "50m", "memory": "32Mi"}, "limits": {"memory": "32Mi"}}
                    }]
                }
            }
        }
    })
}

async fn submit(client: &Client, queue: &str, manifest: &Value) -> Value {
    let plan = call(
        client,
        "workload_plan",
        json!({"queue": queue, "manifest": manifest}),
    )
    .await;
    assert_eq!(plan["status"], "ok", "{plan:#}");
    let submitted = call(
        client,
        "workload_submit",
        json!({"queue": queue, "manifest": manifest, "plan_hash": plan["plan_hash"]}),
    )
    .await;
    assert_eq!(submitted["status"], "ok", "{submitted:#}");
    plan
}

async fn call_until_ok(client: &Client, tool: &str, args: Value, timeout: Duration) -> Value {
    let start = Instant::now();
    loop {
        let r = call(client, tool, args.clone()).await;
        if r["status"] == "ok" {
            return r;
        }
        assert!(
            r["status"] == "rejected" && start.elapsed() < timeout,
            "{tool}: {r:#}"
        );
        eprintln!("  {tool} rejected, retrying: {}", r["error"]);
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

fn unique(prefix: &str) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!("{prefix}-{secs}")
}

#[tokio::test]
#[ignore = "needs a cluster with Kueue; see the module docs"]
async fn an_agent_plans_submits_watches_and_cleans_up_a_job() {
    let env = env();
    let run = unique("e2e");
    let url = serve(&env).await;

    assert!(
        connect(&url, "wrong-token").await.is_err(),
        "an unknown token must not get a session"
    );

    let agent = connect(&url, &submitter(&env, &run)).await.unwrap();
    let tools: Vec<String> = agent
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    assert!(tools.contains(&"workload_submit".to_string()), "{tools:?}");

    let me = call(&agent, "whoami", json!({})).await;
    assert_eq!(me["namespace"], env.namespace.as_str());
    assert_eq!(me["run_id"], env.run_id(&run).as_str());
    assert_eq!(me["kube_user"], env.kube_user.as_str());
    assert!(
        me["submittable_kinds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k == "batch/v1 Job"),
        "{me:#}"
    );
    eprintln!("whoami: {me}");

    let queues = call(&agent, "queues_list", json!({})).await;
    assert!(
        queues["queues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|q| q["queue"] == env.queue.as_str()),
        "{queues:#}"
    );

    let described = call(&agent, "queue_describe", json!({"queue": env.queue})).await;
    assert_eq!(described["status"], "ok", "{described:#}");
    assert!(!described["quotas"].as_array().unwrap().is_empty());

    let hw = call(&agent, "hardware_describe", json!({"queue": env.queue})).await;
    assert_eq!(hw["status"], "ok", "{hw:#}");
    eprintln!(
        "hardware: {}",
        serde_json::to_string_pretty(&hw["flavors"]).unwrap()
    );

    let missing = call(&agent, "queue_describe", json!({"queue": "no-such-queue"})).await;
    assert_eq!(missing["status"], "not_found", "{missing:#}");
    assert_eq!(missing["retryable"], false, "{missing:#}");

    let privileged = {
        let mut m = job("never", "true");
        m["spec"]["template"]["spec"]["containers"][0]["securityContext"] =
            json!({"privileged": true});
        call(
            &agent,
            "workload_plan",
            json!({"queue": env.queue, "manifest": m}),
        )
        .await
    };
    assert_eq!(privileged["status"], "refused", "{privileged:#}");
    assert_eq!(privileged["retryable"], false, "{privileged:#}");

    let name = unique("ck-e2e");
    let manifest = job(&name, "echo crucible-kueue-sentinel; sleep 2");
    let plan = call(
        &agent,
        "workload_plan",
        json!({"queue": env.queue, "manifest": manifest}),
    )
    .await;
    assert_eq!(plan["status"], "ok", "{plan:#}");
    assert_eq!(plan["dry_run"], "accepted");
    assert_eq!(plan["fit"]["verdict"], "fits_now", "{:#}", plan["fit"]);
    assert_eq!(
        plan["finished_retention_seconds"], 600,
        "the manifest's own TTL"
    );
    let hash = plan["plan_hash"].as_str().unwrap().to_string();

    let tampered = call(
        &agent,
        "workload_submit",
        json!({"queue": env.queue, "manifest": job(&name, "echo something else"), "plan_hash": hash}),
    )
    .await;
    assert_eq!(tampered["status"], "refused", "{tampered:#}");

    let viewer = connect(&url, &viewer(&env, &run)).await.unwrap();
    let Value::Object(submit_args) =
        json!({"queue": env.queue, "manifest": manifest, "plan_hash": hash})
    else {
        unreachable!()
    };
    let as_viewer = viewer
        .call_tool(CallToolRequestParams::new("workload_submit").with_arguments(submit_args))
        .await;
    assert!(
        as_viewer.is_err(),
        "a read-only token cannot submit: {as_viewer:?}"
    );

    let submitted = call(
        &agent,
        "workload_submit",
        json!({"queue": env.queue, "manifest": manifest, "plan_hash": hash}),
    )
    .await;
    assert_eq!(submitted["status"], "ok", "{submitted:#}");
    assert_eq!(submitted["submitted"], true);
    assert_eq!(submitted["finished_retention_seconds"], 600);

    let again = call(
        &agent,
        "workload_submit",
        json!({"queue": env.queue, "manifest": manifest, "plan_hash": hash}),
    )
    .await;
    assert_eq!(again["status"], "ok", "{again:#}");
    assert_eq!(again["submitted"], false);

    let done = wait_on_server(&agent, "Job", &name, "finished", Duration::from_secs(300)).await;
    assert_eq!(done["wait"]["reached"], true, "{done:#}");
    assert_eq!(done["phase"], "finished", "{done:#}");
    assert_eq!(done["reason"], "Succeeded", "{done:#}");
    assert_eq!(done["object_status"]["succeeded"], 1, "{done:#}");
    assert!(done["admitted_to"]["cluster_queue"].is_string(), "{done:#}");
    assert!(
        done["next_action"]
            .as_str()
            .unwrap()
            .starts_with("Finished successfully"),
        "{done:#}"
    );
    assert!(
        done.get("workloads").is_none(),
        "the summary leaves conditions out"
    );
    let summary_bytes = done.to_string().len();
    let full = call(
        &agent,
        "workload_status",
        json!({"kind": "Job", "name": name, "detail": "full"}),
    )
    .await;
    assert!(
        full["workloads"][0]["admission"]["clusterQueue"].is_string(),
        "{full:#}"
    );
    eprintln!(
        "status bytes: summary {summary_bytes}, full {}",
        full.to_string().len()
    );
    assert!(summary_bytes < full.to_string().len());

    let logs = call(
        &agent,
        "workload_logs",
        json!({"kind": "Job", "name": name}),
    )
    .await;
    assert_eq!(logs["status"], "ok", "{logs:#}");
    assert!(
        logs["logs"][0]["log"]
            .as_str()
            .unwrap_or_default()
            .contains("crucible-kueue-sentinel"),
        "{logs:#}"
    );
    assert_eq!(logs["logs"][0]["role"], "job", "{logs:#}");
    assert_eq!(logs["budget_bytes"], 16384, "{logs:#}");
    assert_eq!(logs["truncated"], false, "{logs:#}");

    let listed = call(&agent, "workload_list", json!({})).await;
    assert!(
        listed["workloads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["name"] == name.as_str() && w["phase"] == "finished"),
        "{listed:#}"
    );

    let Value::Object(delete_args) = json!({"kind": "Job", "name": name}) else {
        unreachable!()
    };
    let viewer_delete = viewer
        .call_tool(CallToolRequestParams::new("workload_delete").with_arguments(delete_args))
        .await;
    assert!(
        viewer_delete.is_err(),
        "a read-only token gets a step-up challenge, not a delete: {viewer_delete:?}"
    );

    let deleted = call(
        &agent,
        "workload_delete",
        json!({"kind": "Job", "name": name}),
    )
    .await;
    assert_eq!(deleted["status"], "ok", "{deleted:#}");
}

#[tokio::test]
#[ignore = "needs a cluster with Kueue; see the module docs"]
async fn an_agent_stops_and_resumes_a_running_job() {
    let env = env();
    let run = unique("e2e-stop");
    let url = serve(&env).await;
    let agent = connect(&url, &submitter(&env, &run)).await.unwrap();

    let name = unique("ck-e2e-stop");
    let manifest = job(&name, "sleep 600");
    let plan = call(
        &agent,
        "workload_plan",
        json!({"queue": env.queue, "manifest": manifest}),
    )
    .await;
    assert_eq!(plan["status"], "ok", "{plan:#}");
    let submitted = call(
        &agent,
        "workload_submit",
        json!({"queue": env.queue, "manifest": manifest, "plan_hash": plan["plan_hash"]}),
    )
    .await;
    assert_eq!(submitted["status"], "ok", "{submitted:#}");
    wait_for_phase(&agent, &name, &["admitted"], Duration::from_secs(180)).await;

    let stopped = call(
        &agent,
        "workload_stop",
        json!({"kind": "Job", "name": name}),
    )
    .await;
    assert_eq!(stopped["status"], "ok", "{stopped:#}");
    wait_for_phase(&agent, &name, &["stopped"], Duration::from_secs(60)).await;

    let resumed = call(
        &agent,
        "workload_resume",
        json!({"kind": "Job", "name": name}),
    )
    .await;
    assert_eq!(resumed["status"], "ok", "{resumed:#}");
    wait_for_phase(&agent, &name, &["admitted"], Duration::from_secs(180)).await;

    let other_run = connect(&serve(&env).await, &submitter(&env, "someone-else"))
        .await
        .unwrap();
    if env.mode == Mode::Crucible {
        let foreign_stop = call(
            &other_run,
            "workload_stop",
            json!({"kind": "Job", "name": name}),
        )
        .await;
        assert_eq!(foreign_stop["status"], "refused", "{foreign_stop:#}");
        let foreign_delete = call(
            &other_run,
            "workload_delete",
            json!({"kind": "Job", "name": name}),
        )
        .await;
        assert_eq!(foreign_delete["status"], "refused", "{foreign_delete:#}");
    }

    let deleted = call(
        &agent,
        "workload_delete",
        json!({"kind": "Job", "name": name}),
    )
    .await;
    assert_eq!(deleted["status"], "ok", "{deleted:#}");
    let start = Instant::now();
    loop {
        let s = call(
            &agent,
            "workload_status",
            json!({"kind": "Job", "name": name}),
        )
        .await;
        if s["status"] == "not_found" {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "{name} not deleted: {s:#}"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[tokio::test]
#[ignore = "needs a cluster with Kueue; see the module docs"]
async fn a_deployment_is_admitted_pod_by_pod_and_scales_stops_and_resumes() {
    let env = env();
    let run = unique("e2e-deploy");
    let url = serve(&env).await;
    let agent = connect(&url, &submitter(&env, &run)).await.unwrap();
    let name = unique("ck-e2e-deploy");
    let r = json!({"kind": "Deployment", "name": name});

    let plan = submit(&agent, &env.queue, &serving("Deployment", &name, 2)).await;
    assert_eq!(plan["admission"], "per_pod", "{plan:#}");
    let s = wait_until(&agent, "Deployment", &name, Duration::from_secs(180), |s| {
        admitted(s, 2)
    })
    .await;
    assert_eq!(workloads_total(&s), 2, "{s:#}");

    let scaled = call(
        &agent,
        "workload_scale",
        json!({"kind": "Deployment", "name": name, "replicas": 3}),
    )
    .await;
    assert_eq!(scaled["status"], "ok", "{scaled:#}");
    assert_eq!(scaled["from"], 2);
    wait_until(&agent, "Deployment", &name, Duration::from_secs(180), |s| {
        admitted(s, 3)
    })
    .await;

    let stopped = call(&agent, "workload_stop", r.clone()).await;
    assert_eq!(stopped["stopped_from"], 3, "{stopped:#}");
    wait_until(&agent, "Deployment", &name, Duration::from_secs(120), |s| {
        workloads_total(s) == 0
    })
    .await;
    let not_scalable = call(
        &agent,
        "workload_scale",
        json!({"kind": "Job", "name": name, "replicas": 1}),
    )
    .await;
    assert_eq!(not_scalable["status"], "refused", "{not_scalable:#}");

    let resumed = call(&agent, "workload_resume", r.clone()).await;
    assert_eq!(resumed["replicas"], 3, "{resumed:#}");
    wait_until(&agent, "Deployment", &name, Duration::from_secs(180), |s| {
        admitted(s, 3)
    })
    .await;

    let deleted = call(&agent, "workload_delete", r).await;
    assert_eq!(deleted["status"], "ok", "{deleted:#}");
}

#[tokio::test]
#[ignore = "needs a cluster with Kueue; see the module docs"]
async fn a_statefulset_is_admitted_as_one_group_and_stops_by_scaling_to_zero() {
    let env = env();
    let run = unique("e2e-sts");
    let url = serve(&env).await;
    let agent = connect(&url, &submitter(&env, &run)).await.unwrap();
    let name = unique("ck-e2e-sts");
    let r = json!({"kind": "StatefulSet", "name": name});

    let plan = submit(&agent, &env.queue, &serving("StatefulSet", &name, 2)).await;
    assert_eq!(plan["admission"], "all_or_nothing", "{plan:#}");
    let s = wait_until(
        &agent,
        "StatefulSet",
        &name,
        Duration::from_secs(180),
        |s| admitted(s, 1),
    )
    .await;
    assert_eq!(workloads_total(&s), 1, "one group Workload: {s:#}");

    let resize = call(
        &agent,
        "workload_scale",
        json!({"kind": "StatefulSet", "name": name, "replicas": 3}),
    )
    .await;
    assert_eq!(
        resize["status"], "rejected",
        "Kueue only scales a StatefulSet to or from zero: {resize:#}"
    );
    eprintln!("resize rejected, retryable={}", resize["retryable"]);

    let stopped = call(&agent, "workload_stop", r.clone()).await;
    assert_eq!(stopped["stopped_from"], 2, "{stopped:#}");
    let resumed = call_until_ok(
        &agent,
        "workload_resume",
        r.clone(),
        Duration::from_secs(120),
    )
    .await;
    assert_eq!(resumed["replicas"], 2, "{resumed:#}");
    wait_until(
        &agent,
        "StatefulSet",
        &name,
        Duration::from_secs(180),
        |s| admitted(s, 1),
    )
    .await;

    let deleted = call(&agent, "workload_delete", r).await;
    assert_eq!(deleted["status"], "ok", "{deleted:#}");
}

#[tokio::test]
#[ignore = "needs a cluster with Kueue; see the module docs"]
async fn a_tasks_client_gets_a_task_and_polls_a_job_to_completion() {
    let env = env();
    let run = unique("e2e-task");
    let url = serve(&env).await;
    let agent = connect_with_tasks(&url, &submitter(&env, &run)).await;

    let name = unique("ck-e2e-task");
    let id = submit_task(
        &agent,
        &env.queue,
        &job(&name, "echo task-sentinel; sleep 2"),
    )
    .await;
    assert!(
        id.starts_with(&format!("{}/Job/{name}/", env.namespace)),
        "{id}"
    );

    let done = poll_task(&agent, &id, Duration::from_secs(300)).await;
    assert_eq!(done.status(), TaskStatus::Completed);
    let (is_error, body) = completed_body(&done);
    assert!(!is_error, "{body:#}");
    assert_eq!(body["status"], "ok", "{body:#}");
    assert_eq!(body["outcome"], "succeeded", "{body:#}");
    assert_eq!(body["report"]["object_status"]["succeeded"], 1, "{body:#}");

    let again = poll_task(&agent, &id, Duration::from_secs(5)).await;
    assert_eq!(
        again.status(),
        TaskStatus::Completed,
        "terminal stays terminal"
    );

    let failing = unique("ck-e2e-task-fail");
    let id = submit_task(&agent, &env.queue, &job(&failing, "exit 3")).await;
    let done = poll_task(&agent, &id, Duration::from_secs(300)).await;
    let (is_error, body) = completed_body(&done);
    assert!(
        is_error,
        "a failed Job completes the task with a tool error: {body:#}"
    );
    assert_eq!(
        body["status"], "failed",
        "never ok beside a failed outcome: {body:#}"
    );
    assert_eq!(body["outcome"], "failed");
    assert_eq!(
        body["report"]["problem_pods"][0]["exit_code"], 3,
        "{body:#}"
    );

    for n in [&name, &failing] {
        let deleted = call(&agent, "workload_delete", json!({"kind": "Job", "name": n})).await;
        assert_eq!(deleted["status"], "ok", "{deleted:#}");
    }
    let start = Instant::now();
    loop {
        let s = call(
            &agent,
            "workload_status",
            json!({"kind": "Job", "name": failing}),
        )
        .await;
        if s["status"] == "not_found" {
            assert!(
                s["error"]
                    .as_str()
                    .unwrap()
                    .contains("ttlSecondsAfterFinished"),
                "{s:#}"
            );
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(60), "{s:#}");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let gone = agent
        .peer()
        .get_task(GetTaskParams::new(id.clone()))
        .await
        .expect_err("a deleted task's object is gone")
        .to_string();
    assert!(
        gone.contains("no longer exists") && !gone.contains("unknown task"),
        "{gone}"
    );
}

#[tokio::test]
#[ignore = "needs a cluster with Kueue; see the module docs"]
async fn a_failing_job_names_its_problem_pod_and_its_logs_come_first() {
    let env = env();
    let run = unique("e2e-fail");
    let url = serve(&env).await;
    let agent = connect(&url, &submitter(&env, &run)).await.unwrap();
    let name = unique("ck-e2e-fail");
    let script = "for i in $(seq 1 400); do echo noise-line-$i-padding-padding-padding; done; \
        echo boom-sentinel >&2; echo bad-input > /dev/termination-log; exit 3";
    submit(&agent, &env.queue, &job(&name, script)).await;

    let done = wait_on_server(&agent, "Job", &name, "finished", Duration::from_secs(300)).await;
    assert_eq!(done["phase"], "finished", "{done:#}");
    assert_ne!(done["reason"], "Succeeded", "{done:#}");
    let problem = &done["problem_pods"][0];
    assert_eq!(problem["exit_code"], 3, "{done:#}");
    assert_eq!(problem["termination_message"], "bad-input", "{done:#}");
    assert_eq!(problem["role"], "job", "{done:#}");
    assert!(
        done["next_action"]
            .as_str()
            .unwrap()
            .starts_with("Finished and failed"),
        "{done:#}"
    );

    let logs = call(
        &agent,
        "workload_logs",
        json!({"kind": "Job", "name": name, "tail_lines": 2000, "max_bytes": 2048}),
    )
    .await;
    assert_eq!(logs["status"], "ok", "{logs:#}");
    let log = logs["logs"][0]["log"].as_str().unwrap();
    assert!(log.len() <= 2048, "{logs:#}");
    assert!(log.contains("boom-sentinel"), "the tail is kept: {logs:#}");
    assert_eq!(logs["truncated"], true, "{logs:#}");
    assert!(logs["bytes_omitted"].as_u64().unwrap() > 10_000, "{logs:#}");
    assert_eq!(logs["logs"][0]["phase"], "Failed", "{logs:#}");
    let log_bytes = logs.to_string().len();

    let grepped = call(
        &agent,
        "workload_logs",
        json!({"kind": "Job", "name": name, "grep": "boom"}),
    )
    .await;
    assert_eq!(grepped["logs"][0]["log"], "boom-sentinel", "{grepped:#}");
    eprintln!(
        "log bytes: budgeted {log_bytes}, grep {}",
        grepped.to_string().len()
    );

    let deleted = call(
        &agent,
        "workload_delete",
        json!({"kind": "Job", "name": name}),
    )
    .await;
    assert_eq!(deleted["status"], "ok", "{deleted:#}");
}

#[tokio::test]
#[ignore = "needs a cluster with Kueue; see the module docs"]
async fn a_deployment_is_no_task_and_reports_readiness_until_stopped() {
    let env = env();
    let run = unique("e2e-ready-deploy");
    let url = serve(&env).await;
    let agent = connect_with_tasks(&url, &submitter(&env, &run)).await;
    let name = unique("ck-e2e-ready-deploy");
    let manifest = serving("Deployment", &name, 2);

    let plan = call(
        &agent,
        "workload_plan",
        json!({"queue": env.queue, "manifest": manifest}),
    )
    .await;
    assert_eq!(plan["status"], "ok", "{plan:#}");
    let Value::Object(args) =
        json!({"queue": env.queue, "manifest": manifest, "plan_hash": plan["plan_hash"]})
    else {
        unreachable!()
    };
    let submitted = agent
        .call_tool_once(CallToolRequestParams::new("workload_submit").with_arguments(args))
        .await
        .unwrap();
    assert!(
        matches!(submitted, CallToolResponse::Complete(_)),
        "a Deployment is answered synchronously even to a tasks client: {submitted:?}"
    );

    let up = wait_on_server(
        &agent,
        "Deployment",
        &name,
        "ready",
        Duration::from_secs(240),
    )
    .await;
    assert_eq!(up["wait"]["reached"], true, "{up:#}");
    assert_eq!(
        up["readiness"],
        json!({"ready": true, "replicas": 2, "ready_replicas": 2})
    );

    let stopped = call(
        &agent,
        "workload_stop",
        json!({"kind": "Deployment", "name": name}),
    )
    .await;
    assert_eq!(stopped["status"], "ok", "{stopped:#}");
    wait_until(&agent, "Deployment", &name, Duration::from_secs(60), |s| {
        s["readiness"]["ready"] == false
    })
    .await;

    let deleted = call(
        &agent,
        "workload_delete",
        json!({"kind": "Deployment", "name": name}),
    )
    .await;
    assert_eq!(deleted["status"], "ok", "{deleted:#}");
}

#[tokio::test]
#[ignore = "needs a cluster with Kueue; see the module docs"]
async fn cancelling_a_task_stops_the_job_and_unknown_tasks_are_errors() {
    let env = env();
    let run = unique("e2e-task-cancel");
    let url = serve(&env).await;
    let agent = connect_with_tasks(&url, &submitter(&env, &run)).await;
    let name = unique("ck-e2e-task-cancel");

    let id = submit_task(&agent, &env.queue, &job(&name, "sleep 600")).await;
    wait_for_phase(&agent, &name, &["admitted"], Duration::from_secs(180)).await;

    let viewer = connect_with_tasks(&url, &viewer(&env, &run)).await;
    assert!(
        viewer
            .peer()
            .cancel_task(CancelTaskParams::new(id.clone()))
            .await
            .is_err(),
        "a viewer cannot cancel"
    );

    agent
        .peer()
        .cancel_task(CancelTaskParams::new(id.clone()))
        .await
        .unwrap();
    let cancelled = poll_task(&agent, &id, Duration::from_secs(5)).await;
    assert_eq!(cancelled.status(), TaskStatus::Cancelled);
    wait_for_phase(&agent, &name, &["stopped"], Duration::from_secs(60)).await;
    agent
        .peer()
        .cancel_task(CancelTaskParams::new(id.clone()))
        .await
        .unwrap();

    let forged = format!("Job/{name}/00000000-0000-0000-0000-000000000000");
    for bad in [forged.as_str(), "Job/nope", "not-a-task"] {
        assert!(
            agent
                .peer()
                .get_task(GetTaskParams::new(bad.to_string()))
                .await
                .is_err(),
            "{bad} must be an unknown task"
        );
    }

    let other_run = connect_with_tasks(&serve(&env).await, &submitter(&env, "someone-else")).await;
    let foreign = other_run
        .peer()
        .cancel_task(CancelTaskParams::new(id.clone()))
        .await;
    match env.mode {
        Mode::Crucible => assert!(
            foreign.is_err(),
            "another run cannot cancel this run's task"
        ),
        Mode::Tenant => assert!(
            foreign.is_ok(),
            "under rbac ownership a caller whose RBAC permits it may cancel: {foreign:?}"
        ),
    }

    let deleted = call(
        &agent,
        "workload_delete",
        json!({"kind": "Job", "name": name}),
    )
    .await;
    assert_eq!(deleted["status"], "ok", "{deleted:#}");
}

async fn kube_client(env: &Env) -> kube::Client {
    let options = kube::config::KubeConfigOptions {
        context: Some(env.context.clone()),
        ..Default::default()
    };
    let config = kube::Config::from_kubeconfig(&options).await.unwrap();
    kube::Client::try_from(config).unwrap()
}

#[tokio::test]
#[ignore = "needs a cluster with Kueue; see the module docs"]
async fn a_job_made_outside_the_server_is_observed_and_controlled_only_under_rbac_ownership() {
    let env = env();
    let run = unique("e2e-adopt");
    let url = serve(&env).await;
    let agent = connect(&url, &submitter(&env, &run)).await.unwrap();
    let name = unique("ck-e2e-kubectl");
    let jobs: kube::Api<k8s_openapi::api::batch::v1::Job> =
        kube::Api::namespaced(kube_client(&env).await, &env.namespace);
    let mut manifest = job(&name, "echo made-by-kubectl; sleep 600");
    manifest["metadata"]["labels"] = json!({"kueue.x-k8s.io/queue-name": env.queue});
    manifest["spec"]["suspend"] = json!(true);
    jobs.create(
        &kube::api::PostParams::default(),
        &serde_json::from_value(manifest).unwrap(),
    )
    .await
    .unwrap();

    let listed = call(&agent, "workload_list", json!({"filter": "all"})).await;
    let row = listed["workloads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["name"] == name.as_str())
        .cloned()
        .unwrap_or_else(|| panic!("{name} not in {listed:#}"));
    assert_eq!(row["managed"], false, "{row:#}");
    let mine = call(&agent, "workload_list", json!({})).await;
    assert!(
        !mine["workloads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["name"] == name.as_str()),
        "the default filter lists only this run's: {mine:#}"
    );

    let running = wait_on_server(&agent, "Job", &name, "running", Duration::from_secs(180)).await;
    assert_eq!(
        running["pods"]["total"], 1,
        "pods found through the Job's selector: {running:#}"
    );
    let logs = call(
        &agent,
        "workload_logs",
        json!({"kind": "Job", "name": name}),
    )
    .await;
    assert!(
        logs["logs"][0]["log"]
            .as_str()
            .unwrap_or_default()
            .contains("made-by-kubectl"),
        "{logs:#}"
    );

    let stopped = call(
        &agent,
        "workload_stop",
        json!({"kind": "Job", "name": name}),
    )
    .await;
    match env.mode {
        Mode::Crucible => {
            assert_eq!(stopped["status"], "refused", "{stopped:#}");
            let deleted = call(
                &agent,
                "workload_delete",
                json!({"kind": "Job", "name": name}),
            )
            .await;
            assert_eq!(deleted["status"], "refused", "{deleted:#}");
            jobs.delete(&name, &kube::api::DeleteParams::background())
                .await
                .unwrap();
        }
        Mode::Tenant => {
            assert_eq!(stopped["status"], "ok", "{stopped:#}");
            wait_for_phase(&agent, &name, &["stopped"], Duration::from_secs(60)).await;
            let deleted = call(
                &agent,
                "workload_delete",
                json!({"kind": "Job", "name": name}),
            )
            .await;
            assert_eq!(deleted["status"], "ok", "{deleted:#}");
        }
    }
}

#[tokio::test]
#[ignore = "needs a cluster with Kueue; see the module docs"]
async fn the_profile_decides_tolerations_and_secrets_and_the_domain_labels_what_is_made() {
    let env = env();
    let run = unique("e2e-profile");
    let url = serve(&env).await;
    let agent = connect(&url, &submitter(&env, &run)).await.unwrap();
    let name = unique("ck-e2e-profile");
    let mut manifest = job(&name, "true");
    let pod = &mut manifest["spec"]["template"]["spec"];
    pod["tolerations"] =
        json!([{"key": "example.com/dedicated", "operator": "Exists", "effect": "NoSchedule"}]);
    pod["containers"][0]["env"] = json!([{"name": "TOKEN", "valueFrom": {"secretKeyRef": {"name": "e2e-token", "key": "t", "optional": true}}}]);
    let plan = call(
        &agent,
        "workload_plan",
        json!({"queue": env.queue, "manifest": manifest}),
    )
    .await;
    match env.mode {
        Mode::Crucible => {
            assert_eq!(
                plan["status"], "refused",
                "strict refuses tolerations: {plan:#}"
            );
            assert!(
                plan["error"].as_str().unwrap().contains("allowed here:"),
                "{plan:#}"
            );
        }
        Mode::Tenant => assert_eq!(plan["status"], "ok", "tenant admits them: {plan:#}"),
    }

    let plain = unique("ck-e2e-domain");
    submit(&agent, &env.queue, &job(&plain, "true")).await;
    let jobs: kube::Api<k8s_openapi::api::batch::v1::Job> =
        kube::Api::namespaced(kube_client(&env).await, &env.namespace);
    let made = jobs.get(&plain).await.unwrap();
    let labels = made.metadata.labels.unwrap_or_default();
    assert_eq!(
        labels.get(&env.run_label()).map(String::as_str),
        Some(env.run_id(&run).as_str()),
        "{labels:?}"
    );
    let deleted = call(
        &agent,
        "workload_delete",
        json!({"kind": "Job", "name": plain}),
    )
    .await;
    assert_eq!(deleted["status"], "ok", "{deleted:#}");
}

#[tokio::test]
#[ignore = "needs a cluster with Kueue; see the module docs"]
async fn a_token_without_a_namespace_finds_and_defaults_to_the_one_it_can_use() {
    let env = env();
    let run = unique("e2e-ns");
    let url = serve(&env).await;
    let mut claims = token_claims(&env, &run, "kueue.read kueue.submit");
    claims.as_object_mut().unwrap().remove("namespace");
    let agent = connect(&url, &testing::sign("a", &claims)).await.unwrap();

    let me = call(&agent, "whoami", json!({})).await;
    assert_eq!(me["namespaces"], json!([env.namespace]), "{me:#}");
    assert_eq!(
        me["namespace"],
        env.namespace.as_str(),
        "the only usable namespace is the default: {me:#}"
    );
    assert!(
        me["queues"][&env.namespace]
            .as_array()
            .unwrap()
            .iter()
            .any(|q| q == env.queue.as_str()),
        "{me:#}"
    );
    let queues = call(&agent, "queues_list", json!({})).await;
    assert_eq!(queues["namespace"], env.namespace.as_str(), "{queues:#}");
}

#[tokio::test]
#[ignore = "needs a cluster with Kueue; see the module docs"]
async fn naming_a_namespace_the_server_does_not_serve_lists_the_ones_it_does() {
    let env = env();
    let run = unique("e2e-wrong-ns");
    let url = serve(&env).await;
    let agent = connect(&url, &submitter(&env, &run)).await.unwrap();
    let wrong = call(
        &agent,
        "queue_describe",
        json!({"namespace": env.queue, "queue": env.queue}),
    )
    .await;
    assert_eq!(wrong["status"], "not_found", "{wrong:#}");
    let error = wrong["error"].as_str().unwrap();
    assert!(
        error.ends_with(&format!("namespaces you can use: {}", env.namespace)),
        "{wrong:#}"
    );
    assert!(!error.contains("system:serviceaccount"), "{wrong:#}");
}

#[tokio::test]
#[ignore = "needs a cluster with Kueue; see the module docs"]
async fn a_reader_without_pod_rbac_gets_the_status_without_pods() {
    use k8s_openapi::api::rbac::v1::{PolicyRule, Role, RoleBinding, RoleRef, Subject};
    use kube::api::{Api, DeleteParams, PostParams};

    let env = env();
    let run = unique("e2e-jobs-only");
    let url = serve(&env).await;
    let agent = connect(&url, &submitter(&env, &run)).await.unwrap();
    let name = unique("jobs-only");
    submit(&agent, &env.queue, &job(&name, "sleep 20")).await;
    wait_for_phase(
        &agent,
        &name,
        &["admitted", "finished"],
        Duration::from_secs(180),
    )
    .await;

    let user = format!("{run}-reader");
    let kube = kube_client(&env).await;
    let rule = |group: &str, resource: &str, verbs: &[&str]| PolicyRule {
        api_groups: Some(vec![group.into()]),
        resources: Some(vec![resource.into()]),
        verbs: verbs.iter().map(|v| v.to_string()).collect(),
        ..Default::default()
    };
    let roles: Api<Role> = Api::namespaced(kube.clone(), &env.namespace);
    let bindings: Api<RoleBinding> = Api::namespaced(kube.clone(), &env.namespace);
    let meta = kube::api::ObjectMeta {
        name: Some(user.clone()),
        ..Default::default()
    };
    roles
        .create(
            &PostParams::default(),
            &Role {
                metadata: meta.clone(),
                rules: Some(vec![
                    rule("batch", "jobs", &["get"]),
                    rule("kueue.x-k8s.io", "localqueues", &["list"]),
                ]),
            },
        )
        .await
        .unwrap();
    bindings
        .create(
            &PostParams::default(),
            &RoleBinding {
                metadata: meta,
                role_ref: RoleRef {
                    api_group: "rbac.authorization.k8s.io".into(),
                    kind: "Role".into(),
                    name: user.clone(),
                },
                subjects: Some(vec![Subject {
                    api_group: Some("rbac.authorization.k8s.io".into()),
                    kind: "User".into(),
                    name: user.clone(),
                    ..Default::default()
                }]),
            },
        )
        .await
        .unwrap();

    let mut claims = token_claims(&env, &format!("{run}-reader"), "kueue.read");
    let map = claims.as_object_mut().unwrap();
    for key in ["kube_groups", "groups"] {
        map.remove(key);
    }
    let user_claim = match env.mode {
        Mode::Crucible => "kube_user",
        Mode::Tenant => "preferred_username",
    };
    map.insert(user_claim.into(), json!(user));
    let reader = connect(&url, &testing::sign("a", &claims)).await.unwrap();

    for detail in ["summary", "full"] {
        let s = call(
            &reader,
            "workload_status",
            json!({"kind": "Job", "name": name, "detail": detail}),
        )
        .await;
        assert_eq!(s["status"], "ok", "{s:#}");
        assert_eq!(s["name"], name.as_str(), "{s:#}");
        assert!(
            s.get("object_status").is_some(),
            "the Job's own status: {s:#}"
        );
        for hidden in [
            "pods",
            "problem_pods",
            "pod_list",
            "phase",
            "workloads",
            "next_action",
            "events",
        ] {
            assert!(s.get(hidden).is_none(), "{hidden} shown: {s:#}");
        }
        let omitted: Vec<&str> = s["omitted"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["part"].as_str().unwrap())
            .collect();
        assert_eq!(omitted, ["workloads", "pods", "events"], "{s:#}");
        let reasons = s["omitted"].to_string();
        assert!(
            reasons.contains(&format!(
                "you may not get and list pods in namespace {}",
                env.namespace
            )),
            "{s:#}"
        );
    }

    let own = call(
        &agent,
        "workload_status",
        json!({"kind": "Job", "name": name}),
    )
    .await;
    assert_eq!(own["status"], "ok", "{own:#}");
    assert!(own.get("omitted").is_none(), "{own:#}");
    assert!(
        own.get("pods").is_some() && own.get("phase").is_some(),
        "{own:#}"
    );

    bindings
        .delete(&user, &DeleteParams::default())
        .await
        .unwrap();
    roles.delete(&user, &DeleteParams::default()).await.unwrap();
    let deleted = call(
        &agent,
        "workload_delete",
        json!({"kind": "Job", "name": name}),
    )
    .await;
    assert_eq!(deleted["status"], "ok", "{deleted:#}");
}

/// Agents can always discover the hardware behind a queue they may submit to: a user with only
/// `use` on the LocalQueue and `create` on Jobs, and no RBAC on nodes, gets the same
/// `hardware_describe` and `queue_describe` answers as the e2e user.
#[tokio::test]
#[ignore = "needs a cluster with Kueue; see the module docs"]
async fn a_submitter_without_node_rbac_discovers_the_queues_hardware() {
    use k8s_openapi::api::rbac::v1::{PolicyRule, Role, RoleBinding, RoleRef, Subject};
    use kube::api::{Api, DeleteParams, PostParams};

    let env = env();
    let run = unique("e2e-hw");
    let url = serve(&env).await;
    let user = format!("{run}-submitter");
    let kube = kube_client(&env).await;
    let rule = |group: &str, resource: &str, verb: &str| PolicyRule {
        api_groups: Some(vec![group.into()]),
        resources: Some(vec![resource.into()]),
        verbs: vec![verb.into()],
        ..Default::default()
    };
    let roles: Api<Role> = Api::namespaced(kube.clone(), &env.namespace);
    let bindings: Api<RoleBinding> = Api::namespaced(kube.clone(), &env.namespace);
    let meta = kube::api::ObjectMeta {
        name: Some(user.clone()),
        ..Default::default()
    };
    roles
        .create(
            &PostParams::default(),
            &Role {
                metadata: meta.clone(),
                rules: Some(vec![
                    rule("kueue.x-k8s.io", "localqueues", "use"),
                    rule("batch", "jobs", "create"),
                ]),
            },
        )
        .await
        .unwrap();
    bindings
        .create(
            &PostParams::default(),
            &RoleBinding {
                metadata: meta,
                role_ref: RoleRef {
                    api_group: "rbac.authorization.k8s.io".into(),
                    kind: "Role".into(),
                    name: user.clone(),
                },
                subjects: Some(vec![Subject {
                    api_group: Some("rbac.authorization.k8s.io".into()),
                    kind: "User".into(),
                    name: user.clone(),
                    ..Default::default()
                }]),
            },
        )
        .await
        .unwrap();

    let mut claims = token_claims(&env, &run, "kueue.read kueue.submit");
    let map = claims.as_object_mut().unwrap();
    for key in ["kube_groups", "groups"] {
        map.remove(key);
    }
    let user_claim = match env.mode {
        Mode::Crucible => "kube_user",
        Mode::Tenant => "preferred_username",
    };
    map.insert(user_claim.into(), json!(user));
    let submitter = connect(&url, &testing::sign("a", &claims)).await.unwrap();
    let admin = connect(&url, &viewer(&env, &run)).await.unwrap();

    let hw = call(&submitter, "hardware_describe", json!({"queue": env.queue})).await;
    assert_eq!(hw["status"], "ok", "{hw:#}");
    let flavors = hw["flavors"].as_array().unwrap();
    assert!(!flavors.is_empty(), "{hw:#}");
    for f in flavors {
        for key in ["node_labels", "gpu_products", "matching_nodes", "provides"] {
            assert!(f.get(key).is_some(), "{key} missing: {f:#}");
        }
    }
    let full = call(&admin, "hardware_describe", json!({"queue": env.queue})).await;
    assert_eq!(
        hw, full,
        "the submitter sees exactly what the e2e user sees"
    );

    let described = call(&submitter, "queue_describe", json!({"queue": env.queue})).await;
    assert_eq!(described["status"], "ok", "{described:#}");

    bindings
        .delete(&user, &DeleteParams::default())
        .await
        .unwrap();
    roles.delete(&user, &DeleteParams::default()).await.unwrap();
}
