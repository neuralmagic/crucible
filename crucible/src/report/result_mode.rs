//! Tier 1 result mode: a turn command writes its result as a versioned `crucible-contract`
//! [`Envelope`] to `/dev/termination-log` as its final act, so the controller reads the
//! verdict/scope-report atomically from pod status (`containerStatuses[].state.terminated.message`)
//! instead of scraping the marker out of rotation-prone logs.
//!
//! The marker line the caller already printed stays too, old controllers still need it, and it is
//! the fallback a new controller uses for an old engine image. So the termination-message write is
//! belt-and-suspenders and strictly best-effort: a failure is warned, never fatal (the turn's exit
//! code and the marker are untouched).

use anyhow::Context;
use crucible_contract::{ArtifactRef, Envelope, EnvelopeKind};
use serde_json::Value;
use std::path::Path;

/// The container's `terminationMessagePath`, Kubernetes' default (`render_turn` does not override
/// it). The kubelet reads whatever a terminating single-container turn pod writes here into
/// `pod.status.containerStatuses[0].state.terminated.message`.
pub const TERMINATION_LOG_PATH: &str = "/dev/termination-log";

/// Build the Tier 1 envelope for `payload` under `kind` and write it, self-capped, to the default
/// termination-log path. Best-effort: on any failure the marker line already carries the result, so
/// the write is warned and the turn continues unaffected.
pub fn emit(kind: EnvelopeKind, payload: Value) {
    emit_with_artifacts(kind, payload, Vec::new());
}

/// Like [`emit`] but with the Tier 2 artifacts manifest: the authoritative index of every drop-box
/// upload the pod attempted, with content digests and a `delivered` flag. The controller trusts
/// this (it rides the kubelet-authenticated termination message), then checks the drop-box against
/// it, a missing or digest-mismatched artifact is caught without trusting the POST path at all.
pub fn emit_with_artifacts(kind: EnvelopeKind, payload: Value, artifacts: Vec<ArtifactRef>) {
    if let Err(e) = write_to(Path::new(TERMINATION_LOG_PATH), kind, payload, artifacts) {
        eprintln!("[crucible] failed to write the Tier 1 termination message: {e:#}");
    }
}

/// Serialize the self-capped envelope (with its artifacts manifest) and write it to `path`. Split
/// out from [`emit_with_artifacts`] so a test can drive the round-trip against a real temp file
/// without a `/dev/termination-log`.
pub fn write_to(
    path: &Path,
    kind: EnvelopeKind,
    payload: Value,
    artifacts: Vec<ArtifactRef>,
) -> anyhow::Result<()> {
    let mut envelope = Envelope::new(kind, payload);
    envelope.artifacts = artifacts;
    let json = envelope
        .to_capped_json()
        .context("capping the Tier 1 termination envelope")?;
    std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tempfile(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "crucible-result-mode-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir tmp");
        dir.join("termination-log")
    }

    #[test]
    fn verdict_envelope_round_trips_through_a_file() {
        let path = tempfile("verdict");
        let payload = json!({
            "tier": "T1",
            "rationale": "needs a new bench",
            "confidence": "low",
            "cost_usd": 0.42,
            "over_budget": false,
        });
        write_to(&path, EnvelopeKind::Verdict, payload.clone(), Vec::new()).expect("write");
        let back: Envelope =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        assert_eq!(back.kind, EnvelopeKind::Verdict);
        assert_eq!(back.payload, payload);
        assert!(back.artifacts.is_empty());
        assert!(back.usage.is_none());
    }

    #[test]
    fn artifacts_manifest_round_trips_in_the_envelope() {
        let path = tempfile("manifest");
        let artifacts = vec![
            ArtifactRef {
                kind: crucible_contract::ArtifactKind::ScopePack,
                digest: "sha256:aaaa".to_string(),
                bytes: 100,
                delivered: true,
            },
            ArtifactRef {
                kind: crucible_contract::ArtifactKind::ScopeTranscript,
                digest: "sha256:bbbb".to_string(),
                bytes: 200,
                delivered: false,
            },
        ];
        write_to(
            &path,
            EnvelopeKind::ScopeReport,
            json!({"digest": "v1:beef"}),
            artifacts.clone(),
        )
        .expect("write");
        let back: Envelope =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        assert_eq!(back.artifacts, artifacts);
        // The undelivered artifact stays in the manifest so the controller's fold lands it loudly.
        assert!(!back.artifacts[1].delivered);
    }

    #[test]
    fn scope_report_envelope_round_trips_through_a_file() {
        let path = tempfile("scope");
        let payload = json!({
            "stages": [{"name": "freeze", "passed": true, "detail": "ok"}],
            "digest": "v1:beef",
            "cost": 0.1,
        });
        write_to(
            &path,
            EnvelopeKind::ScopeReport,
            payload.clone(),
            Vec::new(),
        )
        .expect("write");
        let back: Envelope =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        assert_eq!(back.kind, EnvelopeKind::ScopeReport);
        assert_eq!(back.payload["digest"], json!("v1:beef"));
    }

    #[test]
    fn oversize_payload_is_still_valid_capped_json() {
        let path = tempfile("oversize");
        let payload = json!({"rationale": "x".repeat(20_000), "tier": "T2"});
        write_to(&path, EnvelopeKind::Verdict, payload, Vec::new()).expect("write");
        let raw = std::fs::read_to_string(&path).expect("read");
        assert!(raw.len() <= crucible_contract::TERMINATION_MESSAGE_CAP);
        let _: Envelope = serde_json::from_str(&raw).expect("still valid JSON");
    }
}
