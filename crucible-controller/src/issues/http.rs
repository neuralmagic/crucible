//! The controller's one outbound HTTP boundary for GitHub reads: a client builder that always
//! carries a user-agent + timeout, and a status classifier that turns a non-2xx response into a
//! typed [`HttpError`] so callers branch on the *kind* of failure (not-found, rate-limited) instead
//! of sniffing a status code or a message string.

use std::time::Duration;

/// User-agent every controller GitHub read rides under. GitHub rejects UA-less requests, so this is
/// mandatory, not decoration.
const USER_AGENT: &str = "crucible-controller";

/// A GitHub read failure, typed so the retry/existence paths can match on the variant. `NotFound`
/// and `RateLimited` are the two the callers branch on; `Status` is any other non-2xx; `Transport`
/// is a connect/read-level failure. Implements `std::error::Error` (via `thiserror`), so `?` in an
/// `anyhow::Result` fn absorbs it unchanged.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HttpError {
    #[error("resource not found (404)")]
    NotFound,
    #[error("rate limited by GitHub")]
    RateLimited,
    #[error("{code}: {body}")]
    Status { code: u16, body: String },
    #[error(transparent)]
    Transport(#[from] reqwest::Error),
}

/// Build a GitHub REST client with the standard UA and a hard per-request `timeout` (no request may
/// hang a poll forever).
pub(crate) fn client(timeout: Duration) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(timeout)
        .build()
}

/// Whether a response is GitHub's rate-limit signal: a `429`, or the `403` GitHub uses for a
/// spent primary-rate-limit budget (`x-ratelimit-remaining: 0`). A plain `403` (a scope/permission
/// denial) is deliberately *not* rate-limiting — the retry path uses that distinction to fall back
/// to an anonymous read rather than sleep.
fn is_rate_limited(resp: &reqwest::Response) -> bool {
    if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return true;
    }
    if resp.status() == reqwest::StatusCode::FORBIDDEN {
        return resp
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim() == "0")
            .unwrap_or(false);
    }
    false
}

/// Pass a 2xx response through, or classify a non-2xx one into a typed [`HttpError`]. `what` names
/// the request (usually its URL) so the `Status` error is self-describing; `max` caps how much of
/// the error body is captured. 404 → [`HttpError::NotFound`], a rate-limit signal →
/// [`HttpError::RateLimited`], any other non-2xx → [`HttpError::Status`].
pub(crate) async fn expect_2xx(
    resp: reqwest::Response,
    what: &str,
    max: usize,
) -> Result<reqwest::Response, HttpError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(HttpError::NotFound);
    }
    if is_rate_limited(&resp) {
        return Err(HttpError::RateLimited);
    }
    let code = status.as_u16();
    let mut body = resp.text().await.unwrap_or_default();
    if body.len() > max {
        let mut end = max;
        while end > 0 && !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
    }
    let body = if body.is_empty() {
        what.to_string()
    } else {
        format!("{what}: {body}")
    };
    Err(HttpError::Status { code, body })
}
