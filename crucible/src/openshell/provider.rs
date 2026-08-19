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
