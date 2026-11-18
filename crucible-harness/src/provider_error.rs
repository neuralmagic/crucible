//! Which provider errors a retry of the same turn cannot fix.

const REFUSALS: [&str; 5] = [
    "invalid_request_error",
    "authentication_error",
    "permission_error",
    "not_found_error",
    "request_too_large",
];

/// Whether the provider refused the request itself, so the same turn sent again is refused
/// again: a typed refusal from the Messages API, or a message that leads with an HTTP 4xx other
/// than a timeout, a conflict, too-early or a rate limit. Anything else may pass on a retry.
pub fn is_refusal(error_type: &str, message: &str) -> bool {
    if REFUSALS.contains(&error_type) {
        return true;
    }
    let status = message
        .split_whitespace()
        .next()
        .filter(|word| word.len() == 3)
        .and_then(|word| word.parse::<u16>().ok());
    matches!(status, Some(400..=499)) && !matches!(status, Some(408 | 409 | 425 | 429))
}

#[cfg(test)]
mod tests {
    use crate::provider_error::is_refusal;

    #[test]
    fn a_refused_request_is_told_from_one_a_retry_may_pass() {
        let proxy = r#"403 {"error":{"code":"credential_placeholder_in_request_body","reason":"known_unavailable","message":"A credential placeholder in the request body cannot be forwarded."}}"#;
        assert!(is_refusal("pi", proxy));
        assert!(is_refusal("pi", "400 status code (no body)"));
        assert!(is_refusal("pi", "404"));
        assert!(is_refusal("invalid_request_error", "prompt is too long"));
        assert!(is_refusal("permission_error", "denied"));
        assert!(is_refusal("authentication_error", "invalid x-api-key"));

        for retryable in [
            "408 timeout",
            "409 conflict",
            "425 too early",
            "429 slow down",
        ] {
            assert!(!is_refusal("pi", retryable), "{retryable}");
        }
        assert!(!is_refusal("pi", "500 upstream connect error"));
        assert!(!is_refusal("pi", "529 overloaded"));
        assert!(!is_refusal("overloaded_error", "busy"));
        assert!(!is_refusal("rate_limit_error", "429 slow down"));
        assert!(!is_refusal("api_error", "internal"));
        assert!(!is_refusal("pi", "aborted"));
        assert!(!is_refusal("pi", ""));
        assert!(!is_refusal("pi", "4031 tokens over the limit"));
        assert!(!is_refusal("codex", "stream closed after 403 bytes"));
    }
}
