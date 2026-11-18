//! Signing a desktop app in through the browser. The app opens `/auth/native` with a loopback
//! redirect and a PKCE challenge, the person approves it on a page here, and the app redeems the
//! one-minute code with its verifier for an api key it keeps.
//!
//! ```text
//!  app ──opens──▶ GET /auth/native?redirect_uri&state&code_challenge&name
//!                   │ not signed in ──▶ /auth/login?rd=… ──▶ back here
//!                   ▼
//!  browser ◀── approval page ── POST /auth/native (pending id from the session)
//!                   ▼
//!  browser ──303──▶ http://127.0.0.1:<port>/callback?code&state ──▶ app
//!  app ──POST /auth/native/token {code, code_verifier, redirect_uri}──▶ {key, id, name, expires_at}
//! ```
//!
//! The key goes only to the process that holds the verifier; the browser only ever sees the code.

use axum::Json;
use axum::extract::{Form, Query, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tower_sessions::Session;
use url::Url;

use crate::agent_auth::people::clients::{RedirectRule, RuleSource};
use crate::agent_auth::people::{CodeChallenge, escape, form_page};

pub const NATIVE_PATH: &str = "/auth/native";

/// The approval page, its answer, and the code exchange, over the pool the codes and keys live in.
pub fn router(pool: sqlx::PgPool) -> axum::Router {
    axum::Router::new()
        .route(NATIVE_PATH, axum::routing::get(ask).post(decide))
        .route(NATIVE_TOKEN_PATH, axum::routing::post(redeem))
        .with_state(pool)
}
pub const NATIVE_TOKEN_PATH: &str = "/auth/native/token";
/// The redirect every app uses: `/callback` on 127.0.0.1, any port.
const CALLBACK: &str = "http://127.0.0.1/callback";
const PENDING_KEY: &str = "native_sign_in";
const CODE_LIFETIME: jiff::SignedDuration = jiff::SignedDuration::from_secs(60);
const KEY_LIFETIME: jiff::SignedDuration = jiff::SignedDuration::from_hours(90 * 24);
const DEFAULT_NAME: &str = "Crucible Playbooks";

#[derive(Debug, Deserialize)]
pub struct NativeQuery {
    redirect_uri: Option<String>,
    state: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    name: Option<String>,
}

/// A request that passed validation, carried in the session until the person decides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Request {
    redirect_uri: String,
    state: String,
    challenge: String,
    name: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Pending {
    id: String,
    sub: String,
    request: Request,
}

fn printable(text: &str, max: usize) -> bool {
    !text.is_empty() && text.len() <= max && text.chars().all(|c| !c.is_control())
}

fn validate(q: NativeQuery) -> Result<Request, String> {
    let rule = RedirectRule::parse(CALLBACK, RuleSource::Registry)
        .map_err(|e| format!("the app callback rule is invalid: {e}"))?;
    let redirect_uri = q.redirect_uri.unwrap_or_default();
    let url = Url::parse(&redirect_uri).map_err(|_| "redirect_uri is not a URL".to_string())?;
    if !rule.admits(&url, &redirect_uri) {
        return Err(format!(
            "redirect_uri must be http://127.0.0.1:<port>/callback, not {redirect_uri:?}"
        ));
    }
    let state = q.state.unwrap_or_default();
    if !printable(&state, 256) || !state.is_ascii() {
        return Err("state must be 1 to 256 printable ASCII characters".into());
    }
    if q.code_challenge_method.as_deref() != Some("S256") {
        return Err("code_challenge_method must be S256".into());
    }
    let challenge = q.code_challenge.unwrap_or_default();
    CodeChallenge::parse(&challenge).map_err(|e| e.to_string())?;
    let name = q
        .name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| DEFAULT_NAME.to_string());
    if !printable(&name, 64) {
        return Err("name must be 1 to 64 printable characters".into());
    }
    Ok(Request {
        redirect_uri,
        state,
        challenge,
        name,
    })
}

fn refused(why: &str) -> Response {
    form_page(
        StatusCode::BAD_REQUEST,
        "Sign-in refused",
        &format!("<p class=\"err\">{}</p>", escape(why)),
    )
}

fn session_failure(e: impl std::fmt::Display) -> Response {
    tracing::warn!(error = %e, "native sign-in session");
    refused("the browser session could not be read; try again")
}

/// `redirect` with `params` added to its query.
fn back_to_app(redirect: &str, params: &[(&str, &str)]) -> Response {
    match Url::parse(redirect) {
        Ok(mut url) => {
            url.query_pairs_mut().extend_pairs(params);
            Redirect::to(url.as_str()).into_response()
        }
        Err(_) => refused("the app's callback is not a URL"),
    }
}

/// The stamp `by` from now.
fn later(by: jiff::SignedDuration) -> Option<String> {
    jiff::Timestamp::now()
        .checked_add(by)
        .ok()
        .map(crate::clock::stamp)
}

fn random(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::fill(&mut buf[..]);
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, buf)
}

/// `GET /auth/native`: the approval page, after a sign-in when there is no session yet.
pub async fn ask(session: Session, uri: Uri, Query(q): Query<NativeQuery>) -> Response {
    let request = match validate(q) {
        Ok(request) => request,
        Err(why) => return refused(&why),
    };
    let claims = match crate::identity::session::native_claims(&session).await {
        Ok(claims) => claims,
        Err(e) => return session_failure(e),
    };
    let Some(claims) = claims else {
        let here = uri.path_and_query().map_or(NATIVE_PATH, |p| p.as_str());
        let rd: String = url::form_urlencoded::byte_serialize(here.as_bytes()).collect();
        return Redirect::to(&format!("/auth/login?rd={rd}")).into_response();
    };
    let pending = Pending {
        id: random(16),
        sub: claims.sub.clone(),
        request,
    };
    if let Err(e) = session.insert(PENDING_KEY, &pending).await {
        return session_failure(e);
    }
    let to = Url::parse(&pending.request.redirect_uri)
        .ok()
        .and_then(|u| u.port().map(|p| format!("127.0.0.1:{p}")))
        .unwrap_or_default();
    let body = format!(
        "<p><b>{name}</b> on this computer asks to act as <b>{login}</b>. Approving gives it an \
         api key that can do what you can, until it expires in 90 days or you revoke it in \
         Settings. The key goes to the app listening on {to}.</p>\
         <form method=\"post\" action=\"{NATIVE_PATH}\">\
         <input type=\"hidden\" name=\"id\" value=\"{id}\">\
         <button name=\"decision\" value=\"approve\">Approve</button> \
         <button name=\"decision\" value=\"deny\">Deny</button></form>",
        name = escape(&pending.request.name),
        login = escape(&claims.login),
        to = escape(&to),
        id = escape(&pending.id),
    );
    form_page(StatusCode::OK, "Sign in an app", &body)
}

#[derive(Debug, Deserialize)]
pub struct Decision {
    id: String,
    decision: String,
}

/// `POST /auth/native`: the person's answer. Approval records a one-minute code and sends the
/// browser back to the app with it.
pub async fn decide(
    State(pool): State<sqlx::PgPool>,
    session: Session,
    Form(form): Form<Decision>,
) -> Response {
    let pending: Option<Pending> = match session.remove(PENDING_KEY).await {
        Ok(pending) => pending,
        Err(e) => return session_failure(e),
    };
    let Some(pending) = pending.filter(|p| p.id == form.id) else {
        return refused("this sign-in request is over; start it again from the app");
    };
    let claims = match crate::identity::session::native_claims(&session).await {
        Ok(claims) => claims,
        Err(e) => return session_failure(e),
    };
    if claims.as_ref().map(|c| c.sub.as_str()) != Some(pending.sub.as_str()) {
        return refused("the browser is no longer signed in as the person who was asked");
    }
    let request = pending.request;
    if form.decision != "approve" {
        return back_to_app(
            &request.redirect_uri,
            &[("error", "access_denied"), ("state", &request.state)],
        );
    }
    let code = random(32);
    let Some(expires) = later(CODE_LIFETIME) else {
        return refused("the code's expiry could not be computed; try again");
    };
    let stored = sqlx::query(
        "INSERT INTO native_sign_in_codes (code_hash, sub, name, redirect_uri, challenge, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(crate::identity::api_key::digest(&code))
    .bind(&pending.sub)
    .bind(&request.name)
    .bind(&request.redirect_uri)
    .bind(&request.challenge)
    .bind(expires)
    .execute(&pool)
    .await;
    if let Err(e) = stored {
        tracing::warn!(error = %e, "recording a native sign-in code");
        return refused("the approval could not be recorded; try again");
    }
    back_to_app(
        &request.redirect_uri,
        &[("code", &code), ("state", &request.state)],
    )
}

#[derive(Debug, Deserialize)]
pub struct Redeem {
    code: String,
    code_verifier: String,
    redirect_uri: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Issued {
    pub key: String,
    pub id: String,
    pub name: String,
    pub expires_at: Option<String>,
}

fn invalid_grant(why: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({"error": "invalid_grant", "error_description": why})),
    )
        .into_response()
}

/// `POST /auth/native/token`: the code and the verifier for the key. The code is deleted as it
/// is read, so it buys one key at most, right or wrong.
pub async fn redeem(State(pool): State<sqlx::PgPool>, Json(body): Json<Redeem>) -> Response {
    let row = sqlx::query(
        "DELETE FROM native_sign_in_codes WHERE code_hash = $1
         RETURNING sub, name, redirect_uri, challenge, expires_at",
    )
    .bind(crate::identity::api_key::digest(&body.code))
    .fetch_optional(&pool)
    .await;
    let row = match row {
        Ok(Some(row)) => row,
        Ok(None) => return invalid_grant("the code is unknown or already used"),
        Err(e) => {
            tracing::warn!(error = %e, "redeeming a native sign-in code");
            return invalid_grant("the code could not be read");
        }
    };
    let field = |name: &str| row.try_get::<String, _>(name).unwrap_or_default();
    if field("expires_at") <= crate::clock::now_rfc3339() {
        return invalid_grant("the code expired");
    }
    if field("redirect_uri") != body.redirect_uri {
        return invalid_grant("redirect_uri is not the one the code was issued to");
    }
    let verified = CodeChallenge::parse(&field("challenge"))
        .and_then(|challenge| challenge.verify(&body.code_verifier));
    if let Err(e) = verified {
        return invalid_grant(&e.to_string());
    }
    let expires = later(KEY_LIFETIME);
    match crate::identity::api_key::mint(&pool, &field("sub"), &field("name"), expires.as_deref())
        .await
    {
        Ok(minted) => (
            StatusCode::OK,
            [(header::CACHE_CONTROL, "no-store")],
            Json(Issued {
                key: minted.secret,
                id: minted.key.id,
                name: minted.key.name,
                expires_at: minted.key.expires_at,
            }),
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "minting a native sign-in key");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "server_error"})),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::identity::native_sign_in::{NativeQuery, validate};

    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    fn query(redirect: &str) -> NativeQuery {
        NativeQuery {
            redirect_uri: Some(redirect.to_string()),
            state: Some("s1".into()),
            code_challenge: Some(CHALLENGE.into()),
            code_challenge_method: Some("S256".into()),
            name: None,
        }
    }

    #[test]
    fn only_a_loopback_callback_with_s256_is_accepted() {
        let ok = validate(query("http://127.0.0.1:53111/callback")).unwrap();
        assert_eq!(ok.name, "Crucible Playbooks");
        assert_eq!(ok.state, "s1");
        for bad in [
            "http://127.0.0.1/callback",
            "http://127.0.0.1:0/callback",
            "http://localhost:53111/callback",
            "http://127.0.0.1:53111/other",
            "http://127.0.0.1:53111/callback?x=1",
            "https://evil.example.com/callback",
            "http://10.0.0.1:53111/callback",
            "not a url",
        ] {
            assert!(validate(query(bad)).is_err(), "{bad}");
        }
        let mut plain = query("http://127.0.0.1:53111/callback");
        plain.code_challenge_method = Some("plain".into());
        assert!(validate(plain).is_err());
        let mut short = query("http://127.0.0.1:53111/callback");
        short.code_challenge = Some("abc".into());
        assert!(validate(short).is_err());
        let mut no_state = query("http://127.0.0.1:53111/callback");
        no_state.state = None;
        assert!(validate(no_state).is_err());
        let mut named = query("http://127.0.0.1:53111/callback");
        named.name = Some("  my laptop  ".into());
        assert_eq!(validate(named).unwrap().name, "my laptop");
        let mut control = query("http://127.0.0.1:53111/callback");
        control.name = Some("a\u{7}b".into());
        assert!(validate(control).is_err());
    }
}
