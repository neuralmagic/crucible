#![allow(clippy::disallowed_macros)]

//! A build context as a single-layer OCI image that the build's init container exports, and the
//! registry credential from `~/.docker/config.json` that pushes it and the built image.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use oci_client::Reference;
use oci_client::client::{Client, ClientConfig, Config, ImageLayer};
use oci_client::secrets::RegistryAuth;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The repository builds push to, and the credential for its registry.
pub struct Registry {
    pub repo: String,
    auth: RegistryAuth,
    authfile: Vec<u8>,
}

#[derive(Deserialize)]
struct DockerConfig {
    #[serde(default)]
    auths: HashMap<String, AuthEntry>,
}

#[derive(Deserialize)]
struct AuthEntry {
    auth: Option<String>,
}

impl Registry {
    /// `repo` with the inline credential `~/.docker/config.json` holds for its registry.
    pub fn load(repo: &str) -> Result<Self> {
        let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
        let path = Path::new(&home).join(".docker/config.json");
        let raw = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        Self::from_config(repo, &raw)
    }

    fn from_config(repo: &str, raw: &[u8]) -> Result<Self> {
        let registry = registry_of(repo)?;
        let config: DockerConfig = serde_json::from_slice(raw).context("parsing docker config")?;
        let auth = config
            .auths
            .get(registry)
            .and_then(|e| e.auth.clone())
            .ok_or_else(|| anyhow!("no inline auth for {registry} in ~/.docker/config.json"))?;
        use base64::Engine;
        let pair = base64::engine::general_purpose::STANDARD
            .decode(&auth)
            .context("decoding the registry credential")?;
        let pair = String::from_utf8(pair).context("the registry credential is not utf-8")?;
        let (user, pass) = pair
            .split_once(':')
            .ok_or_else(|| anyhow!("the credential for {registry} is not user:password"))?;
        let authfile =
            serde_json::to_vec(&serde_json::json!({ "auths": { registry: { "auth": auth } } }))?;
        Ok(Self {
            repo: repo.to_string(),
            auth: RegistryAuth::Basic(user.to_string(), pass.to_string()),
            authfile,
        })
    }

    /// A docker config holding only this registry's credential, for the build's pod.
    pub fn authfile(&self) -> &[u8] {
        &self.authfile
    }

    /// Pushes `dir` as `<repo>:ctx-<hash>` unless that tag exists, and returns the reference.
    pub async fn push_context(&self, dir: &Path) -> Result<String> {
        let tar = context_tar(dir)?;
        let hash = hex(&Sha256::digest(&tar));
        let ctx_ref = format!("{}:ctx-{}", self.repo, &hash[..16]);
        let reference: Reference = ctx_ref.parse().context("parsing the context reference")?;
        let client = Client::new(ClientConfig::default());
        if client
            .fetch_manifest_digest(&reference, &self.auth)
            .await
            .is_ok()
        {
            return Ok(ctx_ref);
        }
        let config = serde_json::json!({
            "architecture": "amd64",
            "os": "linux",
            "config": { "Labels": { "quay.expires-after": "2w" } },
            "rootfs": { "type": "layers", "diff_ids": [format!("sha256:{hash}")] }
        });
        client
            .push(
                &reference,
                &[ImageLayer::oci_v1(tar, None)],
                Config::oci_v1(serde_json::to_vec(&config)?, None),
                &self.auth,
                None,
            )
            .await
            .with_context(|| format!("pushing context {ctx_ref}"))?;
        Ok(ctx_ref)
    }
}

/// The registry host of a fully qualified reference.
fn registry_of(reference: &str) -> Result<&str> {
    let first = reference.split('/').next().unwrap_or_default();
    if !(first.contains('.') || first.contains(':') || first == "localhost") {
        bail!("{reference} is not fully qualified, e.g. quay.io/acme/builds");
    }
    Ok(first)
}

/// `dir` as an uncompressed tar, honoring `.dockerignore` and keeping dotfiles.
fn context_tar(dir: &Path) -> Result<Vec<u8>> {
    if !dir.is_dir() {
        bail!("context {} is not a directory", dir.display());
    }
    let mut walk = ignore::WalkBuilder::new(dir);
    walk.standard_filters(false)
        .hidden(false)
        .add_custom_ignore_filename(".dockerignore");
    let mut tar = tar::Builder::new(Vec::new());
    tar.follow_symlinks(false);
    for entry in walk.build() {
        let entry = entry.context("walking the context")?;
        if entry.depth() == 0 {
            continue;
        }
        let rel = entry.path().strip_prefix(dir)?;
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            tar.append_dir(rel, entry.path())?;
        } else {
            tar.append_path_with_name(entry.path(), rel)?;
        }
    }
    tar.into_inner().context("finishing the context tar")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use crate::context::{Registry, context_tar, registry_of};

    #[test]
    fn a_registry_takes_only_its_own_credential() {
        let raw =
            br#"{"auths":{"quay.io":{"auth":"dXNlcjpwYXNz"},"ghcr.io":{"auth":"b3RoZXI6eA=="}}}"#;
        let reg = Registry::from_config("quay.io/acme/builds", raw).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(reg.authfile()).unwrap(),
            serde_json::json!({ "auths": { "quay.io": { "auth": "dXNlcjpwYXNz" } } })
        );
        assert!(Registry::from_config("docker.io/acme/builds", raw).is_err());
        assert!(registry_of("acme/builds").is_err());
    }

    #[test]
    fn a_context_keeps_dotfiles_and_drops_what_dockerignore_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(dir.path().join(".dockerignore"), "secret.env\n").unwrap();
        std::fs::write(dir.path().join("secret.env"), "x").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref\n").unwrap();
        let tar = context_tar(dir.path()).unwrap();
        let names: Vec<String> = tar::Archive::new(tar.as_slice())
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().display().to_string())
            .collect();
        assert!(names.iter().any(|n| n == "Dockerfile"), "{names:?}");
        assert!(names.iter().any(|n| n == ".git/HEAD"), "{names:?}");
        assert!(!names.iter().any(|n| n == "secret.env"), "{names:?}");
    }
}
