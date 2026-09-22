//! Reading a registry for the catalog: tag listing, tag resolution and per-digest description.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use oci_client::Reference;
use oci_client::manifest::OciManifest;

const READ_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct RegistryError(pub String);

/// What one digest in a repository is: its architectures, config timestamp and labels.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DigestDescription {
    pub arches: Vec<String>,
    pub created_at: Option<String>,
    pub labels: BTreeMap<String, String>,
}

#[async_trait::async_trait]
pub trait RegistryReader: Send + Sync {
    /// The repositories a `registry/owner/<glob>` pattern names right now.
    async fn discover(&self, pattern: &str) -> Result<Vec<String>, RegistryError>;
    /// Every tag in the repository.
    async fn tags(&self, repository: &str) -> Result<Vec<String>, RegistryError>;
    /// The digest a tag points at right now.
    async fn resolve(&self, repository: &str, tag: &str) -> Result<String, RegistryError>;
    /// The description of one digest.
    async fn describe(
        &self,
        repository: &str,
        digest: &str,
    ) -> Result<DigestDescription, RegistryError>;
}

/// Is this catalog entry a pattern to expand rather than one repository?
pub fn is_pattern(entry: &str) -> bool {
    entry.contains('*')
}

/// Split `registry/owner/<glob>` into its parts.
fn split_pattern(pattern: &str) -> Result<(&str, &str, &str), RegistryError> {
    let mut parts = pattern.splitn(3, '/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(registry), Some(owner), Some(glob)) if !glob.contains('/') && !glob.is_empty() => {
            Ok((registry, owner, glob))
        }
        _ => Err(RegistryError(format!(
            "{pattern}: a catalog pattern is registry/owner/<glob>"
        ))),
    }
}

/// `*` matches any run of characters; everything else is literal.
pub fn glob_matches(glob: &str, name: &str) -> bool {
    fn go(g: &[u8], n: &[u8]) -> bool {
        match (g.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => go(&g[1..], n) || (!n.is_empty() && go(g, &n[1..])),
            (Some(c), Some(d)) if c == d => go(&g[1..], &n[1..]),
            _ => false,
        }
    }
    go(glob.as_bytes(), name.as_bytes())
}

/// Is this tag one a human would pick? The feedstock also pushes per-arch tags, the buildx
/// cache, and cosign's signature and attestation tags; none of those are channels.
pub fn is_channel_tag(tag: &str) -> bool {
    !(tag.starts_with("sha256-")
        || tag.starts_with("buildcache-")
        || tag.ends_with("-amd64")
        || tag.ends_with("-arm64"))
}

/// The production reader: OCI distribution calls through the same authfile the contract check
/// and the render's digest pinning use.
pub struct LiveRegistryReader {
    authfile: Option<PathBuf>,
}

impl LiveRegistryReader {
    pub fn new(authfile: Option<PathBuf>) -> Self {
        Self { authfile }
    }

    fn client(&self) -> oci_client::Client {
        use oci_client::client::{ClientConfig, linux_amd64_resolver};
        oci_client::Client::new(ClientConfig {
            platform_resolver: Some(Box::new(linux_amd64_resolver)),
            connect_timeout: Some(READ_TIMEOUT),
            read_timeout: Some(READ_TIMEOUT),
            ..ClientConfig::default()
        })
    }

    fn auth(
        &self,
        reference: &Reference,
    ) -> Result<oci_client::secrets::RegistryAuth, RegistryError> {
        forge::oci::resolve_auth(self.authfile.as_deref(), reference.registry()).map_err(|e| {
            RegistryError(format!(
                "resolving registry auth for {}: {e:#}",
                reference.whole()
            ))
        })
    }
}

fn parse(reference: &str) -> Result<Reference, RegistryError> {
    reference
        .parse()
        .map_err(|e| RegistryError(format!("parsing image ref {reference}: {e}")))
}

fn config_arch(config: &serde_json::Value) -> Option<String> {
    config
        .get("architecture")
        .and_then(|a| a.as_str())
        .map(str::to_string)
}

#[derive(serde::Deserialize)]
struct GithubPackage {
    name: String,
}

#[async_trait::async_trait]
impl RegistryReader for LiveRegistryReader {
    /// GHCR has no `_catalog`; the owner's container packages come from the GitHub API, read
    /// with the authfile's ghcr.io credential (a token with `read:packages`).
    async fn discover(&self, pattern: &str) -> Result<Vec<String>, RegistryError> {
        let (registry, owner, glob) = split_pattern(pattern)?;
        if registry != "ghcr.io" {
            return Err(RegistryError(format!(
                "{pattern}: pattern discovery is only supported on ghcr.io"
            )));
        }
        let reference = parse(&format!("{registry}/{owner}/{glob}"))
            .or_else(|_| parse(&format!("{registry}/{owner}/x")))?;
        let token = match self.auth(&reference)? {
            oci_client::secrets::RegistryAuth::Basic(_, password) => password,
            _ => {
                return Err(RegistryError(format!(
                    "{pattern}: discovering ghcr.io packages needs a ghcr.io login in the authfile"
                )));
            }
        };
        let http = reqwest::Client::builder()
            .timeout(READ_TIMEOUT)
            .user_agent("crucible-controller")
            .build()
            .map_err(|e| RegistryError(format!("building the GitHub client: {e}")))?;
        let mut names = Vec::new();
        for kind in ["orgs", "users"] {
            names.clear();
            let mut page = 1;
            let mut found = true;
            loop {
                let url = format!(
                    "https://api.github.com/{kind}/{owner}/packages?package_type=container&per_page=100&page={page}"
                );
                let res = http
                    .get(&url)
                    .bearer_auth(&token)
                    .header("Accept", "application/vnd.github+json")
                    .send()
                    .await
                    .map_err(|e| RegistryError(format!("listing {owner}'s packages: {e}")))?;
                if res.status() == reqwest::StatusCode::NOT_FOUND && page == 1 {
                    found = false;
                    break;
                }
                if !res.status().is_success() {
                    return Err(RegistryError(format!(
                        "listing {owner}'s packages: GitHub answered {}",
                        res.status()
                    )));
                }
                let packages: Vec<GithubPackage> = res
                    .json()
                    .await
                    .map_err(|e| RegistryError(format!("decoding {owner}'s packages: {e}")))?;
                let n = packages.len();
                names.extend(packages.into_iter().map(|p| p.name));
                if n < 100 {
                    break;
                }
                page += 1;
            }
            if found {
                break;
            }
        }
        let mut repos: Vec<String> = names
            .into_iter()
            .filter(|n| glob_matches(glob, n))
            .map(|n| format!("{registry}/{owner}/{n}"))
            .collect();
        repos.sort();
        Ok(repos)
    }

    async fn tags(&self, repository: &str) -> Result<Vec<String>, RegistryError> {
        let reference = parse(repository)?;
        let auth = self.auth(&reference)?;
        let client = self.client();
        let mut tags = Vec::new();
        let mut last: Option<String> = None;
        loop {
            let page = client
                .list_tags(&reference, &auth, Some(200), last.as_deref())
                .await
                .map_err(|e| RegistryError(format!("listing tags of {repository}: {e}")))?;
            let n = page.tags.len();
            tags.extend(page.tags);
            if n < 200 {
                break;
            }
            last = tags.last().cloned();
        }
        tags.sort();
        tags.dedup();
        Ok(tags)
    }

    async fn resolve(&self, repository: &str, tag: &str) -> Result<String, RegistryError> {
        let reference = parse(&format!("{repository}:{tag}"))?;
        let auth = self.auth(&reference)?;
        self.client()
            .fetch_manifest_digest(&reference, &auth)
            .await
            .map_err(|e| RegistryError(format!("resolving {repository}:{tag}: {e}")))
    }

    async fn describe(
        &self,
        repository: &str,
        digest: &str,
    ) -> Result<DigestDescription, RegistryError> {
        let reference = parse(&format!("{repository}@{digest}"))?;
        let auth = self.auth(&reference)?;
        let client = self.client();
        let (manifest, _) = client.pull_manifest(&reference, &auth).await.map_err(|e| {
            RegistryError(format!(
                "reading the manifest of {repository}@{digest}: {e}"
            ))
        })?;
        let (mut arches, config_ref) = match manifest {
            OciManifest::ImageIndex(index) => {
                let mut arches: Vec<String> = index
                    .manifests
                    .iter()
                    .filter_map(|m| m.platform.as_ref())
                    .filter(|p| {
                        p.os.to_string() == "linux" && p.architecture.to_string() != "unknown"
                    })
                    .map(|p| p.architecture.to_string())
                    .collect();
                arches.sort();
                arches.dedup();
                let entry = index
                    .manifests
                    .iter()
                    .find(|m| {
                        m.platform
                            .as_ref()
                            .is_some_and(|p| p.architecture.to_string() == "amd64")
                    })
                    .or_else(|| {
                        index.manifests.iter().find(|m| {
                            m.platform
                                .as_ref()
                                .is_some_and(|p| p.architecture.to_string() != "unknown")
                        })
                    })
                    .ok_or_else(|| {
                        RegistryError(format!(
                            "{repository}@{digest}: index has no platform manifest"
                        ))
                    })?;
                (arches, reference.clone_with_digest(entry.digest.clone()))
            }
            OciManifest::Image(_) => (Vec::new(), reference.clone()),
        };
        let (_, _, config) = client
            .pull_manifest_and_config(&config_ref, &auth)
            .await
            .map_err(|e| {
                RegistryError(format!(
                    "reading the OCI config of {repository}@{digest}: {e}"
                ))
            })?;
        let config: serde_json::Value = serde_json::from_str(&config).map_err(|e| {
            RegistryError(format!(
                "decoding the OCI config of {repository}@{digest}: {e}"
            ))
        })?;
        if arches.is_empty() {
            arches.extend(config_arch(&config));
        }
        let labels = config
            .get("config")
            .and_then(|c| c.get("Labels"))
            .and_then(|l| l.as_object())
            .map(|l| {
                l.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        Ok(DigestDescription {
            arches,
            created_at: config
                .get("created")
                .and_then(|c| c.as_str())
                .map(str::to_string),
            labels,
        })
    }
}

/// An in-memory registry for tests: repositories of tag -> digest, digests of descriptions, and
/// a counter of `describe` calls so a test can assert the per-digest cache held.
#[cfg(test)]
type TagTable = BTreeMap<String, Result<BTreeMap<String, String>, RegistryError>>;

#[cfg(test)]
pub(crate) struct TableRegistry {
    pub(crate) tags: std::sync::Mutex<TagTable>,
    pub(crate) digests: std::sync::Mutex<BTreeMap<String, DigestDescription>>,
    pub(crate) describes: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl TableRegistry {
    pub(crate) fn new() -> Self {
        Self {
            tags: Default::default(),
            digests: Default::default(),
            describes: Default::default(),
        }
    }

    pub(crate) fn tag(&self, repository: &str, tag: &str, digest: &str) {
        let mut tags = self.tags.lock().unwrap();
        let entry = tags
            .entry(repository.to_string())
            .or_insert_with(|| Ok(BTreeMap::new()));
        if let Ok(map) = entry {
            map.insert(tag.to_string(), digest.to_string());
        }
    }

    pub(crate) fn untag(&self, repository: &str, tag: &str) {
        if let Some(Ok(map)) = self.tags.lock().unwrap().get_mut(repository) {
            map.remove(tag);
        }
    }

    pub(crate) fn fail(&self, repository: &str, error: &str) {
        self.tags.lock().unwrap().insert(
            repository.to_string(),
            Err(RegistryError(error.to_string())),
        );
    }

    pub(crate) fn digest(&self, digest: &str, description: DigestDescription) {
        self.digests
            .lock()
            .unwrap()
            .insert(digest.to_string(), description);
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl RegistryReader for TableRegistry {
    async fn discover(&self, pattern: &str) -> Result<Vec<String>, RegistryError> {
        let (registry, owner, glob) = split_pattern(pattern)?;
        let prefix = format!("{registry}/{owner}/");
        let tags = self.tags.lock().unwrap();
        if let Some(Err(e)) = tags.get(pattern) {
            return Err(e.clone());
        }
        Ok(tags
            .keys()
            .filter(|r| {
                r.strip_prefix(&prefix)
                    .is_some_and(|n| glob_matches(glob, n))
            })
            .cloned()
            .collect())
    }

    async fn tags(&self, repository: &str) -> Result<Vec<String>, RegistryError> {
        match self.tags.lock().unwrap().get(repository) {
            Some(Ok(map)) => Ok(map.keys().cloned().collect()),
            Some(Err(e)) => Err(e.clone()),
            None => Err(RegistryError(format!("{repository}: unknown repository"))),
        }
    }

    async fn resolve(&self, repository: &str, tag: &str) -> Result<String, RegistryError> {
        match self.tags.lock().unwrap().get(repository) {
            Some(Ok(map)) => map
                .get(tag)
                .cloned()
                .ok_or_else(|| RegistryError(format!("{repository}:{tag}: unknown tag"))),
            Some(Err(e)) => Err(e.clone()),
            None => Err(RegistryError(format!("{repository}: unknown repository"))),
        }
    }

    async fn describe(
        &self,
        repository: &str,
        digest: &str,
    ) -> Result<DigestDescription, RegistryError> {
        self.describes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.digests
            .lock()
            .unwrap()
            .get(digest)
            .cloned()
            .ok_or_else(|| RegistryError(format!("{repository}@{digest}: unknown digest")))
    }
}

#[cfg(test)]
mod tests {
    use crate::images::registry::{glob_matches, is_channel_tag, is_pattern, split_pattern};

    #[test]
    fn globs_match_on_the_repository_name() {
        assert!(glob_matches("sandbox-*", "sandbox-go-cc"));
        assert!(glob_matches("sandbox-*-cc", "sandbox-vllm-0.28-cc"));
        assert!(!glob_matches("sandbox-*-cc", "sandbox-go-codex"));
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("loop-base", "loop-base"));
        assert!(!glob_matches("loop-base", "loop-base-x"));
    }

    #[test]
    fn patterns_split_into_registry_owner_and_glob() {
        assert!(is_pattern("ghcr.io/acme/sandbox-*"));
        assert!(!is_pattern("ghcr.io/acme/sandbox-go-cc"));
        assert_eq!(
            split_pattern("ghcr.io/acme/sandbox-*").unwrap(),
            ("ghcr.io", "acme", "sandbox-*")
        );
        assert!(split_pattern("ghcr.io/sandbox-*").is_err());
        assert!(split_pattern("ghcr.io/acme/deep/sandbox-*").is_err());
    }

    #[test]
    fn channel_tags_exclude_feedstock_plumbing() {
        assert!(is_channel_tag("latest"));
        assert!(is_channel_tag("fd2438cd09d2ac0bd3d467e8ad25d511f06f9a4e"));
        assert!(is_channel_tag("v1.2.3"));
        assert!(!is_channel_tag(
            "fd2438cd09d2ac0bd3d467e8ad25d511f06f9a4e-amd64"
        ));
        assert!(!is_channel_tag(
            "fd2438cd09d2ac0bd3d467e8ad25d511f06f9a4e-arm64"
        ));
        assert!(!is_channel_tag("buildcache-amd64"));
        assert!(!is_channel_tag("sha256-9fdff3d4.sig"));
        assert!(!is_channel_tag("sha256-9fdff3d4.att"));
    }
}

#[cfg(test)]
mod live_tests {
    use crate::images::registry::{LiveRegistryReader, RegistryReader, is_channel_tag};

    /// Reads a real feedstock image off GHCR with the process's own registry login.
    /// `cargo test -p crucible-controller --lib -- --ignored images::registry::live_tests`.
    #[tokio::test]
    #[ignore = "needs network and a ghcr.io login"]
    async fn live_reader_lists_resolves_and_describes_a_feedstock_image() {
        let reader = LiveRegistryReader::new(None);
        let repo = "ghcr.io/neuralmagic/sandbox-go-cc";
        let discovered = reader
            .discover("ghcr.io/neuralmagic/sandbox-go-*")
            .await
            .expect("discover");
        assert!(
            discovered.iter().any(|r| r == repo),
            "discovered: {discovered:?}"
        );
        let tags = reader.tags(repo).await.expect("tags");
        assert!(tags.iter().any(|t| t == "latest"), "tags: {tags:?}");
        let channels: Vec<_> = tags.iter().filter(|t| is_channel_tag(t)).collect();
        assert!(!channels.is_empty());
        let digest = reader.resolve(repo, "latest").await.expect("resolve");
        assert!(digest.starts_with("sha256:"), "digest: {digest}");
        let description = reader.describe(repo, &digest).await.expect("describe");
        assert_eq!(description.arches, vec!["amd64", "arm64"]);
        assert!(description.created_at.is_some());
        assert!(
            description
                .labels
                .contains_key(crucible_capability::CAPABILITIES_LABEL)
        );
    }
}
