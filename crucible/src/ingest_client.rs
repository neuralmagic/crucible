//! The engine side of the Tier 2 ingest drop-box: a turn pod POSTs its large artifacts (pack,
//! transcript, and the otel log) to the controller's evidence drop-box before it writes the Tier 1
//! termination message, so the manifest can state the truth about every upload the pod attempted.
//!
//! Delivery is at-least-once and the controller deduplicates (content-addressed), so a retried POST
//! is normal, not an error. We retry a failed POST [`MAX_ATTEMPTS`] times with a short backoff and
//! then *keep going*: the returned [`ArtifactRef`] carries `delivered: false` and the correct digest
//! anyway, so the failure lands loudly on the `work_pods` row via the Tier 1 manifest instead of
//! silently truncating the way the log path did.
//!
//! When `CRUCIBLE_INGEST_URL` is absent (a local run, or an old controller that never injected the
//! env) [`IngestConfig::from_env`] returns `None` and the caller falls back to marker emission.

use crucible_contract::{ArtifactKind, ArtifactRef, content_digest};
use std::path::PathBuf;
use std::time::Duration;

/// How many times a single artifact POST is attempted before giving up and marking it undelivered.
const MAX_ATTEMPTS: u32 = 3;

/// Base backoff between attempts (attempt N waits `BACKOFF * N`). Short, the endpoint is one hop
/// away in-cluster, and the manifest keeps any final miss loud, so we don't want to stall turn exit.
const BACKOFF: Duration = Duration::from_millis(300);

/// Per-request timeout for one POST attempt.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The ingest drop-box coordinates, read from the pod's env. Present only when the controller
/// injected `CRUCIBLE_INGEST_URL` (i.e. the deploy profile named a drop-box) AND the pod knows its
/// own name, otherwise `None`, and the caller emits markers as before.
pub struct IngestConfig {
    /// The controller's ingest base URL, e.g. `http://crucible-controller.autoresearch.svc:8080`.
    base_url: String,
    /// The pod's own name (downward API), the `{pod}` path segment, equal to the token's bound-pod
    /// claim by construction.
    pod: String,
    /// The filesystem path of the projected `crucible-ingest`-audience token, re-read on every
    /// attempt so the kubelet's rotation is always honored.
    token_path: PathBuf,
}

impl IngestConfig {
    /// Read the drop-box config from the pod env, or `None` when it isn't fully present (local run /
    /// old controller). Requires all three of URL, token path, and pod name, a partial set is
    /// treated as "no drop-box" rather than a half-configured POST that can only fail auth.
    pub fn from_env() -> Option<Self> {
        let base_url = non_empty_env(crucible_contract::ENV_INGEST_URL)?;
        let token_path = non_empty_env(crucible_contract::ENV_INGEST_TOKEN_PATH)?;
        let pod = non_empty_env(crucible_contract::ENV_POD_NAME)?;
        Some(IngestConfig {
            base_url: base_url.trim_end_matches('/').to_string(),
            pod,
            token_path: PathBuf::from(token_path),
        })
    }

    /// Construct explicitly (tests point `base_url` at a local server; production uses
    /// [`IngestConfig::from_env`]).
    #[cfg(test)]
    pub fn new(base_url: impl Into<String>, pod: impl Into<String>, token_path: PathBuf) -> Self {
        IngestConfig {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            pod: pod.into(),
            token_path,
        }
    }

    fn url(&self, kind: ArtifactKind) -> String {
        format!(
            "{}/api/pods/{}/artifacts/{}",
            self.base_url,
            self.pod,
            kind.as_str()
        )
    }
}

/// POST one artifact's (already gzipped) `bytes` to the drop-box, retrying [`MAX_ATTEMPTS`] times.
/// Always returns an [`ArtifactRef`] for the Tier 1 manifest, with the locally-computed digest and
/// `delivered` reflecting whether a `2xx` landed. Never fails the turn: a dead drop-box degrades to
/// `delivered: false`, which the controller's fold surfaces loudly.
pub fn post_artifact(cfg: &IngestConfig, kind: ArtifactKind, bytes: &[u8]) -> ArtifactRef {
    let digest = content_digest(bytes);
    let delivered = deliver(cfg, kind, bytes);
    if !delivered {
        eprintln!(
            "[crucible] Tier 2 upload of {} failed after {MAX_ATTEMPTS} attempts — the manifest \
             marks it delivered:false so the controller flags it",
            kind.as_str()
        );
    }
    ArtifactRef {
        kind,
        digest,
        bytes: bytes.len() as u64,
        delivered,
    }
}

/// Attempt the POST up to [`MAX_ATTEMPTS`] times. A `2xx` (stored or deduped) is success; a `413` is
/// a permanent oversize failure (no point retrying); any other status or transport error backs off
/// and retries. Returns whether the artifact is on the controller.
fn deliver(cfg: &IngestConfig, kind: ArtifactKind, bytes: &[u8]) -> bool {
    let client = match reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[crucible] ingest client build failed: {e}");
            return false;
        }
    };
    let url = cfg.url(kind);

    for attempt in 1..=MAX_ATTEMPTS {
        let token = match std::fs::read_to_string(&cfg.token_path) {
            Ok(t) => t.trim().to_string(),
            Err(e) => {
                eprintln!(
                    "[crucible] reading ingest token {}: {e}",
                    cfg.token_path.display()
                );
                return false;
            }
        };
        let resp = client
            .post(&url)
            .bearer_auth(&token)
            .header(reqwest::header::CONTENT_TYPE, "application/gzip")
            .body(bytes.to_vec())
            .send();

        match resp {
            Ok(r) if r.status().is_success() => return true,
            Ok(r) if r.status() == reqwest::StatusCode::PAYLOAD_TOO_LARGE => {
                eprintln!(
                    "[crucible] ingest rejected {} as oversize (413): {}",
                    kind.as_str(),
                    r.text().unwrap_or_default().trim()
                );
                return false;
            }
            Ok(r) => {
                eprintln!(
                    "[crucible] ingest POST {} attempt {attempt}/{MAX_ATTEMPTS} → HTTP {}",
                    kind.as_str(),
                    r.status()
                );
            }
            Err(e) => {
                eprintln!(
                    "[crucible] ingest POST {} attempt {attempt}/{MAX_ATTEMPTS} failed: {e}",
                    kind.as_str()
                );
            }
        }
        if attempt < MAX_ATTEMPTS {
            std::thread::sleep(BACKOFF * attempt);
        }
    }
    false
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_needs_all_three_vars() {
        let _guard = crate::test_env_lock();
        for k in [
            crucible_contract::ENV_INGEST_URL,
            crucible_contract::ENV_INGEST_TOKEN_PATH,
            crucible_contract::ENV_POD_NAME,
        ] {
            unsafe {
                std::env::remove_var(k);
            }
        }
        assert!(IngestConfig::from_env().is_none(), "no vars → None");

        unsafe {
            std::env::set_var(crucible_contract::ENV_INGEST_URL, "http://c:8080/");
        }
        assert!(IngestConfig::from_env().is_none(), "url alone → None");
        unsafe {
            std::env::set_var(crucible_contract::ENV_INGEST_TOKEN_PATH, "/tok");
        }
        assert!(
            IngestConfig::from_env().is_none(),
            "url+token, no pod → None"
        );
        unsafe {
            std::env::set_var(crucible_contract::ENV_POD_NAME, "crucible-turn-x");
        }
        let cfg = IngestConfig::from_env().expect("all three → Some");
        // Trailing slash on the base URL is trimmed so the joined path has exactly one.
        assert_eq!(
            cfg.url(ArtifactKind::ScopePack),
            "http://c:8080/api/pods/crucible-turn-x/artifacts/scope-pack"
        );

        for k in [
            crucible_contract::ENV_INGEST_URL,
            crucible_contract::ENV_INGEST_TOKEN_PATH,
            crucible_contract::ENV_POD_NAME,
        ] {
            unsafe {
                std::env::remove_var(k);
            }
        }
    }

    #[test]
    fn url_joins_without_double_slash() {
        let cfg = IngestConfig::new("http://host:8080", "pod-a", PathBuf::from("/tok"));
        assert_eq!(
            cfg.url(ArtifactKind::ScopeTranscript),
            "http://host:8080/api/pods/pod-a/artifacts/scope-transcript"
        );
    }

    #[test]
    fn a_dead_drop_box_marks_undelivered_but_keeps_the_digest() {
        // Point at a closed port; every attempt errors and we return delivered:false without
        // panicking, the whole point of the fallback path. Digest is still correct.
        let dir = std::env::temp_dir();
        let tok = dir.join(format!("crucible-ingest-tok-{}", std::process::id()));
        std::fs::write(&tok, "fake-token").expect("write token");
        // 127.0.0.1:1 is reliably unbound.
        let cfg = IngestConfig::new("http://127.0.0.1:1", "pod-a", tok.clone());
        let bytes = b"gzip-bytes";
        let art = post_artifact(&cfg, ArtifactKind::ScopePack, bytes);
        assert!(!art.delivered);
        assert_eq!(art.digest, content_digest(bytes));
        assert_eq!(art.bytes, bytes.len() as u64);
        let _ = std::fs::remove_file(&tok);
    }
}
