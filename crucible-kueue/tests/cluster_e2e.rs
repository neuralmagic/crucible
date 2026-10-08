//! End-to-end against a real cluster with Kueue: serves the MCP endpoint on loopback, drives it
//! with an rmcp client, and runs real Jobs through a LocalQueue.
//!
//! Run with:
//!   CRUCIBLE_KUEUE_E2E_CONTEXT=<kube context> CRUCIBLE_KUEUE_E2E_NAMESPACE=<ns> \
//!   CRUCIBLE_KUEUE_E2E_QUEUE=<local queue> cargo test --test cluster_e2e -- --ignored --nocapture
//! The namespace must carry the label Kueue's ClusterQueue selects on and hold the LocalQueue.

use crucible_contract::kueue::token_digest;
use crucible_kueue::tenant::{Registry, Tenants};
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

const SUBMITTER: &str = "e2e-submitter-token";
const VIEWER: &str = "e2e-viewer-token";
const IMAGE: &str = "registry.access.redhat.com/ubi9/ubi-minimal:latest";

struct Env {
    context: String,
    namespace: String,
    queue: String,
}

fn env() -> Env {
    let var = |k: &str| {
        std::env::var(k).unwrap_or_else(|_| panic!("{k} must be set for the cluster e2e"))
    };
    Env {
        context: var("CRUCIBLE_KUEUE_E2E_CONTEXT"),
        namespace: var("CRUCIBLE_KUEUE_E2E_NAMESPACE"),
        queue: var("CRUCIBLE_KUEUE_E2E_QUEUE"),
    }
}

async fn serve(env: &Env, run_id: &str) -> String {
    let tenants = Tenants::from_toml(&format!(
        r#"
[[tenant]]
run_id = "{run_id}"
token_sha256 = "{}"
namespace = "{ns}"
role = "submitter"
queues = ["{q}"]

[[tenant]]
run_id = "{run_id}-viewer"
token_sha256 = "{}"
namespace = "{ns}"
role = "viewer"
"#,
        token_digest(SUBMITTER),
        token_digest(VIEWER),
        ns = env.namespace,
        q = env.queue,
    ))
    .unwrap();
    crucible_kueue::install_tls_provider();
    let options = kube::config::KubeConfigOptions {
        context: Some(env.context.clone()),
        ..Default::default()
    };
    let config = kube::Config::from_kubeconfig(&options).await.unwrap();
    let client = kube::Client::try_from(config).unwrap();
    let auth = crucible_kueue::auth::Auth {
        registry: Arc::new(Registry::new(tenants)),
        oidc: None,
    };
    let app = crucible_kueue::router(client, auth, None, &[]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}{}", crucible_kueue::MCP_PATH)
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
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{tool}: {e}: {text}"))
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
        want.contains(&s["phase"]["phase"].as_str().unwrap_or_default())
    })
    .await
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
            s["phase"]["phase"], s["phase"]["counts"], s["phase"]["message"]
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
    s["phase"]["phase"] == "admitted" && s["phase"]["counts"]["admitted"] == n
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
    let url = serve(&env, &run).await;

    assert!(
        connect(&url, "wrong-token").await.is_err(),
        "an unknown token must not get a session"
    );

    let agent = connect(&url, SUBMITTER).await.unwrap();
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
    assert_eq!(me["role"], "submitter");

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

    let other = call(
        &agent,
        "queue_describe",
        json!({"queue": "not-in-allowlist"}),
    )
    .await;
    assert_eq!(other["status"], "refused", "{other:#}");

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
    let hash = plan["plan_hash"].as_str().unwrap().to_string();

    let tampered = call(
        &agent,
        "workload_submit",
        json!({"queue": env.queue, "manifest": job(&name, "echo something else"), "plan_hash": hash}),
    )
    .await;
    assert_eq!(tampered["status"], "refused", "{tampered:#}");

    let viewer = connect(&url, VIEWER).await.unwrap();
    let as_viewer = call(
        &viewer,
        "workload_submit",
        json!({"queue": env.queue, "manifest": manifest, "plan_hash": hash}),
    )
    .await;
    assert_eq!(as_viewer["status"], "refused", "{as_viewer:#}");

    let submitted = call(
        &agent,
        "workload_submit",
        json!({"queue": env.queue, "manifest": manifest, "plan_hash": hash}),
    )
    .await;
    assert_eq!(submitted["status"], "ok", "{submitted:#}");
    assert_eq!(submitted["submitted"], true);

    let again = call(
        &agent,
        "workload_submit",
        json!({"queue": env.queue, "manifest": manifest, "plan_hash": hash}),
    )
    .await;
    assert_eq!(again["status"], "ok", "{again:#}");
    assert_eq!(again["submitted"], false);

    let done = wait_for_phase(&agent, &name, &["finished"], Duration::from_secs(300)).await;
    assert_eq!(done["object_status"]["succeeded"], 1, "{done:#}");
    assert!(
        done["workloads"][0]["admission"]["clusterQueue"].is_string(),
        "{done:#}"
    );

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

    let listed = call(&agent, "workload_list", json!({})).await;
    assert!(
        listed["workloads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["name"] == name.as_str() && w["phase"] == "finished"),
        "{listed:#}"
    );

    let viewer_delete = call(
        &viewer,
        "workload_delete",
        json!({"kind": "Job", "name": name}),
    )
    .await;
    assert_eq!(viewer_delete["status"], "refused", "{viewer_delete:#}");

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
    let url = serve(&env, &run).await;
    let agent = connect(&url, SUBMITTER).await.unwrap();

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

    let other_run = connect(&serve(&env, "someone-else").await, SUBMITTER)
        .await
        .unwrap();
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
    let url = serve(&env, &unique("e2e-deploy")).await;
    let agent = connect(&url, SUBMITTER).await.unwrap();
    let name = unique("ck-e2e-deploy");
    let r = json!({"kind": "Deployment", "name": name});

    let plan = submit(&agent, &env.queue, &serving("Deployment", &name, 2)).await;
    assert_eq!(plan["admission"], "per_pod", "{plan:#}");
    let s = wait_until(&agent, "Deployment", &name, Duration::from_secs(180), |s| {
        admitted(s, 2)
    })
    .await;
    assert_eq!(s["workloads_total"], 2, "{s:#}");

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
        s["workloads_total"] == 0
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
    let url = serve(&env, &unique("e2e-sts")).await;
    let agent = connect(&url, SUBMITTER).await.unwrap();
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
    assert_eq!(s["workloads_total"], 1, "one group Workload: {s:#}");

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
    let url = serve(&env, &unique("e2e-task")).await;
    let agent = connect_with_tasks(&url, SUBMITTER).await;

    let name = unique("ck-e2e-task");
    let id = submit_task(
        &agent,
        &env.queue,
        &job(&name, "echo task-sentinel; sleep 2"),
    )
    .await;
    assert!(id.starts_with(&format!("Job/{name}/")), "{id}");

    let done = poll_task(&agent, &id, Duration::from_secs(300)).await;
    assert_eq!(done.status(), TaskStatus::Completed);
    let (is_error, body) = completed_body(&done);
    assert!(!is_error, "{body:#}");
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
    assert_eq!(body["outcome"], "failed");

    for n in [&name, &failing] {
        let deleted = call(&agent, "workload_delete", json!({"kind": "Job", "name": n})).await;
        assert_eq!(deleted["status"], "ok", "{deleted:#}");
    }
}

#[tokio::test]
#[ignore = "needs a cluster with Kueue; see the module docs"]
async fn a_deployment_task_completes_once_ready_and_stays_complete_after_scale_down() {
    let env = env();
    let url = serve(&env, &unique("e2e-task-deploy")).await;
    let agent = connect_with_tasks(&url, SUBMITTER).await;
    let name = unique("ck-e2e-task-deploy");

    let id = submit_task(&agent, &env.queue, &serving("Deployment", &name, 2)).await;
    let done = poll_task(&agent, &id, Duration::from_secs(240)).await;
    let (is_error, body) = completed_body(&done);
    assert!(!is_error);
    assert_eq!(
        body["message"], "2/2 replicas admitted and ready",
        "{body:#}"
    );

    let stopped = call(
        &agent,
        "workload_stop",
        json!({"kind": "Deployment", "name": name}),
    )
    .await;
    assert_eq!(stopped["status"], "ok", "{stopped:#}");
    let after = poll_task(&agent, &id, Duration::from_secs(5)).await;
    assert_eq!(after.status(), TaskStatus::Completed);

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
    let url = serve(&env, &unique("e2e-task-cancel")).await;
    let agent = connect_with_tasks(&url, SUBMITTER).await;
    let name = unique("ck-e2e-task-cancel");

    let id = submit_task(&agent, &env.queue, &job(&name, "sleep 600")).await;
    wait_for_phase(&agent, &name, &["admitted"], Duration::from_secs(180)).await;

    let viewer = connect_with_tasks(&url, VIEWER).await;
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

    let other_run = connect_with_tasks(&serve(&env, "someone-else").await, SUBMITTER).await;
    assert!(
        other_run
            .peer()
            .cancel_task(CancelTaskParams::new(id.clone()))
            .await
            .is_err(),
        "another run cannot cancel this run's task"
    );

    let deleted = call(
        &agent,
        "workload_delete",
        json!({"kind": "Job", "name": name}),
    )
    .await;
    assert_eq!(deleted["status"], "ok", "{deleted:#}");
}
