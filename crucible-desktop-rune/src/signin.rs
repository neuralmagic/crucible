//! Signing in through the browser: the controller's `/auth/native` flow, with a loopback listener
//! for the redirect and PKCE so the key only reaches this process.
//!
//! ```text
//! bind 127.0.0.1:0 ──▶ open <controller>/auth/native?redirect_uri&state&code_challenge
//!   browser: sign in, approve ──▶ GET 127.0.0.1:<port>/callback?code&state
//!   POST /auth/native/token {code, code_verifier} ──▶ key ──▶ crux's config.toml
//! ```

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// How long the browser has to come back.
const WAIT: Duration = Duration::from_secs(300);
const NAME: &str = "Crucible Playbooks";

/// A PKCE verifier and its S256 challenge.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn new() -> Self {
        let verifier = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Pkce {
            verifier,
            challenge,
        }
    }
}

fn redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}/callback")
}

/// Where the browser starts.
pub fn authorize_url(base: &str, port: u16, state: &str, challenge: &str) -> String {
    let query: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("redirect_uri", &redirect_uri(port))
        .append_pair("state", state)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("name", NAME)
        .finish();
    format!("{}/auth/native?{query}", base.trim_end_matches('/'))
}

/// What one request to the listener was.
#[derive(Debug, PartialEq, Eq)]
pub enum Callback {
    Code(String),
    /// The browser came back without a code: the person denied, or the controller refused.
    Refused(String),
    /// Something else asked the listener, such as the browser's favicon.
    Other,
}

/// Read the request line `GET /callback?code=…&state=… HTTP/1.1`, checking `state`.
pub fn parse_callback(request_line: &str, state: &str) -> Callback {
    let target = request_line.split_whitespace().nth(1).unwrap_or_default();
    let Some(query) = target.strip_prefix("/callback?") else {
        return Callback::Other;
    };
    let params: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect();
    if params.get("state").map(String::as_str) != Some(state) {
        return Callback::Refused("the browser answered a different sign-in".into());
    }
    match (params.get("code"), params.get("error")) {
        (Some(code), None) if !code.is_empty() => Callback::Code(code.clone()),
        (_, Some(error)) if error == "access_denied" => {
            Callback::Refused("the sign-in was denied in the browser".into())
        }
        (_, Some(error)) => Callback::Refused(format!("the controller refused: {error}")),
        _ => Callback::Refused("the browser came back without a code".into()),
    }
}

fn answer(stream: &mut TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{NAME}</title>\
         <body style=\"font:15px system-ui;margin:4rem auto;max-width:32rem\"><p>{message}</p>"
    );
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Connection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
}

/// Wait on `listener` until the browser brings back a code for `state`, or `WAIT` passes.
fn await_code(listener: &TcpListener, state: &str) -> Result<String, String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("listening for the browser: {e}"))?;
    let until = Instant::now() + WAIT;
    while Instant::now() < until {
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(format!("listening for the browser: {e}")),
        };
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut line = String::new();
        let _ = BufReader::new(&stream).read_line(&mut line);
        match parse_callback(&line, state) {
            Callback::Code(code) => {
                answer(
                    &mut stream,
                    "200 OK",
                    "Signed in. You can close this tab and go back to Crucible Playbooks.",
                );
                return Ok(code);
            }
            Callback::Refused(why) => {
                answer(&mut stream, "400 Bad Request", &why);
                return Err(why);
            }
            Callback::Other => answer(&mut stream, "404 Not Found", "Not found."),
        }
    }
    Err("the browser did not come back within five minutes".into())
}

#[derive(serde::Deserialize)]
struct Issued {
    key: String,
}

/// Trade the code for a key at `base`.
fn redeem(base: &str, code: &str, verifier: &str, port: u16) -> Result<String, String> {
    let config = crux::config::Config {
        url: base.trim_end_matches('/').to_string(),
        auth: crux::config::Auth::None,
    };
    let client = crux::Client::connect(&config).map_err(|e| format!("{e:#}"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("starting the sign-in client: {e}"))?;
    let body = serde_json::json!({
        "code": code,
        "code_verifier": verifier,
        "redirect_uri": redirect_uri(port),
    });
    let issued: Issued = runtime
        .block_on(client.post("/auth/native/token", Some(&body)))
        .map_err(|e| format!("{e:#}"))?;
    Ok(issued.key)
}

/// Sign in to the controller at `base` through the browser, and keep the key in crux's config so
/// the app and the CLI both use it. Blocks until the browser comes back.
pub fn sign_in(base: &str) -> Result<(), String> {
    let base = base.trim().trim_end_matches('/');
    if !(base.starts_with("https://") || base.starts_with("http://")) {
        return Err("the controller URL starts with https://".into());
    }
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|e| format!("listening for the browser: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("listening for the browser: {e}"))?
        .port();
    let pkce = Pkce::new();
    let state = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 16]>());
    crate::editor::open_url(&authorize_url(base, port, &state, &pkce.challenge))?;
    let code = await_code(&listener, &state)?;
    let key = redeem(base, &code, &pkce.verifier, port)?;
    crux::config::FileConfig {
        url: Some(base.to_string()),
        api_token: Some(key),
    }
    .save(None)
    .map(drop)
    .map_err(|e| format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use sha2::Digest;

    use crate::signin::{Callback, Pkce, authorize_url, parse_callback};

    #[test]
    fn the_challenge_is_the_verifiers_s256() {
        let pkce = Pkce::new();
        assert_eq!(pkce.verifier.len(), 43);
        let want = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(sha2::Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, want);
        assert_ne!(Pkce::new().verifier, pkce.verifier);
    }

    #[test]
    fn the_browser_starts_at_the_controllers_native_sign_in() {
        let url = authorize_url("https://crucible.example.com/", 53111, "s+1", "chal");
        assert_eq!(
            url,
            "https://crucible.example.com/auth/native?redirect_uri=http%3A%2F%2F127.0.0.1%3A53111%2Fcallback&state=s%2B1&code_challenge=chal&code_challenge_method=S256&name=Crucible+Playbooks"
        );
    }

    #[test]
    fn only_a_callback_for_this_sign_in_yields_a_code() {
        assert_eq!(
            parse_callback("GET /callback?code=abc&state=s1 HTTP/1.1\r\n", "s1"),
            Callback::Code("abc".into())
        );
        assert_eq!(
            parse_callback("GET /favicon.ico HTTP/1.1", "s1"),
            Callback::Other
        );
        assert!(matches!(
            parse_callback("GET /callback?code=abc&state=other HTTP/1.1", "s1"),
            Callback::Refused(_)
        ));
        assert_eq!(
            parse_callback("GET /callback?error=access_denied&state=s1 HTTP/1.1", "s1"),
            Callback::Refused("the sign-in was denied in the browser".into())
        );
        assert!(matches!(
            parse_callback("GET /callback?state=s1 HTTP/1.1", "s1"),
            Callback::Refused(_)
        ));
        assert_eq!(parse_callback("", "s1"), Callback::Other);
    }
}
