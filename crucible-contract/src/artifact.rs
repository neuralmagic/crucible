//! Tier 2 artifacts: the kinds, their server-side size caps, the typed answers of a run's
//! evidence upload (`PUT /api/evidence/runs/{run}/{kind}`, RFC-0004:C-RUN-EVIDENCE), and the
//! gzipped tar a state directory travels as.
//!
//! The body of an upload is the raw gzipped artifact bytes; the response and error are typed JSON.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::str::FromStr;

const MIB: u64 = 1024 * 1024;

/// The content digest of an artifact's (compressed) bytes, `sha256:<lowercase-hex>`. Both the
/// engine and the controller call this, so the two digests are byte-for-byte comparable
/// and a mismatch always indicates a real integrity failure.
pub fn content_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(bytes))
}

/// The lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    lower_hex(&Sha256::digest(bytes))
}

/// `bytes` as lowercase hex.
pub fn lower_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Infallible: writing to a String never errors.
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The Tier 2 artifact kinds. The serde spelling is the `{kind}` path segment of the upload route
/// (`scope-pack`, `scope-transcript`, `run-session`, `run-files`, `run-commits`, `otel-log`).
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
    /// The gzipped tar of a playbook run's `state/git-memory`: each passing task's commit as a
    /// patch, in the order they were made, and `tasks.json` naming the task of each.
    RunCommits,
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
            ArtifactKind::RunCommits => "run-commits",
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
            ArtifactKind::RunCommits => 64 * MIB,
            ArtifactKind::OtelLog => 32 * MIB,
        }
    }

    /// Every kind, for exhaustive iteration in tests and route registration.
    const ALL: [ArtifactKind; 6] = [
        ArtifactKind::ScopePack,
        ArtifactKind::ScopeTranscript,
        ArtifactKind::RunSession,
        ArtifactKind::RunFiles,
        ArtifactKind::RunCommits,
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
/// uploads against it, so a missing or digest-mismatched artifact is caught without trusting the
/// upload path.
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

/// What an evidence upload stored: the digest the controller computed over the received bytes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct EvidenceStored {
    pub kind: ArtifactKind,
    /// `sha256:<hex>` of the received (compressed) bytes.
    pub digest: String,
    pub bytes: u64,
}

/// An evidence route's error body. `limit_bytes` is populated on a `413` so the caller sees the
/// cap it hit.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct EvidenceRefused {
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_bytes: Option<u64>,
}

/// What a URL path segment leaves unescaped besides ASCII alphanumerics.
const SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.');

/// `raw` escaped as one URL path segment, as a run id is in `/api/runs/{run}/…`.
pub fn path_segment(raw: &str) -> String {
    percent_encoding::utf8_percent_encode(raw, SEGMENT).to_string()
}

/// Gzip `bytes`.
pub fn gzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(bytes)?;
    enc.finish()
}

/// Gunzip `bytes`.
pub fn gunzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::MultiGzDecoder::new(bytes).read_to_end(&mut out)?;
    Ok(out)
}

/// `dir` as one gzipped tar, paths relative to it. Only regular files and directories are
/// archived; a symlink or any other entry is left out.
pub fn tar_gz_dir(dir: &Path) -> std::io::Result<Vec<u8>> {
    fn append(
        builder: &mut tar::Builder<Vec<u8>>,
        dir: &Path,
        prefix: &Path,
    ) -> std::io::Result<()> {
        let mut entries = std::fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            let rel = prefix.join(entry.file_name());
            let kind = entry.file_type()?;
            if kind.is_dir() {
                append(builder, &path, &rel)?;
            } else if kind.is_file() {
                builder.append_path_with_name(&path, &rel)?;
            }
        }
        Ok(())
    }
    let mut builder = tar::Builder::new(Vec::new());
    append(&mut builder, dir, Path::new(""))?;
    gzip(&builder.into_inner()?)
}

/// Unpack a gzipped tar into `dest`, refusing any entry that is not a regular file or a
/// directory, or whose path would leave `dest`.
pub fn untar_gz(bytes: &[u8], dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    let mut archive = tar::Archive::new(flate2::read::MultiGzDecoder::new(bytes));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{} is neither a file nor a directory",
                    entry.path()?.display()
                ),
            ));
        }
        if !entry.unpack_in(dest)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{} leaves the directory it unpacks into",
                    entry.path()?.display()
                ),
            ));
        }
    }
    Ok(())
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
    fn evidence_refused_omits_limit_when_absent() {
        let e = EvidenceRefused {
            error: "boom".to_string(),
            limit_bytes: None,
        };
        assert_eq!(serde_json::to_string(&e).unwrap(), r#"{"error":"boom"}"#);
    }

    #[test]
    fn a_dir_round_trips_through_tar_gz() {
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("triage/out")).unwrap();
        std::fs::write(src.path().join("triage/out/report.json"), b"{}").unwrap();
        std::fs::write(src.path().join("tasks.json"), b"[]").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", src.path().join("link")).unwrap();
        let gz = tar_gz_dir(src.path()).unwrap();
        assert_eq!(
            gz,
            tar_gz_dir(src.path()).unwrap(),
            "stable bytes for the same tree"
        );

        let dest = tempfile::tempdir().unwrap();
        untar_gz(&gz, dest.path()).unwrap();
        assert_eq!(
            std::fs::read(dest.path().join("triage/out/report.json")).unwrap(),
            b"{}"
        );
        assert_eq!(
            std::fs::read(dest.path().join("tasks.json")).unwrap(),
            b"[]"
        );
        assert!(
            !dest.path().join("link").exists(),
            "a symlink is never archived"
        );
    }

    #[test]
    fn untar_refuses_a_symlink_entry_and_an_escaping_path() {
        let dest = tempfile::tempdir().unwrap();

        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        b.append_link(&mut h, "evil", "/etc").unwrap();
        let gz = gzip(&b.into_inner().unwrap()).unwrap();
        assert!(untar_gz(&gz, dest.path()).is_err());

        let mut raw = tar::Header::new_old();
        raw.as_old_mut().name[..9].copy_from_slice(b"../escape");
        raw.set_size(1);
        raw.set_entry_type(tar::EntryType::Regular);
        raw.set_cksum();
        let mut b = tar::Builder::new(Vec::new());
        b.append(&raw, &b"x"[..]).unwrap();
        let gz = gzip(&b.into_inner().unwrap()).unwrap();
        assert!(untar_gz(&gz, dest.path()).is_err());
        assert!(!dest.path().parent().unwrap().join("escape").exists());
    }

    #[test]
    fn a_path_segment_escapes_separators_only() {
        assert_eq!(path_segment("acme_x_7-1700.1"), "acme_x_7-1700.1");
        assert_eq!(path_segment("a/b c?d#e%"), "a%2Fb%20c%3Fd%23e%25");
    }

    #[test]
    fn gzip_round_trips() {
        assert_eq!(gunzip(&gzip(b"line\n").unwrap()).unwrap(), b"line\n");
    }
}
