//! The crucible-kueue tenant record: what the controller writes for a run and the crucible-kueue
//! server reads to authenticate that run's bearer token. Only the token's digest crosses this
//! boundary; the token itself reaches the loop pod through the run Secret alone.

use serde::{Deserialize, Serialize};

/// Label selecting tenant record ConfigMaps in the server's tenants namespace.
pub const TENANT_LABEL: &str = "crucible.dev/kueue-tenant";

/// Value of [`TENANT_LABEL`] on every tenant record.
pub const TENANT_LABEL_VALUE: &str = "true";

/// ConfigMap data key holding the JSON [`TenantRecord`].
pub const TENANT_RECORD_KEY: &str = "tenant.json";

/// Loop-pod env carrying the server's MCP URL.
pub const ENV_KUEUE_URL: &str = "CRUCIBLE_KUEUE_URL";

/// Loop-pod env carrying the run's raw bearer token, projected from the run Secret.
pub const ENV_KUEUE_TOKEN: &str = "CRUCIBLE_KUEUE_TOKEN";

/// OAuth scope to read queues, hardware, and workloads.
pub const SCOPE_READ: &str = "kueue.read";

/// OAuth scope to submit, stop, resume, and delete workloads.
pub const SCOPE_SUBMIT: &str = "kueue.submit";

/// What a token may do inside its namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TenantRole {
    /// Read queues, hardware, and workloads in the namespace.
    Viewer,
    /// Viewer plus submit, stop, resume, and delete of workloads this server created.
    Submitter,
}

impl TenantRole {
    /// The OAuth scopes the role grants.
    pub fn scopes(self) -> &'static [&'static str] {
        match self {
            Self::Viewer => &[SCOPE_READ],
            Self::Submitter => &[SCOPE_READ, SCOPE_SUBMIT],
        }
    }
}

/// One bearer token's scope. The server validates every name before the record can authenticate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantRecord {
    /// The label value the server stamps on everything the token creates: at most 63
    /// characters, alphanumeric at both ends.
    pub run_id: String,
    /// Lowercase hex SHA-256 of the bearer token, as [`token_digest`] computes it.
    pub token_sha256: String,
    pub namespace: String,
    pub role: TenantRole,
    /// LocalQueues the token may submit to; empty means every LocalQueue in the namespace.
    #[serde(default)]
    pub queues: Vec<String>,
    /// Unix seconds after which the record no longer authenticates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

impl TenantRecord {
    /// Whether the record has expired at `now` (Unix seconds).
    pub fn expired_at(&self, now: u64) -> bool {
        self.expires_at.is_some_and(|at| now >= at)
    }
}

/// The digest a [`TenantRecord`] stores for `token`.
pub fn token_digest(token: &str) -> String {
    crate::artifact::sha256_hex(token.as_bytes())
}

/// The tenant record ConfigMap name for a run pod.
pub fn record_name(pod_name: &str) -> String {
    format!("kueue-tenant-{pod_name}")
}

#[cfg(test)]
mod tests {
    use crate::kueue::*;

    #[test]
    fn a_record_round_trips_and_omits_an_absent_expiry() {
        let record = TenantRecord {
            run_id: "run-1".into(),
            token_sha256: token_digest("t"),
            namespace: "team-a".into(),
            role: TenantRole::Submitter,
            queues: vec!["gpu".into()],
            expires_at: None,
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains("expires_at"));
        assert!(json.contains(r#""role":"submitter""#));
        assert_eq!(serde_json::from_str::<TenantRecord>(&json).unwrap(), record);
    }

    #[test]
    fn unknown_fields_are_refused() {
        let json =
            r#"{"run_id":"r","token_sha256":"x","namespace":"n","role":"viewer","admin":true}"#;
        assert!(serde_json::from_str::<TenantRecord>(json).is_err());
    }

    #[test]
    fn expiry_is_inclusive_and_absent_means_never() {
        let mut record = TenantRecord {
            run_id: "r".into(),
            token_sha256: String::new(),
            namespace: "n".into(),
            role: TenantRole::Viewer,
            queues: vec![],
            expires_at: Some(100),
        };
        assert!(!record.expired_at(99));
        assert!(record.expired_at(100));
        record.expires_at = None;
        assert!(!record.expired_at(u64::MAX));
    }

    #[test]
    fn the_digest_is_lowercase_hex_sha256() {
        assert_eq!(
            token_digest("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
