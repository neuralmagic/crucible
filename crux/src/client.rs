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
use crate::sse::{SseEvent, SseParser};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::time::Duration;

/// The whole-request deadline on an event stream.
const EVENT_STREAM_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// How a [`Client`] reaches the controller: one request in, its status and body text out.
///
/// The body comes back as text rather than a parsed value on purpose — a non-2xx body is usually
/// the sentence naming what the caller got wrong, and it has to survive verbatim.
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
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

    /// GET a server-sent event stream, handing each event to `on_event` as it arrives, until the
    /// server closes it. A non-2xx comes back as its status and body, with no events.
    async fn events(
        &self,
        path: &str,
        _last_event_id: Option<&str>,
        _on_event: &mut (dyn FnMut(SseEvent) -> Result<()> + Send),
    ) -> Result<(reqwest::StatusCode, String)> {
        bail!("{path} is an event stream, and this transport cannot stream")
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

/// The request header naming the team a request acts as. The controller answers a team it
/// refuses with the same header set to `refused`.
pub const ACT_AS_HEADER: &str = "x-crucible-act-as";

/// The team every request acts as, and why the controller refused it once it has.
#[derive(Debug, Clone)]
pub struct ActAs {
    team: String,
    refused: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl ActAs {
    /// `team` is the principal, `team:<slug>`.
    pub fn new(team: impl Into<String>) -> Self {
        ActAs {
            team: team.into(),
            refused: std::sync::Arc::default(),
        }
    }

    pub fn team(&self) -> &str {
        &self.team
    }

    /// The controller's reason, once a response has refused the team.
    pub fn refused(&self) -> Option<String> {
        self.refused.lock().ok().and_then(|reason| reason.clone())
    }

    fn note_refusal(&self, body: &str) {
        let reason = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|body| body.get("error")?.as_str().map(str::to_string))
            .unwrap_or_else(|| format!("the controller refused to act as {}", self.team));
        if let Ok(mut slot) = self.refused.lock() {
            *slot = Some(reason);
        }
    }
}

/// The HTTP wire: a reqwest client and the credential it presents.
pub struct HttpTransport {
    http: reqwest::Client,
    endpoint: Endpoint,
    auth: Auth,
    act_as: Option<ActAs>,
}

/// Where an import fetches a pack, and the manifest variant it reads.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct PackImportSource<'a> {
    pub repo: &'a str,
    pub git_ref: Option<&'a str>,
    pub path: Option<&'a str>,
    pub variant: Option<&'a str>,
}

#[derive(Clone)]
pub struct Client {
    transport: std::sync::Arc<dyn Transport>,
}

impl HttpTransport {
    pub fn connect(cfg: &Config) -> Result<Self> {
        Self::with_client(cfg, Self::builder())
    }

    /// The builder [`HttpTransport::connect`] starts from, for a caller that adds a proxy or
    /// resolver overrides before handing it to [`HttpTransport::with_client`].
    #[cfg(not(target_arch = "wasm32"))]
    pub fn builder() -> reqwest::ClientBuilder {
        reqwest::Client::builder().timeout(Duration::from_secs(120))
    }

    /// The browser's fetch has no client-wide timeout.
    #[cfg(target_arch = "wasm32")]
    pub fn builder() -> reqwest::ClientBuilder {
        reqwest::Client::builder()
    }

    pub fn with_client(cfg: &Config, builder: reqwest::ClientBuilder) -> Result<Self> {
        if cfg.url.is_empty() {
            bail!("no controller to talk to: set CONTROLLER_URL, or `url` in the config file");
        }
        let http = builder
            .build()
            .context("building the controller http client")?;
        Ok(Self {
            http,
            endpoint: Endpoint::Configured {
                url: cfg.url.clone(),
            },
            auth: cfg.auth.clone(),
            act_as: None,
        })
    }

    /// Send every request as `act_as`'s team.
    pub fn acting_as(mut self, act_as: ActAs) -> Self {
        self.act_as = Some(act_as);
        self
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.req_accepting(method, path, "application/json")
    }

    fn req_accepting(
        &self,
        method: reqwest::Method,
        path: &str,
        accept: &str,
    ) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.endpoint.base()))
            .header("accept", accept);
        for (name, value) in self.auth.headers() {
            req = req.header(name, value);
        }
        if let Some(act_as) = &self.act_as {
            req = req.header(ACT_AS_HEADER, act_as.team());
        }
        req
    }

    /// The team acted as, when `resp` refuses it.
    fn refused_by(&self, resp: &reqwest::Response) -> Option<&ActAs> {
        let refused = resp
            .headers()
            .get(ACT_AS_HEADER)
            .is_some_and(|value| value == "refused");
        self.act_as.as_ref().filter(|_| refused)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
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
        let refused = self.refused_by(&resp);
        let body = resp.text().await.unwrap_or_default();
        if let Some(act_as) = refused {
            act_as.note_refusal(&body);
        }
        Ok((status, body))
    }

    async fn get_bytes(&self, path: &str) -> Result<(reqwest::StatusCode, Vec<u8>)> {
        let resp = self
            .req(reqwest::Method::GET, path)
            .send()
            .await
            .with_context(|| format!("requesting {path}"))?;
        let status = resp.status();
        let refused = self.refused_by(&resp);
        let bytes = resp
            .bytes()
            .await
            .with_context(|| format!("reading the body of {path}"))?;
        if let Some(act_as) = refused {
            act_as.note_refusal(&String::from_utf8_lossy(&bytes));
        }
        Ok((status, bytes.to_vec()))
    }

    async fn events(
        &self,
        path: &str,
        last_event_id: Option<&str>,
        on_event: &mut (dyn FnMut(SseEvent) -> Result<()> + Send),
    ) -> Result<(reqwest::StatusCode, String)> {
        let mut req = self
            .req_accepting(reqwest::Method::GET, path, "text/event-stream")
            .timeout(EVENT_STREAM_TIMEOUT);
        if let Some(id) = last_event_id {
            req = req.header("last-event-id", id);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("requesting {path}"))?;
        let status = resp.status();
        if !status.is_success() {
            let refused = self.refused_by(&resp);
            let body = resp.text().await.unwrap_or_default();
            if let Some(act_as) = refused {
                act_as.note_refusal(&body);
            }
            return Ok((status, body));
        }
        let mut parser = SseParser::default();
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut resp = resp;
            while let Some(chunk) = resp
                .chunk()
                .await
                .with_context(|| format!("reading the stream of {path}"))?
            {
                for event in parser.push(&chunk) {
                    on_event(event)?;
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            use futures_util::StreamExt;
            let mut chunks = resp.bytes_stream();
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk.with_context(|| format!("reading the stream of {path}"))?;
                for event in parser.push(&chunk) {
                    on_event(event)?;
                }
            }
        }
        Ok((status, String::new()))
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

    pub async fn put<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        let value = serde_json::to_value(body).context("encoding the request body")?;
        parse(
            self.send(reqwest::Method::PUT, path, Some(&value)).await?,
            path,
        )
    }

    /// DELETE a resource whose success is a 204 with no body.
    pub async fn delete(&self, path: &str) -> Result<()> {
        self.send(reqwest::Method::DELETE, path, None)
            .await
            .map(|_| ())
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
    /// A finished run's stored `session.jsonl`.
    pub async fn run_session(&self, run_id: &str) -> Result<String> {
        let path = format!("/api/runs/{}/artifacts/session.jsonl", encode(run_id));
        let (status, bytes) = self.transport.get_bytes(&path).await?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if !status.is_success() {
            bail!(
                "{path} -> HTTP {status}: {}{}",
                text.trim(),
                self.hint_for(status)
            );
        }
        Ok(text)
    }

    /// A running run's live event relay, resumed after `last_event_id` when given.
    pub async fn run_live(
        &self,
        run_id: &str,
        last_event_id: Option<&str>,
        on_event: &mut (dyn FnMut(SseEvent) -> Result<()> + Send),
    ) -> Result<()> {
        let path = format!("/api/runs/{}/live", encode(run_id));
        let (status, text) = self
            .transport
            .events(&path, last_event_id, on_event)
            .await?;
        if !status.is_success() {
            bail!(
                "{path} -> HTTP {status}: {}{}",
                text.trim(),
                self.hint_for(status)
            );
        }
        Ok(())
    }

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

    /// Register pending import `import` under registry id `body.id`, pinned at its rev.
    pub async fn register_import(
        &self,
        import: &str,
        body: &crate::dto::RegisterImport<'_>,
    ) -> Result<crate::dto::Registered> {
        self.post(
            &format!("/api/playbooks/imports/{}/register", encode(import)),
            Some(body),
        )
        .await
    }

    /// Discard pending import `import`.
    pub async fn discard_import(&self, import: &str) -> Result<serde_json::Value> {
        self.post(
            &format!("/api/playbooks/imports/{}/discard", encode(import)),
            None::<&serde_json::Value>,
        )
        .await
    }

    pub async fn playbooks<T: DeserializeOwned>(&self) -> Result<T> {
        self.get("/api/playbooks").await
    }

    pub async fn playbook_caps<T: DeserializeOwned>(&self) -> Result<T> {
        self.get("/api/config/playbook-caps").await
    }

    /// Every provider registration the caller may read, disabled ones included.
    pub async fn register_provider(&self, body: &dto::RegisterProvider) -> Result<dto::Provider> {
        self.post("/api/providers", Some(body)).await
    }

    pub async fn update_provider(
        &self,
        id: &str,
        body: &dto::ProviderBody,
    ) -> Result<dto::Provider> {
        self.put(&format!("/api/providers/{}", encode(id)), body)
            .await
    }

    /// Refused while issues or schedules still pin the provider.
    pub async fn delete_provider(&self, id: &str) -> Result<()> {
        self.delete(&format!("/api/providers/{}", encode(id))).await
    }

    pub async fn providers<T: DeserializeOwned>(&self) -> Result<T> {
        self.get("/api/providers").await
    }

    /// The clusters the caller may dispatch onto, default first, narrowed to what registered
    /// playbook `playbook` can run on when one is named.
    pub async fn dispatch_targets<T: DeserializeOwned>(&self, playbook: Option<&str>) -> Result<T> {
        let mut q = Query::new();
        q.push("playbook", playbook);
        self.get(&format!("/api/dispatch-targets{}", q.finish()))
            .await
    }

    /// The secrets the caller owns or is an owner-group member of. Metadata only.
    pub async fn secrets<T: DeserializeOwned>(&self) -> Result<T> {
        self.get("/api/secrets").await
    }

    pub async fn create_secret<T: DeserializeOwned>(&self, body: &serde_json::Value) -> Result<T> {
        self.post("/api/secrets", Some(body)).await
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

    /// Hand secret `id` to `owner` (`user:<login>` or `team:<slug>`).
    pub async fn transfer_secret(&self, id: &str, owner: &str) -> Result<dto::Secret> {
        self.put(
            &format!("/api/secrets/{}/owner", encode(id)),
            &serde_json::json!({ "owner": owner }),
        )
        .await
    }

    /// A secret and its bindings.
    pub async fn secret(&self, id: &str) -> Result<dto::SecretDetail> {
        self.get(&format!("/api/secrets/{}", encode(id))).await
    }

    /// Who did what to a secret, newest first.
    pub async fn secret_audit(&self, id: &str) -> Result<Vec<dto::SecretAudit>> {
        self.get(&format!("/api/secrets/{}/audit", encode(id)))
            .await
    }

    /// Write `value` as the secret's next version.
    pub async fn rotate_secret(&self, id: &str, value: &str) -> Result<dto::Secret> {
        self.post(
            &format!("/api/secrets/{}/rotate", encode(id)),
            Some(&serde_json::json!({ "value": value })),
        )
        .await
    }

    pub async fn unbind_secret(&self, id: &str, binding: &str) -> Result<()> {
        self.delete(&format!(
            "/api/secrets/{}/bindings/{}",
            encode(id),
            encode(binding)
        ))
        .await
    }

    pub async fn shares(&self, of: &Shared) -> Result<Vec<dto::Share>> {
        self.get(&format!("{}/shares", of.path())).await
    }

    /// Give `grantee` (`user:<login>` or `team:<slug>`) `role` until `not_after`, replacing any
    /// share it had.
    pub async fn grant_share(
        &self,
        of: &Shared,
        grantee: &str,
        role: &str,
        not_after: Option<&str>,
    ) -> Result<dto::Share> {
        self.put(
            &format!("{}/shares/{}", of.path(), encode(grantee)),
            &serde_json::json!({ "role": role, "not_after": not_after }),
        )
        .await
    }

    pub async fn revoke_share(&self, of: &Shared, grantee: &str) -> Result<()> {
        self.delete(&format!("{}/shares/{}", of.path(), encode(grantee)))
            .await
    }

    /// The newest `limit` status transitions across everything the controller works.
    pub async fn events(&self, limit: usize) -> Result<Vec<dto::Event>> {
        self.get(&format!("/api/events?limit={limit}")).await
    }

    /// Every team the caller may see, each with its members.
    pub async fn teams(&self) -> Result<Vec<dto::Team>> {
        self.get("/api/teams").await
    }

    pub async fn create_team(&self, slug: &str, display_name: &str) -> Result<dto::Team> {
        let body = serde_json::json!({ "slug": slug, "display_name": display_name });
        self.post("/api/teams", Some(&body)).await
    }

    pub async fn rename_team(&self, slug: &str, display_name: &str) -> Result<dto::Team> {
        let body = serde_json::json!({ "display_name": display_name });
        self.put(&format!("/api/teams/{}", encode(slug)), &body)
            .await
    }

    /// Refused while the team still owns resources; the refusal names them.
    pub async fn delete_team(&self, slug: &str) -> Result<()> {
        self.delete(&format!("/api/teams/{}", encode(slug))).await
    }

    /// Replace the team's whole member list.
    pub async fn put_team_members(
        &self,
        slug: &str,
        members: &[dto::TeamMemberBody],
    ) -> Result<dto::Team> {
        let body = serde_json::json!({ "members": members });
        self.put(&format!("/api/teams/{}/members", encode(slug)), &body)
            .await
    }

    /// Who changed the team, newest first. Owners only.
    pub async fn team_audit(&self, slug: &str) -> Result<Vec<dto::TeamAudit>> {
        self.get(&format!("/api/teams/{}/audit", encode(slug)))
            .await
    }

    /// The caller's API keys. Refused for a caller that presents a key: only a signed-in session
    /// manages keys.
    pub async fn api_keys(&self) -> Result<Vec<dto::ApiKey>> {
        self.get("/api/keys").await
    }

    /// Mint a key named `name` that expires in `days`, or never when `None`.
    pub async fn mint_api_key(&self, name: &str, days: Option<i64>) -> Result<dto::MintedKey> {
        let body = serde_json::json!({ "name": name, "expires_in_days": days });
        self.post("/api/keys", Some(&body)).await
    }

    pub async fn revoke_api_key(&self, id: &str) -> Result<()> {
        self.delete(&format!("/api/keys/{}", encode(id))).await
    }

    pub async fn mcp_sign_ins(&self) -> Result<dto::McpSignIns> {
        self.get("/api/me/mcp-sign-ins").await
    }

    /// Sign out MCP client `client_id`, or every client when `None`.
    pub async fn sign_out_mcp(&self, client_id: Option<&str>) -> Result<dto::McpSignedOut> {
        let path = match client_id {
            Some(id) => format!("/api/me/mcp-sign-ins?client_id={}", encode(id)),
            None => "/api/me/mcp-sign-ins".to_string(),
        };
        parse(
            self.send(reqwest::Method::DELETE, &path, None).await?,
            &path,
        )
    }

    /// The open decision requests the caller may answer.
    pub async fn decisions(&self) -> Result<dto::Decisions> {
        self.get("/api/decisions").await
    }

    pub async fn decision(&self, id: &str) -> Result<dto::Decision> {
        self.get(&format!("/api/decisions/{}", encode(id))).await
    }

    /// Answer every question of request `id`, as of the evidence `digest` names.
    pub async fn answer_decision(
        &self,
        id: &str,
        labels: &std::collections::BTreeMap<String, Vec<String>>,
        digest: &str,
        note: Option<&str>,
    ) -> Result<dto::Decision> {
        self.post(
            &format!("/api/decisions/{}/answer", encode(id)),
            Some(&serde_json::json!({
                "labels": labels,
                "evidence_digest": digest,
                "note": note,
            })),
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

    /// What rerunning finished run `run_id` from `from` would reuse and dispatch again. An empty
    /// `from` reruns only what did not pass.
    pub async fn rerun_preview<T: DeserializeOwned>(
        &self,
        run_id: &str,
        from: &[String],
    ) -> Result<T> {
        let mut q = Query::new();
        q.push("from", Some(&from.join(",")));
        self.get(&format!("/api/runs/{}/rerun{}", encode(run_id), q.finish()))
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

    /// A registered playbook and its pack files at the pinned rev.
    pub async fn playbook<T: DeserializeOwned>(&self, id: &str) -> Result<T> {
        self.get(&format!("/api/playbooks/{}", encode(id))).await
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

    pub async fn create_schedule<T: DeserializeOwned>(
        &self,
        body: &serde_json::Value,
    ) -> Result<T> {
        self.post("/api/schedules", Some(body)).await
    }

    /// Replace schedule `id` whole with `body`; [`dto::Schedule::body`] builds one from a stored
    /// schedule.
    pub async fn update_schedule<T: DeserializeOwned>(
        &self,
        id: &str,
        body: &serde_json::Value,
    ) -> Result<T> {
        self.put(&format!("/api/schedules/{}", encode(id)), body)
            .await
    }

    pub async fn watches<T: DeserializeOwned>(&self) -> Result<T> {
        self.get("/api/watches").await
    }

    pub async fn watch<T: DeserializeOwned>(&self, id: &str) -> Result<T> {
        self.get(&format!("/api/watches/{}", encode(id))).await
    }

    pub async fn webhooks<T: DeserializeOwned>(&self) -> Result<T> {
        self.get("/api/webhooks").await
    }

    pub async fn webhook<T: DeserializeOwned>(&self, id: &str) -> Result<T> {
        self.get(&format!("/api/webhooks/{}", encode(id))).await
    }

    /// Create a webhook. Its secret is in this answer and nowhere else.
    pub async fn create_webhook(&self, body: &dto::WebhookBody) -> Result<dto::WebhookSecret> {
        self.post("/api/webhooks", Some(body)).await
    }

    /// Replace a webhook. Its verifier and secret stay as created.
    pub async fn update_webhook(&self, id: &str, body: &dto::WebhookBody) -> Result<dto::Webhook> {
        self.put(&format!("/api/webhooks/{}", encode(id)), body)
            .await
    }

    pub async fn webhook_presets(&self) -> Result<Vec<dto::WebhookPreset>> {
        self.get("/api/webhooks/presets").await
    }

    /// Check a transform's CEL the way a save does.
    pub async fn check_webhook(&self, body: &dto::WebhookBody) -> Result<dto::WebhookCheck> {
        let transform = serde_json::json!({
            "filter": body.filter,
            "dedupe": body.dedupe,
            "derive": body.derive,
        });
        self.post("/api/webhooks/check", Some(&transform)).await
    }

    /// Run a transform on `sample`, delivered with `headers`. Nothing is stored.
    pub async fn preview_webhook(
        &self,
        body: &dto::WebhookBody,
        sample: &serde_json::Value,
        headers: &std::collections::BTreeMap<String, String>,
    ) -> Result<dto::WebhookPreview> {
        let asked = serde_json::json!({
            "filter": body.filter,
            "dedupe": body.dedupe,
            "derive": body.derive,
            "sample": sample,
            "headers": headers,
        });
        self.post("/api/webhooks/preview", Some(&asked)).await
    }

    /// Pause a webhook (its address answers not found) or resume it.
    pub async fn set_webhook_enabled(&self, id: &str, enabled: bool) -> Result<dto::Webhook> {
        let body = serde_json::json!({ "enabled": enabled });
        self.post(
            &format!("/api/webhooks/{}/enabled", encode(id)),
            Some(&body),
        )
        .await
    }

    /// Replace a webhook's secret. The old one stops verifying at once.
    pub async fn rotate_webhook_secret(&self, id: &str) -> Result<dto::WebhookSecret> {
        self.post(
            &format!("/api/webhooks/{}/secret", encode(id)),
            None::<&serde_json::Value>,
        )
        .await
    }

    pub async fn delete_webhook(&self, id: &str) -> Result<()> {
        self.delete(&format!("/api/webhooks/{}", encode(id))).await
    }

    /// A page of a webhook's deliveries, newest first: at most `limit`, recorded before `before`.
    pub async fn webhook_deliveries<T: DeserializeOwned>(
        &self,
        id: &str,
        before: Option<&str>,
        limit: Option<i64>,
    ) -> Result<T> {
        let mut query = Vec::new();
        if let Some(before) = before {
            query.push(format!("before={}", encode(before)));
        }
        if let Some(limit) = limit {
            query.push(format!("limit={limit}"));
        }
        let path = format!("/api/webhooks/{}/deliveries", encode(id));
        if query.is_empty() {
            self.get(&path).await
        } else {
            self.get(&format!("{path}?{}", query.join("&"))).await
        }
    }

    pub async fn whoami(&self) -> Result<dto::Whoami> {
        self.get("/api/whoami").await
    }

    /// Cancel playbook launch `key`: the controller parks it and deletes its running pods.
    pub async fn cancel_launch(&self, key: &str) -> Result<dto::CancelAck> {
        self.post(
            &format!("/api/playbook-runs/{}/cancel", encode(key)),
            None::<&serde_json::Value>,
        )
        .await
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
        source: &PackImportSource<'_>,
    ) -> Result<T> {
        self.post("/api/playbooks/imports", Some(source)).await
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

    /// Move a draft to `to`, with everything keyed on its id.
    pub async fn rename_draft<T: DeserializeOwned>(&self, id: &str, to: &str) -> Result<T> {
        let body = serde_json::json!({ "id": to });
        self.post(
            &format!("/api/playbook-drafts/{}/rename", encode(id)),
            Some(&body),
        )
        .await
    }

    /// Copy a draft's newest save into a new draft `to`.
    pub async fn clone_draft<T: DeserializeOwned>(
        &self,
        id: &str,
        to: &str,
        description: Option<&str>,
    ) -> Result<T> {
        let body = serde_json::json!({ "id": to, "description": description });
        self.post(
            &format!("/api/playbook-drafts/{}/clone", encode(id)),
            Some(&body),
        )
        .await
    }

    /// The blueprints the caller can read.
    pub async fn blueprints<T: DeserializeOwned>(&self) -> Result<T> {
        self.get("/api/blueprints").await
    }

    /// Mark a registered playbook as a blueprint, or unmark it.
    pub async fn set_blueprint<T: DeserializeOwned>(&self, id: &str, on: bool) -> Result<T> {
        self.toggle(&format!("/api/playbooks/{}/blueprint", encode(id)), on)
            .await
    }

    /// Make a blueprint public, or private.
    pub async fn set_blueprint_public<T: DeserializeOwned>(
        &self,
        id: &str,
        public: bool,
    ) -> Result<T> {
        self.toggle(
            &format!("/api/playbooks/{}/blueprint/public", encode(id)),
            public,
        )
        .await
    }

    /// `PUT` turns a flag on and `DELETE` turns it off; both answer with the resulting state.
    async fn toggle<T: DeserializeOwned>(&self, path: &str, on: bool) -> Result<T> {
        let method = if on {
            reqwest::Method::PUT
        } else {
            reqwest::Method::DELETE
        };
        parse(self.send(method, path, None).await?, path)
    }

    /// Start a draft from blueprint `id`.
    pub async fn use_blueprint<T: DeserializeOwned>(
        &self,
        id: &str,
        to: &str,
        description: Option<&str>,
    ) -> Result<T> {
        let body = serde_json::json!({ "id": to, "description": description });
        self.post(&format!("/api/blueprints/{}/use", encode(id)), Some(&body))
            .await
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
            crate::text::truncate(&body, 400)
        )
    })
}

/// How long a session follow waits on a running run whose pod is not reachable yet.
const POD_WAITS: u32 = 24;
const POD_WAIT: Duration = Duration::from_secs(5);
/// Back-to-back connections that end with nothing on them before a follow gives up.
const EMPTY_STREAMS: u32 = 5;

/// How one live connection ended.
#[derive(Debug, PartialEq, Eq)]
enum LiveEnd {
    /// The run is not running and nothing was streamed: its stored session is the record.
    Stored,
    Finished,
    WaitForPod,
    Resume,
    Failed(String),
}

impl LiveEnd {
    fn of(reason: Option<&str>, streamed: usize) -> Self {
        match reason {
            Some("run-not-running") if streamed == 0 => LiveEnd::Stored,
            Some("run-not-running" | "bridge-closed") => LiveEnd::Finished,
            Some(reason) if reason.starts_with("completed") => LiveEnd::Finished,
            Some("pod-gone") => LiveEnd::WaitForPod,
            Some(reason) if reason.starts_with("error") => LiveEnd::Failed(reason.to_string()),
            Some(_) | None => LiveEnd::Resume,
        }
    }
}

impl Client {
    /// Hand every line of a run's session to `on_line`: a running run's lines as they land,
    /// resuming after the last event id whenever the relay times out or the connection drops,
    /// and a stopped run's stored session whole.
    pub async fn follow_session(
        &self,
        run_id: &str,
        on_line: &mut (dyn FnMut(&str) -> Result<()> + Send),
    ) -> Result<()> {
        self.follow_run(run_id, on_line, &mut |_| Ok(())).await
    }

    /// [`Self::follow_session`], also handing each pod log line to `on_log` as it lands. A run on
    /// a spoke cluster streams only its pod's log until it ends, and its session lines after.
    pub async fn follow_run(
        &self,
        run_id: &str,
        on_line: &mut (dyn FnMut(&str) -> Result<()> + Send),
        on_log: &mut (dyn FnMut(&str) -> Result<()> + Send),
    ) -> Result<()> {
        let mut last_id: Option<String> = None;
        let mut streamed = 0usize;
        let mut pod_waits = 0u32;
        let mut empty = 0u32;
        loop {
            let mut reason: Option<String> = None;
            let mut events = 0usize;
            let resume = last_id.clone();
            {
                let mut on_event = |event: SseEvent| -> Result<()> {
                    events += 1;
                    match event.event.as_str() {
                        "session" => {
                            on_line(&event.data)?;
                            streamed += 1;
                        }
                        "log" => on_log(&event.data)?,
                        "end" => reason = Some(event.data),
                        _ => {}
                    }
                    if event.id.is_some() {
                        last_id = event.id;
                    }
                    Ok(())
                };
                self.run_live(run_id, resume.as_deref(), &mut on_event)
                    .await?;
            }
            match LiveEnd::of(reason.as_deref(), streamed) {
                LiveEnd::Stored => {
                    for line in self.run_session(run_id).await?.lines() {
                        on_line(line)?;
                    }
                    return Ok(());
                }
                LiveEnd::Finished => return Ok(()),
                LiveEnd::Failed(reason) => {
                    bail!("the live relay for {} ended: {reason}", run_id)
                }
                LiveEnd::WaitForPod => {
                    pod_waits += 1;
                    if pod_waits > POD_WAITS {
                        bail!("run {} is running but its pod never came up", run_id);
                    }
                    tokio::time::sleep(POD_WAIT).await;
                }
                LiveEnd::Resume => {
                    empty = if events == 0 { empty + 1 } else { 0 };
                    if empty >= EMPTY_STREAMS {
                        bail!(
                            "the live relay for {} kept closing with nothing on it",
                            run_id
                        );
                    }
                }
            }
        }
    }
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

/// The `crucible-playbooks://` link that opens launch `key` of `playbook` in the desktop tracker.
pub fn tracker_url(playbook: &str, key: &str) -> String {
    format!(
        "crucible-playbooks://playbook/{}/launch/{}",
        encode(playbook),
        encode(key)
    )
}

/// Percent-encode one path segment or query value.
///
/// RFC 3986 unreserved set only. Deliberately stricter than a URL library's per-component rules:
/// an issue key is `owner/repo#12`, and both `/` and `#` MUST encode or the request goes to a
/// different path (or drops everything after the fragment marker).
/// Something the controller lets its owner share.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Shared {
    Playbook(String),
    Draft(String),
    Provider(String),
}

impl Shared {
    fn path(&self) -> String {
        match self {
            Shared::Playbook(id) => format!("/api/playbooks/{}", encode(id)),
            Shared::Draft(id) => format!("/api/playbook-drafts/{}", encode(id)),
            Shared::Provider(id) => format!("/api/providers/{}", encode(id)),
        }
    }
}

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
    use crate::client::*;

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

    /// A relay that serves one scripted event stream: what a spoke run's `/live` looks like.
    struct Scripted(Vec<SseEvent>);

    #[async_trait::async_trait]
    impl Transport for Scripted {
        async fn send(
            &self,
            _method: reqwest::Method,
            path: &str,
            _body: Option<&serde_json::Value>,
        ) -> Result<(reqwest::StatusCode, String)> {
            bail!("{path}: the scripted relay only streams")
        }
        async fn events(
            &self,
            _path: &str,
            _last_event_id: Option<&str>,
            on_event: &mut (dyn FnMut(SseEvent) -> Result<()> + Send),
        ) -> Result<(reqwest::StatusCode, String)> {
            for event in &self.0 {
                on_event(event.clone())?;
            }
            Ok((reqwest::StatusCode::OK, String::new()))
        }
        fn base(&self) -> &str {
            "http://relay.test"
        }
    }

    fn event(kind: &str, data: &str, id: &str) -> SseEvent {
        SseEvent {
            event: kind.into(),
            data: data.into(),
            id: Some(id.into()),
        }
    }

    #[tokio::test]
    async fn following_a_run_hands_over_log_and_session_lines_in_order() {
        let client = Client::new(std::sync::Arc::new(Scripted(vec![
            event("log", "pulling sandbox image", "0.1"),
            event("log", "scan: 2 issues", "0.2"),
            event("session", r#"{"v":1,"kind":"note","msg":"hi"}"#, "1.2"),
            event("end", "completed: Succeeded", "1.2"),
        ])));
        let (mut sessions, mut logs) = (Vec::new(), Vec::new());
        client
            .follow_run(
                "r1",
                &mut |line| {
                    sessions.push(line.to_string());
                    Ok(())
                },
                &mut |line| {
                    logs.push(line.to_string());
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert_eq!(logs, ["pulling sandbox image", "scan: 2 issues"]);
        assert_eq!(sessions, [r#"{"v":1,"kind":"note","msg":"hi"}"#]);

        let mut only = Vec::new();
        client
            .follow_session("r1", &mut |line| {
                only.push(line.to_string());
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(only, sessions, "following the session alone skips the log");
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

    #[test]
    fn a_live_end_reason_decides_whether_to_stop_wait_or_resume() {
        assert_eq!(LiveEnd::of(Some("run-not-running"), 0), LiveEnd::Stored);
        assert_eq!(LiveEnd::of(Some("run-not-running"), 3), LiveEnd::Finished);
        assert_eq!(LiveEnd::of(Some("bridge-closed"), 3), LiveEnd::Finished);
        assert_eq!(
            LiveEnd::of(Some("completed: Succeeded"), 0),
            LiveEnd::Finished
        );
        assert_eq!(LiveEnd::of(Some("pod-gone"), 0), LiveEnd::WaitForPod);
        assert_eq!(
            LiveEnd::of(Some("error: dial refused"), 1),
            LiveEnd::Failed("error: dial refused".into())
        );
        assert_eq!(LiveEnd::of(Some("timeout"), 9), LiveEnd::Resume);
        assert_eq!(LiveEnd::of(None, 9), LiveEnd::Resume);
    }

    #[tokio::test]
    async fn keys_and_mcp_sign_ins_go_to_their_routes_with_the_lifetime_and_client_asked() {
        use axum::extract::{Path, Query, State};
        use axum::http::StatusCode;
        use axum::routing::{delete, get};
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};

        type Seen = Arc<Mutex<Vec<String>>>;

        async fn mint(State(seen): State<Seen>, body: String) -> String {
            seen.lock().unwrap().push(format!("mint {body}"));
            r#"{"id":"k2","name":"ci","created_at":"2026-11-10T00:00:00Z","expires_at":null,
                "last_used_at":null,"revoked_at":null,"secret":"crk_once",
                "mcp_url":"https://mcp.example/mcp"}"#
                .into()
        }
        async fn revoke(State(seen): State<Seen>, Path(id): Path<String>) -> StatusCode {
            seen.lock().unwrap().push(format!("revoke {id}"));
            StatusCode::NO_CONTENT
        }
        async fn sign_out(
            State(seen): State<Seen>,
            Query(query): Query<HashMap<String, String>>,
        ) -> String {
            seen.lock()
                .unwrap()
                .push(format!("sign-out {:?}", query.get("client_id")));
            r#"{"revoked":2}"#.into()
        }

        let seen: Seen = Arc::default();
        let app = axum::Router::new()
            .route(
                "/api/keys",
                get(|| async {
                    r#"[{"id":"k1","name":"laptop","created_at":"2026-11-01T00:00:00Z",
                        "expires_at":"2027-01-30T00:00:00Z","last_used_at":null,
                        "revoked_at":null}]"#
                })
                .post(mint),
            )
            .route("/api/keys/{id}", delete(revoke))
            .route(
                "/api/me/mcp-sign-ins",
                get(|| async {
                    r#"{"enabled":true,"sign_ins":[{"client_id":"dyn-abc","client_name":"Editor",
                        "client_host":null,"client_kind":"dynamic","resource":"https://mcp.example",
                        "cluster":"hub","approved_at":"2026-11-02T00:00:00Z",
                        "last_used_at":"2026-11-09T00:00:00Z"}]}"#
                })
                .delete(sign_out),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let client = Client::connect(&crate::config::Config {
            url,
            auth: Auth::None,
        })
        .expect("connect");

        let keys = client.api_keys().await.expect("keys");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].expires_at.as_deref(), Some("2027-01-30T00:00:00Z"));

        let minted = client.mint_api_key("ci", None).await.expect("mint");
        assert_eq!(
            (minted.key.id.as_str(), minted.secret.as_str()),
            ("k2", "crk_once")
        );
        assert_eq!(minted.mcp_url.as_deref(), Some("https://mcp.example/mcp"));
        client.mint_api_key("ci", Some(30)).await.expect("mint");
        client.revoke_api_key("k 1").await.expect("revoke");

        let live = client.mcp_sign_ins().await.expect("sign-ins");
        assert!(live.enabled);
        assert_eq!(live.sign_ins[0].client_kind, dto::McpClientKind::Dynamic);
        assert_eq!(
            client
                .sign_out_mcp(Some("dyn-abc"))
                .await
                .expect("one")
                .revoked,
            2
        );
        client.sign_out_mcp(None).await.expect("all");

        assert_eq!(
            *seen.lock().unwrap(),
            [
                r#"mint {"expires_in_days":null,"name":"ci"}"#,
                r#"mint {"expires_in_days":30,"name":"ci"}"#,
                "revoke k 1",
                r#"sign-out Some("dyn-abc")"#,
                "sign-out None",
            ]
        );
    }

    #[tokio::test]
    async fn team_calls_carry_the_slug_the_name_and_the_whole_member_list() {
        use axum::extract::{Path, State};
        use axum::http::{Method, StatusCode};
        use axum::routing::{get, put};
        use std::sync::{Arc, Mutex};

        type Seen = Arc<Mutex<Vec<String>>>;
        const TEAM: &str = r#"{"slug":"infra","display_name":"Infra","created_by":"user:wynn",
            "created_at":"2026-11-01T00:00:00Z","updated_at":"2026-11-01T00:00:00Z",
            "members":[{"kind":"user","member":"wynn","role":"owner","since":"2026-11-01T00:00:00Z",
            "added_by":null,"signed_in":true},{"kind":"group","member":"eng","role":"member",
            "since":"2026-11-02T00:00:00Z","added_by":"user:wynn","signed_in":false}],
            "my_role":"owner","reachable":true}"#;

        async fn note(
            State(seen): State<Seen>,
            method: Method,
            path: Option<Path<String>>,
            body: String,
        ) -> (StatusCode, String) {
            let slug = path.map_or_else(String::new, |Path(slug)| slug);
            seen.lock().unwrap().push(format!("{method} {slug} {body}"));
            if method == Method::DELETE {
                return (StatusCode::NO_CONTENT, String::new());
            }
            (StatusCode::OK, TEAM.to_string())
        }

        let seen: Seen = Arc::default();
        let app = axum::Router::new()
            .route(
                "/api/teams",
                get(|| async { format!("[{TEAM}]") }).post(note),
            )
            .route("/api/teams/{slug}", put(note).delete(note))
            .route("/api/teams/{slug}/members", put(note))
            .route(
                "/api/teams/{slug}/audit",
                get(|| async {
                    r#"[{"id":4,"at":"2026-11-01T00:00:00Z","actor":"user:wynn","subject":null,
                        "auth_path":"session","action":"team:create","resource_type":"team",
                        "resource_id":"infra","decision":"allow","rule":"owner","prior":null,
                        "result":{}}]"#
                }),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let client = Client::connect(&crate::config::Config {
            url,
            auth: Auth::None,
        })
        .expect("connect");

        let teams = client.teams().await.expect("teams");
        assert_eq!(teams[0].my_role, Some(dto::TeamRole::Owner));
        assert_eq!(
            teams[0]
                .members
                .iter()
                .map(|m| (m.kind, m.role, m.signed_in))
                .collect::<Vec<_>>(),
            [
                (dto::MemberKind::User, dto::TeamRole::Owner, true),
                (dto::MemberKind::Group, dto::TeamRole::Member, false),
            ]
        );
        assert_eq!(
            client.team_audit("infra").await.expect("audit")[0].action,
            "team:create"
        );

        client.create_team("infra", "Infra").await.expect("create");
        client
            .rename_team("infra", "Platform")
            .await
            .expect("rename");
        let members = [dto::TeamMemberBody {
            kind: dto::MemberKind::Rule,
            member: "email-domain:example.com".into(),
            role: dto::TeamRole::Maintainer,
        }];
        client
            .put_team_members("infra", &members)
            .await
            .expect("members");
        client.delete_team("infra").await.expect("delete");

        assert_eq!(
            *seen.lock().unwrap(),
            [
                r#"POST  {"display_name":"Infra","slug":"infra"}"#,
                r#"PUT infra {"display_name":"Platform"}"#,
                r#"PUT infra {"members":[{"kind":"rule","member":"email-domain:example.com","role":"maintainer"}]}"#,
                "DELETE infra ",
            ]
        );
    }

    #[tokio::test]
    async fn a_team_acted_as_rides_every_request_and_its_refusal_is_kept() {
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::get;

        async fn whoami(headers: HeaderMap) -> (StatusCode, HeaderMap, String) {
            let asked = headers
                .get(ACT_AS_HEADER)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("nobody")
                .to_string();
            if asked == "team:closed" {
                let mut marked = HeaderMap::new();
                marked.insert(ACT_AS_HEADER, "refused".parse().unwrap());
                return (
                    StatusCode::FORBIDDEN,
                    marked,
                    r#"{"error":"you are not a member of team:closed"}"#.into(),
                );
            }
            (
                StatusCode::OK,
                HeaderMap::new(),
                format!(r#"{{"user":"{asked}"}}"#),
            )
        }

        let app = axum::Router::new().route("/api/whoami", get(whoami));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let cfg = crate::config::Config {
            url: format!("http://{}", listener.local_addr().expect("addr")),
            auth: Auth::None,
        };
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let through = |act_as: Option<ActAs>| {
            let wire = HttpTransport::connect(&cfg).expect("connect");
            let wire = match act_as {
                Some(act_as) => wire.acting_as(act_as),
                None => wire,
            };
            Client::new(std::sync::Arc::new(wire))
        };

        let plain: dto::Whoami = through(None).get("/api/whoami").await.expect("whoami");
        assert_eq!(plain.user.as_deref(), Some("nobody"));

        let infra = ActAs::new("team:infra");
        let as_team: dto::Whoami = through(Some(infra.clone()))
            .get("/api/whoami")
            .await
            .expect("whoami");
        assert_eq!(as_team.user.as_deref(), Some("team:infra"));
        assert_eq!(infra.refused(), None);

        let closed = ActAs::new("team:closed");
        let client = through(Some(closed.clone()));
        let refusal = client
            .get::<dto::Whoami>("/api/whoami")
            .await
            .expect_err("a refused team is an error");
        assert!(
            format!("{refusal:#}").contains("not a member"),
            "{refusal:#}"
        );
        assert_eq!(
            closed.refused().as_deref(),
            Some("you are not a member of team:closed")
        );
    }

    #[tokio::test]
    async fn webhook_calls_read_the_transform_and_send_pause_rotate_and_delete() {
        use axum::extract::{Path, State};
        use axum::http::{Method, StatusCode};
        use axum::routing::{delete, get, post};
        use std::sync::{Arc, Mutex};

        type Seen = Arc<Mutex<Vec<String>>>;
        const HOOK: &str = r#"{"id":"w1","playbook":"triage","verifier":"path_token","enabled":true,
            "max_launches_per_hour":3,"last_delivery_at":null,"consecutive_failures":0,
            "owner_principal":"user:wynn","delivery_url":"https://hooks.example/hooks/w1",
            "filter":"body.action == \"opened\"","dedupe":"string(body.issue.id)",
            "derive":{"repo":"body.repository.full_name"},"params":{"limit":"1"},
            "max_cost":3.0,"max_time":"25m","dispatch_target":"waldorf","header":null}"#;

        async fn note(
            State(seen): State<Seen>,
            method: Method,
            Path(id): Path<String>,
            body: String,
        ) -> (StatusCode, String) {
            seen.lock().unwrap().push(format!("{method} {id} {body}"));
            if method == Method::DELETE {
                return (StatusCode::NO_CONTENT, String::new());
            }
            (StatusCode::OK, HOOK.to_string())
        }

        let seen: Seen = Arc::default();
        let app = axum::Router::new()
            .route("/api/webhooks", get(|| async { format!("[{HOOK}]") }))
            .route("/api/webhooks/{id}", delete(note))
            .route("/api/webhooks/{id}/enabled", post(note))
            .route(
                "/api/webhooks/{id}/secret",
                post(|| async {
                    format!(
                        r#"{{"webhook":{HOOK},"secret":"whsec_once",
                            "delivery_url":"https://hooks.example/hooks/w1/tok"}}"#
                    )
                }),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let client = Client::connect(&crate::config::Config {
            url,
            auth: Auth::None,
        })
        .expect("connect");

        let hooks: Vec<dto::Webhook> = client.webhooks().await.expect("webhooks");
        assert_eq!(
            hooks[0].filter.as_deref(),
            Some(r#"body.action == "opened""#)
        );
        assert_eq!(hooks[0].derive["repo"], "body.repository.full_name");
        assert_eq!(hooks[0].params["limit"], "1");
        assert_eq!(
            (hooks[0].max_cost, hooks[0].max_time.as_str()),
            (3.0, "25m")
        );

        let paused = client
            .set_webhook_enabled("w1", false)
            .await
            .expect("pause");
        assert_eq!(paused.id, "w1");
        let rotated = client.rotate_webhook_secret("w1").await.expect("rotate");
        assert_eq!(rotated.secret, "whsec_once");
        assert_eq!(
            rotated.delivery_url.as_deref(),
            Some("https://hooks.example/hooks/w1/tok")
        );
        client.delete_webhook("w1").await.expect("delete");
        assert_eq!(
            *seen.lock().unwrap(),
            [r#"POST w1 {"enabled":false}"#, "DELETE w1 "]
        );
    }

    #[tokio::test]
    async fn a_webhook_form_saves_checks_and_previews_what_it_holds() {
        use axum::extract::State;
        use axum::http::{Method, StatusCode, Uri};
        use axum::routing::{get, post, put};
        use std::collections::BTreeMap;
        use std::sync::{Arc, Mutex};

        type Seen = Arc<Mutex<Vec<(String, serde_json::Value)>>>;
        const HOOK: &str = r#"{"id":"w9","playbook":"triage","verifier":"hmac_sha256",
            "header":"x-hub-signature-256","retention_days":7,"enabled":true}"#;

        async fn note(
            State(seen): State<Seen>,
            method: Method,
            uri: Uri,
            body: String,
        ) -> (StatusCode, String) {
            let sent = serde_json::from_str(&body).expect("a JSON body");
            seen.lock()
                .unwrap()
                .push((format!("{method} {}", uri.path()), sent));
            let answer = match (method.as_str(), uri.path()) {
                ("POST", "/api/webhooks") => {
                    format!(r#"{{"webhook":{HOOK},"secret":"whsec_new","delivery_url":null}}"#)
                }
                ("POST", "/api/webhooks/check") => r#"{"diagnostics":[{"field":"filter",
                    "message":"undeclared reference to 'bod'","line":1,"column":0}]}"#
                    .to_string(),
                ("POST", "/api/webhooks/preview") => r#"{"filter":{"value":true,"error":null},
                    "dedupe":{"value":"42","error":null},
                    "derive":{"value":null,"error":"no such key: repository"}}"#
                    .to_string(),
                _ => HOOK.to_string(),
            };
            (StatusCode::OK, answer)
        }

        let seen: Seen = Arc::default();
        let app = axum::Router::new()
            .route("/api/webhooks", post(note))
            .route("/api/webhooks/{id}", put(note))
            .route("/api/webhooks/check", post(note))
            .route("/api/webhooks/preview", post(note))
            .route(
                "/api/webhooks/presets",
                get(|| async {
                    r#"[{"id":"github-issues","title":"GitHub issues","verifier":"hmac_sha256",
                        "header":"x-hub-signature-256","filter":"body.action == \"opened\"",
                        "dedupe":"string(body.issue.id)",
                        "derive":{"repo":"body.repository.full_name"}}]"#
                }),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let client = Client::connect(&crate::config::Config {
            url,
            auth: Auth::None,
        })
        .expect("connect");

        let presets = client.webhook_presets().await.expect("presets");
        assert_eq!(presets[0].verifier, dto::WebhookVerifier::HmacSha256);
        assert_eq!(presets[0].derive["repo"], "body.repository.full_name");

        let body = dto::WebhookBody {
            playbook: "triage".into(),
            verifier: dto::WebhookVerifier::HmacSha256,
            header: Some("x-hub-signature-256".into()),
            filter: "body.action == \"opened\"".into(),
            dedupe: "string(body.issue.id)".into(),
            derive: BTreeMap::from([("repo".into(), "body.repository.full_name".into())]),
            params: BTreeMap::from([("limit".into(), "1".into())]),
            max_launches_per_hour: 3,
            retention_days: 7,
            max_cost: 3.0,
            max_time: "25m".into(),
            dispatch_target: None,
        };
        let created = client.create_webhook(&body).await.expect("create");
        assert_eq!(created.secret, "whsec_new");
        assert_eq!(
            created.webhook.header.as_deref(),
            Some("x-hub-signature-256")
        );
        assert_eq!(created.webhook.retention_days, 7);
        client.update_webhook("w9", &body).await.expect("update");

        let check = client.check_webhook(&body).await.expect("check");
        assert_eq!(check.diagnostics[0].field, "filter");
        let sample = serde_json::json!({ "action": "opened" });
        let headers = BTreeMap::from([("x-github-event".to_string(), "issues".to_string())]);
        let preview = client
            .preview_webhook(&body, &sample, &headers)
            .await
            .expect("preview");
        assert_eq!(preview.filter.value, Some(serde_json::json!(true)));
        assert_eq!(
            preview.derive.error.as_deref(),
            Some("no such key: repository")
        );

        let sent = seen.lock().unwrap().clone();
        let whole = serde_json::json!({
            "playbook": "triage", "verifier": "hmac_sha256", "header": "x-hub-signature-256",
            "filter": "body.action == \"opened\"", "dedupe": "string(body.issue.id)",
            "derive": { "repo": "body.repository.full_name" }, "params": { "limit": "1" },
            "max_launches_per_hour": 3, "retention_days": 7, "max_cost": 3.0,
            "max_time": "25m", "dispatch_target": null,
        });
        let transform = serde_json::json!({
            "filter": "body.action == \"opened\"", "dedupe": "string(body.issue.id)",
            "derive": { "repo": "body.repository.full_name" },
        });
        let mut previewed = transform.clone();
        previewed["sample"] = sample;
        previewed["headers"] = serde_json::json!({ "x-github-event": "issues" });
        assert_eq!(
            sent,
            [
                ("POST /api/webhooks".to_string(), whole.clone()),
                ("PUT /api/webhooks/w9".to_string(), whole),
                ("POST /api/webhooks/check".to_string(), transform),
                ("POST /api/webhooks/preview".to_string(), previewed),
            ]
        );
    }

    #[tokio::test]
    async fn provider_calls_send_only_the_fields_a_kind_uses() {
        use axum::extract::State;
        use axum::http::{Method, StatusCode, Uri};
        use axum::routing::{get, put};
        use std::sync::{Arc, Mutex};

        type Seen = Arc<Mutex<Vec<(String, serde_json::Value)>>>;
        const PROVIDER: &str = r#"{"id":"lab","display_name":"Lab vLLM","kind":"custom",
            "models":["glm-5-3"],"default_model":"glm-5-3","roles":["agent"],
            "harness":"opencode","harness_override":null,"endpoint":"https://vllm.example/v1",
            "protocol":"chat_completions","secret_name":"lab-key","secret_owner":"team:infra",
            "price":null,"enabled":true,"owner":"team:infra","actions":["read","update","delete"]}"#;

        async fn note(
            State(seen): State<Seen>,
            method: Method,
            uri: Uri,
            body: String,
        ) -> (StatusCode, String) {
            let sent = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
            seen.lock()
                .unwrap()
                .push((format!("{method} {}", uri.path()), sent));
            if method == Method::DELETE {
                return (StatusCode::NO_CONTENT, String::new());
            }
            (StatusCode::OK, PROVIDER.to_string())
        }

        let seen: Seen = Arc::default();
        let app = axum::Router::new()
            .route(
                "/api/providers",
                get(|| async { format!("[{PROVIDER}]") }).post(note),
            )
            .route("/api/providers/{id}", put(note).delete(note))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let client = Client::connect(&crate::config::Config {
            url,
            auth: Auth::None,
        })
        .expect("connect");

        let listed: Vec<dto::Provider> = client.providers().await.expect("providers");
        assert_eq!(
            listed[0].protocol,
            Some(dto::InferenceProtocol::ChatCompletions)
        );
        assert_eq!(
            listed[0].endpoint.as_deref(),
            Some("https://vllm.example/v1")
        );
        assert_eq!(listed[0].secret_owner.as_deref(), Some("team:infra"));
        assert_eq!(listed[0].actions, ["read", "update", "delete"]);

        let hosted = dto::ProviderBody {
            display_name: "OpenAI".into(),
            kind: dto::ProviderKind::Openai,
            models: Vec::new(),
            default_model: None,
            endpoint: None,
            protocol: None,
            harness: None,
            secret_name: None,
            secret_owner: None,
            enabled: true,
        };
        client
            .register_provider(&dto::RegisterProvider {
                id: "openai".into(),
                owner: "user:wynn".into(),
                provider: hosted,
            })
            .await
            .expect("register");
        let custom = dto::ProviderBody {
            display_name: "Lab vLLM".into(),
            kind: dto::ProviderKind::Custom,
            models: vec!["glm-5-3".into()],
            default_model: Some("glm-5-3".into()),
            endpoint: Some("https://vllm.example/v1".into()),
            protocol: Some(dto::InferenceProtocol::SystemOne),
            harness: Some("opencode".into()),
            secret_name: Some("lab-key".into()),
            secret_owner: Some("team:infra".into()),
            enabled: false,
        };
        client
            .update_provider("lab", &custom)
            .await
            .expect("update");
        client.delete_provider("lab").await.expect("delete");

        assert_eq!(
            *seen.lock().unwrap(),
            [
                (
                    "POST /api/providers".to_string(),
                    serde_json::json!({
                        "id": "openai", "owner": "user:wynn", "display_name": "OpenAI",
                        "kind": "openai", "models": [], "enabled": true,
                    })
                ),
                (
                    "PUT /api/providers/lab".to_string(),
                    serde_json::json!({
                        "display_name": "Lab vLLM", "kind": "custom", "models": ["glm-5-3"],
                        "default_model": "glm-5-3", "endpoint": "https://vllm.example/v1",
                        "protocol": "system_one", "harness": "opencode",
                        "secret_name": "lab-key", "secret_owner": "team:infra",
                        "enabled": false,
                    })
                ),
                (
                    "DELETE /api/providers/lab".to_string(),
                    serde_json::Value::Null
                ),
            ]
        );
    }
}
