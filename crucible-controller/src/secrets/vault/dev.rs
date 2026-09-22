//! A real Vault for the in-crate tests that need one — the secrets registry's routes write bytes,
//! and a registry test that did not is testing nothing.
//!
//! The server comes from one of two places, exactly as `tests/vault_e2e.rs` finds it: CI exports
//! `VAULT_ADDR` plus a root `VAULT_TOKEN`, and a laptop with the `vault` binary on PATH gets a dev
//! server spawned here. With neither, [`DevVault::start`] returns `None` and the caller skips —
//! unless `CRUCIBLE_REQUIRE_VAULT_TESTS` is set, which turns the skip into a failure so CI cannot
//! go green by silently skipping.
//!
//! Test-only: this module is `#[cfg(test)]`, so nothing here ships.

use crate::secrets::vault::{VaultCfg, VaultClient};
use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// Distinct names per test, so a shared CI server never has two tests on one mount or role.
pub(crate) fn uniq(tag: &str) -> String {
    let n = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("{tag}-{}-{n}", std::process::id())
}

/// How many dev servers this binary runs at once: a dozen Vaults booting together is what makes a
/// health poll time out.
static SERVER_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

/// A reachable Vault with a root token. Owns the process when it spawned one.
pub(crate) struct DevVault {
    addr: String,
    root: String,
    http: reqwest::Client,
    child: Option<Child>,
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

fn required() -> bool {
    std::env::var("CRUCIBLE_REQUIRE_VAULT_TESTS")
        .map(|v| !v.trim().is_empty() && v != "0")
        .unwrap_or(false)
}

impl DevVault {
    /// The server this test will use, or `None` when there is none to be had.
    pub(crate) async fn start() -> Option<Self> {
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
        let slot = SERVER_SLOTS
            .acquire()
            .await
            .expect("the slot semaphore is never closed");
        let mut failures = Vec::new();
        for _ in 0..3 {
            match Self::spawn_once().await {
                Ok(Some(mut server)) => {
                    server._slot = Some(slot);
                    return Some(server);
                }
                Ok(None) => return None,
                Err(why) => failures.push(why),
            }
        }
        panic!("no dev vault would start: {}", failures.join(" | "));
    }

    /// One spawn attempt. `Ok(None)` means there is no `vault` binary at all.
    async fn spawn_once() -> Result<Option<Self>, String> {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a scratch port");
            let port = listener.local_addr().expect("scratch port").port();
            drop(listener);
            port
        };
        let root = format!("root-{}", uuid::Uuid::now_v7());
        let log = tempfile::NamedTempFile::new().expect("a log file");
        let errors = log.reopen().expect("reopen the log");
        let bin = std::env::var("VAULT_BIN").unwrap_or_else(|_| "vault".to_string());
        let mut child = match Command::new(bin)
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
                    "the dev vault on {port} exited with {status}: {why}"
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = child.kill();
        let _ = child.wait();
        Err(format!("the dev vault on {port} never became healthy"))
    }

    /// One root-token API call. A provisioning failure is a broken test, so this asserts.
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
    pub(crate) async fn ensure_kv_mount(&self, mount: &str) {
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

    /// Write one KV v2 version with the root token — the stand-in for a path someone else owns.
    pub(crate) async fn put(&self, mount: &str, path: &str, key: &str, value: &str) {
        self.root_call(
            reqwest::Method::POST,
            &format!("{mount}/data/{path}"),
            Some(json!({ "data": { key: value } })),
        )
        .await;
    }

    /// Read one KV v2 version back with the root token, so a test can prove what the hub wrote.
    pub(crate) async fn get(&self, mount: &str, path: &str, key: &str) -> Option<String> {
        let resp = self
            .http
            .get(format!("{}/v1/{mount}/data/{path}", self.addr))
            .header("X-Vault-Token", &self.root)
            .send()
            .await
            .expect("read request");
        if !resp.status().is_success() {
            return None;
        }
        let body: Value = resp.json().await.expect("read body");
        body["data"]["data"][key].as_str().map(str::to_string)
    }

    /// A policy granting everything under one mount.
    pub(crate) async fn write_mount_policy(&self, name: &str, mount: &str) {
        let policy = format!(
            "path \"{mount}/*\" {{ capabilities = [\"create\", \"read\", \"update\", \"delete\", \"list\"] }}\n"
        );
        self.root_call(
            reqwest::Method::PUT,
            &format!("sys/policies/acl/{name}"),
            Some(json!({ "policy": policy })),
        )
        .await;
    }

    /// An AppRole role plus a fresh role id / secret id pair.
    async fn approle(&self, role: &str, policy: &str) -> (String, String) {
        let resp = self
            .http
            .post(format!("{}/v1/sys/auth/approle", self.addr))
            .header("X-Vault-Token", &self.root)
            .json(&json!({"type": "approle"}))
            .send()
            .await
            .expect("auth request");
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        assert!(
            status.is_success() || text.contains("already in use"),
            "enabling approle: {status}: {text}"
        );
        self.root_call(
            reqwest::Method::POST,
            &format!("auth/approle/role/{role}"),
            Some(json!({
                "token_policies": policy,
                "token_ttl": "20m",
                "token_max_ttl": "60m",
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

    /// A child token carrying exactly `policies` — the stand-in for a registrant's own token.
    pub(crate) async fn issue_token(&self, policies: &[&str]) -> String {
        let created = self
            .root_call(
                reqwest::Method::POST,
                "auth/token/create",
                Some(json!({"policies": policies, "ttl": "10m", "no_parent": true})),
            )
            .await;
        created["auth"]["client_token"]
            .as_str()
            .expect("client_token")
            .to_string()
    }

    /// The hub's own client, logged in by AppRole against `mount`, with a policy over the whole
    /// mount — what the chart's Vault role grants the deployed controller.
    pub(crate) async fn hub_client(&self, mount: &str) -> VaultClient {
        self.ensure_kv_mount(mount).await;
        let policy = uniq("hub-policy");
        self.write_mount_policy(&policy, mount).await;
        let (role_id, secret_id) = self.approle(uniq("hub-role").as_str(), &policy).await;
        let pairs = [
            ("VAULT_ADDR", self.addr.as_str()),
            ("VAULT_AUTH", "approle"),
            ("VAULT_MOUNT", mount),
            ("VAULT_ROLE_ID", role_id.as_str()),
            ("VAULT_SECRET_ID", secret_id.as_str()),
        ];
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

    /// A token that can read nothing at all, for the refusal half of a verification read.
    pub(crate) async fn powerless_token(&self) -> String {
        self.issue_token(&["default"]).await
    }
}
