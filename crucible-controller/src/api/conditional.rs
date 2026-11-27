//! Conditional GETs on the API: a JSON answer carries a weak `ETag` of its body and
//! `Cache-Control: private, no-cache`, and a request whose `If-None-Match` names that tag gets
//! `304 Not Modified` with no body. A poller that sends back its last tag reads nothing new while
//! nothing changed, the way the controller itself polls GitHub and GitLab.
//!
//! The tag is a digest of the body the handler built, so it changes exactly when the answer does,
//! for every caller and every policy, with nothing to invalidate. The handler still runs.

use axum::body::{Body, HttpBody as _};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

pub(crate) async fn conditional_get(req: Request, next: Next) -> Response {
    if req.method() != Method::GET {
        return next.run(req).await;
    }
    let asked = req.headers().get(header::IF_NONE_MATCH).cloned();
    let resp = next.run(req).await;
    if resp.status() != StatusCode::OK || !is_json(resp.headers()) {
        return resp;
    }
    if resp.body().size_hint().exact().is_none() {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let tag = etag(&bytes);
    parts.headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    let Ok(value) = HeaderValue::from_str(&tag) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    parts.headers.insert(header::ETAG, value.clone());
    if asked.as_ref().is_some_and(|asked| matches(asked, &tag)) {
        let mut unchanged = StatusCode::NOT_MODIFIED.into_response();
        let headers = unchanged.headers_mut();
        headers.insert(header::ETAG, value);
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-cache"),
        );
        if let Some(vary) = parts.headers.get(header::VARY) {
            headers.insert(header::VARY, vary.clone());
        }
        return unchanged;
    }
    Response::from_parts(parts, Body::from(bytes))
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"))
}

fn etag(body: &[u8]) -> String {
    let digest = Sha256::digest(body);
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("W/\"{hex}\"")
}

fn matches(asked: &HeaderValue, tag: &str) -> bool {
    let Ok(asked) = asked.to_str() else {
        return false;
    };
    let opaque = |t: &str| t.trim().trim_start_matches("W/").to_string();
    let ours = opaque(tag);
    asked
        .split(',')
        .any(|candidate| candidate.trim() == "*" || opaque(candidate) == ours)
}

#[cfg(test)]
mod tests {
    use crate::api::conditional::{etag, matches};
    use axum::http::HeaderValue;

    #[test]
    fn a_tag_matches_itself_weak_or_strong_in_a_list_or_star() {
        let tag = etag(b"[]");
        assert!(tag.starts_with("W/\"") && tag.len() == 36, "{tag}");
        let bare = tag.trim_start_matches("W/");
        for asked in [
            tag.clone(),
            bare.to_string(),
            format!("\"other\", {tag}"),
            "*".to_string(),
        ] {
            assert!(
                matches(&HeaderValue::from_str(&asked).unwrap(), &tag),
                "{asked}"
            );
        }
        assert!(!matches(&HeaderValue::from_static("W/\"other\""), &tag));
        assert_ne!(etag(b"[]"), etag(b"[1]"));
    }
}
