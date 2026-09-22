//! The teams routes against a real Postgres: creation, the owner invariant, nesting and cycles,
//! resolution through groups and rules, the delete guard, and the audit trail.

use super::*;
use crate::api::router;
use crate::authz::model::PLATFORM_ADMINISTRATORS;
use crate::client::Db;
use crate::daemon::queue::{Override, OverrideSink};
use crate::testing::call;
use axum::Router;
use axum::body::Body;
use axum::http::Request as HttpRequest;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;

#[derive(Default)]
struct NoSink;

impl OverrideSink for NoSink {
    fn submit(&self, _ov: Override) {}
}

fn app(pool: PgPool) -> Router {
    router(ApiState::test(Db::new(pool), Arc::new(NoSink)))
}

fn as_user(
    method: &str,
    path: &str,
    user: Option<&str>,
    groups: &[&str],
    body: Option<Value>,
) -> HttpRequest<Body> {
    let mut b = HttpRequest::builder().method(method).uri(path);
    if let Some(user) = user {
        b = b.header("x-auth-request-user", user);
    }
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

fn member(kind: &str, member: &str, role: &str) -> Value {
    json!({"kind": kind, "member": member, "role": role})
}

async fn create(
    app: &Router,
    user: &str,
    slug: &str,
    members: Option<Vec<Value>>,
) -> (StatusCode, Value) {
    let mut body = json!({"slug": slug, "display_name": slug.to_uppercase()});
    if let Some(members) = members {
        body["members"] = Value::Array(members);
    }
    call(
        app,
        as_user("POST", "/api/teams", Some(user), &[], Some(body)),
    )
    .await
}

async fn seed_admin(pool: &PgPool, login: &str) {
    crate::authz::bootstrap::seed_platform_administrators(pool, &[login.to_string()])
        .await
        .expect("seed");
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn the_creator_becomes_the_sole_owner_by_default(pool: PgPool) {
    let app = app(pool.clone());
    let (status, body) = create(&app, "Alice", "llm-d", None).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["slug"], "llm-d");
    assert_eq!(body["created_by"], "user:alice");
    assert_eq!(body["my_role"], "owner");
    assert_eq!(body["reachable"], true);
    assert_eq!(body["members"].as_array().expect("members").len(), 1);
    assert_eq!(body["members"][0]["member"], "alice");
    assert_eq!(body["members"][0]["role"], "owner");

    let (status, body) = call(
        &app,
        as_user("GET", "/api/whoami", Some("alice"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["teams"][0]["team"], "llm-d");
    assert_eq!(body["teams"][0]["role"], "owner");
    assert_eq!(body["teams"][0]["via"][0]["kind"], "direct");

    let (status, trail) = call(
        &app,
        as_user("GET", "/api/teams/llm-d/audit", Some("alice"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(trail[0]["action"], "team:create");
    assert_eq!(trail[0]["actor"], "user:alice");
    assert_eq!(trail[0]["rule"], "user-create-team");
    assert_eq!(trail[0]["auth_path"], "open");
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_team_needs_a_user_owner_and_a_unique_slug(pool: PgPool) {
    let app = app(pool.clone());
    let (status, body) = create(
        &app,
        "alice",
        "grp",
        Some(vec![member("group", "/groups/x", "owner")]),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]
            .as_str()
            .expect("error")
            .contains("owner role")
    );

    let (status, _) = create(&app, "alice", "llm-d", None).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = create(&app, "bob", "llm-d", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let (status, body) = call(
        &app,
        as_user(
            "POST",
            "/api/teams",
            Some("alice"),
            &[],
            Some(json!({"slug": "X", "display_name": "x"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let (status, body) = call(
        &app,
        as_user(
            "POST",
            "/api/teams",
            None,
            &[],
            Some(json!({"slug": "anon", "display_name": "x"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn membership_resolves_through_groups_rules_and_nesting(pool: PgPool) {
    let app = app(pool.clone());
    let (status, _) = create(
        &app,
        "alice",
        "llm-d",
        Some(vec![
            member("user", "alice", "owner"),
            member("group", "/groups/llm-d", "member"),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = create(
        &app,
        "alice",
        "platform",
        Some(vec![
            member("user", "alice", "owner"),
            member("team", "llm-d", "maintainer"),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    sqlx::query(
        "INSERT INTO users (sub, login, email, last_login, created_at, updated_at) \
         VALUES ('sub-carol', 'carol', 'carol@example.com', 'now', 'now', 'now')",
    )
    .execute(&pool)
    .await
    .expect("user");
    let (status, body) = create(
        &app,
        "alice",
        "everyone",
        Some(vec![
            member("user", "alice", "owner"),
            member("rule", "email-domain:example.com", "member"),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // bob is in llm-d by group, so in platform at min(member, maintainer) = member.
    let (status, body) = call(
        &app,
        as_user("GET", "/api/whoami", Some("bob"), &["/groups/llm-d"], None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let teams = body["teams"].as_array().expect("teams");
    assert_eq!(teams.len(), 2, "{body}");
    assert_eq!(teams[0]["team"], "llm-d");
    assert_eq!(teams[0]["role"], "member");
    assert_eq!(teams[0]["via"][0]["kind"], "group");
    assert_eq!(teams[1]["team"], "platform");
    assert_eq!(teams[1]["role"], "member");
    assert_eq!(teams[1]["via"][0]["kind"], "team");

    // carol's recorded email satisfies the rule.
    let (status, body) = call(
        &app,
        as_user("GET", "/api/whoami", Some("carol"), &["/other"], None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["teams"][0]["team"], "everyone");
    assert_eq!(body["teams"][0]["via"][0]["kind"], "rule");

    // The GET of a team reports the caller's own role.
    let (status, body) = call(
        &app,
        as_user(
            "GET",
            "/api/teams/platform",
            Some("bob"),
            &["/groups/llm-d"],
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["my_role"], "member");
    let (status, body) = call(
        &app,
        as_user("GET", "/api/teams/platform", Some("zed"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["my_role"], Value::Null);
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_membership_cycle_is_refused(pool: PgPool) {
    let app = app(pool.clone());
    for slug in ["a", "b", "c"] {
        let (status, _) = create(&app, "alice", &format!("team-{slug}"), None).await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let put = |slug: &str, members: Vec<Value>| {
        as_user(
            "PUT",
            &format!("/api/teams/{slug}/members"),
            Some("alice"),
            &[],
            Some(json!({"members": members})),
        )
    };
    let (status, body) = call(
        &app,
        put(
            "team-a",
            vec![
                member("user", "alice", "owner"),
                member("team", "team-b", "member"),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(
        &app,
        put(
            "team-b",
            vec![
                member("user", "alice", "owner"),
                member("team", "team-c", "member"),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(
        &app,
        put(
            "team-c",
            vec![
                member("user", "alice", "owner"),
                member("team", "team-a", "member"),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = call(
        &app,
        put(
            "team-c",
            vec![
                member("user", "alice", "owner"),
                member("team", "team-c", "member"),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = call(
        &app,
        put(
            "team-c",
            vec![
                member("user", "alice", "owner"),
                member("team", "missing", "member"),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    // Re-listing a team's own current members is not a cycle through itself.
    let (status, body) = call(
        &app,
        put(
            "team-a",
            vec![
                member("user", "alice", "owner"),
                member("team", "team-b", "maintainer"),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["members"][0]["role"], "maintainer");
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn only_an_owner_or_a_platform_administrator_manages_and_denials_are_audited(pool: PgPool) {
    let app = app(pool.clone());
    seed_admin(&pool, "root").await;
    let (status, _) = create(
        &app,
        "alice",
        "llm-d",
        Some(vec![
            member("user", "alice", "owner"),
            member("user", "bob", "maintainer"),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let rename = |user: &str| {
        as_user(
            "PUT",
            "/api/teams/llm-d",
            Some(user),
            &[],
            Some(json!({"display_name": "renamed"})),
        )
    };
    let (status, body) = call(&app, rename("bob")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = call(&app, rename("root")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["display_name"], "renamed");
    let (status, body) = call(&app, rename("alice")).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = call(
        &app,
        as_user("GET", "/api/teams/llm-d/audit", Some("bob"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, trail) = call(
        &app,
        as_user("GET", "/api/teams/llm-d/audit", Some("alice"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows = trail.as_array().expect("rows");
    assert_eq!(rows.len(), 4, "{trail}");
    assert_eq!(rows[0]["rule"], "team-owner-all");
    assert_eq!(rows[1]["rule"], "platform-admin-all");
    assert_eq!(rows[1]["prior"]["display_name"], "LLM-D");
    assert_eq!(rows[1]["result"]["display_name"], "renamed");
    assert_eq!(rows[2]["decision"], "deny");
    assert_eq!(rows[2]["actor"], "user:bob");
    assert_eq!(rows[2]["rule"], "no-rule");

    let (status, body) = call(
        &app,
        as_user(
            "GET",
            "/api/teams?unreachable=true",
            Some("alice"),
            &[],
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = call(
        &app,
        as_user(
            "GET",
            "/api/teams?unreachable=true",
            Some("root"),
            &[],
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.as_array().expect("teams").is_empty());
    let (status, body) = call(
        &app,
        as_user("GET", "/api/teams", Some("nobody"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let slugs: Vec<&str> = body
        .as_array()
        .expect("teams")
        .iter()
        .filter_map(|t| t["slug"].as_str())
        .collect();
    assert_eq!(slugs, vec!["llm-d", PLATFORM_ADMINISTRATORS]);
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_team_that_owns_resources_is_not_deletable(pool: PgPool) {
    let app = app(pool.clone());
    let (status, _) = create(&app, "alice", "llm-d", None).await;
    assert_eq!(status, StatusCode::CREATED);
    sqlx::query(
        "INSERT INTO secrets (id, name, owner, kind, visibility, consumer, mode, vault_path, \
                              created_at, updated_at) \
         VALUES ('s1', 'registry', 'team:llm-d', 'opaque', 'broker_only', 'run', 'managed', \
                 'p', 'now', 'now')",
    )
    .execute(&pool)
    .await
    .expect("secret");
    let (status, body) = call(
        &app,
        as_user("DELETE", "/api/teams/llm-d", Some("alice"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["blocking"][0]["resource_type"], "secret");
    assert_eq!(body["blocking"][0]["name"], "registry");
    assert_eq!(body["others"], 0);

    sqlx::query("DELETE FROM secrets")
        .execute(&pool)
        .await
        .expect("clear");
    let (status, body) = call(
        &app,
        as_user("DELETE", "/api/teams/llm-d", Some("bob"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, _) = call(
        &app,
        as_user("DELETE", "/api/teams/llm-d", Some("alice"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &app,
        as_user("GET", "/api/teams/llm-d", Some("alice"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &app,
        as_user("DELETE", "/api/teams/llm-d", Some("alice"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The trail outlives the team.
    let rows = crate::authz::store::audit_for(&pool, "team", "llm-d", 10)
        .await
        .expect("trail");
    assert_eq!(rows[0].action, "team:delete");
    assert_eq!(
        rows[0].prior.as_ref().expect("prior")["members"][0]["member"],
        "alice"
    );
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn an_unreachable_team_keeps_its_resources_and_lists_for_administrators(pool: PgPool) {
    let app = app(pool.clone());
    seed_admin(&pool, "root").await;
    let (status, _) = create(&app, "alice", "llm-d", None).await;
    assert_eq!(status, StatusCode::CREATED);
    // A team whose only member is a group nobody signed in with: the API refuses to write one,
    // so this is what a departed owner leaves behind.
    let now = crate::clock::now_rfc3339();
    let mut conn = pool.acquire().await.expect("conn");
    let hollow = crate::authz::model::TeamSlug::parse("hollow").expect("slug");
    crate::authz::store::insert_team(&mut conn, &hollow, "Hollow", None, &now)
        .await
        .expect("team");
    crate::authz::store::replace_members(
        &mut conn,
        &hollow,
        &[crate::authz::model::Member {
            member: crate::authz::model::MemberRef::Group("/nobody".into()),
            role: TeamRole::Owner,
        }],
        None,
        &now,
    )
    .await
    .expect("members");
    sqlx::query(
        "INSERT INTO secrets (id, name, owner, kind, visibility, consumer, mode, vault_path, \
                              created_at, updated_at) \
         VALUES ('s1', 'registry', 'team:hollow', 'opaque', 'broker_only', 'run', 'managed', \
                 'p', 'now', 'now')",
    )
    .execute(&pool)
    .await
    .expect("secret");

    let (status, body) = call(
        &app,
        as_user(
            "GET",
            "/api/teams?unreachable=true",
            Some("root"),
            &[],
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listed = body.as_array().expect("teams");
    assert_eq!(listed.len(), 1, "{body}");
    assert_eq!(listed[0]["slug"], "hollow");
    assert_eq!(listed[0]["reachable"], false);
    let (status, body) = call(
        &app,
        as_user("GET", "/api/teams/llm-d", Some("root"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["reachable"], true);

    // The secret stays owned by the team, and a platform administrator can still act on the team.
    let owned = crate::authz::store::owned_by(&pool, &Principal::team(&hollow))
        .await
        .expect("owned");
    assert_eq!(owned.len(), 1);
    let (status, body) = call(
        &app,
        as_user("DELETE", "/api/teams/hollow", Some("root"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["blocking"][0]["id"], "s1");
}

// --- the decision layer: bindings, platform routes, policy sets ------------------------------

fn app_with_roles(pool: PgPool, admins: &[&str], operators: &[&str]) -> Router {
    router(ApiState {
        roles: crate::identity::auth::Roles::new(
            admins.iter().map(|s| s.to_string()).collect(),
            operators.iter().map(|s| s.to_string()).collect(),
            vec![],
        ),
        ..ApiState::test(Db::new(pool), Arc::new(NoSink))
    })
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_platform_mutation_is_decided_before_the_handler_and_a_denial_is_audited(pool: PgPool) {
    let app = app_with_roles(pool.clone(), &["root"], &["op"]);
    // A viewer: refused by the decision with the structured reason, and the refusal is audited.
    let (status, body) = call(
        &app,
        as_user("POST", "/api/reconcile", Some("zed"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["rule"], "no-rule");
    assert!(
        body["error"]
            .as_str()
            .expect("error")
            .contains("user:zed may not platform:update"),
        "{body}"
    );
    let trail = crate::authz::store::audit_for(&pool, "platform", "/api/reconcile", 10)
        .await
        .expect("trail");
    assert_eq!(trail.len(), 1);
    assert_eq!(trail[0].actor, "user:zed");
    assert_eq!(trail[0].decision, "deny");
    assert_eq!(trail[0].action, "platform:update");
    assert_eq!(trail[0].auth_path, "open");

    // An anonymous caller has an empty subject set.
    let (status, body) = call(&app, as_user("POST", "/api/reconcile", None, &[], None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["rule"], "no-rule");

    // A configured operator holds issue:update on the platform, not platform:update.
    let (status, body) = call(
        &app,
        as_user("POST", "/api/reconcile", Some("op"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = call(
        &app,
        as_user(
            "POST",
            "/api/issues/org%2Frepo%231/park",
            Some("op"),
            &[],
            Some(json!({"reason": "x"})),
        ),
    )
    .await;
    assert_ne!(status, StatusCode::FORBIDDEN, "{body}");

    // A configured administrator passes the decision; the handler then runs.
    let (status, _) = call(
        &app,
        as_user("POST", "/api/reconcile", Some("root"), &[], None),
    )
    .await;
    assert_ne!(status, StatusCode::FORBIDDEN);
    // A read is not decided yet, so no audit row was added by the reads above.
    let (status, _) = call(&app, as_user("GET", "/api/whoami", Some("zed"), &[], None)).await;
    assert_eq!(status, StatusCode::OK);
    let trail = crate::authz::store::audit_for(&pool, "platform", "/api/reconcile", 10)
        .await
        .expect("trail");
    assert_eq!(trail.len(), 3, "one row per denied mutation");
}

/// Team membership alone reaches the decision but not the guard it still sits beside: both must
/// allow during the migration (RFC-0003 C-COMPATIBILITY).
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_team_administrator_passes_the_decision_but_not_the_legacy_guard(pool: PgPool) {
    let app = app_with_roles(pool.clone(), &["root"], &[]);
    seed_admin(&pool, "root").await;
    let (status, body) = call(
        &app,
        as_user(
            "PUT",
            &format!("/api/teams/{PLATFORM_ADMINISTRATORS}/members"),
            Some("root"),
            &[],
            Some(json!({"members": [member("user", "root", "owner"), member("user", "alice", "owner")]})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(
        &app,
        as_user("GET", "/api/whoami", Some("alice"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["teams"][0]["team"], PLATFORM_ADMINISTRATORS);
    assert_eq!(body["teams"][0]["role"], "owner");
    assert_eq!(
        body["role"], "viewer",
        "the legacy role ladder does not read the team"
    );
    let (status, body) = call(
        &app,
        as_user("POST", "/api/reconcile", Some("alice"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body["error"].as_str().expect("error").contains("whitelist"),
        "the guard refused, not the decision: {body}"
    );
    assert!(
        crate::authz::store::audit_for(&pool, "platform", "/api/reconcile", 10)
            .await
            .expect("trail")
            .is_empty(),
        "the decision allowed, so nothing was audited"
    );
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn the_vocabulary_and_schema_are_served(pool: PgPool) {
    let app = app(pool.clone());
    let (status, body) = call(
        &app,
        as_user("GET", "/api/authz/actions", Some("zed"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let actions = body.as_array().expect("actions");
    assert_eq!(actions.len(), crate::authz::action::Action::all().len());
    assert!(
        actions
            .iter()
            .any(|a| a["action"] == "team:manage-members" && a["verb"] == "manage-members")
    );
    let res = app
        .clone()
        .oneshot(as_user("GET", "/api/authz/schema", Some("zed"), &[], None))
        .await
        .expect("infallible");
    assert_eq!(res.status(), StatusCode::OK);
    let text = String::from_utf8(
        axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body")
            .to_vec(),
    )
    .expect("utf8");
    assert!(text.contains("action \"team:manage-members\""), "{text}");
    assert!(text.contains("entity UserPrincipal"), "{text}");
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_policy_set_is_validated_stored_and_activated_with_an_audit_row(pool: PgPool) {
    let app = app_with_roles(pool.clone(), &["root"], &[]);
    let active = crate::authz::policy::ActivePolicy::default_set().expect("default");
    crate::authz::bootstrap::load_active_policy(&pool, &active)
        .await
        .expect("load");
    let default_digest = crate::authz::policy::digest(crate::authz::policy::DEFAULT_POLICY);

    let (status, body) = call(
        &app,
        as_user("GET", "/api/authz/policy-sets", Some("zed"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["digest"], default_digest);
    assert_eq!(body[0]["active"], true);

    let post = |user: &str, text: &str| {
        as_user(
            "POST",
            "/api/authz/policy-sets",
            Some(user),
            &[],
            Some(json!({"text": text})),
        )
    };
    let (status, body) = call(
        &app,
        post("zed", "@id(\"x\") permit(principal, action, resource);"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = call(&app, post("root", "permit(principal, action, resource);")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"].as_str().expect("error").contains("@id"),
        "{body}"
    );
    let (status, body) = call(
        &app,
        post(
            "root",
            "@id(\"only-read\") permit(principal, action in [Action::\"read\"], resource);",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"].as_str().expect("error").contains("activate"),
        "{body}"
    );

    let widened = format!(
        "{}\n@id(\"zed-updates\") permit(principal, action == Action::\"platform:update\", resource) when {{ principal.hasTag(\"user:zed\") }};",
        crate::authz::policy::DEFAULT_POLICY
    );
    let (status, body) = call(&app, post("root", &widened)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let digest = body["digest"].as_str().expect("digest").to_string();
    assert_eq!(body["active"], false);
    assert_eq!(body["created_by"], "user:root");

    let (status, body) = call(
        &app,
        as_user(
            "GET",
            &format!("/api/authz/policy-sets/{digest}"),
            Some("zed"),
            &[],
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["text"], widened);

    let activate = |user: &str, d: &str| {
        as_user(
            "POST",
            &format!("/api/authz/policy-sets/{d}/activate"),
            Some(user),
            &[],
            None,
        )
    };
    let (status, body) = call(&app, activate("zed", &digest)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = call(&app, activate("root", "nope")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = call(&app, activate("root", &digest)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["prior"], default_digest);
    assert_eq!(body["active"], digest);

    let trail = crate::authz::store::audit_for(&pool, "policy_set", &digest, 10)
        .await
        .expect("trail");
    assert_eq!(trail.len(), 1);
    assert_eq!(trail[0].action, "policy_set:activate");
    assert_eq!(trail[0].actor, "user:root");
    assert!(
        trail[0].rule.contains("platform-admin-all"),
        "{}",
        trail[0].rule
    );
    assert_eq!(
        trail[0].prior.as_ref().expect("prior")["digest"],
        default_digest
    );
    assert_eq!(trail[0].result.as_ref().expect("result")["digest"], digest);

    // The new set decides from now on: zed's platform:update passes the decision, and only the
    // legacy guard still refuses (both must allow).
    let (status, body) = call(
        &app,
        as_user("POST", "/api/reconcile", Some("zed"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body["error"].as_str().expect("error").contains("whitelist"),
        "{body}"
    );
    let (status, body) = call(
        &app,
        as_user("GET", "/api/authz/policy-sets", Some("zed"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["digest"], digest);
    assert_eq!(body[0]["active"], true);
    assert_eq!(body[1]["active"], false);
}

// --- ownership: owner on create, transfer, attribution -----------------------------------------

async fn team_with(app: &Router, creator: &str, slug: &str, members: Vec<Value>) {
    let (status, body) = create(app, creator, slug, Some(members)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_draft_is_owned_by_its_creator_or_a_team_they_act_in(pool: PgPool) {
    let app = app_with_roles(pool.clone(), &[], &["alice", "bob"]);
    team_with(
        &app,
        "alice",
        "llm-d",
        vec![
            member("user", "alice", "owner"),
            member("user", "bob", "member"),
        ],
    )
    .await;
    let draft = |user: &str, id: &str, owner: Option<&str>| {
        let mut body = json!({"id": id, "description": "d"});
        if let Some(owner) = owner {
            body["owner"] = json!(owner);
        }
        as_user("POST", "/api/playbook-drafts", Some(user), &[], Some(body))
    };
    let (status, body) = call(&app, draft("alice", "mine", None)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = call(&app, draft("alice", "ours", Some("team:llm-d"))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // bob is only a member: create on a team-owned resource needs maintainer.
    let (status, body) = call(&app, draft("bob", "not-yet", Some("team:llm-d"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["rule"], "no-rule");
    let (status, body) = call(&app, draft("alice", "theirs", Some("team:other"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body["error"].as_str().expect("error").contains("act as"),
        "{body}"
    );
    let (status, body) = call(&app, draft("alice", "bad", Some("nope"))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let (status, body) = call(
        &app,
        as_user("GET", "/api/playbook-drafts", Some("alice"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let owners: Vec<(String, String)> = body
        .as_array()
        .expect("drafts")
        .iter()
        .map(|d| {
            (
                d["id"].as_str().expect("id").to_string(),
                d["owner"].as_str().expect("owner").to_string(),
            )
        })
        .collect();
    assert!(
        owners.contains(&("mine".to_string(), "user:alice".to_string())),
        "{owners:?}"
    );
    assert!(
        owners.contains(&("ours".to_string(), "team:llm-d".to_string())),
        "{owners:?}"
    );
    assert!(
        !owners
            .iter()
            .any(|(id, _)| id == "not-yet" || id == "theirs"),
        "{owners:?}"
    );
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_transfer_needs_the_transfer_action_and_an_acted_as_or_administrator_target(
    pool: PgPool,
) {
    let app = app_with_roles(pool.clone(), &["root"], &["alice", "bob"]);
    team_with(
        &app,
        "alice",
        "llm-d",
        vec![member("user", "alice", "owner")],
    )
    .await;
    let (status, body) = call(
        &app,
        as_user(
            "POST",
            "/api/playbook-drafts",
            Some("alice"),
            &[],
            Some(json!({"id": "d1", "description": "d"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let transfer = |user: &str, owner: &str| {
        as_user(
            "PUT",
            "/api/playbook-drafts/d1/owner",
            Some(user),
            &[],
            Some(json!({"owner": owner})),
        )
    };
    // bob holds nothing on alice's draft.
    let (status, body) = call(&app, transfer("bob", "user:bob")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["rule"], "no-rule");
    // alice may transfer, but only to a principal she acts as.
    let (status, body) = call(&app, transfer("alice", "user:bob")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body["error"].as_str().expect("error").contains("act as"),
        "{body}"
    );
    let (status, body) = call(&app, transfer("alice", "user:alice")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = call(&app, transfer("alice", "group:/x")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = call(&app, transfer("alice", "team:llm-d")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["prior"], "user:alice");
    assert_eq!(body["owner"], "team:llm-d");
    // A platform administrator moves it anywhere.
    let (status, body) = call(&app, transfer("root", "user:bob")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(
        &app,
        as_user("GET", "/api/playbook-drafts/d1", Some("bob"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["owner"], "user:bob");
    let (status, body) = call(&app, transfer("root", "user:root")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = call(&app, transfer("root", "user:bob")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(
        &app,
        as_user(
            "PUT",
            "/api/playbook-drafts/nope/owner",
            Some("root"),
            &[],
            Some(json!({"owner": "user:bob"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let trail = crate::authz::store::audit_for(&pool, "playbook_draft", "d1", 10)
        .await
        .expect("trail");
    assert_eq!(trail.len(), 5, "{trail:?}");
    assert_eq!(trail[4].action, "playbook_draft:transfer");
    assert_eq!(trail[4].actor, "user:bob");
    assert_eq!(trail[4].decision, "deny");
    assert_eq!(trail[3].actor, "user:alice");
    assert_eq!(
        trail[3].prior.as_ref().expect("prior")["owner"],
        "user:alice"
    );
    assert_eq!(
        trail[3].result.as_ref().expect("result")["owner"],
        "team:llm-d"
    );
    assert!(
        trail[2].rule.contains("platform-admin-all"),
        "{}",
        trail[2].rule
    );
}

#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_run_is_attributed_to_its_launch_owner_at_spend_time(pool: PgPool) {
    let now = crate::clock::now_rfc3339();
    sqlx::query(
        "INSERT INTO playbooks (id, description, repo, rev, path, tar_gz, tar_digest, tar_bytes, \
                                params_schema, schema_digest, core_rev, created_at, updated_at, owner) \
         VALUES ('pb', 'd', 'o/r', 'abc', '', '\\x00', 'sha256:1', 1, '{}'::jsonb, 'sha256:2', \
                 'pin', $1, $1, 'team:llm-d')",
    )
    .bind(&now)
    .execute(&pool)
    .await
    .expect("playbook");
    sqlx::query(
        "INSERT INTO issues (key, repo, tier, status, priority, input_kind, title, updated_at) \
         VALUES ('pb#1', 'o/r', 0, 'new', 0, 'playbook', 't', $1)",
    )
    .bind(&now)
    .execute(&pool)
    .await
    .expect("issue");
    sqlx::query(
        "INSERT INTO playbook_launches (key, playbook, params, schema_digest, max_cost, max_time, \
                                        created_by, created_at) \
         VALUES ('pb#1', 'pb', '{}'::jsonb, 'sha256:2', 1.0, '1h', 'Alice', $1)",
    )
    .bind(&now)
    .execute(&pool)
    .await
    .expect("launch");
    sqlx::query("INSERT INTO runs (run_id, issue, status) VALUES ('r1', 'pb#1', 'running'), ('r2', NULL, 'running')")
        .execute(&pool)
        .await
        .expect("runs");
    crate::runs::store::attribute_run(&pool, "r1")
        .await
        .expect("attribute");
    crate::runs::store::attribute_run(&pool, "r2")
        .await
        .expect("attribute");
    let attributed: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT run_id, attributed_to FROM runs ORDER BY run_id")
            .fetch_all(&pool)
            .await
            .expect("rows");
    assert_eq!(
        attributed,
        vec![
            ("r1".into(), Some("user:alice".into())),
            ("r2".into(), Some("team:platform-administrators".into())),
        ]
    );
    // A later transfer of the playbook does not rewrite the attribution.
    sqlx::query("UPDATE playbooks SET owner = 'user:bob' WHERE id = 'pb'")
        .execute(&pool)
        .await
        .expect("transfer");
    let still: Option<String> =
        sqlx::query_scalar("SELECT attributed_to FROM runs WHERE run_id = 'r1'")
            .fetch_one(&pool)
            .await
            .expect("row");
    assert_eq!(still.as_deref(), Some("user:alice"));
}

/// C-READ-SCOPING on drafts: a list holds only what the caller may read, a get on the rest is
/// not-found, and a mutation on the rest is not-found too but still audited under its own verb.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn draft_reads_and_writes_are_scoped_to_what_the_caller_may_read(pool: PgPool) {
    let app = app_with_roles(pool.clone(), &[], &["alice", "bob", "carol"]);
    team_with(
        &app,
        "alice",
        "llm-d",
        vec![
            member("user", "alice", "owner"),
            member("user", "bob", "member"),
        ],
    )
    .await;
    for (id, owner) in [("mine", None), ("ours", Some("team:llm-d"))] {
        let mut body = json!({"id": id, "description": "d"});
        if let Some(owner) = owner {
            body["owner"] = json!(owner);
        }
        let (status, body) = call(
            &app,
            as_user(
                "POST",
                "/api/playbook-drafts",
                Some("alice"),
                &[],
                Some(body),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let ids = |body: &Value| -> Vec<String> {
        let mut ids: Vec<String> = body
            .as_array()
            .expect("drafts")
            .iter()
            .map(|d| d["id"].as_str().expect("id").to_string())
            .collect();
        ids.sort();
        ids
    };
    let list = |user: Option<&str>| as_user("GET", "/api/playbook-drafts", user, &[], None);

    let (status, body) = call(&app, list(Some("alice"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), vec!["mine", "ours"]);
    let (status, body) = call(&app, list(Some("bob"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), vec!["ours"]);
    let (status, body) = call(&app, list(Some("carol"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), Vec::<String>::new());
    let (status, body) = call(&app, list(None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), Vec::<String>::new(), "anonymous reads nothing");

    for path in [
        "/api/playbook-drafts/mine",
        "/api/playbook-drafts/mine/files",
        "/api/playbook-drafts/mine/preview",
        "/api/playbook-drafts/mine/tarball",
        "/api/playbook-drafts/mine/origin/files",
    ] {
        let (status, body) = call(&app, as_user("GET", path, Some("bob"), &[], None)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
    }
    let (status, body) = call(
        &app,
        as_user("GET", "/api/playbook-drafts/ours", Some("bob"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["owner"], "team:llm-d");

    // A save on a draft bob may read but not update is the structured denial; on one he may not
    // read it is not-found. Both write an audit row under `playbook_draft:update`.
    let save = |user: &str, id: &str| {
        as_user(
            "POST",
            &format!("/api/playbook-drafts/{id}/versions"),
            Some(user),
            &[],
            Some(json!({"files": {"workflow.star": "def main(): pass"}})),
        )
    };
    let (status, body) = call(&app, save("bob", "ours")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["rule"], "no-rule");
    assert!(
        body["error"]
            .as_str()
            .expect("error")
            .contains("playbook_draft:update"),
        "{body}"
    );
    let (status, body) = call(&app, save("bob", "mine")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = call(&app, save("carol", "ours")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let denials: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT actor, action, resource_id FROM authz_audit
          WHERE decision = 'deny' AND resource_type = 'playbook_draft' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("audit");
    assert_eq!(
        denials,
        vec![
            (
                "user:bob".into(),
                "playbook_draft:update".into(),
                "ours".into()
            ),
            (
                "user:bob".into(),
                "playbook_draft:update".into(),
                "mine".into()
            ),
            (
                "user:carol".into(),
                "playbook_draft:update".into(),
                "ours".into()
            ),
        ]
    );
}

/// C-SHARING on drafts: a share puts the resource in the grantee's lists and reads at its role and
/// nothing more, an expired share allows nothing while still listing, a transfer keeps the shares,
/// revocation takes `share` or platform administration, and every change is audited.
#[sqlx::test(migrator = "crate::MIGRATOR")]
async fn a_share_confers_its_role_until_it_expires_and_survives_transfer(pool: PgPool) {
    let app = app_with_roles(pool.clone(), &["root"], &["alice", "bob", "carol"]);
    team_with(
        &app,
        "alice",
        "llm-d",
        vec![member("user", "alice", "owner")],
    )
    .await;
    let (status, body) = call(
        &app,
        as_user(
            "POST",
            "/api/playbook-drafts",
            Some("alice"),
            &[],
            Some(json!({"id": "mine", "description": "d"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let get = |user: &str| as_user("GET", "/api/playbook-drafts/mine", Some(user), &[], None);
    let save = |user: &str| {
        as_user(
            "POST",
            "/api/playbook-drafts/mine/versions",
            Some(user),
            &[],
            Some(json!({"files": {"workflow.star": "def main(): pass"}})),
        )
    };
    let share = |user: &str, grantee: &str, body: Value| {
        as_user(
            "PUT",
            &format!("/api/playbook-drafts/mine/shares/{grantee}"),
            Some(user),
            &[],
            Some(body),
        )
    };

    let (status, _) = call(&app, get("bob")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A viewer reads and lists, and nothing else.
    let (status, body) = call(&app, share("alice", "user:bob", json!({"role": "viewer"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["role"], "viewer");
    assert_eq!(body["expired"], false);
    let (status, body) = call(&app, get("bob")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(
        &app,
        as_user("GET", "/api/playbook-drafts", Some("bob"), &[], None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["id"], "mine");
    let (status, body) = call(&app, save("bob")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // A share never confers `share`: the grantee may read, so the refusal is structured.
    let (status, body) = call(&app, share("bob", "user:carol", json!({"role": "viewer"}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["rule"], "no-rule");
    // The shares are part of the read: the grantee sees them too.
    let (status, body) = call(
        &app,
        as_user(
            "GET",
            "/api/playbook-drafts/mine/shares",
            Some("bob"),
            &[],
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().map(Vec::len), Some(1));
    let (status, _) = call(
        &app,
        as_user(
            "GET",
            "/api/playbook-drafts/mine/shares",
            Some("carol"),
            &[],
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Raised to editor, a save goes through.
    let (status, body) = call(&app, share("alice", "user:bob", json!({"role": "editor"}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(&app, save("bob")).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Malformed grants.
    let (status, _) = call(&app, share("alice", "run:r1", json!({"role": "viewer"}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = call(
        &app,
        share("alice", "user:alice", json!({"role": "viewer"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = call(
        &app,
        share(
            "alice",
            "user:carol",
            json!({"role": "viewer", "not_after": "tomorrow"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = call(&app, share("alice", "user:carol", json!({"role": "owner"}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // An expired share lists with its expiry and allows nothing.
    let (status, body) = call(
        &app,
        share(
            "alice",
            "user:carol",
            json!({"role": "viewer", "not_after": "2020-01-01T00:00:00Z"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["expired"], true);
    let (status, _) = call(&app, get("carol")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = call(
        &app,
        as_user(
            "GET",
            "/api/playbook-drafts/mine/shares",
            Some("alice"),
            &[],
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let listed: Vec<(String, bool)> = body
        .as_array()
        .expect("shares")
        .iter()
        .map(|s| {
            (
                s["grantee"].as_str().expect("grantee").to_string(),
                s["expired"].as_bool().expect("expired"),
            )
        })
        .collect();
    assert_eq!(
        listed,
        vec![("user:bob".into(), false), ("user:carol".into(), true)]
    );

    // Transfer keeps the shares.
    let (status, body) = call(
        &app,
        as_user(
            "PUT",
            "/api/playbook-drafts/mine/owner",
            Some("alice"),
            &[],
            Some(json!({"owner": "team:llm-d"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = call(&app, get("bob")).await;
    assert_eq!(status, StatusCode::OK);

    // Revocation: not by a grantee, by the owner and by a platform administrator.
    let revoke = |user: &str, grantee: &str| {
        as_user(
            "DELETE",
            &format!("/api/playbook-drafts/mine/shares/{grantee}"),
            Some(user),
            &[],
            None,
        )
    };
    let (status, _) = call(&app, revoke("bob", "user:carol")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&app, revoke("root", "user:carol")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&app, revoke("alice", "user:bob")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&app, revoke("alice", "user:bob")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&app, get("bob")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let trail: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT actor, decision, COALESCE(result->>'grantee', prior->>'grantee', '') \
           FROM authz_audit WHERE action = 'playbook_draft:share' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("audit");
    assert_eq!(
        trail,
        vec![
            ("user:alice".into(), "allow".into(), "user:bob".into()),
            ("user:bob".into(), "deny".into(), String::new()),
            ("user:alice".into(), "allow".into(), "user:bob".into()),
            ("user:alice".into(), "allow".into(), "user:carol".into()),
            ("user:bob".into(), "deny".into(), String::new()),
            ("user:root".into(), "allow".into(), "user:carol".into()),
            ("user:alice".into(), "allow".into(), "user:bob".into()),
        ]
    );
}
