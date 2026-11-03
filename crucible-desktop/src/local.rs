use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager};

use crate::State;
use crate::settings::Error;

pub const BOOT_TIMEOUT: Duration = Duration::from_secs(900);

const SUPERVISE: &str = r#"just controller-local "$1" & read -r _; kill -TERM 0"#;

/// Start `just controller-local <port>` unless one is answering or booting. The child's stdin is
/// a pipe only this process holds, so the controller dies with the app however the app ends.
pub fn ensure(app: &AppHandle, port: u16) -> Result<(), Error> {
    let state = app.state::<State>();
    let repo = state
        .settings
        .lock()
        .map_err(|_| "settings poisoned")?
        .controller_repo()?;
    let mut controllers = state
        .controllers
        .lock()
        .map_err(|_| "controller table poisoned")?;
    if controllers.contains_key(&port) || healthy(port) {
        return Ok(());
    }
    controllers.insert(port, spawn(&repo, port)?);
    Ok(())
}

pub fn stop_all(app: &AppHandle) {
    let Some(state) = app.try_state::<State>() else {
        return;
    };
    let Ok(mut controllers) = state.controllers.lock() else {
        return;
    };
    for (_, mut child) in controllers.drain() {
        drop(child.stdin.take());
        if let Err(err) = child.wait() {
            eprintln!("crucible-desktop: stopping a controller: {err}");
        }
    }
}

pub fn wait_healthy(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while !healthy(port) {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    true
}

pub fn healthy(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let request =
        format!("GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut head = [0u8; 12];
    stream.read_exact(&mut head).is_ok() && head.ends_with(b"200")
}

fn spawn(repo: &Path, port: u16) -> std::io::Result<Child> {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new("sh");
    command
        .args(["-c", SUPERVISE, "crucible-desktop", &port.to_string()])
        .current_dir(repo)
        .stdin(Stdio::piped())
        .process_group(0);
    if let Some(path) = login_path() {
        command.env("PATH", path);
    }
    command.spawn()
}

/// The PATH the user's login shell builds, which an app opened from Finder does not inherit.
fn login_path() -> Option<String> {
    let user = std::env::var("USER").ok()?;
    let record = Command::new("dscl")
        .args([".", "-read", &format!("/Users/{user}"), "UserShell"])
        .output()
        .ok()?;
    let shell = String::from_utf8(record.stdout)
        .ok()?
        .split_whitespace()
        .nth(1)?
        .to_string();
    let env = Command::new(shell)
        .args(["-l", "-c", "env"])
        .output()
        .ok()?;
    String::from_utf8(env.stdout)
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("PATH="))
        .map(str::to_string)
}
