//! Profiles: the controllers the app can talk to, and how each one is reached.
//!
//! ```text
//! profile = "wdc"
//!
//! [profiles.mpp]
//! url = "https://crucible-api.example.com"
//! auth = "crux"                         # the key in crux's config.toml
//!
//! [profiles.wdc]
//! url = "https://crucible-api.wdc.example.com"
//! resolve = { "crucible-api.wdc.example.com" = "lb.example.net" }
//!
//! [profiles.jump]
//! url = "https://crucible-api.internal.example.com"
//! proxy = "socks5h://127.0.0.1:1080"
//! tunnel = "ssh -N -D 1080 jumpbox"     # kept running while the app is
//! shell = "/bin/bash"                    # what runs it, `-l -c`; `$SHELL` when unset
//!
//! [profiles.local]
//! url = "http://127.0.0.1:8870"         # starts `just controller-local 8870` in the repo
//! ```
//!
//! macOS cannot tie a child's life to its parent's, so a tunnel runs under the app's own binary
//! as a supervisor ([`keep`]), which ends the tunnel when the app's end of a pipe closes, however
//! the app ends.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
#[cfg(not(target_arch = "wasm32"))]
use std::net::{IpAddr, SocketAddr};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::keys::Store;

/// How long a tunnel has to start answering.
const TUNNEL_WAIT: Duration = Duration::from_secs(60);
/// How long the local controller has to build and bind.
const LOCAL_WAIT: Duration = Duration::from_mins(15);
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

/// Where a profile's key is kept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Auth {
    /// The app's own file for this profile.
    #[default]
    Own,
    /// crux's `config.toml`, shared with the CLI.
    Crux,
}

/// One controller, as the config file names it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub url: String,
    #[serde(default, skip_serializing_if = "is_own")]
    pub auth: Auth,
    /// An `http://`, `https://`, `socks5://` or `socks5h://` proxy for every request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Hosts reached at another address: an IP, or a name looked up instead.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resolve: BTreeMap<String, String>,
    /// A command kept running while the profile is in use, such as `ssh -N -D 1080 host` or a
    /// port forward. The profile waits for its proxy, or its URL, to answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<String>,
    /// The shell the tunnel or local controller runs under, as `<shell> -l -c <command>`;
    /// `$SHELL` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
}

fn is_own(auth: &Auth) -> bool {
    *auth == Auth::Own
}

impl Profile {
    /// Why this profile cannot be used, if it cannot.
    pub fn check(&self, name: &str) -> Result<(), String> {
        if !valid_name(name) {
            return Err(format!(
                "profile {name:?}: a name is letters, digits, - and _"
            ));
        }
        let url =
            Url::parse(self.url.trim()).map_err(|_| format!("profile {name}: url is not a URL"))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(format!(
                "profile {name}: url starts with http:// or https://"
            ));
        }
        if let Some(proxy) = &self.proxy {
            let ok = Url::parse(proxy).is_ok_and(|u| {
                matches!(u.scheme(), "http" | "https" | "socks5" | "socks5h")
                    && u.host_str().is_some()
            });
            if !ok {
                return Err(format!(
                    "profile {name}: proxy is an http://, https://, socks5:// or socks5h:// URL"
                ));
            }
        }
        for (host, to) in &self.resolve {
            if host.trim().is_empty() || to.trim().is_empty() {
                return Err(format!("profile {name}: resolve maps a host to an address"));
            }
        }
        if self.tunnel.as_deref().is_some_and(|t| t.trim().is_empty()) {
            return Err(format!("profile {name}: tunnel is a command"));
        }
        if self.shell.as_deref().is_some_and(|s| s.trim().is_empty()) {
            return Err(format!("profile {name}: shell is a program"));
        }
        Ok(())
    }

    /// Whether the URL is on this machine.
    pub fn loopback(&self) -> bool {
        Url::parse(&self.url).is_ok_and(|url| match url.host() {
            Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        })
    }

    /// What the profile goes through, for a one-line summary.
    pub fn route(&self) -> Vec<String> {
        let mut route = Vec::new();
        if let Some(tunnel) = &self.tunnel {
            route.push(format!("tunnel {}", tunnel.trim()));
        } else if self.loopback() {
            route.push("local controller".into());
        }
        if let Some(proxy) = &self.proxy {
            route.push(format!("via {proxy}"));
        }
        for (host, to) in &self.resolve {
            route.push(format!("{host} \u{2192} {to}"));
        }
        route
    }
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The profiles to offer: those configured, or one named `default` from crux's resolution
/// (`CONTROLLER_URL`, then crux's config file) when none is.
#[cfg(not(target_arch = "wasm32"))]
pub fn or_default(configured: &BTreeMap<String, Profile>) -> BTreeMap<String, Profile> {
    if !configured.is_empty() {
        return configured.clone();
    }
    let url = crux::config::ConnectArgs::from_env()
        .resolve()
        .map(|config| config.url)
        .unwrap_or_else(|_| "http://127.0.0.1:8870".to_string());
    BTreeMap::from([(
        "default".to_string(),
        Profile {
            url,
            auth: Auth::Crux,
            ..Profile::default()
        },
    )])
}

/// Whether a profile can be talked to yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    /// Waiting on its tunnel or local controller, which this says.
    Starting(String),
    Failed(String),
}

/// A profile in use: where it is, how requests reach it, and whether it answers yet. Workers each
/// hold a clone; the readiness is shared with the tunnel's keeper.
#[derive(Clone)]
pub struct Target {
    pub name: String,
    pub profile: Profile,
    /// Where its own key is kept; `None` uses the one in crux's config.
    keys: Option<Store>,
    ready: Arc<Mutex<Readiness>>,
    act_as: Option<crux::client::ActAs>,
}

impl Target {
    /// `name`'s target, its key in `keys` unless the profile uses crux's.
    pub fn new(name: &str, profile: Profile, keys: &Store) -> Target {
        let keys = match profile.auth {
            Auth::Own => Some(keys.clone()),
            Auth::Crux => None,
        };
        Target {
            name: name.to_string(),
            profile,
            keys,
            ready: Arc::new(Mutex::new(Readiness::Ready)),
            act_as: None,
        }
    }

    /// This target with every request acting as `team` (`team:<slug>`), or as the caller.
    pub fn acting_as(mut self, team: Option<String>) -> Target {
        self.act_as = team.map(crux::client::ActAs::new);
        self
    }

    pub fn act_as(&self) -> Option<&crux::client::ActAs> {
        self.act_as.as_ref()
    }

    /// A target nothing answers at, for workers under test.
    #[cfg(test)]
    pub fn unreachable() -> Target {
        let profile = Profile {
            url: "http://127.0.0.1:9".into(),
            ..Profile::default()
        };
        Target::new("test", profile, &Store::Dir("/nonexistent".into()))
    }

    pub fn url(&self) -> String {
        self.profile.url.trim().trim_end_matches('/').to_string()
    }

    pub fn readiness(&self) -> Readiness {
        self.ready
            .lock()
            .map(|r| r.clone())
            .unwrap_or_else(|_| Readiness::Failed("the profile's state is poisoned".into()))
    }

    fn set(&self, readiness: Readiness) {
        if let Ok(mut r) = self.ready.lock() {
            *r = readiness;
        }
    }

    /// The browser's session cookie signs requests.
    #[cfg(target_arch = "wasm32")]
    pub fn key(&self) -> Result<Option<String>, String> {
        Ok(None)
    }

    /// The key requests carry, read now so a sign-in takes effect on the next connection.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn key(&self) -> Result<Option<String>, String> {
        match &self.keys {
            Some(keys) => keys.key(&self.name),
            None => crux::config::FileConfig::load(None)
                .map(|config| config.api_token)
                .map_err(|e| format!("{e:#}")),
        }
    }

    /// Keep `key` as this profile's.
    pub fn save_key(&self, key: String) -> Result<(), String> {
        match &self.keys {
            Some(keys) => keys.save(&self.name, &self.url(), key),
            None => crux::config::FileConfig {
                url: Some(self.url()),
                api_token: Some(key),
            }
            .save(None)
            .map(drop)
            .map_err(|e| format!("{e:#}")),
        }
    }

    /// The crux config requests are made with.
    pub fn config(&self) -> Result<crux::config::Config, String> {
        Ok(crux::config::Config {
            url: self.url(),
            auth: crux::config::Auth::from_token(self.key()?),
        })
    }

    #[cfg(target_arch = "wasm32")]
    pub fn http(&self) -> Result<reqwest::ClientBuilder, String> {
        Ok(crux::client::HttpTransport::builder())
    }

    /// An HTTP client builder with the profile's proxy and resolver overrides. Names in
    /// `resolve` are looked up here, so this blocks on DNS.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn http(&self) -> Result<reqwest::ClientBuilder, String> {
        let mut builder = crux::client::HttpTransport::builder();
        if let Some(proxy) = &self.profile.proxy {
            let proxy = reqwest::Proxy::all(proxy.as_str())
                .map_err(|e| format!("profile {}: proxy: {e}", self.name))?;
            builder = builder.proxy(proxy);
        }
        for (host, to) in &self.profile.resolve {
            let addrs =
                addresses(to).map_err(|e| format!("profile {}: resolving {to}: {e}", self.name))?;
            builder = builder.resolve_to_addrs(host.trim(), &addrs);
        }
        Ok(builder)
    }

    /// A connection's precondition: the profile answers, or why not yet.
    pub fn ready(&self) -> Result<(), String> {
        match self.readiness() {
            Readiness::Ready => Ok(()),
            Readiness::Starting(what) => Err(format!("{what}\u{2026}")),
            Readiness::Failed(why) => Err(why),
        }
    }

    /// The command this profile keeps running, and how long it has to answer: its tunnel, or for
    /// a profile on this machine with none, the local controller from `repo`.
    fn kept(&self) -> Option<(String, Duration, &'static str)> {
        if cfg!(target_arch = "wasm32") {
            return None;
        }
        if let Some(tunnel) = &self.profile.tunnel {
            return Some((
                tunnel.trim().to_string(),
                TUNNEL_WAIT,
                "Starting the tunnel",
            ));
        }
        let port = Url::parse(&self.profile.url)
            .ok()
            .filter(|_| self.profile.loopback())?
            .port_or_known_default()?;
        Some((
            format!("just controller-local {port}"),
            LOCAL_WAIT,
            "Building and starting the local controller",
        ))
    }

    /// Where answering means the kept command is up: the proxy when there is one, else the URL.
    fn probe_addr(&self) -> Option<String> {
        let url = Url::parse(self.profile.proxy.as_deref().unwrap_or(&self.profile.url)).ok()?;
        Some(format!(
            "{}:{}",
            url.host_str()?.trim_matches(['[', ']']),
            url.port_or_known_default().or(match url.scheme() {
                "socks5" | "socks5h" => Some(1080),
                _ => None,
            })?
        ))
    }

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
fn addresses(to: &str) -> std::io::Result<Vec<SocketAddr>> {
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

fn answers(addr: &str) -> bool {
    addr.to_socket_addrs()
        .ok()
        .into_iter()
        .flatten()
        .any(|a| TcpStream::connect_timeout(&a, PROBE).is_ok())
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

/// What the retired Tauri desktop app's `config.toml` held, as far as this app uses it.
#[derive(Debug, Default, PartialEq)]
pub struct Tauri {
    pub profiles: BTreeMap<String, Profile>,
    pub default: Option<String>,
    pub drop: Option<(f64, String)>,
}

/// The Tauri app's settings in `text`, as profiles of this app. A profile that called crux
/// (`api = "crux"`) gets `crux_url` and crux's key; the others keep their URL. Its `resolve` named
/// a domain to reach elsewhere, here the profile's own host when it is in that domain.
pub fn from_tauri(text: &str, crux_url: &str) -> Tauri {
    #[derive(Deserialize)]
    struct File {
        default_profile: Option<String>,
        #[serde(default)]
        profiles: BTreeMap<String, Old>,
        drop: Option<Drop>,
    }
    #[derive(Deserialize)]
    struct Old {
        url: String,
        #[serde(default)]
        api: Option<String>,
        proxy: Option<String>,
        #[serde(default)]
        resolve: BTreeMap<String, String>,
    }
    #[derive(Deserialize)]
    struct Drop {
        max_cost: Option<f64>,
        max_time: Option<String>,
    }
    let Ok(file) = toml::from_str::<File>(text) else {
        return Tauri::default();
    };
    let profiles: BTreeMap<String, Profile> = file
        .profiles
        .into_iter()
        .filter_map(|(name, old)| {
            let crux = old.api.as_deref() == Some("crux");
            let url = if crux {
                crux_url.to_string()
            } else {
                old.url.trim_end_matches('/').to_string()
            };
            let host = Url::parse(&url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_default();
            let resolve = old
                .resolve
                .into_iter()
                .filter_map(|(domain, to)| {
                    let inside = host == domain || host.ends_with(&format!(".{domain}"));
                    inside.then(|| (host.clone(), to))
                })
                .collect();
            let profile = Profile {
                url,
                auth: if crux { Auth::Crux } else { Auth::Own },
                proxy: old.proxy,
                resolve,
                tunnel: None,
                shell: None,
            };
            profile.check(&name).ok().map(|()| (name, profile))
        })
        .collect();
    Tauri {
        default: file
            .default_profile
            .filter(|name| profiles.contains_key(name)),
        drop: file.drop.and_then(|d| Some((d.max_cost?, d.max_time?))),
        profiles,
    }
}

/// The retired Tauri app's settings file, under `config_dir`.
pub fn tauri_config(config_dir: &Path) -> PathBuf {
    config_dir.join("crucible-desktop").join("config.toml")
}

/// The profiles that can be used: the configured ones that read, or the default from crux.
#[cfg(not(target_arch = "wasm32"))]
pub fn usable(settings: &crate::settings::Settings) -> BTreeMap<String, Profile> {
    let good: BTreeMap<String, Profile> = settings
        .profiles()
        .into_iter()
        .filter_map(|(name, profile)| profile.ok().map(|p| (name, p)))
        .collect();
    or_default(&good)
}

/// The profiles that can be used: only the controller that served this page.
#[cfg(target_arch = "wasm32")]
pub fn usable(_settings: &crate::settings::Settings) -> BTreeMap<String, Profile> {
    let url = web_sys::window()
        .and_then(|window| window.location().origin().ok())
        .unwrap_or_default();
    BTreeMap::from([(
        "controller".to_string(),
        Profile {
            url,
            ..Profile::default()
        },
    )])
}

/// The profile to start in: the one last used, else the first.
pub fn pick(settings: &crate::settings::Settings) -> (String, Profile) {
    let mut usable = usable(settings);
    if let Some(name) = &settings.profile
        && let Some(profile) = usable.remove(name)
    {
        return (name.clone(), profile);
    }
    usable
        .into_iter()
        .next()
        .unwrap_or_else(|| ("default".into(), Profile::default()))
}

/// The Settings form for adding or editing a profile.
pub struct Form {
    /// The profile being edited, by its name before the edit.
    pub editing: Option<String>,
    pub name: rune::TextEdit,
    pub url: rune::TextEdit,
    pub proxy: rune::TextEdit,
    pub tunnel: rune::TextEdit,
    pub shell: rune::TextEdit,
    /// `host = address` pairs, one per line or comma-separated.
    pub resolve: rune::TextEdit,
    pub auth: Auth,
    pub error: Option<String>,
}

impl Form {
    pub fn new() -> Form {
        Form::of(None, &Profile::default())
    }

    pub fn of(name: Option<&str>, profile: &Profile) -> Form {
        let resolve: Vec<String> = profile
            .resolve
            .iter()
            .map(|(host, to)| format!("{host} = {to}"))
            .collect();
        Form {
            editing: name.map(str::to_string),
            name: rune::TextEdit::new(name.unwrap_or_default()),
            url: rune::TextEdit::new(&profile.url),
            proxy: rune::TextEdit::new(profile.proxy.as_deref().unwrap_or_default()),
            tunnel: rune::TextEdit::new(profile.tunnel.as_deref().unwrap_or_default()),
            shell: rune::TextEdit::new(profile.shell.as_deref().unwrap_or_default()),
            resolve: rune::TextEdit::new(resolve.join(", ")),
            auth: profile.auth,
            error: None,
        }
    }

    /// The named profile the form describes, or why it does not describe one.
    pub fn profile(&self) -> Result<(String, Profile), String> {
        let name = self.name.text.trim().to_string();
        let some = |edit: &rune::TextEdit| {
            let text = edit.text.trim();
            (!text.is_empty()).then(|| text.to_string())
        };
        let mut resolve = BTreeMap::new();
        for pair in self
            .resolve
            .text
            .split([',', '\n'])
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            let (host, to) = pair
                .split_once('=')
                .ok_or_else(|| format!("resolve: {pair:?} is host = address"))?;
            resolve.insert(host.trim().to_string(), to.trim().to_string());
        }
        let profile = Profile {
            url: self.url.text.trim().trim_end_matches('/').to_string(),
            auth: self.auth,
            proxy: some(&self.proxy),
            resolve,
            tunnel: some(&self.tunnel),
            shell: some(&self.shell),
        };
        profile.check(&name)?;
        Ok((name, profile))
    }
}

impl crate::App {
    /// Use profile `name`: start what it keeps running if it is not yet, and talk to it from
    /// fresh workers, forgetting what the last profile showed.
    pub fn use_profile(&mut self, name: &str) {
        let Some(profile) = usable(&self.settings).remove(name) else {
            return;
        };
        if self.remote.target.name == name && self.remote.target.profile == profile {
            return;
        }
        let target = match self.targets.get(name) {
            Some((target, _)) if target.profile == profile => target.clone(),
            _ => self.start_target(name, profile),
        };
        self.settings.use_profile(name);
        self.connect(target);
    }

    /// A fresh target for `name`, with what it keeps running started in place of the last one.
    fn start_target(&mut self, name: &str, profile: Profile) -> Target {
        self.targets.remove(name);
        let target =
            Target::new(name, profile, &self.settings.keys()).acting_as(self.settings.act_as(name));
        let keeper = target.start(Path::new(&self.repo), supervisor);
        self.targets
            .insert(name.to_string(), (target.clone(), keeper));
        target
    }

    /// Keep profile keys in the system credential store (`on`) or in key files, moving the ones
    /// kept so far.
    pub fn set_keychain(&mut self, on: bool) {
        let owned: Vec<(String, String)> = self
            .settings
            .profiles()
            .into_iter()
            .filter_map(|(name, profile)| Some((name, profile.ok()?)))
            .filter(|(_, profile)| profile.auth == Auth::Own)
            .map(|(name, profile)| (name, profile.url))
            .collect();
        let (from, to) = (self.settings.keys(), self.settings.keys_in(on));
        self.keys_error = from.move_to(&to, &owned).err();
        if self.keys_error.is_some() {
            return;
        }
        self.settings.set_keychain(on);
        self.targets.clear();
        self.retry_profile();
    }

    /// Start again what the profile in use keeps running.
    pub fn retry_profile(&mut self) {
        let target = self.start_target(
            &self.remote.target.name.clone(),
            self.remote.target.profile.clone(),
        );
        self.connect(target);
    }

    /// The `n`th usable profile, from 0.
    pub fn use_profile_at(&mut self, n: usize) {
        if let Some(name) = usable(&self.settings).into_keys().nth(n) {
            self.use_profile(&name);
        }
    }

    /// Act as `team` (`team:<slug>`) on the profile in use, or as the caller, from fresh workers.
    pub fn act_as(&mut self, team: Option<String>) {
        self.act_as_refused = None;
        self.switch_act_as(team);
    }

    fn switch_act_as(&mut self, team: Option<String>) {
        let current = self.remote.target.act_as().map(|a| a.team().to_string());
        if current == team {
            return;
        }
        let name = self.remote.target.name.clone();
        self.settings.set_act_as(&name, team.clone());
        let target = self.remote.target.clone().acting_as(team);
        if let Some((kept, _)) = self.targets.get_mut(&name) {
            *kept = target.clone();
        }
        self.connect(target);
    }

    /// Drop back to the caller once the controller has refused the team acted as, keeping why.
    pub fn heed_act_as_refusal(&mut self) -> bool {
        let Some(reason) = self.remote.target.act_as().and_then(|a| a.refused()) else {
            return false;
        };
        self.act_as_refused = Some(reason);
        self.switch_act_as(None);
        true
    }

    fn connect(&mut self, target: Target) {
        self.remote = crate::remote::Remote::new(
            target,
            &self.net,
            self.settings.drafts_root(),
            self.waker.as_ref(),
        );
        if matches!(self.selection, Some(crate::Selection::Live { .. })) {
            self.selection = None;
        }
        self.forget_run_view();
    }

    /// Keep the profile form's profile, and use it when it is the one in use or the first.
    pub fn save_profile_form(&mut self) {
        let Some(form) = &mut self.profile_form else {
            return;
        };
        let (name, profile) = match form.profile() {
            Ok(named) => named,
            Err(why) => {
                form.error = Some(why);
                return;
            }
        };
        let renamed = form.editing.clone().filter(|old| *old != name);
        if let Err(why) = self.settings.set_profile_def(&name, &profile) {
            form.error = Some(why);
            return;
        }
        let was_current = form.editing.as_deref() == Some(self.remote.target.name.as_str());
        if let Some(old) = renamed {
            if profile.auth == Auth::Own {
                self.keys_error = self.settings.keys().rename(&old, &name, &profile.url).err();
            }
            self.settings.remove_profile(&old);
            if let Some((_, keeper)) = self.targets.remove(&old) {
                drop(keeper);
            }
        }
        self.profile_form = None;
        if was_current || self.remote.target.name == name || self.settings.profile.is_none() {
            self.use_profile(&name);
        }
    }

    /// Forget profile `name`, stopping what it kept running. The profile in use moves to another.
    pub fn remove_profile(&mut self, name: &str) {
        self.keys_error = self.settings.keys().forget(name).err();
        self.settings.remove_profile(name);
        self.targets.remove(name);
        if self.remote.target.name == name {
            let (next, _) = pick(&self.settings);
            self.use_profile(&next);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    use crate::keys::Store;
    use crate::profile::{Auth, Profile, Readiness, Target, addresses, answers, keep};

    const AS_SUPERVISOR: &str = "CRUCIBLE_PLAYBOOKS_TEST_SUPERVISOR";

    /// This test binary as the supervisor: it reruns itself on [`supervisor`] alone.
    fn test_supervisor() -> std::io::Result<std::process::Command> {
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command
            .args(["profile::tests::supervisor", "--exact", "--nocapture"])
            .env(AS_SUPERVISOR, "1");
        Ok(command)
    }

    #[test]
    fn supervisor() {
        if std::env::var_os(AS_SUPERVISOR).is_some() {
            std::process::exit(keep());
        }
    }

    fn profile(url: &str) -> Profile {
        Profile {
            url: url.into(),
            ..Profile::default()
        }
    }

    fn wait_for(target: &Target, done: impl Fn(&Readiness) -> bool) -> Readiness {
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            let now = target.readiness();
            if done(&now) || Instant::now() > until {
                return now;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn profiles_parse_with_every_route() {
        #[derive(serde::Deserialize)]
        struct File {
            profiles: BTreeMap<String, Profile>,
        }
        let file: File = toml::from_str(
            r#"
[profiles.mpp]
url = "https://api.example.com"
auth = "crux"

[profiles.wdc]
url = "https://api.wdc.example.com"
resolve = { "api.wdc.example.com" = "10.0.0.7" }

[profiles.jump]
url = "https://api.internal.example.com"
proxy = "socks5h://127.0.0.1:1080"
tunnel = "ssh -N -D 1080 jumpbox"
"#,
        )
        .unwrap();
        assert_eq!(file.profiles["mpp"].auth, Auth::Crux);
        assert_eq!(file.profiles["wdc"].auth, Auth::Own);
        assert_eq!(
            file.profiles["jump"].route(),
            [
                "tunnel ssh -N -D 1080 jumpbox",
                "via socks5h://127.0.0.1:1080"
            ]
        );
        for (name, p) in &file.profiles {
            p.check(name).unwrap();
        }
        assert!(
            toml::from_str::<File>("[profiles.x]\nurl = \"https://a\"\nproxxy = \"x\"").is_err(),
            "a typo is refused"
        );
    }

    #[test]
    fn the_tauri_apps_profiles_carry_over() {
        let old = crate::profile::from_tauri(
            r#"
default_profile = "mpp"

[profiles.local]
url = "http://127.0.0.1:8870"

[profiles.mpp]
url = "https://crucible.example.com"
api = "crux"

[profiles.jump]
url = "https://crucible.internal.example.com/"
proxy = "socks5://127.0.0.1:1080"

[drop]
profile = "local"
max_cost = 2.5
max_time = "45m"

[profiles.wdc]
url = "https://crucible.ext.wdc.example.com"

[profiles.wdc.resolve]
"ext.wdc.example.com" = "lb.example.net"
"#,
            "https://crucible-api.example.com",
        );
        assert_eq!(old.default.as_deref(), Some("mpp"));
        assert_eq!(old.drop, Some((2.5, "45m".to_string())));
        assert_eq!(old.profiles["mpp"].url, "https://crucible-api.example.com");
        assert_eq!(old.profiles["mpp"].auth, Auth::Crux);
        assert_eq!(old.profiles["local"].auth, Auth::Own);
        assert!(old.profiles["local"].loopback());
        assert_eq!(
            old.profiles["jump"].url,
            "https://crucible.internal.example.com"
        );
        assert_eq!(
            old.profiles["jump"].proxy.as_deref(),
            Some("socks5://127.0.0.1:1080")
        );
        assert_eq!(
            old.profiles["wdc"].resolve,
            BTreeMap::from([(
                "crucible.ext.wdc.example.com".to_string(),
                "lb.example.net".to_string()
            )]),
            "the domain pin becomes a pin on the profile's host"
        );
        assert_eq!(
            crate::profile::from_tauri("not toml [", "x"),
            crate::profile::Tauri::default()
        );
    }

    #[test]
    fn a_bad_profile_says_which_part() {
        let bad = |p: Profile, name: &str, part: &str| {
            let why = p.check(name).unwrap_err();
            assert!(why.contains(part), "{why}");
        };
        bad(profile("https://a"), "has space", "name");
        bad(profile("ftp://a"), "x", "url");
        bad(profile("nope"), "x", "url");
        let mut p = profile("https://a");
        p.proxy = Some("ftp://proxy".into());
        bad(p.clone(), "x", "proxy");
        p.proxy = None;
        p.resolve.insert("a".into(), " ".into());
        bad(p.clone(), "x", "resolve");
        p.resolve.clear();
        p.tunnel = Some("  ".into());
        bad(p, "x", "tunnel");
    }

    #[test]
    fn loopback_profiles_start_the_local_controller() {
        let local = Target::new(
            "local",
            profile("http://127.0.0.1:8870"),
            &Store::Dir("/k".into()),
        );
        assert_eq!(
            local.kept().map(|k| k.0),
            Some("just controller-local 8870".to_string())
        );
        assert!(profile("http://localhost:9000").loopback());
        assert!(profile("http://[::1]:9000").loopback());
        let remote = Target::new(
            "r",
            profile("https://api.example.com"),
            &Store::Dir("/k".into()),
        );
        assert_eq!(remote.kept(), None);
    }

    #[test]
    fn a_tunnel_is_probed_at_its_proxy_and_otherwise_at_the_url() {
        let mut p = profile("https://api.example.com");
        p.tunnel = Some("ssh -N -D 1080 j".into());
        p.proxy = Some("socks5h://127.0.0.1:1081".into());
        let t = Target::new("j", p.clone(), &Store::Dir("/k".into()));
        assert_eq!(t.probe_addr().as_deref(), Some("127.0.0.1:1081"));
        p.proxy = Some("socks5://127.0.0.1".into());
        let t = Target::new("j", p.clone(), &Store::Dir("/k".into()));
        assert_eq!(t.probe_addr().as_deref(), Some("127.0.0.1:1080"));
        p.proxy = None;
        p.url = "http://127.0.0.1:18870".into();
        let t = Target::new("j", p, &Store::Dir("/k".into()));
        assert_eq!(t.probe_addr().as_deref(), Some("127.0.0.1:18870"));
    }

    #[test]
    fn resolve_takes_an_ip_an_ip_and_port_or_a_name() {
        assert_eq!(addresses("10.0.0.7").unwrap()[0].to_string(), "10.0.0.7:0");
        assert_eq!(
            addresses("10.0.0.7:8443").unwrap()[0].to_string(),
            "10.0.0.7:8443"
        );
        assert!(addresses("localhost").unwrap()[0].ip().is_loopback());
    }

    #[test]
    fn each_profile_keeps_its_own_key() {
        let dir = tempfile::tempdir().unwrap();
        let a = Target::new(
            "a",
            profile("https://a.example.com/"),
            &Store::Dir(dir.path().into()),
        );
        let b = Target::new(
            "b",
            profile("https://b.example.com"),
            &Store::Dir(dir.path().into()),
        );
        assert_eq!(a.key().unwrap(), None, "no file yet is no key");
        a.save_key("crk_a".into()).unwrap();
        assert_eq!(a.key().unwrap().as_deref(), Some("crk_a"));
        assert_eq!(b.key().unwrap(), None);
        assert_eq!(a.config().unwrap().url, "https://a.example.com");
        let saved = std::fs::read_to_string(dir.path().join("a.toml")).unwrap();
        assert!(saved.contains("https://a.example.com"), "{saved}");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("a.toml"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_tunnel_that_comes_up_makes_the_profile_ready_and_dies_with_its_keeper() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut p = profile(&format!("http://127.0.0.1:{port}"));
        p.tunnel = Some(format!(
            "exec python3 -c 'import socket,time; s=socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); s.bind((\"127.0.0.1\", {port})); s.listen(); time.sleep(60)'"
        ));
        let dir = tempfile::tempdir().unwrap();
        let target = Target::new("t", p, &Store::Dir(dir.path().into()));
        let keeper = target.start(dir.path(), test_supervisor).expect("a keeper");
        assert!(matches!(target.readiness(), Readiness::Starting(_)));
        assert_eq!(
            wait_for(&target, |r| *r
                != Readiness::Starting("Starting the tunnel".into())),
            Readiness::Ready
        );
        assert!(target.ready().is_ok());
        drop(keeper);
        let until = Instant::now() + Duration::from_secs(10);
        while answers(&format!("127.0.0.1:{port}")) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !answers(&format!("127.0.0.1:{port}")),
            "dropping the keeper ends the tunnel"
        );
    }

    #[test]
    fn a_tunnel_that_stops_after_answering_is_started_again_until_its_keeper_goes() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let addr = format!("127.0.0.1:{port}");
        let dir = tempfile::tempdir().unwrap();
        let starts = dir.path().join("starts");
        let mut p = profile(&format!("http://{addr}"));
        p.tunnel = Some(format!(
            "echo start >> '{}'; exec python3 -c 'import socket,sys,time; s=socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); s.bind((\"127.0.0.1\", {port})); s.listen(); time.sleep(1.5); sys.stderr.write(\"connection reset\\n\"); sys.exit(255)'",
            starts.display()
        ));
        let started = || {
            std::fs::read_to_string(&starts)
                .unwrap_or_default()
                .lines()
                .count()
        };
        let target = Target::new("t", p, &Store::Dir(dir.path().into()));
        let keeper = target.start(dir.path(), test_supervisor).expect("a keeper");
        assert_eq!(
            wait_for(&target, |r| !matches!(r, Readiness::Starting(_))),
            Readiness::Ready
        );
        assert_eq!(started(), 1);

        let down = wait_for(&target, |r| matches!(r, Readiness::Failed(_)));
        let Readiness::Failed(why) = down else {
            panic!("{down:?}");
        };
        assert!(why.contains("connection reset"), "{why}");
        assert!(target.ready().is_err(), "requests wait while it is down");

        assert_eq!(
            wait_for(&target, |r| *r == Readiness::Ready),
            Readiness::Ready
        );
        assert_eq!(started(), 2);
        assert!(target.ready().is_ok());

        drop(keeper);
        let seen = started();
        let until = Instant::now() + Duration::from_secs(10);
        while answers(&addr) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_secs(5));
        assert!(!answers(&addr), "dropping the keeper ends the tunnel");
        assert_eq!(started(), seen, "and nothing starts it again");
    }

    #[test]
    fn a_tunnel_that_never_answers_is_not_started_again() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let dir = tempfile::tempdir().unwrap();
        let starts = dir.path().join("starts");
        let mut p = profile(&format!("http://127.0.0.1:{port}"));
        p.tunnel = Some(format!("echo start >> '{}'; exit 255", starts.display()));
        let target = Target::new("t", p, &Store::Dir(dir.path().into()));
        let _keeper = target.start(dir.path(), test_supervisor);
        let failed = wait_for(&target, |r| matches!(r, Readiness::Failed(_)));
        assert!(matches!(failed, Readiness::Failed(_)), "{failed:?}");
        std::thread::sleep(Duration::from_secs(5));
        assert_eq!(std::fs::read_to_string(&starts).unwrap(), "start\n");
        assert!(matches!(target.readiness(), Readiness::Failed(_)));
    }

    #[test]
    fn a_tunnel_runs_under_the_profiles_shell() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut p = profile(&format!("http://127.0.0.1:{port}"));
        p.tunnel = Some("echo \"ran under $0\" >&2; exit 3".into());
        p.shell = Some("/bin/sh".into());
        let dir = tempfile::tempdir().unwrap();
        let target = Target::new("t", p.clone(), &Store::Dir(dir.path().into()));
        let _keeper = target.start(dir.path(), test_supervisor);
        let failed = wait_for(&target, |r| matches!(r, Readiness::Failed(_)));
        let Readiness::Failed(why) = failed else {
            panic!("{failed:?}");
        };
        assert!(why.contains("ran under /bin/sh"), "{why}");

        p.shell = Some("/no/such/shell".into());
        let target = Target::new("t", p, &Store::Dir(dir.path().into()));
        let _keeper = target.start(dir.path(), test_supervisor);
        let failed = wait_for(&target, |r| matches!(r, Readiness::Failed(_)));
        let Readiness::Failed(why) = failed else {
            panic!("{failed:?}");
        };
        assert!(why.contains("starting /no/such/shell"), "{why}");
    }

    #[test]
    fn a_tunnel_that_exits_says_what_it_printed() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut p = profile(&format!("http://127.0.0.1:{port}"));
        p.tunnel = Some("echo 'ssh: Could not resolve hostname jumpbox' >&2; exit 255".into());
        let dir = tempfile::tempdir().unwrap();
        let target = Target::new("t", p, &Store::Dir(dir.path().into()));
        let _keeper = target.start(dir.path(), test_supervisor);
        let failed = wait_for(&target, |r| matches!(r, Readiness::Failed(_)));
        let Readiness::Failed(why) = failed else {
            panic!("{failed:?}");
        };
        assert!(why.contains("Could not resolve hostname jumpbox"), "{why}");
        assert!(target.ready().is_err());
    }

    /// An HTTP server answering every request with `body`, and its port.
    fn http_server(body: &'static str) -> u16 {
        use std::io::{Read, Write};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        port
    }

    /// A SOCKS5 server, no auth, CONNECT only, that sends every connection to 127.0.0.1:`to` and
    /// records the host each one asked for. Returns its port.
    fn socks_server(to: u16, asked: std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> u16 {
        use std::io::{Read, Write};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let mut client = client.unwrap();
                let asked = asked.clone();
                std::thread::spawn(move || {
                    let mut hello = [0u8; 2];
                    client.read_exact(&mut hello).unwrap();
                    let mut methods = vec![0u8; hello[1] as usize];
                    client.read_exact(&mut methods).unwrap();
                    client.write_all(&[5, 0]).unwrap();
                    let mut head = [0u8; 4];
                    client.read_exact(&mut head).unwrap();
                    let host = match head[3] {
                        3 => {
                            let mut len = [0u8; 1];
                            client.read_exact(&mut len).unwrap();
                            let mut name = vec![0u8; len[0] as usize];
                            client.read_exact(&mut name).unwrap();
                            String::from_utf8(name).unwrap()
                        }
                        1 => {
                            let mut ip = [0u8; 4];
                            client.read_exact(&mut ip).unwrap();
                            std::net::Ipv4Addr::from(ip).to_string()
                        }
                        other => panic!("address type {other}"),
                    };
                    let mut dest_port = [0u8; 2];
                    client.read_exact(&mut dest_port).unwrap();
                    asked.lock().unwrap().push(host);
                    let upstream = std::net::TcpStream::connect(("127.0.0.1", to)).unwrap();
                    client.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0]).unwrap();
                    let (mut a, mut b) =
                        (client.try_clone().unwrap(), upstream.try_clone().unwrap());
                    let mut client = client;
                    let mut upstream = upstream;
                    std::thread::spawn(move || std::io::copy(&mut a, &mut upstream));
                    let _ = std::io::copy(&mut b, &mut client);
                });
            }
        });
        port
    }

    fn get(target: &Target) -> Result<String, String> {
        let client = target.http()?.build().map_err(|e| e.to_string())?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let response = client
                .get(format!("{}/healthz", target.url()))
                .send()
                .await
                .map_err(|e| format!("{e:?}"))?;
            response.text().await.map_err(|e| e.to_string())
        })
    }

    #[test]
    fn requests_go_through_a_socks5h_proxy_which_resolves_the_name() {
        let server = http_server("through the proxy");
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let socks = socks_server(server, asked.clone());
        let mut p = profile(&format!("http://controller.internal.invalid:{server}"));
        p.proxy = Some(format!("socks5h://127.0.0.1:{socks}"));
        let target = Target::new("jump", p, &Store::Dir("/k".into()));
        assert_eq!(get(&target).unwrap(), "through the proxy");
        assert_eq!(
            *asked.lock().unwrap(),
            ["controller.internal.invalid"],
            "socks5h hands the name to the proxy rather than resolving it here"
        );
    }

    #[test]
    fn a_resolve_pin_reaches_a_name_dns_does_not_know() {
        let server = http_server("pinned");
        let mut p = profile(&format!("http://api.wdc.invalid:{server}"));
        p.resolve
            .insert("api.wdc.invalid".into(), "127.0.0.1".into());
        let pinned = Target::new("wdc", p.clone(), &Store::Dir("/k".into()));
        assert_eq!(get(&pinned).unwrap(), "pinned");
        p.resolve.clear();
        assert!(get(&Target::new("wdc", p, &Store::Dir("/k".into()))).is_err());
    }

    #[test]
    fn something_already_answering_is_used_without_starting_anything() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut p = profile(&format!("http://127.0.0.1:{port}"));
        let dir = tempfile::tempdir().unwrap();
        let starts = dir.path().join("starts");
        p.tunnel = Some(format!("echo start >> '{}'; exit 1", starts.display()));
        let target = Target::new("t", p, &Store::Dir(dir.path().into()));
        let _keeper = target.start(dir.path(), test_supervisor);
        assert_eq!(
            wait_for(&target, |r| !matches!(r, Readiness::Starting(_))),
            Readiness::Ready
        );
        std::thread::sleep(Duration::from_secs(1));
        assert!(!starts.exists(), "the command never ran");
        assert_eq!(target.readiness(), Readiness::Ready);
    }
}
