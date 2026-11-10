//! A real HTTP server on a loopback port, for tests that drive the workers against the wire.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::profile::{Profile, Target};

/// Serve `answer`'s JSON for each request line (`GET /api/whoami HTTP/1.1`), keeping connections
/// alive. Returns the server's URL and how many connections it has accepted.
pub fn serve(
    answer: impl Fn(&str) -> String + Send + Sync + 'static,
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
                while let Some(request) = request(&mut reader) {
                    let body = answer(&request);
                    let sent = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
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

/// The next request's first line, with its headers and body read off the stream.
fn request(reader: &mut impl BufRead) -> Option<String> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut length = 0;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 {
            return None;
        }
        let header = header.trim().to_ascii_lowercase();
        if header.is_empty() {
            break;
        }
        if let Some(n) = header.strip_prefix("content-length:") {
            length = n.trim().parse().ok()?;
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some(line.trim().to_string())
}

/// A profile named `name` at `url`, its key under `keys`.
pub fn target(name: &str, url: &str, keys: &std::path::Path) -> Target {
    let profile = Profile {
        url: url.into(),
        ..Profile::default()
    };
    Target::new(name, profile, &crate::keys::Store::Dir(keys.into()))
}
