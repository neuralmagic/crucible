//! The relying party, end to end against a real Keycloak.
//!
//! No mocks and no stub issuer. Every test here drives the actual browser flow (authorization code
//! with PKCE, the rendered login form, the callback) or presents an access token Keycloak actually
//! minted, against the real router [`crate::human_router`] mounts.
//!
//! It lives inside the crate rather than in `tests/` because the guard it exercises
//! ([`crate::identity::auth::require_auth`]) is crate-private on purpose: the point of these tests is the
//! whole mounted stack, not a public client library.
//!
//! The server comes from `KEYCLOAK_URL`, else the shared dev container `just dev-keycloak`
//! provisions on 58180, which this file will start if docker is available. With neither, the tests
//! print why they are skipping — unless `CRUCIBLE_REQUIRE_KEYCLOAK_TESTS` is set, which turns a
//! skip into a failure so CI can never go green by silently skipping the suite.

use crate::api::state::ApiState;
use crate::client::Db;
use crate::daemon::queue::{Override, OverrideSink};
use crate::identity::auth::AuthMode;
use crate::identity::oidc::{OidcCfg, OidcProvider};
use crate::identity::session::LoginFlow;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sqlx::PgPool;
use std::sync::Arc;
use tower::util::ServiceExt;

/// The realm, clients, and user that `tests/keycloak/crucible-realm.json` imports.
const REALM: &str = "crucible";
const RP_CLIENT: &str = "crucible-controller";
/// The same registration with its group mapper on the access token only, the RH SSO shape.
const ROLES_CLIENT: &str = "crucible-controller-roles";
const RP_SECRET: &str = "controller-client-secret";
const DEVICE_CLIENT: &str = "crucible-cli";
const BRIEF_CLIENT: &str = "crucible-cli-brief";
const OTHER_CLIENT: &str = "other-app";
const USER: &str = "alice";
const PASSWORD: &str = "alice-password";
const USER_GROUP: &str = "/platform-devs";
/// The container `just dev-keycloak` provisions, and its port.
const CONTAINER: &str = "crucible-test-keycloak";
const DEFAULT_URL: &str = "http://127.0.0.1:58180";
/// The redirect URI the flow registers. The realm allows `http://localhost:*`, and nothing ever
/// connects to this port — the test IS the browser, so it follows the redirect itself.
const REDIRECT_URI: &str = "http://localhost:19999/auth/callback";

/// Set when the caller insists the suite must run (CI). A skip then panics.
fn required() -> bool {
    std::env::var("CRUCIBLE_REQUIRE_KEYCLOAK_TESTS")
        .map(|v| !v.trim().is_empty() && v != "0")
        .unwrap_or(false)
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("http client")
}

async fn ready(base: &str) -> bool {
    http()
        .get(format!(
            "{base}/realms/{REALM}/.well-known/openid-configuration"
        ))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

/// Start (or restart) the shared dev container, the same one `just dev-keycloak` provisions, and
/// wait for the realm to answer. Serialized so a whole suite's worth of tests cannot race a dozen
/// `docker run`s against one name.
static START: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn start_container() -> bool {
    let _serialize = START.lock().await;
    let realm_dir = format!("{}/tests/keycloak", env!("CARGO_MANIFEST_DIR"));
    let exists = tokio::process::Command::new("docker")
        .args(["inspect", CONTAINER])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success());
    let started = if exists {
        tokio::process::Command::new("docker")
            .args(["start", CONTAINER])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .is_ok_and(|s| s.success())
    } else {
        tokio::process::Command::new("docker")
            .args([
                "run",
                "-d",
                "--name",
                CONTAINER,
                "-e",
                "KC_BOOTSTRAP_ADMIN_USERNAME=admin",
                "-e",
                "KC_BOOTSTRAP_ADMIN_PASSWORD=admin",
                "-v",
                &format!("{realm_dir}:/opt/keycloak/data/import:ro"),
                "-p",
                "58180:8080",
                "quay.io/keycloak/keycloak:26.4",
                "start-dev",
                "--import-realm",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .is_ok_and(|s| s.success())
    };
    if !started {
        return false;
    }
    for _ in 0..120 {
        if ready(DEFAULT_URL).await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    false
}

/// The issuer's base URL, or `None` when there is none and the suite may skip.
async fn keycloak() -> Option<String> {
    static BASE: tokio::sync::OnceCell<Option<String>> = tokio::sync::OnceCell::const_new();
    BASE.get_or_init(|| async {
        let configured = std::env::var("KEYCLOAK_URL")
            .ok()
            .map(|v| v.trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty());
        let base = configured
            .clone()
            .unwrap_or_else(|| DEFAULT_URL.to_string());
        if ready(&base).await {
            return Some(base);
        }
        // Only the default endpoint is ours to provision; a configured URL is somebody else's.
        if configured.is_none() && start_container().await {
            return Some(DEFAULT_URL.to_string());
        }
        if required() {
            panic!("CRUCIBLE_REQUIRE_KEYCLOAK_TESTS is set but no keycloak answered at {base}");
        }
        eprintln!("no keycloak at {base}; skipping the oidc e2e suite (run `just dev-keycloak`)");
        None
    })
    .await
    .clone()
}

fn issuer(base: &str) -> String {
    format!("{base}/realms/{REALM}")
}

fn cfg(base: &str, device_client: &str) -> OidcCfg {
    cfg_for(base, RP_CLIENT, device_client)
}

fn cfg_for(base: &str, client_id: &str, device_client: &str) -> OidcCfg {
    OidcCfg {
        issuer: issuer(base),
        client_id: client_id.to_string(),
        client_secret: Some(RP_SECRET.to_string()),
        redirect_url: REDIRECT_URI.to_string(),
        scopes: vec![
            "openid".to_string(),
            "email".to_string(),
            "profile".to_string(),
        ],
        device_client_id: Some(device_client.to_string()),
        post_logout_redirect: None,
    }
}

/// The same registration asking for `offline_access`, so the token response carries the offline
/// refresh token the credential is built from.
fn cfg_offline(base: &str) -> OidcCfg {
    let mut cfg = cfg(base, DEVICE_CLIENT);
    cfg.scopes.push("offline_access".to_string());
    cfg
}

struct NoopSink;

impl OverrideSink for NoopSink {
    fn submit(&self, _ov: Override) {}
}

/// The human surface exactly as `serve` mounts it, in native mode against this issuer, with no
/// offline credential: the shape a realm that grants no `offline_access` runs in.
async fn native_router(pool: &PgPool, provider: Arc<OidcProvider>) -> axum::Router {
    native_router_with(pool, provider, None, DEFAULT_REFRESH_AFTER).await
}

/// A credential key for the tests. Fixed material, because the point is the round trip through the
/// row, not the key.
fn test_keys() -> Arc<crate::identity::oidc::credentials::CredentialKeys> {
    Arc::new(
        crate::identity::oidc::credentials::CredentialKeys::new(vec![vec![5u8; 32]]).expect("keys"),
    )
}

/// Long enough that a session's groups are not re-read mid-test unless a test asks for it.
const DEFAULT_REFRESH_AFTER: std::time::Duration = std::time::Duration::from_secs(3600);

/// The same, with the offline credential mounted, so the callback stores one and a live session
/// re-reads its groups through it.
async fn native_router_with(
    pool: &PgPool,
    provider: Arc<OidcProvider>,
    keys: Option<Arc<crate::identity::oidc::credentials::CredentialKeys>>,
    refresh_after: std::time::Duration,
) -> axum::Router {
    let store = crate::identity::session::store(pool)
        .await
        .expect("session store");
    let guard = {
        let mut guard = crate::identity::auth::BearerGuard::new(
            crate::identity::auth::SharedToken::new("s3cr3t"),
            None,
            None,
            None,
            AuthMode::Native,
        )
        .expect("guard builds");
        guard.oidc = Some(provider.clone());
        guard.users = Some(pool.clone());
        guard.refresh = crate::identity::oidc::credentials::OwnerRefresh::from_parts(
            pool.clone(),
            Some(provider.clone()),
            keys.clone(),
        );
        guard.refresh_after = refresh_after;
        Arc::new(guard)
    };
    let state = ApiState {
        roles: crate::identity::auth::Roles::new(vec![], vec![], vec!["platform-devs".to_string()]),
        auth_mode: AuthMode::Native,
        ..ApiState::test(Db::new(pool.clone()), Arc::new(NoopSink))
    }
    .with_oidc(Some(provider.clone()), keys.clone());
    crate::human_router(
        state,
        store,
        false,
        crate::HumanAuth {
            guard,
            routes: crate::identity::oidc::routes::AuthState {
                mode: AuthMode::Native,
                oidc: Some(provider),
                pool: pool.clone(),
                credential_keys: keys,
                proxy_prefix: "/oauth2".to_string(),
            },
        },
        crate::spa::Source::Embedded,
    )
}

// --- driving the router like a browser -----------------------------------------------------------

struct Reply {
    status: StatusCode,
    location: Option<String>,
    cookie: Option<String>,
    body: String,
}

async fn call(app: &axum::Router, req: Request<Body>) -> Reply {
    let res = app.clone().oneshot(req).await.expect("infallible");
    let status = res.status();
    let location = res
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let cookie = res
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or_default().to_string());
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    Reply {
        status,
        location,
        cookie,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

fn get(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut b = Request::get(path);
    if let Some(cookie) = cookie {
        b = b.header(header::COOKIE, cookie);
    }
    b.body(Body::empty()).expect("request")
}

// --- driving keycloak's login form ---------------------------------------------------------------

/// Collect every `Set-Cookie` on a response into one `Cookie` header value. Keycloak's login form
/// needs the auth-session cookies it set on the page it rendered.
fn jar(res: &reqwest::Response) -> String {
    res.headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .filter(|v| v.contains('=') && !v.ends_with('='))
        .collect::<Vec<_>>()
        .join("; ")
}

/// `application/x-www-form-urlencoded` by hand: reqwest is built here without its `urlencoded`
/// feature, and turning a production dependency's feature on for a test is the wrong trade.
fn form_body(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                percent_encoding::utf8_percent_encode(k, percent_encoding::NON_ALPHANUMERIC),
                percent_encoding::utf8_percent_encode(v, percent_encoding::NON_ALPHANUMERIC)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

const FORM: &str = "application/x-www-form-urlencoded";

/// The `action` URL of the rendered login form.
fn form_action(html: &str) -> String {
    let anchor = html
        .find("id=\"kc-form-login\"")
        .expect("keycloak rendered a login form");
    let rest = &html[anchor..];
    let at = rest.find("action=\"").expect("the form has an action") + "action=\"".len();
    let end = rest[at..].find('"').expect("the action is quoted");
    rest[at..at + end].replace("&amp;", "&")
}

/// Sign `alice` in at Keycloak and return the `code` and `state` its redirect carries.
async fn authenticate(authorize_url: &str) -> (String, String) {
    let http = http();
    let page = http.get(authorize_url).send().await.expect("login page");
    assert_eq!(page.status(), StatusCode::OK, "keycloak rendered no form");
    let cookies = jar(&page);
    let html = page.text().await.expect("html");
    let action = form_action(&html);

    let submitted = http
        .post(&action)
        .header(reqwest::header::COOKIE, cookies)
        .header(reqwest::header::CONTENT_TYPE, FORM)
        .body(form_body(&[("username", USER), ("password", PASSWORD)]))
        .send()
        .await
        .expect("login post");
    assert_eq!(
        submitted.status(),
        StatusCode::FOUND,
        "keycloak refused the credentials"
    );
    let location = submitted
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .expect("the login redirects back")
        .to_string();
    let url = url::Url::parse(&location).expect("redirect url");
    let mut code = None;
    let mut state = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.to_string()),
            "state" => state = Some(v.to_string()),
            _ => {}
        }
    }
    (
        code.expect("the redirect carries a code"),
        state.expect("the redirect carries the state"),
    )
}

/// A token response from the direct-access-grant, for the bearer-path tests. Password grants are
/// only ever a way to get a REAL Keycloak-minted token here; nothing in the controller runs one.
async fn tokens(base: &str, client_id: &str) -> serde_json::Value {
    let mut form = vec![
        ("grant_type", "password"),
        ("client_id", client_id),
        ("username", USER),
        ("password", PASSWORD),
        ("scope", "openid"),
    ];
    if client_id == RP_CLIENT || client_id == ROLES_CLIENT {
        form.push(("client_secret", RP_SECRET));
    }
    let res = http()
        .post(format!(
            "{base}/realms/{REALM}/protocol/openid-connect/token"
        ))
        .header(reqwest::header::CONTENT_TYPE, FORM)
        .body(form_body(&form))
        .send()
        .await
        .expect("token request");
    assert!(
        res.status().is_success(),
        "the direct access grant for {client_id} failed: {}",
        res.text().await.unwrap_or_default()
    );
    res.json().await.expect("token json")
}

fn field<'a>(tokens: &'a serde_json::Value, name: &str) -> &'a str {
    tokens[name]
        .as_str()
        .unwrap_or_else(|| panic!("the token response carries no {name}"))
}

/// Drive the whole browser flow and return the session cookie it set. The individual steps are
/// asserted in [`the_browser_flow_signs_a_user_in_and_the_session_carries_their_groups`]; every
/// other test needs the cookie, not the walk.
async fn sign_in(app: &axum::Router) -> String {
    let start = call(app, get("/auth/login", None)).await;
    assert_eq!(start.status, StatusCode::SEE_OTHER);
    let authorize_url = start.location.expect("login redirects to the issuer");
    let flow_cookie = start.cookie.expect("the login parked its flow");
    let (code, state) = authenticate(&authorize_url).await;
    let done = call(
        app,
        get(
            &format!("/auth/callback?code={code}&state={state}"),
            Some(&flow_cookie),
        ),
    )
    .await;
    assert_eq!(done.status, StatusCode::SEE_OTHER, "{}", done.body);
    done.cookie.expect("the callback set a session cookie")
}

/// A playbook whose scope binds a secret, plus one schedule on it, owned by `login` with a stale
/// group snapshot — the shape whose firing needs the owner's live groups.
async fn seed_bound_schedule(pool: &PgPool, login: &str, snapshot_groups: &[&str]) -> String {
    use crate::authz::model::Principal;
    use crate::secrets::store::{NewBinding, NewSecret};
    use crate::secrets::{
        ConsumerClass, ProjectionKind, ScopeKind, SecretKind, SecretMode, SecretName, Visibility,
    };
    sqlx::query(
        r#"INSERT INTO playbooks (id, description, repo, git_ref, rev, path, tar_gz,
                                  tar_digest, tar_bytes, params_schema, schema_digest,
                                  core_rev, created_by, created_at, updated_at)
           VALUES ('survey', 'reads a paper', 'owner/packs', 'main', 'abc123', '', $1,
                   'sha256:tar', 3, '{"type":"object"}'::jsonb, 'sha256:schema', 'core1',
                   $2, '2026-08-23T00:00:00Z', '2026-08-23T00:00:00Z')"#,
    )
    .bind(vec![1u8, 2, 3])
    .bind(login)
    .execute(pool)
    .await
    .expect("register the playbook");

    let owner = Principal::parse(&format!("user:{login}")).expect("owner");
    let name = SecretName::parse("pr_token").expect("name");
    let secret_id = uuid::Uuid::now_v7().to_string();
    let mut conn = pool.acquire().await.expect("conn");
    crate::secrets::store::insert(
        &mut conn,
        &NewSecret {
            id: &secret_id,
            name: &name,
            owner: &owner,
            kind: SecretKind::Opaque,
            visibility: Visibility::BrokerOnly,
            consumer: ConsumerClass::Run,
            mode: SecretMode::Managed,
            vault_path: "user:alice/pr-token",
            current_version: Some(1),
            created_by: Some(login),
        },
    )
    .await
    .expect("register the secret");
    crate::secrets::store::insert_binding(
        &mut conn,
        &NewBinding {
            id: &uuid::Uuid::now_v7().to_string(),
            secret_id: &secret_id,
            scope_kind: ScopeKind::Playbook,
            scope_id: "survey",
            projection_kind: ProjectionKind::Env,
            projection: "AUTORESEARCH_PR_TOKEN",
            declared_name: &name,
            pack_rev: None,
            schema_digest: None,
            created_by: Some(login),
        },
    )
    .await
    .expect("bind");
    drop(conn);

    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        r#"INSERT INTO playbook_standing_launches (id, trigger, playbook, params, schema_digest,
                                                   max_cost, max_time, advance_dedupe, enabled,
                                                   created_by, owner_principal, owner_groups,
                                                   owner_groups_at, created_at, updated_at,
                                                   adopted_repo, adopted_path, adopted_rev,
                                                   adopted_tar_gz, adopted_tar_digest,
                                                   adopted_tar_bytes, adopted_params_schema)
           VALUES ($1, 'schedule', 'survey', '{}'::jsonb, 'sha256:schema', 3.5, '30m', true, true,
                   $2, $3, $4, '2026-08-23T12:00:00Z', '2026-08-23T00:00:00Z',
                   '2026-08-23T00:00:00Z', 'owner/packs', '', 'abc123', $5, 'sha256:tar', 3,
                   '{"type":"object"}'::jsonb)"#,
    )
    .bind(&id)
    .bind(login)
    .bind(format!("user:{login}"))
    .bind(serde_json::Value::from(snapshot_groups.to_vec()))
    .bind(vec![1u8, 2, 3])
    .execute(pool)
    .await
    .expect("store the standing launch");
    sqlx::query(
        "INSERT INTO playbook_schedules (id, cron_expr, tz, next_due_at)
         VALUES ($1, '0 * * * *', 'UTC', '2026-08-23T12:00:00Z')",
    )
    .bind(&id)
    .execute(pool)
    .await
    .expect("store the schedule");
    id
}

// --- the tests -----------------------------------------------------------------------------------

/// The whole browser flow: login starts a PKCE code request, keycloak authenticates the user, the
/// callback validates the ID token it gets back, and the session that comes out carries the login
/// and the groups the realm's mapper asserted.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn the_browser_flow_signs_a_user_in_and_the_session_carries_their_groups(pool: PgPool) {
    let Some(base) = keycloak().await else { return };
    let provider = Arc::new(OidcProvider::new(cfg(&base, DEVICE_CLIENT)).expect("provider"));
    let app = native_router(&pool, provider).await;

    // Nothing is signed in yet, so the guarded surface refuses.
    assert_eq!(
        call(&app, get("/api/whoami", None)).await.status,
        StatusCode::UNAUTHORIZED
    );

    let start = call(&app, get("/auth/login?rd=/issues", None)).await;
    assert_eq!(start.status, StatusCode::SEE_OTHER);
    let authorize_url = start.location.expect("login redirects to the issuer");
    assert!(
        authorize_url.starts_with(&issuer(&base)),
        "the redirect must go to the issuer, got {authorize_url}"
    );
    assert!(
        authorize_url.contains("code_challenge=")
            && authorize_url.contains("code_challenge_method=S256"),
        "the authorization request must carry a PKCE challenge: {authorize_url}"
    );
    assert!(authorize_url.contains("nonce="), "and a nonce");
    assert!(authorize_url.contains("state="), "and a state");
    let flow_cookie = start
        .cookie
        .expect("the login parked its flow on a session");

    let (code, state) = authenticate(&authorize_url).await;
    let done = call(
        &app,
        get(
            &format!("/auth/callback?code={code}&state={state}"),
            Some(&flow_cookie),
        ),
    )
    .await;
    assert_eq!(done.status, StatusCode::SEE_OTHER, "{}", done.body);
    assert_eq!(
        done.location.as_deref(),
        Some("/issues"),
        "the callback lands where the login asked"
    );
    let session = done.cookie.expect("the callback set a session cookie");
    assert_ne!(session, flow_cookie, "the session id is cycled at login");

    let who = call(&app, get("/api/whoami", Some(&session))).await;
    assert_eq!(who.status, StatusCode::OK);
    let who: serde_json::Value = serde_json::from_str(&who.body).expect("whoami json");
    assert_eq!(who["user"], serde_json::json!(USER));
    assert_eq!(who["mode"], serde_json::json!("native"));
    assert_eq!(who["groups"], serde_json::json!([USER_GROUP]));
    assert_eq!(
        who["role"],
        serde_json::json!("operator"),
        "the realm's group grants operator through auth.operatorGroups"
    );

    // The login is recorded, which is what lets a `user:` principal and a cluster token resolve.
    assert!(
        crate::identity::oidc::users::is_known_login(&pool, USER)
            .await
            .expect("lookup")
    );

    // Logout ends the session here and hands the browser to the issuer's own end_session endpoint.
    let out = call(&app, get("/auth/logout", Some(&session))).await;
    assert_eq!(out.status, StatusCode::SEE_OTHER);
    let end_session = out.location.expect("logout redirects");
    assert!(
        end_session.starts_with(&format!("{}/protocol/openid-connect/logout", issuer(&base))),
        "logout must reach the issuer's end_session_endpoint, got {end_session}"
    );
    // Without the hint Keycloak stops on its own confirmation page instead of ending the session
    // and honouring post_logout_redirect_uri.
    assert!(
        end_session.contains("id_token_hint=ey"),
        "logout must carry the login's ID token as id_token_hint, got {end_session}"
    );

    // Where the issuer sends the browser back: the registered redirect URI, with no authorization
    // response on it. That is not a login to refuse.
    let back = call(&app, get("/auth/callback", None)).await;
    assert_eq!(back.status, StatusCode::SEE_OTHER);
    assert_eq!(back.location.as_deref(), Some("/"));
    assert_eq!(
        call(&app, get("/api/whoami", Some(&session))).await.status,
        StatusCode::UNAUTHORIZED,
        "the session is gone"
    );
}

/// The nonce binds the ID token to the request this browser started. A token that answers a
/// different request is refused even though it is genuinely signed by the issuer.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn an_id_token_answering_another_request_is_refused(pool: PgPool) {
    let Some(base) = keycloak().await else { return };
    let _ = &pool;
    let provider = OidcProvider::new(cfg(&base, DEVICE_CLIENT)).expect("provider");

    let (url, flow) = provider
        .authorize("/".to_string())
        .await
        .expect("authorize");
    let (code, state) = authenticate(&url).await;
    assert_eq!(state, flow.state, "keycloak echoes the state verbatim");

    let tampered = LoginFlow {
        nonce: "a-nonce-this-browser-never-sent".to_string(),
        ..flow
    };
    let refusal = provider
        .exchange(code, &tampered)
        .await
        .expect_err("a mismatched nonce must not validate");
    let message = refusal.to_string();
    assert!(
        message.contains("ID token did not validate"),
        "expected an ID token refusal, got {message}"
    );
}

/// The state proves the authorization RESPONSE belongs to the request this browser started, before
/// a single byte of it is exchanged.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_callback_with_the_wrong_state_is_refused(pool: PgPool) {
    let Some(base) = keycloak().await else { return };
    let provider = Arc::new(OidcProvider::new(cfg(&base, DEVICE_CLIENT)).expect("provider"));
    let app = native_router(&pool, provider).await;

    let start = call(&app, get("/auth/login", None)).await;
    let authorize_url = start.location.expect("login redirects");
    let flow_cookie = start.cookie.expect("flow cookie");
    let (code, _state) = authenticate(&authorize_url).await;

    let done = call(
        &app,
        get(
            &format!("/auth/callback?code={code}&state=not-the-state-we-sent"),
            Some(&flow_cookie),
        ),
    )
    .await;
    assert_eq!(done.status, StatusCode::UNAUTHORIZED);
    assert!(done.body.contains("state does not match"), "{}", done.body);

    // And a callback with no flow at all — a replayed link, a cookie-less client — is a 400, not a
    // half-run exchange.
    let orphan = call(
        &app,
        get("/auth/callback?code=whatever&state=whatever", None),
    )
    .await;
    assert_eq!(orphan.status, StatusCode::BAD_REQUEST);
}

/// The CLI path: a real Keycloak access token from the public device-flow client authenticates,
/// and its claims are the caller's identity and groups.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_device_flow_access_token_authenticates_as_its_claims(pool: PgPool) {
    let Some(base) = keycloak().await else { return };
    let provider = Arc::new(OidcProvider::new(cfg(&base, DEVICE_CLIENT)).expect("provider"));
    let app = native_router(&pool, provider).await;
    let tokens = tokens(&base, DEVICE_CLIENT).await;

    let res = call(
        &app,
        Request::get("/api/whoami")
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", field(&tokens, "access_token")),
            )
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let who: serde_json::Value = serde_json::from_str(&res.body).expect("whoami json");
    assert_eq!(who["user"], serde_json::json!(USER));
    assert_eq!(who["groups"], serde_json::json!([USER_GROUP]));
    assert_eq!(who["role"], serde_json::json!("operator"));
}

/// Everything the JWT path refuses, all of it genuinely signed by this issuer: a token minted for
/// the relying party (the browser's credential), a token minted for another application on the same
/// realm, an ID token, and an expired token.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn the_jwt_path_refuses_every_token_that_is_not_the_device_client_s(pool: PgPool) {
    let Some(base) = keycloak().await else { return };
    let provider = Arc::new(OidcProvider::new(cfg(&base, DEVICE_CLIENT)).expect("provider"));
    let app = native_router(&pool, provider).await;

    let bearer = |app: &axum::Router, token: String| {
        let app = app.clone();
        async move {
            call(
                &app,
                Request::get("/api/whoami")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .status
        }
    };

    // The relying party's own access token: that credential belongs to the browser session, and
    // accepting it would make a stolen web token a CLI credential.
    let rp = tokens(&base, RP_CLIENT).await;
    assert_eq!(
        bearer(&app, field(&rp, "access_token").to_string()).await,
        StatusCode::UNAUTHORIZED
    );

    // A different application on the same realm: same issuer, same signing key, wrong audience.
    let other = tokens(&base, OTHER_CLIENT).await;
    assert_eq!(
        bearer(&app, field(&other, "access_token").to_string()).await,
        StatusCode::UNAUTHORIZED
    );

    // An ID token is not a bearer credential, even the device client's own.
    let device = tokens(&base, DEVICE_CLIENT).await;
    assert_eq!(
        bearer(&app, field(&device, "id_token").to_string()).await,
        StatusCode::UNAUTHORIZED
    );

    // Garbage that is merely JWT-SHAPED never reaches the cluster-token path either.
    assert_eq!(
        bearer(&app, "aGVhZGVy.cGF5bG9hZA.c2ln".to_string()).await,
        StatusCode::UNAUTHORIZED
    );

    // An expired token: the brief client's access tokens live one second.
    let brief_provider = Arc::new(OidcProvider::new(cfg(&base, BRIEF_CLIENT)).expect("provider"));
    let brief_app = native_router(&pool, brief_provider).await;
    let brief = tokens(&base, BRIEF_CLIENT).await;
    let token = field(&brief, "access_token").to_string();
    assert_eq!(
        bearer(&brief_app, token.clone()).await,
        StatusCode::OK,
        "the brief client's token is accepted while it lives"
    );
    // The brief client's tokens live one second; the wait clears that plus the guard's skew leeway.
    tokio::time::sleep(std::time::Duration::from_secs(8)).await;
    assert_eq!(
        bearer(&brief_app, token).await,
        StatusCode::UNAUTHORIZED,
        "an expired token is refused"
    );
}

/// The offline credential, end to end. The browser flow stores it encrypted in the same
/// transaction as the `users` row; the row is not the token; and signing out leaves it in place,
/// because a schedule owner who closes their tab must keep firing.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn the_callback_stores_an_encrypted_offline_credential_that_logout_leaves(pool: PgPool) {
    let Some(base) = keycloak().await else { return };
    let refresher = Arc::new(OidcProvider::new(cfg_offline(&base)).expect("provider"));
    let keys = test_keys();
    let app = native_router_with(
        &pool,
        refresher.clone(),
        Some(keys.clone()),
        DEFAULT_REFRESH_AFTER,
    )
    .await;

    let cookie = sign_in(&app).await;
    let stored: Vec<(String, String)> =
        sqlx::query_as("SELECT sub, token_cipher FROM user_credentials")
            .fetch_all(&pool)
            .await
            .expect("credentials");
    assert_eq!(stored.len(), 1, "one row per user");
    let (sub, cipher) = &stored[0];
    assert!(
        !cipher.contains('.'),
        "a JWT-shaped refresh token must not be sitting in the row: {cipher}"
    );
    // The mounted key opens what the callback sealed, which is the only proof the round trip works.
    let refresh = crate::identity::oidc::credentials::OwnerRefresh::new(
        pool.clone(),
        refresher.clone(),
        keys.clone(),
    );
    assert!(
        matches!(
            refresh.refresh(sub).await,
            Ok(crate::identity::oidc::credentials::RefreshOutcome::Claims(
                _
            ))
        ),
        "the stored credential must be spendable"
    );

    // The settings surface sees it.
    let seen = call(&app, get("/api/credentials/me", Some(&cookie))).await;
    assert_eq!(seen.status, StatusCode::OK);
    assert!(seen.body.contains("\"present\":true"), "{}", seen.body);

    // Signing out ends the session and leaves the credential alone.
    let out = call(&app, get("/auth/logout", Some(&cookie))).await;
    assert_eq!(out.status, StatusCode::SEE_OTHER);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM user_credentials")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(rows, 1, "browser logout must not revoke the offline token");
}

/// Two schedules of one owner firing in the same sweep. Both refreshes run concurrently against a
/// realm that permits refresh-token reuse; the per-subject advisory lock serializes them, so both
/// land and whichever token the issuer rotated last is the one stored.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn two_refreshes_of_one_owner_serialize_and_both_land(pool: PgPool) {
    let Some(base) = keycloak().await else { return };
    let provider = Arc::new(OidcProvider::new(cfg_offline(&base)).expect("provider"));
    let keys = test_keys();
    let app = native_router_with(
        &pool,
        provider.clone(),
        Some(keys.clone()),
        DEFAULT_REFRESH_AFTER,
    )
    .await;
    sign_in(&app).await;
    let sub: String = sqlx::query_scalar("SELECT sub FROM user_credentials")
        .fetch_one(&pool)
        .await
        .expect("the login stored one");

    let refresh = Arc::new(crate::identity::oidc::credentials::OwnerRefresh::new(
        pool.clone(),
        provider,
        keys.clone(),
    ));
    let (a, b) = tokio::join!(
        {
            let refresh = refresh.clone();
            let sub = sub.clone();
            async move { refresh.refresh(&sub).await }
        },
        {
            let refresh = refresh.clone();
            let sub = sub.clone();
            async move { refresh.refresh(&sub).await }
        }
    );
    for (which, outcome) in [("first", a), ("second", b)] {
        match outcome.unwrap_or_else(|e| panic!("the {which} refresh failed: {e}")) {
            crate::identity::oidc::credentials::RefreshOutcome::Claims(claims) => {
                assert_eq!(claims.login, USER);
                assert!(
                    claims.groups.iter().any(|g| g == USER_GROUP),
                    "the refreshed ID token carries the realm's groups: {:?}",
                    claims.groups
                );
            }
            other => panic!("the {which} refresh produced {other:?}"),
        }
    }
    // The stored token still works, so the rotation the lock serialized was persisted.
    assert!(
        matches!(
            refresh.refresh(&sub).await,
            Ok(crate::identity::oidc::credentials::RefreshOutcome::Claims(
                _
            ))
        ),
        "the persisted token must still be spendable"
    );
    let status = crate::identity::oidc::credentials::status(&pool, &sub)
        .await
        .expect("status")
        .expect("present");
    assert_eq!(status.failures, 0);
    assert!(status.refreshed_at.is_some(), "the refresh was stamped");
}

/// A due schedule's owner groups are re-read from their offline credential before the row is
/// claimed, so the launch carries what the issuer says NOW and not what the last save recorded.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_fire_time_refresh_puts_live_groups_onto_the_launch(pool: PgPool) {
    let Some(base) = keycloak().await else { return };
    let provider = Arc::new(OidcProvider::new(cfg_offline(&base)).expect("provider"));
    let keys = test_keys();
    let app = native_router_with(
        &pool,
        provider.clone(),
        Some(keys.clone()),
        DEFAULT_REFRESH_AFTER,
    )
    .await;
    sign_in(&app).await;

    let schedule = seed_bound_schedule(&pool, USER, &["/stale-group"]).await;
    let refresh =
        crate::identity::oidc::credentials::OwnerRefresh::new(pool.clone(), provider, keys);
    let db = Db::new(pool.clone());
    let fired = crate::launches::schedules::fire_due_refreshing(
        &db,
        "2026-08-23T12:00:30Z".parse().expect("now"),
        8,
        5,
        std::time::Duration::from_secs(3600),
        Some(&refresh),
    )
    .await
    .expect("fire");
    assert_eq!(fired.len(), 1, "the firing launched");
    let launch = crate::launches::store::get_playbook_launch(&pool, &fired[0])
        .await
        .expect("launch")
        .expect("a launch row");
    assert_eq!(
        launch.launcher_groups,
        vec![USER_GROUP.to_string()],
        "the launch carries the issuer's live groups, not the stale snapshot"
    );
    let row = crate::launches::schedules::ScheduleStore::new(crate::client::Db::new(pool.clone()))
        .get(&schedule)
        .await
        .expect("get")
        .expect("row");
    assert!(!row.owner_signin_required);
    assert_eq!(row.owner_refresh_error, None);
    assert!(row.owner_refresh_at.is_some());
}

/// A user revokes their credential from settings: the row is gone, and the schedules whose scopes
/// bind secrets stop firing until they sign in again.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_revoke_drops_the_credential_and_parks_the_owner_s_schedules(pool: PgPool) {
    let Some(base) = keycloak().await else { return };
    let provider = Arc::new(OidcProvider::new(cfg_offline(&base)).expect("provider"));
    let keys = test_keys();
    let app = native_router_with(
        &pool,
        provider.clone(),
        Some(keys.clone()),
        DEFAULT_REFRESH_AFTER,
    )
    .await;
    let cookie = sign_in(&app).await;
    let schedule = seed_bound_schedule(&pool, USER, &[USER_GROUP]).await;

    let revoked = call(
        &app,
        Request::delete("/api/credentials/me")
            .header(header::COOKIE, &cookie)
            .header("sec-fetch-site", "same-origin")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(revoked.status, StatusCode::OK, "{}", revoked.body);
    assert!(
        revoked.body.contains("\"revoked\":true"),
        "{}",
        revoked.body
    );

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM user_credentials")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(rows, 0, "the credential is gone");

    let refresh =
        crate::identity::oidc::credentials::OwnerRefresh::new(pool.clone(), provider, keys);
    let db = Db::new(pool.clone());
    let fired = crate::launches::schedules::fire_due_refreshing(
        &db,
        "2026-08-23T12:00:30Z".parse().expect("now"),
        8,
        5,
        std::time::Duration::from_secs(3600),
        Some(&refresh),
    )
    .await
    .expect("fire");
    assert!(fired.is_empty(), "a revoked owner dispatches nothing");
    let row = crate::launches::schedules::ScheduleStore::new(crate::client::Db::new(pool.clone()))
        .get(&schedule)
        .await
        .expect("get")
        .expect("row");
    assert!(row.owner_signin_required, "the view says: sign in again");

    // Signing in again stores a fresh credential and re-arms the schedule.
    sign_in(&app).await;
    let row = crate::launches::schedules::ScheduleStore::new(crate::client::Db::new(pool.clone()))
        .get(&schedule)
        .await
        .expect("get")
        .expect("row");
    assert!(!row.owner_signin_required);
}

/// A live session re-reads its groups from the offline credential on use, and a definitive refusal
/// downgrades it: the login stands, every role it carried is gone, and writes are refused until the
/// user signs in again.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_session_refreshes_its_groups_on_use_and_a_refusal_downgrades_it(pool: PgPool) {
    let Some(base) = keycloak().await else { return };
    let provider = Arc::new(OidcProvider::new(cfg_offline(&base)).expect("provider"));
    let keys = test_keys();
    // Zero-length staleness window: every request re-reads, which is what makes the refresh
    // observable inside one test.
    let app = native_router_with(
        &pool,
        provider,
        Some(keys),
        std::time::Duration::from_secs(0),
    )
    .await;
    let cookie = sign_in(&app).await;

    // The realm's group mapper puts alice in the operator group, and the refresh keeps her there.
    let who = call(&app, get("/api/whoami", Some(&cookie))).await;
    assert_eq!(who.status, StatusCode::OK);
    assert!(
        who.body.contains("\"role\":\"operator\"") && who.body.contains(USER_GROUP),
        "the refreshed session keeps its groups: {}",
        who.body
    );
    assert!(who.body.contains("\"downgraded\":false"), "{}", who.body);
    let refreshed: Option<String> = sqlx::query_scalar("SELECT refreshed_at FROM user_credentials")
        .fetch_one(&pool)
        .await
        .expect("row");
    assert!(
        refreshed.is_some(),
        "the session refresh spent the credential"
    );

    // The credential dies under the session. The next request is refused a refresh, so the session
    // is downgraded rather than left holding a role nobody has confirmed.
    sqlx::query("DELETE FROM user_credentials")
        .execute(&pool)
        .await
        .expect("revoke");
    let who = call(&app, get("/api/whoami", Some(&cookie))).await;
    assert_eq!(who.status, StatusCode::OK);
    assert!(
        who.body.contains("\"role\":\"viewer\"") && who.body.contains("\"downgraded\":true"),
        "a refused refresh downgrades the session to viewer: {}",
        who.body
    );
    assert!(
        !who.body.contains(USER_GROUP),
        "and drops its groups: {}",
        who.body
    );
    // The downgrade is not cosmetic: an operator-gated write is refused.
    let write = call(
        &app,
        Request::post("/api/issues/some-key/park")
            .header(header::COOKIE, &cookie)
            .header("sec-fetch-site", "same-origin")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"reason":"because"}"#))
            .expect("request"),
    )
    .await;
    assert_eq!(
        write.status,
        StatusCode::FORBIDDEN,
        "a downgraded session holds no role: {}",
        write.body
    );
}

/// RH SSO puts membership on the access token as realm roles and nothing on the ID token. The
/// callback must read the exchanged access token, nonce and all, and the session carries the
/// groups exactly as if the ID token had asserted them.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn groups_come_from_the_access_token_when_the_id_token_has_none(pool: PgPool) {
    let Some(base) = keycloak().await else { return };
    let provider =
        Arc::new(OidcProvider::new(cfg_for(&base, ROLES_CLIENT, DEVICE_CLIENT)).expect("provider"));
    let app = native_router(&pool, provider).await;
    let start = call(&app, get("/auth/login?rd=/", None)).await;
    let authorize_url = start.location.expect("login redirects to the issuer");
    let flow_cookie = start.cookie.expect("flow cookie");
    let (code, state) = authenticate(&authorize_url).await;
    let done = call(
        &app,
        get(
            &format!("/auth/callback?code={code}&state={state}"),
            Some(&flow_cookie),
        ),
    )
    .await;
    assert_eq!(done.status, StatusCode::SEE_OTHER, "{}", done.body);
    let session = done.cookie.expect("session cookie");
    let who = call(&app, get("/api/whoami", Some(&session))).await;
    let who: serde_json::Value = serde_json::from_str(&who.body).expect("whoami json");
    assert_eq!(
        who["groups"],
        serde_json::json!([USER_GROUP]),
        "the group came off the access token: {who}"
    );
}
