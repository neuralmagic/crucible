//! Tokens a command or evaluate task presents to the run's MCP servers. The engine exchanges the
//! run's pod token at the controller's token endpoint for one token per server, keeps each in a
//! file the task reads, and rewrites the files before the tokens expire for as long as the task
//! runs.

use crucible_contract::agent_servers::ServerName;
use crucible_contract::task_servers::{
    ENV_LOOP_SPIFFE_SOCKET, ENV_RUN_SERVERS, ENV_TASK_SVID_AUDIENCE, ENV_TASK_TOKEN_ENDPOINT,
    JWT_TOKEN_TYPE, POD_TOKEN_TYPE, RunServer, TOKEN_EXCHANGE_GRANT, TaskServer,
};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const RETRY: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum TaskTokenError {
    #[error("[workflow] servers names {name:?}, but the controller granted this run {granted:?}")]
    NotGranted { name: String, granted: Vec<String> },
    #[error("[workflow] servers needs {0} in the engine's environment; the controller sets it")]
    Env(&'static str),
    #[error("{ENV_RUN_SERVERS} is not a list of servers: {0}")]
    Servers(#[from] serde_json::Error),
    #[error("building the token client: {0}")]
    Client(#[source] reqwest::Error),
    #[error("fetching this pod's JWT-SVID from {socket}: {reason}")]
    Svid { socket: String, reason: String },
    #[error("the token endpoint refused a token for {server}: {reason}")]
    Refused { server: String, reason: String },
    #[error("reaching the token endpoint for {server}: {source}")]
    Http {
        server: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("writing the token file for {server}: {source}")]
    File {
        server: String,
        #[source]
        source: std::io::Error,
    },
}

/// The servers `names` asks for among those the controller lets this run's tasks reach.
pub fn wanted(names: &[ServerName]) -> Result<Vec<RunServer>, TaskTokenError> {
    let granted = crucible_contract::task_servers::decode_run_servers(
        &std::env::var(ENV_RUN_SERVERS).unwrap_or_default(),
    )?;
    wanted_among(names, &granted)
}

fn wanted_among(
    names: &[ServerName],
    granted: &[RunServer],
) -> Result<Vec<RunServer>, TaskTokenError> {
    names
        .iter()
        .map(|name| {
            granted
                .iter()
                .find(|s| &s.name == name)
                .cloned()
                .ok_or_else(|| TaskTokenError::NotGranted {
                    name: name.to_string(),
                    granted: granted.iter().map(|s| s.name.to_string()).collect(),
                })
        })
        .collect()
}

/// Where the loop pod gets the JWT-SVID it presents beside its pod token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SvidSource {
    /// The SPIFFE Workload API socket path.
    pub socket: String,
    /// The audience the SVID is requested for: the controller's issuer.
    pub audience: String,
    /// The loop pod's name, which ends the SPIFFE ID the controller accepts. A pod can hold
    /// other identities too, such as a cluster's default one per service account.
    pub pod: String,
}

impl SvidSource {
    fn fetch(&self) -> Result<String, TaskTokenError> {
        let failed = |reason: String| TaskTokenError::Svid {
            socket: self.socket.clone(),
            reason,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| failed(e.to_string()))?;
        runtime.block_on(async {
            let client = spiffe::WorkloadApiClient::connect_to(&format!("unix://{}", self.socket))
                .await
                .map_err(|e| failed(e.to_string()))?;
            let svids = client
                .fetch_all_jwt_svids(&[self.audience.as_str()], None)
                .await
                .map_err(|e| failed(e.to_string()))?;
            let wanted = format!(
                "/{}{}",
                crucible_contract::task_servers::RUN_SPIFFE_PATH,
                self.pod
            );
            svids
                .iter()
                .find(|svid| svid.spiffe_id().path() == wanted)
                .map(|svid| svid.token().to_string())
                .ok_or_else(|| {
                    failed(format!(
                        "no identity ends in {wanted}; this pod holds {:?}",
                        svids
                            .iter()
                            .map(|svid| svid.spiffe_id().to_string())
                            .collect::<Vec<_>>()
                    ))
                })
        })
    }
}

/// The controller's token endpoint and what proves the run to it: its pod token, and the pod's
/// JWT-SVID where the cluster gives loop pods one.
#[derive(Clone)]
pub struct TokenEndpoint {
    url: String,
    pod_token: String,
    svid: Option<SvidSource>,
    client: reqwest::blocking::Client,
}

#[derive(serde::Deserialize)]
struct Issued {
    access_token: String,
    expires_in: u64,
}

#[derive(serde::Deserialize)]
struct Refusal {
    error: String,
}

impl TokenEndpoint {
    pub fn from_env() -> Result<Self, TaskTokenError> {
        let env = |name: &'static str| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .ok_or(TaskTokenError::Env(name))
        };
        let svid = match env(ENV_LOOP_SPIFFE_SOCKET) {
            Ok(socket) => Some(SvidSource {
                socket,
                audience: env(ENV_TASK_SVID_AUDIENCE)?,
                pod: env(crucible_contract::ENV_POD_NAME)?,
            }),
            Err(_) => None,
        };
        Self::new(
            env(ENV_TASK_TOKEN_ENDPOINT)?,
            env(crucible_contract::ENV_POD_TOKEN)?,
            svid,
        )
    }

    pub fn new(
        url: String,
        pod_token: String,
        svid: Option<SvidSource>,
    ) -> Result<Self, TaskTokenError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(TaskTokenError::Client)?;
        Ok(Self {
            url,
            pod_token,
            svid,
            client,
        })
    }

    fn fetch(&self, server: &RunServer, task: &str) -> Result<Issued, TaskTokenError> {
        let http = |source| TaskTokenError::Http {
            server: server.name.to_string(),
            source,
        };
        let scope = server.scopes.join(" ");
        let mut form = vec![
            ("grant_type", TOKEN_EXCHANGE_GRANT),
            ("subject_token", self.pod_token.as_str()),
            ("subject_token_type", POD_TOKEN_TYPE),
            ("audience", server.url.as_str()),
            ("task", task),
        ];
        if !scope.is_empty() {
            form.push(("scope", scope.as_str()));
        }
        let svid = self.svid.as_ref().map(SvidSource::fetch).transpose()?;
        if let Some(svid) = &svid {
            form.push(("actor_token", svid.as_str()));
            form.push(("actor_token_type", JWT_TOKEN_TYPE));
        }
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(&form)
            .finish();
        let response = self
            .client
            .post(&self.url)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .map_err(http)?;
        let status = response.status();
        if !status.is_success() {
            let code = response
                .json::<Refusal>()
                .map(|r| r.error)
                .unwrap_or_default();
            return Err(TaskTokenError::Refused {
                server: server.name.to_string(),
                reason: format!("HTTP {status} {code}").trim_end().to_string(),
            });
        }
        response.json().map_err(http)
    }
}

fn write_token(file: &Path, token: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let staged = file.with_extension("new");
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&staged)?;
    out.write_all(token.as_bytes())?;
    out.sync_all()?;
    std::fs::rename(&staged, file)
}

/// A task's tokens, kept fresh until dropped.
pub struct TaskTokens {
    servers: Vec<TaskServer>,
    stop: Arc<(Mutex<bool>, Condvar)>,
    refresher: Option<std::thread::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl TaskTokens {
    /// Fetch a token for `task` to each of `wanted`, and keep them fresh.
    pub fn grant(
        endpoint: TokenEndpoint,
        wanted: Vec<RunServer>,
        task: &str,
    ) -> Result<Self, TaskTokenError> {
        let file = |server: &RunServer, source| TaskTokenError::File {
            server: server.name.to_string(),
            source,
        };
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::Builder::new()
            .prefix("crucible-task-tokens-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .map_err(|e| TaskTokenError::File {
                server: String::new(),
                source: e,
            })?;
        let mut files: Vec<(RunServer, PathBuf)> = Vec::new();
        let mut lifetime = u64::MAX;
        for server in wanted {
            let issued = endpoint.fetch(&server, task)?;
            let path = dir.path().join(server.name.as_str());
            write_token(&path, &issued.access_token).map_err(|e| file(&server, e))?;
            lifetime = lifetime.min(issued.expires_in);
            files.push((server, path));
        }
        let servers = files
            .iter()
            .map(|(server, path)| TaskServer {
                name: server.name.clone(),
                url: server.url.clone(),
                token_file: path.clone(),
            })
            .collect();
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let refresher = (!files.is_empty()).then(|| {
            let stop = stop.clone();
            let task = task.to_string();
            std::thread::spawn(move || refresh(&endpoint, &files, &task, lifetime, &stop))
        });
        Ok(Self {
            servers,
            stop,
            refresher,
            _dir: dir,
        })
    }

    pub fn servers(&self) -> &[TaskServer] {
        &self.servers
    }
}

impl Drop for TaskTokens {
    fn drop(&mut self) {
        let (stopped, wake) = &*self.stop;
        *stopped.lock().unwrap_or_else(PoisonError::into_inner) = true;
        wake.notify_all();
        if let Some(refresher) = self.refresher.take()
            && refresher.join().is_err()
        {
            eprintln!("crucible: the task token refresher panicked");
        }
    }
}

/// When to replace a token that lives `expires_in` seconds.
fn refresh_after(expires_in: u64) -> Duration {
    Duration::from_secs((expires_in.saturating_mul(2) / 3).max(1))
}

fn refresh(
    endpoint: &TokenEndpoint,
    files: &[(RunServer, PathBuf)],
    task: &str,
    mut lifetime: u64,
    stop: &(Mutex<bool>, Condvar),
) {
    let (stopped, wake) = stop;
    let mut wait = refresh_after(lifetime);
    loop {
        let guard = stopped.lock().unwrap_or_else(PoisonError::into_inner);
        let (guard, _) = wake
            .wait_timeout_while(guard, wait, |stopped| !*stopped)
            .unwrap_or_else(PoisonError::into_inner);
        if *guard {
            return;
        }
        drop(guard);
        let renewed: Result<u64, TaskTokenError> =
            files.iter().try_fold(u64::MAX, |least, (server, path)| {
                let issued = endpoint.fetch(server, task)?;
                write_token(path, &issued.access_token).map_err(|source| TaskTokenError::File {
                    server: server.name.to_string(),
                    source,
                })?;
                Ok(least.min(issued.expires_in))
            });
        wait = match renewed {
            Ok(least) => {
                lifetime = least;
                refresh_after(lifetime)
            }
            Err(e) => {
                eprintln!("crucible: task {task}: {e}; retrying");
                RETRY.min(refresh_after(lifetime))
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use crate::plan::task_tokens::*;
    use std::io::{BufRead, BufReader, Read};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A stand-in for the controller's token endpoint on loopback. It answers request `n` with
    /// token `token-<n>` living `expires_in` seconds, or `refusal` when set, and hands back each
    /// form body it received.
    fn endpoint(
        expires_in: u64,
        refusal: Option<&'static str>,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/oauth/token", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        let served = AtomicUsize::new(0);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let mut conn = conn.unwrap();
                let mut reader = BufReader::new(conn.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                if tx.send(String::from_utf8(body).unwrap()).is_err() {
                    return;
                }
                let n = served.fetch_add(1, Ordering::SeqCst) + 1;
                let (status, json) = match refusal {
                    Some(code) => ("400 Bad Request", format!(r#"{{"error":"{code}"}}"#)),
                    None => (
                        "200 OK",
                        format!(r#"{{"access_token":"token-{n}","expires_in":{expires_in}}}"#),
                    ),
                };
                let reply = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{json}",
                    json.len()
                );
                conn.write_all(reply.as_bytes()).unwrap();
            }
        });
        (url, rx)
    }

    fn kueue() -> RunServer {
        RunServer {
            name: ServerName::parse("kueue").unwrap(),
            url: "http://kueue.example.svc:8850/mcp".into(),
            scopes: vec!["kueue.read".into(), "kueue.submit".into()],
        }
    }

    fn form(body: &str) -> std::collections::BTreeMap<String, String> {
        url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect()
    }

    #[test]
    fn a_pack_gets_only_servers_the_controller_lets_its_tasks_reach() {
        let granted = [kueue()];
        let names = |names: &[&str]| -> Vec<ServerName> {
            names
                .iter()
                .map(|n| ServerName::parse(n).unwrap())
                .collect()
        };
        assert_eq!(
            wanted_among(&names(&["kueue"]), &granted).unwrap(),
            [kueue()]
        );
        assert!(wanted_among(&[], &granted).unwrap().is_empty());
        assert_eq!(
            wanted_among(&names(&["notes"]), &granted)
                .unwrap_err()
                .to_string(),
            "[workflow] servers names \"notes\", but the controller granted this run [\"kueue\"]"
        );
        assert_eq!(
            wanted_among(&names(&["kueue"]), &[])
                .unwrap_err()
                .to_string(),
            "[workflow] servers names \"kueue\", but the controller granted this run []"
        );
    }

    #[test]
    fn a_task_gets_a_token_file_per_server_from_the_pod_token() {
        let (url, seen) = endpoint(900, None);
        let endpoint = TokenEndpoint::new(url, "crr_id_secret".into(), None).unwrap();
        let tokens = TaskTokens::grant(endpoint, vec![kueue()], "probe").unwrap();
        let [server] = tokens.servers() else {
            panic!("{:?}", tokens.servers());
        };
        assert_eq!(server.name.as_str(), "kueue");
        assert_eq!(server.url, "http://kueue.example.svc:8850/mcp");
        assert_eq!(
            std::fs::read_to_string(&server.token_file).unwrap(),
            "token-1"
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&server.token_file), 0o600);
        assert_eq!(mode(server.token_file.parent().unwrap()), 0o700);
        let sent = form(&seen.recv().unwrap());
        let expected = [
            ("grant_type", TOKEN_EXCHANGE_GRANT),
            ("subject_token", "crr_id_secret"),
            ("subject_token_type", POD_TOKEN_TYPE),
            ("audience", "http://kueue.example.svc:8850/mcp"),
            ("task", "probe"),
            ("scope", "kueue.read kueue.submit"),
        ];
        assert_eq!(
            sent,
            expected
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        );
        let file = server.token_file.clone();
        drop(tokens);
        assert!(!file.exists(), "the token outlives the task");
    }

    #[test]
    fn a_token_is_replaced_before_it_expires_while_the_task_runs() {
        let (url, seen) = endpoint(2, None);
        let endpoint = TokenEndpoint::new(url, "crr_id_secret".into(), None).unwrap();
        let tokens = TaskTokens::grant(endpoint, vec![kueue()], "probe").unwrap();
        let file = tokens.servers()[0].token_file.clone();
        seen.recv().unwrap();
        seen.recv_timeout(Duration::from_secs(10)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::fs::read_to_string(&file).unwrap() == "token-1" {
            assert!(std::time::Instant::now() < deadline, "never replaced");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "token-2");
    }

    #[test]
    fn a_refusal_names_the_server_and_the_error_code() {
        let (url, _seen) = endpoint(900, Some("invalid_target"));
        let endpoint = TokenEndpoint::new(url, "crr_id_secret".into(), None).unwrap();
        let Err(e) = TaskTokens::grant(endpoint, vec![kueue()], "probe") else {
            panic!("granted");
        };
        assert_eq!(
            e.to_string(),
            "the token endpoint refused a token for kueue: HTTP 400 Bad Request invalid_target"
        );
    }

    #[test]
    fn a_pod_that_cannot_fetch_its_svid_gets_no_token_and_asks_for_none() {
        let (url, seen) = endpoint(900, None);
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("spire-agent.sock");
        let svid = SvidSource {
            socket: socket.to_string_lossy().into_owned(),
            audience: "http://controller".into(),
            pod: "crucible-run-brave-otter".into(),
        };
        let endpoint = TokenEndpoint::new(url, "crr_id_secret".into(), Some(svid)).unwrap();
        let Err(e) = TaskTokens::grant(endpoint, vec![kueue()], "probe") else {
            panic!("granted without an SVID");
        };
        assert!(
            e.to_string().starts_with(&format!(
                "fetching this pod's JWT-SVID from {}: ",
                socket.display()
            )),
            "{e}"
        );
        assert!(seen.try_recv().is_err(), "the pod token was sent anyway");
    }

    #[test]
    fn no_servers_is_no_request_and_no_refresher() {
        let endpoint = TokenEndpoint::new(
            "http://127.0.0.1:1/oauth/token".into(),
            "crr_x_y".into(),
            None,
        )
        .unwrap();
        let tokens = TaskTokens::grant(endpoint, Vec::new(), "probe").unwrap();
        assert!(tokens.servers().is_empty());
    }
}
