//! The controller API client.
//!
//! Two things it does that a `curl` alias cannot:
//!
//! - **Error bodies survive.** The controller answers a bad adopt with a 422 whose body names the
//!   exact field that failed. That sentence is the whole value of the response, so it is carried
//!   verbatim into the error rather than collapsed into "request failed".
//! - **Keys get encoded.** An issue key is `owner/repo#12`. Both the slash and the hash are path
//!   syntax, and pasting one raw produces a 404 that reads like the issue doesn't exist.
//!
//! Everything below [`Transport`] — the typed methods, the DTOs, the renderers — is written once
//! and runs over either wire. The CLI dials the controller over HTTP; the controller hosts the
//! same tools by calling its own router in process. Neither the operations nor the tools know
//! which one they are on.

#![allow(clippy::disallowed_macros)]

use crate::config::{Auth, Config};
use crate::dto;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::time::Duration;

/// How a [`Client`] reaches the controller: one request in, its status and body text out.
///
/// The body comes back as text rather than a parsed value on purpose — a non-2xx body is usually
/// the sentence naming what the caller got wrong, and it has to survive verbatim.
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<(reqwest::StatusCode, String)>;

    /// GET a body that is not JSON and not necessarily UTF-8 — a run's captured file is whatever
    /// its task wrote. The default decodes through [`Transport::send`], which is correct for a
    /// text body and lossy for a binary one; a transport on a real wire overrides it.
    async fn get_bytes(&self, path: &str) -> Result<(reqwest::StatusCode, Vec<u8>)> {
        let (status, text) = self.send(reqwest::Method::GET, path, None).await?;
        Ok((status, text.into_bytes()))
    }

    /// The origin human-facing links are built against.
    fn base(&self) -> &str;

    /// Where the requests actually go, as opposed to what was configured.
    fn endpoint(&self) -> Endpoint {
        Endpoint::InProcess {
            links: self.base().to_string(),
        }
    }

    /// What each request actually carries. A wire with no per-request credential is not an
    /// unauthenticated one: the in-process transport authenticated its caller before the request
    /// existed.
    fn credential(&self) -> Credential {
        Credential::EdgeApiKey
    }
}

/// Which endpoint a [`Client`] is talking to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// `CONTROLLER_URL`.
    Configured { url: String },
    /// This controller's own router, called in process. Nothing dials `links`; it is the origin
    /// human-facing links are built against.
    InProcess { links: String },
}

impl Endpoint {
    pub fn base(&self) -> &str {
        match self {
            Endpoint::Configured { url } | Endpoint::InProcess { links: url } => url,
        }
    }
}

/// The credential a request arrives at the controller with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// Attached to every request by this process.
    Wire(Auth),
    /// Nothing on the request itself: the MCP surface checked an API key and resolved the caller
    /// before the request existed.
    EdgeApiKey,
}

/// The HTTP wire: a reqwest client and the credential it presents.
pub struct HttpTransport {
    http: reqwest::Client,
    endpoint: Endpoint,
    auth: Auth,
}

pub struct Client {
    transport: std::sync::Arc<dyn Transport>,
}

impl HttpTransport {
    pub fn connect(cfg: &Config) -> Result<Self> {
        if cfg.url.is_empty() {
            bail!("no controller to talk to: set CONTROLLER_URL, or `url` in the config file");
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .context("building the controller http client")?;
        Ok(Self {
            http,
            endpoint: Endpoint::Configured {
                url: cfg.url.clone(),
            },
            auth: cfg.auth.clone(),
        })
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.endpoint.base()))
            .header("accept", "application/json");
        for (name, value) in self.auth.headers() {
            req = req.header(name, value);
        }
        req
    }
}

#[async_trait::async_trait]
impl Transport for HttpTransport {
    /// Send, and hand back the status beside the body. Ruling on the status is the [`Client`]'s
    /// job: one caller (the draft save) needs a 409 body as an answer rather than a failure.
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<(reqwest::StatusCode, String)> {
        let req = self.req(method, path);
        let req = match body {
            Some(b) => req.json(b),
            None => req,
        };
        let resp = req
            .send()
            .await
            .with_context(|| format!("requesting {path}"))?;
        let status = resp.status();
        Ok((status, resp.text().await.unwrap_or_default()))
    }

    async fn get_bytes(&self, path: &str) -> Result<(reqwest::StatusCode, Vec<u8>)> {
        let resp = self
            .req(reqwest::Method::GET, path)
            .send()
            .await
            .with_context(|| format!("requesting {path}"))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .with_context(|| format!("reading the body of {path}"))?;
        Ok((status, bytes.to_vec()))
    }

    fn base(&self) -> &str {
        self.endpoint.base()
    }

    fn endpoint(&self) -> Endpoint {
        self.endpoint.clone()
    }

    fn credential(&self) -> Credential {
        Credential::Wire(self.auth.clone())
    }
}

impl Client {
    /// Wrap a transport that is already connected. The controller's in-process host builds one of
    /// these; the CLI goes through [`Client::connect`].
    pub fn new(transport: std::sync::Arc<dyn Transport>) -> Self {
        Client { transport }
    }

    /// Connect over HTTP.
    pub fn connect(cfg: &Config) -> Result<Self> {
        Ok(Client::new(std::sync::Arc::new(HttpTransport::connect(
            cfg,
        )?)))
    }

    pub fn base(&self) -> &str {
        self.transport.base()
    }

    /// A link into the SPA the controller serves from the same origin this client dials.
    pub fn web_url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base().trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    /// Where this client's requests actually go.
    pub fn endpoint(&self) -> Endpoint {
        self.transport.endpoint()
    }

    /// What this client's requests actually carry.
    pub fn credential(&self) -> Credential {
        self.transport.credential()
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        parse(self.send(reqwest::Method::GET, path, None).await?, path)
    }

    /// POST with a JSON body. `body: None` sends `{}` — the controller's no-body admin actions
    /// (reconcile, approve) still want a JSON content type.
    pub async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: Option<&impl Serialize>,
    ) -> Result<T> {
        let value = match body {
            Some(b) => serde_json::to_value(b).context("encoding the request body")?,
            None => serde_json::json!({}),
        };
        parse(
            self.send(reqwest::Method::POST, path, Some(&value)).await?,
            path,
        )
    }

    /// Send and enforce the status, returning the body text. A non-2xx becomes an error carrying
    /// the status and the controller's body exactly as written — that body is usually a
    /// `{"error": "…"}` naming the field the caller got wrong.
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<String> {
        let (status, text) = self.transport.send(method, path, body).await?;
        if status.is_success() {
            return Ok(text);
        }
        bail!(
            "{path} -> HTTP {status}: {}{}",
            text.trim(),
            self.hint_for(status)
        );
    }

    /// The failures whose raw body does not say what to do about it.
    fn hint_for(&self, status: reqwest::StatusCode) -> String {
        match status {
            reqwest::StatusCode::UNAUTHORIZED => {
                "\nhint: the controller rejected the bearer. Check CONTROLLER_API_TOKEN, or mint a \
                 key on the controller's Settings page."
                    .into()
            }
            reqwest::StatusCode::FORBIDDEN
                if matches!(self.credential(), Credential::Wire(Auth::Bearer { .. })) =>
            {
                "\nhint: this request carried the static bearer, which names nobody, so the \
                 controller saw `anonymous`. Use a key minted on the Settings page \
                 (CONTROLLER_API_TOKEN=crk_…)."
                    .into()
            }
            reqwest::StatusCode::FORBIDDEN => {
                "\nhint: authenticated but not on the controller's admin/operator list \
                 (CONTROLLER_ADMINS / CONTROLLER_OPERATORS)."
                    .into()
            }
            _ => String::new(),
        }
    }

    // --- the operations, one method each ---------------------------------

    pub async fn issues<T: DeserializeOwned>(
        &self,
        kind: Option<&str>,
        status: Option<&str>,
    ) -> Result<T> {
        let mut q = Query::new();
        q.push("kind", kind);
        q.push("status", status);
        self.get(&format!("/api/issues{}", q.finish())).await
    }

    pub async fn issue<T: DeserializeOwned>(&self, key: &str) -> Result<T> {
        self.get(&format!("/api/issues/{}", encode(key))).await
    }

    pub async fn turns<T: DeserializeOwned>(
        &self,
        kind: Option<&str>,
        state: Option<&str>,
    ) -> Result<T> {
        let mut q = Query::new();
        q.push("kind", kind);
        q.push("state", state);
        self.get(&format!("/api/turns{}", q.finish())).await
    }

    pub async fn turn<T: DeserializeOwned>(&self, pod: &str) -> Result<T> {
        self.get(&format!("/api/turns/{}", encode(pod))).await
    }

    pub async fn graph<T: DeserializeOwned>(&self, run_id: &str) -> Result<T> {
        self.get(&format!("/api/runs/{}/graph", encode(run_id)))
            .await
    }

    pub async fn version<T: DeserializeOwned>(&self) -> Result<T> {
        self.get("/api/version").await
    }

    pub async fn run_log<T: DeserializeOwned>(&self, run_id: &str) -> Result<T> {
        self.get(&format!("/api/runs/{}/log", encode(run_id))).await
    }

    pub async fn run_files<T: DeserializeOwned>(&self, run_id: &str) -> Result<T> {
        self.get(&format!("/api/runs/{}/files", encode(run_id)))
            .await
    }

    /// One captured file's bytes. The key's `/` separates the task directory from the declared
    /// path and must survive as a path separator, so each segment is encoded on its own.
    pub async fn run_file(&self, run_id: &str, key: &str) -> Result<Vec<u8>> {
        let key: Vec<String> = key.split('/').map(|s| encode(s).to_string()).collect();
        let path = format!("/api/runs/{}/files/{}", encode(run_id), key.join("/"));
        let (status, bytes) = self.transport.get_bytes(&path).await?;
        if !status.is_success() {
            anyhow::bail!(
                "GET {path} failed: {status}: {}",
                String::from_utf8_lossy(&bytes)
            );
        }
        Ok(bytes)
    }

    pub async fn contracts<T: DeserializeOwned>(&self) -> Result<T> {
        self.get("/api/config/broker-contracts").await
    }

    pub async fn approvals<T: DeserializeOwned>(&self) -> Result<T> {
        self.get("/api/approvals").await
    }

    pub async fn playbooks<T: DeserializeOwned>(&self) -> Result<T> {
        self.get("/api/playbooks").await
    }

    pub async fn playbook_caps<T: DeserializeOwned>(&self) -> Result<T> {
        self.get("/api/config/playbook-caps").await
    }

    /// The secrets the caller owns or is an owner-group member of. Metadata only.
    pub async fn secrets<T: DeserializeOwned>(&self) -> Result<T> {
        self.get("/api/secrets").await
    }

    pub async fn bind_secret<T: DeserializeOwned>(
        &self,
        secret_id: &str,
        body: &serde_json::Value,
    ) -> Result<T> {
        self.post(
            &format!("/api/secrets/{}/bindings", encode(secret_id)),
            Some(body),
        )
        .await
    }

    /// The parameter schema a launch is validated against. An agent reads this before filling one
    /// in; guessing the field names is how a launch turns into a 422.
    pub async fn playbook_schema<T: DeserializeOwned>(&self, id: &str) -> Result<T> {
        self.get(&format!("/api/playbooks/{}/schema", encode(id)))
            .await
    }

    pub async fn launch<T: DeserializeOwned>(&self, id: &str, body: &impl Serialize) -> Result<T> {
        self.post(&format!("/api/playbooks/{}/launch", encode(id)), Some(body))
            .await
    }

    pub async fn playbook_runs<T: DeserializeOwned>(
        &self,
        status: Option<&str>,
        playbook: Option<&str>,
    ) -> Result<T> {
        let mut q = Query::new();
        q.push("status", status);
        q.push("playbook", playbook);
        self.get(&format!("/api/playbook-runs{}", q.finish())).await
    }

    pub async fn playbook_run<T: DeserializeOwned>(&self, key: &str) -> Result<T> {
        self.get(&format!("/api/playbook-runs/{}", encode(key)))
            .await
    }

    pub async fn runs<T: DeserializeOwned>(
        &self,
        status: Option<&str>,
        repo: Option<&str>,
        dispatch_target: Option<&str>,
        limit: Option<i64>,
    ) -> Result<T> {
        let mut q = Query::new();
        q.push("status", status);
        q.push("repo", repo);
        q.push("dispatch_target", dispatch_target);
        q.push("limit", limit.map(|l| l.to_string()).as_deref());
        self.get(&format!("/api/runs{}", q.finish())).await
    }

    pub async fn run<T: DeserializeOwned>(&self, run_id: &str) -> Result<T> {
        self.get(&format!("/api/runs/{}", encode(run_id))).await
    }

    pub async fn schedules<T: DeserializeOwned>(&self) -> Result<T> {
        self.get("/api/schedules").await
    }

    pub async fn watches<T: DeserializeOwned>(&self) -> Result<T> {
        self.get("/api/watches").await
    }

    pub async fn watch<T: DeserializeOwned>(&self, id: &str) -> Result<T> {
        self.get(&format!("/api/watches/{}", encode(id))).await
    }

    pub async fn whoami(&self) -> Result<dto::Whoami> {
        self.get("/api/whoami").await
    }

    pub async fn park<T: DeserializeOwned>(&self, key: &str, reason: &str) -> Result<T> {
        let body = serde_json::json!({ "reason": reason });
        self.post(&format!("/api/issues/{}/park", encode(key)), Some(&body))
            .await
    }

    pub async fn unpark<T: DeserializeOwned>(&self, key: &str, reason: Option<&str>) -> Result<T> {
        let body = serde_json::json!({ "reason": reason });
        self.post(&format!("/api/issues/{}/unpark", encode(key)), Some(&body))
            .await
    }

    pub async fn bump<T: DeserializeOwned>(
        &self,
        key: &str,
        priority: i64,
        reason: Option<&str>,
    ) -> Result<T> {
        let body = serde_json::json!({ "priority": priority, "reason": reason });
        self.post(&format!("/api/issues/{}/bump", encode(key)), Some(&body))
            .await
    }

    pub async fn redispatch<T: DeserializeOwned>(
        &self,
        key: &str,
        justification: &str,
    ) -> Result<T> {
        let body = serde_json::json!({ "justification": justification });
        self.post(
            &format!("/api/issues/{}/redispatch", encode(key)),
            Some(&body),
        )
        .await
    }

    pub async fn reconcile<T: DeserializeOwned>(&self) -> Result<T> {
        self.post("/api/reconcile", None::<&()>).await
    }

    pub async fn approve<T: DeserializeOwned>(&self, key: &str) -> Result<T> {
        self.post(
            &format!("/api/scenarios/{}/approve", encode(key)),
            None::<&()>,
        )
        .await
    }

    pub async fn adopt<T: DeserializeOwned>(&self, body: &AdoptBody) -> Result<T> {
        self.post("/api/scenarios", Some(body)).await
    }

    pub async fn import_pack<T: DeserializeOwned>(
        &self,
        repo: &str,
        git_ref: Option<&str>,
        path: Option<&str>,
    ) -> Result<T> {
        let body = serde_json::json!({ "repo": repo, "git_ref": git_ref, "path": path });
        self.post("/api/playbooks/imports", Some(&body)).await
    }

    pub async fn create_draft<T: DeserializeOwned>(
        &self,
        id: &str,
        description: &str,
        template: Option<&str>,
    ) -> Result<T> {
        let body =
            serde_json::json!({ "id": id, "description": description, "template": template });
        self.post("/api/playbook-drafts", Some(&body)).await
    }

    pub async fn draft_files<T: DeserializeOwned>(
        &self,
        id: &str,
        version: Option<i64>,
    ) -> Result<T> {
        let mut q = Query::new();
        q.push("version", version.map(|v| v.to_string()).as_deref());
        self.get(&format!(
            "/api/playbook-drafts/{}/files{}",
            encode(id),
            q.finish()
        ))
        .await
    }

    /// What a stored version compiled to: the same form, graph and diagnostics the studio paints,
    /// without launching it.
    pub async fn draft_preview<T: DeserializeOwned>(
        &self,
        id: &str,
        version: Option<i64>,
    ) -> Result<T> {
        let mut q = Query::new();
        q.push("version", version.map(|v| v.to_string()).as_deref());
        self.get(&format!(
            "/api/playbook-drafts/{}/preview{}",
            encode(id),
            q.finish()
        ))
        .await
    }

    /// Test-fire a draft. The ack is the registered launch's, so the run is watched the same way.
    pub async fn launch_draft<T: DeserializeOwned>(
        &self,
        id: &str,
        body: &impl Serialize,
    ) -> Result<T> {
        self.post(
            &format!("/api/playbook-drafts/{}/launch", encode(id)),
            Some(body),
        )
        .await
    }

    /// `DELETE /api/playbook-drafts/{id}`: a 204 with no body, so nothing to parse.
    pub async fn delete_draft(&self, id: &str) -> Result<()> {
        self.send(
            reqwest::Method::DELETE,
            &format!("/api/playbook-drafts/{}", encode(id)),
            None,
        )
        .await
        .map(|_| ())
    }

    /// Export the newest compiling version as a PR against `repo`.
    pub async fn graduate_draft<T: DeserializeOwned>(
        &self,
        id: &str,
        repo: &str,
        path: Option<&str>,
    ) -> Result<T> {
        let body = serde_json::json!({ "repo": repo, "path": path });
        self.post(
            &format!("/api/playbook-drafts/{}/graduate", encode(id)),
            Some(&body),
        )
        .await
    }

    /// Register the newest compiling version as `playbook` with no review. `None` re-pins the
    /// playbook the draft last published into, else the one it was seeded from.
    pub async fn publish_draft<T: DeserializeOwned>(
        &self,
        id: &str,
        playbook: Option<&str>,
    ) -> Result<T> {
        let body = serde_json::json!({ "playbook": playbook });
        self.post(
            &format!("/api/playbook-drafts/{}/publish", encode(id)),
            Some(&body),
        )
        .await
    }

    /// Append a draft version. A base that is no longer the newest save comes back as
    /// [`DraftSave::Stale`] rather than an error: the writer's next move is to re-read and merge,
    /// which needs the version that overtook it.
    pub async fn save_draft(
        &self,
        id: &str,
        files: &std::collections::BTreeMap<String, String>,
        base_version: i64,
    ) -> Result<DraftSave> {
        let path = format!("/api/playbook-drafts/{}/versions", encode(id));
        let body = serde_json::json!({ "files": files, "base_version": base_version });
        let (status, text) = self
            .transport
            .send(reqwest::Method::POST, &path, Some(&body))
            .await?;
        if status.is_success() {
            return Ok(DraftSave::Saved(parse(text, &path)?));
        }
        if status == reqwest::StatusCode::CONFLICT
            && let Ok(stale) = serde_json::from_str::<dto::StaleBase>(&text)
        {
            return Ok(DraftSave::Stale(stale));
        }
        bail!(
            "{path} -> HTTP {status}: {}{}",
            text.trim(),
            self.hint_for(status)
        );
    }
}

/// What a draft save landed: the stored version, or the refusal naming the save that overtook it.
pub enum DraftSave {
    Saved(serde_json::Value),
    Stale(dto::StaleBase),
}

/// The `POST /api/scenarios` request. `authoritative` and the two optionals default server-side,
/// but they are spelled out here so the shape is readable next to the CLI flags that fill it.
#[derive(Debug, Serialize, Default)]
pub struct AdoptBody {
    pub title: String,
    pub body: String,
    pub affected_repos: Vec<String>,
    pub justification: String,
    pub authoritative: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codegen_contract: Option<String>,
}

fn parse<T: DeserializeOwned>(body: String, what: &str) -> Result<T> {
    serde_json::from_str(&body).with_context(|| {
        format!(
            "parsing the {what} response: {}",
            crate::render::truncate(&body, 400)
        )
    })
}

/// Accumulates `?a=1&b=2`, skipping absent filters.
struct Query(Vec<String>);

impl Query {
    fn new() -> Self {
        Query(Vec::new())
    }

    fn push(&mut self, name: &str, value: Option<&str>) {
        if let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) {
            self.0.push(format!("{name}={}", encode(v)));
        }
    }

    fn finish(self) -> String {
        if self.0.is_empty() {
            String::new()
        } else {
            format!("?{}", self.0.join("&"))
        }
    }
}

/// Percent-encode one path segment or query value.
///
/// RFC 3986 unreserved set only. Deliberately stricter than a URL library's per-component rules:
/// an issue key is `owner/repo#12`, and both `/` and `#` MUST encode or the request goes to a
/// different path (or drops everything after the fragment marker).
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this prevents: `GET /api/issues/owner/repo#12` reaches a route that doesn't exist
    /// and 404s, which reads exactly like "no such issue".
    #[test]
    fn issue_keys_encode_their_slash_and_hash() {
        assert_eq!(encode("owner/repo#12"), "owner%2Frepo%2312");
        assert_eq!(
            encode("scenario:0192f4a1-8c3e-7000-9abc-1234567890ab"),
            "scenario%3A0192f4a1-8c3e-7000-9abc-1234567890ab"
        );
    }

    #[test]
    fn encoding_leaves_the_unreserved_set_alone() {
        assert_eq!(encode("abcXYZ019-_.~"), "abcXYZ019-_.~");
        assert_eq!(encode("a b"), "a%20b");
    }

    #[test]
    fn a_query_with_no_filters_is_no_query_string() {
        let mut q = Query::new();
        q.push("kind", None);
        q.push("status", Some("   "));
        assert_eq!(q.finish(), "");
    }

    #[test]
    fn query_filters_are_encoded_and_joined() {
        let mut q = Query::new();
        q.push("kind", Some("scenario"));
        q.push("status", Some("awaiting approval"));
        assert_eq!(q.finish(), "?kind=scenario&status=awaiting%20approval");
    }

    /// A body the controller sent that this binary can't parse must show the body, not just
    /// "expected struct" — otherwise the operator has no idea what came back.
    #[test]
    fn a_parse_failure_quotes_the_body() {
        let e = parse::<dto::Whoami>("<html>502 Bad Gateway</html>".into(), "GET /api/whoami")
            .expect_err("html is not a Whoami");
        let text = format!("{e:#}");
        assert!(text.contains("502 Bad Gateway"), "{text}");
        assert!(text.contains("GET /api/whoami"), "{text}");
    }

    /// The link a human is handed hangs off the base this client dialled.
    #[test]
    fn web_urls_hang_off_the_base_without_doubling_the_slash() {
        /// A link is built from the base alone, so the wire underneath it is irrelevant here.
        struct BaseOnly(String);
        #[async_trait::async_trait]
        impl Transport for BaseOnly {
            async fn send(
                &self,
                _method: reqwest::Method,
                _path: &str,
                _body: Option<&serde_json::Value>,
            ) -> Result<(reqwest::StatusCode, String)> {
                unreachable!("web_url sends nothing")
            }
            fn base(&self) -> &str {
                &self.0
            }
        }
        let client = |url: &str| Client::new(std::sync::Arc::new(BaseOnly(url.to_string())));
        assert_eq!(
            client("https://crucible.example.com").web_url("/playbooks/import/abc"),
            "https://crucible.example.com/playbooks/import/abc"
        );
        assert_eq!(
            client("http://127.0.0.1:18080/").web_url("playbooks/drafts/calibrate"),
            "http://127.0.0.1:18080/playbooks/drafts/calibrate"
        );
    }

    /// A save that another editor overtook must come back as data, not as an error: the agent's
    /// next move needs `current_version`, and an error string is where that goes to die. The
    /// controller's real 409 body is what this stands in with.
    #[tokio::test]
    async fn a_stale_draft_save_comes_back_as_the_refusal_not_an_error() {
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::routing::{get, post};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        async fn versions(
            State(saves): State<Arc<AtomicUsize>>,
            body: String,
        ) -> (StatusCode, String) {
            let sent: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
            if sent["base_version"] == serde_json::json!(7) {
                saves.fetch_add(1, Ordering::SeqCst);
                return (
                    StatusCode::OK,
                    r#"{"version":8,"saved_by":"agent-7","saved_at":"2026-08-23T11:00:00Z",
                        "params_schema":null,"schema_digest":"sha256:cc","graph":null,
                        "diagnostics":[]}"#
                        .into(),
                );
            }
            (
                StatusCode::CONFLICT,
                r#"{"error":"this save edited version 6, but wynn saved version 8 at 2026-08-23T11:00:00Z; re-read that version and merge",
                    "base_version":6,"current_version":8,"saved_by":"wynn",
                    "saved_at":"2026-08-23T11:00:00Z"}"#
                    .into(),
            )
        }

        let saves = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/api/playbook-drafts/{id}/versions", post(versions))
            .with_state(saves.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });

        let cfg = crate::config::Config {
            url,
            auth: Auth::None,
        };
        let client = Client::connect(&cfg).expect("connect");
        let files =
            std::collections::BTreeMap::from([("wf.crux".to_string(), "task a {}".to_string())]);

        match client
            .save_draft("calibrate", &files, 7)
            .await
            .expect("save")
        {
            DraftSave::Saved(v) => assert_eq!(v["version"], 8),
            DraftSave::Stale(s) => panic!("a fresh base must save: {}", s.error),
        }
        match client
            .save_draft("calibrate", &files, 6)
            .await
            .expect("refusal")
        {
            DraftSave::Stale(s) => {
                assert_eq!((s.base_version, s.current_version), (6, 8));
                assert_eq!(s.saved_by.as_deref(), Some("wynn"));
            }
            DraftSave::Saved(_) => panic!("a stale base must not write"),
        }
        assert_eq!(
            saves.load(Ordering::SeqCst),
            1,
            "the stale save wrote nothing"
        );
    }

    #[test]
    fn adopt_omits_the_optionals_it_wasnt_given() {
        let body = AdoptBody {
            title: "t".into(),
            body: "b".into(),
            affected_repos: vec!["owner/repo".into()],
            justification: "j".into(),
            ..Default::default()
        };
        let json = serde_json::to_value(&body).expect("encode");
        assert_eq!(json["authoritative"], false);
        assert!(json.get("git_ref").is_none(), "absent, not null: {json}");
        assert!(json.get("codegen_contract").is_none(), "{json}");
    }
}
