//! Postgres-backed HTTP sessions for the human surface (API + SPA).
//!
//! The `sessions` table ships in `migrations/0002_sessions.sql` under the one
//! [`crate::MIGRATOR`]; the store's own `migrate()` is never called, and the roundtrip test
//! below pins our DDL to the store crate's queries. Sessions stay lazy under proxy mode: no
//! `Set-Cookie` and no row unless a handler writes, so bearer/CLI clients see no change. Under
//! native mode the login itself is the write.
//!
//! [`BoundSession`] is the only way handlers touch a session. It binds the proven
//! [`crate::identity::session::Identity`] to the session's source-tagged `"identity"` slot, and a source
//! mismatch flushes the session — which is what makes flipping between the proxy and native
//! identity models safe in both directions. Durable credentials belong in real tables, never in
//! session blobs.

#![allow(clippy::disallowed_macros)]

use anyhow::Context;
use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::convert::Infallible;
use tower_sessions::cookie::SameSite;
use tower_sessions::cookie::time::Duration;
use tower_sessions::{ExpiredDeletion, Expiry, Session, SessionManagerLayer};
use tower_sessions_sqlx_store::PostgresStore;

/// The session cookie's name.
const COOKIE_NAME: &str = "crucible_session";
/// Sliding inactivity window: a session (and its cookie) lives this long past its last request.
const INACTIVITY_DAYS: i64 = 7;
/// The session slot [`BoundSession`] keeps the caller's identity under.
const IDENTITY_KEY: &str = "identity";
/// The session slot the native login's validated claims live under. [`IDENTITY_KEY`] stays the
/// minimal binding key (login + source) so a proxy-mode session's binding is byte-for-byte what it
/// always was; the claims a native session additionally carries hang off their own slot.
const CLAIMS_KEY: &str = "native_claims";
/// The session slot the login's serialized ID token lives under, for `/auth/logout` to send the
/// issuer as `id_token_hint`.
const ID_TOKEN_KEY: &str = "oidc_id_token";
/// The session slot an in-flight authorization-code request parks its PKCE verifier, nonce, and
/// state under, between `/auth/login` and `/auth/callback`.
const FLOW_KEY: &str = "oidc_flow";
/// How often the background sweep garbage-collects expired rows (loads already filter on
/// `expiry_date`, so the sweep's cadence only bounds dead-row buildup).
const SWEEP_PERIOD: std::time::Duration = std::time::Duration::from_secs(3600);

/// The session store over the controller's pool, pointed at our migrated `sessions` table. The
/// schema is resolved from the connection's `current_schema()`, not hardcoded: the migration
/// creates the table unqualified, so it lands wherever the database user's `search_path` points
/// (shared clusters give each user their own schema; local dev and CI use `public`), and the
/// store must query the same place.
pub async fn store(pool: &PgPool) -> anyhow::Result<PostgresStore> {
    let schema: Option<String> = sqlx::query_scalar("select current_schema()")
        .fetch_one(pool)
        .await
        .context("resolving the session schema")?;
    let schema = schema.ok_or_else(|| {
        anyhow::anyhow!("no current schema: the database user's search_path matches no schema")
    })?;
    PostgresStore::new(pool.clone())
        .with_schema_name(&schema)
        .map_err(|e| anyhow::anyhow!("session store schema name {schema:?}: {e}"))?
        .with_table_name("sessions")
        .map_err(|e| anyhow::anyhow!("session store table name: {e}"))
}

/// The session middleware for the human surface. `secure` marks the cookie `Secure`
/// (`CONTROLLER_SESSION_SECURE`, default on; plain-http local dev opts out or browsers drop the
/// cookie). `SameSite::Lax` so a future OAuth callback's top-level redirect still carries it.
/// `with_always_save` makes the inactivity window actually slide on reads — without it only
/// writes refresh `expiry_date`, and a daily reader would still expire 7 days after their last
/// write. Empty sessions are exempt from the always-save, so cookie-less requests stay row-free.
pub fn layer(store: PostgresStore, secure: bool) -> SessionManagerLayer<PostgresStore> {
    SessionManagerLayer::new(store)
        .with_name(COOKIE_NAME)
        .with_http_only(true)
        .with_same_site(SameSite::Lax)
        .with_secure(secure)
        .with_expiry(Expiry::OnInactivity(Duration::days(INACTIVITY_DAYS)))
        .with_always_save(true)
}

/// Spawn the hourly expired-row sweep. Errors are logged and the loop keeps going — a failed
/// sweep only delays garbage collection.
pub fn spawn_expiry_sweep(store: PostgresStore) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_PERIOD);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = store.delete_expired().await {
                tracing::error!(error = %e, "session expiry sweep failed");
            }
        }
    });
}

/// Where a session's identity claim came from. A source mismatch flushes the session, which is
/// exactly what a deployment flipping between the proxy and native modes needs: neither mode ever
/// inherits the other's cookie.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IdentitySource {
    /// The oauth2-proxy sidecar asserted it in `X-Auth-Request-*`.
    Proxy,
    /// This controller minted it from an ID token it validated itself.
    Native,
}

/// The validated claims a native session carries. Written once at `/auth/callback` and read by the
/// auth middleware on every subsequent request; the login here is the one [`SessionIdentity`] binds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeClaims {
    pub sub: String,
    pub login: String,
    pub email: Option<String>,
    pub groups: Vec<String>,
    /// When `groups` was last the issuer's word, RFC3339. Login stamps it; the on-use refresh
    /// restamps it, and its age is what decides whether the next request refreshes at all.
    #[serde(default)]
    pub groups_at: String,
    /// Set when a group refresh was definitively refused. The login stands and every role it
    /// carried is gone until the user signs in again.
    #[serde(default)]
    pub downgraded: bool,
}

/// An authorization-code request in flight: what `/auth/callback` needs to finish the exchange and
/// to prove the response belongs to the request this browser started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginFlow {
    pub state: String,
    pub nonce: String,
    pub pkce_verifier: String,
    /// The in-app path to land on afterwards. Always same-origin and absolute — the login route
    /// refuses anything else, so the callback cannot be turned into an open redirect.
    pub redirect_to: String,
}

/// Every session slot holds its value as JSON text: the store's MessagePack encoding does not
/// round-trip a `serde_json` number, so values never reach it as structured JSON.
async fn write_slot<T: Serialize + ?Sized>(
    session: &Session,
    key: &str,
    value: &T,
) -> Result<(), tower_sessions::session::Error> {
    let text = serde_json::to_string(value)?;
    session.insert(key, &text).await
}

/// Read a slot [`write_slot`] wrote. A slot holding anything but text is absent.
async fn read_slot<T: serde::de::DeserializeOwned>(
    session: &Session,
    key: &str,
) -> Result<Option<T>, tower_sessions::session::Error> {
    decode_slot(session.get(key).await?)
}

/// Remove a slot [`write_slot`] wrote, returning its value.
async fn take_slot<T: serde::de::DeserializeOwned>(
    session: &Session,
    key: &str,
) -> Result<Option<T>, tower_sessions::session::Error> {
    decode_slot(session.remove(key).await?)
}

fn decode_slot<T: serde::de::DeserializeOwned>(
    slot: Option<serde_json::Value>,
) -> Result<Option<T>, tower_sessions::session::Error> {
    match slot {
        Some(serde_json::Value::String(text)) => {
            Ok(Some(crucible_contract::json::from_str(&text)?))
        }
        _ => Ok(None),
    }
}

/// Park an in-flight authorization-code request on the session. The id is cycled first: the flow
/// state is what a session fixation attack would want to plant.
pub async fn start_login(
    session: &Session,
    flow: &LoginFlow,
) -> Result<(), tower_sessions::session::Error> {
    session.cycle_id().await?;
    write_slot(session, FLOW_KEY, flow).await
}

/// Take the in-flight request back off the session. Removed whether or not the callback succeeds,
/// so one authorization response can never be replayed against a second code.
pub async fn take_login(
    session: &Session,
) -> Result<Option<LoginFlow>, tower_sessions::session::Error> {
    take_slot(session, FLOW_KEY).await
}

/// Promote a validated login into a session: everything the pre-login session held is dropped, the
/// id is cycled, and the identity plus its claims are written.
pub async fn establish_native(
    session: &Session,
    claims: &NativeClaims,
    id_token: Option<&str>,
) -> Result<(), tower_sessions::session::Error> {
    session.flush().await?;
    session.cycle_id().await?;
    write_slot(
        session,
        IDENTITY_KEY,
        &SessionIdentity {
            user: claims.login.clone(),
            source: IdentitySource::Native,
        },
    )
    .await?;
    if let Some(id_token) = id_token {
        write_slot(session, ID_TOKEN_KEY, id_token).await?;
    }
    write_slot(session, CLAIMS_KEY, claims).await
}

/// The ID token this session was minted from, for RP-initiated logout's `id_token_hint`.
pub async fn id_token(session: &Session) -> Result<Option<String>, tower_sessions::session::Error> {
    read_slot(session, ID_TOKEN_KEY).await
}

/// The native claims on this session, if it carries a native identity. `None` for an anonymous
/// session, and for a proxy-sourced one — the caller then has no controller-minted identity.
pub async fn native_claims(
    session: &Session,
) -> Result<Option<NativeClaims>, tower_sessions::session::Error> {
    let identity: Option<SessionIdentity> = read_slot(session, IDENTITY_KEY).await?;
    if identity.map(|i| i.source) != Some(IdentitySource::Native) {
        return Ok(None);
    }
    read_slot(session, CLAIMS_KEY).await
}

/// Restamp a live native session's claims after an on-use group refresh. The identity slot is
/// untouched: a refresh changes what a caller may do, never who they are.
pub async fn restamp_native(
    session: &Session,
    claims: &NativeClaims,
) -> Result<(), tower_sessions::session::Error> {
    write_slot(session, CLAIMS_KEY, claims).await
}

/// End a session: the row is deleted and the cookie is cleared.
pub async fn end(session: &Session) -> Result<(), tower_sessions::session::Error> {
    session.flush().await
}

/// The identity a session is bound to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIdentity {
    pub user: String,
    pub source: IdentitySource,
}

/// A session verified against the request's proxy-asserted identity. Handlers that touch session
/// state extract this, never a raw [`Session`] — its accessors are the enforcement point.
///
/// Binding rules:
/// * stored identity == claimed identity (both may be absent): pass through.
/// * a session that carries anything under a different identity claim — a changed user, an
///   anonymous request presenting an identified session, or an identified request presenting a
///   session with anonymous state — is flushed and its id cycled, so a cookie never carries
///   state across an identity change.
/// * an identified request with an empty session stays row-free on reads; the identity is
///   seeded (with a fresh id) only when a handler first writes.
///
/// Store failures reject as 500, never 401: the SPA treats any 401 as "proxy session expired"
/// and bounces the user through `/oauth2/start`.
pub struct BoundSession {
    session: Session,
    identity: Option<SessionIdentity>,
    seeded: bool,
}

impl BoundSession {
    pub fn identity(&self) -> Option<&SessionIdentity> {
        self.identity.as_ref()
    }

    pub async fn get<T: serde::de::DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>, tower_sessions::session::Error> {
        read_slot(&self.session, key).await
    }

    /// Write a value. The first write on an identified request also cycles the session id and
    /// seeds the identity slot, so a pre-identity session id never becomes an identified one.
    pub async fn insert<T: Serialize + Sync>(
        &mut self,
        key: &str,
        value: &T,
    ) -> Result<(), tower_sessions::session::Error> {
        if !self.seeded {
            if let Some(identity) = &self.identity {
                self.session.cycle_id().await?;
                write_slot(&self.session, IDENTITY_KEY, identity).await?;
            }
            self.seeded = true;
        }
        write_slot(&self.session, key, value).await
    }
}

fn internal_error(context: &str, e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "{context}");
    (StatusCode::INTERNAL_SERVER_ERROR, "session store error").into_response()
}

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for BoundSession {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let session = Session::from_request_parts(parts, state)
            .await
            .map_err(|(_, msg)| internal_error("extracting the session", msg))?;
        let Ok(identity) =
            crate::identity::session::Identity::from_request_parts(parts, state).await;
        let source = crate::identity::session::identity_source(parts);
        let claimed = identity.as_deref().map(|user| SessionIdentity {
            user: user.to_string(),
            source,
        });

        let stored: Option<SessionIdentity> = read_slot(&session, IDENTITY_KEY)
            .await
            .map_err(|e| internal_error("loading the session identity", e))?;

        let empty = session.is_empty().await;
        let (identity, seeded) = match (stored, claimed) {
            (stored, claimed) if stored == claimed => {
                let seeded = claimed.is_some();
                (claimed, seeded)
            }
            (None, claimed) if empty => (claimed, false),
            (_, claimed) => {
                session
                    .flush()
                    .await
                    .map_err(|e| internal_error("flushing a rebound session", e))?;
                session
                    .cycle_id()
                    .await
                    .map_err(|e| internal_error("cycling a rebound session id", e))?;
                if let Some(claimed) = &claimed {
                    write_slot(&session, IDENTITY_KEY, claimed)
                        .await
                        .map_err(|e| internal_error("seeding the session identity", e))?;
                }
                (claimed, true)
            }
        };

        Ok(BoundSession {
            session,
            identity,
            seeded,
        })
    }
}

/// The identity [`require_auth`] proved, stamped as a request extension. One shape for every
/// credential, so a handler never has to know which one authenticated the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub user: Option<String>,
    pub groups: Vec<String>,
    /// The issuer's stable subject, on the two paths that carry one (a native session, a validated
    /// JWT). The secrets registry and the schedule owner are spelled in logins; this is what a
    /// durable per-user row is keyed by.
    pub sub: Option<String>,
    /// Which identity model minted it. A session whose stored source differs is flushed, so a
    /// deployment flipping between modes never inherits the other mode's cookie.
    pub source: IdentitySource,
}

impl Resolved {
    pub(crate) fn anonymous(source: IdentitySource) -> Self {
        Resolved {
            user: None,
            groups: Vec::new(),
            sub: None,
            source,
        }
    }

    pub(crate) fn named(source: IdentitySource, user: impl Into<String>) -> Self {
        Resolved {
            user: Some(user.into()),
            groups: Vec::new(),
            sub: None,
            source,
        }
    }
}

/// Which identity model the request's proven identity came from, for the session binding. Requests
/// the middleware never touched read as `Proxy`, the model every existing deployment runs.
pub(crate) fn identity_source(parts: &Parts) -> IdentitySource {
    parts
        .extensions
        .get::<Resolved>()
        .map(|r| r.source)
        .unwrap_or(IdentitySource::Proxy)
}

/// Who is making the request: whatever [`crate::identity::auth::require_auth`] proved. `None` for an anonymous caller.
///
/// Trust boundary: a client-written value never reaches here. The middleware stamps [`Resolved`] on
/// every request it admits, and this extractor prefers it; the `X-Auth-Request-*` fallback below is
/// reachable only where the middleware never ran.
#[derive(Debug, Clone, Default)]
pub struct Identity(pub(crate) Option<String>);

impl Identity {
    pub(crate) fn as_deref(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Identity {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        if let Some(resolved) = parts.extensions.get::<Resolved>() {
            return Ok(Identity(resolved.user.clone()));
        }
        Ok(Identity(asserted_user(&parts.headers)))
    }
}

/// The identity an oauth2-proxy edge asserted, read off the headers it injects.
fn asserted_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

pub(crate) fn asserted_user(headers: &HeaderMap) -> Option<String> {
    asserted_header(headers, "x-auth-request-user")
        .or_else(|| asserted_header(headers, "x-auth-request-email"))
        .map(str::to_string)
}

pub(crate) fn asserted_groups(headers: &HeaderMap) -> Vec<String> {
    asserted_header(headers, "x-auth-request-groups")
        .map(|v| {
            v.split(',')
                .map(|g| g.trim().to_lowercase())
                .filter(|g| !g.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_sessions::SessionStore;
    use tower_sessions::session::{Id, Record};

    fn record(expires_in: tower_sessions::cookie::time::Duration) -> Record {
        Record {
            id: Id::default(),
            data: std::collections::HashMap::from([(
                "k".to_string(),
                serde_json::json!("{\"v\":1}"),
            )]),
            expiry_date: tower_sessions::cookie::time::OffsetDateTime::now_utc() + expires_in,
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn store_roundtrips_against_our_migration(pool: sqlx::PgPool) {
        let store = store(&pool).await.expect("store");
        let mut rec = record(Duration::hours(1));
        store.create(&mut rec).await.expect("create");

        let loaded = store.load(&rec.id).await.expect("load");
        assert_eq!(loaded.as_ref().map(|r| &r.data), Some(&rec.data));

        rec.data
            .insert("k2".to_string(), serde_json::json!("second"));
        store.save(&rec).await.expect("save");
        let loaded = store.load(&rec.id).await.expect("reload").expect("present");
        assert_eq!(loaded.data, rec.data);

        store.delete(&rec.id).await.expect("delete");
        assert!(
            store
                .load(&rec.id)
                .await
                .expect("post-delete load")
                .is_none()
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_slot_keeps_its_numbers_through_the_store(pool: sqlx::PgPool) {
        let store = std::sync::Arc::new(store(&pool).await.expect("store"));
        let session = Session::new(None, store.clone(), None);
        let value = serde_json::json!({"count": 1, "ratio": 2.5, "nested": {"n": [3]}});
        write_slot(&session, "k", &value).await.expect("write");
        session.save().await.expect("save");
        let id = session.id().expect("saved sessions have an id");

        let reloaded = Session::new(Some(id), store, None);
        let got: Option<serde_json::Value> = read_slot(&reloaded, "k").await.expect("read");
        assert_eq!(got, Some(value));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn expired_rows_are_invisible_and_reaped(pool: sqlx::PgPool) {
        let store = store(&pool).await.expect("store");
        let mut rec = record(Duration::hours(-1));
        store.create(&mut rec).await.expect("create");

        assert!(
            store.load(&rec.id).await.expect("load").is_none(),
            "expired rows never load"
        );
        let live: i64 = sqlx::query_scalar("select count(*) from sessions")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(live, 1, "the dead row still exists before the sweep");

        store.delete_expired().await.expect("sweep");
        let live: i64 = sqlx::query_scalar("select count(*) from sessions")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(live, 0);
    }

    /// Flipping a deployment from native mode back to proxy mode must not let a controller-minted
    /// session keep working as a proxy-asserted one: the source tag differs, so the binding flushes
    /// it and the user signs in again through the sidecar.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn flipping_native_to_proxy_flushes_a_native_session(pool: sqlx::PgPool) {
        use axum::body::Body;
        use axum::routing::get;
        use tower::util::ServiceExt;

        let claims = NativeClaims {
            sub: "sub-alice".to_string(),
            login: "alice".to_string(),
            email: None,
            groups: vec!["/groups/team-x".to_string()],
            groups_at: jiff::Timestamp::now().to_string(),
            downgraded: false,
        };
        let store = store(&pool).await.expect("session store");
        let app = axum::Router::new()
            .route(
                "/bound",
                get(|_bound: BoundSession, session: Session| async move {
                    match native_claims(&session).await.expect("claims") {
                        Some(c) => c.login,
                        None => "flushed".to_string(),
                    }
                }),
            )
            // Proxy mode: the guard asserts the SAME login, from the other identity model.
            .layer(axum::middleware::from_fn(
                |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
                    req.extensions_mut()
                        .insert(crate::identity::session::Resolved {
                            user: Some("alice".to_string()),
                            groups: Vec::new(),
                            sub: None,
                            source: IdentitySource::Proxy,
                        });
                    next.run(req).await
                },
            ))
            .route(
                "/establish",
                get(move |session: Session| {
                    let claims = claims.clone();
                    async move {
                        establish_native(&session, &claims, None)
                            .await
                            .expect("establish");
                        "signed in"
                    }
                }),
            )
            .layer(layer(store, false));

        let res = app
            .clone()
            .oneshot(
                axum::http::Request::get("/establish")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("infallible");
        let cookie = res
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap_or_default().to_string())
            .expect("the login set a cookie");

        let res = app
            .oneshot(
                axum::http::Request::get("/bound")
                    .header(axum::http::header::COOKIE, &cookie)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("infallible");
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(
            String::from_utf8_lossy(&body),
            "flushed",
            "a native session must not survive the flip back to proxy mode"
        );
    }

    /// The shared-cluster shape: the database user's `search_path` points at its own schema, the
    /// unqualified migration lands the table there, and the store must follow `current_schema()`
    /// instead of assuming `public`.
    #[tokio::test]
    async fn store_follows_a_non_public_search_path() {
        use sqlx::migrate::MigrateDatabase;
        let url = crate::test_ledger_url();
        sqlx::Postgres::create_database(&url)
            .await
            .expect("create db");
        let opts: sqlx::postgres::PgConnectOptions = url.parse().expect("url");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_with(opts.options([("search_path", "app")]))
            .await
            .expect("connect");
        sqlx::query("create schema app")
            .execute(&pool)
            .await
            .expect("schema");
        crate::MIGRATOR.run(&pool).await.expect("migrate");

        let store = store(&pool).await.expect("store");
        let mut rec = record(Duration::hours(1));
        store.create(&mut rec).await.expect("create");
        assert!(store.load(&rec.id).await.expect("load").is_some());

        let rows: i64 = sqlx::query_scalar("select count(*) from app.sessions")
            .fetch_one(&pool)
            .await
            .expect("count in the app schema");
        assert_eq!(rows, 1, "the row must live in the search_path schema");

        pool.close().await;
        let _ = sqlx::Postgres::drop_database(&url).await;
    }
}
