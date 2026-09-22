//! The hub's one Vault client.
//!
//! The hub is the only Vault client in the fleet: spokes never hold a Vault token and never reach
//! Vault, so everything Vault-shaped is here. The client logs in with the configured method
//! ([`VaultAuth::AppRole`] or [`VaultAuth::Jwt`]), keeps the resulting token renewed ahead of its
//! TTL, and speaks KV v2 on one mount — read a named version, write a new version, list versions,
//! delete.
//!
//! Two properties the rest of the registry depends on:
//!
//! * **The token is an implementation detail.** Callers never see it. It is minted on the first
//!   call, renewed transparently once it is inside [`VaultCfg::renew_threshold`], and re-minted
//!   from scratch when a renewal fails or a call comes back denied. A denied call is retried
//!   exactly once, after a fresh login; a second denial is the caller's answer.
//! * **Nothing secret is loggable.** [`VaultToken`], [`SecretId`], and [`KvData`] redact in
//!   `Debug` and have no `Display`, and an error body never carries a value the client sent.
//!
//! [`VaultClient::read_reference`] is the exception to the first property: reference-mode
//! registration verifies a path with the *registrant's own* Vault token, which is passed in, used
//! for one read, and dropped. It is never stored, never renewed, and never retried with the hub's
//! identity.

pub mod config;
#[cfg(test)]
pub(crate) mod dev;
mod error;

pub use config::{
    DEFAULT_APPROLE_MOUNT, DEFAULT_JWT_MOUNT, DEFAULT_JWT_TOKEN_PATH, DEFAULT_MOUNT,
    DEFAULT_RENEW_THRESHOLD, MountPath, PathError, SecretId, SecretIdSource, VaultAuth, VaultCfg,
    VaultPath, VaultToken,
};
pub use error::VaultError;

use anyhow::{Context, Result};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long any single Vault call may take before it counts as unreachable.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the TCP connect may take on its own.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// A KV v2 version number. Version 0 is not a version — Vault numbers from 1 — so the newtype is
/// what keeps "the current one" (`None`) apart from "version 1".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KvVersion(u64);

impl KvVersion {
    pub fn new(raw: u64) -> Self {
        KvVersion(raw)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl TryFrom<KvVersion> for i64 {
    type Error = std::num::TryFromIntError;

    fn try_from(v: KvVersion) -> Result<Self, Self::Error> {
        i64::try_from(v.0)
    }
}

impl fmt::Display for KvVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The key/value payload of one KV v2 version. `Debug` prints the keys and never the values.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct KvData(BTreeMap<String, String>);

impl KvData {
    pub fn new() -> Self {
        KvData(BTreeMap::new())
    }

    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.0.insert(key.into(), value.into());
        self
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for KvData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "KvData(keys: {:?}, values: <redacted>)",
            self.keys().collect::<Vec<_>>()
        )
    }
}

/// One read version: the payload plus the version it came from.
#[derive(Debug, Clone)]
pub struct KvSecret {
    pub data: KvData,
    pub version: KvVersion,
}

/// One entry of a path's version history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvVersionInfo {
    pub version: KvVersion,
    pub created_time: String,
    /// Soft-deleted: the version exists but reads 404 until it is undeleted.
    pub deleted: bool,
    /// Destroyed: the bytes are gone for good.
    pub destroyed: bool,
}

/// A path's version history, newest last.
#[derive(Debug, Clone)]
pub struct KvVersions {
    pub current: KvVersion,
    pub versions: Vec<KvVersionInfo>,
}

/// A registered pointer at a path outside the registry's own mount, spelled
/// `vault://<mount>/<path>#<key>`.
///
/// Only [`VaultClient::reference`] builds one, because that is where a path inside the registry's
/// own mount is refused — a reference to the crucible mount would launder registry-owned bytes
/// past the ownership check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultRef {
    mount: MountPath,
    path: VaultPath,
    key: String,
}

impl VaultRef {
    pub fn mount(&self) -> &MountPath {
        &self.mount
    }

    pub fn path(&self) -> &VaultPath {
        &self.path
    }

    pub fn key(&self) -> &str {
        &self.key
    }
}

impl fmt::Display for VaultRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "vault://{}/{}#{}", self.mount, self.path, self.key)
    }
}

/// Why a `vault://` reference was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VaultRefError {
    #[error("a vault reference is spelled vault://<mount>/<path>#<key>, not {value:?}")]
    Malformed { value: String },
    #[error("a vault reference needs a #<key>")]
    NoKey,
    #[error("{0}")]
    Path(#[from] PathError),
    #[error("paths inside the {mount} mount are refused as references")]
    InRegistryMount { mount: String },
}

/// What the client has done with its login, for tests that assert a renewal or a re-login
/// actually happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaultStats {
    pub logins: u64,
    pub renewals: u64,
}

/// The live login: the token, whether Vault will renew it, and when it dies. A `None` deadline is
/// a token with no TTL (a root token in a dev server).
#[derive(Debug)]
struct TokenState {
    token: VaultToken,
    renewable: bool,
    expires_at: Option<Instant>,
}

impl TokenState {
    fn new(token: VaultToken, renewable: bool, lease: Duration, now: Instant) -> Self {
        TokenState {
            token,
            renewable,
            expires_at: (!lease.is_zero()).then(|| now + lease),
        }
    }

    /// True once the token has less than `threshold` left. A TTL-less token never does.
    fn needs_refresh(&self, now: Instant, threshold: Duration) -> bool {
        match self.expires_at {
            None => false,
            Some(deadline) => deadline.saturating_duration_since(now) <= threshold,
        }
    }
}

/// One Vault request: everything [`VaultClient::send`] needs, so a retry replays exactly the call
/// that was denied.
struct Call<'a> {
    method: reqwest::Method,
    api_path: String,
    query: Vec<(String, String)>,
    body: Option<serde_json::Value>,
    op: &'a str,
    bad_request: error::BadRequest,
}

impl<'a> Call<'a> {
    /// A KV or metadata call: a 400 here is a request this client built wrong.
    fn kv(method: reqwest::Method, api_path: String, op: &'a str) -> Self {
        Call {
            method,
            api_path,
            query: Vec::new(),
            body: None,
            op,
            bad_request: error::BadRequest::Protocol,
        }
    }

    /// A login, renew, or unwrap: a 400 here is Vault turning the credential down.
    fn credential(api_path: String, body: serde_json::Value, op: &'a str) -> Self {
        Call {
            method: reqwest::Method::POST,
            api_path,
            query: Vec::new(),
            body: Some(body),
            op,
            bad_request: error::BadRequest::CredentialRejected,
        }
    }

    fn with_body(mut self, body: serde_json::Value) -> Self {
        self.body = Some(body);
        self
    }

    fn with_query(mut self, key: &str, value: String) -> Self {
        self.query.push((key.to_string(), value));
        self
    }
}

/// A spent wrapping token and the secret id it yielded.
struct UnwrappedSecret {
    wrapping: SecretId,
    secret_id: SecretId,
}

struct Inner {
    http: reqwest::Client,
    cfg: VaultCfg,
    /// One login shared by every caller. The lock is held across a login/renew round trip, which
    /// coalesces a stampede into a single mint.
    token: tokio::sync::Mutex<Option<TokenState>>,
    /// The result of the last unwrap, so a second login does not replay a single-use token.
    unwrapped: tokio::sync::Mutex<Option<UnwrappedSecret>>,
    logins: AtomicU64,
    renewals: AtomicU64,
}

/// The hub's Vault client. Cloning shares the login and the connection pool.
#[derive(Clone)]
pub struct VaultClient {
    inner: Arc<Inner>,
}

impl fmt::Debug for VaultClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VaultClient")
            .field("addr", &self.inner.cfg.addr)
            .field("mount", &self.inner.cfg.mount)
            .field("auth", &self.inner.cfg.auth.as_str())
            .finish()
    }
}

impl VaultClient {
    /// Build a client from the parsed config. Fails only on a CA bundle that will not load or a
    /// TLS stack that will not build; no network call happens here, so a Vault that is down at
    /// boot does not stop the controller from starting.
    pub fn new(cfg: VaultCfg) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent("crucible-controller");
        if let Some(ca) = &cfg.ca_cert {
            let pem = std::fs::read(ca)
                .with_context(|| format!("reading the Vault CA bundle at {}", ca.display()))?;
            for cert in reqwest::Certificate::from_pem_bundle(&pem)
                .with_context(|| format!("parsing the Vault CA bundle at {}", ca.display()))?
            {
                builder = builder.add_root_certificate(cert);
            }
        }
        let http = builder.build().context("building the Vault HTTP client")?;
        Ok(VaultClient {
            inner: Arc::new(Inner {
                http,
                cfg,
                token: tokio::sync::Mutex::new(None),
                unwrapped: tokio::sync::Mutex::new(None),
                logins: AtomicU64::new(0),
                renewals: AtomicU64::new(0),
            }),
        })
    }

    /// Build from the environment, or `None` when `VAULT_ADDR` is unset.
    pub fn from_env() -> Result<Option<Self>> {
        match VaultCfg::from_env().context("reading the Vault configuration")? {
            None => Ok(None),
            Some(cfg) => Ok(Some(VaultClient::new(cfg)?)),
        }
    }

    /// The mount the registry writes to.
    pub fn mount(&self) -> &MountPath {
        &self.inner.cfg.mount
    }

    pub fn stats(&self) -> VaultStats {
        VaultStats {
            logins: self.inner.logins.load(Ordering::Relaxed),
            renewals: self.inner.renewals.load(Ordering::Relaxed),
        }
    }

    // --- KV v2 -------------------------------------------------------------------------------

    /// Read a version of `path`: the current one when `version` is `None`, otherwise exactly that
    /// version. A deleted or missing version is [`VaultError::NotFound`].
    pub async fn get(
        &self,
        path: &VaultPath,
        version: Option<KvVersion>,
    ) -> Result<KvSecret, VaultError> {
        let op = format!("reading {}/{path}", self.inner.cfg.mount);
        let mut call = Call::kv(reqwest::Method::GET, self.data_path(path), &op);
        if let Some(version) = version {
            call = call.with_query("version", version.get().to_string());
        }
        let body = self.authed(&call).await?.unwrap_or_default();
        let parsed: Envelope<ReadData> = decode(&op, &body)?;
        Ok(KvSecret {
            data: KvData(parsed.data.data),
            version: KvVersion(parsed.data.metadata.version),
        })
    }

    /// Write a new version of `path`, returning the version Vault assigned.
    pub async fn put(&self, path: &VaultPath, data: &KvData) -> Result<KvVersion, VaultError> {
        let op = format!("writing {}/{path}", self.inner.cfg.mount);
        let call = Call::kv(reqwest::Method::POST, self.data_path(path), &op)
            .with_body(json!({ "data": data.0 }));
        let body = self.authed(&call).await?.unwrap_or_default();
        let parsed: Envelope<WriteMetadata> = decode(&op, &body)?;
        Ok(KvVersion(parsed.data.version))
    }

    /// The version history of `path`, oldest first.
    pub async fn versions(&self, path: &VaultPath) -> Result<KvVersions, VaultError> {
        let op = format!("listing versions of {}/{path}", self.inner.cfg.mount);
        let call = Call::kv(reqwest::Method::GET, self.metadata_path(path), &op);
        let body = self.authed(&call).await?.unwrap_or_default();
        let parsed: Envelope<MetadataData> = decode(&op, &body)?;
        let mut versions: Vec<KvVersionInfo> = parsed
            .data
            .versions
            .into_iter()
            .filter_map(|(raw, info)| {
                let version = raw.parse::<u64>().ok()?;
                Some(KvVersionInfo {
                    version: KvVersion(version),
                    created_time: info.created_time,
                    deleted: !info.deletion_time.is_empty(),
                    destroyed: info.destroyed,
                })
            })
            .collect();
        versions.sort_by_key(|v| v.version);
        Ok(KvVersions {
            current: KvVersion(parsed.data.current_version),
            versions,
        })
    }

    /// Soft-delete the named versions: they stop reading, and Vault keeps the bytes until they are
    /// destroyed. An empty list is a no-op.
    pub async fn delete_versions(
        &self,
        path: &VaultPath,
        versions: &[KvVersion],
    ) -> Result<(), VaultError> {
        if versions.is_empty() {
            return Ok(());
        }
        let op = format!("deleting versions of {}/{path}", self.inner.cfg.mount);
        let call = Call::kv(
            reqwest::Method::POST,
            format!("{}/delete/{path}", self.inner.cfg.mount),
            &op,
        )
        .with_body(json!({ "versions": versions.iter().map(|v| v.get()).collect::<Vec<_>>() }));
        self.authed(&call).await?;
        Ok(())
    }

    /// Remove the path and every version of it, irreversibly — what a registry delete does once
    /// the last binding is gone.
    pub async fn delete_all(&self, path: &VaultPath) -> Result<(), VaultError> {
        let op = format!("deleting {}/{path}", self.inner.cfg.mount);
        let call = Call::kv(reqwest::Method::DELETE, self.metadata_path(path), &op);
        self.authed(&call).await?;
        Ok(())
    }

    // --- reference mode ----------------------------------------------------------------------

    /// Parse a `vault://<mount>/<path>#<key>` reference, refusing anything inside the registry's
    /// own mount.
    pub fn reference(&self, raw: &str) -> Result<VaultRef, VaultRefError> {
        let rest = raw
            .trim()
            .strip_prefix("vault://")
            .ok_or_else(|| VaultRefError::Malformed {
                value: raw.to_string(),
            })?;
        let (location, key) = rest.rsplit_once('#').ok_or(VaultRefError::NoKey)?;
        if key.trim().is_empty() {
            return Err(VaultRefError::NoKey);
        }
        let (mount, path) = location
            .trim_start_matches('/')
            .split_once('/')
            .ok_or_else(|| VaultRefError::Malformed {
                value: raw.to_string(),
            })?;
        let mount = MountPath::parse(mount)?;
        let path = VaultPath::parse(path)?;
        if mount == self.inner.cfg.mount {
            return Err(VaultRefError::InRegistryMount {
                mount: mount.to_string(),
            });
        }
        Ok(VaultRef {
            mount,
            path,
            key: key.trim().to_string(),
        })
    }

    /// The reference-mode verification read: one KV v2 read of `reference` with the *registrant's*
    /// token, proving they can read the path and that the key is there. `Ok(None)` is a key
    /// holding something that is not a string.
    ///
    /// The token is borrowed for this call only. It is never stored, never renewed, never retried
    /// with the hub's own login, and never reaches a log line or an error body — a failure carries
    /// the path, never the credential.
    pub async fn read_reference(
        &self,
        token: &VaultToken,
        reference: &VaultRef,
    ) -> Result<Option<String>, VaultError> {
        let op = format!("verifying {reference}");
        let call = Call::kv(
            reqwest::Method::GET,
            format!("{}/data/{}", reference.mount, reference.path),
            &op,
        );
        let body = self.send(token, &call).await?.unwrap_or_default();
        // A referenced path is someone else's: its values can be any JSON, so the verification
        // read only proves the key is there, never that it is a string.
        let parsed: Envelope<AnyReadData> = decode(&op, &body)?;
        let Some(value) = parsed.data.data.get(&reference.key) else {
            return Err(VaultError::NotFound { op });
        };
        Ok(value.as_str().map(str::to_string))
    }

    /// Read a referenced path with the hub's OWN login, which is what a redemption does: the
    /// registrant's token proved they could read the path at registration and at each bind, and
    /// from then on the path owner's grant to the hub's Vault identity is the access.
    ///
    /// Only the reference's own key is returned, and only when it is a string — a credential is a
    /// string, and a structured value would otherwise be projected as its JSON encoding.
    pub async fn read_reference_as_hub(
        &self,
        reference: &VaultRef,
    ) -> Result<KvSecret, VaultError> {
        let op = format!("reading {reference}");
        let call = Call::kv(
            reqwest::Method::GET,
            format!("{}/data/{}", reference.mount, reference.path),
            &op,
        );
        let body = self.authed(&call).await?.unwrap_or_default();
        let parsed: Envelope<AnyReadData> = decode(&op, &body)?;
        let value = parsed
            .data
            .data
            .get(reference.key())
            .ok_or_else(|| VaultError::NotFound { op: op.clone() })?;
        let value = value.as_str().ok_or_else(|| VaultError::Unexpected {
            op: op.clone(),
            detail: "the referenced key is not a string".to_string(),
        })?;
        Ok(KvSecret {
            data: KvData::new().with(reference.key(), value),
            version: KvVersion(parsed.data.metadata.version),
        })
    }

    // --- plumbing ----------------------------------------------------------------------------

    fn data_path(&self, path: &VaultPath) -> String {
        format!("{}/data/{path}", self.inner.cfg.mount)
    }

    fn metadata_path(&self, path: &VaultPath) -> String {
        format!("{}/metadata/{path}", self.inner.cfg.mount)
    }

    /// Send with the hub's own login. A denial is retried exactly once behind a fresh login,
    /// because the common cause is a token Vault revoked or expired out from under us; a second
    /// denial is a policy answer and is returned.
    async fn authed(&self, call: &Call<'_>) -> Result<Option<String>, VaultError> {
        let token = self.token().await?;
        match self.send(&token, call).await {
            Err(e) if e.is_denied() => {
                let fresh = self.relogin(&token).await?;
                self.send(&fresh, call).await
            }
            other => other,
        }
    }

    /// One request. `Ok(None)` is a 204; `Ok(Some)` is the body of a 2xx.
    async fn send(
        &self,
        token: &VaultToken,
        call: &Call<'_>,
    ) -> Result<Option<String>, VaultError> {
        let url = format!("{}/v1/{}", self.inner.cfg.addr, call.api_path);
        let mut req = self
            .inner
            .http
            .request(call.method.clone(), &url)
            .header("X-Vault-Token", token.header_value())
            .header("X-Vault-Request", "true");
        if let Some(ns) = &self.inner.cfg.namespace {
            req = req.header("X-Vault-Namespace", ns);
        }
        if !call.query.is_empty() {
            req = req.query(&call.query);
        }
        if let Some(body) = &call.body {
            req = req.json(body);
        }
        let resp = req.send().await.map_err(|e| VaultError::Unreachable {
            addr: self.inner.cfg.addr.clone(),
            op: call.op.to_string(),
            detail: e.to_string(),
        })?;
        let status = resp.status();
        if status == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        let text = resp.text().await.map_err(|e| VaultError::Unreachable {
            addr: self.inner.cfg.addr.clone(),
            op: call.op.to_string(),
            detail: format!("reading the response body: {e}"),
        })?;
        if status.is_success() {
            return Ok(Some(text));
        }
        Err(error::classify(
            &self.inner.cfg.addr,
            call.op,
            status,
            &text,
            call.bad_request,
        ))
    }

    /// The current login token, renewing or re-minting as needed.
    async fn token(&self) -> Result<VaultToken, VaultError> {
        let mut guard = self.inner.token.lock().await;
        let threshold = self.inner.cfg.renew_threshold;
        if let Some(state) = guard.as_ref() {
            let now = Instant::now();
            if !state.needs_refresh(now, threshold) {
                return Ok(state.token.clone());
            }
            if state.renewable {
                match self.renew(&state.token).await {
                    // A renewal that lands the token straight back inside the threshold means
                    // Vault has stopped extending it (a max TTL); log in again rather than renew
                    // on every call from here on.
                    Ok(renewed) if !renewed.needs_refresh(Instant::now(), threshold) => {
                        let token = renewed.token.clone();
                        *guard = Some(renewed);
                        return Ok(token);
                    }
                    Ok(_) => {
                        tracing::debug!(
                            "vault: the renewed token is still inside the renew threshold; logging in again"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "vault: token renewal failed; logging in again");
                    }
                }
            }
        }
        let state = self.login().await?;
        let token = state.token.clone();
        *guard = Some(state);
        Ok(token)
    }

    /// Mint a fresh token because `stale` was denied. If another caller already replaced it, use
    /// theirs instead of logging in a second time.
    async fn relogin(&self, stale: &VaultToken) -> Result<VaultToken, VaultError> {
        let mut guard = self.inner.token.lock().await;
        if let Some(state) = guard.as_ref()
            && &state.token != stale
        {
            return Ok(state.token.clone());
        }
        let state = self.login().await?;
        let token = state.token.clone();
        *guard = Some(state);
        Ok(token)
    }

    /// `POST auth/token/renew-self`.
    async fn renew(&self, token: &VaultToken) -> Result<TokenState, VaultError> {
        let op = "renewing the vault token".to_string();
        let call = Call::credential("auth/token/renew-self".to_string(), json!({}), &op);
        let body = self.send(token, &call).await?.unwrap_or_default();
        let parsed: AuthEnvelope = decode(&op, &body)?;
        self.inner.renewals.fetch_add(1, Ordering::Relaxed);
        Ok(TokenState::new(
            VaultToken::new(parsed.auth.client_token),
            parsed.auth.renewable,
            Duration::from_secs(parsed.auth.lease_duration),
            Instant::now(),
        ))
    }

    /// Log in with the configured method.
    async fn login(&self) -> Result<TokenState, VaultError> {
        let (api_path, payload) = match &self.inner.cfg.auth {
            VaultAuth::AppRole {
                mount,
                role_id,
                secret_id,
                wrapped,
            } => {
                let secret_id = self.approle_secret_id(secret_id, *wrapped).await?;
                (
                    format!("auth/{mount}/login"),
                    json!({ "role_id": role_id, "secret_id": secret_id.expose() }),
                )
            }
            VaultAuth::Jwt {
                mount,
                role,
                token_path,
            } => {
                let jwt =
                    std::fs::read_to_string(token_path).map_err(|e| VaultError::Unexpected {
                        op: "logging in with JWT".to_string(),
                        detail: format!(
                            "reading the projected token at {}: {e}",
                            token_path.display()
                        ),
                    })?;
                (
                    format!("auth/{mount}/login"),
                    json!({ "role": role, "jwt": jwt.trim() }),
                )
            }
        };
        let op = format!("logging in with {}", self.inner.cfg.auth.as_str());
        // The login body carries the credential, so it is sent with an empty token header and its
        // failure detail comes from Vault's own error text, never from the request.
        let call = Call::credential(api_path, payload, &op);
        let body = self
            .send(&VaultToken::new(""), &call)
            .await?
            .unwrap_or_default();
        let parsed: AuthEnvelope = decode(&op, &body)?;
        self.inner.logins.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            auth = self.inner.cfg.auth.as_str(),
            lease_secs = parsed.auth.lease_duration,
            renewable = parsed.auth.renewable,
            "vault: logged in"
        );
        Ok(TokenState::new(
            VaultToken::new(parsed.auth.client_token),
            parsed.auth.renewable,
            Duration::from_secs(parsed.auth.lease_duration),
            Instant::now(),
        ))
    }

    /// The secret id to log in with. In wrapped mode the configured value is a response-wrapping
    /// token, which Vault honours exactly once: it is unwrapped on the first login and the result
    /// is reused by every later one. A wrapping token that differs from the one already spent is a
    /// rotated Secret, and is unwrapped afresh.
    async fn approle_secret_id(
        &self,
        source: &SecretIdSource,
        wrapped: bool,
    ) -> Result<SecretId, VaultError> {
        let configured = source.load().map_err(|e| VaultError::Unexpected {
            op: "logging in with AppRole".to_string(),
            detail: format!("{e:#}"),
        })?;
        if !wrapped {
            return Ok(configured);
        }
        let mut guard = self.inner.unwrapped.lock().await;
        if let Some(spent) = guard.as_ref()
            && spent.wrapping == configured
        {
            return Ok(spent.secret_id.clone());
        }
        let secret_id = self.unwrap_secret_id(&configured).await?;
        *guard = Some(UnwrappedSecret {
            wrapping: configured,
            secret_id: secret_id.clone(),
        });
        Ok(secret_id)
    }

    /// Exchange a response-wrapping token for the secret id inside it. Single use by
    /// construction: a second unwrap of the same token fails, which is how a stolen mounted
    /// Secret is detected.
    async fn unwrap_secret_id(&self, wrapping: &SecretId) -> Result<SecretId, VaultError> {
        let op = "unwrapping the AppRole secret id".to_string();
        let token = VaultToken::new(wrapping.expose());
        let call = Call::credential("sys/wrapping/unwrap".to_string(), json!({}), &op);
        let body = self.send(&token, &call).await?.unwrap_or_default();
        let parsed: Envelope<UnwrappedSecretId> = decode(&op, &body)?;
        Ok(SecretId::new(parsed.data.secret_id))
    }
}

/// Vault's `{"data": …}` wrapper.
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    data: T,
}

#[derive(Debug, Deserialize)]
struct ReadData {
    #[serde(default)]
    data: BTreeMap<String, String>,
    metadata: ReadMetadata,
}

/// A read of a path the registry does not own: values stay untyped JSON.
#[derive(Debug, Deserialize)]
struct AnyReadData {
    #[serde(default)]
    data: BTreeMap<String, serde_json::Value>,
    metadata: ReadMetadata,
}

#[derive(Debug, Deserialize)]
struct ReadMetadata {
    version: u64,
}

#[derive(Debug, Deserialize)]
struct WriteMetadata {
    version: u64,
}

#[derive(Debug, Deserialize)]
struct MetadataData {
    current_version: u64,
    #[serde(default)]
    versions: BTreeMap<String, MetadataVersion>,
}

#[derive(Debug, Deserialize)]
struct MetadataVersion {
    #[serde(default)]
    created_time: String,
    /// Empty string when the version is live; an RFC 3339 stamp once it is soft-deleted.
    #[serde(default)]
    deletion_time: String,
    #[serde(default)]
    destroyed: bool,
}

#[derive(Debug, Deserialize)]
struct UnwrappedSecretId {
    secret_id: String,
}

#[derive(Debug, Deserialize)]
struct AuthEnvelope {
    auth: AuthBlock,
}

#[derive(Debug, Deserialize)]
struct AuthBlock {
    client_token: String,
    #[serde(default)]
    lease_duration: u64,
    #[serde(default)]
    renewable: bool,
}

/// Parse a response body, turning a shape Vault has never documented into
/// [`VaultError::Unexpected`] rather than a panic.
///
/// Only the failure's kind and position survive into the error. Serde's own message quotes the
/// offending value, and the offending value in a KV read body is the secret.
fn decode<T: serde::de::DeserializeOwned>(op: &str, body: &str) -> Result<T, VaultError> {
    serde_json::from_str(body).map_err(|e| VaultError::Unexpected {
        op: op.to_string(),
        detail: format!(
            "the response did not match the documented shape ({:?} at line {}, column {})",
            e.classify(),
            e.line(),
            e.column()
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(mount: &str) -> VaultClient {
        let cfg = VaultCfg {
            addr: "https://vault.test".to_string(),
            mount: MountPath::parse(mount).expect("mount"),
            auth: VaultAuth::AppRole {
                mount: MountPath::parse("approle").expect("mount"),
                role_id: "rid".to_string(),
                secret_id: SecretIdSource::Inline(SecretId::new("sid")),
                wrapped: false,
            },
            ca_cert: None,
            namespace: None,
            renew_threshold: DEFAULT_RENEW_THRESHOLD,
        };
        VaultClient::new(cfg).expect("client")
    }

    #[test]
    fn a_reference_round_trips() {
        let c = client("crucible");
        let parsed = c
            .reference("vault://team-secrets/apps/ci#token")
            .expect("parse");
        assert_eq!(parsed.mount().as_str(), "team-secrets");
        assert_eq!(parsed.path().as_str(), "apps/ci");
        assert_eq!(parsed.key(), "token");
        assert_eq!(parsed.to_string(), "vault://team-secrets/apps/ci#token");
    }

    #[test]
    fn a_reference_into_the_registry_mount_is_refused() {
        let c = client("crucible");
        assert_eq!(
            c.reference("vault://crucible/user:wren/gh#token"),
            Err(VaultRefError::InRegistryMount {
                mount: "crucible".to_string()
            })
        );
        // The refusal is the mount's, not the spelling's: the same path on another mount parses.
        assert!(c.reference("vault://other/user:wren/gh#token").is_ok());
    }

    #[test]
    fn malformed_references_are_refused() {
        let c = client("crucible");
        for raw in [
            "team-secrets/apps/ci#token",
            "vault://team-secrets/apps/ci",
            "vault://team-secrets/apps/ci#",
            "vault://team-secrets#token",
            "vault://team-secrets/../etc#token",
            "vault://team secrets/apps#token",
        ] {
            assert!(c.reference(raw).is_err(), "expected {raw} to be refused");
        }
    }

    #[test]
    fn a_ttl_less_token_never_refreshes() {
        let now = Instant::now();
        let state = TokenState::new(VaultToken::new("root"), false, Duration::ZERO, now);
        assert!(state.expires_at.is_none());
        assert!(!state.needs_refresh(now + Duration::from_secs(86_400), DEFAULT_RENEW_THRESHOLD));
    }

    #[test]
    fn the_renew_threshold_is_a_deadline_not_a_fraction() {
        let now = Instant::now();
        let state = TokenState::new(VaultToken::new("t"), true, Duration::from_secs(600), now);
        let threshold = Duration::from_secs(300);
        assert!(!state.needs_refresh(now, threshold));
        assert!(!state.needs_refresh(now + Duration::from_secs(299), threshold));
        assert!(state.needs_refresh(now + Duration::from_secs(300), threshold));
        assert!(state.needs_refresh(now + Duration::from_secs(10_000), threshold));
    }

    #[test]
    fn kv_data_redacts_its_values() {
        let data = KvData::new().with("token", "hunter2");
        let rendered = format!("{data:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("token"), "{rendered}");
        assert_eq!(data.get("token"), Some("hunter2"));
        assert_eq!(data.len(), 1);
        assert!(!data.is_empty());
    }

    #[test]
    fn the_client_debug_carries_no_credential() {
        let rendered = format!("{:?}", client("crucible"));
        assert!(rendered.contains("vault.test"));
        assert!(!rendered.contains("sid"), "{rendered}");
    }

    #[test]
    fn a_read_body_parses_into_the_typed_shape() {
        let body = r#"{"data":{"data":{"token":"v"},"metadata":{"version":3,"destroyed":false}}}"#;
        let parsed: Envelope<ReadData> = decode("reading", body).expect("decode");
        assert_eq!(parsed.data.metadata.version, 3);
        assert_eq!(parsed.data.data.get("token").map(String::as_str), Some("v"));
        let bad: Result<Envelope<ReadData>, _> = decode("reading", r#"{"data":{}}"#);
        assert!(matches!(bad, Err(VaultError::Unexpected { .. })));
    }

    #[test]
    fn a_decode_failure_never_echoes_the_body() {
        let err = decode::<Envelope<ReadData>>("reading crucible/x", r#"{"data":"hunter2"}"#)
            .expect_err("expected a decode failure");
        assert!(!err.to_string().contains("hunter2"), "{err}");
    }
}
