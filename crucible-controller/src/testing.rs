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
        owner_refresh: None,
        auth_mode: crate::identity::auth::AuthMode::Proxy,
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
        loop_run_max_age: crate::model::MaxTime::hours(24),
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
        publishers: vec![],
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
        hooks_public_url: None,
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

/// Register (or repin) playbook `survey` to `tree`, with `exposure` as its stored disclosure.
#[cfg(test)]
pub(crate) async fn pin_playbook(
    pool: &sqlx::PgPool,
    tree: crucible_contract::pack_tree::PackTree,
    exposure: &serde_json::Value,
) {
    let pack = crate::playbooks::pack_trees::EncodedPack::new(tree).expect("encode");
    let digest =
        crate::playbooks::pack_trees::put_tree(&mut pool.acquire().await.expect("conn"), &pack)
            .await
            .expect("put tree");
    sqlx::query(
        r#"INSERT INTO playbooks (id, description, repo, git_ref, rev, path, tar_gz, tar_digest,
                                  tar_bytes, params_schema, schema_digest, core_rev, created_by,
                                  created_at, updated_at, tree_digest, exposure)
           VALUES ('survey', 'reads a paper', 'owner/packs', 'main', $1, '', $2, $1, $3,
                   '{"type":"object"}'::jsonb, 'sha256:schema', 'core1', 'wren',
                   '2026-08-23T00:00:00Z', '2026-08-23T00:00:00Z', $4, $5)
           ON CONFLICT (id) DO UPDATE SET rev = excluded.rev, tar_gz = excluded.tar_gz,
               tar_digest = excluded.tar_digest, tar_bytes = excluded.tar_bytes,
               tree_digest = excluded.tree_digest, exposure = excluded.exposure"#,
    )
    .bind(pack.tarball_digest())
    .bind(pack.tarball())
    .bind(i64::try_from(pack.tarball().len()).expect("size"))
    .bind(digest.as_str())
    .bind(exposure)
    .execute(pool)
    .await
    .expect("pin");
}

/// Launch the registered playbook [`pin_playbook`] stored, as `key`.
#[cfg(test)]
pub(crate) async fn launch_playbook(
    pool: &sqlx::PgPool,
    key: &str,
) -> crate::launches::model::PlaybookLaunch {
    let max_time = crate::model::MaxTime::parse("30m").expect("duration");
    let params = serde_json::json!({});
    let mut tx = pool.begin().await.expect("tx");
    let inserted = crate::launches::store::insert_playbook_launch_with(
        &mut tx,
        key,
        &crate::launches::model::NewPlaybookLaunch {
            playbook: "survey",
            repo: "owner/repo",
            title: "survey",
            params: &params,
            schema_digest: "sha256:schema",
            max_cost: 1.0,
            max_time: &max_time,
            advance_dedupe: false,
            dedupe_schedule: None,
            origin: crate::model::LaunchOrigin::Manual,
            draft_version: None,
            created_by: Some("wren"),
            launcher_groups: None,
        },
        &crate::playbooks::exposure::Extraction::Absent,
    )
    .await
    .expect("insert launch");
    assert!(inserted, "playbook survey is registered");
    tx.commit().await.expect("commit");
    crate::launches::store::get_playbook_launch(pool, key)
        .await
        .expect("read launch")
        .expect("launch")
}

/// Register a Chat Completions provider at `url` and make it the platform's autoresearch default,
/// so the ranker's calls land on a `wiremock` server serving canned verdicts.
#[cfg(test)]
pub(crate) async fn register_ranker(pool: &sqlx::PgPool, url: &str) -> anyhow::Result<()> {
    register_ranker_as(pool, "test-ranker", url).await?;
    crate::playbooks::providers::set_default(
        pool,
        &crate::playbooks::providers::DispatchDefault {
            scope_kind: crate::playbooks::providers::DefaultScope::Platform,
            scope_ref: String::new(),
            workload_class: crate::playbooks::providers::WorkloadClass::Autoresearch,
            role: crate::playbooks::providers::ModelRole::Agent,
            provider_id: "test-ranker".to_string(),
            model: None,
            fallback_provider_id: None,
            fallback_model: None,
        },
    )
    .await
}

/// Register a Chat Completions provider `id` at `url`, serving `test-ranker-model`.
#[cfg(test)]
pub(crate) async fn register_ranker_as(
    pool: &sqlx::PgPool,
    id: &str,
    url: &str,
) -> anyhow::Result<()> {
    let endpoint = crate::playbooks::providers::Endpoint {
        url: url.to_string(),
        protocol: crate::playbooks::providers::InferenceProtocol::ChatCompletions,
    };
    crate::playbooks::providers::upsert(
        pool,
        &crate::playbooks::providers::NewProvider {
            owner: crate::authz::model::Principal::platform(),
            id,
            display_name: id,
            kind: crate::playbooks::providers::ProviderKind::Custom,
            models: &["test-ranker-model".to_string()],
            default_model: Some("test-ranker-model"),
            secret: None,
            endpoint: Some(&endpoint),
            harness: None,
            enabled: true,
            created_by: "test",
        },
    )
    .await
}

/// Register one inference key under `owner`, the way an administrator would before pointing a
/// provider at it. Unlike a scope binding it is never bound to anything: the provider row names it.
#[cfg(test)]
pub(crate) async fn register_inference_key(
    pool: &sqlx::PgPool,
    owner: &str,
    name: &str,
) -> anyhow::Result<()> {
    use crate::authz::model::Principal;
    use crate::secrets::store::NewSecret;
    use crate::secrets::{ConsumerClass, SecretKind, SecretMode, SecretName, Visibility};
    let owner = Principal::parse(owner).expect("owner");
    let name = SecretName::parse(name).expect("name");
    let mut conn = pool.acquire().await?;
    crate::secrets::store::insert(
        &mut conn,
        &NewSecret {
            id: &uuid::Uuid::now_v7().to_string(),
            name: &name,
            owner: &owner,
            kind: SecretKind::InferenceApiKey,
            visibility: Visibility::BrokerOnly,
            consumer: ConsumerClass::Run,
            mode: SecretMode::Managed,
            vault_path: "platform/openai-key",
            current_version: Some(1),
            created_by: Some("alice"),
        },
    )
    .await?;
    Ok(())
}

/// Why [`symlink_pack`] cannot be a tree.
#[cfg(test)]
pub(crate) const SYMLINK_REASON: &str = "link is a symbolic link; a pack holds regular files only";

/// A gzipped tarball an older controller could have stored: a compiling draft skeleton beside a
/// symlink, so conversion records it unconvertible.
#[cfg(test)]
pub(crate) fn symlink_pack() -> Vec<u8> {
    use crate::playbooks::drafts::{SKELETON_MANIFEST, SKELETON_WORKFLOW};
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(gz);
    for (path, body) in [
        ("crucible.toml", SKELETON_MANIFEST),
        ("workflow.star", SKELETON_WORKFLOW),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        builder
            .append_data(&mut header, path, body.as_bytes())
            .expect("file");
    }
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    builder
        .append_link(&mut header, "link", "crucible.toml")
        .expect("link");
    builder.into_inner().expect("tar").finish().expect("gzip")
}

/// Write [`symlink_pack`] into the legacy pack columns of every `table` row matching `filter`,
/// then run startup conversion, which records those bytes unconvertible and leaves the rows
/// without a tree.
#[cfg(test)]
pub(crate) async fn make_unconvertible(pool: &sqlx::PgPool, table: &str, filter: &str) {
    let tgz = symlink_pack();
    let digest = crucible_contract::content_digest(&tgz);
    let (bytes_col, digest_col, size_col, tree_col) = match table {
        "playbook_standing_launches" => (
            "adopted_tar_gz",
            "adopted_tar_digest",
            "adopted_tar_bytes",
            "adopted_tree_digest",
        ),
        "pack_tarballs" => ("tar_gz", "digest", "bytes", "tree_digest"),
        _ => ("tar_gz", "tar_digest", "tar_bytes", "tree_digest"),
    };
    let updated = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {table} SET {bytes_col} = $1, {digest_col} = $2, {size_col} = $3 WHERE {filter}"
    )))
    .bind(&tgz)
    .bind(&digest)
    .bind(i64::try_from(tgz.len()).expect("size"))
    .execute(pool)
    .await
    .expect("write legacy bytes")
    .rows_affected();
    assert!(updated > 0, "no {table} row matches {filter}");
    crate::playbooks::pack_migration::convert_pack_trees(pool)
        .await
        .expect("convert");
    let (treeless, reason): (i64, Option<String>) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT (SELECT count(*) FROM {table} WHERE {filter} AND {tree_col} IS NULL),
                (SELECT unconvertible_reason FROM pack_digest_aliases WHERE old_digest = $1)"
    )))
    .bind(&digest)
    .fetch_one(pool)
    .await
    .expect("read conversion");
    assert_eq!(treeless, i64::try_from(updated).expect("count"));
    assert_eq!(reason.as_deref(), Some(SYMLINK_REASON));
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

    /// `len` bytes of text gzip cannot shrink much (a xorshift stream over 64 symbols), for packs
    /// that must be over the delivery budget while each file stays under the draft per-file cap.
    pub fn incompressible_text(len: usize, seed: u64) -> String {
        const SYMBOLS: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                char::from(SYMBOLS[(x >> 58) as usize])
            })
            .collect()
    }

    /// Write three files that each fit the draft per-file cap but together gzip past the delivery
    /// budget.
    pub fn write_over_budget_blobs(dir: &Path) {
        for seed in 1..=3 {
            std::fs::write(
                dir.join(format!("blob{seed}.txt")),
                incompressible_text(500 * 1024, seed),
            )
            .expect("blob");
        }
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
