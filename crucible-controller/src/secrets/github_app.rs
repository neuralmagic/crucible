//! GitHub App installation-token minting for the pack-PR credential (the `AUTORESEARCH_PR_TOKEN`
//! replacement). The org's token policy forbids long-lived classic PATs, so the controller
//! authenticates its pack-branch pushes + `gh pr create` as the org's GitHub App instead:
//! sign a short-lived RS256 JWT with the App's private key (iss = app id), exchange it at
//! `POST /app/installations/{id}/access_tokens` for an installation token (`ghs_…`, 1h lifetime),
//! and cache that in-process until ~5 minutes before expiry.
//!
//! Installation tokens are documented to work both as the git-over-HTTPS password
//! (`https://x-access-token:<token>@github.com/…`) and as `GH_TOKEN` for the `gh` CLI, so one
//! mint covers both halves of `engine::open_pack_pr`.
//!
//! Configured by three env vars (all-or-nothing, see [`GithubAppTokenSource::from_env`]):
//! `CONTROLLER_GITHUB_APP_ID`, `CONTROLLER_GITHUB_APP_INSTALLATION_ID`, and
//! `CONTROLLER_GITHUB_APP_KEY_PATH` (the PEM the chart mounts from a secret). The handle is
//! threaded onto [`crate::config::ControllerCfg`] at startup — the same `#[arg(skip)]` shape as
//! the autopilot flag — so every `cfg.clone()` shares the one token cache.

#![allow(clippy::disallowed_macros)]

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Re-mint when the cached token has less than this long left: GitHub mints 1h tokens, and a
/// token must outlive the whole push + `gh pr create` sequence it's handed to.
const REFRESH_MARGIN_SECS: i64 = 300;
/// The app JWT's lifetime. GitHub caps it at 10 minutes; 9 leaves drift headroom.
const JWT_TTL_SECS: i64 = 540;
const _: () = assert!(
    JWT_TTL_SECS <= 600,
    "GitHub rejects app JWTs living past 10 minutes"
);
/// Backdate `iat` so a slightly-fast local clock doesn't produce a JWT GitHub sees as
/// issued-in-the-future (GitHub's own recommendation).
const JWT_IAT_BACKDATE_SECS: i64 = 60;

const DEFAULT_API_BASE: &str = "https://api.github.com";

/// The app JWT's claim set (GitHub ignores everything else).
#[derive(Debug, Serialize, Deserialize)]
struct AppJwtClaims {
    iat: i64,
    exp: i64,
    /// The App ID. GitHub accepts it as a string or a number; string round-trips the env var.
    iss: String,
}

/// `POST /app/installations/{id}/access_tokens` response (the fields we use).
#[derive(Debug, Deserialize)]
struct InstallationTokenResponse {
    token: String,
    /// RFC 3339, e.g. `2026-07-05T22:14:10Z`.
    expires_at: String,
}

/// `GET /app` response (the field we use). The slug is the App's URL name, and the bot account
/// GitHub attributes its writes to is that slug with a `[bot]` suffix.
#[derive(Debug, Deserialize)]
struct AppResponse {
    slug: String,
}

/// `GET /users/{login}` response (the field we use).
#[derive(Debug, Deserialize)]
struct UserResponse {
    id: u64,
}

/// The bot account GitHub credits the App's writes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotIdentity {
    pub name: String,
    pub email: String,
}

impl BotIdentity {
    /// The `<id>+<login>@users.noreply.github.com` form GitHub links back to the account.
    fn new(login: String, user_id: u64) -> Self {
        Self {
            email: format!("{user_id}+{login}@users.noreply.github.com"),
            name: login,
        }
    }
}

#[derive(Debug, Clone)]
struct CachedToken {
    token: String,
    expires_at: Timestamp,
}

/// Per-entry expiry: a minted token stays cached until [`REFRESH_MARGIN_SECS`] before its own
/// wall-clock `expires_at`, then the next read re-mints.
struct TokenExpiry;

impl moka::Expiry<(), CachedToken> for TokenExpiry {
    fn expire_after_create(
        &self,
        _key: &(),
        value: &CachedToken,
        _created_at: Instant,
    ) -> Option<Duration> {
        Some(refresh_after(value.expires_at, Timestamp::now()))
    }

    fn expire_after_update(
        &self,
        _key: &(),
        value: &CachedToken,
        _updated_at: Instant,
        _duration_until_expiry: Option<Duration>,
    ) -> Option<Duration> {
        Some(refresh_after(value.expires_at, Timestamp::now()))
    }
}

/// A cached, self-refreshing source of GitHub App installation tokens. Cloning shares the cache
/// (a moka handle is an `Arc` inside), so the one instance threaded onto the cfg serves every
/// reconcile pass.
#[derive(Debug, Clone)]
pub struct GithubAppTokenSource {
    app_id: String,
    installation_id: String,
    key_path: PathBuf,
    api_base: String,
    http: reqwest::Client,
    /// A single-slot cache (unit key). `try_get_with` coalesces concurrent mints so parallel
    /// callers never stampede GitHub, and a failed mint is not cached.
    cache: moka::future::Cache<(), CachedToken>,
    /// The App's bot account, resolved once. A failed resolve is not stored.
    identity: std::sync::Arc<tokio::sync::OnceCell<BotIdentity>>,
}

impl GithubAppTokenSource {
    /// Build from explicit parts (tests point `api_base` at a wiremock server).
    pub(crate) fn new(
        app_id: impl Into<String>,
        installation_id: impl Into<String>,
        key_path: impl Into<PathBuf>,
        api_base: impl Into<String>,
    ) -> Self {
        Self {
            app_id: app_id.into(),
            installation_id: installation_id.into(),
            key_path: key_path.into(),
            api_base: api_base.into(),
            http: reqwest::Client::new(),
            cache: moka::future::Cache::builder()
                .expire_after(TokenExpiry)
                .build(),
            identity: std::sync::Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    /// Build from the three `CONTROLLER_GITHUB_APP_*` env vars. All unset ⇒ `Ok(None)` (the app
    /// path is off, the PAT chain applies); partially set ⇒ an error, so a half-configured deploy
    /// fails loudly at startup instead of silently falling back to a PAT that's about to expire.
    /// Honors `GITHUB_API_URL` for the API base (the `triage`/`approvals` convention).
    pub fn from_env() -> Result<Option<Self>> {
        let get = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let parts = (
            get("CONTROLLER_GITHUB_APP_ID"),
            get("CONTROLLER_GITHUB_APP_INSTALLATION_ID"),
            get("CONTROLLER_GITHUB_APP_KEY_PATH"),
        );
        let (app_id, installation_id, key_path) = match parts {
            (None, None, None) => return Ok(None),
            (Some(a), Some(i), Some(k)) => (a, i, k),
            (a, i, k) => {
                let missing: Vec<&str> = [
                    (a.is_none(), "CONTROLLER_GITHUB_APP_ID"),
                    (i.is_none(), "CONTROLLER_GITHUB_APP_INSTALLATION_ID"),
                    (k.is_none(), "CONTROLLER_GITHUB_APP_KEY_PATH"),
                ]
                .into_iter()
                .filter_map(|(gone, name)| gone.then_some(name))
                .collect();
                bail!(
                    "GitHub App auth is partially configured: missing {} (set all three \
                     CONTROLLER_GITHUB_APP_* vars, or none to use the PAT chain)",
                    missing.join(", ")
                );
            }
        };
        let api_base = get("GITHUB_API_URL").unwrap_or_else(|| DEFAULT_API_BASE.to_string());
        Ok(Some(Self::new(app_id, installation_id, key_path, api_base)))
    }

    /// The current installation token: served from the cache while it has more than
    /// [`REFRESH_MARGIN_SECS`] left (its per-entry expiry), re-minted otherwise. `try_get_with`
    /// coalesces concurrent mints, so parallel callers never stampede GitHub.
    pub(crate) async fn token(&self) -> Result<String> {
        let now = Timestamp::now();
        let this = self.clone();
        let minted = self
            .cache
            .try_get_with((), async move { this.mint(now).await })
            .await
            .map_err(|e| anyhow::anyhow!("{e:#}"))?;
        Ok(minted.token)
    }

    /// The App's bot account as a git author: its slug, then the `<slug>[bot]` account's id.
    pub async fn identity(&self) -> Result<BotIdentity> {
        self.identity
            .get_or_try_init(|| async {
                let jwt = app_jwt(&self.app_id, &self.read_key()?, Timestamp::now())?;
                let app: AppResponse = self
                    .get("/app", &jwt)
                    .await
                    .context("resolving the App's bot identity")?;
                let login = format!("{}[bot]", app.slug);
                // The installation token, not the app JWT: `/users` is not an app-JWT route.
                let token = self.token().await?;
                let user: UserResponse = self
                    .get(&format!("/users/{}", encode_segment(&login)), &token)
                    .await
                    .with_context(|| format!("resolving the numeric id of {login}"))?;
                Ok(BotIdentity::new(login, user.id))
            })
            .await
            .cloned()
    }

    /// The App's private key, read per use so a rotated mount is picked up.
    fn read_key(&self) -> Result<Vec<u8>> {
        std::fs::read(&self.key_path).with_context(|| {
            format!(
                "reading the GitHub App private key at {}",
                self.key_path.display()
            )
        })
    }

    /// One authenticated GET against the API base. `bearer` is an app JWT or an installation
    /// token, whichever the route takes.
    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str, bearer: &str) -> Result<T> {
        self.call(reqwest::Method::GET, path, bearer).await
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        bearer: &str,
    ) -> Result<T> {
        let url = format!("{}{path}", self.api_base.trim_end_matches('/'));
        let resp = self
            .http
            .request(method.clone(), &url)
            .bearer_auth(bearer)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "crucible-controller")
            .send()
            .await
            .with_context(|| format!("{method} {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let body: String = body.chars().take(500).collect();
            bail!("{method} {url} returned {status}: {body}");
        }
        resp.json()
            .await
            .with_context(|| format!("decoding the response to {method} {url}"))
    }

    /// A token for one consumer, bypassing the cache. Lives an hour from issuance.
    pub(crate) async fn token_for_one_consumer(&self) -> Result<String> {
        Ok(self.mint(Timestamp::now()).await?.token)
    }

    /// Sign the app JWT and exchange it for an installation token.
    async fn mint(&self, now: Timestamp) -> Result<CachedToken> {
        let pem = self.read_key().context("minting the installation token")?;
        let jwt = app_jwt(&self.app_id, &pem, now)?;
        let path = format!("/app/installations/{}/access_tokens", self.installation_id);
        let body: InstallationTokenResponse = self
            .call(reqwest::Method::POST, &path, &jwt)
            .await
            .context("minting the installation token")?;
        let expires_at: Timestamp = body.expires_at.parse().with_context(|| {
            format!(
                "minting the installation token: unparseable expires_at {:?}",
                body.expires_at
            )
        })?;
        Ok(CachedToken {
            token: body.token,
            expires_at,
        })
    }
}

/// How long a token expiring at `expires_at` stays worth handing out from `now`: its wall-clock
/// life minus [`REFRESH_MARGIN_SECS`], clamped to zero (already inside the margin ⇒ expire now).
fn refresh_after(expires_at: Timestamp, now: Timestamp) -> Duration {
    let secs = expires_at.as_second() - now.as_second() - REFRESH_MARGIN_SECS;
    Duration::from_secs(secs.max(0) as u64)
}

/// The brackets in a `<slug>[bot]` login are not path-safe.
fn encode_segment(raw: &str) -> String {
    const LOGIN: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
        .add(b'[')
        .add(b']')
        .add(b'/')
        .add(b'?')
        .add(b'#')
        .add(b'%')
        .add(b' ');
    percent_encoding::utf8_percent_encode(raw, LOGIN).to_string()
}

/// The signed app JWT (RS256, iss = app id) — the credential GitHub exchanges for an
/// installation token. Pure so the claim/alg construction is testable against a fixed key.
fn app_jwt(app_id: &str, key_pem: &[u8], now: Timestamp) -> Result<String> {
    // jsonwebtoken can't auto-pick its crypto backend when the dep tree enables both of its
    // provider features (ours does: oci-client turns on rust_crypto, we ask for aws_lc_rs to
    // match the rustls provider the daemon installs) — install one explicitly before the first
    // sign. Err = already installed, which is fine.
    let _ = jsonwebtoken::crypto::aws_lc::DEFAULT_PROVIDER.install_default();
    let claims = AppJwtClaims {
        iat: now.as_second() - JWT_IAT_BACKDATE_SECS,
        exp: now.as_second() + JWT_TTL_SECS,
        iss: app_id.to_string(),
    };
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(key_pem)
        .context("parsing the GitHub App private key (expected the RSA PEM GitHub issues)")?;
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &claims,
        &key,
    )
    .context("signing the GitHub App JWT")
}

/// Test fixtures shared with `engine.rs`'s preference-order test.
#[cfg(test)]
pub(crate) mod testkey {
    use std::path::PathBuf;

    /// A throwaway 2048-bit RSA key in the PKCS#1 PEM shape GitHub's key download uses.
    /// Test-only; it has never signed anything real.
    pub(crate) const TEST_KEY_PEM: &str = "-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEAl74cp2tiwAsF94sGObZVI5IdrYEH0+LobS+iXbpUyap36FWw
rg9ss1egsVAtGi0uSJa8pBjtp1w2e5yT+LinHTr4VTWv2IhjVJ2RQxMUrHvaNy+h
BVobZ9zQTMnwju20EFILPPfbw59Y0rcUZeRt8gssoTLnood0oSeO2uPmpJThfvSC
bbkLrwigFcfSqViWmV3Hm8kYgISv7aiPTnN3b5NDdEkikP0ow8dXWnhfN16wFax8
xMBRNanUXCTVScM5G8tZwzd7F9XhhpZSTK/CuTnXwtXUVaeuNSyCL+BD6N9tOF3e
FWUW1jO18BDP2BWAK6IHlxWkFm6WcKb4B3DR9QIDAQABAoIBABE2TCtvbQr0lSTa
8ltUg9FzmbCHF3tg3IAaFvRDwP9ZD0iILnDSqL//u2dcJ/7f9hqKF/QTLHiUkfeG
hMBXOFmCnrg89+kmBgJglyFrxidkgGr3GPAxpDSCobwRbJsselrJkPhwSE98a90G
+FDbJDTVhfvzpBOrAr85l0+GlLBr/VjWr6ctw4RwlhnsvQXnpAlbrdqjf6OGKi5u
Y2kzmEggADGllHUUCBBHpmE8MKNx+zrWBuTzEnIEFvVg/0juAKX+IkNSnQsm2XwW
YgvZ0PavTF/4tBDN/Ttepnw5M9/8E5VRVZGtNK3VigHK8OADpRIWQlAhkK5AJwlV
S275+CcCgYEA07RJV3s07W4BZj+V0Gn0xGFqFgHgZgxYeQp3IdretC9P8dzJ6iCv
By++uzOegvr4XYUn6G3+lNM8AF7QHykJF4Sg5/Fr1GVv7/gDPvkXi1s3rTZLcbU6
Ec4DjHgp2l+JeQRjXuK+4c5m8g5q7j/WKnvUs/XISYVO0ZMfCXUmmJ8CgYEAt34L
ei4s8f1SO/6PqwrGmyu7FVCC4rYAFm1aPbIPtanq+fkNvxmGI2d4aQ/+a3AqhDHa
UewDpvYoTOTLxDiMNOMOOGWmYKdwfPXuQTS7b0Uopc09UcOAIZIONoEFF41pmRSL
xKPVrZCZgs0P5VyD9wuj62GTHlHYiZCpX2gqSOsCgYEAjtjeiA3Vd8O7Y//RmdB0
3TGSAImBnboE1J+QJSLnFJO8EMnW4IjvMR0xSGWbNmwbvBbGB9p4ZnllyiYvrmbl
AJ54aCkJhkZv0m752br//QMuvUyeeXo8VZk54cWPEA9Y1nR0jKjY/cpkwj2iP2KJ
ox7tNgTJAXrW5SitT5dh1KcCgYBbA5c3zE2Y3lj6zyJ96YNnlkJeqSeywim69hSr
w3WNWzHlOca6wjNJvln4aul8aw97sKqktdd96l1E/rufoZjR5sm36ZukF4lxQh8i
ksBhycEGtI20z67vd9265TYcX5VAS/Oj3svvImkyevpmfwQp9skgyK5LfLdWTL3m
R+mpbwKBgFrUgy4XmY/N5pnt/m7FaLYga3kWUMjQ9V4eqEL3BIDCwdJgcnLAwZ7C
0y/DhUtGAB7uz1v2Y8it0EOHdoTs5Y9OzeQGA8F28zncSIw5uxuRxqfM5pkvGe45
9NOMzQOzwidTiXc6QVzYR3dALSlteigBmehOXGrvCvHhJvxEjCR7
-----END RSA PRIVATE KEY-----
";

    pub(crate) const TEST_PUB_PEM: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAl74cp2tiwAsF94sGObZV
I5IdrYEH0+LobS+iXbpUyap36FWwrg9ss1egsVAtGi0uSJa8pBjtp1w2e5yT+Lin
HTr4VTWv2IhjVJ2RQxMUrHvaNy+hBVobZ9zQTMnwju20EFILPPfbw59Y0rcUZeRt
8gssoTLnood0oSeO2uPmpJThfvSCbbkLrwigFcfSqViWmV3Hm8kYgISv7aiPTnN3
b5NDdEkikP0ow8dXWnhfN16wFax8xMBRNanUXCTVScM5G8tZwzd7F9XhhpZSTK/C
uTnXwtXUVaeuNSyCL+BD6N9tOF3eFWUW1jO18BDP2BWAK6IHlxWkFm6WcKb4B3DR
9QIDAQAB
-----END PUBLIC KEY-----
";

    pub(crate) fn write_test_key(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("app-key.pem");
        std::fs::write(&path, TEST_KEY_PEM).expect("write test key");
        path
    }
}

#[cfg(test)]
mod tests {
    use super::testkey::{TEST_KEY_PEM, TEST_PUB_PEM, write_test_key};
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A wiremock stand-in for `POST /app/installations/{id}/access_tokens`.
    async fn mount_exchange(server: &MockServer, token: &str, expires_at: Timestamp, hits: u64) {
        Mock::given(method("POST"))
            .and(path("/app/installations/99/access_tokens"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "token": token,
                "expires_at": expires_at.to_string(),
            })))
            .expect(hits)
            .mount(server)
            .await;
    }

    /// The two reads [`GithubAppTokenSource::identity`] makes.
    async fn mount_identity(server: &MockServer, slug: &str, user_id: u64, hits: u64) {
        Mock::given(method("GET"))
            .and(path("/app"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "slug": slug,
            })))
            .expect(hits)
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/users/{slug}%5Bbot%5D")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": user_id,
            })))
            .expect(hits)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn the_identity_is_the_bot_account_github_credits_the_app_with() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        let in_an_hour = Timestamp::from_second(Timestamp::now().as_second() + 3600).unwrap();
        mount_exchange(&server, "ghs_minted", in_an_hour, 1).await;
        mount_identity(&server, "crucible-bot", 299632118, 1).await;

        let source =
            GithubAppTokenSource::new("4210340", "99", write_test_key(dir.path()), server.uri());
        let who = source.identity().await.unwrap();
        assert_eq!(who.name, "crucible-bot[bot]");
        assert_eq!(
            who.email,
            "299632118+crucible-bot[bot]@users.noreply.github.com"
        );
        // expect(1) on both routes verifies the second ask does not re-read GitHub.
        assert_eq!(source.identity().await.unwrap(), who);
    }

    /// `/users` is not an app-JWT route, so it must present the installation token.
    #[tokio::test]
    async fn the_bot_lookup_authenticates_with_the_installation_token() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        let in_an_hour = Timestamp::from_second(Timestamp::now().as_second() + 3600).unwrap();
        mount_exchange(&server, "ghs_minted", in_an_hour, 1).await;
        mount_identity(&server, "crucible-bot", 7, 1).await;

        let source =
            GithubAppTokenSource::new("4210340", "99", write_test_key(dir.path()), server.uri());
        source.identity().await.unwrap();

        let requests = server.received_requests().await.unwrap();
        let users = requests
            .iter()
            .find(|r| r.url.path().starts_with("/users/"))
            .expect("the bot lookup");
        assert_eq!(
            users.headers.get("authorization").unwrap(),
            "Bearer ghs_minted"
        );
    }

    /// A failed resolve is not remembered.
    #[tokio::test]
    async fn a_failed_identity_resolve_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        let source =
            GithubAppTokenSource::new("4210340", "99", write_test_key(dir.path()), server.uri());
        assert!(source.identity().await.is_err(), "nothing is mounted yet");

        let in_an_hour = Timestamp::from_second(Timestamp::now().as_second() + 3600).unwrap();
        mount_exchange(&server, "ghs_minted", in_an_hour, 1).await;
        mount_identity(&server, "crucible-bot", 7, 1).await;
        assert_eq!(source.identity().await.unwrap().name, "crucible-bot[bot]");
    }

    #[test]
    fn app_jwt_carries_the_documented_claims_and_alg() {
        let now: Timestamp = "2026-07-05T12:00:00Z".parse().unwrap();
        let jwt = app_jwt("4210340", TEST_KEY_PEM.as_bytes(), now).unwrap();

        let header = jsonwebtoken::decode_header(&jwt).unwrap();
        assert_eq!(header.alg, jsonwebtoken::Algorithm::RS256);

        // Verify the signature with the matching public key (a bad key must fail below), and
        // check the claims GitHub validates: iss = app id, iat backdated, exp ≤ 10 min out.
        let key = jsonwebtoken::DecodingKey::from_rsa_pem(TEST_PUB_PEM.as_bytes()).unwrap();
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        validation.validate_exp = false; // `now` is fixed, not the wall clock
        validation.required_spec_claims.clear();
        let decoded = jsonwebtoken::decode::<AppJwtClaims>(&jwt, &key, &validation).unwrap();
        assert_eq!(decoded.claims.iss, "4210340");
        assert_eq!(decoded.claims.iat, now.as_second() - JWT_IAT_BACKDATE_SECS);
        assert_eq!(decoded.claims.exp, now.as_second() + JWT_TTL_SECS);

        // And the signature is real: a different key must not verify.
        let wrong = jsonwebtoken::DecodingKey::from_secret(b"nope");
        assert!(jsonwebtoken::decode::<AppJwtClaims>(&jwt, &wrong, &validation).is_err());
    }

    #[test]
    fn freshness_margin_is_five_minutes() {
        let now: Timestamp = "2026-07-05T12:00:00Z".parse().unwrap();
        let plus = |secs: i64| Timestamp::from_second(now.as_second() + secs).unwrap();
        let fresh = |secs: i64| !refresh_after(plus(secs), now).is_zero();
        assert!(fresh(3600), "an hour left is fresh");
        assert!(fresh(301), "just over the margin is fresh");
        assert!(!fresh(300), "exactly the margin re-mints");
        assert!(!fresh(60), "inside the margin re-mints");
        assert!(!fresh(-10), "expired re-mints");
    }

    #[tokio::test]
    async fn mints_once_and_serves_the_cache_after() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        let in_an_hour = Timestamp::from_second(Timestamp::now().as_second() + 3600).unwrap();
        mount_exchange(&server, "ghs_minted", in_an_hour, 1).await;

        let source =
            GithubAppTokenSource::new("4210340", "99", write_test_key(dir.path()), server.uri());
        assert_eq!(source.token().await.unwrap(), "ghs_minted");
        assert_eq!(source.token().await.unwrap(), "ghs_minted");
        // wiremock's expect(1) verifies on drop that the second call never hit the server.

        // The exchange must have authenticated with a Bearer app JWT.
        let requests = server.received_requests().await.unwrap();
        let auth = requests[0].headers.get("authorization").unwrap();
        assert!(auth.to_str().unwrap().starts_with("Bearer eyJ"));
    }

    #[tokio::test]
    async fn a_token_inside_the_expiry_margin_is_reminted() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        let in_an_hour = Timestamp::from_second(Timestamp::now().as_second() + 3600).unwrap();
        mount_exchange(&server, "ghs_fresh", in_an_hour, 1).await;

        let source =
            GithubAppTokenSource::new("4210340", "99", write_test_key(dir.path()), server.uri());
        // Pre-seed the cache with a token about to expire (1 min left < the 5 min margin).
        let stale_at = Timestamp::from_second(Timestamp::now().as_second() + 60).unwrap();
        source
            .cache
            .insert(
                (),
                CachedToken {
                    token: "ghs_stale".into(),
                    expires_at: stale_at,
                },
            )
            .await;

        assert_eq!(source.token().await.unwrap(), "ghs_fresh");
    }

    #[tokio::test]
    async fn a_failed_exchange_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/app/installations/99/access_tokens"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad credentials"))
            .mount(&server)
            .await;

        let source =
            GithubAppTokenSource::new("4210340", "99", write_test_key(dir.path()), server.uri());
        let err = format!("{:#}", source.token().await.unwrap_err());
        assert!(err.contains("minting the installation token"), "{err}");
        assert!(err.contains("401"), "{err}");
    }

    #[tokio::test]
    async fn from_env_is_all_or_nothing() {
        let _g = crate::ENV_LOCK.lock().await;
        let vars = [
            "CONTROLLER_GITHUB_APP_ID",
            "CONTROLLER_GITHUB_APP_INSTALLATION_ID",
            "CONTROLLER_GITHUB_APP_KEY_PATH",
            "GITHUB_API_URL",
        ];
        let prior: Vec<_> = vars.iter().map(std::env::var_os).collect();

        for v in vars {
            unsafe {
                std::env::remove_var(v);
            }
        }
        assert!(from_env_unset_is_none());

        // Partial config must fail loudly, naming what's missing.
        unsafe {
            std::env::set_var("CONTROLLER_GITHUB_APP_ID", "4210340");
        }
        let err = format!("{:#}", GithubAppTokenSource::from_env().unwrap_err());
        assert!(err.contains("partially configured"), "{err}");
        assert!(
            err.contains("CONTROLLER_GITHUB_APP_INSTALLATION_ID"),
            "{err}"
        );
        assert!(err.contains("CONTROLLER_GITHUB_APP_KEY_PATH"), "{err}");

        // Fully set builds the source.
        unsafe {
            std::env::set_var("CONTROLLER_GITHUB_APP_INSTALLATION_ID", "99");
        }
        unsafe {
            std::env::set_var("CONTROLLER_GITHUB_APP_KEY_PATH", "/var/run/secrets/x.pem");
        }
        let source = GithubAppTokenSource::from_env().unwrap().unwrap();
        assert_eq!(source.app_id, "4210340");
        assert_eq!(source.installation_id, "99");
        assert_eq!(source.key_path, PathBuf::from("/var/run/secrets/x.pem"));
        assert_eq!(source.api_base, DEFAULT_API_BASE);

        for (v, p) in vars.iter().zip(prior) {
            match p {
                Some(val) => unsafe { std::env::set_var(v, val) },
                None => unsafe { std::env::remove_var(v) },
            }
        }
    }

    fn from_env_unset_is_none() -> bool {
        matches!(GithubAppTokenSource::from_env(), Ok(None))
    }
}
