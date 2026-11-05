//! `buildit`'s logs on a stderr that is no terminal, the way a loop pod and the run log read them.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn logs_written_to_a_pipe_carry_no_ansi_escapes() {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "t0k3n").unwrap();
    let mut cache = Running(
        Command::new(env!("CARGO_BIN_EXE_buildit"))
            .args(["cache", "--bind", "127.0.0.1:0", "--dir"])
            .arg(dir.path().join("blobs"))
            .arg("--token-file")
            .arg(&token)
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stderr = cache.0.stderr.take().unwrap();
    let line = BufReader::new(stderr)
        .lines()
        .map(Result::unwrap)
        .find(|line| line.contains("buildit cache listening"))
        .expect("the cache logs that it listens");
    assert!(line.contains("INFO"), "{line:?}");
    assert!(!line.contains('\x1b'), "{line:?}");
}
