//! Tier 2 artifacts: the kinds, their server-side size caps, and the ingest
//! request/response types for `POST /api/pods/{pod}/artifacts/{kind}`.
//!
//! The body of an ingest request is the raw gzipped artifact bytes, so the "request" type here is
//! only the path (`{pod}`, `{kind}`); the response and error are the typed JSON the ingest
//! endpoint returns.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fmt::Write as _;
use std::str::FromStr;

const MIB: u64 = 1024 * 1024;

/// The content digest of an artifact's (compressed) bytes, `sha256:<lowercase-hex>`. Both the
/// engine and the controller drop-box call this, so the two digests are byte-for-byte comparable
/// and a mismatch always indicates a real integrity failure.
pub fn content_digest(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    let mut s = String::with_capacity("sha256:".len() + hash.len() * 2);
    s.push_str("sha256:");
    for b in hash {
        // Infallible: writing to a String never errors.
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The Tier 2 artifact kinds. The serde spelling is the `{kind}` path segment of the ingest route
/// (`scope-pack`, `scope-transcript`, `run-session`, `run-files`, `otel-log`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    /// The gzipped tar of a surviving scope turn's frozen pack directory.
    ScopePack,
    /// The gzipped session NDJSON of a scope turn's propose/refine/adversary turns.
    ScopeTranscript,
    /// The gzipped `state/session.jsonl` of a loop run.
    RunSession,
    /// The gzipped tar of a loop run's `state/files`, one directory per task that captured.
    RunFiles,
    /// The raw OTLP jsonl the in-process collector captured next to the agent.
    OtelLog,
}

impl ArtifactKind {
    /// The `{kind}` path segment / on-disk subdirectory name.
    pub const fn as_str(self) -> &'static str {
        match self {
            ArtifactKind::ScopePack => "scope-pack",
            ArtifactKind::ScopeTranscript => "scope-transcript",
            ArtifactKind::RunSession => "run-session",
            ArtifactKind::RunFiles => "run-files",
            ArtifactKind::OtelLog => "otel-log",
        }
    }

    /// The server-side size cap, in bytes, of the (compressed) artifact body; oversize is a 413.
    pub const fn max_bytes(self) -> u64 {
        match self {
            ArtifactKind::ScopePack => 16 * MIB,
            ArtifactKind::ScopeTranscript => 32 * MIB,
            ArtifactKind::RunSession => 128 * MIB,
            ArtifactKind::RunFiles => 256 * MIB,
            ArtifactKind::OtelLog => 32 * MIB,
        }
    }

    /// Every kind, for exhaustive iteration in tests and route registration.
    const ALL: [ArtifactKind; 5] = [
        ArtifactKind::ScopePack,
        ArtifactKind::ScopeTranscript,
        ArtifactKind::RunSession,
        ArtifactKind::RunFiles,
        ArtifactKind::OtelLog,
    ];
}

impl fmt::Display for ArtifactKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error parsing a `{kind}` path segment that names no known artifact kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownArtifactKind(String);

impl fmt::Display for UnknownArtifactKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown artifact kind: {}", self.0)
    }
}

impl std::error::Error for UnknownArtifactKind {}

impl FromStr for ArtifactKind {
    type Err = UnknownArtifactKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ArtifactKind::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| UnknownArtifactKind(s.to_string()))
    }
}

/// One entry in the Tier 1 manifest: the authoritative index of an artifact the pod attempted to
/// upload. It rides the kubelet-authenticated termination message; the controller checks the
/// drop-box against it, so a missing or digest-mismatched artifact is caught without trusting the
/// POST path.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ArtifactRef {
    pub kind: ArtifactKind,
    /// Content digest of the (compressed) bytes, `sha256:<hex>`.
    pub digest: String,
    /// Size of the (compressed) bytes.
    pub bytes: u64,
    /// Whether the pod's POST for this artifact landed a `2xx`; `false` means the upload failed
    /// after retries.
    pub delivered: bool,
}

/// The path parameters of `POST /api/pods/{pod}/artifacts/{kind}`. The body is the raw gzipped
/// artifact bytes rather than JSON.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct IngestPath {
    pod: String,
    kind: ArtifactKind,
}

/// The ingest endpoint's success response. Uploads are content-addressed, so a re-POST of a digest
/// the drop-box already holds returns `200` with `stored: false` and changes nothing (at-least-once
/// delivery against an idempotent endpoint).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct IngestResponse {
    pub kind: ArtifactKind,
    /// The digest the ingest endpoint computed over the received bytes, `sha256:<hex>`.
    pub digest: String,
    pub bytes: u64,
    /// `true` if this POST wrote the artifact; `false` if it was already present (dedup).
    pub stored: bool,
}

/// The ingest endpoint's error body. `limit_bytes` is populated on a `413` so the caller sees the
/// cap it hit.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct IngestError {
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_bytes: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_serde_matches_route_segment() {
        for kind in ArtifactKind::ALL {
            let json = serde_json::to_string(&kind).expect("serialize kind");
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
            let round: ArtifactKind = serde_json::from_str(&json).expect("deserialize kind");
            assert_eq!(round, kind);
            assert_eq!(kind.as_str().parse::<ArtifactKind>().unwrap(), kind);
        }
    }

    #[test]
    fn route_segments_are_exact() {
        assert_eq!(ArtifactKind::ScopePack.as_str(), "scope-pack");
        assert_eq!(ArtifactKind::ScopeTranscript.as_str(), "scope-transcript");
        assert_eq!(ArtifactKind::RunSession.as_str(), "run-session");
        assert_eq!(ArtifactKind::OtelLog.as_str(), "otel-log");
    }

    #[test]
    fn caps_are_the_adr_values() {
        assert_eq!(ArtifactKind::ScopePack.max_bytes(), 16 * MIB);
        assert_eq!(ArtifactKind::ScopeTranscript.max_bytes(), 32 * MIB);
        assert_eq!(ArtifactKind::RunSession.max_bytes(), 128 * MIB);
        assert_eq!(ArtifactKind::OtelLog.max_bytes(), 32 * MIB);
    }

    #[test]
    fn unknown_kind_is_an_error_not_a_panic() {
        assert!("nope".parse::<ArtifactKind>().is_err());
        assert!("scope_pack".parse::<ArtifactKind>().is_err()); // underscore instead of the kebab form
    }

    #[test]
    fn content_digest_is_stable_prefixed_lowercase_hex() {
        // Known-answer: sha256("").
        assert_eq!(
            content_digest(b""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(content_digest(b"crucible"), content_digest(b"crucible"));
        assert_ne!(content_digest(b"crucible"), content_digest(b"Crucible"));
        let d = content_digest(b"anything");
        assert_eq!(d.len(), "sha256:".len() + 64);
        assert!(
            d.strip_prefix("sha256:")
                .unwrap()
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        );
    }

    #[test]
    fn ingest_error_omits_limit_when_absent() {
        let e = IngestError {
            error: "boom".to_string(),
            limit_bytes: None,
        };
        assert_eq!(serde_json::to_string(&e).unwrap(), r#"{"error":"boom"}"#);
    }
}
