//! The OAuth device flow: asking for a code, waiting for the person, and renewing tokens.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::forge::Account;
use crate::net::Handle;

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const USER_AGENT: &str = "crucible-playbooks";
const RENEW_BEFORE: i64 = 10 * 60;
const SLOW_DOWN_STEP: u64 = 5;
const TICK: Duration = Duration::from_millis(100);
const MAX_EXPIRES_IN: u64 = 30 * 60;
const MAX_INTERVAL: u64 = 60;

/// What the person types at the forge, and where.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct DeviceCode {
    device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

fn default_interval() -> u64 {
    5
}

impl DeviceCode {
    pub fn page(&self) -> &str {
        &self.verification_uri
    }

    fn checked(mut self, account: &Account) -> Result<DeviceCode, String> {
        let home = format!("https://{}/", account.host());
        if !self.verification_uri.starts_with(&home) {
            return Err(format!(
                "{} sent a sign-in page off its own host: {:?}",
                account.name(),
                self.verification_uri
            ));
        }
        self.expires_in = self.expires_in.min(MAX_EXPIRES_IN);
        self.interval = self.interval.clamp(1, MAX_INTERVAL);
        Ok(self)
    }
}

/// A signed-in forge account's tokens. Never formatted.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub login: String,
    access: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Tokens({}, ..)", self.login)
    }
}

impl Tokens {
    pub(crate) fn access(&self) -> &str {
        &self.access
    }

    pub fn client_id<'a>(&'a self, offered: Option<&'a str>) -> Option<&'a str> {
        self.client_id.as_deref().or(offered)
    }

    fn due(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|at| at - RENEW_BEFORE <= now)
    }

    pub fn renew_at(&self) -> Option<i64> {
        self.expires_at.map(|at| at - RENEW_BEFORE)
    }
}

#[derive(Debug, Deserialize)]
struct TokenAnswer {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    error: Option<String>,
    error_description: Option<String>,
    interval: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
struct Issued {
    access: String,
    refresh: Option<String>,
    expires_at: Option<i64>,
}

#[derive(Debug, PartialEq, Eq)]
enum Poll {
    Pending,
    SlowDown(u64),
    Issued(Issued),
    Failed(String),
}

fn read_answer(answer: TokenAnswer, now: i64) -> Poll {
    match (answer.access_token, answer.error.as_deref()) {
        (Some(access), None) if !access.is_empty() => Poll::Issued(Issued {
            access,
            refresh: answer.refresh_token.filter(|r| !r.is_empty()),
            expires_at: answer.expires_in.map(|secs| now + secs),
        }),
        (_, Some("authorization_pending")) => Poll::Pending,
        (_, Some("slow_down")) => Poll::SlowDown(answer.interval.unwrap_or(SLOW_DOWN_STEP)),
        (_, Some("expired_token")) => Poll::Failed("the code expired; sign in again".into()),
        (_, Some("access_denied")) => Poll::Failed("the sign-in was denied".into()),
        (_, Some(error)) => Poll::Failed(match answer.error_description {
            Some(detail) if !detail.is_empty() => format!("refused: {detail}"),
            _ => format!("refused: {error}"),
        }),
        _ => Poll::Failed("answered without a token".into()),
    }
}

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

pub fn unix_now() -> i64 {
    now()
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("building the HTTP client: {e}"))
}

async fn form<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    url: &str,
    fields: &[(&str, &str)],
    doing: &str,
) -> Result<T, String> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish();
    let resp = http
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
        .send()
        .await
        .map_err(|e| format!("{doing}: {}", e.without_url()))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("{doing}: {}", e.without_url()))?;
    match serde_json::from_str(&text) {
        Ok(answer) => Ok(answer),
        Err(_) if !status.is_success() => Err(format!("{doing}: answered {status}")),
        Err(e) => Err(format!("{doing}: {e}")),
    }
}

#[derive(Deserialize)]
struct Who {
    #[serde(alias = "username")]
    login: String,
}

async fn login_of(
    http: &reqwest::Client,
    account: &Account,
    access: &str,
) -> Result<String, String> {
    let doing = format!("reading the {} account", account.name());
    let resp = http
        .get(account.user_url())
        .bearer_auth(access)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| format!("{doing}: {}", e.without_url()))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("{doing}: answered {status}"));
    }
    resp.json::<Who>()
        .await
        .map(|who| who.login)
        .map_err(|e| format!("{doing}: {}", e.without_url()))
}

pub fn start(runtime: &Handle, account: &Account, client_id: &str) -> Result<DeviceCode, String> {
    let http = client()?;
    let mut fields = vec![("client_id", client_id)];
    if let Some(scope) = account.scope() {
        fields.push(("scope", scope));
    }
    let code: DeviceCode = runtime.block_on(form(
        &http,
        &account.device_url(),
        &fields,
        &format!("asking {} for a sign-in code", account.name()),
    ))?;
    code.checked(account)
}

fn pause(secs: u64, stop: &AtomicBool) -> Result<(), String> {
    let until = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < until {
        if stop.load(Ordering::Relaxed) {
            return Err("the sign-in was cancelled".into());
        }
        std::thread::sleep(TICK);
    }
    Ok(())
}

pub fn finish(
    runtime: &Handle,
    account: &Account,
    client_id: &str,
    code: &DeviceCode,
    stop: &AtomicBool,
) -> Result<Tokens, String> {
    let http = client()?;
    let deadline = now().saturating_add(i64::try_from(code.expires_in).unwrap_or(i64::MAX));
    let mut interval = code.interval.max(1);
    let doing = format!("waiting for {}", account.name());
    loop {
        pause(interval, stop)?;
        if now() > deadline {
            return Err("the code expired; sign in again".into());
        }
        let answer: TokenAnswer = runtime.block_on(form(
            &http,
            &account.token_url(),
            &[
                ("client_id", client_id),
                ("device_code", &code.device_code),
                ("grant_type", DEVICE_GRANT),
            ],
            &doing,
        ))?;
        match read_answer(answer, now()) {
            Poll::Pending => {}
            Poll::SlowDown(next) => interval = next.max(interval + SLOW_DOWN_STEP),
            Poll::Failed(why) => return Err(format!("{}: {why}", account.name())),
            Poll::Issued(issued) => {
                let login = runtime.block_on(login_of(&http, account, &issued.access))?;
                return Ok(Tokens {
                    login,
                    access: issued.access,
                    refresh: issued.refresh,
                    expires_at: issued.expires_at,
                    client_id: Some(client_id.to_string()),
                });
            }
        }
    }
}

pub fn revoke(
    runtime: &Handle,
    account: &Account,
    offered: Option<&str>,
    tokens: &Tokens,
) -> Result<(), String> {
    let Some(url) = account.revoke_url() else {
        return Ok(());
    };
    let Some(client_id) = tokens.client_id(offered) else {
        return Err(format!(
            "no client to revoke the {} token with",
            account.name()
        ));
    };
    let http = client()?;
    let doing = format!("revoking the {} token", account.name());
    runtime
        .block_on(form::<serde_json::Value>(
            &http,
            &url,
            &[("client_id", client_id), ("token", tokens.access())],
            &doing,
        ))
        .map(|_| ())
}

pub fn current(
    runtime: &Handle,
    account: &Account,
    offered: Option<&str>,
    tokens: Tokens,
) -> Result<Tokens, String> {
    if !tokens.due(now()) {
        return Ok(tokens);
    }
    let (Some(refresh), Some(client_id)) = (tokens.refresh.as_deref(), tokens.client_id(offered))
    else {
        return Err(format!(
            "the {} sign-in expired; sign in again",
            account.name()
        ));
    };
    let client_id = client_id.to_string();
    let http = client()?;
    let answer: TokenAnswer = runtime.block_on(form(
        &http,
        &account.token_url(),
        &[
            ("client_id", client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
        ],
        &format!("renewing the {} sign-in", account.name()),
    ))?;
    match read_answer(answer, now()) {
        Poll::Issued(issued) => Ok(Tokens {
            login: tokens.login,
            access: issued.access,
            refresh: issued.refresh.or(tokens.refresh),
            expires_at: issued.expires_at,
            client_id: Some(client_id),
        }),
        Poll::Failed(why) => Err(format!("{}: {why}; sign in again", account.name())),
        Poll::Pending | Poll::SlowDown(_) => {
            Err(format!("{} did not renew the sign-in", account.name()))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::forge::device::*;

    fn answer(json: serde_json::Value) -> TokenAnswer {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn a_token_answer_carries_its_expiry_from_now() {
        let got = read_answer(
            answer(serde_json::json!({
                "access_token": "ghu_a",
                "refresh_token": "ghr_b",
                "expires_in": 28800,
                "refresh_token_expires_in": 15897600,
                "token_type": "bearer",
                "scope": "",
            })),
            1_000,
        );
        assert_eq!(
            got,
            Poll::Issued(Issued {
                access: "ghu_a".into(),
                refresh: Some("ghr_b".into()),
                expires_at: Some(29_800),
            })
        );
        let gitlab = read_answer(
            answer(serde_json::json!({
                "access_token": "gl_a",
                "token_type": "Bearer",
                "expires_in": 7200,
                "refresh_token": "gl_r",
                "created_at": 1_000,
            })),
            1_000,
        );
        assert_eq!(
            gitlab,
            Poll::Issued(Issued {
                access: "gl_a".into(),
                refresh: Some("gl_r".into()),
                expires_at: Some(8_200),
            })
        );
        let lasting = read_answer(answer(serde_json::json!({"access_token": "gho_c"})), 5);
        assert_eq!(
            lasting,
            Poll::Issued(Issued {
                access: "gho_c".into(),
                refresh: None,
                expires_at: None,
            })
        );
    }

    #[test]
    fn device_flow_errors_wait_slow_down_or_end() {
        let pending = answer(serde_json::json!({"error": "authorization_pending"}));
        assert_eq!(read_answer(pending, 0), Poll::Pending);
        let slow = answer(serde_json::json!({"error": "slow_down", "interval": 10}));
        assert_eq!(read_answer(slow, 0), Poll::SlowDown(10));
        let slow = answer(serde_json::json!({"error": "slow_down"}));
        assert_eq!(read_answer(slow, 0), Poll::SlowDown(SLOW_DOWN_STEP));
        for (error, says) in [
            ("expired_token", "expired"),
            ("access_denied", "denied"),
            ("invalid_grant", "invalid_grant"),
        ] {
            match read_answer(answer(serde_json::json!({"error": error})), 0) {
                Poll::Failed(why) => assert!(why.contains(says), "{error}: {why}"),
                other => panic!("{error}: {other:?}"),
            }
        }
        let described = answer(serde_json::json!({
            "error": "device_flow_disabled",
            "error_description": "Device Flow must be explicitly enabled for this App",
        }));
        assert_eq!(
            read_answer(described, 0),
            Poll::Failed("refused: Device Flow must be explicitly enabled for this App".into())
        );
        assert!(matches!(
            read_answer(answer(serde_json::json!({})), 0),
            Poll::Failed(_)
        ));
    }

    #[test]
    fn tokens_renew_ten_minutes_before_they_expire_and_never_format() {
        let tokens = Tokens {
            login: "wren".into(),
            access: "ghu_secret".into(),
            refresh: Some("ghr_secret".into()),
            expires_at: Some(10_000),
            client_id: None,
        };
        assert!(!tokens.due(10_000 - RENEW_BEFORE - 1));
        assert!(tokens.due(10_000 - RENEW_BEFORE));
        assert_eq!(tokens.renew_at(), Some(10_000 - RENEW_BEFORE));
        let lasting = Tokens {
            expires_at: None,
            ..tokens.clone()
        };
        assert!(!lasting.due(i64::MAX));
        let shown = format!("{tokens:?}");
        assert!(!shown.contains("secret"), "{shown}");
        assert_eq!(tokens.client_id(Some("set")), Some("set"));
        assert_eq!(tokens.client_id(None), None);
        let issued = Tokens {
            client_id: Some("issuer".into()),
            ..tokens.clone()
        };
        assert_eq!(issued.client_id(Some("set")), Some("issuer"));
        assert_eq!(issued.client_id(None), Some("issuer"));
        let kept: Tokens = serde_json::from_value(serde_json::json!({
            "login": "wren", "access": "a"
        }))
        .unwrap();
        assert_eq!(kept.client_id(Some("set")), Some("set"));
    }

    #[test]
    fn the_codes_both_forges_send_read() {
        let github: DeviceCode = serde_json::from_value(serde_json::json!({
            "device_code": "3584d83530557fdd1f46af8289938c8ef79f9dc5",
            "user_code": "WDJB-MJHT",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 900,
            "interval": 5,
        }))
        .unwrap();
        assert_eq!(github.page(), "https://github.com/login/device");
        let gitlab: DeviceCode = serde_json::from_value(serde_json::json!({
            "device_code": "GmRhmhcxhwAzkoEqiMEg_DnyEysNkuNhszIySk9eS",
            "user_code": "0A44L90H",
            "verification_uri": "https://gitlab.example.com/oauth/device",
            "verification_uri_complete": "https://gitlab.example.com/oauth/device?user_code=0A44L90H",
            "expires_in": 300,
        }))
        .unwrap();
        assert_eq!(gitlab.page(), "https://gitlab.example.com/oauth/device");
        assert_eq!(gitlab.interval, 5);
    }

    #[test]
    fn a_code_opens_only_the_forges_own_page_and_waits_a_bounded_time() {
        let code = |page: &str, expires: u64, interval: u64| DeviceCode {
            device_code: "d".into(),
            user_code: "U".into(),
            verification_uri: page.into(),
            expires_in: expires,
            interval,
        };
        let gl = Account::Gitlab("gitlab.example.com".into());
        let ok = code("https://gitlab.example.com/oauth/device", u64::MAX, 0)
            .checked(&gl)
            .unwrap();
        assert_eq!(ok.page(), "https://gitlab.example.com/oauth/device");
        assert_eq!(ok.expires_in, MAX_EXPIRES_IN);
        assert_eq!(ok.interval, 1);
        for bad in [
            "https://gitlab.example.com.evil.io/device",
            "http://gitlab.example.com/oauth/device",
            "file:///etc/passwd",
            "https://github.com/login/device",
        ] {
            assert!(code(bad, 60, 5).checked(&gl).is_err(), "{bad}");
        }
        let gh = code("https://github.com/login/device", 900, 600)
            .checked(&Account::Github)
            .unwrap();
        assert_eq!(gh.interval, MAX_INTERVAL);
    }

    #[test]
    fn a_user_answer_names_its_login_on_either_forge() {
        let gh: Who =
            serde_json::from_value(serde_json::json!({"login": "wren", "id": 1})).unwrap();
        let gl: Who =
            serde_json::from_value(serde_json::json!({"username": "wren-gl", "id": 1})).unwrap();
        assert_eq!((gh.login.as_str(), gl.login.as_str()), ("wren", "wren-gl"));
    }

    #[test]
    fn a_cancelled_wait_stops() {
        let stop = AtomicBool::new(true);
        assert_eq!(pause(60, &stop), Err("the sign-in was cancelled".into()));
    }
}
