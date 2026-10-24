//! RFC 7591 dynamic client registration, narrowed to native public clients: a registration may
//! name only loopback `http` redirect URIs, asks for no secret and gets none, and receives no
//! registration access token. A registered client is self-described: its name is shown as
//! unverified. Registrations live in Postgres and die after [`DcrPolicy::idle`] unused; the source
//! address is rate limited in memory and the number of live registrations is capped.

use crate::agent_auth::people::clients::{
    Client, ClientId, DynamicId, RedirectRule, RuleSource, display_name,
};
use anyhow::Context;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const DCR_ENV: &str = "CONTROLLER_PEOPLE_DCR";
pub const TRUSTED_PROXIES_ENV: &str = "CONTROLLER_PEOPLE_TRUSTED_PROXIES";
const ID_BYTES: usize = 24;
const MAX_REDIRECTS: usize = 8;
const MAX_REDIRECT_LEN: usize = 512;
const MAX_NAME: usize = 100;
const UNNAMED: &str = "Unnamed client";
/// The limiter forgets sources beyond this many, oldest windows first.
const MAX_TRACKED_SOURCES: usize = 10_000;

/// How long a registration lives unused, how many may live at once, and how fast one source may
/// register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DcrPolicy {
    pub idle: Duration,
    pub max_active: i64,
    pub per_source: u32,
    pub window: Duration,
    /// The proxies whose `X-Forwarded-For` names the source; any other peer is its own source.
    pub trusted_proxies: Vec<ipnet::IpNet>,
}

impl Default for DcrPolicy {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(30 * 24 * 3600),
            max_active: 10_000,
            per_source: 20,
            window: Duration::from_secs(3600),
            trusted_proxies: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DcrConfigError {
    #[error("{DCR_ENV} {0:?} is not true or false")]
    Flag(String),
    #[error("{TRUSTED_PROXIES_ENV} entry {0:?} is not an IP network such as 10.128.0.0/14")]
    Proxy(String),
}

impl DcrPolicy {
    /// On unless `CONTROLLER_PEOPLE_DCR` turns it off.
    pub fn from_lookup(raw: Option<String>) -> Result<Option<Self>, DcrConfigError> {
        match raw.as_deref().map(str::trim) {
            None | Some("") | Some("true" | "1" | "on" | "yes") => Ok(Some(Self::default())),
            Some("false" | "0" | "off" | "no") => Ok(None),
            Some(other) => Err(DcrConfigError::Flag(other.to_string())),
        }
    }

    /// `CONTROLLER_PEOPLE_TRUSTED_PROXIES`: comma-separated networks, such as the cluster's pod
    /// network the router runs in. Unset trusts no proxy.
    pub fn trusted_proxies(raw: Option<String>) -> Result<Vec<ipnet::IpNet>, DcrConfigError> {
        raw.unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(|e| {
                e.parse::<ipnet::IpNet>()
                    .or_else(|_| e.parse::<IpAddr>().map(ipnet::IpNet::from))
                    .map_err(|_| DcrConfigError::Proxy(e.to_string()))
            })
            .collect()
    }
}

/// An RFC 7591 §3.2.2 refusal.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegistrationError {
    #[error("{0}")]
    RedirectUri(String),
    #[error("{0}")]
    Metadata(String),
    #[error("too many registrations from this address; retry later")]
    RateLimited,
    #[error("this controller holds as many client registrations as it accepts; retry later")]
    Full,
    #[error("registration failed")]
    Store(String),
}

impl IntoResponse for RegistrationError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            Self::RedirectUri(_) => (StatusCode::BAD_REQUEST, "invalid_redirect_uri"),
            Self::Metadata(_) => (StatusCode::BAD_REQUEST, "invalid_client_metadata"),
            Self::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "temporarily_unavailable"),
            Self::Full => (StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable"),
            Self::Store(_) => (StatusCode::INTERNAL_SERVER_ERROR, "server_error"),
        };
        let description = match &self {
            Self::Store(_) => "registration failed".to_string(),
            other => other.to_string(),
        };
        crate::agent_auth::oauth_error(status, code, &description)
    }
}

/// The client metadata a registration may send; anything else is ignored (RFC 7591 §2).
#[derive(Debug, Default, Deserialize)]
pub struct RegistrationRequest {
    pub redirect_uris: Option<Vec<String>>,
    pub client_name: Option<String>,
    pub token_endpoint_auth_method: Option<String>,
    pub grant_types: Option<Vec<String>>,
    pub response_types: Option<Vec<String>>,
}

/// The RFC 7591 §3.2.1 answer: a public client, no secret, no registration access token.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Registered {
    pub client_id: String,
    pub client_id_issued_at: u64,
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    pub token_endpoint_auth_method: String,
}

/// A self-declared name, without control characters or runs of whitespace, at most 100 characters.
pub fn sanitize_name(raw: Option<&str>) -> String {
    let words: Vec<&str> = raw
        .unwrap_or_default()
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect();
    let name: String = words.join(" ").chars().take(MAX_NAME).collect();
    display_name(&name).unwrap_or_else(|| UNNAMED.to_string())
}

fn loopback_rules(uris: &[String]) -> Result<Vec<RedirectRule>, RegistrationError> {
    if uris.is_empty() {
        return Err(RegistrationError::RedirectUri(
            "redirect_uris must list at least one loopback URI".into(),
        ));
    }
    if uris.len() > MAX_REDIRECTS {
        return Err(RegistrationError::RedirectUri(format!(
            "redirect_uris lists more than {MAX_REDIRECTS} URIs"
        )));
    }
    uris.iter()
        .map(|raw| {
            let refused = || {
                RegistrationError::RedirectUri(format!(
                    "redirect_uri {raw:?} is not http on localhost, 127.0.0.1 or [::1] without query, fragment or credentials"
                ))
            };
            if raw.len() > MAX_REDIRECT_LEN {
                return Err(refused());
            }
            match RedirectRule::parse(raw, RuleSource::Document) {
                Ok(rule @ RedirectRule::Loopback { .. }) => Ok(rule),
                _ => Err(refused()),
            }
        })
        .collect()
}

fn subset(
    given: Option<&[String]>,
    default: &str,
    allowed: &[&str],
    field: &str,
) -> Result<Vec<String>, RegistrationError> {
    let list = match given {
        None => vec![default.to_string()],
        Some(list) => list.to_vec(),
    };
    let outside: Vec<&String> = list
        .iter()
        .filter(|g| !allowed.contains(&g.as_str()))
        .collect();
    if !outside.is_empty() || !list.iter().any(|g| g == default) {
        return Err(RegistrationError::Metadata(format!(
            "{field} {list:?} must include {default:?} and only {allowed:?}"
        )));
    }
    Ok(list)
}

/// A registration that passed every check, ready to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
}

/// The registration as this controller accepts it, or why not.
pub fn check(req: RegistrationRequest) -> Result<Checked, RegistrationError> {
    match req.token_endpoint_auth_method.as_deref() {
        None | Some("none") => {}
        Some(other) => {
            return Err(RegistrationError::Metadata(format!(
                "token_endpoint_auth_method {other:?}: only public clients (\"none\") may register"
            )));
        }
    }
    let redirect_uris = req.redirect_uris.unwrap_or_default();
    loopback_rules(&redirect_uris)?;
    Ok(Checked {
        client_name: sanitize_name(req.client_name.as_deref()),
        grant_types: subset(
            req.grant_types.as_deref(),
            "authorization_code",
            &["authorization_code", "refresh_token"],
            "grant_types",
        )?,
        response_types: subset(
            req.response_types.as_deref(),
            "code",
            &["code"],
            "response_types",
        )?,
        redirect_uris,
    })
}

/// Fixed-window counts per source address.
#[derive(Default)]
struct Limiter {
    windows: HashMap<IpAddr, (u64, u32)>,
}

impl Limiter {
    fn admit(&mut self, source: IpAddr, now: u64, policy: &DcrPolicy) -> bool {
        let window = policy.window.as_secs().max(1);
        let start = now - now % window;
        if self.windows.len() >= MAX_TRACKED_SOURCES {
            self.windows.retain(|_, (s, _)| *s == start);
        }
        let entry = self.windows.entry(source).or_insert((start, 0));
        if entry.0 != start {
            *entry = (start, 0);
        }
        if entry.1 >= policy.per_source {
            return false;
        }
        entry.1 += 1;
        true
    }
}

/// Dynamic registrations over Postgres.
pub struct Dcr {
    pool: PgPool,
    policy: DcrPolicy,
    limiter: Mutex<Limiter>,
}

fn at(secs: u64) -> Result<i64, RegistrationError> {
    i64::try_from(secs)
        .map_err(|_| RegistrationError::Store(format!("timestamp {secs} does not fit")))
}

fn new_id() -> Result<DynamicId, RegistrationError> {
    let mut bytes = [0u8; ID_BYTES];
    aws_lc_rs::rand::fill(&mut bytes)
        .map_err(|_| RegistrationError::Store("drawing random bytes failed".into()))?;
    DynamicId::parse(&format!(
        "{}{}",
        DynamicId::PREFIX,
        URL_SAFE_NO_PAD.encode(bytes)
    ))
    .map_err(|e| RegistrationError::Store(e.to_string()))
}

impl Dcr {
    pub fn new(pool: PgPool, policy: DcrPolicy) -> Self {
        Self {
            pool,
            policy,
            limiter: Mutex::new(Limiter::default()),
        }
    }

    fn cutoff(&self, now: u64) -> anyhow::Result<i64> {
        i64::try_from(now.saturating_sub(self.policy.idle.as_secs()))
            .context("the idle cutoff does not fit the store")
    }

    /// Register a client for `source`, which counts against its rate.
    pub async fn register(
        &self,
        req: RegistrationRequest,
        source: Option<IpAddr>,
        now: u64,
    ) -> Result<Registered, RegistrationError> {
        let checked = check(req)?;
        let admitted = {
            let mut limiter = self.limiter.lock().unwrap_or_else(|e| e.into_inner());
            let source = source.unwrap_or(IpAddr::from([0, 0, 0, 0]));
            limiter.admit(source, now, &self.policy)
        };
        if !admitted {
            return Err(RegistrationError::RateLimited);
        }
        let id = new_id()?;
        let store = |e: sqlx::Error| RegistrationError::Store(format!("{e:#}"));
        let inserted = sqlx::query(
            "INSERT INTO people_dynamic_clients
               (client_id, client_name, redirect_uris, created_at, last_used_at)
             SELECT $1, $2, $3, $4, $4
              WHERE (SELECT count(*) FROM people_dynamic_clients WHERE last_used_at > $5) < $6",
        )
        .bind(id.as_str())
        .bind(&checked.client_name)
        .bind(serde_json::json!(checked.redirect_uris))
        .bind(at(now)?)
        .bind(
            self.cutoff(now)
                .map_err(|e| RegistrationError::Store(format!("{e:#}")))?,
        )
        .bind(self.policy.max_active)
        .execute(&self.pool)
        .await
        .map_err(store)?
        .rows_affected();
        if inserted == 0 {
            return Err(RegistrationError::Full);
        }
        tracing::info!(
            client_id = id.as_str(),
            client_name = checked.client_name,
            redirect_uris = ?checked.redirect_uris,
            source = ?source,
            "dynamic client registered"
        );
        Ok(Registered {
            client_id: id.as_str().to_string(),
            client_id_issued_at: now,
            client_name: checked.client_name,
            redirect_uris: checked.redirect_uris,
            grant_types: checked.grant_types,
            response_types: checked.response_types,
            token_endpoint_auth_method: "none".into(),
        })
    }

    /// The registration behind `id` while it is live, stamping its use.
    pub async fn client(&self, id: &DynamicId, now: u64) -> anyhow::Result<Option<Client>> {
        let row: Option<(String, serde_json::Value)> = sqlx::query_as(
            "UPDATE people_dynamic_clients SET last_used_at = $2
              WHERE client_id = $1 AND last_used_at > $3
          RETURNING client_name, redirect_uris",
        )
        .bind(id.as_str())
        .bind(i64::try_from(now).context("the clock does not fit the store")?)
        .bind(self.cutoff(now)?)
        .fetch_optional(&self.pool)
        .await
        .context("reading a dynamic client registration")?;
        let Some((name, uris)) = row else {
            return Ok(None);
        };
        let uris: Vec<String> =
            serde_json::from_value(uris).context("decoding a registration's redirect URIs")?;
        let redirects = loopback_rules(&uris)
            .map_err(anyhow::Error::from)
            .with_context(|| format!("stored registration {}", id.as_str()))?;
        Ok(Some(Client {
            id: ClientId::Dynamic(id.clone()),
            name,
            redirects,
            resources: None,
        }))
    }

    /// Delete registrations idle past the limit and the approvals issued to them. Returns how many
    /// registrations went.
    pub async fn sweep(&self, now: u64) -> anyhow::Result<u64> {
        let mut tx = self
            .pool
            .begin()
            .await
            .context("opening the registration sweep")?;
        let gone = sqlx::query("DELETE FROM people_dynamic_clients WHERE last_used_at <= $1")
            .bind(self.cutoff(now)?)
            .execute(&mut *tx)
            .await
            .context("sweeping dynamic client registrations")?
            .rows_affected();
        sqlx::query(
            "DELETE FROM people_grants g
              WHERE g.client_id LIKE 'dyn-%'
                AND NOT EXISTS (SELECT 1 FROM people_dynamic_clients c
                                 WHERE c.client_id = g.client_id)",
        )
        .execute(&mut *tx)
        .await
        .context("sweeping approvals of expired registrations")?;
        tx.commit()
            .await
            .context("committing the registration sweep")?;
        Ok(gone)
    }
}

/// The address a registration counts against: the last `X-Forwarded-For` hop when the connection
/// comes from one of `trusted` (the cluster's router appends the client it saw), else the peer.
pub fn source(
    peer: Option<SocketAddr>,
    headers: &HeaderMap,
    trusted: &[ipnet::IpNet],
) -> Option<IpAddr> {
    let peer = peer.map(|p| p.ip());
    let behind_proxy = peer.is_some_and(|ip| trusted.iter().any(|net| net.contains(&ip)));
    let forwarded = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|hop| hop.trim().parse::<IpAddr>().ok())
        .next_back();
    match (behind_proxy, forwarded) {
        (true, Some(hop)) => Some(hop),
        _ => peer,
    }
}

/// `POST /oauth/register`.
pub(crate) async fn register(
    State(auth): State<Arc<crate::agent_auth::AgentAuth>>,
    headers: HeaderMap,
    extensions: axum::http::Extensions,
    body: axum::body::Bytes,
) -> Response {
    let Some(dcr) = auth.people.as_ref().and_then(|p| p.clients.dynamic()) else {
        return crate::agent_auth::oauth_error(
            StatusCode::NOT_FOUND,
            "invalid_request",
            "this controller accepts no client registrations",
        );
    };
    let req: RegistrationRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => {
            return RegistrationError::Metadata(format!("not a JSON client metadata object: {e}"))
                .into_response();
        }
    };
    let peer = extensions
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    match dcr
        .register(
            req,
            source(peer, &headers, &dcr.policy.trusted_proxies),
            crate::clock::unix_secs(),
        )
        .await
    {
        Ok(registered) => {
            let mut response = (StatusCode::CREATED, axum::Json(registered)).into_response();
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Err(e) => {
            tracing::info!(reason = %e, "client registration refused");
            e.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::agent_auth::people::dynamic::*;

    fn req(uris: &[&str]) -> RegistrationRequest {
        RegistrationRequest {
            redirect_uris: Some(uris.iter().map(|u| u.to_string()).collect()),
            client_name: Some("pi".into()),
            ..RegistrationRequest::default()
        }
    }

    #[test]
    fn only_public_loopback_code_clients_pass_the_check() {
        for ok in [
            "http://127.0.0.1:53120/callback",
            "http://localhost:8765/callback",
            "http://[::1]:40000/oauth/done",
            "http://127.0.0.1/callback",
        ] {
            assert!(check(req(&[ok])).is_ok(), "{ok}");
        }
        for (bad, want) in [
            (req(&["https://app.example/callback"]), "RedirectUri"),
            (req(&["com.example.app:/callback"]), "RedirectUri"),
            (req(&["http://127.0.0.1:5000/callback#f"]), "RedirectUri"),
            (req(&["http://127.0.0.1:5000/callback?x=1"]), "RedirectUri"),
            (req(&["http://user@127.0.0.1:5000/callback"]), "RedirectUri"),
            (req(&["http://evil.example:5000/callback"]), "RedirectUri"),
            (req(&[]), "RedirectUri"),
            (RegistrationRequest::default(), "RedirectUri"),
            (
                req(&["http://127.0.0.1:1/callback"; MAX_REDIRECTS + 1]),
                "RedirectUri",
            ),
            (
                RegistrationRequest {
                    token_endpoint_auth_method: Some("client_secret_basic".into()),
                    ..req(&["http://127.0.0.1:5000/callback"])
                },
                "Metadata",
            ),
            (
                RegistrationRequest {
                    grant_types: Some(vec!["client_credentials".into()]),
                    ..req(&["http://127.0.0.1:5000/callback"])
                },
                "Metadata",
            ),
            (
                RegistrationRequest {
                    grant_types: Some(vec!["authorization_code".into(), "implicit".into()]),
                    ..req(&["http://127.0.0.1:5000/callback"])
                },
                "Metadata",
            ),
            (
                RegistrationRequest {
                    response_types: Some(vec!["token".into()]),
                    ..req(&["http://127.0.0.1:5000/callback"])
                },
                "Metadata",
            ),
        ] {
            let got = check(bad);
            assert!(
                format!("{got:?}").starts_with(&format!("Err({want}")),
                "{got:?}"
            );
        }
        let full = check(RegistrationRequest {
            grant_types: Some(vec!["authorization_code".into(), "refresh_token".into()]),
            response_types: Some(vec!["code".into()]),
            token_endpoint_auth_method: Some("none".into()),
            ..req(&["http://127.0.0.1:5000/callback"])
        })
        .unwrap();
        assert_eq!(full.grant_types, ["authorization_code", "refresh_token"]);
    }

    #[test]
    fn names_are_cleaned_and_capped() {
        assert_eq!(sanitize_name(Some("  Claude\tCode\n ")), "Claude Code");
        assert_eq!(sanitize_name(Some("a\u{7}b")), "a b");
        assert_eq!(sanitize_name(None), UNNAMED);
        assert_eq!(sanitize_name(Some(" \n ")), UNNAMED);
        let long = "é".repeat(500);
        assert_eq!(sanitize_name(Some(&long)).chars().count(), MAX_NAME);
    }

    #[test]
    fn the_switch_is_on_unless_turned_off() {
        assert_eq!(DcrPolicy::from_lookup(None), Ok(Some(DcrPolicy::default())));
        assert_eq!(
            DcrPolicy::from_lookup(Some("true".into())),
            Ok(Some(DcrPolicy::default()))
        );
        assert_eq!(DcrPolicy::from_lookup(Some("false".into())), Ok(None));
        assert_eq!(DcrPolicy::from_lookup(Some(" off ".into())), Ok(None));
        assert!(DcrPolicy::from_lookup(Some("maybe".into())).is_err());
    }

    #[test]
    fn the_source_is_the_peer_or_a_trusted_proxys_last_hop() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("6.6.6.6, 203.0.113.9"),
        );
        let trusted = DcrPolicy::trusted_proxies(Some("10.128.0.0/14, 192.0.2.7".into())).unwrap();
        let public: SocketAddr = "198.51.99.1:443".parse().unwrap();
        let router: SocketAddr = "10.128.0.5:443".parse().unwrap();
        let in_cluster: SocketAddr = "10.200.0.9:443".parse().unwrap();
        let named: SocketAddr = "192.0.2.7:443".parse().unwrap();
        assert_eq!(
            source(Some(public), &headers, &trusted),
            Some(public.ip()),
            "a public peer is not trusted to forward"
        );
        assert_eq!(
            source(Some(in_cluster), &headers, &trusted),
            Some(in_cluster.ip()),
            "a private peer outside the trusted networks picks no bucket"
        );
        assert_eq!(
            source(Some(router), &headers, &trusted),
            Some("203.0.113.9".parse().unwrap())
        );
        assert_eq!(
            source(Some(named), &headers, &trusted),
            Some("203.0.113.9".parse().unwrap()),
            "a bare address is a one-address network"
        );
        assert_eq!(
            source(Some(router), &headers, &[]),
            Some(router.ip()),
            "with no trusted proxy every peer is its own source"
        );
        assert_eq!(
            source(Some(router), &HeaderMap::new(), &trusted),
            Some(router.ip())
        );
        assert_eq!(source(None, &headers, &trusted), None);
        assert_eq!(DcrPolicy::trusted_proxies(None), Ok(Vec::new()));
        assert!(matches!(
            DcrPolicy::trusted_proxies(Some("10.0.0.0/99".into())),
            Err(DcrConfigError::Proxy(_))
        ));
    }

    #[test]
    fn the_limiter_counts_per_source_per_window() {
        let policy = DcrPolicy {
            per_source: 2,
            window: Duration::from_secs(60),
            ..DcrPolicy::default()
        };
        let mut l = Limiter::default();
        let a: IpAddr = "203.0.113.1".parse().unwrap();
        let b: IpAddr = "203.0.113.2".parse().unwrap();
        let t = 1_800_000_040 - 1_800_000_040 % 60;
        assert!(l.admit(a, t, &policy));
        assert!(l.admit(a, t + 1, &policy));
        assert!(!l.admit(a, t + 2, &policy));
        assert!(l.admit(b, t + 2, &policy));
        assert!(l.admit(a, t + 60, &policy), "a new window starts over");
    }

    async fn registered(dcr: &Dcr, uris: &[&str], now: u64) -> Registered {
        dcr.register(req(uris), Some("203.0.113.7".parse().unwrap()), now)
            .await
            .unwrap()
    }

    const T0: u64 = 1_800_000_000;

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_registration_is_a_public_client_with_an_unguessable_id(pool: PgPool) {
        let dcr = Dcr::new(pool, DcrPolicy::default());
        let r = registered(&dcr, &["http://127.0.0.1:53120/callback"], T0).await;
        assert!(
            r.client_id.starts_with("dyn-") && r.client_id.len() >= 36,
            "{}",
            r.client_id
        );
        assert_eq!(r.token_endpoint_auth_method, "none");
        assert_eq!(r.client_name, "pi");
        let again = registered(&dcr, &["http://127.0.0.1:53120/callback"], T0).await;
        assert_ne!(r.client_id, again.client_id);
        let body = serde_json::to_value(&r).unwrap();
        for absent in [
            "client_secret",
            "registration_access_token",
            "registration_client_uri",
        ] {
            assert!(body.get(absent).is_none(), "{absent}");
        }
        let id = DynamicId::parse(&r.client_id).unwrap();
        let client = dcr.client(&id, T0 + 1).await.unwrap().unwrap();
        assert_eq!(client.name, "pi");
        assert!(
            client.redirect("http://127.0.0.1:61000/callback").is_ok(),
            "any port"
        );
        assert!(client.redirect("http://127.0.0.1:61000/other").is_err());
        assert!(client.redirect("http://localhost:61000/callback").is_err());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn one_source_is_rate_limited_and_the_total_is_capped(pool: PgPool) {
        let dcr = Dcr::new(
            pool.clone(),
            DcrPolicy {
                per_source: 2,
                max_active: 3,
                ..DcrPolicy::default()
            },
        );
        let uri = &["http://127.0.0.1:5000/callback"];
        let a: IpAddr = "203.0.113.1".parse().unwrap();
        let b: IpAddr = "203.0.113.2".parse().unwrap();
        for _ in 0..2 {
            dcr.register(req(uri), Some(a), T0).await.unwrap();
        }
        assert_eq!(
            dcr.register(req(uri), Some(a), T0).await,
            Err(RegistrationError::RateLimited)
        );
        dcr.register(req(uri), Some(b), T0).await.unwrap();
        assert_eq!(
            dcr.register(req(uri), Some(b), T0).await,
            Err(RegistrationError::Full)
        );
        let idle = T0 + DcrPolicy::default().idle.as_secs() + 3600;
        assert_eq!(dcr.sweep(idle).await.unwrap(), 3);
        dcr.register(req(uri), Some(b), idle).await.unwrap();
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_idle_registration_expires_and_takes_its_approvals(pool: PgPool) {
        let dcr = Dcr::new(pool.clone(), DcrPolicy::default());
        let r = registered(&dcr, &["http://127.0.0.1:5000/callback"], T0).await;
        let id = DynamicId::parse(&r.client_id).unwrap();
        let idle = DcrPolicy::default().idle.as_secs();
        assert!(
            dcr.client(&id, T0 + idle - 1).await.unwrap().is_some(),
            "use slides the window"
        );
        assert!(dcr.client(&id, T0 + 2 * idle - 2).await.unwrap().is_some());
        assert!(dcr.client(&id, T0 + 3 * idle).await.unwrap().is_none());
        sqlx::query(
            "INSERT INTO people_grants (id, sub, client_id, client_name, resource, cluster, scopes,
                                        created_at, last_used_at)
             VALUES ('g1', 's', $1, 'pi', 'r', 'c', '[]', 0, 0)",
        )
        .bind(&r.client_id)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(dcr.sweep(T0 + 3 * idle).await.unwrap(), 1);
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM people_grants")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }
}
