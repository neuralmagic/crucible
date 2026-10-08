//! Caller identity. A tenant token is registered by its SHA-256, either in the operator's static
//! tenants file or in a controller-written tenant record; the matching entry pins the caller to one
//! namespace, a role, and a queue allowlist. Realm access tokens carry the same scope as claims
//! ([`crate::auth`]). Nothing a tool receives in its arguments can widen that scope.

use crucible_contract::kueue::{TenantRecord, TenantRole};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;
use tokio::sync::RwLock;

#[derive(Debug, thiserror::Error)]
pub enum TenantsError {
    #[error("parsing the tenants file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("tenant {index}: {source}")]
    Entry {
        index: usize,
        #[source]
        source: EntryError,
    },
    #[error("tenants {first} and {second} share a token digest")]
    DuplicateToken { first: usize, second: usize },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NameError {
    #[error(
        "{what} {value:?} is not a DNS-1123 label (lowercase alphanumerics and '-', 1-63 chars)"
    )]
    NotDnsLabel { what: &'static str, value: String },
    #[error(
        "{what} {value:?} is not a label value (alphanumerics, '-', '_', '.', 1-63 chars, alphanumeric at both ends)"
    )]
    NotLabelValue { what: &'static str, value: String },
}

fn is_dns_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

fn is_label_value(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && s.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && s.bytes().last().is_some_and(|b| b.is_ascii_alphanumeric())
}

macro_rules! dns_label_newtype {
    ($name:ident, $what:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: &str) -> Result<Self, NameError> {
                if is_dns_label(value) {
                    Ok(Self(value.to_string()))
                } else {
                    Err(NameError::NotDnsLabel {
                        what: $what,
                        value: value.to_string(),
                    })
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

dns_label_newtype!(Namespace, "namespace");
dns_label_newtype!(QueueName, "local queue");

/// Run identifier stamped as a label on everything a caller creates.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunId(String);

impl RunId {
    pub fn parse(value: &str) -> Result<Self, NameError> {
        if is_label_value(value) {
            Ok(Self(value.to_string()))
        } else {
            Err(NameError::NotLabelValue {
                what: "run id",
                value: value.to_string(),
            })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who launched the run a delegated token acts for: the token's `sub`, kept as a label value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Launcher(String);

impl Launcher {
    pub fn parse(value: &str) -> Result<Self, NameError> {
        if is_label_value(value) {
            Ok(Self(value.to_string()))
        } else {
            Err(NameError::NotLabelValue {
                what: "launcher",
                value: value.to_string(),
            })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Refusal {
    #[error("this token is bound to namespace {pinned:?}, not {requested:?}")]
    NamespacePinned { pinned: String, requested: String },
    #[error("name a namespace: this token has no default one")]
    NamespaceRequired,
    #[error(transparent)]
    Namespace(NameError),
    #[error("role {role:?} may not call {tool}; it needs the submitter role")]
    Role {
        role: TenantRole,
        tool: &'static str,
    },
    #[error("local queue {queue:?} is not in this run's allowlist {allowed:?}")]
    Queue { queue: String, allowed: Vec<String> },
}

/// Who a request comes from, before it names a namespace. A run's or tenant's identity is pinned
/// to one namespace; a person's picks one per call (authorized by the tool policy's access checks)
/// and may have a default.
#[derive(Debug, Clone)]
pub struct Identity {
    pub run_id: RunId,
    pub namespace: Option<Namespace>,
    pub pinned: bool,
    pub role: TenantRole,
    pub queues: Vec<QueueName>,
    pub launched_by: Option<Launcher>,
}

impl Identity {
    /// A caller fixed to `caller`'s namespace.
    pub fn pinned(caller: Caller) -> Self {
        Self {
            run_id: caller.run_id,
            namespace: Some(caller.namespace),
            pinned: true,
            role: caller.role,
            queues: caller.queues,
            launched_by: caller.launched_by,
        }
    }

    /// The caller acting in `requested`, or in its own namespace when none is named.
    pub fn in_namespace(&self, requested: Option<&str>) -> Result<Caller, Refusal> {
        let namespace = match (requested, &self.namespace, self.pinned) {
            (None, Some(own), _) => own.clone(),
            (None, None, _) => return Err(Refusal::NamespaceRequired),
            (Some(r), Some(own), true) if r != own.as_str() => {
                return Err(Refusal::NamespacePinned {
                    pinned: own.to_string(),
                    requested: r.to_string(),
                });
            }
            (Some(r), _, _) => Namespace::parse(r).map_err(Refusal::Namespace)?,
        };
        Ok(Caller {
            run_id: self.run_id.clone(),
            namespace,
            role: self.role,
            queues: self.queues.clone(),
            launched_by: self.launched_by.clone(),
        })
    }
}

/// An [`Identity`] resolved to the one namespace a call acts in.
#[derive(Debug, Clone)]
pub struct Caller {
    pub run_id: RunId,
    pub namespace: Namespace,
    pub role: TenantRole,
    /// Empty means every LocalQueue in the namespace.
    pub queues: Vec<QueueName>,
    /// The human a delegated token acts for, stamped on everything this caller creates. Static
    /// tenant tokens carry no launcher.
    pub launched_by: Option<Launcher>,
}

impl Caller {
    pub fn require_submitter(&self, tool: &'static str) -> Result<(), Refusal> {
        match self.role {
            TenantRole::Submitter => Ok(()),
            TenantRole::Viewer => Err(Refusal::Role {
                role: self.role,
                tool,
            }),
        }
    }

    pub fn authorize_queue(&self, queue: &QueueName) -> Result<(), Refusal> {
        if self.queues.is_empty() || self.queues.contains(queue) {
            return Ok(());
        }
        Err(Refusal::Queue {
            queue: queue.to_string(),
            allowed: self.queues.iter().map(ToString::to_string).collect(),
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TenantsFile {
    #[serde(default, rename = "tenant")]
    tenants: Vec<TenantRecord>,
}

#[derive(Debug, Clone)]
struct Entry {
    caller: Caller,
    expires_at: Option<u64>,
}

impl Entry {
    fn parse(record: &TenantRecord) -> Result<([u8; 32], Self), EntryError> {
        let digest = parse_digest(&record.token_sha256).ok_or_else(|| EntryError::BadDigest {
            value: record.token_sha256.clone(),
        })?;
        let caller = Caller {
            run_id: RunId::parse(&record.run_id)?,
            namespace: Namespace::parse(&record.namespace)?,
            role: record.role,
            queues: record
                .queues
                .iter()
                .map(|q| QueueName::parse(q))
                .collect::<Result<_, _>>()?,
            launched_by: None,
        };
        Ok((
            digest,
            Self {
                caller,
                expires_at: record.expires_at,
            },
        ))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EntryError {
    #[error("token_sha256 {value:?} is not 64 hex characters")]
    BadDigest { value: String },
    #[error(transparent)]
    Name(#[from] NameError),
}

/// A snapshot of registered tokens, keyed by digest.
#[derive(Debug, Default)]
pub struct Tenants {
    by_digest: HashMap<[u8; 32], Entry>,
}

impl Tenants {
    /// The operator's static tenants file. Any invalid entry fails the whole file.
    pub fn from_toml(text: &str) -> Result<Self, TenantsError> {
        let file: TenantsFile = toml::from_str(text)?;
        let mut by_digest = HashMap::new();
        let mut index_of = HashMap::new();
        for (index, record) in file.tenants.iter().enumerate() {
            let (digest, entry) =
                Entry::parse(record).map_err(|source| TenantsError::Entry { index, source })?;
            if let Some(first) = index_of.insert(digest, index) {
                return Err(TenantsError::DuplicateToken {
                    first,
                    second: index,
                });
            }
            by_digest.insert(digest, entry);
        }
        Ok(Self { by_digest })
    }

    /// Controller-written records. A record that fails validation, or whose digest another
    /// record or `reserved` already holds, is left out and reported, never admitted.
    pub fn from_records<'a>(
        records: impl IntoIterator<Item = (&'a str, &'a TenantRecord)>,
        reserved: &Tenants,
    ) -> (Self, Vec<RecordRejection>) {
        let mut by_digest: HashMap<[u8; 32], (String, Entry)> = HashMap::new();
        let mut collided: HashMap<[u8; 32], Vec<String>> = HashMap::new();
        let mut rejected = Vec::new();
        for (source, record) in records {
            let (digest, entry) = match Entry::parse(record) {
                Ok(parsed) => parsed,
                Err(error) => {
                    rejected.push(RecordRejection {
                        source: source.to_string(),
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            if reserved.by_digest.contains_key(&digest) {
                rejected.push(RecordRejection {
                    source: source.to_string(),
                    reason: "its token digest is already in the static tenants file".to_string(),
                });
                continue;
            }
            if let Some(sources) = collided.get_mut(&digest) {
                sources.push(source.to_string());
                continue;
            }
            match by_digest.remove(&digest) {
                Some((first, _)) => {
                    collided.insert(digest, vec![first, source.to_string()]);
                }
                None => {
                    by_digest.insert(digest, (source.to_string(), entry));
                }
            }
        }
        for sources in collided.into_values() {
            for source in &sources {
                rejected.push(RecordRejection {
                    source: source.clone(),
                    reason: format!("its token digest is shared by {sources:?}"),
                });
            }
        }
        let by_digest = by_digest
            .into_iter()
            .map(|(digest, (_, entry))| (digest, entry))
            .collect();
        (Self { by_digest }, rejected)
    }

    pub fn len(&self) -> usize {
        self.by_digest.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_digest.is_empty()
    }

    fn lookup(&self, digest: &[u8; 32], now: u64) -> Option<Caller> {
        self.by_digest
            .get(digest)
            .filter(|e| e.expires_at.is_none_or(|at| now < at))
            .map(|e| e.caller.clone())
    }
}

/// A controller-written record the registry refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordRejection {
    /// Where the record came from, e.g. `namespace/configmap`.
    pub source: String,
    pub reason: String,
}

/// Every token the server accepts: the static file plus the live controller-written records.
pub struct Registry {
    file: Tenants,
    live: RwLock<Tenants>,
}

impl Registry {
    pub fn new(file: Tenants) -> Self {
        Self {
            file,
            live: RwLock::new(Tenants::default()),
        }
    }

    /// Replace the live records with `records`, returning the ones refused.
    pub async fn replace_live<'a>(
        &self,
        records: impl IntoIterator<Item = (&'a str, &'a TenantRecord)>,
    ) -> Vec<RecordRejection> {
        let (live, rejected) = Tenants::from_records(records, &self.file);
        *self.live.write().await = live;
        rejected
    }

    pub async fn live_len(&self) -> usize {
        self.live.read().await.len()
    }

    pub fn file_len(&self) -> usize {
        self.file.len()
    }

    /// The caller `token` belongs to, if it is registered and unexpired at `now` (Unix seconds).
    pub async fn authenticate(&self, token: &str, now: u64) -> Option<Caller> {
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        match self.file.lookup(&digest, now) {
            Some(caller) => Some(caller),
            None => self.live.read().await.lookup(&digest, now),
        }
    }
}

fn parse_digest(value: &str) -> Option<[u8; 32]> {
    let bytes = hex::decode(value.trim()).ok()?;
    bytes.try_into().ok()
}

#[cfg(test)]
mod tests {
    use crate::tenant::*;
    use crucible_contract::kueue::token_digest;

    fn file(token: &str, extra: &str) -> String {
        format!(
            r#"
[[tenant]]
run_id = "run-1"
token_sha256 = "{}"
namespace = "team-a"
role = "submitter"
queues = ["gpu"]
{extra}
"#,
            token_digest(token)
        )
    }

    fn record(token: &str, run: &str) -> TenantRecord {
        TenantRecord {
            run_id: run.to_string(),
            token_sha256: token_digest(token),
            namespace: "team-b".to_string(),
            role: TenantRole::Viewer,
            queues: vec![],
            expires_at: None,
        }
    }

    fn registry(text: &str) -> Registry {
        Registry::new(Tenants::from_toml(text).unwrap())
    }

    #[tokio::test]
    async fn a_registered_token_resolves_to_its_tenant() {
        let reg = registry(&file("secret", ""));
        let caller = reg.authenticate("secret", 0).await.unwrap();
        assert_eq!(caller.namespace.as_str(), "team-a");
        assert_eq!(caller.run_id.as_str(), "run-1");
        assert_eq!(caller.role, TenantRole::Submitter);
    }

    #[tokio::test]
    async fn unknown_missing_or_non_bearer_tokens_are_rejected() {
        let reg = registry(&file("secret", ""));
        assert!(reg.authenticate("nope", 0).await.is_none());
        assert!(reg.authenticate("", 0).await.is_none());
    }

    #[tokio::test]
    async fn an_expired_file_entry_stops_authenticating_at_its_expiry() {
        let reg = registry(&file("secret", "expires_at = 100"));
        assert!(reg.authenticate("secret", 99).await.is_some());
        assert!(reg.authenticate("secret", 100).await.is_none());
    }

    #[tokio::test]
    async fn live_records_authenticate_until_replaced() {
        let reg = registry("");
        let a = record("live-a", "run-a");
        let rejected = reg.replace_live([("ns/a", &a)]).await;
        assert!(rejected.is_empty());
        let caller = reg.authenticate("live-a", 0).await.unwrap();
        assert_eq!(caller.namespace.as_str(), "team-b");
        assert_eq!(caller.role, TenantRole::Viewer);
        reg.replace_live([]).await;
        assert!(reg.authenticate("live-a", 0).await.is_none());
    }

    #[tokio::test]
    async fn an_expired_live_record_does_not_authenticate() {
        let reg = registry("");
        let mut a = record("live-a", "run-a");
        a.expires_at = Some(50);
        reg.replace_live([("ns/a", &a)]).await;
        assert!(reg.authenticate("live-a", 49).await.is_some());
        assert!(reg.authenticate("live-a", 50).await.is_none());
    }

    #[tokio::test]
    async fn an_invalid_live_record_is_rejected_by_name_and_the_rest_still_load() {
        let reg = registry("");
        let good = record("good", "run-good");
        let mut bad = record("bad", "run-bad");
        bad.namespace = "Not_A_Label".to_string();
        let mut digestless = record("x", "run-x");
        digestless.token_sha256 = "abc".to_string();
        let rejected = reg
            .replace_live([("ns/good", &good), ("ns/bad", &bad), ("ns/x", &digestless)])
            .await;
        let sources: Vec<&str> = rejected.iter().map(|r| r.source.as_str()).collect();
        assert_eq!(sources, ["ns/bad", "ns/x"]);
        assert!(rejected[0].reason.contains("Not_A_Label"));
        assert!(rejected[1].reason.contains("abc"));
        assert!(reg.authenticate("good", 0).await.is_some());
        assert!(reg.authenticate("bad", 0).await.is_none());
        assert_eq!(reg.live_len().await, 1);
    }

    #[tokio::test]
    async fn live_records_sharing_a_digest_are_all_refused() {
        let reg = registry("");
        let a = record("same", "run-a");
        let b = record("same", "run-b");
        let c = record("same", "run-c");
        let rejected = reg
            .replace_live([("ns/a", &a), ("ns/b", &b), ("ns/c", &c)])
            .await;
        assert_eq!(rejected.len(), 3);
        assert!(reg.authenticate("same", 0).await.is_none());
    }

    #[tokio::test]
    async fn a_live_record_cannot_shadow_a_file_token() {
        let reg = registry(&file("secret", ""));
        let shadow = record("secret", "run-shadow");
        let rejected = reg.replace_live([("ns/shadow", &shadow)]).await;
        assert_eq!(rejected.len(), 1);
        assert!(rejected[0].reason.contains("static tenants file"));
        let caller = reg.authenticate("secret", 0).await.unwrap();
        assert_eq!(caller.run_id.as_str(), "run-1");
    }

    #[test]
    fn a_viewer_cannot_submit_and_the_refusal_names_the_tool() {
        let caller = Caller {
            run_id: RunId::parse("r").unwrap(),
            namespace: Namespace::parse("ns").unwrap(),
            role: TenantRole::Viewer,
            queues: vec![],
            launched_by: None,
        };
        let err = caller.require_submitter("workload_submit").unwrap_err();
        assert!(err.to_string().contains("workload_submit"));
    }

    #[test]
    fn a_pinned_identity_acts_only_in_its_own_namespace() {
        let identity = Identity::pinned(Caller {
            run_id: RunId::parse("r").unwrap(),
            namespace: Namespace::parse("team-a").unwrap(),
            role: TenantRole::Submitter,
            queues: vec![],
            launched_by: None,
        });
        assert_eq!(
            identity.in_namespace(None).unwrap().namespace.as_str(),
            "team-a"
        );
        assert_eq!(
            identity
                .in_namespace(Some("team-a"))
                .unwrap()
                .namespace
                .as_str(),
            "team-a"
        );
        assert_eq!(
            identity.in_namespace(Some("team-b")).unwrap_err(),
            Refusal::NamespacePinned {
                pinned: "team-a".into(),
                requested: "team-b".into()
            }
        );
    }

    #[test]
    fn an_unpinned_identity_still_needs_a_valid_namespace() {
        let identity = Identity {
            run_id: RunId::parse("p").unwrap(),
            namespace: None,
            pinned: false,
            role: TenantRole::Submitter,
            queues: vec![],
            launched_by: None,
        };
        assert!(matches!(
            identity.in_namespace(Some("Not_Valid")),
            Err(Refusal::Namespace(_))
        ));
        assert_eq!(
            identity.in_namespace(None).unwrap_err(),
            Refusal::NamespaceRequired
        );
    }

    #[test]
    fn the_queue_allowlist_is_enforced_and_empty_means_any() {
        let mut caller = Caller {
            run_id: RunId::parse("r").unwrap(),
            namespace: Namespace::parse("ns").unwrap(),
            role: TenantRole::Submitter,
            queues: vec![QueueName::parse("gpu").unwrap()],
            launched_by: None,
        };
        assert!(
            caller
                .authorize_queue(&QueueName::parse("gpu").unwrap())
                .is_ok()
        );
        let err = caller
            .authorize_queue(&QueueName::parse("cpu").unwrap())
            .unwrap_err();
        assert!(err.to_string().contains("\"cpu\""));
        caller.queues.clear();
        assert!(
            caller
                .authorize_queue(&QueueName::parse("cpu").unwrap())
                .is_ok()
        );
    }

    #[test]
    fn invalid_entries_fail_the_whole_file() {
        let bad_ns = file("secret", "").replace("team-a", "Team_A");
        assert!(matches!(
            Tenants::from_toml(&bad_ns),
            Err(TenantsError::Entry {
                index: 0,
                source: EntryError::Name(_)
            })
        ));
        let bad_digest = file("secret", "").replace(&token_digest("secret"), "abc");
        assert!(matches!(
            Tenants::from_toml(&bad_digest),
            Err(TenantsError::Entry {
                source: EntryError::BadDigest { .. },
                ..
            })
        ));
        let unknown_field = file("secret", "admin = true");
        assert!(matches!(
            Tenants::from_toml(&unknown_field),
            Err(TenantsError::Parse(_))
        ));
    }

    #[test]
    fn two_tenants_with_the_same_token_are_refused() {
        let twice = format!("{}{}", file("secret", ""), file("secret", ""));
        assert!(matches!(
            Tenants::from_toml(&twice),
            Err(TenantsError::DuplicateToken {
                first: 0,
                second: 1
            })
        ));
    }

    #[test]
    fn label_and_name_validation_matches_kubernetes_rules() {
        assert!(Namespace::parse("a-1").is_ok());
        assert!(Namespace::parse("-a").is_err());
        assert!(Namespace::parse(&"a".repeat(64)).is_err());
        assert!(RunId::parse("Run_1.x").is_ok());
        assert!(RunId::parse("_run").is_err());
        assert!(RunId::parse("").is_err());
    }
}
