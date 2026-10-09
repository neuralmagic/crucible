//! Linking a person's username on an OpenShift cluster through that cluster's own OAuth server.
//!
//! The controller is a confidential client there (an `OAuthClient` whose secret is mounted per
//! cluster). The authorization code is redeemed with PKCE for an access token scoped `user:info`,
//! which only proves who the person is on that cluster: the caller reviews it, records the
//! username, and drops it. The token is never stored, logged, or put in an error.
//!
//! The in-flight request lives on the browser session, bound to the person and the cluster that
//! started it, and is taken off on the first callback whatever its outcome.

use crate::identity::api_key::constant_time_eq;
use crate::runs::clusters::ClusterClients;
use crucible_contract::cluster_link::{ExternalUrl, SCOPE, https_or_loopback};
use oauth2::{
    AuthType, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

/// How long a started link waits for its callback.
pub const FLOW_TTL: jiff::SignedDuration = jiff::SignedDuration::from_mins(10);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LinkFlowError {
    #[error("cluster {cluster:?} is not one this controller is connected to")]
    UnknownCluster { cluster: String },
    #[error(
        "cluster {cluster:?} has no OAuth link registered ([cluster_link].openshift and a mounted client secret); paste a token instead"
    )]
    NotOffered { cluster: String },
    #[error("cluster {cluster:?} did not publish usable OAuth metadata: {detail}")]
    Discovery { cluster: String, detail: String },
    #[error("cluster {cluster:?}'s OAuth server does not support PKCE with S256")]
    NoS256 { cluster: String },
    #[error("this callback answers no link this browser started; start the link again")]
    NoFlow,
    #[error(
        "the link to cluster {cluster:?} was started more than ten minutes ago; start it again"
    )]
    Expired { cluster: String },
    #[error("the callback's state does not match the link this browser started")]
    StateMismatch,
    #[error("the link was started for cluster {started:?} but the callback came for {answered:?}")]
    WrongCluster { started: String, answered: String },
    #[error("the link was started by another person in this browser; start it again")]
    WrongPerson,
    #[error("cluster {cluster:?} refused the link: {error}")]
    Refused { cluster: String, error: String },
    #[error("the callback carried no code")]
    NoCode,
    #[error("cluster {cluster:?} did not redeem the code: {detail}")]
    Exchange { cluster: String, detail: String },
    #[error("the OAuth client secret for cluster {cluster:?} could not be read: {detail}")]
    Secret { cluster: String, detail: String },
}

impl LinkFlowError {
    /// The outcome code the callback sends the browser back to Settings with. Codes, not text,
    /// so a crafted URL cannot put words on that page.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoFlow => "no-flow",
            Self::Expired { .. } => "expired",
            Self::StateMismatch => "state-mismatch",
            Self::WrongCluster { .. } => "wrong-cluster",
            Self::WrongPerson => "wrong-person",
            Self::Refused { .. } => "refused",
            Self::NoCode | Self::Exchange { .. } => "exchange",
            Self::UnknownCluster { .. }
            | Self::NotOffered { .. }
            | Self::Discovery { .. }
            | Self::NoS256 { .. }
            | Self::Secret { .. } => "unavailable",
        }
    }
}

/// The deployment's link configuration: the profile's `[cluster_link]` table, and where each
/// cluster's client secret is mounted (`<secrets>/<cluster>`).
#[derive(Debug, Clone)]
pub struct LinkCfg {
    pub client_id: String,
    pub external_url: ExternalUrl,
    pub secrets: PathBuf,
    /// Clusters whose OAuth server links people.
    pub openshift: Vec<String>,
}

impl LinkCfg {
    pub fn from_profile(
        table: &crucible::deploy::profile::ClusterLinkCfg,
        secrets: PathBuf,
    ) -> Result<Self, crucible_contract::cluster_link::ExternalUrlError> {
        Ok(Self {
            client_id: table.client_id.to_string(),
            external_url: ExternalUrl::parse(&table.external_url)?,
            secrets,
            openshift: table.openshift.iter().map(|c| c.to_string()).collect(),
        })
    }
}

/// A link waiting for its callback, parked on the browser session.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingLink {
    pub cluster: String,
    pub sub: String,
    state: String,
    pkce_verifier: String,
    token_endpoint: String,
    started_at: jiff::Timestamp,
}

impl std::fmt::Debug for PendingLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingLink")
            .field("cluster", &self.cluster)
            .field("sub", &self.sub)
            .field("started_at", &self.started_at)
            .finish_non_exhaustive()
    }
}

/// What the cluster's OAuth server sent back to the callback.
#[derive(Debug, Default, Deserialize)]
pub struct Callback {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// An access token a cluster issued to the person. Hand it to a review and drop it.
pub struct ClusterToken(String);

impl ClusterToken {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ClusterToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClusterToken(<redacted>)")
    }
}

/// RFC 8414 metadata as an OpenShift API server publishes it.
#[derive(Deserialize)]
struct AuthServerMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
}

/// The link flow over the controller's per-cluster clients.
pub struct ClusterLinks {
    cfg: LinkCfg,
    clusters: Arc<ClusterClients>,
    http: reqwest::Client,
}

impl ClusterLinks {
    /// The flow the deploy profile's `[cluster_link]` table configures, with each cluster's secret
    /// under `CONTROLLER_CLUSTER_LINK_SECRETS`. `None` without the table.
    pub fn from_env(
        profile: Option<&std::path::Path>,
        clusters: Arc<ClusterClients>,
    ) -> anyhow::Result<Option<Self>> {
        use anyhow::Context;
        let Some(path) = profile else {
            return Ok(None);
        };
        let profile = crucible::deploy::DeployProfile::load(path)
            .context("reading the deploy profile for cluster links")?;
        let Some(table) = profile.cluster_link.as_ref() else {
            return Ok(None);
        };
        let secrets = std::env::var("CONTROLLER_CLUSTER_LINK_SECRETS").context(
            "the deploy profile has a [cluster_link] table, so CONTROLLER_CLUSTER_LINK_SECRETS must name the mounted client secrets",
        )?;
        let cfg = LinkCfg::from_profile(table, PathBuf::from(secrets))
            .context("[cluster_link].external_url")?;
        tracing::info!(
            clusters = ?cfg.openshift,
            client_id = cfg.client_id,
            "linking cluster usernames through OpenShift OAuth"
        );
        Self::new(cfg, clusters).map(Some)
    }

    pub fn new(cfg: LinkCfg, clusters: Arc<ClusterClients>) -> anyhow::Result<Self> {
        use anyhow::Context;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .build()
            .context("building the cluster link http client")?;
        Ok(Self {
            cfg,
            clusters,
            http,
        })
    }

    /// Whether `cluster` links through its OAuth server: the profile lists it, it is connected,
    /// and its client secret is mounted.
    pub fn offers(&self, cluster: &str) -> bool {
        self.registered(cluster).is_ok()
    }

    /// Start a link: the authorization URL to send the browser to, and what to park on the session.
    pub async fn start(
        &self,
        sub: &str,
        cluster: &str,
        now: jiff::Timestamp,
    ) -> Result<(Url, PendingLink), LinkFlowError> {
        self.registered(cluster)?;
        let metadata = self.discover(cluster).await?;
        let authorize = AuthUrl::new(metadata.authorization_endpoint)
            .map_err(|e| discovery_error(cluster, e.to_string()))?;
        let client = self.client(cluster)?.set_auth_uri(authorize);
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, state) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new(SCOPE.to_string()))
            .set_pkce_challenge(challenge)
            .url();
        Ok((
            url,
            PendingLink {
                cluster: cluster.to_string(),
                sub: sub.to_string(),
                state: state.secret().clone(),
                pkce_verifier: verifier.secret().clone(),
                token_endpoint: metadata.token_endpoint,
                started_at: now,
            },
        ))
    }

    /// Finish a link: `pending` is what the session held (already taken off it), `sub` the
    /// person the callback arrived as, `cluster` the cluster its path names.
    pub async fn finish(
        &self,
        pending: Option<PendingLink>,
        sub: &str,
        cluster: &str,
        callback: Callback,
        now: jiff::Timestamp,
    ) -> Result<ClusterToken, LinkFlowError> {
        let pending = pending.ok_or(LinkFlowError::NoFlow)?;
        let state = callback.state.as_deref().unwrap_or_default();
        if !constant_time_eq(state.as_bytes(), pending.state.as_bytes()) {
            return Err(LinkFlowError::StateMismatch);
        }
        if pending.cluster != cluster {
            return Err(LinkFlowError::WrongCluster {
                started: pending.cluster,
                answered: cluster.to_string(),
            });
        }
        if pending.sub != sub {
            return Err(LinkFlowError::WrongPerson);
        }
        if now > pending.started_at + FLOW_TTL {
            return Err(LinkFlowError::Expired {
                cluster: pending.cluster,
            });
        }
        if let Some(error) = callback.error {
            let detail = callback.error_description.unwrap_or_default();
            return Err(LinkFlowError::Refused {
                cluster: pending.cluster,
                error: format!("{error} {detail}").trim_end().to_string(),
            });
        }
        let code = callback.code.ok_or(LinkFlowError::NoCode)?;
        self.registered(cluster)?;
        let token = TokenUrl::new(pending.token_endpoint)
            .map_err(|e| discovery_error(cluster, e.to_string()))?;
        let client = self.client(cluster)?.set_token_uri(token);
        let response = client
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(PkceCodeVerifier::new(pending.pkce_verifier))
            .request_async(&crate::identity::oidc::bridge(self.http.clone()))
            .await
            .map_err(|e| LinkFlowError::Exchange {
                cluster: cluster.to_string(),
                detail: exchange_failure(&e),
            })?;
        Ok(ClusterToken(response.access_token().secret().clone()))
    }

    fn registered(&self, cluster: &str) -> Result<(), LinkFlowError> {
        if !self.clusters.names().iter().any(|c| c == cluster) {
            return Err(LinkFlowError::UnknownCluster {
                cluster: cluster.to_string(),
            });
        }
        if !self.cfg.openshift.iter().any(|c| c == cluster)
            || !self.cfg.secrets.join(cluster).is_file()
        {
            return Err(LinkFlowError::NotOffered {
                cluster: cluster.to_string(),
            });
        }
        Ok(())
    }

    /// The cluster's OAuth endpoints, read from its API server through the controller's own client.
    async fn discover(&self, cluster: &str) -> Result<AuthServerMetadata, LinkFlowError> {
        let discovery = |detail: String| discovery_error(cluster, detail);
        let client = self
            .clusters
            .client(cluster)
            .await
            .map_err(|e| discovery(format!("{e:#}")))?;
        let request = http::Request::get("/.well-known/oauth-authorization-server")
            .body(Vec::new())
            .map_err(|e| discovery(e.to_string()))?;
        let metadata: AuthServerMetadata = client
            .request(request)
            .await
            .map_err(|e| discovery(e.to_string()))?;
        for endpoint in [&metadata.authorization_endpoint, &metadata.token_endpoint] {
            let url = Url::parse(endpoint).map_err(|e| discovery(format!("{endpoint:?}: {e}")))?;
            if !https_or_loopback(&url) {
                return Err(discovery(format!("{endpoint:?} is not https")));
            }
        }
        if !metadata
            .code_challenge_methods_supported
            .iter()
            .any(|m| m == "S256")
        {
            return Err(LinkFlowError::NoS256 {
                cluster: cluster.to_string(),
            });
        }
        Ok(metadata)
    }

    /// The confidential client registered on `cluster`, without its endpoints.
    fn client(&self, cluster: &str) -> Result<oauth2::basic::BasicClient, LinkFlowError> {
        let secret = std::fs::read_to_string(self.cfg.secrets.join(cluster)).map_err(|e| {
            LinkFlowError::Secret {
                cluster: cluster.to_string(),
                detail: e.to_string(),
            }
        })?;
        let redirect = self
            .cfg
            .external_url
            .callback(cluster)
            .map_err(|e| discovery_error(cluster, e.to_string()))?;
        Ok(
            oauth2::basic::BasicClient::new(ClientId::new(self.cfg.client_id.clone()))
                .set_client_secret(ClientSecret::new(secret.trim().to_string()))
                .set_auth_type(AuthType::RequestBody)
                .set_redirect_uri(RedirectUrl::from_url(redirect)),
        )
    }
}

fn discovery_error(cluster: &str, detail: String) -> LinkFlowError {
    LinkFlowError::Discovery {
        cluster: cluster.to_string(),
        detail,
    }
}

/// A token endpoint failure, without anything the server echoed back that could hold the code.
fn exchange_failure<RE: std::error::Error + 'static>(
    e: &oauth2::RequestTokenError<RE, oauth2::basic::BasicErrorResponse>,
) -> String {
    match e {
        oauth2::RequestTokenError::ServerResponse(r) => r.error().to_string(),
        oauth2::RequestTokenError::Request(e) => format!("the request failed: {e}"),
        oauth2::RequestTokenError::Parse(e, _) => format!("the response did not parse: {e}"),
        oauth2::RequestTokenError::Other(e) => e.clone(),
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use crate::identity::cluster_link::*;
    use axum::extract::{Form, Query, State};
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Redirect, Response};
    use base64::Engine;
    use crucible_contract::cluster_link::DEFAULT_CLIENT_ID;
    use sha2::Digest;
    use std::collections::HashMap;
    use std::sync::Mutex;

    pub(crate) const SECRET: &str = "oauthclient-secret";
    pub(crate) const EXTERNAL: &str = "https://crucible.example.com/console";

    #[derive(Default)]
    pub(crate) struct Issued {
        /// code -> (code_challenge, redirect_uri, client_id)
        pub(crate) codes: HashMap<String, (String, String, String)>,
        next: u32,
    }

    /// One OpenShift cluster: its API server's RFC 8414 document and its OAuth server, which
    /// enforces what the real one does for a confidential client with PKCE.
    #[derive(Clone)]
    pub(crate) struct OpenShift {
        pub(crate) base: String,
        pub(crate) s256: bool,
        pub(crate) redirect: String,
        pub(crate) issued: Arc<Mutex<Issued>>,
    }

    #[derive(Deserialize)]
    struct AuthorizeQuery {
        response_type: String,
        client_id: String,
        redirect_uri: String,
        scope: String,
        state: String,
        code_challenge: String,
        code_challenge_method: String,
    }

    #[derive(Deserialize)]
    struct TokenForm {
        grant_type: String,
        code: String,
        redirect_uri: String,
        client_id: String,
        client_secret: String,
        code_verifier: String,
    }

    async fn metadata(State(os): State<OpenShift>) -> Response {
        let methods: Vec<&str> = if os.s256 {
            vec!["plain", "S256"]
        } else {
            vec!["plain"]
        };
        axum::Json(serde_json::json!({
            "issuer": os.base,
            "authorization_endpoint": format!("{}/oauth/authorize", os.base),
            "token_endpoint": format!("{}/oauth/token", os.base),
            "code_challenge_methods_supported": methods,
        }))
        .into_response()
    }

    async fn authorize(State(os): State<OpenShift>, Query(q): Query<AuthorizeQuery>) -> Response {
        if q.response_type != "code"
            || q.client_id != DEFAULT_CLIENT_ID
            || q.redirect_uri != os.redirect
            || q.scope != SCOPE
            || q.code_challenge_method != "S256"
        {
            return (StatusCode::BAD_REQUEST, "invalid_request").into_response();
        }
        let mut issued = os.issued.lock().unwrap();
        issued.next += 1;
        let code = format!("code-{}", issued.next);
        issued.codes.insert(
            code.clone(),
            (q.code_challenge, q.redirect_uri.clone(), q.client_id),
        );
        Redirect::to(&format!("{}?code={code}&state={}", q.redirect_uri, q.state)).into_response()
    }

    async fn token(State(os): State<OpenShift>, Form(f): Form<TokenForm>) -> Response {
        let refuse = |error: &str| {
            (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": error})),
            )
                .into_response()
        };
        if f.client_id != DEFAULT_CLIENT_ID || f.client_secret != SECRET {
            return refuse("invalid_client");
        }
        let Some((challenge, redirect, client)) = os.issued.lock().unwrap().codes.remove(&f.code)
        else {
            return refuse("invalid_grant");
        };
        let derived = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(sha2::Sha256::digest(f.code_verifier.as_bytes()));
        if f.grant_type != "authorization_code"
            || derived != challenge
            || redirect != f.redirect_uri
            || client != f.client_id
        {
            return refuse("invalid_grant");
        }
        axum::Json(serde_json::json!({
            "access_token": format!("sha256~for-{}", f.code),
            "token_type": "Bearer",
            "expires_in": 300,
        }))
        .into_response()
    }

    pub(crate) struct Fleet {
        pub(crate) links: ClusterLinks,
        pub(crate) pirate: OpenShift,
        pub(crate) dir: tempfile::TempDir,
    }

    pub(crate) async fn serve(os_for: impl FnOnce(String) -> OpenShift) -> OpenShift {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let os = os_for(base);
        let app = axum::Router::new()
            .route(
                "/.well-known/oauth-authorization-server",
                axum::routing::get(metadata),
            )
            .route("/oauth/authorize", axum::routing::get(authorize))
            .route("/oauth/token", axum::routing::post(token))
            .with_state(os.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        os
    }

    pub(crate) fn register(dir: &std::path::Path, cluster: &str, server: &str) {
        let spoke = dir.join("clusters").join(cluster);
        std::fs::create_dir_all(&spoke).unwrap();
        let token = spoke.join("token");
        std::fs::write(&token, "controller-sa-token").unwrap();
        std::fs::write(
            spoke.join("kubeconfig"),
            format!(
                r#"
apiVersion: v1
kind: Config
clusters:
  - name: {cluster}
    cluster: {{ server: "{server}" }}
users:
  - name: sa
    user: {{ tokenFile: {} }}
contexts:
  - name: {cluster}
    context: {{ cluster: {cluster}, user: sa, namespace: crucible-system }}
current-context: {cluster}
"#,
                token.display()
            ),
        )
        .unwrap();
    }

    /// `pirate` links through OAuth; `waldorf` is connected with a secret too; `nosecret` is
    /// connected but has no OAuthClient secret mounted.
    pub(crate) async fn fleet(s256: bool) -> Fleet {
        let external = ExternalUrl::parse(EXTERNAL).unwrap();
        let redirect_for = |cluster: &str| external.callback(cluster).unwrap().to_string();
        let pirate = serve(|base| OpenShift {
            base,
            s256,
            redirect: redirect_for("pirate"),
            issued: Arc::default(),
        })
        .await;
        let waldorf = serve(|base| OpenShift {
            base,
            s256: true,
            redirect: redirect_for("waldorf"),
            issued: Arc::default(),
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        register(dir.path(), "pirate", &pirate.base);
        register(dir.path(), "waldorf", &waldorf.base);
        register(dir.path(), "nosecret", &waldorf.base);
        let secrets = dir.path().join("secrets");
        std::fs::create_dir_all(&secrets).unwrap();
        std::fs::write(secrets.join("pirate"), format!("{SECRET}\n")).unwrap();
        std::fs::write(secrets.join("waldorf"), SECRET).unwrap();
        let clusters = Arc::new(ClusterClients::new(Some(dir.path().join("clusters"))));
        let cfg = LinkCfg {
            client_id: DEFAULT_CLIENT_ID.to_string(),
            external_url: ExternalUrl::parse(EXTERNAL).unwrap(),
            secrets,
            openshift: vec!["pirate".into(), "waldorf".into(), "nosecret".into()],
        };
        Fleet {
            links: ClusterLinks::new(cfg, clusters).unwrap(),
            pirate,
            dir,
        }
    }

    /// What the browser does between `start` and the callback: follow the authorization URL and
    /// read the query the cluster redirected back with.
    pub(crate) async fn authorize_in_browser(url: &Url) -> Callback {
        let res = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
            .get(url.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::SEE_OTHER,
            "{:?}",
            res.text().await
        );
        let location = Url::parse(res.headers()["location"].to_str().unwrap()).unwrap();
        let mut q: HashMap<String, String> = location.query_pairs().into_owned().collect();
        Callback {
            code: q.remove("code"),
            state: q.remove("state"),
            ..Callback::default()
        }
    }

    pub(crate) fn now() -> jiff::Timestamp {
        jiff::Timestamp::now()
    }
}

#[cfg(test)]
mod tests {
    use crate::identity::cluster_link::testing::*;
    use crate::identity::cluster_link::*;
    use crucible_contract::cluster_link::DEFAULT_CLIENT_ID;
    use std::collections::HashMap;

    #[tokio::test]
    async fn a_link_redeems_the_code_the_cluster_issued_for_this_browser() {
        let f = fleet(true).await;
        assert!(f.links.offers("pirate"));
        let (url, pending) = f.links.start("sub-alice", "pirate", now()).await.unwrap();
        let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert!(
            url.as_str()
                .starts_with(&format!("{}/oauth/authorize?", f.pirate.base))
        );
        assert_eq!(q["client_id"], DEFAULT_CLIENT_ID);
        assert_eq!(
            q["redirect_uri"],
            "https://crucible.example.com/console/oauth/cluster-link/pirate/callback"
        );
        assert_eq!(q["scope"], "user:info");
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(!q.contains_key("client_secret"));
        assert_eq!(pending.cluster, "pirate");
        assert_eq!(pending.sub, "sub-alice");

        let callback = authorize_in_browser(&url).await;
        let token = f
            .links
            .finish(Some(pending), "sub-alice", "pirate", callback, now())
            .await
            .unwrap();
        assert_eq!(token.expose(), "sha256~for-code-1");
    }

    #[tokio::test]
    async fn a_callback_whose_state_differs_is_refused_before_the_code_is_spent() {
        let f = fleet(true).await;
        let (url, pending) = f.links.start("sub-alice", "pirate", now()).await.unwrap();
        let mut callback = authorize_in_browser(&url).await;
        let code = callback.code.clone();
        callback.state = Some("forged".to_string());
        assert_eq!(
            f.links
                .finish(
                    Some(pending.clone()),
                    "sub-alice",
                    "pirate",
                    callback,
                    now()
                )
                .await
                .unwrap_err(),
            LinkFlowError::StateMismatch
        );
        let missing = Callback {
            code,
            ..Callback::default()
        };
        assert_eq!(
            f.links
                .finish(Some(pending), "sub-alice", "pirate", missing, now())
                .await
                .unwrap_err(),
            LinkFlowError::StateMismatch
        );
        assert_eq!(
            f.pirate.issued.lock().unwrap().codes.len(),
            1,
            "the code was never redeemed"
        );
    }

    #[tokio::test]
    async fn a_callback_arriving_for_another_cluster_is_refused() {
        let f = fleet(true).await;
        let (url, pending) = f.links.start("sub-alice", "pirate", now()).await.unwrap();
        let callback = authorize_in_browser(&url).await;
        assert_eq!(
            f.links
                .finish(Some(pending), "sub-alice", "waldorf", callback, now())
                .await
                .unwrap_err(),
            LinkFlowError::WrongCluster {
                started: "pirate".into(),
                answered: "waldorf".into()
            }
        );
    }

    #[tokio::test]
    async fn a_callback_arriving_as_another_person_is_refused() {
        let f = fleet(true).await;
        let (url, pending) = f.links.start("sub-alice", "pirate", now()).await.unwrap();
        let callback = authorize_in_browser(&url).await;
        assert_eq!(
            f.links
                .finish(Some(pending), "sub-mallory", "pirate", callback, now())
                .await
                .unwrap_err(),
            LinkFlowError::WrongPerson
        );
    }

    /// The session hands the pending link out once; the second callback finds nothing, and a code
    /// replayed under a fresh link is refused by the cluster too.
    #[tokio::test]
    async fn a_replayed_callback_is_refused() {
        let f = fleet(true).await;
        let (url, pending) = f.links.start("sub-alice", "pirate", now()).await.unwrap();
        let callback = authorize_in_browser(&url).await;
        let replay = Callback {
            code: callback.code.clone(),
            state: callback.state.clone(),
            ..Callback::default()
        };
        f.links
            .finish(
                Some(pending.clone()),
                "sub-alice",
                "pirate",
                callback,
                now(),
            )
            .await
            .unwrap();
        assert_eq!(
            f.links
                .finish(None, "sub-alice", "pirate", replay, now())
                .await
                .unwrap_err(),
            LinkFlowError::NoFlow
        );
        let reused = Callback {
            code: Some("code-1".into()),
            state: Some(pending.state.clone()),
            ..Callback::default()
        };
        let err = f
            .links
            .finish(Some(pending), "sub-alice", "pirate", reused, now())
            .await
            .unwrap_err();
        assert_eq!(
            err,
            LinkFlowError::Exchange {
                cluster: "pirate".into(),
                detail: "invalid_grant".into()
            }
        );
    }

    #[tokio::test]
    async fn a_link_older_than_ten_minutes_is_refused() {
        let f = fleet(true).await;
        let started = now();
        let (url, pending) = f.links.start("sub-alice", "pirate", started).await.unwrap();
        let callback = authorize_in_browser(&url).await;
        let late = started + FLOW_TTL + jiff::SignedDuration::from_secs(1);
        assert_eq!(
            f.links
                .finish(Some(pending), "sub-alice", "pirate", callback, late)
                .await
                .unwrap_err(),
            LinkFlowError::Expired {
                cluster: "pirate".into()
            }
        );
    }

    #[tokio::test]
    async fn the_clusters_refusal_is_reported_and_nothing_is_redeemed() {
        let f = fleet(true).await;
        let (_, pending) = f.links.start("sub-alice", "pirate", now()).await.unwrap();
        let state = pending.state.clone();
        let callback = Callback {
            state: Some(state),
            error: Some("access_denied".into()),
            error_description: Some("the user denied the request".into()),
            ..Callback::default()
        };
        assert_eq!(
            f.links
                .finish(Some(pending), "sub-alice", "pirate", callback, now())
                .await
                .unwrap_err(),
            LinkFlowError::Refused {
                cluster: "pirate".into(),
                error: "access_denied the user denied the request".into()
            }
        );
    }

    #[tokio::test]
    async fn a_wrong_client_secret_fails_the_exchange_without_echoing_the_code() {
        let f = fleet(true).await;
        let (url, pending) = f.links.start("sub-alice", "pirate", now()).await.unwrap();
        let callback = authorize_in_browser(&url).await;
        let code = callback.code.clone().unwrap();
        std::fs::write(f.links.cfg.secrets.join("pirate"), "rotated-elsewhere").unwrap();
        let err = f
            .links
            .finish(Some(pending), "sub-alice", "pirate", callback, now())
            .await
            .unwrap_err();
        assert_eq!(
            err,
            LinkFlowError::Exchange {
                cluster: "pirate".into(),
                detail: "invalid_client".into()
            }
        );
        let shown = err.to_string();
        assert!(
            !shown.contains(&code) && !shown.contains("rotated-elsewhere"),
            "{shown}"
        );
    }

    #[tokio::test]
    async fn only_connected_clusters_with_a_mounted_secret_offer_a_link() {
        let f = fleet(true).await;
        assert!(!f.links.offers("nosecret"));
        assert!(!f.links.offers("../secrets/pirate"));
        assert_eq!(
            f.links
                .start("sub-alice", "nosecret", now())
                .await
                .unwrap_err(),
            LinkFlowError::NotOffered {
                cluster: "nosecret".into()
            }
        );
        for unknown in ["elsewhere", "../secrets/pirate", "pirate/../waldorf"] {
            assert_eq!(
                f.links
                    .start("sub-alice", unknown, now())
                    .await
                    .unwrap_err(),
                LinkFlowError::UnknownCluster {
                    cluster: unknown.into()
                },
                "{unknown}"
            );
        }
    }

    #[tokio::test]
    async fn a_cluster_without_s256_is_refused_before_the_browser_leaves() {
        let f = fleet(false).await;
        assert_eq!(
            f.links
                .start("sub-alice", "pirate", now())
                .await
                .unwrap_err(),
            LinkFlowError::NoS256 {
                cluster: "pirate".into()
            }
        );
    }

    #[tokio::test]
    async fn neither_the_token_nor_the_verifier_reaches_a_debug_string() {
        let f = fleet(true).await;
        let (url, pending) = f.links.start("sub-alice", "pirate", now()).await.unwrap();
        let shown = format!("{pending:?}");
        assert!(!shown.contains(&pending.pkce_verifier) && !shown.contains(&pending.state));
        let callback = authorize_in_browser(&url).await;
        let token = f
            .links
            .finish(Some(pending), "sub-alice", "pirate", callback, now())
            .await
            .unwrap();
        assert_eq!(format!("{token:?}"), "ClusterToken(<redacted>)");
    }
}
