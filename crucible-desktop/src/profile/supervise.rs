//! A profile's kept command: the thread that watches it, and the supervisor process that ends it
//! when the app does.

use crate::profile::{Readiness, Target};
use std::io::{BufRead, BufReader};
#[cfg(not(target_arch = "wasm32"))]
use std::net::{IpAddr, SocketAddr};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PROBE: Duration = Duration::from_millis(800);
/// How often a kept command is looked at.
const POLL: Duration = Duration::from_millis(400);
/// How long a command that stopped rests before it is started again, doubling to the longest.
const RESTART: Duration = Duration::from_secs(2);
const RESTART_MAX: Duration = Duration::from_secs(60);
/// Lines of a tunnel's output kept to say why it stopped.
const TAIL: usize = 6;

/// The flag that runs the app as a tunnel's supervisor, and where the command goes.
pub const KEEP_FLAG: &str = "--keep";
const KEEP: &str = "CRUCIBLE_PLAYBOOKS_KEEP";
/// The shell the supervisor runs the command under, when the profile names one.
const KEEP_SHELL: &str = "CRUCIBLE_PLAYBOOKS_KEEP_SHELL";
/// How long a tunnel has to stop on TERM before it is killed.
#[cfg(not(target_arch = "wasm32"))]
const KILL_AFTER: Duration = Duration::from_secs(3);

/// Builds the command that supervises a kept one: [`supervisor`], outside tests.
pub type Supervisor = fn() -> std::io::Result<Command>;

/// One profile's kept command, watched on a thread of its own for as long as its keeper lives.
///
/// ```text
/// answers already? ──yes──▶ Ready, done
///   │ no
///   ▼
/// spawn ──▶ answers? ──yes──▶ Ready ──▶ exits ──▶ Failed ──▶ rest, doubling ──▶ spawn
///              │ exits or runs out of time
///              ▼
///           Failed: never answered ──▶ done        answered before ──▶ rest ──▶ spawn
/// ```
struct Watch {
    target: Target,
    command: String,
    shell: Option<String>,
    dir: PathBuf,
    addr: String,
    wait: Duration,
    what: &'static str,
    supervisor: Supervisor,
    kept: Arc<Mutex<Option<Kept>>>,
    stop: Arc<AtomicBool>,
}

impl Watch {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn spawn(&self) -> Result<(), String> {
        let kept = (self.supervisor)()
            .and_then(|s| Kept::spawn(s, &self.command, self.shell.as_deref(), &self.dir))
            .map_err(|why| format!("starting `{}`: {why}", self.command))?;
        match self.kept.lock() {
            Ok(mut slot) if !self.stopped() => *slot = Some(kept),
            _ => kept.end(),
        }
        Ok(())
    }

    fn end(&self) {
        if let Some(kept) = self.kept.lock().ok().and_then(|mut slot| slot.take()) {
            kept.end();
        }
    }

    /// Why the command stopped, once it has.
    fn exited(&self) -> Option<String> {
        let mut slot = self.kept.lock().ok()?;
        let kept = slot.as_mut()?;
        let status = kept.child.try_wait().ok().flatten()?;
        let said = kept.tail.lock().map(|t| t.join("\n")).unwrap_or_default();
        Some(stopped(&self.command, status, &said))
    }

    /// Wait for the command to answer, or say why it will not. `None` once the keeper is gone.
    fn up(&self) -> Option<Result<(), String>> {
        let until = Instant::now() + self.wait;
        loop {
            if self.stopped() {
                return None;
            }
            if answers(&self.addr) {
                return Some(Ok(()));
            }
            if let Some(why) = self.exited() {
                return Some(Err(why));
            }
            if Instant::now() > until {
                self.end();
                return Some(Err(format!(
                    "`{}` did not answer on {} within {}s",
                    self.command,
                    self.addr,
                    self.wait.as_secs()
                )));
            }
            std::thread::sleep(POLL);
        }
    }

    /// Sleep `long`, or return false as soon as the keeper is gone.
    fn rest(&self, long: Duration) -> bool {
        let until = Instant::now() + long;
        while Instant::now() < until {
            if self.stopped() {
                return false;
            }
            std::thread::sleep(POLL);
        }
        !self.stopped()
    }

    fn run(self) {
        if answers(&self.addr) {
            self.target.set(Readiness::Ready);
            return;
        }
        if let Err(why) = self.spawn() {
            self.target.set(Readiness::Failed(why));
            return;
        }
        let mut answered = false;
        let mut pause = RESTART;
        loop {
            match self.up() {
                None => return,
                Some(Ok(())) => {
                    self.target.set(Readiness::Ready);
                    answered = true;
                    pause = RESTART;
                    let why = loop {
                        if self.stopped() {
                            return;
                        }
                        if let Some(why) = self.exited() {
                            break why;
                        }
                        std::thread::sleep(POLL);
                    };
                    self.target.set(Readiness::Failed(why));
                }
                Some(Err(why)) => {
                    self.target.set(Readiness::Failed(why));
                    if !answered {
                        return;
                    }
                }
            }
            loop {
                if !self.rest(pause) {
                    return;
                }
                self.end();
                if answers(&self.addr) {
                    self.target.set(Readiness::Ready);
                    pause = RESTART;
                    continue;
                }
                pause = (pause * 2).min(RESTART_MAX);
                match self.spawn() {
                    Ok(()) => {
                        self.target.set(Readiness::Starting(self.what.to_string()));
                        break;
                    }
                    Err(why) => self.target.set(Readiness::Failed(why)),
                }
            }
        }
    }
}

fn stopped(command: &str, status: std::process::ExitStatus, said: &str) -> String {
    let mut why = format!("`{command}` stopped ({status})");
    if !said.trim().is_empty() {
        why.push_str(": ");
        why.push_str(said.trim());
    }
    why
}

/// `to` as addresses: an IP, an `ip:port`, or a name looked up now.
#[cfg(not(target_arch = "wasm32"))]
pub(in crate::profile) fn addresses(to: &str) -> std::io::Result<Vec<SocketAddr>> {
    let to = to.trim();
    if let Ok(ip) = to.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, 0)]);
    }
    if let Ok(addr) = to.parse::<SocketAddr>() {
        return Ok(vec![addr]);
    }
    let found: Vec<SocketAddr> = (to, 0).to_socket_addrs()?.collect();
    if found.is_empty() {
        return Err(std::io::Error::other("no address"));
    }
    Ok(found)
}

pub(in crate::profile) fn answers(addr: &str) -> bool {
    addr.to_socket_addrs()
        .ok()
        .into_iter()
        .flatten()
        .any(|a| TcpStream::connect_timeout(&a, PROBE).is_ok())
}

impl Target {
    /// Start what the profile keeps running, unless something already answers in its place, and
    /// mark the target ready once it answers. Once it has answered, a command that stops is
    /// started again. The keeper stops it when dropped. Returns at once: the probe and the
    /// command run on the watch thread.
    pub fn start(&self, repo: &Path, supervisor: Supervisor) -> Option<Keeper> {
        let (command, wait, what) = self.kept()?;
        let addr = self.probe_addr()?;
        self.set(Readiness::Starting(what.to_string()));
        let watch = Watch {
            target: self.clone(),
            command,
            shell: self.profile.shell.as_deref().map(|s| s.trim().to_string()),
            dir: repo.to_path_buf(),
            addr,
            wait,
            what,
            supervisor,
            kept: Arc::default(),
            stop: Arc::default(),
        };
        let keeper = Keeper {
            kept: Arc::clone(&watch.kept),
            stop: Arc::clone(&watch.stop),
        };
        let started = std::thread::Builder::new()
            .name("crucible-playbooks-tunnel".into())
            .spawn(move || watch.run());
        if let Err(why) = started {
            self.set(Readiness::Failed(format!(
                "starting the tunnel's watch: {why}"
            )));
            return None;
        }
        Some(keeper)
    }
}

/// A command kept running for a profile. Dropping it stops the command and its restarts.
pub struct Keeper {
    kept: Arc<Mutex<Option<Kept>>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Keeper {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(kept) = self.kept.lock().ok().and_then(|mut slot| slot.take()) {
            kept.end();
        }
    }
}

/// One run of a kept command under its supervisor.
struct Kept {
    child: Child,
    stdin: Option<ChildStdin>,
    tail: Arc<Mutex<Vec<String>>>,
}

impl Kept {
    /// Run `command` under `supervisor`, which [`keep`]s it in `shell`, else `$SHELL`.
    fn spawn(
        mut supervisor: Command,
        command: &str,
        shell: Option<&str>,
        dir: &Path,
    ) -> std::io::Result<Kept> {
        match shell {
            Some(shell) => supervisor.env(KEEP_SHELL, shell),
            None => supervisor.env_remove(KEEP_SHELL),
        };
        let mut child = supervisor
            .env(KEEP, command)
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take();
        let tail = Arc::new(Mutex::new(Vec::new()));
        if let Some(stderr) = child.stderr.take() {
            let tail = Arc::clone(&tail);
            let _ = std::thread::Builder::new()
                .name("crucible-playbooks-tunnel-err".into())
                .spawn(move || {
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        if let Ok(mut tail) = tail.lock() {
                            tail.push(line);
                            let over = tail.len().saturating_sub(TAIL);
                            tail.drain(..over);
                        }
                    }
                });
        }
        Ok(Kept { child, stdin, tail })
    }

    /// Close the pipe the supervisor reads, which then kills the command's process group, and
    /// reap it.
    fn end(mut self) {
        self.stdin = None;
        let _ = std::thread::Builder::new()
            .name("crucible-playbooks-tunnel-reap".into())
            .spawn(move || {
                let _ = self.child.wait();
            });
    }
}

/// The app itself as a supervisor: `crucible-playbooks --keep`, the command in [`KEEP`].
pub fn supervisor() -> std::io::Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command.arg(KEEP_FLAG);
    Ok(command)
}

/// Supervise the command in [`KEEP`], as `--keep`: run it under the login shell in a process group
/// of its own, kill that group once stdin closes, and return the command's exit code.
///
/// ```text
/// app ──stdin pipe──▶ supervisor ──spawns, own pgid──▶ $SHELL -l -c <command> ──▶ ssh …
///      app exits, crashes or is killed: the pipe closes ──▶ killpg(TERM), then KILL
///      the command exits first: the supervisor exits with its code
/// ```
#[cfg(not(target_arch = "wasm32"))]
pub fn keep() -> i32 {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    let Ok(command) = std::env::var(KEEP) else {
        eprintln!("{KEEP_FLAG} needs the command in {KEEP}");
        return 2;
    };
    let shell = std::env::var(KEEP_SHELL)
        .ok()
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| crate::editor::Os::HOST.login_shell());
    let mut child = match Command::new(&shell)
        .args(["-l", "-c", &command])
        .env_remove(KEEP)
        .env_remove(KEEP_SHELL)
        .stdin(Stdio::null())
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("starting {shell}: {e}");
            return 127;
        }
    };
    let Ok(group) = libc::pid_t::try_from(child.id()) else {
        let _ = child.kill();
        return 1;
    };
    let _ = std::thread::Builder::new()
        .name("keep-watch".into())
        .spawn(move || {
            let mut sink = Vec::new();
            let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut sink);
            // SAFETY: killpg only sends a signal; `group` is the group this process created.
            unsafe { libc::killpg(group, libc::SIGTERM) };
            std::thread::sleep(KILL_AFTER);
            // SAFETY: as above.
            unsafe { libc::killpg(group, libc::SIGKILL) };
        });
    match child.wait() {
        Ok(status) => status
            .code()
            .or_else(|| status.signal().map(|s| 128 + s))
            .unwrap_or(1),
        Err(_) => 1,
    }
}
