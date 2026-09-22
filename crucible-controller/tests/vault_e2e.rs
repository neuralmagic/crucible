//! The Vault client, end to end against a real `vault server -dev`.
//!
//! No mocks: every test provisions a real KV v2 mount, a real AppRole (or JWT) role with a real
//! policy, and drives [`crucible_controller::secrets::vault::VaultClient`] over HTTP against the server.
//!
//! The server comes from one of two places. CI starts `vault server -dev` and exports `VAULT_ADDR`
//! plus `VAULT_TOKEN`; a laptop with the `vault` binary on PATH gets a per-test dev server spawned
//! here. With neither, the tests print why they are skipping — unless
//! `CRUCIBLE_REQUIRE_VAULT_TESTS` is set, which turns a skip into a failure so CI can never go
//! green by silently skipping the suite.

use crucible_controller::secrets::vault::{
    KvData, KvVersion, MountPath, SecretId, SecretIdSource, VaultAuth, VaultCfg, VaultClient,
    VaultError, VaultPath, VaultRefError, VaultToken,
};
use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// A fixed P-256 keypair for the JWT-auth test. Test-only material: it authenticates nothing but
/// this file's dev server, which lives for the length of one test.
const JWT_PRIVATE_KEY: &str = "-----BEGIN PRIVATE KEY-----\n\
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgPB3vrmOKVvmw6rRr\n\
M4MddJxuJISL8XCO89zP3Mn1e7KhRANCAAQb9wmajC409RoTOAs04FB2FdNo6Bnt\n\
3Ad5E0+ohjd0MyUiRtL3sYmMQdEMa4BpD419peYkJBr615mnh9QrjQf7\n\
-----END PRIVATE KEY-----\n";
const JWT_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\n\
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEG/cJmowuNPUaEzgLNOBQdhXTaOgZ\n\
7dwHeRNPqIY3dDMlIkbS97GJjEHRDGuAaQ+NfaXmJCQa+teZp4fUK40H+w==\n\
-----END PUBLIC KEY-----\n";

/// The KV v2 mount every test provisions under; the client's own default.
const MOUNT: &str = "crucible";

/// Distinct path prefixes per test, so a shared CI server never has two tests on one path.
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// How many dev servers this binary runs at once. Every test that spawns its own gets full
/// isolation, but a dozen Vaults booting simultaneously on one box is what makes a health poll
/// time out, so the spawns queue.
static SERVER_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

fn uniq(tag: &str) -> String {
    let n = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("{tag}-{}-{n}", std::process::id())
}

// --- the dev server -----------------------------------------------------------------------------

/// A reachable Vault with a root token. Owns the process when it spawned one.
struct DevVault {
    addr: String,
    root: String,
    http: reqwest::Client,
    child: Option<Child>,
    /// Held for the life of the server, so only [`SERVER_SLOTS`] of them boot at a time.
    _slot: Option<tokio::sync::SemaphorePermit<'static>>,
}

impl Drop for DevVault {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Set when the caller insists the suite must run (CI). A skip then panics.
fn required() -> bool {
    std::env::var("CRUCIBLE_REQUIRE_VAULT_TESTS")
        .map(|v| !v.trim().is_empty() && v != "0")
        .unwrap_or(false)
}

fn vault_bin() -> String {
    std::env::var("VAULT_BIN").unwrap_or_else(|_| "vault".to_string())
}

/// The server for a test that may share one: CI's, else a freshly spawned dev server.
async fn vault() -> Option<DevVault> {
    let external = (
        std::env::var("VAULT_ADDR").ok().filter(|v| !v.is_empty()),
        std::env::var("VAULT_TOKEN").ok().filter(|v| !v.is_empty()),
    );
    if let (Some(addr), Some(root)) = external {
        return Some(DevVault {
            addr: addr.trim_end_matches('/').to_string(),
            root,
            http: reqwest::Client::new(),
            child: None,
            _slot: None,
        });
    }
    spawn_vault().await
}

/// A dev server this test alone owns — for the tests that seal it or otherwise wreck it.
///
/// A port is picked by binding zero and letting go, which leaves a window for another process to
/// take it, so a server that dies on startup is retried on a fresh port rather than reported as a
/// timeout.
async fn spawn_vault() -> Option<DevVault> {
    let slot = SERVER_SLOTS
        .acquire()
        .await
        .expect("the slot semaphore is never closed");
    let mut failures = Vec::new();
    for _ in 0..3 {
        match spawn_once().await {
            Ok(Some(mut server)) => {
                server._slot = Some(slot);
                return Some(server);
            }
            // No binary: skip (or fail, when the suite is required).
            Ok(None) => return None,
            Err(why) => failures.push(why),
        }
    }
    panic!("no dev vault would start: {}", failures.join(" | "));
}

/// One spawn attempt. `Ok(None)` means there is no `vault` binary at all.
async fn spawn_once() -> Result<Option<DevVault>, String> {
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a scratch port");
        let port = listener.local_addr().expect("scratch port").port();
        drop(listener);
        port
    };
    let root = format!("root-{}", uuid::Uuid::now_v7());
    let log = tempfile::NamedTempFile::new().expect("a log file");
    let errors = log.reopen().expect("reopen the log");
    let mut child = match Command::new(vault_bin())
        .args([
            "server",
            "-dev",
            "-dev-no-store-token",
            &format!("-dev-root-token-id={root}"),
            &format!("-dev-listen-address=127.0.0.1:{port}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::from(errors))
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            let msg = format!(
                "no `vault` binary ({e}); install it with `brew install hashicorp/tap/vault` \
                 or point VAULT_ADDR/VAULT_TOKEN at a dev server"
            );
            assert!(!required(), "{msg}");
            eprintln!("SKIP: {msg}");
            return Ok(None);
        }
    };
    let http = reqwest::Client::new();
    let addr = format!("http://127.0.0.1:{port}");
    for _ in 0..200 {
        if let Ok(resp) = http.get(format!("{addr}/v1/sys/health")).send().await
            && resp.status().is_success()
        {
            return Ok(Some(DevVault {
                addr,
                root,
                http,
                child: Some(child),
                _slot: None,
            }));
        }
        if let Ok(Some(status)) = child.try_wait() {
            let why = std::fs::read_to_string(log.path()).unwrap_or_default();
            return Err(format!(
                "the dev vault on {port} exited with {status}: {}",
                why.trim().lines().next_back().unwrap_or("(no output)")
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = child.kill();
    let _ = child.wait();
    let why = std::fs::read_to_string(log.path()).unwrap_or_default();
    Err(format!(
        "the dev vault on {port} never became healthy: {}",
        why.trim().lines().next_back().unwrap_or("(no output)")
    ))
}

/// `let Some(v) = start!() else { return };` — the skip is the only way out.
macro_rules! start {
    () => {
        match vault().await {
            Some(v) => v,
            None => return,
        }
    };
    (owned) => {
        match spawn_vault().await {
            Some(v) => v,
            None => return,
        }
    };
}

impl DevVault {
    /// One root-token API call. Tests are allowed to panic, and a provisioning failure is a broken
    /// test, so this asserts rather than propagating.
    async fn root_call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let mut req = self
            .http
            .request(method.clone(), format!("{}/v1/{path}", self.addr))
            .header("X-Vault-Token", &self.root);
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req.send().await.expect("vault request");
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        assert!(status.is_success(), "{method} {path} -> {status}: {text}");
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }

    /// Idempotent: a shared CI server already has the mount from the first test that ran.
    async fn ensure_kv_mount(&self, mount: &str) {
        let resp = self
            .http
            .post(format!("{}/v1/sys/mounts/{mount}", self.addr))
            .header("X-Vault-Token", &self.root)
            .json(&json!({"type": "kv", "options": {"version": "2"}}))
            .send()
            .await
            .expect("mount request");
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        assert!(
            status.is_success() || text.contains("already in use"),
            "enabling {mount}: {status}: {text}"
        );
    }

    /// Idempotent enable of an auth method.
    async fn ensure_auth(&self, path: &str, kind: &str) {
        let resp = self
            .http
            .post(format!("{}/v1/sys/auth/{path}", self.addr))
            .header("X-Vault-Token", &self.root)
            .json(&json!({"type": kind}))
            .send()
            .await
            .expect("auth request");
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        assert!(
            status.is_success() || text.contains("already in use"),
            "enabling {path}: {status}: {text}"
        );
    }

    /// A policy over exactly one prefix of the KV mount.
    async fn write_prefix_policy(&self, name: &str, mount: &str, prefix: &str) {
        let policy = format!(
            "path \"{mount}/data/{prefix}/*\" {{ capabilities = [\"create\", \"read\", \"update\", \"delete\", \"list\"] }}\n\
             path \"{mount}/metadata/{prefix}/*\" {{ capabilities = [\"create\", \"read\", \"update\", \"delete\", \"list\"] }}\n\
             path \"{mount}/delete/{prefix}/*\" {{ capabilities = [\"update\"] }}\n"
        );
        self.root_call(
            reqwest::Method::PUT,
            &format!("sys/policies/acl/{name}"),
            Some(json!({ "policy": policy })),
        )
        .await;
    }

    /// An AppRole role plus a fresh role id / secret id pair.
    async fn approle(
        &self,
        role: &str,
        policy: &str,
        ttl: &str,
        max_ttl: &str,
    ) -> (String, String) {
        self.ensure_auth("approle", "approle").await;
        self.root_call(
            reqwest::Method::POST,
            &format!("auth/approle/role/{role}"),
            Some(json!({
                "token_policies": policy,
                "token_ttl": ttl,
                "token_max_ttl": max_ttl,
                "secret_id_num_uses": 0,
                "secret_id_ttl": "60m",
            })),
        )
        .await;
        let role_id = self
            .root_call(
                reqwest::Method::GET,
                &format!("auth/approle/role/{role}/role-id"),
                None,
            )
            .await["data"]["role_id"]
            .as_str()
            .expect("role_id")
            .to_string();
        let secret_id = self
            .root_call(
                reqwest::Method::POST,
                &format!("auth/approle/role/{role}/secret-id"),
                Some(json!({})),
            )
            .await["data"]["secret_id"]
            .as_str()
            .expect("secret_id")
            .to_string();
        (role_id, secret_id)
    }

    /// A response-wrapping token carrying a fresh secret id for `role` — what an operator mints
    /// into the Secret the chart mounts.
    async fn wrapped_secret_id(&self, role: &str) -> String {
        let wrapped: Value = self
            .http
            .post(format!(
                "{}/v1/auth/approle/role/{role}/secret-id",
                self.addr
            ))
            .header("X-Vault-Token", &self.root)
            .header("X-Vault-Wrap-TTL", "120s")
            .json(&json!({}))
            .send()
            .await
            .expect("wrap request")
            .json()
            .await
            .expect("wrap body");
        wrapped["wrap_info"]["token"]
            .as_str()
            .expect("a wrapping token")
            .to_string()
    }

    /// A child token carrying exactly `policy` — the stand-in for a registrant's own Vault token.
    async fn issue_token(&self, policy: &str) -> VaultToken {
        let created = self
            .root_call(
                reqwest::Method::POST,
                "auth/token/create",
                Some(json!({"policies": [policy], "ttl": "10m", "no_parent": true})),
            )
            .await;
        VaultToken::new(
            created["auth"]["client_token"]
                .as_str()
                .expect("client_token"),
        )
    }

    /// Revoke every token the given AppRole role minted, by accessor — what an operator does when
    /// a credential leaks, and the only way to make a live client's token stop working.
    async fn revoke_approle_tokens(&self, role: &str) {
        let listed = self
            .root_call(
                reqwest::Method::from_bytes(b"LIST").expect("LIST"),
                "auth/token/accessors",
                None,
            )
            .await;
        let accessors = listed["data"]["keys"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut revoked = 0;
        for accessor in accessors {
            let Some(accessor) = accessor.as_str() else {
                continue;
            };
            let resp = self
                .http
                .post(format!("{}/v1/auth/token/lookup-accessor", self.addr))
                .header("X-Vault-Token", &self.root)
                .json(&json!({ "accessor": accessor }))
                .send()
                .await
                .expect("lookup-accessor");
            if !resp.status().is_success() {
                continue;
            }
            let looked: Value = resp.json().await.unwrap_or(Value::Null);
            if looked["data"]["meta"]["role_name"].as_str() != Some(role) {
                continue;
            }
            self.root_call(
                reqwest::Method::POST,
                "auth/token/revoke-accessor",
                Some(json!({ "accessor": accessor })),
            )
            .await;
            revoked += 1;
        }
        assert!(revoked > 0, "found no live token for role {role} to revoke");
    }
}

// --- client construction ------------------------------------------------------------------------

/// Build a client the way the deploy does: through the chart-shaped environment block, so the
/// config parser is on the e2e path rather than bypassed.
fn client_from_env(pairs: &[(&str, &str)]) -> VaultClient {
    let owned: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    let cfg = VaultCfg::from_lookup(|name| {
        owned
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    })
    .expect("the vault config parses")
    .expect("VAULT_ADDR is set");
    VaultClient::new(cfg).expect("client")
}

fn approle_client(
    server: &DevVault,
    role_id: &str,
    secret_id: &str,
    renew_threshold_secs: &str,
) -> VaultClient {
    client_from_env(&[
        ("VAULT_ADDR", &server.addr),
        ("VAULT_AUTH", "approle"),
        ("VAULT_MOUNT", MOUNT),
        ("VAULT_ROLE_ID", role_id),
        ("VAULT_SECRET_ID", secret_id),
        ("VAULT_RENEW_THRESHOLD_SECS", renew_threshold_secs),
    ])
}

fn path(raw: &str) -> VaultPath {
    VaultPath::parse(raw).expect("a valid vault path")
}

// --- the tests ----------------------------------------------------------------------------------

/// AppRole login plus the whole KV v2 surface: write a version, read the current one, read a named
/// one, list the history, soft-delete a version, and remove the path for good.
#[tokio::test]
async fn approle_login_and_the_kv_v2_surface() {
    let server = start!();
    let id = uniq("approle");
    server.ensure_kv_mount(MOUNT).await;
    server.write_prefix_policy(&id, MOUNT, &id).await;
    let (role_id, secret_id) = server.approle(&id, &id, "20m", "1h").await;
    let client = approle_client(&server, &role_id, &secret_id, "60");

    let secret = path(&format!("{id}/user:wren/gh-token"));
    let v1 = client
        .put(
            &secret,
            &KvData::new().with("token", "first").with("note", "n"),
        )
        .await
        .expect("write v1");
    assert_eq!(v1, KvVersion::new(1));
    assert_eq!(client.stats().logins, 1, "one login covers every call");

    let v2 = client
        .put(&secret, &KvData::new().with("token", "second"))
        .await
        .expect("write v2");
    assert_eq!(v2, KvVersion::new(2));

    let current = client.get(&secret, None).await.expect("read current");
    assert_eq!(current.version, v2);
    assert_eq!(current.data.get("token"), Some("second"));
    assert_eq!(
        current.data.get("note"),
        None,
        "a new version replaces, not merges"
    );

    let named = client.get(&secret, Some(v1)).await.expect("read v1");
    assert_eq!(named.version, v1);
    assert_eq!(named.data.get("token"), Some("first"));
    assert_eq!(named.data.get("note"), Some("n"));

    let history = client.versions(&secret).await.expect("versions");
    assert_eq!(history.current, v2);
    assert_eq!(
        history
            .versions
            .iter()
            .map(|v| v.version)
            .collect::<Vec<_>>(),
        vec![v1, v2]
    );
    assert!(history.versions.iter().all(|v| !v.deleted && !v.destroyed));

    client
        .delete_versions(&secret, &[v1])
        .await
        .expect("soft delete v1");
    let after = client
        .versions(&secret)
        .await
        .expect("versions after delete");
    assert!(after.versions[0].deleted, "v1 is soft-deleted");
    assert!(!after.versions[1].deleted, "v2 is untouched");
    let gone = client.get(&secret, Some(v1)).await;
    assert!(
        matches!(gone, Err(VaultError::NotFound { .. })),
        "a deleted version reads as not found, got {gone:?}"
    );
    assert_eq!(
        client
            .get(&secret, None)
            .await
            .expect("current survives")
            .version,
        v2
    );

    client.delete_versions(&secret, &[]).await.expect("no-op");

    client.delete_all(&secret).await.expect("delete the path");
    assert!(matches!(
        client.get(&secret, None).await,
        Err(VaultError::NotFound { .. })
    ));
    assert!(matches!(
        client.versions(&secret).await,
        Err(VaultError::NotFound { .. })
    ));
    assert_eq!(
        client.stats().logins,
        1,
        "the whole surface ran on one login"
    );
}

/// The same client code against a JWT role bound to a subject and audience — the method the hub
/// switches to once workload identity is confirmed. Selected by config alone.
#[tokio::test]
async fn jwt_login_is_selected_by_config() {
    let server = start!();
    let id = uniq("jwt");
    server.ensure_kv_mount(MOUNT).await;
    server.write_prefix_policy(&id, MOUNT, &id).await;
    server.ensure_auth("jwt", "jwt").await;
    server
        .root_call(
            reqwest::Method::POST,
            "auth/jwt/config",
            Some(json!({ "jwt_validation_pubkeys": [JWT_PUBLIC_KEY] })),
        )
        .await;
    let subject = format!("system:serviceaccount:crucible:{id}");
    server
        .root_call(
            reqwest::Method::POST,
            &format!("auth/jwt/role/{id}"),
            Some(json!({
                "role_type": "jwt",
                "user_claim": "sub",
                "bound_subject": subject,
                "bound_audiences": ["vault"],
                "token_policies": id,
                "token_ttl": "20m",
            })),
        )
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let token_path = dir.path().join("token");
    std::fs::write(&token_path, sign_jwt(&subject, "vault")).expect("write the projected token");

    let client = client_from_env(&[
        ("VAULT_ADDR", &server.addr),
        ("VAULT_AUTH", "jwt"),
        ("VAULT_MOUNT", MOUNT),
        ("VAULT_JWT_ROLE", &id),
        ("VAULT_JWT_TOKEN_FILE", &token_path.to_string_lossy()),
    ]);

    let secret = path(&format!("{id}/deploy/db-url"));
    let version = client
        .put(&secret, &KvData::new().with("url", "postgres://x"))
        .await
        .expect("write under a JWT login");
    assert_eq!(version, KvVersion::new(1));
    assert_eq!(
        client
            .get(&secret, None)
            .await
            .expect("read")
            .data
            .get("url"),
        Some("postgres://x")
    );
    assert_eq!(client.stats().logins, 1);

    // A JWT for another subject is refused by the role's binding, so the login itself is what the
    // config selected.
    std::fs::write(
        &token_path,
        sign_jwt("system:serviceaccount:crucible:impostor", "vault"),
    )
    .expect("write");
    let other = client_from_env(&[
        ("VAULT_ADDR", &server.addr),
        ("VAULT_AUTH", "jwt"),
        ("VAULT_MOUNT", MOUNT),
        ("VAULT_JWT_ROLE", &id),
        ("VAULT_JWT_TOKEN_FILE", &token_path.to_string_lossy()),
    ]);
    let refused = other.get(&secret, None).await;
    assert!(
        matches!(refused, Err(VaultError::Denied { .. })),
        "a bound_subject mismatch is a denial, got {refused:?}"
    );
}

/// One RS/ES256 JWT for the projected-token stand-in.
fn sign_jwt(subject: &str, audience: &str) -> String {
    let _ = jsonwebtoken::crypto::aws_lc::DEFAULT_PROVIDER.install_default();
    let now = jiff::Timestamp::now().as_second();
    let claims = json!({
        "sub": subject,
        "aud": audience,
        "iat": now - 30,
        "exp": now + 600,
    });
    let key = jsonwebtoken::EncodingKey::from_ec_pem(JWT_PRIVATE_KEY.as_bytes())
        .expect("the test EC key parses");
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256),
        &claims,
        &key,
    )
    .expect("sign")
}

/// A token inside the renew threshold is renewed under the caller, with no re-login and no error
/// reaching the caller.
#[tokio::test]
async fn a_token_inside_the_renew_threshold_is_renewed_transparently() {
    let server = start!();
    let id = uniq("renew");
    server.ensure_kv_mount(MOUNT).await;
    server.write_prefix_policy(&id, MOUNT, &id).await;
    // A 60s token with a threshold just under it: the first call is comfortably outside the
    // threshold, and two seconds later the next call is inside it.
    let (role_id, secret_id) = server.approle(&id, &id, "60s", "60m").await;
    let client = approle_client(&server, &role_id, &secret_id, "59");

    let secret = path(&format!("{id}/token"));
    client
        .put(&secret, &KvData::new().with("k", "v"))
        .await
        .expect("write");
    assert_eq!(
        client.stats(),
        crucible_controller::secrets::vault::VaultStats {
            logins: 1,
            renewals: 0
        }
    );

    tokio::time::sleep(Duration::from_secs(2)).await;
    let read = client
        .get(&secret, None)
        .await
        .expect("read after the renew");
    assert_eq!(read.data.get("k"), Some("v"));
    let stats = client.stats();
    assert_eq!(stats.renewals, 1, "the call renewed the token itself");
    assert_eq!(stats.logins, 1, "a renewal is not a re-login");
}

/// A token Vault will not extend past its max TTL falls back to a fresh login instead of renewing
/// on every call.
#[tokio::test]
async fn a_token_vault_will_not_extend_is_re_minted() {
    let server = start!();
    let id = uniq("maxttl");
    server.ensure_kv_mount(MOUNT).await;
    server.write_prefix_policy(&id, MOUNT, &id).await;
    let (role_id, secret_id) = server.approle(&id, &id, "60s", "60s").await;
    // A threshold past the role's max TTL: no renewal can ever land outside it.
    let client = approle_client(&server, &role_id, &secret_id, "3600");

    let secret = path(&format!("{id}/token"));
    client
        .put(&secret, &KvData::new().with("k", "v"))
        .await
        .expect("write");
    let read = client.get(&secret, None).await.expect("read");
    assert_eq!(read.data.get("k"), Some("v"));
    let stats = client.stats();
    assert!(stats.renewals >= 1, "it tried to renew first: {stats:?}");
    assert_eq!(stats.logins, 2, "then it logged in again: {stats:?}");
}

/// A revoked token is one re-login away from working, and the caller never sees the denial.
#[tokio::test]
async fn a_revoked_token_triggers_one_relogin() {
    let server = start!();
    let id = uniq("revoked");
    server.ensure_kv_mount(MOUNT).await;
    server.write_prefix_policy(&id, MOUNT, &id).await;
    let (role_id, secret_id) = server.approle(&id, &id, "20m", "1h").await;
    let client = approle_client(&server, &role_id, &secret_id, "60");

    let secret = path(&format!("{id}/token"));
    client
        .put(&secret, &KvData::new().with("k", "v"))
        .await
        .expect("write");
    assert_eq!(client.stats().logins, 1);

    server.revoke_approle_tokens(&id).await;

    let read = client
        .get(&secret, None)
        .await
        .expect("the read survives the revocation");
    assert_eq!(read.data.get("k"), Some("v"));
    let stats = client.stats();
    assert_eq!(stats.logins, 2, "exactly one re-login: {stats:?}");
    assert_eq!(
        stats.renewals, 0,
        "a revoked token is not renewed: {stats:?}"
    );

    // And the fresh token is the one every later call uses.
    client.get(&secret, None).await.expect("read again");
    assert_eq!(client.stats().logins, 2);
}

/// A real policy denial survives the one retry and reaches the caller as 403, rather than looping
/// on logins.
#[tokio::test]
async fn a_policy_denial_is_returned_after_one_retry() {
    let server = start!();
    let id = uniq("denied");
    server.ensure_kv_mount(MOUNT).await;
    server.write_prefix_policy(&id, MOUNT, &id).await;
    let (role_id, secret_id) = server.approle(&id, &id, "20m", "1h").await;
    let client = approle_client(&server, &role_id, &secret_id, "60");

    let outside = path(&format!("{}-other/token", id));
    let denied = client.get(&outside, None).await;
    let Err(err) = denied else {
        panic!("expected a denial");
    };
    assert!(matches!(err, VaultError::Denied { .. }), "{err:?}");
    assert_eq!(err.http_status(), axum::http::StatusCode::FORBIDDEN);
    assert_eq!(
        client.stats().logins,
        2,
        "one retry behind a fresh login, then the answer stands"
    );
}

/// A missing path is 404, not an error the caller has to string-match.
#[tokio::test]
async fn a_missing_path_is_not_found() {
    let server = start!();
    let id = uniq("missing");
    server.ensure_kv_mount(MOUNT).await;
    server.write_prefix_policy(&id, MOUNT, &id).await;
    let (role_id, secret_id) = server.approle(&id, &id, "20m", "1h").await;
    let client = approle_client(&server, &role_id, &secret_id, "60");

    let err = client
        .get(&path(&format!("{id}/never-written")), None)
        .await
        .expect_err("expected a miss");
    assert!(matches!(err, VaultError::NotFound { .. }), "{err:?}");
    assert_eq!(err.http_status(), axum::http::StatusCode::NOT_FOUND);
}

/// A Vault that is not listening is unreachable, and unreachable is a 503.
#[tokio::test]
async fn an_unreachable_vault_is_a_503() {
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        port
    };
    let client = client_from_env(&[
        ("VAULT_ADDR", &format!("http://127.0.0.1:{port}")),
        ("VAULT_ROLE_ID", "rid"),
        ("VAULT_SECRET_ID", "sid"),
    ]);
    let err = client
        .get(&path("nobody/home"), None)
        .await
        .expect_err("expected a connect failure");
    assert!(matches!(err, VaultError::Unreachable { .. }), "{err:?}");
    assert_eq!(
        err.http_status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
}

/// A sealed Vault is its own failure kind: the hub is fine, the credential is fine, the operator
/// has to unseal.
#[tokio::test]
async fn a_sealed_vault_is_its_own_failure() {
    let server = start!(owned);
    let id = uniq("sealed");
    server.ensure_kv_mount(MOUNT).await;
    server.write_prefix_policy(&id, MOUNT, &id).await;
    let (role_id, secret_id) = server.approle(&id, &id, "20m", "1h").await;
    let client = approle_client(&server, &role_id, &secret_id, "60");
    let secret = path(&format!("{id}/token"));
    client
        .put(&secret, &KvData::new().with("k", "v"))
        .await
        .expect("write before the seal");

    server
        .root_call(reqwest::Method::PUT, "sys/seal", None)
        .await;

    let err = client
        .get(&secret, None)
        .await
        .expect_err("a sealed vault serves nothing");
    assert!(matches!(err, VaultError::Sealed { .. }), "{err:?}");
    assert_eq!(
        err.http_status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
}

/// Reference mode: the verification read runs on the registrant's own token, proves the key is
/// there, and leaves nothing behind — the hub never logs in, and the token is not remembered.
#[tokio::test]
async fn the_reference_verification_read_uses_the_callers_token() {
    let server = start!();
    let id = uniq("ref");
    let other_mount = "team-secrets";
    server.ensure_kv_mount(MOUNT).await;
    server.ensure_kv_mount(other_mount).await;
    let their_path = format!("{id}/ci");
    server
        .root_call(
            reqwest::Method::POST,
            &format!("{other_mount}/data/{their_path}"),
            Some(json!({"data": {"token": "theirs"}})),
        )
        .await;
    let policy = format!("{id}-registrant");
    server
        .root_call(
            reqwest::Method::PUT,
            &format!("sys/policies/acl/{policy}"),
            Some(json!({
                "policy": format!(
                    "path \"{other_mount}/data/{their_path}\" {{ capabilities = [\"read\"] }}\n"
                )
            })),
        )
        .await;
    let registrant = server.issue_token(&policy).await;

    // The hub's own AppRole covers only the crucible mount, so a successful read here can only
    // have used the registrant's token.
    let hub_policy = uniq("hub");
    server
        .write_prefix_policy(&hub_policy, MOUNT, &hub_policy)
        .await;
    let (role_id, secret_id) = server.approle(&hub_policy, &hub_policy, "20m", "1h").await;
    let client = approle_client(&server, &role_id, &secret_id, "60");

    let reference = client
        .reference(&format!("vault://{other_mount}/{their_path}#token"))
        .expect("the reference parses");
    client
        .read_reference(&registrant, &reference)
        .await
        .expect("the registrant can read their own path");
    assert_eq!(
        client.stats().logins,
        0,
        "a reference read never mints a hub token"
    );

    // A key the path does not carry is a miss, not a pass.
    let missing_key = client
        .reference(&format!("vault://{other_mount}/{their_path}#absent"))
        .expect("parse");
    let err = client
        .read_reference(&registrant, &missing_key)
        .await
        .expect_err("expected a missing key");
    assert!(matches!(err, VaultError::NotFound { .. }), "{err:?}");

    // Nothing was persisted: a token without the policy is refused on the very next call, and the
    // hub's identity is never substituted for it.
    let stranger = server.issue_token("default").await;
    let err = client
        .read_reference(&stranger, &reference)
        .await
        .expect_err("a token without the grant cannot verify");
    assert!(matches!(err, VaultError::Denied { .. }), "{err:?}");
    assert_eq!(client.stats().logins, 0, "still no hub login");

    // And the registry's own mount is refused as a reference target outright.
    assert_eq!(
        client.reference(&format!("vault://{MOUNT}/{their_path}#token")),
        Err(VaultRefError::InRegistryMount {
            mount: MOUNT.to_string()
        })
    );
}

/// The secret id arrives as a response-wrapping token in a mounted file, is unwrapped once, and the
/// result carries every later login of THIS process — a wrapping token is single use, so a re-login
/// that replayed it would strand the client. The cache is per-process and Vault's honour of the
/// token is per-token, so a second process (or a restart) reading the same file cannot log in at
/// all: that is why the chart mounts a plain secret id and offers no `wrapped` knob.
#[tokio::test]
async fn a_wrapped_secret_id_file_is_unwrapped_once_and_survives_a_relogin() {
    let server = start!();
    let id = uniq("wrapped");
    server.ensure_kv_mount(MOUNT).await;
    server.write_prefix_policy(&id, MOUNT, &id).await;
    let (role_id, _) = server.approle(&id, &id, "20m", "1h").await;
    let wrapping_token = server.wrapped_secret_id(&id).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let secret_id_file = dir.path().join("secret-id");
    std::fs::write(&secret_id_file, format!("{wrapping_token}\n")).expect("write");

    let secret_id_path = secret_id_file.to_string_lossy().into_owned();
    let env: Vec<(&str, &str)> = vec![
        ("VAULT_ADDR", &server.addr),
        ("VAULT_AUTH", "approle"),
        ("VAULT_MOUNT", MOUNT),
        ("VAULT_ROLE_ID", &role_id),
        ("VAULT_SECRET_ID_FILE", &secret_id_path),
        ("VAULT_SECRET_ID_WRAPPED", "true"),
    ];
    let client = client_from_env(&env);

    let secret = path(&format!("{id}/token"));
    client
        .put(&secret, &KvData::new().with("k", "v"))
        .await
        .expect("write behind an unwrapped secret id");
    assert_eq!(
        client.get(&secret, None).await.expect("read").data.get("k"),
        Some("v")
    );
    assert_eq!(client.stats().logins, 1);

    // The token dies; the re-login must reuse the already-unwrapped secret id rather than replay
    // the spent wrapping token.
    server.revoke_approle_tokens(&id).await;
    let read = client
        .get(&secret, None)
        .await
        .expect("the read survives the revocation");
    assert_eq!(read.data.get("k"), Some("v"));
    assert_eq!(client.stats().logins, 2, "exactly one re-login");

    // Rotation still lands: a new wrapping token in the file is unwrapped afresh.
    let rotated = server.wrapped_secret_id(&id).await;
    std::fs::write(&secret_id_file, format!("{rotated}\n")).expect("write");
    server.revoke_approle_tokens(&id).await;
    assert_eq!(
        client.get(&secret, None).await.expect("read").data.get("k"),
        Some("v")
    );
    assert_eq!(client.stats().logins, 3);

    // A wrapping token is single use: a second client pointed at the spent file cannot log in.
    let second = client_from_env(&env);
    assert!(second.get(&secret, None).await.is_err());
}

/// The unused-import guard: these are the types a caller of the registry actually needs.
#[test]
fn the_public_surface_is_nameable() {
    let _ = MountPath::parse(MOUNT).expect("mount");
    let _ = SecretIdSource::Inline(SecretId::new("sid"));
    let auth = VaultAuth::AppRole {
        mount: MountPath::parse("approle").expect("mount"),
        role_id: "rid".to_string(),
        secret_id: SecretIdSource::Inline(SecretId::new("sid")),
        wrapped: false,
    };
    assert_eq!(auth.as_str(), "approle");
}
