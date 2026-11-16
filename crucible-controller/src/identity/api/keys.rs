//! The caller's own API keys: mint one, list them, take one back.
//!
//! Every route here needs an issuer subject, because a key hangs off a `users` row and a credential
//! that names nobody the controller has seen sign in has no row to hang from.
//!
//! None of them may be reached with an API key. A key that can mint keys is a key that outlives its
//! own revocation: revoke it and whatever it minted still works. Minting stays a thing a person
//! does in a browser, once, deliberately.

use crate::api::dto::{bad_request, forbidden, not_found};
use crate::api::state::{ApiResult, ApiState, ErrorBody, Json};
use crate::identity::api_key::ApiKey;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The default lifetime the mint form offers. A key meant to outlive a quarter has to ask.
const DEFAULT_EXPIRY_DAYS: i64 = 90;
/// The longest name worth storing; past this the caller is pasting, not naming.
const MAX_NAME: usize = 120;

/// What a mint asks for.
#[derive(Debug, Deserialize, ToSchema)]
pub struct MintBody {
    /// What to call it, so its owner can tell two keys apart later.
    pub name: String,
    /// Days until it expires. Omitted is [`DEFAULT_EXPIRY_DAYS`]; an explicit `null` never expires.
    #[serde(default, deserialize_with = "expiry_days")]
    pub expires_in_days: Option<Option<i64>>,
}

/// Distinguishes "the field was absent" from "the field was `null`", which mean different
/// lifetimes here and would otherwise collapse into the same `None`.
fn expiry_days<'de, D>(d: D) -> Result<Option<Option<i64>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<i64>::deserialize(d).map(Some)
}

/// A freshly minted key. The only response that ever carries `secret`.
#[derive(Debug, Serialize, ToSchema)]
pub struct MintedDto {
    #[serde(flatten)]
    pub key: ApiKey,
    /// The full `crk_…` credential. Shown once — the controller keeps only its digest and cannot
    /// show it again.
    pub secret: String,
    /// Where to point an MCP client, so the page can show a command that works rather than one
    /// with a hole in it. Null on a deployment that exposes no MCP route; the SPA's own origin is
    /// not a usable guess, because `/mcp` rides the machine Route on its own hostname.
    pub mcp_url: Option<String>,
}

fn no_subject() -> Response {
    forbidden("this credential names no issuer subject, so it holds no api keys")
}

/// A key may not manage keys. See the module note.
fn not_from_a_key() -> Response {
    forbidden("api keys cannot manage api keys; sign in to mint or revoke one")
}

#[utoipa::path(
    get,
    path = "/api/keys",
    responses(
        (status = 200, description = "Every api key the caller holds", body = Vec<ApiKey>),
        (status = 403, description = "The caller has no issuer subject, or presented a key", body = ErrorBody)
    )
)]
pub(crate) async fn list_keys(
    State(state): State<ApiState>,
    subject: crate::identity::auth::Subject,
    path: crate::identity::auth::AuthPath,
) -> ApiResult<Response> {
    if path == crate::identity::auth::AuthPath::ApiKey {
        return Ok(not_from_a_key());
    }
    let Some(sub) = subject.as_deref() else {
        return Ok(no_subject());
    };
    Ok(Json(crate::identity::api_key::list(state.db.pool(), sub).await?).into_response())
}

#[utoipa::path(
    post,
    path = "/api/keys",
    request_body = MintBody,
    responses(
        (status = 201, description = "The minted key, including its one-time secret", body = MintedDto),
        (status = 400, description = "The name is empty or too long, or the lifetime is under a day or past the year 9999", body = ErrorBody),
        (status = 403, description = "The caller has no issuer subject, or presented a key", body = ErrorBody)
    )
)]
pub(crate) async fn mint_key(
    State(state): State<ApiState>,
    subject: crate::identity::auth::Subject,
    path: crate::identity::auth::AuthPath,
    Json(body): Json<MintBody>,
) -> ApiResult<Response> {
    if path == crate::identity::auth::AuthPath::ApiKey {
        return Ok(not_from_a_key());
    }
    let Some(sub) = subject.as_deref() else {
        return Ok(no_subject());
    };
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME {
        return Ok(bad_request(format!(
            "an api key needs a name of 1 to {MAX_NAME} characters"
        )));
    }
    let days = match body.expires_in_days {
        None => Some(DEFAULT_EXPIRY_DAYS),
        Some(explicit) => explicit,
    };
    let expires_at = match days {
        None => None,
        Some(days) if days < 1 => {
            return Ok(bad_request(
                "an api key's lifetime must be at least a day; send null to never expire",
            ));
        }
        Some(days) => match expiry_stamp(jiff::Timestamp::now(), days) {
            Some(at) => Some(at),
            None => {
                return Ok(bad_request(
                    "an api key's lifetime cannot run past the year 9999; send null to never expire",
                ));
            }
        },
    };
    let minted =
        crate::identity::api_key::mint(state.db.pool(), sub, name, expires_at.as_deref()).await?;
    Ok((
        StatusCode::CREATED,
        Json(MintedDto {
            key: minted.key,
            secret: minted.secret,
            mcp_url: configured_mcp_url(),
        }),
    )
        .into_response())
}

/// `CONTROLLER_MCP_URL`: where the hosted MCP surface answers, when the deployment serves it on a
/// hostname of its own.
pub(crate) fn configured_mcp_url() -> Option<String> {
    std::env::var("CONTROLLER_MCP_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
}

/// `now + days` as the schema's stamp, or `None` past the last instant a stamp can hold.
fn expiry_stamp(now: jiff::Timestamp, days: i64) -> Option<String> {
    let span = jiff::Span::new().try_days(days).ok()?;
    let at = now
        .to_zoned(jiff::tz::TimeZone::UTC)
        .checked_add(span)
        .ok()?;
    Some(crate::clock::stamp(at.timestamp()))
}

#[utoipa::path(
    delete,
    path = "/api/keys/{id}",
    params(("id" = String, Path, description = "The key's public id")),
    responses(
        (status = 204, description = "The key is revoked"),
        (status = 404, description = "The caller holds no live key with that id", body = ErrorBody),
        (status = 403, description = "The caller has no issuer subject, or presented a key", body = ErrorBody)
    )
)]
pub(crate) async fn revoke_key(
    State(state): State<ApiState>,
    subject: crate::identity::auth::Subject,
    path: crate::identity::auth::AuthPath,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    if path == crate::identity::auth::AuthPath::ApiKey {
        return Ok(not_from_a_key());
    }
    let Some(sub) = subject.as_deref() else {
        return Ok(no_subject());
    };
    match crate::identity::api_key::revoke(state.db.pool(), sub, &id).await? {
        true => Ok(StatusCode::NO_CONTENT.into_response()),
        false => Ok(not_found("no live api key of yours has that id")),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use serde_json::{Value, json};
    use sqlx::PgPool;

    use crate::api::state::ApiState;
    use crate::identity::auth::AuthPath;
    use crate::identity::session::Resolved;

    const SUB: &str = "sub-alice";
    const DAY: i64 = 86_400;

    async fn app(pool: &PgPool, path: AuthPath) -> axum::Router {
        crate::identity::oidc::users::record_login(
            pool,
            SUB,
            "alice",
            None,
            jiff::Timestamp::now(),
        )
        .await
        .unwrap();
        let resolved = Resolved {
            user: Some("alice".to_string()),
            groups: Vec::new(),
            sub: Some(SUB.to_string()),
        };
        crate::api::router(ApiState::test(crate::Db::new(pool.clone()))).layer(
            axum::middleware::from_fn(
                move |mut req: axum::extract::Request, next: axum::middleware::Next| {
                    let resolved = resolved.clone();
                    async move {
                        req.extensions_mut().insert(path);
                        req.extensions_mut().insert(resolved);
                        next.run(req).await
                    }
                },
            ),
        )
    }

    async fn mint(app: &axum::Router, body: Value) -> (StatusCode, Value) {
        let req = Request::post("/api/keys")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let (status, bytes) = crate::testing::oneshot_bytes(app, req).await;
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| panic!("not json: {}", String::from_utf8_lossy(&bytes)));
        (status, body)
    }

    fn seconds(stamp: &Value) -> i64 {
        let stamp = stamp.as_str().unwrap();
        assert_eq!(stamp.len(), "2026-11-16T00:00:00Z".len(), "{stamp}");
        stamp.parse::<jiff::Timestamp>().unwrap().as_second()
    }

    #[test]
    fn an_expiry_is_whole_utc_days_from_now_in_the_schema_stamp() {
        let at =
            |now: &str, days| crate::identity::api::keys::expiry_stamp(now.parse().unwrap(), days);
        assert_eq!(
            at("2026-11-16T12:30:45.5Z", 1).as_deref(),
            Some("2026-11-17T12:30:45Z")
        );
        assert_eq!(
            at("2028-02-28T00:00:00Z", 2).as_deref(),
            Some("2028-03-01T00:00:00Z")
        );
        assert_eq!(
            at("2026-11-16T00:00:00Z", 90).as_deref(),
            Some("2027-02-14T00:00:00Z")
        );
        assert_eq!(at("2026-11-16T00:00:00Z", 4_000_000), None);
        assert_eq!(at("2026-11-16T00:00:00Z", i64::MAX), None);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_mint_expires_the_asked_days_out_ninety_by_default_and_never_on_null(pool: PgPool) {
        let app = app(&pool, AuthPath::Session).await;
        for (body, days) in [
            (json!({"name": "ci", "expires_in_days": 30}), Some(30)),
            (json!({"name": "ci", "expires_in_days": 1}), Some(1)),
            (json!({"name": "ci"}), Some(90)),
            (json!({"name": "ci", "expires_in_days": null}), None),
        ] {
            let before = jiff::Timestamp::now().as_second();
            let (status, minted) = mint(&app, body.clone()).await;
            let after = jiff::Timestamp::now().as_second();
            assert_eq!(status, StatusCode::CREATED, "{body}: {minted}");
            match days {
                Some(days) => {
                    let at = seconds(&minted["expires_at"]);
                    assert!(
                        (before + days * DAY..=after + days * DAY).contains(&at),
                        "{body}: {minted}"
                    );
                }
                None => assert_eq!(minted["expires_at"], Value::Null, "{body}"),
            }
            let who = crate::identity::api_key::verify(&pool, minted["secret"].as_str().unwrap())
                .await
                .unwrap();
            assert_eq!(who.sub, SUB, "{body}");
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_lifetime_under_a_day_or_past_the_calendar_is_a_bad_request_and_mints_nothing(
        pool: PgPool,
    ) {
        let app = app(&pool, AuthPath::Session).await;
        for days in [0, -1, i64::MIN, 4_000_000, i64::MAX] {
            let (status, body) = mint(&app, json!({"name": "ci", "expires_in_days": days})).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{days}: {body}");
            let error = body["error"].as_str().unwrap();
            assert!(error.contains("lifetime"), "{days}: {error}");
            assert!(
                !error.contains("jiff") && !error.contains("unit"),
                "{days}: {error}"
            );
        }
        assert!(
            crate::identity::api_key::list(&pool, SUB)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_failed_query_answers_a_fixed_500_that_names_nothing_internal(pool: PgPool) {
        let app = app(&pool, AuthPath::Session).await;
        sqlx::query("DROP TABLE api_keys CASCADE")
            .execute(&pool)
            .await
            .unwrap();
        for req in [
            Request::get("/api/keys").body(Body::empty()).unwrap(),
            Request::post("/api/keys")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({"name": "ci"}).to_string()))
                .unwrap(),
        ] {
            let (status, bytes) = crate::testing::oneshot_bytes(&app, req).await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(&bytes[..], br#"{"error":"internal error"}"#);
        }
    }
}
