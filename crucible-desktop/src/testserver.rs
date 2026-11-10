//! A real HTTP server on a loopback port, for tests that drive the workers against the wire.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::profile::{Profile, Target};

/// A request as the server read it: its first line and its headers, names lowercased.
pub struct Asked {
    pub line: String,
    pub headers: Vec<(String, String)>,
}

impl Asked {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(held, _)| held == name)
            .map(|(_, value)| value.as_str())
    }
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: String,
}

impl Reply {
    pub fn ok(body: String) -> Reply {
        Reply {
            status: 200,
            headers: Vec::new(),
            body,
        }
    }
}

/// Serve `answer`'s JSON for each request line (`GET /api/whoami HTTP/1.1`), keeping connections
/// alive. Returns the server's URL and how many connections it has accepted.
pub fn serve(
    answer: impl Fn(&str) -> String + Send + Sync + 'static,
) -> (String, Arc<AtomicUsize>) {
    serve_replies(move |asked| Reply::ok(answer(&asked.line)))
}

/// [`serve`], with the request's headers to answer from and the status and headers to answer with.
pub fn serve_replies(
    answer: impl Fn(&Asked) -> Reply + Send + Sync + 'static,
) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let accepted = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&accepted);
    let answer = Arc::new(answer);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            counted.fetch_add(1, Ordering::SeqCst);
            let answer = Arc::clone(&answer);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                while let Some(asked) = request(&mut reader) {
                    let reply = answer(&asked);
                    let extra: String = reply
                        .headers
                        .iter()
                        .map(|(name, value)| format!("{name}: {value}\r\n"))
                        .collect();
                    let sent = write!(
                        stream,
                        "HTTP/1.1 {} Answer\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\n\r\n{}",
                        reply.status,
                        reply.body.len(),
                        reply.body
                    );
                    if sent.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (url, accepted)
}

/// The next request, with its body read off the stream.
fn request(reader: &mut impl BufRead) -> Option<Asked> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut length = 0;
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 {
            return None;
        }
        let header = header.trim();
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        let (name, value) = (name.trim().to_ascii_lowercase(), value.trim().to_string());
        if name == "content-length" {
            length = value.parse().ok()?;
        }
        headers.push((name, value));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some(Asked {
        line: line.trim().to_string(),
        headers,
    })
}

/// A profile named `name` at `url`, its key under `keys`.
pub fn target(name: &str, url: &str, keys: &std::path::Path) -> Target {
    let profile = Profile {
        url: url.into(),
        ..Profile::default()
    };
    Target::new(name, profile, &crate::keys::Store::Dir(keys.into()))
}
