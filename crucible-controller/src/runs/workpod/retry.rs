//! Transient-vs-terminal classification for kube API failures. Callers use it to retry
//! transient failures instead of treating a temporarily unreachable cluster as "the pod is
//! gone": aborting a live turn or closing a ledger row on an unanswered call would be wrong.

#![allow(clippy::disallowed_macros)]

use std::time::Duration;

pub(crate) const POLL_BACKOFF_START: Duration = Duration::from_secs(1);
pub(crate) const POLL_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// How a kube API call failed, from the caller's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KubeFailure {
    /// 401/403: the credential is rejected (for a spoke, an expired or untrusted federated
    /// token). Not retryable; requires a configuration or RBAC fix.
    AuthRejected(u16),
    /// 404: the object does not exist. Terminal.
    Gone,
    /// Everything else: connection/DNS/TLS failures, 5xx, timeouts. Retryable with backoff.
    Transient,
}

pub(crate) fn classify(err: &kube::Error) -> KubeFailure {
    match err {
        kube::Error::Api(resp) => match resp.code {
            401 | 403 => KubeFailure::AuthRejected(resp.code),
            404 => KubeFailure::Gone,
            _ => KubeFailure::Transient,
        },
        _ => KubeFailure::Transient,
    }
}

/// Classify an `anyhow` error whose chain may carry a `kube::Error`. `None` = not a kube API
/// failure (e.g. a render or IO error); the caller keeps its pre-existing handling for those.
pub(crate) fn classify_chain(err: &anyhow::Error) -> Option<KubeFailure> {
    err.downcast_ref::<kube::Error>().map(classify)
}

#[cfg(test)]
mod tests {
    use crate::runs::workpod::retry::*;

    fn api_err(code: u16) -> kube::Error {
        kube::Error::Api(Box::new(kube::core::Status {
            status: Some(kube::core::response::StatusSummary::Failure),
            message: "x".into(),
            reason: "x".into(),
            code,
            ..Default::default()
        }))
    }

    #[test]
    fn auth_gone_transient_split() {
        assert_eq!(classify(&api_err(401)), KubeFailure::AuthRejected(401));
        assert_eq!(classify(&api_err(403)), KubeFailure::AuthRejected(403));
        assert_eq!(classify(&api_err(404)), KubeFailure::Gone);
        assert_eq!(classify(&api_err(500)), KubeFailure::Transient);
        assert_eq!(classify(&api_err(429)), KubeFailure::Transient);
    }

    #[test]
    fn chain_classification_survives_context() {
        let e = anyhow::Error::from(api_err(403)).context("polling the turn pod");
        assert_eq!(classify_chain(&e), Some(KubeFailure::AuthRejected(403)));
        let plain = anyhow::anyhow!("not kube");
        assert_eq!(classify_chain(&plain), None);
    }
}
