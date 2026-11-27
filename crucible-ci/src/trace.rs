//! One client span per request a CI system answers, with the rate-limit headers it sent back.

use tracing::Span;

pub(crate) fn request(system: &'static str, doing: &str, request: &reqwest::Request) -> Span {
    tracing::info_span!(
        "ci.request",
        "otel.name" = doing,
        "otel.kind" = "client",
        "ci.system" = system,
        "http.request.method" = %request.method(),
        "url.path" = request.url().path(),
        "http.response.status_code" = tracing::field::Empty,
        "ci.ratelimit.remaining" = tracing::field::Empty,
        "ci.ratelimit.limit" = tracing::field::Empty,
    )
}

pub(crate) fn response(span: &Span, response: &reqwest::Response) {
    let status = response.status().as_u16();
    span.record("http.response.status_code", status);
    let header = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok());
    let remaining = header("ratelimit-remaining");
    if let Some(value) = remaining {
        span.record("ci.ratelimit.remaining", value);
    }
    if let Some(value) = header("ratelimit-limit") {
        span.record("ci.ratelimit.limit", value);
    }
    tracing::debug!(parent: span, status, remaining, "ci request");
}
