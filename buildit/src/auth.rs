#![allow(clippy::disallowed_macros)]

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct DockerConfig {
    #[serde(default)]
    auths: HashMap<String, AuthEntry>,
}

#[derive(Deserialize, Serialize, Clone)]
struct AuthEntry {
    auth: Option<String>,
}

// refs must be fully qualified so we know whose credential to ship
pub fn registry_of(image: &str) -> Result<String> {
    let first = image
        .split('/')
        .next()
        .ok_or_else(|| anyhow!("empty image ref"))?;
    if !(first.contains('.') || first.contains(':') || first == "localhost") {
        bail!(
            "image ref {image} is not fully qualified; use e.g. quay.io/acme/foo:tag \
             so buildit knows which registry credential to ship"
        );
    }
    Ok(first.to_string())
}

fn docker_config_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".docker/config.json"))
}

// ship ONLY the destination registry's token, never the whole docker config
pub fn minimal_authfile(registry: &str) -> Result<Vec<u8>> {
    let path = docker_config_path()?;
    let raw = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    minimal_authfile_from(&raw, registry)
}

fn inline_auth(raw: &[u8], registry: &str) -> Result<String> {
    let cfg: DockerConfig = serde_json::from_slice(raw).context("parsing docker config")?;
    // docker.io creds hide under the legacy index URL
    let candidates: &[&str] = if registry == "docker.io" || registry == "index.docker.io" {
        &[registry, "https://index.docker.io/v1/"]
    } else {
        &[registry]
    };
    candidates
        .iter()
        .find_map(|key| cfg.auths.get(*key).and_then(|e| e.auth.clone()))
        .ok_or_else(|| {
            anyhow!(
                "no inline auth for {registry} in ~/.docker/config.json \
                 (run `docker login {registry}`; a macOS credsStore won't write an inline token)"
            )
        })
}

fn minimal_authfile_from(raw: &[u8], registry: &str) -> Result<Vec<u8>> {
    let auth = inline_auth(raw, registry)?;
    let minimal = serde_json::json!({ "auths": { registry: { "auth": auth } } });
    serde_json::to_vec(&minimal).context("serializing authfile")
}

// the base64 auth entry decoded back into (user, pass) for oci-client
pub fn basic_credentials(registry: &str) -> Result<(String, String)> {
    let path = docker_config_path()?;
    let raw = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    basic_credentials_from(&raw, registry)
}

fn basic_credentials_from(raw: &[u8], registry: &str) -> Result<(String, String)> {
    use base64::Engine;
    let auth = inline_auth(raw, registry)?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(auth)
        .context("decoding auth entry")?;
    let decoded = String::from_utf8(decoded).context("auth entry is not utf-8")?;
    let (user, pass) = decoded
        .split_once(':')
        .ok_or_else(|| anyhow!("auth entry for {registry} is not user:pass shaped"))?;
    Ok((user.to_string(), pass.to_string()))
}

#[cfg(test)]
mod tests {
    use crate::auth::{minimal_authfile_from, registry_of};

    #[test]
    fn registry_extraction() {
        assert_eq!(registry_of("quay.io/acme/foo:tag").unwrap(), "quay.io");
        assert_eq!(registry_of("localhost:5000/foo").unwrap(), "localhost:5000");
        assert_eq!(registry_of("localhost/foo").unwrap(), "localhost");
        assert!(registry_of("acme/foo:tag").is_err());
        assert!(registry_of("ubuntu").is_err());
    }

    #[test]
    fn authfile_contains_only_target_registry() {
        let cfg = br#"{"auths":{
            "quay.io":{"auth":"cXVheXNlY3JldA=="},
            "ghcr.io":{"auth":"Z2hzZWNyZXQ="}
        }}"#;
        let out = minimal_authfile_from(cfg, "quay.io").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["auths"]["quay.io"]["auth"], "cXVheXNlY3JldA==");
        assert!(v["auths"].get("ghcr.io").is_none());
    }

    #[test]
    fn dockerhub_legacy_key_fallback() {
        let cfg = br#"{"auths":{"https://index.docker.io/v1/":{"auth":"aHVi"}}}"#;
        let out = minimal_authfile_from(cfg, "docker.io").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["auths"]["docker.io"]["auth"], "aHVi");
    }

    #[test]
    fn basic_credentials_decode() {
        // dXNlcjpwYTpzcw== = "user:pa:ss" (password may contain colons)
        let cfg = br#"{"auths":{"quay.io":{"auth":"dXNlcjpwYTpzcw=="}}}"#;
        let (user, pass) = crate::auth::basic_credentials_from(cfg, "quay.io").unwrap();
        assert_eq!(user, "user");
        assert_eq!(pass, "pa:ss");
    }

    #[test]
    fn missing_inline_auth_is_a_clear_error() {
        let cfg = br#"{"auths":{"quay.io":{}},"credsStore":"osxkeychain"}"#;
        let err = minimal_authfile_from(cfg, "quay.io")
            .unwrap_err()
            .to_string();
        assert!(err.contains("docker login quay.io"), "got: {err}");
    }
}
