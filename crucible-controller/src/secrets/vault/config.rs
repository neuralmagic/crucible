//! What the chart tells the controller about Vault, and the validated types the client is built
//! from.
//!
//! `VAULT_ADDR` alone decides whether the hub is a Vault client at all: unset or blank means no
//! Vault, and every other variable is ignored. Set, and the rest of the block must be coherent or
//! startup fails — a half-configured Vault is a deploy mistake, not a runtime fallback.

#![allow(clippy::disallowed_macros)]

use anyhow::{Context, Result, bail};
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// The KV v2 mount the registry owns when the chart does not say otherwise.
pub const DEFAULT_MOUNT: &str = "crucible";
/// Vault's own default AppRole auth mount.
pub const DEFAULT_APPROLE_MOUNT: &str = "approle";
/// Vault's own default JWT auth mount.
pub const DEFAULT_JWT_MOUNT: &str = "jwt";
/// Where the chart projects the service-account token JWT auth logs in with.
pub const DEFAULT_JWT_TOKEN_PATH: &str = "/var/run/secrets/vault/token";
/// Renew once the login token has less than this long to live.
pub const DEFAULT_RENEW_THRESHOLD: Duration = Duration::from_secs(300);

/// A single Vault mount name: one path segment, no slashes.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MountPath(String);

/// A path under a mount: slash-separated segments, each non-empty and free of traversal.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VaultPath(String);

/// Why a mount or path was refused. Every one of these would otherwise become a URL the caller
/// controls, so they are refused before a request is built rather than escaped.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PathError {
    #[error("a vault {noun} must not be empty")]
    Empty { noun: &'static str },
    #[error("a vault mount is one path segment, not {value:?}")]
    NotOneSegment { value: String },
    #[error("vault path {value:?} has an empty segment")]
    EmptySegment { value: String },
    #[error("vault path {value:?} traverses with `.` or `..`")]
    Traversal { value: String },
    #[error("vault {noun} {value:?} has a character outside [A-Za-z0-9._:@-]")]
    BadCharacter { noun: &'static str, value: String },
}

/// The characters a segment may carry. `:` is in because principals are spelled `user:<login>` and
/// `group:<group path>`; everything outside this set (whitespace, `%`, `?`, `#`, `..`) would need
/// URL escaping, so it is refused instead.
fn segment_ok(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '@'))
}

impl MountPath {
    pub fn parse(raw: &str) -> Result<Self, PathError> {
        let raw = raw.trim().trim_matches('/');
        if raw.is_empty() {
            return Err(PathError::Empty { noun: "mount" });
        }
        if raw.contains('/') {
            return Err(PathError::NotOneSegment {
                value: raw.to_string(),
            });
        }
        if !segment_ok(raw) {
            return Err(PathError::BadCharacter {
                noun: "mount",
                value: raw.to_string(),
            });
        }
        Ok(MountPath(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl VaultPath {
    pub fn parse(raw: &str) -> Result<Self, PathError> {
        let raw = raw.trim().trim_matches('/');
        if raw.is_empty() {
            return Err(PathError::Empty { noun: "path" });
        }
        for segment in raw.split('/') {
            if segment.is_empty() {
                return Err(PathError::EmptySegment {
                    value: raw.to_string(),
                });
            }
            if segment == "." || segment == ".." {
                return Err(PathError::Traversal {
                    value: raw.to_string(),
                });
            }
            if !segment_ok(segment) {
                return Err(PathError::BadCharacter {
                    noun: "path",
                    value: raw.to_string(),
                });
            }
        }
        Ok(VaultPath(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MountPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for VaultPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for MountPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MountPath({})", self.0)
    }
}

impl fmt::Debug for VaultPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VaultPath({})", self.0)
    }
}

/// A Vault token. `Debug` redacts, and there is no `Display`, so a token cannot reach a log line,
/// a span field, or an error body by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct VaultToken(String);

impl VaultToken {
    pub fn new(raw: impl Into<String>) -> Self {
        VaultToken(raw.into())
    }

    pub(crate) fn header_value(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for VaultToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("VaultToken(<redacted>)")
    }
}

/// An AppRole secret id. Redacted for the same reason as [`VaultToken`].
#[derive(Clone, PartialEq, Eq)]
pub struct SecretId(String);

impl SecretId {
    pub fn new(raw: impl Into<String>) -> Self {
        SecretId(raw.into())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretId(<redacted>)")
    }
}

/// Where the AppRole secret id comes from. A file is the deploy shape (a pre-created Secret the
/// chart mounts); an inline value is for tests and local runs.
#[derive(Debug, Clone)]
pub enum SecretIdSource {
    Inline(SecretId),
    File(PathBuf),
}

impl SecretIdSource {
    /// Read the configured secret id. A file is read at every login, so rotating the mounted
    /// Secret takes effect without a restart.
    pub(crate) fn load(&self) -> Result<SecretId> {
        match self {
            SecretIdSource::Inline(id) => Ok(id.clone()),
            SecretIdSource::File(path) => {
                let raw = std::fs::read_to_string(path).with_context(|| {
                    format!("reading the AppRole secret id at {}", path.display())
                })?;
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    bail!("the AppRole secret id at {} is empty", path.display());
                }
                Ok(SecretId::new(trimmed))
            }
        }
    }
}

/// The login method, selected by `VAULT_AUTH`.
#[derive(Debug, Clone)]
pub enum VaultAuth {
    /// Role id from the chart, secret id from a mounted Secret. `wrapped` means the secret id
    /// arrives as a response-wrapping token that is unwrapped at login.
    AppRole {
        mount: MountPath,
        role_id: String,
        secret_id: SecretIdSource,
        wrapped: bool,
    },
    /// The projected service-account token, presented to a JWT role bound to this subject and
    /// audience.
    Jwt {
        mount: MountPath,
        role: String,
        token_path: PathBuf,
    },
}

impl VaultAuth {
    /// The `VAULT_AUTH` literal this method is selected by.
    pub fn as_str(&self) -> &'static str {
        match self {
            VaultAuth::AppRole { .. } => "approle",
            VaultAuth::Jwt { .. } => "jwt",
        }
    }
}

/// Everything the client needs to talk to Vault.
#[derive(Debug, Clone)]
pub struct VaultCfg {
    /// `VAULT_ADDR`, trailing slashes trimmed.
    pub addr: String,
    /// The KV v2 mount the registry writes to.
    pub mount: MountPath,
    pub auth: VaultAuth,
    /// A PEM bundle added to the client's roots (`VAULT_CACERT`); unset uses the system roots.
    pub ca_cert: Option<PathBuf>,
    /// `X-Vault-Namespace`, for a namespaced enterprise cluster.
    pub namespace: Option<String>,
    /// Renew once the token has less than this left.
    pub renew_threshold: Duration,
}

impl VaultCfg {
    /// Read the block from the process environment. `None` = no `VAULT_ADDR` = the hub is not a
    /// Vault client.
    pub fn from_env() -> Result<Option<Self>> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// The pure half of [`Self::from_env`]: every variable comes from `get`, so a test never
    /// touches the process environment.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        let var = |name: &str| {
            get(name)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let Some(addr) = var("VAULT_ADDR") else {
            return Ok(None);
        };
        let addr = addr.trim_end_matches('/').to_string();
        let mount = match var("VAULT_MOUNT") {
            Some(raw) => MountPath::parse(&raw).context("VAULT_MOUNT")?,
            None => MountPath::parse(DEFAULT_MOUNT).context("VAULT_MOUNT")?,
        };
        let auth_kind = var("VAULT_AUTH").unwrap_or_else(|| "approle".to_string());
        let auth = match auth_kind.as_str() {
            "approle" => {
                let mount = match var("VAULT_APPROLE_MOUNT") {
                    Some(raw) => MountPath::parse(&raw).context("VAULT_APPROLE_MOUNT")?,
                    None => {
                        MountPath::parse(DEFAULT_APPROLE_MOUNT).context("VAULT_APPROLE_MOUNT")?
                    }
                };
                let role_id = match (var("VAULT_ROLE_ID"), var("VAULT_ROLE_ID_FILE")) {
                    (Some(id), _) => id,
                    (None, Some(path)) => std::fs::read_to_string(&path)
                        .with_context(|| format!("reading VAULT_ROLE_ID_FILE at {path}"))?
                        .trim()
                        .to_string(),
                    (None, None) => {
                        bail!("VAULT_AUTH=approle needs VAULT_ROLE_ID or VAULT_ROLE_ID_FILE",)
                    }
                };
                if role_id.is_empty() {
                    bail!("the AppRole role id is empty");
                }
                let secret_id = match (var("VAULT_SECRET_ID"), var("VAULT_SECRET_ID_FILE")) {
                    (Some(id), _) => SecretIdSource::Inline(SecretId::new(id)),
                    (None, Some(path)) => SecretIdSource::File(PathBuf::from(path)),
                    (None, None) => {
                        bail!("VAULT_AUTH=approle needs VAULT_SECRET_ID or VAULT_SECRET_ID_FILE",)
                    }
                };
                let wrapped = parse_bool(var("VAULT_SECRET_ID_WRAPPED").as_deref())
                    .context("VAULT_SECRET_ID_WRAPPED")?;
                VaultAuth::AppRole {
                    mount,
                    role_id,
                    secret_id,
                    wrapped,
                }
            }
            "jwt" => {
                let mount = match var("VAULT_JWT_MOUNT") {
                    Some(raw) => MountPath::parse(&raw).context("VAULT_JWT_MOUNT")?,
                    None => MountPath::parse(DEFAULT_JWT_MOUNT).context("VAULT_JWT_MOUNT")?,
                };
                let Some(role) = var("VAULT_JWT_ROLE") else {
                    bail!("VAULT_AUTH=jwt needs VAULT_JWT_ROLE");
                };
                let token_path = var("VAULT_JWT_TOKEN_FILE")
                    .unwrap_or_else(|| DEFAULT_JWT_TOKEN_PATH.to_string());
                VaultAuth::Jwt {
                    mount,
                    role,
                    token_path: PathBuf::from(token_path),
                }
            }
            other => bail!("VAULT_AUTH must be `approle` or `jwt`, not {other:?}"),
        };
        let renew_threshold = match var("VAULT_RENEW_THRESHOLD_SECS") {
            Some(raw) => Duration::from_secs(
                raw.parse::<u64>()
                    .context("VAULT_RENEW_THRESHOLD_SECS must be a whole number of seconds")?,
            ),
            None => DEFAULT_RENEW_THRESHOLD,
        };
        Ok(Some(VaultCfg {
            addr,
            mount,
            auth,
            ca_cert: var("VAULT_CACERT").map(PathBuf::from),
            namespace: var("VAULT_NAMESPACE"),
            renew_threshold,
        }))
    }
}

/// A chart-shaped boolean: unset is false, and anything outside the two spellings is a deploy
/// mistake rather than a silent false.
fn parse_bool(raw: Option<&str>) -> Result<bool> {
    match raw {
        None => Ok(false),
        Some(v) => match v.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Ok(true),
            "false" | "0" | "no" => Ok(false),
            other => bail!("expected a boolean, got {other:?}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn no_addr_means_no_vault() {
        let cfg = VaultCfg::from_lookup(lookup(&[("VAULT_AUTH", "approle")])).expect("parse");
        assert!(cfg.is_none());
        let blank = VaultCfg::from_lookup(lookup(&[("VAULT_ADDR", "   ")])).expect("parse");
        assert!(blank.is_none());
    }

    #[test]
    fn approle_is_the_default_method_and_trims_the_addr() {
        let cfg = VaultCfg::from_lookup(lookup(&[
            ("VAULT_ADDR", "https://vault.example.com/"),
            ("VAULT_ROLE_ID", "rid"),
            ("VAULT_SECRET_ID", "sid"),
        ]))
        .expect("parse")
        .expect("configured");
        assert_eq!(cfg.addr, "https://vault.example.com");
        assert_eq!(cfg.mount.as_str(), DEFAULT_MOUNT);
        assert_eq!(cfg.auth.as_str(), "approle");
        assert_eq!(cfg.renew_threshold, DEFAULT_RENEW_THRESHOLD);
        let VaultAuth::AppRole {
            mount,
            role_id,
            wrapped,
            ..
        } = cfg.auth
        else {
            panic!("expected approle");
        };
        assert_eq!(mount.as_str(), DEFAULT_APPROLE_MOUNT);
        assert_eq!(role_id, "rid");
        assert!(!wrapped);
    }

    #[test]
    fn jwt_is_selected_by_config() {
        let cfg = VaultCfg::from_lookup(lookup(&[
            ("VAULT_ADDR", "https://vault.example.com"),
            ("VAULT_AUTH", "jwt"),
            ("VAULT_JWT_ROLE", "crucible-hub"),
            ("VAULT_MOUNT", "crucible"),
            ("VAULT_CACERT", "/etc/vault/ca.pem"),
            ("VAULT_NAMESPACE", "ns"),
            ("VAULT_RENEW_THRESHOLD_SECS", "42"),
        ]))
        .expect("parse")
        .expect("configured");
        assert_eq!(cfg.renew_threshold, Duration::from_secs(42));
        assert_eq!(
            cfg.ca_cert.as_deref(),
            Some(std::path::Path::new("/etc/vault/ca.pem"))
        );
        assert_eq!(cfg.namespace.as_deref(), Some("ns"));
        let VaultAuth::Jwt {
            mount,
            role,
            token_path,
        } = cfg.auth
        else {
            panic!("expected jwt");
        };
        assert_eq!(mount.as_str(), DEFAULT_JWT_MOUNT);
        assert_eq!(role, "crucible-hub");
        assert_eq!(token_path, PathBuf::from(DEFAULT_JWT_TOKEN_PATH));
    }

    #[test]
    fn a_half_configured_block_fails_loudly() {
        let cases: Vec<Vec<(&str, &str)>> = vec![
            vec![("VAULT_ADDR", "https://v"), ("VAULT_SECRET_ID", "sid")],
            vec![("VAULT_ADDR", "https://v"), ("VAULT_ROLE_ID", "rid")],
            vec![("VAULT_ADDR", "https://v"), ("VAULT_AUTH", "jwt")],
            vec![("VAULT_ADDR", "https://v"), ("VAULT_AUTH", "kubernetes")],
            vec![
                ("VAULT_ADDR", "https://v"),
                ("VAULT_ROLE_ID", "rid"),
                ("VAULT_SECRET_ID", "sid"),
                ("VAULT_SECRET_ID_WRAPPED", "maybe"),
            ],
            vec![
                ("VAULT_ADDR", "https://v"),
                ("VAULT_ROLE_ID", "rid"),
                ("VAULT_SECRET_ID", "sid"),
                ("VAULT_MOUNT", "a/b"),
            ],
            vec![
                ("VAULT_ADDR", "https://v"),
                ("VAULT_ROLE_ID", "rid"),
                ("VAULT_SECRET_ID", "sid"),
                ("VAULT_RENEW_THRESHOLD_SECS", "soon"),
            ],
        ];
        for case in cases {
            assert!(
                VaultCfg::from_lookup(lookup(&case)).is_err(),
                "expected {case:?} to fail"
            );
        }
    }

    #[test]
    fn wrapped_secret_ids_are_opt_in() {
        for (raw, expected) in [("true", true), ("1", true), ("no", false)] {
            let cfg = VaultCfg::from_lookup(lookup(&[
                ("VAULT_ADDR", "https://v"),
                ("VAULT_ROLE_ID", "rid"),
                ("VAULT_SECRET_ID", "sid"),
                ("VAULT_SECRET_ID_WRAPPED", raw),
            ]))
            .expect("parse")
            .expect("configured");
            let VaultAuth::AppRole { wrapped, .. } = cfg.auth else {
                panic!("expected approle");
            };
            assert_eq!(wrapped, expected, "for {raw}");
        }
    }

    #[test]
    fn paths_refuse_traversal_and_url_metacharacters() {
        assert!(VaultPath::parse("user:wren/gh-token").is_ok());
        assert!(VaultPath::parse("/group:idp/team-x/key/").is_ok());
        assert_eq!(
            VaultPath::parse("a/../b"),
            Err(PathError::Traversal {
                value: "a/../b".to_string()
            })
        );
        assert_eq!(
            VaultPath::parse("a//b"),
            Err(PathError::EmptySegment {
                value: "a//b".to_string()
            })
        );
        assert!(matches!(
            VaultPath::parse("a b"),
            Err(PathError::BadCharacter { .. })
        ));
        assert!(matches!(
            VaultPath::parse("a?b=c"),
            Err(PathError::BadCharacter { .. })
        ));
        assert!(matches!(
            VaultPath::parse("a#b"),
            Err(PathError::BadCharacter { .. })
        ));
        assert!(matches!(
            VaultPath::parse("  "),
            Err(PathError::Empty { .. })
        ));
    }

    #[test]
    fn a_mount_is_one_segment() {
        assert_eq!(
            MountPath::parse("/crucible/").expect("parse").as_str(),
            "crucible"
        );
        assert!(matches!(
            MountPath::parse("crucible/data"),
            Err(PathError::NotOneSegment { .. })
        ));
    }

    #[test]
    fn secrets_redact_in_debug() {
        let token = VaultToken::new("hvs.super-secret");
        assert_eq!(format!("{token:?}"), "VaultToken(<redacted>)");
        let id = SecretId::new("sid-secret");
        assert_eq!(format!("{id:?}"), "SecretId(<redacted>)");
        let source = SecretIdSource::Inline(id);
        assert!(!format!("{source:?}").contains("sid-secret"));
        let cfg = VaultCfg::from_lookup(lookup(&[
            ("VAULT_ADDR", "https://v"),
            ("VAULT_ROLE_ID", "rid"),
            ("VAULT_SECRET_ID", "sid-secret"),
        ]))
        .expect("parse")
        .expect("configured");
        assert!(!format!("{cfg:?}").contains("sid-secret"));
    }

    #[test]
    fn a_secret_id_file_is_read_at_login() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret-id");
        std::fs::write(&path, "  sid-from-file\n").expect("write");
        let source = SecretIdSource::File(path.clone());
        assert_eq!(source.load().expect("load").expose(), "sid-from-file");
        std::fs::write(&path, "   ").expect("write");
        assert!(source.load().is_err());
    }
}
