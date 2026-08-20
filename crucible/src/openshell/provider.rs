//! Vertex credential for the openshell backend: crucible mints the access token with `gcp_auth`
//! and serves it via a static `google-cloud` provider credential, re-minted fresh at the top of
//! each turn (turns are minutes, a token lives ~1h), no refresh worker to race, and the
//! long-lived ADC secret never leaves the host: the sandbox only ever holds a ~1h access token.
//!
//! The token reaches claude through the provider's GCE **metadata emulator**, not an env var, so
//! the Vertex cred is never a readable sandbox env var; it rides the Create/UpdateProvider gRPC
//! request body ([`crate::openshell::grpc`]), never an argv, so there is no process-listing leak.

use anyhow::{Context, Result};

/// Vertex needs a `cloud-platform`-scoped OAuth2 access token.
const SCOPES: &[&str] = &["https://www.googleapis.com/auth/cloud-platform"];

/// The Codex CLI's public OAuth client id (OpenAI's registration for the ChatGPT sign-in flow;
/// not configurable, a different id fails the grant).
const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// The ChatGPT OAuth token endpoint the `refresh_token` grant posts to.
const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// The env carrying the host's `~/.codex/auth.json` contents (a `secretKeyRef` in the loop pod,
/// same delivery as `GCLOUD_CREDENTIALS`). It holds the refresh token, which is exactly why it
/// never leaves the host.
const CODEX_CREDENTIALS_ENV: &str = "CODEX_CREDENTIALS";

/// The provider name the sandbox attaches.
pub const PROVIDER_NAME: &str = "ci-gcp";

/// The `google-cloud` provider credential slot we set the minted access token into. The
/// gateway's metadata emulator serves this token to the sandbox's google-auth library.
pub const CRED_KEY: &str = "GCP_ADC_ACCESS_TOKEN";

/// Mint a fresh Vertex access token from Application Default Credentials.
///
/// `gcp_auth` resolves ADC from a service-account key JSON (via `GOOGLE_APPLICATION_CREDENTIALS`),
/// a user refresh-token ADC (`~/.config/gcloud/application_default_credentials.json`), or the
/// metadata server in-cluster, so the same call works on the laptop and in the pod. [`ensure_adc`]
/// does have to branch on the credential shape first: the two ADC file paths above are not
/// interchangeable, see its doc comment.
pub async fn mint_vertex_token() -> Result<String> {
    ensure_adc()?;
    let provider = gcp_auth::provider().await.context(
        "resolve GCP ADC (run `gcloud auth application-default login`, or set \
         GOOGLE_APPLICATION_CREDENTIALS to a service-account key)",
    )?;
    let token = provider
        .token(SCOPES)
        .await
        .context("mint a cloud-platform-scoped Vertex access token")?;
    Ok(token.as_str().to_string())
}

/// The openshell provider type slug for Vertex/Google Cloud. crucible mints the token itself
/// and sets it as a static credential (no `--from-gcloud-adc`, no refresh strategy): the gateway
/// serves this exact token via the metadata emulator, and crucible replaces it per turn.
pub const PROVIDER_TYPE: &str = "google-cloud";

/// Materialize ADC from the `GCLOUD_CREDENTIALS` env (how the loop pod ships the secret, a
/// `secretKeyRef` to the ADC JSON) so `gcp_auth` resolves it. No-op when the env is unset (the
/// laptop's real ADC file / metadata path is used).
///
/// The two ADC shapes resolve through different `gcp_auth` code paths, and mixing them up is a
/// hard failure, not a fallback: a `service_account` key parses fine as JSON but `gcp_auth`'s
/// well-known-path reader (`~/.config/gcloud/application_default_credentials.json`) deserializes
/// strictly into the `authorized_user` shape (`client_id`/`client_secret`/`refresh_token`), so a
/// service-account key placed there fails to parse. And a service-account key only resolves via
/// `GOOGLE_APPLICATION_CREDENTIALS` pointing at its file, `gcp_auth` errors out on that path the
/// moment the env var is set, it never falls through to the well-known path if parsing fails, so
/// `GOOGLE_APPLICATION_CREDENTIALS` must only be set when the credential is actually a
/// service-account key.
fn ensure_adc() -> Result<()> {
    let creds = match std::env::var("GCLOUD_CREDENTIALS") {
        Ok(c) if !c.trim().is_empty() => c,
        _ => return Ok(()),
    };
    let home = std::env::var("HOME").context("HOME unset; cannot place ADC")?;
    let is_service_account = serde_json::from_str::<serde_json::Value>(&creds)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str().map(str::to_string)))
        .is_some_and(|t| t == "service_account");

    let path = std::path::Path::new(&home).join(if is_service_account {
        ".config/gcloud/crucible-adc-service-account.json"
    } else {
        ".config/gcloud/application_default_credentials.json"
    });
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating ADC dir {}", parent.display()))?;
        }
        std::fs::write(&path, &creds)
            .with_context(|| format!("writing ADC to {}", path.display()))?;
    }
    if is_service_account {
        // SAFETY: `ensure_adc` runs synchronously at the top of `mint_vertex_token`, before its
        // first `.await`, so this write completes before `gcp_auth` reads the env in this call.
        unsafe { std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", &path) };
    }
    Ok(())
}

/// The short-lived material a codex sandbox is handed, via its seeded `$CODEX_HOME/auth.json`
/// ([`crate::harness::codex`]): one access token, the account it belongs to, and the id token
/// codex echoes back. No refresh token, by construction: the loop process is the single
/// refresher, so a sandbox that outlives its access token fails loudly instead of racing the
/// host for a rotation. No gateway provider is involved (provider env resolves to a placeholder
/// at the L7 proxy and codex's transport is an L4 WebSocket, so codex needs the real bytes).
///
/// This whole OAuth path exists only for the personal-ChatGPT-subscription trial; once an
/// `OPENAI_API_KEY` is available, codex takes the key directly and this module's codex half is
/// deleted, not migrated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexToken {
    pub access_token: String,
    pub account_id: String,
    pub id_token: String,
}

/// The `~/.codex/auth.json` shape, as far as crucible cares: the OAuth material under `tokens`.
#[derive(serde::Deserialize)]
struct CodexAuthFile {
    tokens: CodexAuthTokens,
}

#[derive(serde::Deserialize)]
struct CodexAuthTokens {
    refresh_token: String,
    #[serde(default)]
    account_id: String,
    #[serde(default)]
    id_token: String,
}

/// The `refresh_token` grant's response. OpenAI rotates the refresh token: the one in the
/// response replaces the one that made the grant, and the old one is dead. The mint persists the
/// rotation ([`persist_codex_state`]) or the stored material dies after one use.
#[derive(serde::Deserialize)]
struct CodexRefreshResponse {
    access_token: String,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    refresh_token: String,
}

/// Mint a short-lived ChatGPT access token for a codex turn from the host's OAuth material.
///
/// The refresh material is read from the state file ([`codex_state_path`]) when one exists, else
/// from the `CODEX_CREDENTIALS` env (the verbatim contents of a `codex login`-produced
/// `~/.codex/auth.json`). That precedence is load-bearing: each grant rotates the refresh token,
/// the rotation lands in the state file, and the env secret is seed material for the first mint
/// only, re-reading it after a rotation would present a dead token. Called at the top of each
/// turn; the loop process is the only refresher, so concurrent codex loops must not share a
/// credential.
pub async fn mint_codex_token() -> Result<CodexToken> {
    mint_codex_token_at(&codex_state_path()?, CODEX_TOKEN_URL).await
}

/// Where the rotated OAuth material lives between mints. Under `$HOME` like [`ensure_adc`]'s
/// materialized ADC: durable for the life of the pod (or the laptop), never inside a sandbox.
fn codex_state_path() -> Result<std::path::PathBuf> {
    let home = std::env::var("HOME").context("HOME unset; cannot place the codex OAuth state")?;
    Ok(std::path::Path::new(&home).join(".config/crucible/codex-credentials.json"))
}

/// [`mint_codex_token`] with the state path and token endpoint injectable for tests.
async fn mint_codex_token_at(state: &std::path::Path, token_url: &str) -> Result<CodexToken> {
    let raw = match std::fs::read_to_string(state) {
        Ok(s) => s,
        Err(_) => std::env::var(CODEX_CREDENTIALS_ENV).with_context(|| {
            format!(
                "{CODEX_CREDENTIALS_ENV} unset and no state at {}; a codex turn needs the \
                 host's ~/.codex/auth.json contents (run `codex login`, then ship the file \
                 as the secret)",
                state.display()
            )
        })?,
    };
    let auth: CodexAuthFile = serde_json::from_str(raw.trim())
        .with_context(|| format!("parsing {CODEX_CREDENTIALS_ENV} as a codex auth.json"))?;
    let (token, rotated) = refresh_codex_token(token_url, &auth).await?;
    if let Some(rotated) = rotated {
        persist_codex_state(state, &token, &rotated).with_context(|| {
            format!(
                "persisting the rotated codex refresh token to {}",
                state.display()
            )
        })?;
    }
    Ok(token)
}

/// Write the post-grant OAuth material to the state file, atomically (temp + rename) and
/// owner-only, in the auth.json shape [`mint_codex_token_at`] reads back.
fn persist_codex_state(
    state: &std::path::Path,
    token: &CodexToken,
    refresh_token: &str,
) -> Result<()> {
    let body = serde_json::json!({
        "tokens": {
            "access_token": token.access_token,
            "refresh_token": refresh_token,
            "account_id": token.account_id,
            "id_token": token.id_token,
        },
        "last_refresh": jiff::Timestamp::now().to_string(),
    })
    .to_string();
    if let Some(parent) = state.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating the state dir {}", parent.display()))?;
    }
    let tmp = state.with_extension("json.tmp");
    std::fs::write(&tmp, &body).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .with_context(|| format!("restricting {}", tmp.display()))?;
    std::fs::rename(&tmp, state)
        .with_context(|| format!("moving the state into place at {}", state.display()))?;
    Ok(())
}

/// A refresh grant the auth service rejected. `401`, and any body naming `refresh_token_expired`,
/// means the stored material is dead and the host has to `codex login` again; anything else is
/// transient.
#[derive(Debug, thiserror::Error)]
#[error("codex refresh_token grant failed ({status}): {body}")]
pub struct CodexGrantError {
    pub status: u16,
    pub body: String,
}

/// The grant itself, split out so a test can point it at a local endpoint. Returns the sandbox's
/// short-lived material plus the rotated refresh token when the response carried one.
async fn refresh_codex_token(
    token_url: &str,
    auth: &CodexAuthFile,
) -> Result<(CodexToken, Option<String>)> {
    let body = serde_json::json!({
        "client_id": CODEX_OAUTH_CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": auth.tokens.refresh_token,
    });
    let response = reqwest::Client::new()
        .post(token_url)
        .json(&body)
        .send()
        .await
        .context("posting the codex refresh_token grant")?;
    let status = response.status();
    let text = response
        .text()
        .await
        .context("reading the codex token response")?;
    if !status.is_success() {
        return Err(CodexGrantError {
            status: status.as_u16(),
            body: text,
        }
        .into());
    }
    let refreshed: CodexRefreshResponse =
        serde_json::from_str(&text).context("parsing the codex token response")?;
    let rotated = (!refreshed.refresh_token.is_empty()).then_some(refreshed.refresh_token);
    let token = CodexToken {
        access_token: refreshed.access_token,
        account_id: auth.tokens.account_id.clone(),
        id_token: if refreshed.id_token.is_empty() {
            auth.tokens.id_token.clone()
        } else {
            refreshed.id_token
        },
    };
    Ok((token, rotated))
}

/// The gateway-minted AWS provider the sandbox attaches for read-only S3 artifact reads, the
/// read half of the S3 role split. The `aws-s3` profile carries the SigV4-signed S3 egress
/// endpoints, so attaching the provider is what makes sandbox S3 requests sign at the proxy;
/// the sandbox itself never holds an AWS credential. Read-only is enforced by the assumed IAM
/// role, not the endpoint access flag.
pub const AWS_PROVIDER_NAME: &str = "crucible-s3-ro";

/// The provider profile id (`providers/aws-s3.yaml`): generic STS refresh + S3 signing endpoints.
pub const AWS_PROVIDER_TYPE: &str = "aws-s3";

/// The primary credential the STS refresh attaches to; one mint co-populates the secret key and
/// session token siblings.
pub const AWS_PRIMARY_CRED: &str = "AWS_ACCESS_KEY_ID";

/// The static provider carrying the broker's per-run bearer token. Its profile is endpointless:
/// the credential boundary comes from the sandbox policy's broker endpoint, which carries a
/// `credential_binding` naming this provider. The sandbox only ever sees the
/// `openshell:resolve:env:` placeholder; the proxy resolves it to the real token at egress, for
/// the broker endpoint alone.
pub const BROKER_PROVIDER_NAME: &str = "crucible-broker";

/// The custom (crucible-imported) profile id backing [`BROKER_PROVIDER_NAME`].
pub const BROKER_PROFILE_ID: &str = "crucible-broker";

/// The credential env key the profile declares; also the placeholder key the sandbox's
/// `.mcp.json` header resolves through.
pub const BROKER_CRED_KEY: &str = "BROKER_TOKEN";

/// The placeholder the sandbox sends in place of the broker token.
pub fn broker_token_placeholder() -> String {
    format!(
        "{}{}",
        openshell_core::secrets::PLACEHOLDER_PREFIX_PUBLIC,
        BROKER_CRED_KEY
    )
}

/// The endpointless `crucible-broker` profile: one required bearer credential, no endpoints, no
/// refresh (the token is per-run static).
pub fn broker_profile() -> openshell_core::proto::ProviderProfile {
    openshell_core::proto::ProviderProfile {
        id: BROKER_PROFILE_ID.to_string(),
        display_name: "Crucible broker".to_string(),
        description: "Per-run bearer token for the loop-pod provisioning broker".to_string(),
        category: openshell_core::proto::ProviderProfileCategory::Agent as i32,
        credentials: vec![openshell_core::proto::ProviderProfileCredential {
            name: "token".to_string(),
            description: "Broker bearer token".to_string(),
            env_vars: vec![BROKER_CRED_KEY.to_string()],
            required: true,
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_identity_constants_are_stable() {
        // The metadata emulator and any gateway-side logs key on these; keep them fixed.
        assert_eq!(PROVIDER_NAME, "ci-gcp");
        assert_eq!(PROVIDER_TYPE, "google-cloud");
        assert_eq!(CRED_KEY, "GCP_ADC_ACCESS_TOKEN");
    }

    #[test]
    fn codex_grant_identity_constants_are_stable() {
        // The upstream codex CLI's own OAuth identity; a drift here is a broken grant, not a
        // config choice.
        assert_eq!(CODEX_OAUTH_CLIENT_ID, "app_EMoamEEZ73f0CkXaXp7hrann");
        assert_eq!(CODEX_TOKEN_URL, "https://auth.openai.com/oauth/token");
    }

    /// A one-shot HTTP server that answers the next request with `status` + `body` and hands back
    /// the request it saw. Real sockets, real reqwest, no mocked client.
    async fn one_shot_token_endpoint(
        status: u16,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!(
            "http://{}/oauth/token",
            listener.local_addr().expect("addr")
        );
        let handle = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.expect("accept");
            let mut seen = Vec::new();
            let mut buf = [0u8; 4096];
            // One read is enough: reqwest writes headers and this small body together.
            let n = sock.read(&mut buf).await.expect("read");
            seen.extend_from_slice(&buf[..n]);
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(response.as_bytes()).await.expect("write");
            sock.flush().await.expect("flush");
            String::from_utf8_lossy(&seen).to_string()
        });
        (url, handle)
    }

    fn auth_file() -> CodexAuthFile {
        serde_json::from_str(
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"stale","refresh_token":"rt-abc",
                "account_id":"acct-1","id_token":"id-old"},"last_refresh":"2026-08-01T00:00:00Z"}"#,
        )
        .expect("auth.json shape")
    }

    #[tokio::test]
    async fn refresh_grant_sends_the_client_id_and_returns_only_short_lived_material() {
        let (url, server) = one_shot_token_endpoint(
            200,
            r#"{"access_token":"at-new","id_token":"id-new","refresh_token":"rt-rotated"}"#,
        )
        .await;
        let (token, rotated) = refresh_codex_token(&url, &auth_file())
            .await
            .expect("refresh");
        assert_eq!(token.access_token, "at-new");
        assert_eq!(token.id_token, "id-new");
        assert_eq!(token.account_id, "acct-1", "account rides the stored auth");
        // The rotation is handed back to the mint for persistence, never onto the token the
        // sandbox sees.
        assert_eq!(rotated.as_deref(), Some("rt-rotated"));

        let request = server.await.expect("server task");
        assert!(request.contains("POST /oauth/token"), "{request}");
        assert!(
            request.contains("\"grant_type\":\"refresh_token\""),
            "{request}"
        );
        assert!(request.contains(CODEX_OAUTH_CLIENT_ID), "{request}");
        assert!(
            request.contains("\"refresh_token\":\"rt-abc\""),
            "{request}"
        );
    }

    #[tokio::test]
    async fn a_response_without_an_id_token_falls_back_to_the_stored_one() {
        let (url, server) = one_shot_token_endpoint(200, r#"{"access_token":"at-new"}"#).await;
        let (token, rotated) = refresh_codex_token(&url, &auth_file())
            .await
            .expect("refresh");
        assert_eq!(token.id_token, "id-old");
        assert_eq!(rotated, None, "no rotation in the response, none reported");
        let _ = server.await;
    }

    #[tokio::test]
    async fn a_rejected_grant_is_an_error_carrying_the_status_and_body() {
        let (url, server) = one_shot_token_endpoint(400, r#"{"error":"invalid_grant"}"#).await;
        let err = refresh_codex_token(&url, &auth_file())
            .await
            .expect_err("400 must not mint");
        let msg = format!("{err:#}");
        assert!(msg.contains("400"), "{msg}");
        assert!(msg.contains("invalid_grant"), "{msg}");
        let _ = server.await;
    }

    /// `mint_codex_token` reads process-global env, so its tests take ENV_LOCK; a sync test with
    /// its own runtime keeps the guard off the await path.
    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(f)
    }

    /// A state path inside a fresh tempdir, so the mint's file-first read misses and the test
    /// controls the env fallback. The tempdir guard rides along to keep the dir alive.
    fn absent_state() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("codex-credentials.json");
        (dir, path)
    }

    #[test]
    fn a_missing_credentials_env_names_the_fix() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::remove_var("CODEX_CREDENTIALS") };
        let (_dir, state) = absent_state();
        let err =
            block_on(mint_codex_token_at(&state, CODEX_TOKEN_URL)).expect_err("no credentials");
        let msg = format!("{err:#}");
        assert!(msg.contains("CODEX_CREDENTIALS"), "{msg}");
        assert!(msg.contains("codex login"), "{msg}");
    }

    #[test]
    fn garbage_credentials_fail_before_any_network_call() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::set_var("CODEX_CREDENTIALS", "not json") };
        let (_dir, state) = absent_state();
        let err = block_on(mint_codex_token_at(&state, CODEX_TOKEN_URL)).expect_err("garbage");
        assert!(format!("{err:#}").contains("codex auth.json"));
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::remove_var("CODEX_CREDENTIALS") };
    }

    #[test]
    fn a_rotated_refresh_token_is_persisted_for_the_next_mint() {
        let (_dir, state) = absent_state();
        std::fs::write(
            &state,
            r#"{"tokens":{"access_token":"stale","refresh_token":"rt-abc",
                "account_id":"acct-1","id_token":"id-old"},"last_refresh":"2026-08-01T00:00:00Z"}"#,
        )
        .expect("seed state");
        block_on(async {
            let (url, server) = one_shot_token_endpoint(
                200,
                r#"{"access_token":"at-new","id_token":"id-new","refresh_token":"rt-rotated"}"#,
            )
            .await;
            let token = mint_codex_token_at(&state, &url).await.expect("mint");
            assert_eq!(token.access_token, "at-new");
            let _ = server.await;
        });
        let persisted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&state).expect("state survives"))
                .expect("state is json");
        assert_eq!(persisted["tokens"]["refresh_token"], "rt-rotated");
        assert_eq!(persisted["tokens"]["access_token"], "at-new");
        assert_eq!(persisted["tokens"]["account_id"], "acct-1");
        assert_eq!(persisted["tokens"]["id_token"], "id-new");
        assert!(persisted["last_refresh"].is_string());
        let mode =
            std::os::unix::fs::MetadataExt::mode(&std::fs::metadata(&state).expect("metadata"));
        assert_eq!(mode & 0o777, 0o600, "owner-only, it holds a live grant");
        // The next mint reads the rotation back, proving the file is its own input shape.
        block_on(async {
            let (url, server) =
                one_shot_token_endpoint(200, r#"{"access_token":"at-3","refresh_token":"rt-3"}"#)
                    .await;
            mint_codex_token_at(&state, &url).await.expect("re-mint");
            let request = server.await.expect("server task");
            assert!(
                request.contains("\"refresh_token\":\"rt-rotated\""),
                "the second grant must spend the rotated token: {request}"
            );
        });
    }

    #[test]
    fn a_grant_without_rotation_leaves_the_state_untouched() {
        let (_dir, state) = absent_state();
        let original = r#"{"tokens":{"access_token":"stale","refresh_token":"rt-abc",
                "account_id":"acct-1","id_token":"id-old"},"last_refresh":"2026-08-01T00:00:00Z"}"#;
        std::fs::write(&state, original).expect("seed state");
        block_on(async {
            let (url, server) = one_shot_token_endpoint(200, r#"{"access_token":"at-new"}"#).await;
            mint_codex_token_at(&state, &url).await.expect("mint");
            let _ = server.await;
        });
        assert_eq!(
            std::fs::read_to_string(&state).expect("state survives"),
            original,
            "nothing rotated, nothing rewritten"
        );
    }

    /// Real mint against the host's `~/.codex/auth.json`. Ignored by default (CI has no ChatGPT
    /// login); run locally with
    /// `CODEX_CREDENTIALS="$(cat ~/.codex/auth.json)" cargo test -- --ignored mints_a_real_codex_token`.
    /// The grant ROTATES the refresh token: the rotation lands in
    /// `~/.config/crucible/codex-credentials.json` (which later mints prefer), and codex's own
    /// copy in `~/.codex/auth.json` is stale from then on, so expect `codex login` to be needed
    /// again on this machine.
    #[tokio::test]
    #[ignore = "requires a codex login; run locally with --ignored"]
    async fn mints_a_real_codex_token() {
        let token = mint_codex_token()
            .await
            .expect("mint from CODEX_CREDENTIALS");
        assert!(
            token.access_token.len() > 20,
            "token looks too short: {} chars",
            token.access_token.len()
        );
        assert!(!token.access_token.contains(char::is_whitespace));
    }

    /// Real mint against the ambient ADC. Ignored by default because CI has no GCP
    /// credentials; run locally with `cargo test -- --ignored mints_a_real_token` after
    /// `gcloud auth application-default login`. No mock, this proves `gcp_auth` resolves
    /// our actual credential path and returns a usable bearer string.
    #[tokio::test]
    #[ignore = "requires GCP ADC; run locally with --ignored"]
    async fn mints_a_real_token() {
        let tok = mint_vertex_token().await.expect("mint from ADC");
        // Google OAuth2 access tokens are long opaque strings (ya29.* for user ADC).
        assert!(tok.len() > 20, "token looks too short: {} chars", tok.len());
        assert!(
            !tok.contains(char::is_whitespace),
            "token must be a single opaque string"
        );
    }

    // `ensure_adc` mutates process-global env (HOME, GOOGLE_APPLICATION_CREDENTIALS), so its
    // tests must never run concurrently with each other or with anything else touching those
    // vars in this crate.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A `service_account` key JSON (shape rotated in from the real `crucible-vertex-adc`
    /// secret) must resolve through `GOOGLE_APPLICATION_CREDENTIALS`, not the well-known
    /// `authorized_user` ADC path, that's the bug this test guards against regressing.
    #[test]
    fn service_account_creds_set_google_application_credentials() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().expect("tempdir");
        let key_json = r#"{
            "type": "service_account",
            "project_id": "p",
            "private_key_id": "k",
            "private_key": "-----BEGIN PRIVATE KEY-----\nfake\n-----END PRIVATE KEY-----\n",
            "client_email": "sa@p.iam.gserviceaccount.com",
            "client_id": "1",
            "token_uri": "https://oauth2.googleapis.com/token"
        }"#;

        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::set_var("HOME", home.path());
            std::env::set_var("GCLOUD_CREDENTIALS", key_json);
            std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS");
        }
        ensure_adc().expect("ensure_adc");

        let expected = home
            .path()
            .join(".config/gcloud/crucible-adc-service-account.json");
        assert_eq!(
            std::env::var("GOOGLE_APPLICATION_CREDENTIALS").as_deref(),
            Ok(expected.to_str().unwrap()),
            "service_account creds must point GOOGLE_APPLICATION_CREDENTIALS at their file"
        );
        assert_eq!(
            std::fs::read_to_string(&expected).expect("adc file written"),
            key_json
        );
        // The well-known authorized_user path must be left untouched, a stray file there
        // would otherwise get picked up as a (broken) ConfigDefaultCredentials source.
        assert!(
            !home
                .path()
                .join(".config/gcloud/application_default_credentials.json")
                .exists()
        );

        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::remove_var("GCLOUD_CREDENTIALS");
            std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS");
        }
    }

    /// The legacy `authorized_user` (personal OAuth) shape must keep resolving through the
    /// well-known ADC path exactly as before, with `GOOGLE_APPLICATION_CREDENTIALS` left unset.
    #[test]
    fn authorized_user_creds_use_the_well_known_path_only() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().expect("tempdir");
        let user_json = r#"{
            "type": "authorized_user",
            "client_id": "id",
            "client_secret": "secret",
            "refresh_token": "token"
        }"#;

        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::set_var("HOME", home.path());
            std::env::set_var("GCLOUD_CREDENTIALS", user_json);
            std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS");
        }
        ensure_adc().expect("ensure_adc");

        assert!(
            std::env::var("GOOGLE_APPLICATION_CREDENTIALS").is_err(),
            "authorized_user creds must not set GOOGLE_APPLICATION_CREDENTIALS"
        );
        let expected = home
            .path()
            .join(".config/gcloud/application_default_credentials.json");
        assert_eq!(
            std::fs::read_to_string(&expected).expect("adc file written"),
            user_json
        );

        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::remove_var("GCLOUD_CREDENTIALS");
        }
    }
}
