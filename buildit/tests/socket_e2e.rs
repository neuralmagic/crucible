//! The `buildit` binary against a real `buildit mcp` serving a run on its unix socket, with the
//! shipped policy and the flags the crucible engine passes. The server's cluster is unreachable,
//! so every call that gets past buildit's own checks fails at its first Kubernetes request, whose
//! label selector names the caller the socket built as.

use serde_json::Value;
use sha2::Digest;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const RUN_ID: &str = "crucible-run-socket-e2e";
const ENV_SERVER: &str = "CRUCIBLE_BUILDIT_SOCKET";

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Run {
    dir: tempfile::TempDir,
}

impl Run {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::create_dir_all(root.join("ws/svc")).unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        std::fs::write(root.join("ws/svc/Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(
            root.join("kubeconfig"),
            "apiVersion: v1\nkind: Config\ncurrent-context: none\n\
             clusters: [{name: none, cluster: {server: \"http://127.0.0.1:9\"}}]\n\
             users: [{name: none, user: {}}]\n\
             contexts: [{name: none, context: {cluster: none, user: none}}]\n",
        )
        .unwrap();
        Self { dir }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn socket(&self) -> PathBuf {
        self.path("state/buildit.sock")
    }

    fn serve(&self) -> Server {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let issuer = "http://127.0.0.1:9=http://127.0.0.1:9/jwks";
        let resource = format!("http://127.0.0.1:{port}/mcp");
        let socket = self.socket();
        let workdir = self.path("ws");
        let child = Command::new(env!("CARGO_BIN_EXE_buildit"))
            .args([
                "mcp",
                "--bind",
                &format!("127.0.0.1:{port}"),
                "--kubecontext",
                "none",
                "--namespace",
                "builds",
                "--run-issuer",
                issuer,
                "--resource",
                &resource,
                "--insecure-jwks-http",
                "--run-id",
                RUN_ID,
                "--tool-policy",
                concat!(env!("CARGO_MANIFEST_DIR"), "/deploy/buildit-policy.toml"),
                "--socket",
                socket.to_str().unwrap(),
                "--socket-workdir",
                workdir.to_str().unwrap(),
            ])
            .env("KUBECONFIG", self.path("kubeconfig"))
            .env_remove(ENV_SERVER)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let server = Server(child);
        let deadline = Instant::now() + Duration::from_secs(30);
        while !(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
            && std::os::unix::net::UnixStream::connect(&socket).is_ok())
        {
            assert!(Instant::now() < deadline, "buildit mcp never listened");
            std::thread::sleep(Duration::from_millis(100));
        }
        server
    }

    /// `buildit <args>` with the server's socket in the environment: its one stdout object and
    /// its exit code.
    fn buildit(&self, args: &[&str]) -> (Value, i32) {
        self.buildit_at(Some(&self.socket()), args)
    }

    fn buildit_at(&self, socket: Option<&Path>, args: &[&str]) -> (Value, i32) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_buildit"));
        cmd.args(args)
            .env("KUBECONFIG", self.path("kubeconfig"))
            .env_remove(ENV_SERVER)
            .stdin(Stdio::null());
        if let Some(socket) = socket {
            cmd.env(ENV_SERVER, socket);
        }
        let out = cmd.output().unwrap();
        let stdout = String::from_utf8(out.stdout).unwrap();
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines.len(), 1, "{args:?} printed {stdout:?}");
        let reply: Value = serde_json::from_str(lines[0]).unwrap();
        assert!(reply.is_object(), "{args:?}: {reply}");
        (reply, out.status.code().unwrap())
    }
}

fn run_caller() -> String {
    let hash: String = sha2::Sha256::digest(RUN_ID.as_bytes())[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("loop-{hash}")
}

fn error(reply: &Value) -> &str {
    reply["error"].as_str().unwrap_or_else(|| panic!("{reply}"))
}

#[test]
fn the_cli_drives_the_runs_socket_under_the_runs_own_name_and_the_shipped_policy() {
    let run = Run::new();
    let server = run.serve();
    let mode = std::fs::metadata(run.socket())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o777,
        0o600,
        "only the run's user reaches the socket"
    );

    let svc = run.path("ws/svc");
    let svc = svc.to_str().unwrap();
    let (digest, code) = run.buildit_at(None, &["context-digest", svc]);
    assert_eq!(code, 0, "{digest}");
    let digest = digest["context_digest"].as_str().unwrap().to_string();
    assert!(
        digest.starts_with("sha256:") && digest.len() == 71,
        "{digest}"
    );

    let label = format!("buildit.dev/sandbox={}", run_caller());
    let (reply, code) = run.buildit(&["build", "-c", svc]);
    assert_eq!(code, 1, "{reply}");
    let e = error(&reply);
    assert!(
        e.contains("listing builder pods") && e.contains(&label),
        "{e}"
    );
    for args in [
        &["status", "buildit-0123"][..],
        &["clean", "buildit-0123"],
        &["clean"],
        &["logs", "buildit-0123", "--tail", "5"],
        &["push", "buildit-0123"],
        &["run", "buildit-0123", "--", "true"],
    ] {
        let (reply, code) = run.buildit(args);
        assert_eq!(code, 1, "{args:?}: {reply}");
        let e = error(&reply);
        assert!(
            e.contains(&label) || e.contains("publishes no builds"),
            "{args:?}: {e}"
        );
    }

    let outside = run.path("elsewhere");
    let (reply, code) = run.buildit(&["build", "-c", outside.to_str().unwrap()]);
    assert_eq!(code, 1, "{reply}");
    assert!(error(&reply).contains("outside the workspace"), "{reply}");

    let (reply, code) = run.buildit(&["build", "-c", svc, "--timeout", "7200"]);
    assert_eq!(code, 1, "{reply}");
    assert_eq!(reply["status"], "refused", "{reply}");
    assert!(error(&reply).contains("build-timeout-cap"), "{reply}");

    let (reply, code) = run.buildit(&["status", "Not_An_Id"]);
    assert_eq!(code, 1, "{reply}");
    assert!(error(&reply).contains("invalid build_id"), "{reply}");

    drop(server);
    assert!(
        run.socket().exists(),
        "a killed server leaves its socket file"
    );
    let (reply, code) = run.buildit(&["status", "buildit-0123"]);
    assert_eq!(code, 1, "{reply}");
    assert!(error(&reply).contains("connecting to buildit"), "{reply}");

    let _again = run.serve();
    let (reply, code) = run.buildit(&["status", "buildit-0123"]);
    assert_eq!(code, 1, "{reply}");
    assert!(
        error(&reply).contains(&label),
        "the restarted server is the same caller: {reply}"
    );
}

#[test]
fn usage_errors_exit_2_and_a_missing_context_is_a_json_error() {
    let run = Run::new();
    let out = Command::new(env!("CARGO_BIN_EXE_buildit"))
        .args(["status", "buildit-0123"])
        .env_remove(ENV_SERVER)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());

    let (reply, code) = run.buildit_at(Some(&run.socket()), &["build", "-c", "/no/such/dir"]);
    assert_eq!(code, 1, "{reply}");
    assert!(error(&reply).contains("resolving the context"), "{reply}");
    let (reply, code) = run.buildit_at(None, &["context-digest", "/no/such/dir"]);
    assert_eq!(code, 1, "{reply}");
    assert!(reply["error"].is_string(), "{reply}");
}
