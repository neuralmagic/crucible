//! The typed failure vocabulary of every Vault call.
//!
//! Callers never inspect an HTTP status: they match the variant, or hand it to
//! [`VaultError::http_status`] for the API boundary's response code.

use axum::http::StatusCode;

/// A failed Vault call, classified by what the operator has to do about it.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    /// No usable response: DNS, connect, TLS, timeout, or a standby node that could not forward.
    #[error("vault at {addr} is unreachable while {op}: {detail}")]
    Unreachable {
        addr: String,
        op: String,
        detail: String,
    },
    /// 403: the token's policy does not cover the path, or the token is gone.
    #[error("vault denied {op}: {detail}")]
    Denied { op: String, detail: String },
    /// 404: no such path, no such version, or a version that was deleted.
    #[error("vault has nothing at {op}")]
    NotFound { op: String },
    /// 503/501: sealed, uninitialized, or in maintenance.
    #[error("vault is sealed or uninitialized while {op}: {detail}")]
    Sealed { op: String, detail: String },
    /// A response outside the documented contract (unexpected status, or a body that does not
    /// parse).
    #[error("vault returned an unexpected response while {op}: {detail}")]
    Unexpected { op: String, detail: String },
}

impl VaultError {
    /// The status an API handler returns for this failure.
    pub fn http_status(&self) -> StatusCode {
        match self {
            VaultError::Denied { .. } => StatusCode::FORBIDDEN,
            VaultError::NotFound { .. } => StatusCode::NOT_FOUND,
            VaultError::Unreachable { .. }
            | VaultError::Sealed { .. }
            | VaultError::Unexpected { .. } => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    /// True when a retry with a freshly minted token could plausibly succeed.
    pub(crate) fn is_denied(&self) -> bool {
        matches!(self, VaultError::Denied { .. })
    }
}

/// How much of an error body is carried into a [`VaultError`]. Vault error bodies are operator
/// text, never secret material, but they are unbounded.
const MAX_DETAIL: usize = 500;

/// What a 400 means for the call that got it. Vault answers a rejected credential (a wrong secret
/// id, a JWT outside the role's bindings, a spent wrapping token) with 400, not 403, so only the
/// caller knows whether a 400 is a denial or a malformed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BadRequest {
    /// Login and unwrap: a 400 is the credential being turned down.
    CredentialRejected,
    /// Everything else: a 400 is a request this client built wrong.
    Protocol,
}

/// Classify one non-success Vault response. `body` is the raw response body; Vault's own shape is
/// `{"errors": [...]}`, and anything else is carried through truncated.
pub(crate) fn classify(
    addr: &str,
    op: &str,
    status: StatusCode,
    body: &str,
    bad_request: BadRequest,
) -> VaultError {
    let detail = detail_of(body);
    let op = op.to_string();
    match status.as_u16() {
        400 if bad_request == BadRequest::CredentialRejected => VaultError::Denied { op, detail },
        403 => VaultError::Denied { op, detail },
        404 => VaultError::NotFound { op },
        501 | 503 => VaultError::Sealed { op, detail },
        // 429 standby, 472 DR secondary, 473 performance standby: the node is up but cannot serve
        // this request, which is an availability problem, not a policy one.
        429 | 472 | 473 => VaultError::Unreachable {
            addr: addr.to_string(),
            op,
            detail,
        },
        _ => VaultError::Unexpected {
            op,
            detail: format!("HTTP {status}: {detail}"),
        },
    }
}

/// Join Vault's `errors` array, or fall back to the raw body; truncated either way.
fn detail_of(body: &str) -> String {
    let joined = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            let errors = v.get("errors")?.as_array()?;
            Some(
                errors
                    .iter()
                    .filter_map(|e| e.as_str())
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| body.trim().to_string());
    joined.chars().take(MAX_DETAIL).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rejected_credential_is_a_denial_not_a_protocol_error() {
        let status = StatusCode::BAD_REQUEST;
        let body = r#"{"errors":["invalid role or secret ID"]}"#;
        let rejected = classify(
            "http://vault.test",
            "logging in with approle",
            status,
            body,
            BadRequest::CredentialRejected,
        );
        assert!(rejected.is_denied());
        assert_eq!(rejected.http_status(), StatusCode::FORBIDDEN);
        let malformed = classify(
            "http://vault.test",
            "writing crucible/x",
            status,
            body,
            BadRequest::Protocol,
        );
        assert!(matches!(malformed, VaultError::Unexpected { .. }));
    }

    #[test]
    fn statuses_map_to_the_documented_codes() {
        let cases = [
            (403, StatusCode::FORBIDDEN),
            (404, StatusCode::NOT_FOUND),
            (503, StatusCode::SERVICE_UNAVAILABLE),
            (501, StatusCode::SERVICE_UNAVAILABLE),
            (429, StatusCode::SERVICE_UNAVAILABLE),
            (418, StatusCode::SERVICE_UNAVAILABLE),
        ];
        for (raw, expected) in cases {
            let status = StatusCode::from_u16(raw).expect("test status");
            assert_eq!(
                classify(
                    "http://vault.test",
                    "reading x",
                    status,
                    "{}",
                    BadRequest::Protocol
                )
                .http_status(),
                expected
            );
        }
    }

    #[test]
    fn sealed_and_denied_are_distinct_variants() {
        let sealed = classify(
            "http://vault.test",
            "reading x",
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"errors":["Vault is sealed"]}"#,
            BadRequest::Protocol,
        );
        assert!(matches!(sealed, VaultError::Sealed { .. }));
        assert!(sealed.to_string().contains("Vault is sealed"));
        let denied = classify(
            "http://vault.test",
            "reading x",
            StatusCode::FORBIDDEN,
            r#"{"errors":["permission denied"]}"#,
            BadRequest::Protocol,
        );
        assert!(denied.is_denied());
    }

    #[test]
    fn a_non_json_body_survives_truncated() {
        let long = "x".repeat(MAX_DETAIL * 2);
        let err = classify(
            "http://vault.test",
            "reading x",
            StatusCode::IM_A_TEAPOT,
            &long,
            BadRequest::Protocol,
        );
        let VaultError::Unexpected { detail, .. } = err else {
            panic!("expected Unexpected");
        };
        assert!(detail.ends_with(&"x".repeat(MAX_DETAIL)));
        assert!(!detail.ends_with(&"x".repeat(MAX_DETAIL + 1)));
    }

    #[test]
    fn an_empty_errors_array_falls_back_to_the_body() {
        let err = classify(
            "http://vault.test",
            "reading x",
            StatusCode::IM_A_TEAPOT,
            r#"{"errors":[]}"#,
            BadRequest::Protocol,
        );
        assert!(err.to_string().contains("errors"));
    }
}
