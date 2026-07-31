//! The fleet file: `[clusters.<name>]` entries describing the remote spoke clusters a hub can
//! delegate work to, plus how a deploy profile folds them in. These live here (not in crucible) so
//! any consumer holding a [`crate::kube::KubeTarget`] can read the same cluster wiring without
//! pulling in the engine.
//!
//! The hub holds each spoke's credential: a named Secret carrying a scoped-SA kubeconfig under the
//! `kubeconfig` key. Reference-only, nothing here ever reads a credential's contents.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The fleet file's default name, resolved in the profile's own directory.
pub const FLEET_FILE_NAME: &str = "clusters.toml";

/// One remote spoke cluster the hub can delegate jobs to.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClusterEntry {
    /// Hub Secret holding the spoke kubeconfig (key `kubeconfig`). Reference-only.
    pub kubeconfig_secret: String,
    /// hostname -> IP, rendered as pod-spec `hostAliases` on the loop pod (the routable but
    /// DNS-dark tier; TLS stays intact because the hostname is preserved).
    #[serde(default)]
    pub host_aliases: BTreeMap<String, String>,
    /// HTTP CONNECT proxy fronting the spoke API server (the proxy-reachable tier, e.g. squid:
    /// `http://10.x.x.x:3128`). Honored by the spoke client; no tunnel involved.
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// SSH bastion for the non-routable tier. Accepted by the schema, not implemented yet:
    /// selecting a bastioned cluster is a render/check error until the tunnel lands.
    #[serde(default)]
    pub bastion: Option<BastionCfg>,
}

/// The `[clusters.<name>.bastion]` block (schema only until the tunnel is implemented).
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BastionCfg {
    pub host: String,
    pub user: String,
    /// Hub Secret holding the SSH private key. Reference-only.
    pub key_secret: String,
}

/// A spoke's reachability tier, derived from its entry (named in unreachable-spoke tool errors).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterTier {
    Public,
    DnsDark,
    Proxy,
    Bastion,
}

impl ClusterTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::DnsDark => "dns-dark",
            Self::Proxy => "proxy",
            Self::Bastion => "bastion",
        }
    }
}

impl ClusterEntry {
    pub fn tier(&self) -> ClusterTier {
        if self.bastion.is_some() {
            ClusterTier::Bastion
        } else if self.proxy_url.is_some() {
            ClusterTier::Proxy
        } else if !self.host_aliases.is_empty() {
            ClusterTier::DnsDark
        } else {
            ClusterTier::Public
        }
    }

    /// Reject an entry that would only fail later at job-submit time: an empty Secret name, or a
    /// `proxy_url` in a scheme kube's client builder can't use. `name` is the `[clusters.<name>]`
    /// key, quoted back to the operator.
    pub fn validate(&self, name: &str) -> Result<()> {
        if self.kubeconfig_secret.trim().is_empty() {
            bail!("[clusters.{name}].kubeconfig_secret must be a non-empty Secret name");
        }
        if let Some(proxy) = &self.proxy_url
            && !(proxy.starts_with("http://") || proxy.starts_with("socks5://"))
        {
            bail!("[clusters.{name}].proxy_url must be an http:// or socks5:// URL, got {proxy:?}");
        }
        Ok(())
    }
}

/// The fleet file: `[clusters.<name>]` tables shared by every profile in the directory.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct FleetFile {
    #[serde(default)]
    pub clusters: BTreeMap<String, ClusterEntry>,
}

impl FleetFile {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading fleet file {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing fleet file {}", path.display()))
    }

    /// Which fleet file applies to `profile_path`: an explicit override (which must then exist), or
    /// `clusters.toml` beside the profile when that file is present. `None` = no fleet file.
    pub fn resolve_path(
        profile_path: &Path,
        override_path: Option<&Path>,
    ) -> Result<Option<PathBuf>> {
        match override_path {
            Some(p) => {
                if !p.exists() {
                    bail!("--clusters {} does not exist", p.display());
                }
                Ok(Some(p.to_path_buf()))
            }
            None => {
                let sibling = profile_path
                    .parent()
                    .filter(|d| !d.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."))
                    .join(FLEET_FILE_NAME);
                Ok(sibling.exists().then_some(sibling))
            }
        }
    }

    /// Fold these entries into `dest`, leaving any name `dest` already has (a profile-local
    /// `[clusters.<name>]` wins over the fleet file).
    pub fn merge_into(self, dest: &mut BTreeMap<String, ClusterEntry>) {
        for (name, entry) in self.clusters {
            dest.entry(name).or_insert(entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_derives_from_the_entry_shape() {
        let public: ClusterEntry = toml::from_str("kubeconfig_secret = \"s\"").expect("parses");
        assert_eq!(public.tier(), ClusterTier::Public);
        let dark: ClusterEntry = toml::from_str(
            "kubeconfig_secret = \"s\"\nhost_aliases = { \"api.spoke.example\" = \"10.0.0.1\" }",
        )
        .expect("parses");
        assert_eq!(dark.tier(), ClusterTier::DnsDark);
        let proxied: ClusterEntry =
            toml::from_str("kubeconfig_secret = \"s\"\nproxy_url = \"http://10.0.0.1:3128\"")
                .expect("parses");
        assert_eq!(proxied.tier(), ClusterTier::Proxy);
        let bastioned: ClusterEntry = toml::from_str(
            "kubeconfig_secret = \"s\"\n[bastion]\nhost = \"jump\"\nuser = \"u\"\nkey_secret = \"k\"",
        )
        .expect("the bastion block is accepted by the schema");
        assert_eq!(bastioned.tier(), ClusterTier::Bastion);
    }

    #[test]
    fn entry_rejects_unknown_fields_empty_secret_and_bad_proxy_scheme() {
        assert!(toml::from_str::<ClusterEntry>("kubeconfig_secret = \"s\"\ntypo = 1").is_err());

        let empty: ClusterEntry = toml::from_str("kubeconfig_secret = \"\"").expect("parses");
        let err = empty.validate("gpu-east").expect_err("empty secret name");
        assert!(err.to_string().contains("non-empty"), "{err:#}");

        let schemeless: ClusterEntry =
            toml::from_str("kubeconfig_secret = \"s\"\nproxy_url = \"squid.example:3128\"")
                .expect("parses");
        let err = schemeless
            .validate("gpu-east")
            .expect_err("schemeless proxy");
        assert!(err.to_string().contains("http:// or socks5://"), "{err:#}");

        let ok: ClusterEntry =
            toml::from_str("kubeconfig_secret = \"s\"\nproxy_url = \"http://10.0.0.1:3128\"")
                .expect("parses");
        ok.validate("gpu-east").expect("valid entry");
    }

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "forge-fleet-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir tmp");
        dir
    }

    #[test]
    fn sibling_fleet_file_resolves_and_merges_without_clobbering() {
        let dir = tempdir("merge");
        std::fs::write(
            dir.join(FLEET_FILE_NAME),
            "[clusters.gpu-east]\nkubeconfig_secret = \"fleet-secret\"\n\
             [clusters.pirate]\nkubeconfig_secret = \"pirate-secret\"\n",
        )
        .expect("write fleet");
        let profile_path = dir.join("profile.toml");

        let path = FleetFile::resolve_path(&profile_path, None)
            .expect("resolves")
            .expect("sibling exists");
        let mut dest = BTreeMap::new();
        dest.insert(
            "gpu-east".to_string(),
            toml::from_str::<ClusterEntry>("kubeconfig_secret = \"local-secret\"").expect("parses"),
        );
        FleetFile::load(&path).expect("loads").merge_into(&mut dest);

        assert_eq!(
            dest.get("gpu-east").map(|c| c.kubeconfig_secret.as_str()),
            Some("local-secret"),
            "an existing entry must win over the fleet file"
        );
        assert_eq!(
            dest.get("pirate").map(|c| c.kubeconfig_secret.as_str()),
            Some("pirate-secret"),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_sibling_is_none_and_a_missing_override_is_an_error() {
        let dir = tempdir("resolve");
        let profile_path = dir.join("profile.toml");
        assert!(
            FleetFile::resolve_path(&profile_path, None)
                .expect("no sibling is fine")
                .is_none()
        );
        let err = FleetFile::resolve_path(&profile_path, Some(&dir.join("missing.toml")))
            .expect_err("an explicit fleet file must exist");
        assert!(err.to_string().contains("does not exist"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
