//! The whole assembled daemon, end to end.
//!
//! One real [`crucible_controller::daemon::run`] over the production
//! [`crucible_controller::daemon::assemble`] wiring, against:
//! - a **wiremock GitHub** backed by one `Arc<Mutex<World>>` in-memory model (issues, PRs,
//!   reviews, comments): the in-process approval polls read it over real
//!   HTTP, and the `gh`/`git` *subprocess* edges are PATH-shimmed scripts that curl the same
//!   World, so every side effect lands in one asserted place;
//! - a **wiremock OpenAI** chat-completions ranker;
//! - a **command-backend scope agent** (a scripted `CRUCIBLE_BIN`, the `scope.rs` pattern);
//! - a **fake `PodDispatcher`** (the WorkPod primitive's cluster boundary) whose `create` "runs" a
//!   scripted loop run: writes the synthetic session log where the completion edge expects it and
//!   flips its pod to completed through the injectable completion stream.
//!
//! Everything of OURS is real — queue, worker, reconcile, approvals, ingest, ledger, event log; only
//! the external world (GitHub, the ranker, the cluster) is faked over real boundaries.

#![allow(clippy::disallowed_macros)]

use anyhow::Result;
#[cfg(feature = "autoresearch")]
use crucible_controller::daemon::autopilot;
use crucible_controller::daemon::{self, CompletionStream, DaemonConfig};
use crucible_controller::{
    ControllerCfg, Db, IssueKey, OverrideStore, PodDispatcher, QueueConfig, Status, WorkQueue,
};
use k8s_openapi::api::core::v1::Pod;
#[cfg(feature = "autoresearch")]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[cfg(feature = "autoresearch")]
use wiremock::Request;
#[cfg(feature = "autoresearch")]
use wiremock::matchers::path_regex;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Serializes the tests in this binary: they mutate process-global env vars and the process-wide
/// launcher seam. (The crate's own ENV_LOCK is `cfg(test)`-internal; integration tests need their
/// own. Cargo runs test *binaries* sequentially, so cross-binary races don't exist.)
static E2E_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// --- the in-memory GitHub World -----------------------------------------------------------------

#[cfg(feature = "autoresearch")]
#[derive(Debug, Clone)]
struct WorldPr {
    number: u64,
    head: String,
    base: String,
    /// `[{state, user.login, author_association}]`, served verbatim on `/pulls/{n}/reviews`.
    reviews: Vec<serde_json::Value>,
    /// Served verbatim on `/issues/{n}/comments`.
    comments: Vec<serde_json::Value>,
}

/// The one in-memory model every fake edge reads and writes: the upstream issues the pollers see,
/// the PRs the (shimmed) `gh` opens, the branch pushes the (shimmed) `git` records, and any `gh`
/// invocation the shim didn't recognize — the leak-check's "nothing unasserted happened" set.
#[cfg(feature = "autoresearch")]
#[derive(Debug, Default)]
struct World {
    issues: Vec<serde_json::Value>,
    prs: Vec<WorldPr>,
    pushes: Vec<String>,
    unknown_gh: Vec<String>,
    next_pr_number: u64,
}

#[cfg(feature = "autoresearch")]
impl World {
    fn pr_url(number: u64) -> String {
        format!("https://github.com/testorg/widget/pull/{number}")
    }
}

#[cfg(feature = "autoresearch")]
fn upstream_issue(number: u64, title: &str, body: &str, labels: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "number": number,
        "title": title,
        "body": body,
        "labels": labels.iter().map(|l| serde_json::json!({"name": l})).collect::<Vec<_>>(),
        "html_url": format!("https://github.com/testorg/widget/issues/{number}"),
        "updated_at": "2026-07-02T00:00:00Z",
        "state": "open",
    })
}

/// Mount every World-backed route on `server`. The GitHub REST reads serve the model; the
/// `/fake-gh/*` + `/fake-git/*` routes are the shimmed subprocesses' way into the same model.
#[cfg(feature = "autoresearch")]
async fn mount_world(server: &MockServer, world: Arc<Mutex<World>>) {
    // Upstream issue list (triage's watermark poll). Matched on the path regardless of query.
    let w = world.clone();
    Mock::given(method("GET"))
        .and(path("/repos/testorg/widget/issues"))
        .respond_with(move |_req: &Request| {
            let w = w.lock().expect("world");
            ResponseTemplate::new(200).set_body_json(w.issues.clone())
        })
        .mount(server)
        .await;

    // Single upstream issue (confirm_tier's fetch, the staleness/close approvals).
    let w = world.clone();
    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/testorg/widget/issues/\d+$"))
        .respond_with(move |req: &Request| {
            let n: u64 = req
                .url
                .path_segments()
                .and_then(|mut s| s.next_back())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let w = w.lock().expect("world");
            match w.issues.iter().find(|i| i["number"].as_u64() == Some(n)) {
                Some(issue) => ResponseTemplate::new(200).set_body_json(issue.clone()),
                None => ResponseTemplate::new(404),
            }
        })
        .mount(server)
        .await;

    // PR reviews + conversation comments (the approval poll reads these).
    let w = world.clone();
    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/testorg/widget/pulls/\d+/reviews$"))
        .respond_with(move |req: &Request| {
            // /repos/testorg/widget/pulls/{n}/reviews — the number is the 5th segment.
            let n = nth_path_number(req, 4);
            let w = w.lock().expect("world");
            let reviews = w
                .prs
                .iter()
                .find(|p| p.number == n)
                .map(|p| p.reviews.clone())
                .unwrap_or_default();
            ResponseTemplate::new(200).set_body_json(reviews)
        })
        .mount(server)
        .await;
    let w = world.clone();
    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/testorg/widget/issues/\d+/comments$"))
        .respond_with(move |req: &Request| {
            // /repos/testorg/widget/issues/{n}/comments — the number is the 5th segment.
            let n = nth_path_number(req, 4);
            let w = w.lock().expect("world");
            let comments = w
                .prs
                .iter()
                .find(|p| p.number == n)
                .map(|p| p.comments.clone())
                .unwrap_or_default();
            ResponseTemplate::new(200).set_body_json(comments)
        })
        .mount(server)
        .await;

    // The OpenAI-compatible ranker (frozen shape): confirms T1.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant",
                "content": r#"{"tier":"T1","affinity":"perf","rationale":"confirmed by e2e ranker","cost_usd":0.0}"#}}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20}
        })))
        .mount(server)
        .await;

    // The `gh` shim's routes: pr list (idempotency probe) + pr create.
    let w = world.clone();
    Mock::given(method("GET"))
        .and(path("/fake-gh/pr-list"))
        .respond_with(move |req: &Request| {
            let head = query_param(req, "head").unwrap_or_default();
            let w = w.lock().expect("world");
            let url = w
                .prs
                .iter()
                .find(|p| p.head == head)
                .map(|p| World::pr_url(p.number))
                .unwrap_or_default();
            ResponseTemplate::new(200).set_body_string(url)
        })
        .mount(server)
        .await;
    let w = world.clone();
    Mock::given(method("POST"))
        .and(path("/fake-gh/pr-create"))
        .respond_with(move |req: &Request| {
            let head = query_param(req, "head").unwrap_or_default();
            let base = query_param(req, "base").unwrap_or_default();
            let mut w = w.lock().expect("world");
            let number = w.next_pr_number;
            w.next_pr_number += 1;
            w.prs.push(WorldPr {
                number,
                head,
                base,
                reviews: Vec::new(),
                comments: Vec::new(),
            });
            ResponseTemplate::new(200).set_body_string(World::pr_url(number))
        })
        .mount(server)
        .await;

    // The `git` shim's push recorder and the `gh` shim's unknown-invocation trap.
    let w = world.clone();
    Mock::given(method("POST"))
        .and(path("/fake-git/push"))
        .respond_with(move |req: &Request| {
            let refspec = String::from_utf8_lossy(&req.body).to_string();
            w.lock().expect("world").pushes.push(refspec);
            ResponseTemplate::new(200)
        })
        .mount(server)
        .await;
    let w = world.clone();
    Mock::given(method("POST"))
        .and(path("/fake-gh/unknown"))
        .respond_with(move |req: &Request| {
            let cmd = query_param(req, "cmd").unwrap_or_default();
            w.lock().expect("world").unknown_gh.push(cmd);
            ResponseTemplate::new(200)
        })
        .mount(server)
        .await;
}

#[cfg(feature = "autoresearch")]
fn nth_path_number(req: &Request, idx: usize) -> u64 {
    req.url
        .path_segments()
        .and_then(|mut s| s.nth(idx))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[cfg(feature = "autoresearch")]
fn query_param(req: &Request, name: &str) -> Option<String> {
    req.url
        .query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

// --- the shimmed subprocess edges ----------------------------------------------------------------

#[cfg(feature = "autoresearch")]
fn write_exec(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write script");
    let mut p = std::fs::metadata(path).expect("metadata").permissions();
    p.set_mode(0o755);
    std::fs::set_permissions(path, p).expect("chmod");
}

/// The command-backend scope agent: `crucible scope --propose` materializes the pack dir (SCOPE.md
/// + crucible.toml, what the approval pushes and the PR body embeds) and prints a surviving report.
#[cfg(feature = "autoresearch")]
fn write_fake_crucible(dir: &Path, argfile: &Path) -> PathBuf {
    let report = r#"{"stages":[{"name":"ingest","passed":true,"detail":"goal"},{"name":"propose","passed":true,"detail":"drafted"},{"name":"validate","passed":true,"detail":"crucible check: OK"},{"name":"freeze","passed":true,"detail":"wrote SCOPE.md"}],"digest":"v1:e2e-digest","cost":0.42}"#;
    let bin = dir.join("crucible");
    write_exec(
        &bin,
        &format!(
            r#"#!/bin/sh
if [ "$1" = scope ]; then
  echo "$@" >> '{argfile}'
  out=""; prev=""
  for a in "$@"; do
    if [ "$prev" = "--out" ]; then out="$a"; fi
    prev="$a"
  done
  mkdir -p "$out"
  printf 'identity: v1:e2e-digest\ncheck: PASS\n' > "$out/SCOPE.md"
  printf '%s' '{manifest}' > "$out/crucible.toml"
  printf '%s' '{report}'
  exit 0
fi
exit 0
"#,
            argfile = argfile.display(),
            manifest = LOOP_PACK_MANIFEST,
        ),
    );
    bin
}

/// The `gh` shim: `pr list`/`pr create` curl the World; anything else is recorded as an unknown
/// invocation (the leak check asserts none happened) and fails loudly.
#[cfg(feature = "autoresearch")]
fn write_fake_gh(dir: &Path, base: &str) {
    write_exec(
        &dir.join("gh"),
        &format!(
            r#"#!/bin/sh
if [ "$1" = pr ] && [ "$2" = list ]; then
  # gh pr list --repo R --head H --state open --json url --jq …
  exec curl -fsS "{base}/fake-gh/pr-list?head=$6"
fi
if [ "$1" = pr ] && [ "$2" = create ]; then
  # gh pr create --repo R --draft --head H --base B --title T --body BODY
  exec curl -fsS -X POST "{base}/fake-gh/pr-create?head=$7&base=$9"
fi
curl -fsS -X POST "{base}/fake-gh/unknown?cmd=$1-$2" >/dev/null 2>&1
exit 1
"#
        ),
    );
}

/// The `git` shim: intercepts `push` (records the refspec in the World, no network) and delegates
/// every local operation (init/config/checkout/commit/rev-parse) to the real git.
#[cfg(feature = "autoresearch")]
fn write_fake_git(dir: &Path, base: &str, real_git: &str) {
    write_exec(
        &dir.join("git"),
        &format!(
            r#"#!/bin/sh
for a in "$@"; do
  if [ "$a" = push ]; then
    for last in "$@"; do :; done
    curl -fsS -X POST "{base}/fake-git/push" --data-binary "$last" >/dev/null
    exit 0
  fi
done
exec {real_git} "$@"
"#
        ),
    );
}

#[cfg(feature = "autoresearch")]
fn real_git_path() -> String {
    let out = std::process::Command::new("which")
        .arg("git")
        .output()
        .expect("which git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// --- the fake pod dispatcher + injectable completion stream --------------------------------------

/// The scripted loop run, on the WorkPod primitive's cluster boundary: `create` receives the
/// controller-stamped loop pod, reads its run-id + issue-key annotations, stores the synthetic
/// session log where the completion edge looks for it (the run-session artifact), and (when
/// `deliver`) flips the pod to completed by pushing the key through the injected completion
/// stream — the same edge the kube pod watch feeds in production. `delete` (the GC) is a recorded
/// no-op; `await_terminal`/`logs` are unreachable in these tests (grounded ranking runs `local`,
/// never a WorkPod turn).
struct FakePodDispatcher {
    pool: sqlx::PgPool,
    deliver: bool,
    tx: tokio::sync::mpsc::UnboundedSender<IssueKey>,
    /// The session log every created pod "produces".
    session: &'static str,
    launches: Mutex<Vec<String>>,
    /// The issue key of every pod created, in order.
    keys: Mutex<Vec<String>>,
    deleted: Mutex<Vec<String>>,
}

#[cfg(feature = "autoresearch")]
const SESSION_LOG: &str = r#"{"v":1,"kind":"identity","identity":{"digest":"v1:e2e-digest"}}
{"v":1,"kind":"row","row":{"iter":0,"decision":"baseline","score":200.0}}
{"v":1,"kind":"row","row":{"iter":1,"decision":"keep","score":260.0}}
{"v":1,"kind":"budget","spent":1.1,"elapsed_secs":120}
{"v":1,"kind":"summary","rows":[],"gate":"bench","best_score":260.0}
{"v":1,"kind":"shutdown","outcome":"finished","reason":"all iterations completed"}
"#;

fn pod_annotation<'a>(pod: &'a Pod, key: &str) -> Option<&'a str> {
    pod.metadata
        .annotations
        .as_ref()?
        .get(key)
        .map(String::as_str)
}

#[async_trait::async_trait]
impl PodDispatcher for FakePodDispatcher {
    async fn create(&self, _cluster: &str, _namespace: &str, mut pod: Pod) -> Result<Pod> {
        let run_id = pod_annotation(&pod, crucible_controller::daemon::RUN_ID_ANNOTATION)
            .expect("stamped run-id annotation")
            .to_string();
        let issue_key = pod_annotation(&pod, crucible_controller::daemon::ISSUE_KEY_ANNOTATION)
            .expect("stamped issue-key annotation")
            .to_string();
        crucible_controller::runs::blob_store::put_run_session(
            &self.pool,
            &run_id,
            self.session.as_bytes(),
        )
        .await?;
        self.launches.lock().expect("launches").push(run_id);
        self.keys.lock().expect("keys").push(issue_key.clone());
        if self.deliver {
            let _ = self.tx.send(IssueKey(issue_key));
        }
        // The API server would populate the UID; the pack ConfigMap owner-refs the created pod.
        pod.metadata.uid = Some("e2e-pod-uid".to_string());
        Ok(pod)
    }

    async fn await_terminal(
        &self,
        _cluster: &str,
        _namespace: &str,
        _name: &str,
        _timeout: Duration,
    ) -> Result<crucible_controller::runs::workpod::TerminalState> {
        anyhow::bail!("await_terminal is not exercised on the run path (out-of-band completion)")
    }

    async fn logs(&self, _cluster: &str, _namespace: &str, _name: &str) -> Result<String> {
        anyhow::bail!("logs is not exercised on the run path")
    }

    async fn delete(&self, _cluster: &str, _namespace: &str, name: &str) -> Result<()> {
        self.deleted.lock().expect("deleted").push(name.to_string());
        Ok(())
    }
}

/// A channel-backed [`CompletionStream`] (the harness's stand-in for the kube pod watch).
fn channel_completions() -> (
    tokio::sync::mpsc::UnboundedSender<IssueKey>,
    CompletionStream,
) {
    use futures_util::StreamExt;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<IssueKey>();
    let stream =
        futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|k| (k, rx)) })
            // Never terminate: the daemon `select!`s this stream in a loop, and a closed channel (the
            // sender dropped during test unwinding) must park, not end the stream mid-poll.
            .chain(futures_util::stream::pending());
    (tx, Box::pin(stream))
}

// --- harness plumbing -----------------------------------------------------------------------------

#[cfg(feature = "autoresearch")]
const KEY: &str = "testorg/widget#1";

/// A unique ledger URL on the test server, so parallel e2e tests never share a database. The
/// database is created by `Db::open` and left behind (the test server is throwaway).
fn test_ledger_url() -> String {
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must point at the test server");
    crucible_controller::sibling_db_url(
        &base,
        &format!("crucible_e2e_{}", uuid::Uuid::new_v4().simple()),
    )
    .expect("deriving a test ledger URL")
}

/// The smallest single-domain loop manifest the loop-run render accepts: what the scope fake
/// freezes and what a seeded approval stores.
#[cfg(feature = "autoresearch")]
const LOOP_PACK_MANIFEST: &str = concat!(
    "[repo]\n",
    "url = \"https://github.com/testorg/widget.git\"\n",
    "\n",
    "[agent]\n",
    "backend = \"openshell\"\n",
    "goal = \"make it faster\"\n",
    "sandbox_image = \"ghcr.io/org/sandbox:latest\"\n",
    "\n",
    "[judge]\n",
    "measure_cmd = \"bench\"\n",
    "direction = \"higher\"\n",
    "objective = \"score\"\n",
    "\n",
    "[deploy]\n",
);

/// The smallest deploy profile the loop-run render accepts, written where `test_cfg` points.
const DEPLOY_PROFILE: &str = concat!(
    "[cluster]\n",
    "loop_namespace = \"autoresearch\"\n",
    "rig_namespace = \"rig\"\n",
    "service_account = \"autoresearch-loop\"\n",
    "supervisor_image = \"registry.example.com/openshell-supervisor:latest\"\n",
    "\n",
    "[image]\n",
    "loop = \"ghcr.io/neuralmagic/crucible:latest\"\n",
    "pull_secret = \"example-pull\"\n",
);

fn test_cfg(state_dir: &Path, repos: Vec<String>) -> ControllerCfg {
    std::fs::create_dir_all(state_dir).expect("state dir");
    std::fs::write(state_dir.join("deploy-profile.toml"), DEPLOY_PROFILE).expect("deploy profile");
    ControllerCfg {
        secret_provider: None,
        schedule_owner_ttl_secs: 604800,
        local_secret_allowlist: Vec::new(),
        state_dir: state_dir.to_path_buf(),
        scratch_dir: None,
        db: test_ledger_url(),
        repos,
        allowed_orgs: vec![],
        pod_namespace: "default".to_string(),
        turn_service_account: None,
        dispatch_cluster: "hub".to_string(),
        cluster_policy: Vec::new(),
        dispatch_cluster_by_contract: Vec::new(),
        clusters_dir: None,
        spoke_service_accounts: Vec::new(),
        control_port: 7777,
        playbook_executor: crucible_controller::PlaybookExecutor::Pod,
        playbook_max_cost_cap: 25.0,
        playbook_max_time_cap: crucible_controller::model::MaxTime::hours(4),
        schedule_auto_disable_failures: 5,
        allow_t3: false,
        allowed_tiers: vec![crucible_contract::Tier::T0, crucible_contract::Tier::T1],
        prescope_grounded: false,
        rank_horizon_days: 0,
        grounded_executor: crucible_controller::GroundedExecutor::Local,
        deploy_profile: Some(state_dir.join("deploy-profile.toml")),
        render_no_pin: true,
        grounded_sandbox_image: None,
        admins: vec![],
        operators: vec![],
        operator_groups: vec![],
        session_secure_cookies: true,
        #[cfg(feature = "autoresearch")]
        autopilot: None,
        autoresearch: cfg!(feature = "autoresearch"),
        overrides_configmap: "crucible-controller-overrides".to_string(),
        overrides_namespace: None,
        overrides: None,
        github_app: None,
        scope_executor: crucible_controller::ScopeExecutor::Local,
        scope_sandbox_image: None,
        pr_repo_map: Vec::new(),
        build_backends: false,
        registry_authfile: None,
        image_catalog_repos: Vec::new(),
        image_catalog_interval_secs: 600,
        build_push_authfile: None,
        build_github_token: None,
        build_context_git_url: None,
        build_context_git_ref: "main".to_string(),
        build_context_git_token_file: None,
        jira_base_url: None,
        jira_email: None,
        jira_api_token: None,
        public_url: None,
        jira_emission_project: None,
        jira_emission_epic_type_id: None,
        jira_emission_task_type_id: None,
        emission_labels: vec![],
        broker_contracts: Default::default(),
        profile: crucible_controller::Profile::default(),
    }
}

fn daemon_cfg() -> DaemonConfig {
    DaemonConfig {
        // Millisecond-scale backoff so a transient failure retries inside the test budget.
        queue: QueueConfig {
            base_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_millis(200),
            park_after: 5,
        },
        // The first discovery tick fires after the ~1s stagger floor, then every 100ms.
        discovery_interval: Duration::from_millis(100),
        // The tests use a fixed cadence — no runtime override store threaded here.
        discovery_interval_fn: None,
        // The drift check must never fire mid-test (its first fire is one full interval out).
        verify_interval: Duration::from_secs(3600),
        // No API surface in the harness — the manual reconcile trigger never fires.
        reconcile_now: None,
    }
}

/// Await a DB-observable state instead of sleeping blind: poll until the issue reaches `want`.
/// The deadline is generous (a 2-core CI runner is slow, not wrong) and the failure is loud: the
/// issue's current state plus its scope row, so a wedged chain is diagnosable from the panic.
async fn wait_for_status(db: &Db, key: &str, want: Status) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(issue) = crucible_controller::issues::store::get_issue(db.pool(), key)
            .await
            .expect("get_issue")
            && issue.status == want
        {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            let issue = crucible_controller::issues::store::get_issue(db.pool(), key)
                .await
                .expect("get_issue");
            let scope = crucible_controller::issues::store::latest_scope_for_issue(db.pool(), key)
                .await
                .expect("latest_scope");
            panic!(
                "timed out waiting for {key} to reach {want:?}\n  issue: {issue:?}\n  scope: {scope:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Join the daemon task under a hard deadline: a lost shutdown must fail this test loudly, never
/// wedge the whole suite (and the merge queue behind it) until an external job timeout.
async fn join_daemon(handle: tokio::task::JoinHandle<Result<()>>) {
    tokio::time::timeout(Duration::from_secs(60), handle)
        .await
        .expect("daemon did not shut down within 60s of the shutdown signal")
        .expect("daemon task")
        .expect("daemon run");
}

async fn event_ndjson(db: &Db) -> Result<String> {
    let mut buf = Vec::new();
    crucible_controller::event_log::export_ndjson(db.pool(), &mut buf).await?;
    Ok(String::from_utf8(buf)?)
}

#[cfg(feature = "autoresearch")]
async fn event_trace(db: &Db) -> Vec<(String, String)> {
    let body = event_ndjson(db).await.unwrap_or_default();
    body.lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).expect("event line");
            (
                v["from"].as_str().expect("from").to_string(),
                v["to"].as_str().expect("to").to_string(),
            )
        })
        .collect()
}

/// Set an env var for the test's duration; restores (or removes) on drop even on panic.
#[cfg(feature = "autoresearch")]
struct EnvGuard {
    name: &'static str,
    prior: Option<std::ffi::OsString>,
}

#[cfg(feature = "autoresearch")]
impl EnvGuard {
    fn set(name: &'static str, value: &str) -> Self {
        let prior = std::env::var_os(name);
        unsafe {
            std::env::set_var(name, value);
        }
        EnvGuard { name, prior }
    }

    fn unset(name: &'static str) -> Self {
        let prior = std::env::var_os(name);
        unsafe {
            std::env::remove_var(name);
        }
        EnvGuard { name, prior }
    }
}

#[cfg(feature = "autoresearch")]
#[cfg(feature = "autoresearch")]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prior {
            Some(v) => unsafe { std::env::set_var(self.name, v) },
            None => unsafe { std::env::remove_var(self.name) },
        }
    }
}

/// Uninstall the process-wide pod dispatcher on drop (even on panic), so a failed test can't poison
/// the next one through the global seam.
struct DispatcherGuard;

impl Drop for DispatcherGuard {
    fn drop(&mut self) {
        crucible_controller::reset_dispatcher();
    }
}

// --- the tests -------------------------------------------------------------------------------------

/// The full transition chain through ONE running assembled daemon:
/// triage discovers → ranker confirms → scope survives → approval PR opens → (the World grants
/// approval) → launch fires → completion ingested → per-candidate rows + ledger + event log all
/// correct → done. Then `--once` idempotence over the settled state, then the leak check.
#[cfg(feature = "autoresearch")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_chain_new_to_done_with_once_idempotence() -> Result<()> {
    let _g = E2E_LOCK.lock().await;
    let root = tempfile::tempdir()?;
    let state_dir = root.path().join("state");
    let fakebin = root.path().join("fakebin");
    std::fs::create_dir_all(&fakebin)?;

    // The World: one open, perf-shaped upstream issue for triage to discover.
    let world = Arc::new(Mutex::new(World {
        issues: vec![upstream_issue(
            1,
            "p99 latency doubled under prefix caching",
            "throughput drops 40% with the cache enabled; benchmark attached",
            &["performance"],
        )],
        next_pr_number: 101,
        ..World::default()
    }));
    let server = MockServer::start().await;
    mount_world(&server, world.clone()).await;

    // The subprocess shims + the command-backend scope agent.
    let argfile = root.path().join("scope-args.txt");
    let crucible_bin = write_fake_crucible(&fakebin, &argfile);
    write_fake_gh(&fakebin, &server.uri());
    write_fake_git(&fakebin, &server.uri(), &real_git_path());

    let old_path = std::env::var("PATH").unwrap_or_default();
    let _env = [
        EnvGuard::set("PATH", &format!("{}:{old_path}", fakebin.display())),
        EnvGuard::set("CRUCIBLE_BIN", &crucible_bin.to_string_lossy()),
        EnvGuard::set("GITHUB_API_URL", &server.uri()),
        EnvGuard::set("CONTROLLER_RANKER_API_URL", &server.uri()),
        EnvGuard::set("CONTROLLER_PACK_REPO", "testorg/widget"),
        EnvGuard::unset("CONTROLLER_APPROVERS"),
    ];

    let cfg = test_cfg(&state_dir, vec!["testorg/widget".to_string()]);
    let db = Db::open(cfg.db_url()).await?;
    // Boot seed (Lane O3): discovery reads the DB's watch-set, not `cfg.repos`, directly.
    crucible_controller::issues::repo_watch::seed_watched_repos(db.pool(), &cfg.repos).await?;

    // The injectable cluster edges: the fake pod dispatcher + the channel-backed completion stream.
    let (tx, completions) = channel_completions();
    let launcher = Arc::new(FakePodDispatcher {
        pool: db.pool().clone(),
        deliver: true,
        tx,
        session: SESSION_LOG,
        launches: Mutex::new(Vec::new()),
        keys: Mutex::new(Vec::new()),
        deleted: Mutex::new(Vec::new()),
    });
    crucible_controller::install_dispatcher(launcher.clone());
    let _launcher_guard = DispatcherGuard;

    // The assembled daemon — the same `assemble` the binary calls.
    let queue = WorkQueue::new();
    let overrides = Arc::new(OverrideStore::new());
    let wiring = daemon::assemble(
        &db,
        &cfg,
        queue,
        overrides,
        completions,
        crucible_controller::authz::policy::ActivePolicy::default_set()
            .expect("the shipped default policy set loads"),
    );
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let handle = {
        let shutdown = shutdown.clone();
        tokio::spawn(daemon::run(
            Vec::new(),
            daemon_cfg(),
            wiring,
            shutdown,
            None,
        ))
    };

    // discovery → rank → scope → the approval gate opens its draft PR.
    wait_for_status(&db, KEY, Status::AwaitingApproval).await;
    {
        let scope = crucible_controller::issues::store::latest_scope_for_issue(db.pool(), KEY)
            .await?
            .expect("scope row");
        assert_eq!(scope.pack_digest.as_deref(), Some("v1:e2e-digest"));
        assert_eq!(
            scope.approval_pr.as_deref(),
            Some("https://github.com/testorg/widget/pull/101")
        );
        assert!(!scope.is_approved(), "the approval is still shut");
        let w = world.lock().expect("world");
        assert_eq!(w.prs.len(), 1, "exactly one approval PR opened");
        assert_eq!(w.prs[0].head, "crucible-pack/testorg_widget_1");
        assert_eq!(w.prs[0].base, "crucible-pack/testorg_widget_1-base");
    }

    // The human at the approval: the World grants an authorized review approval.
    world.lock().expect("world").prs[0]
        .reviews
        .push(serde_json::json!({
            "state": "APPROVED",
            "user": {"login": "maint"},
            "author_association": "MEMBER"
        }));

    // approval poll → launch → the scripted run completes → completion ingested.
    wait_for_status(&db, KEY, Status::Done).await;

    // Quiesce: one more discovery tick over the settled state must be a no-op, then shut down and
    // join (the leak-check finish — the worker drains and exits).
    let settled_events = event_trace(&db).await.len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    shutdown.notify_waiters();
    join_daemon(handle).await;

    // The full transition chain, in order, from the event log.
    eprintln!("--- events ---\n{}", event_ndjson(&db).await?);
    let trace = event_trace(&db).await;
    assert_eq!(
        trace,
        vec![
            ("new".to_string(), "new".to_string()), // ranker confirmed the tier
            ("new".to_string(), "scoped".to_string()),
            ("scoped".to_string(), "awaiting-approval".to_string()),
            (
                "awaiting-approval".to_string(),
                "awaiting-approval".to_string()
            ), // approval stamped
            ("awaiting-approval".to_string(), "running".to_string()),
            ("running".to_string(), "done".to_string()),
        ],
        "the exact outer-loop chain, nothing more"
    );
    assert_eq!(
        trace.len(),
        settled_events,
        "the settled state produced no further events"
    );

    // Provenance: the run row + per-candidate rows + the ledger.
    let issue = crucible_controller::issues::store::get_issue(db.pool(), KEY)
        .await?
        .expect("issue");
    assert_eq!(issue.status, Status::Done);
    assert_eq!(issue.tier.as_deref(), Some("T1"));
    let scope = crucible_controller::issues::store::latest_scope_for_issue(db.pool(), KEY)
        .await?
        .expect("scope");
    assert_eq!(scope.approved_by.as_deref(), Some("maint"));
    let runs = crucible_controller::runs::store::list_runs_for_scope(db.pool(), scope.id).await?;
    assert_eq!(runs.len(), 1, "one launch, one run row");
    let run = &runs[0];
    assert_eq!(run.status, "finished");
    assert_eq!(run.best_score, Some(260.0));
    assert_eq!(run.cost_usd, Some(1.1));
    assert_eq!(run.identity_digest.as_deref(), Some("v1:e2e-digest"));
    let candidates =
        crucible_controller::runs::store::list_candidates_for_run(db.pool(), &run.run_id).await?;
    assert_eq!(candidates.len(), 2, "baseline + keep, per-candidate rows");
    assert_eq!(candidates[1].decision.as_deref(), Some("keep"));
    assert_eq!(candidates[1].score, Some(260.0));
    let today = jiff::Timestamp::now().strftime("%Y-%m-%d").to_string();
    let total = crucible_controller::ledger::ledger_day_total(db.pool(), &today).await?;
    assert!(
        (total - 1.52).abs() < 1e-9,
        "rank $0.00 + scope $0.42 + run $1.10 = $1.52, got {total}"
    );

    // `--once` idempotence: a second pass over the settled state is a no-op — no keys drained
    // (done is terminal), no new events, no new spend, no second rank call.
    let rank_calls_before = rank_calls(&server).await;
    let drained = autopilot::run_once_with(&db, &cfg).await?;
    assert_eq!(drained, 0, "nothing non-terminal remains");
    assert_eq!(event_trace(&db).await.len(), trace.len(), "no new events");
    let total_after = crucible_controller::ledger::ledger_day_total(db.pool(), &today).await?;
    assert!((total_after - total).abs() < 1e-12, "no new spend");
    assert_eq!(rank_calls(&server).await, rank_calls_before);

    // The leak check: nothing the test didn't expect happened in the World.
    {
        let w = world.lock().expect("world");
        assert_eq!(w.prs.len(), 1, "no extra PRs");
        assert_eq!(w.prs[0].comments.len(), 0, "no comments posted");
        assert_eq!(
            w.pushes.len(),
            2,
            "exactly the pack head + pristine base pushes: {:?}",
            w.pushes
        );
        assert!(
            w.pushes
                .iter()
                .any(|p| p.ends_with("refs/heads/crucible-pack/testorg_widget_1")),
            "head branch pushed"
        );
        assert!(
            w.pushes
                .iter()
                .any(|p| p.ends_with("refs/heads/crucible-pack/testorg_widget_1-base")),
            "pristine base pushed"
        );
        assert!(
            w.unknown_gh.is_empty(),
            "no unshimmed gh call: {:?}",
            w.unknown_gh
        );
    }
    assert_eq!(
        rank_calls(&server).await,
        1,
        "one bounded rank call, then cache hits"
    );
    {
        let launches = launcher.launches.lock().expect("launches");
        assert_eq!(launches.len(), 1, "one launch, no double-spend");
        // The succeeded run pod was GC'd once its session was ingested (the WorkPod GC policy).
        let deleted = launcher.deleted.lock().expect("deleted");
        assert_eq!(
            deleted.len(),
            1,
            "the finished run pod was collected + deleted"
        );
    }
    // The run flowed through the WorkPod primitive: its `work_pods` row reached `collected`.
    {
        let work_pods = crucible_controller::runs::work_pods::work_pods_in_states(
            db.pool(),
            &[crucible_controller::WorkPodState::Collected],
        )
        .await?;
        assert_eq!(
            work_pods.len(),
            1,
            "one run tracked + collected as a work pod"
        );
        assert_eq!(work_pods[0].kind, "run");
        assert_eq!(work_pods[0].issue_key.as_deref(), Some(KEY));
    }
    let scope_invocations = std::fs::read_to_string(&argfile)?;
    assert_eq!(scope_invocations.lines().count(), 1, "one scope turn");
    assert!(scope_invocations.contains("--propose"));
    assert!(scope_invocations.contains(&format!("--issue {KEY}")));

    Ok(())
}

/// The crash-recovery case: the daemon dies after launch, before the completion is delivered.
/// A fresh daemon over the same DB re-enqueues the still-`running` row at startup and reconciles
/// the missed completion from evidence (the stored session artifact) — and nothing double-spends.
#[cfg(feature = "autoresearch")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_recovery_reconciles_a_missed_completion() -> Result<()> {
    let _g = E2E_LOCK.lock().await;
    let root = tempfile::tempdir()?;
    let state_dir = root.path().join("state");
    let fakebin = root.path().join("fakebin");
    std::fs::create_dir_all(&fakebin)?;
    let argfile = root.path().join("scope-args.txt");
    let crucible_bin = write_fake_crucible(&fakebin, &argfile);
    // No GitHub, no ranker, no pack repo: the approvals are all clean no-ops on this path.
    let _env = [
        EnvGuard::set("CRUCIBLE_BIN", &crucible_bin.to_string_lossy()),
        EnvGuard::unset("GITHUB_API_URL"),
        EnvGuard::unset("GH_TOKEN"),
        EnvGuard::unset("GITHUB_TOKEN"),
        EnvGuard::unset("AUTORESEARCH_PR_TOKEN"),
        EnvGuard::unset("CONTROLLER_PACK_REPO"),
    ];

    let cfg = test_cfg(&state_dir, Vec::new());
    let db = Db::open(cfg.db_url()).await?;

    // Seed an approved, awaiting-approval issue (the state right before launch).
    crucible_controller::issues::store::upsert_issue(
        db.pool(),
        &crucible_controller::NewIssue {
            key: KEY.to_string(),
            repo: "testorg/widget".to_string(),
            priority: 0,
            evidence_url: None,
            title: None,
            author: None,
            body: None,
            labels: Vec::new(),
            upstream_updated_at: None,
        },
    )
    .await?;
    assert!(
        crucible_controller::issues::store::claim_issue(
            db.pool(),
            KEY,
            Status::New,
            Status::AwaitingApproval
        )
        .await?
    );
    let scope_id = crucible_controller::issues::store::insert_scope(
        db.pool(),
        &crucible_controller::NewScope {
            issue: KEY.to_string(),
            pack_digest: Some("v1:e2e-digest".to_string()),
            check_outcome: Some("PASS".to_string()),
        },
    )
    .await?;
    sqlx::query(
        "UPDATE scopes SET approved_by='maint', approved_at='2026-07-02T00:00:00Z' WHERE id=$1",
    )
    .bind(scope_id)
    .execute(db.pool())
    .await?;
    // The approved pack the launch renders: the same loop manifest the scope fake writes.
    let pack = root.path().join("approved-pack");
    std::fs::create_dir_all(&pack)?;
    std::fs::write(pack.join("crucible.toml"), LOOP_PACK_MANIFEST)?;
    crucible_controller::playbooks::packs::store_pack_tree(db.pool(), KEY, &pack).await?;

    // Daemon #1: launches the run (the fake writes the session log) but the completion is NEVER
    // delivered — the pod "finishes" while the controller is down.
    let (tx1, completions1) = channel_completions();
    let launcher1 = Arc::new(FakePodDispatcher {
        pool: db.pool().clone(),
        deliver: false,
        tx: tx1,
        session: SESSION_LOG,
        launches: Mutex::new(Vec::new()),
        keys: Mutex::new(Vec::new()),
        deleted: Mutex::new(Vec::new()),
    });
    crucible_controller::install_dispatcher(launcher1.clone());
    let _launcher_guard = DispatcherGuard;

    let keys = crucible_controller::issues::store::non_terminal_keys(db.pool()).await?;
    let shutdown1 = Arc::new(tokio::sync::Notify::new());
    let handle1 = {
        let wiring = daemon::assemble(
            &db,
            &cfg,
            WorkQueue::new(),
            Arc::new(OverrideStore::new()),
            completions1,
            crucible_controller::authz::policy::ActivePolicy::default_set()
                .expect("the shipped default policy set loads"),
        );
        let shutdown = shutdown1.clone();
        tokio::spawn(daemon::run(keys, daemon_cfg(), wiring, shutdown, None))
    };
    wait_for_status(&db, KEY, Status::Running).await;
    assert_eq!(launcher1.launches.lock().expect("launches").len(), 1);

    // Kill the daemon mid-"run": drop it after launch, before completion.
    shutdown1.notify_waiters();
    join_daemon(handle1).await;
    assert_eq!(
        crucible_controller::issues::store::get_issue(db.pool(), KEY)
            .await?
            .expect("issue")
            .status,
        Status::Running,
        "the crash left the row mid-flight"
    );

    // Daemon #2, fresh over the same DB: the startup re-enqueue reconciles the missed completion
    // from evidence. Its launcher must never fire — recovery is ingest, not relaunch.
    let (tx2, completions2) = channel_completions();
    let launcher2 = Arc::new(FakePodDispatcher {
        pool: db.pool().clone(),
        deliver: false,
        tx: tx2,
        session: SESSION_LOG,
        launches: Mutex::new(Vec::new()),
        keys: Mutex::new(Vec::new()),
        deleted: Mutex::new(Vec::new()),
    });
    crucible_controller::install_dispatcher(launcher2.clone());

    let keys = crucible_controller::issues::store::non_terminal_keys(db.pool()).await?;
    assert_eq!(keys, vec![KEY.to_string()], "the running row re-enqueues");
    let shutdown2 = Arc::new(tokio::sync::Notify::new());
    let handle2 = {
        let wiring = daemon::assemble(
            &db,
            &cfg,
            WorkQueue::new(),
            Arc::new(OverrideStore::new()),
            completions2,
            crucible_controller::authz::policy::ActivePolicy::default_set()
                .expect("the shipped default policy set loads"),
        );
        let shutdown = shutdown2.clone();
        tokio::spawn(daemon::run(keys, daemon_cfg(), wiring, shutdown, None))
    };
    wait_for_status(&db, KEY, Status::Done).await;
    shutdown2.notify_waiters();
    join_daemon(handle2).await;

    // Reconciled from evidence, nothing double-spent.
    assert_eq!(
        launcher2.launches.lock().expect("launches").len(),
        0,
        "recovery must not relaunch"
    );
    let runs = crucible_controller::runs::store::list_runs_for_scope(db.pool(), scope_id).await?;
    assert_eq!(runs.len(), 1, "still exactly one run row");
    assert_eq!(runs[0].status, "finished");
    assert_eq!(runs[0].cost_usd, Some(1.1));
    let today = jiff::Timestamp::now().strftime("%Y-%m-%d").to_string();
    let total = crucible_controller::ledger::ledger_day_total(db.pool(), &today).await?;
    assert!(
        (total - 1.1).abs() < 1e-9,
        "the run cost ledgered exactly once, got {total}"
    );
    let trace = event_trace(&db).await;
    assert_eq!(
        trace,
        vec![
            ("awaiting-approval".to_string(), "running".to_string()),
            ("running".to_string(), "done".to_string()),
        ],
        "launch before the crash, recovery ingest after it"
    );
    Ok(())
}

#[cfg(feature = "autoresearch")]
async fn rank_calls(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .expect("request recording on")
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/chat/completions")
        .count()
}

/// The real kube run-launch path against a live cluster — the established carve-out: compile-tested
/// here, exercised by hand. Renders the pack's loop pod, stamps it controller-owned, and creates it
/// over the [`crucible_controller::KubePodDispatcher`]. Needs a reachable cluster plus
/// `CRUCIBLE_E2E_PACK_DIR` (a frozen pack with a manifest) and `CONTROLLER_DEPLOY_PROFILE` (the
/// deploy profile the render reads).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live cluster + CRUCIBLE_E2E_PACK_DIR + CONTROLLER_DEPLOY_PROFILE"]
async fn kube_dispatcher_creates_a_stamped_run_pod_live() -> Result<()> {
    let _g = E2E_LOCK.lock().await;
    let pack_out = PathBuf::from(
        std::env::var("CRUCIBLE_E2E_PACK_DIR").expect("CRUCIBLE_E2E_PACK_DIR is required"),
    );
    let profile = PathBuf::from(
        std::env::var("CONTROLLER_DEPLOY_PROFILE").expect("CONTROLLER_DEPLOY_PROFILE is required"),
    );
    let namespace =
        std::env::var("CONTROLLER_POD_NAMESPACE").unwrap_or_else(|_| "autoresearch".to_string());
    let run_id = format!("e2e-live-{}", std::process::id());
    let pod_name = crucible_controller::run_pod_name(&run_id);
    let cm_name = format!("{pod_name}-pack");
    let cm_name_for_render = cm_name.clone();
    let (rendered, cm) = tokio::task::spawn_blocking(
        move || -> Result<(Pod, Option<k8s_openapi::api::core::v1::ConfigMap>)> {
            let (mut pod, cm) = crucible_controller::render_run_docs(
                &pack_out,
                &profile,
                &cm_name_for_render,
                &crucible_controller::RunRenderOpts::Loop {
                    iterations: 6,
                    max_cost: 25.0,
                    pr_repo: None,
                    agent: crucible_controller::playbooks::providers::AgentSelection::default(),
                },
                Some(std::sync::Arc::new(crucible::deploy::RegistryDigests)),
            )?;
            crucible_controller::stamp_run_pod(
                &mut pod,
                &pod_name,
                "testorg/widget#1",
                &run_id,
                None,
                None,
            );
            Ok((pod, cm))
        },
    )
    .await??;
    let dispatcher = crucible_controller::KubePodDispatcher::new(std::sync::Arc::new(
        crucible_controller::runs::clusters::ClusterClients::new(None),
    ));
    let hub = crucible_controller::runs::clusters::HUB_CLUSTER;
    dispatcher.create(hub, &namespace, rendered).await?;
    // The pack CM (delivered off the PVC, since the loop image never baked this domain) rides the same
    // create path. The production dispatch owner-refs it to the created pod; this smoke test just
    // proves the render emits it and the kube create accepts it.
    if let Some(cm) = cm {
        dispatcher.create_configmap(hub, &namespace, cm).await?;
    }
    Ok(())
}

// --- the standing launches ------------------------------------------------------------------------

/// A finished playbook run: one task passed, the graph completed.
const PLAYBOOK_SESSION_LOG: &str = r#"{"v":1,"kind":"identity","identity":{"digest":"v1:e2e-digest"}}
{"v":1,"kind":"task_result","task":"hello","status":"pass","output":{},"cost_usd":0.4}
{"v":1,"kind":"budget","spent":0.4,"elapsed_secs":42}
{"v":1,"kind":"shutdown","outcome":"finished","reason":"graph complete"}
"#;

/// A playbook pack that registers and renders: a one-task graph with a required `topic` and an
/// optional `depth`, committed to a local git repo the registry clones.
fn write_playbook_repo(dir: &Path) -> String {
    const MANIFEST: &str = concat!(
        "[repo]\n",
        "path = \".\"\n",
        "\n",
        "[agent]\n",
        "backend = \"openshell\"\n",
        "goal = \"draft the roundup\"\n",
        "sandbox_image = \"registry.example.com/sandbox:latest\"\n",
        "\n",
        "[workflow]\n",
        "type = \"playbook\"\n",
        "file = \"workflow.star\"\n",
    );
    const WORKFLOW: &str = concat!(
        "params = {\n",
        "    \"topic\": {\"type\": \"string\", \"required\": True},\n",
        "    \"depth\": {\"type\": \"string\", \"default\": \"shallow\"},\n",
        "}\n",
        "\n",
        "hello = command(name = \"hello\", run = \"echo hello\")\n",
        "\n",
        "workflow(type = \"playbook\", tasks = [hello], result = hello)\n",
    );
    let repo = dir.join("packrepo");
    std::fs::create_dir_all(&repo).expect("mkdir");
    let d = repo.to_string_lossy().to_string();
    std::fs::write(repo.join("crucible.toml"), MANIFEST).expect("manifest");
    std::fs::write(repo.join("workflow.star"), WORKFLOW).expect("source");
    for args in [
        vec!["init", "--quiet", "-b", "main", &d],
        vec!["-C", &d, "config", "user.email", "e2e@example.com"],
        vec!["-C", &d, "config", "user.name", "e2e"],
        vec!["-C", &d, "add", "-A"],
        vec!["-C", &d, "commit", "--quiet", "-m", "pack"],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(&args)
                .status()
                .expect("git runs")
                .success(),
            "git {args:?}"
        );
    }
    d
}

/// A contract registry every target matches: contract checking is not what this test exercises.
struct PermissiveContracts;

#[async_trait::async_trait]
impl crucible_controller::runs::contract::ContractReader for PermissiveContracts {
    async fn contract_version(
        &self,
        _target: &crucible_controller::runs::contract::DispatchTarget,
    ) -> Result<Option<String>, crucible_controller::runs::contract::ContractReadError> {
        Ok(Some(
            crucible_controller::runs::contract::CONTROLLER_CONTRACT_VERSION.to_string(),
        ))
    }
}

/// One admin request against the API router: JSON in, (status, JSON) out.
async fn api(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (http::StatusCode, serde_json::Value) {
    use tower::ServiceExt;
    let req = http::Request::builder()
        .method(method)
        .uri(uri)
        .header("x-auth-request-user", "wren");
    let req = match body {
        Some(json) => req
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(json.to_string())),
        None => req.body(axum::body::Body::empty()),
    }
    .expect("request");
    let res = app.clone().oneshot(req).await.expect("response");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// Every standing trigger through ONE running assembled daemon: a one-shot whose instant passes,
/// a schedule whose window is due, and a watch whose Jira query matches a ticket, all saved through
/// the API and all fired by the daemon's one trigger sweep. Each becomes an ordinary launch that
/// dispatches on the (fake) cluster and completes; the sidecars record what fired; and a settled
/// sweep fires nothing twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn standing_launches_fire_through_one_sweep() -> Result<()> {
    let _g = E2E_LOCK.lock().await;
    let root = tempfile::tempdir()?;
    let state_dir = root.path().join("state");

    // The tracker: one ticket that matches the watch, however it is asked.
    let jira = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/rest/api/3/search/jql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issues": [{"key": "ACME-10192", "fields": {"updated": "2026-08-26T12:00:00.000+0000"}}]
        })))
        .mount(&jira)
        .await;

    let mut cfg = test_cfg(&state_dir, Vec::new());
    cfg.admins = vec!["wren".to_string()];
    // Three launches fire in one sweep; a capped launch waits for a slot rather than failing, and
    // the cap is not what this test exercises.
    cfg.profile.max_concurrent_pods = 3;
    cfg.jira_base_url = Some(jira.uri());
    cfg.jira_email = Some("e2e@example.com".to_string());
    cfg.jira_api_token = Some("token".to_string());
    let db = Db::open(cfg.db_url()).await?;

    // The injectable cluster edge: every playbook pod "runs" the finished playbook session.
    let (tx, completions) = channel_completions();
    let launcher = Arc::new(FakePodDispatcher {
        pool: db.pool().clone(),
        deliver: true,
        tx,
        session: PLAYBOOK_SESSION_LOG,
        launches: Mutex::new(Vec::new()),
        keys: Mutex::new(Vec::new()),
        deleted: Mutex::new(Vec::new()),
    });
    crucible_controller::install_dispatcher(launcher.clone());
    let _launcher_guard = DispatcherGuard;

    // The assembled daemon and the API it shares its queue and override store with — the same
    // wiring the binary does.
    let queue = WorkQueue::new();
    let overrides = Arc::new(OverrideStore::new());
    let wiring = daemon::assemble(
        &db,
        &cfg,
        queue.clone(),
        overrides.clone(),
        completions,
        crucible_controller::authz::policy::ActivePolicy::default_set()
            .expect("the shipped default policy set loads"),
    );
    let app = crucible_controller::api::router(crucible_controller::api::state::ApiState::new(
        db.clone(),
        Arc::new(crucible_controller::QueueOverrideSink::new(
            overrides,
            queue.clone(),
        )),
        Arc::new(queue),
        Arc::new(crucible_controller::runs::clusters::ClusterClients::new(
            None,
        )),
        None,
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(crucible_controller::runs::contract::ContractRegistry::new(
            Arc::new(PermissiveContracts),
        )),
        crucible_controller::authz::policy::ActivePolicy::default_set()
            .expect("the shipped default policy set loads"),
        &cfg,
    ));

    // The pack, registered from a local clone.
    let repo = write_playbook_repo(root.path());
    let (status, registered) = api(
        &app,
        "POST",
        "/api/playbooks",
        Some(serde_json::json!({
            "id": "survey",
            "description": "reads a paper and files a spec",
            "repo": repo,
            "git_ref": "main",
        })),
    )
    .await;
    assert_eq!(status, http::StatusCode::CREATED, "{registered}");

    // Three standing launches, one per trigger.
    let fire_at = (jiff::Timestamp::now() + jiff::Span::new().seconds(1)).to_string();
    let (status, one_shot) = api(
        &app,
        "POST",
        "/api/one-shots",
        Some(serde_json::json!({
            "playbook": "survey",
            "params": {"topic": "deferred topic", "depth": "deep"},
            "max_cost": 3.5,
            "max_time": "30m",
            "fire_at": fire_at,
        })),
    )
    .await;
    assert_eq!(status, http::StatusCode::CREATED, "{one_shot}");
    assert_eq!(one_shot["owner_principal"], "user:wren");
    assert_eq!(one_shot["dispatch_target"], "hub");

    let (status, schedule) = api(
        &app,
        "POST",
        "/api/schedules",
        Some(serde_json::json!({
            "playbook": "survey",
            "params": {"topic": "scheduled topic"},
            "max_cost": 3.5,
            "max_time": "30m",
            "cron_expr": "0 * * * *",
            "tz": "UTC",
        })),
    )
    .await;
    assert_eq!(status, http::StatusCode::CREATED, "{schedule}");
    let schedule_id = schedule["id"].as_str().expect("id").to_string();
    // The next top of the hour is too far away for a test: the window is due already.
    sqlx::query("UPDATE playbook_schedules SET next_due_at = $2 WHERE id = $1")
        .bind(&schedule_id)
        .bind("2020-01-01T00:00:00Z")
        .execute(db.pool())
        .await?;

    let (status, watch) = api(
        &app,
        "POST",
        "/api/watches",
        Some(serde_json::json!({
            "playbook": "survey",
            "tracker": "jira",
            "query": "labels = backport-request",
            "key_param": "topic",
            "params": {"depth": "deep"},
            "max_cost": 3.5,
            "max_time": "30m",
            "since": "2026-01-01T00:00:00Z",
        })),
    )
    .await;
    assert_eq!(status, http::StatusCode::CREATED, "{watch}");
    let watch_id = watch["id"].as_str().expect("id").to_string();

    let shutdown = Arc::new(tokio::sync::Notify::new());
    let handle = {
        let shutdown = shutdown.clone();
        tokio::spawn(daemon::run(
            Vec::new(),
            daemon_cfg(),
            wiring,
            shutdown,
            None,
        ))
    };

    // The sweep fires all three; each launch dispatches and completes.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let keys = loop {
        let keys = launcher.keys.lock().expect("keys").clone();
        if keys.len() >= 3 {
            break keys;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "only {keys:?} dispatched in 60s"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    for key in &keys {
        wait_for_status(&db, key, Status::Done).await;
    }

    // Quiesce: more discovery ticks over the settled state fire nothing more.
    tokio::time::sleep(Duration::from_millis(500)).await;
    shutdown.notify_waiters();
    join_daemon(handle).await;
    eprintln!("--- events ---\n{}", event_ndjson(&db).await?);
    assert_eq!(
        launcher.launches.lock().expect("launches").len(),
        3,
        "one launch per trigger, none twice"
    );

    // Every launch is an ordinary run with its trigger's origin, params, and audit trail.
    let mut origins = Vec::new();
    for key in &keys {
        let (status, run) = api(&app, "GET", &format!("/api/playbook-runs/{key}"), None).await;
        assert_eq!(status, http::StatusCode::OK, "{run}");
        let launch = &run["launch"];
        assert_eq!(launch["status"], "done", "{run}");
        assert_eq!(run["runs"][0]["status"], "finished", "{run}");
        let origin = launch["origin"].as_str().expect("origin").to_string();
        match origin.as_str() {
            "deferred" => assert_eq!(launch["params"]["topic"], "deferred topic"),
            "schedule" => {
                assert_eq!(launch["params"]["topic"], "scheduled topic");
                assert_eq!(launch["schedule"], schedule_id.as_str());
            }
            "watch" => assert_eq!(launch["params"]["topic"], "ACME-10192"),
            other => panic!("unexpected origin {other}"),
        }
        origins.push(origin);
        let events: Vec<(String, String)> =
            sqlx::query_as("SELECT from_status, to_status FROM events WHERE key = $1 ORDER BY id")
                .bind(key)
                .fetch_all(db.pool())
                .await?;
        assert_eq!(
            events,
            vec![
                ("new".to_string(), "new".to_string()),
                ("new".to_string(), "running".to_string()),
                ("running".to_string(), "done".to_string()),
            ],
            "{key}: fired, launched, completed"
        );
    }
    origins.sort();
    assert_eq!(origins, vec!["deferred", "schedule", "watch"]);

    // The sidecars record what fired.
    let (_, one_shots) = api(&app, "GET", "/api/one-shots", None).await;
    let fired = &one_shots.as_array().expect("array")[0];
    assert_eq!(fired["status"], "fired", "{fired}");
    assert!(
        keys.contains(&fired["fired_key"].as_str().expect("fired_key").to_string()),
        "{fired}"
    );
    let (_, schedules) = api(&app, "GET", "/api/schedules", None).await;
    let row = &schedules.as_array().expect("array")[0];
    assert_eq!(row["consecutive_failures"], 0, "{row}");
    let last = row["last_fired_at"].as_str().expect("last_fired_at");
    assert!(
        row["next_due_at"].as_str().expect("next_due_at") > last,
        "{row}"
    );
    let (_, hits) = api(&app, "GET", &format!("/api/watches/{watch_id}/hits"), None).await;
    let hits = hits.as_array().expect("array");
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0]["item_id"], "ACME-10192");
    assert!(keys.contains(&hits[0]["launch_key"].as_str().expect("key").to_string()));
    let (_, watches) = api(&app, "GET", "/api/watches", None).await;
    let w = &watches.as_array().expect("array")[0];
    assert_eq!(w["watermark"], "2026-08-26T12:00:00Z", "{w}");
    assert!(w["last_swept_at"].as_str().is_some(), "{w}");
    assert!(
        jira.received_requests().await.expect("requests").len() >= 2,
        "the watch was swept more than once and launched once"
    );
    Ok(())
}
