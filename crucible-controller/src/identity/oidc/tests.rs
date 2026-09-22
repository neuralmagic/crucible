use super::*;

use crate::identity::auth::AuthMode;

#[test]
fn groups_come_from_the_groups_claim_and_fall_back_to_realm_roles() {
    let claims: GroupClaims = serde_json::from_value(
        serde_json::json!({ "groups": [" /Groups/Team-X ", "", "/groups/sre"] }),
    )
    .expect("claims");
    assert_eq!(claims.normalized(), vec!["/groups/team-x", "/groups/sre"]);

    // A realm that emits no `groups` mapper still names its realm roles; that is the shape a
    // plain Keycloak install has before a group mapper is added to the client.
    let claims: GroupClaims =
        serde_json::from_value(serde_json::json!({ "realm_access": { "roles": ["Ops", "dev"] } }))
            .expect("claims");
    assert_eq!(claims.normalized(), vec!["ops", "dev"]);

    // With both present the explicit claim wins: the roles list is the fallback, not a supplement.
    let claims: GroupClaims = serde_json::from_value(serde_json::json!({
        "groups": ["/groups/team-x"],
        "realm_access": { "roles": ["ops"] }
    }))
    .expect("claims");
    assert_eq!(claims.normalized(), vec!["/groups/team-x"]);

    let claims: GroupClaims = serde_json::from_value(serde_json::json!({})).expect("claims");
    assert!(claims.normalized().is_empty());
}

/// The login is what every role list, ownership check, and `user:` principal is compared against,
/// so it is normalized once here and never again.
#[test]
fn the_login_prefers_preferred_username_then_email_then_the_subject() {
    let login = |p: Option<&str>, e: Option<&str>| {
        login_from(
            &p.map(str::to_string),
            &e.map(str::to_string),
            "b8c2-uuid-subject",
        )
    };
    assert_eq!(login(Some("Alice"), Some("alice@example.com")), "alice");
    assert_eq!(
        login(Some("  "), Some("Alice@Example.com")),
        "alice@example.com",
        "a blank preferred_username falls through"
    );
    assert_eq!(login(None, None), "b8c2-uuid-subject");
}

/// A JWT is the issuer's word; a cluster token is the API server's. The guard must never hand one
/// to the other's validator.
#[test]
fn only_a_three_segment_token_looks_like_a_jwt() {
    use crate::identity::auth::looks_like_jwt;
    assert!(looks_like_jwt("aGVhZGVy.cGF5bG9hZA.c2ln"));
    assert!(!looks_like_jwt("sha256~AbCdEf"));
    assert!(!looks_like_jwt("opaque-token"));
    assert!(!looks_like_jwt("two.segments"));
    assert!(!looks_like_jwt("four.seg.ments.here"));
    assert!(!looks_like_jwt("empty..segment"));
}

#[tokio::test]
async fn the_registration_reads_off_the_environment_and_refuses_half_of_one() {
    let _lock = crate::ENV_LOCK.lock().await;
    let vars = [
        "CONTROLLER_OIDC_ISSUER",
        "CONTROLLER_OIDC_CLIENT_ID",
        "CONTROLLER_OIDC_CLIENT_SECRET",
        "CONTROLLER_OIDC_REDIRECT_URL",
        "CONTROLLER_OIDC_SCOPES",
        "CONTROLLER_OIDC_DEVICE_CLIENT_ID",
        "CONTROLLER_AUTH_MODE",
    ];
    for var in vars {
        // SAFETY: the crate-wide env lock is held for the whole test.
        unsafe { std::env::remove_var(var) };
    }

    assert!(
        OidcCfg::from_env()
            .expect("no issuer is not an error")
            .is_none(),
        "no issuer configured is a proxy-mode or local deployment"
    );
    assert_eq!(AuthMode::from_env(), AuthMode::Proxy);

    unsafe {
        std::env::set_var("CONTROLLER_OIDC_ISSUER", "https://idp.example.com/realms/r");
        std::env::set_var("CONTROLLER_AUTH_MODE", " NATIVE ");
    }
    assert_eq!(AuthMode::from_env(), AuthMode::Native);
    let refusal = OidcCfg::from_env().expect_err("a client id is required");
    assert!(format!("{refusal:#}").contains("CONTROLLER_OIDC_CLIENT_ID"));

    unsafe { std::env::set_var("CONTROLLER_OIDC_CLIENT_ID", "crucible-controller") };
    let refusal = OidcCfg::from_env().expect_err("a redirect url is required");
    assert!(format!("{refusal:#}").contains("CONTROLLER_OIDC_REDIRECT_URL"));

    unsafe {
        std::env::set_var(
            "CONTROLLER_OIDC_REDIRECT_URL",
            "https://crucible.example.com/auth/callback",
        )
    };
    let cfg = OidcCfg::from_env().expect("reads").expect("configured");
    assert_eq!(
        cfg.scopes,
        vec!["openid", "email", "profile", "offline_access"],
        "the default scope set carries offline_access"
    );
    assert!(cfg.client_secret.is_none());
    assert!(
        cfg.device_client_id.is_none(),
        "no device client, no JWT path"
    );

    // The deploy's list wins verbatim, comma- or space-separated, because a scope the realm has
    // not registered breaks login outright.
    unsafe { std::env::set_var("CONTROLLER_OIDC_SCOPES", " openid, email  profile ") };
    let cfg = OidcCfg::from_env().expect("reads").expect("configured");
    assert_eq!(cfg.scopes, vec!["openid", "email", "profile"]);

    for var in vars {
        unsafe { std::env::remove_var(var) };
    }
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_login_maps_a_subject_to_a_login_and_the_name_follows_the_subject(pool: sqlx::PgPool) {
    let now = jiff::Timestamp::now();
    assert!(
        !users::is_known_login(&pool, "alice").await.expect("lookup"),
        "nobody has signed in yet"
    );

    users::record_login(&pool, "sub-alice", "alice", Some("alice@example.com"), now)
        .await
        .expect("first login");
    assert!(users::is_known_login(&pool, "alice").await.expect("lookup"));
    assert!(
        users::is_known_login(&pool, "  ALICE ")
            .await
            .expect("lookup"),
        "the lookup normalizes like the role lists"
    );
    assert_eq!(
        users::login_for_sub(&pool, "sub-alice")
            .await
            .expect("lookup"),
        Some("alice".to_string())
    );

    // The same subject signing in under a renamed account keeps one row.
    users::record_login(&pool, "sub-alice", "alice2", None, now)
        .await
        .expect("rename");
    assert_eq!(
        users::login_for_sub(&pool, "sub-alice")
            .await
            .expect("lookup"),
        Some("alice2".to_string())
    );
    assert!(!users::is_known_login(&pool, "alice").await.expect("lookup"));

    // A login that moves to a NEW subject (an account deleted and recreated in the IdP) takes the
    // name with it: two rows holding one login would make `user:<login>` ambiguous.
    users::record_login(&pool, "sub-alice-v2", "alice2", None, now)
        .await
        .expect("recreated account");
    assert_eq!(
        users::login_for_sub(&pool, "sub-alice")
            .await
            .expect("lookup"),
        None
    );
    assert_eq!(
        users::login_for_sub(&pool, "sub-alice-v2")
            .await
            .expect("lookup"),
        Some("alice2".to_string())
    );
    let rows: i64 = sqlx::query_scalar("select count(*) from users")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(rows, 1);
}

/// A JWT naming an unknown kid needs no credential, so the re-fetch it forces is rate limited: one
/// request to the issuer per interval, however many tokens arrive.
#[test]
fn a_forced_jwks_refresh_is_granted_once_per_interval() {
    let provider = OidcProvider::new(OidcCfg {
        issuer: "https://issuer.example.com/realms/r".to_string(),
        client_id: "controller".to_string(),
        client_secret: None,
        redirect_url: "https://controller.example.com/auth/callback".to_string(),
        scopes: vec!["openid".to_string()],
        device_client_id: Some("cli".to_string()),
        post_logout_redirect: None,
    })
    .expect("provider");

    let t0 = std::time::Instant::now();
    assert!(
        provider.claim_forced_jwks_refresh(t0),
        "the first unknown kid heals a rotation"
    );
    for offset in [
        Duration::from_millis(1),
        JWKS_MIN_REFRESH_INTERVAL - Duration::from_millis(1),
    ] {
        let later = t0.checked_add(offset).expect("instant");
        assert!(
            !provider.claim_forced_jwks_refresh(later),
            "a flood inside the interval sends nothing to the issuer"
        );
    }
    let after = t0.checked_add(JWKS_MIN_REFRESH_INTERVAL).expect("instant");
    assert!(
        provider.claim_forced_jwks_refresh(after),
        "the next interval heals a later rotation"
    );
}

/// A real HTTP issuer: discovery and the key set answer normally, the token endpoint answers
/// whatever the test hands it. This is the one shape a live Keycloak cannot be asked for — a
/// router answering on behalf of a pod that is restarting — and it is the shape production runs.
pub(crate) async fn issuer_answering(
    token: wiremock::ResponseTemplate,
) -> (wiremock::MockServer, Arc<OidcProvider>) {
    use wiremock::matchers::{method, path};
    let server = wiremock::MockServer::start().await;
    let base = server.uri();
    wiremock::Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issuer": base,
                "authorization_endpoint": format!("{base}/auth"),
                "token_endpoint": format!("{base}/token"),
                "jwks_uri": format!("{base}/certs"),
                "response_types_supported": ["code"],
                "subject_types_supported": ["public"],
                "id_token_signing_alg_values_supported": ["RS256"],
            })),
        )
        .mount(&server)
        .await;
    wiremock::Mock::given(method("GET"))
        .and(path("/certs"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({ "keys": [] })),
        )
        .mount(&server)
        .await;
    wiremock::Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(token)
        .mount(&server)
        .await;
    let provider = OidcProvider::new(OidcCfg {
        issuer: base,
        client_id: "rp".to_string(),
        client_secret: None,
        redirect_url: "http://localhost/auth/callback".to_string(),
        scopes: vec!["openid".to_string()],
        device_client_id: None,
        post_logout_redirect: None,
    })
    .expect("provider");
    (server, Arc::new(provider))
}

/// Everything short of an issuer-authored refusal is the issuer being unreachable. A 5xx from the
/// route in front of a restarting Keycloak carries an HTML or empty body, which the oauth2 client
/// reports as a parse or "other" failure — classifying those as refusals would count a failure on
/// the credential, downgrade live sessions, and park schedules over a rollout.
#[tokio::test]
async fn an_issuer_front_door_error_is_unreachable_not_a_refusal() {
    let cases = [
        (
            "a route answering 503 with its own HTML error page",
            wiremock::ResponseTemplate::new(503)
                .set_body_string("<html><body>Application is not available</body></html>"),
        ),
        (
            "a gateway answering 502 with no body at all",
            wiremock::ResponseTemplate::new(502),
        ),
        (
            "the issuer's own transient error code",
            wiremock::ResponseTemplate::new(503)
                .set_body_json(serde_json::json!({ "error": "temporarily_unavailable" })),
        ),
        (
            "an internal error the issuer authored",
            wiremock::ResponseTemplate::new(500)
                .set_body_json(serde_json::json!({ "error": "server_error" })),
        ),
    ];
    for (what, template) in cases {
        let (_server, provider) = issuer_answering(template).await;
        let err = provider.refresh("offline-token").await.expect_err("fails");
        assert!(
            matches!(err, OidcError::Unavailable(_)),
            "{what} must retry, not kill the credential: {err:?}"
        );
    }
}

/// The other side of the split: an OAuth error document naming a dead grant is definitive, and
/// costs the user a fresh sign-in.
#[tokio::test]
async fn a_revoked_grant_is_a_refusal() {
    for code in ["invalid_grant", "invalid_client", "unauthorized_client"] {
        let (_server, provider) = issuer_answering(
            wiremock::ResponseTemplate::new(400)
                .set_body_json(serde_json::json!({ "error": code })),
        )
        .await;
        let err = provider.refresh("offline-token").await.expect_err("fails");
        assert!(
            matches!(err, OidcError::Rejected(_)),
            "{code} is definitive: {err:?}"
        );
    }
}

/// A deployment that keeps the redirect URI its client already registers (the sidecar's
/// `/oauth2/callback`) needs the controller to answer there; `/auth/callback` stays too.
#[sqlx::test(migrations = "./migrations")]
async fn the_callback_is_mounted_at_the_redirect_urls_path(pool: sqlx::PgPool) {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt as _;

    let provider = OidcProvider::new(OidcCfg {
        issuer: "https://issuer.example.com/realms/r".to_string(),
        client_id: "controller".to_string(),
        client_secret: None,
        redirect_url: "https://controller.example.com/oauth2/callback".to_string(),
        scopes: vec!["openid".to_string()],
        device_client_id: None,
        post_logout_redirect: None,
    })
    .expect("provider");
    assert_eq!(
        provider.callback_path().as_deref(),
        Some("/oauth2/callback")
    );

    let app = routes::router(routes::AuthState {
        mode: AuthMode::Native,
        oidc: Some(Arc::new(provider)),
        pool,
        credential_keys: None,
        proxy_prefix: String::new(),
    });
    for path in ["/oauth2/callback", "/auth/callback"] {
        let res = app
            .clone()
            .oneshot(
                Request::get(format!("{path}?code=x&state=y"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_ne!(
            res.status(),
            StatusCode::NOT_FOUND,
            "{path} must be mounted"
        );
    }
    let res = app
        .oneshot(
            Request::get("/oauth2/other")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

/// RH SSO asserts membership as realm roles; other realms use a `groups` claim. Both read the
/// same way, `groups` winning when present, and the inventory names claims without values.
#[test]
fn groups_come_from_realm_roles_or_a_groups_claim() {
    let roles: GroupClaims = serde_json::from_value(
        serde_json::json!({"sub": "u", "realm_access": {"roles": ["/groups/Team-X", "offline_access"]}}),
    )
    .expect("claims");
    assert_eq!(roles.normalized(), vec!["/groups/team-x", "offline_access"]);
    let groups: GroupClaims = serde_json::from_value(
        serde_json::json!({"groups": [" /groups/a ", ""], "realm_access": {"roles": ["ignored"]}}),
    )
    .expect("claims");
    assert_eq!(groups.normalized(), vec!["/groups/a"]);

    let enc =
        |b: &[u8]| base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, b);
    let jwt = format!(
        "{}.{}.sig",
        enc(b"{}"),
        enc(br#"{"sub":"u","groups":["g"],"aud":"x"}"#)
    );
    assert_eq!(claim_names(&jwt), vec!["aud", "groups", "sub"]);
    assert!(claim_names("not-a-jwt").is_empty());
}
