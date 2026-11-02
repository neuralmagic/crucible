//! Fixtures the test modules share: a scratch dir, parsed args, a workflow, a manifest file, and
//! the one lock every test that mutates the process environment takes.

use crate::args::Args;
use clap::Parser;
use std::fs;
use std::path::PathBuf;

pub(crate) fn manifest_toml(effort_line: &str) -> String {
    format!(
        r#"
        [repo]
        path = "."
        [judge]
        measure_cmd = "m"
        direction = "higher"
        objective = "v"
        [agent]
        backend = "command"
        agent_cmd = "a"
        goal = "g"
        {effort_line}
    "#
    )
}

pub(crate) fn args_from(argv: &[&str]) -> Args {
    crate::cli::Cli::parse_from(argv).run
}

pub(crate) fn tempdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "crucible-run-test-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    fs::create_dir_all(&dir).expect("mkdir tmp");
    dir
}

/// One request off `s`, answered `200`: its request line, its headers lowercased, its body.
pub(crate) fn read_one_request(s: &mut std::net::TcpStream) -> (String, String, Vec<u8>) {
    answer_one_request(s, "200 OK")
}

/// One request off `s`, answered with `status` (e.g. `"503 Service Unavailable"`): its request
/// line, its headers lowercased, its body.
pub(crate) fn answer_one_request(
    s: &mut std::net::TcpStream,
    status: &str,
) -> (String, String, Vec<u8>) {
    use std::io::{Read, Write};
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let (head_end, len) = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).to_ascii_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            if buf.len() >= i + 4 + len {
                break (i, len);
            }
        }
        let n = s.read(&mut chunk).expect("read");
        assert!(n > 0, "the client hung up mid-request");
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let (line, headers) = head.split_once("\r\n").unwrap_or((head.as_str(), ""));
    let body = buf[head_end + 4..head_end + 4 + len].to_vec();
    s.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").as_bytes())
        .expect("respond");
    let _ = s.flush();
    (line.to_string(), headers.to_ascii_lowercase(), body)
}
