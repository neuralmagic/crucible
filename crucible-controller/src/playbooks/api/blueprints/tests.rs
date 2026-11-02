//! Blueprints end to end: the real router, a real Postgres, and for references a real Vault.
//!
//! Team `mlr` publishes `mlr-pack`, which declares four secrets: one the team binds, one its
//! owner binds from a personal secret, one bound only for reviewed revisions, and one nobody
//! binds. `reed` owns the team, `kim` is a member, `mallory` holds an editor share on the pack,
//! and `zed` is nobody in particular.

use crate::api::router;
use crate::api::state::ApiState;
use crate::client::Db;
use crate::daemon::queue::{Override, OverrideSink};
use crate::secrets::store;
use crate::secrets::vault::dev::{DevVault, uniq};
use crate::testing::call;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;

struct NoSink;

impl OverrideSink for NoSink {
    fn submit(&self, _ov: Override) {}
}

fn app(pool: PgPool, vault: Option<Arc<crate::secrets::vault::VaultClient>>) -> Router {
    router(ApiState {
        roles: crate::identity::auth::Roles::new(vec!["root".into()], vec![], vec![]),
        vault,
        ..ApiState::test(Db::new(pool), Arc::new(NoSink))
    })
}

fn request(
    method: &str,
    path: &str,
    user: &str,
    act_as: Option<&str>,
    body: Option<Value>,
) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("x-auth-request-user", user);
    if let Some(team) = act_as {
        b = b.header(crate::authz::ACT_AS_HEADER, team);
    }
    match body {
        Some(body) => b
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request"),
        None => b.body(Body::empty()).expect("request"),
    }
}

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    user: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    call(app, request(method, path, user, None, body)).await
}

const WORKFLOW: &str = "params = {}\nhello = command(name = \"hello\", run = \"echo hello\")\nworkflow(type = \"playbook\", tasks = [hello], result = hello)\n";

fn manifest(secrets: &[&str]) -> String {
    let mut text = "[repo]\npath = \".\"\n\n[agent]\n\n[workflow]\ntype = \"playbook\"\nfile = \"workflow.star\"\n".to_string();
    for name in secrets {
        text.push_str(&format!(
            "\n[[secret]]\nname = \"{name}\"\nenv = \"{}\"\n",
            name.to_uppercase()
        ));
    }
    text
}

fn files(secrets: &[&str]) -> Value {
    json!({"crucible.toml": manifest(secrets), "workflow.star": WORKFLOW})
}

const DECLARED: [&str; 4] = ["gh_token", "personal_key", "pinned_key", "unbound_key"];

/// Team `mlr` with `reed` owning it and `kim` in it, and `mlr-pack` published from a team draft.
/// Returns the pack's revision.
async fn publish_pack(app: &Router, pool: &PgPool, declared: &[&str]) -> String {
    crate::authz::bootstrap::seed_playbook_publishers(pool, &["reed".to_string()])
        .await
        .expect("seed publishers");
    let (status, body) = send(
        app,
        "POST",
        "/api/teams",
        "reed",
        Some(json!({
            "slug": "mlr",
            "display_name": "MLR",
            "members": [
                {"kind": "user", "member": "reed", "role": "owner"},
                {"kind": "user", "member": "kim", "role": "member"},
            ],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = send(
        app,
        "POST",
        "/api/playbook-drafts",
        "reed",
        Some(json!({"id": "studio", "description": "mlr sweep", "owner": "team:mlr"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = send(
        app,
        "POST",
        "/api/playbook-drafts/studio/versions",
        "reed",
        Some(json!({"files": files(declared)})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(
        app,
        "POST",
        "/api/playbook-drafts/studio/publish",
        "reed",
        Some(json!({"playbook": "mlr-pack"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let pack = crate::playbooks::registry::get(pool, "mlr-pack")
        .await
        .expect("read")
        .expect("registered");
    assert_eq!(pack.owner.to_string(), "team:mlr");
    body["rev"].as_str().expect("rev").to_string()
}

/// A minted secret named `name`, owned by `user` or by the team they act as.
async fn mint(app: &Router, user: &str, act_as: Option<&str>, name: &str) -> String {
    let (status, body) = call(
        app,
        request(
            "POST",
            "/api/secrets",
            user,
            act_as,
            Some(json!({"name": name, "kind": "opaque", "mint": "github-app"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().expect("id").to_string()
}

async fn bind(
    app: &Router,
    user: &str,
    secret: &str,
    scope: &str,
    declared: &str,
    extra: Value,
) -> (StatusCode, Value) {
    let mut body = json!({
        "scope_kind": "playbook", "scope_id": scope,
        "projection_kind": "env", "projection": declared.to_uppercase(),
        "declared_name": declared,
    });
    if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
        body.extend(extra.clone());
    }
    send(
        app,
        "POST",
        &format!("/api/secrets/{secret}/bindings"),
        user,
        Some(body),
    )
    .await
}

fn by_name<'a>(list: &'a Value, name: &str) -> &'a Value {
    list.as_array()
        .expect("a list")
        .iter()
        .find(|e| e["declared_name"] == name)
        .unwrap_or_else(|| panic!("{name} in {list}"))
}

fn names(list: &Value) -> Vec<String> {
    list.as_array()
        .expect("a list")
        .iter()
        .map(|e| e["declared_name"].as_str().expect("name").to_string())
        .collect()
}

async fn catalog(app: &Router, user: &str) -> Value {
    let (status, body) = send(app, "GET", "/api/blueprints", user, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

async fn scope_bindings(pool: &PgPool, scope: &str) -> Vec<store::BindingRow> {
    store::bindings_for_scope(pool, crate::secrets::ScopeKind::Playbook, scope)
        .await
        .expect("bindings")
        .into_iter()
        .map(|(b, _)| b)
        .collect()
}

/// The whole life of one blueprint: who may mark and publish it, what the catalog shows each
/// caller, what a use carries and leaves as placeholders, and how placeholders get filled.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_teams_blueprint_carries_its_secrets_to_members_and_placeholders_to_everyone_else(
    pool: PgPool,
) {
    let app = app(pool.clone(), None);
    let rev = publish_pack(&app, &pool, &DECLARED).await;
    let gh = mint(&app, "reed", Some("team:mlr"), "gh_token").await;
    let pinned = mint(&app, "reed", Some("team:mlr"), "pinned_key").await;
    let personal = mint(&app, "reed", None, "personal_key").await;

    let (status, body) = bind(&app, "reed", &gh, "mlr-pack", "gh_token", json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body["carries"].is_null(), "not a blueprint yet: {body}");
    let gh_source = body["id"].as_str().expect("id").to_string();

    let (status, body) = send(
        &app,
        "PUT",
        "/api/playbooks/mlr-pack/blueprint/public",
        "reed",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "only a blueprint is public: {body}"
    );

    let (status, body) = send(
        &app,
        "PUT",
        "/api/playbooks/mlr-pack/shares/user:mallory",
        "reed",
        Some(json!({"role": "editor"})),
    )
    .await;
    assert!(status.is_success(), "{body}");
    let (status, body) = send(
        &app,
        "PUT",
        "/api/playbooks/mlr-pack/blueprint",
        "kim",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a member may not update: {body}"
    );
    let (status, body) = send(
        &app,
        "PUT",
        "/api/playbooks/mlr-pack/blueprint",
        "mallory",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let error = body["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("1 more") && !error.contains("gh_token"),
        "an editor may update but not bind the team's secret, which they may not read: {body}"
    );
    let (status, body) = send(
        &app,
        "PUT",
        "/api/playbooks/mlr-pack/blueprint",
        "reed",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({"id": "mlr-pack", "blueprint": true, "public": false})
    );

    let (status, body) = bind(
        &app,
        "reed",
        &pinned,
        "mlr-pack",
        "pinned_key",
        json!({"pack_rev": rev}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["carries"], false, "pinned: {body}");
    let (status, body) = bind(
        &app,
        "reed",
        &personal,
        "mlr-pack",
        "personal_key",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["carries"], false, "personal: {body}");

    let listed = catalog(&app, "kim").await;
    assert_eq!(listed.as_array().map(Vec::len), Some(1), "{listed}");
    assert_eq!(listed[0]["id"], "mlr-pack");
    assert_eq!(listed[0]["owner"], "team:mlr");
    assert_eq!(listed[0]["rev"], rev.as_str());
    assert_eq!(listed[0]["public"], false);
    let secrets = &listed[0]["secrets"];
    assert_eq!(
        names(secrets),
        DECLARED.map(String::from).to_vec(),
        "{secrets}"
    );
    assert_eq!(by_name(secrets, "gh_token")["carries"], true);
    assert!(by_name(secrets, "gh_token")["reason"].is_null());
    for (name, reason) in [
        ("personal_key", "personal_secret"),
        ("pinned_key", "pinned"),
        ("unbound_key", "unbound"),
    ] {
        assert_eq!(by_name(secrets, name)["carries"], false, "{name}");
        assert_eq!(by_name(secrets, name)["reason"], reason, "{name}");
    }
    assert_eq!(catalog(&app, "zed").await, json!([]), "private to the team");

    let (status, body) = send(
        &app,
        "PUT",
        "/api/playbooks/mlr-pack/blueprint/public",
        "kim",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a member may not share: {body}"
    );
    let (status, body) = send(
        &app,
        "PUT",
        "/api/playbooks/mlr-pack/blueprint/public",
        "reed",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["public"], true);
    let trail = crate::authz::store::audit_for(&pool, "playbook", "mlr-pack", 20)
        .await
        .expect("trail");
    assert!(
        trail.iter().any(|e| e.action == "playbook:share"
            && e.result == Some(json!({"public": true}))
            && e.prior == Some(json!({"public": false}))),
        "{trail:?}"
    );

    let listed = catalog(&app, "zed").await;
    assert_eq!(listed[0]["id"], "mlr-pack", "{listed}");
    assert_eq!(
        by_name(&listed[0]["secrets"], "gh_token")["reason"],
        "not_in_team"
    );
    let (status, body) = send(&app, "GET", "/api/playbooks/mlr-pack", "zed", None).await;
    assert_eq!(status, StatusCode::OK, "public read: {body}");
    assert!(
        body["actions"] == json!(["read"]),
        "publicity confers read alone: {body}"
    );
    let (status, body) = send(&app, "GET", "/api/playbooks/mlr-pack/shares", "zed", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "not the shares: {body}");

    let (status, used) = send(
        &app,
        "POST",
        "/api/blueprints/mlr-pack/use",
        "kim",
        Some(json!({"id": "kim-copy", "expected_rev": rev})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{used}");
    assert_eq!(used["id"], "kim-copy");
    assert_eq!(names(&used["bindings"]), vec!["gh_token"], "{used}");
    assert_eq!(used["bindings"][0]["carried_from"], "mlr-pack");
    assert_eq!(used["bindings"][0]["scope_id"], "kim-copy");
    assert_eq!(used["bindings"][0]["projection"], "GH_TOKEN");
    assert_eq!(
        names(&used["placeholders"]),
        vec!["personal_key", "pinned_key", "unbound_key"]
    );
    assert_eq!(
        by_name(&used["placeholders"], "personal_key"),
        &json!({
            "declared_name": "personal_key", "projection_kind": "env",
            "projection": "PERSONAL_KEY", "reason": "personal_secret",
        }),
        "the shape of the binding, never the secret"
    );
    let carried = scope_bindings(&pool, "kim-copy").await;
    assert_eq!(carried.len(), 1);
    assert_eq!(carried[0].secret_id, gh);
    assert_eq!(carried[0].carried_from.as_deref(), Some("mlr-pack"));
    assert_eq!(
        carried[0].carried_from_binding.as_deref(),
        Some(gh_source.as_str())
    );
    assert_eq!(carried[0].pack_rev, None);
    let draft = crate::playbooks::drafts::get(&pool, "kim-copy")
        .await
        .expect("read")
        .expect("the draft");
    assert_eq!(draft.owner.to_string(), "user:kim");
    let origin = draft.origin.expect("an origin");
    assert_eq!(origin.playbook.as_deref(), Some("mlr-pack"));
    assert_eq!(origin.rev.as_deref(), Some(rev.as_str()));
    let trail = store::audit_for_secret(&pool, &gh, 10)
        .await
        .expect("trail");
    assert!(
        trail.iter().any(|e| e.actor.as_deref() == Some("user:kim")
            && e.detail.as_deref()
                == Some("playbook kim-copy as env GH_TOKEN, carried from blueprint mlr-pack")),
        "{trail:?}"
    );

    let mine = mint(&app, "kim", None, "kim_key").await;
    let (status, body) = bind(&app, "kim", &mine, "kim-copy", "personal_key", json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body["carries"].is_null(), "a draft is no blueprint: {body}");
    let (status, detail) = send(&app, "GET", "/api/playbook-drafts/kim-copy", "kim", None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(
        names(&detail["placeholders"]),
        vec!["pinned_key", "unbound_key"],
        "a bind fills its placeholder"
    );
    let (status, body) = send(
        &app,
        "POST",
        "/api/playbook-drafts/kim-copy/versions",
        "kim",
        Some(json!({"files": files(&["gh_token", "personal_key", "pinned_key"])})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, detail) = send(&app, "GET", "/api/playbook-drafts/kim-copy", "kim", None).await;
    assert_eq!(
        names(&detail["placeholders"]),
        vec!["pinned_key"],
        "a save that stops declaring a name drops its placeholder"
    );
    let declared = crate::secrets::manifest::parse_declared(&manifest(&[
        "gh_token",
        "personal_key",
        "pinned_key",
    ]))
    .expect("declared");
    let resolve = || async {
        let kim = crate::authz::resolve::recorded_principals(&pool, Some("kim"), &[])
            .await
            .expect("principals");
        crate::secrets::launch::resolve(
            &pool,
            &crate::secrets::launch::Scope::playbook("kim-copy"),
            &declared,
            &kim,
            crate::secrets::launch::Revision::Draft,
            None,
        )
        .await
        .expect("resolve")
    };
    match resolve().await {
        Err(crate::secrets::launch::Refusal::MissingBinding { name, .. }) => {
            assert_eq!(
                name.as_str(),
                "pinned_key",
                "the launch names the placeholder"
            );
        }
        other => panic!("an unfilled placeholder refuses the launch: {other:?}"),
    }
    let also = mint(&app, "kim", None, "kim_pinned").await;
    let (status, body) = bind(&app, "kim", &also, "kim-copy", "pinned_key", json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        resolve().await.map(|mints| mints.len()).ok(),
        Some(3),
        "the carried team secret resolves for a member"
    );

    let (status, used) = send(
        &app,
        "POST",
        "/api/blueprints/mlr-pack/use",
        "zed",
        Some(json!({"id": "zed-copy"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{used}");
    assert_eq!(used["bindings"], json!([]));
    assert_eq!(
        by_name(&used["placeholders"], "gh_token")["reason"],
        "not_in_team"
    );
    assert!(scope_bindings(&pool, "zed-copy").await.is_empty());

    let (status, body) = send(
        &app,
        "POST",
        "/api/blueprints/mlr-pack/use",
        "kim",
        Some(json!({"id": "stale", "expected_rev": "tree1:moved"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = send(
        &app,
        "POST",
        "/api/blueprints/mlr-pack/use",
        "kim",
        Some(json!({"id": "zed-copy"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        crate::playbooks::drafts::get(&pool, "stale")
            .await
            .expect("read")
            .is_none()
    );
    assert!(scope_bindings(&pool, "stale").await.is_empty());
    assert!(
        scope_bindings(&pool, "zed-copy").await.is_empty(),
        "a refused use carries nothing onto a draft that already held the id"
    );

    let (status, body) = send(
        &app,
        "DELETE",
        "/api/playbooks/mlr-pack/blueprint",
        "reed",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({"id": "mlr-pack", "blueprint": false, "public": false})
    );
    assert_eq!(catalog(&app, "zed").await, json!([]));
    assert_eq!(catalog(&app, "kim").await, json!([]));
    let (status, _) = send(&app, "GET", "/api/playbooks/mlr-pack", "zed", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unmarking made it private");
    let (status, body) = send(
        &app,
        "POST",
        "/api/blueprints/mlr-pack/use",
        "kim",
        Some(json!({"id": "late"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        scope_bindings(&pool, "kim-copy").await.len(),
        3,
        "what kim carried and bound stays"
    );
}

/// The carry rule is a policy rule: a set without it leaves the team's secret as a placeholder,
/// and the catalog says so before anyone tries.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_policy_set_without_the_carry_rule_carries_nothing(pool: PgPool) {
    let app = app(pool.clone(), None);
    publish_pack(&app, &pool, &["gh_token"]).await;
    let gh = mint(&app, "reed", Some("team:mlr"), "gh_token").await;
    let (status, body) = bind(&app, "reed", &gh, "mlr-pack", "gh_token", json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = send(
        &app,
        "PUT",
        "/api/playbooks/mlr-pack/blueprint",
        "reed",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let rule = "@id(\"team-member-bind-carried\")";
    let start = crate::authz::policy::DEFAULT_POLICY
        .find(rule)
        .expect("the shipped carry rule");
    let end = start
        + crate::authz::policy::DEFAULT_POLICY[start..]
            .find("};")
            .expect("its end")
        + 2;
    let text = format!(
        "{}{}",
        &crate::authz::policy::DEFAULT_POLICY[..start],
        &crate::authz::policy::DEFAULT_POLICY[end..]
    );
    let (status, set) = send(
        &app,
        "POST",
        "/api/authz/policy-sets",
        "root",
        Some(json!({"text": text})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{set}");
    let digest = set["digest"].as_str().expect("digest");
    let (status, body) = send(
        &app,
        "POST",
        &format!("/api/authz/policy-sets/{digest}/activate"),
        "root",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let listed = catalog(&app, "kim").await;
    assert_eq!(
        by_name(&listed[0]["secrets"], "gh_token")["reason"],
        "policy_denied",
        "{listed}"
    );
    let (status, used) = send(
        &app,
        "POST",
        "/api/blueprints/mlr-pack/use",
        "kim",
        Some(json!({"id": "kim-copy"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{used}");
    assert_eq!(used["bindings"], json!([]));
    assert_eq!(
        by_name(&used["placeholders"], "gh_token")["reason"],
        "policy_denied"
    );
    let (status, used) = send(
        &app,
        "POST",
        "/api/blueprints/mlr-pack/use",
        "reed",
        Some(json!({"id": "reed-copy"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{used}");
    assert_eq!(
        names(&used["bindings"]),
        vec!["gh_token"],
        "the team's owner may bind it outright: {used}"
    );
}

/// A team secret stored by reference carries only with the user's own Vault token, the check a
/// direct bind of it makes; without one it is a placeholder.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_reference_carries_only_with_the_users_own_vault_token(pool: PgPool) {
    let Some(vault) = DevVault::start().await else {
        return;
    };
    let mount = uniq("crucible");
    let client = vault.hub_client(&mount).await;
    let app = app(pool.clone(), Some(Arc::new(client)));
    let other = uniq("other");
    vault.ensure_kv_mount(&other).await;
    vault.put(&other, "team/creds", "api", "s3cret").await;
    let policy = uniq("reader");
    vault.write_mount_policy(&policy, &other).await;
    let reader = vault.issue_token(&[&policy]).await;

    publish_pack(&app, &pool, &["ref_key"]).await;
    let (status, body) = call(
        &app,
        request(
            "POST",
            "/api/secrets",
            "reed",
            Some("team:mlr"),
            Some(json!({
                "name": "ref_key", "kind": "opaque",
                "reference": format!("vault://{other}/team/creds#api"), "vault_token": reader,
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let secret = body["id"].as_str().expect("id").to_string();
    let (status, body) = bind(
        &app,
        "reed",
        &secret,
        "mlr-pack",
        "ref_key",
        json!({"vault_token": reader}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = send(
        &app,
        "PUT",
        "/api/playbooks/mlr-pack/blueprint",
        "reed",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let listed = catalog(&app, "kim").await;
    assert_eq!(
        by_name(&listed[0]["secrets"], "ref_key")["reason"],
        "unverified"
    );
    let (status, used) = send(
        &app,
        "POST",
        "/api/blueprints/mlr-pack/use",
        "kim",
        Some(json!({"id": "no-token"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{used}");
    assert_eq!(
        by_name(&used["placeholders"], "ref_key")["reason"],
        "unverified"
    );
    let (status, used) = send(
        &app,
        "POST",
        "/api/blueprints/mlr-pack/use",
        "kim",
        Some(json!({"id": "with-token", "vault_token": reader})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{used}");
    assert_eq!(names(&used["bindings"]), vec!["ref_key"], "{used}");
    assert_eq!(used["placeholders"], json!([]));
}
