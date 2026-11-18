//! Signing in through the browser: the controller's `/auth/native` flow, with a loopback listener
//! for the redirect and PKCE so the key only reaches this process.
//!
//! ```text
//! bind 127.0.0.1:0 ──▶ open <controller>/auth/native?redirect_uri&state&code_challenge
//!   browser: sign in, approve ──▶ GET 127.0.0.1:<port>/callback?code&state
//!   POST /auth/native/token {code, code_verifier} ──▶ key ──▶ the profile's key file
//! ```

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::net::Handle;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use crate::profile::Target;

/// How long the browser has to come back.
const WAIT: Duration = Duration::from_secs(300);
const NAME: &str = "Crucible Playbooks";
/// The longest request line the listener reads.
const REQUEST_LINE: u64 = 8 * 1024;

/// Stops a sign-in that is waiting on the browser, when dropped.
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    /// A handle for the window, and the flag the waiting sign-in reads.
    pub fn new() -> (Cancel, Arc<AtomicBool>) {
        let stop = Arc::new(AtomicBool::new(false));
        (Cancel(Arc::clone(&stop)), stop)
    }
}

impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

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
    /// Something else asked the listener, such as the browser's favicon, or a callback that is
    /// not this sign-in's. The listener keeps waiting.
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
        return Callback::Other;
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
         <body style=\"font:15px system-ui;margin:4rem auto;max-width:32rem\"><p>{}</p>",
        escaped(message)
    );
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Connection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
}

/// `text` as HTML text.
fn escaped(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Wait on `listener` until the browser brings back a code for `state`, `WAIT` passes, or `stop`
/// is set.
fn await_code(listener: &TcpListener, state: &str, stop: &AtomicBool) -> Result<String, String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("listening for the browser: {e}"))?;
    let until = Instant::now() + WAIT;
    while Instant::now() < until {
        if stop.load(Ordering::Relaxed) {
            return Err("the sign-in was cancelled".into());
        }
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
        let _ = BufReader::new((&stream).take(REQUEST_LINE)).read_line(&mut line);
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

/// Trade the code for a key at `target`, the way its other requests go.
fn redeem(
    target: &Target,
    runtime: Result<Handle, String>,
    code: &str,
    verifier: &str,
    port: u16,
) -> Result<String, String> {
    let runtime = runtime?;
    let config = crux::config::Config {
        url: target.url(),
        auth: crux::config::Auth::None,
    };
    let wire = crux::client::HttpTransport::with_client(&config, target.http()?)
        .map_err(|e| format!("{e:#}"))?;
    let client = crux::Client::new(std::sync::Arc::new(wire));
    let body = serde_json::json!({
        "code": code,
        "code_verifier": verifier,
        "redirect_uri": redirect_uri(port),
    });
    let issued: Issued = runtime.block_on(crate::net::within(
        "the sign-in",
        client.post("/auth/native/token", Some(&body)),
    ))?;
    Ok(issued.key)
}

/// Sign in to `target`'s controller through the browser, and keep the key where the profile keeps
/// it. Blocks until the browser comes back or `stop` is set, so not for a thread of `runtime`.
pub fn sign_in(
    target: &Target,
    runtime: Result<Handle, String>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let base = target.url();
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|e| format!("listening for the browser: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("listening for the browser: {e}"))?
        .port();
    let pkce = Pkce::new();
    let state = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 16]>());
    crate::editor::open_url(&authorize_url(&base, port, &state, &pkce.challenge))?;
    let code = await_code(&listener, &state, stop)?;
    let key = redeem(target, runtime, &code, &pkce.verifier, port)?;
    target.save_key(key)
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use sha2::Digest;

    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use crate::signin::{
        Callback, Cancel, Pkce, authorize_url, await_code, escaped, parse_callback,
    };

    /// What the listener at `port` answers `GET path`.
    fn get(port: u16, path: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(stream, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        answer
    }

    #[test]
    fn the_listener_waits_through_strays_for_its_own_callback() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (_cancel, stop) = Cancel::new();
        let waiting = std::thread::spawn(move || await_code(&listener, "s1", &stop));

        assert!(get(port, "/favicon.ico").starts_with("HTTP/1.1 404"));
        assert!(get(port, "/callback?code=evil&state=guess").starts_with("HTTP/1.1 404"));
        assert!(!waiting.is_finished(), "neither ends the sign-in");

        let answer = get(port, "/callback?code=abc&state=s1");
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert_eq!(waiting.join().unwrap(), Ok("abc".to_string()));
    }

    #[test]
    fn a_refusal_is_shown_as_text_and_ends_the_sign_in() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (_cancel, stop) = Cancel::new();
        let waiting = std::thread::spawn(move || await_code(&listener, "s1", &stop));
        let answer = get(
            port,
            "/callback?state=s1&error=%3Cscript%3Ealert(1)%3C/script%3E",
        );
        assert!(answer.starts_with("HTTP/1.1 400"), "{answer}");
        assert!(
            answer.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "{answer}"
        );
        assert!(!answer.contains("<script>"));
        assert!(waiting.join().unwrap().is_err());
        assert_eq!(escaped("a<b & \"c\""), "a&lt;b &amp; &quot;c&quot;");
    }

    #[test]
    fn dropping_the_cancel_handle_stops_the_wait() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let (cancel, stop) = Cancel::new();
        let waiting = std::thread::spawn(move || await_code(&listener, "s1", &stop));
        std::thread::sleep(Duration::from_millis(150));
        assert!(!waiting.is_finished());
        let dropped = Instant::now();
        drop(cancel);
        assert_eq!(
            waiting.join().unwrap(),
            Err("the sign-in was cancelled".to_string())
        );
        assert!(dropped.elapsed() < Duration::from_secs(1));
        let (cancel, stop) = Cancel::new();
        assert!(!stop.load(Ordering::Relaxed));
        drop(cancel);
        assert!(stop.load(Ordering::Relaxed));
    }

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
        assert_eq!(
            parse_callback("GET /callback?code=abc&state=other HTTP/1.1", "s1"),
            Callback::Other,
            "a callback for another sign-in is not this one's answer"
        );
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
