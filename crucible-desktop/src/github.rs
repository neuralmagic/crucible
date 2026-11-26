//! Signing in to GitHub through the GitHub App's device flow, keeping the user token where the
//! app keeps keys, renewing it before it expires, and linking it to a controller.
//!
//! ```text
//! POST github.com/login/device/code {client_id} ──▶ user_code, verification_uri
//!   person enters the code at verification_uri
//! POST github.com/login/oauth/access_token {device_code} every `interval` ──▶ ghu_ + ghr_
//! PUT <controller>/api/me/github {token} ──▶ the controller records the login
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::net::Handle;
use crate::profile::Target;

const DEVICE_CODE: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN: &str = "https://github.com/login/oauth/access_token";
const USER: &str = "https://api.github.com/user";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const USER_AGENT: &str = "crucible-playbooks";
const RENEW_BEFORE: i64 = 10 * 60;
const SLOW_DOWN_STEP: u64 = 5;
const TICK: Duration = Duration::from_millis(100);

/// What the person types at GitHub, and where.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct DeviceCode {
    device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    expires_in: u64,
    interval: u64,
}

/// A signed-in GitHub account's tokens. Never formatted.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub login: String,
    access: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<i64>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Tokens({}, ..)", self.login)
    }
}

impl Tokens {
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
        (_, Some("access_denied")) => Poll::Failed("the sign-in was denied at GitHub".into()),
        (_, Some(error)) => Poll::Failed(match answer.error_description {
            Some(detail) if !detail.is_empty() => format!("GitHub refused: {detail}"),
            _ => format!("GitHub refused: {error}"),
        }),
        _ => Poll::Failed("GitHub answered without a token".into()),
    }
}

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("building the GitHub client: {e}"))
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
    if !status.is_success() {
        return Err(format!("{doing}: GitHub answered {status}"));
    }
    resp.json()
        .await
        .map_err(|e| format!("{doing}: {}", e.without_url()))
}

#[derive(Deserialize)]
struct Account {
    login: String,
}

async fn login_of(http: &reqwest::Client, access: &str) -> Result<String, String> {
    let doing = "reading the GitHub account";
    let resp = http
        .get(USER)
        .bearer_auth(access)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("{doing}: {}", e.without_url()))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("{doing}: GitHub answered {status}"));
    }
    resp.json::<Account>()
        .await
        .map(|a| a.login)
        .map_err(|e| format!("{doing}: {}", e.without_url()))
}

pub fn start(runtime: &Handle, client_id: &str) -> Result<DeviceCode, String> {
    let http = client()?;
    runtime.block_on(form(
        &http,
        DEVICE_CODE,
        &[("client_id", client_id)],
        "asking GitHub for a sign-in code",
    ))
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
    client_id: &str,
    code: &DeviceCode,
    stop: &AtomicBool,
) -> Result<Tokens, String> {
    let http = client()?;
    let deadline = now() + i64::try_from(code.expires_in).unwrap_or(i64::MAX);
    let mut interval = code.interval.max(1);
    loop {
        pause(interval, stop)?;
        if now() > deadline {
            return Err("the code expired; sign in again".into());
        }
        let answer: TokenAnswer = runtime.block_on(form(
            &http,
            ACCESS_TOKEN,
            &[
                ("client_id", client_id),
                ("device_code", &code.device_code),
                ("grant_type", DEVICE_GRANT),
            ],
            "waiting for GitHub",
        ))?;
        match read_answer(answer, now()) {
            Poll::Pending => {}
            Poll::SlowDown(next) => interval = next.max(interval + SLOW_DOWN_STEP),
            Poll::Failed(why) => return Err(why),
            Poll::Issued(issued) => {
                let login = runtime.block_on(login_of(&http, &issued.access))?;
                return Ok(Tokens {
                    login,
                    access: issued.access,
                    refresh: issued.refresh,
                    expires_at: issued.expires_at,
                });
            }
        }
    }
}

pub fn current(runtime: &Handle, client_id: &str, tokens: Tokens) -> Result<Tokens, String> {
    if !tokens.due(now()) {
        return Ok(tokens);
    }
    let Some(refresh) = tokens.refresh.as_deref() else {
        return Err("the GitHub sign-in expired; sign in again".into());
    };
    let http = client()?;
    let answer: TokenAnswer = runtime.block_on(form(
        &http,
        ACCESS_TOKEN,
        &[
            ("client_id", client_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
        ],
        "renewing the GitHub sign-in",
    ))?;
    match read_answer(answer, now()) {
        Poll::Issued(issued) => Ok(Tokens {
            login: tokens.login,
            access: issued.access,
            refresh: issued.refresh.or(tokens.refresh),
            expires_at: issued.expires_at,
        }),
        Poll::Failed(why) => Err(why),
        Poll::Pending | Poll::SlowDown(_) => Err("GitHub did not renew the sign-in".into()),
    }
}

#[derive(Deserialize)]
struct Linked {
    login: String,
}

fn controller(target: &Target) -> Result<crux::Client, String> {
    let config = target.config()?;
    let wire = crux::client::HttpTransport::with_client(&config, target.http()?)
        .map_err(|e| format!("{e:#}"))?;
    Ok(crux::Client::new(std::sync::Arc::new(wire)))
}

pub fn link(runtime: &Handle, target: &Target, tokens: &Tokens) -> Result<String, String> {
    let client = controller(target)?;
    let body = serde_json::json!({ "token": tokens.access });
    let linked: Linked = runtime.block_on(crate::net::within(
        "linking the GitHub account",
        client.put("/api/me/github", &body),
    ))?;
    Ok(linked.login)
}

pub fn unlink(runtime: &Handle, target: &Target) -> Result<(), String> {
    let client = controller(target)?;
    runtime.block_on(crate::net::within(
        "unlinking the GitHub account",
        client.delete("/api/me/github"),
    ))
}

/// What a sign-in or link thread reports back.
pub enum Event {
    Code(DeviceCode),
    SignedIn(Result<Tokens, String>),
    Linked {
        profile: String,
        answer: Result<String, String>,
        renew_at: Option<i64>,
    },
}

/// The GitHub sign-in as the app shows it.
#[derive(Default)]
pub struct Session {
    pub loaded: bool,
    pub login: Option<String>,
    pub code: Option<DeviceCode>,
    pub busy: bool,
    pub error: Option<String>,
    pub linked: Option<(String, String)>,
    pub next: Option<(String, i64)>,
    pub events: Option<std::sync::mpsc::Receiver<Event>>,
    pub cancel: Option<crate::signin::Cancel>,
}

impl Session {
    pub fn due(&self, profile: &str, now: i64) -> bool {
        if self.login.is_none() || self.busy {
            return false;
        }
        match &self.next {
            Some((at_profile, at)) if at_profile == profile => *at <= now,
            _ => true,
        }
    }
}

pub fn unix_now() -> i64 {
    now()
}

#[cfg(test)]
mod tests {
    use crate::github::*;

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
            (
                "incorrect_client_credentials",
                "incorrect_client_credentials",
            ),
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
            Poll::Failed(
                "GitHub refused: Device Flow must be explicitly enabled for this App".into()
            )
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
    }

    #[test]
    fn the_code_github_sends_reads() {
        let code: DeviceCode = serde_json::from_value(serde_json::json!({
            "device_code": "3584d83530557fdd1f46af8289938c8ef79f9dc5",
            "user_code": "WDJB-MJHT",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 900,
            "interval": 5,
        }))
        .unwrap();
        assert_eq!(code.user_code, "WDJB-MJHT");
        assert_eq!(code.interval, 5);
    }

    #[test]
    fn a_link_is_due_once_per_profile_until_its_renewal_or_retry() {
        let mut session = Session::default();
        assert!(!session.due("local", 0));
        session.login = Some("wren".into());
        assert!(session.due("local", 0));
        session.next = Some(("local".into(), 100));
        assert!(!session.due("local", 99));
        assert!(session.due("local", 100));
        assert!(session.due("cluster", 0));
        session.busy = true;
        assert!(!session.due("cluster", 0));
    }

    #[test]
    fn a_cancelled_wait_stops() {
        let stop = AtomicBool::new(true);
        assert_eq!(pause(60, &stop), Err("the sign-in was cancelled".into()));
    }
}
