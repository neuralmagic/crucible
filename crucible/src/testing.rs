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

/// A local repository to clone from: `main`, a `feature/x` branch one commit past it, an
/// annotated tag `v1` on `main`, a `refs/pull/7/head` commit no branch holds, and a commit no
/// ref holds at all.
pub(crate) struct GitOrigin {
    pub(crate) path: PathBuf,
    pub(crate) main: String,
    pub(crate) feature: String,
    pub(crate) pull: String,
    pub(crate) dangling: String,
}

pub(crate) fn git_origin(name: &str) -> GitOrigin {
    let path = tempdir(name);
    let git = |args: &[&str]| -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&path)
            .args([
                "-c",
                "user.email=c@l",
                "-c",
                "user.name=c",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_owned()
    };
    git(&["init", "-q"]);
    fs::write(path.join("a.txt"), "main\n").expect("write");
    git(&["add", "-A"]);
    git(&["commit", "-qm", "main"]);
    let main = git(&["rev-parse", "HEAD"]);
    git(&["tag", "-a", "v1", "-m", "v1"]);
    git(&["checkout", "-qb", "feature/x"]);
    fs::write(path.join("a.txt"), "feature\n").expect("write");
    git(&["commit", "-qam", "feature"]);
    let feature = git(&["rev-parse", "HEAD"]);
    let tree = git(&["rev-parse", "HEAD^{tree}"]);
    let pull = git(&["commit-tree", &tree, "-p", &main, "-m", "pull"]);
    git(&["update-ref", "refs/pull/7/head", &pull]);
    let dangling = git(&["commit-tree", &tree, "-p", &main, "-m", "dangling"]);
    git(&["checkout", "-q", "main"]);
    GitOrigin {
        path,
        main,
        feature,
        pull,
        dangling,
    }
}

/// The commit `dir` has checked out, and whether it is detached.
pub(crate) fn checked_out(dir: &std::path::Path) -> (String, bool) {
    let repo = git2::Repository::open(dir).expect("a repository");
    let head = repo
        .head()
        .expect("a head")
        .peel_to_commit()
        .expect("a commit")
        .id()
        .to_string();
    (head, repo.head_detached().expect("head state"))
}
