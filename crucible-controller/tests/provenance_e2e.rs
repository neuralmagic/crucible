//! Commit provenance end to end, against a real Postgres: the engine binary runs a playbook whose
//! task reads its provenance input and reports a pushed commit, the controller ingests and signs
//! the run, the controller binary serves the statement with the key it loaded from its
//! environment, and `crux verify` checks it over HTTP. A tampered envelope fails.
//!
//! The engine is `$CRUCIBLE_BIN`, else the workspace's `crucible` binary, which a workspace build
//! produces.

use anyhow::{Context, Result, ensure};
use crucible_contract::provenance::{DispatchProvenance, EnvelopeDigest, Signer};
use ed25519_dalek::pkcs8::EncodePrivateKey;
use sqlx::postgres::PgPoolOptions;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

const STARTUP: Duration = Duration::from_secs(120);
const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const REPO: &str = "https://github.com/acme/widget";
const RUN: &str = "20261027T120000Z-provenance";

const MANIFEST: &str = r#"
[repo]
path = "."
[workspace]
dir = "workspace"
setup_cmd = "mkdir -p workspace && git -C workspace init -q && git -C workspace -c user.email=c@l -c user.name=c -c commit.gpgsign=false commit -q --allow-empty -m baseline"
[agent]
backend = "command"
agent_cmd = "true"
goal = "bump a dependency"
[workflow]
type = "playbook"
file = "workflow.star"
"#;

fn workflow() -> String {
    format!(
        r#"
build = command(name = "build", run = "printf '{{}}\n'")
push = command(
    name = "push",
    run = "python3 -c 'import json, os; json.dump(json.load(open(os.environ[\"CRUCIBLE_INPUTS_FILE\"]))[\"provenance\"], open(\"provenance.json\", \"w\"))' && printf '{{\"commit\": \"{REPO}/commit/{SHA}\", \"pr\": \"{REPO}/pull/7\"}}\n'",
    emits = {{"commit": "link", "pr": "link"}},
    depends_on = [build],
)
workflow(type = "playbook", tasks = [build, push])
"#
    )
}

fn engine_bin() -> Result<PathBuf> {
    let bin = match std::env::var_os("CRUCIBLE_BIN") {
        Some(bin) => PathBuf::from(bin),
        None => target_dir()?.join(profile_dir()?).join("crucible"),
    };
    ensure!(
        bin.is_file(),
        "no engine at {}: build it with `cargo build -p crucible` or set CRUCIBLE_BIN",
        bin.display()
    );
    let out = std::process::Command::new(&bin)
        .arg("--contract-version")
        .output()?;
    let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    ensure!(
        version == crucible_contract::CONTRACT_VERSION,
        "the engine at {} is contract {version}, not {}: rebuild it",
        bin.display(),
        crucible_contract::CONTRACT_VERSION
    );
    Ok(bin)
}

/// Where the workspace's binaries land, which a configured build directory can move away from
/// this test's own executable.
fn target_dir() -> Result<PathBuf> {
    let out = std::process::Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--offline",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .context("running cargo metadata")?;
    ensure!(
        out.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    meta["target_directory"]
        .as_str()
        .map(PathBuf::from)
        .context("cargo metadata names no target directory")
}

/// The profile directory this test was built into (`debug`), the parent of its `deps`.
fn profile_dir() -> Result<std::ffi::OsString> {
    std::env::current_exe()?
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .map(std::ffi::OsStr::to_os_string)
        .context("locating this test's profile directory")
}

/// Run the playbook as a controller would have dispatched it, and return its session log.
fn run_engine(dir: &Path, dispatch: &DispatchProvenance) -> Result<String> {
    std::fs::write(dir.join("crucible.toml"), MANIFEST)?;
    std::fs::write(dir.join("workflow.star"), workflow())?;
    let out = std::process::Command::new(engine_bin()?)
        .args([
            "plan",
            "run",
            "--manifest",
            "crucible.toml",
            "--max-cost",
            "1",
            "--max-time",
            "5m",
        ])
        .current_dir(dir)
        .env("FORGE_STORAGE_ROOT", dir.join("storage"))
        .env(crucible_contract::ENV_RUN_NAME, RUN)
        .env(
            crucible_contract::provenance::ENV_PROVENANCE,
            serde_json::to_string(dispatch)?,
        )
        .output()
        .context("running the engine")?;
    ensure!(
        out.status.success(),
        "the engine failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::read_to_string(dir.join("state/session.jsonl")).context("reading the session log")
}

async fn ledger(name: &str) -> Result<String> {
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must point at the test server");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&base)
        .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"DROP DATABASE IF EXISTS "{name}" WITH (FORCE)"#
    )))
    .execute(&admin)
    .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(r#"CREATE DATABASE "{name}""#)))
        .execute(&admin)
        .await?;
    crucible_controller::sibling_db_url(&base, name)
}

fn free_addr() -> Result<SocketAddr> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?)
}

/// The controller binary on `db`, serving at `api` and signing with the key at `key`.
fn controller(db: &str, api: SocketAddr, key: &Path, dir: &Path) -> Result<tokio::process::Child> {
    let log = std::fs::File::create(dir.join("controller.log"))?;
    tokio::process::Command::new(env!("CARGO_BIN_EXE_crucible-controller"))
        .arg("autopilot")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("KUBECONFIG", dir.join("no-kubeconfig"))
        .env("DATABASE_URL", db)
        .env("CONTROLLER_API_ADDR", api.to_string())
        .env("CONTROLLER_HOOKS_ADDR", free_addr()?.to_string())
        .env("CONTROLLER_STATE_DIR", dir.join("state"))
        .env("CONTROLLER_SCRATCH_DIR", dir.join("scratch"))
        .env(crucible_controller::provenance::KEY_PATH_ENV, key)
        .env("RUST_LOG", "info")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .context("spawning the controller")
}

async fn wait_healthy(
    child: &mut tokio::process::Child,
    api: SocketAddr,
    dir: &Path,
) -> Result<()> {
    let http = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + STARTUP;
    loop {
        if let Some(status) = child.try_wait()? {
            anyhow::bail!(
                "the controller exited {status} before serving:\n{}",
                std::fs::read_to_string(dir.join("controller.log")).unwrap_or_default()
            );
        }
        let healthy = http
            .get(format!("http://{api}/healthz"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success());
        if healthy {
            return Ok(());
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "the controller never answered /healthz:\n{}",
            std::fs::read_to_string(dir.join("controller.log")).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn a_pushed_commit_is_signed_served_and_verified_and_a_tampered_one_is_not() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let pack = dir.path().join("pack");
    std::fs::create_dir_all(&pack)?;
    let dispatch = DispatchProvenance {
        cluster: Some("east".into()),
        provider: Some("registry-provider".into()),
        sandbox_image: Some(format!(
            "registry.example/sandbox@sha256:{}",
            "b".repeat(64)
        )),
    };

    // The engine hands the pushing task its provenance, and reports the commit it pushed.
    let session = run_engine(&pack, &dispatch)?;
    let seen: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(
        pack.join("workspace/provenance.json"),
    )?)?;
    assert_eq!(
        seen,
        serde_json::json!({
            "run": RUN,
            "task": "push",
            "cluster": "east",
            "provider": "registry-provider",
            "sandbox_image": dispatch.sandbox_image,
            "agents": {},
        })
    );

    // The controller ingests the run beside what it recorded at dispatch, and signs what the run
    // pushed with a key mounted as PKCS#8 PEM.
    let db_url = ledger("provenance_e2e").await?;
    let db = crucible_controller::Db::open(&db_url).await?;
    let pool = db.pool().clone();
    crucible_controller::runs::ingest::ingest_session(
        &db,
        &crucible_controller::runs::ingest::IngestTarget {
            run_id: RUN,
            scope_id: None,
            issue: None,
            pod: None,
            session_uri: "db://run-session/e2e",
        },
        &session,
    )
    .await?;
    crucible_controller::provenance::set_dispatch_provenance(&pool, RUN, &dispatch).await?;
    let key = dir.path().join("provenance.pem");
    let pem = ed25519_dalek::SigningKey::from_bytes(&[42; 32])
        .to_pkcs8_pem(ed25519_dalek::pkcs8::spki::der::pem::LineEnding::LF)?;
    std::fs::write(&key, pem.as_bytes())?;
    let signer = crucible_controller::provenance::load_signer(&key)?;
    let signed = crucible_controller::provenance::attest_run(&pool, &signer, RUN).await?;
    assert_eq!(signed.len(), 1, "one commit pushed, one statement");

    // The controller binary serves it; crux checks it over HTTP against the published key set.
    let api = free_addr()?;
    let mut child = controller(&db_url, api, &key, dir.path())?;
    wait_healthy(&mut child, api, dir.path()).await?;
    let client = crux::Client::connect(&crux::config::Config {
        url: format!("http://{api}"),
        auth: crux::config::Auth::None,
    })?;

    let by_commit = crux::ops::verify(&client, SHA, Some(REPO), None).await?;
    assert!(
        by_commit.starts_with(&format!("ok   {}", signed[0])),
        "{by_commit}"
    );
    assert!(by_commit.contains("cluster  east"), "{by_commit}");
    assert!(
        by_commit.contains("provider registry-provider"),
        "{by_commit}"
    );
    assert!(by_commit.contains("check    build pass"), "{by_commit}");
    assert!(
        by_commit.contains(&format!("pr       {REPO}/pull/7")),
        "{by_commit}"
    );

    let keys = dir.path().join("keys.json");
    std::fs::write(
        &keys,
        serde_json::to_vec(&Signer::from_seed(&[42; 32]).key_set())?,
    )?;
    let by_digest =
        crux::ops::verify(&client, signed[0].as_str(), None, Some(keys.as_path())).await?;
    assert!(by_digest.starts_with("ok   "), "{by_digest}");

    let stranger = dir.path().join("stranger.json");
    std::fs::write(
        &stranger,
        serde_json::to_vec(&Signer::from_seed(&[1; 32]).key_set())?,
    )?;
    let unknown = crux::ops::verify(&client, SHA, None, Some(stranger.as_path()))
        .await
        .expect_err("a key set without the signer's key verifies nothing");
    assert!(format!("{unknown:#}").contains("KeyId"), "{unknown:#}");

    let wrong_repo = crux::ops::verify(&client, SHA, Some("https://github.com/acme/other"), None)
        .await
        .expect_err("the subject names another repository");
    assert!(
        format!("{wrong_repo:#}").contains("Subject"),
        "{wrong_repo:#}"
    );

    // Rewriting the stored bytes breaks the digest the statement is addressed by.
    let original: Vec<u8> =
        sqlx::query_scalar("SELECT envelope FROM commit_provenance WHERE envelope_digest = $1")
            .bind(signed[0].as_str())
            .fetch_one(&pool)
            .await?;
    let forged = forge(&original)?;
    sqlx::query("UPDATE commit_provenance SET envelope = $2 WHERE envelope_digest = $1")
        .bind(signed[0].as_str())
        .bind(&forged)
        .execute(&pool)
        .await?;
    let tampered = crux::ops::verify(&client, SHA, None, None)
        .await
        .expect_err("a rewritten envelope fails");
    assert!(
        format!("{tampered:#}").contains(&format!("FAIL {}", signed[0])),
        "{tampered:#}"
    );
    assert!(format!("{tampered:#}").contains("Digest"), "{tampered:#}");

    // Re-addressing the forgery under its own digest leaves the signature to refuse it.
    sqlx::query("UPDATE commit_provenance SET envelope_digest = $2 WHERE envelope_digest = $1")
        .bind(signed[0].as_str())
        .bind(EnvelopeDigest::of(&forged).as_str())
        .execute(&pool)
        .await?;
    let readdressed = crux::ops::verify(&client, SHA, None, None)
        .await
        .expect_err("a forged statement under its own digest fails");
    assert!(
        format!("{readdressed:#}").contains("Signature"),
        "{readdressed:#}"
    );
    Ok(())
}

/// The envelope with its statement rewritten to name another cluster, signature kept.
fn forge(envelope: &[u8]) -> Result<Vec<u8>> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut env: serde_json::Value = serde_json::from_slice(envelope)?;
    let payload = b64.decode(env["payload"].as_str().context("payload")?)?;
    let mut statement: serde_json::Value = serde_json::from_slice(&payload)?;
    statement["predicate"]["cluster"] = serde_json::json!("somewhere else");
    env["payload"] = serde_json::json!(b64.encode(serde_json::to_vec(&statement)?));
    Ok(serde_json::to_vec(&env)?)
}
