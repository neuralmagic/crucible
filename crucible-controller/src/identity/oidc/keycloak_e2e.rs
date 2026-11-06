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
//! The server comes from `KEYCLOAK_URL`, else the shared `crucible-test-keycloak` container on
//! 58180, which this file creates if docker is available and recreates whenever the realm files
//! differ from the ones it imported. With neither, the tests print why they are skipping — unless `CRUCIBLE_REQUIRE_KEYCLOAK_TESTS` is set, which turns a
//! skip into a failure so CI can never go green by silently skipping the suite.

use crate::api::state::ApiState;
use crate::client::Db;
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
const USER: &str = "alice";
const PASSWORD: &str = "alice-password";
const USER_GROUP: &str = "/platform-devs";
/// The shared container this suite provisions, and its port.
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

/// The label that records which realm files a container imported. Keycloak imports a realm only
/// when it does not exist yet, so a container built from other files (an older realm, another
/// worktree's) keeps serving them until it is replaced.
const REALM_LABEL: &str = "io.crucible.realm-digest";

fn realm_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/keycloak")
}

/// A digest over every realm file the container imports, by name and content.
fn realm_digest() -> String {
    use sha2::{Digest, Sha256};
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(realm_dir())
        .expect("realm dir")
        .map(|entry| entry.expect("realm dir entry").path())
        .collect();
    files.sort();
    let mut hash = Sha256::new();
    for file in files {
        hash.update(file.file_name().expect("file name").as_encoded_bytes());
        hash.update(std::fs::read(&file).expect("realm file"));
    }
    format!("{:x}", hash.finalize())
}

async fn docker(args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new("docker")
        .args(args)
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The realm digest the shared container was created with: `None` when there is no container,
/// empty when it predates the label.
async fn container_digest() -> Option<String> {
    docker(&[
        "inspect",
        "--format",
        &format!("{{{{ index .Config.Labels \"{REALM_LABEL}\" }}}}"),
        CONTAINER,
    ])
    .await
    .map(|label| label.replace("<no value>", ""))
}

/// Bring the shared container up on the current realm files and wait for the realm to answer. A
/// container created from any other files is replaced. Serialized so a whole suite's worth of
/// tests cannot race a dozen `docker run`s against one name.
static START: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn start_container() -> bool {
    let _serialize = START.lock().await;
    let want = realm_digest();
    match container_digest().await {
        Some(have) if have == want => {
            docker(&["start", CONTAINER]).await;
        }
        have => {
            if have.is_some() {
                eprintln!("{CONTAINER} imported other realm files; recreating it");
                docker(&["rm", "-f", CONTAINER]).await;
            }
            docker(&[
                "run",
                "-d",
                "--name",
                CONTAINER,
                "--label",
                &format!("{REALM_LABEL}={want}"),
                "-e",
                "KC_BOOTSTRAP_ADMIN_USERNAME=admin",
                "-e",
                "KC_BOOTSTRAP_ADMIN_PASSWORD=admin",
                "-v",
                &format!("{}:/opt/keycloak/data/import:ro", realm_dir().display()),
                "-p",
                "58180:8080",
                "quay.io/keycloak/keycloak:26.4",
                "start-dev",
                "--import-realm",
            ])
            .await;
        }
    }
    if container_digest().await.as_deref() != Some(want.as_str()) {
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

/// The issuer's base URL, or `None` when there is none and the suite may skip. `KEYCLOAK_URL` is
/// somebody else's issuer and is used as it is; the default endpoint is the shared container, which
/// is checked against the realm files before it is trusted.
async fn keycloak() -> Option<String> {
    static BASE: tokio::sync::OnceCell<Option<String>> = tokio::sync::OnceCell::const_new();
    BASE.get_or_init(|| async {
        let configured = std::env::var("KEYCLOAK_URL")
            .ok()
            .map(|v| v.trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty());
        let base = match configured {
            Some(base) => ready(&base).await.then_some(base),
            None => start_container()
                .await
                .then(|| DEFAULT_URL.to_string()),
        };
        if base.is_none() && required() {
            panic!("CRUCIBLE_REQUIRE_KEYCLOAK_TESTS is set but no keycloak with the current realm answered");
        }
        if base.is_none() {
            eprintln!("no keycloak with the current realm; skipping the oidc e2e suite (needs docker, or KEYCLOAK_URL)");
        }
        base
    })
    .await
    .clone()
}

fn issuer(base: &str) -> String {
    format!("{base}/realms/{REALM}")
}

fn cfg(base: &str) -> OidcCfg {
    cfg_for(base, RP_CLIENT)
}

fn cfg_for(base: &str, client_id: &str) -> OidcCfg {
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
        post_logout_redirect: None,
    }
}

/// The human surface exactly as `serve` mounts it, against this issuer.
async fn native_router(pool: &PgPool, provider: Arc<OidcProvider>) -> axum::Router {
    let store = crate::identity::session::store(pool)
        .await
        .expect("session store");
    let guard = {
        let mut guard = crate::identity::auth::BearerGuard::new(
            crate::identity::auth::SharedToken::new("s3cr3t"),
            None,
        )
        .expect("guard builds");
        guard.oidc = Some(provider.clone());
        Arc::new(guard)
    };
    let state = ApiState {
        roles: crate::identity::auth::Roles::new(vec![], vec![], vec!["platform-devs".to_string()]),
        ..ApiState::test(Db::new(pool.clone()))
    };
    crate::human_router(
        state,
        store,
        false,
        crate::HumanAuth {
            guard,
            routes: crate::identity::oidc::routes::AuthState {
                oidc: Some(provider),
                pool: pool.clone(),
            },
            issuer: None,
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

/// The whole browser flow: login starts a PKCE code request, keycloak authenticates the user, the
/// callback validates the ID token it gets back, and the session that comes out carries the login
/// and the groups the realm's mapper asserted.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn the_browser_flow_signs_a_user_in_and_the_session_carries_their_groups(pool: PgPool) {
    let Some(base) = keycloak().await else { return };
    let provider = Arc::new(OidcProvider::new(cfg(&base)).expect("provider"));
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
    assert_eq!(who["groups"], serde_json::json!([USER_GROUP]));
    assert_eq!(
        who["role"],
        serde_json::json!("operator"),
        "the realm's group grants operator through auth.operatorGroups"
    );

    // The login is recorded, which is what lets a `user:` principal resolve.
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
    let provider = OidcProvider::new(cfg(&base)).expect("provider");

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
    let provider = Arc::new(OidcProvider::new(cfg(&base)).expect("provider"));
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

/// A Keycloak access token is no bearer: genuinely signed tokens from the public device-flow client
/// and from this relying party, and an ID token, are all refused.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_keycloak_token_is_not_a_bearer(pool: PgPool) {
    let Some(base) = keycloak().await else { return };
    let provider = Arc::new(OidcProvider::new(cfg(&base)).expect("provider"));
    let app = native_router(&pool, provider).await;
    let device = tokens(&base, DEVICE_CLIENT).await;
    let rp = tokens(&base, RP_CLIENT).await;
    for token in [
        field(&device, "access_token"),
        field(&device, "id_token"),
        field(&rp, "access_token"),
    ] {
        let res = call(
            &app,
            Request::get("/api/whoami")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{}", res.body);
    }
}

/// RH SSO puts membership on the access token as realm roles and nothing on the ID token. The
/// callback must read the exchanged access token, nonce and all, and the session carries the
/// groups exactly as if the ID token had asserted them.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn groups_come_from_the_access_token_when_the_id_token_has_none(pool: PgPool) {
    let Some(base) = keycloak().await else { return };
    let provider = Arc::new(OidcProvider::new(cfg_for(&base, ROLES_CLIENT)).expect("provider"));
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
