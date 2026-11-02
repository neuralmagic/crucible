//! The hosted MCP surface: the operations `crux` gives a shell, spoken over HTTP to any
//! MCP client, authenticated by an API key.
//!
//! Two things make this cheap. The tools, their DTOs, and their renderers already exist once in
//! `crux` above a [`Transport`]; and an axum `Router` is a `tower::Service`, so this
//! process can answer its own API calls by calling the router directly. No socket, no second
//! authentication, no duplicated handler. The controller answers the tools the way its own tests
//! drive handlers.
//!
//! ```text
//!   MCP client ──Bearer crk_…──▶ require_api_key ──▶ StreamableHttpService (stateless)
//!                                      │                      │
//!                              stamps Resolved         a tool calls Client
//!                              into CALLER                     │
//!                                                       RouterTransport
//!                                                              │
//!                                             api::router().oneshot(request)
//!                                             carrying that same Resolved
//! ```
//!
//! Identity rides a task-local rather than a constructor argument because rmcp builds its handler
//! from a factory that takes none. In stateless mode that factory runs once per request, inside
//! [`StreamableHttpService::handle`] and so inside the scope opened below, and the caller is
//! captured into the handler before rmcp spawns the task that serves it — which is what keeps the
//! identity attached to the request rather than to a session that could outlive it.

#![allow(clippy::disallowed_macros)]

use crate::identity::auth::AuthPath;
use crate::identity::session::Resolved;
use axum::Router;
use axum::body::Body;
use axum::response::IntoResponse as _;
use crux::client::Transport;
use http_body_util::BodyExt as _;
use std::sync::Arc;
use tower::ServiceExt as _;

tokio::task_local! {
    /// Who opened the MCP session being served on this task.
    static CALLER: Resolved;
}

/// The in-process wire: an API request answered by this controller's own router.
struct RouterTransport {
    api: Router,
    /// Stamped onto every synthetic request, so a handler reads exactly what it would have read
    /// had the call arrived over the network under the same key.
    caller: Resolved,
    /// What `web_url` builds links against. The tools hand run and draft links to an agent, and a
    /// link to `127.0.0.1` would be a link to the controller's own pod.
    base: String,
}

#[async_trait::async_trait]
impl Transport for RouterTransport {
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> anyhow::Result<(reqwest::StatusCode, String)> {
        let body = match body {
            Some(value) => Body::from(serde_json::to_vec(value)?),
            None => Body::empty(),
        };
        let mut request = http::Request::builder()
            .method(method.as_str())
            .uri(path)
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(http::header::ACCEPT, "application/json")
            .body(body)?;
        // What require_auth would have stamped. This router is never layered with that guard —
        // the key was checked at the edge of the MCP surface — so the extensions are put on here.
        request.extensions_mut().insert(self.caller.clone());
        request.extensions_mut().insert(AuthPath::ApiKey);

        let response = self
            .api
            .clone()
            .oneshot(request)
            .await
            .map_err(|e| anyhow::anyhow!("dispatching {path} in process: {e}"))?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| anyhow::anyhow!("reading the {path} response: {e}"))?
            .to_bytes();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        Ok((reqwest::StatusCode::from_u16(status.as_u16())?, text))
    }

    fn base(&self) -> &str {
        &self.base
    }
}

/// The MCP surface, guarded by its own credential.
///
/// Merged outside the human bearer guard by [`crate::Surface::bind`], like `/metrics` and the
/// pod evidence routes — each of those authenticates with the credential its callers actually hold, and
/// an MCP client holds an API key.
pub fn router(api: Router, keys: crate::identity::auth::KeyGuard, public_url: String) -> Router {
    let mut config = rmcp::transport::streamable_http_server::StreamableHttpServerConfig::default();
    config.allowed_hosts = allowed_hosts(&public_url);
    // Stateless: every request is a POST that stands on its own, answered as plain JSON rather
    // than an SSE frame. It fits what authenticates here — the key rides every request, so there
    // is nothing a session would remember that the credential does not already say — and it leaves
    // no per-session state for a restart or a second replica to lose.
    config.legacy_session_mode = false;
    config.json_response = true;
    let service = rmcp::transport::streamable_http_server::StreamableHttpService::new(
        move || {
            // Inside the task-local scope opened below; a session with no caller cannot happen,
            // because require_api_key runs first and refuses everything it cannot name.
            let caller = CALLER
                .try_with(Clone::clone)
                .map_err(|_| std::io::Error::other("no authenticated caller on this task"))?;
            Ok(crux::CrucibleMcp::new(Arc::new(crux::Client::new(
                Arc::new(RouterTransport {
                    api: api.clone(),
                    caller,
                    base: public_url.clone(),
                }),
            ))))
        },
        Arc::new(
            rmcp::transport::streamable_http_server::session::local::LocalSessionManager::default(),
        ),
        config,
    );

    Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn(carry_caller))
        .layer(axum::middleware::from_fn_with_state(
            keys,
            crate::identity::auth::require_api_key,
        ))
}

/// The `Host` values this surface answers on.
///
/// rmcp defaults to loopback only, because a locally-running MCP server is the case DNS rebinding
/// attacks it. A hosted one has to name its own hostnames or it 403s every real request — and the
/// hostname it is reached on is not necessarily the one it builds links against, since the machine
/// API rides its own path-scoped Route. `CONTROLLER_MCP_ALLOWED_HOSTS` is that list;
/// `CONTROLLER_PUBLIC_URL`'s authority and loopback are always in it, so a dev run needs no config
/// and a deployment that forgets the variable still serves the surface its links point at.
fn allowed_hosts(public_url: &str) -> Vec<String> {
    let mut hosts = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "[::1]".to_string(),
    ];
    if let Ok(url) = public_url.parse::<http::Uri>()
        && let Some(authority) = url.authority()
    {
        hosts.push(authority.to_string());
        hosts.push(authority.host().to_string());
    }
    if let Ok(configured) = std::env::var("CONTROLLER_MCP_ALLOWED_HOSTS") {
        hosts.extend(
            configured
                .split(',')
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(str::to_string),
        );
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Move the identity the guard proved onto the task, where the rmcp session factory can reach it.
///
/// A request with no identity cannot get here — `require_api_key` runs first and refuses anything
/// it cannot name — so a missing one is a layering mistake, and the honest answer to it is to serve
/// nothing rather than to invent an anonymous caller with a session of their own.
async fn carry_caller(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let Some(caller) = req.extensions().get::<Resolved>().cloned() else {
        tracing::error!("mcp request reached the session layer unauthenticated");
        return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    CALLER.scope(caller, next.run(req)).await
}

#[cfg(test)]
mod tests {
    use crate::mcp::*;

    struct NoSink;

    impl crate::daemon::queue::OverrideSink for NoSink {
        fn submit(&self, _ov: crate::daemon::queue::Override) {}
    }

    /// Bind a new secret owned by `owner` to playbook scope `scope` as `declared`.
    async fn bind(pool: &sqlx::PgPool, owner: &str, scope: &str, declared: &str) {
        let owner = crate::authz::model::Principal::parse(owner).expect("owner");
        let name = crate::secrets::SecretName::parse(declared).expect("name");
        let id = uuid::Uuid::now_v7().to_string();
        let mut conn = pool.acquire().await.expect("conn");
        crate::secrets::store::insert(
            &mut conn,
            &crate::secrets::store::NewSecret {
                id: &id,
                name: &name,
                owner: &owner,
                kind: crate::secrets::SecretKind::Opaque,
                visibility: crate::secrets::Visibility::BrokerOnly,
                consumer: crate::secrets::ConsumerClass::Run,
                mode: crate::secrets::SecretMode::Managed,
                vault_path: &format!("{owner}/{declared}"),
                current_version: Some(1),
                created_by: None,
            },
        )
        .await
        .expect("register");
        crate::secrets::store::insert_binding(
            &mut conn,
            &crate::secrets::store::NewBinding {
                id: &uuid::Uuid::now_v7().to_string(),
                secret_id: &id,
                scope_kind: crate::secrets::ScopeKind::Playbook,
                scope_id: scope,
                projection_kind: crate::secrets::ProjectionKind::Env,
                projection: &declared.to_uppercase().replace('-', "_"),
                declared_name: &name,
                pack_rev: None,
                schema_digest: None,
                created_by: None,
            },
        )
        .await
        .expect("bind");
    }

    /// `crux draft-rename` and `crux draft-clone` against this controller's own router: the CLI's
    /// client, DTOs and renderers over the real handlers and database.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn crux_renames_and_clones_a_draft(pool: sqlx::PgPool) {
        let api = crate::api::router(crate::api::state::ApiState::test(
            crate::client::Db::new(pool.clone()),
            Arc::new(NoSink),
        ));
        let client = crux::Client::new(Arc::new(RouterTransport {
            api,
            caller: Resolved::named(crate::identity::session::IdentitySource::Proxy, "alice"),
            base: "https://crucible.example.com".to_string(),
        }));
        crux::ops::draft_create(&client, "studio", "a drafted pack", None, false)
            .await
            .expect("create");
        bind(&pool, "user:alice", "studio", "gh-token").await;
        bind(&pool, "user:bob", "studio", "bob-token").await;

        let renamed = crux::ops::draft_rename(&client, "studio", "survey-v2", false)
            .await
            .expect("rename");
        assert_eq!(
            renamed,
            "renamed draft studio to survey-v2\n\
             moved: 2 secret bindings, 0 shares, 0 standing launches, 0 launches\n\
             studio: https://crucible.example.com/playbooks/drafts/survey-v2\n"
        );

        let cloned = crux::ops::draft_clone(&client, "survey-v2", "copy", None, false)
            .await
            .expect("clone");
        assert_eq!(
            cloned,
            "cloned draft survey-v2 to copy at version 1\n\
             bound: gh-token\n\
             not copied:\n  bob-token: you may not bind its secret\n\
             studio: https://crucible.example.com/playbooks/drafts/copy\n"
        );
        let copy = crate::playbooks::drafts::get(&pool, "copy")
            .await
            .expect("read")
            .expect("the copy");
        assert_eq!(
            copy.description, "a drafted pack",
            "defaults to the source's"
        );

        let taken = crux::ops::draft_rename(&client, "survey-v2", "copy", false)
            .await
            .expect_err("the id is taken");
        assert!(taken.to_string().contains("409"), "{taken:#}");
        assert!(
            crate::playbooks::drafts::get(&pool, "survey-v2")
                .await
                .expect("read")
                .is_some()
        );

        assert_eq!(
            crux::ops::draft_delete(&client, "copy")
                .await
                .expect("delete"),
            "deleted draft copy\n"
        );
        let left = crate::secrets::store::bindings_for_scope(
            &pool,
            crate::secrets::ScopeKind::Playbook,
            "copy",
        )
        .await
        .expect("bindings");
        assert!(left.is_empty(), "{left:?}");
    }
}
