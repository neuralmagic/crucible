//! Server-side span-enriched flow rendering behind `GET /api/runs/{run_id}/flow-enriched`: the
//! run's `session.jsonl` plus its Datadog span timings folded through the linked engine's
//! `crucible::flow::render` (the library form of `crucible flow --dd-trace`), cached on the scratch
//! volume. The span fetch lives here: it is the one part of the report that reaches a network,
//! and the engine library keeps its renders pure. Datadog creds come from the environment only
//! (`DD_API_KEY`/`DD_APP_KEY`/`DD_SITE`) and never appear in logs or errors.

use crucible::flow::{FlowFormat, FlowInput};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A paginated Datadog span search can hold the request open a long time; give up after this
/// rather than pinning an HTTP worker forever.
const RENDER_TIMEOUT: Duration = Duration::from_secs(120);

/// The Datadog spans search page size, cursor pause, and lookback window (`crucible flow`'s
/// `--dd-window` default).
const DD_PAGE_LIMIT: u32 = 100;
const DD_PAGE_PAUSE: Duration = Duration::from_millis(300);
const DD_WINDOW: &str = "48h";
const DD_DEFAULT_SITE: &str = "datadoghq.com";

/// The Datadog credentials and endpoint a span fetch uses, read off the process env.
#[derive(Debug, Clone)]
pub(crate) struct DatadogAccess {
    api_key: String,
    app_key: String,
    /// The spans search URL: `https://api.<DD_SITE>/api/v2/spans/events/search`, or whatever
    /// `DD_API_URL` names as the API base.
    search_url: String,
}

/// Pre-flight: both DD keys present and non-empty in the process env. The error names the
/// missing var.
pub(crate) fn datadog_env_ready() -> Result<DatadogAccess, &'static str> {
    let need = |var: &'static str| std::env::var(var).ok().filter(|v| !v.is_empty()).ok_or(var);
    let api_key = need("DD_API_KEY")?;
    let app_key = need("DD_APP_KEY")?;
    let base = std::env::var("DD_API_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            let site = std::env::var("DD_SITE")
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| DD_DEFAULT_SITE.to_string());
            format!("https://api.{site}")
        });
    Ok(DatadogAccess {
        api_key,
        app_key,
        search_url: format!("{}/api/v2/spans/events/search", base.trim_end_matches('/')),
    })
}

/// The engine's `plain_token` rule for a trace id: non-empty, ascii-alphanumeric only. Checked
/// controller-side (capped at 64 chars) so a bad id is a 400, and so the id can be spliced into
/// the search query verbatim.
pub(crate) fn valid_trace_id(trace_id: &str) -> bool {
    !trace_id.is_empty()
        && trace_id.len() <= 64
        && trace_id.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Cache path on the scratch volume: `<scratch_dir>/flow-cache/<safe(run_id)>--<trace_id>.html`.
/// Both inputs are immutable, so the pair is a complete key; `trace_id` is already validated
/// ascii-alphanumeric upstream, `run_id` gets sanitized here.
pub(crate) fn cache_path(scratch_dir: &Path, run_id: &str, trace_id: &str) -> PathBuf {
    let safe_run: String = run_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    scratch_dir
        .join("flow-cache")
        .join(format!("{safe_run}--{trace_id}.html"))
}

/// Renders are cheap to redo, so the cache is bounded by entry count rather than aged out.
const CACHE_MAX_ENTRIES: usize = 128;

/// Bound the flow cache: keep the newest [`CACHE_MAX_ENTRIES`] files by mtime, never deleting
/// `just_written` (mtime granularity can tie with older entries). Missing dir is a no-op.
pub(crate) async fn prune_cache(scratch_dir: &Path, just_written: &Path) -> std::io::Result<()> {
    let dir = scratch_dir.join("flow-cache");
    let mut entries = Vec::new();
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    while let Some(ent) = rd.next_entry().await? {
        let meta = ent.metadata().await?;
        if meta.is_file() {
            let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            entries.push((mtime, ent.path()));
        }
    }
    if entries.len() <= CACHE_MAX_ENTRIES {
        return Ok(());
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.0));
    for (_, path) in entries.into_iter().skip(CACHE_MAX_ENTRIES) {
        if path != just_written {
            tokio::fs::remove_file(&path).await?;
        }
    }
    Ok(())
}

/// One page of the spans search: the trace's spans minus heartbeats, oldest first. `cursor` is
/// absent on the first page.
fn search_body(trace_id: &str, cursor: Option<&str>) -> Value {
    let mut page = json!({ "limit": DD_PAGE_LIMIT });
    if let Some(c) = cursor {
        page["cursor"] = Value::String(c.to_string());
    }
    json!({
        "data": {
            "type": "search_request",
            "attributes": {
                "filter": {
                    "query": format!("trace_id:{trace_id} -resource_name:heartbeat"),
                    "from": format!("now-{DD_WINDOW}"),
                    "to": "now",
                },
                "page": page,
                "sort": "timestamp",
            },
        }
    })
}

/// One page response -> (spans, next cursor). An empty result set arrives as a missing or null
/// `data`.
fn parse_page(body: &str) -> Result<(Vec<Value>, Option<String>), serde_json::Error> {
    let mut payload: Value = serde_json::from_str(body)?;
    let cursor = payload
        .pointer("/meta/page/after")
        .and_then(Value::as_str)
        .map(String::from);
    let batch = match payload.get_mut("data").map(Value::take) {
        Some(Value::Array(a)) => a,
        _ => Vec::new(),
    };
    Ok((batch, cursor))
}

/// Fetch every span of one trace as the JSON-array export `crucible::flow` joins, following the
/// `meta.page.after` cursor. A server echoing the cursor it was just given would page forever, so
/// a repeated cursor ends the walk.
async fn fetch_trace_spans(dd: &DatadogAccess, trace_id: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| format!("building the Datadog client: {e}"))?;
    let url = &dd.search_url;
    let mut spans: Vec<Value> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let resp = client
            .post(url)
            .header("DD-API-KEY", &dd.api_key)
            .header("DD-APPLICATION-KEY", &dd.app_key)
            .json(&search_body(trace_id, cursor.as_deref()))
            .send()
            .await
            .map_err(|e| format!("POST {url}: {e}"))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| format!("reading the response from {url}: {e}"))?;
        if !status.is_success() {
            let detail: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
            return Err(format!(
                "POST {url} returned {status}: {}",
                detail.chars().take(200).collect::<String>()
            ));
        }
        let (batch, next) =
            parse_page(&body).map_err(|e| format!("decoding the response from {url}: {e}"))?;
        let got = batch.len();
        spans.extend(batch);
        let repeated = next.is_some() && next == cursor;
        cursor = next;
        if cursor.is_none() || got == 0 || repeated {
            break;
        }
        tokio::time::sleep(DD_PAGE_PAUSE).await;
    }
    serde_json::to_string(&Value::Array(spans))
        .map_err(|e| format!("serializing fetched spans: {e}"))
}

/// Render the span-enriched `flow.html` for `session` and `trace_id` into `out_html`: fetch the
/// trace's spans from Datadog, fold them with the session log through the linked engine, and
/// write the page. The whole thing runs under [`RENDER_TIMEOUT`]. The error detail (DD API
/// failure, bad session log, timeout) may echo the request and stays server-side.
pub(crate) async fn render_flow(
    dd: &DatadogAccess,
    session: &Path,
    trace_id: &str,
    out_html: &Path,
) -> Result<(), String> {
    let work = async {
        let session_log = tokio::fs::read_to_string(session)
            .await
            .map_err(|e| format!("reading {}: {e}", session.display()))?;
        let spans_json = fetch_trace_spans(dd, trace_id).await?;
        let html = tokio::task::spawn_blocking(move || {
            let input = FlowInput {
                session_log,
                spans_json: Some(spans_json),
            };
            crucible::flow::render(&input, FlowFormat::Html)
        })
        .await
        .map_err(|e| format!("joining the flow render: {e}"))?
        .map_err(|e| format!("rendering the flow report: {e:#}"))?;
        tokio::fs::write(out_html, html)
            .await
            .map_err(|e| format!("writing {}: {e}", out_html.display()))
    };
    match tokio::time::timeout(RENDER_TIMEOUT, work).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "flow render exceeded {}s",
            RENDER_TIMEOUT.as_secs()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn trace_id_validation_is_the_engines_plain_token_rule() {
        assert!(valid_trace_id("abc123DEF"));
        assert!(valid_trace_id("0"));
        for bad in [
            "",
            "abc/../x",
            "abc def",
            "abc-def",
            "trace:1",
            "..",
            "a\0b",
            &"x".repeat(65),
        ] {
            assert!(!valid_trace_id(bad), "must reject {bad:?}");
        }
    }

    #[test]
    fn cache_path_sanitizes_the_run_id_and_keys_on_the_pair() {
        let p = cache_path(Path::new("/state"), "owner/repo#1", "deadbeef");
        assert_eq!(
            p,
            PathBuf::from("/state/flow-cache/owner-repo-1--deadbeef.html")
        );
        // Distinct traces for the same run get distinct cache entries.
        assert_ne!(p, cache_path(Path::new("/state"), "owner/repo#1", "cafe"));
    }

    /// Restore the DD env after a test that rewrote it.
    struct EnvRestore(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl EnvRestore {
        fn capture() -> Self {
            EnvRestore(
                ["DD_API_KEY", "DD_APP_KEY", "DD_SITE", "DD_API_URL"]
                    .into_iter()
                    .map(|v| (v, std::env::var_os(v)))
                    .collect(),
            )
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (var, prior) in self.0.drain(..) {
                match prior {
                    Some(val) => unsafe { std::env::set_var(var, val) },
                    None => unsafe { std::env::remove_var(var) },
                }
            }
        }
    }

    #[test]
    fn datadog_env_ready_names_the_missing_var_and_resolves_the_site() {
        let _g = crate::ENV_LOCK.blocking_lock();
        let _restore = EnvRestore::capture();

        unsafe {
            std::env::remove_var("DD_API_KEY");
            std::env::set_var("DD_APP_KEY", "app");
            std::env::remove_var("DD_SITE");
            std::env::remove_var("DD_API_URL");
        }
        assert_eq!(datadog_env_ready().expect_err("no api key"), "DD_API_KEY");

        // An empty value is as good as unset.
        unsafe {
            std::env::set_var("DD_API_KEY", "api");
            std::env::set_var("DD_APP_KEY", "");
        }
        assert_eq!(datadog_env_ready().expect_err("no app key"), "DD_APP_KEY");

        unsafe {
            std::env::set_var("DD_APP_KEY", "app");
        }
        let dd = datadog_env_ready().expect("both keys set");
        assert_eq!(
            dd.search_url, "https://api.datadoghq.com/api/v2/spans/events/search",
            "the US1 site is the default"
        );
        unsafe {
            std::env::set_var("DD_SITE", "datadoghq.eu");
        }
        assert_eq!(
            datadog_env_ready().expect("ready").search_url,
            "https://api.datadoghq.eu/api/v2/spans/events/search"
        );
        unsafe {
            std::env::set_var("DD_API_URL", "http://127.0.0.1:9/");
        }
        assert_eq!(
            datadog_env_ready().expect("ready").search_url,
            "http://127.0.0.1:9/api/v2/spans/events/search",
            "an explicit API base wins over the site"
        );
    }

    fn access(server: &MockServer) -> DatadogAccess {
        DatadogAccess {
            api_key: "dummy-api".to_string(),
            app_key: "dummy-app".to_string(),
            search_url: format!("{}/api/v2/spans/events/search", server.uri()),
        }
    }

    /// The span search is the engine's `--dd-trace` contract: the trace id spliced into the
    /// filter, both keys as headers, the cursor followed until the last page, and the folded page
    /// written to `out_html`.
    #[tokio::test]
    async fn render_flow_pages_through_the_span_search_and_writes_the_page() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/spans/events/search"))
            .and(header("DD-API-KEY", "dummy-api"))
            .and(header("DD-APPLICATION-KEY", "dummy-app"))
            .and(body_partial_json(
                json!({"data": {"attributes": {"filter": {
                    "query": "trace_id:deadbeef -resource_name:heartbeat"
                }}}}),
            ))
            .and(body_partial_json(
                json!({"data": {"attributes": {"page": {"cursor": "p2"}}}}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"attributes": {"service": "crucible", "resource_name": "iteration"}}],
                "meta": {"page": {}}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v2/spans/events/search"))
            .and(header("DD-API-KEY", "dummy-api"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"attributes": {"service": "crucible", "resource_name": "run"}}],
                "meta": {"page": {"after": "p2"}}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().expect("dir");
        let session = dir.path().join("session.jsonl");
        std::fs::write(&session, "{}\n").expect("session");
        let out = dir.path().join("out.html");

        render_flow(&access(&server), &session, "deadbeef", &out)
            .await
            .expect("render succeeds");
        let html = std::fs::read_to_string(&out).expect("out");
        assert!(html.contains("<html"), "a rendered page: {html:.80}");
    }

    #[tokio::test]
    async fn render_flow_carries_a_dd_failure_and_writes_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(403).set_body_string("{\"errors\": [\"Forbidden\"]}\n"),
            )
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("dir");
        let session = dir.path().join("session.jsonl");
        std::fs::write(&session, "{}\n").expect("session");
        let out = dir.path().join("out.html");

        let detail = render_flow(&access(&server), &session, "abc", &out)
            .await
            .expect_err("must fail");
        assert!(
            detail.contains("403") && detail.contains("Forbidden"),
            "{detail}"
        );
        assert!(!out.exists(), "a failed render leaves no page behind");
    }

    #[tokio::test]
    async fn render_flow_reports_an_unreadable_session_before_any_fetch() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
            .expect(0)
            .mount(&server)
            .await;
        let msg = render_flow(
            &access(&server),
            Path::new("/nope/session.jsonl"),
            "abc",
            Path::new("/nope.html"),
        )
        .await
        .expect_err("must fail");
        assert!(msg.contains("reading /nope/session.jsonl"), "{msg}");
    }

    #[test]
    fn search_body_first_and_follow_pages_match_the_export_spec() {
        let b = search_body("6893a2b1c4d5e6f7", None);
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

        let b = search_body("abc123", Some("eyJhZnRlciI6"));
        assert_eq!(b["data"]["attributes"]["page"]["cursor"], "eyJhZnRlciI6");
    }

    #[test]
    fn parse_page_extracts_spans_and_tolerates_a_missing_data() {
        let (batch, cursor) = parse_page(
            r#"{"data": [{"a": 1}, {"a": 2}], "meta": {"page": {"after": "next-cursor"}}}"#,
        )
        .expect("parses");
        assert_eq!(batch.len(), 2);
        assert_eq!(cursor.as_deref(), Some("next-cursor"));
        let (batch, cursor) = parse_page(r#"{"data": null, "meta": {}}"#).expect("parses");
        assert!(batch.is_empty());
        assert!(cursor.is_none());
        assert!(parse_page("<html>oops</html>").is_err());
    }

    #[tokio::test]
    async fn prune_cache_bounds_the_dir_and_keeps_the_just_written_entry() {
        let state = tempfile::tempdir().expect("dir");
        let cache = state.path().join("flow-cache");
        std::fs::create_dir(&cache).expect("mkdir");
        for i in 0..CACHE_MAX_ENTRIES + 12 {
            std::fs::write(cache.join(format!("run-{i}--t.html")), "x").expect("w");
        }
        let just = cache.join("run-0--t.html");
        prune_cache(state.path(), &just).await.expect("prune");
        let n = std::fs::read_dir(&cache).expect("rd").count();
        // mtime ties can spare one extra entry when just_written lands in the tail.
        assert!(n <= CACHE_MAX_ENTRIES + 1, "still {n} entries");
        assert!(just.exists());
    }

    #[tokio::test]
    async fn prune_cache_missing_dir_is_a_noop() {
        let state = tempfile::tempdir().expect("dir");
        prune_cache(state.path(), Path::new("/nowhere"))
            .await
            .expect("noop");
    }
}
