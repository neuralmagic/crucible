//! `crucible flow --dd-trace`: fetch one trace's spans from the Datadog spans API
//! instead of a pre-exported `--spans` file. The pages' `data` entries concatenate
//! into the same JSON array shape `flow::parse_spans` reads, so the join code never
//! knows which path produced the export.
//!
//! POST `https://api.<site>/api/v2/spans/events/search`, filtered to the trace id
//! with heartbeat resources dropped, 100 spans per page, following the
//! `meta.page.after` cursor with a 300 ms pause between pages. Credentials come
//! from the environment only (`DD_API_KEY`, `DD_APP_KEY`, `DD_SITE`) and are never
//! written anywhere: not to logs, not into error messages.

use serde_json::{Value, json};

const API_KEY_VAR: &str = "DD_API_KEY";
const APP_KEY_VAR: &str = "DD_APP_KEY";
const SITE_VAR: &str = "DD_SITE";
const DEFAULT_SITE: &str = "datadoghq.com";
const PAGE_LIMIT: u32 = 100;
const PAGE_PAUSE: std::time::Duration = std::time::Duration::from_millis(300);

#[derive(Debug, thiserror::Error)]
pub(crate) enum DdError {
    #[error("{var} is not set; --dd-trace reads Datadog credentials only from the environment")]
    MissingEnv { var: &'static str },
    #[error("--dd-trace must be a Datadog trace id (alphanumeric), got {value:?}")]
    BadTraceId { value: String },
    #[error("--dd-window must be a Datadog duration like 48h or 7d, got {value:?}")]
    BadWindow { value: String },
    #[error("building the Datadog API client")]
    Client(#[source] reqwest::Error),
    #[error("POST {url}")]
    Request {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("POST {url} returned {status}: {detail}")]
    Status {
        url: String,
        status: reqwest::StatusCode,
        detail: String,
    },
    #[error("reading the response from {url}")]
    Read {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("decoding the response from {url}")]
    Decode {
        url: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("serializing fetched spans")]
    Serialize(#[source] serde_json::Error),
}

#[derive(Debug)]
struct DdConfig {
    api_key: String,
    app_key: String,
    site: String,
}

/// Credentials from an env-shaped lookup; injected so tests never touch process env.
/// An unset or empty required key is a hard error naming the variable.
fn config_from(get: impl Fn(&str) -> Option<String>) -> Result<DdConfig, DdError> {
    let need = |var: &'static str| {
        get(var)
            .filter(|v| !v.is_empty())
            .ok_or(DdError::MissingEnv { var })
    };
    Ok(DdConfig {
        api_key: need(API_KEY_VAR)?,
        app_key: need(APP_KEY_VAR)?,
        site: get(SITE_VAR)
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_SITE.into()),
    })
}

/// Both the trace id and the window are spliced into the search query verbatim, so
/// only plain alphanumerics pass; anything else would rewrite the filter.
fn plain_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// One page of the spans search. `cursor` is absent on the first page.
fn request_body(trace_id: &str, window: &str, cursor: Option<&str>) -> Value {
    let mut page = json!({ "limit": PAGE_LIMIT });
    if let Some(c) = cursor {
        page["cursor"] = Value::String(c.into());
    }
    json!({
        "data": {
            "type": "search_request",
            "attributes": {
                "filter": {
                    "query": format!("trace_id:{trace_id} -resource_name:heartbeat"),
                    "from": format!("now-{window}"),
                    "to": "now",
                },
                "page": page,
                "sort": "timestamp",
            },
        }
    })
}

/// One page response -> (spans, next cursor). Takes the raw body text so a non-JSON
/// error page still yields a readable Status error instead of a bare decode failure.
fn parse_page(
    url: &str,
    status: reqwest::StatusCode,
    body: &str,
) -> Result<(Vec<Value>, Option<String>), DdError> {
    if !status.is_success() {
        return Err(DdError::Status {
            url: url.into(),
            status,
            detail: crate::turn_trace::truncate(
                &body.split_whitespace().collect::<Vec<_>>().join(" "),
                200,
            ),
        });
    }
    let mut payload: Value = serde_json::from_str(body).map_err(|source| DdError::Decode {
        url: url.into(),
        source,
    })?;
    let cursor = payload
        .pointer("/meta/page/after")
        .and_then(Value::as_str)
        .map(String::from);
    // An empty result set arrives as a missing or null `data`.
    let batch = match payload.get_mut("data").map(Value::take) {
        Some(Value::Array(a)) => a,
        _ => Vec::new(),
    };
    Ok((batch, cursor))
}

/// Fetch every span of one trace, minus heartbeats, as the JSON-array export string
/// the `--spans` path reads.
pub(crate) fn fetch_trace_spans(trace_id: &str, window: &str) -> Result<String, DdError> {
    if !plain_token(trace_id) {
        return Err(DdError::BadTraceId {
            value: trace_id.into(),
        });
    }
    if !plain_token(window) {
        return Err(DdError::BadWindow {
            value: window.into(),
        });
    }
    let cfg = config_from(|k| std::env::var(k).ok())?;
    let url = format!("https://api.{}/api/v2/spans/events/search", cfg.site);
    let client = reqwest::blocking::Client::builder()
        .build()
        .map_err(DdError::Client)?;
    let mut spans: Vec<Value> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0u32;
    loop {
        let resp = client
            .post(&url)
            .header("DD-API-KEY", &cfg.api_key)
            .header("DD-APPLICATION-KEY", &cfg.app_key)
            .json(&request_body(trace_id, window, cursor.as_deref()))
            .send()
            .map_err(|source| DdError::Request {
                url: url.clone(),
                source,
            })?;
        let status = resp.status();
        let body = resp.text().map_err(|source| DdError::Read {
            url: url.clone(),
            source,
        })?;
        let (batch, next) = parse_page(&url, status, &body)?;
        pages += 1;
        let got = batch.len();
        spans.extend(batch);
        cursor = next;
        if cursor.is_none() || got == 0 {
            break;
        }
        std::thread::sleep(PAGE_PAUSE);
    }
    println!(
        "[crucible flow] datadog: {} spans over {pages} page(s)",
        spans.len()
    );
    serde_json::to_string(&Value::Array(spans)).map_err(DdError::Serialize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn config(pairs: &[(&str, &str)]) -> Result<DdConfig, DdError> {
        let map = env(pairs);
        config_from(|k| map.get(k).cloned())
    }

    #[test]
    fn request_body_first_page_matches_the_export_spec() {
        let b = request_body("6893a2b1c4d5e6f7", "48h", None);
        let attrs = &b["data"]["attributes"];
        assert_eq!(b["data"]["type"], "search_request");
        assert_eq!(
            attrs["filter"]["query"],
            "trace_id:6893a2b1c4d5e6f7 -resource_name:heartbeat"
        );
        assert_eq!(attrs["filter"]["from"], "now-48h");
        assert_eq!(attrs["filter"]["to"], "now");
        assert_eq!(attrs["page"]["limit"], 100);
        assert!(attrs["page"].get("cursor").is_none());
        assert_eq!(attrs["sort"], "timestamp");
    }

    #[test]
    fn request_body_follow_page_carries_the_cursor_and_window() {
        let b = request_body("abc123", "7d", Some("eyJhZnRlciI6"));
        let attrs = &b["data"]["attributes"];
        assert_eq!(attrs["page"]["cursor"], "eyJhZnRlciI6");
        assert_eq!(attrs["page"]["limit"], 100);
        assert_eq!(attrs["filter"]["from"], "now-7d");
    }

    #[test]
    fn parse_page_extracts_spans_and_the_after_cursor() {
        let body = r#"{
            "data": [
                {"attributes": {"service": "crucible", "resource_name": "iteration"}},
                {"attributes": {"service": "claude-code"}}
            ],
            "meta": {"page": {"after": "next-cursor"}}
        }"#;
        let (batch, cursor) = parse_page("u", reqwest::StatusCode::OK, body).unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0]["attributes"]["service"], "crucible");
        assert_eq!(cursor.as_deref(), Some("next-cursor"));
    }

    #[test]
    fn parse_page_final_page_has_no_cursor_and_tolerates_missing_data() {
        let (batch, cursor) =
            parse_page("u", reqwest::StatusCode::OK, r#"{"data": [{"a": 1}]}"#).unwrap();
        assert_eq!(batch.len(), 1);
        assert!(cursor.is_none());
        let (batch, cursor) = parse_page(
            "u",
            reqwest::StatusCode::OK,
            r#"{"data": null, "meta": {}}"#,
        )
        .unwrap();
        assert!(batch.is_empty());
        assert!(cursor.is_none());
    }

    #[test]
    fn parse_page_non_2xx_is_a_status_error_with_a_folded_snippet() {
        let err = parse_page(
            "https://api.datadoghq.com/api/v2/spans/events/search",
            reqwest::StatusCode::FORBIDDEN,
            "{\"errors\": [\"Forbidden\"]}\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("403"), "{err}");
        match err {
            DdError::Status { status, detail, .. } => {
                assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
                assert!(detail.contains("Forbidden"), "{detail}");
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn parse_page_garbage_on_200_is_a_decode_error() {
        let err = parse_page("u", reqwest::StatusCode::OK, "<html>oops</html>").unwrap_err();
        assert!(matches!(err, DdError::Decode { .. }), "{err:?}");
    }

    #[test]
    fn config_missing_or_empty_keys_name_the_env_var() {
        let err = config(&[("DD_APP_KEY", "b")]).unwrap_err();
        assert!(err.to_string().contains("DD_API_KEY"), "{err}");
        let err = config(&[("DD_API_KEY", "a")]).unwrap_err();
        assert!(err.to_string().contains("DD_APP_KEY"), "{err}");
        let err = config(&[("DD_API_KEY", ""), ("DD_APP_KEY", "b")]).unwrap_err();
        assert!(matches!(err, DdError::MissingEnv { var: "DD_API_KEY" }));
    }

    #[test]
    fn config_site_defaults_to_us1_and_honors_dd_site() {
        let cfg = config(&[("DD_API_KEY", "a"), ("DD_APP_KEY", "b")]).unwrap();
        assert_eq!(cfg.site, "datadoghq.com");
        let cfg = config(&[
            ("DD_API_KEY", "a"),
            ("DD_APP_KEY", "b"),
            ("DD_SITE", "datadoghq.eu"),
        ])
        .unwrap();
        assert_eq!(cfg.site, "datadoghq.eu");
    }

    #[test]
    fn fetch_rejects_query_splicing_before_touching_env_or_network() {
        let err = fetch_trace_spans("abc OR *", "48h").unwrap_err();
        assert!(matches!(err, DdError::BadTraceId { .. }), "{err:?}");
        let err = fetch_trace_spans("abc123", "48h OR *").unwrap_err();
        assert!(matches!(err, DdError::BadWindow { .. }), "{err:?}");
        let err = fetch_trace_spans("", "48h").unwrap_err();
        assert!(matches!(err, DdError::BadTraceId { .. }), "{err:?}");
    }
}
