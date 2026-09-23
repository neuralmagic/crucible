//! Shared test fixtures: config harnesses, ledger helpers, and the pack repos the playbook tests
//! build.

/// Write an executable script for a test to spawn. The bytes go through a short-lived `sh`
/// child rather than this process, so no thread here ever holds the file open for writing: a
/// sibling test forking during that window would inherit the descriptor and the spawn under
/// test would fail with ETXTBSY.
#[cfg(test)]
pub fn write_exec(path: &std::path::Path, body: &str) {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new("sh")
        .args(["-c", r#"cat > "$1" && chmod 755 "$1""#, "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn sh");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(body.as_bytes())
        .expect("write script");
    assert!(
        child.wait().expect("sh").success(),
        "writing {}",
        path.display()
    );
}

/// A `ControllerCfg` for tests that drive the daemon against a temp state dir and a fresh test
/// ledger, every field set explicitly rather than taken from the clap defaults.
#[cfg(test)]
pub(crate) fn controller_cfg(
    state_dir: &std::path::Path,
    repos: Vec<String>,
) -> crate::config::ControllerCfg {
    crate::config::ControllerCfg {
        db: crate::test_ledger_url(),
        repos,
        ..cfg_with(state_dir)
    }
}

/// A `ControllerCfg` parsed from `args` the way the binary parses its command line: clap defaults
/// and `CONTROLLER_*` env vars apply. `args[0]` is the program name.
#[cfg(test)]
pub(crate) fn cfg_from_args<I, T>(args: I) -> crate::config::ControllerCfg
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    <ArgHarness as clap::Parser>::parse_from(args).cfg
}

/// [`cfg_from_args`] that reports a parse refusal instead of exiting.
#[cfg(test)]
pub(crate) fn try_cfg_from_args<I, T>(args: I) -> Result<crate::config::ControllerCfg, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    <ArgHarness as clap::Parser>::try_parse_from(args).map(|h| h.cfg)
}

#[cfg(test)]
#[derive(clap::Parser)]
struct ArgHarness {
    #[command(flatten)]
    cfg: crate::config::ControllerCfg,
}

/// The same explicit `ControllerCfg`, rooted at `state_dir`, for tests that hold their own pool
/// and never open the `db` URL.
#[cfg(test)]
pub(crate) fn cfg_with(state_dir: &std::path::Path) -> crate::config::ControllerCfg {
    crate::config::ControllerCfg {
        secret_provider: None,
        schedule_owner_ttl_secs: 604800,
        local_secret_allowlist: Vec::new(),
        state_dir: state_dir.to_path_buf(),
        scratch_dir: None,
        db: "postgres://unused.invalid/none".to_string(),
        repos: vec![],
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
        playbook_max_time_cap: crate::model::MaxTime::hours(4),
        schedule_auto_disable_failures: 5,
        allow_t3: false,
        allowed_tiers: vec![crucible_contract::Tier::T0, crucible_contract::Tier::T1],
        prescope_grounded: false,
        rank_horizon_days: 0,
        grounded_executor: crucible_controller::GroundedExecutor::Local,
        deploy_profile: None,
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
        profile: crate::config::Profile::default(),
    }
}

/// One request through `app`: the status and the raw response bytes.
#[cfg(test)]
pub(crate) async fn oneshot_bytes(
    app: &axum::Router,
    req: axum::http::Request<axum::body::Body>,
) -> (axum::http::StatusCode, axum::body::Bytes) {
    use tower::ServiceExt as _;
    let res = app.clone().oneshot(req).await.expect("infallible");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, bytes)
}

/// [`oneshot_bytes`] with the body decoded as JSON, `Null` when it is not JSON.
#[cfg(test)]
pub(crate) async fn call(
    app: &axum::Router,
    req: axum::http::Request<axum::body::Body>,
) -> (axum::http::StatusCode, serde_json::Value) {
    let (status, bytes) = oneshot_bytes(app, req).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// Real engine inputs for tests that render through the linked `crucible` library: a deploy
/// profile, the smallest loop and playbook packs that render, and workflow sources that compile.
#[cfg(test)]
pub mod fixtures {
    use std::path::{Path, PathBuf};

    /// The smallest deploy profile a render accepts.
    pub const DEPLOY_PROFILE: &str = concat!(
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

    /// Write [`DEPLOY_PROFILE`] as `<dir>/profile.toml`.
    pub fn write_deploy_profile(dir: &Path) -> PathBuf {
        let path = dir.join("profile.toml");
        std::fs::write(&path, DEPLOY_PROFILE).expect("write deploy profile");
        path
    }

    /// The smallest single-domain loop manifest `deploy render` accepts: an openshell agent with a
    /// sandbox image, a judge, and the `[deploy]` target a single domain must name.
    pub const LOOP_PACK_MANIFEST: &str = concat!(
        "[repo]\n",
        "url = \"https://github.com/owner/repo.git\"\n",
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

    /// A playbook manifest that both registers (`[workflow] type = "playbook"`) and renders as a
    /// run (`[repo]` + `[agent]`).
    pub const PLAYBOOK_PACK_MANIFEST: &str = concat!(
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

    /// The smallest playbook manifest the registry accepts from a git checkout: an empty `[agent]`
    /// and the workflow file beside it.
    pub const PLAYBOOK_REPO_MANIFEST: &str = "[repo]\npath = \".\"\n\n[agent]\n\n[workflow]\ntype = \"playbook\"\nfile = \"workflow.star\"\n";

    /// Run one git command and fail the test if it does not exit 0.
    pub fn run_git(args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    /// A real git repo under `<dir>/packrepo` holding a one-file playbook pack (`manifest` as
    /// `crucible.toml`, `source` as `workflow.star`), committed on `main`. Returns its path.
    /// Non-GitHub paths ride `repo_clone_url` untouched, so a local path is a legitimate clone
    /// source.
    pub fn git_pack_repo(dir: &Path, manifest: &str, source: &str) -> String {
        let repo = dir.join("packrepo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        let d = repo.to_string_lossy().to_string();
        run_git(&["init", "--quiet", "-b", "main", &d]);
        run_git(&["-C", &d, "config", "user.email", "t@example.com"]);
        run_git(&["-C", &d, "config", "user.name", "t"]);
        std::fs::write(repo.join("crucible.toml"), manifest).expect("manifest");
        std::fs::write(repo.join("workflow.star"), source).expect("source");
        run_git(&["-C", &d, "add", "-A"]);
        run_git(&["-C", &d, "commit", "--quiet", "-m", "pack"]);
        d
    }

    /// A one-task playbook that declares no parameters.
    pub const WORKFLOW_NO_PARAMS: &str = concat!(
        "params = {}\n",
        "\n",
        "hello = command(name = \"hello\", run = \"echo hello\")\n",
        "\n",
        "workflow(type = \"playbook\", tasks = [hello], result = hello)\n",
    );

    /// A one-task playbook with one required `topic` parameter.
    pub const WORKFLOW_TOPIC: &str = concat!(
        "params = {\"topic\": {\"type\": \"string\", \"required\": True}}\n",
        "\n",
        "hello = command(name = \"hello\", run = \"echo hello\")\n",
        "\n",
        "workflow(type = \"playbook\", tasks = [hello], result = hello)\n",
    );

    /// A one-task playbook with a required `topic` and an optional `depth` parameter.
    pub const WORKFLOW_TOPIC_DEPTH: &str = concat!(
        "params = {\n",
        "    \"topic\": {\"type\": \"string\", \"required\": True},\n",
        "    \"depth\": {\"type\": \"string\", \"default\": \"shallow\"},\n",
        "}\n",
        "\n",
        "hello = command(name = \"hello\", run = \"echo hello\")\n",
        "\n",
        "workflow(type = \"playbook\", tasks = [hello], result = hello)\n",
    );

    /// A playbook the compiler refuses: `dpeth` is undefined at line 3, column 39.
    pub const WORKFLOW_BROKEN: &str = concat!(
        "params = {}\n",
        "\n",
        "hello = command(name = \"hello\", run = dpeth)\n",
        "\n",
        "workflow(type = \"playbook\", tasks = [hello], result = hello)\n",
    );

    /// A source the parser refuses outright, so even params extraction fails.
    pub const WORKFLOW_UNPARSABLE: &str = "params = {\n";

    /// Write a loop pack (just its manifest) under `<dir>/pack`.
    pub fn write_loop_pack(dir: &Path) -> PathBuf {
        let pack = dir.join("pack");
        std::fs::create_dir_all(&pack).expect("mkdir pack");
        std::fs::write(pack.join("crucible.toml"), LOOP_PACK_MANIFEST).expect("write manifest");
        pack
    }

    /// Write a playbook pack (manifest + `source`) under `<dir>/pack`.
    pub fn write_playbook_pack(dir: &Path, source: &str) -> PathBuf {
        let pack = dir.join("pack");
        std::fs::create_dir_all(&pack).expect("mkdir pack");
        std::fs::write(pack.join("crucible.toml"), PLAYBOOK_PACK_MANIFEST).expect("write manifest");
        std::fs::write(pack.join("workflow.star"), source).expect("write workflow");
        pack
    }

    /// The params schema the engine extracts from `source`, for asserting what a registration
    /// stored against what the pack declares.
    pub fn schema_of(source: &str) -> serde_json::Value {
        crucible::plan::starlark::declared_params(source, Path::new("workflow.star"))
            .expect("the fixture source declares params")
    }

    /// Every command the rendered pod runs, init containers first, one per line: `command` and
    /// `args` joined with spaces. The pod runs exec-form argv, not a shell script.
    pub fn wrapper_of(pod: &k8s_openapi::api::core::v1::Pod) -> String {
        let spec = pod.spec.as_ref().expect("the rendered pod carries a spec");
        let line = |c: &k8s_openapi::api::core::v1::Container| {
            let mut parts = c.command.clone().unwrap_or_default();
            parts.extend(c.args.clone().unwrap_or_default());
            parts.join(" ")
        };
        spec.init_containers
            .iter()
            .flatten()
            .chain(spec.containers.iter())
            .map(line)
            .collect::<Vec<_>>()
            .join("\n")
    }
}
