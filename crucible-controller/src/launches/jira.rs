//! Controller-side Jira Cloud fetch for the adopt-by-key path.
//!
//! A Jira issue is onboarded like a scenario: an admin adopts it by key (e.g. `ACME-1234`) and
//! the controller fetches its title/body ONCE, server-side, with the operator's basic-auth creds.
//! The fetched body is then stored like a scenario's free text and rides the same scope->loop path,
//! so the loop (and the sandbox) never see Jira creds. This is the fetch primitive + the strong
//! identity type behind that flow; the `POST /api/jira` handler owns the DB write.
//!
//! Auth + endpoint mirror the broker's Jira client: basic auth (email + API token), and the
//! `/rest/api/2/issue/{KEY}` endpoint so `fields.description` comes back as PLAIN TEXT (wiki markup)
//! rather than the api/3 ADF document blob.

#![allow(clippy::disallowed_macros)]

use crate::daemon::queue::BoxFuture;
use crate::launches::tracker::{
    EmittedKind, IssueEmitter, NewTrackerIssue, TrackerHit, TrackerIssue, TrackerQueryError,
};
use anyhow::{Context, Result, bail};
use std::time::Duration;

/// A single GET is idempotent, so a short timeout is the only guard needed here (unlike the triage
/// sweep's paginated retries, an adopt is one human-driven request that can just surface a failure).
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// The controller's Jira creds, resolved once at startup from config and carried on
/// [`crate::api::state::ApiState`] (mirroring the runtime config store). `None` there means Jira adopt is
/// unconfigured and the endpoint answers a clear error instead of a half-configured fetch.
#[derive(Clone)]
pub struct JiraConfig {
    /// The Jira Cloud base URL, e.g. `https://example.atlassian.net` (no trailing slash).
    pub(crate) base_url: String,
    /// The account email the API token belongs to (basic-auth username).
    pub(crate) email: String,
    /// A Jira Cloud API token (basic-auth password). Confidential — never logged.
    pub(crate) api_token: String,
}

impl JiraConfig {
    /// Build from the parsed config fields, requiring all three to be present and non-empty. `None`
    /// when any is unset, so a partially-configured deploy degrades to "adopt disabled" rather than
    /// issuing a broken request.
    pub(crate) fn from_parts(
        base_url: Option<String>,
        email: Option<String>,
        api_token: Option<String>,
    ) -> Option<Self> {
        let base_url = base_url?.trim_end_matches('/').to_string();
        let email = email?;
        let api_token = api_token?;
        if base_url.is_empty() || email.is_empty() || api_token.is_empty() {
            return None;
        }
        Some(Self {
            base_url,
            email,
            api_token,
        })
    }

    /// The site label for the stored key (`jira:{site}:{PROJ-N}`), derived from the base URL's host:
    /// the first DNS label of `example.atlassian.net` is `example`. Falls back to the whole host, then
    /// to `jira`, so the key is always well-formed even for an unusual base URL. Callers may override
    /// this with an explicit site on the adopt request.
    pub(crate) fn site_label(&self) -> String {
        let after_scheme = self
            .base_url
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(&self.base_url);
        let host = after_scheme
            .split('/')
            .next()
            .unwrap_or("")
            .split(':') // drop any :port
            .next()
            .unwrap_or("");
        let first_label = host.split('.').next().unwrap_or("");
        if !first_label.is_empty() {
            first_label.to_string()
        } else if !host.is_empty() {
            host.to_string()
        } else {
            "jira".to_string()
        }
    }
}

/// A validated Jira issue identity: a site label + a `PROJ-N` issue key split into its project and
/// number. Constructing one guarantees the key parses, so the stored `jira:{site}:{PROJ-N}` key
/// round-trips cleanly through [`crate::issues::model::InputKind::from_parts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JiraRef {
    site: String,
    project: String,
    number: u64,
}

impl JiraRef {
    /// Parse from a site label and a raw `PROJ-N` issue key (case preserved). `Err` on a blank site
    /// or a key that isn't `PROJECT-<number>` — the caller turns that into a 422, not an `Unknown`
    /// row (adoption is human-driven, so a malformed key is a request error worth reporting).
    pub(crate) fn parse(site: &str, issue_key: &str) -> Result<Self> {
        let site = site.trim();
        if site.is_empty() {
            bail!("site must be non-empty");
        }
        if site.contains(':') {
            bail!("site must not contain a colon");
        }
        let issue_key = issue_key.trim();
        let (project, num) = issue_key
            .rsplit_once('-')
            .with_context(|| format!("issue key `{issue_key}` is not PROJECT-NUMBER"))?;
        if project.is_empty() {
            bail!("issue key `{issue_key}` has an empty project");
        }
        let number: u64 = num
            .parse()
            .with_context(|| format!("issue key `{issue_key}` has a non-numeric issue number"))?;
        Ok(Self {
            site: site.to_string(),
            project: project.to_string(),
            number,
        })
    }

    /// The `PROJ-N` issue key as Jira's REST API and `/browse/` URLs expect it.
    pub(crate) fn issue_key(&self) -> String {
        format!("{}-{}", self.project, self.number)
    }

    /// The stored `issues.key` for this ref — the `jira:{site}:{PROJ-N}` form
    /// [`crate::issues::model::InputKind::from_parts`] decodes back into `InputKind::Jira`.
    pub(crate) fn storage_key(&self) -> String {
        format!("jira:{}:{}", self.site, self.issue_key())
    }
}

/// The fetched issue content the adopt path stores: the loop only ever sees these two strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JiraIssue {
    pub(crate) title: String,
    pub(crate) body: String,
}

/// Fetch one Jira issue's title + body by key. Takes the creds explicitly (no env reads), so a test
/// can point `cfg.base_url` at a local listener without racing process-global env. The issue key is
/// logged (it's not sensitive); the body never is.
#[tracing::instrument(name = "jira.fetch_issue", skip(cfg), fields(otel.kind = "client", peer.service = "jira", issue = %jira_ref.issue_key()), err)]
pub(crate) async fn fetch_jira_issue(cfg: &JiraConfig, jira_ref: &JiraRef) -> Result<JiraIssue> {
    let tracker = JiraTracker::new(cfg.clone())?;
    let issue = tracker.fetch(&jira_ref.issue_key()).await?;
    Ok(JiraIssue {
        title: issue.title,
        body: issue.body.unwrap_or_default(),
    })
}

/// Search pages per sweep runaway guard (a page is [`SEARCH_PAGE_SIZE`] issues).
const MAX_SEARCH_PAGES: usize = 50;
const SEARCH_PAGE_SIZE: u32 = 100;

/// Slack subtracted from the watermark before it goes into JQL: naive JQL datetimes are read in
/// the *account's* timezone, so an account ahead of UTC could otherwise miss updates. Re-seen
/// issues are idempotent upserts, so a day of slop only costs refetches.
const WATERMARK_SLACK_HOURS: i64 = 24;

/// The Jira read/search/comment client: api/2 for issue reads + comments (plain-text bodies), api/3
/// `/search/jql` for search (api/2 search is removed upstream, HTTP 410). Cheap to clone.
#[derive(Clone)]
pub struct JiraTracker {
    cfg: JiraConfig,
    client: reqwest::Client,
}

/// The full api/2 issue payload: known fields are pulled out by name, the rest becomes
/// [`TrackerIssue::extra_fields`].
#[derive(serde::Deserialize)]
struct RawFullIssue {
    fields: serde_json::Map<String, serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct RawSearch {
    issues: Vec<RawSearchIssue>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(serde::Deserialize)]
struct RawSearchIssue {
    key: String,
    fields: RawSearchFields,
}

#[derive(serde::Deserialize)]
struct RawSearchFields {
    updated: String,
}

#[derive(serde::Deserialize)]
struct RawComment {
    id: String,
}

/// Bail with the status + a capped body excerpt (the body may echo the request — never let issue
/// content into an error).
async fn expect_success(resp: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let detail = resp.text().await.unwrap_or_default();
    bail!(
        "jira API {status} {what}: {}",
        detail.chars().take(200).collect::<String>()
    )
}

/// Render a stored watermark (Jira's own `updated` string) into a JQL-safe `yyyy-MM-dd HH:mm`,
/// minus [`WATERMARK_SLACK_HOURS`].
fn render_watermark(raw: &str) -> Result<String> {
    let ts = jiff::Timestamp::strptime("%Y-%m-%dT%H:%M:%S%.f%z", raw)
        .or_else(|_| raw.parse::<jiff::Timestamp>())
        .with_context(|| format!("unparseable search watermark `{raw}`"))?;
    let slacked = ts
        .checked_sub(jiff::Span::new().hours(WATERMARK_SLACK_HOURS))
        .context("applying watermark slack")?;
    Ok(slacked.strftime("%Y-%m-%d %H:%M").to_string())
}

/// The impl owns ordering (the trait promises oldest-first), so a stored query bringing its own
/// ORDER BY would nest invalidly inside the parens — reject it up front.
impl crate::launches::tracker::TrackerSearch for JiraTracker {
    fn check_query(&self, query: &str) -> std::result::Result<(), TrackerQueryError> {
        if query.trim().is_empty() {
            return Err(TrackerQueryError::Empty);
        }
        if query.to_ascii_lowercase().contains("order by") {
            return Err(TrackerQueryError::OwnOrdering);
        }
        Ok(())
    }

    fn search(
        &self,
        query: &str,
        watermark: Option<&str>,
    ) -> crate::daemon::queue::BoxFuture<Result<Vec<TrackerHit>>> {
        let this = self.clone();
        let query = query.to_string();
        let watermark = watermark.map(str::to_string);
        Box::pin(async move { this.search(&query, watermark.as_deref()).await })
    }
}

impl JiraTracker {
    pub(crate) fn new(cfg: JiraConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("crucible-controller-jira")
            .timeout(FETCH_TIMEOUT)
            .build()
            .context("building the Jira API client")?;
        Ok(JiraTracker { cfg, client })
    }

    fn get(&self, url: &str) -> reqwest::RequestBuilder {
        self.client
            .get(url)
            .basic_auth(&self.cfg.email, Some(&self.cfg.api_token))
            .header(reqwest::header::ACCEPT, "application/json")
    }

    async fn fetch(&self, id: &str) -> Result<TrackerIssue> {
        let url = format!("{}/rest/api/2/issue/{id}", self.cfg.base_url);
        let resp = self
            .get(&url)
            .send()
            .await
            .context("sending the Jira issue request")?;
        let resp = expect_success(resp, &format!("fetching {id}")).await?;
        let raw: RawFullIssue = resp
            .json()
            .await
            .with_context(|| format!("decoding Jira issue {id}"))?;
        let mut fields = raw.fields;

        let title = match fields.remove("summary") {
            Some(serde_json::Value::String(s)) => s,
            _ => bail!("jira issue {id} has no summary"),
        };
        let body = match fields.remove("description") {
            Some(serde_json::Value::String(s)) => Some(s),
            _ => None,
        };
        let labels = match fields.remove("labels") {
            Some(serde_json::Value::Array(items)) => items
                .into_iter()
                .filter_map(|v| match v {
                    serde_json::Value::String(s) => Some(s),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        let updated_at = match fields.remove("updated") {
            Some(serde_json::Value::String(s)) => s,
            _ => String::new(),
        };
        // `status` stays in extra_fields (its name is information); only the category maps.
        let open = fields
            .get("status")
            .and_then(|s| s.pointer("/statusCategory/key"))
            .and_then(|v| v.as_str())
            .map(|k| k != "done")
            .unwrap_or(true);

        Ok(TrackerIssue {
            id: id.to_string(),
            title,
            body,
            labels,
            url: format!("{}/browse/{id}", self.cfg.base_url),
            updated_at,
            open,
            extra_fields: fields.into_iter().filter(|(_, v)| !v.is_null()).collect(),
        })
    }

    /// The watch trigger's page walk: oldest-first hits at or after the watermark.
    async fn search(&self, query: &str, watermark: Option<&str>) -> Result<Vec<TrackerHit>> {
        <Self as crate::launches::tracker::TrackerSearch>::check_query(self, query)?;
        let jql = match watermark {
            Some(w) => format!(
                "({query}) AND updated >= \"{}\" ORDER BY updated ASC",
                render_watermark(w)?
            ),
            None => format!("({query}) ORDER BY updated ASC"),
        };
        let url = format!("{}/rest/api/3/search/jql", self.cfg.base_url);
        let mut hits = Vec::new();
        let mut page_token: Option<String> = None;
        for _ in 0..MAX_SEARCH_PAGES {
            let mut req = self.get(&url).query(&[
                ("jql", jql.as_str()),
                ("fields", "updated"),
                ("maxResults", &SEARCH_PAGE_SIZE.to_string()),
            ]);
            if let Some(t) = &page_token {
                req = req.query(&[("nextPageToken", t.as_str())]);
            }
            let resp = req
                .send()
                .await
                .context("sending the Jira search request")?;
            let resp = expect_success(resp, "searching").await?;
            let raw: RawSearch = resp
                .json()
                .await
                .context("decoding the Jira search response")?;
            hits.extend(raw.issues.into_iter().map(|i| TrackerHit {
                id: i.key,
                updated_at: i.fields.updated,
            }));
            match raw.next_page_token {
                Some(t) => page_token = Some(t),
                None => return Ok(hits),
            }
        }
        bail!("jira search exceeded {MAX_SEARCH_PAGES} pages — narrow the watch query")
    }

    // Phase-2 JiraCommentSink write-back groundwork (post-once, edit-in-place); no caller yet.
    #[allow(dead_code)]
    async fn post(&self, id: &str, body: &str) -> Result<String> {
        let url = format!("{}/rest/api/2/issue/{id}/comment", self.cfg.base_url);
        let resp = self
            .client
            .post(&url)
            .basic_auth(&self.cfg.email, Some(&self.cfg.api_token))
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await
            .context("sending the Jira comment")?;
        let resp = expect_success(resp, &format!("commenting on {id}")).await?;
        let raw: RawComment = resp
            .json()
            .await
            .context("decoding the Jira comment response")?;
        Ok(raw.id)
    }

    // Phase-2 JiraCommentSink write-back groundwork (edits the post above in place); no caller yet.
    #[allow(dead_code)]
    async fn edit(&self, id: &str, comment_id: &str, body: &str) -> Result<()> {
        let url = format!(
            "{}/rest/api/2/issue/{id}/comment/{comment_id}",
            self.cfg.base_url
        );
        let resp = self
            .client
            .put(&url)
            .basic_auth(&self.cfg.email, Some(&self.cfg.api_token))
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await
            .context("sending the Jira comment edit")?;
        expect_success(resp, &format!("editing comment {comment_id} on {id}")).await?;
        Ok(())
    }
}

/// The kind→issuetype mapping + target project for emitted issues, per deploy. Type ids differ
/// per Jira instance, so they are config, never constants.
#[derive(Debug, Clone)]
pub struct JiraEmissionCfg {
    /// Project key emitted issues land in (e.g. `ACME`).
    pub(crate) project_key: String,
    /// Issue type id for [`EmittedKind::Container`] (the epic type).
    pub(crate) container_type_id: String,
    /// Issue type id for [`EmittedKind::Task`].
    pub(crate) task_type_id: String,
}

/// [`IssueEmitter`] for Jira Cloud: `POST /rest/api/2/issue` (api/2 takes a plain-string
/// description), epic-child linking via `fields.parent` (company-managed projects).
#[derive(Clone)]
pub struct JiraEmitter {
    tracker: JiraTracker,
    emission: JiraEmissionCfg,
}

/// The create response — `key` is the `PROJ-N` the rest of the tracker API addresses.
#[derive(serde::Deserialize)]
struct RawCreated {
    key: String,
}

impl JiraEmitter {
    pub(crate) fn new(tracker: JiraTracker, emission: JiraEmissionCfg) -> Self {
        JiraEmitter { tracker, emission }
    }

    fn fields(&self, issue: NewTrackerIssue) -> serde_json::Map<String, serde_json::Value> {
        let type_id = match issue.kind {
            EmittedKind::Container => &self.emission.container_type_id,
            EmittedKind::Task => &self.emission.task_type_id,
        };
        let mut fields = serde_json::Map::new();
        fields.insert(
            "project".into(),
            serde_json::json!({ "key": self.emission.project_key }),
        );
        fields.insert("issuetype".into(), serde_json::json!({ "id": type_id }));
        fields.insert("summary".into(), serde_json::Value::String(issue.title));
        fields.insert("description".into(), serde_json::Value::String(issue.body));
        fields.insert("labels".into(), serde_json::json!(issue.labels));
        if let Some(parent) = &issue.parent {
            fields.insert("parent".into(), serde_json::json!({ "key": parent }));
        }
        // Config extras win over the built fields — that's the per-deploy escape hatch.
        fields.extend(issue.extra_fields);
        fields
    }

    async fn create(&self, issue: NewTrackerIssue) -> Result<String> {
        let fields = self.fields(issue);
        let url = format!("{}/rest/api/2/issue", self.tracker.cfg.base_url);
        let resp = self
            .tracker
            .client
            .post(&url)
            .basic_auth(&self.tracker.cfg.email, Some(&self.tracker.cfg.api_token))
            .json(&serde_json::json!({ "fields": fields }))
            .send()
            .await
            .context("sending the Jira create request")?;
        let resp = expect_success(resp, "creating an issue").await?;
        let raw: RawCreated = resp
            .json()
            .await
            .context("decoding the Jira create response")?;
        Ok(raw.key)
    }

    async fn update(&self, id: &str, issue: NewTrackerIssue) -> Result<()> {
        // Same field set as create minus project/issuetype: Jira's edit endpoint rejects
        // changing either, and an in-place patch never means a re-type anyway.
        let mut fields = self.fields(issue);
        fields.remove("project");
        fields.remove("issuetype");
        let url = format!("{}/rest/api/2/issue/{id}", self.tracker.cfg.base_url);
        let resp = self
            .tracker
            .client
            .put(&url)
            .basic_auth(&self.tracker.cfg.email, Some(&self.tracker.cfg.api_token))
            .json(&serde_json::json!({ "fields": fields }))
            .send()
            .await
            .context("sending the Jira update request")?;
        expect_success(resp, "updating an issue").await?;
        Ok(())
    }

    async fn web_link(&self, id: &str, link_url: &str, title: &str) -> Result<()> {
        // `globalId` = the URL: Jira upserts a remote link with a matching globalId, so a
        // re-emission refreshes the one link instead of stacking duplicates.
        let url = format!(
            "{}/rest/api/2/issue/{id}/remotelink",
            self.tracker.cfg.base_url
        );
        let resp = self
            .tracker
            .client
            .post(&url)
            .basic_auth(&self.tracker.cfg.email, Some(&self.tracker.cfg.api_token))
            .json(&serde_json::json!({
                "globalId": link_url,
                "object": { "url": link_url, "title": title }
            }))
            .send()
            .await
            .context("sending the Jira remote-link request")?;
        expect_success(resp, "adding a web link").await?;
        Ok(())
    }
}

impl IssueEmitter for JiraEmitter {
    fn create_issue(&self, issue: NewTrackerIssue) -> BoxFuture<Result<String>> {
        let this = self.clone();
        Box::pin(async move { this.create(issue).await })
    }

    fn update_issue(&self, id: &str, issue: NewTrackerIssue) -> BoxFuture<Result<()>> {
        let this = self.clone();
        let id = id.to_string();
        Box::pin(async move { this.update(&id, issue).await })
    }

    fn add_web_link(&self, id: &str, url: &str, title: &str) -> BoxFuture<Result<()>> {
        let this = self.clone();
        let (id, url, title) = (id.to_string(), url.to_string(), title.to_string());
        Box::pin(async move { this.web_link(&id, &url, &title).await })
    }
}

/// The trackers the controller config carries credentials for: Jira when the `jira_*` trio is
/// set. Nothing else exists yet.
pub fn trackers(jira: Option<JiraConfig>) -> crate::launches::tracker::Trackers {
    let mut trackers = crate::launches::tracker::Trackers::default();
    if let Some(cfg) = jira {
        match JiraTracker::new(cfg) {
            Ok(tracker) => {
                trackers = trackers.with(
                    crate::launches::tracker::TrackerKind::Jira,
                    std::sync::Arc::new(tracker),
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "trackers: Jira client build failed"
                );
            }
        }
    }
    trackers
}

#[cfg(test)]
mod tests {
    use crate::issues::model::InputKind;
    use crate::launches::jira::*;
    use crate::launches::tracker::{TrackerQueryError, TrackerSearch};
    use std::sync::{Arc, Mutex};

    /// Serve canned JSON bodies over real HTTP, one per connection, capturing each raw request.
    /// Returns (addr, captured-requests) — the reqwest path is exercised for real, no mocks.
    fn spawn_server(bodies: Vec<String>) -> (std::net::SocketAddr, Arc<Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let captured = Arc::new(Mutex::new(Vec::new()));
        let cap = captured.clone();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for body in bodies {
                let (mut socket, _) = listener.accept().expect("accept");
                // Read the WHOLE request (headers, then Content-Length worth of body). A single
                // read can return the headers alone; answering then would (a) capture a torn
                // request and (b) close the socket while the client is still writing the body,
                // which surfaces as "Connection reset by peer".
                let mut req = Vec::new();
                let mut buf = [0u8; 8192];
                let header_end = loop {
                    let n = socket.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    req.extend_from_slice(&buf[..n]);
                    if let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(pos + 4);
                    }
                };
                if let Some(header_end) = header_end {
                    let headers = String::from_utf8_lossy(&req[..header_end]).to_lowercase();
                    let content_length = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    while req.len() < header_end + content_length {
                        let n = socket.read(&mut buf).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        req.extend_from_slice(&buf[..n]);
                    }
                }
                cap.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&req).to_string());
                // `Connection: close` so the client never pools this socket: each response ends
                // the connection and the next request re-connects to a fresh accept. Without it,
                // reqwest reuses the (already closed) keep-alive socket for the next request and
                // a non-idempotent POST fails with a connection reset instead of retrying.
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(resp.as_bytes());
            }
        });
        (addr, captured)
    }

    fn tracker_for(addr: std::net::SocketAddr) -> JiraTracker {
        JiraTracker::new(JiraConfig {
            base_url: format!("http://{addr}"),
            email: "bot@example.com".to_string(),
            api_token: "tok".to_string(),
        })
        .expect("client builds")
    }

    #[tokio::test]
    async fn tracker_fetch_maps_common_fields_and_keeps_extras() {
        let payload = serde_json::json!({
            "fields": {
                "summary": "Fix the router",
                "description": "steps",
                "labels": ["agentops", "llm-d"],
                "updated": "2026-07-22T14:30:00.000+0000",
                "status": {"name": "Closed", "statusCategory": {"key": "done"}},
                "customfield_10014": "ACME-9000",
                "assignee": null
            }
        });
        let (addr, requests) = spawn_server(vec![payload.to_string()]);
        let t = tracker_for(addr);

        let issue = t.fetch("ACME-1").await.expect("fetch");
        assert_eq!(issue.title, "Fix the router");
        assert_eq!(issue.body.as_deref(), Some("steps"));
        assert_eq!(issue.labels, vec!["agentops", "llm-d"]);
        assert_eq!(issue.updated_at, "2026-07-22T14:30:00.000+0000");
        assert!(!issue.open, "done status category means closed");
        assert_eq!(issue.url, format!("http://{addr}/browse/ACME-1"));
        assert_eq!(
            issue
                .extra_fields
                .get("customfield_10014")
                .and_then(|v| v.as_str()),
            Some("ACME-9000"),
            "unmapped fields ride in extra_fields"
        );
        assert!(
            issue.extra_fields.contains_key("status"),
            "status stays lossless in extra_fields"
        );
        assert!(
            !issue.extra_fields.contains_key("assignee"),
            "null fields are dropped"
        );
        assert!(requests.lock().unwrap()[0].starts_with("GET /rest/api/2/issue/ACME-1"));
    }

    #[tokio::test]
    async fn tracker_search_paginates_and_renders_the_watermark_clause() {
        let page1 = serde_json::json!({
            "issues": [{"key": "PROJ-1", "fields": {"updated": "2026-07-20T00:00:00.000+0000"}}],
            "nextPageToken": "tok2"
        });
        let page2 = serde_json::json!({
            "issues": [{"key": "PROJ-2", "fields": {"updated": "2026-07-21T00:00:00.000+0000"}}]
        });
        let (addr, requests) = spawn_server(vec![page1.to_string(), page2.to_string()]);
        let t = tracker_for(addr);

        let hits = t
            .search("project = PROJ", Some("2026-07-22T14:30:00.000+0000"))
            .await
            .expect("search");
        assert_eq!(
            hits.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            vec!["PROJ-1", "PROJ-2"]
        );

        let reqs = requests.lock().unwrap();
        assert!(reqs[0].starts_with("GET /rest/api/3/search/jql?"));
        // 24h slack: the 07-22 14:30 watermark renders as 07-21 14:30 (query-encoded).
        assert!(
            reqs[0].contains("2026-07-21+14%3A30") || reqs[0].contains("2026-07-21%2014%3A30"),
            "watermark clause missing: {}",
            reqs[0]
        );
        assert!(
            reqs[1].contains("nextPageToken=tok2"),
            "second page must carry the token"
        );
    }

    #[tokio::test]
    async fn tracker_search_rejects_a_query_with_its_own_order_by() {
        let t = tracker_for("127.0.0.1:9".parse().expect("addr"));
        let err = t
            .search("project = X ORDER BY created", None)
            .await
            .expect_err("must reject");
        assert!(format!("{err:#}").contains("ORDER BY"));
    }

    #[test]
    fn tracker_query_validation_returns_typed_errors() {
        let t = tracker_for("127.0.0.1:9".parse().expect("addr"));

        assert_eq!(
            t.check_query("  ")
                .expect_err("blank query must be rejected"),
            TrackerQueryError::Empty
        );
        assert_eq!(
            t.check_query("project = X order by created")
                .expect_err("query ordering must be rejected"),
            TrackerQueryError::OwnOrdering
        );
    }

    #[tokio::test]
    async fn tracker_comments_post_then_edit() {
        let (addr, requests) = spawn_server(vec![
            serde_json::json!({"id": "10001"}).to_string(),
            "{}".to_string(),
        ]);
        let t = tracker_for(addr);

        let cid = t.post("PROJ-1", "adopted").await.expect("post");
        assert_eq!(cid, "10001");
        t.edit("PROJ-1", &cid, "terminal").await.expect("edit");

        let reqs = requests.lock().unwrap();
        assert!(reqs[0].starts_with("POST /rest/api/2/issue/PROJ-1/comment"));
        assert!(reqs[1].starts_with("PUT /rest/api/2/issue/PROJ-1/comment/10001"));
    }

    #[test]
    fn watermark_renders_utc_minus_slack() {
        assert_eq!(
            render_watermark("2026-07-22T14:30:00.000+0000").expect("parse"),
            "2026-07-21 14:30"
        );
        // RFC3339 (a tracker-agnostic caller may hand one back) parses too.
        assert_eq!(
            render_watermark("2026-07-22T14:30:00Z").expect("parse"),
            "2026-07-21 14:30"
        );
        assert!(render_watermark("not a date").is_err());
    }

    #[tokio::test]
    async fn emitter_creates_container_then_child_task_under_it() {
        let (addr, requests) = spawn_server(vec![
            serde_json::json!({"id": "1", "key": "PROJ-100"}).to_string(),
            serde_json::json!({"id": "2", "key": "PROJ-101"}).to_string(),
        ]);
        let emitter = JiraEmitter::new(
            tracker_for(addr),
            JiraEmissionCfg {
                project_key: "PROJ".to_string(),
                container_type_id: "10000".to_string(),
                task_type_id: "10002".to_string(),
            },
        );

        let epic = emitter
            .create_issue(NewTrackerIssue {
                title: "experiment: routing ablation".to_string(),
                body: "scenario + pack refs".to_string(),
                labels: vec!["agentops".to_string()],
                kind: EmittedKind::Container,
                parent: None,
                extra_fields: std::collections::BTreeMap::new(),
            })
            .await
            .expect("create epic");
        assert_eq!(epic, "PROJ-100");

        let child = emitter
            .create_issue(NewTrackerIssue {
                title: "review PR 7".to_string(),
                body: "pr link".to_string(),
                labels: vec![],
                kind: EmittedKind::Task,
                parent: Some(epic.clone()),
                extra_fields: std::collections::BTreeMap::from([(
                    "customfield_10001".to_string(),
                    serde_json::json!("team-x"),
                )]),
            })
            .await
            .expect("create child");
        assert_eq!(child, "PROJ-101");

        let reqs = requests.lock().unwrap();
        assert!(reqs[0].starts_with("POST /rest/api/2/issue"));
        let body0: serde_json::Value =
            serde_json::from_str(reqs[0].split("\r\n\r\n").nth(1).expect("body"))
                .expect("json body");
        assert_eq!(body0["fields"]["project"]["key"], "PROJ");
        assert_eq!(body0["fields"]["issuetype"]["id"], "10000");
        assert_eq!(body0["fields"]["labels"][0], "agentops");
        assert!(body0["fields"].get("parent").is_none());

        let body1: serde_json::Value =
            serde_json::from_str(reqs[1].split("\r\n\r\n").nth(1).expect("body"))
                .expect("json body");
        assert_eq!(body1["fields"]["issuetype"]["id"], "10002");
        assert_eq!(body1["fields"]["parent"]["key"], "PROJ-100");
        assert_eq!(
            body1["fields"]["customfield_10001"], "team-x",
            "config extras merged into the create body"
        );
    }

    /// Live smoke against a real Jira Cloud instance — run explicitly with
    /// `JIRA_LIVE_BASE_URL/EMAIL/TOKEN/ISSUE set` + `cargo test ... live_smoke -- --ignored`.
    /// Read-only (fetch + search); comment posting is covered by the listener tests.
    #[tokio::test]
    #[ignore = "hits a live Jira instance; needs JIRA_LIVE_* env"]
    async fn live_smoke_fetch_and_search() {
        let need = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} must be set"));
        let cfg = JiraConfig {
            base_url: need("JIRA_LIVE_BASE_URL"),
            email: need("JIRA_LIVE_EMAIL"),
            api_token: need("JIRA_LIVE_TOKEN"),
        };
        let issue_key = need("JIRA_LIVE_ISSUE");
        let project = issue_key.rsplit_once('-').expect("PROJ-N").0.to_string();
        let t = JiraTracker::new(cfg).expect("client");

        let issue = t.fetch(&issue_key).await.expect("live fetch");
        assert!(!issue.title.is_empty());
        assert!(!issue.updated_at.is_empty());
        eprintln!(
            "fetched {issue_key}: title={:?} open={} labels={:?} extra_fields={} updated={}",
            issue.title,
            issue.open,
            issue.labels,
            issue.extra_fields.len(),
            issue.updated_at
        );

        let hits = t
            .search(&format!("project = {project}"), Some(&issue.updated_at))
            .await
            .expect("live search");
        eprintln!("search since {}: {} hits", issue.updated_at, hits.len());
        assert!(
            hits.iter().any(|h| h.id == issue_key),
            "the fetched issue must appear in a search watermarked at its own updated time"
        );
    }

    #[test]
    fn jira_ref_parses_and_round_trips_through_input_kind() {
        let r = JiraRef::parse("example", "ACME-1234").expect("valid key");
        assert_eq!(r.issue_key(), "ACME-1234");
        assert_eq!(r.storage_key(), "jira:example:ACME-1234");
        assert_eq!(
            InputKind::from_parts("jira", &r.storage_key()),
            InputKind::Jira {
                site: "example".to_string(),
                project: "ACME".to_string(),
                number: 1234,
            }
        );
    }

    #[test]
    fn jira_ref_trims_and_preserves_project_case() {
        let r = JiraRef::parse("  example ", "  Infra-7 ").expect("valid key");
        assert_eq!(r.issue_key(), "Infra-7");
        assert_eq!(r.storage_key(), "jira:example:Infra-7");
    }

    #[test]
    fn jira_ref_rejects_malformed_keys() {
        for (site, key) in [
            ("example", "NOHYPHEN"),
            ("example", "PROJ-"),
            ("example", "PROJ-abc"),
            ("example", "-7"),
            ("", "PROJ-1"),
            ("red:hat", "PROJ-1"),
        ] {
            assert!(
                JiraRef::parse(site, key).is_err(),
                "site={site:?} key={key:?} should be rejected"
            );
        }
    }

    #[test]
    fn jira_config_requires_all_three_parts() {
        assert!(JiraConfig::from_parts(None, Some("e".into()), Some("t".into())).is_none());
        assert!(JiraConfig::from_parts(Some("https://x".into()), None, Some("t".into())).is_none());
        assert!(JiraConfig::from_parts(Some("https://x".into()), Some("e".into()), None).is_none());
        assert!(
            JiraConfig::from_parts(Some("https://x/".into()), Some("".into()), Some("t".into()))
                .is_none()
        );
        let cfg = JiraConfig::from_parts(
            Some("https://x/".into()),
            Some("e".into()),
            Some("t".into()),
        )
        .expect("all present");
        assert_eq!(cfg.base_url, "https://x", "trailing slash trimmed");
    }

    #[test]
    fn site_label_derives_from_base_url_host() {
        let cfg = |u: &str| JiraConfig {
            base_url: u.to_string(),
            email: "e".into(),
            api_token: "t".into(),
        };
        assert_eq!(cfg("https://example.atlassian.net").site_label(), "example");
        assert_eq!(cfg("http://localhost:8080").site_label(), "localhost");
        assert_eq!(cfg("https://jira.example.com/").site_label(), "jira");
    }

    /// A real HTTP round-trip against a one-shot local listener returning a canonical
    /// `/rest/api/2/issue/{KEY}` payload — no mock client, the actual reqwest path.
    #[tokio::test]
    async fn fetch_jira_issue_pulls_summary_and_description() -> Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 4096];
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let _ = socket.read(&mut buf).await;
            let body = serde_json::json!({
                "fields": {"summary": "Fix the router", "description": "steps to repro\nmore"}
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.flush().await;
        });
        let cfg = JiraConfig {
            base_url: format!("http://{addr}"),
            email: "me@example.com".to_string(),
            api_token: "tok".to_string(),
        };
        let r = JiraRef::parse("example", "ACME-1")?;
        let issue = fetch_jira_issue(&cfg, &r).await?;
        assert_eq!(issue.title, "Fix the router");
        assert_eq!(issue.body, "steps to repro\nmore");
        server.await.expect("server task");
        Ok(())
    }

    /// A null description (Jira's empty-body shape) decodes to an empty string, not an error.
    #[tokio::test]
    async fn fetch_jira_issue_tolerates_null_description() -> Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 4096];
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let _ = socket.read(&mut buf).await;
            let body =
                serde_json::json!({"fields": {"summary": "t", "description": null}}).to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.flush().await;
        });
        let cfg = JiraConfig {
            base_url: format!("http://{addr}"),
            email: "me@example.com".to_string(),
            api_token: "tok".to_string(),
        };
        let r = JiraRef::parse("example", "ACME-2")?;
        let issue = fetch_jira_issue(&cfg, &r).await?;
        assert_eq!(issue.body, "");
        server.await.expect("server task");
        Ok(())
    }
}
