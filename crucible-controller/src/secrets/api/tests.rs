//! The secrets registry's routes, against a real Postgres and a real Vault.
//!
//! No mocks: a registration in these tests writes a KV version to a live `vault server -dev` and
//! the assertions read it back with the root token. Where Vault is unreachable the Vault-backed
//! tests skip (and CI's `CRUCIBLE_REQUIRE_VAULT_TESTS` turns that skip into a failure); the
//! authorization tests need no Vault at all, because every refusal they assert happens before the
//! registry would reach for one.

use super::*;
use crate::api::{openapi_spec, router};
use crate::client::Db;
use crate::daemon::queue::Override;
use crate::daemon::queue::OverrideSink;
use crate::identity::auth::AuthPath;
use crate::secrets::store;
use crate::secrets::vault::dev::{DevVault, uniq};
use crate::testing::call;
use axum::Router;
use axum::body::Body;
use axum::http::Request as HttpRequest;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;

/// A sink that records nothing: the registry never enqueues an override.
#[derive(Default)]
struct NoSink;

impl OverrideSink for NoSink {
    fn submit(&self, _ov: Override) {}
}

/// The registry router with `alice` as an operator, plus whatever Vault client the test has.
fn app(pool: PgPool, vault: Option<Arc<crate::secrets::vault::VaultClient>>) -> Router {
    router(ApiState {
        roles: crate::identity::auth::Roles::new(
            vec![],
            vec!["alice".into(), "bob".into()],
            vec![],
        ),
        vault,
        ..ApiState::test(Db::new(pool), Arc::new(NoSink))
    })
}

/// One request as `user`, carrying `groups` as the edge would assert them.
fn as_user(
    method: &str,
    path: &str,
    user: &str,
    groups: &[&str],
    body: Option<Value>,
) -> HttpRequest<Body> {
    let mut b = HttpRequest::builder()
        .method(method)
        .uri(path)
        .header("x-auth-request-user", user);
    if !groups.is_empty() {
        b = b.header("x-auth-request-groups", groups.join(","));
    }
    match body {
        Some(body) => b
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request"),
        None => b.body(Body::empty()).expect("request"),
    }
}

/// `let Some(rig) = rig(&pool).await else { return };` — the skip is the only way out.
struct Rig {
    vault: DevVault,
    mount: String,
    app: Router,
}

async fn rig(pool: &PgPool) -> Option<Rig> {
    let vault = DevVault::start().await?;
    let mount = uniq("crucible");
    let client = vault.hub_client(&mount).await;
    let app = app(pool.clone(), Some(Arc::new(client)));
    Some(Rig { vault, mount, app })
}

fn register_body(name: &str, owner: Option<&str>, value: &str) -> Value {
    let mut body = json!({"name": name, "kind": "opaque", "value": value});
    if let Some(owner) = owner {
        body["owner"] = json!(owner);
    }
    body
}

// --- authorization, which needs no Vault --------------------------------------------------------

/// Registering as a group the caller does not hold is a 403 whatever the group is, and the message
/// says what they do hold rather than whether the group exists.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_group_outside_the_callers_claims_is_refused(pool: PgPool) {
    let app = app(pool.clone(), None);
    let (status, body) = call(
        &app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &["/groups/team-x"],
            Some(register_body("k", Some("group:/groups/team-y"), "v")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("group:/groups/team-y"),
        "{body}"
    );
    assert_eq!(
        store::list(&pool, None).await.expect("list").len(),
        0,
        "a refused registration stores nothing"
    );
}

/// The tail-segment match that grants the operator role must not grant ownership: a caller whose
/// claim is `/groups/team-x` does not own `group:team-x`.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn owning_a_group_is_a_whole_path_match(pool: PgPool) {
    let app = app(pool, None);
    let (status, _) = call(
        &app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &["/groups/team-x"],
            Some(register_body("k", Some("group:team-x"), "v")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// The static API token proves no group claims, so a `group:` owner on that path is not a
/// principal the caller acts as, whatever the asserted header says.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn the_static_token_path_refuses_a_group_owner(pool: PgPool) {
    let app = app(pool, None);
    let mut req = as_user(
        "POST",
        "/api/secrets",
        "alice",
        &["/groups/team-x"],
        Some(register_body("k", Some("group:/groups/team-x"), "v")),
    );
    req.extensions_mut().insert(AuthPath::StaticToken);
    let (status, body) = call(&app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("act as"),
        "{body}"
    );

    // The same request over the edge path, where the claims are the edge's, is not refused here:
    // it gets as far as the missing Vault client.
    let mut req = as_user(
        "POST",
        "/api/secrets",
        "alice",
        &["/groups/team-x"],
        Some(register_body("k", Some("group:/groups/team-x"), "v")),
    );
    req.extensions_mut().insert(AuthPath::Edge);
    let (status, _) = call(&app, req).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

/// Any signed-in user may register a secret of their own (RFC-0003 C-POLICY: a user owner holds
/// every action on what they own); no global role stands in front of the registry. An anonymous
/// caller has no principal to own one.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn registration_needs_a_principal_and_no_role(pool: PgPool) {
    let app = app(pool, None);
    let (status, body) = call(
        &app,
        as_user(
            "POST",
            "/api/secrets",
            "carol",
            &[],
            Some(register_body("k", None, "v")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let mut req = HttpRequest::builder()
        .method("POST")
        .uri("/api/secrets")
        .header("content-type", "application/json")
        .body(Body::from(register_body("k", None, "v").to_string()))
        .expect("request");
    req.extensions_mut().insert(AuthPath::Session);
    let (status, body) = call(&app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

/// The kind rules are the caller's business and are answered before the deployment's Vault is.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn file_shaped_kinds_refuse_agent_visible(pool: PgPool) {
    let app = app(pool, None);
    for kind in ["file", "registry_authfile", "kubeconfig"] {
        let (status, body) = call(
            &app,
            as_user(
                "POST",
                "/api/secrets",
                "alice",
                &[],
                Some(json!({
                    "name": "k", "kind": kind, "visibility": "agent_visible", "value": "v"
                })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{kind}: {body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("agent_visible"),
            "{kind}: {body}"
        );
    }
}

/// A minted registration needs no Vault, records no version, and carries its minter.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_minted_secret_registers_with_no_vault_and_no_stored_version(pool: PgPool) {
    let app = app(pool.clone(), None);
    let (status, body) = call(
        &app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &[],
            Some(json!({"name": "pr-token", "kind": "opaque", "mint": "github-app"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["mode"], "minted", "{body}");
    assert_eq!(body["vault_path"], "mint://github-app", "{body}");
    assert!(body["current_version"].is_null(), "{body}");

    let stored = store::list(&pool, None).await.expect("list");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].mode, crate::secrets::SecretMode::Minted);
    assert_eq!(stored[0].current_version, None);
}

/// A minter issues a bearer token, so the registration has to be shaped like one.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_minted_secret_must_be_an_opaque_run_credential(pool: PgPool) {
    let app = app(pool.clone(), None);
    for (body, expected) in [
        (
            json!({"name": "k", "kind": "registry_authfile", "mint": "github-app"}),
            "cannot be minted",
        ),
        (
            json!({"name": "k", "kind": "opaque", "consumer": "hub", "mint": "github-app"}),
            "kubeconfig",
        ),
    ] {
        let (status, got) = call(
            &app,
            as_user("POST", "/api/secrets", "alice", &[], Some(body)),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{got}");
        assert!(
            got["error"].as_str().unwrap_or_default().contains(expected),
            "{got}"
        );
    }
    assert!(store::list(&pool, None).await.expect("list").is_empty());
}

/// The three sources are exclusive, and an unknown minter is refused by name.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_registration_names_exactly_one_source(pool: PgPool) {
    let app = app(pool.clone(), None);
    for body in [
        json!({"name": "k", "kind": "opaque", "value": "v", "mint": "github-app"}),
        json!({"name": "k", "kind": "opaque"}),
    ] {
        let (status, got) = call(
            &app,
            as_user("POST", "/api/secrets", "alice", &[], Some(body)),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{got}");
        assert!(
            got["error"]
                .as_str()
                .unwrap_or_default()
                .contains("exactly one"),
            "{got}"
        );
    }
    let (status, got) = call(
        &app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &[],
            Some(json!({"name": "k", "kind": "opaque", "mint": "gitlab-app"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{got}");
    assert!(
        got["error"]
            .as_str()
            .unwrap_or_default()
            .contains("gitlab-app"),
        "{got}"
    );
    assert!(store::list(&pool, None).await.expect("list").is_empty());
}

/// There is no stored version to replace.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_minted_secret_cannot_be_rotated(pool: PgPool) {
    let app = app(pool.clone(), None);
    let (status, created) = call(
        &app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &[],
            Some(json!({"name": "pr-token", "kind": "opaque", "mint": "github-app"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("id");

    let (status, body) = call(
        &app,
        as_user(
            "POST",
            &format!("/api/secrets/{id}/rotate"),
            "alice",
            &[],
            Some(json!({"value": "v"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("issued fresh at every dispatch"),
        "{body}"
    );
}

/// Nothing was written, so the delete works on a deployment with no Vault.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_minted_secret_deletes_without_a_vault_client(pool: PgPool) {
    let app = app(pool.clone(), None);
    let (status, created) = call(
        &app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &[],
            Some(json!({"name": "pr-token", "kind": "opaque", "mint": "github-app"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("id");

    let (status, body) = call(
        &app,
        as_user("DELETE", &format!("/api/secrets/{id}"), "alice", &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(store::list(&pool, None).await.expect("list").is_empty());
}

/// Registering without a Vault client is a 503 that stores nothing, not a metadata row pointing at
/// bytes that were never written.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn without_a_vault_client_registration_is_unavailable(pool: PgPool) {
    let app = app(pool.clone(), None);
    let (status, _) = call(
        &app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &[],
            Some(register_body("k", None, "v")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(store::list(&pool, None).await.expect("list").is_empty());
}

/// Reads keep working with no Vault client: the registry's metadata lives in Postgres, so listing,
/// fetching one secret with its bindings, and reading the audit trail all answer 200 while every
/// write route is 503. A deployment that never became a registry host still shows what it holds.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn the_read_routes_work_without_a_vault_client(pool: PgPool) {
    let owner = crate::authz::model::Principal::parse("user:alice").expect("principal");
    let name = crate::secrets::SecretName::parse("pr-token").expect("name");
    let declared = crate::secrets::SecretName::parse("pr-token").expect("declared name");
    let mut conn = pool.acquire().await.expect("conn");
    store::insert(
        &mut conn,
        &store::NewSecret {
            id: "sec-1",
            name: &name,
            owner: &owner,
            kind: crate::secrets::SecretKind::Opaque,
            visibility: crate::secrets::Visibility::BrokerOnly,
            consumer: crate::secrets::ConsumerClass::Run,
            mode: crate::secrets::SecretMode::Managed,
            vault_path: "user:alice/pr-token",
            current_version: Some(1),
            created_by: Some("user:alice"),
        },
    )
    .await
    .expect("insert");
    store::insert_binding(
        &mut conn,
        &store::NewBinding {
            id: "bind-1",
            secret_id: "sec-1",
            scope_kind: crate::secrets::ScopeKind::Repo,
            scope_id: "neuralmagic/crucible",
            projection_kind: crate::secrets::ProjectionKind::Env,
            projection: "PR_TOKEN",
            declared_name: &declared,
            pack_rev: None,
            schema_digest: None,
            created_by: Some("user:alice"),
        },
    )
    .await
    .expect("bind");
    store::audit(
        &mut conn,
        &store::NewAudit {
            secret_id: Some("sec-1"),
            secret_name: name.as_str(),
            owner: &owner,
            action: crate::secrets::AuditAction::Register,
            actor: Some(&owner),
            detail: None,
        },
    )
    .await
    .expect("audit");
    drop(conn);

    let app = app(pool.clone(), None);
    let (status, body) = call(&app, as_user("GET", "/api/secrets", "alice", &[], None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body[0]["id"], "sec-1");
    let (status, body) = call(
        &app,
        as_user("GET", "/api/secrets/sec-1", "alice", &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["vault_path"], "user:alice/pr-token");
    assert_eq!(body["bindings"][0]["projection"], "PR_TOKEN");
    let (status, body) = call(
        &app,
        as_user("GET", "/api/secrets/sec-1/audit", "alice", &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body[0]["action"], "register");

    // ...and every write route names the missing configuration rather than half-writing.
    for req in [
        as_user(
            "POST",
            "/api/secrets/sec-1/rotate",
            "alice",
            &[],
            Some(json!({"value": "v2"})),
        ),
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &[],
            Some(register_body("another", None, "v")),
        ),
    ] {
        let (status, body) = call(&app, req).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(
            body["error"].as_str().unwrap_or_default().contains("Vault"),
            "{body}"
        );
    }
}

// --- the registry against a real Vault ----------------------------------------------------------

/// Register as self and as a claimed group: the bytes land in Vault under the owner's path, the
/// row records the version, and the response carries metadata only.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_secret_registers_as_self_or_any_claimed_group(pool: PgPool) {
    let Some(rig) = rig(&pool).await else { return };

    let (status, body) = call(
        &rig.app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &["/groups/team-x"],
            Some(register_body("gh-token", None, "hunter2")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["owner"], "user:alice");
    assert_eq!(body["visibility"], "broker_only", "the default");
    assert_eq!(body["consumer"], "run");
    assert_eq!(body["mode"], "managed");
    assert_eq!(body["current_version"], 1);
    assert!(
        !body.to_string().contains("hunter2"),
        "the value never comes back: {body}"
    );
    assert_eq!(
        rig.vault
            .get(&rig.mount, "crucible/registry/user:alice/gh-token", "value")
            .await
            .as_deref(),
        Some("hunter2"),
        "the hub is the writer"
    );

    let (status, body) = call(
        &rig.app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &["/groups/team-x"],
            Some(register_body(
                "gh-token",
                Some("group:/groups/team-x"),
                "team-secret",
            )),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["owner"], "group:/groups/team-x");
    assert_eq!(
        rig.vault
            .get(
                &rig.mount,
                "crucible/registry/group:/groups/team-x/gh-token",
                "value"
            )
            .await
            .as_deref(),
        Some("team-secret"),
        "one name per owner, not per registry"
    );

    // The same owner and name twice is a conflict, not a silent overwrite of someone's value.
    let (status, _) = call(
        &rig.app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &[],
            Some(register_body("gh-token", None, "again")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        rig.vault
            .get(&rig.mount, "crucible/registry/user:alice/gh-token", "value")
            .await
            .as_deref(),
        Some("hunter2"),
        "the conflicting registration did not write"
    );
}

/// Rotation writes a new KV version and advances the row; nothing on the surface reads a value
/// back.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn rotation_writes_a_new_version_and_advances_the_row(pool: PgPool) {
    let Some(rig) = rig(&pool).await else { return };
    let (_, created) = call(
        &rig.app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &[],
            Some(register_body("gh-token", None, "first")),
        ),
    )
    .await;
    let id = created["id"].as_str().expect("an id").to_string();

    let (status, body) = call(
        &rig.app,
        as_user(
            "POST",
            &format!("/api/secrets/{id}/rotate"),
            "alice",
            &[],
            Some(json!({"value": "second"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["current_version"], 2);
    assert!(!body.to_string().contains("second"), "{body}");
    assert_eq!(
        rig.vault
            .get(&rig.mount, "crucible/registry/user:alice/gh-token", "value")
            .await
            .as_deref(),
        Some("second")
    );
    let stored = store::get(&pool, &id).await.expect("read").expect("a row");
    assert_eq!(stored.current_version, Some(2));

    // Somebody else's secret does not exist for them (RFC-0003 C-READ-SCOPING).
    let (status, _) = call(
        &rig.app,
        as_user(
            "POST",
            &format!("/api/secrets/{id}/rotate"),
            "bob",
            &[],
            Some(json!({"value": "third"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        rig.vault
            .get(&rig.mount, "crucible/registry/user:alice/gh-token", "value")
            .await
            .as_deref(),
        Some("second"),
        "a refused rotation writes nothing"
    );
}

/// A transfer moves the managed bytes to the new owner's path, re-owns the row, re-points a
/// provider that named the secret by owner, and leaves the old path empty. The new owner's members
/// can then act on it and the old owner cannot.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_transfer_moves_the_bytes_and_the_row_to_the_new_owner(pool: PgPool) {
    let Some(rig) = rig(&pool).await else { return };
    let (_, created) = call(
        &rig.app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &[],
            Some(json!({"name": "openai-key", "kind": "inference_api_key", "value": "sk-live"})),
        ),
    )
    .await;
    let id = created["id"].as_str().expect("an id").to_string();
    call(
        &rig.app,
        as_user(
            "POST",
            &format!("/api/secrets/{id}/rotate"),
            "alice",
            &[],
            Some(json!({"value": "sk-live-2"})),
        ),
    )
    .await;
    crate::playbooks::providers::insert(
        &pool,
        &crate::playbooks::providers::NewProvider {
            owner: crate::authz::model::Principal::platform(),
            id: "openai",
            display_name: "OpenAI",
            kind: crate::playbooks::providers::ProviderKind::OpenAi,
            models: &[],
            default_model: Some("gpt-5.6-luna"),
            secret: Some(&crate::playbooks::providers::ProviderSecretRef {
                name: "openai-key".into(),
                owner: crate::authz::model::Principal::parse("user:alice").expect("a principal"),
            }),
            endpoint: None,
            harness: None,
            enabled: true,
            created_by: "alice",
        },
    )
    .await
    .expect("a provider");

    let (status, body) = call(
        &rig.app,
        as_user(
            "PUT",
            &format!("/api/secrets/{id}/owner"),
            "alice",
            &["/groups/team-x"],
            Some(json!({"owner": "group:/groups/team-x"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["owner"], "group:/groups/team-x");
    assert_eq!(
        body["vault_path"],
        "crucible/registry/group:/groups/team-x/openai-key"
    );
    assert_eq!(
        body["current_version"], 1,
        "the copy is the new path's first version"
    );
    assert!(!body.to_string().contains("sk-live"), "{body}");

    assert_eq!(
        rig.vault
            .get(
                &rig.mount,
                "crucible/registry/group:/groups/team-x/openai-key",
                "value"
            )
            .await
            .as_deref(),
        Some("sk-live-2"),
        "the current version moved"
    );
    assert_eq!(
        rig.vault
            .get(
                &rig.mount,
                "crucible/registry/user:alice/openai-key",
                "value"
            )
            .await,
        None,
        "the old path is destroyed"
    );
    let stored = store::get(&pool, &id).await.expect("read").expect("a row");
    assert_eq!(stored.owner.to_string(), "group:/groups/team-x");
    assert_eq!(stored.current_version, Some(1));
    let provider = crate::playbooks::providers::get(&pool, "openai")
        .await
        .expect("read")
        .expect("a provider");
    assert_eq!(
        provider.secret.map(|s| s.owner.to_string()).as_deref(),
        Some("group:/groups/team-x"),
        "the provider follows the secret"
    );

    let (_, trail) = call(
        &rig.app,
        as_user(
            "GET",
            &format!("/api/secrets/{id}/audit"),
            "bob",
            &["/groups/team-x"],
            None,
        ),
    )
    .await;
    let latest = &trail.as_array().expect("rows")[0];
    assert_eq!(latest["action"], "transfer");
    assert_eq!(latest["owner"], "group:/groups/team-x");
    assert_eq!(latest["actor"], "user:alice");
    assert_eq!(latest["detail"], "from user:alice");

    // A member of the new owner rotates it at its new path; the old owner, no longer a member,
    // cannot touch it.
    let (status, body) = call(
        &rig.app,
        as_user(
            "POST",
            &format!("/api/secrets/{id}/rotate"),
            "bob",
            &["/groups/team-x"],
            Some(json!({"value": "sk-live-3"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["current_version"], 2);
    let (status, _) = call(
        &rig.app,
        as_user("GET", &format!("/api/secrets/{id}"), "alice", &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A transfer is refused when the caller holds only one side of it, when the target already has
/// the name, and when the target is the current owner. A refusal moves nothing in Vault.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_transfer_needs_both_principals_and_a_free_name(pool: PgPool) {
    let Some(rig) = rig(&pool).await else { return };
    let register = |name: &str, owner: Option<&str>, groups: &[&str]| {
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            groups,
            Some(register_body(name, owner, "v")),
        )
    };
    let (_, created) = call(&rig.app, register("gh-token", None, &[])).await;
    let id = created["id"].as_str().expect("an id").to_string();
    let (status, _) = call(
        &rig.app,
        register(
            "gh-token",
            Some("group:/groups/team-x"),
            &["/groups/team-x"],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let transfer = |user: &str, groups: &[&str], to: &str| {
        as_user(
            "PUT",
            &format!("/api/secrets/{id}/owner"),
            user,
            groups,
            Some(json!({"owner": to})),
        )
    };
    // Not a member of the target.
    let (status, body) = call(&rig.app, transfer("alice", &[], "group:/groups/team-y")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // Not a member of the current owner.
    let (status, body) = call(
        &rig.app,
        transfer("bob", &["/groups/team-y"], "group:/groups/team-y"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    // The target already has one by that name.
    let (status, body) = call(
        &rig.app,
        transfer("alice", &["/groups/team-x"], "group:/groups/team-x"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    // Already the owner.
    let (status, body) = call(&rig.app, transfer("alice", &[], "user:self")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    // Not a principal at all.
    let (status, body) = call(&rig.app, transfer("alice", &[], "team-x")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let stored = store::get(&pool, &id).await.expect("read").expect("a row");
    assert_eq!(stored.owner.to_string(), "user:alice");
    assert_eq!(
        rig.vault
            .get(&rig.mount, "crucible/registry/user:alice/gh-token", "value")
            .await
            .as_deref(),
        Some("v"),
        "a refused transfer moves nothing"
    );
    assert_eq!(
        rig.vault
            .get(
                &rig.mount,
                "crucible/registry/group:/groups/team-y/gh-token",
                "value"
            )
            .await,
        None
    );
}

/// A minted secret has no bytes to move, so its transfer needs no Vault client and changes only
/// the owner.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_minted_secret_transfers_without_a_vault_client(pool: PgPool) {
    let app = app(pool.clone(), None);
    let (status, created) = call(
        &app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &[],
            Some(json!({"name": "gh-app", "kind": "opaque", "mint": "github-app"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("an id").to_string();

    let (status, body) = call(
        &app,
        as_user(
            "PUT",
            &format!("/api/secrets/{id}/owner"),
            "alice",
            &["/groups/team-x"],
            Some(json!({"owner": "group:/groups/team-x"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["owner"], "group:/groups/team-x");
    assert_eq!(body["vault_path"], created["vault_path"]);
    assert_eq!(body["current_version"], Value::Null);
}

/// A controller API key never reaches an agent: the value is refused at registration, a rotation
/// into one is refused, and a binding is refused against the bytes stored now, not the ones the
/// registration saw.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn an_agent_visible_controller_key_is_refused_at_registration_rotation_and_bind(
    pool: PgPool,
) {
    let Some(rig) = rig(&pool).await else { return };
    let key = format!(
        "{}0123456789abcdef_s3cr3t",
        crate::identity::api_key::PREFIX
    );
    let path = "crucible/registry/user:alice/gh-token";

    let (status, body) = call(
        &rig.app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &[],
            Some(json!({
                "name": "gh-token", "kind": "opaque",
                "visibility": "agent_visible", "value": key
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("controller api key"),
        "{body}"
    );
    assert_eq!(
        rig.vault.get(&rig.mount, path, "value").await,
        None,
        "a refused registration writes nothing"
    );

    // The same value broker-only is its owner's business: no agent ever reads it.
    let (status, body) = call(
        &rig.app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &[],
            Some(json!({
                "name": "hub-key", "kind": "opaque",
                "visibility": "broker_only", "value": key
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, created) = call(
        &rig.app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &[],
            Some(json!({
                "name": "gh-token", "kind": "opaque",
                "visibility": "agent_visible", "value": "hunter2"
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("an id").to_string();

    let (status, body) = call(
        &rig.app,
        as_user(
            "POST",
            &format!("/api/secrets/{id}/rotate"),
            "alice",
            &[],
            Some(json!({ "value": key })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        rig.vault.get(&rig.mount, path, "value").await.as_deref(),
        Some("hunter2"),
        "a refused rotation writes nothing"
    );

    let bind = json!({
        "scope_kind": "repo",
        "scope_id": "owner/repo",
        "projection_kind": "env",
        "projection": "AUTORESEARCH_PR_TOKEN",
    });
    let (status, body) = call(
        &rig.app,
        as_user(
            "POST",
            &format!("/api/secrets/{id}/bindings"),
            "alice",
            &[],
            Some(bind.clone()),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a non-key value binds unaffected: {body}"
    );

    // A write straight to the registry path stands in for a rotation the API did not see.
    rig.vault.put(&rig.mount, path, "value", &key).await;
    let (status, body) = call(
        &rig.app,
        as_user(
            "POST",
            &format!("/api/secrets/{id}/bindings"),
            "alice",
            &[],
            Some(json!({
                "scope_kind": "repo",
                "scope_id": "owner/other",
                "projection_kind": "env",
                "projection": "AUTORESEARCH_PR_TOKEN",
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("controller api key"),
        "{body}"
    );
}

/// The verification read the registrant's own token performs refuses a pointer at someone else's
/// controller key before any row exists.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn an_agent_visible_reference_to_a_controller_key_is_refused(pool: PgPool) {
    let Some(rig) = rig(&pool).await else { return };
    let other = uniq("other");
    rig.vault.ensure_kv_mount(&other).await;
    rig.vault
        .put(
            &other,
            "team/creds",
            "api",
            &format!(
                "{}0123456789abcdef_s3cr3t",
                crate::identity::api_key::PREFIX
            ),
        )
        .await;
    let policy = uniq("reader");
    rig.vault.write_mount_policy(&policy, &other).await;
    let reader = rig.vault.issue_token(&[&policy]).await;
    let reference = format!("vault://{other}/team/creds#api");

    let (status, body) = call(
        &rig.app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &[],
            Some(json!({
                "name": "elsewhere", "kind": "opaque", "visibility": "agent_visible",
                "reference": reference, "vault_token": reader
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("controller api key"),
        "{body}"
    );
    assert!(store::list(&pool, None).await.expect("list").is_empty());

    let (status, body) = call(
        &rig.app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &[],
            Some(json!({
                "name": "elsewhere", "kind": "opaque", "visibility": "broker_only",
                "reference": reference, "vault_token": reader
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // A reference registered while its path held something harmless is re-read with the binder's
    // token, so a rotation at the far end is caught.
    rig.vault.put(&other, "team/other", "api", "hunter2").await;
    let harmless = format!("vault://{other}/team/other#api");
    let (status, body) = call(
        &rig.app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &[],
            Some(json!({
                "name": "far", "kind": "opaque", "visibility": "agent_visible",
                "reference": harmless, "vault_token": reader
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().expect("an id").to_string();

    rig.vault
        .put(
            &other,
            "team/other",
            "api",
            &format!(
                "{}fedcba9876543210_r0t4t3d",
                crate::identity::api_key::PREFIX
            ),
        )
        .await;
    let (status, body) = call(
        &rig.app,
        as_user(
            "POST",
            &format!("/api/secrets/{id}/bindings"),
            "alice",
            &[],
            Some(json!({
                "scope_kind": "repo",
                "scope_id": "owner/repo",
                "projection_kind": "env",
                "projection": "AUTORESEARCH_PR_TOKEN",
                "vault_token": reader,
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("controller api key"),
        "{body}"
    );
}

/// Bind and unbind need owner membership, and a bound secret cannot be deleted out from under the
/// scope that uses it.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn binding_needs_ownership_and_blocks_deletion(pool: PgPool) {
    let Some(rig) = rig(&pool).await else { return };
    let (_, created) = call(
        &rig.app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &["/groups/team-x"],
            Some(register_body(
                "gh-token",
                Some("group:/groups/team-x"),
                "hunter2",
            )),
        ),
    )
    .await;
    let id = created["id"].as_str().expect("an id").to_string();
    let bind = json!({
        "scope_kind": "repo",
        "scope_id": "owner/repo",
        "projection_kind": "env",
        "projection": "AUTORESEARCH_PR_TOKEN",
        "pack_rev": "abc123"
    });

    // bob is not a member of the owning group, so the secret does not exist for him.
    let (status, _) = call(
        &rig.app,
        as_user(
            "POST",
            &format!("/api/secrets/{id}/bindings"),
            "bob",
            &[],
            Some(bind.clone()),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // carol is in the owning group and holds no global role: the group's ownership is enough
    // to read it.
    let (status, _) = call(
        &rig.app,
        as_user(
            "GET",
            &format!("/api/secrets/{id}"),
            "carol",
            &["/groups/team-x"],
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, binding) = call(
        &rig.app,
        as_user(
            "POST",
            &format!("/api/secrets/{id}/bindings"),
            "alice",
            &["/groups/team-x"],
            Some(bind.clone()),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{binding}");
    assert_eq!(binding["declared_name"], "gh-token", "defaults to the name");
    assert_eq!(binding["pack_rev"], "abc123");
    let binding_id = binding["id"].as_str().expect("an id").to_string();

    // The same scope cannot bind the same declared name twice.
    let (status, _) = call(
        &rig.app,
        as_user(
            "POST",
            &format!("/api/secrets/{id}/bindings"),
            "alice",
            &["/groups/team-x"],
            Some(bind),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, body) = call(
        &rig.app,
        as_user(
            "DELETE",
            &format!("/api/secrets/{id}"),
            "alice",
            &["/groups/team-x"],
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("repo owner/repo"),
        "the refusal names what still uses it: {body}"
    );

    let (status, _) = call(
        &rig.app,
        as_user(
            "DELETE",
            &format!("/api/secrets/{id}/bindings/{binding_id}"),
            "alice",
            &["/groups/team-x"],
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = call(
        &rig.app,
        as_user(
            "DELETE",
            &format!("/api/secrets/{id}"),
            "alice",
            &["/groups/team-x"],
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(store::get(&pool, &id).await.expect("read").is_none());
    assert_eq!(
        rig.vault
            .get(
                &rig.mount,
                "crucible/registry/group:/groups/team-x/gh-token",
                "value"
            )
            .await,
        None,
        "delete destroys the bytes it owned"
    );
}

/// The launch check's read: every binding on a scope, with the secret each resolves to.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_scope_lists_its_bindings_with_their_secrets(pool: PgPool) {
    let Some(rig) = rig(&pool).await else { return };
    let (_, created) = call(
        &rig.app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &[],
            Some(register_body("gh-token", None, "hunter2")),
        ),
    )
    .await;
    let id = created["id"].as_str().expect("an id").to_string();
    let (status, _) = call(
        &rig.app,
        as_user(
            "POST",
            &format!("/api/secrets/{id}/bindings"),
            "alice",
            &[],
            Some(json!({
                "scope_kind": "playbook",
                "scope_id": "dep-sweep",
                "declared_name": "pr_token",
                "projection_kind": "env",
                "projection": "AUTORESEARCH_PR_TOKEN"
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let bound = store::bindings_for_scope(&pool, crate::secrets::ScopeKind::Playbook, "dep-sweep")
        .await
        .expect("the launch check reads this");
    assert_eq!(bound.len(), 1);
    let (binding, secret) = &bound[0];
    assert_eq!(binding.declared_name.as_str(), "pr_token");
    assert_eq!(binding.secret_id, id);
    assert_eq!(secret.id, id);
    assert_eq!(secret.owner.to_string(), "user:alice");
    assert!(
        store::bindings_for_scope(&pool, crate::secrets::ScopeKind::Repo, "dep-sweep")
            .await
            .expect("read")
            .is_empty(),
        "the scope kind is part of the key"
    );
}

/// A reference registers only when the registrant's own token can read the path, every bind
/// re-reads it with the binder's token, and the token itself is never stored.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_reference_is_verified_with_the_callers_own_token(pool: PgPool) {
    let Some(rig) = rig(&pool).await else { return };
    let other = uniq("other");
    rig.vault.ensure_kv_mount(&other).await;
    rig.vault
        .put(&other, "team/creds", "api", "elsewhere")
        .await;
    let policy = uniq("reader");
    rig.vault.write_mount_policy(&policy, &other).await;
    let reader = rig.vault.issue_token(&[&policy]).await;
    let reference = format!("vault://{other}/team/creds#api");

    // No token at all: the registry does not fall back to the hub's own reach.
    let (status, _) = call(
        &rig.app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &[],
            Some(json!({"name": "elsewhere", "kind": "opaque", "reference": reference})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // A token that cannot read the path is a refusal, not a stored pointer.
    let powerless = rig.vault.powerless_token().await;
    let (status, _) = call(
        &rig.app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &[],
            Some(json!({
                "name": "elsewhere", "kind": "opaque", "reference": reference,
                "vault_token": powerless
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(store::list(&pool, None).await.expect("list").is_empty());

    // A path inside the registry's own mount is not a reference at all.
    let (status, _) = call(
        &rig.app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &[],
            Some(json!({
                "name": "laundered", "kind": "opaque",
                "reference": format!("vault://{}/user:bob/gh-token#value", rig.mount),
                "vault_token": reader
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, body) = call(
        &rig.app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &[],
            Some(json!({
                "name": "elsewhere", "kind": "opaque", "reference": reference,
                "vault_token": reader
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["mode"], "reference");
    assert_eq!(body["vault_path"], reference);
    assert_eq!(body["current_version"], Value::Null);
    let id = body["id"].as_str().expect("an id").to_string();

    // The token reached nothing that persists: not the row, not the trail.
    let stored = store::get(&pool, &id).await.expect("read").expect("a row");
    assert!(!format!("{stored:?}").contains(&reader));
    let trail = store::audit_for_secret(&pool, &id, 100)
        .await
        .expect("read");
    assert!(!format!("{trail:?}").contains(&reader));
    assert!(!body.to_string().contains(&reader));

    // A rotation has no meaning for a path the registry does not own.
    let (status, _) = call(
        &rig.app,
        as_user(
            "POST",
            &format!("/api/secrets/{id}/rotate"),
            "alice",
            &[],
            Some(json!({"value": "nope"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Binding re-reads the path with the binder's own token.
    let bind = json!({
        "scope_kind": "domain", "scope_id": "vllm",
        "projection_kind": "env", "projection": "API_KEY"
    });
    let (status, _) = call(
        &rig.app,
        as_user(
            "POST",
            &format!("/api/secrets/{id}/bindings"),
            "alice",
            &[],
            Some(bind.clone()),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "no token, no bind"
    );

    let mut with_powerless = bind.clone();
    with_powerless["vault_token"] = json!(powerless);
    let (status, _) = call(
        &rig.app,
        as_user(
            "POST",
            &format!("/api/secrets/{id}/bindings"),
            "alice",
            &[],
            Some(with_powerless),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a binder proves their reach");

    let mut with_reader = bind;
    with_reader["vault_token"] = json!(reader);
    let (status, body) = call(
        &rig.app,
        as_user(
            "POST",
            &format!("/api/secrets/{id}/bindings"),
            "alice",
            &[],
            Some(with_reader),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(!body.to_string().contains(&reader));
}

/// Every mutation leaves a line naming who did it.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn every_mutation_writes_an_audit_row_with_the_acting_principal(pool: PgPool) {
    let Some(rig) = rig(&pool).await else { return };
    let (_, created) = call(
        &rig.app,
        as_user(
            "POST",
            "/api/secrets",
            "alice",
            &["/groups/team-x"],
            Some(register_body(
                "gh-token",
                Some("group:/groups/team-x"),
                "hunter2",
            )),
        ),
    )
    .await;
    let id = created["id"].as_str().expect("an id").to_string();
    let (_, binding) = call(
        &rig.app,
        as_user(
            "POST",
            &format!("/api/secrets/{id}/bindings"),
            "alice",
            &["/groups/team-x"],
            Some(json!({
                "scope_kind": "repo", "scope_id": "owner/repo",
                "projection_kind": "env", "projection": "TOKEN"
            })),
        ),
    )
    .await;
    let binding_id = binding["id"].as_str().expect("an id").to_string();
    call(
        &rig.app,
        as_user(
            "POST",
            &format!("/api/secrets/{id}/rotate"),
            "alice",
            &["/groups/team-x"],
            Some(json!({"value": "second"})),
        ),
    )
    .await;
    call(
        &rig.app,
        as_user(
            "DELETE",
            &format!("/api/secrets/{id}/bindings/{binding_id}"),
            "alice",
            &["/groups/team-x"],
            None,
        ),
    )
    .await;
    call(
        &rig.app,
        as_user(
            "DELETE",
            &format!("/api/secrets/{id}"),
            "alice",
            &["/groups/team-x"],
            None,
        ),
    )
    .await;

    let (status, trail) = call(
        &rig.app,
        as_user(
            "GET",
            &format!("/api/secrets/{id}/audit"),
            "alice",
            &["/groups/team-x"],
            None,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the trail outlives the row, but the route needs the row to check ownership: {trail}"
    );

    let trail = store::audit_for_secret(&pool, &id, 100)
        .await
        .expect("read");
    let actions: Vec<&str> = trail.iter().map(|a| a.action.as_str()).collect();
    assert_eq!(
        actions,
        vec!["delete", "unbind", "rotate", "bind", "register"],
        "newest first"
    );
    for row in &trail {
        assert_eq!(row.actor.as_deref(), Some("user:alice"));
        assert_eq!(row.owner, "group:/groups/team-x");
        assert_eq!(row.secret_name, "gh-token");
        assert!(
            !format!("{row:?}").contains("hunter2") && !format!("{row:?}").contains("second"),
            "no value reaches the trail: {row:?}"
        );
    }
}

/// The list route answers with what the caller owns; `?all=true` is an admin's view and is still
/// metadata.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn listing_is_scoped_to_the_callers_principals(pool: PgPool) {
    let Some(rig) = rig(&pool).await else { return };
    for (user, groups, owner, name) in [
        ("alice", &["/groups/team-x"][..], None, "alice-own"),
        (
            "alice",
            &["/groups/team-x"][..],
            Some("group:/groups/team-x"),
            "team-owned",
        ),
        ("bob", &[][..], None, "bob-own"),
    ] {
        let (status, body) = call(
            &rig.app,
            as_user(
                "POST",
                "/api/secrets",
                user,
                groups,
                Some(register_body(name, owner, "v")),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    let (status, body) = call(&rig.app, as_user("GET", "/api/secrets", "bob", &[], None)).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = body
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert_eq!(names, vec!["bob-own"]);

    let (_, body) = call(
        &rig.app,
        as_user("GET", "/api/secrets", "alice", &["/groups/team-x"], None),
    )
    .await;
    let mut names: Vec<&str> = body
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(names, vec!["alice-own", "team-owned"]);

    // `all=true` from a non-admin stays scoped.
    let (_, body) = call(
        &rig.app,
        as_user("GET", "/api/secrets?all=true", "bob", &[], None),
    )
    .await;
    assert_eq!(body.as_array().expect("a list").len(), 1);
}

// --- the shape of the surface -------------------------------------------------------------------

/// No response on this surface has anywhere to put a value: every schema a secrets route can
/// answer with is walked, and a `value`/`vault_token` property in one of them would be a leak the
/// type system already forbids — this is the assertion that keeps it that way.
#[test]
fn no_secrets_response_schema_carries_a_value() {
    let spec: Value =
        serde_json::from_str(&openapi_spec().expect("the spec renders")).expect("valid json");
    let components = &spec["components"]["schemas"];
    let paths = spec["paths"].as_object().expect("paths");
    let secret_paths: Vec<&String> = paths
        .keys()
        .filter(|p| p.starts_with("/api/secrets"))
        .collect();
    assert!(
        secret_paths.len() >= 5,
        "the registry's routes are in the document: {secret_paths:?}"
    );

    for path in secret_paths {
        for (method, op) in paths[path].as_object().expect("an operation map") {
            let responses = op["responses"].as_object().cloned().unwrap_or_default();
            for (code, response) in responses {
                let mut seen = Vec::new();
                assert_no_value(
                    &response,
                    components,
                    &mut seen,
                    &format!("{method} {path} {code}"),
                );
            }
        }
    }
}

/// Walk a schema fragment, following `$ref`s, asserting no property is a secret value.
fn assert_no_value(node: &Value, components: &Value, seen: &mut Vec<String>, where_: &str) {
    match node {
        Value::Object(map) => {
            if let Some(reference) = map.get("$ref").and_then(Value::as_str) {
                let name = reference.rsplit('/').next().unwrap_or_default().to_string();
                if seen.contains(&name) {
                    return;
                }
                seen.push(name.clone());
                assert_no_value(&components[&name], components, seen, where_);
                return;
            }
            if let Some(properties) = map.get("properties").and_then(Value::as_object) {
                for name in properties.keys() {
                    assert!(
                        !matches!(name.as_str(), "value" | "vault_token" | "secret"),
                        "{where_} can answer with a {name:?} property"
                    );
                }
            }
            for child in map.values() {
                assert_no_value(child, components, seen, where_);
            }
        }
        Value::Array(items) => {
            for child in items {
                assert_no_value(child, components, seen, where_);
            }
        }
        _ => {}
    }
}

/// The preview gate's secrets section: the declared names, and a warning where the deploy profile
/// already fills one.
#[test]
fn the_preview_gate_lists_declared_secrets_and_warns_on_profile_overlap() {
    let declared = crate::secrets::manifest::parse_declared(
        "[[secret]]\nname = \"pr_token\"\nenv = \"AUTORESEARCH_PR_TOKEN\"\n\
         [[secret]]\nname = \"registry\"\nkind = \"registry_authfile\"\npath = \"/etc/quay/push.json\"\n",
    )
    .expect("parses");
    let profile = vec!["AUTORESEARCH_PR_TOKEN".to_string()];
    let dto = PackSecretsDto::new(&declared, &profile);

    let names: Vec<&str> = dto.declared.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, vec!["pr_token", "registry"]);
    assert_eq!(
        dto.declared[1].kind,
        crate::secrets::SecretKind::RegistryAuthfile
    );
    assert_eq!(
        dto.declared[1].projection.as_deref(),
        Some("/etc/quay/push.json")
    );
    assert_eq!(dto.warnings.len(), 1, "{:?}", dto.warnings);
    assert!(
        dto.warnings[0].contains("pr_token") && dto.warnings[0].contains("secret_env"),
        "{:?}",
        dto.warnings
    );
    assert!(
        PackSecretsDto::new(&declared, &[]).warnings.is_empty(),
        "no profile, no warning"
    );
}
